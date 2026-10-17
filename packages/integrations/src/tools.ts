// Tools for any of the frameworks: defined once with `tool()`, given to any
// agent's `tools`, which turns them into the framework's own. And the tools
// that are Restate itself: calling a handler, sending to one, running a
// workflow, and waiting for a signal.
//
// A tool runs with the agent's context, and with the id of the model's tool
// call: Restate calls carry an idempotency key made of the invocation id and
// that id, so a tool call run again after a crash makes the same call, once.
import * as restate from '@restatedev/restate-sdk';
import { randomUUID } from 'node:crypto';
import { z } from 'zod';
import { type Configure, configured, ToolError } from './common.ts';
import { signals, signalsOf } from './signals.ts';

const TOOL = Symbol.for('restate-agents.tool');

/** A tool call: the agent's context, and the id of the model's call. */
export interface ToolCall {
  /** The agent's context: Restate calls, `ctx.key`, `ctx.db`... */
  ctx: restate.SqlObjectContext;
  /** The id of the model's tool call: the same when a crash runs the call again. */
  callId: string;
}

/** What a tool's `run` gets besides its input. */
export interface ToolContext extends ToolCall {
  /**
   * The tool call's idempotency key, `<invocation id>:<call id>`: for a call
   * the tool makes once, however often it runs (a Restate call's
   * `idempotencyKey`, or a payment API's).
   */
  idempotencyKey: string;
}

/** A tool of any framework: see {@link tool}. */
export interface Tool<I = Record<string, unknown>> {
  readonly [TOOL]: true;
  readonly name: string;
  readonly description: string;
  /** The input: a zod object. */
  readonly input: z.ZodObject;
  /** Its result, as text for the model. */
  run(input: I, call: ToolCall): Promise<string>;
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
 * `run` returns the result (or a promise of it), text or anything JSON; it
 * gets the agent's `ctx`, the tool call's `callId`, and an `idempotencyKey`
 * made of both, for what the tool must do once. A `restate.TerminalError` it throws (or
 * a Restate call it makes fails with) is its answer, `Error: <message>`, for
 * the model to act on.
 */
export const tool = <S extends z.ZodObject>(options: {
  name: string;
  description: string;
  input: S;
  run: (input: z.infer<S>, context: ToolContext) => unknown;
}): Tool<z.infer<S>> => ({
  [TOOL]: true,
  name: options.name,
  description: options.description,
  input: options.input,
  run: async (input, { ctx, callId }) => {
    try {
      return text(await options.run(input, { ctx, callId, idempotencyKey: `${ctx.request().id}:${callId}` }));
    } catch (error) {
      // A terminal error is the tool's answer, which the model can act on; any
      // other error fails the attempt, which Restate retries.
      if (error instanceof restate.TerminalError) return `Error: ${error.message}`;
      throw new ToolError(options.name, error);
    }
  },
});

/**
 * The Restate calls an invocation's tools made that have not answered yet,
 * in the object's state: each with the tool, its input as JSON, the call id
 * it was made with, and the attempt that made it, or last made it again.
 */
interface OpenCalls {
  invocation: string;
  calls: { tool: string; input: string; callId: string; attempt: string }[];
}

/** The state key of the {@link OpenCalls}. */
const OPEN_CALLS = 'restate-agents.calls';

/** Each attempt's id, and its open calls, by its context. */
const attempts = new WeakMap<object, { id: string; open: Promise<OpenCalls> }>();

/**
 * `body`, a Restate call of tool `name` with `input`, made once however often
 * the tool call runs: it gets the call id that the call was first made with.
 * A crash or a suspension restarts the handler, and a framework then runs a
 * tool call that was cut off again with its id (pi, Claude, LangGraph), or
 * tells the model it was interrupted, and the model makes it again with a new
 * id (OpenCode, the DeepSeek Harness). Made again with the same input, it is
 * the call an earlier attempt made: its id, so the same idempotency key, so
 * the same Restate call (a wait for a signal, a sub-agent's task, a
 * workflow), which it waits for again.
 */
async function once<T>(name: string, input: unknown, context: ToolCall, body: (call: ToolContext) => Promise<T>) {
  const { ctx } = context;
  const invocation = ctx.request().id;
  let attempt = attempts.get(ctx);
  if (!attempt) {
    const open = ctx
      .get<OpenCalls>(OPEN_CALLS)
      .then((o) => (o?.invocation === invocation ? o : { invocation, calls: [] }));
    attempt = { id: randomUUID(), open };
    attempts.set(ctx, attempt);
  }
  const { id } = attempt;
  const open = await attempt.open;
  const json = JSON.stringify(input);
  let call =
    open.calls.find((c) => c.callId === context.callId) ??
    open.calls.find((c) => c.attempt !== id && c.tool === name && c.input === json);
  if (call) call.attempt = id;
  else open.calls.push((call = { tool: name, input: json, callId: context.callId, attempt: id }));
  ctx.set(OPEN_CALLS, open);
  const done = () => {
    open.calls = open.calls.filter((c) => c !== call);
    ctx.set(OPEN_CALLS, open);
  };
  try {
    const result = await body({ ctx, callId: call.callId, idempotencyKey: `${invocation}:${call.callId}` });
    done();
    return result;
  } catch (error) {
    // A terminal error answers the call; any other fails the attempt, and the
    // retry makes the call again.
    if (error instanceof restate.TerminalError) done();
    throw error;
  }
}

/** A service, virtual object or workflow, as its definition (`restate.service(...)` and so on), of handlers `M`. */
type Target<M = unknown> =
  | restate.ServiceDefinition<string, M>
  | restate.VirtualObjectDefinition<string, M>
  | restate.WorkflowDefinition<string, M>;
/** Whether a target is keyed: a virtual object (key) or a workflow (id). */
const keyed = (target: Target) => 'object' in target || 'workflow' in target;

/** Call `method` of `service`, with JSON in and out, once per tool call (see {@link ToolContext}). */
const invoke = (
  { ctx, idempotencyKey }: ToolContext,
  target: { service: string; method: string; key?: string; parameter: unknown },
) =>
  ctx.genericCall<unknown, unknown>({
    ...target,
    inputSerde: restate.serde.json,
    outputSerde: restate.serde.json,
    idempotencyKey,
  });

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
  key?: Configure<string>;
}

