use crate::input::Input;
use crate::llm::Llm;
use crate::promptctx::shared_context;
use crate::spec::{Lens, Spec};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const LENS_SYSTEM: &str = "You are a single code reviewer. \
Separate suspicions that lack evidence into unverified rather than reporting them as findings. \
Point out only problems newly introduced by this diff; cite unchanged code only as supporting evidence. \
Do not suggest changes that are already applied, or unrelated docstring/type-hint/comment/unused-import cleanups. \
Respond strictly in the specified JSON schema only.";

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
            "You are \"{}\". {}\nDo not agree just to agree — if your judgment differs from this identity's perspective, say so clearly.\n\n{}",
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
            format!(
                "- id=\"{}\" | {} — selection signal: {}",
                l.id, who, l.signal
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let ctx = shared_context(spec, input);
    let task = format!(
        "# Task\nChoose 1-3 review lenses that fit the nature of the diff below (no swapping after selection).\n\n\
         ## Candidate lenses\n{catalog}\n\n\
         ## Output (JSON only)\n{{\"selected\":[\"id\", ...]}}\n",
        catalog = catalog
    );
    let v = llm
        .json_ctx(
            Some(&ctx),
            &task,
            Some("You are a Tech Lead who only performs lens selection. Respond strictly in the JSON schema only."),
        )
        .context("Lens selection failed")?;
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
        "Lens selection result is empty, or contains only ids not present in the spec"
    );
    Ok(valid)
}

fn build_review_task(spec: &Spec, lens_title: &str, lens_guide: &str) -> String {
    format!(
        "# Task\nReview the diff below independently from the \"{lens_title}\" perspective (do not reference other reviewers' results).\n\n\
         ## This lens's focus\n{lens_guide}\n\n\
         ## Review principles\n\
         - Every finding requires file:line evidence. Suspicions without evidence go under unverified.\n\
         - severity must be one of P0 (critical) through P3 (minor).\n\
         - label must be exactly one of: {labels}\n\n\
         ## Output (JSON only, no code fences)\n\
         {{\"findings\":[{{\"file\":\"...\",\"line\":\"...\",\"claim\":\"...\",\"evidence\":\"...\",\
         \"impact\":\"...\",\"severity\":\"P0|P1|P2|P3\",\"label\":<one of the allowed values>,\
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
        .ok_or_else(|| anyhow::anyhow!("Lens not in spec: {lens_id}"))?;
    // ctx (context/conventions/requirements/diff) is identical across lenses — it's passed
    // separately from task (the lens-specific instructions) so the OpenRouter backend can
    // cache it as its own block.
    let ctx = shared_context(spec, input);
    let task = build_review_task(spec, &lens.title, &lens.guide);
    let system = persona_system(lens);
    let mut out: LensOutput = llm
        .json_ctx_typed(Some(&ctx), &task, Some(&system))
        .with_context(|| format!("Lens review failed: {lens_id}"))?;
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
    "Find concrete implementations worth keeping. Do not manufacture praise without evidence.";

pub fn review_good_things(llm: &Llm, spec: &Spec, input: &Input) -> Result<GoodThingsOutput> {
    let ctx = shared_context(spec, input);
    let task = format!(
        "# Task\nFind good implementations in the diff below that are worth keeping.\n\n\
         ## This lens's focus\n{guide}\n\n\
         ## Output (JSON only, no code fences)\n\
         {{\"good_things\":[{{\"file_line\":\"file:line\",\"practice\":\"...\",\"why\":\"...\"}}]}}\n\
         If there is no concrete implementation to cite as evidence, return good_things as an empty array.\n",
        guide = GOOD_THINGS_GUIDE,
    );
    let out: GoodThingsOutput = llm
        .json_ctx_typed(Some(&ctx), &task, Some(LENS_SYSTEM))
        .context("Good Things lens failed")?;
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
        let out: LensOutput = serde_json::from_str(json)
            .expect("parsing should succeed for the whole array even with one field missing");
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
