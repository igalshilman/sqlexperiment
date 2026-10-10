#!/usr/bin/env python3
"""Validate and aggregate the benchmark; Python standard library only."""
import csv
import argparse
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
parser = argparse.ArgumentParser()
parser.add_argument("--input", default="benches/results/amplification.csv")
parser.add_argument("--summary", default="benches/results/amplification_summary.csv")
parser.add_argument("--report", default="benches/AMPLIFICATION_RESULTS.md")
args = parser.parse_args()
rows = list(csv.DictReader((ROOT / args.input).open()))
DIMENSIONS = ["layout", "rows", "payload_bytes", "operation", "cache", "run", "phase", "kind"]
for row in rows:
    for key in row.keys() - set(DIMENSIONS):
        row[key] = int(row[key])

repeated = defaultdict(list)
for row in rows:
    repeated[tuple(row[k] for k in DIMENSIONS if k != "run")].append(row)
for key, copies in repeated.items():
    expected = {k: v for k, v in copies[0].items() if k != "run"}
    assert all({k: v for k, v in r.items() if k != "run"} == expected for r in copies), key
    assert len({r["run"] for r in copies}) == len(copies), key
repeats = {len(copies) for copies in repeated.values()}
assert len(repeats) == 1, repeats

TRAFFIC = ["gets", "misses", "unique_read_keys", "puts", "unique_write_keys", "deletes",
           "unique_delete_keys", "read_value_bytes", "write_value_bytes", "reclaimed_value_bytes",
           "get_key_bytes", "put_key_bytes", "delete_key_bytes"]
phases = defaultdict(dict)
for row in rows:
    phases[tuple(row[k] for k in DIMENSIONS[:-1])][row["kind"]] = row
for key, objects in phases.items():
    assert set(objects) == {"all", "block", "node", "head", "intent", "other"}, key
    for field in TRAFFIC:
        assert objects["all"][field] == sum(r[field] for k, r in objects.items() if k != "all"), (key, field)
    block = objects["block"]
    assert 3 * block["puts"] <= block["write_value_bytes"] <= 4099 * block["puts"], key

groups = defaultdict(list)
for row in rows:
    if row["run"] == "0":
        groups[tuple(row[k] for k in DIMENSIONS[:5])].append(row)

summaries = []
def ratio(n, d):
    return round(n / d, 6) if d else ""

for key, group in groups.items():
    total = [r for r in group if r["kind"] == "all"]
    blocks = [r for r in group if r["kind"] == "block"]
    query_blocks = [r for r in blocks if r["phase"] == "query"]
    summary = dict(zip(DIMENSIONS[:5], key))
    # Unique-key counts are meaningful per phase; never sum them across phases.
    summary.update({f: sum(r[f] for r in total) for f in TRAFFIC if not f.startswith("unique_")})
    summary["page_gets"] = sum(r["gets"] for r in blocks)
    summary["query_page_gets"] = sum(r["gets"] for r in query_blocks)
    summary["query_unique_pages"] = sum(r["unique_read_keys"] for r in query_blocks)
    summary["page_puts"] = sum(r["puts"] for r in blocks)
    summary["map_gets"] = sum(r["gets"] for r in group if r["kind"] == "node")
    summary["open_gets"] = sum(r["gets"] for r in total if r["phase"] == "open")
    for kind in ["block", "node", "head", "intent"]:
        summary[f"{kind}_write_bytes"] = sum(r["write_value_bytes"] for r in group if r["kind"] == kind)
    for field in ["logical_read_bytes", "logical_write_bytes", "logical_delete_bytes"]:
        summary[field] = max(r[field] for r in total)
    for field in ["db_pages", "stored_value_bytes", "stored_key_bytes"]:
        summary[field] = total[-1][field]
    for field in ["local_db_write_bytes", "local_wal_write_bytes"]:
        summary[field] = sum(r[field] for r in total)
    summary["read_key_value_bytes"] = summary["read_value_bytes"] + summary["get_key_bytes"]
    summary["mutation_key_value_bytes"] = summary["write_value_bytes"] + summary["put_key_bytes"] + summary["delete_key_bytes"]
    summary["read_amplification"] = ratio(summary["read_key_value_bytes"], summary["logical_read_bytes"])
    summary["write_amplification"] = ratio(summary["mutation_key_value_bytes"], summary["logical_write_bytes"])
    summary["page_write_amplification"] = ratio(summary["write_value_bytes"], summary["page_puts"] * 4096)
    summary["storage_vs_db"] = ratio(summary["stored_value_bytes"], summary["db_pages"] * 4096)
    live_rows = int(summary["rows"]) + (summary["operation"] == "insert") - (summary["operation"] == "delete")
    summary["storage_vs_app"] = ratio(summary["stored_value_bytes"] + summary["stored_key_bytes"], live_rows * (16 + int(summary["payload_bytes"])))
    summaries.append(summary)

with (ROOT / args.summary).open("w", newline="") as out:
    writer = csv.DictWriter(out, fieldnames=list(summaries[0]), lineterminator="\n")
    writer.writeheader()
    writer.writerows(summaries)

