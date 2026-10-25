# Blobs in S3, the root in Restate

An experiment, on a branch: keep each object's root in Restate's state and its
blobs in an object store such as S3. Restate's state then holds the root and
the sweep, whatever the size of its database.

## How it works

[`blobs::SplitStore`](../src/blobs.rs) is a `KvStore` over two stores: the
root's (Restate's state) and a `BlobStore` (S3 through
[`object_store`](https://docs.rs/object_store), the feature `object-store`).
It queues writes in memory. Its sync puts the blobs in parallel, then the
root (or the sweep), then syncs the root's store. A root is stored only after
every blob it names, and that is the only order the state relies on.

**Zombies.** Restate fences an earlier attempt's writes to its state, but not
its writes to S3. Such an attempt (a zombie) read the same root as the new
owner. With numbered blobs, it would number its blobs the same way, and its
PUTs would replace blobs the owner's committed root names, silently.

**Content-addressed blobs close it.** A blob's key is the hash of its frame,
of what the frame is compressed against, and of its database's identity: what
a key holds never changes. A zombie's put, or a retried one, writes either the
same bytes a root names, or a blob no root names. No conditional puts, claims
or numbering are needed, and equal blobs are stored once.

**Garbage.** A flush that fails midway, or a zombie, leaves blobs no root
names. A sweep (`State::collect_garbage`) lists the blob store, marks what the
root names, and deletes what the previous sweep also found unnamed, at least
`garbage_after` versions before. A later flush may name a hash again, so a
flush that puts a blob the last sweep found unnamed puts the sweep without it;
and a sweep puts the root again, at the next version, so a state that opened
before it cannot flush after it.

## What the tests show

- `tests/split.rs` (with `--features simulation,object-store`, over
  `object_store`'s `InMemory` too): only the root and the sweep reach Restate,
  and the blobs go to the blob store; a zombie flushing after the owner
  changes nothing a root names; a flush failing midway leaves the last root,
  and blobs that sweeps delete.
- `tests/sweep.rs`: a blob a flush names again between two sweeps is kept; a
  store that cannot list is never swept; a reader does not sweep; equal blobs
  are stored once.

## Not done

- **Latency and cost are not measured.** A flush is two round trips: S3,
  then Restate. Today's chunks of 4 pages are a few KiB compressed, and S3
  bills per request, so larger chunks may suit S3 better.
- **Not run against real S3.** `ObjectStoreBlobs` is tested on
  `object_store`'s `InMemory` only.
- **No host uses it.** The bindings and the functions runtime still open the
  state on Restate's state alone.
- **The alternative,** numbered blobs with claims in Restate, is on
  `claude/blob-claims`.
