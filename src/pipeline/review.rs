use crate::discourse;
use crate::evidence;
use crate::fixcheck;
use crate::humanvoice;
use crate::input;
use crate::lens::{self, Finding};
use crate::llm::Llm;
use crate::manifest;
use crate::pipeline::{enforce_secret_scan, par_map, prepare_out};
use crate::policy;
use crate::quantify;
use crate::report;
use crate::requirements;
use crate::semgrep;
use crate::spec::Spec;
use crate::state;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// `review`'s CLI arguments, bundled so `run_review` takes one param instead of growing a
/// positional list every time a new flag is added (see #104 — it used to be 13 separate args
/// behind `#[allow(clippy::too_many_arguments)]`).
pub(crate) struct ReviewArgs<'a> {
    pub(crate) spec_path: &'a Path,
    pub(crate) diff_path: &'a Path,
    pub(crate) requirements_path: &'a Option<PathBuf>,
    pub(crate) conventions_path: &'a Option<PathBuf>,
    pub(crate) deterministic_results_path: &'a Option<PathBuf>,
    pub(crate) lenses_arg: &'a Option<String>,
    pub(crate) out: &'a Path,
    pub(crate) concurrency: usize,
    pub(crate) max_rounds: usize,
    pub(crate) prior: &'a Option<PathBuf>,
    pub(crate) human_voice: bool,
    pub(crate) lang: &'a Option<String>,
    /// #119: overall wall-clock budget across every remaining stage. None = no deadline
    /// (existing behavior). Checked between stages, not mid-call.
    pub(crate) deadline_minutes: Option<u64>,
    /// #122: skip the local secret scan's refuse-by-default behavior.
    pub(crate) allow_sensitive_input: bool,
}

/// True once `started.elapsed()` has passed `deadline_minutes` — always false when unset.
fn deadline_exceeded(started: std::time::Instant, deadline_minutes: Option<u64>) -> bool {
    match deadline_minutes {
        None => false,
        Some(m) => started.elapsed() > std::time::Duration::from_secs(m * 60),
    }
}

type LensReviewResults = (
    Vec<Result<(String, lens::LensOutput)>>,
    Option<Result<lens::GoodThingsOutput>>,
);

