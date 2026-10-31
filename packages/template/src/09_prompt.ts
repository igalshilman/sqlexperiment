/**
 * 09 · A model in a handler
 *
 * `pi.prompt(ctx, text)` asks a model from a handler. It answers once: the
 * conversation is kept in the key's database, and a retry resumes it instead
 * of asking again. Each key is a conversation of its own: here, one per
 * language. `object` comes from restate-agents: restate-objects' object, set
 * up for agents.
 *
 *   node src/09_prompt.ts                                                      # and register it (see 01)
 *   curl localhost:8080/translator/French/translate --json '"Good morning!"'   # "Bonjour !"
 *   curl localhost:8080/translator/Japanese/translate --json '"Good morning!"'
 */
import { object, serve } from 'restate-agents';
import * as pi from 'restate-agents/pi';

const translator = object({
  name: 'translator',
  handlers: {
    translate: async (ctx, text: string) =>
      pi.prompt(ctx, `Translate into ${ctx.key}. Answer with the translation only.\n\n${text}`),
  },
});

await serve(translator);
