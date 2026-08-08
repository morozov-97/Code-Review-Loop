use crate::cargo_audit;
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

/// #164: folds a newly-arrived deterministic tool's result object into whatever's accumulated so
/// far, key by key — so semgrep's `sast`/`secrets` entries and cargo-audit's `dependency_sca`
/// entry coexist in one object instead of the second tool to finish clobbering the first.
/// `quantify::deterministic_gate` only cares that each entry has a `status` field, not which
/// top-level key it lives under, so any two tools contributing disjoint keys just merge cleanly.
fn merge_deterministic_results(
    existing: &mut Option<serde_json::Value>,
    incoming: serde_json::Value,
) {
    match existing {
        None => *existing = Some(incoming),
        Some(current) => {
            if let (Some(current_obj), Some(incoming_obj)) =
                (current.as_object_mut(), incoming.as_object())
            {
                for (k, v) in incoming_obj {
                    current_obj.insert(k.clone(), v.clone());
                }
            }
        }
    }
}

/// True once `started.elapsed()` has passed `deadline_minutes` — always false when unset.
fn deadline_exceeded(started: std::time::Instant, deadline_minutes: Option<u64>) -> bool {
    match deadline_minutes {
        None => false,
        Some(m) => started.elapsed() > std::time::Duration::from_secs(m * 60),
    }
}

/// #169/#164: caps a deterministic tool's own default timeout at whatever's left of the overall
/// --deadline-minutes budget, mirroring Llm::effective_timeout's same base.min(remaining).max(floor)
/// shape — a background semgrep/cargo-audit run shouldn't be able to eat the whole deadline on
/// its own.
fn deterministic_tool_timeout(
    base: std::time::Duration,
    deadline: Option<std::time::Instant>,
) -> std::time::Duration {
    match deadline {
        None => base,
        Some(d) => base
            .min(d.saturating_duration_since(std::time::Instant::now()))
            .max(std::time::Duration::from_secs(1)),
    }
}

type LensReviewResults = Vec<Result<(String, lens::LensOutput)>>;

/// #162: a lens with a `model` override in its spec runs on a clone of `llm` with just that
/// field swapped — same provider/gate/usage tracker/calls_log, different model id. Returns
/// `None` when no override applies (the overwhelmingly common case), so callers can fall back
/// to the shared `llm` reference without an allocation.
fn lens_specific_llm(llm: &Llm, sp: &Spec, id: &str) -> Option<Llm> {
    let lens = sp.lenses.iter().find(|l| l.id == id)?;
    let model = lens.model.as_ref()?;
    if llm.model.as_ref() == Some(model) {
        return None;
    }
    let mut overridden = llm.clone();
    overridden.model = Some(model.clone());
    Some(overridden)
}

