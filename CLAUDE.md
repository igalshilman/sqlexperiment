# Working on this repository

- No backwards compatibility: this is an experiment. Rename, move, delete and
  change APIs, storage formats and layouts freely; no shims, deprecations or
  migration paths.
- Optimize for simplicity, above all in the examples: as few lines as
  possible.
- Decide yourself; ask the owner only critical questions.
- Examples use real models (an API key); scripted models are for tests only
  (tests/fakes).
- Commit in small steps, and open frequent PRs so the owner can follow the
  changes; merge them yourself once the checks pass (no real users yet).
