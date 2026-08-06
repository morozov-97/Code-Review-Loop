use crate::discourse::Resolution;
use crate::input::Input;
use crate::lens::Finding;
use crate::policy::{PolicyResult, PolicyStatus};
use crate::requirements::RequirementCheck;
use crate::spec::ScoringConfig;
use std::collections::HashMap;

/// #115: whether every stage that could have contributed to `verdict`/`score` actually ran
/// successfully. `report.rs` already renders a "(PARTIAL)"/"(FAILED)" marker on the verdict
/// line based on this, but that's markdown text — this field exists so a programmatic consumer
/// (future structured output, a CI script reading state.json, anything matching on
/// `QuantSummary` directly instead of parsing the rendered report) gets the same signal without
/// having to parse a string.
///
/// `Failed` (added after review — see the follow-up issue filed alongside #115) is distinct
/// from `Partial`: some supplementary stage failing (good_things, human-voice, a `--prior` fix
/// check) still leaves a meaningful defect review behind, but every selected lens failing means
/// there's no defect-finding coverage at all — calling that "Partial" the same way undersells
/// how little the resulting verdict actually reflects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewCompleteness {
    Complete,
    Partial,
    Failed,
}

pub struct QuantSummary {
    pub verdict: String, // APPROVE|COMMENT|REQUEST_CHANGES|NEEDS_CONTEXT
    pub score: i64,      // 0-100
    pub score_deductions: Vec<String>,
    pub estimated_effort_1_5: u8,
    pub time_best_min: u32,
    pub time_average_min: u32,
    pub time_worst_min: u32,
    pub completeness: ReviewCompleteness,
}

fn severity_penalty(scoring: &ScoringConfig, severity: &str) -> i64 {
    match severity {
        "P0" => scoring.p0,
        "P1" => scoring.p1,
        "P2" => scoring.p2,
        "P3" => scoring.p3,
        _ => 0,
    }
}

/// Deduct points from 100 using only CONFIRMED findings. Records the deduction reasons as strings alongside.
/// Deduction amounts come from `spec.scoring` (see #106 — used to be hardcoded here with no way
/// to tune per team policy; `ScoringConfig`'s defaults preserve the original P0=25/P1=12/P2=5/P3=1).
fn score(
    scoring: &ScoringConfig,
    findings: &[Finding],
    resolved: &HashMap<String, Resolution>,
) -> (i64, Vec<String>) {
    let mut total = 100i64;
    let mut deductions = Vec::new();
    for f in findings {
        if resolved.get(&f.id).map(|r| r.status.as_str()) == Some("CONFIRMED") {
            let p = severity_penalty(scoring, &f.severity);
            total -= p;
            deductions.push(format!(
                "[{}] {}:{} -{} pts — {}",
                f.severity, f.file, f.line, p, f.claim
            ));
        }
    }
    (total.max(0), deductions)
}

/// Estimated review effort based on change size. Assumption: the thresholds are a design choice (uncertain), should be adjusted per team size.
fn effort_and_time(input: &Input, lens_count: usize) -> (u8, u32, u32, u32) {
    let lines = input.added_lines + input.removed_lines;
    let mut effort: u8 = match lines {
        0..=50 => 1,
        51..=200 => 2,
        201..=500 => 3,
        501..=1000 => 4,
        _ => 5,
    };
    if input.changed_files.len() > 10 && effort < 5 {
        effort += 1;
    }
    if lens_count >= 4 && effort < 5 {
        effort += 1;
    }
    let effort = effort.min(5);
    let best = effort as u32 * 5;
    let average = effort as u32 * 15;
    let worst = effort as u32 * 40;
    (effort, best, average, worst)
}

