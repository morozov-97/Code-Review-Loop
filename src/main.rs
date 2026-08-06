mod describe;
mod discourse;
mod fixcheck;
mod humanvoice;
mod improve;
mod input;
mod lens;
mod llm;
mod policy;
mod promptctx;
mod quantify;
mod report;
mod requirements;
mod semgrep;
mod spec;
mod state;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use lens::Finding;
use llm::Llm;
use spec::Spec;
use std::path::PathBuf;

#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
enum Backend {
    /// claude -p subprocess
    Claude,
    /// OpenRouter REST API (requires OPENROUTER_API_KEY)
    Openrouter,
}

#[derive(Parser, Debug)]
#[command(
    name = "codereview",
    version,
    about = "다각도(멀티 렌즈) 코드 리뷰 파이프라인 — 렌즈별 독립 리뷰 후 discourse로 교차검증"
)]
struct Cli {
    #[arg(long, default_value = "claude", global = true)]
    claude_bin: String,
    #[arg(long, value_enum, default_value = "claude", global = true)]
    backend: Backend,
    #[arg(long, global = true)]
    model: Option<String>,
    /// Low-cost model used for simple judgment stages like lens selection, good things,
    /// requirements verification, fix check, etc. Defaults to --model when unset (preserves existing behavior).
    #[arg(long, global = true)]
    cheap_model: Option<String>,
    #[arg(long, default_value_t = 2, global = true)]
    retries: u32,
    #[arg(long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Independent per-lens review + discourse cross-verification (default pipeline)
    Review {
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        diff: PathBuf,
        #[arg(long)]
        requirements: Option<PathBuf>,
        #[arg(long)]
        conventions: Option<PathBuf>,
        #[arg(long)]
        deterministic_results: Option<PathBuf>,
        /// Manually specify lenses (comma-separated). If unset, the LLM picks based on the diff's nature.
        #[arg(long)]
        lenses: Option<String>,
        #[arg(long, default_value = "runs")]
        out: PathBuf,
        /// Per-lens reviews (review_lens) are independent of each other and can run in parallel —
        /// default is 3 (sized for 1-3 selected lenses + 1 always lens) to avoid running serially by default.
        #[arg(long, default_value_t = 3)]
        concurrency: usize,
        /// Maximum number of discourse rounds
        #[arg(long, default_value_t = 2)]
        max_rounds: usize,
        /// Previous round's --out directory (state.json). When set, adds FIXED/STILL_OPEN verdicts for previously confirmed findings.
        #[arg(long)]
        prior: Option<PathBuf>,
        /// Rewrite confirmed findings/good things in a human reviewer comment tone and attach to the report
        #[arg(long)]
        human_voice: bool,
    },
    /// PR title/summary/walkthrough/labels/splittability + TODO scan
    Describe {
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        diff: PathBuf,
        #[arg(long)]
        requirements: Option<PathBuf>,
        #[arg(long)]
        conventions: Option<PathBuf>,
        #[arg(long, default_value = "runs")]
        out: PathBuf,
    },
    /// Concrete code improvement suggestions (based on diff snippets)
    Improve {
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        diff: PathBuf,
        #[arg(long)]
        requirements: Option<PathBuf>,
        #[arg(long)]
        conventions: Option<PathBuf>,
        #[arg(long, default_value = "runs")]
        out: PathBuf,
    },
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("에러: {e:#}");
        std::process::exit(1);
    }
}

