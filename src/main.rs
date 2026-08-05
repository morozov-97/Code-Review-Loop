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
    /// claude -p 서브프로세스
    Claude,
    /// OpenRouter REST API (OPENROUTER_API_KEY 필요)
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
    /// 렌즈 선정·good things·요구사항 검증·fix check 등 단순 판정 단계에 쓸 저비용 모델.
    /// 미지정 시 --model과 동일(기존 동작 유지).
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
    /// 렌즈별 독립 리뷰 + discourse 교차검증(기본 파이프라인)
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
        /// 렌즈 수동 지정(콤마 구분). 미지정 시 LLM이 diff 성격 보고 선정.
        #[arg(long)]
        lenses: Option<String>,
        #[arg(long, default_value = "runs")]
        out: PathBuf,
        /// 렌즈별 리뷰(review_lens)는 서로 독립이라 병렬 실행 가능 — 기본값을 3으로 둬서
        /// (선정 렌즈 1~3개 + always 렌즈 1개 규모에 맞춤) 기본 실행이 직렬로 도는 걸 피한다.
        #[arg(long, default_value_t = 3)]
        concurrency: usize,
        /// discourse 최대 라운드 수
        #[arg(long, default_value_t = 2)]
        max_rounds: usize,
        /// 이전 라운드 --out 디렉터리(state.json). 지정 시 이전 확정 finding의 FIXED/STILL_OPEN 판정 추가.
        #[arg(long)]
        prior: Option<PathBuf>,
        /// 확정 findings·good things를 사람 리뷰 코멘트 톤으로 재작성해 리포트에 첨부
        #[arg(long)]
        human_voice: bool,
    },
    /// PR 제목·요약·walkthrough·라벨·분리 가능 여부 + TODO 스캔
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
    /// 구체적 코드 개선안(diff 스니펫 기반)
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

