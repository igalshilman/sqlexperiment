/**
 * A coder on OpenCode, made your own: a plugin of yours keeps its edits in
 * `src/` (OpenCode's permissions), and a handler of yours writes code, then
 * reviews and fixes it, as two prompts of one session.
 *
 * Every project is a key, `coder/<project>`, with its conversation in its
 * database and its files in a directory of its own. Coders run real
 * commands: use a sandbox.
 *
 *   ANTHROPIC_API_KEY=… WORKSPACE=/tmp/review node review.ts
 *   npx @restatedev/restate-server   # Restate, in another terminal
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:8080/coder/fizzbuzz/build -d 'Write fizzbuzz in TypeScript, with a test'
 *   curl localhost:8080/coder/fizzbuzz/chat -d 'Also write a README at the top'   # refused: not in src/
 *   curl localhost:8080/coder/fizzbuzz/messages
 */
import type * as sql from 'restate-sql';
import type { Plugin } from '@opencode/plugin';
import { chatHandler, object, serve } from 'restate-agents';
import * as opencode from 'restate-agents/opencode';

const workspace = process.env.WORKSPACE ?? '/tmp/review';

/**
 * An OpenCode plugin: an edit outside `src/` is denied, and the model is told
 * why. OpenCode asks its permissions before each edit (`write`, `edit`, a
 * patch), with the files' paths in the workspace.
 */
const onlySrc: Plugin.Plugin = {
  id: 'review.only-src',
  async setup(ctx) {
    const hook = await ctx.permission.hook('evaluate', (request) => {
      if (request.action === 'edit' && request.resources.some((path) => !path.startsWith('src/'))) {
        request.effect = 'deny';
        request.message = 'Edit only files under src/.';
      }
    });
    return () => hook.dispose();
  },
};

/** The options of a project's invocations: its directory, and the plugin. */
const options = (ctx: sql.ObjectContext): opencode.Options => ({
  workspace: `${workspace}/${ctx.key}`,
  plugins: [onlySrc],
});

const coder = object({
  name: 'coder',
  handlers: {
    chat: opencode.chat(options),
    messages: opencode.messages(),
    /**
     * Write `task`, then review it: two prompts of one session, each
     * answered once, however often the handler runs. The answer is the
     * review's.
     */
    build: chatHandler((ctx, task) =>
      opencode.withOpenCode(ctx, options(ctx), async ({ prompt }) => {
        await prompt(task);
        return prompt('Review what you wrote: run the tests, fix what fails, and say what you changed.');
      }),
    ),
  },
});

await serve(coder);
