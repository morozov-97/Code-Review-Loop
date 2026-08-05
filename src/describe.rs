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
///
/// `!l.starts_with("+++")`만으로 hunk body 줄과 파일 헤더를 구분하면, 추가된 줄의 내용
/// 자체가 "++ "로 시작할 때(raw 줄이 "+++ ..."가 됨 — input.rs::parse_diff_stats가 겪은
/// 것과 동일한 충돌) 진짜 TODO 줄이 헤더로 오인돼 스캔에서 빠진다. `@@`/`diff --git` 앵커로
/// hunk body 여부를 명시적으로 추적하면 이 모호성이 없다(hunk body 줄은 항상 `+`/`-`/` `
/// 마커로 시작해 이 앵커 문자열을 raw 상태로 흉내 낼 수 없음).
pub fn todo_sections(diff: &str) -> Vec<String> {
    let markers = ["TODO", "FIXME", "XXX"];
    let mut in_hunk_body = false;
    let mut out = Vec::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_hunk_body = false;
            continue;
        }
        if line.starts_with("@@") {
            in_hunk_body = true;
            continue;
        }
        if !in_hunk_body || !line.starts_with('+') {
            continue;
        }
        let content = line.strip_prefix('+').unwrap_or(line);
        if markers.iter().any(|m| content.contains(m)) {
            out.push(content.trim().to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_sections_keeps_a_leading_plus_that_belongs_to_the_code_itself() {
        // added line's own content is itself a markdown "+ " bullet (common list marker),
        // so the diff line is "++ TODO ..." — the outer '+' is only the diff marker.
        let diff = "diff --git a/CHANGELOG.md b/CHANGELOG.md\n\
                     --- a/CHANGELOG.md\n\
                     +++ b/CHANGELOG.md\n\
                     @@ -1,1 +1,2 @@\n\
                      existing line\n\
                     ++ TODO: revisit this bullet\n";
        let sections = todo_sections(diff);
        assert_eq!(sections, vec!["+ TODO: revisit this bullet".to_string()]);
    }

    #[test]
    fn todo_sections_finds_todo_even_when_content_collides_with_a_file_header() {
        // 한 단계 더 깊은 충돌: 추가된 줄 내용이 "++ "로 시작하면 raw 줄이 "+++ ..."가 되어
        // (파일 헤더 자체와 구분 불가한 접두어) 예전엔 TODO 스캔에서 통째로 빠졌다.
        let diff = "diff --git a/note.txt b/note.txt\n\
                     --- a/note.txt\n\
                     +++ b/note.txt\n\
                     @@ -1,1 +1,2 @@\n\
                      existing line\n\
                     +++ TODO: fix this later\n";
        let sections = todo_sections(diff);
        assert_eq!(sections, vec!["++ TODO: fix this later".to_string()]);
    }
}