/** The tool's input: the handler's, and the key if the model chooses it. */
function inputOf(target: Target, { input, key }: TargetOptions<z.ZodObject | undefined>) {
  const shape = input?.shape ?? {};
  if (!keyed(target) || key !== undefined) return z.object(shape);
  return z.object({ key: z.string().describe(`Which ${target.name}: its key`), ...shape });
}

/** The key, and the handler's input, of a tool call's input. */
async function split(
  target: Target,
  options: TargetOptions<z.ZodObject | undefined>,
  all: Record<string, unknown>,
  ctx: restate.SqlObjectContext,
) {
  if (!keyed(target)) return { key: undefined, parameter: options.input ? all : undefined };
  if (options.key !== undefined) {
    return { key: await configured(options.key, ctx), parameter: options.input ? all : undefined };
  }
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
export const call = <M, S extends z.ZodObject | undefined = undefined>(
  target: Target<M>,
  handler: keyof M & string,
  options: TargetOptions<S>,
): Tool =>
  tool({
    name: options.name ?? `${target.name}_${handler}`,
    description: options.description,
    input: inputOf(target, options),
    run: async (input, context) =>
      once(`${target.name}/${handler}`, input, context, async (c) =>
        invoke(c, { service: target.name, method: handler, ...(await split(target, options, input, c.ctx)) }),
      ),
  });

/**
 * A tool that sends to `handler` of `target`, without waiting for it: a
 * one-way call, made once, after `delay` if given (such as `{ minutes: 5 }`).
 */
export const send = <M, S extends z.ZodObject | undefined = undefined>(
  target: Target<M>,
  handler: keyof M & string,
  options: TargetOptions<S> & { delay?: restate.Duration },
): Tool =>
  tool({
    name: options.name ?? `${target.name}_${handler}`,
    description: options.description,
    input: inputOf(target, options),
    run: async (input, context) =>
      once(`${target.name}/${handler}`, input, context, async ({ ctx, idempotencyKey }) => {
        const { key, parameter } = await split(target, options, input, ctx);
        ctx.genericSend<unknown>({
          service: target.name,
          method: handler,
          key,
          parameter,
          inputSerde: restate.serde.json,
          delay: options.delay,
          idempotencyKey,
        });
        return `sent to ${target.name}${key === undefined ? '' : `/${key}`}/${handler}`;
      }),
  });

/**
 * A tool that runs the workflow `target` (its `run` handler) to the end, and
 * answers with its result. Each tool call is a workflow of its own, with an
 * id made of the agent's key and the tool call's id, unless `key` gives one.
 */
export const workflow = <S extends z.ZodObject | undefined = undefined>(
  target: restate.WorkflowDefinition<string, { run: unknown }>,
  options: TargetOptions<S>,
): Tool =>
  tool({
    name: options.name ?? target.name,
    description: options.description,
    input: z.object(options.input?.shape ?? {}),
    run: async (input, context) =>
      once(target.name, input, context, async (c) =>
        invoke(c, {
          service: target.name,
          method: 'run',
          key: options.key === undefined ? `${c.ctx.key}-${c.callId}` : await configured(options.key, c.ctx),
          parameter: options.input ? input : undefined,
        }),
      ),
  });

/** An object with a `chat` handler; another object needs subagent's `handler`. */
type WithChat<M> = M extends { chat: unknown }
  ? unknown
  : { 'an agent with a chat handler, or a handler option': never };

/**
 * A tool, named after `agent` (an agent, or any virtual object) unless `name` names it, that hands
 * a task to it: to the object keyed `<this key>.<task name>`, through its
 * `handler` (`chat`), and answers with its answer. Tasks of different names
 * run at once; the same name continues the same task.
 *
 *   const lead = pi.agent({ name: 'lead', tools: [subagent(coder, { description: 'a coding agent' })] });
 */
export function subagent<M>(
  agent: restate.VirtualObjectDefinition<string, M> & WithChat<M>,
  options?: { description?: string; name?: string },
): Tool;
/** A sub-agent through another of its handlers. */
export function subagent<M>(
  agent: restate.VirtualObjectDefinition<string, M>,
  options: { description?: string; name?: string; handler: keyof M & string },
): Tool;
export function subagent<M>(
  agent: restate.VirtualObjectDefinition<string, M>,
  {
    description = `the ${agent.name} agent`,
    name: toolName = agent.name,
    handler,
  }: { description?: string; name?: string; handler?: keyof M & string } = {},
): Tool {
  return tool({
    name: toolName,
    description:
      `Hand a task to ${description}, and get its answer. ` +
      'Tasks of different names run at once; the same name continues the same task.',
    input: z.object({
      name: z
        .string()
        .regex(/^[a-z0-9-]+$/)
        .describe('A short name for the task, such as "fizzbuzz"'),
      task: z.string().describe('The task in full: the agent sees nothing else'),
    }),
    run: async ({ name, task }, context) =>
      once(agent.name, { name, task }, context, (c) =>
        invoke(c, { service: agent.name, key: `${c.ctx.key}.${name}`, method: handler ?? 'chat', parameter: task }),
      ),
  });
}

/**
 * A tool that waits for the signal `name` to the agent, and answers with its
 * value: a person's approval, say, or another system's result. The agent
 * waits without holding a process. With an `input`, the model says what it
 * waits for (such as the refund to approve), which `pending` shows.
 *
 *   signal('approval', {
 *     description: 'Wait for a manager to approve a refund.',
 *     input: z.object({ order: z.string(), amount: z.number() }),
 *   })
 *
 * Each wait has an id: `pending` (each `object()` has it) lists them, and
 * the agent's `signal` handler resolves one, by its id, or the oldest wait
 * for a name:
 *
 *   curl localhost:8080/support/alice/pending
 *   curl localhost:8080/support/alice/signal --json '{"id": "inv_1f…:toolu_01", "value": "approved"}'
 *   curl localhost:8080/support/alice/signal --json '{"name": "approval", "value": "approved"}'
 *
 * It answers with the id of the wait it resolved, or `null` if none waited:
 * the signal is dropped then, unless it has `"keep": true`, which keeps it
 * for the next wait for its name.
 */
export const signal = (name: string, options: { description: string; input?: z.ZodObject; name?: string }): Tool =>
  tool({
    name: options.name ?? `wait_for_${name}`,
    description: options.description,
    input: z.object(options.input?.shape ?? {}),
    run: async (input, context) =>
      once(`signal/${name}`, input, context, async ({ ctx, idempotencyKey: id }) => {
        return ctx
          .objectClient(signals, signalsOf(ctx.request().target.service, ctx.key))
          .wait({ id, name, ...(options.input ? { input } : {}) }, restate.rpc.opts({ idempotencyKey: id }));
      }),
  });