fn verdict(
    findings: &[Finding],
    resolved: &HashMap<String, Resolution>,
    policies: &[PolicyResult],
    requirements: &Option<Vec<RequirementCheck>>,
) -> String {
    let confirmed: Vec<&Finding> = findings
        .iter()
        .filter(|f| resolved.get(&f.id).map(|r| r.status.as_str()) == Some("CONFIRMED"))
        .collect();

    if confirmed.iter().any(|f| f.severity == "P0") {
        return "REQUEST_CHANGES".to_string();
    }
    if policies.iter().any(|p| p.status == PolicyStatus::Fail) {
        return "REQUEST_CHANGES".to_string();
    }
    if confirmed.iter().any(|f| f.severity == "P1") {
        return "COMMENT".to_string();
    }
    if let Some(reqs) = requirements {
        if reqs
            .iter()
            .any(|r| r.status == "MISSING" || r.status == "AMBIGUOUS")
        {
            return "NEEDS_CONTEXT".to_string();
        }
    }
    if confirmed.is_empty() {
        "APPROVE".to_string()
    } else {
        "COMMENT".to_string()
    }
}

pub fn summarize(
    scoring: &ScoringConfig,
    input: &Input,
    findings: &[Finding],
    resolved: &HashMap<String, Resolution>,
    policies: &[PolicyResult],
    requirements: &Option<Vec<RequirementCheck>>,
    lens_count: usize,
) -> QuantSummary {
    let (sc, deductions) = score(scoring, findings, resolved);
    let (effort, best, average, worst) = effort_and_time(input, lens_count);
    let v = verdict(findings, resolved, policies, requirements);
    QuantSummary {
        verdict: v,
        score: sc,
        score_deductions: deductions,
        estimated_effort_1_5: effort,
        time_best_min: best,
        time_average_min: average,
        time_worst_min: worst,
        // Caller sets this to Partial when stage_errors is non-empty (see pipeline/review.rs) —
        // summarize() itself has no visibility into stages outside its own inputs, so Complete
        // is just the default absent that outside signal, not a claim this stage succeeded.
        completeness: ReviewCompleteness::Complete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::RunConfig;

    fn test_input() -> Input {
        Input {
            diff: "diff --git a/x b/x\n+++ b/x\n".to_string(),
            changed_files: vec!["x".to_string()],
            added_lines: 1,
            removed_lines: 0,
            requirements: None,
            conventions: None,
            deterministic_results: None,
            config: RunConfig::default(),
        }
    }

    #[test]
    fn summarize_defaults_completeness_to_complete() {
        // #115: summarize() itself has no visibility into stages outside its own inputs, so it
        // always returns Complete — the caller (run_review) is responsible for downgrading to
        // Partial when stage_errors is non-empty, not this function.
        let quant = summarize(
            &ScoringConfig::default(),
            &test_input(),
            &[],
            &HashMap::new(),
            &[],
            &None,
            1,
        );
        assert_eq!(quant.completeness, ReviewCompleteness::Complete);
    }

    fn finding(id: &str, severity: &str) -> Finding {
        Finding {
            id: id.to_string(),
            file: "src/x.rs".to_string(),
            line: "1".to_string(),
            claim: "claim".to_string(),
            evidence: "evidence".to_string(),
            impact: String::new(),
            severity: severity.to_string(),
            label: "possible bug".to_string(),
            confidence: "high".to_string(),
            recommendation: String::new(),
            lens: "design".to_string(),
            reviewer: "Reviewer".to_string(),
            evidence_unverified: false,
        }
    }

    fn resolution(id: &str, status: &str) -> Resolution {
        Resolution {
            finding_id: id.to_string(),
            status: status.to_string(),
            merged_into: String::new(),
            reason: String::new(),
        }
    }

    fn policy(status: PolicyStatus) -> PolicyResult {
        PolicyResult {
            title: "Some policy".to_string(),
            status,
            evidence: String::new(),
        }
    }

    fn requirement(status: &str) -> RequirementCheck {
        RequirementCheck {
            requirement: "must do X".to_string(),
            status: status.to_string(),
            evidence: String::new(),
        }
    }

    // --- score() ---

    #[test]
    fn score_deducts_only_confirmed_findings_by_severity() {
        let findings = vec![finding("a", "P0"), finding("b", "P2")];
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), resolution("a", "CONFIRMED"));
        resolved.insert("b".to_string(), resolution("b", "REJECTED"));
        let (sc, deductions) = score(&ScoringConfig::default(), &findings, &resolved);
        assert_eq!(sc, 75); // 100 - 25 (P0 only; P2 rejected, not deducted)
        assert_eq!(deductions.len(), 1);
    }

    #[test]
    fn score_treats_unresolved_findings_as_not_deducted() {
        let findings = vec![finding("a", "P0")];
        let (sc, deductions) = score(&ScoringConfig::default(), &findings, &HashMap::new());
        assert_eq!(sc, 100);
        assert!(deductions.is_empty());
    }

    #[test]
    fn score_clamps_at_zero_instead_of_going_negative() {
        let findings: Vec<Finding> = (0..10).map(|i| finding(&format!("p{i}"), "P0")).collect();
        let mut resolved = HashMap::new();
        for f in &findings {
            resolved.insert(f.id.clone(), resolution(&f.id, "CONFIRMED"));
        }
        let (sc, _) = score(&ScoringConfig::default(), &findings, &resolved);
        assert_eq!(sc, 0);
    }

    #[test]
    fn score_uses_custom_scoring_config_instead_of_the_hardcoded_defaults() {
        // #106: severity weights used to be hardcoded with no way to tune per team policy.
        let findings = vec![finding("a", "P0")];
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), resolution("a", "CONFIRMED"));
        let custom = ScoringConfig {
            p0: 40,
            p1: 12,
            p2: 5,
            p3: 1,
        };
        let (sc, _) = score(&custom, &findings, &resolved);
        assert_eq!(sc, 60); // 100 - 40 (custom P0 weight), not the default 25
    }

    // --- verdict() ---

    #[test]
    fn verdict_request_changes_on_confirmed_p0() {
        let findings = vec![finding("a", "P0")];
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), resolution("a", "CONFIRMED"));
        assert_eq!(verdict(&findings, &resolved, &[], &None), "REQUEST_CHANGES");
    }

    #[test]
    fn verdict_request_changes_on_policy_failure_even_with_no_findings() {
        let policies = vec![policy(PolicyStatus::Fail)];
        assert_eq!(
            verdict(&[], &HashMap::new(), &policies, &None),
            "REQUEST_CHANGES"
        );
    }

    #[test]
    fn verdict_comment_on_confirmed_p1_without_p0() {
        let findings = vec![finding("a", "P1")];
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), resolution("a", "CONFIRMED"));
        assert_eq!(verdict(&findings, &resolved, &[], &None), "COMMENT");
    }

    #[test]
    fn verdict_needs_context_on_missing_requirement() {
        let reqs = vec![requirement("MISSING")];
        assert_eq!(
            verdict(&[], &HashMap::new(), &[], &Some(reqs)),
            "NEEDS_CONTEXT"
        );
    }

    #[test]
    fn verdict_needs_context_on_ambiguous_requirement() {
        let reqs = vec![requirement("AMBIGUOUS")];
        assert_eq!(
            verdict(&[], &HashMap::new(), &[], &Some(reqs)),
            "NEEDS_CONTEXT"
        );
    }

    #[test]
    fn verdict_approve_when_nothing_confirmed_and_no_other_signal() {
        assert_eq!(verdict(&[], &HashMap::new(), &[], &None), "APPROVE");
    }

    #[test]
    fn verdict_comment_on_confirmed_p2_only() {
        let findings = vec![finding("a", "P2")];
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), resolution("a", "CONFIRMED"));
        assert_eq!(verdict(&findings, &resolved, &[], &None), "COMMENT");
    }
}
