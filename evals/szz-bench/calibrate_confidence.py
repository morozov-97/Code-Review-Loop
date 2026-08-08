#!/usr/bin/env python3
"""
#163 attempt: is a finding's self-reported confidence correlated with whether it actually
identified the real defect's location?

Non-circular by construction: ground truth is `git blame` against the historical fix commit's
parent tree, checking whether the finding's cited file:line is *literally the same lines SZZ
already independently traced back to this diff's own bug-introducing commit* (extract.py's own
mechanism, reused here) -- not this tool's own judgment re-grading the finding.

Important scope limits, not overclaimed:
- This is a location-match proxy, not full semantic verification. A finding can cite the right
  lines for the wrong reason, or the wrong lines while still being directionally right.
- It measures the *Finding*'s own self-reported confidence (set by the reviewing lens), not the
  *discourse move* confidence that `discourse::votes::confidence_weight` actually weights AGREE/
  CHALLENGE moves by (issue #163's literal target) -- related, not identical.
- Only covers the positive set (each entry has a `fix_sha` to blame against) -- the negative set
  has no fix commit, so there's no location ground truth to check against there.
- Findings within one diff aren't independent of each other; treat sample sizes accordingly.

Usage: python3 calibrate_confidence.py --repo /path/to/target/repo --bench-dir ./szz-out
Reads --bench-dir/manifest.json and --bench-dir/results/<case>/state.json (produced by
run_benchmark.sh). Prints location-match rate per confidence bucket; does not modify anything.
"""
import argparse
import json
import re
import subprocess
from collections import defaultdict


def run(repo, args):
    return subprocess.run(["git"] + args, cwd=repo, capture_output=True, text=True).stdout


def parse_line_range(line_field):
    nums = [int(n) for n in re.findall(r"\d+", line_field)]
    return (min(nums), max(nums)) if nums else None


def blamed_commits(repo, fix_sha, path, start, end):
    out = run(repo, ["blame", "--porcelain", "-L", f"{start},{end}", f"{fix_sha}^", "--", path])
    if not out:
        return set()
    return {m.group(1) for line in out.split("\n") if (m := re.match(r"^([0-9a-f]{40}) ", line))}


def print_buckets(title, rows, only_status=None):
    buckets = defaultdict(lambda: {"verified": 0, "unverified": 0, "no_line": 0})
    for r in rows:
        if only_status is not None and r["status"] != only_status:
            continue
        b = buckets[r["confidence"]]
        if r["location_verified"] is None:
            b["no_line"] += 1
        elif r["location_verified"]:
            b["verified"] += 1
        else:
            b["unverified"] += 1
    print(f"\n=== {title} ===")
    for conf in ["high", "medium", "low", "unknown"]:
        b = buckets.get(conf)
        if not b:
            continue
        total = b["verified"] + b["unverified"]
        rate = round(b["verified"] / total, 3) if total else None
        print(
            f"  {conf:8s}: verified={b['verified']:3d} unverified={b['unverified']:3d} "
            f"no_line_number={b['no_line']:2d}  rate={rate}"
        )


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", required=True, help="path to the target git repo (read-only)")
    ap.add_argument("--bench-dir", required=True, help="output dir from run_benchmark.sh")
    args = ap.parse_args()

    manifest = json.load(open(f"{args.bench_dir}/manifest.json"))
    positives = {
        e["file"].replace(".patch", ""): e for e in manifest if e["label"] == "positive_bic"
    }

    rows = []
    for base, entry in positives.items():
        try:
            state = json.load(open(f"{args.bench_dir}/results/{base}/state.json"))
        except FileNotFoundError:
            continue
        resolved = state.get("resolved", {})
        for f in state.get("findings", []):
            status = resolved.get(f["id"], {}).get("status", "UNKNOWN")
            rng = parse_line_range(f["line"])
            location_verified = None
            if rng:
                start, end = rng
                commits = blamed_commits(args.repo, entry["fix_sha"], f["file"], start, end)
                location_verified = entry["sha"] in commits
            rows.append(
                {
                    "base": base,
                    "finding_id": f["id"],
                    "confidence": f.get("confidence", "unknown"),
                    "status": status,
                    "severity": f.get("severity"),
                    "location_verified": location_verified,
                }
            )

    json.dump(rows, open(f"{args.bench_dir}/confidence_calibration_rows.json", "w"), indent=2)
    print_buckets("Location-match rate by self-reported confidence (ALL findings)", rows)
    print_buckets(
        "Location-match rate by self-reported confidence (CONFIRMED only)",
        rows,
        only_status="CONFIRMED",
    )
    print(f"\ntotal findings examined: {len(rows)}")


if __name__ == "__main__":
    main()
