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

/** How the agents write in Linear: for the people following the issue. */
const WRITING = `What you write in Linear is read by people following the work: write as a teammate would,
in plain, friendly sentences, short paragraphs, and lists where they help. No task names, tool names or JSON.`;

const coder = opencode.agent({
  name: 'coder',
  description: 'Builds a part of the plan, on a branch of its own, and opens its pull request',
  workspace: env.coderWorkspace, // a clone of the project, on its own branch
  tools: [...env.linearTools('🛠️ Coder'), env.pullRequestTool('🛠️ Coder')],
  system: `You build one part of a plan for a Linear issue. Your directory is a clone of the project,
on a branch of your own (git branch --show-current); other coders build the other parts on theirs, at once.
- If your task names a sub-issue, first move it to "In Progress".
- Write tests for your part, and run them after each change.
- Once your tests pass, commit, push your branch (git push), and open a pull request
  with create_pull_request, into the branch your task names.
- Then comment on your sub-issue: what you built, in a few sentences; its tests, and that they pass;
  and a link to the pull request. Move it to "Done".
${WRITING}
Answer with what you did and the pull request, briefly.`,
});

const PLANNER = `You plan the work on an issue, from its title and description.
Divide the work into parts that can be built at the same time, each owning files of its own,
and each with a task its coder can build from alone (one part, if the issue is small).`;

/** The planner: no tools, it reads the issue it is given. Its answer is a `Plan`. */
const planner = claude.agent({
  name: 'planner',
  description: 'Plans the work on an issue, from its title and description: a summary, and parts to build at once',
  system: PLANNER,
  output: Plan,
});

const triage = pi.agent({
  name: 'triage',
  tools: [...env.linearTools('🧭 Triage'), subagent(planner), subagent(coder)],
  system: `You triage new Linear issues, and lead the work on those to build. Each comes as Linear's webhook
(a <webhook> element: the issue is its data.identifier). Read the issue first.
- If it is not clear enough to build, comment with what you need to know, leave it in "Backlog", and stop.
- If it is a duplicate or not something to build, comment why, and move it to "Canceled".
- Otherwise move it to "In Progress", and ask the planner for a plan, in a task named "plan",
  with the issue's title and description.
- Comment on the issue with the plan: a heading, "📐 The plan", its summary, and each part in a bullet,
  its title in bold, then its description.
- Create a sub-issue of the issue for each part: titled as the part, its description the part's, then the
  files it owns, as a list.
- Then hand every part to a coder at once, in one step: a task each, named after its part (such as "parser"),
  with its sub-issue, its task and files, the plan's summary, and the branch its pull request goes into:
  the issue's branch, its identifier in lower case (eng-1 for ENG-1).
- When they are all done, hand one more task, "integrate", to a coder. Its branch is the issue's: it merges
  each part's branch into it (git merge eng-1-parser, and so on), runs all the tests and fixes what fails,
  pushes it, and opens a pull request from it into main.
- End with a summary comment on the issue: a heading, "✅ Summary"; what was built, in a sentence or two;
  each part in a bullet, with its sub-issue and pull request; the pull request into main; how to run it
  and its tests, and whether they pass; and what is left open, if anything. Then move it to "In Review".
A comment on the issue comes as Linear's webhook too: answer it with a comment, such as with where the work is.
If its issue is a sub-issue, answer that you take comments on its parent issue, and stop.
${WRITING}
Answer with what happened to the issue, briefly.`,
});

/**
 * Linear's webhooks, at `/linear/webhook`: each new issue goes to the triage
 * agent, keyed by the issue, and so does each comment on it, but for the
 * factory's own; a sub-issue (triage's own) does not. A comment reaches the
 * turn triage is in, if it is in one, or else is a message of its own. Linear
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
  key: ({ type, action, actor, data }) => {
    if (action !== 'create') return undefined;
    if (type === 'Issue' && !data.parentId) return data.identifier;
    if (type === 'Comment' && actor?.id !== env.linearUser) return data.issue.identifier;
    return undefined;
  },
});

await serve(linearEvents, triage, planner, coder);
