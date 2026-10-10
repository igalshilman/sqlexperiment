# Recovery through one pending-save record

Introduced in crate 0.8.0; retained in crate 0.10.0 with format v8. This supersedes the retirement-only proposal
and the old full-store GC. Production code requires only get/put/delete.

## Invariant

At most one save is pending per namespace. Its durable intent exists before any
new page or map node is written. The intent remains until either aborted new
objects or committed retired objects have all been durably deleted. No historical
readers may retain the preceding snapshot. One owner includes outstanding requests:
a cancelled old request must never arrive during recovery or after a new save.

The map planner identifies replaced edges while rewriting touched paths. It retains
unchanged subtrees without traversing them. It removes unreachable temporary nodes
from the write plan and filters retirement candidates against references retained
by the final new nodes/head. The normal path does not enumerate all stored keys or
walk the full old map. Test-only reachability checks verify exact object accounting.

## Persistent record

`v8/pending_save` contains:

| Field | Encoding |
|---|---|
| Magic | 4 bytes, `TKP8` |
| Old head length, new head length | Two u32 LE lengths; zero old length means no old head |
| Created count, retired count | Two u32 LE counts |
| Old head, new head | Exact head byte strings |
| Created entries, then retired entries | Each entry: kind u8 (0 block, 1 node) and ID u64 LE |

Entries in each list are strictly sorted by kind and ID, without duplicates.
Created IDs exceed the old counter and do not exceed the new counter. Retired IDs
are nonzero and no larger than the old counter. The heads must differ; a truly
unchanged save is a no-op. Head structure and exact lengths are validated. Current
head bytes must match one of the recorded heads. No hashes, generation reservation,
page payload duplication, external intent object or additional commit flag is used.

For C new objects and R retired objects, intent size is
`20 + old_head_bytes + new_head_bytes + 9*(C+R)` bytes. The 1 MiB limit is checked
before mutations. Whole-record read/write and in-memory planning are intentional
prototype limits. Chunking is deferred because it introduces another lifecycle.
Validation detects malformed records, not all valid-looking storage corruption.

## Ordered save

1. Finish SQL work and checkpoint locally; stop the engine.
2. Construct the complete new-object and retirement plan.
3. Put intent and await durability.
4. Put new objects, awaiting every successful durable acknowledgment.
5. Put the new head and await durability: commit.
6. Delete retired objects, await every durable acknowledgment.
7. Delete intent and await durability.

Object puts and retired deletes each use bounded concurrency. The intent put, head
put, and final intent delete are ordered stages. Ordinary errors drain the active
phase; cancellation requires host settlement. A new session cannot start until open
has completed recovery. Same-key mutations from different attempts cannot overlap.

## Crash and recovery cases

| Interrupted stage | Durable/visible state | Recovery |
|---|---|---|
| Intent put, before acknowledgment | No object puts have begun | If intent exists, select old head and remove any listed new IDs; missing deletes are harmless |
| New-object writes | Intent exists, old head selected | Confirm old head, delete every created ID, then delete intent |
| Head put | Old or new head may be visible; durability uncertain | Confirm the selected head durably before either cleanup branch |
| Retirement | New head is durable; intent lists old objects | Confirm new head, retry all retired deletes, then delete intent |
| Intent deletion | Chosen cleanup branch has completed durably | If intent reappears after crash, repeating that branch is safe |

Confirming an absent old head uses durable `delete(head)`. A failed confirmation
performs no object deletion. Recovering a visible new head commits that choice:
all its new objects had acknowledged durability before its original head put began.
A head matching neither recorded value causes an error before any mutation.

Open checks the pending key before returning a usable session. A recovery failure
keeps the namespace unavailable to new SQL work until retry succeeds. Recovery
reads no database payloads and does not fetch map nodes: the ID lists are sufficient.

The record is not removed on a precommit error merely because publication failed:
new writes might already be durable. Similarly, an error deleting one old object
does not erase the intent, even when other deletes have succeeded.

`Ok(SaveReport)` means the head put was acknowledged. `cleanup_error: Some` means
committed with unfinished maintenance; SQL must not be repeated on that basis.
An error/cancellation before acknowledgment can have an unknown commit outcome.

## Costs

For C created objects and R retired objects, a normal nonempty save performs
**C+2 puts and R+1 deletes**: objects plus intent/head, retired objects plus intent.
There is no extra head rewrite to clear cleanup metadata. New stores with no old
objects still use an intent: a failed initial publication must also be recoverable.
A no-op save with an identical head and empty object lists performs no mutations.

Cold open adds one get for the pending key alongside the head get. Interrupted-save
recovery adds one head confirmation, C or R idempotent deletes, and intent deletion.
These are operation counts, not guaranteed network round trips. Concurrency may
need multiple waves. Logical durable deletion does not imply immediate physical
space return by the backend or its replication layer.

For HeadOverrides, shadowed base blocks remain until a fold removes structural
references; at most 64 pages are shadowed. Superseded overrides are reclaimed on
each save. Radix and InlineRoot retire replaced pages and nodes immediately.

## Why the engine WAL is separate

The private engine WAL supplies SQL transaction processing and rollback within a
session. It never reaches durable K/V. The pending-save record covers publication
of the resulting immutable objects, storing IDs and head metadata only. A durable
engine WAL/database VFS would be another valid design with different page-write
costs; this implementation keeps the single-copy immutable payload approach.

Old v6 objects and independently injected garbage are outside this v8 protocol.
Use a fresh namespace; no migration or generic repair collector is supplied.
