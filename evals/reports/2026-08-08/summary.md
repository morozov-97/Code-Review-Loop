# Real-benchmark report — 2026-08-08

Snapshot of one day's empirical work on `codereview`, using real `--backend openrouter` calls
against a real, unrelated private production codebase (not part of this repo — app name, file
paths, and any in-app text are deliberately omitted). `evals/README.md` is the living narrative
this was built up in; this file is a fixed point-in-time summary so the numbers below don't shift
under later edits to that file. Cross-references to the relevant sections/PRs are included instead
of duplicating their full detail.

## Cost

Every number is from an actual `manifest.json` (`usage.calls`/`usage.cost_usd`), not estimated.

| Run | Reviews | LLM calls | Real cost |
|---|---|---|---|
| 41-case benchmark, full pipeline | 41 | 243 | $0.1446 |
| 41-case benchmark, single-lens baseline (comparison) | 41 | 73 | $0.0480 |
| 24-case rerun (discourse-move-confidence data) | 24 | 139 | $0.0774 |
| 78-case scale-up, full pipeline | 78 | 451 | $0.3022 |
| 78-case scale-up, single-lens baseline (comparison) | 78 | 136 | $0.1158 |
| 41-case self-consistency (3 independent passes each) | 123 | 222 | $0.1589 |
| **Total** | **385** | **1264** | **$0.8469** |

## Methodology