def fmt(value):
    if value == "":
        return "—"
    if isinstance(value, float):
        return f"{value:,.2f}×"
    if isinstance(value, int):
        return f"{value:,}"
    return value

def table(data, fields):
    result = ["| " + " | ".join(title for _, title in fields) + " |",
              "|" + "|".join("---" for _ in fields) + "|"]
    result += ["| " + " | ".join(fmt(row[key]) for key, _ in fields) + " |" for row in data]
    return "\n".join(result)

report = ["# Amplification results — 2026-10-10", "",
          "Current storage format with adaptive LZ4 page envelopes.", "",
          f"{len(rows):,} raw class/phase rows; {len(phases):,} measured phases; "
          f"{len(summaries):,} workload/cache configurations, {next(iter(repeats))} repeats each. "
          "Every counter agrees across repeats. Generated by `benches/scripts/analyze_amplification.py`.", "",
          "See [README.md](README.md) for workload definitions and denominators. "
          "Cold totals include open. Warm totals include only the repeated query. "
          "Bytes count logical keys and values, without transport framing or physical replication.", ""]

open_shapes = {(r["gets"], r["unique_read_keys"]) for r in rows if r["phase"] == "open" and r["kind"] == "block"}
report += ["## Cold-open baseline", "",
           "Observed (block gets, distinct blocks) during open: " + ", ".join(map(str, sorted(open_shapes))) + ".", ""]

for count, payload in [(100, 64), (10000, 256), (100, 9000)]:
    report += [f"## Radix reads: {count:,} rows, {payload:,}-byte payload", ""]
    selected = [s for s in summaries if s["layout"] == "Radix" and s["rows"] == str(count)
                and s["payload_bytes"] == str(payload) and s["cache"] == "cold" and s["operation"] in
                ["pk_hit", "pk_miss", "range_10", "covering_index", "secondary_rows", "unindexed_filter", "full_scan"]]
    report += [table(selected, [("operation", "Operation"), ("db_pages", "DB pages"), ("open_gets", "Open gets"),
                ("query_page_gets", "Query page gets"), ("query_unique_pages", "Distinct query pages"),
                ("gets", "Total gets"), ("read_key_value_bytes", "Read bytes + keys"), ("read_amplification", "Read amp")]), ""]

report += ["## Radix writes: 100 rows, 64-byte payload", ""]
selected = [s for s in summaries if s["layout"] == "Radix" and s["rows"] == "100" and s["payload_bytes"] == "64"
            and s["operation"] in ["read_only_save", "rollback", "replace_payload", "update_index", "insert", "delete", "ten_adjacent", "ten_scattered"]]
report += [table(selected, [("operation", "Operation"), ("gets", "Gets"), ("page_puts", "Pages put"),
            ("puts", "Puts"), ("deletes", "Deletes"), ("write_value_bytes", "Put value bytes"),
            ("mutation_key_value_bytes", "Mutation bytes + keys"), ("write_amplification", "App write amp"),
            ("page_write_amplification", "Page→KV amp")]), ""]

report += ["## Locality and map layout: 10,000 rows, 256-byte payload", ""]
selected = [s for s in summaries if s["rows"] == "10000" and s["operation"] in ["replace_payload", "ten_adjacent", "ten_scattered"]]
report += [table(selected, [("layout", "Layout"), ("operation", "Operation"), ("page_puts", "Pages put"),
            ("block_write_bytes", "Encoded page bytes"), ("node_write_bytes", "Map bytes"), ("head_write_bytes", "Head bytes"),
            ("intent_write_bytes", "Intent bytes"), ("write_amplification", "App write amp"),
            ("page_write_amplification", "Page→KV amp"), ("storage_vs_db", "Stored values/DB")]), ""]

report += ["## Recovery cost: Radix, 100 rows, 64-byte payload", ""]
selected = [s for s in summaries if s["layout"] == "Radix" and s["rows"] == "100" and s["payload_bytes"] == "64"
            and s["operation"] in ["abort_before_head", "head_ack_lost", "cleanup_error", "intent_delete_error"]]
report += [table(selected, [("operation", "Interruption"), ("gets", "Gets"), ("puts", "Puts"), ("deletes", "Deletes"),
            ("read_key_value_bytes", "Read bytes + keys"), ("mutation_key_value_bytes", "Mutation bytes + keys"),
            ("reclaimed_value_bytes", "Reclaimed value bytes")]), ""]
warm = [s for s in summaries if s["cache"] == "warm"]
report += [f"Same-session warm queries: {sum(s['gets'] == 0 for s in warm)}/{len(warm)} configurations issued zero gets. "
           "The raw files retain all layouts, wide rows, phase splits, local WAL writes, key lengths, "
           "reclaimed bytes and retained storage.", ""]
(ROOT / args.report).write_text("\n".join(report))
print(f"Validated {len(rows)} rows, {len(phases)} phases, {len(summaries)} configurations; repeats={repeats}")
