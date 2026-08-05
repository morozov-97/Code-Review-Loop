use crate::input::Input;
use crate::llm::Llm;
use crate::promptctx::shared_context;
use crate::spec::Spec;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const IMPROVE_SYSTEM: &str = "당신은 구체적인 코드 개선안을 제시하는 리뷰어다. \
이번 diff가 추가한(+) 라인에 대해서만 제안한다. 이미 반영된 것, docstring/타입힌트/주석/unused import 제안은 하지 않는다. \
반드시 지정된 JSON 스키마로만 응답한다.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub relevant_file: String,
    #[serde(default)]
    pub language: String,
    pub existing_code: String,
    pub suggestion_content: String,
    pub improved_code: String,
    pub one_sentence_summary: String,
    pub label: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ImproveOutput {
    #[serde(default)]
    suggestions: Vec<Suggestion>,
}

pub fn run(llm: &Llm, spec: &Spec, input: &Input) -> Result<Vec<Suggestion>> {
    let ctx = shared_context(spec, input);
    let task = format!(
        "# 과제\n이번 diff의 신규(+) 라인에 대해 구체적인 코드 개선안을 제시한다.\n\n\
         ## 규칙\n\
         - existing_code/improved_code는 실제 diff에 있는 코드를 그대로 인용/수정.\n\
         - one_sentence_summary는 6단어 이내.\n\
         - label은 다음 중 하나만: {labels}\n\n\
         ## 출력(JSON만, 코드펜스 없이)\n\
         {{\"suggestions\":[{{\"relevant_file\":\"...\",\"language\":\"...\",\"existing_code\":\"...\",\
         \"suggestion_content\":\"...\",\"improved_code\":\"...\",\"one_sentence_summary\":\"...\",\
         \"label\":<허용값 중 하나>}}]}}\n",
        labels = spec.labels_prompt(),
    );
    let v = llm
        .json_ctx(Some(&ctx), &task, Some(IMPROVE_SYSTEM))
        .context("improve 실패")?;
    let out: ImproveOutput = serde_json::from_value(v).context("improve JSON 스키마 불일치")?;
    Ok(out.suggestions)
}
