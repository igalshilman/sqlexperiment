// DeepSeek Harness agents in Restate virtual objects: the session (the
// harness's event log) is in the object's database (persistence.ts).
//
//   const assistant = deepseek.agent({ name: 'assistant', tools: [weather] });
//
// or a handler of an object of your own: `handlers: { chat: deepseek.chat() }`,
// with options for each invocation: `deepseek.chat(async (ctx) => ({ system }))`,
// or a prompt of your own handler: `deepseek.prompt(ctx, text, options)`
//
// The model is DeepSeek's (@deepseek-ai/dsh-llm-deepseek-api-key): `model`,
// or else MODEL, or else deepseek/deepseek-flash, with DEEPSEEK_API_KEY.
//
// The handler is not replayed: a retried attempt (a crash, a lost connection)
// boots the harness again and resumes the session from the stored log.
// - The harness's session checkpoint policy stores the log before each model
//   request and before each tool body: a tool runs only once its call is
//   stored.
// - A crash leaves the turn open. Resuming closes it, and gives each tool
//   call that was stored without a result an error result saying its outcome
//   is unknown; the handler then asks the agent to continue. The model sees
//   the unknown outcome: a tool that must not run twice needs a way for the
//   model to name the call it retries (such as a `retryOf` argument).
// - A tool's error fails the attempt, which Restate retries (the harness
//   would hand it to the model as the tool's result).
// - A message is answered once per invocation: its message carries the
//   invocation id, and a retried attempt returns the answer of the turn that
//   finished it.
import * as restate from 'restate-fns';
import { Context } from '@deepseek-ai/cordis';
import { AgentRegistry } from '@deepseek-ai/dsh-agent';
import { AgentLoop } from '@deepseek-ai/dsh-agent-loop';
import * as deepseek from '@deepseek-ai/dsh-llm-deepseek-api-key';
import { LlmRuntime, MessageId, freezeMessage, type Message, type UserMessage } from '@deepseek-ai/dsh-llm';
import { SessionId, SessionStore, type SessionEvent } from '@deepseek-ai/dsh-session';
import * as checkpoints from '@deepseek-ai/dsh-session-checkpoint-policy';
import { SessionProjectionRegistry } from '@deepseek-ai/dsh-session-projection';
import { SystemPrompt } from '@deepseek-ai/dsh-system-prompt';
import {
  defineTool,
  type InferArgs,
  type ParameterSchemaSpec,
  type ValueSchemaSpec,
  type ToolDefinition,
  type ToolRunContext,
  ToolRuntime,
} from '@deepseek-ai/dsh-tools';
import {
  aborted,
  CANCELLED,
  cancellationOf,
  chatHandler,
  type Configure,
  configured,
  messagesHandler,
  modelIdOf,
  modelOf,
  nextPromptId,
  settleSteering,
  type Steerable,
  toolCallEnded,
  toolCallStarted,
} from '../common.ts';
import { object } from '../object.ts';
import { isTool, type Tool as AnyTool } from '../tools.ts';
import { z } from 'zod';
import { SqlSessionPersistence, storedEvents } from './persistence.ts';

export { SqlSessionPersistence };

/**
 * A tool whose result is text: `defineTool` of @deepseek-ai/dsh-tools, with
 * the output that takes.
 */
const textTool = <const S extends ParameterSchemaSpec>(options: {
  name: string;
  description: string;
  parameters: S;
  execute: (args: InferArgs<S>, exec: ToolRunContext) => Promise<string>;
}): ToolDefinition =>
  defineTool<S, { type: 'string' }>({
    ...options,
    output: { schema: { type: 'string' }, render: (_args, value) => [{ type: 'text', text: value }] },
  });

/** The default model. */
const DEFAULT_MODEL = 'deepseek/deepseek-flash';

/** The harness, its session in `db`: the services an agent loop needs, the persistence and its checkpoints. */
async function boot(db: restate.SqlSession, system?: string): Promise<Context> {
  const harness = new Context();
  await harness.plugin(LlmRuntime);
  await harness.plugin(SessionStore);
  await harness.plugin(SystemPrompt, system ? { personaPrefix: system } : {});
  await harness.plugin(ToolRuntime, {});
  await harness.plugin(AgentRegistry);
  await harness.plugin(SessionProjectionRegistry);
  await harness.plugin(SqlSessionPersistence, { db });
  await harness.plugin(checkpoints);
  await harness.plugin(deepseek, {}); // DEEPSEEK_API_KEY
  await harness.plugin(AgentLoop, { agents: [] });
  return harness;
}

/** The text that asks the agent to finish an interrupted prompt. */
const CONTINUE = 'Continue: your last turn was interrupted. Finish the request.';

const promptId = (id: string) => MessageId(`prompt:${id}`);
const steerId = (id: string) => MessageId(`steer:${id}`);
const textOf = (message: Message) => message.content.flatMap((c) => (c.type === 'text' ? [c.text] : [])).join('');

