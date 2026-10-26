# Examples

Each one is a file you run with Node (24 or later), from here, after `make
js` at the repository's root. See [Run an example](#run-an-example). Start
with [`chat.ts`](basic/chat.ts), and [`restate-agents`](../typescript/packages/integrations/README.md),
the integrations they use. They come in three levels: **basic**, an agent
and what it keeps; **intermediate**, one of Restate's patterns at a time (its
tools, a person in the loop, teams, workflows, coding agents); and
**advanced**, applications that put them together.

### Basic

| File                                 | What                                                                                                                                          |
| ------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------- |
| [`chat.ts`](basic/chat.ts)           | **Start here.** A durable chat agent on pi, with a tool: `pi.agent({ name: 'assistant', tools: [weather] })`                                  |
| [`opencode.ts`](basic/opencode.ts)   | The same agent on OpenCode 2.0                                                                                                                |
| [`langgraph.ts`](basic/langgraph.ts) | The same agent on LangGraph                                                                                                                   |
| [`claude.ts`](basic/claude.ts)       | The same agent on Claude (the Anthropic SDK's tool runner; `ANTHROPIC_API_KEY`)                                                               |
| [`deepseek.ts`](basic/deepseek.ts)   | The same agent on the DeepSeek Harness (`DEEPSEEK_API_KEY`)                                                                                   |
| [`models.ts`](basic/models.ts)       | Models you set up yourself: LangGraph on `ChatOpenAI`s of your own (a gateway, no retries of its own; a high temperature)                     |
| [`memory.ts`](basic/memory.ts)       | An assistant that remembers facts about you in its own SQL table, through its tools (`ctx.db`), with a handler of your own beside the agent's |
| [`notes.ts`](basic/notes.ts)         | SQL in a virtual object, with `ctx.db`, and no agent                                                                                          |

### Intermediate

| File                                      | What                                                                                                                                        |
| ----------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| [`support.ts`](intermediate/support.ts)   | A support agent whose tools are Restate itself: a service call, a signal (a manager's approval) and a workflow                              |
| [`triage.ts`](intermediate/triage.ts)     | Agents as steps of a workflow: classify a ticket, draft a reply, and wait for a person's review                                             |
| [`router.ts`](intermediate/router.ts)     | A front desk of your own: a handler asks a model which team handles a message (`pi.prompt`), then calls that team's agent                   |
| [`team.ts`](intermediate/team.ts)         | A team on three frameworks: an editor (pi) hands research to a researcher (LangGraph) and drafting to a writer (OpenCode)                   |
| [`followup.ts`](intermediate/followup.ts) | An agent that schedules a check-in for a day later: `send` with a delay                                                                     |
| [`harness.ts`](intermediate/harness.ts)   | An agent each user configures: its model and persona from the object's database (`pi.chat(options)`), and `compact` and `reset` handlers    |
| [`coding.ts`](intermediate/coding.ts)     | A coordinator (pi) that hands coding tasks to coders of its own (OpenCode)                                                                  |
| [`review.ts`](intermediate/review.ts)     | A coder (OpenCode) made your own: a plugin that keeps its edits in `src/`, and a handler that writes code, then reviews it (`withOpenCode`) |
| [`ui/`](ui/app.ts)                        | A chat page for `support.ts` on Restate's own client: the conversation as the agent works, Approve and Reject, Steer, Stop (`npx vite ui`)  |

### Advanced

| Folder                                   | What                                                                                                                                                                                                                                                    |
| ---------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`gateway/`](advanced/gateway/README.md) | A tool gateway for agents: one per tenant, holding dangerous calls for approval, with limits per tenant and user, and an agent per case                                                                                                                 |
| [`factory/`](advanced/factory/README.md) | A software factory on Linear: a webhook starts it; a triage agent (pi) asks a planner (Claude) for a structured plan, then hands each part to a coder (DeepSeek), at once; each moves its issue along, through Linear's MCP server                      |
| [`pairing/`](advanced/pairing/README.md) | A pair programmer you talk with from a terminal, about a project: its loop in Restate, pi's coding tools in the project's directory; you steer it as it works; a reviewer on another model reads its diff; it ships through a workflow once you approve |

Each agent is a virtual object, one per key, whose conversation is in the
object's SQL database: a message is answered once per invocation, and a
crash resumes the reply. The integrations are in
[`restate-agents`](../typescript/packages/integrations/README.md).

## Run an example

```sh
OPENAI_API_KEY=… node basic/chat.ts                                          # the agent, on port 9080
npx @restatedev/restate-server  # Restate, in another terminal
npx @restatedev/restate deployments register http://localhost:9080           # tell Restate where the agent is
curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice'
curl localhost:8080/assistant/alice/messages                                 # the conversation so far
```

Without the CLI, register through the admin API:
`curl localhost:9070/deployments --json '{"uri": "http://localhost:9080"}'`.

With Restate in Docker, the agent is at the host's address, as the
container sees it:

```sh
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway ghcr.io/restatedev/restate:1.8.0
npx @restatedev/restate deployments register http://host.docker.internal:9080
```

The examples take OpenAI's models, with `OPENAI_API_KEY` (all but
`claude.ts` and `deepseek.ts`, on their providers' own, and
`factory/`, on all three). `MODEL` picks
another model, such as `MODEL=anthropic/claude-sonnet-5-5` with
`ANTHROPIC_API_KEY`, for the agents on pi, OpenCode and LangGraph
(LangGraph takes OpenAI's and Anthropic's); Claude takes Anthropic's
models, and DeepSeek its own, and refuse another provider's `MODEL` when
they start (see
[the models](../typescript/packages/integrations/README.md#models)). To call an agent
from code, or to send a message again safely, see
[Calling an agent](../typescript/packages/integrations/README.md#calling-an-agent).

## Python

The Python examples are in [`examples-python`](../examples-python/README.md).

## Write your own

A new file here, beside the others, runs the same way: its package has
`restate-agents`, the SDK and zod. For a package of your own, see
[Build your own](../typescript/packages/integrations/README.md#build-your-own).