Ground truth came from [SZZ](https://en.wikipedia.org/wiki/SZZ_algorithm) — real
bug-introducing commits traced via `git blame` from a real project's own `fix:` commit history,
not hand-picked or self-labeled. Tooling: `evals/szz-bench/` (`extract.py`, `run_benchmark.sh`,
`aggregate.py`, `calibrate_confidence.py`, `calibrate_move_confidence.py`), all committed and
reusable against any git repo. Two runs, not independent of each other:

- **41-case run** (24 positive / 17 negative) — the original run. `evals/README.md`, section "A
  41-case benchmark derived from real git history."
- **78-case run** (38 positive / 40 negative, this repo's real ceiling for traceable
  bug-introducing commits even with loosened attribution bounds) — 29 of 78 diffs overlap with the
  41-case run (`extract.py`'s selection is deterministic), so this is the original measurement
  scaled up, not a from-scratch replication. `evals/README.md`, section "Scaled up: 78 cases."

## Results

**Full pipeline (persona lenses + discourse) vs. a single-lens baseline** (`--lenses ""`, the
closest single-strong-reviewer approximation buildable from existing flags):

| | full pipeline (41-case) | single-lens (41-case) | full pipeline (78-case) | single-lens (78-case) |
|---|---|---|---|---|
| Recall | 0.792 | 0.375 | 0.816 | 0.395 |
| False-positive rate | 0.647 | 0.294 | 0.675 | 0.300 |
| Precision | 0.633 | 0.643 | 0.534 | 0.556 |
| Avg. cost/calls per diff | $0.0035 / 5.9 | $0.0012 / 1.8 | $0.0039 / 5.8 | $0.0015 / 1.7 |

Precision stayed close between the two configs at both sample sizes (within ~0.01–0.11, no
consistent direction — full pipeline slightly ahead at n=41, single-lens slightly ahead at n=78) —
the full pipeline's extra findings aren't disproportionately noise, it's genuinely more sensitive,
not just louder. Recall roughly doubled at both sizes. Cost ratio held at ~3.4x both times. The
comparison was re-run independently at n=78, not just scaled from the n=41 numbers — same effect
size showed up on a sample nearly twice as large, which is evidence against the original n=41
result being a fluke of that particular sample.

**Discourse confidence calibration** (does self-reported confidence predict whether a finding is
really at the historical defect's location, checked via `git blame` against the fix commit — not
self-graded):

| Signal | n=41-case sample | n=78-case sample |
|---|---|---|
| `Finding.confidence`, all findings (high vs. medium) | 0.495 vs. 0.537 | 0.515 vs. 0.505 |
| `Finding.confidence`, CONFIRMED only (high vs. medium) | 0.75 vs. 0.7 | 0.648 vs. 0.5 |
| Discourse-move AGREE confidence (high vs. medium) | 0.44 vs. 0.929 (n=50/14) | 0.467 vs. 0.3 (n=90/10) |

The move-level "medium beats high" result at n=64 (41-case run) did **not** replicate at n=100
(78-case run) — reverted to the expected direction, read as a small-sample artifact rather than a
real, reproducible finding. Across both runs and both signals, confidence tiers stay only weakly
distinguishable — self-reported confidence is not a reliable signal to trust without independent
verification.

**Self-consistency baseline** (#209 — does running the single-lens config N times independently
and voting recover the full pipeline's recall, at comparable cost?): 3 independent passes per diff
on the 41-case set.

| Aggregation | Recall | Precision |
|---|---|---|
| Any of 3 flagged it | 0.667 | 0.640 |
| Majority (2 of 3) | 0.292 | 0.636 |
| All 3 agreed | 0.083 | 1.000 |
| *(single pass, n=1)* | 0.375 | 0.643 |
| *(full pipeline)* | 0.792 | 0.633 |

Real cost: $0.0039 / 5.4 calls per diff — essentially the same as the full pipeline ($0.0035 /
5.9). Even the most lenient aggregation falls short of the full pipeline's recall at comparable
cost; majority voting is *worse* than a single pass, a real consequence of each pass's <50%
per-trial hit rate (not a bug — majority-of-N only helps when per-trial accuracy exceeds 50%).
Real evidence the full pipeline's advantage is from the architecture (persona diversity +
discourse), not just from making more LLM calls per diff.

## Real bugs found and fixed along the way

All found via real diffs failing/misbehaving during the runs above, not hypothesized in advance.

- `quantify::verdict` let a policy failure alone (missing tests/changelog, oversized diff) force
  `REQUEST_CHANGES` — same tier as a confirmed critical defect. Saturated verdict to
  `REQUEST_CHANGES` on 41/41 diffs in the first run, regardless of actual code quality. Fixed
  (policy failure now caps at `COMMENT`); the 78-case run confirms the fix works — verdicts are
  genuinely distributed now instead of saturated.
- `discourse::votes::VOTE_THRESHOLD` was a hardcoded constant; made spec-configurable
  (`[discourse].vote_threshold`), with two example presets (`specs/high-recall.toml`,
  `specs/low-noise.toml`) alongside the balanced default.
- Six distinct real secret-scanner false positives found and fixed across the two runs: a `==`
  comparison mistaken for an assignment, a dotted property reference, a whole expression
  (function call + fallback) captured as a value, a bare identifier with no digits, a PEM-header
  marker string matched inside code that processes it as text (not an embedded key), and a bare
  increment expression. One related regression risk (a real quoted secret followed by a statement
  semicolon) was caught and fixed in the same pass, before it shipped.
- `evals/szz-bench/aggregate.py` itself had a real bug: its verdict-parsing regex predated the
  `verdict_reason` slug format and excluded digits from the reason name's character class, so
  every verdict silently came back `UNKNOWN` after the `verdict_reason` feature shipped.
- Two real, correctly-flagged (not bugs) secret-scanner hits required manual verification and
  `--allow-sensitive-input`: a client-side Firebase Web API key and a Supabase anon key — both
  documented by their respective vendors as safe to embed client-side, protected by
  Security-Rules-style access control rather than by hiding the key.

## What this doesn't prove

- One repo, one spec, two runs of the same repo (not two independent repos).
- No cross-language/cross-repo validation.
- Confidence-calibration checks a location-match proxy, not full semantic correctness of a claim.
- 78 is still short of the 100–500 scale a fully convincing benchmark would want; this repo's real
  history doesn't yield more than 38 attributable positive cases without accepting noisier
  attribution from larger, harder-to-attribute commits.
- The self-consistency baseline used N=3 only, at n=41 (not re-run at n=78), and used simple
  per-diff flagged/not-flagged voting rather than matching individual findings across the 3
  passes — a finding-level match (same claim, not just "something got confirmed") wasn't attempted.
