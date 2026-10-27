# A software factory

A new issue in Linear starts it, through a webhook. Agents on two
frameworks take it from there. A triage agent (pi) decides whether the issue
is ready to build, and leads the work: it asks a planner (pi) for a
plan, which comes back structured (a summary, and parts that can be built
at once); it makes a Linear sub-issue for each part, and hands every part to
a coder (OpenCode) of its own, all at once: each on a git branch of its
own, which it opens a pull request of, on GitHub. As each answers with its
pull request, triage has a reviewer (pi) review it, which posts its
review on the pull request; triage hands the changes it asks for back to the
coder, and has it reviewed again, until it approves. With every pull request
approved, triage marks the issue Done, and leaves the merging to people.
Triage and the coders move their issues along the team's workflow, and
comment on them, through Linear's MCP server: on the real Linear, or on a
stand-in, [`linear.ts`](linear.ts), with the same tools.

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
  │  linear.webhook (webhook()): checks the signature; one message per                 │       │
  │    delivery; skips sub-issues                                                      │       │
  │        │ send                                                                      │       │
  │        ▼                                                                           │       │
  │  ┌────────────────────────────────────┐  In Progress, the plan, sub-issues,        │       │
  │  │ triage/ENG-1                   pi  │  Done once approved ───────────────────────┼───────┘
  │  │ (the parent: triages, leads, and   │  (or Backlog, Canceled)                    │
  │  │  has each pull request reviewed)   │                                            │
  │  └──┬───────────────▲───────┬──────┬──┘                                            │
  │     │ subagent:     │ Plan, │      │     subagents: a coder per part,              │
  │     │ plan          │ JSON  │      └───────────┐  then a reviewer per PR;          │
  │     ▼               │       ▼                  ▼  its changes back to the coder    │
  │  ┌──────────────────┴───┐ ┌────────────┐ ┌────────────┐                            │
  │  │ planner/ENG-1.plan   │ │ coder/     │ │ reviewer/  │  coders push and           │
  │  │                   pi │ │ ENG-1.     │ │ ENG-1.     │  open the PRs;             │
  │  │                      │ │ parser,    │ │ parser,    │  reviewers read            │
  │  │ replies with the     │ │ ENG-1.     │ │ ENG-1.     │  them, and post            │
  │  │ plan, as JSON        │ │ history    │ │ history    │  their reviews             │
  │  │ (its output,         │ │   OpenCode │ │         pi │  on them                   │
  │  │  checked)            │ │            │ │            │                            │
  │  └──────────────────────┘ └─────┬──────┘ └─────┬──────┘                            │
  └─────────────────────────────────┼──────────────┼───────────────────────────────────┘
   each coder in a clone of its     │ push, PR     │ read, review
   own, on its own branch, with     ▼              ▼
   OpenCode's read, edit, bash ┌───────────────────────────┐
                               │ GitHub: eng-1-parser and  │
                               │ eng-1-history, PRs → main │
                               └───────────────────────────┘
```

The planner only plans: a pi agent whose `output` is the plan's schema
([`answers.ts`](answers.ts)), and no tools: triage hands it the issue's title
and description. The model replies with the plan as JSON of that schema, which
is checked, and the planner's `chat` answers with it, as a call answers.
The parent acts on it: a sub-issue and a coder per part.

Each agent is a virtual object with its conversation in its own SQL
database, keyed by its task (`coder/ENG-1.parser`): a crash resumes it
where it was, and the parent waits for its answers without holding a
process. Tasks of different names run at once, so the coders work side by
side. Each part has one task of each kind, `coder/ENG-1.parser` and
`reviewer/ENG-1.parser`, which triage hands each round to: the coder
remembers its work, the reviewer its last review, so it reads what changed
since. The factory stops with the pull requests approved and open, for
people to merge.

## An issue's life

```
  time ──▶

  Linear                     triage (pi), the parent   planner, reviewer   coders (OpenCode)
    │                               │                    (pi)           parser     history
  ENG-1  Backlog ──── webhook ─────▶│ reads it              │             │           │
    │◀── In Progress, comment: ─────┤                       │             │           │
    │    "let me make a plan"       │                       │             │           │
    │                               ├── subagent: plan ────▶│ the issue   │           │
    │                               │                       │ in the task │           │
    │                               │◀── Plan, as JSON ─────┤             │           │
    │◀── comment: the plan ─────────┤                       │             │           │
  ENG-2  sub-issue: parser ◀────────┤                       │             │           │
  ENG-3  sub-issue: history ◀───────┤                       │             │           │
    │                               ├── subagent: coder, parser ─────────▶│           │
    │                               ├── subagent: coder, history ────────────────────▶│
  ENG-2  In Progress, comment: "picking it up" ◀──────────────────────────┤           │
  ENG-3  In Progress, comment: "picking it up" ◀──────────────────────────────────────┤
    │                               │                       │   code, tests, a PR     │
  ENG-2  In Review, a comment ◀───────────────────────────────────────────┤           │
  ENG-3  In Review, a comment ◀───────────────────────────────────────────────────────┤
    │                               │◀── the PRs ─────────────────────────┴───────────┘
    │◀── comment: PRs to review ────┤                       │
    │                               ├── subagent: reviewer ▶│ a review each,
    │                               │                       │ posted on its PR
    │                               │◀── approved, or ──────┤
    │                               │    changes            │
  ENG-2/3  comment: the review ◀────┤                       │
    │                               ├── changes, to their coders, then reviews again
    │                               │   (three reviews of a PR at most)
  ENG-2  Done ◀─────────────────────┤                       │
  ENG-3  Done ◀─────────────────────┤                       │
    │◀── comment: what was built ───┤  (the pull requests, approved, not merged)
  ENG-1  Done ◀─────────────────────┤
