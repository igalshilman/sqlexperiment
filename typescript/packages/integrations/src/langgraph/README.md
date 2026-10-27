# LangGraph

[LangGraph](https://langchain-ai.github.io/langgraphjs/) runs an agent as a
graph of steps and stores a checkpoint after each one, so a run can resume
where it stopped. Here the checkpoints are in the agent's SQL database
([SQL in Restate virtual objects](../../../sql/README.md)), and the thread is
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

For an object of your own, `langgraph.chat(options)` and
`langgraph.messages()` are the handlers; `options` may be a function of the
invocation's context (each user's system prompt, say). `langgraph.prompt(ctx,
text, options)` answers a prompt in a handler of your own, once, however
often the handler runs; each prompt of an invocation is a prompt of its own. The graph is the usual tool-calling
loop: a model node, a tools node, and back to the model until it answers.

## Through crashes

The checkpointer ([`checkpointer.ts`](checkpointer.ts), `SqlCheckpointSaver`)
keeps checkpoints, channel values and pending writes in three tables; a step
stores only the channels it changed, so a long conversation does not store
its whole message list at every step. The handler is not replayed: a retried
attempt runs it again from the start, and LangGraph carries on from the last
step it stored.

- **Each step is stored before the next starts** (`durability: 'sync'`). So
  the tool calls a model made are stored before any tool runs, and a crash
  during the tools resumes them with the same tool call ids, without asking
  the model again.
- **A tool is idempotent by its tool call id** (`callId` of `tool()`'s
  `run`), which it passes on, such as in an idempotency key.
- **A tool's error fails the attempt** (`handleToolErrors: false`), and
  Restate retries it; a `restate.TerminalError` of a `restate-agents` tool is
  its answer to the model.
- **A message is answered once per invocation**: the graph records each
  answer in its state, by invocation id, and a retried attempt returns it. A
  run that a crash interrupted (its prompt without an answer) is finished
  before the next message starts.

## Run

```sh
cd examples-typescript && OPENAI_API_KEY=… node langgraph.ts
```

With a Restate server, and the agent registered with it: see
[Run an example](../../../../../examples-typescript/README.md#run-an-example).

## Tests

The tests run on a scripted model, [`tests/fakes/langgraph.ts`](../../../../tests/fakes/langgraph.ts)
(`make test-ts T=langgraph`, from the repository root):

- [`tests/storage/langgraph.test.ts`](../../../../tests/storage/langgraph.test.ts):
  LangGraph's checkpointer conformance suite
  (`@langchain/langgraph-checkpoint-validation`, 718 tests);
  [`langgraph-fork.test.ts`](../../../../tests/storage/langgraph-fork.test.ts),
  a forked thread; and
  [`langgraph-upgrade.test.ts`](../../../../tests/storage/langgraph-upgrade.test.ts),
  a thread of an earlier version of the checkpointer.
- [`tests/agents/langgraph.test.ts`](../../../../tests/agents/langgraph.test.ts):
  each point above, with crashes that stop an attempt in a tool call, or
  at any of its state requests; the
  system prompt; and `messages`. With `durability: 'async'`, or `ToolNode`'s
  default error handling, it fails.
- [`tests/agents/langgraph.ts`](../../../../tests/agents/langgraph.ts): the
  bank agent, through crashes in [the crash test](../../../../tests/crash.ts)
  (`make crash-test`): every message moves money exactly once.
