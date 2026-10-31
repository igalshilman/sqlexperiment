// pi agents in Restate virtual objects: a pi-durable Harness for each
// invocation, with its storage in the object's database.
//
//   const assistant = pi.agent({ name: 'assistant', tools: [weather] });
//   const lead = pi.agent({ name: 'lead', tools: [subagent(coder)] });
//
// or a handler of an object of your own: `handlers: { chat: pi.chat() }`,
// with options for each invocation: `pi.chat(async (ctx) => ({ model }))`
//
// or, for more than a prompt:
//
//   compact: (ctx) => withPi(ctx, { tools }, async ({ conversation, context }) => ...),
//
// The model is `model`, MODEL, or else openai/gpt-6-sol, with the
// provider's API key (such as OPENAI_API_KEY).
//
// It runs `work` over `ctx.db`: pi-durable's storage is the object's
// database (storage.ts), and each pi-durable commit is an SQL transaction
// in it, stored in order with everything else the handler does. The Harness
// is opened with `tools` (made with pi-durable's `defineTool`) and closed at
// the end. A retried attempt (a crash, a lost connection) opens it again and
// carries on: a prompt is submitted once, by its id (pi-durable's requestId),
// an answered prompt returns its answer, and unfinished work resumes: a model
// call is made again, and a tool call declared `replay: 'safe'` runs again
// with the same call id, which a tool passes on to stay idempotent. A tool's
// error fails the attempt, which Restate retries (pi-durable would hand it to
// the model as the tool's result).
//
// With a `workspace` directory, the agent also gets pi-durable's coding
// tools (`read`, `write`, `edit`, `bash`) over it, on the local file system.
// The files are not in the database: they last as long as the disk does,
// and a coding tool cut off by a crash is not run again (the model is told
// it was interrupted).
import * as restate from 'restate-objects';
import type { Context } from '@earendil-works/chord';
import { awaitWithContext, BACKGROUND_CONTEXT, withCancel, withoutAbortSignal } from '@earendil-works/chord/context';
import { mkdir } from 'node:fs/promises';
import { type Message, type MutableModels, Type } from '@earendil-works/pi-ai';
import { createModels } from '@earendil-works/pi-ai/models';
import {
  AssistantEntry,
  createRegistry,
  defineExtension,
  defineTool,
  Harness,
  section,
  type Conversation,
  type ConversationStreamOptions,
  type ToolRegistration,
} from '@earendil-works/pi-durable';
import { NodeExecutionEnv } from '@earendil-works/pi-durable/env/node';
import { CodingTools } from '@earendil-works/pi-durable/tools';
import {
  cancellationOf,
  CHECK_MODEL,
  chatHandler,
  checkModelName,
  type Configure,
  configured,
  messagesHandler,
  modelOf,
  providerOf,
  nextPromptId,
  settleSteering,
  type Steerable,
  toolCallEnded,
  toolCallStarted,
  type Transcript,
  transcribing,
} from '../common.ts';
import { type Answer, withOutput } from '../output.ts';
import { type Definition, object } from '../object.ts';
import { isTool, jsonSchemaOf, type Tool as AnyTool } from '../tools.ts';
import type { z } from 'zod';
import { openSqlKvStorage } from './storage.ts';

/** What `work` gets: `prompt(text)`, the `conversation()`, and the `harness` and chord `context` for anything else. */
export interface Pi {
  harness: Harness;
  context: Context;
  /** Answer `text`, once per prompt `id`, with the answer's text. By default, each prompt of an invocation has an id of its own, the same on every attempt. */
  prompt: (text: string, id?: string) => Promise<string>;
  /** The object's conversation, on the model: for `reset`, `compact`, `entries` and the like. */
  conversation: () => Promise<Conversation>;
}

/** The text of a message's content. */
const textOf = (content: Message['content']): string =>
  typeof content === 'string' ? content : content.flatMap((c) => (c.type === 'text' ? [c.text] : [])).join('');

