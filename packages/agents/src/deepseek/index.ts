// DeepSeek Harness agents in Restate virtual objects: the session (the
// harness's event log) is in the object's database (persistence.ts).
//
//   const assistant = deepseek.agent({ name: 'assistant', tools: [weather] });
//
// or a handler of an object of your own: `handlers: { chat: deepseek.chat() }`,
// with options for each invocation: `deepseek.chat(async (ctx) => ({ system }))`,
// or a prompt of your own handler: `deepseek.prompt(ctx, text, options)`
//
// The model is `model`, or else MODEL, or else deepseek/deepseek-flash:
// DeepSeek's (@deepseek-ai/dsh-llm-deepseek-api-key), with DEEPSEEK_API_KEY,
// or any model pi-ai knows (@deepseek-ai/dsh-llm-pi-ai), with its provider's
// key: `openai/gpt-6-sol` with OPENAI_API_KEY, say.
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
import * as restate from 'restate-objects';
import type { Database } from 'restate-objects';
import { Context } from '@deepseek-ai/cordis';
import { AgentRegistry } from '@deepseek-ai/dsh-agent';
import { AgentLoop } from '@deepseek-ai/dsh-agent-loop';
import * as deepseek from '@deepseek-ai/dsh-llm-deepseek-api-key';
import * as piAi from '@deepseek-ai/dsh-llm-pi-ai';
import { LlmRuntime, MessageId, freezeMessage, type Message, type UserMessage } from '@deepseek-ai/dsh-llm';
import { SessionId, SessionStore, type SessionEvent } from '@deepseek-ai/dsh-session';
import * as checkpoints from '@deepseek-ai/dsh-session-checkpoint-policy';
import { SessionProjectionRegistry } from '@deepseek-ai/dsh-session-projection';
import { SystemPrompt } from '@deepseek-ai/dsh-system-prompt';
import { builtinProviders } from '@earendil-works/pi-ai/providers/all';
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
  checkModelName,
  configured,
  messagesHandler,
  ModelError,
  modelOf,
  nextPromptId,
  providerOf,
  settleSteering,
  type Steerable,
  toolCallEnded,
  toolCallStarted,
  Transcript,
  transcribing,
  refusal,
  storedAnswer,
  UNANSWERED,
} from '../common.ts';
import { type Answer, withOutput } from '../output.ts';
import { type Definition, object } from '../object.ts';
import { isTool, jsonSchemaOf, type Tool as AnyTool } from '../tools.ts';
import type { z } from 'zod';
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
const DEEPSEEK_MODEL = 'deepseek/deepseek-flash';

/** A model as the harness names it: its provider route, and its id there. */
interface Route {
  provider: string;
  model: string;
}

/**
 * `model` (`provider/model-id`) as the harness's route: DeepSeek's own
 * provider, or else pi-ai's provider of that name, if it has the model.
 */
function routeOf(model: string): Route {
  const [provider, id] = providerOf(model);
  if (provider === 'deepseek') return { provider: 'deepseek-official', model: id };
  const from = process.env.MODEL === model ? ' (from MODEL)' : '';
  const known = builtinProviders().find((p) => p.id === provider);
  if (!known) {
    throw new ModelError(`deepseek: the model "${model}"${from} is not provider/model-id of a provider pi-ai knows`);
  }
  if (!known.getModels().some((m) => m.id === id)) {
    throw new ModelError(`deepseek: pi-ai knows no model "${id}" of ${provider} (the model "${model}"${from})`);
  }
  return { provider, model: id };
}

/**
 * The harness, its session in `db`: the services
 * an agent loop needs, the persistence and its checkpoints, and the provider
 * of `route`.
 */
