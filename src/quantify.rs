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
    /// #189: which branch of `verdict()` actually produced `verdict` above — see its own doc
    /// comment for why this exists (verdict alone can't distinguish a confirmed defect from an
    /// unrelated policy failure).
    pub verdict_reason: VerdictReason,
    pub score: i64, // 0-100
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
            total = total.saturating_sub(p);
            deductions.push(format!(
                "[{}] {}:{} -{} pts — {}",
                f.severity, f.file, f.line, p, f.claim
            ));
        }
    }
    // #146: Spec::load now validates each scoring.pN into 0..=100, but clamping both ends here
    // too is cheap defense in depth against a `Finding` built directly (not via Spec::load —
    // e.g. a future caller, or a test) with an out-of-range ScoringConfig.
    (total.clamp(0, 100), deductions)
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

/// #151: `deterministic_results` (Semgrep auto-run, or `--deterministic-results`) used to be
/// rendered into report.md's table and nowhere else — a SAST "fail" and `verdict = APPROVE`
/// could coexist, since nothing in the verdict computation ever looked at this field. Scans
/// every check's `status` (keyed by check id, e.g. `{"sast": {"status": "fail", ...}, ...}`,
/// the same shape `report.rs::deterministic_table` already reads) for "fail"/"error".
fn deterministic_gate(deterministic_results: &Option<serde_json::Value>) -> Option<&'static str> {
    let obj = deterministic_results.as_ref()?.as_object()?;
    let mut has_error = false;
    for entry in obj.values() {
        match entry.get("status").and_then(|s| s.as_str()) {
            Some("fail") => return Some("REQUEST_CHANGES"),
            Some("error") => has_error = true,
            _ => {}
        }
    }
    if has_error {
        Some("NEEDS_CONTEXT")
    } else {
        None
    }
}

/// #189: `verdict` alone can't tell a caller whether `REQUEST_CHANGES` means "a confirmed P0
/// defect" or "the changelog wasn't updated" — found via a real 41-case benchmark where every
/// single case (positive and negative alike) came back `REQUEST_CHANGES`, because this repo's
/// commit style never satisfies the default spec's test/changelog policy, regardless of actual
/// code quality. This exists so that signal is readable without re-deriving it from
/// findings/policies/deterministic_results by hand — every branch below was already being
/// evaluated by `verdict()`; this just names which one actually fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictReason {
    /// A CONFIRMED P0 finding — an actual, discourse-confirmed defect.
    ConfirmedP0Defect,
    /// A policy check (tests-accompany-changes, changelog-updated, diff-size) failed — a
    /// process signal, not a claim that the code itself has a confirmed defect.
    PolicyFailure,
    /// An external deterministic tool (semgrep/cargo-audit/`--deterministic-results`) reported
    /// `"fail"` on at least one check.
    DeterministicCheckFailed,
    /// A deterministic tool reported `"error"` (couldn't run cleanly) with no `"fail"` present —
    /// distinct from an actual failing check.
    DeterministicCheckErrored,
    /// A P0/P1 finding exists but discourse couldn't reach consensus (`UNCERTAIN`) — a real
    /// signal worth attention, but not a confirmed defect either.
    UnresolvedHighSeverityFinding,
    /// A CONFIRMED P1 finding.
    ConfirmedP1Defect,
    /// A requirement came back MISSING or AMBIGUOUS.
    MissingOrAmbiguousRequirement,
    /// A CONFIRMED finding exists (P2/P3 only — nothing above fired).
    ConfirmedMinorDefect,
    /// No CONFIRMED findings and none of the above — genuinely clean by every signal checked.
    NoConfirmedFindings,
}

