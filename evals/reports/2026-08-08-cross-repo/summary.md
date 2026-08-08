# Real-benchmark report — 2026-08-08 (cross-repo)

A second, independent SZZ-derived benchmark, this time against Code-Review-Loop's own git
history (a Rust CLI, not the Flutter/Supabase app the first 78-case benchmark used) — the "does
this generalize to another repo/language" question the main benchmark couldn't answer on its
own. It doesn't generalize well: recall and precision both drop sharply, and a same-diff repeat
run shows the underlying scoring is far noisier than a single benchmark pass suggests.

## Cost

| Run | Reviews | LLM calls | Real cost |
|---|---|---|---|
| Cross-repo benchmark (34 diffs: 9 positive / 25 negative, 3 failed the local secret scan before any call was made) | 31 | 167 | $0.0837 |
| Same-diff repeat run (12 of the 34 diffs, re-run independently) | 12 | 65 | $0.0426 |
| **Total** | **43** | **232** | **$0.1263** |

## Methodology

Same tooling as the main benchmark (`evals/szz-bench/extract.py`, SZZ against `fix:`-prefixed
commits, `git blame` on the parent to trace the bug-introducing commit), pointed at
Code-Review-Loop's own repository instead of the original target. 57 fix-grep commits were found;
only 9 yielded a blame-traceable BIC (this is expected — SZZ's traceable-positive rate is usually
well under the raw fix-commit count, and matched the main benchmark's own experience). 25
same-era commits were sampled as the negative set.

3 of the 34 extracted diffs modify `src/secretscan.rs` itself and were refused by the local
secret scanner before any LLM call was made — that file's own test fixtures contain
credential-shaped strings (e.g. a fake AWS access key) by design, so the scanner correctly (if
inconveniently, for this specific repo) treats diffs touching it as sensitive. All 3 are in the
negative set; the positive set (9 cases) was unaffected. This reduced the usable negative sample
from 25 to 22 without any manual override (`--allow-sensitive-input` was not used).

The repeat-run measurement re-ran 12 of the 34 diffs (all 9 positives, 3 negatives) a second,
fully independent time — same spec, same diff, same model, no `--temperature` override (provider
default, unset in both runs) — and compared each pair's outcome.

## Results

### Recall / precision (single pass, n=34, same scoring method as the main benchmark)

| | Cross-repo (this report) | Main benchmark (n=78) |
|---|---|---|
| Recall | **0.444** (4/9) | 0.816 |
| Precision | **0.222** (4/18) | 0.53–0.63 |

Both numbers are far weaker here. The 5 missed positives all share the same failure mode already
identified in the main benchmark's own miss analysis: `verdict_reason=policy_failure` — no
lens/discourse round ever proposed a confirmed finding matching the actual injected regression;
the only reason the verdict wasn't a plain APPROVE was an unrelated policy check (test/doc
coverage, diff size, etc.) failing on the side. That's now the same pattern in two different
repos, in two different languages, on two different domains — evidence this is a real, systematic
gap (lens coverage / prompting not surfacing the right defect class), not sampling noise from one
benchmark.

False-positive rate (14/22 usable negatives, 63.6%) is close to the main benchmark's rate
(27/40, 67.5%) — the tendency to flag *something* on an already-clean diff looks roughly
repo-independent, while catching the actual planted defect (recall) does not.

### Vote-net distribution among this run's CONFIRMED findings

`discourse/mod.rs` now logs each CONFIRMED finding's local vote net in `state.json` (added this
session specifically so this kind of resweep doesn't need a full re-run to check). Across this
run's `state.json` files, logged nets clustered at exactly the default 0.6 threshold (13
occurrences) or a full 1.0 (35 occurrences) — little middle ground. Since the dominant miss
pattern above is "no finding was ever proposed," not "a proposed finding fell just short of the
threshold," retuning `vote_threshold` has limited room to move recall on the cases actually
missed in this run.

### Same-diff repeat run (n=12): does the verdict even hold from one run to the next?

| | Count |
|---|---|
| Same catch/miss outcome both runs | 6/12 (50.0%) |
| **Flipped catch/miss outcome** | **6/12 (50.0%)** |

Confirmed-finding count, both runs, same spec/diff/model:

| Case | Run 1 confirmed | Run 2 confirmed | Outcome |
|---|---|---|---|
| pos-00 | 2 | 3 | same (caught both times) |
| pos-01 | 1 | 1 | same (caught both times) |
| pos-02 | 2 | 0 | **flipped: caught → missed** |
| pos-03 | 0 | 1 | **flipped: missed → caught** |
| pos-04 | 0 | 0 | same (missed both times) |
| pos-05 | 5 | 0 | **flipped: caught → missed** |
| pos-06 | 0 | 4 | **flipped: missed → caught** |
| pos-07 | 0 | 1 | **flipped: missed → caught** |
| pos-08 | 0 | 2 | **flipped: missed → caught** |
| neg-00 | 0 | 0 | same (clean both times) |
| neg-01 | 6 | 13 | same (flagged both times) |
| neg-02 | 0 | 0 | same (clean both times) |

Every negative in this subset (3/3) was stable across both runs; 5 of 9 positives (55.6%) were
not. This is consistent with the earlier finding: whether a genuinely clean diff gets flagged at
all is comparatively stable, but whether a real defect gets *caught* is not — a single run's
recall number is one noisy draw, not a fixed property of the tool on a given diff. A recall of
0.444 measured once could plausibly have come out anywhere in a wide range on a re-run of the
exact same 9 cases.

This replaces the only non-determinism evidence this project had before today — a single
before/after anecdote from an early promptfoo experiment (see "LLM non-determinism is real"
below) — with a real, systematic, same-config measurement.

## Real bugs found and fixed along the way

None found via manual triage of this run's findings — unlike the main benchmark's write-up, this
run's flagged findings were not individually read and verified against the target commits. The
numbers above come from the same automated confirmed-finding-count scoring the main benchmark
uses, not from a human-reviewed audit of each case.

## What this doesn't prove

- n=9 positives is a small sample; a single-point recall estimate at this size has a wide
  confidence interval on its own, before even accounting for the run-to-run variance measured
  above. Treat 0.444 as "much lower than 0.816, in the same direction the failure-mode analysis
  would predict," not as a precise number.
- Only one alternate repo/language was tested (a Rust CLI tool), and it happens to be this
  project's own codebase — reviewing your own source is not the same setting as reviewing an
  unrelated team's code, and any prompt/lens tuning that happened to be informed by working on
  this codebase could cut either way (helping or hurting) in ways that wouldn't apply to a truly
  unrelated third repo.
- The repeat-run comparison only covers 12 of the 34 cases (cost/time-bounded), and used the
  provider's default (unset) temperature in both runs — it measures non-determinism as currently
  shipped, not whether a lower `--temperature` (now available, see `src/cli.rs`) would reduce the
  flip rate; that's a separate, still-open measurement.
- No causal diagnosis of *why* recall is lower here (subtler Rust logic bugs vs. the original
  benchmark's more surface-level app-flow bugs, lens selection not fitting this domain well,
  etc.) — this report establishes that generalization is weak, not why.
