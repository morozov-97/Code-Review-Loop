use crate::lens::Finding;
use serde::{Deserialize, Serialize};

/// All fields use `#[serde(default)]` — we hit a real case where the LLM omitted one of
/// these (usually `detail`) and it killed the entire discourse round with a schema mismatch,
/// so no report came out at all (canary_flutter production test). If `kind`/`target` is empty,
/// that move can't target any finding anyway, so it just becomes a silent no-op in the
/// vote/count (safe per the quantify logic) — better than letting a missing field kill the
/// whole round.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Move {
    #[serde(rename = "move", default)]
    pub kind: String, // AGREE|CHALLENGE|CONNECT|SURFACE
    #[serde(default)]
    pub lens: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub new_evidence: String,
    #[serde(default)]
    pub confidence: String, // high|medium|low (only meaningful for AGREE/CHALLENGE)
    /// Only meaningful for CHALLENGE: existence|severity. See normalize_challenge_axis for normalization.
    #[serde(default)]
    pub challenge_axis: String,
}

const VALID_CHALLENGE_AXES: [&str; 2] = ["EXISTENCE", "SEVERITY"];

/// Found in production: 4 lenses independently caught the same SQL injection, but a single
/// CHALLENGE meaning "severity is overstated" got tallied with the same vote weight as an
/// existence dispute, pushing the finding to UNCERTAIN and making it vanish from the report
/// entirely (#75). If challenge_axis is missing or outside the schema, we safely fall back to
/// "SEVERITY" (no effect on the vote) — the same reasoning behind this whole session's
/// principle of "fail in the direction that can't lose a finding": defaulting to existence
/// instead would silently reproduce the old "a severity disagreement erases the finding" bug
/// every time the LLM fails to fill in this new field.
pub fn normalize_challenge_axis(raw: &str) -> String {
    let upper = raw.trim().to_ascii_uppercase();
    if VALID_CHALLENGE_AXES.contains(&upper.as_str()) {
        upper
    } else {
        "SEVERITY".to_string()
    }
}

const VALID_MOVE_KINDS: [&str; 4] = ["AGREE", "CHALLENGE", "CONNECT", "SURFACE"];

/// Found in production (#94): every other LLM-controlled categorical field here
/// (severity/status/challenge_axis) gets normalized, but `kind` didn't — an LLM returning
/// "Challenge" instead of "CHALLENGE" made an existence dispute silently contribute zero vote
/// weight instead of suppressing confirmation, and also slipped past the "at least one
/// CHALLENGE per round" check. Unrecognized values fall back to "SURFACE" — the same no-vote
/// treatment CONNECT/SURFACE already get — so a garbled kind can't accidentally count as an
/// AGREE or a suppressed CHALLENGE.
pub fn normalize_move_kind(raw: &str) -> String {
    let upper = raw.trim().to_ascii_uppercase();
    if VALID_MOVE_KINDS.contains(&upper.as_str()) {
        upper
    } else {
        "SURFACE".to_string()
    }
}

/// Found in production (#94): `confidence_weight` only matches exact lowercase "high"/"low",
/// so a case variant like "Low" silently fell through to the 0.6 default — which happens to
/// sit exactly on the vote threshold, so a single miscased low-confidence vote could wrongly
/// confirm a finding on its own. Lowercasing here is enough: genuinely unrecognized values
/// still hit confidence_weight's own "unspecified" fallback exactly as before.
pub fn normalize_confidence(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

/// All fields use `#[serde(default)]` — same reason as Move above (we've seen one missing
/// field kill an entire round in production) applies here too. Even if status arrives
/// unnormalized (including an empty string), `normalize_status` safely falls back to
/// UNCERTAIN.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolution {
    #[serde(default)]
    pub finding_id: String,
    #[serde(default)]
    pub status: String, // CONFIRMED|REJECTED|MERGED|UNCERTAIN
    #[serde(default)]
    pub merged_into: String,
    #[serde(default)]
    pub reason: String,
}

const VALID_RESOLUTION_STATUSES: [&str; 4] = ["CONFIRMED", "REJECTED", "MERGED", "UNCERTAIN"];

/// Same issue as severity/requirements.status: quantify.rs/report.rs match this field with
/// an exact string comparison, so if the LLM strays on case or whitespace, it silently
/// disappears from all three of score/verdict/report at once (neither CONFIRMED nor REJECTED,
/// so it's invisible entirely). Failure must land on the safe side (UNCERTAIN — eligible for
/// re-judgment in the next round).
pub fn normalize_status(raw: &str) -> String {
    let upper = raw.trim().to_ascii_uppercase();
    if VALID_RESOLUTION_STATUSES.contains(&upper.as_str()) {
        upper
    } else {
        "UNCERTAIN".to_string()
    }
}

