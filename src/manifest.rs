//! #129: `Llm::usage()` gives an end-of-run aggregate, but nothing captures per-run context
//! (which model, which spec, what got dropped/truncated, how many lenses actually returned
//! results) — so debugging "why did this run behave differently from last time" means
//! eyeballing stdout/stderr after the fact. This is v1: only what's already cheaply available
//! by the end of `run_review`, written alongside report.md/state.json. Not attempting the
//! fuller per-call/per-stage latency and retry tracking the issue also floats — that needs
//! instrumenting `Llm` itself, a bigger change than this pass.
use crate::llm::Usage;
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

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
        )
        .unwrap();

        let path = write(&dir, &manifest).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("\"round\": 2"));
        assert!(contents.contains("some-model"));
        assert!(contents.contains("Cargo.lock"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
