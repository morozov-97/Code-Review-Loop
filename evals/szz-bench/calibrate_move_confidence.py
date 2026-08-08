#!/usr/bin/env python3
"""
#163, second pass: does a *discourse move's own* self-reported confidence (what
discourse::votes::confidence_weight() actually weights AGREE/CHALLENGE by) predict whether the
finding it targets is really at the historical defect's location?

calibrate_confidence.py checked Finding.confidence (the reviewing lens's own self-report) — this
checks the discourse-move confidence instead, joined against the same non-circular git-blame
ground truth. Needs the Confidence column in report.md's Discourse Audit table (added alongside
this script) — a report generated before that change won't parse.

Only reports AGREE moves as the primary signal (the ones confidence_weight applies a positive
vote to). CHALLENGE(EXISTENCE) moves are reported separately: a "correct" existence-challenge
targets a finding that does NOT blame-match, the inverse of AGREE's "correct" direction — don't
average the two together.

Usage: python3 calibrate_move_confidence.py --repo /path/to/target/repo --bench-dir ./szz-out
Reads --bench-dir/manifest.json and --bench-dir/results/<case>/{report.md,state.json}.
"""
import argparse
import json
import os
import re
import subprocess
from collections import defaultdict


def git(repo, args):
    return subprocess.run(["git"] + args, cwd=repo, capture_output=True, text=True).stdout


def parse_line_range(line_field):
    nums = [int(n) for n in re.findall(r"\d+", line_field)]
    return (min(nums), max(nums)) if nums else None


def blamed_commits(repo, fix_sha, path, start, end):
    out = git(repo, ["blame", "--porcelain", "-L", f"{start},{end}", f"{fix_sha}^", "--", path])
    if not out:
        return set()
    return {m.group(1) for line in out.split("\n") if (m := re.match(r"^([0-9a-f]{40}) ", line))}


def parse_moves(report_path):
    text = open(report_path).read()
    m = re.search(r"## Discourse Audit\n\n(.*?)(\n##|\Z)", text, re.S)
    if not m:
        return []
    rows = [
        l for l in m.group(1).split("\n")
        if l.startswith("|") and not l.startswith("| Round") and not re.match(r"^\|[-\s|]+\|$", l)
    ]
    moves = []
    for r in rows:
        cells = [c.strip() for c in r.split("|")[1:-1]]
        if len(cells) < 6:
            continue
        round_, kind, confidence, axis, lens, target = cells[:6]
        moves.append({"round": round_, "kind": kind, "confidence": confidence, "axis": axis, "target": target})
    return moves


def print_buckets(title, rows, verified_key="verified", unverified_key="unverified"):
    buckets = defaultdict(lambda: {verified_key: 0, unverified_key: 0})
    for r in rows:
        b = buckets[r["confidence"]]
        b[verified_key if r["location_verified"] else unverified_key] += 1
    print(f"\n=== {title} ===")
    for conf in ["high", "medium", "low", "unknown", ""]:
        b = buckets.get(conf)
        if not b:
            continue
        total = b[verified_key] + b[unverified_key]
        rate = round(b[verified_key] / total, 3) if total else None
        print(f"  {conf or '(blank)':8s}: {verified_key}={b[verified_key]:3d} {unverified_key}={b[unverified_key]:3d} rate={rate} n={total}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", required=True)
    ap.add_argument("--bench-dir", required=True)
    args = ap.parse_args()

    manifest = json.load(open(f"{args.bench_dir}/manifest.json"))
    positives = {e["file"].replace(".patch", ""): e for e in manifest if e["label"] == "positive_bic"}

    agree_rows, challenge_rows = [], []
    for base, entry in positives.items():
        report_path = f"{args.bench_dir}/results/{base}/report.md"
        state_path = f"{args.bench_dir}/results/{base}/state.json"
        if not os.path.exists(report_path) or not os.path.exists(state_path):
            continue
        state = json.load(open(state_path))
        findings_by_id = {f["id"]: f for f in state.get("findings", [])}

        for mv in parse_moves(report_path):
            target = findings_by_id.get(mv["target"])
            if not target:
                continue
            rng = parse_line_range(target["line"])
            if not rng:
                continue
            start, end = rng
            commits = blamed_commits(args.repo, entry["fix_sha"], target["file"], start, end)
            row = {"base": base, "confidence": mv["confidence"], "location_verified": entry["sha"] in commits}
            if mv["kind"] == "AGREE":
                agree_rows.append(row)
            elif mv["kind"] == "CHALLENGE" and mv["axis"].upper() == "EXISTENCE":
                challenge_rows.append(row)

    json.dump({"agree": agree_rows, "challenge_existence": challenge_rows},
              open(f"{args.bench_dir}/move_confidence_rows.json", "w"), indent=2)

    print_buckets("AGREE moves: location-match rate of the TARGET finding, by move's own confidence", agree_rows)
    print(f"\ntotal AGREE moves examined: {len(agree_rows)}")

    print("\n(CHALLENGE(EXISTENCE): a 'correct' challenge targets a finding that does NOT blame-match -- inverse of AGREE)")
    print_buckets("CHALLENGE(EXISTENCE) moves, by move's own confidence", challenge_rows,
                  verified_key="target_verified", unverified_key="target_unverified")
    print(f"\ntotal CHALLENGE(EXISTENCE) moves examined: {len(challenge_rows)}")


if __name__ == "__main__":
    main()
