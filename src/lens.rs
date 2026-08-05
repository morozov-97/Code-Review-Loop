use crate::input::Input;
use crate::llm::Llm;
use crate::promptctx::shared_context;
use crate::spec::{Lens, Spec};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const LENS_SYSTEM: &str = "당신은 코드 리뷰어 한 명이다. \
근거 없는 의심은 finding이 아니라 unverified로 분리한다. \
이번 diff가 새로 만든 문제만 지적하고, 미변경 코드는 근거로만 인용한다. \
이미 반영된 수정이나 독립적인 docstring·타입힌트·주석·unused import 제안은 하지 않는다. \
반드시 지정된 JSON 스키마로만 응답한다.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    #[serde(default)]
    pub id: String,
    pub file: String,
    #[serde(default = "unknown")]
    pub line: String,
    pub claim: String,
    pub evidence: String,
    #[serde(default)]
    pub impact: String,
    pub severity: String, // P0-P3
    pub label: String,
    #[serde(default = "unknown")]
    pub confidence: String,
    #[serde(default)]
    pub recommendation: String,
    #[serde(default)]
    pub lens: String,
    /// 이 렌즈의 페르소나 이름(spec에 없으면 빈 문자열).
    #[serde(default)]
    pub reviewer: String,
}

fn unknown() -> String {
    "UNKNOWN".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LensOutput {
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub unverified: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoodThing {
    pub file_line: String,
    pub practice: String,
    pub why: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoodThingsOutput {
    #[serde(default)]
    pub good_things: Vec<GoodThing>,
}

/// 페르소나가 지정된 렌즈는 캐릭터 정체성을 시스템 프롬프트 앞단에 붙인다(동조성 억제).
fn persona_system(lens: &Lens) -> String {
    if lens.persona_name.is_empty() {
        LENS_SYSTEM.to_string()
    } else {
        format!(
            "당신은 \"{}\"이다. {}\n동의를 위한 동의를 하지 않는다 — 이 정체성의 관점에서 판단이 다르면 명확히 다르게 말한다.\n\n{}",
            lens.persona_name, lens.persona_voice, LENS_SYSTEM
        )
    }
}

/// 렌즈 후보(always 제외) 중 diff 성격에 맞는 3~5개를 LLM으로 선정한다.
pub fn select_lenses(llm: &Llm, spec: &Spec, input: &Input) -> Result<Vec<String>> {
    let optional = spec.optional_lenses();
    if optional.is_empty() {
        return Ok(Vec::new());
    }
    let catalog = optional
        .iter()
        .map(|l| {
            let who = if l.persona_name.is_empty() {
                l.title.clone()
            } else {
                format!("{} ({})", l.title, l.persona_name)
            };
            format!("- id=\"{}\" | {} — 선정 신호: {}", l.id, who, l.signal)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let ctx = shared_context(spec, input);
    let task = format!(
        "# 과제\n아래 diff 성격에 맞는 리뷰 렌즈를 1~3개 고른다(선정 이후 교체 없음).\n\n\
         ## 렌즈 후보\n{catalog}\n\n\
         ## 출력(JSON만)\n{{\"selected\":[\"id\", ...]}}\n",
        catalog = catalog
    );
    let v = llm
        .json_ctx(
            Some(&ctx),
            &task,
            Some("렌즈 선정만 수행하는 Tech Lead다. 반드시 JSON 스키마로만 응답한다."),
        )
        .context("렌즈 선정 실패")?;
    let selected: Vec<String> = v
        .get("selected")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let valid: Vec<String> = selected
        .into_iter()
        .filter(|id| spec.lens_by_id(id).is_some())
        .collect();
    anyhow::ensure!(
        !valid.is_empty(),
        "렌즈 선정 결과가 비어있거나 spec에 없는 id뿐"
    );
    Ok(valid)
}

fn build_review_task(spec: &Spec, lens_title: &str, lens_guide: &str) -> String {
    format!(
        "# 과제\n아래 diff를 \"{lens_title}\" 관점(다른 리뷰어 결과는 참조하지 않음)에서 독립적으로 리뷰한다.\n\n\
         ## 이 렌즈의 초점\n{lens_guide}\n\n\
         ## 리뷰 원칙\n\
         - finding마다 file:line 근거 필수. 근거 없는 의심은 unverified로.\n\
         - severity는 P0(치명)~P3(사소) 중 하나.\n\
         - label은 다음 중 하나만: {labels}\n\n\
         ## 출력(JSON만, 코드펜스 없이)\n\
         {{\"findings\":[{{\"file\":\"...\",\"line\":\"...\",\"claim\":\"...\",\"evidence\":\"...\",\
         \"impact\":\"...\",\"severity\":\"P0|P1|P2|P3\",\"label\":<허용값 중 하나>,\
         \"confidence\":\"high|medium|low\",\"recommendation\":\"...\"}}],\"unverified\":[\"...\"]}}\n",
        lens_title = lens_title,
        lens_guide = lens_guide,
        labels = spec.labels_prompt(),
    )
}

pub fn review_lens(llm: &Llm, spec: &Spec, input: &Input, lens_id: &str) -> Result<LensOutput> {
    let lens = spec
        .lens_by_id(lens_id)
        .ok_or_else(|| anyhow::anyhow!("spec에 없는 렌즈: {lens_id}"))?;
    // ctx(맥락·컨벤션·요구사항·diff)는 렌즈마다 동일 — OpenRouter 백엔드에서 별도 블록으로
    // 캐싱되도록 task(렌즈별 지시문)와 분리해서 전달한다.
    let ctx = shared_context(spec, input);
    let task = build_review_task(spec, &lens.title, &lens.guide);
    let system = persona_system(lens);
    let v = llm
        .json_ctx(Some(&ctx), &task, Some(&system))
        .with_context(|| format!("렌즈 리뷰 실패: {lens_id}"))?;
    let mut out: LensOutput = serde_json::from_value(v)
        .with_context(|| format!("렌즈 리뷰 JSON 스키마 불일치: {lens_id}"))?;
    let reviewer = if lens.persona_name.is_empty() {
        lens.title.clone()
    } else {
        lens.persona_name.clone()
    };
    for (i, f) in out.findings.iter_mut().enumerate() {
        f.id = format!("{}-{}", lens_id, i + 1);
        f.lens = lens_id.to_string();
        f.reviewer = reviewer.clone();
        if f.line.trim().is_empty() {
            f.line = unknown();
        }
    }
    Ok(out)
}

const GOOD_THINGS_GUIDE: &str =
    "유지할 가치가 있는 구체적 구현을 찾는다. 근거 없는 칭찬은 만들지 않는다.";

pub fn review_good_things(llm: &Llm, spec: &Spec, input: &Input) -> Result<GoodThingsOutput> {
    let ctx = shared_context(spec, input);
    let task = format!(
        "# 과제\n아래 diff에서 유지해야 할 좋은 구현을 찾는다.\n\n\
         ## 이 렌즈의 초점\n{guide}\n\n\
         ## 출력(JSON만, 코드펜스 없이)\n\
         {{\"good_things\":[{{\"file_line\":\"file:line\",\"practice\":\"...\",\"why\":\"...\"}}]}}\n\
         근거로 인용할 구체적 구현이 없으면 good_things를 빈 배열로 반환한다.\n",
        guide = GOOD_THINGS_GUIDE,
    );
    let v = llm
        .json_ctx(Some(&ctx), &task, Some(LENS_SYSTEM))
        .context("Good Things 렌즈 실패")?;
    let out: GoodThingsOutput =
        serde_json::from_value(v).context("Good Things JSON 스키마 불일치")?;
    Ok(out)
}
