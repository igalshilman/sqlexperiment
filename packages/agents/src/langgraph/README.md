# LangGraph

[LangGraph](https://langchain-ai.github.io/langgraphjs/) runs an agent as a
graph of steps and stores a checkpoint after each one, so a run can resume
where it stopped. Here the checkpoints are in the agent's SQL database
([SQL in Restate virtual objects](../../../sessions/README.md)), and the thread is
the object's key.

## Use

```ts
import { serve } from 'restate-agents';
import * as langgraph from 'restate-agents/langgraph';
import { weather } from './weather.ts';

const assistant = langgraph.agent({ name: 'assistant', system: 'Answer briefly.', tools: [weather] });
await serve(assistant);
```

| Option   | What                                                                                                                                                                                     |
| -------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `model`  | `openai/model-id` (`ChatOpenAI`) or `anthropic/model-id` (`ChatAnthropic`); or else `MODEL`, or else `openai/gpt-6-sol` (`OPENAI_API_KEY`); or any LangChain chat model that calls tools |
| `system` | The system prompt                                                                                                                                                                        |
| `tools`  | `tool()`s and Restate's tools of `restate-agents`, or LangChain's tools                                                                                                                  |
| `output` | A zod object: the answer as data, which the agent replies with as JSON, and `chat` answers with (see [Structured answers](../../README.md#structured-answers))                           |

For a session object of your own, `langgraph.chat(options)` is the handler and
`langgraph.messages()` the view; `options` may be a function of the
invocation's context (each user's system prompt, say). `langgraph.prompt(ctx,
text, options)` answers a prompt in a handler of your own, once, however
often the handler runs; each prompt of an invocation is a prompt of its own. The graph is the usual tool-calling
loop: a model node, a tools node, and back to the model until it answers.

## Through crashes

The checkpointer ([`checkpointer.ts`](checkpointer.ts), `SqlCheckpointSaver`)
keeps checkpoints, channel values and pending writes in three tables; a step
stores only the channels it changed. An agent's thread keeps its latest
checkpoint only (`history: 'latest'`), as each step changes the message
list: kept at every step, it would grow with the square of the
conversation. `new SqlCheckpointSaver(db)` of your own graph keeps them all,
for LangGraph's history. The handler is not replayed: a retried
attempt runs it again from the start, and LangGraph carries on from the last
step it stored.

- **Each step is stored before the next starts** (`durability: 'sync'`). So
  the tool calls a model made are stored before any tool runs, and a crash
  during the tools resumes them with the same tool call ids, without asking
  the model again.
- **A tool is idempotent by its tool call id** (`callId` of `tool()`'s
  `run`), which it passes on, such as in an idempotency key.
- **A tool's error fails the attempt**, and Restate retries it (the ToolNode
  would hand it to the model); a `restate.TerminalError` of a
  `restate-agents` tool is its answer to the model, and so are arguments
  that do not fit a tool, and a call of a tool the agent does not have.
- **A message is answered once per invocation**: the graph records each
  answer in its state, by invocation id, and a retried attempt returns it. A
  run that a crash interrupted (its prompt without an answer) is finished
  before the next message starts.

## Run

```sh
cd packages/examples && OPENAI_API_KEY=… node basic/langgraph.ts
```

With a Restate server, and the agent registered with it: see
[Run an example](../../../examples/README.md#run-an-example).

## Tests

The tests run on a scripted model, [`test/fakes/langgraph.ts`](../../test/fakes/langgraph.ts)
(`pnpm --filter restate-agents test langgraph`, from the repository root):

- [`test/storage/langgraph.test.ts`](../../test/storage/langgraph.test.ts):
  LangGraph's checkpointer conformance suite
  (`@langchain/langgraph-checkpoint-validation`, 718 tests); and
  [`langgraph-fork.test.ts`](../../test/storage/langgraph-fork.test.ts),
  a forked thread.
- [`test/langgraph.test.ts`](../../test/langgraph.test.ts):
  each point above, with crashes that stop an attempt in a tool call, or
  at any of its state requests; the
  system prompt; arguments that do not fit a tool; and `messages`. With
  `durability: 'async'`, it fails.
- [`test/langgraph.ts`](../../test/langgraph.ts): the
  bank agent, through crashes in [the crash test](../../../e2e/crash.ts)
  (`pnpm --filter e2e crash`): every message moves money exactly once.