/** An agent's options: `chat`, `prompt` and `withPi` take them. */
export interface Options<O extends z.ZodObject | undefined = z.ZodObject | undefined> {
  /** The model, as `provider/model-id`: MODEL, or else openai/gpt-6-sol. */
  model?: string;
  /** A pi-ai model registry; by default, one of the model's provider. */
  models?: MutableModels;
  /** The system prompt: who the agent is, and how it answers. */
  system?: string;
  /** The agent's tools: `tool()`s, Restate's (`call`, `signal`...), or pi-durable's `defineTool`s. */
  tools?: (ToolRegistration | AnyTool)[];
  /** A directory for the coding tools, made if missing; without one, the agent has no file system. */
  workspace?: string;
  /** OpenAI's service tier for each request: `fast` for its fast mode (priced higher), `flex`, and so on. */
  serviceTier?: ServiceTier;
  /**
   * The answer as data: a zod object, whose JSON the agent replies with
   * (checked), and `chat` and `prompt` answer with (see output.ts).
   */
  output?: O;
}

/** OpenAI's service tiers (its Responses API's `service_tier`). */
export type ServiceTier = 'auto' | 'default' | 'fast' | 'flex' | 'priority';

/** A registry of `provider`'s models, such as anthropicProvider() from @earendil-works/pi-ai/providers/anthropic. */
async function modelsOf(provider: string): Promise<MutableModels> {
  const models = createModels();
  let module: Record<string, () => Parameters<MutableModels['setProvider']>[0]>;
  try {
    module = await import(`@earendil-works/pi-ai/providers/${provider}`);
  } catch {
    throw new restate.TerminalError(`pi knows no model provider "${provider}": ${CHECK_MODEL}`);
  }
  // amazon-bedrock: amazonBedrockProvider
  const name = `${provider.replace(/-(\w)/g, (_, c: string) => c.toUpperCase())}Provider`;
  models.setProvider(module[name]!());
  return models;
}

/** `t` as a pi-durable tool. */
const native = (t: AnyTool, ctx: restate.ObjectContext): ToolRegistration =>
  defineTool({
    name: t.name,
    description: t.description,
    parameters: Type.Unsafe<Record<string, unknown>>(jsonSchemaOf(t)),
    replay: 'safe', // a crash runs it again, with the same call id
    // Aborted (the conversation's work, on a cancellation), it waits no more.
    execute: async (args, { callId }, task) => ({
      content: [{ type: 'text', text: await awaitWithContext(t.run(args, { ctx, callId }), task) }],
    }),
  });

/**
 * The tools of an agent, `system` added to its system prompt, and, with a
 * `workspace`, pi's coding tools working there, with the environment they
 * run in.
 */
async function extensions(tools: ToolRegistration[], system: string | undefined, workspace: string | undefined) {
  const registry = createRegistry();
  registry.install(defineExtension({ name: 'tools', tools }));
  if (system) registry.install(defineExtension({ name: 'system', sections: [section('system', () => system)] }));
  if (!workspace) return { registry, env: undefined };
  await mkdir(workspace, { recursive: true });
  registry.install(CodingTools);
  // Tell the model where the coding tools work.
  registry.install(defineExtension({ name: 'workspace', sections: [section('cwd', () => workspace)] }));
  return { registry, env: () => new NodeExecutionEnv({ cwd: workspace }) };
}

/**
 * The root conversation of `harness`, on `model`. It keeps the model it was
 * created with: on another one now (`model` or MODEL changed), it moves to
 * this one.
 */
async function rootOn(harness: Harness, context: Context, model: string): Promise<Conversation> {
  const [provider, modelId] = providerOf(model);
  const root = await harness.root(context, { agent: { model: { provider, modelId } } });
  const { model: current } = await root.agent(context);
  if (current?.provider !== provider || current.modelId !== modelId) {
    await root.configure({ model: { provider, modelId } }, context);
  }
  return root;
}

