#!/usr/bin/env python3
"""Aggregates a run_benchmark.sh output directory into TP/FP/TN/FN + precision/recall.

Classification signal is "did the report's ## Findings table contain at least one CONFIRMED
finding" (report.rs populates that table only with CONFIRMED-status findings), not the raw
`verdict` field -- verdict can saturate to REQUEST_CHANGES purely from an unrelated policy
failure (missing tests/changelog) regardless of actual code quality on some repos. Check your
own run's Policy Checks tables before trusting `verdict` as a signal; this script doesn't assume
either way.

Usage: python3 aggregate.py --dir ./szz-out
Reads ./szz-out/manifest.json and ./szz-out/results/<case>/report.md.
"""
import argparse
import json
import os
import re


def parse_report(path):
    if not os.path.exists(path):
        return None
    text = open(path).read()
    # verdict_reason (added alongside the verdict/policy decoupling fix) renders inline as
    # `**Verdict: X _(reason)_**` -- match that shape first and fall back to the older
    # `**Verdict: X**` (no reason) so this still works against a report from before that change.
    m = re.search(r"\*\*Verdict:\s*([A-Z_]+)\s*_\(([a-z0-9_]+)\)_\s*.*?\*\*\s*.*?Score:\s*(\d+)/100", text)
    if m:
        verdict, verdict_reason, score = m.group(1), m.group(2), int(m.group(3))
    else:
        m = re.search(r"\*\*Verdict:\s*([A-Z_]+)\*\*\s*.*?Score:\s*(\d+)/100", text)
        verdict = m.group(1) if m else "UNKNOWN"
        verdict_reason = None
        score = int(m.group(2)) if m else None

    fm = re.search(r"## Findings\n\n(.*?)(\n##|\n### Needs Human Review|\Z)", text, re.S)
    findings_section = fm.group(1) if fm else ""
    confirmed_rows = [
        l for l in findings_section.split("\n")
        if l.startswith("|") and not l.startswith("| ID") and not re.match(r"^\|[-\s|]+\|$", l)
    ]

    um = re.search(r"### Needs Human Review.*?\n\n(.*?)(\n##|\Z)", text, re.S)
    uncertain_section = um.group(1) if um else ""
    uncertain_rows = [
        l for l in uncertain_section.split("\n")
        if l.startswith("|") and not l.startswith("| ID") and not re.match(r"^\|[-\s|]+\|$", l)
    ]

    return {"verdict": verdict, "verdict_reason": verdict_reason, "score": score,
            "confirmed_count": len(confirmed_rows), "uncertain_count": len(uncertain_rows)}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dir", required=True, help="output dir passed as --out-dir to run_benchmark.sh")
    args = ap.parse_args()

    manifest = json.load(open(os.path.join(args.dir, "manifest.json")))
    rows = []
    for entry in manifest:
        base = entry["file"].replace(".patch", "")
        parsed = parse_report(os.path.join(args.dir, "results", base, "report.md"))
        rows.append({**entry, "base": base,
                     **(parsed or {"verdict": "MISSING", "verdict_reason": None, "score": None, "confirmed_count": 0, "uncertain_count": 0})})

    pos = [r for r in rows if r["label"] == "positive_bic"]
    neg = [r for r in rows if r["label"] == "negative_clean"]
    flagged = lambda r: r["confirmed_count"] > 0

    TP = sum(1 for r in pos if flagged(r))
    FN = sum(1 for r in pos if not flagged(r))
    FP = sum(1 for r in neg if flagged(r))
    TN = sum(1 for r in neg if not flagged(r))
    precision = TP / (TP + FP) if (TP + FP) else float("nan")
    recall = TP / (TP + FN) if (TP + FN) else float("nan")

    verdict_counts_pos, verdict_counts_neg = {}, {}
    for r in pos:
        verdict_counts_pos[r["verdict"]] = verdict_counts_pos.get(r["verdict"], 0) + 1
    for r in neg:
        verdict_counts_neg[r["verdict"]] = verdict_counts_neg.get(r["verdict"], 0) + 1

    summary = {
        "n_positive": len(pos), "n_negative": len(neg),
        "TP": TP, "FN": FN, "FP": FP, "TN": TN,
        "precision": round(precision, 3) if precision == precision else None,
        "recall": round(recall, 3) if recall == recall else None,
        "verdict_counts_positive_set": verdict_counts_pos,
        "verdict_counts_negative_set": verdict_counts_neg,
    }
    print(json.dumps(summary, indent=2))

    if len(set(r["verdict"] for r in rows)) == 1:
        print(
            "\nWARNING: every case got the same verdict -- verdict is likely saturated by a "
            "policy check unrelated to code quality (e.g. missing tests/changelog) on this repo. "
            "Check each report's Policy Checks table before trusting verdict as a signal; the "
            "confirmed-finding-based precision/recall above is the more reliable read.",
            )

    json.dump(rows, open(os.path.join(args.dir, "all_rows.json"), "w"), indent=2, ensure_ascii=False)
    json.dump(summary, open(os.path.join(args.dir, "summary.json"), "w"), indent=2)


if __name__ == "__main__":
    main()