impl VerdictReason {
    /// Short slug rendered on report.md's verdict line — stable, machine-matchable, not prose.
    pub fn as_slug(&self) -> &'static str {
        match self {
            VerdictReason::ConfirmedP0Defect => "confirmed_p0_defect",
            VerdictReason::PolicyFailure => "policy_failure",
            VerdictReason::DeterministicCheckFailed => "deterministic_check_failed",
            VerdictReason::DeterministicCheckErrored => "deterministic_check_errored",
            VerdictReason::UnresolvedHighSeverityFinding => "unresolved_high_severity_finding",
            VerdictReason::ConfirmedP1Defect => "confirmed_p1_defect",
            VerdictReason::MissingOrAmbiguousRequirement => "missing_or_ambiguous_requirement",
            VerdictReason::ConfirmedMinorDefect => "confirmed_minor_defect",
            VerdictReason::NoConfirmedFindings => "no_confirmed_findings",
        }
    }
}

fn verdict(
    findings: &[Finding],
    resolved: &HashMap<String, Resolution>,
    policies: &[PolicyResult],
    requirements: &Option<Vec<RequirementCheck>>,
    deterministic_results: &Option<serde_json::Value>,
) -> (String, VerdictReason) {
    let confirmed: Vec<&Finding> = findings
        .iter()
        .filter(|f| resolved.get(&f.id).map(|r| r.status.as_str()) == Some("CONFIRMED"))
        .collect();

    if confirmed.iter().any(|f| f.severity == "P0") {
        return (
            "REQUEST_CHANGES".to_string(),
            VerdictReason::ConfirmedP0Defect,
        );
    }
    if policies.iter().any(|p| p.status == PolicyStatus::Fail) {
        return ("REQUEST_CHANGES".to_string(), VerdictReason::PolicyFailure);
    }
    if let Some(v) = deterministic_gate(deterministic_results) {
        let reason = if v == "REQUEST_CHANGES" {
            VerdictReason::DeterministicCheckFailed
        } else {
            VerdictReason::DeterministicCheckErrored
        };
        return (v.to_string(), reason);
    }
    // #136: verdict/score used to look at CONFIRMED findings only — a P0/P1 that discourse
    // couldn't reach consensus on (UNCERTAIN) had zero influence here, so a diff with a
    // genuinely unresolved high-severity finding could still land on APPROVE as long as nothing
    // else failed. MERGED is deliberately excluded — that status means "folded into another
    // finding," not "unresolved," and the merge target's own status is checked independently.
    if findings.iter().any(|f| {
        matches!(f.severity.as_str(), "P0" | "P1")
            && resolved.get(&f.id).map(|r| r.status.as_str()) == Some("UNCERTAIN")
    }) {
        return (
            "NEEDS_CONTEXT".to_string(),
            VerdictReason::UnresolvedHighSeverityFinding,
        );
    }
    if confirmed.iter().any(|f| f.severity == "P1") {
        return ("COMMENT".to_string(), VerdictReason::ConfirmedP1Defect);
    }
    if let Some(reqs) = requirements {
        if reqs
            .iter()
            .any(|r| r.status == "MISSING" || r.status == "AMBIGUOUS")
        {
            return (
                "NEEDS_CONTEXT".to_string(),
                VerdictReason::MissingOrAmbiguousRequirement,
            );
        }
    }
    if confirmed.is_empty() {
        ("APPROVE".to_string(), VerdictReason::NoConfirmedFindings)
    } else {
        ("COMMENT".to_string(), VerdictReason::ConfirmedMinorDefect)
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
    let (v, reason) = verdict(
        findings,
        resolved,
        policies,
        requirements,
        &input.deterministic_results,
    );
    QuantSummary {
        verdict: v,
        verdict_reason: reason,
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
    fn score_clamps_at_100_even_with_a_scoring_config_that_bypassed_spec_load_validation() {
        // #146: Spec::load now rejects a negative penalty, but score() defensively clamps the
        // upper end too — in case a ScoringConfig is ever built directly rather than parsed
        // from spec.toml.
        let scoring = ScoringConfig {
            p0: -50,
            ..Default::default()
        };
        let findings = vec![finding("a", "P0")];
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), resolution("a", "CONFIRMED"));
        let (sc, _) = score(&scoring, &findings, &resolved);
        assert_eq!(
            sc, 100,
            "score must never exceed 100 regardless of penalty sign"
        );
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

    // --- verdict().0 ---

    #[test]
    fn verdict_request_changes_on_confirmed_p0() {
        let findings = vec![finding("a", "P0")];
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), resolution("a", "CONFIRMED"));
        assert_eq!(
            verdict(&findings, &resolved, &[], &None, &None).0,
            "REQUEST_CHANGES"
        );
    }

    #[test]
    fn verdict_request_changes_on_policy_failure_even_with_no_findings() {
        let policies = vec![policy(PolicyStatus::Fail)];
        assert_eq!(
            verdict(&[], &HashMap::new(), &policies, &None, &None).0,
            "REQUEST_CHANGES"
        );
    }

    #[test]
    fn verdict_needs_context_on_an_uncertain_p0_even_with_nothing_else_confirmed() {
        // #136: an UNCERTAIN P0 (discourse couldn't reach consensus) used to have zero
        // influence on the verdict — nothing else failing meant APPROVE despite the unresolved
        // high-severity finding.
        let findings = vec![finding("a", "P0")];
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), resolution("a", "UNCERTAIN"));
        assert_eq!(
            verdict(&findings, &resolved, &[], &None, &None).0,
            "NEEDS_CONTEXT"
        );
    }

    #[test]
    fn verdict_needs_context_on_an_uncertain_p1() {
        let findings = vec![finding("a", "P1")];
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), resolution("a", "UNCERTAIN"));
        assert_eq!(
            verdict(&findings, &resolved, &[], &None, &None).0,
            "NEEDS_CONTEXT"
        );
    }

    #[test]
    fn verdict_ignores_an_uncertain_p2_since_it_is_not_high_severity() {
        let findings = vec![finding("a", "P2")];
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), resolution("a", "UNCERTAIN"));
        assert_eq!(
            verdict(&findings, &resolved, &[], &None, &None).0,
            "APPROVE"
        );
    }

    #[test]
    fn verdict_ignores_a_merged_p0_since_merged_means_folded_not_unresolved() {
        // MERGED is deliberately excluded from the #136 check — it means "folded into another
        // finding," and that survivor's own status is what should matter, not this entry.
        let findings = vec![finding("a", "P0")];
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), resolution("a", "MERGED"));
        assert_eq!(
            verdict(&findings, &resolved, &[], &None, &None).0,
            "APPROVE"
        );
    }

    #[test]
    fn verdict_request_changes_still_wins_over_an_unrelated_uncertain_p1() {
        // A confirmed P0 elsewhere must still take priority over the new NEEDS_CONTEXT check.
        let findings = vec![finding("a", "P0"), finding("b", "P1")];
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), resolution("a", "CONFIRMED"));
        resolved.insert("b".to_string(), resolution("b", "UNCERTAIN"));
        assert_eq!(
            verdict(&findings, &resolved, &[], &None, &None).0,
            "REQUEST_CHANGES"
        );
    }

    #[test]
    fn verdict_comment_on_confirmed_p1_without_p0() {
        let findings = vec![finding("a", "P1")];
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), resolution("a", "CONFIRMED"));
        assert_eq!(
            verdict(&findings, &resolved, &[], &None, &None).0,
            "COMMENT"
        );
    }

    #[test]
    fn verdict_needs_context_on_missing_requirement() {
        let reqs = vec![requirement("MISSING")];
        assert_eq!(
            verdict(&[], &HashMap::new(), &[], &Some(reqs), &None).0,
            "NEEDS_CONTEXT"
        );
    }

    #[test]
    fn verdict_needs_context_on_ambiguous_requirement() {
        let reqs = vec![requirement("AMBIGUOUS")];
        assert_eq!(
            verdict(&[], &HashMap::new(), &[], &Some(reqs), &None).0,
            "NEEDS_CONTEXT"
        );
    }

    #[test]
    fn verdict_approve_when_nothing_confirmed_and_no_other_signal() {
        assert_eq!(
            verdict(&[], &HashMap::new(), &[], &None, &None).0,
            "APPROVE"
        );
    }

    #[test]
    fn verdict_request_changes_on_a_deterministic_check_fail_even_with_nothing_confirmed() {
        // #151: a SAST "fail" used to have zero influence on verdict — only report.md's table
        // showed it.
        let det = serde_json::json!({"sast": {"status": "fail", "evidence": "1 findings"}});
        assert_eq!(
            verdict(&[], &HashMap::new(), &[], &None, &Some(det)).0,
            "REQUEST_CHANGES"
        );
    }

    #[test]
    fn verdict_needs_context_on_a_deterministic_check_error_with_no_fail() {
        let det = serde_json::json!({"sast": {"status": "error", "evidence": "scan incomplete"}});
        assert_eq!(
            verdict(&[], &HashMap::new(), &[], &None, &Some(det)).0,
            "NEEDS_CONTEXT"
        );
    }

    #[test]
    fn verdict_unaffected_by_a_deterministic_check_that_passes() {
        let det = serde_json::json!({"sast": {"status": "pass", "evidence": "0 findings"}});
        assert_eq!(
            verdict(&[], &HashMap::new(), &[], &None, &Some(det)).0,
            "APPROVE"
        );
    }

    #[test]
    fn verdict_request_changes_from_deterministic_fail_wins_over_needs_context_from_uncertain() {
        // A "fail" is checked before the #136 UNCERTAIN check — same priority tier as a
        // confirmed P0/policy failure.
        let findings = vec![finding("a", "P1")];
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), resolution("a", "UNCERTAIN"));
        let det = serde_json::json!({"secrets": {"status": "fail", "evidence": "1 findings"}});
        assert_eq!(
            verdict(&findings, &resolved, &[], &None, &Some(det)).0,
            "REQUEST_CHANGES"
        );
    }

    #[test]
    fn verdict_comment_on_confirmed_p2_only() {
        let findings = vec![finding("a", "P2")];
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), resolution("a", "CONFIRMED"));
        assert_eq!(
            verdict(&findings, &resolved, &[], &None, &None).0,
            "COMMENT"
        );
    }

    // --- VerdictReason (#189) ---

    #[test]
    fn verdict_reason_distinguishes_a_policy_failure_from_a_confirmed_p0_defect() {
        // The exact complaint #189 was filed over: both produce REQUEST_CHANGES, but callers
        // need to tell them apart.
        let (v1, r1) = verdict(
            &[finding("a", "P0")],
            &{
                let mut m = HashMap::new();
                m.insert("a".to_string(), resolution("a", "CONFIRMED"));
                m
            },
            &[],
            &None,
            &None,
        );
        let (v2, r2) = verdict(
            &[],
            &HashMap::new(),
            &[policy(PolicyStatus::Fail)],
            &None,
            &None,
        );
        assert_eq!(v1, "REQUEST_CHANGES");
        assert_eq!(v2, "REQUEST_CHANGES");
        assert_eq!(r1, VerdictReason::ConfirmedP0Defect);
        assert_eq!(r2, VerdictReason::PolicyFailure);
        assert_ne!(r1, r2, "same verdict string, but the reason must differ");
    }

    #[test]
    fn verdict_reason_distinguishes_a_deterministic_fail_from_a_deterministic_error() {
        let fail = serde_json::json!({"sast": {"status": "fail"}});
        let error = serde_json::json!({"sast": {"status": "error"}});
        let (_, r_fail) = verdict(&[], &HashMap::new(), &[], &None, &Some(fail));
        let (_, r_error) = verdict(&[], &HashMap::new(), &[], &None, &Some(error));
        assert_eq!(r_fail, VerdictReason::DeterministicCheckFailed);
        assert_eq!(r_error, VerdictReason::DeterministicCheckErrored);
    }

    #[test]
    fn verdict_reason_is_no_confirmed_findings_on_a_genuinely_clean_diff() {
        let (v, r) = verdict(&[], &HashMap::new(), &[], &None, &None);
        assert_eq!(v, "APPROVE");
        assert_eq!(r, VerdictReason::NoConfirmedFindings);
    }
}