/**
 * The answer to prompt `id` in the session log: the last reply of the first
 * turn completed after it, or {@link CANCELLED} if a cancellation aborted its
 * turn; undefined while it is absent or unfinished (a crash's interrupted
 * turn is continued).
 */
function answerOf(events: readonly SessionEvent[], id: MessageId): string | undefined {
  const at = events.findIndex((e) => e.type === 'user/message' && e.data.id === id);
  if (at < 0) return undefined;
  let answer: string | undefined;
  for (const e of events.slice(at)) {
    if (e.type === 'assistant/message') answer = textOf(e.data.message);
    if (e.type === 'turn/end' && e.data.reason.kind === 'completed') return answer ?? '';
    if (e.type === 'turn/end' && e.data.reason.kind === 'aborted' && e.data.reason.reason.kind === 'user') {
      return CANCELLED;
    }
  }
  return undefined;
}

/** The last prompt in the session log, by id. */
function lastPrompt(events: readonly SessionEvent[]): MessageId | undefined {
  const prompt = events.findLast(
    (e): e is SessionEvent<'user/message'> => e.type === 'user/message' && e.data.id.startsWith('prompt:'),
  );
  return prompt?.data.id;
}

const message = (id: string, text: string): UserMessage =>
  freezeMessage({ id: MessageId(id), role: 'user', content: [{ type: 'text', text }], source: { kind: 'user' } });

/** A JSON schema node as the harness's value schema: what it cannot say, it takes as any JSON. */
function valueSpec(node: Record<string, unknown>): ValueSchemaSpec {
  const annotations = typeof node.description === 'string' ? { description: node.description } : {};
  const variants = (node.anyOf ?? node.oneOf) as Record<string, unknown>[] | undefined;
  if (variants && variants.length >= 2) {
    const [first, second, ...rest] = variants.map(valueSpec) as [
      ValueSchemaSpec,
      ValueSchemaSpec,
      ...ValueSchemaSpec[],
    ];
    return { ...annotations, oneOf: [first, second, ...rest] };
  }
  switch (node.type) {
    case 'string':
    case 'number':
    case 'integer':
    case 'boolean':
    case 'null':
      return {
        ...annotations,
        type: node.type,
        ...(Array.isArray(node.enum) ? { enum: node.enum } : {}),
      };
    case 'array':
      return {
        ...annotations,
        type: 'array',
        ...(node.items ? { items: valueSpec(node.items as Record<string, unknown>) } : {}),
      };
    case 'object':
      return {
        ...annotations,
        type: 'object',
        properties: parameterSpec(node),
        additionalProperties: node.additionalProperties !== false,
      };
    default:
      return { ...annotations, type: 'json' };
  }
}

/** The properties of a JSON schema object as the harness's parameters. */
function parameterSpec(node: Record<string, unknown>): ParameterSchemaSpec {
  const required = new Set((node.required as string[] | undefined) ?? []);
  const properties = (node.properties ?? {}) as Record<string, Record<string, unknown>>;
  return Object.fromEntries(
    Object.entries(properties).map(([name, property]) => [
      name,
      { ...valueSpec(property), ...(required.has(name) ? { required: true as const } : {}) },
    ]),
  );
}

/** `t` as a harness tool. */
const native = (t: AnyTool, ctx: restate.ObjectContext): ToolDefinition =>
  textTool({
    name: t.name,
    description: t.description,
    parameters: parameterSpec(z.toJSONSchema(t.input) as Record<string, unknown>),
    // Aborted (the agent's turn, on a cancellation), it waits no more.
    execute: (args, exec) => {
      const run = t.run(t.input.parse(args), { ctx, callId: exec.callId });
      if (!exec.signal) return run;
      const stopped = aborted(exec.signal).catch(() => {
        throw new restate.CancelledError();
      });
      return Promise.race([run, stopped]);
    },
  });

/** An agent's options: `chat` and `prompt` take them. */
export interface Options {
  /** The model, as `deepseek/model-id`: MODEL, or else deepseek/deepseek-flash. */
  model?: string;
  /** The system prompt: who the agent is, and how it answers. */
  system?: string;
  /** The agent's tools: `tool()`s, Restate's (`call`, `signal`...), or the harness's (`defineTool`). */
  tools?: (AnyTool | ToolDefinition)[];
}

/**
 * Answer `text`, once: a retried attempt resumes the same prompt. Each prompt
 * of an invocation is a prompt of its own. The session is the object's key.
 */
