# A LangGraph agent in a Restate virtual object

[LangGraph](https://langchain-ai.github.io/langgraphjs/) runs an agent as a
graph of steps and stores a checkpoint after each one, so a run can resume
where it stopped. Here the checkpoints live in a Restate virtual object's SQL
database ([SQL in Restate virtual objects](../../../sdk/SQL.md)): one object per
conversation, which is the LangGraph thread.

- [`checkpointer.ts`](checkpointer.ts): `SqlCheckpointSaver`, a LangGraph
  checkpointer over the object's database (`ctx.db`).
  Checkpoints, channel values and pending writes are rows in three tables.
  A step stores only the channels it changed, at their new versions, so a long
  conversation does not store its whole message list again at every step.
  Each `put` and `putWrites` is one SQL transaction.
- [`index.ts`](index.ts): `restate-agents/langgraph`. `chat({ model?,
  tools? })` makes a handler, `handlers: { chat: langgraph.chat() }`, and `agent({ name, ...options })` a virtual object whose `chat` it is (with SQL set up, see [`object`](../object.ts)), that
  takes a message (a string) and returns the answer. Its graph is the usual
  tool-calling loop (a model node, a tools node, back to the model until it
  answers):

  ```js
  new StateGraph(State)
    .addNode('model', async ({ messages }) => ({ messages: [await withTools.invoke(messages)] }))
    .addNode('tools', new ToolNode(tools, { handleToolErrors: false }))
    .addNode('answer', ({ prompt, messages }) => ({ answers: { [prompt]: messages.at(-1).text } }))
    .addEdge(START, 'model')
    .addConditionalEdges('model', toolsCondition, { tools: 'tools', [END]: 'answer' })
    .addEdge('tools', 'model')
    .addEdge('answer', END);
  ```

  The handler compiles the graph with the checkpointer over `ctx.db`, the
  thread being the object's key, and answers the message once per invocation
  (`ctx.request().id`).

  Its model is `model`, or else `MODEL`, by default
  `anthropic/claude-sonnet-5-5` (`ChatAnthropic`, with `ANTHROPIC_API_KEY`);
  or any LangChain chat model that calls tools, as `model`. The tests use a scripted one,
  [`tests/fakes/langgraph.ts`](../../../../tests/fakes/langgraph.ts).

- [`tests/storage/langgraph.test.ts`](../../../../tests/storage/langgraph.test.ts):
  LangGraph's checkpointer conformance suite
  (`@langchain/langgraph-checkpoint-validation`, 718 tests) on the
  checkpointer, over a sqlite-kv database in memory.
- [`tests/storage/langgraph-fork.test.ts`](../../../../tests/storage/langgraph-fork.test.ts):
  a thread forked from an earlier checkpoint, which the suite does not cover:
  each branch keeps its own values.
- [`tests/storage/langgraph-upgrade.test.ts`](../../../../tests/storage/langgraph-upgrade.test.ts):
  a thread checkpointed with numeric versions goes on with the current
  checkpointer.
- [`tests/agents/langgraph.ts`](../../../../tests/agents/langgraph.ts): a GraphAgent object
  per conversation, whose tools move money in the [ledger](../../../../tests/sql/ledger.ts),
  and [its tests](../../../../tests/agents/langgraph.test.ts): the handler with a fake
  `ctx`, through crashes.
- [`examples/langgraph.ts`](../../../../examples/langgraph.ts): a durable
  chat agent with a tool, `langgraph.agent({ name: 'assistant', tools: [weather] })`.

## Through crashes

The handler is not replayed: a retried attempt runs it again from the start,
and LangGraph carries on from the last step it stored.

- **Each step is stored before the next starts** (`durability: 'sync'`; by
  default LangGraph stores a step while the next one runs). So the tool calls
  a model made are stored before any tool runs, and a crash during the tools
  resumes them with the same tool call ids, without asking the model again.
  The checkpoint's commit is stored in order with everything after it, the
  tools' calls included.
- **A tool is idempotent by its tool call id**: the bank agent's uses it in
  the transfer id, which the ledger dedupes on, so a tool run again moves the money once. Its
  call to the ledger has an idempotency key made from the request id
  (`ctx.request().id`, the same on every attempt) and the tool call id, so a
  retried attempt reaches the call the first one made.
- **A tool's error fails the attempt** (`handleToolErrors: false`), and
  Restate retries it. By default LangGraph's `ToolNode` would hand the error
  to the model as the tool's result, and the model would answer with it.
- **A message is answered once per invocation**: the graph records each
  answer in its state, by invocation id, and a retried attempt returns it; a
  caller that resends a message uses an idempotency key. A run that a crash
  interrupted is finished before the next message starts.

[`langgraph.test.ts`](../../../../tests/agents/langgraph.test.ts) checks each of these, with a crash that
stops an attempt in a tool call and abandons it, keeping what it committed;
with `durability: 'async'` or `'exit'`, or `ToolNode`'s default error
handling, it fails. The [crash test](../../../../tests/crash.ts) runs four
conversations of eight transfers and an audit each, beside the other agents',
against a real server: every message moves money exactly once.

## Run

```sh
make js   # from the repository root
cd examples && ANTHROPIC_API_KEY=… node langgraph.ts   # a chat session, on port 9080
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway ghcr.io/restatedev/restate:1.7.13   # Restate (see the repository's README, Requirements), in another terminal
restate deployments register http://host.docker.internal:9080   # as the server's container sees this host
curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice'
make test-ts T=langgraph   # the conformance suite and the handler's tests, from the repository root
```
