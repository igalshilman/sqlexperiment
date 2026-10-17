// pi agents in Restate virtual objects: a pi-durable Harness for each
// invocation, with its storage in the object's database.
//
//   const assistant = pi.agent({ name: 'assistant', tools: [weather] });
//   const lead = pi.agent({ name: 'lead', tools: [pi.subagent(coder)] });
//
// or a handler of an object of your own: `handlers: { chat: pi.chat() }`
//
// or, for more than a prompt:
//
//   chat: (ctx, message) => withPi(ctx, { tools }, (pi) => pi.prompt(ctx.request().id, message)),
//
// The model is `model`, PI_MODEL, or else anthropic/claude-sonnet-5-5, with
// the provider's API key (such as ANTHROPIC_API_KEY).
//
// It runs `work` over `ctx.db`: pi-durable's storage is the object's
// database (storage.ts), and each pi-durable commit is an SQL transaction
// in it, stored in order with everything else the handler does. The Harness
// is opened with `tools` (made with pi-durable's `defineTool`) and closed at
// the end. A retried attempt (a crash, a lost connection) opens it again and
// carries on: a prompt is submitted once, by its id (pi-durable's requestId),
// an answered prompt returns its answer, and unfinished work resumes: a model
// call is made again, and a tool call declared `replay: 'safe'` runs again
// with the same call id, which a tool passes on to stay idempotent.
//
// With a `workspace` directory, the agent also gets pi-durable's coding
// tools (`read`, `write`, `edit`, `bash`) over it, on the local file system.
// The files are not in the database: they last as long as the disk does,
// and a coding tool cut off by a crash is not run again (the model is told
// it was interrupted).
import * as restate from '@restatedev/restate-sdk';
import type { Context } from '@earendil-works/chord';
import { BACKGROUND_CONTEXT } from '@earendil-works/chord/context';
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
  type ToolRegistration,
} from '@earendil-works/pi-durable';
import { NodeExecutionEnv } from '@earendil-works/pi-durable/env/node';
import { CodingTools } from '@earendil-works/pi-durable/tools';
import { forCall, type PerCall } from '../common.ts';
import { object } from '../object.ts';
import { openSqlKvStorage } from './storage.ts';

/** What `work` gets: `prompt(id, text)`, and the `harness` and chord `context` for anything else. */
export interface Pi {
  harness: Harness;
  context: Context;
  /** Answer `text`, once per prompt `id`, with the answer's text. */
  prompt(id: string, text: string): Promise<string>;
}

/** The text of a message's content. */
const textOf = (content: Message['content']): string =>
  typeof content === 'string' ? content : content.flatMap((c) => (c.type === 'text' ? [c.text] : [])).join('');

/** The default model, as `provider/model-id`. */
export const DEFAULT_MODEL = 'anthropic/claude-sonnet-5-5';

export interface PiOptions {
  /** The model, as `provider/model-id`: PI_MODEL, or else {@link DEFAULT_MODEL}. */
  model?: string;
  /** A pi-ai model registry; by default, one of the model's provider. */
  models?: MutableModels;
  /** The agent's tools, made with pi-durable's `defineTool`. */
  tools?: ToolRegistration[];
  /** A directory for the coding tools, made if missing; without one, the agent has no file system. */
  workspace?: string;
}

/** A registry of `provider`'s models, such as anthropicProvider() from @earendil-works/pi-ai/providers/anthropic. */
async function modelsOf(provider: string): Promise<MutableModels> {
  const models = createModels();
  const module = await import(`@earendil-works/pi-ai/providers/${provider}`);
  models.setProvider(module[`${provider}Provider`]());
  return models;
}

/**
 * Run `work(pi)` with a Harness over the object's database: `model` is the
 * model to use as `provider/model-id`, and `tools` the agent's tools. `pi`
 * has `prompt(id, text)`, and the `harness` and chord `context` for anything
 * else.
 */
