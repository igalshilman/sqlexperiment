# Examples

Each one is a file you run with Node (24 or later), from here, after `make
js` at the repository's root. Then register it with a Restate server and
call it:

```sh
ANTHROPIC_API_KEY=… node chat.ts
restate deployments register http://localhost:9080
curl localhost:8080/assistant/alice/chat --json '"Hi, I am Alice"'
```

| File                            | What                                                                           |
| ------------------------------- | ------------------------------------------------------------------------------ |
| [`chat.ts`](chat.ts)            | A durable chat agent on pi, with a tool: `chat: pi.chat({ tools: [weather] })` |
| [`opencode.ts`](opencode.ts)    | The same agent on OpenCode 2.0                                                 |
| [`langgraph.ts`](langgraph.ts)  | The same agent on LangGraph                                                    |
| [`claude.ts`](claude.ts)        | The same agent on Claude (the Anthropic SDK's tool runner)                     |
| [`deepseek.ts`](deepseek.ts)    | The same agent on the DeepSeek Harness (`DEEPSEEK_API_KEY`)                    |
| [`coding.ts`](coding.ts)        | A coordinator (pi) that hands coding tasks to coders of its own (OpenCode)     |
| [`notes.ts`](notes.ts)          | SQL in a virtual object, with `ctx.db`                                         |
| [`gateway/`](gateway/README.md) | A tool gateway for agents: approvals, and limits per tenant and user           |

Each agent is a virtual object, one per key, whose conversation is in the
object's SQL database: a message is answered once per invocation, and a
crash resumes the reply. The integrations are in
[`restate-agents`](../packages/integrations/README.md).
