# Tests

The tests of the TypeScript packages, on scripted models that need no API key
([`fakes/`](fakes/fakes.ts)), in place of the real ones: `vi.mock` in the unit
tests, and `node --import ./fakes/fakes.ts` for processes.

- [`agents/`](agents): an agent per integration, whose tools move money in a
  ledger, through crashes of the handler ([`testing.ts`](testing.ts)): every
  message moves the money once (`make test-ts`).
- [`storage/`](storage): each integration's storage on a database in memory,
  with the framework's own conformance suite where it has one.
- [`sql/`](sql): SQL in virtual objects: a ledger, a shop, and
  `ctx.transaction`'s sends.
- [`gateway.test.ts`](gateway.test.ts): the gateway example's parts.
- [`crash.ts`](crash.ts): all of them against a real Restate server, through
  crashes: the services abort at random, and the server is killed twice
  (`make crash-test`). [`serve.ts`](serve.ts) serves them,
  [`faults.ts`](faults.ts) injects the faults.
- [`concurrency.ts`](concurrency.ts): the context without journal replay,
  against a real Restate server (`make concurrency-test`).
