# Examples

SQL in Restate virtual objects, with the patched TypeScript SDK. `make
examples` builds them (see the [Makefile](../Makefile)); the crate's own
example is [`sqlite-kv/examples/lookup.rs`](../sqlite-kv/examples/lookup.rs).

One npm package, here, on the patched Restate TypeScript SDK and the
[addon](../sqlite-kv/node/README.md) (`make examples-js` installs it):

- [`chat/`](chat/chat.ts): the smallest one, a durable chat in a few
  lines: a `session` object whose `chat` is a pi agent, its conversation in
  the object's database.
- [`coding/`](coding/coding.ts): a coding team: a coordinator you chat with
  (pi), which hands each coding task to a coding agent of its own (OpenCode),
  an object per task, with its own database and directory.
- [`node/`](node/README.md): SQL in Restate virtual objects, how it works,
  and two examples:
  - [`ledger.ts`](node/ledger.ts): accounts and transfers, deduped by
    transfer id;
  - [`shop.ts`](node/shop.ts): warehouses and orders, each object with its
    own database, over idempotent RPC and delayed sends.
- [`pi/`](pi/README.md): a durable agent on pi-durable, whose storage is the
  object's database.
- [`langgraph/`](langgraph/README.md): a LangGraph agent, whose checkpoints
  are in the object's database, through a checkpointer that passes LangGraph's
  conformance suite.
- [`claude/`](claude/README.md): a Claude agent on the Anthropic SDK's tool
  runner, whose conversation is in the object's database.
- [`deepseek/`](deepseek/README.md): an agent on the DeepSeek Harness, whose
  event-sourced session is in the object's database, through a session
  persistence plugin.
- [`opencode/`](opencode/README.md): OpenCode 2.0, embedded, whose database
  (sessions, messages, durable events) is the object's database, through an
  injected SQL client.
- [`gateway/`](gateway/README.md): a tool gateway for agents: tools as
  plain journaled services, held for approval,
  and limited per tenant and user with scopes and limit rules.
- [`serve.ts`](serve.ts): serves them in one endpoint (`npm run serve`).
- [`faults.ts`](faults.ts): fault injection for the crash test.
- [`test.ts`](test.ts): all of them against a real Restate server, through
  crashes.
- [`testing.ts`](testing.ts): test support: an object's state in memory, and
  a fake object for an agent's handler, through crashes.
- [`testing/`](testing/fakes.ts): scripted models for the tests, which need
  no API key, in place of the real ones: `vi.mock` in the unit tests, and
  `node --import ./testing/fakes.ts` for the crash test's processes.

The agents use real models, with their provider's API key: by default
Anthropic's `claude-sonnet-5-5` (`ANTHROPIC_API_KEY`), and DeepSeek's
`deepseek-flash` for the DeepSeek agent (`DEEPSEEK_API_KEY`).

## TypeScript

The examples are TypeScript that Node runs as it is (type stripping, Node 24
or later, which OpenCode needs): `node serve.ts`, `node test.ts`; vitest runs
the tests. So they use only syntax that erases (no enums, namespaces or parameter
properties), and import each other by their `.ts` names.

- [`tsconfig.json`](tsconfig.json): type checking only, strict, with
  `noUncheckedIndexedAccess`, by TypeScript 7 (`npm run typecheck`).
- [`.oxlintrc.json`](.oxlintrc.json): oxlint, with type-aware rules such as
  `no-floating-promises` (`npm run lint`).
- [`.oxfmtrc.json`](.oxfmtrc.json): oxfmt formats the code and the docs
  (`npm run format`).

`npm run check` (or `make lint-examples`) runs all three; `make format`
formats.

## Validation

[`test.ts`](test.ts) runs every TypeScript example against a real server
through crashes. The ledger and the shop run in one process, aborting at 1.5%
of state requests; the pi agents run in another and the LangGraph agents in a
third, aborting at 0.3% (a pi prompt makes some 200 state requests, so one
attempt in three crashes). The server is killed twice. Meanwhile:

- 60 orders are placed (a quarter twice), half of them paid, the rest
  expiring;
- four sessions of each agent take eight transfer prompts each (a third sent
  twice) and an audit.

It checks that each warehouse's stock balances and agrees with every order,
that each prompt moved money exactly once, and, from the server's
`sys_invocation` table, that there is exactly one `reserve` and one `transfer`
invocation per order and prompt, though their callers ran many attempts. It
passed twice, with 109 and 108 restarts of the shop's process and 85 and 92 of
the agents', and again on the context with plain promises (81 and 111). With
the LangGraph agents too, it passed twice: 64 transfers, one per prompt, with
28 and 26 restarts of their process. With the Claude agents too, in a fourth process aborting
at 1.5% (a Claude prompt makes few state requests), it passed twice: 96
transfers, one per prompt, with 46 and 42 restarts of their process. With the
DeepSeek Harness agents too, in a fifth process aborting at 0.3% (a prompt
resumes the session from its stored log, some hundreds of state requests), it
passed twice: 128 transfers, one per prompt, with 11 and 18 restarts of their
process. In TypeScript, every example included, it passed twice again, with 10
and 14 restarts of the DeepSeek Harness agents' process. With the OpenCode
agents too, in a sixth process aborting at 0.1% (a prompt makes some hundreds
of state requests, and the process takes seconds to start), it passed twice on
Node 24: 160 transfers, one per prompt, with 44 and 32 restarts of their
process. Its first run found that OpenCode gives up on a turn after ten
resumes; see [`opencode/`](opencode/README.md#through-crashes).

```sh
make crash-test
```

[`concurrency.ts`](concurrency.ts) tests the context without journal replay
against a real server, with the inactivity timeout at one second: plain
promises under `Promise.all`, `Promise.race` and a hand-written combinator; a
call that starts unawaited, and one awaited late beside a pending 30 s sleep;
`ctx.run` refused with a terminal error; failures, invocation ids, statements
on `ctx.db` beside a call and a sleep; transactions sharing the attempt's
session, queued, a nested call refused and an escaping SQL error not retried;
a transaction's sends reaching their target; a suspension resuming on the
first of two operations (the call at 3 s, not the sleep at 20 s); and a
cancellation reaching three pending operations. A handler that replays its
journal keeps `RestatePromise`s.

```sh
make concurrency-test
```

Both run a released Restate server (see [the requirements](../README.md#requirements)):
by default the Makefile downloads `restate-server` from the `RESTATE_VERSION`
release for this platform. Set `RESTATE_FROM=image` to take it out of
`RESTATE_IMAGE` with docker instead, or `RESTATE_SERVER` to a binary.
