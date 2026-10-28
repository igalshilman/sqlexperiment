/**
 * A software factory, in miniature: a new Linear issue starts it, through a
 * webhook. Triage (pi) leads the work: a planner (the DeepSeek Harness) plans it in parts, a
 * coder (OpenCode) builds each part on a branch of its own, with a pull
 * request, and a reviewer (pi) reviews it, its changes going back to the
 * coder, until it approves. How to run it, and follow it: README.md.
 */
import { serve, subagent, webhook } from 'restate-agents';
import * as deepseek from 'restate-agents/deepseek';
import * as opencode from 'restate-agents/opencode';
import * as pi from 'restate-agents/pi';
import * as env from './env.ts';
import * as github from './github.ts';
import { Built, Plan, Review } from './answers.ts';
import * as prompts from './prompts.ts';
import { byAnAgent } from './signatures.ts';

/**
 * Each agent on a model for its work: triage, which coordinates, on OpenAI's
 * GPT Sol, the reviewer on its GPT Astra, both in OpenAI's fast mode; the
 * planner on DeepSeek V4.1 Flash, on Modal's endpoint; the coders on Claude
 * Sonnet. Their system prompts are in prompts.ts.
 */
const reviewer = pi.agent({
  name: 'reviewer',
  description: 'Reviews a pull request, as pushed, and posts its review on it: approved, or the changes to make',
  model: 'openai/gpt-6-astra',
  serviceTier: 'fast',
  tools: env.reviewTools('🔍 Reviewer'),
  system: prompts.REVIEWER,
  output: Review,
});

const coder = opencode.agent({
  name: 'coder',
  description: 'Builds a part of the plan, on a branch of its own, and opens its pull request; answers its reviews',
  model: 'anthropic/claude-sonnet-5-5',
  workspace: env.coderWorkspace, // a clone of the project, on its own branch
  tools: [...env.linearTools('🛠️ Coder'), env.pullRequestTool('🛠️ Coder'), env.pullRequestCommentTool('🛠️ Coder')],
  system: prompts.CODER,
  output: Built,
});

/**
 * The planner: no tools, it reads the issue it is given. Its answer is a
 * `Plan`. On the DeepSeek Harness, its model on Modal's endpoint
 * (MODAL_ENDPOINT_URL, with a proxy token's MODAL_PROXY_KEY and MODAL_PROXY_SECRET).
 */
const planner = deepseek.agent({
  name: 'planner',
  description: 'Plans the work on an issue, from its title and description: a summary, and parts to build at once',
  model: 'modal/deepseek-ai/DeepSeek-V4.1-Flash',
  system: prompts.PLANNER,
  output: Plan,
});

const triage = pi.agent({
  name: 'triage',
  model: 'openai/gpt-6-sol',
  serviceTier: 'fast', // OpenAI's fast mode
  tools: [...env.linearTools('🧭 Triage'), subagent(planner), subagent(coder), subagent(reviewer)],
  system: prompts.TRIAGE,
});

/**
 * Linear's events, to triage's conversation for the issue:
 * - a new issue starts one (a sub-issue, triage's own, does not);
 * - a new comment on it, but for the agents' own, joins it.
 * Anything else is dropped.
 */
const linearEvents = webhook({
  name: 'linear',
  description: "Linear's new issues, and comments on them, to triage",
  auth: env.linearWebhookAuth,
  idempotencyHeader: 'linear-delivery',
  route: [triage],
  // The event's shape: https://linear.app/developers/webhooks
  key: ({ type, action, data }) => {
    if (action !== 'create') return undefined;
    if (type === 'Issue' && !data.parentId) return data.identifier;
    if (type === 'Comment' && !byAnAgent(data.body)) return data.issue.identifier;
    return undefined;
  },
});

/**
 * GitHub's events, to the coder of each pull request: a person's comment on
 * it or review of it, and a CI run on its branch that failed. Anything else
 * is dropped. See github.ts.
 */
const githubEvents = webhook({
  name: 'github',
  description: "GitHub's comments, reviews and failed CI runs on the coders' pull requests, to their coders",
  auth: github.webhookAuth,
  idempotencyHeader: 'x-github-delivery',
  route: [coder],
  key: github.coderOf,
  message: github.whatHappened,
});

await serve(linearEvents, githubEvents, triage, planner, coder, reviewer);
