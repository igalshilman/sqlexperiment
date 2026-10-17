// Tools for any of the frameworks: defined once with `tool()`, given to any
// agent's `tools`, which turns them into the framework's own. And the tools
// that are Restate itself: calling a handler, sending to one, running a
// workflow, and waiting for a signal.
//
// A tool runs with the agent's context, and with the id of the model's tool
// call: Restate calls carry an idempotency key made of the invocation id and
// that id, so a tool call run again after a crash makes the same call, once.
import * as restate from '@restatedev/restate-sdk';
import { z } from 'zod';
import type { PerCall } from './common.ts';
import { forCall } from './common.ts';
import { signals } from './signals.ts';

const TOOL = Symbol.for('restate-agents.tool');

/** What a tool's `run` gets besides its input. */
export interface ToolContext {
  /** The agent's context: Restate calls, `ctx.key`, `ctx.db`... */
  ctx: restate.SqlObjectContext;
  /** The id of the model's tool call: the same when a crash runs the call again. */
  callId: string;
}

/** A tool of any framework: see {@link tool}. */
export interface Tool<I = Record<string, unknown>> {
  readonly [TOOL]: true;
  readonly name: string;
  readonly description: string;
  /** The input: a zod object. */
  readonly input: z.ZodObject;
  /** Its result, as text for the model. */
  run(input: I, context: ToolContext): Promise<string>;
}

/** Whether `value` is a {@link Tool}, rather than a framework's own. */
export const isTool = (value: unknown): value is Tool => typeof value === 'object' && value !== null && TOOL in value;

/** A result as text: a string as it is, anything else as JSON. */
const text = (value: unknown) => (typeof value === 'string' ? value : (JSON.stringify(value) ?? 'done'));

/**
 * A tool, for any framework's agent:
 *
 *   const weather = tool({
 *     name: 'get_weather',
 *     description: 'The current weather in a city.',
 *     input: z.object({ city: z.string() }),
 *     run: async ({ city }) => `Sunny in ${city}`,
 *   });
 *
 * `run` returns the result, text or anything JSON; it gets the agent's `ctx`
 * and the tool call's `callId` too.
 */
export const tool = <S extends z.ZodObject>(options: {
  name: string;
  description: string;
  input: S;
  run: (input: z.infer<S>, context: ToolContext) => Promise<unknown>;
}): Tool<z.infer<S>> => ({
  [TOOL]: true,
  name: options.name,
  description: options.description,
  input: options.input,
  run: async (input, context) => text(await options.run(input, context)),
});

/** A service, virtual object or workflow, as its definition (`restate.service(...)` and so on). */
type Target = { name: string };
/** Whether a target is keyed: a virtual object (key) or a workflow (id). */
const keyed = (target: Target) => 'object' in target || 'workflow' in target;

interface TargetOptions<S extends z.ZodObject | undefined> {
  /** The tool's name, for the model; by default the handler's, such as `orders_status`. */
  name?: string;
  /** What it does, for the model. */
  description: string;
  /** The handler's input: a zod object, which the model fills in. Without one, the handler takes none. */
  input?: S;
  /**
   * For a virtual object, its key (or the workflow's id): fixed, or for an
   * invocation. Without one, the model chooses it.
   */
  key?: PerCall<string>;
}

/** The tool's input: the handler's, and the key if the model chooses it. */
function inputOf(target: Target, { input, key }: TargetOptions<z.ZodObject | undefined>) {
  const shape = input?.shape ?? {};
  if (!keyed(target) || key !== undefined) return z.object(shape);
  return z.object({ key: z.string().describe(`Which ${target.name}: its key`), ...shape });
}

/** The key, and the handler's input, of a tool call's input. */
function split(
  target: Target,
  options: TargetOptions<z.ZodObject | undefined>,
  all: Record<string, unknown>,
  ctx: restate.SqlObjectContext,
) {
  if (!keyed(target)) return { key: undefined, parameter: options.input ? all : undefined };
  if (options.key !== undefined) return { key: forCall(options.key, ctx), parameter: options.input ? all : undefined };
  const { key, ...rest } = all as { key: string };
  return { key, parameter: options.input ? rest : undefined };
}

