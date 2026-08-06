# Additional Research Evidence (2026-07-29)

## 1) Research Evidence Summary (from the perspective of code review accuracy, false-positive rate, and cost)

- **SWE-PRBench (arXiv:2603.26130)**  
  Reports that across a benchmark based on 350 PRs, 8 LLMs' detection rate for human-labeled issues stays mostly in the **15-31%** range even under config_A (2000 token diff+summary).  
  Per-type analysis shows **Type2 (context-based) issues degrade significantly**.  
  → For reviews, **focusing on verifiable finding candidates + mitigating context overload** matters more than "saying a lot."

- **ContextCRBench / FSE 2026 (2511.07017)**  
  Reports that supplementing existing benchmarks' context limitations (coarse context at the issue-description/file level) shows PR-text/text-based context has a significant effect on performance.  
  → This provides grounding for decomposing the context passed before/after `discourse` in our pipeline into "text-core/symbol-level" units.

- **SWR-Bench (FSE 2026 research)**  
  A setup using 1000 verified PRs, full-project context, and structurally labeled ground truth.  
  The reported results suggest that a simple fixed prompt has significant drawbacks, and that suppressing false positives matters for multi-pass review (or staged aggregation).  
  → In multi-angle discourse, an **evidence-based, top-priority reduction strategy** is more rational than majority vote.

- **Claw-SWE-Bench (arXiv:2606.12344)**  
  On the same backbone, the minimal adapter scores Pass@1 **19.1%** vs. the full adapter's **73.4%** — a large gap on the identical model.  
  → Evidence that **carefully designing the adapter/workflow itself** creates a bigger performance difference than the LLM's raw capability.

- **AACR-Bench (arXiv:2601.19494)**  
  Reports a **285% increase** in latent-defect detection rate by using an expert-verified pipeline to compensate for noise/omissions in PR-label-based data.  
  → In the validation-data construction stage, a "human verification + recall-bias correction" procedure has a direct impact on perceived performance.

- **SWE-Cycle (arXiv:2605.13139)**  
  States that staged integration such as environment restoration → implementation → test generation, along with end-to-end FullCycle, better reflects real-world difficulty than a single task.  
  → Since review-only judgment isn't the same as final quality, split comment candidates into **static scoring + execution-based verification (optional/isolated environment)**.

- **Evaluating AGENTS.md (arXiv:2602.11988)**  
  Reports that repository context files aren't always beneficial, and in some cases can lower task success while **increasing reasoning tokens by 20% or more**.  
  → When designing `reviewer memory`/`summary rules`, we should prefer an **execution-evidence-based policy over an overly-refined standing rule set**.

- **Survey: 99 papers (arXiv:2602.13377)**  
  A code review evaluation survey that synthesizes 99 papers, emphasizing the need for fine-grained multi-layer task decomposition, dynamic execution verification, and multilingual/multi-domain expansion.  
  → This aligns with `Code-Review-Loop`'s current score/effort/vet separation design, and follow-up runs will need per-category slicing.

## 2) Implementation References (additional candidate repos to review)

- [OpenReview](https://github.com/vercel-labs/openreview): `@openreview` on-demand review + sandboxed execution + inline suggestion + durable workflow (GitHub event-driven).  
  A public implementation demonstrating a "review → apply suggestion → reaction-based re-run" structure that fits our current routine.

- [Gito](https://gito.bot/): a multi-vendor (Anthropic/OpenAI/compatible LLM) + local/CI integration approach exposing a structure that separates statistics from learned policy.

- [Mira](https://docs.miracode.ai/): a self-hostable code review bot. Emphasizes indexing-based rules, vulnerability detection (OSV), and per-PR/permission rule learning (react/feedback) features.

- [Open Code Review (Alibaba)](https://github.com/alibaba/open-code-review): a deterministic + agent hybrid structure, a CLI-centric approach that handles both rule-based (e.g. NPE/SQLi/XSS) and line-level review together.

- [Code Review Bench (withmartian)](https://www.codereviewbenchmark.com/): provides a production-style PR-tracking benchmark, structuring operational metrics to compare scores across real tools.

## 3) Items to Adopt Immediately for Accuracy Improvement (mapped to our repo)

1. **One step further:** Instead of stopping after a single review, bake at least one `generate-review-revise` pass centered on P0/P1 into the workflow (sample toggle)
2. **Strengthen the context gate:** Pass only the necessary file/symbol/test information into per-lens prompts, and restrict excessive `context token` expansion to `severity <= P2` only.
3. **Separate accuracy/false-positive metrics:** Separate candidate aggregation (score) from confirmed aggregation, and always expose both as "confirmed rate vs. candidate rate" in the UI.
4. **Adaptive cost policy:** Reduce discourse/re-review rounds for large changes, and add extra consensus rounds only for small P0/P1 clusters.
5. **Report-based rule audit:** Ensure reproducibility by also reflecting the public-release sync policy consistently, in line with `docs/organization/public-sync-mapping.md`.

## 4) Next Experiment Proposal (for testing token/accuracy hypotheses)

- Validate with 3 A/B switches:
  - `context size`: minimal/medium/detailed context
  - `discourse`: off / limited / full
  - `re-review`: off / enabled (P0/P1 only)
- Record the following metrics simultaneously for each set:
  - Precision@P0,P1, confirmed_fdp, duplicate overlap, hallucination profile,
  - token consumption (input/output), number of execution steps, processing time.
- Don't draw early conclusions from numeric trends in existing benches/tests alone — report run-to-run variance across 3+ repetitions.
