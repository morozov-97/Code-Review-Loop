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
    Ok(out.results)
}