export const withPi = async <T>(
  ctx: restate.SqlObjectContext,
  { model = process.env.PI_MODEL || DEFAULT_MODEL, models, tools = [], workspace }: PiOptions,
  work: (pi: Pi) => Promise<T>,
): Promise<T> => {
  const db = ctx.db;
  const [provider = '', modelId = ''] = model.split('/');
  models ??= await modelsOf(provider);
  const context = BACKGROUND_CONTEXT;
  const registry = createRegistry();
  registry.install(defineExtension({ name: 'tools', tools }));
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
    return await work({
      harness,
      context,
      async prompt(id, text) {
        const root = await harness.root(context, { agent: { model: { provider, modelId } } });
        const submission = await root.submit({ type: 'input', content: text, requestId: id }, context);
        const settled = await submission.wait(context);
        if (settled.status !== 'done' || settled.answer === undefined) {
          throw new restate.TerminalError(`prompt ${id} unanswered: ${settled.reason ?? 'unknown'}`);
        }
        const { answer } = settled;
        const entry = await root.commit((tx) => tx.entry(AssistantEntry, answer), context);
        const reply = entry?.model?.at(-1);
        if (!reply) throw new restate.TerminalError(`prompt ${id}: no answer entry`);
        return textOf(reply.content);
      },
    });
  } finally {
    await harness.close(context);
  }
};

/** Answer `text`, once per invocation: a retried attempt resumes the same prompt. */
export const prompt = (ctx: restate.SqlObjectContext, text: string, options: PiOptions = {}) =>
  withPi(ctx, options, (pi) => pi.prompt(ctx.request().id, text));

export interface ChatOptions {
  /** The model, as `provider/model-id`: PI_MODEL, or else {@link DEFAULT_MODEL}. */
  model?: string;
  /** The agent's tools, each made with pi-durable's `defineTool`, or for an invocation. */
  tools?: PerCall<ToolRegistration>[];
  /** A directory for the coding tools (see {@link PiOptions.workspace}). */
  workspace?: PerCall<string | undefined>;
}

/** A chat handler: each message an invocation, the conversation in the object's database. */
export const chat =
  ({ model, tools = [], workspace }: ChatOptions = {}) =>
  (ctx: restate.SqlObjectContext, message: string): Promise<string> =>
    prompt(ctx, message, {
      model,
      tools: tools.map((tool) => forCall(tool, ctx)),
      workspace: workspace === undefined ? undefined : forCall(workspace, ctx),
    });

/**
 * A tool, named after `agent` (an object, such as one of {@link agent}), that
 * hands a task to it: to the object keyed `<this key>.<task name>`, through
 * its `handler` (`chat`). Tasks of different names run at once; the same name
 * continues the same task. A call is made once per tool call, however many
 * attempts run it.
 */
export const subagent =
  (agent: { name: string }, { description = `the ${agent.name} agent`, handler = 'chat' } = {}) =>
  (ctx: restate.SqlObjectContext): ToolRegistration =>
    defineTool({
      name: agent.name,
      description:
        `Hand a task to ${description}, and get its answer. ` +
        'Tasks of different names run at once; the same name continues the same task.',
      parameters: Type.Object({
        name: Type.String({ description: 'A short name for the task, such as "fizzbuzz"', pattern: '^[a-z0-9-]+$' }),
        task: Type.String({ description: 'The task in full: the agent sees nothing else' }),
      }),
      replay: 'safe', // a crash runs it again, with the same call id: the same call
      execute: async ({ name, task }, { callId }) => {
        const answer = await ctx.genericCall<string, string>({
          service: agent.name,
          key: `${ctx.key}.${name}`,
          method: handler,
          parameter: task,
          inputSerde: restate.serde.json,
          outputSerde: restate.serde.json,
          idempotencyKey: `${ctx.request().id}:${callId}`,
        });
        return { content: [{ type: 'text', text: answer }] };
      },
    });

/** An agent: a virtual object `name`, one per key, whose `chat` handler is {@link chat}. */
export const agent = <P extends string>({
  name,
  description,
  ...options
}: ChatOptions & { name: P; description?: string }) => object({ name, description, handlers: { chat: chat(options) } });
