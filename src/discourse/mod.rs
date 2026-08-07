//! Cross-verification layer: after each lens independently reviews the diff, discourse runs
//! rounds where lenses challenge/agree on each other's findings and reach a resolution.
//!
//! #130: split out of one large file along its natural seams — schema (wire format + LLM-output
//! normalization), prompt (system prompt + per-round prompt construction), votes (confidence
//! tallying), leaving this file with just the orchestration loop itself.
mod prompt;
mod schema;
#[cfg(test)]
mod test_support;
mod votes;

pub use schema::{Move, Resolution};

use crate::input::Input;
use crate::lens::Finding;
use crate::llm::Llm;
use crate::promptctx::shared_context;
use crate::spec::Spec;
use anyhow::{Context, Result};
use prompt::{build_round_prompt, DISCOURSE_SYSTEM};
use schema::{
    normalize_challenge_axis, normalize_confidence, normalize_move_kind, normalize_status,
    surface_id, DiscourseRound,
};
use std::collections::HashMap;
use votes::{direct_vote_net, merge_vote_weight, VOTE_THRESHOLD};

pub struct DiscourseAudit {
    pub round: usize,
    pub moves: Vec<Move>,
}

/// Iterates discourse rounds. Stops once there are no unresolved/UNCERTAIN findings left, or
/// max_rounds is reached. Retries once per round if a round comes back with no CHALLENGE.
///
/// `outer_round` is the overall pipeline round carried across via `--prior` (the same number
/// used by lens.rs::finding_id). If SURFACE ids were built from only the discourse-internal
/// loop's `round` (which always restarts from 1), two different outer_round calls could
/// coincidentally produce the same (round, index) pair, causing two completely different
/// findings to share an id — the source of double-counted scores and overwritten discourse
/// verdicts. We guard against this the same way lens.rs does, by folding outer_round into
/// the id.
pub fn run(
    llm: &Llm,
    spec: &Spec,
    input: &Input,
    findings: &mut Vec<Finding>,
    max_rounds: usize,
    outer_round: usize,
) -> Result<(Vec<DiscourseAudit>, HashMap<String, Resolution>)> {
    let max_rounds = max_rounds.max(1);
    let mut resolved: HashMap<String, Resolution> = HashMap::new();
    let mut audit: Vec<DiscourseAudit> = Vec::new();
    // Discourse used to see only findings_catalog (the claim/evidence text left by reviewers)
    // and never the raw diff at all — there was no way to actually cross-check and refute an
    // absence claim like "not in the diff" (a false-positive case: it confirmed a cancel call
    // was "missing" from dispose even though it was right there in the diff). Attaching ctx
    // lets every round use the actual diff as evidence when making a verdict.
    let ctx = shared_context(spec, input);

    for round in 1..=max_rounds {
        let unresolved = findings.iter().any(|f| {
            resolved
                .get(&f.id)
                .map(|r| r.status == "UNCERTAIN")
                .unwrap_or(true)
        });
        if !unresolved {
            break;
        }

        let mut dr = run_round_call(llm, &ctx, spec, findings, &resolved, round)?;
        for m in dr.moves.iter_mut() {
            m.kind = normalize_move_kind(&m.kind);
            m.confidence = normalize_confidence(&m.confidence);
        }
        if !dr.moves.iter().any(|m| m.kind == "CHALLENGE") {
            dr = run_round_call(llm, &ctx, spec, findings, &resolved, round)
                .context("retry request for missing CHALLENGE failed")?;
            for m in dr.moves.iter_mut() {
                m.kind = normalize_move_kind(&m.kind);
                m.confidence = normalize_confidence(&m.confidence);
            }
        }

        for (i, sf) in dr.surfaced.iter_mut().enumerate() {
            sf.id = surface_id(outer_round, round, i + 1);
            // The code always sets lens authoritatively — same principle as regular findings
            // (lens.rs:221). Prevents any out-of-schema lens value the LLM sends from
            // surviving as-is.
            sf.lens = "discourse".to_string();
            if sf.line.trim().is_empty() {
                sf.line = "UNKNOWN".to_string();
            }
            sf.severity = crate::lens::normalize_severity(&sf.severity);
        }
        findings.extend(dr.surfaced.clone());

        for mut r in dr.resolutions.clone() {
            r.status = normalize_status(&r.status);
            // #140: mirrors fixcheck::verify_supersedes's treatment of superseded_by — a
            // MERGED resolution naming a merged_into that isn't an actual finding id would
            // otherwise vanish (excluded from `confirmed` as MERGED, but its vote weight never
            // credited to anything real either, since the target it names doesn't exist).
            if r.status == "MERGED" && !findings.iter().any(|f| f.id == r.merged_into) {
                r.reason = format!(
                    "{} [Verification failed: merged_into(\"{}\") is not among this round's findings — safely reverted to UNCERTAIN]",
                    r.reason, r.merged_into
                );
                r.status = "UNCERTAIN".to_string();
            }
            // #140: the prompt says resolutions "should only judge findings that are UNRESOLVED
            // or were UNCERTAIN in the previous round," but nothing enforced that — a later
            // round's LLM output re-judging an already-CONFIRMED/REJECTED id would silently
            // replace it. Once a finding has a final verdict, keep it.
            let already_final = matches!(
                resolved.get(&r.finding_id).map(|e| e.status.as_str()),
                Some("CONFIRMED") | Some("REJECTED")
            );
            if already_final {
                continue;
            }
            resolved.insert(r.finding_id.clone(), r);
        }

        for m in dr.moves.iter_mut() {
            if m.kind == "CHALLENGE" {
                m.challenge_axis = normalize_challenge_axis(&m.challenge_axis);
            }
        }

        audit.push(DiscourseAudit {
            round,
            moves: dr.moves,
        });

        if round == max_rounds {
            break;
        }
    }

    // UNCERTAIN/unjudged findings left after rounds run out: instead of just discarding them,
    // tally the AGREE/CHALLENGE votes across all rounds as a confidence-weighted vote for a
    // final verdict.
    for f in findings.iter() {
        let still_uncertain = resolved
            .get(&f.id)
            .map(|r| r.status == "UNCERTAIN")
            .unwrap_or(true);
        if !still_uncertain {
            continue;
        }

        // #140: was a hand-duplicated copy of votes::direct_vote_net's own logic (missing its
        // new_evidence/dedup enforcement) — now calls it directly, so both fixes apply to the
        // actual tally that decides CONFIRMED/REJECTED, not just to direct_vote_net's own
        // internal use inside merge_vote_weight.
        let net: f64 = direct_vote_net(&audit, &f.id) + merge_vote_weight(&resolved, &audit, &f.id);

        let (status, reason) = if net >= VOTE_THRESHOLD {
            (
                "CONFIRMED".to_string(),
                format!("discourse rounds exhausted, confirmed by confidence-weighted vote (net={net:.2})"),
            )
        } else if net <= -VOTE_THRESHOLD {
            (
                "REJECTED".to_string(),
                format!("discourse rounds exhausted, rejected by confidence-weighted vote (net={net:.2})"),
            )
        } else {
            (
                "UNCERTAIN".to_string(),
                format!("discourse rounds exhausted, no verdict (net={net:.2})"),
            )
        };

        resolved.insert(
            f.id.clone(),
            Resolution {
                finding_id: f.id.clone(),
                status,
                merged_into: String::new(),
                reason,
            },
        );
    }

    // #140: a severity-axis CHALLENGE deliberately never affects the confirm/reject vote (see
    // #75) or `finding.severity` itself — auto-adjusting severity off an unvalidated LLM
    // proposal risks silently downgrading a real P0 with nothing to catch it, worse than the
    // status quo. But a CONFIRMED finding the panel also disputed the severity of currently has
    // zero visibility next to the score-affecting entry itself (only the separate Discourse
    // Audit table shows raw moves) — this makes that dispute visible right on the finding.
    for f in findings.iter() {
        let is_confirmed = resolved
            .get(&f.id)
            .map(|r| r.status == "CONFIRMED")
            .unwrap_or(false);
        if !is_confirmed {
            continue;
        }
        let disputed = audit
            .iter()
            .flat_map(|a| a.moves.iter())
            .any(|m| m.target == f.id && m.kind == "CHALLENGE" && m.challenge_axis == "SEVERITY");
        if disputed {
            if let Some(r) = resolved.get_mut(&f.id) {
                r.reason =
                    format!("{} [severity disputed by a discourse CHALLENGE — see Discourse Audit for detail]", r.reason);
            }
        }
    }

    Ok((audit, resolved))
}

