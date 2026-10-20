# The patched TypeScript SDK

The TypeScript SDK 1.17.4 with one change that is not released: with
explicit recovery (`restate.internal.explicitRecovery()`), a handler's
promises can be awaited concurrently. [`restate-sql`](../sql/README.md) needs
it: a database's reads are concurrent, and so are a handler's own awaits
beside them. Everything else (SQL in a virtual object, its contexts) is that
library, on the SDK's public API.

|                                 | What                      | Version                                                                                                            | Form                                            |
| ------------------------------- | ------------------------- | ------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------- |
| [`packages/sdk-typescript/`](.) | `@restatedev/restate-sdk` | 1.17.4 ([`b1080a1`](https://github.com/restatedev/sdk-typescript/commit/b1080a11984b35419e0e825a0e82bc17c656a22d)) | A patch, applied to a fresh clone by `setup.sh` |

## TypeScript

[`setup.sh`](setup.sh) (`make sdk-ts`) clones the SDK at the pinned release
into `.build/` (ignored), applies [`patches/`](patches), and packs the SDK as
`.build/restatedev-restate-sdk-1.17.4.tgz`, which the pnpm workspace installs
wherever `@restatedev/restate-sdk` is used (an override in
[`pnpm-workspace.yaml`](../../pnpm-workspace.yaml)). It needs git, node and
pnpm. After editing the clone, [`patches/regenerate.sh`](patches/regenerate.sh)
writes the patch back.

## The change

The published SDK drives each awaited `RestatePromise` with a loop of its
own, and only one of them may wait for progress from the server at a time
(`ExternalProgressChannel`: "At most one `awaitNext` may be pending at a
time; a concurrent second call rejects"). So two promises awaited at once
that both need the server, such as `Promise.all([ctx.sleep(10), call])` or
two state reads with lazy state, fail the attempt with `awaitNext already
pending`, and it is retried, forever. `RestatePromise.all` and
`RestatePromise.race` avoid it by making one promise of many; a library's
promises cannot join them.

With journal replay, that order of awaits is part of what a replay must
match. With explicit recovery nothing is replayed, so the patch drives every
awaited promise of the invocation from one loop, `PromiseSelector`: each
promise registers with it when it is awaited, and it asks the VM about all of
them at once (one `FirstCompleted` future), as the only one waiting for
external progress. A suspension then resumes on the first of them to
complete, and a cancellation reaches all of them. Without explicit recovery
nothing changes.

| Change                                                                                                          | Where                                                            |
| --------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------- |
| The selector                                                                                                    | `promise_selector.ts`, `test/promise_selector.test.ts`           |
| With explicit recovery, `PromisesExecutor` hands each awaited promise to it, and closes it at the attempt's end | `promises.ts`, `context_impl.ts`, `endpoint/handlers/generic.ts` |
| An ephemeral state get (protocol v8, `getProject`) registers with it, so that its notification is driven        | `context_impl.ts`                                                |

The SDK's tests pass with the patch (64, of which 22 are the selector's,
against a fake VM and the published single-waiter progress channel).
[`tests/concurrency.ts`](../../tests/concurrency.ts) checks it against a
real server; without the patch, its concurrent awaits fail as above.
