use crate::input::Input;
use crate::lens::Finding;
use crate::llm::Llm;
use crate::promptctx::shared_context;
use crate::spec::Spec;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const REQ_SYSTEM: &str = "당신은 요구사항 충족 여부를 diff와 대조해 판정한다. \
근거가 없으면 MET으로 판정하지 않는다. 반드시 지정된 JSON 스키마로만 응답한다.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequirementCheck {
    pub requirement: String,
    pub status: String, // MET|MISSING|AMBIGUOUS|N/A
    pub evidence: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RequirementsOutput {
    #[serde(default)]
    requirements: Vec<RequirementCheck>,
}

/// requirements 미제공 시 None 반환(검증 대상 없음, N/A 나열하지 않음).
pub fn verify(
    llm: &Llm,
    spec: &Spec,
    input: &Input,
    confirmed: &[&Finding],
) -> Result<Option<Vec<RequirementCheck>>> {
    if input.requirements.is_none() {
        return Ok(None);
    }
    let findings_summary = confirmed
        .iter()
        .map(|f| format!("- [{}] {}:{} — {}", f.severity, f.file, f.line, f.claim))
        .collect::<Vec<_>>()
        .join("\n");
    // shared_context에 요구사항·컨벤션·diff가 이미 포함됨 — 다른 호출과 동일한 ctx라
    // OpenRouter 백엔드에서 캐시 재사용 대상이 된다.
    let ctx = shared_context(spec, input);
    let task = format!(
        "# 과제\n요구사항 각각을 diff와 대조해 판정한다.\n\n\
         ## 확정된 findings(참고용, 요구사항 미충족의 근거가 될 수 있음)\n{fs}\n\n\
         ## 출력(JSON만, 코드펜스 없이)\n\
         {{\"requirements\":[{{\"requirement\":\"요구사항 원문 그대로\",\"status\":\"MET|MISSING|AMBIGUOUS|N/A\",\
         \"evidence\":\"file:line 근거 또는 누락/모호 사유\"}}]}}\n",
        fs = if findings_summary.is_empty() { "(없음)".to_string() } else { findings_summary },
    );
    let v = llm
        .json_ctx(Some(&ctx), &task, Some(REQ_SYSTEM))
        .context("요구사항 검증 실패")?;
    let out: RequirementsOutput =
        serde_json::from_value(v).context("요구사항 검증 JSON 스키마 불일치")?;
    Ok(Some(out.requirements))
}
