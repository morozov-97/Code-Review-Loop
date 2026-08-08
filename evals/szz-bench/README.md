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
```

`extract.py --fix-grep` defaults to `^fix` (Conventional Commits style) — pass a different pattern
for repos using another convention. `--max-files`/`--max-lines` bound how large a commit can be
and still be used (large commits make blame-based attribution noisier, per SZZ's own known
limitations).

## What's not committed here

The actual diffs/reports/labels from the 41-case run aren't in this directory — they're real code
and real commit messages from an unrelated private repo, not something to publish. Only the
tooling that produced them is here; the anonymized results are in `../README.md`.
