# Future work

Work that waits on something outside this repository. Each item names what
it waits on, and where the code marks it (`TODO:` comments).

## Unjournaled state reads (Restate service protocol v8)

**Waits on:** a Restate release with service protocol v8.

**Now:** the SQL stores of the TypeScript, Python and Rust SDKs read the
object's state with regular, journaled gets. Restate 1.8.0, the release the
Makefile downloads, negotiates protocol v7 at most.

**Then:** read with v8's ephemeral get, which leaves the read out of the
journal. A handler without journal replay never reads its journal back, so
each read's entry is pure overhead: a database read of N pages adds N
entries. The code is already in the patched SDKs, unused:

| SDK        | Read now (TODO)                                          | Ephemeral read, in place                                                                                                                      |
| ---------- | -------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| TypeScript | `objectStore` in `restate-sql`'s `session.ts`: `ctx.get` | The SDK's `ephemeralStateGet` is private (only `getProject` uses it): the SDK needs to expose it, and the selector drives it by completion id |
| Python     | `sql_get` in `server_context.py`                         | `_sql_get_ephemeral`: `vm.ephemeral_state_get`, the selector's `wait_ephemeral`                                                               |
| Rust       | `RawState::get` in `context/mod.rs`                      | `ContextInternal::get_ephemeral`, `EphemeralGetFuture`                                                                                        |

The Python and Rust files are in their SDK's patch (`python/sdk/patches`,
`rust/sdk/patches`); the TypeScript one is `typescript/packages/sql`. The SDKs' discovery already
advertises v8; a v7 server picks v7.

**Found out (19 October 2026):** v8 is only on Restate's
`service-protocol-v8` branch. There, `protocol_v8` is one of the
`experimental!` features in `crates/types/src/config/common.rs`, enabled with
`RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V8=true`, and `service_protocol.rs`
picks v8 only when it is set. Neither the `v1.8.0` tag (its experimental
features have no `protocol_v8`, and `ServiceProtocolVersion` ends at `V7`)
nor Restate's `main` has it, and the 1.8.0 binaries (release and image) speak
`invocation.v1` to `v7` only.

**To switch:** once a release has v8,

1. set `RESTATE_VERSION` to it in the Makefile, and export
   `RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V8=true` there;
2. set the variable on the server `rust/functions/tests/restate.rs`
   starts, and in the READMEs' commands that start Restate;
3. switch the three reads above to the ephemeral ones, and regenerate the
   patches (`patches/regenerate.sh` in each SDK).

## Concurrent awaits in the TypeScript SDK

**Waits on:** a release of the TypeScript SDK whose explicit recovery lets a
handler await promises concurrently.

**Now:** the SDK is patched (`typescript/packages/sdk-typescript/patches`):
with explicit recovery, one loop, `PromiseSelector`, drives every awaited
promise. The published 1.17.4 lets one awaited promise wait for the server at
a time (`ExternalProgressChannel.awaitNext`), so concurrent awaits that need
the server fail with `awaitNext already pending`, and `restate-sql`'s reads
are concurrent.

**Then:** drop the patch, and the SDK build with it: the workspace takes
`@restatedev/restate-sdk` from npm. `tests/concurrency.ts` tells whether a
release has it.
