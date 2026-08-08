use crate::procutil::{spawn_and_wait, which};
use std::time::Duration;

/// Higher than semgrep::DEFAULT_TIMEOUT (120s) — `cargo audit`'s first run on a machine clones
/// the advisory-db git repo, which semgrep's static analysis has no equivalent of.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(180);

/// #164: a second deterministic source alongside semgrep, for this project's own ecosystem
/// (Rust/Cargo) — the point isn't that this specific tool matters most, it's proving the
/// `--deterministic-results` interface generalizes beyond whatever semgrep happens to emit.
/// Dependency CVEs are exactly the class of problem a deterministic tool catches more reliably
/// (and more cheaply) than an LLM reading a diff. Populates the `dependency_sca` check
/// (previously always `NOT_RUN` unless supplied externally via `--deterministic-results`) — see
/// README's "Deterministic results JSON shape" section for the full contract every entry here
/// (and semgrep's) follows.
pub fn try_run(timeout: Duration) -> Option<serde_json::Value> {
    let bin = which("cargo")?;
    let output = spawn_and_wait(&bin, &["audit", "--json"], timeout)?;
    // #144-style leniency: cargo-audit not being installed as a subcommand, or a network
    // failure fetching the advisory database, both land here as "stdout wasn't valid JSON" —
    // falls back to NOT_RUN like every other failure mode in this module, never a guessed result.
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    build_deterministic_result(&v)
}

/// Split out for the same reason `semgrep::build_deterministic_results` is: unit-testable
/// without shelling out to a real `cargo audit`.
fn build_deterministic_result(v: &serde_json::Value) -> Option<serde_json::Value> {
    let vulnerabilities = v.get("vulnerabilities")?;
    let count = vulnerabilities
        .get("count")
        .and_then(|c| c.as_u64())
        .unwrap_or(0);
    let found = vulnerabilities
        .get("found")
        .and_then(|f| f.as_bool())
        .unwrap_or(count > 0);
    let plural = if count == 1 { "y" } else { "ies" };
    Some(serde_json::json!({
        "dependency_sca": {
            "status": if found { "fail" } else { "pass" },
            "evidence": format!("cargo audit found {count} known vulnerabilit{plural} in the dependency tree"),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_deterministic_result_reports_pass_when_no_vulnerabilities_found() {
        let v = serde_json::json!({"vulnerabilities": {"found": false, "count": 0, "list": []}});
        let out = build_deterministic_result(&v).unwrap();
        assert_eq!(out["dependency_sca"]["status"], "pass");
    }

    #[test]
    fn build_deterministic_result_reports_fail_when_vulnerabilities_are_found() {
        let v = serde_json::json!({"vulnerabilities": {"found": true, "count": 2, "list": []}});
        let out = build_deterministic_result(&v).unwrap();
        assert_eq!(out["dependency_sca"]["status"], "fail");
        assert!(out["dependency_sca"]["evidence"]
            .as_str()
            .unwrap()
            .contains("2 known vulnerabilities"));
    }

    #[test]
    fn build_deterministic_result_falls_back_to_the_count_when_found_is_missing() {
        // Defensive: don't assume every cargo-audit version includes `found` explicitly.
        let v = serde_json::json!({"vulnerabilities": {"count": 1, "list": []}});
        let out = build_deterministic_result(&v).unwrap();
        assert_eq!(out["dependency_sca"]["status"], "fail");
    }

    #[test]
    fn build_deterministic_result_returns_none_when_the_shape_is_unrecognized() {
        let v = serde_json::json!({"unexpected": "shape"});
        assert!(build_deterministic_result(&v).is_none());
    }
}