```

A pull request the reviewer has not approved after three reviews leaves its
sub-issue, and the issue, **In Review**. And not every issue goes the whole
way: triage leaves one that is unclear in **Backlog**, with a comment asking
what it needs to know, and moves one that is a duplicate or nothing to build
to **Canceled**.

## The files

| File                           | What                                                                                                                                       |
| ------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------ |
| [`factory.ts`](factory.ts)     | The webhook receiver, and the agents: triage, the parent, with a planner, coders and a reviewer as subagents                               |
| [`env.ts`](env.ts)             | The environment: Linear's tools, user and webhook auth (the real Linear or the stand-in); GitHub's pull requests, and the reviewer's tools |
| [`answers.ts`](answers.ts)     | The agents' structured answers (`output`): the planner's plan, the reviewer's review, a coder's pull request                               |
| [`prompts.ts`](prompts.ts)     | The agents' system prompts: triage's, the planner's, a coder's, the reviewer's                                                             |
| [`mcp.ts`](mcp.ts)             | An MCP server's tools as tools for any agent (`tool()`), asked for when the endpoint starts                                                |
| [`workspace.ts`](workspace.ts) | The coders' clones of the project, each on its own branch, and what they pushed; a first commit for an empty project; the stand-in's       |
| [`linear.ts`](linear.ts)       | Linear, a stand-in: issues and sub-issues in memory, its MCP server, and a signed webhook for each issue and comment                       |

## Run it, on the stand-in

From [`examples-typescript/`](../..), after `make js` at the repository
root, each in a terminal of its own, with a key for each agent's model:
OpenAI's for triage and the planner (GPT Sol) and the reviewer (GPT Astra),
all in OpenAI's fast mode, and Anthropic's for the coders (Claude Sonnet):

```sh
node advanced/factory/linear.ts                    # Linear (a stand-in), on port 3333
OPENAI_API_KEY=… ANTHROPIC_API_KEY=… node advanced/factory/factory.ts
npx @restatedev/restate-server
```

Then register the factory with Restate, file an issue, and follow it. The
new issue is what starts the factory: the stand-in posts Linear's webhook
for it to Restate, at `localhost:8080/linear/webhook`.

```sh
npx @restatedev/restate deployments register http://localhost:9080
curl localhost:3333/issues --json '{"title": "A calculator CLI", "description": "calc.py: +, -, *, / on two numbers, and a history of the last 10 results in a file; with tests"}'
curl localhost:3333/issues/ENG-1                   # the issue: its state, history, sub-issues and comments
curl localhost:8080/triage/ENG-1/status            # what triage is doing now
curl localhost:8080/triage/ENG-1/messages          # triage's conversation, with its tool calls
curl localhost:8080/planner/ENG-1.plan/messages    # the planner's
curl localhost:8080/coder/ENG-1.parser/messages    # a coder's, of a part triage named "parser"
curl localhost:8080/reviewer/ENG-1.parser/messages # its reviewer's: the reviews, round by round
curl localhost:3333/issues/ENG-1/comments --json '{"body": "How is it going?"}'   # triage answers on the issue
```

Restate's UI, at http://localhost:9070, shows the same work as calls:
triage's, and the planner's, coders' and reviewer's under it.

`linear.ts` prints each issue, move and comment as it happens. Without
GitHub, a git repository in `WORKSPACE/origin.git` stands in for the
project's, and `factory.ts` prints each pull request, and each review. The
coders run real commands, on this machine, in `WORKSPACE`
(`/tmp/restate-factory` by default): run it in a sandbox (a container, a
VM).

## Run it, on Linear and GitHub

The same agents, on your Linear workspace and a GitHub repository: they
move and comment on real issues, make real sub-issues, and open real pull
requests. Use a team and a repository of their own, for the factory.

1. **A key the factory acts with**: a personal API key (Settings, Security
   & access), or an OAuth app's token, of an app installed with `actor=app`
   (which takes a workspace admin), so that the factory acts in Linear as
   itself ([Linear's docs](https://linear.app/developers/oauth-actor-authorization)).
   It needs to write issues and comments. The factory sends it to Linear's
   MCP server (`https://mcp.linear.app/mcp`) as a bearer token. The agents
   sign what they write, so the factory tells its comments from yours
   either way.