/**
 * A tool that calls `handler` of `target` and answers with its result:
 *
 *   call(orders, 'status', {
 *     description: 'The status of an order.',
 *     input: z.object({ order: z.string() }),
 *   })
 *
 * Of a virtual object, the model chooses the key, unless `key` gives it.
 */
export const call = <S extends z.ZodObject | undefined = undefined>(
  target: Target,
  handler: string,
  options: TargetOptions<S>,
): Tool =>
  tool({
    name: options.name ?? `${target.name}_${handler}`,
    description: options.description,
    input: inputOf(target, options),
    run: async (input, { ctx, callId }) => {
      const { key, parameter } = split(target, options, input, ctx);
      return ctx.genericCall<unknown, unknown>({
        service: target.name,
        method: handler,
        key,
        parameter,
        inputSerde: restate.serde.json,
        outputSerde: restate.serde.json,
        idempotencyKey: `${ctx.request().id}:${callId}`,
      });
    },
  });

/**
 * A tool that sends to `handler` of `target`, without waiting for it: a
 * one-way call, made once, after `delay` if given (such as `{ minutes: 5 }`).
 */
export const send = <S extends z.ZodObject | undefined = undefined>(
  target: Target,
  handler: string,
  options: TargetOptions<S> & { delay?: restate.Duration },
): Tool =>
  tool({
    name: options.name ?? `${target.name}_${handler}`,
    description: options.description,
    input: inputOf(target, options),
    run: async (input, { ctx, callId }) => {
      const { key, parameter } = split(target, options, input, ctx);
      ctx.genericSend<unknown>({
        service: target.name,
        method: handler,
        key,
        parameter,
        inputSerde: restate.serde.json,
        delay: options.delay,
        idempotencyKey: `${ctx.request().id}:${callId}`,
      });
      return `sent to ${target.name}${key === undefined ? '' : `/${key}`}/${handler}`;
    },
  });

/**
 * A tool that runs the workflow `target` (its `run` handler) to the end, and
 * answers with its result. Each tool call is a workflow of its own, with an
 * id made of the agent's key and the tool call's id, unless `key` gives one.
 */
export const workflow = <S extends z.ZodObject | undefined = undefined>(
  target: Target,
  options: Omit<TargetOptions<S>, 'key'> & { key?: PerCall<string> },
): Tool =>
  tool({
    name: options.name ?? target.name,
    description: options.description,
    input: z.object(options.input?.shape ?? {}),
    run: async (input, { ctx, callId }) =>
      ctx.genericCall<unknown, unknown>({
        service: target.name,
        method: 'run',
        key: options.key === undefined ? `${ctx.key}-${callId}` : forCall(options.key, ctx),
        parameter: options.input ? input : undefined,
        inputSerde: restate.serde.json,
        outputSerde: restate.serde.json,
        idempotencyKey: `${ctx.request().id}:${callId}`,
      }),
  });

/**
 * A tool that waits for the signal `name` to the agent, and answers with its
 * value: a person's approval, say, or another system's result. The agent
 * waits without holding a process.
 *
 * Signal it with the `signals` object (which {@link serve} serves), keyed
 * `<agent>:<key>`:
 *
 *   curl localhost:8080/signals/assistant:alice/resolve --json '{"name": "approval", "value": "approved"}'
 *
 * A signal sent before the agent waits for it is kept until it does.
 */
export const signal = (name: string, options: { description: string }): Tool =>
  tool({
    name: `wait_for_${name}`,
    description: options.description,
    input: z.object({}),
    run: async (_input, { ctx, callId }) =>
      ctx
        .objectClient(signals, `${ctx.request().target.service}:${ctx.key}`)
        .wait(name, restate.rpc.opts({ idempotencyKey: `${ctx.request().id}:${callId}` })),
  });
