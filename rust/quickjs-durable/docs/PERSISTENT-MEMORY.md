# Evaluation: keeping a key's memory between calls

An experiment, not merged. On `main`, each call of an exported function
starts from the module as evaluated (its golden image), and what lasts
between calls is in `ctx.db` and `ctx.kv`. On this branch, a key's memory
lives on instead: the module's variables, and everything they reach, are
the object's state.

```js
const messages = [];

export function say(text) {
  messages.push({ role: 'user', content: text });
  return messages.length;
}
```

## What changes

- A call starts on the image the key's last call left, and a settled call
  keeps it (on `main` it clears it).
- The bridge seeds `Math.random` once per key, and resets only the call's
  state at each call: its ID, request counter and pending promises.
- A trap (a call out of time) undoes its turn: memory back to the last
  commit, and the turn's SQL rolled back with it, so the two agree. On
  `main`, the SQL before a trap stands, and the image is dropped anyway.
- A call left waiting (cancelled, say) does not block the key: the next call
  runs on what it committed, and its pending requests are dropped.
- Three examples keep state in variables: `memory.js` (a class, a closure, a
  generator, across calls), `assistant.js` (the conversation) and `coder.js`
  (the task history). The other twelve still use `ctx.kv` and SQL.

All suites pass on the branch, in all three tracking modes and against a
Restate server, plus `tests/objects.rs`: memory across calls, a trap
leaving memory as it was, a call left waiting, which exports are handlers;
the crash sweep also checks the next call's memory after each recovery.

## What it costs

200 calls on one key, each a new attempt (the way each Restate invocation
is), over a store in memory; each message is ~2 KiB of text that does not
compress away. Per call: time, and bytes written to the store.

| Workload                     | Model          | Call 1          | Call 100        | Call 200        | Total, 200 calls |
| ---------------------------- | -------------- | --------------- | --------------- | --------------- | ---------------- |
| Counter in `ctx.kv`          | fresh (`main`) | 6.4 ms, 0.6 KiB | 2.0 ms, 0.4 KiB | 2.0 ms, 0.4 KiB | 178 KiB          |
|                              | memory kept    | 3.9 ms, 3.1 KiB | 2.1 ms, 2.3 KiB | 2.1 ms, 1.4 KiB | 480 KiB          |
| Chat, messages in SQL        | fresh          | 4.8 ms, 1.7 KiB | 3.3 ms, 4.4 KiB | 4.3 ms, 1.5 KiB | 590 KiB          |
|                              | memory kept    | 4.6 ms, 3.7 KiB | 2.9 ms, 5.9 KiB | 4.5 ms, 2.3 KiB | 886 KiB          |
| Chat, an array in a variable | memory kept    | 1.6 ms, 3.1 KiB | 4.2 ms, 2.4 KiB | 4.0 ms, 2.5 KiB | 751 KiB          |
| Chat, the array in `ctx.kv`  | either         | 2.7 ms, ~2 KiB  | 31 ms, ~235 KiB | 57 ms, ~450 KiB | ~44 MiB          |

- **A variable costs what SQL does.** The conversation in a variable wrote
  751 KiB over 200 calls, flat at ~2.5 KiB a call, against 590 KiB for the
  same messages in SQL on `main`: each call writes only the pages it
  changed. Both are linear. The array in `ctx.kv`, the natural way to write
  it without either, is quadratic: it rewrites the whole conversation every
  call, 44 MiB in all.
- **Calls that keep nothing pay for the image.** The KV counter wrote 2.7x
  as much (480 against 178 KiB): each call changes some pages of the heap
  (the bridge's state, the allocator, the garbage collector), and they are
  saved. On `main` a settled call writes only its record.
- **Restoring grows with the memory kept.** The variable chat's calls went
  from 1.6 ms to ~4 ms as its image grew to 157 changed pages (~630 KiB for
  ~400 KiB of messages). A fresh call restores nothing: it starts from
  golden.

## What it means for users

For:

- No state to load or save: the variables are the state, with closures,
  classes, maps and generators as they are. The examples get shorter (the
  assistant drops its KV round trip).
- The cost is in proportion to what a call changes, like SQL, without a
  schema.

Against:

- **A changed file strands every key.** Memory belongs to the source that
  made it: after a deploy, existing keys refuse their calls ("another
  version of program ..."). There is no way to carry a heap to new code; it
  would need versions kept side by side (old keys on old code) or an
  explicit export and import of state, which is what SQL already is.
- **Bugs are permanent.** A leak, or a corrupted structure, stays in the
  key's memory for good; on `main` the next call starts clean.
- **Nothing outside can read it.** SQL can be queried and migrated; a heap
  can only be asked through the object's own handlers.
- **Calls that keep nothing pay** ~2.7x the storage of a fresh call, and
  every call restores the image first.

## Recommendation

Keep `main`'s model, fresh calls with state in SQL and KV, as the default.
A key's memory is a good fit for agents and sessions (a conversation, a
loop's progress) that live for a while and are not upgraded mid-life. If it
comes back, it should be opt-in per program, together with version pinning
(old keys keep their source), so a deploy does not strand them.
