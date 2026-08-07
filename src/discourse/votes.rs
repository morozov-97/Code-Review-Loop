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
///
/// #140: two rules the prompt states but didn't used to be locally enforced:
/// - an AGREE with empty `new_evidence` counts the same as one with none — the prompt asks for
///   AGREE "only when there's new file:line evidence," this makes that a real requirement
///   instead of trusting the LLM's own compliance.
/// - the same lens issuing more than one AGREE/CHALLENGE for the same target within a single
///   round (a response quirk, not a second independent signal) used to double-count every
///   repeat instead of counting once per (round, lens).
///
/// #149: the (round, lens) dedup slot used to be claimed by the *first* move seen regardless of
/// its weight — a no-op move (CONNECT/SURFACE, or an AGREE with empty new_evidence) listed
/// before a real vote-bearing move from the same lens/round claimed the slot first, silently
/// dropping the real one right after it. The slot is now only claimed by a move that actually
/// carries non-zero weight, so a no-op move never blocks a real vote that follows it.
pub(super) fn direct_vote_net(audit: &[DiscourseAudit], target_id: &str) -> f64 {
    let mut seen: std::collections::HashSet<(usize, &str)> = std::collections::HashSet::new();
    let mut net = 0.0;
    for a in audit {
        for m in &a.moves {
            if m.target != target_id {
                continue;
            }
            let weight = match m.kind.as_str() {
                "AGREE" if !m.new_evidence.trim().is_empty() => confidence_weight(&m.confidence),
                "CHALLENGE" if m.challenge_axis == "EXISTENCE" => -confidence_weight(&m.confidence),
                _ => 0.0,
            };
            if weight == 0.0 {
                continue;
            }
            if !seen.insert((a.round, m.lens.as_str())) {
                continue;
            }
            net += weight;
        }
    }
    net
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

    #[test]
    fn direct_vote_net_ignores_an_agree_with_no_new_evidence() {
        // #140: the prompt asks for AGREE "only when there's new file:line evidence" — an
        // empty new_evidence must not carry the same weight as a real one.
        let audit = vec![DiscourseAudit {
            round: 1,
            moves: vec![Move {
                kind: "AGREE".to_string(),
                lens: "tests".to_string(),
                target: "f1".to_string(),
                detail: "agreed".to_string(),
                new_evidence: String::new(),
                confidence: "high".to_string(),
                challenge_axis: String::new(),
            }],
        }];
        assert_eq!(direct_vote_net(&audit, "f1"), 0.0);
    }

    #[test]
    fn direct_vote_net_counts_an_agree_with_new_evidence() {
        let audit = vec![DiscourseAudit {
            round: 1,
            moves: vec![agree("f1", "high")],
        }];
        assert_eq!(direct_vote_net(&audit, "f1"), confidence_weight("high"));
    }

    #[test]
    fn direct_vote_net_counts_a_repeated_agree_from_the_same_lens_in_the_same_round_only_once() {
        // #140: two AGREE moves from the same lens targeting the same finding within one round
        // used to double the vote weight instead of counting once.
        let audit = vec![DiscourseAudit {
            round: 1,
            moves: vec![agree("f1", "high"), agree("f1", "high")],
        }];
        assert_eq!(
            direct_vote_net(&audit, "f1"),
            confidence_weight("high"),
            "a duplicate AGREE from the same lens in the same round must not double the vote"
        );
    }

    #[test]
    fn direct_vote_net_counts_the_same_lens_again_in_a_later_round() {
        // Dedup is scoped to (round, lens) — a lens reaffirming across genuinely different
        // rounds is still meaningful and must still count each time.
        let audit = vec![
            DiscourseAudit {
                round: 1,
                moves: vec![agree("f1", "high")],
            },
            DiscourseAudit {
                round: 2,
                moves: vec![agree("f1", "high")],
            },
        ];
        assert_eq!(
            direct_vote_net(&audit, "f1"),
            2.0 * confidence_weight("high")
        );
    }

    #[test]
    fn direct_vote_net_counts_two_different_lenses_in_the_same_round() {
        let audit = vec![DiscourseAudit {
            round: 1,
            moves: vec![
                agree("f1", "high"),
                Move {
                    lens: "other-lens".to_string(),
                    ..agree("f1", "high")
                },
            ],
        }];
        assert_eq!(
            direct_vote_net(&audit, "f1"),
            2.0 * confidence_weight("high")
        );
    }

    #[test]
    fn direct_vote_net_does_not_let_a_leading_noop_move_consume_the_dedup_slot() {
        // #149: a CONNECT (no vote weight) from a lens, followed by a real AGREE from the same
        // lens targeting the same finding in the same round, used to have the CONNECT claim
        // the (round, lens) dedup slot first — silently dropping the real AGREE right after it.
        let audit = vec![DiscourseAudit {
            round: 1,
            moves: vec![
                Move {
                    kind: "CONNECT".to_string(),
                    lens: "security".to_string(),
                    target: "f1".to_string(),
                    detail: "connects to another finding".to_string(),
                    new_evidence: String::new(),
                    confidence: String::new(),
                    challenge_axis: String::new(),
                },
                Move {
                    lens: "security".to_string(),
                    ..agree("f1", "high")
                },
            ],
        }];
        assert_eq!(
            direct_vote_net(&audit, "f1"),
            confidence_weight("high"),
            "the real AGREE right after a no-op CONNECT from the same lens must still count"
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
