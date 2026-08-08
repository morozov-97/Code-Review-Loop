# Dated real-benchmark reports

`evals/README.md` is a living narrative — useful for the story of how the benchmark evolved, less
useful as a fixed record once later edits (fixes, scale-ups, corrections) shift context around an
earlier finding. This directory holds point-in-time snapshots instead: what was measured, what it
cost, and what was found, frozen as of the date it ran.

## Naming convention

- One report per real benchmark session, in its own dated folder:
  `evals/reports/YYYY-MM-DD/summary.md`.
- If more than one distinct session happens on the same calendar date, disambiguate with a short
  slug: `evals/reports/YYYY-MM-DD-<slug>/summary.md` (e.g. `2026-08-08-scale-up`). Don't reuse a
  date+slug combination — if a session continues work from an earlier report the same day, extend
  that report instead of creating a near-duplicate.
- The file inside is always named `summary.md`, even if a report is the only thing in its folder
  — keeps every report addressable the same way (`evals/reports/*/summary.md` globs cleanly) and
  leaves room for a report to carry supplementary files later without a rename.
- Use [TEMPLATE.md](TEMPLATE.md) as the starting point for a new report — same section order and
  headers every time, so a reader who's seen one can skim any other the same way. Fill in every
  section; don't delete one because it's inconvenient (e.g. skipping "What this doesn't prove"
  because the results looked good).

## Index

| Date | Summary |
|---|---|
| [2026-08-08](2026-08-08/summary.md) | First SZZ-derived real benchmark (41→78 cases), full-pipeline-vs-single-lens comparison, discourse confidence calibration, $0.57 total real cost across 184 runs. |

Add a row here whenever a new report lands — this table is the entry point, not something a
reader should have to reconstruct by listing directories.