/** The error of prompt `id`, which ended unanswered for `reason`: the model's, if it is the model's. */
function unanswered(id: string, why: string | null | undefined, model: string) {
  const reason = why ?? 'unknown';
  const check = reason === 'model_error' || reason === 'no_model' ? ` (the model ${model}: ${CHECK_MODEL})` : '';
  return new restate.TerminalError(`prompt ${id} unanswered: ${reason}${check}`);
}

/**
 * Run `work(pi)` with a Harness over the object's database: `model` is the
 * model to use as `provider/model-id`, and `tools` the agent's tools. `pi`
 * has `prompt(text)`, and the `harness` and chord `context` for anything
 * else.
 */
export const withPi = async <T>(
  ctx: restate.ObjectContext,
  { model: named, models, system, tools = [], workspace, serviceTier }: Options,
  work: (pi: Pi) => Promise<T>,
): Promise<T> => {
  const model = modelOf(named);
  models ??= await modelsOf(providerOf(model)[0]);
  // A tool's error fails the attempt, which Restate retries (pi-durable would
  // hand it to the model): the work is cancelled, so the tool call stores no
  // result, and the retry runs it again with the same call id.
  const { context, cancel } = withCancel(BACKGROUND_CONTEXT);
  const { promise: failure, reject: failed } = Promise.withResolvers<never>();
  failure.catch(() => {});
  // A cancelled invocation, whenever it comes, aborts the conversation's
  // work instead (abortOn): pi-durable ends the turn, and the tool call with it.
  let cancelled: restate.CancelledError | undefined;
  const turn = cancellationOf(ctx);
  const onCancel = () => {
    cancelled = turn!.reason as restate.CancelledError;
    failed(cancelled);
  };
  turn?.addEventListener('abort', onCancel, { once: true });
  // The root conversation, once the Harness is open (below).
  const conversation = () => rootOn(harness, context, model);
  // Messages steered to the turn: given as pi-durable's steering input (by
  // its id, once), which the run takes once the round's tools are done.
  const steerable: Steerable = {
    steer: async ({ id, text }) => {
      await (await conversation()).submit({ type: 'input', content: text, requestId: id, whenBusy: 'steer' }, context);
    },
    has: async (id) => {
      const root = await conversation();
      return (await root.commit((tx) => tx.submissionByRequest(root.id, id), context)) !== undefined;
    },
  };
  // A tool call's error: the attempt fails, and the call never returns.
  const stopped = (error: unknown, task: Context): Promise<never> => {
    // The turn was cancelled, and the abort ended the call: pi-durable records it.
    if (cancelled) throw error;
    failed(error);
    if (error instanceof restate.CancelledError) {
      cancelled = error;
      return awaitWithContext(new Promise<never>(() => {}), task);
    }
    cancel(error);
    return awaitWithContext(new Promise<never>(() => {}), context);
  };
  const tracked = (t: ToolRegistration): ToolRegistration => ({
    ...t,
    execute: async (args, api, task) => {
      try {
        await toolCallStarted(ctx);
        const result = await t.execute(args, api, task);
        await toolCallEnded(ctx, steerable);
        return result;
      } catch (error) {
        return stopped(error, task);
      }
    },
  });
  const { registry, env } = await extensions(
    tools.map((t) => tracked(isTool(t) ? native(t, ctx) : t)),
    system,
    workspace,
  );
  // pi-durable hands its stream settings to pi-ai's requests as they are: serviceTier is pi-ai's.
  const settings = serviceTier ? { stream: { serviceTier } as ConversationStreamOptions } : undefined;
  const harness = await Harness.open(await openSqlKvStorage(ctx.db), { models, registry, env, settings }, context);

  const prompt = async (text: string, id = nextPromptId(ctx)) => {
    const root = await conversation();
    const submission = await root.submit({ type: 'input', content: text, requestId: id }, context);
    const settled = await submission.wait(context);
    // A prompt the cancellation's abort ended fails with the cancellation.
    if (settled.status !== 'done' || settled.answer === undefined)
      throw cancelled ?? unanswered(id, settled.reason, model);
    const { answer } = settled;
    const entry = await root.commit((tx) => tx.entry(AssistantEntry, answer), context);
    const reply = entry?.model?.at(-1);
    if (!reply) throw new restate.TerminalError(`prompt ${id}: no answer entry`);
    await settleSteering(ctx, steerable);
    return textOf(reply.content);
  };
  // `failure`, once it comes; a cancellation first aborts the conversation's work.
  const abortOn = async (): Promise<never> => {
    try {
      return await failure;
    } catch (error) {
      if (error instanceof restate.CancelledError) await (await conversation()).abort(context);
      throw error;
    }
  };

  try {
    // The log reads the root conversation through this Harness, while it is open.
    const read = async (transcript: Transcript) => transcribe(await harness.root(context), context, transcript);
    return await transcribing(ctx, read, () =>
      Promise.race([abortOn(), work({ harness, context, conversation, prompt })]),
    );
  } finally {
    turn?.removeEventListener('abort', onCancel);
    await harness.close(withoutAbortSignal(context));
  }
};

