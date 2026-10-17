# A Claude agent in a Restate virtual object

The Anthropic SDK's tool runner (`client.beta.messages.toolRunner` in
[`@anthropic-ai/sdk`](https://github.com/anthropics/anthropic-sdk-typescript)) is a light agent
harness: the tool-calling loop, in the process, with nothing stored. It keeps
the conversation in its params, so an agent built on it is durable when the
conversation is: here it is in a Restate virtual object's SQL database
([SQL in Restate virtual objects](../../../sdk/SQL.md)), one object per
conversation.

It is lighter than the Claude Agent SDK (`@anthropic-ai/claude-agent-sdk`),
which runs Claude Code as a subprocess and mirrors its transcripts to a store
after the fact (`sessionStore`), so a tool can run before its call is stored.
Claude Code's own tools (bash, read, write, edit, glob, grep) come to the
runner as `betaAgentToolset20260401` from `@anthropic-ai/sdk/tools/agent-toolset/node`;
the agent takes whatever tools you give it.

- [`conversation.ts`](conversation.ts): `Conversation`, the messages over
  the object's database (`ctx.db`), in order, each with
  the id of the invocation (`ctx.request().id`) it belongs to, and the answer
  to each. Each `append` is one SQL transaction.
- [`index.ts`](index.ts): `restate-agents/claude`. `chat({ model?, system?,
  tools? })` makes a handler, `handlers: { chat: claude.chat() }`, and `agent({ name, ...options })` a virtual object whose `chat` it is (with SQL set up, see [`object`](../object.ts)), that takes
  a message (a string) and returns the answer: it starts a runner with the
  stored messages in `ctx.db`, and stores each step:

  ```js
  for await (const message of runner) {
    const reply = { role: 'assistant', content: message.content };
    if (!message.content.some((c) => c.type === 'tool_use')) {
      await conversation.append(id, [reply], textOf(message.content));
      return;
    }
    // The reply first, then its tools.
    await conversation.append(id, [reply]);
    const results = await runner.generateToolResponse();
    check();
    await conversation.append(id, [results]);
  }
  ```

  A tool gets its tool call id from `callId(context)`. A tool may be given
  per invocation, as a function of `ctx`.

- [`tests/agents/claude.ts`](../../../../tests/agents/claude.ts): a ClaudeAgent object per
  conversation, whose tools move money in the [ledger](../../../../tests/sql/ledger.ts), and
  [its tests](../../../../tests/agents/claude.test.ts): the handler with a fake `ctx`,
  through crashes, and a scripted model ([`tests/fakes/claude.ts`](../../../../tests/fakes/claude.ts)).
- [`examples/claude.ts`](../../../../examples/claude.ts): a durable chat agent with a tool, `claude.agent({ name: 'assistant', tools: [weather] })`.

The model is `model`, or else `CLAUDE_MODEL`, by default `claude-sonnet-5-5`, with
`ANTHROPIC_API_KEY`.

## Through crashes

The handler is not replayed: a retried attempt runs it again from the start,
with the stored messages.

- **Each reply is stored before its tools run, and their results before the
  next request.** The runner runs a reply's tools after the loop body, and
  `generateToolResponse()` in the body runs them there instead (the runner
  keeps their results and does not run them again). So a crash during the
  tools resumes them with the same tool call ids, without asking the model
  again: an attempt that finds a stored reply with no results runs its tools
  first.
- **A tool is idempotent by its tool call id** (`callId(context)`): the bank
  agent's uses it in the transfer id, which the ledger dedupes on, so a tool run again moves
  the money once. Its call to the ledger has an idempotency key made from the
  request id (`ctx.request().id`, the same on every attempt) and the tool call
  id, so a retried attempt reaches the call the first one made.
- **A tool's error fails the attempt**, and Restate retries it. By default the
  runner would hand the error to the model as the tool's result, and the model
  would answer with it.
- **A message is answered once per invocation** (`ctx.request().id`): its
  answer is stored with the model's last reply, and a retried attempt returns
  it; a caller that resends a message uses an idempotency key. A message that
  a crash interrupted is finished before the next one starts.

[`claude.test.ts`](../../../../tests/agents/claude.test.ts) checks each of these, with a crash that
stops an attempt in a tool call and abandons it, keeping what it committed;
storing the reply with its results, after the tools, or handing a tool's
error to the model, fails it. The [crash test](../../../../tests/crash.ts) runs
four conversations of eight transfers and an audit each, beside the other
agents', against a real server: every message moves money exactly
once.

## Run

```sh
make js   # from the repository root
cd examples && ANTHROPIC_API_KEY=… node claude.ts   # a chat session, on port 9080
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway ghcr.io/restatedev/restate:1.7.13   # Restate (see the repository's README, Requirements), in another terminal
restate deployments register http://host.docker.internal:9080   # as the server's container sees this host
curl localhost:8080/assistant/alice/chat --json '"Hi, I am Alice"'
make test-ts T=claude   # the handler's tests, from the repository root
```
