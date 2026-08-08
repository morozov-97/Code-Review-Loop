//! #129: `Llm::usage()` gives an end-of-run aggregate, but nothing captures per-run context
//! (which model, which spec, what got dropped/truncated, how many lenses actually returned
//! results) — so debugging "why did this run behave differently from last time" means
//! eyeballing stdout/stderr after the fact. This is v1: only what's already cheaply available
//! by the end of `run_review`, written alongside report.md/state.json.
//!
//! #172: added per-stage wall-clock timing (`StageTimings`) on top of that v1. Stage timings are
//! wired up from `pipeline/review.rs`'s existing stage boundaries — where two stages now run
//! concurrently (#168's lens selection + mandatory review, #170's requirements + human_voice),
//! the reported field covers the whole overlapping phase rather than fabricating an attribution
//! split neither stage's own thread reports back on its own. Also added `calls`: one
//! `CallRecord` (attempts, latency, success) per logical LLM call across the whole run, via
//! `Llm::with_calls_log` — this is the per-call half the header comment above used to say was
//! out of scope. Still doesn't attribute each call to a stage (every call site would need to
//! thread a stage label through, a larger change) or track queue-wait/peak-in-flight
//! (`CallGate` enforces the cap but doesn't currently expose its own occupancy over time) — a
//! call's rough stage can be inferred by cross-referencing its position in `calls` against
//! `stages`' timings, not read off directly.
use crate::llm::{CallRecord, Usage};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Wall-clock milliseconds per stage of `run_review`. Fields stay 0 when a stage didn't run at
/// all (e.g. `discourse_ms` on a clean diff with no findings, `fixcheck_ms` without --prior) —
/// that's a legitimate measurement (the stage really did take ~0ms because it was skipped), not
/// a missing value, so plain `u128` rather than `Option<u128>` throughout.
#[derive(Debug, Default, Serialize)]
pub(crate) struct StageTimings {
    /// #200: one `(tool id, elapsed ms)` entry per registered `DeterministicTool` that actually
    /// spawned — empty (not a per-tool `Option`) when `deterministic_results` was already
    /// supplied externally and no auto-detected tool ran at all. Was two separate `Option<u128>`
    /// fields (`semgrep_ms`, `cargo_audit_ms`) before the deterministic-tool plugin interface
    /// made the tool list open-ended.
    pub(crate) deterministic_tool_timings: Vec<(String, u128)>,
    /// Combined: lens selection (when --lenses isn't given) and mandatory-lens review run
    /// concurrently (#168), followed by optional-lens review once selection returns.
    pub(crate) lens_selection_and_review_ms: u128,
    pub(crate) discourse_ms: u128,
    pub(crate) fixcheck_ms: u128,
    /// Combined: requirements verification and human-voice rewrite run concurrently (#170).
    pub(crate) requirements_and_human_voice_ms: u128,
    pub(crate) total_ms: u128,
}

#[derive(Debug, Serialize)]
pub(crate) struct Manifest {
    pub(crate) codereview_version: String,
    pub(crate) model: Option<String>,
    pub(crate) cheap_model: Option<String>,
    pub(crate) spec_name: String,
    pub(crate) spec_path: String,
    /// Non-cryptographic hash of the spec file's raw content — cheap way to notice "this run
    /// used a different spec.toml than that one" without diffing the whole file.
    pub(crate) spec_hash: String,
    pub(crate) round: usize,
    pub(crate) selected_lenses: Vec<String>,
    pub(crate) successful_lens_count: usize,
    /// Warnings/failures collected along the way (lens failures, good_things failure, discourse
    /// skipped/failed, etc.) — same strings already shown in report.md's own notes section.
    pub(crate) stage_errors: Vec<String>,
    /// Files `prioritize_and_cap_diff` dropped from what was actually sent to the LLM.
    pub(crate) dropped_files: Vec<String>,
    pub(crate) usage: Usage,
    pub(crate) stages: StageTimings,
    /// #172: one entry per logical LLM call across the whole run (main + cheap model combined).
    pub(crate) calls: Vec<CallRecord>,
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read spec file for hashing: {}", path.display()))?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build(
    spec_path: &Path,
    spec_name: &str,
    model: Option<String>,
    cheap_model: Option<String>,
    round: usize,
    selected_lenses: Vec<String>,
    successful_lens_count: usize,
    stage_errors: Vec<String>,
    dropped_files: Vec<String>,
    usage: Usage,
    stages: StageTimings,
    calls: Vec<CallRecord>,
) -> Result<Manifest> {
    Ok(Manifest {
        codereview_version: env!("CARGO_PKG_VERSION").to_string(),
        model,
        cheap_model,
        spec_name: spec_name.to_string(),
        spec_path: spec_path.display().to_string(),
        spec_hash: hash_file(spec_path)?,
        round,
        selected_lenses,
        successful_lens_count,
        stage_errors,
        dropped_files,
        usage,
        stages,
        calls,
    })
}

pub(crate) fn write(out_dir: &Path, manifest: &Manifest) -> Result<PathBuf> {
    let path = out_dir.join("manifest.json");
    std::fs::write(&path, serde_json::to_string_pretty(manifest)?)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_file_is_stable_for_the_same_content_and_differs_for_different_content() {
        let dir = std::env::temp_dir().join("codereview-loop-manifest-hash-test");
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.toml");
        let b = dir.join("b.toml");
        std::fs::write(&a, "name = \"x\"").unwrap();
        std::fs::write(&b, "name = \"y\"").unwrap();

        let hash_a1 = hash_file(&a).unwrap();
        let hash_a2 = hash_file(&a).unwrap();
        let hash_b = hash_file(&b).unwrap();

        assert_eq!(hash_a1, hash_a2, "hashing the same content must be stable");
        assert_ne!(hash_a1, hash_b, "different content must hash differently");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_and_write_roundtrips_through_json() {
        let dir = std::env::temp_dir().join("codereview-loop-manifest-write-test");
        std::fs::create_dir_all(&dir).unwrap();
        let spec_path = dir.join("spec.toml");
        std::fs::write(&spec_path, "name = \"t\"").unwrap();

        let manifest = build(
            &spec_path,
            "t",
            Some("some-model".to_string()),
            None,
            2,
            vec!["design".to_string()],
            1,
            vec!["good_things: boom".to_string()],
            vec!["Cargo.lock".to_string()],
            Usage::default(),
            StageTimings {
                total_ms: 1234,
                ..Default::default()
            },
            vec![CallRecord {
                attempts: 1,
                latency_ms: 42,
                success: true,
                model: Some("some-model".to_string()),
            }],
        )
        .unwrap();

        let path = write(&dir, &manifest).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("\"round\": 2"));
        assert!(contents.contains("some-model"));
        assert!(contents.contains("Cargo.lock"));
        assert!(contents.contains("\"latency_ms\": 42"));
        assert!(contents.contains("\"total_ms\": 1234"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
