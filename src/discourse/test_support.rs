//! Shared test fixtures for discourse's submodules (mod.rs and prompt.rs both need a minimal
//! Spec/Finding to exercise prompt building and full `run()` orchestration).
use crate::lens::Finding;
use crate::spec::Spec;

pub(super) fn test_spec() -> Spec {
    Spec {
        name: "test".to_string(),
        context: String::new(),
        lenses: Vec::new(),
        deterministic_checks: Vec::new(),
        labels: vec!["bug".to_string()],
        diff_size_limit: 0,
        test_path_patterns: Vec::new(),
        doc_path_patterns: Vec::new(),
        ignored_path_patterns: Vec::new(),
        scoring: Default::default(),
        discourse: Default::default(),
    }
}

pub(super) fn test_finding(claim: &str, evidence: &str) -> Finding {
    Finding {
        id: "design-r1-1".to_string(),
        file: "x.rs".to_string(),
        line: "1".to_string(),
        claim: claim.to_string(),
        evidence: evidence.to_string(),
        impact: String::new(),
        severity: "P1".to_string(),
        label: "possible bug".to_string(),
        confidence: "high".to_string(),
        recommendation: String::new(),
        lens: "design".to_string(),
        reviewer: "Reviewer".to_string(),
        evidence_unverified: false,
    }
}
