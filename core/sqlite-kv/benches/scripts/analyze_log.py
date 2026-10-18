#!/usr/bin/env python3
"""Compare the commit record (v14) with the previous format's log (v13).

Reads benches/results/log-shipping.csv (v14) and log-shipping-v13.csv (v13, its
strong-backend rows), checks that repeats agree, writes log-shipping-summary.csv
and prints Markdown tables of critical-path round trips, mutations, bytes and
storage per database case.

With `--sweep FILE`, prints instead a table of one policy sweep (a
log_shipping run with `POLICIES` set): per session with a cache, averaged over
the four workloads, bytes by key kind.
"""
import csv
import statistics
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RESULTS = ROOT / "results"
RTT_MS = 5
COUNTERS = ["round_trips", "gets", "puts", "deletes", "bytes_read", "bytes_written",
            "log_segments", "stored_bytes"]


def label(version, row):
    """A checkpoint policy's label: each version's bindings cadence says so."""
    after = row["checkpoint_after"]
    size = int(row.get("checkpoint_log_bytes", 256 * 1024))
    inline = int(row.get("inline_log_bytes", size))
    sealed = f", sealed past {inline // 1024} KiB" if inline < size else ""
    bindings = (version, after, size, inline) in [
        ("v13", "60", 256 * 1024, 256 * 1024), ("v14", "32", 64 * 1024, 4 * 1024)]
    return (f"{version}: {after} tx or {size // 1024} KiB{sealed}"
            + (" (bindings)" if bindings else ""))


def configurations():
    """Yield (case, design label, cache, rows of one run, repeats)."""
    for name, version in [("log-shipping-v13.csv", "v13"), ("log-shipping.csv", "v14")]:
        runs = defaultdict(lambda: defaultdict(list))
        for row in csv.DictReader((RESULTS / name).open()):
            if row["backend"] != "strong":
                continue
            key = (row["rows"], row["payload_bytes"], row["indexes"], label(version, row), row["cache"])
            runs[key][row["run"]].append(row)
        for key, by_run in runs.items():
            copies = list(by_run.values())
            first = [[r[c] for c in COUNTERS] for r in copies[0]]
            if any([[r[c] for c in COUNTERS] for r in other] != first for other in copies[1:]):
                sys.exit(f"repeats disagree in {name} for {key}")
            yield key[:3], key[3], key[4], copies[0], len(copies)


def sessions(rows):
    out = defaultdict(dict)
    for row in rows:
        out[(int(row["iteration"]), row["workload"])][row["phase"]] = row
    return out


def total(phases, column):
    return sum(int(p[column]) for p in phases.values())


def summarize(rows):
    reads, writes, saves, opens = [], [], [], []
    write_bytes, write_mutations, read_gets, read_bytes = [], [], [], []
    for (_, workload), phases in sessions(rows).items():
        opens.append(int(phases["open"]["round_trips"]))
        read_bytes.append(total(phases, "bytes_read"))
        if workload == "read_pk":
            reads.append(total(phases, "round_trips"))
            read_gets.append(total(phases, "gets"))
        else:
            writes.append(total(phases, "round_trips"))
            saves.append(int(phases["save"]["round_trips"]))
            write_bytes.append(total(phases, "bytes_written"))
            write_mutations.append(total(phases, "puts") + total(phases, "deletes"))
    saves.sort()
    return {
        "open_rt": statistics.mean(opens),
        "read_rt": statistics.mean(reads),
        "write_rt": statistics.mean(writes),
        "save_rt": statistics.mean(saves),
        "save_p95": saves[int(0.95 * (len(saves) - 1))],
        "save_max": saves[-1],
        "write_mutations": statistics.mean(write_mutations),
        "write_bytes": statistics.mean(write_bytes),
        "read_gets": statistics.mean(read_gets),
        "read_bytes": statistics.mean(read_bytes),
        "stored_bytes": int(rows[-1]["stored_bytes"]),
    }


