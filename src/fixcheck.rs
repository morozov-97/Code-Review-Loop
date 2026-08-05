use crate::input::Input;
use crate::lens::Finding;
use crate::llm::Llm;
use crate::promptctx::shared_context;
use crate::spec::Spec;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const FIXCHECK_SYSTEM: &str =
    "당신은 이전 라운드에서 확정된 finding이 이번 diff에서 실제로 고쳐졌는지 판정한다. \
근거 없이 FIXED로 판정하지 않는다. 확인 불가하면 UNKNOWN. 반드시 지정된 JSON 스키마로만 응답한다.";

/// 필드 전부 `#[serde(default)]` — discourse::Move/Resolution과 동일 이유. status가
/// 빠지거나 스키마 밖 값이면 "UNKNOWN"(사람이 다시 봐야 함)으로 안전하게 떨어진다.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixStatus {
    #[serde(default)]
    pub finding_id: String,
    #[serde(default = "unknown_status")]
    pub status: String, // FIXED|STILL_OPEN|UNKNOWN
    #[serde(default)]
    pub evidence: String,
}

fn unknown_status() -> String {
    "UNKNOWN".to_string()
}

const VALID_FIX_STATUSES: [&str; 3] = ["FIXED", "STILL_OPEN", "UNKNOWN"];

