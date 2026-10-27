/**
 * A software factory, in miniature: a new issue in Linear starts it, through
 * a webhook. A triage agent (pi) decides whether the issue is ready to build,
 * and leads the work: it asks a planner (Claude) for a plan, which comes back
 * structured, part by part; then it makes a sub-issue for each part, and
 * hands every part to a coder (OpenCode) of its own, all at once. Triage and
 * the coders move their issues along Linear's workflow as they go, and
 * comment on them, with Linear's MCP server; the planner only plans.
 *
 * From examples-typescript/, each in a terminal of its own, with a key for
 * each agent's model: OpenAI's for triage and the coders, Anthropic's for the
 * planner:
 *
 *   node advanced/factory/linear.ts                    # Linear (a stand-in), on port 3333
 *   OPENAI_API_KEY=… ANTHROPIC_API_KEY=… node advanced/factory/factory.ts
 *   npx @restatedev/restate-server
 *
 * Then register the factory with Restate, file an issue, and follow it. The
 * new issue is what starts the factory: the stand-in posts Linear's webhook
 * for it to Restate, at localhost:8080/linear/webhook.
 *
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:3333/issues --json '{"title": "A calculator CLI", "description": "calc.py: +, -, *, / on two numbers, and a history of the last 10 results in a file; with tests"}'
 *   curl localhost:3333/issues/ENG-1                   # the issue: its state, sub-issues and comments
 *   curl localhost:8080/triage/ENG-1/status            # what triage is doing now
 *   curl localhost:8080/triage/ENG-1/messages          # triage's conversation, with its tool calls
 *   curl localhost:8080/planner/ENG-1.plan/messages    # the planner's
 *   curl localhost:8080/coder/ENG-1.parser/messages    # a coder's, of a part triage named "parser"
 *   curl localhost:3333/issues/ENG-1/comments --json '{"body": "How is it going?"}'   # triage answers on the issue
 *
 * With the real Linear, LINEAR_API_KEY instead of the stand-in, and Linear's
 * webhook pointed at Restate's ingress; with a project on GitHub,
 * GITHUB_REPO and GITHUB_TOKEN (see the README).
 *
 * Each agent has a conversation per task, keyed as subagents are:
 * `triage/ENG-1`, `planner/ENG-1.plan`, `coder/ENG-1.<part>`. Each coder
 * works in a git worktree of the project of its own, WORKSPACE/ENG-1.<part>
 * (/tmp/restate-factory by default), on its own branch (eng-1-<part>), and
 * opens a pull request into the issue's branch (eng-1); the `integrate`
 * coder merges them there, and opens the issue's pull request into main.
 * The coders run real commands, on this machine: run it in a sandbox (a
 * container, a VM).
 *
 * Environment (env.ts): LINEAR_API_KEY and LINEAR_WEBHOOK_SECRET (the real
 * Linear), GITHUB_REPO and GITHUB_TOKEN (the project on GitHub; else a git
 * repository in WORKSPACE stands in for it), WORKSPACE. Not MODEL: each
 * agent is on its own provider, with its default model.
 */
import { serve, subagent, webhook } from 'restate-agents';
import * as claude from 'restate-agents/claude';
import * as opencode from 'restate-agents/opencode';
import * as pi from 'restate-agents/pi';
import * as env from './env.ts';
import { Plan } from './plan.ts';

const coder = opencode.agent({
  name: 'coder',
  description: 'Builds a part of the plan, on a branch of its own, and opens its pull request',
  workspace: env.coderWorkspace, // a git worktree of the project, on its own branch
  tools: [...env.linearTools('🛠️ Coder'), env.pullRequestTool('🛠️ Coder')],
  system: `You build one part of a plan for a Linear issue. Your directory is a git worktree of the project,
on a branch of your own (git branch --show-current); other coders build the other parts on theirs, at once.
- If your task names a sub-issue, first move it to "In Progress".
- Write tests for your part, and run them after each change.
- Once your tests pass, commit, push your branch (git push -u origin HEAD), and open a pull request
  with create_pull_request, into the branch your task names.
- Then comment on your sub-issue with what you did, its tests' result, and the pull request. Move it to "Done".
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
- Comment on the issue with the plan (headed "📐 The plan"), and create a sub-issue of it for each part, titled as the part.
- Then hand every part to a coder at once, in one step: a task each, named after its part (such as "parser"),
  with its sub-issue, its task and files, the plan's summary, and the branch its pull request goes into:
  the issue's branch, its identifier in lower case (eng-1 for ENG-1).
- When they are all done, hand one more task, "integrate", to a coder. Its branch is the issue's: it merges
  each part's branch into it (git merge eng-1-parser, and so on), runs all the tests and fixes what fails,
  pushes it, and opens a pull request from it into main.
- End with a summary comment on the issue, headed "✅ Summary": what was built, part by part (with each
  sub-issue and its pull request); the pull request into main; how to run it and its tests, and whether they
  pass; and what is left open. Then move it to "In Review".
A comment on the issue comes as Linear's webhook too: answer it with a comment, such as with where the work is.
If its issue is a sub-issue, answer that you take comments on its parent issue, and stop.
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
