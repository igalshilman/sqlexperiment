/**
 * Agents as steps of a workflow: a support ticket is classified by one agent,
 * a reply is drafted by another, and a person reviews it before it goes out.
 * The workflow calls the agents like any Restate handler, and waits for the
 * review without holding a process.
 *
 *   ANTHROPIC_API_KEY=… node triage.ts
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:8080/triage/t1/run/send --json '"I was charged twice for order 103."'
 *   curl localhost:8080/triage/t1/draft                               # the drafted reply
 *   curl localhost:8080/triage/t1/review --json '"Sorry about that! We refunded the second charge."'
 *   curl localhost:8080/restate/workflow/triage/t1/output              # what went out
 */
import * as restate from '@restatedev/restate-sdk';
import { serve } from 'restate-agents';
import * as claude from 'restate-agents/claude';
import * as pi from 'restate-agents/pi';

const classifier = claude.agent({
  name: 'classifier',
  system: 'Classify a support ticket. Answer with one word: billing, bug, or other.',
});

const drafter = pi.agent({
  name: 'drafter',
  system: 'Draft a short, kind reply to a support ticket.',
});

const triage = restate.workflow({
  name: 'triage',
  handlers: {
    run: async (ctx: restate.WorkflowContext, ticket: string) => {
      // Each ticket has its own conversation with each agent, keyed by its id.
      const kind = await ctx.objectClient(classifier, ctx.key).chat(ticket);
      const draft = await ctx.objectClient(drafter, ctx.key).chat(`A ${kind} ticket: ${ticket}`);
      ctx.set('draft', draft);

      // A person reviews the draft: the reply that goes out is theirs.
      const reply = await ctx.promise<string>('review');
      return { kind, reply };
    },

    /** The drafted reply, once there is one. */
    draft: async (ctx: restate.WorkflowSharedContext) => (await ctx.get<string>('draft')) ?? null,

    /** The reviewed reply. */
    review: async (ctx: restate.WorkflowSharedContext, reply: string) => {
      await ctx.promise<string>('review').resolve(reply);
    },
  },
});

await serve(triage, classifier, drafter);
