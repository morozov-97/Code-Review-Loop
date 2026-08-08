#!/usr/bin/env python3
"""
#209: aggregates a run_self_consistency.sh output directory into recall/precision under three
aggregation rules -- any-of-N flagged it (most lenient), majority, and all-N agreed (strictest)
-- for comparison against the single-pass (N=1) and full-pipeline numbers aggregate.py already
produces.

"Flagged" uses the same per-diff signal as aggregate.py: does the report's ## Findings table
contain at least one CONFIRMED finding (report.rs only populates that table with CONFIRMED-status
findings). Each of the N independent passes is judged separately, then combined by the threshold
rule -- this deliberately does NOT try to match individual findings across passes (each pass's
findings have no shared identity across independent runs; matching them would need semantic
comparison this script doesn't attempt).

Usage: python3 aggregate_self_consistency.py --dir ./szz-out
Reads ./szz-out/manifest.json and ./szz-out/self-consistency/<case>/rep-<N>/report.md.
"""
import argparse
import glob
import json
import os
import re


def parse_confirmed_count(report_path):
    if not os.path.exists(report_path):
        return None
    text = open(report_path).read()
    fm = re.search(r"## Findings\n\n(.*?)(\n##|\n### Needs Human Review|\Z)", text, re.S)
    findings_section = fm.group(1) if fm else ""
    confirmed_rows = [
        l for l in findings_section.split("\n")
        if l.startswith("|") and not l.startswith("| ID") and not re.match(r"^\|[-\s|]+\|$", l)
    ]
    return len(confirmed_rows)


def load_usage(rep_dir):
    p = os.path.join(rep_dir, "manifest.json")
    if not os.path.exists(p):
        return None, None
    m = json.load(open(p))
    return m["usage"]["calls"], m["usage"]["cost_usd"]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dir", required=True, help="output dir passed as --out-dir to run_self_consistency.sh")
    args = ap.parse_args()

    manifest = json.load(open(os.path.join(args.dir, "manifest.json")))
    sc_dir = os.path.join(args.dir, "self-consistency")

    rows = []
    for entry in manifest:
        base = entry["file"].replace(".patch", "")
        case_dir = os.path.join(sc_dir, base)
        rep_dirs = sorted(glob.glob(os.path.join(case_dir, "rep-*")))
        if not rep_dirs:
            continue
        flagged_count = 0
        total_calls = 0
        total_cost = 0.0
        for rep_dir in rep_dirs:
            cc = parse_confirmed_count(os.path.join(rep_dir, "report.md"))
            if cc and cc > 0:
                flagged_count += 1
            calls, cost = load_usage(rep_dir)
            total_calls += calls or 0
            total_cost += cost or 0.0
        rows.append({
            "case": base, "label": entry["label"], "n_reps": len(rep_dirs),
            "flagged_count": flagged_count, "total_calls": total_calls, "total_cost": total_cost,
        })

    pos = [r for r in rows if r["label"] == "positive_bic"]
    neg = [r for r in rows if r["label"] == "negative_clean"]

    print(f"n_positive={len(pos)} n_negative={len(neg)}")

    def flagged_at(row, mode):
        n = row["n_reps"]
        if mode == "any":
            thresh = 1
        elif mode == "majority":
            thresh = n // 2 + 1
        elif mode == "all":
            thresh = n
        return row["flagged_count"] >= thresh

    for mode, label in [("any", "any-of-N flagged (union, most lenient)"),
                         ("majority", "majority agreed"),
                         ("all", "all-N agreed (strictest)")]:
        tp = sum(1 for r in pos if flagged_at(r, mode))
        fn = len(pos) - tp
        fp = sum(1 for r in neg if flagged_at(r, mode))
        tn = len(neg) - fp
        recall = tp / (tp + fn) if (tp + fn) else None
        precision = tp / (tp + fp) if (tp + fp) else None
        print(f"  {label:32s} recall={recall} precision={precision} TP={tp} FP={fp}")

    if rows:
        avg_calls = sum(r["total_calls"] for r in rows) / len(rows)
        avg_cost = sum(r["total_cost"] for r in rows) / len(rows)
        print(f"avg calls per diff (all reps combined): {avg_calls:.1f}, avg cost: ${avg_cost:.4f}")

    json.dump(rows, open(os.path.join(args.dir, "self_consistency_rows.json"), "w"), indent=2)


if __name__ == "__main__":
    main()