2. **Restate, reachable by Linear**: a tunnel to Restate's ingress, such as
   `cloudflared tunnel --url http://localhost:8080` (or `ngrok http 8080`).
3. **A webhook**: in Linear's settings, under API, a webhook to
   `https://<the tunnel>/linear/webhook`, for Issues and Comments, on the
   factory's team only. Its signing secret is `LINEAR_WEBHOOK_SECRET`.
4. **The team's states**: the agents move issues to Todo, In Progress,
   In Review, Done and Canceled. Add any the team has not (In Review, say),
   under the team's workflow settings.
5. **A repository, and a token for it**: a repository (if it is empty,
   the factory pushes a README to `main` first; a test runner helps the
   coders), and a fine-grained personal access token for it alone, with read and write
   access to its contents and pull requests: `GITHUB_TOKEN`. The coders push
   their branches with it, and open pull requests through GitHub's MCP
   server (`https://api.githubcopilot.com/mcp/`), where the reviewers post
   their reviews, as comments (an author cannot approve their own pull
   request on GitHub: the factory's approval shows in Linear). With Linear's GitHub
   integration, a pull request shows on its issue too: its branch names it
   (`eng-1-parser`).

```sh
LINEAR_API_KEY=lin_api_… LINEAR_WEBHOOK_SECRET=lin_wh_… \
GITHUB_REPO=igalshilman/demos GITHUB_TOKEN=github_pat_… \
OPENAI_API_KEY=… ANTHROPIC_API_KEY=… \
  node advanced/factory/factory.ts
npx @restatedev/restate-server              # and register the factory, as above
```

Then create an issue on the team, in Linear: it moves as the agents work,
and its sub-issues appear, each moving on its own. Comment on the issue
("How is it going?"): triage answers there. While it waits for its coders,
it takes the comment once they answer.

## What it shows

- **A webhook into agents**: `webhook()` of restate-agents makes the
  `linear` service. Linear posts to it at Restate's ingress, and it
  delivers each new issue, and each comment on it, to the triage agent,
  keyed by the issue, as a message that says where it comes from
  (`<webhook name="linear" delivery="…">`): to the turn it is in, if it is
  in one, or else as a message of its own. The agents' own comments come
  back too, and are dropped: they open with an agent's signature
  ("🧭 Triage"), whichever Linear user posted them.
  A delivery Linear sends again has the same `linear-delivery`, the
  message's idempotency key: it reaches the same message, not a second
  one. With a secret, a webhook that Linear did not sign is refused (401).
  Its routes, auth and idempotency header are in the handler's metadata,
  in the admin API and the UI.
- **Structured answers from agents**: the planner's answer is a `Plan`, the
  reviewer's a `Review`, a coder's what it `Built` (its pull request): the
  model's reply checked against its schema
  (`output`), the subagent call's return value, not text to parse.
- **A conversation between agents, led by their parent**: triage takes a
  coder's pull request to the reviewer, and its changes back to the coder
  (OpenCode and pi), round by round, each in the same task every time,
  so the coder knows its work, and the reviewer its last review.
- **Work divided, at once**: the parent's subagent calls in one step run at
  the same time, each a coder with a conversation of its own; the parent
  waits for them all, holding no process.
- **Status in Linear**: triage and each coder move their own issue with
  Linear's tools, so the board shows the work as it goes, down to each part;
  and say what they are about to do as they start it, in a comment: triage
  that it is making a plan, each coder what it picks up and how, triage
  each review on its sub-issue (approved, or the changes asked for).
  They all write as one Linear user, so each comment, and each sub-issue's
  description, opens with who wrote it ("🧭 Triage", "🛠️ Coder · parser"):
  `env.linearTools(who)` adds it, in code. They write for the people
  following the issue, in plain sentences; triage ends with a "✅ Summary"
  comment: what was built, how to run it, whether each pull request is
  approved, and what is left open.
- **MCP tools in any agent**: `mcpTools(url)` turns an MCP server's tools
  into `tool()`s, which pi and OpenCode take alike. A tool call is
  stored once it is done; a crash makes again only one that had not
  finished (moving an issue twice is harmless; a comment may show twice).
