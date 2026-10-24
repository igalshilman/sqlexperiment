# Blobs in S3, the root in Restate

An experiment, on a branch: keep each object's root in Restate's state and its
blobs in an object store such as S3. Restate's state then holds one small key
per object, whatever the size of its database.

## How it works

[`blobs::SplitStore`](../src/blobs.rs) is a `KvStore` over two stores: the
root's (Restate's state) and a `BlobStore` (S3 through
[`object_store`](https://docs.rs/object_store), the feature `object-store`).
It queues writes in memory. Its sync puts the blobs in parallel, then the
root, then syncs the root's store. A root is stored only after every blob it
names, and that is the only order the state relies on.

**Zombies.** Restate fences an earlier attempt's writes to its state, but not
its writes to S3. Such an attempt (a zombie) read the same root as the new
owner, so it numbers its blobs the same way. Its PUTs would replace blobs the
owner's committed root names, silently. A test reproduces this: with
overwrites allowed, the owner's page reads back the zombie's bytes.

Two changes close it:

1. **Blobs are write-once.** `BlobStore::put_new` refuses a key that holds a
   value (S3's `If-None-Match: *`). A key holding the same bytes is this
   writer's own retried put; other bytes fail the flush.
2. **Each writer jumps.** A store that says its blobs are write-once
   (`KvStore::write_once_blobs`) makes each writer's first flush number its
   blobs from a random jump of up to 2^24 past the root's next blob. Two
   attempts rarely collide. When they do, the store refuses, the flush fails,
   and the next open jumps again. The jump also stops a reopen from reusing
   the numbers of its own failed flush, which the state did until now.

A flush whose outcome is unknown leaves blobs that no root names and that no
later flush overwrites. `blobs::orphans(root, blobs)` lists them, to delete
after a grace period.

## What the tests show

- `tests/split.rs` (run with `--features simulation,object-store` to also
  cover `object_store`'s `InMemory`):
  - only the root reaches the key/value store, and garbage is deleted from the
    blob store;
  - a zombie flushing after the owner, with the same jump, is refused and
    changes nothing;
  - a zombie flushing first fails the owner's flush, and the owner's next open
    jumps elsewhere and flushes;
  - a failed upload leaves the last root and one orphan, which the sweep
    finds;
  - a retried put of the same bytes succeeds.
- The campaign runs odd seeds on a write-once store
  (`MemoryKv::write_once`), which panics on a second put of a blob key: 300
  seeds of faults, crashes, cancellations and reopens pass. With the jump
  disabled, the first odd seed fails (`v16/blob/28 put twice`).
- The zombie tests and the write-once campaign each fail when their
  protection is removed.

## Not done

- **Latency and cost are not measured.** A flush is two round trips: S3,
  then Restate. Today's chunks of 4 pages are a few KiB compressed, and S3
  bills per request, so larger chunks may suit S3 better.
- **Not run against real S3.** `ObjectStoreBlobs` maps `PutMode::Create` and
  is tested on `object_store`'s `InMemory` only.
- **Nothing runs the sweep.** `orphans` is a function, not a schedule.
- **No host uses it.** The bindings and the functions runtime still open the
  state on Restate's state alone.
