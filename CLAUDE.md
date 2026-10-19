# Working on this repository

- No backwards compatibility: this is an experiment. Rename, move, delete and
  change APIs, storage formats and layouts freely; no shims, deprecations or
  migration paths.
- Optimize for readability and developer experience, above all in what users
  see (examples, APIs, docs): clear names, a doc comment on how to run it,
  idiomatic code; short, but never cramped.
- Decide yourself; ask the owner only critical questions.
- Examples use real models (an API key); scripted models are for tests only
  (tests/fakes).
- Commit in small steps, and open frequent PRs so the owner can follow the
  changes; merge them yourself once the checks pass (no real users yet).
- [FUTURE.md](FUTURE.md) lists work waiting on something outside this
  repository (such as a Restate release), with its `TODO:` comments: check
  it when that changes.