/// (본 모델, 저비용 모델) 쌍. `--cheap-model` 미지정 시 저비용 모델은 본 모델과 동일해
/// 기존 동작을 그대로 유지한다. 둘 다 하나의 usage tracker를 공유해 합산 사용량을 낸다.
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
    // 하드 상한은 아님 — 렌즈/discourse/verify 호출마다 diff 전체가 재전송되므로
    // 큰 diff일수록 토큰 비용이 선형 이상으로 커진다는 것만 미리 알린다(silent truncation 없음).
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

    // 1~2단계(입력 정규화·컨벤션 주입)는 input::normalize + 각 프롬프트 빌더에서 처리됨.

    // 4단계: 렌즈 선정
    let optional_selected: Vec<String> = match lenses_arg {
        Some(s) => {
            // 중복 지정("--lenses design,design")을 걸러낸다 — review_lens는 렌즈별로
            // 한 번만 불려야 finding id(위치 기반 번호)가 그 안에서 서로 겹치지 않는다.
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

    // 7단계: 렌즈별 독립 리뷰(봉인 후 순차 공개 — 병렬 실행해도 서로 결과를 참조하지 않으므로 동일).
    // 렌즈 하나가 실패해도(LLM 호출 에러 등) 나머지 렌즈 결과는 살리고, 실패는 리포트에
    // 남긴다 — 렌즈 하나 때문에 리뷰 전체가 부분결과 없이 중단되는 걸 피한다.
    let lens_results: Vec<Result<(String, lens::LensOutput)>> =
        par_map(concurrency, selected_ids.clone(), |id| {
            let out = lens::review_lens(llm, &sp, &inp, &id, round)?;
            println!(
                "  렌즈 완료: {} — finding {}건, 미검증 {}건",
                id,
                out.findings.len(),
                out.unverified.len()
            );
            Ok((id, out))
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

    // good_things는 findings/score/verdict에 영향 없는 부가 정보라, 실패해도 핵심 리뷰 결과를
    // 통째로 날릴 이유가 없다 — 경고만 남기고 빈 목록으로 계속 진행한다.
    let good_things = if sp.lens_by_id("good_things").is_some() {
        match lens::review_good_things(cheap_llm, &sp, &inp) {
            Ok(out) => out.good_things,
            Err(e) => {
                eprintln!("경고: good_things 렌즈 실패 — {e:#}");
                stage_errors.push(format!("good_things: {e:#}"));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    // 8~9단계: discourse 라운드
    let (audit, mut resolved) = if findings.is_empty() {
        println!("finding 없음 — discourse 생략");
        (Vec::new(), std::collections::HashMap::new())
    } else {
        println!("discourse 시작 (최대 {}라운드)", max_rounds);
        discourse::run(llm, &sp, &mut findings, max_rounds)?
    };

    // 이전 라운드(--prior) 대비: 확정됐던 finding이 이번 diff에서 고쳐졌는지 판정.
    // STILL_OPEN이면 이번 라운드 작업셋에 재편입(score/verdict에 계속 반영).
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
        fix_results = fixcheck::run(cheap_llm, &sp, &inp, &prior_confirmed)?;
        for fr in &fix_results {
            if fr.status == "STILL_OPEN" {
                if let Some(orig) = prior_confirmed.iter().find(|f| f.id == fr.finding_id) {
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

    // 6단계: 정책 렌즈(로컬 결정론)
    let policies = policy::check_all(&sp, &inp);

    // 11단계: 요구사항 검증
    let confirmed_refs: Vec<&Finding> = findings
        .iter()
        .filter(|f| resolved.get(&f.id).map(|r| r.status.as_str()) == Some("CONFIRMED"))
        .collect();
    // 실패 시 "요구사항 미제공"(None)과 같은 취급으로 넘어가되, 그 둘을 헷갈리지 않도록
    // stage_errors에 남긴다 — requirements가 verdict의 NEEDS_CONTEXT 판정에 관여하므로
    // 조용히 통과시키는 것보다야 낫지만, 렌즈 실패처럼 이 단계 하나로 리뷰 전체를 죽이진 않는다.
    let req_results = match requirements::verify(cheap_llm, &sp, &inp, &confirmed_refs) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("경고: requirements 검증 실패 — {e:#}");
            stage_errors.push(format!("requirements: {e:#}"));
            None
        }
    };

    // 10단계: 정량 요약 + verdict
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

    // 12단계: 출력
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

/// concurrency 만큼 스레드를 묶어 순차 실행(청크 단위 배리어).
/// 항목별 결과를 개별 Result로 모은다(하나 실패해도 나머지는 계속 처리) — 호출부가
/// 부분 실패를 그대로 무시할지, 에러만 걸러내 계속 진행할지 결정한다. 과거엔 첫 실패에서
/// 전체를 중단시켰는데, 렌즈 리뷰처럼 서로 독립적인 항목엔 과함(다른 렌즈까지 다 날아감).
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

/// 실제 API 없이 12단계 파이프라인이 실제로 맞물려 도는지 확인하는 최소 E2E 테스트.
/// Llm::fixture는 호출 순서대로 응답을 꺼내므로 concurrency=1(직렬)로만 결정적이다 —
/// 시나리오도 그에 맞춰 렌즈 1개(always만, optional 없음 → 렌즈 선정 LLM 호출 자체가
/// 생략됨)·good_things 렌즈 없음·requirements 없음·--prior 없음·human-voice 없음으로
/// 최소화해서, 정확히 두 번의 LLM 호출(렌즈 리뷰 1회 + discourse 1라운드)만 필요하게 짰다.
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

        // 1) review_lens("test_lens", round=1) 응답 — id는 review_lens가 덮어쓰므로 임의값으로 둬도 됨.
        let lens_response = r#"{"findings":[{"file":"src/example.rs","line":"10","claim":"test claim","evidence":"test evidence","impact":"","severity":"P1","label":"possible bug","confidence":"high","recommendation":""}],"unverified":[]}"#.to_string();
        // 2) discourse round 1 응답 — CHALLENGE를 포함해야 자동 재요청(3번째 호출)이 안 붙는다.
        //    target id는 review_lens가 실제로 부여하는 "test_lens-r1-1"과 맞춰야 resolutions가 먹는다.
        let discourse_response = r#"{"moves":[{"move":"CHALLENGE","lens":"reviewer","target":"test_lens-r1-1","detail":"needs more evidence","new_evidence":"","confidence":"medium"}],"resolutions":[{"finding_id":"test_lens-r1-1","status":"CONFIRMED","merged_into":"","reason":"confirmed for e2e test"}],"surfaced":[]}"#.to_string();

        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(vec![lens_response, discourse_response], 0, usage.clone());
        let cheap_llm = llm.clone();

        run_review(
            &llm, &cheap_llm, &spec_path, &diff_path, &None, &None, &None, &None, &out_dir,
            1, // concurrency=1: fixture 큐 순서가 곧 호출 순서가 되도록 강제
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
