import { serve, subagent, webhook } from 'restate-agents';
import * as opencode from 'restate-agents/opencode';
import * as pi from 'restate-agents/pi';
import * as env from './env.ts';
import * as github from './github.ts';
import { Built, Plan, Review } from './answers.ts';
import * as prompts from './prompts.ts';
import { byAnAgent } from './signatures.ts';

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

const planner = pi.agent({
  name: 'planner',
  description: 'Plans the work on an issue, from its title and description: a summary, and parts to build at once',
  model: 'openai/gpt-6.1-sol',
  serviceTier: 'fast',
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
