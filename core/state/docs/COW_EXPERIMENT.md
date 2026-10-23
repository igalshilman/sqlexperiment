# Experiment: copy-on-write pages with zstd

Local experiment, not merged. `--features cow` swaps the log and its
checkpoints (`src/snapshot.rs`) for `src/cow.rs`, behind the same `State` API:

- A flush puts one immutable blob, `cow/blob/<n>`: the changed pages, and the
  records too large for the root, compressed together in one zstd frame. Then
  it puts the root, `cow/root`, one zstd frame mapping each page to its blob slot,
  and syncs once. Nothing is overwritten, so there is no log replay and no
  checkpoint.
- A blob below half live is copied forward into the next flush's blob
  (`COW_COMPACT`); dead blobs are deleted by the flush after. Blobs hold at
  most 64 slots (`COW_BLOB_PAGES`); a cold read fetches a whole blob.
- `INLINE_BYTES=0` (the root's record budget) puts the effects in the blob,
  compressed with the pages.

Correctness: the public-API campaign (`tests/campaign.rs`, 300 seeds: 11,789
flushes, 2,797 crashes, 974 cancellations, 1,531 handovers) reads every reopen
as the model. It, and a few tests, still fail on assertions about the log
(seals, hints, checkpoints).

## Results

`core/sql/benches/cow_compare.rs`, one flush per session, an owner cache
warmed with every page, 100–300 flushes. Bytes are keys and values put per
flush. Round trips: 2.0 per flush for both, warm; without a cache the log
takes 3.0–17.7, copy-on-write 3.0–11.5.

| Workload                         | Log B/flush | CoW B/flush | CoW, effects in blob, 16-page blobs | Stored log / CoW | Cold read KiB log / CoW / CoW-16 |
| -------------------------------- | ----------- | ----------- | ----------------------------------- | ---------------- | -------------------------------- |
| Chat row + effects (300)         | 1,870       | 1,922       | **1,743** (−7%)                     | 62 / 67 KiB      | 57 / 11 / 11                     |
| 100 rows, update 1               | **908**     | 2,459       | 2,459 (2.7×)                        | 11 / 11 KiB      | 7 / 9 / 9                        |
| 10k rows, update 1               | 7,853       | **4,816**   | 4,889 (−38%)                        | 1.3 / 1.7 MiB    | 93 / 271 / 75                    |
| 10k rows, update 10 (scattered)  | 63,826      | 56,071      | **54,503** (−15%)                   | 1.3 / 1.8 MiB    | 61 / 242 / 81                    |
| 10k rows, insert 1               | **1,676**   | 4,507       | 4,582 (2.7×)                        | 1.3 / 1.0 MiB    | 70 / 187 / 57                    |
| 100k rows, update 1 (100)        | 7,265       | **4,917**   | 5,510 (−24%)                        | 12.7 / 10.2 MiB  | 137 / 275 / 79                   |

- **Effects with the pages help a little.** In the chat workload the effects
  echo the row's text: one frame saves 9% against effects in the root, 7%
  against the log.
- **Small changes to the same pages: the log wins 2.7×.** It compresses each
  page against its previous image, so an insert or a small update costs about
  what changed; copy-on-write writes the touched pages whole (compressed
  together, ~800 B each). Compaction is not the cost: off, 2,452 B.
- **Scattered updates in a larger database: copy-on-write wins 15–38%.** The
  log writes each page twice (logged and sealed, then checkpointed), and every
  ~2 flushes checkpoints for update 10 (21 puts a flush against 2).
- **Storage**: batch compression stores a bulk load 20% smaller than
  per-page zstd, but compaction at half live lets updates grow it 1.4×
  (without compaction, update 10 reaches 6.8 MiB, writing 36 KB a flush).
- **Cold reads** fetch whole blobs: 64-page blobs read 2–4× the log's bytes,
  in 4 gets against 18–50; 16-page blobs about match the log's bytes.
- **The root grows with the database.** It holds the whole page map; fully
  fragmented by scattered updates it costs ~2.8 B a page on every flush: 1.8 KB
  at 700 pages, 19 KB at 7,000 (the 100k rows), 216 KB at 70,000. Large
  databases would need the map in chunks, stored in the blobs, at the cost of
  a round trip on a cold open.
- **Compaction may read before it writes**: a victim blob in neither the
  state nor the page cache is fetched first, a round trip more for that flush.
