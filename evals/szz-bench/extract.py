#!/usr/bin/env python3
"""SZZ-style bug-introducing-commit extraction from a real git repo.

For each commit whose message matches `--fix-grep` (default: `^fix`), finds the commit(s) that
last touched the lines it changed (via `git blame` on its parent) -- that's the bug-introducing
commit (BIC). Standard/established technique (https://en.wikipedia.org/wiki/SZZ_algorithm), not
invented for this tool -- real ground truth derived from a project's own history, not fabricated
or hand-picked.

Also emits a same-era "negative" set: commits never identified as any fix's BIC, similar in size
to the positive set. Important caveat, worth restating at point of use, not just here: a commit
in the negative set means "no fix commit was later observed touching these lines" -- absence of
evidence, not proof of cleanliness. A defect that was never fixed, or fixed without a message
matching `--fix-grep`, would be mislabeled "clean."

Usage:
    python3 extract.py --repo /path/to/target/repo --out-dir ./out [--positive-limit 30] [--negative-limit 30]

Writes positive.json and negative.json to --out-dir: each a list of
{sha, subject, label}. Does not touch --repo (read-only).
"""
import argparse
import json
import re
import subprocess
import sys
from collections import Counter


def run(repo, args):
    return subprocess.run(
        ["git"] + args, cwd=repo, capture_output=True, text=True, check=True
    ).stdout


def fix_commits(repo, fix_grep):
    out = run(repo, ["log", "--format=%H|%ci|%s", "--grep", fix_grep, "-i", "--no-merges"])
    out = out.strip()
    if not out:
        return []
    commits = []
    for line in out.split("\n"):
        sha, date, subject = line.split("|", 2)
        commits.append((sha, date, subject))
    return commits


def changed_files(repo, sha):
    out = run(repo, ["show", "--name-only", "--format=", sha])
    return [f for f in out.strip().split("\n") if f]


def diff_stat(repo, sha):
    out = run(repo, ["show", "--shortstat", "--format=", sha])
    m = re.search(
        r"(\d+) files? changed(?:, (\d+) insertions?\(\+\))?(?:, (\d+) deletions?\(-\))?", out
    )
    if not m:
        return None
    return int(m.group(1)), int(m.group(2) or 0), int(m.group(3) or 0)


def old_side_hunks(repo, sha, path):
    out = run(repo, ["diff", "-U0", f"{sha}^", sha, "--", path])
    ranges = []
    for line in out.split("\n"):
        m = re.match(r"^@@ -(\d+)(?:,(\d+))? \+\d+(?:,\d+)? @@", line)
        if m:
            start, count = int(m.group(1)), int(m.group(2) or 1)
            if count > 0:
                ranges.append((start, count))
    return ranges


def blame_commits_for_range(repo, sha, path, start, count):
    try:
        out = run(
            repo,
            ["blame", "--porcelain", "-L", f"{start},{start + count - 1}", f"{sha}^", "--", path],
        )
    except subprocess.CalledProcessError:
        return []
    return [m.group(1) for line in out.split("\n") if (m := re.match(r"^([0-9a-f]{40}) ", line))]


def find_bic(repo, fix_sha):
    counter = Counter()
    for f in changed_files(repo, fix_sha):
        for start, count in old_side_hunks(repo, fix_sha, f):
            for c in blame_commits_for_range(repo, fix_sha, f, start, count):
                counter[c] += 1
    return counter.most_common(1)[0][0] if counter else None


def commit_subject(repo, sha):
    return run(repo, ["show", "-s", "--format=%s", sha]).strip()


def is_merge_or_outsized(repo, sha, max_files, max_lines):
    parents = run(repo, ["show", "-s", "--format=%P", sha]).strip().split()
    if len(parents) > 1:
        return True
    stat = diff_stat(repo, sha)
    if stat is None:
        return True
    files, ins, dele = stat
    return files > max_files or (ins + dele) > max_lines


def build_positive_set(repo, fix_grep, limit, max_files, max_lines):
    fixes = fix_commits(repo, fix_grep)
    print(f"total fix-grep commits found: {len(fixes)}", file=sys.stderr)
    results, seen = [], set()
    for sha, _date, subject in fixes:
        if is_merge_or_outsized(repo, sha, max_files, max_lines):
            continue
        bic = find_bic(repo, sha)
        if bic is None or bic in seen or is_merge_or_outsized(repo, bic, max_files, max_lines):
            continue
        seen.add(bic)
        results.append({"sha": bic, "subject": commit_subject(repo, bic), "label": "positive_bic",
                         "fix_sha": sha, "fix_subject": subject})
        print(f"[{len(results)}] fix={sha[:8]} <- bic={bic[:8]} {commit_subject(repo, bic)[:60]}", file=sys.stderr)
        if len(results) >= limit:
            break
    return results, {r["sha"] for r in results}


def build_negative_set(repo, fix_grep, bic_shas, limit, max_files, max_lines, min_lines):
    fix_shas = {sha for sha, _, _ in fix_commits(repo, fix_grep)}
    all_out = run(repo, ["log", "--format=%H|%ci|%s", "--no-merges"]).strip()
    candidates = []
    for line in all_out.split("\n"):
        sha, _date, subject = line.split("|", 2)
        if sha in bic_shas or sha in fix_shas:
            continue
        stat = diff_stat(repo, sha)
        if stat is None:
            continue
        files, ins, dele = stat
        total = ins + dele
        if files > max_files or total > max_lines or total < min_lines:
            continue
        if subject.lower().startswith("docs"):
            continue
        candidates.append({"sha": sha, "subject": subject, "label": "negative_clean"})
    if not candidates:
        return []
    step = max(1, len(candidates) // limit)
    return candidates[::step][:limit]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", required=True, help="path to the target git repo (read-only)")
    ap.add_argument("--out-dir", required=True)
    ap.add_argument("--fix-grep", default="^fix", help="grep pattern for bug-fix commit messages")
    ap.add_argument("--positive-limit", type=int, default=30)
    ap.add_argument("--negative-limit", type=int, default=30)
    ap.add_argument("--max-files", type=int, default=8, help="skip commits touching more files than this")
    ap.add_argument("--max-lines", type=int, default=400, help="skip commits with more than this many changed lines")
    ap.add_argument("--min-negative-lines", type=int, default=5, help="skip trivial negative-set candidates")
    args = ap.parse_args()

    import os
    os.makedirs(args.out_dir, exist_ok=True)

    positive, bic_shas = build_positive_set(
        args.repo, args.fix_grep, args.positive_limit, args.max_files, args.max_lines
    )
    negative = build_negative_set(
        args.repo, args.fix_grep, bic_shas, args.negative_limit, args.max_files, args.max_lines,
        args.min_negative_lines,
    )
    json.dump(positive, open(os.path.join(args.out_dir, "positive.json"), "w"), indent=2, ensure_ascii=False)
    json.dump(negative, open(os.path.join(args.out_dir, "negative.json"), "w"), indent=2, ensure_ascii=False)
    print(f"positive: {len(positive)}, negative: {len(negative)}", file=sys.stderr)


if __name__ == "__main__":
    main()
