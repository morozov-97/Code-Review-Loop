# Golden-set regression scaffold

This directory sets up a [promptfoo](https://www.promptfoo.dev/) golden-set that runs the
actual `codereview` binary (`--backend openrouter`) against a few fixed diffs and checks
`report.md`'s verdict and key finding keywords — the empirical accuracy check this project
otherwise lacks.

## Validated against a real model

This has actually been run end-to-end with `openai/gpt-oss-120b` via OpenRouter. The
`promptfoo` harness itself works (`exec:` provider syntax, `run-codereview.sh`'s argument
handling, `assert-report.cjs`'s grading). Three real bugs were found and fixed this way, not by
inspection:

- `clean.patch` originally had no accompanying test/doc changes, so the deterministic policy
  checks (`Tests accompany behavior changes`, `Changelog/documentation updated`) correctly
  forced `REQUEST_CHANGES` even though the change itself had no real findings — this was the
  golden set's own expectation being wrong, not a tool bug. Fixed by making the fixture pair a
  test-file and changelog change with the source change.
- `sql-injection.patch`'s `expectContains: ["find_user"]` was flaky — the LLM reliably quotes
  the vulnerable *line* (which includes the `username` variable) but doesn't reliably name the
  *enclosing function*. Replaced with `"username"`.
- A per-test singular `provider: "exec:./run-codereview-default-rounds.sh"` field silently
  didn't override anything — every result row reported the same provider id regardless, and
  every assertion (including CI) still passed since the fast and default-rounds paths both
  satisfy the loose bounds. Fixed by switching to `providers:` (plural) allow-lists instead
  (see "What's here" below). **Re-confirmed with a second real run** after the fix: filtering
  to just the SQL injection case (`--filter-pattern "SQL injection"`) now produces two visibly
  distinct provider columns (`fast-round` / `default-rounds`) pointing at the two different
  scripts, and `default-rounds` took ~2x as long wall-clock (225s vs. 113s) — consistent with a
  real second discourse round actually running, not just a different label on the same call.

## Also validated against a real external codebase (not the golden set)

Beyond the 5 synthetic fixtures above, `codereview` was run once against a real single-commit
diff from an unrelated private production mobile app (not part of this repo or the golden set) —
an auth/session error-handling refactor in a Flutter/Supabase codebase, 1 file, +29/-23 lines.
Identifying details (app name, file paths, in-app UI strings) are deliberately omitted here; only
the review mechanics and real output numbers are recorded.

- **Result:** `REQUEST_CHANGES`, score 94/100, effort 3/5, 7 provider calls, cost $0.0045
  (`--backend openrouter`, default model).
- **The finding was real, not fabricated:** the diff had collapsed a specific
  network-exception branch (previously mapped to its own error code) into a broader generic
  exception handler, losing the ability for callers to distinguish network failures from other
  auth failures. The review caught exactly that, cited the correct before/after lines, and
  scored it as a minor (P2/P3) deduction rather than overstating it.
- **The one result worth calling out:** discourse's cross-verification rejected two of the four
  raw candidate findings before they reached the report — one claimed a fallback code path had
  been removed (it never existed in the original code either), the other claimed error detail
  was being dropped (it wasn't; the exception subclass relationship preserved it). Both
  rejections were correct on inspection of the actual diff. This is the first observed instance
  in this repo of the "independent review + anonymous cross-verification catches an unfounded
  claim" mechanism actually firing on a real diff, not just passing a unit test for the
  mechanism's plumbing.
- **What this does and doesn't prove:** one external diff is n=1 — it demonstrates the mechanism
  can work, not that it reliably does. It's not a substitute for the labeled, larger-scale
  benchmark comparing single-LLM vs. multi-persona-plus-discourse accuracy that issue #161 asks
  for and that this repo still doesn't have.

### Second run: the vote gate prevented a bad outcome, but didn't reach full precision

A second real diff from the same private app (unrelated feature, 14 files, +80/-65 lines) —
adding per-message mode tracking to a chat feature — produced `REQUEST_CHANGES`, score 95/100,
effort 4/5, 7 provider calls, cost $0.0086.

One finding claimed a P0 compile error: an undefined-variable reference inside a JSON
deserialization factory. Discourse `AGREE`d on it in **both** rounds (vote net cleared
`VOTE_THRESHOLD`). Checked against the actual source file (not just the diff) to settle it: the
claim was **wrong** — the referenced line was inside an unrelated instance method (not the JSON
factory the LLM thought it was in), where the identifier is a valid field reference, not an
undefined variable.

What actually happened in the report: the finding stayed `UNCERTAIN` — `evidence_unverified`
blocked it from being confirmed and counted toward the score/verdict, and it was routed to
"Needs Human Review" instead. This is the local vote/evidence gate (issue #148, fixed earlier in
this repo's history) doing its job for real: a wrong, high-severity, discourse-agreed claim did
not get to drive the verdict.

But it's a partial result, not a clean one. The same report *did* fully `REJECT` four other
unfounded candidate findings via discourse `CHALLENGE` — this one only made it to `UNCERTAIN`,
not a confident rejection. So: the gate stopped the worse outcome (a false P0 silently scored),
but didn't reach the better outcome (confidently identifying the claim as wrong without a human
having to check the source file). Recorded here as a real, mixed data point rather than rounding
it up to a clean success.

### Also found along the way: a secret-scanner false positive

Reviewing the first diff above required `--allow-sensitive-input` — the local secret scanner
(`src/secretscan.rs`) refused to send it, flagging an ordinary `max_tokens: <value>` parameter in
an LLM API call as a suspected credential. Root cause and fix proposal filed as
[#181](https://github.com/Loop-Suite/Code-Review-Loop/issues/181):
`SECRET_KEY_MARKERS` includes the bare substring `"TOKEN"` with no word-boundary check, so any
`*_TOKEN`-containing identifier (`max_tokens`, `token_count`, etc.) trips it, not just genuine
token/credential fields.

## A 41-case benchmark derived from real git history (not hand-picked, not fabricated)

The two single-diff runs above are n=1 anecdotes. Issue #161 asks for something closer to a real
labeled benchmark — but a labeled benchmark needs ground truth, and an LLM (or the person running
it) hand-picking diffs and then judging its own output is circular, not independent evidence. The
approach here instead derives ground truth from a real project's own history, using
[SZZ](https://en.wikipedia.org/wiki/SZZ_algorithm) (a standard, established bug-introducing-commit
technique — not something invented for this write-up): for each commit whose message starts with
`fix:` in a real, unrelated private production app's git history (same Flutter/Supabase app as the
two runs above), `git blame` on the fix's parent commit finds which earlier commit last touched the
lines the fix changed. That earlier commit is a real, historically-confirmed bug-introducing commit
(BIC) — not a guess, not synthetic.

- **Positive set (24 diffs):** BICs traced from 119 real `fix:` commits (27 traceable candidates
  found; 3 dropped — 2 pure reverts, 1 trivial rename — leaving 24). Each is a real diff that a
  later commit in the same project's history confirms introduced a defect.
- **Negative set (17 diffs):** same-era, similarly-sized commits (`feat`/`chore`/`perf`/`style`)
  that were never identified as any fix's BIC. Important caveat, stated plainly: this is *absence
  of evidence*, not *proof of cleanliness* — a commit with an undiscovered defect (never fixed, or
  fixed without a `fix:`-prefixed commit message) would be mislabeled "clean" here. This is a known
  limitation of SZZ-derived negative sets in general, not specific to this run.
- **Ran the real `codereview` binary against all 41** (`--backend openrouter`, default model,
  `--max-rounds 1`), same as every other real run in this file.

**The single biggest finding: `verdict` was `REQUEST_CHANGES` on all 41/41 diffs**, positive and
negative alike — including six-line one-file commits and pure reverts. Cause, confirmed by reading
every report's Policy Checks table: `default.toml`'s "tests accompany behavior changes" and
"changelog/documentation updated" policies fail on essentially every commit in this real project,
because this team's actual workflow doesn't add a dedicated test file or changelog entry per
commit — and `quantify::verdict` returns `REQUEST_CHANGES` on *any* policy failure before it ever
looks at confirmed findings. This isn't a new bug — the README already carried a caveat that
`specs/default.toml`'s test/doc policy is "strict enough that even this project's own clean-diff
eval fixture needed a padded test+changelog change to pass it" — but this is that same caveat
confirmed at n=41 on a real project instead of n=1 on a synthetic fixture. **Practical
consequence: raw `verdict` is not a usable accuracy signal for a project whose commit style
doesn't match the default spec's assumptions**, regardless of how good or bad the underlying
review is. Anyone evaluating this tool against their own repo should check their spec's policy
pass rate before trusting `verdict` at all.

**Fixed structurally, not just diagnosed**, in a follow-up to #189: a policy failure alone no
longer forces `REQUEST_CHANGES` — it caps at `COMMENT` unless a confirmed code defect (or a
deterministic tool failure) also fired. `verdict_reason` (the earlier #189 fix) still tells you
which branch produced a verdict; this closes the saturation at its source rather than only
labeling it. The 41-case numbers above predate this fix and are left as-is (they're an accurate
record of what was actually measured); a re-run against the same diffs today would show fewer
policy-driven `REQUEST_CHANGES`/`COMMENT` results without any change to the underlying reviews.

Because of that, the numbers below use "did the review produce at least one `CONFIRMED` finding"
(the `## Findings` table, which the report generator populates only with `CONFIRMED`-status
findings) as the actual signal, not the saturated `verdict` field:

| | predicted "flagged" | predicted "clean" |
|---|---|---|
| **actually had a defect (BIC)** | TP = 19 | FN = 5 |
| **no known defect (negative set)** | FP = 11 | TN = 6 |

Precision (of diffs flagged, how many had a real known defect): **0.633**. Recall (of diffs with a
real known defect, how many got flagged): **0.792**.

Two things that make the raw numbers above easy to over- or under-read, checked by hand rather than
assumed:

- **Spot-checked the false positives — most aren't hallucinations.** Read the actual `CONFIRMED`
  findings on several FP cases (negative-set diffs the tool flagged). They were real, defensible
  observations on real code (e.g. a new cross-feature import creating coupling between two
  previously-independent modules; a batch of maintainability/best-practice notes on a
  security-relevant file that was substantially rewritten) — not nonsense. They just weren't *the*
  defect a later `fix:` commit happened to address, which is the only thing this benchmark's label
  can see. The true "made something up" rate is very likely lower than 11/17 suggests; this
  benchmark can't distinguish "wrong finding" from "real-but-different finding" without a human
  reading every case, which wasn't done here.
- **The 5 false negatives mostly weren't silent misses.** In every FN case checked, the review
  still surfaced *something* in that diff — just not the specific bug a later commit fixed, and
  discourse left those specific claims `UNCERTAIN` rather than `CONFIRMED` (visible in each
  report's "Needs Human Review" section). So "flagged nothing" is a real category, but doesn't
  describe most of the misses.
- **Follow-up work did eventually get real (if partial) data for #163.** This per-diff run alone
  didn't — that needed per-finding, and later per-discourse-move, ground truth. Both were built as
  follow-ups (`evals/szz-bench/calibrate_confidence.py`, `calibrate_move_confidence.py`) and
  produced a genuinely surprising result: AGREE moves at "medium" self-reported confidence had a
  *higher* location-match rate against the real defect (0.929, n=14) than "high" confidence ones
  (0.44, n=50) — the opposite of what `confidence_weight`'s 1.0-for-high/0.6-for-medium weighting
  assumes. Small samples, not a settled result, no constants changed — full numbers and caveats on
  the #163 thread.

**What this does and doesn't answer for #161:** it's real data at 41 cases instead of 5 or 1, with
a methodology that doesn't require fabricating ground truth. It does not include the
single-strong-reviewer-vs-persona-pipeline comparison matrix #161 explicitly asks for (that would
mean running the same 41 diffs through a stripped-down single-pass config too, not done here), and
41 is still small next to the 100-500 the issue names. Posted as a real data point on #161 rather
than closing it — the comparison-matrix and larger-N gaps remain open.

**Four more real secret-scanner false positives found while running this**, distinct from #181's,
all now fixed ([#186](https://github.com/Loop-Suite/Code-Review-Loop/pull/186)/
[#192](https://github.com/Loop-Suite/Code-Review-Loop/pull/192), tracked as
[#185](https://github.com/Loop-Suite/Code-Review-Loop/issues/185)): a `token == null` comparison
had the first `=` of `==` mistaken for an assignment, capturing `"null) {"` as a fake secret value;
`const supabaseKey = AppConfig.supabaseAnonKey;` had a property *reference* (not a literal)
flagged as a credential; `(await freshToken()) ?? supabaseKey;` had a whole expression captured as
"the value"; and a bare identifier with no digits (`'apikey': supabaseKey`) read far more like a
variable name than a generated secret. Separately, one diff assigned real-shaped Google API key
values to client-side Firebase config (`apiKey: 'AIzaSy...'`) — a correct pattern match, not a
bug: sent with `--allow-sensitive-input` after manual confirmation, since Firebase web API keys
are documented by Google as safe to embed client-side (access is controlled by Firebase Security
Rules, not by hiding this value).

### Comparison matrix: full pipeline vs. a single-lens baseline

The 41-case run above only exercised the full persona+discourse pipeline — it couldn't say
whether that pipeline actually beats something simpler, which is the comparison #161 explicitly
asked for. Ran the same 41 diffs a second time with `--lenses ""` (only the always-included
generalist lens, no persona selection) — the closest approximation to "one strong reviewer"
buildable from existing flags, still with the unavoidable minimum of one discourse round (there's
no flag to skip discourse entirely).

| | full pipeline | single-lens baseline |
|---|---|---|
| Recall (caught the known defect) | 19/24 = **0.792** | 9/24 = **0.375** |
| False-positive rate (flagged a "clean" commit) | 11/17 = **0.647** | 5/17 = **0.294** |
| Precision | 19/30 = **0.633** | 9/14 = **0.643** |
| F1 | **0.704** | **0.474** |
| Avg. cost/calls per diff | $0.0035 / 5.9 calls | $0.0012 / 1.8 calls |

**Reading this honestly:** the full pipeline catches more than twice as many known defects
(recall roughly doubles), but also flags "clean" commits more than twice as often — precision is
nearly identical between the two (0.633 vs 0.643). The full pipeline isn't more accurate *per
flag*; it's more sensitive *overall*, catching more of everything (real and spurious alike) at
~3x the cost. F1 favors the full pipeline (0.704 vs 0.474), but F1 assumes precision and recall
matter equally, which is a team's call, not a technical fact this benchmark can settle. Of the 41
cases, both configurations agreed 21 times (12 both-flagged, 9 both-clean); the full pipeline
alone caught 18 the baseline missed; the baseline alone caught 2 the full pipeline missed
(including one real BIC that discourse's vote-gating left `UNCERTAIN` on the full run — the
cheaper single-pass config isn't strictly dominated on every case).

This is real data toward #161, not a settled verdict: it's one repo, one spec, 41 diffs, and
`--lenses ""` isn't identical to "the best possible single-strong-reviewer prompt" — a
purpose-built single-pass reviewer prompt might do better than the generalist lens used here as a
stand-in.

## ⚠ LLM non-determinism is real, not just a theoretical caveat

Running `sql-injection.patch` twice produced two different discourse outcomes: once the SQL
injection finding was left `UNCERTAIN` and dropped from the report's scored findings entirely
(this is what led to the `discourse::run` fixes for report visibility and
`challenge_axis` — see issues #75 and #79), and once it was correctly `CONFIRMED`. The
`expectVerdictIn`/`expectContains` looseness in this config absorbs some of that variance, but
5 distinct golden diffs is not enough to give a statistical reliability guarantee. Treat a single green
run as "didn't regress obviously," not as proof the tool reliably catches everything it's
supposed to.

## What's here

- `promptfooconfig.yaml` — 5 test cases (a clean test+doc-paired diff, a SQL injection, a
  prompt-injection attempt against the reviewer itself, a hardcoded API key, a
  panic-on-untrusted-input `.unwrap()`) against 2 configured providers (`fast-round` /
  `default-rounds`, see below) — 4 cases are restricted via `providers: [fast-round]` to just the
  fast path; the SQL injection case is left unrestricted so it runs (#176) against both, producing
  6 result rows total. See the file's own header comment for a real bug this design replaced: an
  earlier attempt used a per-test singular `provider:` field expecting it to override the script,
  which silently didn't work (confirmed by actually running it against OpenRouter — every row
  reported the same provider id) despite every assertion, including CI, passing anyway.
- `diffs/*.patch` — the golden diffs.
- `run-codereview.sh` (provider `fast-round`) — wrapper that runs
  `codereview review --backend openrouter --max-rounds 1` on a diff and prints `report.md` plus a
  `MANIFEST_PROVIDER_CALLS` marker line built from `manifest.json`'s `usage.calls`.
- `run-codereview-default-rounds.sh` (provider `default-rounds`, #176) — identical, but without
  forcing `--max-rounds 1` — lets the CLI's own default (2 rounds) apply, so the SQL injection case
  measures the actual default-path cost/behavior instead of only the cheaper path every other case
  exercises.
- `assert-report.cjs` — checks `report.md`'s `Verdict:` line, required/forbidden substrings, and
  (#176) optionally a loose upper bound on provider calls via `expectMaxProviderCalls` — a
  regression guard against a gross call-count blowup, not a tight cost budget (there's no real
  historical data in this repo to calibrate a tight one against).

## Running it

```bash
cargo build --release
export OPENROUTER_API_KEY=...
cd evals
npx promptfoo@latest eval -c promptfooconfig.yaml --no-cache -j 1 -o result.json
```

Costs real LLM call money per run (6 cases, each several sequential calls). `-j 1` runs cases
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
