// A coding team: you chat with a coordinator (pi), which hands each coding
// task to a coding agent of its own (OpenCode). Each agent is an object with
// its conversation in its own SQL database, and each coder works in its own
// directory. A crash resumes where it left off.
//
//   ANTHROPIC_API_KEY=… node coding/coding.ts
//   curl localhost:8080/coordinator/alice/chat --json '"Write fizzbuzz.py and primes.py, and run them"'
//
// Coders work on the local file system, in $WORKSPACE/<task> (a temporary
// directory without WORKSPACE): run it in a sandbox.
import * as restate from '@restatedev/restate-sdk';
import { Type } from '@earendil-works/pi-ai';
import { defineTool } from '@earendil-works/pi-durable';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { prompt as code } from '../opencode/with-opencode.ts';
import { prompt } from '../pi/with-pi.ts';

export const coder = restate.object({
  name: 'coder',
  handlers: {
    code: (ctx: restate.SqlObjectContext, task: string) =>
      code(ctx, task, { directory: join(process.env.WORKSPACE ?? tmpdir(), ctx.key) }),
  },
  options: restate.sqlOptions,
});

export const coordinator = restate.object({
  name: 'coordinator',
  handlers: {
    chat: (ctx: restate.SqlObjectContext, message: string) => prompt(ctx, message, { tools: [delegate(ctx)] }),
  },
  options: restate.sqlOptions,
});

/** The coordinator's tool: a task to the coder of its name, which runs at once with the others. */
const delegate = (ctx: restate.SqlObjectContext) =>
  defineTool({
    name: 'code',
    description:
      'Hand a coding task to a coding agent with files of its own, and get its report. ' +
      'Tasks of different names run at once; the same name continues the same task.',
    parameters: Type.Object({
      name: Type.String({ description: 'A short name for the task, such as "fizzbuzz"', pattern: '^[a-z0-9-]+$' }),
      task: Type.String({ description: 'The task in full: the coding agent sees nothing else' }),
    }),
    replay: 'safe', // a crash runs it again, with the same call id: the same call to the coder
    execute: async ({ name, task }, { callId }) => {
      const report = await ctx
        .objectClient(coder, `${ctx.key}.${name}`)
        .code(task, restate.rpc.opts({ idempotencyKey: `${ctx.request().id}:${callId}` }));
      return { content: [{ type: 'text', text: report }] };
    },
  });

if (import.meta.main) await restate.serve({ services: [coordinator, coder] });