async function boot(db: restate.Database, route: Route, system?: string): Promise<Context> {
  const harness = new Context();
  await harness.plugin(LlmRuntime);
  await harness.plugin(SessionStore);
  await harness.plugin(SystemPrompt, system ? { personaPrefix: system } : {});
  await harness.plugin(ToolRuntime, {});
  await harness.plugin(AgentRegistry);
  await harness.plugin(SessionProjectionRegistry);
  await harness.plugin(SqlSessionPersistence, { db });
  await harness.plugin(checkpoints);
  // DeepSeek's with DEEPSEEK_API_KEY; another's with the key pi-ai finds for it (OPENAI_API_KEY...).
  if (route.provider === 'deepseek-official') await harness.plugin(deepseek, {});
  else await harness.plugin(piAi, { providers: { [route.provider]: {} } });
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
 * turn completed after it, {@link CANCELLED} if a cancellation aborted its
 * turn, or {@link UNANSWERED} if the model's provider refused it for good;
 * undefined while it is absent or unfinished (a crash's interrupted turn, or
 * one that failed otherwise, is continued).
 */
function answerOf(events: readonly SessionEvent[], id: MessageId): string | undefined {
  const at = events.findIndex((e) => e.type === 'user/message' && e.data.id === id);
  if (at < 0) return undefined;
  let answer: string | undefined;
  for (const e of events.slice(at)) {
    if (e.type === 'assistant/message') answer = textOf(e.data.message);
    if (e.type !== 'turn/end') continue;
    const { reason } = e.data;
    if (reason.kind === 'completed') return answer ?? '';
    if (reason.kind === 'aborted' && reason.reason.kind === 'user') return CANCELLED;
    if (reason.kind === 'error' && refusal(turnError(reason.error))) return UNANSWERED;
  }
  return undefined;
}

/** The error a turn failed with, as the attempt fails with it (see {@link throwIfTurnFailed}). */
const turnError = (error: unknown) => new Error(`the agent's turn failed: ${JSON.stringify(error)}`);

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
const native = (t: AnyTool, ctx: restate.SessionContext): ToolDefinition =>
  textTool({
    name: t.name,
    description: t.description,
    parameters: parameterSpec(jsonSchemaOf(t)),
    // Aborted (the agent's turn, on a cancellation), it waits no more.
    execute: (args, exec) => {
      const run = t.run(args, { ctx, callId: exec.callId });
      if (!exec.signal) return run;
      const stopped = aborted(exec.signal).catch(() => {
        throw new restate.CancelledError();
      });
      return Promise.race([run, stopped]);
    },
  });

/** An agent's options: `chat` and `prompt` take them. */
export interface Options<O extends z.ZodObject | undefined = z.ZodObject | undefined> {
  /**
   * The model, as `provider/model-id`: MODEL, or else deepseek/deepseek-flash.
   * DeepSeek's, or any pi-ai knows, such as `openai/gpt-6-sol`.
   */
  model?: string;
  /** The system prompt: who the agent is, and how it answers. */
  system?: string;
  /** The agent's tools: `tool()`s, Restate's (`call`, `signal`...), or the harness's (`defineTool`). */
  tools?: (AnyTool | ToolDefinition)[];
  /**
   * The answer as data: a zod object, whose JSON the agent replies with
   * (checked), and `chat` and `prompt` answer with (see output.ts).
   */
  output?: O;
}

/** {@link prompt}, answering with the agent's text. */
const promptText = (ctx: restate.SessionContext, text: string, options: Options): Promise<string> =>
  transcribing(
    ctx,
    () => transcribe(ctx.db, ctx.key),
    () => answerOnce(ctx, text, options),
  );

/** An agent of the harness, on a session. */
type Agent = Awaited<ReturnType<Context['agents']['create']>>['agent'];

const answerOnce = async (
  ctx: restate.SessionContext,
  text: string,
  { model: named, system, tools = [] }: Options,
): Promise<string> => {
  const id = nextPromptId(ctx);
  const route = routeOf(modelOf(named, DEEPSEEK_MODEL));
  const harness = await boot(ctx.db, route, system);
  // A tool's error fails the attempt, which Restate retries: the harness
  // stores nothing more, so the retry finds the tool's call without a result.
  const { promise: failed, reject: fail } = Promise.withResolvers<never>();
  failed.catch(() => {});
  const turn = cancellationOf(ctx);
  const onCancel = () => fail(turn!.reason);
  turn?.addEventListener('abort', onCancel, { once: true });
  let agent: Agent | undefined;
  try {
    const sessionId = SessionId(ctx.key);
    const steerable = () => steeringOf(agent!);
    const options = {
      agentOptions: route,
      setup: (agentCtx: Context) => {
        for (const t of tools) agentCtx.tools.register(tracked(isTool(t) ? native(t, ctx) : t, ctx, steerable, fail));
      },
    };
    // Resuming closes a turn a crash left open.
    const started = (await harness.sessionPersistence.stat(sessionId))
      ? await harness.agents.resume({ resumeSessionId: sessionId, ...options })
      : await harness.agents.create({ sessionId, ...options });
    agent = started.agent;
    const answer = await answered(harness, agent, id, text, failed);
    await settleSteering(ctx, steerable());
    return answer;
  } catch (e) {
    // A cancelled invocation cancels the agent's turn, which the log keeps.
    if (e instanceof restate.CancelledError && agent) await cancelTurn(harness, agent);
    else (harness.sessionPersistence as SqlSessionPersistence).abandon();
    throw e;
  } finally {
    turn?.removeEventListener('abort', onCancel);
    await harness.fiber.dispose();
  }
};

/**
 * The answer of prompt `id`, `text`, asked once: a prompt left unfinished
 * (this one's, after a crash) is finished first, then this one starts.
 */
async function answered(harness: Context, agent: Agent, id: string, text: string, failed: Promise<never>) {
  // The log as this attempt found it: a turn that failed before is continued like an interrupted one.
  const found = agent.session.snapshotEvents().length;
  for (;;) {
    const events = agent.session.snapshotEvents();
    // First: a prompt refused in this attempt fails it, rather than answer.
    throwIfTurnFailed(events.slice(found));
    const answer = answerOf(events, promptId(id));
    if (answer !== undefined) return storedAnswer(answer);
    const last = lastPrompt(events);
    const unfinished = last !== undefined && answerOf(events, last) === undefined;
    agent.followup(unfinished ? message(`continue:${events.length}`, CONTINUE) : message(promptId(id), text));
    await Promise.race([agent.whenIdle(), failed]);
    await harness.sessions.flush(agent.session);
  }
}

/**
 * Fail the attempt if a turn of `events` (this attempt's) failed: its model
 * request, or a checkpoint. Its end may be stored already, so only this
 * attempt's turns count: the retry asks the model again.
 */
function throwIfTurnFailed(events: readonly SessionEvent[]) {
  const end = events.findLast((e): e is SessionEvent<'turn/end'> => e.type === 'turn/end')?.data.reason;
  if (end?.kind === 'error') throw turnError(end.error);
}

/** Cancel `agent`'s turn, and store it cancelled. */
async function cancelTurn(harness: Context, agent: Agent) {
  agent.cancel({ kind: 'user' });
  await agent.whenIdle();
  await harness.sessions.flush(agent.session);
}

/**
 * Messages steered to the turn: the agent's steering, which its next step
 * takes (after the tool calls), and the log keeps, by id.
 */
const steeringOf = (agent: Agent): Steerable => ({
  steer: async (steered) => agent.steer(message(steerId(steered.id), steered.text)),
  has: async (steered) =>
    agent.session.snapshotEvents().some((e) => e.type === 'user/message' && e.data.id === steerId(steered)),
});

/**
 * `definition`, its calls published as they start and end (for the clients
 * that follow the conversation, and the steering), and its error failing the
 * attempt.
 */
const tracked = (
  definition: ToolDefinition,
  ctx: restate.SessionContext,
  steerable: () => Steerable,
  fail: (error: unknown) => void,
): ToolDefinition => ({
  ...definition,
  execute: async (args, exec) => {
    try {
      await toolCallStarted(ctx);
      const result = await definition.execute(args, exec);
      await toolCallEnded(ctx, steerable());
      return result;
    } catch (error) {
      fail(error);
      throw error;
    }
  },
});

/**
 * Answer `text`, once: a retried attempt resumes the same prompt. Each prompt
 * of an invocation is a prompt of its own. The session is the object's key.
 * With `output`, it answers with the value the agent replies with (see
 * output.ts), not its text.
 */
export const prompt = <O extends z.ZodObject | undefined = undefined>(
  ctx: restate.SessionContext,
  text: string,
  options: Options<O> = {},
): Promise<Answer<O>> => withOutput(text, options, (t, o) => promptText(ctx, t, o)) as Promise<Answer<O>>;

/**
 * A chat handler: each message an invocation, the session in the object's
 * database, with `options`, or the options a function of the invocation's
 * context gives. It takes the message as plain text (`curl -d 'Hi'`) or as
 * a JSON string (`curl --json '"Hi"'`).
 */
export const chat = <O extends z.ZodObject | undefined = undefined>(options: Configure<Options<O>> = {}) => {
  if (typeof options === 'object') {
    checkModelName('deepseek', options.model);
    routeOf(modelOf(options.model, DEEPSEEK_MODEL));
  }
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

/**
 * A handler of the conversation as clients read it: a page at a time, or
 * what is new or changed since a cursor, from the log each change writes
 * (log.ts). It answers while a message is answered.
 */
export const messages = () => messagesHandler();

/** The session `key` in `db`, from its log, as `messages` gives it. */
const transcribe = async (db: Database, key: string) => {
  const transcript = new Transcript();
  for (const e of await storedEvents(db, SessionId(key))) {
    // The prompts and the messages steered to them, not the requests to
    // continue an interrupted turn.
    if (e.type === 'user/message' && /^(prompt|steer):/.test(e.data.id)) transcript.say('user', textOf(e.data));
    if (e.type === 'assistant/message') transcript.say('assistant', textOf(e.data.message));
    if (e.type === 'tool/call') transcript.call(e.data.callId, e.data.name, parsed(e.data.arguments));
    if (e.type === 'tool/result')
      transcript.result(e.data.message.toolCallId, textOf(e.data.message), e.data.message.isError);
  }
  return transcript;
};

/** An agent: a virtual object `name`, one per key, whose `chat` handler is {@link chat}, and `messages` {@link messages}. */
export const agent = <P extends string, O extends z.ZodObject | undefined = undefined>({
  name,
  description,
  schema,
  background,
  ...options
}: Options<O> & Definition<P>) =>
  object({ name, description, schema, background, handlers: { chat: chat(options) }, views: { messages: messages() } });
