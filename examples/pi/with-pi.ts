// withPi: a pi-durable Harness for one Restate invocation, with its storage in
// the virtual object's database.
//
//   prompt: (ctx, { id, text }) =>
//     withPi(ctx, { models, model: 'anthropic/claude-sonnet-4-5', tools }, (pi) => pi.prompt(id, text)),
//
// It runs `work` in one `ctx.sqlSession`: pi-durable's storage is the object's
// database (pi-storage.ts), and each pi-durable commit is an SQL transaction
// in it, stored in order with everything else the handler does. The Harness
// is opened with `tools` (made with pi-durable's `defineTool`) and closed at
// the end. A retried attempt (a crash, a lost connection) opens it again and
// carries on: a prompt is submitted once, by its id (pi-durable's requestId),
// an answered prompt returns its answer, and unfinished work resumes: a model
// call is made again, and a tool call declared `replay: 'safe'` runs again
// with the same call id, which a tool passes on to stay idempotent.
import * as restate from '@restatedev/restate-sdk';
import type { Context } from '@earendil-works/chord';
import { BACKGROUND_CONTEXT } from '@earendil-works/chord/context';
import type { MutableModels } from '@earendil-works/pi-ai';
import {
  AssistantEntry,
  createRegistry,
  defineExtension,
  Harness,
  type ToolRegistration,
} from '@earendil-works/pi-durable';
import { textOf } from './utils.ts';
import { openSqlKvStorage } from './pi-storage.ts';

/** What `work` gets: `prompt(id, text)`, and the `harness` and chord `context` for anything else. */
export interface Pi {
  harness: Harness;
  context: Context;
  /** Answer `text`, once per prompt `id`, with the answer's text. */
  prompt(id: string, text: string): Promise<string>;
}

export interface PiOptions {
  /** A pi-ai model registry. */
  models: MutableModels;
  /** The model to use, as `provider/model-id`. */
  model: string;
  /** The agent's tools, made with pi-durable's `defineTool`. */
  tools?: ToolRegistration[];
}

/**
 * Run `work(pi)` with a Harness over the object's database: `models` is a
 * pi-ai model registry, `model` the model to use as `provider/model-id`, and
 * `tools` the agent's tools. `pi` has `prompt(id, text)`, and the `harness`
 * and chord `context` for anything else.
 */
export const withPi = <T>(
  ctx: restate.UnjournaledObjectContext,
  { models, model, tools = [] }: PiOptions,
  work: (pi: Pi) => Promise<T>,
): Promise<T> =>
  ctx.sqlSession(async (db) => {
    const [provider = '', modelId = ''] = model.split('/');
    const context = BACKGROUND_CONTEXT;
    const registry = createRegistry();
    registry.install(defineExtension({ name: 'tools', tools }));
    const harness = await Harness.open(await openSqlKvStorage(db), { models, registry }, context);
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
  });