export const prompt = async (
  ctx: restate.ObjectContext,
  text: string,
  { model: named, system, tools = [] }: Options = {},
): Promise<string> => {
  const id = nextPromptId(ctx);
  const harness = await boot(ctx.db, system);
  // A tool's error fails the attempt, which Restate retries: the harness
  // stores nothing more, so the retry finds the tool's call without a result.
  const { promise: failed, reject: fail } = Promise.withResolvers<never>();
  failed.catch(() => {});
  // A cancelled invocation cancels the agent's turn, which the log keeps.
  let cancelTurn: (() => Promise<void>) | undefined;
  const turn = cancellationOf(ctx);
  const onCancel = () => fail(turn!.reason);
  turn?.addEventListener('abort', onCancel, { once: true });
  // Messages steered to the turn: the agent's steering, which its next step
  // takes (after the tool calls), and the log keeps, by id.
  let steerable!: Steerable;
  try {
    const sessionId = SessionId(ctx.key);
    const options = {
      agentOptions: { provider: 'deepseek-official', model: modelIdOf(modelOf(named, DEFAULT_MODEL), 'deepseek') },
      setup: (agentCtx: Context) =>
        tools.forEach((t) => {
          const definition = isTool(t) ? native(t, ctx) : t;
          agentCtx.tools.register({
            ...definition,
            execute: async (args, exec) => {
              try {
                await toolCallStarted(ctx);
                const result = await definition.execute(args, exec);
                await toolCallEnded(ctx, steerable);
                return result;
              } catch (error) {
                fail(error);
                throw error;
              }
            },
          });
        }),
    };
    // Resuming closes a turn a crash left open.
    const { agent } = (await harness.sessionPersistence.stat(sessionId))
      ? await harness.agents.resume({ resumeSessionId: sessionId, ...options })
      : await harness.agents.create({ sessionId, ...options });
    steerable = {
      steer: async (steered) => agent.steer(message(steerId(steered.id), steered.text)),
      has: async (steered) =>
        agent.session.snapshotEvents().some((e) => e.type === 'user/message' && e.data.id === steerId(steered)),
    };
    // The log as this attempt found it: a turn that failed before is
    // continued like an interrupted one.
    const found = agent.session.snapshotEvents().length;
    cancelTurn = async () => {
      agent.cancel({ kind: 'user' });
      await agent.whenIdle();
      await harness.sessions.flush(agent.session);
    };
    for (;;) {
      const events = agent.session.snapshotEvents();
      const answer = answerOf(events, promptId(id));
      if (answer !== undefined) {
        await settleSteering(ctx, steerable);
        return answer;
      }
      // A turn of this attempt that failed (the model's request, or a
      // checkpoint) fails the attempt. Its end may be stored already, so
      // only this attempt's turns count: the retry asks the model again.
      const end = events.slice(found).findLast((e): e is SessionEvent<'turn/end'> => e.type === 'turn/end')
        ?.data.reason;
      if (end?.kind === 'error') throw new Error(`the agent's turn failed: ${JSON.stringify(end.error)}`);
      // Finish an unfinished prompt first (this one's, after a crash), then
      // start this one.
      const last = lastPrompt(events);
      if (last !== undefined && answerOf(events, last) === undefined) {
        agent.followup(message(`continue:${events.length}`, CONTINUE));
      } else {
        agent.followup(message(promptId(id), text));
      }
      await Promise.race([agent.whenIdle(), failed]);
      await harness.sessions.flush(agent.session);
    }
  } catch (e) {
    if (e instanceof restate.CancelledError && cancelTurn) {
      await cancelTurn();
      throw e;
    }
    (harness.sessionPersistence as SqlSessionPersistence).abandon();
    throw e;
  } finally {
    turn?.removeEventListener('abort', onCancel);
    await harness.fiber.dispose();
  }
};

/**
 * A chat handler: each message an invocation, the session in the object's
 * database, with `options`, or the options a function of the invocation's
 * context gives. It takes the message as plain text (`curl -d 'Hi'`) or as
 * a JSON string (`curl --json '"Hi"'`).
 */
export const chat = (options: Configure<Options> = {}) => {
  if (typeof options === 'object' && options.model !== undefined) modelIdOf(options.model, 'deepseek');
  return chatHandler(async (ctx, text) => prompt(ctx, text, await configured(options, ctx)));
};

/** `json` parsed, or as it is if it is not JSON. */
const parsed = (json: string): unknown => {
  try {
    return JSON.parse(json);
  } catch {
    return json;
  }
};

/** A handler of the session, a page at a time: what was said, in order, with the tool calls, from its log. */
export const messages = () =>
  messagesHandler(async (ctx, transcript) => {
    for (const e of await storedEvents(ctx.db, SessionId(ctx.key))) {
      // The prompts and the messages steered to them, not the requests to
      // continue an interrupted turn.
      if (e.type === 'user/message' && /^(prompt|steer):/.test(e.data.id)) transcript.say('user', textOf(e.data));
      if (e.type === 'assistant/message') transcript.say('assistant', textOf(e.data.message));
      if (e.type === 'tool/call') transcript.call(e.data.callId, e.data.name, parsed(e.data.arguments));
      if (e.type === 'tool/result')
        transcript.result(e.data.message.toolCallId, textOf(e.data.message), e.data.message.isError);
    }
  });

/** An agent: a virtual object `name`, one per key, whose `chat` handler is {@link chat}, and `messages` {@link messages}. */
export const agent = <P extends string>({
  name,
  description,
  ...options
}: Options & { name: P; description?: string }) =>
  object({ name, description, handlers: { chat: chat(options), messages: messages() } });
