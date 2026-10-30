# SQL in objects against a Restate server — 2026-10-22

[`tests/sql/bench.ts`](../../tests/sql/bench.ts)
calls SQL handlers of a virtual object (`restate-fns`) through a real Restate
server's ingress, and reads what the objects store from the server's state
table. Run it with `pnpm --filter tests bench`.

Rows hold what an agent stores: model replies, tool calls and tool output, as
generated JSON text of about 1.15 KB per row (zstd compresses it about 2.5×
per 4 KiB page, as it does such text). `PAYLOAD=random` stores random bytes
instead, the incompressible worst case. Results:
[`results/restate-text.json`](results/restate-text.json) and
[`results/restate-random.json`](results/restate-random.json).

**Setup.** One 4-vCPU cloud container: Restate 1.8.0 (one node, local disk),
the objects (Node.js 22) and the client, over loopback, so there is no network
latency. Latencies are end to end: the ingress call, the invocation, SQL and
the commit. The objects read state with journaled gets (service protocol v7;
see [FUTURE.md](../../../FUTURE.md)).

## Databases of 1, 10 and 100 MB

One database per object key, of 1 KiB rows, loaded 1,000 rows per call in one
transaction. Then 200 sequential updates and reads of random rows, 50 reads
from a shared handler, and five rounds of a restart of the objects' process
(an empty page cache) followed by a read and an update.

| Text rows                  | 1 MB   | 10 MB   | 100 MB  |
| -------------------------- | ------ | ------- | ------- |
| Logical data               | 1.1 MB | 11.5 MB | 115 MB  |
| Stored in Restate          | 0.5 MB | 5.3 MB  | 58.7 MB |
| Keys stored                | 345    | 3,350   | 33,424  |
| Load, per 1 MB call (p50)  | 330 ms | 334 ms  | 269 ms  |
| Update a row, warm (p50)   | 17 ms  | 18 ms   | 19 ms   |
| Read a row, warm (p50)     | 12 ms  | 12 ms   | 12 ms   |
| Read, shared handler (p50) | 25 ms  | 26 ms   | 36 ms   |
| Read, cold process¹        | 60 ms  | 57 ms   | 49 ms   |
| Update after a cold read¹  | 30 ms  | 29 ms   | 29 ms   |

¹ Median of five restarts.

| Random rows                | 1 MB   | 10 MB   | 100 MB   |
| -------------------------- | ------ | ------- | -------- |
| Logical data               | 1.0 MB | 10.2 MB | 102 MB   |
| Stored in Restate          | 1.1 MB | 10.6 MB | 113.8 MB |
| Load, per 1 MB call (p50)  | 202 ms | 208 ms  | 210 ms   |
| Update a row, warm (p50)   | 17 ms  | 16 ms   | 15 ms    |
| Read a row, warm (p50)     | 13 ms  | 12 ms   | 12 ms    |
| Read, shared handler (p50) | 26 ms  | 31 ms   | 40 ms    |
| Read, cold process¹        | 78 ms  | 57 ms   | 50 ms    |
| Update after a cold read¹  | 32 ms  | 29 ms   | 30 ms    |

- **Size does not change a call's latency.** A warm read or update costs the
  same on 1 MB as on 100 MB: the owner's page cache holds the pages, and a
  commit writes only the pages that changed, compressed against their
  previous images.
- **A cold process reads only what it needs.** The first read after a
  restart takes about 50–80 ms at every size: an open reads the commit
  record, then only the pages the query touches, never the whole database.
- **Text stores at half its size.** 115 MB of text rows took 58.7 MB in
  Restate; random rows took 1.11× their size (SQLite fits three 1 KiB rows in
  a 4 KiB page). Restate holds one key per 4 KiB page: 33,424 for 100 MB.
- **A large transaction leaves garbage until the next commit.** Right after
  a 1 MB load, the database's stored bytes were 1.6–2× what they settle to:
  the load's checkpoint wrote its pages after its log, and the next commit
  deletes the log.
- **Shared handlers read without a cache.** A reader keeps no page cache, so
  each page it touches is a get: a point read costs 25–40 ms, twice the
  owner's, and a scan pays for every page (below).

## Workloads

The other phases, with text rows (random rows are within 20%):

| Phase                                 | Calls | At once | p50    | p99    | Calls/s |
| ------------------------------------- | ----- | ------- | ------ | ------ | ------- |
| Insert a row, a new key per call      | 2,000 | 32      | 159 ms | 573 ms | 187     |
| Count in a transaction, one key       | 500   | 1       | 23 ms  | 47 ms  | 41      |
| Insert 2 KiB, 16 keys                 | 4,000 | 16      | 52 ms  | 100 ms | 298     |
| Insert 200 rows in a transaction      | 50    | 1       | 54 ms  | 117 ms | 18      |
| Scan ~0.5 MB, shared handler, 16 keys | 1,600 | 16      | 655 ms | 835 ms | 25      |
| The same scan, cold process           | 16    | 16      | 943 ms | 952 ms | 17      |
| Count in a transaction, cold process  | 1     | 1       | 37 ms  |        |         |

A full scan from a shared handler is the slow path: over a hundred pages,
each a journaled get. A reader with the owner's page cache, or v8's unjournaled
gets, would cut it.

## Limits

One machine, shared by the server, the objects and the client, over
loopback: a deployment adds the network's round trips to every get and
commit, and Restate's replication to every commit. Cold reads are five
samples each. The load generates its rows in JavaScript, one statement per
row, which accounts for much of its time.
