// withPi: a pi-durable Harness for one Restate invocation, with its storage in
// the virtual object's database.
//
//   prompt: (ctx, { id, text }) =>
//     withPi(ctx, { models, model: 'anthropic/claude-sonnet-4-5', tools }, (pi) => pi.prompt(id, text)),
//
// It runs `work` in one `ctx.sqlSession`: pi-durable's storage is the object's
// database (pi-storage.mjs), and each pi-durable commit is an SQL transaction
// in it, stored in order with everything else the handler does. The Harness
// is opened with `tools` (made with pi-durable's `defineTool`) and closed at
// the end. A retried attempt (a crash, a lost connection) opens it again and
// carries on: a prompt is submitted once, by its id (pi-durable's requestId),
// an answered prompt returns its answer, and unfinished work resumes: a model
// call is made again, and a tool call declared `replay: 'safe'` runs again
// with the same call id, which a tool passes on to stay idempotent.
import * as restate from '@restatedev/restate-sdk';
import { BACKGROUND_CONTEXT } from '@earendil-works/chord/context';
import { AssistantEntry, createRegistry, defineExtension, Harness } from '@earendil-works/pi-durable';
import { openSqlKvStorage } from './pi-storage.mjs';

/**
 * Run `work(pi)` with a Harness over the object's database: `models` is a
 * pi-ai model registry, `model` the model to use as `provider/model-id`, and
 * `tools` the agent's tools. `pi` has `prompt(id, text)`, and the `harness`
 * and chord `context` for anything else.
 */
export const withPi = (ctx, { models, model, tools = [] }, work) =>
  ctx.sqlSession(async (db) => {
    const [provider, modelId] = model.split('/');
    const context = BACKGROUND_CONTEXT;
    const registry = createRegistry();
    registry.install(defineExtension({ name: 'tools', tools }));
    const harness = await Harness.open(await openSqlKvStorage(db), { models, registry }, context);
    try {
      return await work({
        harness,
        context,
        /** Answer `text`, once per prompt `id`, with the answer's text. */
        async prompt(id, text) {
          const root = await harness.root(context, { agent: { model: { provider, modelId } } });
          const submission = await root.submit({ type: 'input', content: text, requestId: id }, context);
          const settled = await submission.wait(context);
          if (settled.status !== 'done') {
            throw new restate.TerminalError(`prompt ${id} unanswered: ${settled.reason ?? 'unknown'}`);
          }
          const answer = await root.commit((tx) => tx.entry(AssistantEntry, settled.answer), context);
          return textOf(answer.model.at(-1).content);
        },
      });
    } finally {
      await harness.close(context);
    }
  });

const textOf = (content) =>
  typeof content === 'string'
    ? content
    : content
        .filter((c) => c.type === 'text')
        .map((c) => c.text)
        .join('');