/// A (main model, cheap model) pair. If `--cheap-model` isn't specified, the cheap model is the
/// same as the main model, preserving existing behavior. Both share a single usage tracker to produce combined usage totals.
fn build_llm(cli: &Cli) -> Result<(Llm, Llm)> {
    let usage = Llm::new_usage_tracker();
    let cheap_model = cli.cheap_model.clone().or_else(|| cli.model.clone());
    let (main_llm, cheap_llm) = match cli.backend {
        Backend::Claude => (
            Llm::claude_cli(
                cli.claude_bin.clone(),
                cli.model.clone(),
                cli.retries,
                cli.verbose,
                usage.clone(),
            ),
            Llm::claude_cli(
                cli.claude_bin.clone(),
                cheap_model,
                cli.retries,
                cli.verbose,
                usage.clone(),
            ),
        ),
        Backend::Openrouter => (
            Llm::openrouter(cli.model.clone(), cli.retries, cli.verbose, usage.clone())?,
            Llm::openrouter(cheap_model, cli.retries, cli.verbose, usage.clone())?,
        ),
    };
    Ok((main_llm, cheap_llm))
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let (llm, cheap_llm) = build_llm(&cli)?;

    match &cli.cmd {
        Cmd::Review {
            spec,
            diff,
            requirements,
            conventions,
            deterministic_results,
            lenses,
            out,
            concurrency,
            max_rounds,
            prior,
            human_voice,
        } => run_review(
            &llm,
            &cheap_llm,
            spec,
            diff,
            requirements,
            conventions,
            deterministic_results,
            lenses,
            out,
            *concurrency,
            *max_rounds,
            prior,
            *human_voice,
        ),
        Cmd::Describe {
            spec,
            diff,
            requirements,
            conventions,
            out,
        } => run_describe(&llm, spec, diff, requirements, conventions, out),
        Cmd::Improve {
            spec,
            diff,
            requirements,
            conventions,
            out,
        } => run_improve(&llm, spec, diff, requirements, conventions, out),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_review(
    llm: &Llm,
    cheap_llm: &Llm,
    spec_path: &PathBuf,
    diff_path: &PathBuf,
    requirements_path: &Option<PathBuf>,
    conventions_path: &Option<PathBuf>,
    deterministic_results_path: &Option<PathBuf>,
    lenses_arg: &Option<String>,
    out: &PathBuf,
    concurrency: usize,
    max_rounds: usize,
    prior: &Option<PathBuf>,
    human_voice: bool,
) -> Result<()> {
    let sp = Spec::load(spec_path)?;
    let mut inp = input::normalize(
        diff_path,
        requirements_path,
        conventions_path,
        deterministic_results_path,
    )?;
    // Not a hard cap — since the full diff is resent on every lens/discourse/verify call, this
    // just gives an early warning that token cost grows more than linearly with diff size (no silent truncation).
    const DIFF_WARN_CHARS: usize = 300_000;
    if inp.diff.len() > DIFF_WARN_CHARS {
        eprintln!(
            "경고: diff가 {}자로 큼 — 렌즈별 리뷰·discourse·requirements 호출마다 전체가 재전송되어 토큰 비용이 커짐",
            inp.diff.len()
        );
    }
    if inp.deterministic_results.is_none() {
        if let Some(v) = semgrep::try_run(&inp.changed_files) {
            println!("semgrep 자동 감지 — 로컬 실행 결과를 deterministic checks에 반영");
            inp.deterministic_results = Some(v);
        }
    }
    let out_dir = prepare_out(out)?;

    let prior_state = match prior {
        None => None,
        Some(p) => Some(state::load(p)?),
    };
    let round = prior_state.as_ref().map(|s| s.round + 1).unwrap_or(1);

    println!(
        "리뷰 시작(round {}) — {} ({}개 파일, +{}/-{})",
        round,
        sp.name,
        inp.changed_files.len(),
        inp.added_lines,
        inp.removed_lines
    );

    // Steps 1-2 (input normalization, convention injection) are handled by input::normalize + each prompt builder.

    // Step 4: lens selection
    let optional_selected: Vec<String> = match lenses_arg {
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
                anyhow::ensure!(sp.lens_by_id(id).is_some(), "spec에 없는 렌즈 id: {id}");
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
    println!("선정 렌즈: {}", selected_ids.join(", "));

    // Step 7: independent per-lens review (seal-then-reveal in sequence — equivalent to parallel
    // execution since results never reference each other).
    // Even if one lens fails (LLM call error, etc.), the remaining lens results are kept and the
    // failure is recorded in the report — avoids aborting the whole review without partial
    // results just because of one lens.
    // good_things is an independent LLM call that doesn't depend on findings (only needs
    // diff/spec), yet it used to run sequentially after all lens reviews finished — adding one
    // review's worth of round-trip time to the critical path for no real reason. Now it runs
    // concurrently with the lens par_map on a separate thread.
    let (lens_results, good_things_result): (
        Vec<Result<(String, lens::LensOutput)>>,
        Option<Result<lens::GoodThingsOutput>>,
    ) = std::thread::scope(|s| {
        let lens_handle = s.spawn(|| {
            par_map(concurrency, selected_ids.clone(), |id| {
                let out = lens::review_lens(llm, &sp, &inp, &id, round)?;
                println!(
                    "  렌즈 완료: {} — finding {}건, 미검증 {}건",
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

        let lens_results = lens_handle.join().expect("렌즈 리뷰 스레드 panic");
        let good_things_result =
            good_things_handle.map(|h| h.join().expect("good_things 스레드 panic"));
        (lens_results, good_things_result)
    });

    let mut findings: Vec<Finding> = Vec::new();
    let mut unverified: Vec<(String, String)> = Vec::new();
    let mut stage_errors: Vec<String> = Vec::new();
    for r in lens_results {
        match r {
            Ok((id, out)) => {
                findings.extend(out.findings);
                for u in out.unverified {
                    unverified.push((id.clone(), u));
                }
            }
            Err(e) => {
                eprintln!("경고: 렌즈 리뷰 실패 — {e:#}");
                stage_errors.push(format!("{e:#}"));
            }
        }
    }

    // good_things is supplementary info that doesn't affect findings/score/verdict, so there's no
    // reason for its failure to discard the core review result entirely — just log a warning and continue with an empty list.
    let good_things = match good_things_result {
        Some(Ok(out)) => out.good_things,
        Some(Err(e)) => {
            eprintln!("경고: good_things 렌즈 실패 — {e:#}");
            stage_errors.push(format!("good_things: {e:#}"));
            Vec::new()
        }
        None => Vec::new(),
    };

    // Steps 8-9: discourse rounds
    let (audit, mut resolved) = if findings.is_empty() {
        println!("finding 없음 — discourse 생략");
        (Vec::new(), std::collections::HashMap::new())
    } else {
        println!("discourse 시작 (최대 {}라운드)", max_rounds);
        discourse::run(llm, &sp, &inp, &mut findings, max_rounds, round)?
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
        fix_results = fixcheck::run(
            cheap_llm,
            &sp,
            &inp,
            &prior_confirmed,
            &this_round_confirmed,
        )?;
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
                            reason: format!("이전 라운드 미해결(재확인): {}", fr.evidence),
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
    let req_results = match requirements::verify(cheap_llm, &sp, &inp, &confirmed_refs) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("경고: requirements 검증 실패 — {e:#}");
            stage_errors.push(format!("requirements: {e:#}"));
            None
        }
    };

    // Step 10: quantitative summary + verdict
    let quant = quantify::summarize(
        &inp,
        &findings,
        &resolved,
        &policies,
        &req_results,
        selected_ids.len(),
    );

    let hv = if human_voice {
        Some(humanvoice::rewrite(
            llm,
            &sp,
            &inp,
            &confirmed_refs,
            &good_things,
        )?)
    } else {
        None
    };

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

    println!(
        "\n종료 — verdict={} score={}/100",
        quant.verdict, quant.score
    );
    println!("리포트: {}", path.display());
    println!("다음 라운드: --prior {}", out_dir.display());
    println!("{}", llm.usage().summary());
    Ok(())
}

fn run_describe(
    llm: &Llm,
    spec_path: &PathBuf,
    diff_path: &PathBuf,
    requirements_path: &Option<PathBuf>,
    conventions_path: &Option<PathBuf>,
    out: &PathBuf,
) -> Result<()> {
    let sp = Spec::load(spec_path)?;
    let inp = input::normalize(diff_path, requirements_path, conventions_path, &None)?;
    let out_dir = prepare_out(out)?;
    let d = describe::run(llm, &sp, &inp)?;
    let todos = describe::todo_sections(&inp.diff);
    let path = report::write_describe(&out_dir, &d, &todos)?;
    println!("describe 완료: {}", path.display());
    println!("{}", llm.usage().summary());
    Ok(())
}

fn run_improve(
    llm: &Llm,
    spec_path: &PathBuf,
    diff_path: &PathBuf,
    requirements_path: &Option<PathBuf>,
    conventions_path: &Option<PathBuf>,
    out: &PathBuf,
) -> Result<()> {
    let sp = Spec::load(spec_path)?;
    let inp = input::normalize(diff_path, requirements_path, conventions_path, &None)?;
    let out_dir = prepare_out(out)?;
    let suggestions = improve::run(llm, &sp, &inp)?;
    let path = report::write_improve(&out_dir, &suggestions)?;
    println!(
        "improve 완료: 제안 {}건 — {}",
        suggestions.len(),
        path.display()
    );
    println!("{}", llm.usage().summary());
    Ok(())
}

fn prepare_out(p: &PathBuf) -> Result<PathBuf> {
    std::fs::create_dir_all(p)
        .with_context(|| format!("출력 디렉터리 생성 실패: {}", p.display()))?;
    Ok(p.clone())
}

/// Groups threads by concurrency and runs them in sequence (chunk-wise barrier).
/// Collects each item's result as an individual Result (processing continues even if one fails) —
/// the caller decides whether to ignore partial failures as-is or filter out errors and continue.
/// It used to abort everything on the first failure, which is excessive for independent items like lens reviews (would wipe out the other lenses too).
fn par_map<T, R, F>(concurrency: usize, items: Vec<T>, f: F) -> Vec<Result<R>>
where
    T: Send,
    R: Send,
    F: Fn(T) -> Result<R> + Sync,
{
    let c = concurrency.max(1);
    let mut out: Vec<Result<R>> = Vec::new();
    let mut rest = items;
    while !rest.is_empty() {
        let take = c.min(rest.len());
        let chunk: Vec<T> = rest.drain(..take).collect();
        let mut results: Vec<Result<R>> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk.into_iter().map(|item| s.spawn(|| f(item))).collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .map_err(|_| anyhow!("worker thread panicked"))
                        .and_then(|r| r)
                })
                .collect()
        });
        out.append(&mut results);
    }
    out
}

#[cfg(test)]
mod par_map_tests {
    use super::*;

    #[test]
    fn par_map_keeps_successful_results_when_one_item_fails() {
        let items = vec![1, 2, 3];
        let results = par_map(2, items, |i| {
            if i == 2 {
                Err(anyhow!("boom on {i}"))
            } else {
                Ok(i * 10)
            }
        });
        assert_eq!(results.len(), 3);
        let (oks, errs): (Vec<_>, Vec<_>) = results.into_iter().partition(Result::is_ok);
        assert_eq!(oks.len(), 2);
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn par_map_preserves_all_successes_when_nothing_fails() {
        let items = vec![1, 2, 3, 4, 5];
        let results = par_map(3, items, |i| Ok::<_, anyhow::Error>(i * 2));
        let values: Vec<i32> = results.into_iter().map(|r| r.unwrap()).collect();
        let mut sorted = values.clone();
        sorted.sort();
        assert_eq!(sorted, vec![2, 4, 6, 8, 10]);
    }
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
            &llm, &cheap_llm, &spec_path, &diff_path, &None, &None, &None, &None, &out_dir,
            1, // concurrency=1: forces the fixture queue order to match the call order
            1, // max_rounds
            &None, false,
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
}