def sweep(path):
    totals, sessions_of = defaultdict(lambda: defaultdict(int)), defaultdict(set)
    columns = ["round_trips", "bytes_written", "commit_written", "page_written", "log_written",
               "bytes_read", "puts", "deletes"]
    for row in csv.DictReader(Path(path).open()):
        if row["run"] != "0" or row["cache"] != "true":
            continue
        key = tuple(int(row[c]) for c in
                    ["rows", "checkpoint_after", "checkpoint_log_bytes", "inline_log_bytes"])
        sessions_of[key].add((row["iteration"], row["workload"]))
        for c in columns:
            totals[key][c] += int(row[c])
    print("| DB rows | Checkpoint after | Sealed past | Round trips | Bytes written | Record "
          "| Pages | Stored values | Bytes read | Read + written | Puts + deletes |")
    print("|---|---|---|---|---|---|---|---|---|---|---|")
    for key in sorted(totals):
        rows, after, log_bytes, inline = key
        v = {c: n / len(sessions_of[key]) for c, n in totals[key].items()}
        sealed = f"{inline // 1024} KiB" if inline < log_bytes else "never"
        print(f"| {rows:,} | {after} tx or {log_bytes // 1024} KiB | {sealed} "
              f"| {v['round_trips']:.2f} | {v['bytes_written']:,.0f} | {v['commit_written']:,.0f} "
              f"| {v['page_written']:,.0f} | {v['log_written']:,.0f} | {v['bytes_read']:,.0f} "
              f"| {v['bytes_written'] + v['bytes_read']:,.0f} | {v['puts'] + v['deletes']:.2f} |")


def main():
    if sys.argv[1:2] == ["--sweep"]:
        return sweep(sys.argv[2])
    results, repeats = {}, set()
    for case, design, cache, rows, n in configurations():
        results[(case, design, cache)] = summarize(rows)
        repeats.add(n)
    with (RESULTS / "log-shipping-summary.csv").open("w", newline="") as f:
        writer = csv.writer(f)
        metrics = list(next(iter(results.values())))
        writer.writerow(["rows", "payload_bytes", "indexes", "design", "cache", *metrics])
        for (case, design, cache), r in sorted(results.items()):
            writer.writerow([*case, design, cache, *(round(v, 3) for v in r.values())])

    print(f"Generated by `benches/scripts/analyze_log.py`; every configuration has "
          f"{'/'.join(map(str, sorted(repeats)))} identical repeats and 128 sessions.\n")
    for case in sorted({k[0] for k in results}, key=lambda c: int(c[0])):
        rows, payload, indexes = case
        print(f"### {int(rows):,} rows, {payload}-byte payload, {indexes} secondary index\n")
        print("| Design | Cache | Open RT | Read session RT | Write session RT "
              f"| Save RT mean / p95 / max | Write session @ {RTT_MS} ms | Mutations / write "
              "| Bytes written / write | Bytes read / session | Gets / read | Stored bytes |")
        print("|---|---|---|---|---|---|---|---|---|---|---|---|")
        order = sorted((k for k in results if k[0] == case), key=lambda k: (k[2], k[1]))
        for key in order:
            _, design, cache = key
            r = results[key]
            print(f"| {design} | {'yes' if cache == 'true' else 'no'} "
                  f"| {r['open_rt']:.2f} | {r['read_rt']:.2f} | {r['write_rt']:.2f} "
                  f"| {r['save_rt']:.2f} / {r['save_p95']} / {r['save_max']} "
                  f"| {r['write_rt'] * RTT_MS:.0f} ms | {r['write_mutations']:.1f} "
                  f"| {r['write_bytes']:,.0f} | {r['read_bytes']:,.0f} | {r['read_gets']:.1f} "
                  f"| {r['stored_bytes']:,} |")
        print()


if __name__ == "__main__":
    main()
