use crate::input::Input;
use crate::llm::Llm;
use crate::promptctx::shared_context;
use crate::spec::Spec;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DESCRIBE_SYSTEM: &str =
    "당신은 PR 설명을 작성하는 리뷰어다. diff에 없는 내용을 지어내지 않는다. \
반드시 지정된 JSON 스키마로만 응답한다.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Describe {
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub walkthrough: Vec<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    pub can_be_split: String, // yes|no|unknown
    #[serde(default)]
    pub can_be_split_note: String,
}

pub fn run(llm: &Llm, spec: &Spec, input: &Input) -> Result<Describe> {
    let ctx = shared_context(spec, input);
    let task = "# 과제\n아래 diff의 PR 설명을 작성한다.\n\n\
         ## 출력(JSON만, 코드펜스 없이)\n\
         {\"title\":\"50자 이내 한 줄\",\"summary\":\"2~4문장\",\
         \"walkthrough\":[\"파일/영역별 변경 요약, 항목당 1줄\"],\
         \"labels\":[\"feature|fix|refactor|chore|docs|test 중 해당하는 것\"],\
         \"can_be_split\":\"yes|no|unknown\",\"can_be_split_note\":\"근거\"}\n";
    let v = llm
        .json_ctx(Some(&ctx), task, Some(DESCRIBE_SYSTEM))
        .context("describe 실패")?;
    serde_json::from_value(v).context("describe JSON 스키마 불일치")
}

/// diff가 새로 추가한(+) 라인에서만 TODO/FIXME/XXX 스캔. 결정론적(LLM 미사용).
pub fn todo_sections(diff: &str) -> Vec<String> {
    let markers = ["TODO", "FIXME", "XXX"];
    diff.lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .filter(|l| markers.iter().any(|m| l.contains(m)))
        .map(|l| l.strip_prefix('+').unwrap_or(l).trim().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_sections_keeps_a_leading_plus_that_belongs_to_the_code_itself() {
        // added line's own content is itself a markdown "+ " bullet (common list marker),
        // so the diff line is "++ TODO ..." — the outer '+' is only the diff marker.
        let diff = "+++ b/CHANGELOG.md\n++ TODO: revisit this bullet\n";
        let sections = todo_sections(diff);
        assert_eq!(sections, vec!["+ TODO: revisit this bullet".to_string()]);
    }
}
