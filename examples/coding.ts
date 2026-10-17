/**
 * A coding team: you chat with a coordinator (pi), which hands each coding
 * task to a coder of its own (OpenCode).
 *
 * Every coder is an object of its own, `coder/<you>.<task>`, with its own
 * database and its own directory. Tasks run at once, and each picks up where
 * it left off after a crash. Coders run real commands: use a sandbox.
 *
 *   ANTHROPIC_API_KEY=… WORKSPACE=/tmp/coding node coding.ts
 *   restate deployments register http://localhost:9080
 *   curl localhost:8080/coordinator/alice/chat --json '"Write fizzbuzz.py and primes.py, and run them"'
 */
import * as restate from '@restatedev/restate-sdk';
import * as opencode from 'restate-agents/opencode';
import * as pi from 'restate-agents/pi';

const workspace = process.env.WORKSPACE ?? '/tmp/coding';

const coder = restate.object({
  name: 'coder',
  handlers: {
    code: opencode.chat({ directory: (ctx) => `${workspace}/${ctx.key}` }),
  },
  options: restate.sqlOptions,
});

const coordinator = restate.object({
  name: 'coordinator',
  handlers: {
    chat: pi.chat({
      tools: [pi.subagent(coder, 'code', 'a coding agent with files of its own')],
    }),
  },
  options: restate.sqlOptions,
});

await restate.serve({ services: [coordinator, coder] });
