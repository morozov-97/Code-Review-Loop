use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Review lens (an item selected from Google eng-practices' 12 axes based on the diff's nature).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Lens {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub guide: String,
    /// If true, always force-include in lens selection (Functionality, Good Things).
    #[serde(default)]
    pub always: bool,
    /// Signal that prompts selection of this lens (inserted verbatim into the selection prompt).
    #[serde(default)]
    pub signal: String,
    /// Characterized persona name (empty = no persona). Aims to suppress sycophancy.
    #[serde(default)]
    pub persona_name: String,
    /// One-line summary of the persona's perspective/principles.
    #[serde(default)]
    pub persona_voice: String,
    /// generalist | specialist | famous_engineer | custom. Display-only, doesn't affect logic.
    #[serde(default)]
    pub tier: String,
    /// #162: overrides `--model`/`--cheap-model` for just this lens, when set. Every persona
    /// otherwise runs on the same underlying model differentiated only by system prompt — their
    /// failure modes are more likely to be correlated than genuinely independent reviewers' would
    /// be (see README's caveats section). Wiring one or two lenses to a different model via
    /// `--backend openrouter`/`custom` is a way to actually test whether that matters, not a
    /// claim that it does by default.
    #[serde(default)]
    pub model: Option<String>,
}

/// Checklist item for a deterministic tool (Semgrep/CodeQL, etc). The LLM doesn't judge this — external results are shown as-is.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeterministicCheck {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub tool: String,
}

/// Severity → score-deduction weight (`-{amount} pts` in report.md). Defaults preserve the
/// original hardcoded values (see #106) — add a `[scoring]` table to a spec to override any
/// subset; unset fields keep their default so a partial table doesn't zero out the rest.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScoringConfig {
    #[serde(default = "ScoringConfig::default_p0")]
    pub p0: i64,
    #[serde(default = "ScoringConfig::default_p1")]
    pub p1: i64,
    #[serde(default = "ScoringConfig::default_p2")]
    pub p2: i64,
    #[serde(default = "ScoringConfig::default_p3")]
    pub p3: i64,
}

impl ScoringConfig {
    fn default_p0() -> i64 {
        25
    }
    fn default_p1() -> i64 {
        12
    }
    fn default_p2() -> i64 {
        5
    }
    fn default_p3() -> i64 {
        1
    }
}

/// #4 (LLM-accuracy operating points): how easily discourse confirms/rejects a finding from its
/// confidence-weighted vote tally. Previously a hardcoded `VOTE_THRESHOLD` const in
/// `discourse/votes.rs` — a team that wants to bias toward catching more real defects at the
/// cost of more noise (or the reverse) had no way to express that short of editing this
/// project's own source. Default (0.6) preserves the original hardcoded value.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DiscourseConfig {
    #[serde(default = "DiscourseConfig::default_vote_threshold")]
    pub vote_threshold: f64,
}

impl DiscourseConfig {
    fn default_vote_threshold() -> f64 {
        0.6
    }
}

impl Default for DiscourseConfig {
    fn default() -> Self {
        DiscourseConfig {
            vote_threshold: Self::default_vote_threshold(),
        }
    }
}

