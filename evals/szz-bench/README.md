# SZZ-derived real benchmark tooling

Extracted from the real 41-diff run documented in [../README.md](../README.md#a-41-case-benchmark-derived-from-real-git-history-not-hand-picked-not-fabricated).
Generalized into reusable tooling here so scaling it up (more diffs, a different target repo, the
single-strong-reviewer comparison matrix issue #161 still asks for) doesn't mean re-deriving the
approach from scratch.

## What this does

Builds a ground-truth-labeled diff set from a real git repo's own history using
[SZZ](https://en.wikipedia.org/wiki/SZZ_algorithm) — a positive set (commits later fix commits
prove introduced a real defect, found via `git blame`) and a negative set (same-era commits never
identified as any fix's cause) — then runs the actual `codereview` binary against every diff and
reports precision/recall based on whether each review produced a `CONFIRMED` finding.

**Read the caveats in [../README.md](../README.md#a-41-case-benchmark-derived-from-real-git-history-not-hand-picked-not-fabricated)
before trusting the numbers this produces on a new repo** — in particular: the negative set is
"no fix commit was later observed," not "verified defect-free"; `verdict` can saturate to
`REQUEST_CHANGES` on every case if the target repo's commit style doesn't match your spec's
policy checks (this happened on the run above); and the precision/recall here measure "flagged
something in the diff," not "found the exact historically-fixed defect" — spot-check both
directions by hand before citing a number from this tool as settled.

## Usage

```bash
# 1. Extract ground-truth diffs from a real target repo (read-only against it)
python3 extract.py --repo /path/to/target/repo --out-dir ./szz-out \
  --positive-limit 30 --negative-limit 30

# 2. Run the actual codereview binary against all of them (real API calls — real cost/time)
cargo build --release   # from the repo root, if not already built
export OPENROUTER_API_KEY=...
./run_benchmark.sh --repo /path/to/target/repo --szz-dir ./szz-out --out-dir ./szz-out

# 3. Aggregate into precision/recall
python3 aggregate.py --dir ./szz-out

# 4. Optional (issue #163): does self-reported confidence predict finding location accuracy?
python3 calibrate_confidence.py --repo /path/to/target/repo --bench-dir ./szz-out

# 5. Optional (issue #163, the literal target this time): does a discourse *move's* confidence
#    predict whether the finding it AGREEs/CHALLENGEs with is really at the defect's location?
#    Needs report.md's Discourse Audit "Confidence" column (added alongside this script) --
#    reports generated before that change won't parse.
python3 calibrate_move_confidence.py --repo /path/to/target/repo --bench-dir ./szz-out
```

`calibrate_confidence.py` is a narrower, non-circular attempt at #163 — it checks each finding's
cited file:line against `git blame` on the historical fix's parent tree (does it land on the same
lines SZZ already traced back to the bug-introducing commit), bucketed by the finding's own
self-reported confidence. Read its own docstring before citing a number from it: it's a location-
match proxy (not full semantic verification), and it measures the *Finding*'s confidence field,
not the *discourse move* confidence `confidence_weight()` actually weights — related, not
identical.

`calibrate_move_confidence.py` measures the thing `calibrate_confidence.py` explicitly couldn't:
the discourse *move's* own confidence (AGREE/CHALLENGE), joined against the same blame-based
ground truth via the finding each move targets. Real result from the 41-case run: AGREE moves at
"medium" confidence had a *higher* location-match rate (0.929, n=14) than "high" confidence ones
(0.44, n=50) — the opposite of what `confidence_weight`'s 1.0-for-high/0.6-for-medium weighting
assumes. Small samples (especially medium's n=14), so treat as a real signal worth more data, not
a settled result — see the full writeup and caveats in the #163 comment this shipped alongside.

`extract.py --fix-grep` defaults to `^fix` (Conventional Commits style) — pass a different pattern
for repos using another convention. `--max-files`/`--max-lines` bound how large a commit can be
and still be used (large commits make blame-based attribution noisier, per SZZ's own known
limitations).

## What's not committed here

The actual diffs/reports/labels from the 41-case run aren't in this directory — they're real code
and real commit messages from an unrelated private repo, not something to publish. Only the
tooling that produced them is here; the anonymized results are in `../README.md`.