pub(crate) fn run_review(llm: &Llm, cheap_llm: &Llm, args: &ReviewArgs) -> Result<()> {
    let started = std::time::Instant::now();
    // #119: without this, --deadline-minutes only stopped new *stages* from starting — a call
    // already in flight could still run its full per-call timeout (600s) past the deadline.
    // Attaching the deadline to the Llm itself shrinks every individual call's own timeout to
    // whatever's actually left, so it's a real wall-clock bound, not just a between-stage check.
    let deadline_instant = args
        .deadline_minutes
        .map(|m| started + std::time::Duration::from_secs(m * 60));
    let llm = &llm.clone().with_deadline(deadline_instant);
    let cheap_llm = &cheap_llm.clone().with_deadline(deadline_instant);
    let sp = Spec::load(args.spec_path)?;
    let (mut inp, dropped_files) = input::normalize(
        args.diff_path,
        args.requirements_path,
        args.conventions_path,
        args.deterministic_results_path,
        args.lang.clone(),
    )?;
    enforce_secret_scan(&inp.diff, args.allow_sensitive_input)?;
    // Not a hard cap — since the full diff is resent on every lens/discourse/verify call, this
    // just gives an early warning that token cost grows more than linearly with diff size (no silent truncation).
    const DIFF_WARN_CHARS: usize = 300_000;
    if inp.diff.len() > DIFF_WARN_CHARS {
        eprintln!(
            "Warning: diff is {} characters (~{} tokens estimated), which is large — the full diff is resent on every lens review/discourse/requirements call, driving up token cost",
            inp.diff.len(),
            input::estimate_tokens(&inp.diff)
        );
    }
    if inp.deterministic_results.is_none() {
        if let Some(v) = semgrep::try_run(&inp.changed_files) {
            println!(
                "semgrep auto-detected — reflecting local run results in deterministic checks"
            );
            inp.deterministic_results = Some(v);
        }
    }
    let out_dir = prepare_out(args.out)?;

    let prior_state = match args.prior {
        None => None,
        Some(p) => Some(state::load(p)?),
    };
    let round = prior_state.as_ref().map(|s| s.round + 1).unwrap_or(1);

    println!(
        "Starting review (round {}) — {} ({} files, +{}/-{})",
        round,
        sp.name,
        inp.changed_files.len(),
        inp.added_lines,
        inp.removed_lines
    );

    // Steps 1-2 (input normalization, convention injection) are handled by input::normalize + each prompt builder.

    // Step 4: lens selection
    let optional_selected: Vec<String> = match args.lenses_arg {
        Some(s) => {
            // Filters out duplicate specifications ("--lenses design,design") — review_lens must
            // be called only once per lens so finding ids (position-based numbers) don't collide within it.
            let mut seen = std::collections::HashSet::new();
            let ids: Vec<String> = s
                .split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .filter(|x| seen.insert(x.clone()))
                .collect();
            for id in &ids {
                let lens = sp.lens_by_id(id);
                anyhow::ensure!(lens.is_some(), "lens id not found in spec: {id}");
                // #96: always lenses (e.g. good_things) are already added below and, for
                // good_things specifically, run through a dedicated review call with its own
                // schema — letting one in here via --lenses would run it a second time through
                // the generic defect-finding prompt, producing findings that pollute the score.
                anyhow::ensure!(
                    !lens.unwrap().always,
                    "{id} is an always lens and cannot be specified via --lenses (it's always included automatically)"
                );
            }
            ids
        }
        None => lens::select_lenses(cheap_llm, &sp, &inp)?,
    };
    let mut selected_ids: Vec<String> = optional_selected;
    for l in sp.always_lenses() {
        if l.id != "good_things" && !selected_ids.contains(&l.id) {
            selected_ids.push(l.id.clone());
        }
    }
    println!("Selected lenses: {}", selected_ids.join(", "));

    // Step 7: independent per-lens review (seal-then-reveal in sequence — equivalent to parallel
    // execution since results never reference each other).
    // Even if one lens fails (LLM call error, etc.), the remaining lens results are kept and the
    // failure is recorded in the report — avoids aborting the whole review without partial
    // results just because of one lens.
    // good_things is an independent LLM call that doesn't depend on findings (only needs
    // diff/spec), yet it used to run sequentially after all lens reviews finished — adding one
    // review's worth of round-trip time to the critical path for no real reason. Now it runs
    // concurrently with the lens par_map on a separate thread.
    let (lens_results, good_things_result): LensReviewResults = std::thread::scope(|s| {
        let lens_handle = s.spawn(|| {
            par_map(args.concurrency, selected_ids.clone(), |id| {
                let out = lens::review_lens(llm, &sp, &inp, &id, round)?;
                println!(
                    "  Lens complete: {} — {} findings, {} unverified",
                    id,
                    out.findings.len(),
                    out.unverified.len()
                );
                Ok((id, out))
            })
        });
        let good_things_handle = sp
            .lens_by_id("good_things")
            .is_some()
            .then(|| s.spawn(|| lens::review_good_things(cheap_llm, &sp, &inp)));

        // #113: par_map already isolates per-item worker panics into a Result — this thread
        // itself panicking (outside per-item processing) shouldn't be treated any differently
        // and take the whole CLI down with it. A single synthetic error entry composes with the
        // existing "for r in lens_results { ... Err(e) => stage_errors.push(...) }" loop below
        // exactly like a normal per-lens failure would.
        let lens_results: Vec<Result<(String, lens::LensOutput)>> = match lens_handle.join() {
            Ok(r) => r,
            Err(_) => vec![Err(anyhow::anyhow!("lens review thread panicked"))],
        };
        let good_things_result = good_things_handle.map(|h| {
            h.join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("good_things thread panicked")))
        });
        (lens_results, good_things_result)
    });

    let mut findings: Vec<Finding> = Vec::new();
    let mut unverified: Vec<(String, String)> = Vec::new();
    let mut stage_errors: Vec<String> = Vec::new();
    // #115 follow-up: tracked separately from findings.is_empty() — a clean diff with zero
    // real findings and "every lens errored out" both leave `findings` empty, but only the
    // latter means the review has zero defect-finding coverage (completeness::Failed below).
    let mut successful_lens_count = 0usize;
    for r in lens_results {
        match r {
            Ok((id, out)) => {
                successful_lens_count += 1;
                findings.extend(out.findings);
                for u in out.unverified {
                    unverified.push((id.clone(), u));
                }
            }
            Err(e) => {
                eprintln!("Warning: lens review failed — {e:#}");
                stage_errors.push(format!("{e:#}"));
            }
        }
    }
    // #123: local, deterministic check that each finding's file:line citation actually
    // corresponds to a line the diff shows — before discourse spends LLM calls debating
    // findings whose evidence may be hallucinated.
    evidence::verify(&mut findings, &inp.diff);

    // good_things is supplementary info that doesn't affect findings/score/verdict, so there's no
    // reason for its failure to discard the core review result entirely — just log a warning and continue with an empty list.
    let good_things = match good_things_result {
        Some(Ok(out)) => out.good_things,
        Some(Err(e)) => {
            eprintln!("Warning: good_things lens failed — {e:#}");
            stage_errors.push(format!("good_things: {e:#}"));
            Vec::new()
        }
        None => Vec::new(),
    };

    // Steps 8-9: discourse rounds
    // #117: discourse used to propagate failure via `?`, aborting the whole run (no report.md
    // at all) even though every lens result up to this point is otherwise usable. Falls back to
    // "nothing resolved" on error instead — every finding just stays effectively UNCERTAIN
    // (unresolved), so none of it can be wrongly CONFIRMED off a failed round; verdict/score
    // simply don't count anything from this stage, same fail-safe direction as a missing
    // resolution already gets treated everywhere else in this function.
    let (audit, mut resolved) = if findings.is_empty() {
        println!("No findings — skipping discourse");
        (Vec::new(), std::collections::HashMap::new())
    } else if deadline_exceeded(started, args.deadline_minutes) {
        eprintln!(
            "Warning: --deadline-minutes exceeded — skipping discourse and every stage after it"
        );
        stage_errors.push("discourse: skipped, --deadline-minutes exceeded".to_string());
        (Vec::new(), std::collections::HashMap::new())
    } else {
        println!("Starting discourse (up to {} rounds)", args.max_rounds);
        match discourse::run(llm, &sp, &inp, &mut findings, args.max_rounds, round) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Warning: discourse failed — {e:#}");
                stage_errors.push(format!("discourse: {e:#}"));
                (Vec::new(), std::collections::HashMap::new())
            }
        }
    };

    // Compared to the previous round (--prior): determine whether previously confirmed findings
    // were fixed in this diff. If STILL_OPEN, re-fold them into this round's working set (keeps affecting score/verdict).
    let mut fix_results: Vec<fixcheck::FixStatus> = Vec::new();
    if let Some(ps) = &prior_state {
        let prior_confirmed: Vec<Finding> = ps
            .findings
            .iter()
            .filter(|f| {
                ps.resolved
                    .get(&f.id)
                    .map(|r| r.status == "CONFIRMED")
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        // Give fixcheck the findings this round itself already CONFIRMED as reference material —
        // if a prior finding still hasn't been fixed but this round caught the same root cause
        // under a new id, it's judged SUPERSEDED so it isn't re-folded below (double counting).
        let this_round_confirmed: Vec<&Finding> = findings
            .iter()
            .filter(|f| {
                resolved
                    .get(&f.id)
                    .map(|r| r.status == "CONFIRMED")
                    .unwrap_or(false)
            })
            .collect();
        // #117: on failure, just skip re-folding for this round (fix_results stays empty)
        // instead of aborting the whole run via `?` — a --prior fixcheck failure shouldn't
        // discard every lens/discourse result already computed above.
        fix_results = if deadline_exceeded(started, args.deadline_minutes) {
            eprintln!("Warning: --deadline-minutes exceeded — skipping fix check");
            stage_errors.push("fixcheck: skipped, --deadline-minutes exceeded".to_string());
            Vec::new()
        } else {
            match fixcheck::run(
                cheap_llm,
                &sp,
                &inp,
                &prior_confirmed,
                &this_round_confirmed,
            ) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("Warning: fix check failed — {e:#}");
                    stage_errors.push(format!("fixcheck: {e:#}"));
                    Vec::new()
                }
            }
        };
        // Since re-folding runs after discourse::run (above), it doesn't go through this round's
        // discourse verification (intentional — fixcheck itself is responsible for judging
        // "still open"). Only guards against duplicate re-folding of the same id: discourse
        // SURFACE ids are now scoped by outer_round so there's no cross-round collision, but we
        // still defensively check the same id doesn't get added twice in this loop. SUPERSEDED
        // isn't STILL_OPEN to begin with, so it's naturally excluded from re-folding below
        // (residual limitation: doesn't catch cases where fixcheck's LLM judgment mislabels something as SUPERSEDED).
        for fr in &fix_results {
            if fr.status == "STILL_OPEN" {
                if let Some(orig) = prior_confirmed.iter().find(|f| f.id == fr.finding_id) {
                    if findings.iter().any(|f| f.id == orig.id) {
                        continue;
                    }
                    findings.push(orig.clone());
                    resolved.insert(
                        orig.id.clone(),
                        discourse::Resolution {
                            finding_id: orig.id.clone(),
                            status: "CONFIRMED".to_string(),
                            merged_into: String::new(),
                            reason: format!(
                                "Unresolved from previous round (re-confirmed): {}",
                                fr.evidence
                            ),
                        },
                    );
                }
            }
        }
    }

    // Step 6: policy lens (local, deterministic)
    let policies = policy::check_all(&sp, &inp);

    // Step 11: requirements verification
    let confirmed_refs: Vec<&Finding> = findings
        .iter()
        .filter(|f| resolved.get(&f.id).map(|r| r.status.as_str()) == Some("CONFIRMED"))
        .collect();
    // On failure, treated the same as "requirements not provided" (None), but recorded in
    // stage_errors so the two aren't conflated — since requirements factors into the verdict's
    // NEEDS_CONTEXT judgment, this beats silently letting it pass, but this single stage still
    // doesn't kill the whole review the way a lens failure would.
    let req_results = if deadline_exceeded(started, args.deadline_minutes) {
        eprintln!("Warning: --deadline-minutes exceeded — skipping requirements verification");
        stage_errors.push("requirements: skipped, --deadline-minutes exceeded".to_string());
        None
    } else {
        match requirements::verify(cheap_llm, &sp, &inp, &confirmed_refs) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Warning: requirements verification failed — {e:#}");
                stage_errors.push(format!("requirements: {e:#}"));
                None
            }
        }
    };

    // #117: human-voice doesn't read `quant` at all, so it can run before summarize() — moved
    // up from after it so a failure here (now caught instead of propagated via `?`) is already
    // reflected in stage_errors by the time summarize()/report::write need it, instead of
    // landing after the verdict was already computed.
    let hv = if !args.human_voice {
        None
    } else if deadline_exceeded(started, args.deadline_minutes) {
        eprintln!("Warning: --deadline-minutes exceeded — skipping human-voice rewrite");
        stage_errors.push("human_voice: skipped, --deadline-minutes exceeded".to_string());
        None
    } else {
        match humanvoice::rewrite(llm, &sp, &inp, &confirmed_refs, &good_things) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("Warning: human-voice rewrite failed — {e:#}");
                stage_errors.push(format!("human_voice: {e:#}"));
                None
            }
        }
    };

    // Step 10: quantitative summary + verdict
    let mut quant = quantify::summarize(
        &sp.scoring,
        &inp,
        &findings,
        &resolved,
        &policies,
        &req_results,
        selected_ids.len(),
    );
    // #115: every stage that could have failed by this point (lens/good_things/discourse/
    // fixcheck/requirements/human-voice) already recorded into stage_errors — this is the
    // single place that turns "did anything fail" into the structured signal on QuantSummary
    // itself, not just the rendered report.md text. Failed (every selected lens errored out,
    // zero defect-finding coverage) is distinct from Partial (some other stage failed but at
    // least one lens succeeded) — see ReviewCompleteness's doc comment.
    if !stage_errors.is_empty() {
        quant.completeness = if successful_lens_count == 0 && !selected_ids.is_empty() {
            quantify::ReviewCompleteness::Failed
        } else {
            quantify::ReviewCompleteness::Partial
        };
    }

    // Step 12: output
    let path = report::write(report::ReportCtx {
        out_dir: &out_dir,
        spec: &sp,
        input: &inp,
        selected_lenses: &selected_ids,
        round,
        findings: &findings,
        resolved: &resolved,
        unverified: &unverified,
        good_things: &good_things,
        policies: &policies,
        requirements: &req_results,
        audit: &audit,
        quant: &quant,
        fix_results: &fix_results,
        human_voice: hv.as_deref(),
        stage_errors: &stage_errors,
    })?;

    state::write(
        &out_dir,
        &state::State::new(round, findings.clone(), resolved.clone()),
    )?;

    // #129: best-effort — a manifest write failure shouldn't take down an otherwise-successful
    // review run (report.md/state.json already landed by this point).
    match manifest::build(
        args.spec_path,
        &sp.name,
        llm.model.clone(),
        cheap_llm.model.clone(),
        round,
        selected_ids.clone(),
        successful_lens_count,
        stage_errors.clone(),
        dropped_files,
        llm.usage(),
    )
    .and_then(|m| manifest::write(&out_dir, &m))
    {
        Ok(_) => {}
        Err(e) => eprintln!("Warning: failed to write manifest.json — {e:#}"),
    }

    println!(
        "\nDone — verdict={} score={}/100",
        quant.verdict, quant.score
    );
    println!("Report: {}", path.display());
    println!("Next round: --prior {}", out_dir.display());
    println!("{}", llm.usage().summary());
    Ok(())
}