impl Default for ScoringConfig {
    fn default() -> Self {
        ScoringConfig {
            p0: Self::default_p0(),
            p1: Self::default_p1(),
            p2: Self::default_p2(),
            p3: Self::default_p3(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Spec {
    pub name: String,
    /// Review context (domain/organizational background). Inserted verbatim into the prompt.
    #[serde(default)]
    pub context: String,
    pub lenses: Vec<Lens>,
    #[serde(default)]
    pub deterministic_checks: Vec<DeterministicCheck>,
    /// List of labels allowed on findings.
    pub labels: Vec<String>,
    /// If the diff's total changed lines exceed this value, policy `diff_size` FAILs. 0 = unset (N/A).
    #[serde(default)]
    pub diff_size_limit: usize,
    /// Path patterns (substring match) recognized as test files.
    #[serde(default)]
    pub test_path_patterns: Vec<String>,
    /// Path patterns (substring match) recognized as doc/changelog files.
    #[serde(default)]
    pub doc_path_patterns: Vec<String>,
    /// #126: path patterns (substring match) for files that are neither "behavior" nor
    /// test/docs — CI config, tooling scripts, generated/vendored files, lockfiles, non-source
    /// fixtures, etc. A file matching one of these is excluded from `tests_included`'s and
    /// `docs_updated`'s "does this need a test/doc" bucket entirely, instead of silently
    /// falling into the same catch-all as real source changes.
    #[serde(default)]
    pub ignored_path_patterns: Vec<String>,
    /// Per-severity score-deduction weights. Absent `[scoring]` table = all defaults.
    #[serde(default)]
    pub scoring: ScoringConfig,
    /// Discourse confirmation/rejection sensitivity. Absent `[discourse]` table = default.
    #[serde(default)]
    pub discourse: DiscourseConfig,
}

impl Spec {
    pub fn load(path: &Path) -> Result<Spec> {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read spec file: {}", path.display()))?;
        let spec: Spec = toml::from_str(&s)
            .with_context(|| format!("failed to parse spec TOML: {}", path.display()))?;
        anyhow::ensure!(!spec.lenses.is_empty(), "lenses is empty");
        anyhow::ensure!(!spec.labels.is_empty(), "labels is empty");

        // lens_by_id() only returns the first match — if an id is empty or duplicated,
        // --lenses/selection results referencing that id could silently map to the wrong (or
        // unintended) lens. This must be caught right at TOML parse time, not leak
        // unpredictably at runtime.
        let mut seen_ids = std::collections::HashSet::new();
        for l in &spec.lenses {
            anyhow::ensure!(
                !l.id.trim().is_empty(),
                "lenses has an entry with an empty id (title=\"{}\")",
                l.title
            );
            anyhow::ensure!(
                seen_ids.insert(l.id.clone()),
                "lenses has a duplicate id: \"{}\"",
                l.id
            );
        }

        // If an empty string pattern sneaks in, policy::matches_one's substring match
        // (path.contains("")) is always true, causing that policy to always leak through as
        // PASS — catch TOML typos (empty array entries) at load time.
        anyhow::ensure!(
            spec.test_path_patterns.iter().all(|p| !p.trim().is_empty()),
            "test_path_patterns has an empty pattern"
        );
        anyhow::ensure!(
            spec.doc_path_patterns.iter().all(|p| !p.trim().is_empty()),
            "doc_path_patterns has an empty pattern"
        );
        anyhow::ensure!(
            spec.ignored_path_patterns
                .iter()
                .all(|p| !p.trim().is_empty()),
            "ignored_path_patterns has an empty pattern"
        );

        // #146: score() only clamps the final total at the low end (.max(0)) — a negative
        // penalty here would make CONFIRMED findings *add* to the score instead of subtracting,
        // with nothing else catching it. Range-check at load time instead of trusting the TOML.
        for (name, value) in [
            ("p0", spec.scoring.p0),
            ("p1", spec.scoring.p1),
            ("p2", spec.scoring.p2),
            ("p3", spec.scoring.p3),
        ] {
            anyhow::ensure!(
                (0..=100).contains(&value),
                "scoring.{name} must be between 0 and 100, got {value}"
            );
        }

        // A threshold <= 0 would make a single CHALLENGE (or even no votes at all) enough to
        // confirm a finding, defeating the point of a confirmation gate entirely; there's no
        // natural upper bound (a very high threshold is a legitimate "require several
        // high-confidence agrees" choice), so only the low end is checked.
        anyhow::ensure!(
            spec.discourse.vote_threshold > 0.0,
            "discourse.vote_threshold must be > 0, got {}",
            spec.discourse.vote_threshold
        );

        Ok(spec)
    }

    pub fn lens_by_id(&self, id: &str) -> Option<&Lens> {
        self.lenses.iter().find(|l| l.id == id)
    }

    pub fn always_lenses(&self) -> Vec<&Lens> {
        self.lenses.iter().filter(|l| l.always).collect()
    }

    pub fn optional_lenses(&self) -> Vec<&Lens> {
        self.lenses.iter().filter(|l| !l.always).collect()
    }

    pub fn labels_prompt(&self) -> String {
        self.labels
            .iter()
            .map(|l| format!("\"{l}\""))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_spec(name: &str, toml: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("codereview-loop-spec-load-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, toml).unwrap();
        path
    }

    #[test]
    fn load_rejects_duplicate_lens_ids() {
        let path = write_spec(
            "dup.toml",
            r#"
name = "t"
labels = ["bug"]
[[lenses]]
id = "design"
title = "Design"
[[lenses]]
id = "design"
title = "Design again"
"#,
        );
        let err = Spec::load(&path).expect_err("duplicate lens id must be rejected");
        assert!(err.to_string().contains("duplicate"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_rejects_empty_lens_id() {
        let path = write_spec(
            "empty-id.toml",
            r#"
name = "t"
labels = ["bug"]
[[lenses]]
id = ""
title = "No id"
"#,
        );
        let err = Spec::load(&path).expect_err("empty lens id must be rejected");
        assert!(err.to_string().contains("empty"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_accepts_valid_unique_lens_ids() {
        let path = write_spec(
            "ok.toml",
            r#"
name = "t"
labels = ["bug"]
[[lenses]]
id = "design"
title = "Design"
[[lenses]]
id = "tests"
title = "Tests"
"#,
        );
        let spec = Spec::load(&path).expect("valid spec should load");
        assert_eq!(spec.lenses.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_defaults_scoring_when_table_is_absent() {
        let path = write_spec(
            "no-scoring.toml",
            r#"
name = "t"
labels = ["bug"]
[[lenses]]
id = "design"
title = "Design"
"#,
        );
        let spec = Spec::load(&path).expect("spec without [scoring] should load");
        assert_eq!(spec.scoring.p0, 25);
        assert_eq!(spec.scoring.p1, 12);
        assert_eq!(spec.scoring.p2, 5);
        assert_eq!(spec.scoring.p3, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_partial_scoring_table_only_overrides_given_fields() {
        // #106: a spec should be able to tune just the fields it cares about — a partial
        // [scoring] table must not zero out the severities it doesn't mention.
        let path = write_spec(
            "partial-scoring.toml",
            r#"
name = "t"
labels = ["bug"]
[[lenses]]
id = "design"
title = "Design"
[scoring]
p0 = 40
"#,
        );
        let spec = Spec::load(&path).expect("spec with partial [scoring] should load");
        assert_eq!(spec.scoring.p0, 40);
        assert_eq!(spec.scoring.p1, 12);
        assert_eq!(spec.scoring.p2, 5);
        assert_eq!(spec.scoring.p3, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_rejects_empty_test_path_pattern() {
        let path = write_spec(
            "empty-pattern.toml",
            r#"
name = "t"
labels = ["bug"]
test_path_patterns = ["tests/", ""]
[[lenses]]
id = "design"
title = "Design"
"#,
        );
        let err = Spec::load(&path).expect_err("empty test_path_patterns entry must be rejected");
        assert!(err.to_string().contains("test_path_patterns"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_rejects_empty_doc_path_pattern() {
        let path = write_spec(
            "empty-doc-pattern.toml",
            r#"
name = "t"
labels = ["bug"]
doc_path_patterns = [""]
[[lenses]]
id = "design"
title = "Design"
"#,
        );
        let err = Spec::load(&path).expect_err("empty doc_path_patterns entry must be rejected");
        assert!(err.to_string().contains("doc_path_patterns"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_rejects_empty_ignored_path_pattern() {
        let path = write_spec(
            "empty-ignored-pattern.toml",
            r#"
name = "t"
labels = ["bug"]
ignored_path_patterns = ["vendor/", ""]
[[lenses]]
id = "design"
title = "Design"
"#,
        );
        let err =
            Spec::load(&path).expect_err("empty ignored_path_patterns entry must be rejected");
        assert!(err.to_string().contains("ignored_path_patterns"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_defaults_ignored_path_patterns_to_empty_when_absent() {
        let path = write_spec(
            "no-ignored.toml",
            r#"
name = "t"
labels = ["bug"]
[[lenses]]
id = "design"
title = "Design"
"#,
        );
        let spec = Spec::load(&path).expect("spec without ignored_path_patterns should load");
        assert!(spec.ignored_path_patterns.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_defaults_vote_threshold_to_zero_point_six_when_discourse_table_is_absent() {
        let path = write_spec(
            "no-discourse-table.toml",
            r#"
name = "t"
labels = ["bug"]
[[lenses]]
id = "design"
title = "Design"
"#,
        );
        let spec = Spec::load(&path).unwrap();
        assert_eq!(spec.discourse.vote_threshold, 0.6);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_honors_a_custom_vote_threshold() {
        let path = write_spec(
            "custom-threshold.toml",
            r#"
name = "t"
labels = ["bug"]
[[lenses]]
id = "design"
title = "Design"
[discourse]
vote_threshold = 1.2
"#,
        );
        let spec = Spec::load(&path).unwrap();
        assert_eq!(spec.discourse.vote_threshold, 1.2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_rejects_a_zero_or_negative_vote_threshold() {
        let path = write_spec(
            "zero-threshold.toml",
            r#"
name = "t"
labels = ["bug"]
[[lenses]]
id = "design"
title = "Design"
[discourse]
vote_threshold = 0.0
"#,
        );
        let err = Spec::load(&path).expect_err("a zero vote_threshold must be rejected");
        assert!(err.to_string().contains("vote_threshold"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_accepts_the_shipped_high_recall_and_low_noise_presets() {
        // #4: regression guard against the preset files drifting out of sync with Spec's schema
        // (they're full copies of default.toml, not an "extends" — see their own header comment)
        // or silently ending up back at the default threshold.
        let high_recall =
            Spec::load(std::path::Path::new("specs/high-recall.toml")).expect("should load");
        assert!(
            high_recall.discourse.vote_threshold < 0.6,
            "high-recall preset must lower the confirmation bar below the 0.6 default"
        );

        let low_noise =
            Spec::load(std::path::Path::new("specs/low-noise.toml")).expect("should load");
        assert!(
            low_noise.discourse.vote_threshold > 0.6,
            "low-noise preset must raise the confirmation bar above the 0.6 default"
        );
    }

    #[test]
    fn load_rejects_a_negative_scoring_penalty() {
        let path = write_spec(
            "negative-scoring.toml",
            r#"
name = "t"
labels = ["bug"]
[[lenses]]
id = "design"
title = "Design"
[scoring]
p0 = -50
"#,
        );
        let err = Spec::load(&path).expect_err("a negative scoring penalty must be rejected");
        assert!(err.to_string().contains("scoring.p0"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_rejects_a_scoring_penalty_over_100() {
        let path = write_spec(
            "huge-scoring.toml",
            r#"
name = "t"
labels = ["bug"]
[[lenses]]
id = "design"
title = "Design"
[scoring]
p1 = 999
"#,
        );
        let err = Spec::load(&path).expect_err("a scoring penalty over 100 must be rejected");
        assert!(err.to_string().contains("scoring.p1"));
        let _ = std::fs::remove_file(&path);
    }
}
