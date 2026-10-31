# Future work

Work that waits on something outside this repository. Each item names what
it waits on, and where the code marks it (`TODO:` comments).

## Unjournaled state reads (Restate service protocol v8)

**Waits on:** a Restate release with service protocol v8, and a TypeScript
SDK release that exposes its ephemeral get.

**Now:** the SQL store of `restate-sessions` reads the object's state with
regular, journaled gets (`objectStore` in `packages/sessions/src/session.ts`:
`ctx.get`). Restate 1.8.0, the release the tests run
(`@restatedev/restate-server` in `packages/e2e`), negotiates protocol v7
at most.

**Then:** read with v8's ephemeral get, which leaves the read out of the
journal. A handler without journal replay never reads its journal back, so
each read's entry is pure overhead: a database read of N pages adds N
entries. The SDK has it in its internals, unused: its `ephemeralStateGet` is
private (only `getProject` uses it), so the SDK needs to expose it, and the
selector would drive it by completion id.

**Found out (19 October 2026):** v8 is only on Restate's
`service-protocol-v8` branch. There, `protocol_v8` is one of the
`experimental!` features in `crates/types/src/config/common.rs`, enabled with
`RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V8=true`, and `service_protocol.rs`
picks v8 only when it is set. Neither the `v1.8.0` tag (its experimental
features have no `protocol_v8`, and `ServiceProtocolVersion` ends at `V7`)
nor Restate's `main` has it, and the 1.8.0 binaries (release and image) speak
`invocation.v1` to `v7` only. The image
`ghcr.io/restatedev/restate:service-protocol-v8-stats` (restate-server
1.8.2-dev) has v8, behind that variable, for local benchmarks.

**To switch:** once both releases are out, update `@restatedev/restate-server`
in `packages/e2e` to the server's (with
`RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V8=true` where the tests start it,
unless it enables v8 by default), and switch the read above to the
ephemeral one.

## The head with the invocation (eager state keys)

**Waits on:** a TypeScript SDK release whose service options set
`eagerStateKeysWhitelist` (1.17.4 has no such option). Restate has it since
1.8.0 ([restatedev/restate#5261](https://github.com/restatedev/restate/pull/5261)).

**Now:** session objects set lazy state, so an invocation does not start with
every tail entry and pack, and a session's first read is the head
(`v22/head`): a round trip, cached or not.

**Then:** with lazy state still on, whitelist `v22/head` (`HEAD_KEY`, in
`packages/sessions/native/src/state/format.rs`); the server ships it with the
invocation (best effort, within its eager-state size limit), and the open
starts with it. Tail entries and packs
change names with each flush and checkpoint, so only the head can be
listed. The options are `sqlOptions` in `restate-sessions`
(`packages/sessions/src/object.ts`), which every session object has; its
`TODO:` marks them.

## Concurrent awaits in the TypeScript SDK

**Waits on:** an SDK release whose explicit recovery lets a handler
await concurrently.

**Now:** `restate-sessions` drives a SQL handler's promises with a selector of its
own (`packages/sessions/src/selector.ts`), which takes over the
invocation's `promisesExecutor.doProgress`: the SDK's internals, so the
package pins `@restatedev/restate-sdk` 1.17.4. The published SDK lets one
awaited promise wait for the server at a time
(`ExternalProgressChannel.awaitNext`), so concurrent awaits that need the
server fail with `awaitNext already pending`.

**Then:** drop the selector, and the pin, once `packages/e2e/concurrency.ts`
passes without it (on today's SDK, its concurrent awaits fail).

## The SQL engine's huge pages, in the addon

**Waits on:** a turso release whose buffer arena can do without
`MADV_HUGEPAGE`, or takes its size (now 3 MiB, 1 MiB under `TESTING=true`).

**Now:** `turso_core` maps a 3 MiB arena for each database it opens and
asks for transparent huge pages: where the kernel grants them on request
(`madvise`, the usual default), a cold open's first read waits for a
2 MiB page to be zeroed, up to 15 ms on a virtual machine. The Node.js addon
runs in its users' processes, which it leaves alone.

**Then:** open the engine without huge pages in the addon's `sql` module (`open_engine`, `packages/sessions/native/src/sql`).
