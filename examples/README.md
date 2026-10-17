# Examples

Each one is a file you run with Node (24 or later), from here, after `make
js` at the repository's root. See [Run an example](#run-an-example).

| File                            | What                                                                                                                                          |
| ------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| [`chat.ts`](chat.ts)            | A durable chat agent on pi, with a tool: `pi.agent({ name: 'assistant', tools: [weather] })`                                                  |
| [`opencode.ts`](opencode.ts)    | The same agent on OpenCode 2.0                                                                                                                |
| [`langgraph.ts`](langgraph.ts)  | The same agent on LangGraph                                                                                                                   |
| [`claude.ts`](claude.ts)        | The same agent on Claude (the Anthropic SDK's tool runner)                                                                                    |
| [`deepseek.ts`](deepseek.ts)    | The same agent on the DeepSeek Harness (`DEEPSEEK_API_KEY`)                                                                                   |
| [`coding.ts`](coding.ts)        | A coordinator (pi) that hands coding tasks to coders of its own (OpenCode)                                                                    |
| [`support.ts`](support.ts)      | A support agent whose tools are Restate itself: a service call, a signal (a manager's approval) and a workflow                                |
| [`memory.ts`](memory.ts)        | An assistant that remembers facts about you in its own SQL table, through its tools (`ctx.db`), with a handler of your own beside the agent's |
| [`harness.ts`](harness.ts)      | pi's Harness configured as it runs (`withPi`): each user's model and persona from its database, and `compact` and `reset` handlers            |
| [`team.ts`](team.ts)            | A team on three frameworks: an editor (pi) hands research to a researcher (LangGraph) and drafting to a writer (Claude)                       |
| [`followup.ts`](followup.ts)    | An agent that schedules a check-in for a day later: `send` with a delay                                                                       |
| [`triage.ts`](triage.ts)        | Agents as steps of a workflow: classify a ticket, draft a reply, and wait for a person's review                                               |
| [`notes.ts`](notes.ts)          | SQL in a virtual object, with `ctx.db`                                                                                                        |
| [`gateway/`](gateway/README.md) | A tool gateway for agents: approvals, and limits per tenant and user                                                                          |

Each agent is a virtual object, one per key, whose conversation is in the
object's SQL database: a message is answered once per invocation, and a
crash resumes the reply. The integrations are in
[`restate-agents`](../packages/integrations/README.md).

## Run an example

```sh
ANTHROPIC_API_KEY=… node chat.ts                      # the agent, on port 9080
npx @restatedev/restate-server                         # Restate, in another terminal
npx @restatedev/restate deployments register http://localhost:9080   # tell Restate where the agent is
curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice'
curl localhost:8080/assistant/alice/messages           # the conversation so far
```

Without the CLI, register through the admin API:
`curl localhost:9070/deployments --json '{"uri": "http://localhost:9080"}'`.
`make restate-server`, at the repository's root, downloads the server's
binary to `target/restate/restate-server`.

With Restate in Docker, the agent is at the host's address, as the
container sees it:

```sh
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway ghcr.io/restatedev/restate:1.7.13
npx @restatedev/restate deployments register http://host.docker.internal:9080
```

`MODEL` picks another model, such as `MODEL=openai/gpt-6-luna` with
`OPENAI_API_KEY`, for the agents on pi and OpenCode; Claude and LangGraph
take Anthropic's models, and DeepSeek its own (see
[the models](../packages/integrations/README.md#models)). To call an agent
from code, or to send a message again safely, see
[Calling an agent](../packages/integrations/README.md#calling-an-agent).

## Python

The Python examples are in [`python/examples`](../python/README.md#examples).