fn run_round_call(
    llm: &Llm,
    ctx: &str,
    spec: &Spec,
    findings: &[Finding],
    resolved: &HashMap<String, Resolution>,
    round: usize,
) -> Result<DiscourseRound> {
    let task = build_round_prompt(spec, findings, resolved, round);
    let dr: DiscourseRound = llm
        .json_ctx_typed(Some(ctx), &task, Some(DISCOURSE_SYSTEM))
        .with_context(|| format!("discourse round {round} failed"))?;
    Ok(dr)
}

#[cfg(test)]
mod tests {
    use super::test_support::test_finding;
    use super::*;

    #[test]
    fn run_confirms_merge_survivor_even_with_zero_direct_votes() {
        // Production scenario: two lenses (design, security) independently caught the same
        // bug. discourse MERGEs the design side into the security side, but there isn't a
        // single AGREE/CHALLENGE directly targeting the security side this round — before the
        // fix, the security side stayed at UNCERTAIN with zero votes and never factored into
        // the score at all.
        let mut findings = vec![
            test_finding("same bug from a design perspective too", "evidence A"),
            test_finding("SQL injection from a security perspective", "evidence B"),
        ];
        findings[0].id = "design-r1-1".to_string();
        findings[1].id = "security-r1-1".to_string();

        let response = serde_json::json!({
            "moves": [{"move": "CHALLENGE", "lens": "tests", "target": "design-r1-1", "detail": "d", "confidence": "high"}],
            "resolutions": [{"finding_id": "design-r1-1", "status": "MERGED", "merged_into": "security-r1-1", "reason": "same root cause"}],
            "surfaced": []
        })
        .to_string();
        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(vec![response], 0, usage);
        let input = Input {
            diff: "diff --git a/x b/x\n+++ b/x\n".to_string(),
            changed_files: vec!["x".to_string()],
            added_lines: 1,
            removed_lines: 0,
            requirements: None,
            conventions: None,
            deterministic_results: None,
            config: crate::core::RunConfig::default(),
        };

        let (_audit, resolved) = run(
            &llm,
            &super::test_support::test_spec(),
            &input,
            &mut findings,
            1,
            1,
        )
        .unwrap();

        assert_eq!(resolved["design-r1-1"].status, "MERGED");
        assert_eq!(
            resolved["security-r1-1"].status, "CONFIRMED",
            "the MERGE target (survivor) must be confirmed even without direct votes"
        );
    }