/// discourse::Resolution/requirements::normalize_status와 동일 문제: main.rs/report.rs가
/// status를 정확 문자열 매칭하므로, 대소문자·공백이 어긋나면 STILL_OPEN 재편입도,
/// "이전 라운드 대비" 표시도 조용히 빠진다. 실패는 UNKNOWN(사람이 다시 봐야 함)으로.
fn normalize_status(raw: &str) -> String {
    let upper = raw.trim().to_ascii_uppercase();
    if VALID_FIX_STATUSES.contains(&upper.as_str()) {
        upper
    } else {
        "UNKNOWN".to_string()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FixCheckOutput {
    #[serde(default)]
    results: Vec<FixStatus>,
}

/// finding의 원래 evidence 문자열이 새 diff에 그대로(문자 그대로) 남아있는지 본다.
/// 이게 참인데 LLM이 FIXED라고 판정했다면 — 지적됐던 근거가 안 바뀐 채로 남아있다는
/// 뜻이라 LLM 판정이 틀렸을 가능성이 높다. evidence가 비어있으면 판단 근거가 없으니
/// LLM 판정을 그대로 둔다(과도한 재정의 방지).
fn evidence_still_present(evidence: &str, diff: &str) -> bool {
    let evidence = evidence.trim();
    !evidence.is_empty() && diff.contains(evidence)
}

/// FIXED 판정을 LLM 혼자 내리게 두지 않는다 — 원래 evidence가 diff에 문자 그대로
/// 남아있는데도 FIXED로 나왔다면 UNKNOWN으로 낮춰 사람이 다시 보게 한다(실패는 항상
/// 더 엄격한 방향으로, 조용히 STILL_OPEN을 놓치는 방향으로 새면 안 된다).
fn corroborate(
    mut results: Vec<FixStatus>,
    prior_confirmed: &[Finding],
    diff: &str,
) -> Vec<FixStatus> {
    for r in results.iter_mut() {
        if r.status != "FIXED" {
            continue;
        }
        let Some(orig) = prior_confirmed.iter().find(|f| f.id == r.finding_id) else {
            continue;
        };
        if evidence_still_present(&orig.evidence, diff) {
            r.evidence = format!(
                "{} [결정적 재검증: 원래 evidence가 새 diff에 그대로 남아있어 FIXED 판정을 UNKNOWN으로 낮춤]",
                r.evidence
            );
            r.status = "UNKNOWN".to_string();
        }
    }
    results
}

/// LLM이 finding_id를 결과 배열에서 그냥 언급하지 않고 넘어가는 경우(FIXED라고 명시하지도
/// 않았지만 STILL_OPEN/UNKNOWN으로도 안 나옴)를 "판정 누락"이 아니라 암묵적 "고쳐짐"으로
/// 취급하면 안 된다 — main.rs의 재편입 루프는 결과 배열에 있는 항목만 보므로, 언급 자체가
/// 없으면 이전에 CONFIRMED였던 P0/P1이 점수·리포트에서 조용히 사라진다. 실패는 항상
/// STILL_OPEN(더 엄격한 방향, 사람이 다시 봄)으로 나야 한다.
fn fill_missing_as_still_open(
    mut results: Vec<FixStatus>,
    prior_confirmed: &[Finding],
) -> Vec<FixStatus> {
    for f in prior_confirmed {
        if !results.iter().any(|r| r.finding_id == f.id) {
            results.push(FixStatus {
                finding_id: f.id.clone(),
                status: "STILL_OPEN".to_string(),
                evidence:
                    "fix check 응답에 이 finding_id가 없었음(누락) — 안전하게 STILL_OPEN 처리"
                        .to_string(),
            });
        }
    }
    results
}

/// prior_confirmed 비어있으면 빈 결과(라운드 1이거나 이전에 확정 finding 없음).
pub fn run(
    llm: &Llm,
    spec: &Spec,
    input: &Input,
    prior_confirmed: &[Finding],
) -> Result<Vec<FixStatus>> {
    if prior_confirmed.is_empty() {
        return Ok(Vec::new());
    }
    let list = prior_confirmed
        .iter()
        .map(|f| {
            format!(
                "- id={} | {}:{} | {}\n  근거: {}",
                f.id, f.file, f.line, f.claim, f.evidence
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let ctx = shared_context(spec, input);
    let task = format!(
        "# 과제\n이전 라운드에서 확정된 아래 finding들이 이번 diff에서 고쳐졌는지 판정한다.\n\n\
         ## 이전 라운드 확정 findings\n{list}\n\n\
         ## 출력(JSON만, 코드펜스 없이)\n\
         {{\"results\":[{{\"finding_id\":\"...\",\"status\":\"FIXED|STILL_OPEN|UNKNOWN\",\"evidence\":\"...\"}}]}}\n",
        list = list
    );
    let v = llm
        .json_ctx(Some(&ctx), &task, Some(FIXCHECK_SYSTEM))
        .context("fix check 실패")?;
    let mut out: FixCheckOutput =
        serde_json::from_value(v).context("fix check JSON 스키마 불일치")?;
    for r in out.results.iter_mut() {
        r.status = normalize_status(&r.status);
    }
    let results = fill_missing_as_still_open(out.results, prior_confirmed);
    Ok(corroborate(results, prior_confirmed, &input.diff))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(id: &str, evidence: &str) -> Finding {
        Finding {
            id: id.to_string(),
            file: "src/x.rs".to_string(),
            line: "1".to_string(),
            claim: "claim".to_string(),
            evidence: evidence.to_string(),
            impact: String::new(),
            severity: "P1".to_string(),
            label: "possible bug".to_string(),
            confidence: "high".to_string(),
            recommendation: String::new(),
            lens: "design".to_string(),
            reviewer: "Reviewer".to_string(),
        }
    }

    #[test]
    fn corroborate_downgrades_fixed_to_unknown_when_evidence_still_present() {
        let prior = vec![finding("a", "unsafe { *ptr }")];
        let results = vec![FixStatus {
            finding_id: "a".to_string(),
            status: "FIXED".to_string(),
            evidence: "diff no longer touches this".to_string(),
        }];
        let diff = "some context\nunsafe { *ptr }\nmore context";
        let out = corroborate(results, &prior, diff);
        assert_eq!(out[0].status, "UNKNOWN");
        assert!(out[0].evidence.contains("결정적 재검증"));
    }

    #[test]
    fn corroborate_leaves_fixed_alone_when_evidence_is_gone() {
        let prior = vec![finding("a", "unsafe { *ptr }")];
        let results = vec![FixStatus {
            finding_id: "a".to_string(),
            status: "FIXED".to_string(),
            evidence: "replaced with safe accessor".to_string(),
        }];
        let diff = "some context\nlet v = safe_accessor();\nmore context";
        let out = corroborate(results, &prior, diff);
        assert_eq!(out[0].status, "FIXED");
    }

    #[test]
    fn corroborate_leaves_non_fixed_statuses_untouched() {
        let prior = vec![finding("a", "unsafe { *ptr }")];
        let results = vec![FixStatus {
            finding_id: "a".to_string(),
            status: "STILL_OPEN".to_string(),
            evidence: "still there".to_string(),
        }];
        let diff = "unsafe { *ptr }";
        let out = corroborate(results, &prior, diff);
        assert_eq!(out[0].status, "STILL_OPEN");
    }

    #[test]
    fn fix_check_output_survives_result_missing_status() {
        let json = serde_json::json!({"results": [{"finding_id": "a"}]});
        let out: FixCheckOutput =
            serde_json::from_value(json).expect("status 없어도 파싱 성공해야 함");
        assert_eq!(out.results[0].finding_id, "a");
        assert_eq!(out.results[0].status, "UNKNOWN");
    }

    #[test]
    fn normalize_status_is_case_insensitive() {
        assert_eq!(normalize_status("Fixed"), "FIXED");
        assert_eq!(normalize_status("STILL_OPEN"), "STILL_OPEN");
    }

    #[test]
    fn normalize_status_falls_back_to_unknown_on_unknown_or_empty_value() {
        assert_eq!(normalize_status("IN_PROGRESS"), "UNKNOWN");
        assert_eq!(normalize_status(""), "UNKNOWN");
    }

    #[test]
    fn fill_missing_as_still_open_synthesizes_entry_for_omitted_finding_id() {
        // LLM이 두 finding 중 하나만 결과에 넣고 나머지는 그냥 언급을 빼먹은 경우 —
        // "빠짐"이 "고쳐짐"으로 둔갑하면 안 되고 STILL_OPEN으로 안전하게 재편입돼야 한다.
        let prior = vec![finding("a", "unsafe { *ptr }"), finding("b", "eval(input)")];
        let results = vec![FixStatus {
            finding_id: "a".to_string(),
            status: "FIXED".to_string(),
            evidence: "replaced with safe accessor".to_string(),
        }];
        let out = fill_missing_as_still_open(results, &prior);
        assert_eq!(out.len(), 2);
        let b = out
            .iter()
            .find(|r| r.finding_id == "b")
            .expect("b가 합성돼야 함");
        assert_eq!(b.status, "STILL_OPEN");
    }

    #[test]
    fn fill_missing_as_still_open_leaves_fully_covered_results_untouched() {
        let prior = vec![finding("a", "unsafe { *ptr }")];
        let results = vec![FixStatus {
            finding_id: "a".to_string(),
            status: "FIXED".to_string(),
            evidence: "e".to_string(),
        }];
        let out = fill_missing_as_still_open(results, &prior);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].status, "FIXED");
    }
}
