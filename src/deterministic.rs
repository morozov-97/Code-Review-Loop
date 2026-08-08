//! #200: common interface every deterministic (non-LLM-judged) tool implements, so
//! `pipeline/review.rs` can spawn/join/merge N of them generically instead of one hand-written
//! background-thread block per tool. That per-tool-block shape (established by #169 for semgrep,
//! copy-pasted for cargo-audit by #164) worked for two tools but doesn't scale — this is the
//! abstraction a real third tool would need. Follows the same extraction pattern `procutil.rs`
//! did for the raw subprocess mechanics underneath this.

use serde_json::Value;
use std::process::Output;
use std::time::Duration;

/// A local, deterministic (not LLM-judged) analysis tool — semgrep, cargo-audit, or a future
/// language-specific linter/compiler/coverage tool. Implementors are typically zero-sized marker
/// structs (`SemgrepTool`, `CargoAuditTool`); all the actual behavior lives in these methods.
pub(crate) trait DeterministicTool: Send + Sync {
    /// Short id used in logs and per-tool manifest timings (e.g. "semgrep", "cargo_audit").
    fn id(&self) -> &'static str;
    /// Binary to look up via `procutil::which`.
    fn binary(&self) -> &'static str;
    /// Default timeout if the caller has no tighter `--deadline-minutes` budget to cap it by.
    fn default_timeout(&self) -> Duration;
    /// True if this tool needs the diff's changed files to do anything meaningful — if true and
    /// none of them exist on disk, `try_run` bails before spawning anything (matches semgrep's
    /// existing behavior: no point scanning nothing). False means the tool always runs
    /// regardless (matches cargo-audit, which scans the whole dependency tree, not specific
    /// files that changed).
    fn requires_changed_files(&self) -> bool;
    /// Full argument list to invoke, given the changed files that exist on disk (already
    /// filtered to existence). Ignored entirely by a tool where `requires_changed_files()` is
    /// false.
    fn args(&self, existing_files: &[&str]) -> Vec<String>;
    /// Parse raw process output into `deterministic_results` shape
    /// (`{"<check_id>": {"status": "pass"|"fail"|"error", "evidence": "..."}, ...}`), or `None`
    /// on any failure or unrecognized shape — every implementor is expected to fall back to
    /// `NOT_RUN` (by returning `None`) rather than fabricate a guessed result.
    fn parse(&self, output: &Output) -> Option<Value>;
}

/// Shared by every `DeterministicTool`: PATH lookup, changed-files gating, spawn+wait, parse. A
/// tool's own module just implements the trait above; this is the one place that actually calls
/// it, so no tool needs its own copy of "how do I run a subprocess and hand off to my parser."
pub(crate) fn try_run(
    tool: &dyn DeterministicTool,
    changed_files: &[String],
    timeout: Duration,
) -> Option<Value> {
    let bin = crate::procutil::which(tool.binary())?;
    let existing: Vec<&str> = changed_files
        .iter()
        .map(|s| s.as_str())
        .filter(|f| std::path::Path::new(f).exists())
        .collect();
    if tool.requires_changed_files() && existing.is_empty() {
        return None;
    }
    let args = tool.args(&existing);
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = crate::procutil::spawn_and_wait(&bin, &args_ref, timeout)?;
    tool.parse(&output)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeTool {
        requires_changed_files: bool,
        parsed: Option<Value>,
    }

    impl DeterministicTool for FakeTool {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn binary(&self) -> &'static str {
            "sh"
        }
        fn default_timeout(&self) -> Duration {
            Duration::from_secs(5)
        }
        fn requires_changed_files(&self) -> bool {
            self.requires_changed_files
        }
        fn args(&self, _existing_files: &[&str]) -> Vec<String> {
            vec!["-c".to_string(), "echo hi".to_string()]
        }
        fn parse(&self, _output: &Output) -> Option<Value> {
            self.parsed.clone()
        }
    }

    #[test]
    fn try_run_bails_without_spawning_when_changed_files_are_required_but_none_exist() {
        let tool = FakeTool {
            requires_changed_files: true,
            parsed: Some(serde_json::json!({"x": {"status": "pass"}})),
        };
        let result = try_run(
            &tool,
            &["this/file/does/not/exist/on/disk".to_string()],
            Duration::from_secs(5),
        );
        assert!(result.is_none());
    }

    #[test]
    fn try_run_runs_regardless_of_changed_files_when_not_required() {
        let tool = FakeTool {
            requires_changed_files: false,
            parsed: Some(serde_json::json!({"x": {"status": "pass"}})),
        };
        let result = try_run(&tool, &[], Duration::from_secs(5));
        assert_eq!(result, Some(serde_json::json!({"x": {"status": "pass"}})));
    }

    #[test]
    fn try_run_returns_none_when_parse_returns_none() {
        let tool = FakeTool {
            requires_changed_files: false,
            parsed: None,
        };
        let result = try_run(&tool, &[], Duration::from_secs(5));
        assert!(result.is_none());
    }
}
