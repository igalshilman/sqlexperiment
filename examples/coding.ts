/**
 * A coding team: you chat with a coordinator (pi), which hands each coding
 * task to a coder of its own (OpenCode).
 *
 * Every coder is an agent of its own, `coder/<you>.<task>`, with its own
 * database and its own directory. Tasks run at once, and each picks up where
 * it left off after a crash. Coders run real commands: use a sandbox.
 *
 *   ANTHROPIC_API_KEY=… WORKSPACE=/tmp/coding node coding.ts
 *   restate deployments register http://localhost:9080
 *   curl localhost:8080/coordinator/alice/chat --json '"Write fizzbuzz.py and primes.py, and run them"'
 */
import { serve } from 'restate-agents';
import * as opencode from 'restate-agents/opencode';
import * as pi from 'restate-agents/pi';

const workspace = process.env.WORKSPACE ?? '/tmp/coding';

const coder = opencode.agent({
  name: 'coder',
  directory: (ctx) => `${workspace}/${ctx.key}`,
});

const coordinator = pi.agent({
  name: 'coordinator',
  tools: [pi.subagent(coder, { description: 'a coding agent with files of its own' })],
});

await serve(coordinator, coder);