/// Shared by both the mandatory-lens and optional-lens `par_map` calls (#168) — kept as one
/// plain fn instead of a closure defined twice so the "lens complete" logging can't drift
/// between the two call sites.
fn review_one_lens(
    llm: &Llm,
    sp: &Spec,
    inp: &input::Input,
    id: &str,
    round: usize,
) -> Result<(String, lens::LensOutput)> {
    let overridden = lens_specific_llm(llm, sp, id);
    let effective_llm = overridden.as_ref().unwrap_or(llm);
    let out = lens::review_lens(effective_llm, sp, inp, id, round)?;
    println!(
        "  Lens complete: {} — {} findings, {} unverified",
        id,
        out.findings.len(),
        out.unverified.len()
    );
    Ok((id.to_string(), out))
}

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
    enforce_secret_scan(&inp, args.allow_sensitive_input)?;
    // Not a hard cap — since the full diff (plus requirements/conventions) is resent on every
    // lens/discourse/verify call, this just gives an early warning that token cost grows more
    // than linearly with input size (no silent truncation). #142: requirements/conventions used
    // to be excluded from this warning entirely, despite being sent to the LLM the same way and
    // having no size cap of their own (unlike the diff, which prioritize_and_cap_diff bounds).
    const DIFF_WARN_CHARS: usize = 300_000;
    let requirements_len = inp.requirements.as_deref().map(str::len).unwrap_or(0);
    let conventions_len = inp.conventions.as_deref().map(str::len).unwrap_or(0);
    let total_context_len = inp.diff.len() + requirements_len + conventions_len;
    if total_context_len > DIFF_WARN_CHARS {
        let est_tokens = input::estimate_tokens(&inp.diff)
            + input::estimate_tokens(inp.requirements.as_deref().unwrap_or(""))
            + input::estimate_tokens(inp.conventions.as_deref().unwrap_or(""));
        eprintln!(
            "Warning: diff+requirements+conventions is {total_context_len} characters total \
             (diff {}, requirements {requirements_len}, conventions {conventions_len}; \
             ~{est_tokens} tokens estimated), which is large — the full diff is resent on every \
             lens review/discourse/requirements call, driving up token cost",
            inp.diff.len()
        );
    }
    // #169: semgrep used to run synchronously right here, blocking lens selection on a
    // subprocess whose result nothing in the LLM context even reads (shared_context never
    // includes deterministic_results — only quantify::deterministic_gate does, at the very end
    // of this function). Spawned in the background instead; joined just before that gate needs
    // it, so it overlaps with everything from lens review through human-voice.
    let semgrep_started = std::time::Instant::now();
    let semgrep_handle = inp.deterministic_results.is_none().then(|| {
        let changed_files = inp.changed_files.clone();
        let timeout = deterministic_tool_timeout(semgrep::DEFAULT_TIMEOUT, deadline_instant);
        std::thread::spawn(move || semgrep::try_run(&changed_files, timeout))
    });
    // #164: a second deterministic source, run concurrently with semgrep for the same reason
    // (nothing in the LLM context reads it before the deterministic gate at the very end) —
    // gated on the same is_none() check, since an externally-supplied --deterministic-results
    // means neither auto-detected tool should run or override what the caller passed in.
    let cargo_audit_started = std::time::Instant::now();
    let cargo_audit_handle = inp.deterministic_results.is_none().then(|| {
        let timeout = deterministic_tool_timeout(cargo_audit::DEFAULT_TIMEOUT, deadline_instant);
        std::thread::spawn(move || cargo_audit::try_run(timeout))
    });
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

    // Step 4: lens selection. Manual --lenses validation is synchronous (no LLM call) and must
    // fail before anything else starts — so it's resolved eagerly here, before the thread::scope
    // below, rather than inside it (spawning the mandatory-lens par_map before knowing whether
    // manual validation even passed would mean an invalid --lenses value still burns real LLM
    // calls before the error is returned).
    let manual_ids: Option<Vec<String>> = match args.lenses_arg {
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
                // #96: always lenses are already added below and run unconditionally — letting
                // one in here via --lenses would run it a second time through the generic
                // defect-finding prompt, producing duplicate findings that pollute the score.
                anyhow::ensure!(
                    !lens.unwrap().always,
                    "{id} is an always lens and cannot be specified via --lenses (it's always included automatically)"
                );
            }
            Some(ids)
        }
        None => None,
    };
    let mandatory_ids: Vec<String> = sp.always_lenses().iter().map(|l| l.id.clone()).collect();

    // Step 7: independent per-lens review (seal-then-reveal in sequence — equivalent to parallel
    // execution since results never reference each other).
    // Even if one lens fails (LLM call error, etc.), the remaining lens results are kept and the
    // failure is recorded in the report — avoids aborting the whole review without partial
    // results just because of one lens.
    //
    // #168: when --lenses isn't given, automatic lens selection used to be an unconditional
    // barrier before any lens review started — even though the mandatory (always) lenses don't
    // depend on its result at all. Running selection on its own thread alongside the
    // mandatory-lens par_map hides that round trip behind work that has to happen anyway.
    let lens_stage_started = std::time::Instant::now();
    let (optional_selected, mandatory_results): (Result<Vec<String>>, LensReviewResults) =
        std::thread::scope(|s| {
            let selection_handle = manual_ids
                .is_none()
                .then(|| s.spawn(|| lens::select_lenses(cheap_llm, &sp, &inp)));
            let mandatory_handle = s.spawn(|| {
                par_map(args.concurrency, mandatory_ids.clone(), |id| {
                    review_one_lens(llm, &sp, &inp, &id, round)
                })
            });

            let optional_selected: Result<Vec<String>> = match manual_ids {
                Some(ids) => Ok(ids),
                None => selection_handle
                    .unwrap()
                    .join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("lens selection thread panicked"))),
            };
            let mandatory_results = mandatory_handle
                .join()
                .unwrap_or_else(|_| vec![Err(anyhow::anyhow!("lens review thread panicked"))]);
            (optional_selected, mandatory_results)
        });
    let optional_selected = optional_selected?;

    let mut selected_ids: Vec<String> = optional_selected.clone();
    for id in &mandatory_ids {
        if !selected_ids.contains(id) {
            selected_ids.push(id.clone());
        }
    }
    println!("Selected lenses: {}", selected_ids.join(", "));

    let optional_results: LensReviewResults = if optional_selected.is_empty() {
        Vec::new()
    } else {
        par_map(args.concurrency, optional_selected, |id| {
            review_one_lens(llm, &sp, &inp, &id, round)
        })
    };
    let lens_selection_and_review_ms = lens_stage_started.elapsed().as_millis();
    let lens_results: LensReviewResults = mandatory_results
        .into_iter()
        .chain(optional_results)
        .collect();

    let mut findings: Vec<Finding> = Vec::new();
    let mut unverified: Vec<(String, String)> = Vec::new();
    let mut stage_errors: Vec<String> = Vec::new();
    // #115 follow-up: tracked separately from findings.is_empty() — a clean diff with zero
    // real findings and "every lens errored out" both leave `findings` empty, but only the
    // latter means the review has zero defect-finding coverage (completeness::Failed below).
    let mut successful_lens_count = 0usize;
    // #174: good_things used to come from a fully separate always-on lens/call joined on its own
    // thread — it's now just whichever lens is lens::GOOD_THINGS_HOST_LENS's own `good_things`
    // field, extracted in the same pass as findings/unverified below.
    let mut good_things: Vec<lens::GoodThing> = Vec::new();
    for r in lens_results {
        match r {
            Ok((id, out)) => {
                // #158: a response of literally `{}` (no findings, no unverified, no summary)
                // used to count toward successful_lens_count exactly the same as a thorough
                // review that happened to find nothing — is_degenerate distinguishes them via
                // the summary field, which the prompt now requires even on a clean result.
                if out.is_degenerate() {
                    eprintln!(
                        "Warning: lens {id} returned an empty response (no findings, no unverified items, no summary) — not counted as a completed review"
                    );
                    stage_errors.push(format!(
                        "{id}: empty lens response (no findings/unverified/summary)"
                    ));
                } else {
                    successful_lens_count += 1;
                }
                if id == lens::GOOD_THINGS_HOST_LENS {
                    good_things = out.good_things.clone();
                }
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

    // Steps 8-9: discourse rounds
    // #117: discourse used to propagate failure via `?`, aborting the whole run (no report.md
    // at all) even though every lens result up to this point is otherwise usable. Falls back to
    // "nothing resolved" on error instead — every finding just stays effectively UNCERTAIN
    // (unresolved), so none of it can be wrongly CONFIRMED off a failed round; verdict/score
    // simply don't count anything from this stage, same fail-safe direction as a missing
    // resolution already gets treated everywhere else in this function.
    let discourse_started = std::time::Instant::now();
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
    let discourse_ms = discourse_started.elapsed().as_millis();
    // #139: discourse's SURFACE moves can add brand-new findings straight into `findings`
    // (src/discourse/mod.rs) without ever going through evidence::verify — the earlier call at
    // the top of this function only saw the original per-lens findings. Re-running here is
    // idempotent for findings already checked and catches every SURFACEd one too.
    evidence::verify(&mut findings, &inp.diff);

    // Compared to the previous round (--prior): determine whether previously confirmed findings
    // were fixed in this diff. If STILL_OPEN, re-fold them into this round's working set (keeps affecting score/verdict).
    let fixcheck_started = std::time::Instant::now();
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
        // #117/#135: on failure or a deadline skip, don't just drop fix_results to empty — that
        // used to mean the re-fold loop below (which only re-adds STILL_OPEN entries) silently
        // dropped every prior-round CONFIRMED finding from this round's score/verdict instead of
        // erring toward "still open, keep counting it." fill_missing_as_still_open (normally
        // just fixcheck::run's own "the LLM omitted this id" safety net) does exactly that
        // synthesis when handed an empty results list — every prior_confirmed id is "missing."
        fix_results = if deadline_exceeded(started, args.deadline_minutes) {
            eprintln!("Warning: --deadline-minutes exceeded — skipping fix check");
            stage_errors.push("fixcheck: skipped, --deadline-minutes exceeded".to_string());
            fixcheck::fill_missing_as_still_open(Vec::new(), &prior_confirmed)
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
                    fixcheck::fill_missing_as_still_open(Vec::new(), &prior_confirmed)
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
        //
        // #135: UNKNOWN re-folds the same way as STILL_OPEN — corroborate() (fixcheck.rs)
        // downgrades a wrongly-FIXED verdict to UNKNOWN specifically to protect a finding whose
        // evidence is still present verbatim, but UNKNOWN wasn't previously re-folded at all, so
        // that protection ended in the same silent drop it was built to prevent. "We don't know
        // it's fixed" must mean "keep counting it," same as an explicit STILL_OPEN.
        for fr in &fix_results {
            if fr.status == "STILL_OPEN" || fr.status == "UNKNOWN" {
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
    let fixcheck_ms = fixcheck_started.elapsed().as_millis();

    // Step 6: policy lens (local, deterministic)
    let policies = policy::check_all(&sp, &inp);

    // Step 11: requirements verification
    let confirmed_refs: Vec<&Finding> = findings
        .iter()
        .filter(|f| resolved.get(&f.id).map(|r| r.status.as_str()) == Some("CONFIRMED"))
        .collect();

    // #170: requirements verification and human-voice rewrite don't read each other's output —
    // human-voice doesn't read `quant` either (#117: that's why it can run before summarize()
    // below), and requirements doesn't read human-voice's rewritten text. They only share
    // confirmed_refs/good_things, both already available here. Run them concurrently instead of
    // one after the other.
    //
    // On failure (or a deadline skip), each is treated the same as "not provided"/"not
    // rewritten" (None), but recorded in stage_errors so the two aren't conflated — since
    // requirements factors into the verdict's NEEDS_CONTEXT judgment, this beats silently
    // letting it pass, but neither stage kills the whole review the way a lens failure would.
    let req_hv_started = std::time::Instant::now();
    let (req_results, hv): (Option<Vec<requirements::RequirementCheck>>, Option<String>) =
        std::thread::scope(|s| {
            let req_handle = s.spawn(|| {
                if deadline_exceeded(started, args.deadline_minutes) {
                    return Err("skipped, --deadline-minutes exceeded".to_string());
                }
                requirements::verify(cheap_llm, &sp, &inp, &confirmed_refs)
                    .map_err(|e| format!("{e:#}"))
            });
            let hv_handle = args.human_voice.then(|| {
                s.spawn(|| {
                    if deadline_exceeded(started, args.deadline_minutes) {
                        return Err("skipped, --deadline-minutes exceeded".to_string());
                    }
                    humanvoice::rewrite(llm, &sp, &inp, &confirmed_refs, &good_things)
                        .map_err(|e| format!("{e:#}"))
                })
            });

            let req_results = match req_handle
                .join()
                .unwrap_or_else(|_| Err("requirements thread panicked".to_string()))
            {
                Ok(r) => r,
                Err(msg) => {
                    eprintln!("Warning: requirements verification failed — {msg}");
                    stage_errors.push(format!("requirements: {msg}"));
                    None
                }
            };
            let hv = hv_handle.and_then(|h| {
                match h
                    .join()
                    .unwrap_or_else(|_| Err("human_voice thread panicked".to_string()))
                {
                    Ok(v) => Some(v),
                    Err(msg) => {
                        eprintln!("Warning: human-voice rewrite failed — {msg}");
                        stage_errors.push(format!("human_voice: {msg}"));
                        None
                    }
                }
            });
            (req_results, hv)
        });
    let requirements_and_human_voice_ms = req_hv_started.elapsed().as_millis();

    // #169: this is the earliest point that actually needs deterministic_results (quantify's
    // deterministic gate, right below) — join the background semgrep run here instead of
    // blocking on it before lens selection even started.
    let semgrep_ms = semgrep_handle.map(|handle| {
        match handle.join() {
            Ok(Some(v)) => {
                println!(
                    "semgrep auto-detected — reflecting local run results in deterministic checks"
                );
                merge_deterministic_results(&mut inp.deterministic_results, v);
            }
            Ok(None) => {}
            Err(_) => {
                eprintln!("Warning: semgrep background thread panicked");
                stage_errors.push("semgrep: background thread panicked".to_string());
            }
        }
        semgrep_started.elapsed().as_millis()
    });
    // #164: joined right alongside semgrep — both were spawned at the same point and neither
    // blocks lens selection/review, so joining them back to back here doesn't add wall-clock
    // beyond whichever of the two ran longer.
    let cargo_audit_ms = cargo_audit_handle.map(|handle| {
        match handle.join() {
            Ok(Some(v)) => {
                println!(
                    "cargo audit auto-detected — reflecting local run results in deterministic checks"
                );
                merge_deterministic_results(&mut inp.deterministic_results, v);
            }
            Ok(None) => {}
            Err(_) => {
                eprintln!("Warning: cargo audit background thread panicked");
                stage_errors.push("cargo_audit: background thread panicked".to_string());
            }
        }
        cargo_audit_started.elapsed().as_millis()
    });

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
        manifest::StageTimings {
            semgrep_ms,
            cargo_audit_ms,
            lens_selection_and_review_ms,
            discourse_ms,
            fixcheck_ms,
            requirements_and_human_voice_ms,
            total_ms: started.elapsed().as_millis(),
        },
        llm.calls(),
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

    fn spec_with_lens_models(dir: &std::path::Path) -> Spec {
        let spec_path = dir.join("model-override-spec.toml");
        write_file(
            &spec_path,
            r#"
name = "model override test spec"
labels = ["possible bug"]

[[lenses]]
id = "no_override"
title = "No Override"
always = true

[[lenses]]
id = "same_as_shared"
title = "Same As Shared"
always = true
model = "shared-model"

[[lenses]]
id = "different_model"
title = "Different Model"
always = true
model = "diverse-model"
"#,
        );
        Spec::load(&spec_path).unwrap()
    }

    #[test]
    fn lens_specific_llm_returns_none_for_a_lens_with_no_model_field() {
        let dir = std::env::temp_dir().join("codereview-loop-lens-specific-llm-test-1");
        std::fs::create_dir_all(&dir).unwrap();
        let sp = spec_with_lens_models(&dir);
        let mut llm = Llm::fixture(vec![], 0, Llm::new_usage_tracker());
        llm.model = Some("shared-model".to_string());

        assert!(lens_specific_llm(&llm, &sp, "no_override").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lens_specific_llm_returns_none_when_the_override_matches_the_shared_model() {
        let dir = std::env::temp_dir().join("codereview-loop-lens-specific-llm-test-2");
        std::fs::create_dir_all(&dir).unwrap();
        let sp = spec_with_lens_models(&dir);
        let mut llm = Llm::fixture(vec![], 0, Llm::new_usage_tracker());
        llm.model = Some("shared-model".to_string());

        assert!(lens_specific_llm(&llm, &sp, "same_as_shared").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lens_specific_llm_returns_an_overridden_clone_when_the_lens_names_a_different_model() {
        let dir = std::env::temp_dir().join("codereview-loop-lens-specific-llm-test-3");
        std::fs::create_dir_all(&dir).unwrap();
        let sp = spec_with_lens_models(&dir);
        let mut llm = Llm::fixture(vec![], 0, Llm::new_usage_tracker());
        llm.model = Some("shared-model".to_string());

        let overridden = lens_specific_llm(&llm, &sp, "different_model").unwrap();
        assert_eq!(overridden.model.as_deref(), Some("diverse-model"));
        // Everything else about the lens's own shared client (usage tracker, gate, etc.) still
        // comes along via the clone — only `model` diverges from `llm`.
        assert_eq!(overridden.retries, llm.retries);
        let _ = std::fs::remove_dir_all(&dir);
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
        //    line "1" is deliberately verifiable against the diff below (evidence::verify), and
        //    #148 now requires a CONFIRMED resolution to be backed by both a real vote AND a
        //    verified citation, not just the LLM's say-so.
        let lens_response = r#"{"findings":[{"file":"src/example.rs","line":"1","claim":"test claim","evidence":"test evidence","impact":"","severity":"P1","label":"possible bug","confidence":"high","recommendation":""}],"unverified":[]}"#.to_string();
        // 2) discourse round 1 response — must include a CHALLENGE, or an automatic re-request (3rd call) gets attached.
        //    the target id must match "test_lens-r1-1", the id review_lens actually assigns, for resolutions to take effect.
        //    A real AGREE (with new_evidence) is required alongside the CHALLENGE so the stated
        //    CONFIRMED resolution actually clears VOTE_THRESHOLD (#148) — the CHALLENGE here is
        //    severity-axis, so it doesn't offset that AGREE's vote weight.
        let discourse_response = r#"{"moves":[{"move":"CHALLENGE","lens":"reviewer","target":"test_lens-r1-1","detail":"severity may be overstated","new_evidence":"","confidence":"medium","challenge_axis":"severity"},{"move":"AGREE","lens":"other-reviewer","target":"test_lens-r1-1","detail":"confirmed independently","new_evidence":"corroborating evidence","confidence":"high"}],"resolutions":[{"finding_id":"test_lens-r1-1","status":"CONFIRMED","merged_into":"","reason":"confirmed for e2e test"}],"surfaced":[]}"#.to_string();

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
    fn run_review_does_not_confirm_off_an_llm_stated_resolution_alone() {
        // #148 repro at the full pipeline level: a finding whose citation isn't verifiable
        // (line 10 doesn't exist in this 1-line diff), challenged but never AGREEd by anyone,
        // used to still end up CONFIRMED (and scored) purely because the discourse response's
        // "resolutions" array said so — this is now blocked.
        let dir = std::env::temp_dir().join("codereview-loop-e2e-no-bare-confirm-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let spec_path = dir.join("spec.toml");
        write_file(
            &spec_path,
            "name = \"e2e test spec\"\nlabels = [\"possible bug\"]\n\n[[lenses]]\nid = \"test_lens\"\ntitle = \"Test Lens\"\nguide = \"test\"\nalways = true\n",
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
                concurrency: 1,
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
            !report.contains("Score: 88/100"),
            "an unbacked, unverified CONFIRMED must not be scored as if it were real:\n{report}"
        );
        assert!(
            report.contains("Score: 100/100"),
            "with no genuinely confirmed finding, score should stay at 100:\n{report}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_review_flags_a_discourse_surfaced_finding_whose_file_line_does_not_exist_in_the_diff() {
        // #139: evidence::verify used to only run once, before discourse — a finding added via
        // a SURFACE move never got checked at all and rendered as if it had passed verification.
        let dir = std::env::temp_dir().join("codereview-loop-e2e-surface-evidence-test");
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

        let lens_response = r#"{"findings":[{"file":"src/example.rs","line":"1","claim":"real claim","evidence":"real evidence","impact":"","severity":"P1","label":"possible bug","confidence":"high","recommendation":""}],"unverified":[]}"#.to_string();
        // "surfaced" finding cites line 999, which doesn't exist anywhere in the 3-line diff
        // above — nothing in "resolutions" targets it, so it stays UNCERTAIN and shows up in
        // the "Needs Human Review" table, where the evidence_unverified marker is checked.
        let discourse_response = r#"{"moves":[{"move":"CHALLENGE","lens":"reviewer","target":"test_lens-r1-1","detail":"needs more evidence","new_evidence":"","confidence":"medium"}],"resolutions":[],"surfaced":[{"file":"src/example.rs","line":"999","claim":"hallucinated claim","evidence":"hallucinated evidence","impact":"","severity":"P2","label":"possible bug","confidence":"medium","recommendation":""}]}"#.to_string();

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
                concurrency: 1,
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
            report.contains("hallucinated claim"),
            "the surfaced finding should appear in the report:\n{report}"
        );
        assert!(
            report.contains("src/example.rs:999 ⚠️ unverified"),
            "the surfaced finding's fabricated line must be flagged unverified:\n{report}"
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
    fn run_review_does_not_count_a_degenerate_empty_lens_response_as_successful() {
        // #158: a lens response of literally `{}` parses without error — no missing-field
        // failure, no malformed JSON — but it's indistinguishable from a genuinely clean,
        // thoroughly-reviewed diff unless something in the response says the LLM actually
        // engaged with it. Must get the same FAILED treatment as an outright parse failure.
        let dir = std::env::temp_dir().join("codereview-loop-e2e-degenerate-lens-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let spec_path = dir.join("spec.toml");
        write_file(
            &spec_path,
            "name = \"e2e test spec\"\nlabels = [\"possible bug\"]\n\n[[lenses]]\nid = \"test_lens\"\ntitle = \"Test Lens\"\nguide = \"test\"\nalways = true\n",
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
        let degenerate_lens_response = "{}".to_string();

        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(vec![degenerate_lens_response], 0, usage.clone());
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
        .expect("run_review must still succeed on a degenerate lens response");

        let report =
            std::fs::read_to_string(out_dir.join("report.md")).expect("report.md should exist");
        let verdict_line = report
            .lines()
            .find(|l| l.starts_with("**Verdict:"))
            .unwrap();
        assert!(
            verdict_line.contains("(FAILED"),
            "a degenerate {{}} response must not count as a completed review:\n{verdict_line}"
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

    fn prior_confirmed_p0_finding() -> Finding {
        Finding {
            id: "prior-r1-1".to_string(),
            file: "src/example.rs".to_string(),
            line: "1".to_string(),
            claim: "prior round SQL injection".to_string(),
            evidence: "prior evidence".to_string(),
            impact: String::new(),
            severity: "P0".to_string(),
            label: "security".to_string(),
            confidence: "high".to_string(),
            recommendation: String::new(),
            lens: "security".to_string(),
            reviewer: "Reviewer".to_string(),
            evidence_unverified: false,
        }
    }

    fn write_prior_state(dir: &std::path::Path, finding: Finding) -> std::path::PathBuf {
        let prior_dir = dir.join("prior");
        std::fs::create_dir_all(&prior_dir).unwrap();
        let mut resolved = std::collections::HashMap::new();
        resolved.insert(
            finding.id.clone(),
            discourse::Resolution {
                finding_id: finding.id.clone(),
                status: "CONFIRMED".to_string(),
                merged_into: String::new(),
                reason: "confirmed in round 1".to_string(),
            },
        );
        state::write(&prior_dir, &state::State::new(1, vec![finding], resolved)).unwrap();
        prior_dir
    }

    #[test]
    fn run_review_still_carries_a_prior_confirmed_finding_when_fixcheck_itself_fails() {
        // #135: a --prior fixcheck call that fails outright (LLM error, here simulated by an
        // exhausted fixture queue) used to leave fix_results empty, silently dropping every
        // prior-round CONFIRMED finding from this round's score/verdict instead of erring
        // toward "still open, keep counting it."
        let dir = std::env::temp_dir().join("codereview-loop-e2e-prior-fixcheck-failure-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let spec_path = dir.join("spec.toml");
        write_file(
            &spec_path,
            "name = \"e2e test spec\"\nlabels = [\"possible bug\"]\n\n[[lenses]]\nid = \"test_lens\"\ntitle = \"Test Lens\"\nguide = \"test\"\nalways = true\n",
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
        let prior_dir = write_prior_state(&dir, prior_confirmed_p0_finding());
        let out_dir = dir.join("out");

        // Only one response queued — enough for round 2's lens call (no new findings, so
        // discourse gets skipped entirely: "No findings — skipping discourse"). By the time
        // fixcheck::run() tries its own LLM call, the fixture queue is empty, so it fails.
        let lens_response = r#"{"findings":[],"unverified":[]}"#.to_string();
        let usage = Llm::new_usage_tracker();
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
                prior: &Some(prior_dir),
                human_voice: false,
                lang: &None,
                deadline_minutes: None,
                allow_sensitive_input: false,
            },
        )
        .expect("run_review should complete even though fixcheck itself fails");

        let report =
            std::fs::read_to_string(out_dir.join("report.md")).expect("report.md should exist");
        assert!(
            report.contains("prior round SQL injection"),
            "the prior-round CONFIRMED P0 must still be carried into this round:\n{report}"
        );
        assert!(
            report.contains("Verdict: REQUEST_CHANGES"),
            "a carried-over confirmed P0 must still drive the verdict:\n{report}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_review_still_carries_a_prior_confirmed_finding_when_the_deadline_is_already_exceeded() {
        // #135: the --deadline-minutes skip path had the same bug as the outright-failure path
        // — fix_results stayed empty, so a --deadline-minutes run always silently dropped every
        // prior-round CONFIRMED finding, deadline or not.
        let dir = std::env::temp_dir().join("codereview-loop-e2e-prior-deadline-skip-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let spec_path = dir.join("spec.toml");
        write_file(
            &spec_path,
            "name = \"e2e test spec\"\nlabels = [\"possible bug\"]\n\n[[lenses]]\nid = \"test_lens\"\ntitle = \"Test Lens\"\nguide = \"test\"\nalways = true\n",
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
        let prior_dir = write_prior_state(&dir, prior_confirmed_p0_finding());
        let out_dir = dir.join("out");

        let lens_response = r#"{"findings":[],"unverified":[]}"#.to_string();
        let usage = Llm::new_usage_tracker();
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
                prior: &Some(prior_dir),
                human_voice: false,
                lang: &None,
                // Already exceeded by the time fixcheck would run.
                deadline_minutes: Some(0),
                allow_sensitive_input: false,
            },
        )
        .expect("run_review should complete even with the deadline already exceeded");

        let report =
            std::fs::read_to_string(out_dir.join("report.md")).expect("report.md should exist");
        assert!(
            report.contains("prior round SQL injection"),
            "the prior-round CONFIRMED P0 must still be carried into this round:\n{report}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_review_still_carries_a_prior_confirmed_finding_downgraded_to_unknown() {
        // #135: a finding whose original evidence is still present verbatim in the diff must
        // downgrade to UNKNOWN — but UNKNOWN wasn't previously re-folded (only STILL_OPEN was),
        // so this safety net produced the exact silent drop it was built to prevent.
        // #174: this downgrade is now resolved locally (fixcheck::locally_resolvable) before
        // ever reaching the LLM — see the fixture below, which only queues the round-2 lens
        // call's response, not a fixcheck one.
        let dir = std::env::temp_dir().join("codereview-loop-e2e-prior-unknown-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let spec_path = dir.join("spec.toml");
        write_file(
            &spec_path,
            "name = \"e2e test spec\"\nlabels = [\"possible bug\"]\n\n[[lenses]]\nid = \"test_lens\"\ntitle = \"Test Lens\"\nguide = \"test\"\nalways = true\n",
        );
        let diff_path = dir.join("diff.patch");
        // The added line deliberately still contains the prior finding's evidence text
        // verbatim, so corroborate() downgrades a FIXED verdict to UNKNOWN.
        write_file(
            &diff_path,
            "diff --git a/src/example.rs b/src/example.rs\n\
             --- a/src/example.rs\n\
             +++ b/src/example.rs\n\
             @@ -1,1 +1,1 @@\n\
             -old line\n\
             +still has prior evidence right here\n",
        );
        let prior_dir = write_prior_state(&dir, prior_confirmed_p0_finding());
        let out_dir = dir.join("out");

        let lens_response = r#"{"findings":[],"unverified":[]}"#.to_string();
        let usage = Llm::new_usage_tracker();
        // Only the round-2 lens response is queued — if fixcheck called the LLM instead of
        // resolving this finding locally, the empty queue would error instead of run_review
        // completing successfully.
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
                prior: &Some(prior_dir),
                human_voice: false,
                lang: &None,
                deadline_minutes: None,
                allow_sensitive_input: false,
            },
        )
        .expect("run_review should complete");

        let report =
            std::fs::read_to_string(out_dir.join("report.md")).expect("report.md should exist");
        assert!(
            report.contains("prior round SQL injection"),
            "a finding downgraded to UNKNOWN must still be carried into this round:\n{report}"
        );
        assert!(
            report.contains("Verdict: REQUEST_CHANGES"),
            "the carried-over P0 must still drive the verdict:\n{report}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
