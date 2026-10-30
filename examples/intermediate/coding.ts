/**
 * A coding team: you chat with a coordinator (pi), which hands each coding
 * task to a coder of its own (OpenCode).
 *
 * Every coder is an agent of its own, `coder/<you>.<task>`, with its own
 * database and its own directory. Tasks run at once, and each picks up where
 * it left off after a crash. Coders run real commands: use a sandbox.
 *
 *   OPENAI_API_KEY=… WORKSPACE=/tmp/coding node intermediate/coding.ts
 *   npx @restatedev/restate-server   # Restate, in another terminal
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:8080/coordinator/alice/chat -d 'Write fizzbuzz.py and primes.py, and run them'
 */
import { serve, subagent } from 'restate-agents';
import * as opencode from 'restate-agents/opencode';
import * as pi from 'restate-agents/pi';

const workspace = process.env.WORKSPACE ?? '/tmp/coding';

const coder = opencode.agent({
  name: 'coder',
  description: 'A coding agent, with files of its own',
  workspace: (ctx) => `${workspace}/${ctx.key}`, // a directory of its own
});

const coordinator = pi.agent({
  name: 'coordinator',
  tools: [subagent(coder)],
});

await serve(coordinator, coder);
