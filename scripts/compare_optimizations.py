#!/usr/bin/env python3
"""Compare validated baseline and optimized phase summaries; no dependencies."""
import csv
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DIMS = ["layout", "rows", "payload_bytes", "operation", "cache"]


def load(name):
    rows = list(csv.DictReader((ROOT / "results" / name).open()))
    result = {tuple(row[k] for k in DIMS): row for row in rows}
    assert len(result) == len(rows), "duplicate workload"
    return result


baseline = load("baseline-amplification_summary.csv")
optimized = load("amplification_summary.csv")
assert baseline.keys() == optimized.keys(), "different workloads"
metrics = ["gets", "puts", "deletes", "read_key_value_bytes", "mutation_key_value_bytes",
           "node_write_bytes", "write_value_bytes", "query_page_gets"]
output = []
for key in baseline:
    row = dict(zip(DIMS, key))
    for metric in metrics:
        before, after = int(baseline[key][metric]), int(optimized[key][metric])
        row.update({f"{metric}_before": before, f"{metric}_after": after,
                    f"{metric}_delta": after - before})
    output.append(row)
with (ROOT / "results" / "optimization-comparison.csv").open("w", newline="") as stream:
    writer = csv.DictWriter(stream, fieldnames=list(output[0]))
    writer.writeheader()
    writer.writerows(output)
for metric in metrics[:5]:
    regressions = [r for r in output if r[f"{metric}_delta"] > 0]
    print(f"{metric}: {len(regressions)} regressions across {len(output)} configurations")
    assert not regressions, (metric, regressions)
