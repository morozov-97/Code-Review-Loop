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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixStatus {
    pub finding_id: String,
    pub status: String, // FIXED|STILL_OPEN|UNKNOWN
    pub evidence: String,
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
    let out: FixCheckOutput = serde_json::from_value(v).context("fix check JSON 스키마 불일치")?;
    Ok(corroborate(out.results, prior_confirmed, &input.diff))
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
}
