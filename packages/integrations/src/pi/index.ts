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
// The model is `model`, MODEL, or else anthropic/claude-sonnet-5-5, with the
// provider's API key (such as ANTHROPIC_API_KEY).
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
import * as restate from '@restatedev/restate-sdk';
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
  type ToolRegistration,
} from '@earendil-works/pi-durable';
import { NodeExecutionEnv } from '@earendil-works/pi-durable/env/node';
import { CodingTools } from '@earendil-works/pi-durable/tools';
import {
  CHECK_MODEL,
  chatHandler,
  checkModelName,
  type Configure,
  configured,
  messagesHandler,
  modelOf,
  nextPromptId,
} from '../common.ts';
import { object } from '../object.ts';
import { isTool, type Tool as AnyTool } from '../tools.ts';
import { z } from 'zod';
import { openSqlKvStorage } from './storage.ts';

/** What `work` gets: `prompt(text)`, the `conversation()`, and the `harness` and chord `context` for anything else. */
export interface Pi {
  harness: Harness;
  context: Context;
  /** Answer `text`, once per prompt `id`, with the answer's text. By default, each prompt of an invocation has an id of its own, the same on every attempt. */
  prompt(text: string, id?: string): Promise<string>;
  /** The object's conversation, on the model: for `reset`, `compact`, `entries` and the like. */
  conversation: () => Promise<Conversation>;
}

/** The text of a message's content. */
const textOf = (content: Message['content']): string =>
  typeof content === 'string' ? content : content.flatMap((c) => (c.type === 'text' ? [c.text] : [])).join('');

/** The default model, as `provider/model-id`. */
export const DEFAULT_MODEL = 'anthropic/claude-sonnet-5-5';

/** An agent's options: `chat`, `prompt` and `withPi` take them. */
export interface Options {
  /** The model, as `provider/model-id`: MODEL, or else {@link DEFAULT_MODEL}. */
  model?: string;
  /** A pi-ai model registry; by default, one of the model's provider. */
  models?: MutableModels;
  /** The system prompt: who the agent is, and how it answers. */
  system?: string;
  /** The agent's tools: `tool()`s, Restate's (`call`, `signal`...), or pi-durable's `defineTool`s. */
  tools?: (ToolRegistration | AnyTool)[];
  /** A directory for the coding tools, made if missing; without one, the agent has no file system. */
  workspace?: string;
}

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
const native = (t: AnyTool, ctx: restate.SqlObjectContext): ToolRegistration =>
  defineTool({
    name: t.name,
    description: t.description,
    parameters: Type.Unsafe<Record<string, unknown>>(z.toJSONSchema(t.input)),
    replay: 'safe', // a crash runs it again, with the same call id
    execute: async (args, { callId }) => ({
      content: [{ type: 'text', text: await t.run(t.input.parse(args), { ctx, callId }) }],
    }),
  });

/**
 * Run `work(pi)` with a Harness over the object's database: `model` is the
 * model to use as `provider/model-id`, and `tools` the agent's tools. `pi`
 * has `prompt(text)`, and the `harness` and chord `context` for anything
 * else.
 */