/// A minimal E2E test verifying that the 12-step pipeline actually meshes together, without a
/// real API. Llm::fixture pulls responses out in call order, so it's only deterministic with
/// concurrency=1 (serial) — the scenario is minimized to match: 1 lens (always only, no
/// optional → the lens-selection LLM call itself is skipped), no good_things lens, no
/// requirements, no --prior, no human-voice, so exactly two LLM calls are needed (1 lens review + 1 discourse round).
#[cfg(test)]
mod e2e_tests {
    use super::*;
    use crate::llm::Llm;
    use std::io::Write as _;

    fn write_file(path: &std::path::Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn run_review_end_to_end_with_fixture_llm_produces_expected_report() {
        let dir = std::env::temp_dir().join("codereview-loop-e2e-review-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let spec_path = dir.join("spec.toml");
        write_file(
            &spec_path,
            r#"
name = "e2e test spec"
labels = ["possible bug"]

[[lenses]]
id = "test_lens"
title = "Test Lens"
guide = "test"
always = true
"#,
        );

        let diff_path = dir.join("diff.patch");
        write_file(
            &diff_path,
            "diff --git a/src/example.rs b/src/example.rs\n\
             --- a/src/example.rs\n\
             +++ b/src/example.rs\n\
             @@ -1,1 +1,1 @@\n\
             -old line\n\
             +new line\n",
        );

        let out_dir = dir.join("out");

        // 1) review_lens("test_lens", round=1) response — id can be left arbitrary since review_lens overwrites it.
        let lens_response = r#"{"findings":[{"file":"src/example.rs","line":"10","claim":"test claim","evidence":"test evidence","impact":"","severity":"P1","label":"possible bug","confidence":"high","recommendation":""}],"unverified":[]}"#.to_string();
        // 2) discourse round 1 response — must include a CHALLENGE, or an automatic re-request (3rd call) gets attached.
        //    the target id must match "test_lens-r1-1", the id review_lens actually assigns, for resolutions to take effect.
        let discourse_response = r#"{"moves":[{"move":"CHALLENGE","lens":"reviewer","target":"test_lens-r1-1","detail":"needs more evidence","new_evidence":"","confidence":"medium"}],"resolutions":[{"finding_id":"test_lens-r1-1","status":"CONFIRMED","merged_into":"","reason":"confirmed for e2e test"}],"surfaced":[]}"#.to_string();

        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(vec![lens_response, discourse_response], 0, usage.clone());
        let cheap_llm = llm.clone();

        run_review(
            &llm,
            &cheap_llm,
            &ReviewArgs {
                spec_path: &spec_path,
                diff_path: &diff_path,
                requirements_path: &None,
                conventions_path: &None,
                deterministic_results_path: &None,
                lenses_arg: &None,
                out: &out_dir,
                concurrency: 1, // forces the fixture queue order to match the call order
                max_rounds: 1,
                prior: &None,
                human_voice: false,
                lang: &None,
                deadline_minutes: None,
                allow_sensitive_input: false,
            },
        )
        .expect("run_review should complete end-to-end against the fixture LLM");

        let report =
            std::fs::read_to_string(out_dir.join("report.md")).expect("report.md should exist");
        assert!(
            report.contains("Verdict: COMMENT"),
            "expected COMMENT verdict (confirmed P1, no P0):\n{report}"
        );
        assert!(
            report.contains("Score: 88/100"),
            "expected score 88 (100 - 12 for one confirmed P1):\n{report}"
        );
        assert!(
            report.contains("test claim"),
            "expected the confirmed finding's claim to appear in the report:\n{report}"
        );
        assert!(
            std::fs::metadata(out_dir.join("state.json")).is_ok(),
            "state.json should be written for --prior to pick up next round"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_review_survives_a_discourse_failure_instead_of_aborting_the_whole_run() {
        // #117: discourse::run used to propagate via `?`, aborting run_review entirely (no
        // report.md at all) even though the lens review that ran just before it succeeded. A
        // malformed discourse response (invalid JSON) should now degrade to "nothing resolved"
        // and still produce a report — marked PARTIAL (#112/#115) since stage_errors is non-empty.
        let dir = std::env::temp_dir().join("codereview-loop-e2e-discourse-failure-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let spec_path = dir.join("spec.toml");
        write_file(
            &spec_path,
            r#"
name = "e2e test spec"
labels = ["possible bug"]

[[lenses]]
id = "test_lens"
title = "Test Lens"
guide = "test"
always = true
"#,
        );

        let diff_path = dir.join("diff.patch");
        write_file(
            &diff_path,
            "diff --git a/src/example.rs b/src/example.rs\n\
             --- a/src/example.rs\n\
             +++ b/src/example.rs\n\
             @@ -1,1 +1,1 @@\n\
             -old line\n\
             +new line\n",
        );

        let out_dir = dir.join("out");

        let lens_response = r#"{"findings":[{"file":"src/example.rs","line":"10","claim":"test claim","evidence":"test evidence","impact":"","severity":"P1","label":"possible bug","confidence":"high","recommendation":""}],"unverified":[]}"#.to_string();
        // Malformed on purpose — retries=0 below means json_ctx_typed fails after one attempt,
        // so discourse::run returns Err without consuming any further fixture responses.
        let broken_discourse_response = "this is not json".to_string();

        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(
            vec![lens_response, broken_discourse_response],
            0,
            usage.clone(),
        );
        let cheap_llm = llm.clone();

        run_review(
            &llm,
            &cheap_llm,
            &ReviewArgs {
                spec_path: &spec_path,
                diff_path: &diff_path,
                requirements_path: &None,
                conventions_path: &None,
                deterministic_results_path: &None,
                lenses_arg: &None,
                out: &out_dir,
                concurrency: 1,
                max_rounds: 1,
                prior: &None,
                human_voice: false,
                lang: &None,
                deadline_minutes: None,
                allow_sensitive_input: false,
            },
        )
        .expect("run_review must still succeed despite the discourse failure");

        let report =
            std::fs::read_to_string(out_dir.join("report.md")).expect("report.md should exist");
        assert!(
            report.contains("discourse"),
            "the discourse failure must be recorded in stage_errors:\n{report}"
        );
        let verdict_line = report
            .lines()
            .find(|l| l.starts_with("**Verdict:"))
            .unwrap();
        assert!(
            verdict_line.contains("PARTIAL"),
            "verdict line must be marked partial when discourse failed:\n{verdict_line}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_review_marks_the_report_failed_when_every_selected_lens_errors_out() {
        // #115 follow-up: distinct from the discourse-failure test above — here the *lens*
        // review itself fails (malformed response), so there's zero defect-finding coverage at
        // all. The report must say FAILED, not the more forgiving PARTIAL a supplementary-stage
        // failure gets.
        let dir = std::env::temp_dir().join("codereview-loop-e2e-all-lenses-failed-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let spec_path = dir.join("spec.toml");
        write_file(
            &spec_path,
            r#"
name = "e2e test spec"
labels = ["possible bug"]

[[lenses]]
id = "test_lens"
title = "Test Lens"
guide = "test"
always = true
"#,
        );

        let diff_path = dir.join("diff.patch");
        write_file(
            &diff_path,
            "diff --git a/src/example.rs b/src/example.rs\n\
             --- a/src/example.rs\n\
             +++ b/src/example.rs\n\
             @@ -1,1 +1,1 @@\n\
             -old line\n\
             +new line\n",
        );

        let out_dir = dir.join("out");
        // Malformed on purpose — the single selected lens's only call fails, findings stays
        // empty, and (no findings) skips discourse entirely, so this is the only fixture entry needed.
        let broken_lens_response = "this is not json".to_string();

        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(vec![broken_lens_response], 0, usage.clone());
        let cheap_llm = llm.clone();

        run_review(
            &llm,
            &cheap_llm,
            &ReviewArgs {
                spec_path: &spec_path,
                diff_path: &diff_path,
                requirements_path: &None,
                conventions_path: &None,
                deterministic_results_path: &None,
                lenses_arg: &None,
                out: &out_dir,
                concurrency: 1,
                max_rounds: 1,
                prior: &None,
                human_voice: false,
                lang: &None,
                deadline_minutes: None,
                allow_sensitive_input: false,
            },
        )
        .expect("run_review must still succeed even when every lens fails");

        let report =
            std::fs::read_to_string(out_dir.join("report.md")).expect("report.md should exist");
        let verdict_line = report
            .lines()
            .find(|l| l.starts_with("**Verdict:"))
            .unwrap();
        assert!(
            verdict_line.contains("(FAILED"),
            "verdict line must say FAILED when every lens errored out, not just PARTIAL:\n{verdict_line}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_review_skips_discourse_once_deadline_minutes_is_exceeded() {
        // #119: an overall deadline must actually stop remaining stages from starting, not just
        // exist as an unused flag. deadline_minutes: Some(0) is exceeded almost immediately, so
        // discourse must be skipped even though findings exist — proven by the fixture queue
        // having only the lens response: if discourse::run were still called, Llm::fixture would
        // panic with "fixture response queue is empty".
        let dir = std::env::temp_dir().join("codereview-loop-e2e-deadline-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let spec_path = dir.join("spec.toml");
        write_file(
            &spec_path,
            r#"
name = "e2e test spec"
labels = ["possible bug"]

[[lenses]]
id = "test_lens"
title = "Test Lens"
guide = "test"
always = true
"#,
        );

        let diff_path = dir.join("diff.patch");
        write_file(
            &diff_path,
            "diff --git a/src/example.rs b/src/example.rs\n\
             --- a/src/example.rs\n\
             +++ b/src/example.rs\n\
             @@ -1,1 +1,1 @@\n\
             -old line\n\
             +new line\n",
        );

        let out_dir = dir.join("out");
        let lens_response = r#"{"findings":[{"file":"src/example.rs","line":"10","claim":"test claim","evidence":"test evidence","impact":"","severity":"P1","label":"possible bug","confidence":"high","recommendation":""}],"unverified":[]}"#.to_string();

        let usage = Llm::new_usage_tracker();
        // Only one response queued — if discourse ran anyway, the fixture would panic
        // ("more LLM calls than expected") instead of run_review returning cleanly.
        let llm = Llm::fixture(vec![lens_response], 0, usage.clone());
        let cheap_llm = llm.clone();

        run_review(
            &llm,
            &cheap_llm,
            &ReviewArgs {
                spec_path: &spec_path,
                diff_path: &diff_path,
                requirements_path: &None,
                conventions_path: &None,
                deterministic_results_path: &None,
                lenses_arg: &None,
                out: &out_dir,
                concurrency: 1,
                max_rounds: 1,
                prior: &None,
                human_voice: false,
                lang: &None,
                deadline_minutes: Some(0),
                allow_sensitive_input: false,
            },
        )
        .expect("run_review must still succeed when the deadline is already exceeded");

        let report =
            std::fs::read_to_string(out_dir.join("report.md")).expect("report.md should exist");
        assert!(
            report.contains("deadline"),
            "the deadline skip must be recorded in stage_errors:\n{report}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_review_rejects_always_lens_in_manual_lenses_arg() {
        // #96: before this check, manually passing an always lens id (e.g. good_things) via
        // --lenses slipped past validation and got reviewed a second time through the generic
        // defect-finding prompt, on top of its own dedicated call — polluting findings/score.
        let dir = std::env::temp_dir().join("codereview-loop-e2e-always-lens-reject-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let spec_path = dir.join("spec.toml");
        write_file(
            &spec_path,
            r#"
name = "e2e test spec"
labels = ["possible bug"]

[[lenses]]
id = "good_things"
title = "Good Things"
guide = "test"
always = true
"#,
        );

        let diff_path = dir.join("diff.patch");
        write_file(
            &diff_path,
            "diff --git a/src/example.rs b/src/example.rs\n\
             --- a/src/example.rs\n\
             +++ b/src/example.rs\n\
             @@ -1,1 +1,1 @@\n\
             -old line\n\
             +new line\n",
        );

        let out_dir = dir.join("out");
        let usage = Llm::new_usage_tracker();
        // Empty fixture: validation must fail before any LLM call is made.
        let llm = Llm::fixture(vec![], 0, usage.clone());
        let cheap_llm = llm.clone();

        let err = run_review(
            &llm,
            &cheap_llm,
            &ReviewArgs {
                spec_path: &spec_path,
                diff_path: &diff_path,
                requirements_path: &None,
                conventions_path: &None,
                deterministic_results_path: &None,
                lenses_arg: &Some("good_things".to_string()),
                out: &out_dir,
                concurrency: 1,
                max_rounds: 1,
                prior: &None,
                human_voice: false,
                lang: &None,
                deadline_minutes: None,
                allow_sensitive_input: false,
            },
        )
        .expect_err("manually selecting an always lens must be rejected");
        assert!(
            err.to_string().contains("always"),
            "expected an always-lens rejection error, got: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