/** {@link prompt}, answering with the agent's text. */
const promptText = (ctx: restate.ObjectContext, text: string, options: Options): Promise<string> =>
  withPi(ctx, options, (session) => session.prompt(text));

/**
 * Answer `text`, once: a retried attempt resumes the same prompt. Each prompt
 * of an invocation is a prompt of its own.
 * With `output`, it answers with the value the agent replies with (see
 * output.ts), not its text.
 */
export const prompt = <O extends z.ZodObject | undefined = undefined>(
  ctx: restate.ObjectContext,
  text: string,
  options: Options<O> = {},
): Promise<Answer<O>> => withOutput(text, options, (t, o) => promptText(ctx, t, o)) as Promise<Answer<O>>;

/**
 * A chat handler: each message an invocation, the conversation in the
 * object's database, with `options`, or the options a function of the
 * invocation's context gives (each user's model, say). It takes the message
 * as plain text (`curl -d 'Hi'`) or as a JSON string (`curl --json '"Hi"'`).
 */
export const chat = <O extends z.ZodObject | undefined = undefined>(options: Configure<Options<O>> = {}) => {
  if (typeof options === 'object') checkModelName('pi', options.model);
  return chatHandler(async (ctx, message) => prompt(ctx, message, await configured(options, ctx)));
};

/**
 * A handler of the conversation as clients read it: a page at a time, or
 * what is new or changed since a cursor, from the log each change writes
 * (log.ts). It answers while a message is answered.
 */
export const messages = () => messagesHandler();

/** The conversation of `root`, from pi-durable's entries, as `messages` gives it. */
const transcribe = async (root: Conversation, context: Context, transcript: Transcript) => {
  for (let cursor: Parameters<typeof root.entries>[2]; ;) {
    const page = await root.entries({ order: 'ascending' }, 100, cursor, context);
    for (const m of page.items.flatMap((entry) => entry.model ?? [])) {
      if (m.role === 'user') transcript.say('user', textOf(m.content));
      if (m.role === 'assistant') {
        transcript.say('assistant', textOf(m.content));
        for (const c of m.content) if (c.type === 'toolCall') transcript.call(c.id, c.name, c.arguments);
      }
      if (m.role === 'toolResult') transcript.result(m.toolCallId, textOf(m.content), m.isError);
    }
    cursor = page.next;
    if (cursor === undefined) return;
  }
};

/** An agent: a virtual object `name`, one per key, whose `chat` handler is {@link chat}, and `messages` {@link messages}. */
export const agent = <P extends string, O extends z.ZodObject | undefined = undefined>({
  name,
  description,
  schema,
  background,
  ...options
}: Options<O> & Definition<P>) =>
  object({ name, description, schema, background, handlers: { chat: chat(options), messages: messages() } });