export const withPi = async <T>(
  ctx: restate.SqlObjectContext,
  { model: named, models, system, tools = [], workspace }: Options,
  work: (pi: Pi) => Promise<T>,
): Promise<T> => {
  const db = ctx.db;
  const model = modelOf(named, DEFAULT_MODEL);
  const [provider = '', modelId = ''] = model.split('/');
  models ??= await modelsOf(provider);
  // A tool's error fails the attempt, which Restate retries (pi-durable would
  // hand it to the model): the work is cancelled, so the tool call stores no
  // result, and the retry runs it again with the same call id.
  const { context, cancel } = withCancel(BACKGROUND_CONTEXT);
  const { promise: failure, reject: failed } = Promise.withResolvers<never>();
  failure.catch(() => {});
  const failing = (t: ToolRegistration): ToolRegistration => ({
    ...t,
    execute: (...args) =>
      t.execute(...args).catch((error: unknown) => {
        failed(error);
        cancel(error);
        return awaitWithContext(new Promise<never>(() => {}), context);
      }),
  });
  const registry = createRegistry();
  registry.install(
    defineExtension({ name: 'tools', tools: tools.map((t) => failing(isTool(t) ? native(t, ctx) : t)) }),
  );
  if (system) registry.install(defineExtension({ name: 'system', sections: [section('system', () => system)] }));
  let env: (() => NodeExecutionEnv) | undefined;
  if (workspace) {
    await mkdir(workspace, { recursive: true });
    env = () => new NodeExecutionEnv({ cwd: workspace });
    registry.install(CodingTools);
    // Tell the model where the coding tools work.
    registry.install(defineExtension({ name: 'workspace', sections: [section('cwd', () => workspace)] }));
  }
  const harness = await Harness.open(await openSqlKvStorage(db), { models, registry, env }, context);
  try {
    // The root conversation keeps the model it was created with: on another
    // one now (`model` or MODEL changed), it moves to this one.
    const conversation = async () => {
      const root = await harness.root(context, { agent: { model: { provider, modelId } } });
      const { model: current } = await root.agent(context);
      if (current?.provider !== provider || current.modelId !== modelId) {
        await root.configure({ model: { provider, modelId } }, context);
      }
      return root;
    };
    return await Promise.race([
      failure,
      work({
        harness,
        context,
        conversation,
        async prompt(text, id = nextPromptId(ctx)) {
          const root = await conversation();
          const submission = await root.submit({ type: 'input', content: text, requestId: id }, context);
          const settled = await submission.wait(context);
          if (settled.status !== 'done' || settled.answer === undefined) {
            const reason = settled.reason ?? 'unknown';
            const check =
              reason === 'model_error' || reason === 'no_model' ? ` (the model ${model}: ${CHECK_MODEL})` : '';
            throw new restate.TerminalError(`prompt ${id} unanswered: ${reason}${check}`);
          }
          const { answer } = settled;
          const entry = await root.commit((tx) => tx.entry(AssistantEntry, answer), context);
          const reply = entry?.model?.at(-1);
          if (!reply) throw new restate.TerminalError(`prompt ${id}: no answer entry`);
          return textOf(reply.content);
        },
      }),
    ]);
  } finally {
    await harness.close(withoutAbortSignal(context));
  }
};

/**
 * Answer `text`, once: a retried attempt resumes the same prompt. Each prompt
 * of an invocation is a prompt of its own.
 */
export const prompt = (ctx: restate.SqlObjectContext, text: string, options: Options = {}) =>
  withPi(ctx, options, (session) => session.prompt(text));

/**
 * A chat handler: each message an invocation, the conversation in the
 * object's database, with `options`, or the options a function of the
 * invocation's context gives (each user's model, say). It takes the message
 * as plain text (`curl -d 'Hi'`) or as a JSON string (`curl --json '"Hi"'`).
 */
export const chat = (options: Configure<Options> = {}) => {
  if (typeof options === 'object') checkModelName('pi', options.model);
  return chatHandler(async (ctx, message) => prompt(ctx, message, await configured(options, ctx)));
};

/**
 * A handler of the conversation, a page at a time: what was said, in order,
 * with the tool calls, from pi-durable's entries. It answers while a message
 * is answered, with the entries committed so far.
 */
export const messages = () =>
  messagesHandler(async (ctx, transcript) => {
    const context = BACKGROUND_CONTEXT;
    // Reading needs no model, nor MODEL's provider: an empty registry.
    const options = { models: createModels(), registry: createRegistry() };
    const harness = await Harness.open(await openSqlKvStorage(ctx.db), options, context);
    try {
      const root = await harness.root(context);
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
    } finally {
      await harness.close(context);
    }
  });

/** An agent: a virtual object `name`, one per key, whose `chat` handler is {@link chat}, and `messages` {@link messages}. */
export const agent = <P extends string>({
  name,
  description,
  ...options
}: Options & { name: P; description?: string }) =>
  object({ name, description, handlers: { chat: chat(options), messages: messages() } });
