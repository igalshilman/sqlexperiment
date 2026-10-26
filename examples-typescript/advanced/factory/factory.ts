/**
 * A software factory, in miniature: a new issue in Linear starts it, through
 * a webhook. A triage agent (pi) decides whether the issue is ready to build,
 * and leads the work: it asks a planner (Claude) for a plan, which comes back
 * structured, part by part; then it makes a sub-issue for each part, and
 * hands every part to a coder (DeepSeek) of its own, all at once. Each agent
 * moves its issue along Linear's workflow as it goes, and comments on it,
 * with Linear's MCP server.
 *
 *   node advanced/factory/linear.ts                   # Linear (a stand-in), on port 3333
 *   OPENAI_API_KEY=… ANTHROPIC_API_KEY=… DEEPSEEK_API_KEY=… node advanced/factory/factory.ts   # in another terminal
 *   npx @restatedev/restate-server                    # Restate, in another terminal
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:3333/issues --json '{"title": "A calculator CLI", "description": "calc.py: +, -, *, / on two numbers, and a history of the last 10 results in a file; with tests"}'
 *   curl localhost:3333/issues/ENG-1                   # its state, sub-issues and comments, as the agents work
 *   curl localhost:8080/triage/ENG-1/messages          # what triage did
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
import { createHmac, timingSafeEqual } from 'node:crypto';
import * as restate from '@restatedev/restate-sdk';
import { chatHandler, object, serve, subagent } from 'restate-agents';
import * as claude from 'restate-agents/claude';
import * as deepseek from 'restate-agents/deepseek';
import * as pi from 'restate-agents/pi';
import { mcpTools } from './mcp.ts';
import { planOf, PLANS, submitPlan } from './plan.ts';
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

const PLANNER = `You plan the work on a Linear issue: read it, then submit a plan with \`submit_plan\`.
Divide the work into parts that can be built at the same time, each owning files of its own,
and each with a task its coder can build from alone (one part, if the issue is small).
Do not move or comment on the issue: the one who asked does.`;

/**
 * The planner: its answer is the plan it submits, as JSON (`Plan`), not
 * text. It answers through `plan`, a handler of its own: a prompt, then the
 * plan the prompt submitted.
 */
const planner = object({
  name: 'planner',
  schema: PLANS,
  handlers: {
    plan: chatHandler(async (ctx, issue) => {
      await claude.prompt(ctx, issue, { system: PLANNER, tools: [...linear, submitPlan] });
      return JSON.stringify(await planOf(ctx));
    }),
    messages: claude.messages(),
  },
});

const triage = pi.agent({
  name: 'triage',
  tools: [
    ...linear,
    subagent(planner, {
      handler: 'plan',
      description: 'a planner, who answers with a plan as JSON: a summary, and parts that can be built at once',
    }),
    subagent(coder, { description: 'a coder, who builds a part of the plan in the issue’s directory' }),
  ],
  system: `You triage new Linear issues, and lead the work on those to build. Read the issue first.
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
  data: { identifier: string; title: string; description?: string; parentId?: string | null };
}

/** Whether Linear signed `body`: its HMAC-SHA256, with the webhook's secret. */
const signed = (body: Uint8Array, signature: string | undefined, secret: string) => {
  const expected = createHmac('sha256', secret).update(body).digest();
  const given = Buffer.from(signature ?? '', 'hex');
  return given.length === expected.length && timingSafeEqual(given, expected);
};

/**
 * Linear's webhooks: each new issue goes to the triage agent, keyed by the
 * issue; a sub-issue (the planner's own) does not. Linear sends a webhook
 * again until it is answered, with the same delivery id: the idempotency key
 * makes a delivery that comes again reach the same message, not send it
 * twice. With LINEAR_WEBHOOK_SECRET, a webhook Linear did not sign is
 * refused: the body comes as it was sent, for its signature.
 */
const webhooks = restate.service({
  name: 'linear',
  handlers: {
    webhook: restate.handlers.handler(
      { input: restate.serde.binary, accept: 'application/json' },
      async (ctx: restate.Context, body: Uint8Array) => {
        const headers = ctx.request().headers;
        if (webhookSecret && !signed(body, headers.get('linear-signature'), webhookSecret)) {
          throw new restate.TerminalError('Not signed by Linear', { errorCode: 401 });
        }
        const event = JSON.parse(new TextDecoder().decode(body)) as LinearWebhook;
        if (event.type !== 'Issue' || event.action !== 'create' || event.data.parentId) return;

        const { identifier, title, description } = event.data;
        ctx
          .objectSendClient(triage, identifier)
          .chat(
            `Issue ${identifier}: ${title}\n\n${description ?? ''}`,
            restate.rpc.sendOpts({ idempotencyKey: headers.get('linear-delivery') ?? ctx.request().id }),
          );
      },
    ),
  },
});

await serve(webhooks, triage, planner, coder);
