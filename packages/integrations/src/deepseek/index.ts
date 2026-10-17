// DeepSeek Harness agents in Restate virtual objects: the session (the
// harness's event log) is in the object's database (persistence.ts).
//
//   const assistant = deepseek.agent({ name: 'assistant', tools: [weather] });
//
// or a handler of an object of your own: `handlers: { chat: deepseek.chat() }`
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
import type * as restate from '@restatedev/restate-sdk';
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
import { chatHandler, forCall, modelIdOf, modelOf, type PerCall } from '../common.ts';
import { object } from '../object.ts';
import { isTool, type Tool as AnyTool } from '../tools.ts';
import { z } from 'zod';
import { SqlSessionPersistence } from './persistence.ts';

export { SqlSessionPersistence };

/**
 * A tool whose result is text: `defineTool` of @deepseek-ai/dsh-tools, with
 * the output that takes.
 */
export const tool = <const S extends ParameterSchemaSpec>(options: {
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
export const DEFAULT_MODEL = 'deepseek/deepseek-flash';

/** Run a tool's `body`, whose error also goes to `fail`. */
async function guard<T>(fail: (error: unknown) => void, body: () => Promise<T>): Promise<T> {
  try {
    return await body();
  } catch (e) {
    fail(e);
    throw e;
  }
}

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
const textOf = (message: Message) => message.content.flatMap((c) => (c.type === 'text' ? [c.text] : [])).join('');

/**
 * The answer to prompt `id` in the session log: the last reply of the first
 * turn completed after it; undefined while it is absent or unfinished.
 */
function answerOf(events: readonly SessionEvent[], id: MessageId): string | undefined {
  const at = events.findIndex((e) => e.type === 'user/message' && e.data.id === id);
  if (at < 0) return undefined;
  let answer: string | undefined;
  for (const e of events.slice(at)) {
    if (e.type === 'assistant/message') answer = textOf(e.data.message);
    if (e.type === 'turn/end' && e.data.reason.kind === 'completed') return answer ?? '';
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
const native = (t: AnyTool, ctx: restate.SqlObjectContext): ToolDefinition =>
  tool({
    name: t.name,
    description: t.description,
    parameters: parameterSpec(z.toJSONSchema(t.input) as Record<string, unknown>),
    execute: (args, exec) => t.run(t.input.parse(args), { ctx, callId: exec.callId }),
  });

export interface ChatOptions {
  /** The model, as `deepseek/model-id`: MODEL, or else {@link DEFAULT_MODEL}. */
  model?: string;
  /** The system prompt: who the agent is, and how it answers. */
  system?: string;
  /** The agent's tools: `tool()`s, Restate's (`call`, `signal`...), or the harness's (`tool` here, or `defineTool`), or for an invocation. */
  tools?: (PerCall<ToolDefinition> | AnyTool)[];
}

/** The answer to a message, each message an invocation, the session in the object's database. */
const respond =
  ({ model: named, system, tools = [] }: ChatOptions = {}) =>
  async (ctx: restate.SqlObjectContext, text: string): Promise<string> => {
    const id = ctx.request().id;
    const harness = await boot(ctx.db, system);
    // A tool's error fails the attempt, which Restate retries: the harness
    // stores nothing more, so the retry finds the tool's call without a result.
    const { promise: failed, reject: fail } = Promise.withResolvers<never>();
    failed.catch(() => {});
    try {
      const sessionId = SessionId(ctx.key);
      const options = {
        agentOptions: { provider: 'deepseek-official', model: modelIdOf(modelOf(named, DEFAULT_MODEL), 'deepseek') },
        setup: (agentCtx: Context) =>
          tools.forEach((t) => {
            const definition = isTool(t) ? native(t, ctx) : forCall(t, ctx);
            agentCtx.tools.register({
              ...definition,
              execute: (args, exec) => guard(fail, () => definition.execute(args, exec)),
            });
          }),
      };
      // Resuming closes a turn a crash left open.
      const { agent } = (await harness.sessionPersistence.stat(sessionId))
        ? await harness.agents.resume({ resumeSessionId: sessionId, ...options })
        : await harness.agents.create({ sessionId, ...options });
      // The log as this attempt found it: a turn that failed before is
      // continued like an interrupted one.
      const found = agent.session.snapshotEvents().length;
      for (;;) {
        const events = agent.session.snapshotEvents();
        const answer = answerOf(events, promptId(id));
        if (answer !== undefined) return answer;
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
      (harness.sessionPersistence as SqlSessionPersistence).abandon();
      throw e;
    } finally {
      await harness.fiber.dispose();
    }
  };

/**
 * A chat handler: each message an invocation, the session in the object's database.
 * It takes the message as plain text (`curl -d 'Hi'`) or as a JSON string
 * (`curl --json '"Hi"'`).
 */
export const chat = (options: ChatOptions = {}) => chatHandler(respond(options));

/** An agent: a virtual object `name`, one per key, whose `chat` handler is {@link chat}. */
export const agent = <P extends string>({
  name,
  description,
  ...options
}: ChatOptions & { name: P; description?: string }) => object({ name, description, handlers: { chat: chat(options) } });