    #[test]
    fn run_confirms_finding_despite_severity_only_challenge() {
        // Production repro (#75): an SQL injection independently confirmed by 4 lenses got
        // pushed to UNCERTAIN and vanished from the report because of a single CHALLENGE
        // meaning "severity is overstated." Before the fix, this test's net would have been
        // AGREE(1.0) + CHALLENGE(-1.0) = 0.0, i.e. UNCERTAIN.
        let mut findings = vec![test_finding("SQL injection found", "evidence")];
        findings[0].id = "security-r1-1".to_string();

        let response = serde_json::json!({
            "moves": [
                {"move": "AGREE", "lens": "security", "target": "security-r1-1", "confidence": "high", "new_evidence": "e"},
                {"move": "CHALLENGE", "lens": "style", "target": "security-r1-1", "confidence": "high", "challenge_axis": "severity", "detail": "severity is overstated"}
            ],
            "resolutions": [],
            "surfaced": []
        })
        .to_string();
        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(vec![response], 0, usage);
        let input = Input {
            diff: "diff --git a/x b/x\n+++ b/x\n".to_string(),
            changed_files: vec!["x".to_string()],
            added_lines: 1,
            removed_lines: 0,
            requirements: None,
            conventions: None,
            deterministic_results: None,
            config: crate::core::RunConfig::default(),
        };

        let (_audit, resolved) = run(
            &llm,
            &super::test_support::test_spec(),
            &input,
            &mut findings,
            1,
            1,
        )
        .unwrap();

        assert_eq!(
            resolved["security-r1-1"].status, "CONFIRMED",
            "a severity-axis CHALLENGE must not subtract from the confirm vote"
        );
    }

