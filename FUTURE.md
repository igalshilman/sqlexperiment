# Future work

Work that waits on something outside this repository. Each item names what
it waits on, and where the code marks it (`TODO:` comments).

## Unjournaled state reads (Restate service protocol v8)

**Waits on:** a Restate release with service protocol v8.

**Now:** the SQL store of `restate-sessions` reads the object's state with
v8's ephemeral get, left out of the journal, wherever the attempt negotiated
v8, and with a regular, journaled get on v7 (`objectStore` in
`packages/sessions/src/session.ts`; the version comes from the attempt's
content type, in `host` in `object.ts`). The SDK (1.18.1) speaks v5 to v8;
its `ephemeralStateGet` is private in its types, and the selector
(`packages/sessions/src/selector.ts`) drives its completions. Restate 1.8.0,
the release the tests run (`@restatedev/restate-server` in `packages/e2e`),
negotiates v7 at most, so its reads are journaled.

A handler without journal replay never reads its journal back, so on v7 each
read's entry is pure overhead: a database read of N pages adds N entries.
`packages/e2e/concurrency.ts` checks the journal on either protocol.

**Found out (19 October 2026):** v8 is only on Restate's
`service-protocol-v8` branch. There, `protocol_v8` is one of the
`experimental!` features in `crates/types/src/config/common.rs`, enabled with
`RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V8=true`, and `service_protocol.rs`
picks v8 only when it is set. Neither the `v1.8.0` tag nor Restate's `main`
has it, and the 1.8.0 binaries speak `invocation.v1` to `v7` only. The image
`ghcr.io/restatedev/restate:service-protocol-v8-stats` (restate-server
1.8.2-dev) has v8, behind that variable. To run the e2e tests against it,
with its `restate-server` binary (`/usr/local/bin/restate-server` in the
image):

```sh
RESTATE_SERVER=/path/to/v8/restate-server RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V8=true pnpm --filter e2e concurrency
```

**To switch:** once the release is out, update `@restatedev/restate-server`
in `packages/e2e` to it (with `RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V8=true`
where the tests start it, unless it enables v8 by default), and drop the v7
fallback: the journaled get in `objectStore`, and the version check in
`host`.

## Concurrent awaits in the TypeScript SDK

**Waits on:** an SDK release whose explicit recovery lets a handler
await concurrently.

**Now:** `restate-sessions` drives a SQL handler's promises with a selector of its
own (`packages/sessions/src/selector.ts`), which takes over the
invocation's `promisesExecutor.doProgress`: the SDK's internals, so the
package pins `@restatedev/restate-sdk` 1.18.1. The published SDK lets one
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
