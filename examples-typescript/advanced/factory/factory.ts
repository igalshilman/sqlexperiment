/**
 * A software factory, in miniature: a new Linear issue starts it, through a
 * webhook. Triage (pi) leads the work: a planner (Claude) plans it in parts,
 * and a coder (OpenCode) builds each part on a branch of its own, with a pull
 * request. How to run it, and follow it: README.md.
 */
import { serve, subagent, webhook } from 'restate-agents';
import * as claude from 'restate-agents/claude';
import * as opencode from 'restate-agents/opencode';
import * as pi from 'restate-agents/pi';
import * as env from './env.ts';
import { Plan } from './plan.ts';
import * as prompts from './prompts.ts';

/**
 * Each agent on a model for its work: triage, which coordinates, on OpenAI's,
 * in its fast mode; the coders on Claude Sonnet; the planner on Claude Opus,
 * at medium effort. Their system prompts are in prompts.ts.
 */
const coder = opencode.agent({
  name: 'coder',
  description: 'Builds a part of the plan, on a branch of its own, and opens its pull request',
  model: 'anthropic/claude-sonnet-5-5',
  workspace: env.coderWorkspace, // a clone of the project, on its own branch
  tools: [...env.linearTools('🛠️ Coder'), env.pullRequestTool('🛠️ Coder')],
  system: prompts.CODER,
});

/** The planner: no tools, it reads the issue it is given. Its answer is a `Plan`. */
const planner = claude.agent({
  name: 'planner',
  description: 'Plans the work on an issue, from its title and description: a summary, and parts to build at once',
  model: 'anthropic/claude-opus-5-5',
  effort: 'medium',
  system: prompts.PLANNER,
  output: Plan,
});

const triage = pi.agent({
  name: 'triage',
  model: 'openai/gpt-6-sol',
  serviceTier: 'fast', // OpenAI's fast mode
  tools: [...env.linearTools('🧭 Triage'), subagent(planner), subagent(coder)],
  system: prompts.TRIAGE,
});

/**
 * Linear's webhooks, at `/linear/webhook`: each new issue goes to the triage
 * agent, keyed by the issue, and so does each comment on it, but for the
 * agents' own (signed, see env.ts); a sub-issue (triage's own) does not. A
 * comment reaches the turn triage is in, if it is in one, or else is a
 * message of its own. Linear
 * sends a webhook again until it is answered, with the same
 * `linear-delivery`: it reaches the same message. With LINEAR_WEBHOOK_SECRET,
 * a webhook Linear did not sign is refused.
 */
const linearEvents = webhook({
  name: 'linear',
  description: "Linear's new issues, and comments on them, to triage",
  auth: env.webhookAuth,
  idempotencyHeader: 'linear-delivery',
  route: [triage],
  // Linear's webhook: https://linear.app/developers/webhooks
  key: ({ type, action, data }) => {
    if (action !== 'create') return undefined;
    if (type === 'Issue' && !data.parentId) return data.identifier;
    if (type === 'Comment' && !env.byAnAgent(data.body)) return data.issue.identifier;
    return undefined;
  },
});

await serve(linearEvents, triage, planner, coder);
