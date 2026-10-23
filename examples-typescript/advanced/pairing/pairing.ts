/**
 * A pair programmer you talk with about a project, for as long as the project
 * lasts: each project (the object's key) has a directory that keeps its
 * files, a plan in its database, and a conversation that goes on from one
 * message to the next. The agent's loop runs here, in Restate: each step of
 * it is stored, so a crash carries on from the last one, mid-message.
 *
 *   ANTHROPIC_API_KEY=… node advanced/pairing/pairing.ts
 *   npx @restatedev/restate-server   # Restate, in another terminal
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   node advanced/pairing/pair.ts fizzbuzz       # in another terminal: talk with it
 *
 * Or with curl:
 *
 *   curl localhost:8080/pair/fizzbuzz/chat/send -d 'Write fizzbuzz.py, with tests, and ship it'
 *   curl localhost:8080/pair/fizzbuzz/tasks      # the plan, as it works
 *   curl localhost:8080/pair/fizzbuzz/steer -d 'Use pytest'
 *   curl localhost:8080/pair/fizzbuzz/signal --json '{"name": "approval", "value": "approved"}'
 *   curl localhost:8080/pair/fizzbuzz/messages
 *
 * The pair's coding tools run real commands, on this machine, in
 * WORKSPACE/<project> (/tmp/restate-pairing by default): run it in a sandbox
 * (a container, a VM). REVIEWER sets the reviewer's model (Claude Opus by
 * default).
 */
import { handlers } from 'restate-fns';
import { object, serve, signal, subagent, workflow } from 'restate-agents';
import * as claude from 'restate-agents/claude';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';
import { Change, shipping } from './shipping.ts';
import { done, plan, PLAN, tasksOf } from './tools.ts';

const workspace = process.env.WORKSPACE ?? '/tmp/restate-pairing';

const reviewer = claude.agent({
  name: 'reviewer',
  model: process.env.REVIEWER ?? 'anthropic/claude-opus-5-5',
  system:
    'You review code changes. Read the diff you are given, and answer with what to fix, ' +
    'most important first: bugs, missing tests, unclear code. If there is nothing to fix, say so.',
});

const approval = signal('approval', {
  description: 'Ask the person to approve a change before you ship it, and wait: approved, or what to change.',
  input: z.object({
    title: z.string().describe('The change, in a line'),
    summary: z.string().describe('What changed and why, and what the review said'),
  }),
});

const SYSTEM = `You are a pair programmer, working with a person on their project, in its directory.
- Before you change anything, set a plan with \`plan\`, and mark each task \`done\` once it is.
- Read a file before you change it, and run the tests after each change.
- Keep the project a git repository (\`git init\` it if it is none), and commit each change that works.
- Once the work is done, hand its diff to the reviewer, and fix what the review finds.
- Ship only what the person approved: ask with \`wait_for_approval\`, then ship with \`shipping\`.
Answer briefly: what you did, and what is next.`;

const pair = object({
  name: 'pair',
  schema: PLAN,
  handlers: {
    // Each project's own directory: options for each invocation.
    chat: pi.chat((ctx) => ({
      system: SYSTEM,
      workspace: `${workspace}/${ctx.key}`,
      tools: [
        plan,
        done,
        subagent(reviewer, { description: 'a reviewer, who reads a diff and says what to fix' }),
        approval,
        workflow(shipping, {
          description: 'Ship an approved change: its checks run, then its pull request opens.',
          input: Change,
        }),
      ],
    })),
    messages: pi.messages(),

    /** The plan: shared, so it answers while the pair works. */
    tasks: handlers.object.shared(async (ctx) => tasksOf(ctx.db)),
  },
});

await serve(pair, reviewer, shipping);
