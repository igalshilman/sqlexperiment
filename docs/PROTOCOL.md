# Commit, checkpoint and recovery

The backend offers whole-value gets and atomic, durable single-key puts and
deletes. A durable log turns one put into a multi-page commit; checkpoints then
keep pages at stable keys, as SQLite does with its WAL. One owner uses the
namespace at a time and settles every outstanding request before another opens.

## Why this shape

Without a log, the only way to commit many pages atomically is to write new
immutable objects and switch one pointer to them. That needs a page map, object
allocation, a root pointer and an intent record to reclaim aborted or replaced
objects. The previous format (v11) did exactly that.

With a log, the commit record carries the pages. Pages can then be overwritten in
place, because until the checkpoint record moves past a transaction, replay
applies that transaction's images over whatever is stored at `page/<n>`. Nothing
is allocated, so nothing can leak, and there is no intent or map.

## Commit

A save encodes its changed pages (after the private engine checkpoint) as one
transaction at `Log::next`, the sequence after the last committed transaction.

1. Put every segment but the last, concurrently.
2. Put the last segment, which carries the commit flag. Its acknowledgement
   commits the save.

A transaction of up to 1 MiB of images is a single put.

## Replay

Open reads `meta`, then reads `log/<start>`, `log/<start+1>`, … concurrently in
batches of `checkpoint_after + 2` (at most 16). It applies each transaction when it
reaches the commit segment, and stops at the first absent key or at the first
segment whose transaction start is not the expected one. Everything after that
point is debris from a failed save: a part of an uncommitted transaction, or a
part left behind when a shorter transaction later reused the sequence. The next
commit overwrites it from `Log::next`. Ignoring debris is safe because a commit
segment is only written after its transaction's other segments were acknowledged.

A page reads as its latest image in the replayed log, else `page/<n>`.

## Checkpoint

Each save may also checkpoint, in the same round trip as its commit:

- **When.** More than `checkpoint_after` transactions, or more than 256 KiB of
  segments, have been replayed.
- **What.** Whole transactions from the oldest, until about 12 pages are covered.
  Each page gets its *latest* committed image from the replayed log. Writing a
  newer image than the checkpoint boundary is harmless, because the log still
  replays over it. Holes become deletes. The session's own, not yet committed
  pages are never written in place.
- **Recording.** `meta.log_start` may move past a transaction only after all its
  pages are acknowledged. Without a cache the save writes `meta` in a second round
  trip. With a `PageCache`, the owner remembers the acknowledged position (a
  *claim*) and the next save writes `meta` in its first round trip, alongside its
  own commit.
- **Garbage.** Segments in `[deleted_below, log_start)` of the durable `meta` are
  deleted, and the next `meta` records the new `deleted_below`. Deleting segments
  that a durable `meta` has not passed could make replay start at a missing key
  and silently skip committed transactions.
- **Large saves.** A save whose segments exceed 256 KiB checkpoints itself and the
  rest of the log right after its commit is acknowledged, then writes `meta`. That
  takes extra round trips once, but later opens do not replay a bulk load.

Every step is idempotent. A crash at any point leaves state that replay reads
correctly; at worst it replays more of the log or leaves garbage segments for the
next checkpoint.

## Owner cache across checkpoints

A `PageCache` keeps a base: the log start of the last durable `meta` its owner
saw. A cached page is current unless a committed transaction at or after the base
changed it. While such a transaction is in the replayed log, its image overrides
the cache. Only a checkpoint can expose a stale value, by moving `meta` past it.
The owner's own checkpoints write through the cache.

When open reads a `meta` that this owner did not record (another writer
checkpointed, or this owner's save failed with an unknown outcome), it reads the
segments from the base to the new log start, concurrently with replay and often
from the speculative probe. The latest image of each page they change replaces a
cached value; every other cached page stays. Those segments are below the new log
start, so they are committed and never rewritten, only deleted. If any is absent,
if they do not chain into whole transactions, or if there are more than one probe
batch, the cache drops every page. Garbage collection deletes them in the save
after the one that recorded the checkpoint, so a brief other owner (a few saves
with default settings) leaves the cache warm; a longer one resets it.

A failed save leaves the pages alone: whatever it wrote in place belongs to
transactions after the base, which the next open refreshes if `meta` passed
them. It forgets only the claim, the acknowledged commit and the parked engine.

## Uncertain outcomes and confirmation

An error or cancellation can leave a put absent, visible but not durable, or
durable. If a failed commit is visible but not durable, a later commit must not
build on it, and no reader should observe it. Open therefore re-puts the commit
segment of the last transaction, and the `meta` it read, before exposing them.
It skips this when:

- the backend reports `durable_when_visible()`, as most replicated stores do; or
- the owner's `PageCache` shows that this process wrote that commit or that `meta`
  and had it acknowledged.

Every earlier commit was acknowledged, or confirmed this way, before its successor
was written. The earlier segments of a multi-segment transaction were
acknowledged before its commit segment was written.

| Interrupted step | State after a crash | Recovery |
|---|---|---|
| Non-commit segment | Debris; transaction uncommitted | Ignored by replay; overwritten by the next commit |
| Commit segment | Absent, or present (then committed) | Replay decides; open confirms a present tail |
| Checkpoint page put | Old or new committed image at `page/<n>` | Log still replays over it |
| `meta` put | Old or new record; both are valid | Open confirms what it reads |
| Garbage delete | Segment present or absent below a durable start | Deleted again by a later checkpoint |

## Costs

| Operation | Round trips | Mutations |
|---|---|---|
| Steady-state save (owner with cache) | 1 | 1 commit put, plus checkpoint pages, garbage deletes and a `meta` put when due |
| Save without cache | 1, or 2 when it checkpoints | Same, with `meta` in the second round trip |
| Large save | 2 or more | Segments, then all logged pages and `meta` |
| Open (owner with cache) | 1 | None |
| Open, fresh owner, weak backend | 2 to 3 | Re-put of the commit tail and `meta` |
| Cold page read | 1 get | None |

Measurements are in [LOG_RESULTS.md](../benches/LOG_RESULTS.md).

Use a dedicated namespace. Independently injected garbage is outside the protocol.
