# Golden-set regression scaffold

This directory sets up a [promptfoo](https://www.promptfoo.dev/) golden-set that runs the
actual `codereview` binary (`--backend openrouter`) against a few fixed diffs and checks
`report.md`'s verdict and key finding keywords — the empirical accuracy check this project
otherwise lacks.

## Validated against a real model

This has actually been run end-to-end with `openai/gpt-oss-120b` via OpenRouter. The
`promptfoo` harness itself works (`exec:` provider syntax, `run-codereview.sh`'s argument
handling, `assert-report.cjs`'s grading). Two real bugs were found and fixed this way, not by
inspection:

- `clean.patch` originally had no accompanying test/doc changes, so the deterministic policy
  checks (`Tests accompany behavior changes`, `Changelog/documentation updated`) correctly
  forced `REQUEST_CHANGES` even though the change itself had no real findings — this was the
  golden set's own expectation being wrong, not a tool bug. Fixed by making the fixture pair a
  test-file and changelog change with the source change.
- `sql-injection.patch`'s `expectContains: ["find_user"]` was flaky — the LLM reliably quotes
  the vulnerable *line* (which includes the `username` variable) but doesn't reliably name the
  *enclosing function*. Replaced with `"username"`.

## ⚠ LLM non-determinism is real, not just a theoretical caveat

Running `sql-injection.patch` twice produced two different discourse outcomes: once the SQL
injection finding was left `UNCERTAIN` and dropped from the report's scored findings entirely
(this is what led to the `discourse::run` fixes for report visibility and
`challenge_axis` — see issues #75 and #79), and once it was correctly `CONFIRMED`. The
`expectVerdictIn`/`expectContains` looseness in this config absorbs some of that variance, but
5 golden cases is not enough to give a statistical reliability guarantee. Treat a single green
run as "didn't regress obviously," not as proof the tool reliably catches everything it's
supposed to.

## What's here

- `promptfooconfig.yaml` — 5 test cases: a clean (test+doc-paired) diff, a SQL injection, a
  prompt-injection attempt against the reviewer itself, a hardcoded API key, and a
  panic-on-untrusted-input `.unwrap()`.
- `diffs/*.patch` — the golden diffs.
- `run-codereview.sh` — wrapper that runs `codereview review --backend openrouter` on a diff
  and prints `report.md` (this is what promptfoo's `exec` provider invokes).
- `assert-report.cjs` — checks `report.md`'s `Verdict:` line and required/forbidden substrings.

## Running it

```bash
cargo build --release
export OPENROUTER_API_KEY=...
cd evals
npx promptfoo@latest eval -c promptfooconfig.yaml --no-cache -j 1 -o result.json
```

Costs real LLM call money per run (5 cases, each several sequential calls). `-j 1` runs cases
serially — safe default, raise it if you want speed over predictable ordering in the output.

## Notes

- The `prompt-injection-attempt.patch` case is the most valuable one here: it's an empirical
  regression test for the `src/promptctx.rs::fenced()` fix — confirming under a real model
  (not just the unit tests, which only check fence-length logic) that embedded fake
  instructions in a diff don't flip the verdict to APPROVE. This one has passed on every run
  so far.
- Individual finding wording isn't asserted on — LLM phrasing varies. Only the verdict
  direction and a couple of stable keywords are checked, and even those needed one correction
  (see above) after an actual run showed a keyword wasn't as stable as guessed.
- Not wired into CI (`.github/workflows/ci.yml`) — it costs money per run and needs a secret,
  so it should be a deliberately-triggered job (e.g. `workflow_dispatch`), not on every push.
