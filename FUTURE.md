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
entries. The code is already in place, unused:

| SDK        | Read now (TODO)                                          | Ephemeral read, in place                                                                                                                      |
| ---------- | -------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| TypeScript | `objectStore` in `restate-sql`'s `session.ts`: `ctx.get` | The SDK's `ephemeralStateGet` is private (only `getProject` uses it): the SDK needs to expose it, and the selector drives it by completion id |
| Python     | `sql_get` in `restate-sql`'s `object.py`                 | `_get_ephemeral`: the patched SDK's `vm.ephemeral_state_get`, the selector's `wait_ephemeral`                                                 |
| Rust       | `RawState::get` in `context/mod.rs`                      | `ContextInternal::get_ephemeral`, `EphemeralGetFuture`                                                                                        |

The TypeScript and Python reads are in `restate-sql` (`typescript/packages/sql`,
`python/sql`), the Rust one in its SDK's patch (`rust/sdk/patches`); the
Python SDK's patch has its ephemeral get (`python/sdk/patches`). The SDKs'
discovery already advertises v8; a v7 server picks v7.

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
   Rust patch (`patches/regenerate.sh`).

## Explicit recovery in the Python SDK

**Waits on:** a release of the Python SDK with explicit recovery, as the
TypeScript SDK 1.17.4 has (`restate.internal.explicitRecovery()`). Release
1.0.5 is on shared core 7.0.3, without it.

**Now:** `restate-sql` (`python/sql`) runs on the Python SDK 1.0.5 with a
patch (`python/sdk/patches`): shared core 8.1.0 and its
`JournalReplay::Disabled`, a `journal_replay` option of services and
handlers, state commands named after their key, and protocol v8's ephemeral
get.

**Then:** drop `python/sdk`, depend on the release in `python/sql`'s
`pyproject.toml`, and switch `restate_sql.VirtualObject` to the release's
option, if it is named differently.

## Concurrent awaits in the TypeScript and Python SDKs

**Waits on:** a release of each SDK whose explicit recovery lets a handler
await concurrently.

**Now:** `restate-sql` drives a SQL handler's promises with a selector of its
own (`typescript/packages/sql/src/selector.ts`), which takes over the
invocation's `promisesExecutor.doProgress`: the SDK's internals, so the
package pins `@restatedev/restate-sdk` 1.17.4. The published SDK lets one
awaited promise wait for the server at a time
(`ExternalProgressChannel.awaitNext`), so concurrent awaits that need the
server fail with `awaitNext already pending`.

**Then:** drop the selector, and the pin. `tests/concurrency.ts` tells
whether a release has it (with the hook off, its concurrent awaits fail).

The same holds in Python: `restate-sql`'s selector
(`python/sql/restate_sql/selector.py`) takes over the context's
`create_poll_or_cancel_coroutine` and its futures, so `python/sql` pins
`restate-sdk` 1.0.5. The SDK drives each awaited future with a loop of its
own, which concurrent waits race on; `python/tests/test_context.py` tells
(with the hook off, 4 of its 13 tests fail).