/// Same reason as lens.rs::finding_id: without including outer_round (the overall pipeline
/// round carried across via --prior), the discourse-internal round restarts from 1 on every
/// outer_round call, so two different outer_rounds can coincidentally produce the same
/// (round, index) pair and end up with two completely different findings sharing one id.
pub fn surface_id(outer_round: usize, round: usize, index: usize) -> String {
    format!("surface-o{outer_round}-r{round}-{index}")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct DiscourseRound {
    #[serde(default)]
    pub(super) moves: Vec<Move>,
    #[serde(default)]
    pub(super) resolutions: Vec<Resolution>,
    #[serde(default)]
    pub(super) surfaced: Vec<Finding>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_deserializes_when_detail_is_missing() {
        // Production repro: in a canary_flutter review, the LLM omitted the detail field and
        // it killed the entire round.
        let json = serde_json::json!({
            "move": "CHALLENGE",
            "lens": "tests",
            "target": "every_line-r1-1",
            "confidence": "high"
        });
        let m: Move =
            serde_json::from_value(json).expect("should parse successfully even without detail");
        assert_eq!(m.kind, "CHALLENGE");
        assert_eq!(m.detail, "");
    }

    #[test]
    fn move_deserializes_when_only_kind_is_present() {
        let json = serde_json::json!({"move": "SURFACE"});
        let m: Move = serde_json::from_value(json)
            .expect("should parse successfully even with all other fields missing");
        assert_eq!(m.kind, "SURFACE");
        assert_eq!(m.target, "");
        assert_eq!(m.lens, "");
    }

    #[test]
    fn discourse_round_survives_resolution_missing_status() {
        // Same class of failure as Move.detail: if one element of the resolutions array is
        // missing status, parsing dies for the whole round, including moves/surfaced.
        let json = serde_json::json!({
            "moves": [],
            "resolutions": [{"finding_id": "security-r1-1"}],
            "surfaced": []
        });
        let dr: DiscourseRound = serde_json::from_value(json)
            .expect("should parse the whole round successfully even without status");
        assert_eq!(dr.resolutions[0].finding_id, "security-r1-1");
        assert_eq!(dr.resolutions[0].status, "");
    }

    #[test]
    fn normalize_status_passes_through_valid_values() {
        for s in ["CONFIRMED", "REJECTED", "MERGED", "UNCERTAIN"] {
            assert_eq!(normalize_status(s), s);
        }
    }

    #[test]
    fn normalize_status_is_case_insensitive() {
        assert_eq!(normalize_status("Confirmed"), "CONFIRMED");
    }

    #[test]
    fn normalize_status_falls_back_to_uncertain_on_unknown_or_empty_value() {
        assert_eq!(normalize_status("IN_PROGRESS"), "UNCERTAIN");
        assert_eq!(normalize_status(""), "UNCERTAIN");
    }

    #[test]
    fn normalize_move_kind_passes_through_valid_values() {
        for k in ["AGREE", "CHALLENGE", "CONNECT", "SURFACE"] {
            assert_eq!(normalize_move_kind(k), k);
        }
    }

    #[test]
    fn normalize_move_kind_is_case_insensitive() {
        assert_eq!(normalize_move_kind("Challenge"), "CHALLENGE");
        assert_eq!(normalize_move_kind("agree"), "AGREE");
    }

    #[test]
    fn normalize_move_kind_falls_back_to_surface_on_unknown_or_empty_value() {
        assert_eq!(normalize_move_kind("REJECT"), "SURFACE");
        assert_eq!(normalize_move_kind(""), "SURFACE");
    }

    #[test]
    fn normalize_confidence_lowercases_case_variants() {
        assert_eq!(normalize_confidence("Low"), "low");
        assert_eq!(normalize_confidence("HIGH"), "high");
        assert_eq!(normalize_confidence("Medium"), "medium");
    }

    #[test]
    fn normalize_challenge_axis_passes_through_valid_values_case_insensitively() {
        assert_eq!(normalize_challenge_axis("existence"), "EXISTENCE");
        assert_eq!(normalize_challenge_axis("Severity"), "SEVERITY");
    }

    #[test]
    fn normalize_challenge_axis_falls_back_to_severity_on_unknown_or_empty_value() {
        // The default has to be "a dispute that only takes issue with severity" so the
        // finding's existence isn't silently negated — this falls to the safe side even if
        // the LLM leaves the field unfilled (or sends an out-of-schema value).
        assert_eq!(normalize_challenge_axis(""), "SEVERITY");
        assert_eq!(normalize_challenge_axis("scope"), "SEVERITY");
    }

    #[test]
    fn surface_id_differs_across_outer_prior_rounds_for_the_same_position() {
        // Before the fix: the discourse-internal round restarted from 1 on every --prior
        // call, so outer_round 1 and 2's (round=1, index=1) both produced the same
        // "surface-r1-1".
        assert_ne!(surface_id(1, 1, 1), surface_id(2, 1, 1));
    }

    #[test]
    fn surface_id_differs_across_inner_rounds_and_positions() {
        assert_ne!(surface_id(1, 1, 1), surface_id(1, 2, 1));
        assert_ne!(surface_id(1, 1, 1), surface_id(1, 1, 2));
    }
}
