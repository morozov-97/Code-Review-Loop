<!--
Template for a dated real-benchmark report. Copy this file to
evals/reports/<date>/summary.md (see evals/reports/README.md for the naming rule), fill in every
section, then delete this comment block. Keep the section headers and order — a reader who's seen
one report should be able to skim any other one the same way.

Ground rules for every report in this directory (same as the rest of evals/):
- Every number must come from a real --backend openrouter run against a real target — nothing
  estimated, nothing hypothetical.
- No app name, file paths, in-app text, or other identifying detail about the target codebase.
  Describe it generically (language/framework is fine: "a Flutter/Supabase app").
- No fabricated ground truth. If a claim needs a comparison baseline or labeled data, say where
  that came from (SZZ-derived, a real fix commit, etc.) or don't make the claim.
- State what the report does NOT prove as plainly as what it does. A caveat you skip is a claim
  the next reader will over-trust.
-->

# Real-benchmark report — <YYYY-MM-DD>

<One or two sentences: what prompted this run, and the one-line takeaway. This is what someone
skimming the reports index reads first.>

## Cost

<Every number from an actual `manifest.json` (`usage.calls`/`usage.cost_usd`), not estimated.>

| Run | Reviews | LLM calls | Real cost |
|---|---|---|---|
| <label> | <n> | <n> | $<amount> |

## Methodology

<What ground truth this run used and how it was derived (e.g. SZZ against a real repo's `fix:`
history) — enough that a reader can judge whether the numbers below are trustworthy without
re-deriving the method. Link to reusable tooling (`evals/szz-bench/...`) instead of re-describing
it in full.>

## Results

<The actual findings — tables/numbers preferred over prose. If this report builds on a prior
dated report's numbers, show both instead of only the latest, and say explicitly whether this is
an independent measurement or an extension/re-run of the same underlying sample.>

## Real bugs found and fixed along the way

<Anything discovered through running this — false positives, regressions, tooling bugs — found
via the run itself, not hypothesized in advance. Link the PR/issue for each. If nothing was found,
say so rather than omitting the section.>

## What this doesn't prove

<Sample size, scope (one repo/one spec/etc.), methodology limits, anything a reader might
over-generalize from these numbers if not told explicitly not to.>
