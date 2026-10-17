# An OpenCode 2.0 agent in a Restate virtual object

[OpenCode](https://opencode.ai) 2.0 can run embedded: `@opencode/sdk` hosts
the whole OpenCode server in your process, with no HTTP listener, and its
workerd profile runs it in a Cloudflare Durable Object, on the object's
SQLite. Here it runs in a Restate virtual object instead: one object per
session, whose OpenCode database (sessions, messages, the inbox,
durable events) is the object's SQL database
([SQL in Restate virtual objects](../../../sdk/SQL.md)).

The integration pins OpenCode 2.0.27 exactly, with the Effect release it is
built on (4.0.1). OpenCode needs Node 24.

- [`db.ts`](db.ts) (`restate-agents/opencode/db`): `openOpenCode(db, fence, options)`
  boots OpenCode's embedded host with its database in `db`, the object's database,
  `ctx.db`. The host is OpenCode's workerd profile (no local
  filesystem, shell, terminal or version control; config inline; plugins in
  code) but for its database. The workerd profile reads a Durable Object's
  SQLite, whose `exec` is synchronous; here OpenCode gets an Effect
  `SqlClient` instead, which is asynchronous, through
  `Database.configuredClient`, OpenCode's seam for an injected client. Its
  statements run one at a time, and each OpenCode transaction is a sqlite-kv
  transaction. (The Promise API's `OpenCode.create` drops its replacements,
  so the host is booted with the Effect API's.)
- [`index.ts`](index.ts): `restate-agents/opencode`. `withOpenCode(ctx, {
  plugins, model, tools, directory }, work)` runs `work` with OpenCode for one
  invocation, over `ctx.db`, and its `prompt(id, text)` answers a prompt in
  the object's session; `prompt(ctx, text, options)` answers a message once
  per invocation (`ctx.request().id`); `tool(...)` defines a tool. `chat({
  model?, tools?, directory? })` makes a handler, `handlers: { chat:
  opencode.chat() }`, that takes a message (a string) and returns the answer.
  The agent's tools replace OpenCode's own, unless it has a `directory` of
  the local machine to work in, where it keeps OpenCode's own tools too.
- [`tests/agents/opencode.ts`](../../../../tests/agents/opencode.ts): the OpenCodeAgent
  object, whose bank tools move money in the [ledger](../../../../tests/sql/ledger.ts), and
  [its tests](../../../../tests/agents/opencode.test.ts): the handler with a fake `ctx`,
  through crashes, and a scripted model
  ([`tests/fakes/opencode.ts`](../../../../tests/fakes/opencode.ts)), an AI SDK language
  model that OpenCode calls through a plugin's `language` hook.
- [`tests/storage/opencode.test.ts`](../../../../tests/storage/opencode.test.ts): the
  Effect `SqlClient` of `db.ts` over a sqlite-kv database in memory.
- [`examples/opencode.ts`](../../../../examples/opencode.ts): a durable chat,
  one `session` object with `chat: opencode.chat()`; and
  [`examples/coding.ts`](../../../../examples/coding.ts), OpenCode coders,
  each with a directory, under a pi coordinator.

The model is `model`, or else `OPENCODE_MODEL=provider/model-id`, by default
`anthropic/claude-sonnet-5-5` (with `ANTHROPIC_API_KEY`), which OpenCode's
own providers serve.

## What embedded means here

OpenCode runs in the handler's own Node process: no child process, no
listening socket, no network hop. `@opencode/sdk` assembles OpenCode's
server (sessions, the agent loop, plugins, model providers, the event log)
as a library, and its client calls the server's HTTP router through an
in-process `fetch`: requests and responses are objects in memory. With the
host booted and a turn answered, the process had no child process and no
listening socket, and OpenCode's tables (20 of them) were in the object's
database. Each attempt boots the host (about 100 ms) and closes it at the
end. The only traffic out is the model's, to its provider's API; tools reach
other services through Restate.

It is OpenCode's agent core, not its terminal UI, and in the workerd
profile it spawns nothing either: its shell, terminal, file and version
control services are disabled or stubbed. An agent that should run commands
needs a tool that runs them elsewhere (a sandbox, a service), idempotently,
like the bank agent's ledger tools in the tests.

## What we found

- **OpenCode resumes on its own.** A turn holds an execution claim from its
  start to its end, and booting resumes every claimed turn: the same
  recovery a Durable Object's eviction relies on is what a retried Restate
  attempt gets.
- **A resumed turn does not rerun a tool.** A tool call that was running
  gets an "interrupted" result, and a synthetic message asks the model to
  continue without repeating completed work, as the DeepSeek Harness does.
  So a tool that must not run twice needs `retryOf` (or an equivalent).
- **OpenCode fails a turn after ten resumes.** Under Restate the retry
  policy decides; the first crash test run hit the limit.
- **The database is a seam.** `Database.configuredClient` takes any Effect
  `SqlClient`; nothing in OpenCode needed the Durable Object's synchronous
  `exec`.
- **The Promise API's `OpenCode.create(options)` ignores its second
  argument** (the layer replacements), though its types declare it; the
  Effect API's `create` honours it, and its client's calls are Effects.
- **A prompt waits in the inbox before it is a message**, and a prompt
  queued while a turn runs is answered in that turn's run: a prompt's answer
  is the last assistant message before the turn's idle marker, or before the
  next prompt. So a new prompt waits for an unfinished one: queued into a
  resumed turn, it would be answered in place of the rest of it.
- **Node 24.** OpenCode's packages use `await using`, which Node 22 does not
  parse.
- **Cost.** A prompt makes about 230 store requests; importing OpenCode
  takes about 2.3 s per process, which is why its crash test process aborts
  at 0.1%.

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
  it retries.** The bank agent's transfer tool takes `retryOf`, the id of the call to
  retry, and uses it (or its own call id) as the transfer id, which the
  ledger dedupes on; so a retried transfer moves the money once. Its call to
  the ledger has an idempotency key made from the request id
  (`ctx.request().id`, the same on every attempt) and the transfer id, so a
  retried attempt reaches the call the first one made. The tool reports the
  transfer the ledger applied, so a retry whose details differ from the call
  it names says what really moved.
- **A tool's error fails the attempt**, and Restate retries it. OpenCode would
  hand the error to the model as the tool's result. Instead the failed
  attempt stores nothing more (its fence closes, and the tool call never
  returns), so its retry finds the call interrupted, and retries it as above.
- **A message is answered once per invocation**: the invocation id
  (`ctx.request().id`) is its OpenCode message id, and a retried attempt
  returns the answer of the turn that answered it; a caller that resends a
  message uses an idempotency key. A prompt that a crash interrupted is
  finished before the next prompt starts: OpenCode resumes it at boot, and a new prompt waits for it,
  since one queued while it runs would join its turn.

[`opencode.test.ts`](../../../../tests/agents/opencode.test.ts) checks each of these, with a crash that
stops an attempt in a tool call and abandons it, keeping what it stored (the
next attempt fences it, as Restate does). Its scripted model makes a fresh tool call
id for every call, as a model does, so a retry that started the prompt over
would move the money twice: with a transfer tool that ignores `retryOf`,
four of its five tests fail. The [crash test](../../../../tests/crash.ts) runs
four sessions of eight transfers and an audit each, beside the other
agents', against a real server: every message moves money exactly once.

A prompt takes some hundreds of SQL statements (OpenCode's migrations check,
its session, messages, inbox and durable events), and booting OpenCode takes
about 100 ms per attempt, after a one-time import of about 2 s per process.

## Run

```sh
make js   # from the repository root
cd examples && ANTHROPIC_API_KEY=… node opencode.ts   # a chat session, on port 9080
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway ghcr.io/restatedev/restate:1.7.13   # Restate (see the repository's README, Requirements), in another terminal
restate deployments register http://host.docker.internal:9080   # as the server's container sees this host
curl localhost:8080/session/alice/chat --json '"Hi, I am Alice"'
make test-ts T=opencode   # the handler's tests, from the repository root
```
