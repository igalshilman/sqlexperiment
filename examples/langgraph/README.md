# A LangGraph agent in a Restate virtual object

[LangGraph](https://langchain-ai.github.io/langgraphjs/) runs an agent as a
graph of steps and stores a checkpoint after each one, so a run can resume
where it stopped. Here the checkpoints live in a Restate virtual object's SQL
database ([SQL in Restate virtual objects](../node/README.md)): one object per
conversation, which is the LangGraph thread.

- [`checkpointer.mjs`](checkpointer.mjs): `SqlCheckpointSaver`, a LangGraph
  checkpointer over the object's database (the `db` of `ctx.sqlSession`).
  Checkpoints, channel values and pending writes are rows in three tables.
  A step stores only the channels it changed, at their new versions, so a long
  conversation does not store its whole message list again at every step.
  Each `put` and `putWrites` is one SQL transaction.
- [`agent.mjs`](agent.mjs): a GraphAgent object per conversation. Its graph is
  the usual tool-calling loop (a model node, a tools node, back to the model
  until it answers), whose tools move money in the
  [ledger](../node/ledger.mjs):

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

  Its handler, `prompt(ctx, { id, text })`, compiles the graph with the
  checkpointer, in one `ctx.sqlSession`, and answers the prompt once per id.
- [`teller.mjs`](teller.mjs): a scripted chat model that needs no API key, so
  the agent and its tests run offline. Any LangChain chat model that calls
  tools takes its place.
- [`checkpointer.test.mjs`](checkpointer.test.mjs): LangGraph's checkpointer
  conformance suite (`@langchain/langgraph-checkpoint-validation`, 718 tests)
  on the checkpointer, over a sqlite-kv database in memory.
- [`agent.test.mjs`](agent.test.mjs): the handler with a fake `ctx`, through
  crashes.

## Through crashes

The handler is not replayed: a retried attempt runs it again from the start,
and LangGraph carries on from the last step it stored.

- **Each step is stored before the next starts** (`durability: 'sync'`; by
  default LangGraph stores a step while the next one runs). So the tool calls
  a model made are stored before any tool runs, and a crash during the tools
  resumes them with the same tool call ids, without asking the model again.
  The checkpoint's commit is stored in order with everything after it, the
  tools' calls included.
- **A tool is idempotent by its tool call id**: it uses it in the transfer id,
  which the ledger dedupes on, so a tool run again moves the money once. Its
  call to the ledger has an idempotency key made from the request id
  (`ctx.request().id`, the same on every attempt) and the tool call id, so a
  retried attempt reaches the call the first one made.
- **A tool's error fails the attempt** (`handleToolErrors: false`), and
  Restate retries it. By default LangGraph's `ToolNode` would hand the error
  to the model as the tool's result, and the model would answer with it.
- **A prompt is answered once per id**: the graph records each answer in its
  state, and a prompt asked again returns it. A run that a crash interrupted
  is finished before the next prompt starts.

[`agent.test.mjs`](agent.test.mjs) checks each of these, with a crash that
stops an attempt in a tool call and abandons it, keeping what it committed;
with `durability: 'async'` or `'exit'`, or `ToolNode`'s default error
handling, it fails. The [crash test](../README.md#validation) runs four
conversations of eight transfers and an audit each, beside the pi agent's,
against a real server: every prompt moves money exactly once.

## Run

```sh
make examples-js   # from the repository root
cd examples && node langgraph/agent.mjs   # the agent and the ledger, on port 9080
restate deployments register http://localhost:9080
curl localhost:8080/Ledger/bank/init --json '[8, 1000]'
curl localhost:8080/GraphAgent/alice/prompt --json '{"id": "1", "text": "Please move 5 from 1 to 2"}'
npm run test:langgraph   # the conformance suite and the handler's tests
```
