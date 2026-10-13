# An OpenCode 2.0 agent in a Restate virtual object

[OpenCode](https://opencode.ai) 2.0 can run embedded: `@opencode/sdk` hosts
the whole OpenCode server in your process, with no HTTP listener, and its
workerd profile runs it in a Cloudflare Durable Object, on the object's
SQLite. Here it runs in a Restate virtual object instead: one OpenCodeAgent
object per session, whose OpenCode database (sessions, messages, the inbox,
durable events) is the object's SQL database
([SQL in Restate virtual objects](../node/README.md)).

The example pins OpenCode 2.0.27 exactly, with the Effect release it is
built on (4.0.1). OpenCode needs Node 24.

- [`opencode-db.ts`](opencode-db.ts): `openOpenCode(db, fence, options)`
  boots OpenCode's embedded host with its database in `db`, the database of
  `ctx.sqlSession`. The host is OpenCode's workerd profile (no local
  filesystem, shell, terminal or version control; config inline; plugins in
  code) but for its database. The workerd profile reads a Durable Object's
  SQLite, whose `exec` is synchronous; here OpenCode gets an Effect
  `SqlClient` instead, which is asynchronous, through
  `Database.configuredClient`, OpenCode's seam for an injected client. Its
  statements run one at a time, and each OpenCode transaction is a sqlite-kv
  transaction. (The Promise API's `OpenCode.create` drops its replacements,
  so the host is booted with the Effect API's.)
- [`with-opencode.ts`](with-opencode.ts): `withOpenCode(ctx, { plugins,
  model, tools }, work)` runs `work` with OpenCode for one invocation, in one
  `ctx.sqlSession`, and `prompt(id, text)` answers a prompt in the object's
  session. The agent's tools replace OpenCode's own.
- [`agent.ts`](agent.ts): the OpenCodeAgent object. Its handler,
  `prompt(ctx, { id, text })`, gives the agent its bank tools, which move
  money in the [ledger](../node/ledger.ts), and answers the prompt.
- [`teller.ts`](teller.ts): a scripted model, an AI SDK language model that
  OpenCode calls through a plugin's `language` hook, so the agent and its
  tests run without an API key; or a real model with
  `OPENCODE_MODEL=provider/model-id` (such as
  `anthropic/claude-sonnet-5-5`, with the provider's API key), which
  OpenCode's own providers serve.
- [`agent.test.ts`](agent.test.ts): the handler with a fake `ctx`, through
  crashes.

## Through crashes

The handler is not replayed: a retried attempt boots OpenCode again, on the
stored database. OpenCode resumes interrupted work on its own: it records an
execution claim when a turn starts, releases it when the turn ends, and
resumes every claimed turn when it boots (in a Durable Object, eviction mid
turn leaves the same claim). Restate runs one attempt of an object's handler
at a time, so a claim found at boot is always one a dead attempt left.

- **Restate decides how often a turn is retried.** OpenCode counts the
  resumes of a turn, and fails the turn after ten ("interrupted repeatedly"),
  so that a turn that keeps crashing its process cannot loop forever. Here
  that is the object's retry policy's job, so `openOpenCode` replaces
  OpenCode's restart service with one without a limit. (The first crash test
  run found this: a prompt crashed more than ten times and failed.)
- **A tool runs only once its call is stored.** OpenCode stores the model's
  reply, with its tool calls, before the tools run.
- **A tool call that was running has an interrupted outcome.** Resuming gives
  it an "interrupted" result, and asks the model to continue without
  repeating completed work.
- **A transfer is idempotent by its tool call id, and a retry names the call
  it retries.** The transfer tool takes `retryOf`, the id of the call to
  retry, and uses it (or its own call id) as the transfer id, which the
  ledger dedupes on; so a retried transfer moves the money once. Its call to
  the ledger has an idempotency key made from the request id
  (`ctx.request().id`, the same on every attempt) and the transfer id, so a
  retried attempt reaches the call the first one made.
- **A tool's error fails the attempt**, and Restate retries it. OpenCode would
  hand the error to the model as the tool's result. Instead the failed
  attempt stores nothing more (its fence closes, and the tool call never
  returns), so its retry finds the call interrupted, and retries it as above.
- **A prompt is answered once per id**: the prompt's id is its OpenCode
  message id, and a prompt asked again returns the answer of the turn that
  answered it. A prompt that a crash interrupted is finished before the next
  prompt starts: OpenCode resumes it at boot, and queues the next one.

[`agent.test.ts`](agent.test.ts) checks each of these, with a crash that
stops an attempt in a tool call and abandons it, keeping what it stored (the
next attempt fences it, as Restate does). The teller makes a fresh tool call
id for every call, as a model does, so a retry that started the prompt over
would move the money twice: with a transfer tool that ignores `retryOf`,
four of its five tests fail. The [crash test](../README.md#validation) runs
four sessions of eight transfers and an audit each, beside the other
agents', against a real server: every prompt moves money exactly once.

A prompt takes some hundreds of SQL statements (OpenCode's migrations check,
its session, messages, inbox and durable events), and booting OpenCode takes
about 100 ms per attempt, after a one-time import of about 2 s per process.

## Run

```sh
make examples-js   # from the repository root
cd examples && node opencode/agent.ts   # the agent and the ledger, on port 9080
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway ghcr.io/restatedev/restate:1.7.13   # Restate (see the repository's README, Requirements), in another terminal
restate deployments register http://host.docker.internal:9080   # as the server's container sees this host
curl localhost:8080/Ledger/bank/init --json '[8, 1000]'
curl localhost:8080/OpenCodeAgent/alice/prompt --json '{"id": "p1", "text": "Please move 5 from 1 to 2"}'
npm run test:opencode   # the handler's tests
```
