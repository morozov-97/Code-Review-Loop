use super::schema::Resolution;
use super::DiscourseAudit;
use std::collections::HashMap;

/// ReConcile-style confidence bucket → weight. Instead of discarding leftover UNCERTAIN
/// findings without a verdict once rounds run out, we make a final call from accumulated
/// AGREE/CHALLENGE votes.
pub(super) fn confidence_weight(c: &str) -> f64 {
    match c {
        "high" => 1.0,
        "low" => 0.3,
        _ => 0.6, // medium and unspecified
    }
}

pub(super) const VOTE_THRESHOLD: f64 = 0.6;

/// Tallies only the AGREE/CHALLENGE(existence) votes that directly target one finding
/// (excludes merge_vote_weight itself — this lets merge_vote_weight, when it calls this
/// function recursively, isolate just "how much this finding itself was trusted").
pub(super) fn direct_vote_net(audit: &[DiscourseAudit], target_id: &str) -> f64 {
    audit
        .iter()
        .flat_map(|a| a.moves.iter())
        .filter(|m| m.target == target_id)
        .map(|m| match m.kind.as_str() {
            "AGREE" => confidence_weight(&m.confidence),
            "CHALLENGE" if m.challenge_axis == "EXISTENCE" => -confidence_weight(&m.confidence),
            _ => 0.0,
        })
        .sum()
}

/// When finding X gets MERGED into Y, discourse has decided "X and Y are the same issue,"
/// not that X is fake — but the MERGE itself isn't an AGREE/CHALLENGE vote targeting the
/// survivor (Y), so if nothing directly targets Y, it's left with no votes at all and stays
/// UNCERTAIN, never factoring into the score (two lenses independently catching the same bug
/// then evaporates to a score of 0). We treat the MERGED verdict itself as equivalent to one
/// high-confidence AGREE vote and add it to the survivor's tally.
///
/// However, if X itself was MERGED while its net vote was negative from an existence-axis
/// CHALLENGE (i.e. the dispute against it was winning), we must not ignore that dispute signal
/// and launder the survivor into a confirmation — such a merge earns no credit (0, not a
/// penalty — the merge itself isn't necessarily wrong). Chains (A→B→C) are also followed
/// recursively, judging each hop independently by the same standard — a visited set guards
/// against cycles.
pub(super) fn merge_vote_weight(
    resolved: &HashMap<String, Resolution>,
    audit: &[DiscourseAudit],
    target_id: &str,
) -> f64 {
    let mut total = 0.0;
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    visited.insert(target_id.to_string());
    let mut queue = vec![target_id.to_string()];
    while let Some(id) = queue.pop() {
        for r in resolved.values() {
            if r.status != "MERGED" || r.merged_into != id {
                continue;
            }
            if !visited.insert(r.finding_id.clone()) {
                continue; // cycle (already visited) — don't count it again
            }
            if direct_vote_net(audit, &r.finding_id) >= 0.0 {
                total += confidence_weight("high");
            }
            queue.push(r.finding_id.clone());
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::super::schema::{normalize_confidence, normalize_move_kind, Move};
    use super::*;

    #[test]
    fn direct_vote_net_treats_miscased_challenge_and_confidence_like_canonical_ones() {
        // Regression for #94: before normalization, a "Challenge"/"Low" pair from the LLM
        // silently contributed 0.0 instead of -0.3, because the vote-tally match arms compare
        // against the exact uppercase/lowercase literals.
        let canonical = vec![DiscourseAudit {
            round: 1,
            moves: vec![existence_challenge("f1", "low")],
        }];
        let miscased = vec![DiscourseAudit {
            round: 1,
            moves: vec![Move {
                kind: normalize_move_kind("Challenge"),
                confidence: normalize_confidence("Low"),
                target: "f1".to_string(),
                challenge_axis: "EXISTENCE".to_string(),
                ..Default::default()
            }],
        }];
        assert_eq!(
            direct_vote_net(&canonical, "f1"),
            direct_vote_net(&miscased, "f1")
        );
    }

    fn merged_resolution(id: &str, into: &str) -> Resolution {
        Resolution {
            finding_id: id.to_string(),
            status: "MERGED".to_string(),
            merged_into: into.to_string(),
            reason: String::new(),
        }
    }

    fn existence_challenge(target: &str, confidence: &str) -> Move {
        Move {
            kind: "CHALLENGE".to_string(),
            lens: "tests".to_string(),
            target: target.to_string(),
            detail: "disputed".to_string(),
            new_evidence: String::new(),
            confidence: confidence.to_string(),
            challenge_axis: "EXISTENCE".to_string(),
        }
    }

    fn agree(target: &str, confidence: &str) -> Move {
        Move {
            kind: "AGREE".to_string(),
            lens: "tests".to_string(),
            target: target.to_string(),
            detail: "agreed".to_string(),
            new_evidence: "e".to_string(),
            confidence: confidence.to_string(),
            challenge_axis: String::new(),
        }
    }

    #[test]
    fn merge_vote_weight_counts_only_merges_targeting_this_id() {
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), merged_resolution("a", "b"));
        resolved.insert(
            "c".to_string(),
            Resolution {
                finding_id: "c".to_string(),
                status: "CONFIRMED".to_string(),
                merged_into: String::new(),
                reason: String::new(),
            },
        );
        let audit = Vec::new();
        assert_eq!(
            merge_vote_weight(&resolved, &audit, "b"),
            confidence_weight("high")
        );
        assert_eq!(merge_vote_weight(&resolved, &audit, "c"), 0.0);
        assert_eq!(merge_vote_weight(&resolved, &audit, "nonexistent"), 0.0);
    }

    #[test]
    fn merge_vote_weight_does_not_credit_a_disputed_source() {
        // #87: if A gets MERGED into B while its net vote is negative from an existence-axis
        // CHALLENGE, that dispute must not be ignored to launder B into a confirmation.
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), merged_resolution("a", "b"));
        let audit = vec![DiscourseAudit {
            round: 1,
            moves: vec![existence_challenge("a", "high")],
        }];
        assert_eq!(
            merge_vote_weight(&resolved, &audit, "b"),
            0.0,
            "a finding merged while still disputed must not get credit"
        );
    }

    #[test]
    fn merge_vote_weight_still_credits_an_undisputed_source() {
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), merged_resolution("a", "b"));
        let audit = Vec::new(); // no disputes
        assert_eq!(
            merge_vote_weight(&resolved, &audit, "b"),
            confidence_weight("high")
        );
    }

    #[test]
    fn merge_vote_weight_propagates_through_a_merge_chain() {
        // #88: if A→B→C merges across two hops, A's signal must propagate all the way to C too.
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), merged_resolution("a", "b"));
        resolved.insert("b".to_string(), merged_resolution("b", "c"));
        let audit = vec![DiscourseAudit {
            round: 1,
            moves: vec![agree("a", "high")],
        }];
        assert_eq!(
            merge_vote_weight(&resolved, &audit, "c"),
            2.0 * confidence_weight("high"),
            "both hops, A (agreed) and B, must get credit"
        );
    }

    #[test]
    fn merge_vote_weight_ignores_cycles() {
        let mut resolved = HashMap::new();
        resolved.insert("a".to_string(), merged_resolution("a", "b"));
        resolved.insert("b".to_string(), merged_resolution("b", "a"));
        let audit = Vec::new();
        // It's enough that this terminates without an infinite loop — termination matters more
        // than the exact value.
        let _ = merge_vote_weight(&resolved, &audit, "a");
    }
}