    #[test]
    fn run_still_lets_existence_challenge_suppress_confirmation() {
        // Regression guard: excluding the severity axis from the vote must not neutralize the
        // existence axis too.
        let mut findings = vec![test_finding("a claim that might be fake", "evidence")];
        findings[0].id = "security-r1-1".to_string();

        let response = serde_json::json!({
            "moves": [
                {"move": "AGREE", "lens": "security", "target": "security-r1-1", "confidence": "high", "new_evidence": "e"},
                {"move": "CHALLENGE", "lens": "style", "target": "security-r1-1", "confidence": "high", "challenge_axis": "existence", "detail": "the evidence itself isn't in the diff"}
            ],
            "resolutions": [],
            "surfaced": []
        })
        .to_string();
        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(vec![response], 0, usage);
        let input = Input {
            diff: "diff --git a/x b/x\n+++ b/x\n".to_string(),
            changed_files: vec!["x".to_string()],
            added_lines: 1,
            removed_lines: 0,
            requirements: None,
            conventions: None,
            deterministic_results: None,
            config: crate::core::RunConfig::default(),
        };

        let (_audit, resolved) = run(
            &llm,
            &super::test_support::test_spec(),
            &input,
            &mut findings,
            1,
            1,
        )
        .unwrap();

        assert_eq!(
            resolved["security-r1-1"].status, "UNCERTAIN",
            "an existence-axis CHALLENGE must still block confirmation (net=0)"
        );
    }

    #[test]
    fn run_does_not_launder_a_disputed_finding_via_merge() {
        // #87 repro (at the run() level): design-r1-1 receives an existence-axis CHALLENGE and
        // gets MERGED into security-r1-1. security-r1-1 itself has zero direct votes — before
        // the fix, it would have gotten confirmed off merge_vote_weight's 1.0 alone.
        let mut findings = vec![
            test_finding("a suspicious SQL injection claim", "evidence A"),
            test_finding("a weak security-related point", "evidence B"),
        ];
        findings[0].id = "design-r1-1".to_string();
        findings[1].id = "security-r1-1".to_string();

        let response = serde_json::json!({
            "moves": [{
                "move": "CHALLENGE", "lens": "tests", "target": "design-r1-1",
                "detail": "the evidence itself isn't in the diff", "confidence": "high",
                "challenge_axis": "existence"
            }],
            "resolutions": [{
                "finding_id": "design-r1-1", "status": "MERGED",
                "merged_into": "security-r1-1", "reason": "merged as the same issue"
            }],
            "surfaced": []
        })
        .to_string();
        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(vec![response], 0, usage);
        let input = Input {
            diff: "diff --git a/x b/x\n+++ b/x\n".to_string(),
            changed_files: vec!["x".to_string()],
            added_lines: 1,
            removed_lines: 0,
            requirements: None,
            conventions: None,
            deterministic_results: None,
            config: crate::core::RunConfig::default(),
        };

        let (_audit, resolved) = run(
            &llm,
            &super::test_support::test_spec(),
            &input,
            &mut findings,
            1,
            1,
        )
        .unwrap();

        assert_eq!(resolved["design-r1-1"].status, "MERGED");
        assert_eq!(
            resolved["security-r1-1"].status, "UNCERTAIN",
            "the survivor must not be laundered into confirmation just because a disputed finding was merged into it"
        );
    }

    #[test]
    fn run_reverts_a_merged_resolution_whose_merged_into_does_not_name_a_real_finding() {
        // #140: mirrors fixcheck::verify_supersedes's treatment of an invalid superseded_by —
        // a hallucinated merged_into used to make the finding vanish (MERGED, excluded from
        // `confirmed`, but its vote weight never credited to anything real either).
        let mut findings = vec![test_finding("a claim", "evidence")];
        findings[0].id = "a".to_string();

        let response = serde_json::json!({
            "moves": [{
                "move": "CHALLENGE", "lens": "tests", "target": "nonexistent-target",
                "detail": "d", "confidence": "high", "challenge_axis": "existence"
            }],
            "resolutions": [{
                "finding_id": "a", "status": "MERGED",
                "merged_into": "does-not-exist", "reason": "merged as same issue"
            }],
            "surfaced": []
        })
        .to_string();
        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(vec![response], 0, usage);
        let input = Input {
            diff: "diff --git a/x b/x\n+++ b/x\n".to_string(),
            changed_files: vec!["x".to_string()],
            added_lines: 1,
            removed_lines: 0,
            requirements: None,
            conventions: None,
            deterministic_results: None,
            config: crate::core::RunConfig::default(),
        };

        let (_audit, resolved) = run(
            &llm,
            &super::test_support::test_spec(),
            &input,
            &mut findings,
            1,
            1,
        )
        .unwrap();

        assert_ne!(
            resolved["a"].status, "MERGED",
            "a MERGED resolution naming a nonexistent merged_into must not stand as-is"
        );
    }

