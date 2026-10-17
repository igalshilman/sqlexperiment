# The patched shared core

The Restate SDKs' shared core (the VM of the service protocol, in Rust), with
the changes that SQL in a virtual object needs, as a patch for the SDKs' builds
to apply to a fresh clone: the TypeScript SDK's
([`../sdk-typescript`](../sdk-typescript/README.md)) compiles it to
WebAssembly, and the Python SDK's ([`../sdk-python`](../sdk-python/README.md))
links it natively.

|                                                  | Version                                                                                                                                   |
| ------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------- |
| [`sdk-shared-core.patch`](sdk-shared-core.patch) | [`382649d`](https://github.com/restatedev/sdk-shared-core/commit/382649d0652aa6d971ec16e788d24b4b2c8c8b45), unreleased main (protocol V8) |

| Change                                                     | Why                                                                                                                                                                                        |
| ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `JournalReplay::Disabled`, a `VMOptions` field             | A retried attempt runs the handler again against the state as stored, instead of replaying a journal its reads would not match                                                             |
| State commands (get, set, clear) are named after their key | Tooling can hide them by name, such as a SQL database's pages and log (`dev.restate.tooling.journal.hidden.name.prefix`); a journal recorded with unnamed state commands no longer replays |

## Without journal replay

After the input, the VM skips the recorded commands; completion ids start at
`1 + 2 × known entries` (a command takes at most two, so ids of earlier
attempts are never reused); awakeables, whose signal ids cannot be made fresh,
are refused. The change is in `VMOptions`, `SysInput` and
`Journal::skip_replayed`.

The shared core's tests pass with the patch (280). An SDK's
`patches/regenerate.sh` writes the patch back from its edited clone.
