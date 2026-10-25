# Blobs in S3, the root in Restate

An experiment, on a branch: keep each object's root in Restate's state and its
blobs in an object store such as S3. Restate's state then holds two small keys
per object (the root and the claim), whatever the size of its database.

## How it works

[`blobs::SplitStore`](../src/blobs.rs) is a `KvStore` over two stores: the
root's (Restate's state) and a `BlobStore` (S3 through
[`object_store`](https://docs.rs/object_store), the feature `object-store`).
It queues writes in memory. Its sync uploads the blobs in parallel, then puts
the root (or the claim), then syncs the root's store. A root is stored only
after every blob it names, and that is the only order the state relies on.

**Zombies.** Restate fences an earlier attempt's writes to its state, but not
its uploads to S3. Such an attempt (a zombie) read the same root as the new
owner, so it would number its blobs the same way, and its uploads would
replace blobs the owner's committed root names, silently. An upload may also
land late: S3 may store a request whose response was lost, or one still in
flight when its attempt gave up, after a retry already runs.

**Claims.** So the owner claims its blob numbers in Restate before it uploads
any. At its first flush that puts a blob, it writes `v17/claim`, 4096 numbers
past its next blob, beside the root, and syncs; a sync that fails ends the
state, and with it the attempt. Every later attempt opens at the claim and
numbers past it. Restate fences the claim like the root, so a zombie that
claims after it was fenced fails before it uploads, and one that claimed
before uploads only at numbers no other attempt uses and no committed root
names. No two attempts ever put the same key.

**Garbage.** Numbers only grow, so a blob below the root's next number that
the root does not name is dead for good: a crashed attempt's uploads, a
zombie's, and late ones are dead once the root moves past them, as every
retry's first flush does. A sweep lists the blob store's keys, marks the dead
against a durable root in Restate, and deletes those the sweep before marked,
`garbage_after` versions earlier; it keeps its record (`v17/sweep`) in the
blob store. The owner sweeps every `sweep_every` flushes; a worker that reads
Restate's root may sweep beside it.

Two other designs came before. Write-once blobs (S3's `If-None-Match: *`)
with a random jump per writer made collisions rare, not impossible, and a
collision failed the flush. Content-addressed blobs, tried on
`claude/focused-cori-nw68l4`, need no claim; numbered blobs with claims were
chosen instead.

## What the tests show

- `tests/split.rs` (with `--features simulation,object-store`, over
  `object_store`'s `InMemory` too): only the root and the claim reach Restate;
  an attempt claims once, before its first blob; a retry numbers past what the
  failed attempt claimed; a zombie that claims after it was fenced puts
  nothing, and one that claimed before never touches the owner's blobs; a
  sweep deletes what they leave.
- `tests/split_campaign.rs` runs `SplitStore` over a blob store that fails as
  S3 does ([`MemoryBlobs::faults`](../src/simulation/blobs.rs)): of a flush's
  uploads some land, some fail before storing, some store and lose the
  response, some land after the attempt failed, while Restate syncs fail
  before or after storing, new attempts fence old ones, zombies flush and
  sweep, and owners and workers sweep with faults of their own. Every step
  checks that no blob key is put twice; every open reads the acknowledged
  state or the one a failed flush made; once everything in flight landed, a
  few sweeps leave only what the root names. 400 seeds pass. Skipping the
  claim fails seed 0 (blob keys put twice), and so does putting a root before
  its blobs (a blob gone).
- The other suites run unfenced too: the crash test fails a flush at the
  claim's sync and its own, and crashes at every write; the campaigns' seeds
  on `MemoryKv::unfenced`, which panics on a blob key put twice, include
  failpoints at the claim, the sweep's record and the listing.

## Not done

- **Latency and cost are not measured.** A flush is two round trips: S3,
  then Restate; an attempt's first flush adds one, the claim's sync. A sweep
  lists the blobs (one S3 request per 1000 keys) every 16 flushes. Today's
  chunks of 4 pages are a few KiB compressed, and S3 bills per request, so
  larger chunks may suit S3 better.
- **Not run against real S3.** `ObjectStoreBlobs` is tested on
  `object_store`'s `InMemory` only.
- **A zombie in the owner's process costs a retry.** It shares the owner's
  cache, and its flush makes the owner's next flush refuse ("another state
  flushed"): nothing is written, and Restate retries the attempt.
- **No host uses it.** The bindings and the functions runtime still open the
  state on Restate's state alone.
