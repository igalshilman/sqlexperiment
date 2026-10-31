/**
 * 16 · pi-durable by hand
 *
 * What `pi.prompt` (lesson 09) does, written out with pi-durable itself: for
 * what restate-agents' pi leaves out, such as extensions of your own,
 * compaction or forks. Three things make pi-durable durable in a handler:
 *
 * - Its storage is the key's database: `openSqlKvStorage(ctx.db)`. Each
 *   pi-durable commit is an SQL transaction there, stored with the object.
 * - The prompt is submitted with the invocation's id as its `requestId`. A
 *   retried attempt (a crash, a lost connection) opens the Harness again and
 *   finds the same submission, finished or still running, rather than ask the
 *   model twice.
 * - `restartLimit: Infinity`: work a crash cut off runs again however often.
 *   Restate's retry policy decides when to stop, not pi-durable (which would
 *   park it after 5 crashes in a row, and the prompt would never be answered).
 *
 * The Harness is opened for the attempt and closed at its end. The
 * conversation stays in the database: each message to the same key goes on
 * with it.
 *
 *   node src/16_pi_durable.ts                    # and register it (see 01)
 *   curl localhost:8080/poet/alice/write --json '"the sea"'
 *   curl localhost:8080/poet/alice/write --json '"Again, shorter."'   # the same conversation
 */
import { BACKGROUND_CONTEXT } from '@earendil-works/chord/context';
import { createModels } from '@earendil-works/pi-ai/models';
import { openaiProvider } from '@earendil-works/pi-ai/providers/openai';
import { AssistantEntry, createRegistry, defineExtension, Harness, section } from '@earendil-works/pi-durable';
import { TerminalError } from 'restate-session-objects';
import { object, serve } from 'restate-agents';
import { openSqlKvStorage } from 'restate-agents/pi/storage';

// The models the Harness may ask (OpenAI's, with OPENAI_API_KEY), and its
// extensions: here, one that adds the system prompt.
const models = createModels();
models.setProvider(openaiProvider());
const registry = createRegistry();
registry.install(defineExtension({ name: 'poet', sections: [section('system', () => 'You write short poems.')] }));

const poet = object({
  name: 'poet',
  handlers: {
    write: async (ctx, request: string) => {
      const context = BACKGROUND_CONTEXT;
      const storage = await openSqlKvStorage(ctx.db);
      const harness = await Harness.open(storage, { models, registry, settings: { restartLimit: Infinity } }, context);
      try {
        const root = await harness.root(context, { agent: { model: { provider: 'openai', modelId: 'gpt-6-sol' } } });
        // Once per invocation: a retry finds this submission again, by its id.
        const submission = await root.submit({ type: 'input', content: request, requestId: ctx.request.id }, context);
        const { status, answer, reason } = await submission.wait(context);
        if (status !== 'done' || answer === undefined) throw new TerminalError(`unanswered: ${reason ?? status}`);
        // The answer is an entry of the conversation: the model's last message.
        const entry = await root.commit((tx) => tx.entry(AssistantEntry, answer), context);
        const reply = entry?.model?.at(-1)?.content ?? '';
        return typeof reply === 'string'
          ? reply
          : reply.flatMap((part) => (part.type === 'text' ? [part.text] : [])).join('');
      } finally {
        await harness.close(context);
      }
    },
  },
});

await serve(poet);
