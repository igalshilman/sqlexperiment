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
entries. The TypeScript and Python SDKs have it in their internals, unused:

| SDK        | Read now (TODO)                                          | Ephemeral read, in place                                                                                                                      |
| ---------- | -------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| TypeScript | `objectStore` in `restate-fns`'s `session.ts`: `ctx.get` | The SDK's `ephemeralStateGet` is private (only `getProject` uses it): the SDK needs to expose it, and the selector drives it by completion id |
| Python     | `sql_get` in `restate-fns`'s `object.py`                 | Not exposed: the SDK (1.1.0) has no ephemeral get, though its shared core 8.1.0 does; the SDK needs one, and the selector would drive it      |
| Rust       | `RawState::get` in `context/mod.rs`                      | `ContextInternal::get_ephemeral`, `EphemeralGetFuture`                                                                                        |

The TypeScript and Python reads are in `restate-fns` (`typescript/packages/fns`,
`python/fns`), the Rust one in its SDK's patch (`rust/sdk/patches`). The
TypeScript and Rust SDKs' discovery already advertises v8; a v7 server picks
v7.

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

## Concurrent awaits in the TypeScript and Python SDKs

**Waits on:** a release of each SDK whose explicit recovery lets a handler
await concurrently.

**Now:** `restate-fns` drives a SQL handler's promises with a selector of its
own (`typescript/packages/fns/src/selector.ts`), which takes over the
invocation's `promisesExecutor.doProgress`: the SDK's internals, so the
package pins `@restatedev/restate-sdk` 1.17.4. The published SDK lets one
awaited promise wait for the server at a time
(`ExternalProgressChannel.awaitNext`), so concurrent awaits that need the
server fail with `awaitNext already pending`.

**Then:** drop the selector, and the pin. `tests/concurrency.ts` tells
whether a release has it (with the hook off, its concurrent awaits fail).

The same holds in Python: `restate-fns`'s selector
(`python/fns/restate_fns/selector.py`) takes over the context's
`create_poll_or_cancel_coroutine` and its futures, so `python/fns` pins
`restate-sdk` 1.1.0. The SDK drives each awaited future with a loop of its
own, which concurrent waits race on; `python/tests/test_context.py` tells
(with the hook off, 2 of its 13 tests fail on 1.1.0).

## The OpenAI Agents SDK's latest, beside a trusted LiteLLM

**Waits on:** a stable LiteLLM release that takes `openai` 3, and is
trusted again: LiteLLM 1.107.0 on PyPI was a credential stealer with a
backdoor ([SafeDep's report](https://safedep.io/litellm-pypi-compromise-credential-stealer-backdoor/)).

**Now:** `python/agents/pyproject.toml` pins `litellm==1.106.2` (the newest
stable release, clean per the report; the lock has its hashes) and
`openai-agents` below 0.21 (0.20), since 0.21 and later need `openai` 3,
which LiteLLM 1.106.2 does not take.

**Then:** check the new LiteLLM release (its source and its wheels' files:
no `.pth` or `sitecustomize.py`), pin it exactly, and lift the
`openai-agents` pin (its `TODO:`).
