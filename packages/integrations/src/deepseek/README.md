# A DeepSeek Harness agent in a Restate virtual object

The [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)
(`@deepseek-ai/dsh-*`, on npm) is an agent harness built from plugins on
[cordis](https://www.npmjs.com/package/@deepseek-ai/cordis): an agent loop,
a model service with provider adapters, a tool registry, and an
event-sourced session, which is the source of truth for everything the agent
did. Its storage is a plugin too (`ctx.sessionPersistence`): here it is a
Restate virtual object's SQL database
([SQL in Restate virtual objects](../../../sdk/SQL.md)), one object per
conversation, which is the harness's session.

The integration uses the harness's `next` release line, 0.2.0-rc.2, pinned
exactly: its packages are release candidates, and require each other's exact
versions.

- [`persistence.ts`](persistence.ts): `SqlSessionPersistence`, the
  harness's session persistence over the object's database (`ctx.db`): each session's header, and its event log, one row per
  event. An append is kept in memory; a flush, the harness's durability
  barrier, stores what was appended before it in one SQL transaction.
- [`index.ts`](index.ts): `restate-agents/deepseek`. `chat({ model?, system?,
  tools? })` makes a handler, `handlers: { chat: deepseek.chat() }`, and `agent({ name, ...options })` a virtual object whose `chat` it is (with SQL set up, see [`object`](../object.ts)), that
  takes a message (a string) and returns the answer. It boots the harness over
  `ctx.db`: the services an agent loop needs, the persistence and the
  harness's session checkpoint policy, and the model adapter. It creates the
  session, or resumes it, gives the agent its tools, and answers the message.
  Its model is DeepSeek's (`@deepseek-ai/dsh-llm-deepseek-api-key`): `model`,
  or else `DEEPSEEK_MODEL`, by default `deepseek-flash`, with
  `DEEPSEEK_API_KEY`.
- [`tests/agents/deepseek.ts`](../../../../tests/agents/deepseek.ts): a DeepSeekAgent
  object per conversation, whose bank tools move money in the
  [ledger](../../../../tests/sql/ledger.ts), and [its tests](../../../../tests/agents/deepseek.test.ts):
  the handler with a fake `ctx`, through crashes, and a scripted model,
  [`tests/fakes/deepseek.ts`](../../../../tests/fakes/deepseek.ts).
- [`examples/deepseek.ts`](../../../../examples/deepseek.ts): a durable chat agent with a tool, `deepseek.agent({ name: 'assistant', tools: [weather] })`.

## Through crashes

The handler is not replayed: a retried attempt boots the harness again and
resumes the session from the stored log. The harness does not resume an
interrupted turn: resuming closes it, and leaves the model to decide what to
do about what it was doing. So:

- **A tool runs only once its call is stored.** The harness's checkpoint
  policy (`@deepseek-ai/dsh-session-checkpoint-policy`) flushes the session
  before each model request, before each tool body and before each step, and
  a flush stores the log in the database. A crash loses only events that
  nothing acted on.
- **A tool call stored without a result has an unknown outcome.** Resuming
  closes the interrupted turn and gives each such call an error result that
  says so, and tells the model to retry only an idempotent operation. The
  handler then asks the agent to continue.
- **A transfer is idempotent by its tool call id, and a retry names the call
  it retries.** The bank agent's transfer tool takes `retryOf`, the id of the call to
  retry, and uses it (or its own call id) as the transfer id, which the ledger
  dedupes on; so a retried transfer moves the money once. Its call to the
  ledger has an idempotency key made from the request id
  (`ctx.request().id`, the same on every attempt) and the transfer id, so a
  retried attempt reaches the call the first one made. The tool reports the
  transfer the ledger applied, so a retry whose details differ from the call
  it names says what really moved.
- **A tool's error fails the attempt**, and Restate retries it. By default the
  harness would hand the error to the model as the tool's result. The failed
  attempt stores nothing more (`abandon()`), so its retry finds the call
  without a result, and retries it as above. (The harness's teardown happens
  to close the session before the error result is appended, too, so the tests
  pass without `abandon()`; it makes the outcome independent of that order.)
- **A failed turn fails the attempt**: a model request that failed, or a
  checkpoint that could not store the log, ends the agent's turn with an
  error; the handler then throws rather than ask the agent to continue, and
  Restate retries. The failed turn's end may be stored by then, so only a
  turn of the current attempt fails it: the retry, or the next message,
  continues past the stored failure like an interrupted turn and asks the
  model again.
- **A message is answered once per invocation**: the message carries the
  invocation id (`ctx.request().id`), and a retried attempt returns the answer
  of the turn that finished it; a caller that resends a message uses an
  idempotency key. A message that a crash interrupted is finished before the
  next one starts.

[`deepseek.test.ts`](../../../../tests/agents/deepseek.test.ts) checks each of these, with a crash that
stops an attempt in a tool call and abandons it, keeping what it stored (the
next attempt fences it, as Restate does). Its scripted model makes a fresh tool call
id for every call, as a model does, so a retry that started the message over
would move the money twice: without the checkpoint policy four of its five
tests fail, and with a transfer tool that ignores `retryOf` three do. The
[crash test](../../../../tests/crash.ts) runs four conversations of eight
transfers and an audit each, beside the other agents', against a real server:
every message moves money exactly once.

## Run

```sh
make js   # from the repository root
cd examples && DEEPSEEK_API_KEY=… node deepseek.ts   # a chat session, on port 9080
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway ghcr.io/restatedev/restate:1.7.13   # Restate (see the repository's README, Requirements), in another terminal
restate deployments register http://host.docker.internal:9080   # as the server's container sees this host
curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice'
make test-ts T=deepseek   # the handler's tests, from the repository root
```
