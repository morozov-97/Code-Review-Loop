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

/// All fields use `#[serde(default)]` — findings is a JSON array, so if even one required
/// field is missing on any element, serde fails that element outright, and parsing
/// `Vec<Finding>` dies entirely from that one failure (dropping the same lens's other
/// perfectly good findings along the partial-failure path). line/confidence were already
/// loosened after hitting this problem in production — the remaining fields carry the same
/// risk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    #[serde(default)]
    pub id: String,
    #[serde(default = "unknown")]
    pub file: String,
    #[serde(default = "unknown", deserialize_with = "string_or_number")]
    pub line: String,
    #[serde(default)]
    pub claim: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub impact: String,
    #[serde(default)]
    pub severity: String, // P0-P3, normalize_severity safely treats an empty value as P0 too
    #[serde(default = "unknown")]
    pub label: String,
    #[serde(default = "unknown")]
    pub confidence: String,
    #[serde(default)]
    pub recommendation: String,
    #[serde(default)]
    pub lens: String,
    /// This lens's persona name (empty string if not in the spec).
    #[serde(default)]
    pub reviewer: String,
}

fn unknown() -> String {
    "UNKNOWN".to_string()
}

/// We've actually observed the LLM responding with a JSON integer like `"line": 20`
/// (accepting only a string would cause a schema mismatch that drops that lens's entire
/// findings along the partial-failure path) — accepts both strings and numbers.
fn string_or_number<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrNumber {
        String(String),
        Number(serde_json::Number),
    }
    match StringOrNumber::deserialize(deserializer)? {
        StringOrNumber::String(s) => Ok(s),
        StringOrNumber::Number(n) => Ok(n.to_string()),
    }
}

const VALID_SEVERITIES: [&str; 4] = ["P0", "P1", "P2", "P3"];

