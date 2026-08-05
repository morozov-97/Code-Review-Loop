# Golden-set regression scaffold (unverified)

This directory sets up a [promptfoo](https://www.promptfoo.dev/) golden-set that runs the
actual `codereview` binary (`--backend openrouter`) against a few fixed diffs and checks
`report.md`'s verdict and key finding keywords — the empirical accuracy check this project
currently lacks (see the roadmap discussion this scaffold came out of).

## ⚠ Not yet executed or validated

Everything in this directory was written but **never actually run**. Building and running it
requires a real `OPENROUTER_API_KEY` and costs real LLM call money per run, neither of which
were available in the environment this was authored in. Unlike the rest of this project's
tests (`cargo test`, all passing), nothing here has been confirmed to actually work —
`promptfoo`'s exact `exec` provider config syntax, the wrapper script's argument handling, and
whether the three golden diffs produce the expected verdicts against a real model are all
unverified. Treat this as a starting point to run and fix, not as a working test suite yet.

## What's here

- `promptfooconfig.yaml` — 3 test cases (clean diff, obvious SQL injection, a diff that
  attempts prompt injection against the reviewer itself)
- `diffs/*.patch` — the golden diffs
- `run-codereview.sh` — wrapper that runs `codereview review --backend openrouter` on a diff
  and prints `report.md` (this is what promptfoo's `exec` provider invokes)
- `assert-report.cjs` — checks `report.md`'s `Verdict:` line and required/forbidden substrings

## To actually validate this

```bash
cargo build --release
export OPENROUTER_API_KEY=...
cd evals
npx promptfoo@latest eval -c promptfooconfig.yaml --no-cache -j 1 -o result.json
```

Then fix whatever's broken — likely candidates: the `exec:` provider syntax/args, whether
`run-codereview.sh` needs the diff file path resolved relative to `evals/` vs the repo root,
and whether the golden diffs actually trigger the expected verdicts with the model you're
using (loosen `expectVerdictIn`/`expectContains` if a reasonable model disagrees with the
guessed expectation).

## Notes

- The `prompt-injection-attempt.patch` case is the most valuable one here: it's an empirical
  regression test for the `src/promptctx.rs::fenced()` fix — confirming under a real model
  (not just the unit tests, which only check fence-length logic) that embedded fake
  instructions in a diff don't flip the verdict to APPROVE.
- Individual finding wording isn't asserted on — LLM phrasing varies. Only the verdict
  direction and a couple of stable keywords (e.g. the injected function's name) are checked.
- Not wired into CI (`.github/workflows/ci.yml`) — it costs money per run and needs a secret,
  so it should be a deliberately-triggered job (e.g. `workflow_dispatch`), not on every push,
  if/once it's validated.
