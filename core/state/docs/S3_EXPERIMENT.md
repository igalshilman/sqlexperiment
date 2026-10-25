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

## What it costs

Measured with `S3=1 cargo bench --features simulation --bench flush_costs`
(in `core/sql`): SQL workloads of 300 flushes (200 for 10k rows and up, 100
past 100k), a session per flush as the hosts make, blobs unfenced (so each
session claims first), a sweep every 16 versions; S3 at US East prices
(Standard: $0.005 per 1,000 PUTs and LISTs, $0.023 per GB-month; Express One
Zone: $0.00113 per 1,000 PUTs, $0.0032 per GB uploaded, $0.11 per GB-month;
deletes are free).

| Workload                   | S3 PUTs per flush | Mean PUT | Upload per flush | LIST pages per flush | Standard, per million flushes | Express, per million flushes |
| -------------------------- | ----------------- | -------- | ---------------- | -------------------- | ----------------------------- | ---------------------------- |
| A chat message and effects | 1.2               | 1,000 B  | 1.1 KB           | 0.06                 | $6.1                          | $1.4                         |
| 100 rows, update 1         | 1.5               | 520 B    | 0.8 KB           | 0.06                 | $7.9                          | $1.8                         |
| 10k rows, update 1         | 3.0               | 130 B    | 0.4 KB           | 0.06                 | $15                           | $3.5                         |
| 10k rows, update 10        | 21.7              | 120 B    | 2.7 KB           | 0.11                 | $109                          | $25                          |
| 100k rows, update 1        | 3.0               | 85 B     | 0.2 KB           | 0.12                 | $16                           | $3.5                         |
| 200k rows, update 10       | 28.9              | 64 B     | 1.5 KB           | 0.31                 | $146                          | $33                          |

- **Requests are the whole bill.** Storage is not: the 200k-row database
  (22 MB in S3) costs $0.0005 a month in Standard. A flush puts a blob per
  changed chunk (a delta) and per changed map node, and past a small
  database nearly every one is under 512 bytes (64 to 130 on average): S3
  bills the same for a 100-byte PUT as for a 5 GB one. An object that
  flushes once a second, a single-row update each, costs about $39 a month
  in PUTs on Standard, $9 on Express.
- **Sweeps grow with the database.** A sweep lists every key, a LIST page
  per 1,000: under 0.3 pages a flush here, but a database of a million keys
  would cost about 60 pages a flush at a sweep every 16 versions (some $300
  per million flushes on Standard). Sweeps should follow the garbage made,
  not the versions.
- **Claims cost latency, not money.** Every session claims at its first
  flush: one more Restate round trip (3.2 a flush, cached, where Restate
  alone takes 2.2).
- **Cold reads are cheap but slow.** A cold open of the 200k-row database
  gets 59 blobs ($0.00002), in a few rounds of S3's first-byte latency.

What would change the bill:

1. **A pack per flush.** Put a flush's blobs as one object, and name a blob
   by its pack and range: one PUT a flush whatever it changed (Standard about
   $5 per million flushes; the 10-row updates 22 to 29 times less). A pack
   is deleted only once all its blobs are dead, so long-lived blobs pin old
   packs: a sweep must compact them, rewriting the live blobs.
2. **Small blobs in Restate, large ones in S3.** Restate is not billed per
   request: keep the deltas and nodes (under a few KiB) there, fenced, and
   put only bases in S3. Steady-state flushes then make next to no S3
   requests; S3 holds the bulk of the bytes. A blob's reference must say
   where it is (a bit of its number, say), and a delta that grows past the
   threshold moves with its next rebase.
3. **Both, as a log-structured tier.** Flushes write to Restate only; now
   and then, a batch of their blobs moves to S3 as one pack, and Restate
   keeps only what is recent: about one PUT per batch.
4. **S3 Express One Zone**: 4.4 times cheaper per PUT, 4.8 times dearer to
   store, one zone.

## Not done

- **Latency is not measured** against real S3: a flush is two round trips,
  S3 then Restate, and a session's first adds the claim's sync.
- **Not run against real S3.** `ObjectStoreBlobs` is tested on
  `object_store`'s `InMemory` only.
- **A zombie in the owner's process costs a retry.** It shares the owner's
  cache, and its flush makes the owner's next flush refuse ("another state
  flushed"): nothing is written, and Restate retries the attempt.
- **No host uses it.** The bindings and the functions runtime still open the
  state on Restate's state alone.
