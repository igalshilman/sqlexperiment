#!/usr/bin/env python3
"""Validate paired traffic and summarize codec timing; Python standard library."""
import csv
import itertools
import statistics
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RESULTS = ROOT / "benches/results"
DIMS = ["rows", "payload_bytes", "payload", "codec", "operation"]


def read(name):
    with (RESULTS / name).open() as f:
        return list(csv.DictReader(f))


def write(name, rows):
    with (RESULTS / name).open("w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=list(rows[0]), lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)


rows = read("page-compression.csv")
groups = defaultdict(list)
for row in rows:
    assert None not in row and None not in row.values(), "malformed CSV"
    groups[tuple(row[k] for k in DIMS)].append(row)
expected = set(itertools.product(
    [("100", "64"), ("100", "512"), ("100", "9000"), ("10000", "256")],
    ["Repetitive", "Json", "Random"], ["Raw", "Zstd"],
    ["cold_pk", "range_10", "full_scan", "replace_payload", "ten_adjacent", "ten_scattered", "read_only_save"]))
assert set(groups) == {(*size, payload, codec, op) for size, payload, codec, op in expected}
summaries = {}
for key, copies in groups.items():
    assert {r["run"] for r in copies} == {"0", "1", "2"} and len(copies) == 3, key
    fixed = {k: v for k, v in copies[0].items() if k not in ["run", "elapsed_ns"]}
    assert all({k: v for k, v in r.items() if k not in ["run", "elapsed_ns"]} == fixed for r in copies), key
    row = dict(fixed)
    metrics = set(row) - set(DIMS)
    for field in metrics:
        row[field] = int(row[field])
    assert row["put_value_bytes"] == sum(row[f"{kind}_write_bytes"] for kind in ["page", "log", "commit"]), key
    if row["codec"] == "Raw":
        assert row["page_write_bytes"] == row["page_puts"] * 4099, key
        assert row["page_read_bytes"] == row["page_gets"] * 4099, key
    else:
        assert 3 * row["page_puts"] <= row["page_write_bytes"] <= 4099 * row["page_puts"], key
        assert 3 * row["page_gets"] <= row["page_read_bytes"] <= 4099 * row["page_gets"], key
    if row["app_write_bytes"] == 0:
        # Reads write nothing.
        assert row["deletes"] == row["page_puts"] == 0 and row["puts"] <= 2, key
    times = [int(r["elapsed_ns"]) / 1000 for r in copies]
    row.update(elapsed_us_min=min(times), elapsed_us_median=statistics.median(times), elapsed_us_max=max(times))
    summaries[key] = row
write("page-compression-summary.csv", list(summaries.values()))

comparisons = []
equal = ["gets", "puts", "deletes", "page_gets", "page_puts", "db_pages", "commit_write_bytes"]
traffic = ["read_key_value_bytes", "mutation_key_value_bytes", "page_write_bytes", "log_write_bytes", "stored_value_bytes"]
for key, raw in summaries.items():
    if raw["codec"] != "Raw":
        continue
    compressed = summaries[(*key[:3], "Zstd", key[4])]
    for metric in equal:
        assert raw[metric] == compressed[metric], (key, metric)
    for metric in traffic:
        assert compressed[metric] <= raw[metric], (key, metric)
    out = {k: raw[k] for k in DIMS if k != "codec"}
    for metric in traffic:
        out[f"{metric}_raw"] = raw[metric]
        out[f"{metric}_zstd"] = compressed[metric]
        out[f"{metric}_reduction_pct"] = round(100 * (1 - compressed[metric] / raw[metric]), 4) if raw[metric] else ""
    out["zstd_app_write_amplification"] = round(compressed["mutation_key_value_bytes"] / compressed["app_write_bytes"], 6) if compressed["app_write_bytes"] else ""
    comparisons.append(out)
write("page-compression-comparison.csv", comparisons)

cpu_groups = defaultdict(list)
for r in read("page-codec.csv"):
    cpu_groups[tuple(r[k] for k in ["rows", "payload_bytes", "payload", "codec"])].append(r)
assert set(cpu_groups) == {(a, b, p, c) for a, b in [("100", "64"), ("100", "512"), ("100", "9000"), ("10000", "256")] for p in ["Repetitive", "Json", "Random"] for c in ["Raw", "Lz4", "Zstd"]}
cpu = []
for key, copies in cpu_groups.items():
    assert {r["run"] for r in copies} == {"0", "1", "2"} and len(copies) == 3, key
    fields = ["sample_pages", "iterations", "encoded_bytes", "compressed_pages"]
    assert len({tuple(r[k] for k in fields) for r in copies}) == 1, key
    out = dict(zip(["rows", "payload_bytes", "payload", "codec"], key))
    out.update({k: int(copies[0][k]) for k in fields})
    for op in ["encode", "decode"]:
        samples = [int(r[f"{op}_ns"]) / (int(r["sample_pages"]) * int(r["iterations"])) for r in copies]
        out.update({f"{op}_ns_per_page_min": min(samples), f"{op}_ns_per_page_median": statistics.median(samples), f"{op}_ns_per_page_max": max(samples)})
    cpu.append(out)
write("page-codec-summary.csv", cpu)
print(f"Validated {len(rows)} SQL observations, {len(summaries)} configurations, {len(comparisons)} raw/zstd pairs; all three traffic repeats match.")
print(f"Validated {len(cpu_groups) * 3} CPU samples; identical page selections and sizes across repeats.")
print("Raw/zstd pairs have identical operation counts, commit record bytes and logical page counts; no encoded-traffic regressions.")
for row in comparisons:
    if row["operation"] == "replace_payload" and (row["rows"], row["payload_bytes"]) in [("100", "64"), ("10000", "256")]:
        print({k: row[k] for k in ["rows", "payload_bytes", "payload", "mutation_key_value_bytes_raw", "mutation_key_value_bytes_zstd", "mutation_key_value_bytes_reduction_pct"]})
