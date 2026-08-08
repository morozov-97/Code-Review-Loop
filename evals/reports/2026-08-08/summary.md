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
| 78-case scale-up | 78 | 451 | $0.3022 |
| **Total** | **184** | **906** | **$0.5722** |

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

| | full pipeline (41-case) | single-lens (41-case) | full pipeline (78-case) |
|---|---|---|---|
| Recall | 0.792 | 0.375 | 0.816 |
| False-positive rate | 0.647 | 0.294 | — |
| Precision | 0.633 | 0.643 | 0.534 |
| Avg. cost/calls per diff | $0.0035 / 5.9 | $0.0012 / 1.8 | — |

Precision stayed close between the two configs at n=41 (0.633 vs. 0.643) — the full pipeline's
extra findings aren't disproportionately noise, it's genuinely more sensitive, not just louder.
Recall roughly doubled both times the comparison was measurable. The single-lens baseline
comparison wasn't re-run at n=78.

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
- The comparison-matrix (full pipeline vs. baseline) was only measured once, at n=41 — not re-run
  at n=78.
- Confidence-calibration checks a location-match proxy, not full semantic correctness of a claim.
- 78 is still short of the 100–500 scale a fully convincing benchmark would want; this repo's real
  history doesn't yield more than 38 attributable positive cases without accepting noisier
  attribution from larger, harder-to-attribute commits.
