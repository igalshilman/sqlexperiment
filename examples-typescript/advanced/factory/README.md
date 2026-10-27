# A software factory

A new issue in Linear starts it, through a webhook. Agents on three
frameworks take it from there. A triage agent (pi) decides whether the issue
is ready to build, and leads the work: it asks a planner (Claude) for a
plan, which comes back structured (a summary, and parts that can be built
at once); it makes a Linear sub-issue for each part, and hands every part to
a coder (DeepSeek) of its own, all at once, in the issue's repository. Each
agent moves its issue along the team's workflow, and comments on it,
through Linear's MCP server: on the real Linear, or on a stand-in,
[`linear.ts`](linear.ts), with the same tools.

## How it fits together

```
  ┌──────────────────────────────────┐
  │ Linear                           │  linear.ts (a stand-in, :3333),
  │  issues, sub-issues, comments    │  or linear.app (LINEAR_API_KEY)
  │  MCP server                      │◀────────────────────────────────────────────────────────┐
  └────────────────┬─────────────────┘                                                         │
                   │ webhook {action: "create", type: "Issue", …}                              │
                   │ Linear-Delivery: <id>   Linear-Signature: <hmac>                          │
                   ▼                                                                           │
  ┌────────────────────────────────────────────────────────────────────────────────────┐       │
  │ Restate                                                                            │       │
  │                                                                                    │       │
  │  linear.webhook (a service): checks the signature; one message per                 │       │
  │    delivery; skips sub-issues                                                      │       │
  │        │ send                                                                      │       │
  │        ▼                                                                           │       │
  │  ┌────────────────────────────────────┐  In Progress, the plan,                    │       │
  │  │ triage/ENG-1                   pi  │  a sub-issue per part,  ───────────────────┼───────┤
  │  │ (the parent: triages, then leads)  │  In Review at the end                      │       │
  │  └──┬───────────────▲───────┬─────────┘  (or Backlog, Canceled)                    │       │
  │     │ subagent:     │ Plan, │ subagent, one per part, all at once                  │       │
  │     │ plan          │ JSON  ├──────────────┬──────────────┐                        │       │
  │     ▼               │       ▼              ▼              ▼                        │       │
  │  ┌──────────────────┴───┐ ┌────────────┐ ┌────────────┐ ┌────────────┐             │       │
  │  │ planner/ENG-1.plan   │ │ coder/     │ │ coder/     │ │ coder/     │  each moves │       │
  │  │               Claude │ │ ENG-1.     │ │ ENG-1.     │ │ ENG-1.     │  its sub-   │       │
  │  │                      │ │ parser     │ │ history    │ │ integrate  │  issue      │       │
  │  │ hands in its plan    │ │  DeepSeek  │ │  DeepSeek  │ │  DeepSeek  ├─────────────┼───────┘
  │  │ with submit_plan     │ └─────┬──────┘ └─────┬──────┘ └─────┬──────┘             │
  │  │ (checked, and kept   │       │              │              │                    │
  │  │  in its database)    │       │              │              │                    │
  │  └──────────────────────┘       │              │              │                    │
  └─────────────────────────────────┼──────────────┼──────────────┼────────────────────┘
   list, read, write, bash;         ▼              ▼              ▼
   commit (only its own        ┌─────────────────────────────────────┐
   files, one at a time)       │  WORKSPACE/ENG-1 (a git repository) │
                               └─────────────────────────────────────┘
```

The planner only plans: its `plan` handler prompts Claude, which hands in
its plan with the `submit_plan` tool, whose input is the plan's schema
([`plan.ts`](plan.ts)), and answers with the plan as JSON. The parent acts
on it: a sub-issue and a coder per part.

Each agent is a virtual object with its conversation in its own SQL
database, keyed by its task (`coder/ENG-1.parser`): a crash resumes it
where it was, and the parent waits for its answers without holding a
process. Tasks of different names run at once, so the coders work side by
side; an `integrate` task runs the whole test suite once they are done.

## An issue's life

```
  time ──▶

  Linear                     triage (pi), the parent    planner (Claude)   coders (DeepSeek)
    │                                │                        │            parser     history
  ENG-1  Backlog ──── webhook ──────▶│ reads it               │              │           │
    │◀──────── In Progress ──────────┤                        │              │           │
    │                                ├── subagent: plan ─────▶│ reads ENG-1  │           │
    │                                │                        │ submit_plan  │           │
    │                                │◀── Plan, as JSON ──────┤              │           │
    │◀──────── comment: the plan ────┤                        │              │           │
  ENG-2  sub-issue: parser ◀─────────┤                        │              │           │
  ENG-3  sub-issue: history ◀────────┤                        │              │           │
    │                                ├── subagent: parser ──────────────────▶│           │
    │                                ├── subagent: history ─────────────────────────────▶│
  ENG-2  In Progress ◀───────────────────────────────────────────────────────┤           │
  ENG-3  In Progress ◀───────────────────────────────────────────────────────────────────┤
    │                                │                        │   code, tests, commit    │
  ENG-2  Done, a comment ◀───────────────────────────────────────────────────┤           │
  ENG-3  Done, a comment ◀───────────────────────────────────────────────────────────────┤
    │                                │◀── answers ───────────────────────────┴───────────┘
    │                                ├── subagent: integrate (a coder runs every test)
    │◀──── comment: what was built ──┤
  ENG-1  In Review ◀─────────────────┤
```