    #[test]
    fn run_does_not_let_a_later_round_overwrite_an_already_confirmed_finding() {
        // #140: the prompt says resolutions should only judge UNRESOLVED/UNCERTAIN findings,
        // but nothing enforced it — a later round re-judging an already-CONFIRMED id used to
        // silently replace it.
        let mut findings = vec![
            test_finding("finding a", "evidence A"),
            test_finding("finding b", "evidence B"),
        ];
        findings[0].id = "a".to_string();
        findings[1].id = "b".to_string();

        // Round 1: confirms "a" outright; "b" stays unresolved (nothing targets it), so the
        // loop continues into round 2.
        let round1 = serde_json::json!({
            "moves": [{
                "move": "CHALLENGE", "lens": "tests", "target": "nonexistent-target",
                "detail": "d", "confidence": "high", "challenge_axis": "existence"
            }],
            "resolutions": [{"finding_id": "a", "status": "CONFIRMED", "reason": "round1 confirm"}],
            "surfaced": []
        })
        .to_string();
        // Round 2: legitimately confirms "b", but also tries to flip already-CONFIRMED "a" to
        // REJECTED — that flip must not take effect.
        let round2 = serde_json::json!({
            "moves": [{
                "move": "CHALLENGE", "lens": "tests", "target": "nonexistent-target-2",
                "detail": "d", "confidence": "high", "challenge_axis": "existence"
            }],
            "resolutions": [
                {"finding_id": "a", "status": "REJECTED", "reason": "round2 tries to flip"},
                {"finding_id": "b", "status": "CONFIRMED", "reason": "round2 confirm b"}
            ],
            "surfaced": []
        })
        .to_string();
        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(vec![round1, round2], 0, usage);
        let input = Input {
            diff: "diff --git a/x b/x\n+++ b/x\n".to_string(),
            changed_files: vec!["x".to_string()],
            added_lines: 1,
            removed_lines: 0,
            requirements: None,
            conventions: None,
            deterministic_results: None,
            config: crate::core::RunConfig::default(),
        };

        let (_audit, resolved) = run(
            &llm,
            &super::test_support::test_spec(),
            &input,
            &mut findings,
            2,
            1,
        )
        .unwrap();

        assert_eq!(
            resolved["a"].status, "CONFIRMED",
            "an already-CONFIRMED finding must not be flipped by a later round"
        );
        assert!(
            resolved["a"].reason.contains("round1 confirm"),
            "the original round-1 resolution must survive, got: {}",
            resolved["a"].reason
        );
        assert_eq!(resolved["b"].status, "CONFIRMED");
    }

    #[test]
    fn run_notes_a_severity_dispute_on_a_confirmed_finding_without_changing_its_status() {
        // #140: a severity-axis CHALLENGE deliberately never flips confirm/reject or touches
        // finding.severity (see #75) — but it should be visible next to the finding itself,
        // not only in the separate Discourse Audit table.
        let mut findings = vec![test_finding("a claim", "evidence")];
        findings[0].id = "a".to_string();
        findings[0].severity = "P0".to_string();

        let response = serde_json::json!({
            "moves": [{
                "move": "CHALLENGE", "lens": "tests", "target": "a",
                "detail": "severity is overstated", "confidence": "high", "challenge_axis": "severity"
            }],
            "resolutions": [{"finding_id": "a", "status": "CONFIRMED", "reason": "confirmed"}],
            "surfaced": []
        })
        .to_string();
        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(vec![response], 0, usage);
        let input = Input {
            diff: "diff --git a/x b/x\n+++ b/x\n".to_string(),
            changed_files: vec!["x".to_string()],
            added_lines: 1,
            removed_lines: 0,
            requirements: None,
            conventions: None,
            deterministic_results: None,
            config: crate::core::RunConfig::default(),
        };

        let (_audit, resolved) = run(
            &llm,
            &super::test_support::test_spec(),
            &input,
            &mut findings,
            1,
            1,
        )
        .unwrap();

        assert_eq!(
            resolved["a"].status, "CONFIRMED",
            "a severity-axis challenge must not change confirm/reject"
        );
        assert!(
            resolved["a"].reason.contains("severity disputed"),
            "the severity dispute must be visible on the finding itself, got: {}",
            resolved["a"].reason
        );
    }
}