/// The LLM can produce values outside the specified literals (P0-P3) — case, whitespace,
/// synonyms, etc., especially on the --cheap-model path. Since quantify.rs/report.rs match
/// this field with an exact string comparison, an unnormalized value silently counts as a
/// 0-point score/verdict — failure must land on the safe side (P0, stricter scrutiny), never
/// quietly leak toward APPROVE.
pub fn normalize_severity(raw: &str) -> String {
    let upper = raw.trim().to_ascii_uppercase();
    if VALID_SEVERITIES.contains(&upper.as_str()) {
        upper
    } else {
        "P0".to_string()
    }
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

/// For a lens with a persona set, prepend the character identity to the front of the system
/// prompt (to suppress conformity/sycophancy).
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

/// The prompt instructs "1-3", but the LLM may not follow that instruction — without a hard
/// cap, calls would scale directly with the number of lenses, spiking token cost and latency.
/// Enforce the cutoff in code to be sure.
const MAX_AUTO_SELECTED_LENSES: usize = 3;

/// Uses the LLM to select 1-3 lenses (excluding always-on ones) that fit the diff's nature.
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
    let mut seen = std::collections::HashSet::new();
    let valid: Vec<String> = selected
        .into_iter()
        .filter(|id| spec.lens_by_id(id).is_some())
        .filter(|id| seen.insert(id.clone()))
        .take(MAX_AUTO_SELECTED_LENSES)
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

/// On `--prior` re-review, a previous round's STILL_OPEN finding is re-carried as-is into
/// this round's findings/resolved (src/main.rs). Without a round number in the id, if the
/// same lens gets selected again next round, the same position-based number (e.g. "design-1")
/// gets reissued, causing two different findings to share an id — the source of double
/// score deductions and duplicate reports. Folding round into the id prevents this collision
/// (relying on the invariant that a re-carried finding always carries an earlier round
/// number).
fn finding_id(lens_id: &str, round: usize, index: usize) -> String {
    format!("{lens_id}-r{round}-{index}")
}

pub fn review_lens(
    llm: &Llm,
    spec: &Spec,
    input: &Input,
    lens_id: &str,
    round: usize,
) -> Result<LensOutput> {
    let lens = spec
        .lens_by_id(lens_id)
        .ok_or_else(|| anyhow::anyhow!("spec에 없는 렌즈: {lens_id}"))?;
    // ctx (context/conventions/requirements/diff) is identical across lenses — it's passed
    // separately from task (the lens-specific instructions) so the OpenRouter backend can
    // cache it as its own block.
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
        f.id = finding_id(lens_id, round, i + 1);
        f.lens = lens_id.to_string();
        f.reviewer = reviewer.clone();
        f.severity = normalize_severity(&f.severity);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_severity_passes_through_valid_values() {
        for s in ["P0", "P1", "P2", "P3"] {
            assert_eq!(normalize_severity(s), s);
        }
    }

    #[test]
    fn normalize_severity_trims_and_uppercases() {
        assert_eq!(normalize_severity(" p1 "), "P1");
    }

    #[test]
    fn normalize_severity_falls_back_to_most_severe_on_unknown_value() {
        assert_eq!(normalize_severity("Critical"), "P0");
        assert_eq!(normalize_severity(""), "P0");
    }

    #[test]
    fn finding_id_differs_across_rounds_for_the_same_position() {
        assert_ne!(finding_id("design", 1, 1), finding_id("design", 2, 1));
    }

    fn optional_lens(id: &str) -> crate::spec::Lens {
        crate::spec::Lens {
            id: id.to_string(),
            title: id.to_string(),
            guide: String::new(),
            always: false,
            signal: String::new(),
            persona_name: String::new(),
            persona_voice: String::new(),
            tier: String::new(),
        }
    }

    fn test_spec(lens_ids: &[&str]) -> crate::spec::Spec {
        crate::spec::Spec {
            name: "test".to_string(),
            context: String::new(),
            lenses: lens_ids.iter().map(|id| optional_lens(id)).collect(),
            deterministic_checks: Vec::new(),
            labels: vec!["bug".to_string()],
            diff_size_limit: 0,
            test_path_patterns: Vec::new(),
            doc_path_patterns: Vec::new(),
        }
    }

    fn test_input() -> Input {
        Input {
            diff: "diff --git a/x b/x\n+++ b/x\n".to_string(),
            changed_files: vec!["x".to_string()],
            added_lines: 1,
            removed_lines: 0,
            requirements: None,
            conventions: None,
            deterministic_results: None,
        }
    }

    #[test]
    fn select_lenses_caps_count_and_dedupes_even_if_llm_ignores_the_instruction() {
        let spec = test_spec(&["design", "complexity", "tests", "naming", "style"]);
        let inp = test_input();
        // Simulates the LLM ignoring the "1-3" instruction and returning 5 (with 1 duplicate).
        let response =
            r#"{"selected":["design","complexity","design","tests","naming","style"]}"#.to_string();
        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(vec![response], 0, usage);

        let selected = select_lenses(&llm, &spec, &inp).unwrap();

        assert_eq!(selected.len(), MAX_AUTO_SELECTED_LENSES);
        let unique: std::collections::HashSet<_> = selected.iter().collect();
        assert_eq!(unique.len(), selected.len(), "no duplicates expected");
        assert_eq!(selected, vec!["design", "complexity", "tests"]);
    }

    #[test]
    fn finding_id_differs_across_positions_within_a_round() {
        assert_ne!(finding_id("design", 1, 1), finding_id("design", 1, 2));
    }

    #[test]
    fn finding_line_accepts_json_integer_like_live_every_line_response() {
        // In a real dogfooding run, the every_line lens responded with "line": 20 (an
        // integer), and the whole lens got dropped with "invalid type: integer `20`,
        // expected a string".
        let json = r#"{"findings":[{"file":"x.dart","line":20,"claim":"c","evidence":"e","severity":"P3","label":"typo"}]}"#;
        let out: LensOutput = serde_json::from_str(json).unwrap();
        assert_eq!(out.findings[0].line, "20");
    }

    #[test]
    fn finding_line_accepts_json_integer_like_live_functionality_response() {
        // Same problem reproduced in the functionality lens with "line": 21.
        let json = r#"{"findings":[{"file":"x.dart","line":21,"claim":"c","evidence":"e","severity":"P2","label":"possible bug"}]}"#;
        let out: LensOutput = serde_json::from_str(json).unwrap();
        assert_eq!(out.findings[0].line, "21");
    }

    #[test]
    fn finding_line_still_accepts_string() {
        let json = r#"{"findings":[{"file":"x.dart","line":"17,21-25","claim":"c","evidence":"e","severity":"P2","label":"possible bug"}]}"#;
        let out: LensOutput = serde_json::from_str(json).unwrap();
        assert_eq!(out.findings[0].line, "17,21-25");
    }

    #[test]
    fn finding_line_defaults_when_absent() {
        let json = r#"{"findings":[{"file":"x.dart","claim":"c","evidence":"e","severity":"P2","label":"possible bug"}]}"#;
        let out: LensOutput = serde_json::from_str(json).unwrap();
        assert_eq!(out.findings[0].line, "UNKNOWN");
    }

    #[test]
    fn lens_output_survives_one_finding_missing_a_required_field() {
        // Found during a full review: if even one of file/claim/evidence/severity/label is
        // missing, the entire findings array fails to parse, dropping this lens's other
        // perfectly good findings too.
        let json = r#"{"findings":[
            {"file":"a.rs","line":"1","claim":"ok","evidence":"e","severity":"P1","label":"possible bug"},
            {"file":"b.rs","line":"2","claim":"second finding"}
        ]}"#;
        let out: LensOutput =
            serde_json::from_str(json).expect("일부 필드 누락에도 전체 파싱 성공해야 함");
        assert_eq!(out.findings.len(), 2);
        assert_eq!(out.findings[0].claim, "ok");
        assert_eq!(out.findings[1].evidence, "");
    }

    #[test]
    fn finding_severity_and_label_default_to_safe_values_when_absent() {
        let json = r#"{"findings":[{"file":"a.rs","claim":"c"}]}"#;
        let out: LensOutput = serde_json::from_str(json).unwrap();
        assert_eq!(normalize_severity(&out.findings[0].severity), "P0");
        assert_eq!(out.findings[0].label, "UNKNOWN");
        assert_eq!(out.findings[0].file, "a.rs");
    }
}