Not every issue goes the whole way: triage leaves one that is unclear in
**Backlog**, with a comment asking what it needs to know, and moves one that
is a duplicate or nothing to build to **Canceled**.

## The files

| File                           | What                                                                                                     |
| ------------------------------ | -------------------------------------------------------------------------------------------------------- |
| [`factory.ts`](factory.ts)     | The webhook receiver, and the agents: triage, the parent, with a planner and coders as subagents         |
| [`plan.ts`](plan.ts)           | The plan's schema, the `submit_plan` tool that hands it in, and the table that keeps it                  |
| [`mcp.ts`](mcp.ts)             | An MCP server's tools as tools for any agent (`tool()`), asked for when the endpoint starts              |
| [`workspace.ts`](workspace.ts) | Coding tools for the coders (list, read and write files; bash; commit), in the issue's directory         |
| [`linear.ts`](linear.ts)       | Linear, a stand-in: issues and sub-issues in memory, its MCP server, and a signed webhook for each issue |

## Run it, on the stand-in

From [`examples-typescript/`](../..), after `make js` at the repository root:

```sh
node advanced/factory/linear.ts       # Linear (a stand-in), on port 3333
OPENAI_API_KEY=… ANTHROPIC_API_KEY=… DEEPSEEK_API_KEY=… node advanced/factory/factory.ts   # in another terminal
npx @restatedev/restate-server        # Restate, in another terminal
npx @restatedev/restate deployments register http://localhost:9080

curl localhost:3333/issues --json '{"title": "A calculator CLI", "description": "calc.py: +, -, *, / on two numbers, and a history of the last 10 results in a file; with tests"}'
curl localhost:3333/issues/ENG-1      # its state, history, sub-issues and comments, as the agents work
curl localhost:8080/triage/ENG-1/messages
```

No DeepSeek key? The coder's harness takes any model pi-ai knows: give it
`model: 'openai/gpt-6-sol'` in [`factory.ts`](factory.ts), and it needs only
`OPENAI_API_KEY`.

`linear.ts` prints each issue, move and comment as it happens. The coders
run real commands, on this machine, in `WORKSPACE/<issue>`
(`/tmp/restate-factory` by default): run it in a sandbox (a container, a VM).

## Run it, on Linear

The same agents, on your Linear workspace: they move and comment on real
issues, and make real sub-issues. Use a team of its own, for the factory.

1. **An API key**: in Linear's settings, under Security & access, a
   personal API key. The factory sends it to Linear's MCP server
   (`https://mcp.linear.app/mcp`) as a bearer token.
2. **Restate, reachable by Linear**: a tunnel to Restate's ingress, such as
   `cloudflared tunnel --url http://localhost:8080` (or `ngrok http 8080`).
3. **A webhook**: in Linear's settings, under API, a webhook to
   `https://<the tunnel>/linear/webhook`, for Issues, on the factory's team
   only. Its signing secret is `LINEAR_WEBHOOK_SECRET`.
4. **The team's states**: the agents move issues to Todo, In Progress,
   In Review, Done and Canceled. Add any the team has not (In Review, say),
   under the team's workflow settings.

```sh
LINEAR_API_KEY=lin_api_… LINEAR_WEBHOOK_SECRET=lin_wh_… \
OPENAI_API_KEY=… ANTHROPIC_API_KEY=… DEEPSEEK_API_KEY=… node advanced/factory/factory.ts
npx @restatedev/restate-server        # and register the factory, as above
```

Then create an issue on the team, in Linear: it moves as the agents work,
and its sub-issues appear, each moving on its own.

## What it shows

- **A webhook into Restate**: Linear posts to Restate's ingress, and
  `linear.webhook` sends the issue to its triage agent. A delivery Linear
  sends again has the same delivery id, the message's idempotency key: it
  reaches the same message, not a second one. With a secret, a webhook
  that Linear did not sign is refused (the handler takes the body as it
  was sent, `restate.serde.binary`, to check its signature).
- **A structured answer from an agent**: the planner's answer is a `Plan`,
  checked against its schema when the model hands it in (`submit_plan`),
  not text to parse.
- **Work divided, at once**: the parent's subagent calls in one step run at
  the same time, each a coder with a conversation of its own; the parent
  waits for them all, holding no process.
- **Status in Linear**: every agent moves its own issue with Linear's tools,
  so the board shows the work as it goes, down to each part.
- **MCP tools in any agent**: `mcpTools(url)` turns an MCP server's tools
  into `tool()`s, which pi, Claude and DeepSeek take alike. A tool call is
  stored once it is done; a crash makes again only one that had not
  finished (moving an issue twice is harmless; a comment may show twice).
