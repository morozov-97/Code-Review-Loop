use crate::input::Input;
use crate::lens::{Finding, GoodThing};
use crate::llm::Llm;
use crate::promptctx::shared_context;
use crate::spec::Spec;
use anyhow::{Context, Result};

pub const HUMANVOICE_SYSTEM: &str =
    "당신은 Google 코드리뷰 가이드라인 톤을 따르는 human reviewer다. \
사소한 지적은 'Nit:'으로 표시하고, 단정보다 질문형을 섞어 정중하게 쓴다. \
확정된 목록에 없는 새 지적은 만들지 않는다.";

/// 확정 findings·good things를 사람이 PR에 직접 남기는 리뷰 코멘트 톤으로 재작성.
pub fn rewrite(
    llm: &Llm,
    spec: &Spec,
    input: &Input,
    confirmed: &[&Finding],
    good_things: &[GoodThing],
) -> Result<String> {
    if confirmed.is_empty() && good_things.is_empty() {
        return Ok("(확정된 finding·good things 없음 — human-voice 리라이트 생략)".to_string());
    }
    let findings_text = confirmed
        .iter()
        .map(|f| {
            format!(
                "- [{}] {}:{} {} (근거: {})",
                f.severity, f.file, f.line, f.claim, f.evidence
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let good_text = good_things
        .iter()
        .map(|g| format!("- {} — {}", g.file_line, g.practice))
        .collect::<Vec<_>>()
        .join("\n");
    let ctx = shared_context(spec, input);
    let task =
        format!(
        "# 과제\n아래 확정된 리뷰 결과를 사람이 PR에 직접 남기는 리뷰 코멘트 톤으로 다시 쓴다.\n\n\
         ## 확정 findings\n{findings_text}\n\n## Good things\n{good_text}\n\n\
         ## 출력 규칙\n\
         - 마크다운 코멘트 본문만 출력(메타코멘트·서론 없이).\n\
         - 사소한 지적은 'Nit:'으로 시작.\n\
         - 단정 대신 질문형을 섞어서 정중하게.\n\
         - 위 목록에 없는 새 지적을 만들지 말 것 — 재서술만.\n",
        findings_text = if findings_text.is_empty() { "(없음)".to_string() } else { findings_text },
        good_text = if good_text.is_empty() { "(없음)".to_string() } else { good_text },
    );
    llm.text_ctx(Some(&ctx), &task, Some(HUMANVOICE_SYSTEM))
        .context("human-voice 리라이트 실패")
}
