/**
 * A software factory, in miniature: a new issue in Linear starts it, through
 * a webhook. A triage agent (pi) decides whether the issue is ready to build,
 * and leads the work: it asks a planner (Claude) for a plan, which comes back
 * structured, part by part; then it makes a sub-issue for each part, and
 * hands every part to a coder (DeepSeek) of its own, all at once. Each agent
 * moves its issue along Linear's workflow as it goes, and comments on it,
 * with Linear's MCP server.
 *
 *   node advanced/factory/linear.ts             # Linear (a stand-in), on port 3333
 *   OPENAI_API_KEY=… ANTHROPIC_API_KEY=… DEEPSEEK_API_KEY=… \
 *     node advanced/factory/factory.ts          # in another terminal
 *   npx @restatedev/restate-server              # Restate, in another terminal
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:3333/issues --json '{"title": "A calculator CLI", "description": "calc.py: +, -, *, / on two numbers, and a history of the last 10 results in a file; with tests"}'
 *   curl localhost:3333/issues/ENG-1            # its state, sub-issues and comments, as the agents work
 *   curl localhost:8080/triage/ENG-1/messages   # what triage did
 *
 * With the real Linear, LINEAR_API_KEY instead of the stand-in, and Linear's
 * webhook pointed at Restate's ingress (see the README).
 *
 * Each agent has a conversation per task, keyed as subagents are:
 * `triage/ENG-1`, `planner/ENG-1.plan`, `coder/ENG-1.<part>`. The coders run
 * real commands, on this machine, in WORKSPACE/<issue>
 * (/tmp/restate-factory by default): run it in a sandbox (a container, a VM).
 *
 * Environment: LINEAR_API_KEY and LINEAR_WEBHOOK_SECRET (the real Linear),
 * WORKSPACE. Not MODEL: each agent is on its own provider, with its default
 * model.
 */
import { serve, subagent, webhook } from 'restate-agents';
import * as claude from 'restate-agents/claude';
import * as deepseek from 'restate-agents/deepseek';
import * as pi from 'restate-agents/pi';
import { mcpTools } from './mcp.ts';
import { Plan } from './plan.ts';
import { workspaceTools } from './workspace.ts';

const workspace = process.env.WORKSPACE ?? '/tmp/restate-factory';
const apiKey = process.env.LINEAR_API_KEY;
const webhookSecret = process.env.LINEAR_WEBHOOK_SECRET;

/** Linear's tools (issues, sub-issues, states, comments): the real Linear's with an API key, or the stand-in's. */
const linear = apiKey
  ? await mcpTools('https://mcp.linear.app/mcp', { authorization: `Bearer ${apiKey}` })
  : await mcpTools('http://localhost:3333/mcp');

/** The issue a key is about: `ENG-1`, of `ENG-1.parser`. */
const issueOf = (key: string) => key.split('.')[0]!;

const coder = deepseek.agent({
  name: 'coder',
  // Every coder of an issue works in the issue's directory, at once.
  tools: [...linear, ...workspaceTools((key) => `${workspace}/${issueOf(key)}`)],
  system: `You build one part of a plan for a Linear issue, in the project's directory.
Other coders build the other parts in the same directory at the same time: change only the files of your part.
- If your task names a sub-issue, first move it to "In Progress".
- Read a file before you change it; write tests for your part, and run them after each change.
- Once your tests pass, commit your files with \`commit\` (never git in bash).
- Then comment on your sub-issue with what you did, and move it to "Done".
Answer with what you did, briefly.`,
});

const PLANNER = `You plan the work on a Linear issue: read it, then hand in a plan.
Divide the work into parts that can be built at the same time, each owning files of its own,
and each with a task its coder can build from alone (one part, if the issue is small).
Do not move or comment on the issue: the one who asked does.`;

/** The planner: its answer is a `Plan`, as JSON, which the model hands in, not text. */
const planner = claude.agent({ name: 'planner', system: PLANNER, tools: linear, output: Plan });

const triage = pi.agent({
  name: 'triage',
  tools: [
    ...linear,
    subagent(planner, { description: 'a planner, who answers with a plan as JSON: a summary, and parts at once' }),
    subagent(coder, { description: 'a coder, who builds a part of the plan in the issue’s directory' }),
  ],
  system: `You triage new Linear issues, and lead the work on those to build. Each comes as Linear's webhook
(a <webhook> element: the issue is its data.identifier). Read the issue first.
- If it is not clear enough to build, comment with what you need to know, leave it in "Backlog", and stop.
- If it is a duplicate or not something to build, comment why, and move it to "Canceled".
- Otherwise move it to "In Progress", and ask the planner for a plan, in a task named "plan".
- Comment on the issue with the plan, and create a sub-issue of it for each part, titled as the part.
- Then hand every part to a coder at once, in one step: a task each, named after its part,
  with its sub-issue, its task and files, and the plan's summary.
- When they are all done, hand one more task, "integrate", to a coder: run all the tests, and fix what fails.
- Comment on the issue with what was built and how to run it, and move it to "In Review".
Answer with what happened to the issue, briefly.`,
});

/** A Linear webhook (https://linear.app/developers/webhooks), the part the factory reads. */
interface LinearWebhook {
  action: 'create' | 'update' | 'remove';
  type: string;
  data: { identifier: string; parentId?: string | null };
}

/**
 * Linear's webhooks, at `/linear/webhook`: each new issue goes to the triage
 * agent, keyed by the issue; a sub-issue (the planner's own) does not.
 * Linear sends a webhook again until it is answered, with the same
 * `linear-delivery`: it reaches the same message. With LINEAR_WEBHOOK_SECRET,
 * a webhook Linear did not sign is refused.
 */
const issues = webhook<LinearWebhook>({
  name: 'linear',
  description: "Linear's new issues, to triage",
  auth: webhookSecret ? { header: 'linear-signature', secret: webhookSecret } : undefined,
  idempotencyHeader: 'linear-delivery',
  route: [triage],
  key: ({ type, action, data }) =>
    type === 'Issue' && action === 'create' && !data.parentId ? data.identifier : undefined,
});

await serve(issues, triage, planner, coder);
