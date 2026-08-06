use crate::core::RunConfig;
use anyhow::{Context, Result};
use std::path::Path;

/// Normalized input. Missing information is left as None; the caller (report) is responsible
/// for displaying it as UNKNOWN.
pub struct Input {
    pub diff: String,
    pub changed_files: Vec<String>,
    pub added_lines: usize,
    pub removed_lines: usize,
    pub requirements: Option<String>,
    pub conventions: Option<String>,
    /// Deterministic tool results. check id -> (status, evidence). If absent, every item in
    /// the spec is NOT_RUN.
    pub deterministic_results: Option<serde_json::Value>,
    /// Cross-cutting run settings (currently just output language) — see `core::RunConfig`.
    pub config: RunConfig,
}

fn read_opt(p: &Option<std::path::PathBuf>) -> Result<Option<String>> {
    match p {
        None => Ok(None),
        Some(path) => {
            let s = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read file: {}", path.display()))?;
            Ok(Some(s))
        }
    }
}

fn push_unique(files: &mut Vec<String>, p: String) {
    if !files.contains(&p) {
        files.push(p);
    }
}

/// Extracts the list of changed files and +/- line counts from a unified diff, based on the
/// `diff --git a/X b/X` or `+++ b/X` headers.
///
/// Explicitly tracks whether we're inside a hunk body (after each file's first `@@ ... @@`,
/// up to the next `diff --git`) — with pure prefix matching and no tracking, when an
/// added/removed line's **own content** starts with `++ `/`-- ` (marker `+`/`-` plus that
/// content makes the raw line `+++ `/`--- `), the hunk body line gets mistaken for a file
/// header, letting a fake path slip into changed_files while the line itself is dropped from
/// the line count. `diff --git `/`@@` are the only anchors that a hunk body line (which
/// always starts with a `+`/`-`/` ` marker) can never impersonate in raw form, so there's no
/// ambiguity here.
fn parse_diff_stats(diff: &str) -> (Vec<String>, usize, usize) {
    let mut files: Vec<String> = Vec::new();
    let mut added = 0usize;
    let mut removed = 0usize;
    // A delete-only file has "+++ /dev/null" and no new path — we need to fall back to the
    // preceding "--- a/X" line's path so changed_files isn't left empty (a bug where a
    // pure-deletion diff couldn't even start the pipeline).
    let mut pending_old_path: Option<String> = None;
    let mut in_hunk_body = false;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            in_hunk_body = false;
            pending_old_path = None;
            // Renames/binaries/new empty files have no hunk at all, so --- / +++ never show
            // up (git simply doesn't emit those lines) — this header is the only one that's
            // always present, so we grab the b/ side path from it up front. If --- / +++ do
            // follow, they push again there, but push_unique keeps it from duplicating.
            if let Some(idx) = rest.rfind(" b/") {
                let b_path = &rest[idx + 3..];
                if !b_path.is_empty() {
                    push_unique(&mut files, b_path.to_string());
                }
            }
            continue;
        }
        if line.starts_with("@@") {
            in_hunk_body = true;
            continue;
        }
        if in_hunk_body {
            if line.starts_with('+') {
                added += 1;
            } else if line.starts_with('-') {
                removed += 1;
            }
            continue;
        }
        // From here on: after diff --git, before the first @@ — the actual header section.
        if let Some(rest) = line.strip_prefix("--- ") {
            let path = rest.strip_prefix("a/").unwrap_or(rest);
            pending_old_path = if path == "/dev/null" {
                None
            } else {
                Some(path.to_string())
            };
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            // trim_start_matches("b/") strips repeatedly, so for a repo whose real path
            // starts with b/ (e.g. a top-level directory named b), it wrongly strips the
            // diff's b/ marker twice — strip_prefix removes it only once.
            let path = rest.strip_prefix("b/").unwrap_or(rest);
            let resolved = if path == "/dev/null" {
                pending_old_path.take()
            } else {
                Some(path.to_string())
            };
            if let Some(p) = resolved {
                push_unique(&mut files, p);
            }
            continue;
        }
        // Metadata such as index/similarity/rename from·to/new file mode/Binary files — ignored.
    }
    (files, added, removed)
}

pub fn normalize(
    diff_path: &Path,
    requirements_path: &Option<std::path::PathBuf>,
    conventions_path: &Option<std::path::PathBuf>,
    deterministic_results_path: &Option<std::path::PathBuf>,
    language: Option<String>,
) -> Result<Input> {
    let diff = std::fs::read_to_string(diff_path)
        .with_context(|| format!("failed to read diff file: {}", diff_path.display()))?;
    anyhow::ensure!(!diff.trim().is_empty(), "diff is empty");
    let (changed_files, added_lines, removed_lines) = parse_diff_stats(&diff);
    anyhow::ensure!(
        !changed_files.is_empty(),
        "no changed files found in diff (check unified diff format)"
    );

    let requirements = read_opt(requirements_path)?;
    let conventions = read_opt(conventions_path)?;
    let deterministic_results = match deterministic_results_path {
        None => None,
        Some(p) => {
            let s = std::fs::read_to_string(p).with_context(|| {
                format!("failed to read deterministic results file: {}", p.display())
            })?;
            Some(serde_json::from_str(&s).with_context(|| {
                format!(
                    "failed to parse deterministic results JSON: {}",
                    p.display()
                )
            })?)
        }
    };

    Ok(Input {
        diff,
        changed_files,
        added_lines,
        removed_lines,
        requirements,
        conventions,
        deterministic_results,
        config: RunConfig { language },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_diff_stats_preserves_real_path_starting_with_b_slash() {
        let diff = "diff --git a/b/foo.txt b/b/foo.txt\n\
                     --- a/b/foo.txt\n\
                     +++ b/b/foo.txt\n\
                     @@ -1 +1 @@\n\
                     -old\n\
                     +new\n";
        let (files, added, removed) = parse_diff_stats(diff);
        assert_eq!(files, vec!["b/foo.txt".to_string()]);
        assert_eq!(added, 1);
        assert_eq!(removed, 1);
    }

    #[test]
    fn parse_diff_stats_strips_normal_b_prefix() {
        let diff = "diff --git a/src/main.rs b/src/main.rs\n\
                     --- a/src/main.rs\n\
                     +++ b/src/main.rs\n\
                     @@ -1 +1 @@\n\
                     -old\n\
                     +new\n";
        let (files, _, _) = parse_diff_stats(diff);
        assert_eq!(files, vec!["src/main.rs".to_string()]);
    }

    #[test]
    fn parse_diff_stats_captures_delete_only_files_via_old_path() {
        let diff = "diff --git a/src/dead_code.rs b/src/dead_code.rs\n\
                     deleted file mode 100644\n\
                     --- a/src/dead_code.rs\n\
                     +++ /dev/null\n\
                     @@ -1,3 +0,0 @@\n\
                     -fn unused() {}\n\
                     -// dead\n\
                     -// code\n";
        let (files, added, removed) = parse_diff_stats(diff);
        assert_eq!(files, vec!["src/dead_code.rs".to_string()]);
        assert_eq!(added, 0);
        assert_eq!(removed, 3);
    }

    #[test]
    fn parse_diff_stats_handles_mixed_delete_and_modify() {
        let diff = "diff --git a/src/old.rs b/src/old.rs\n\
                     deleted file mode 100644\n\
                     --- a/src/old.rs\n\
                     +++ /dev/null\n\
                     @@ -1,1 +0,0 @@\n\
                     -gone\n\
                     diff --git a/src/main.rs b/src/main.rs\n\
                     --- a/src/main.rs\n\
                     +++ b/src/main.rs\n\
                     @@ -1 +1 @@\n\
                     -old\n\
                     +new\n";
        let (files, _, _) = parse_diff_stats(diff);
        assert_eq!(
            files,
            vec!["src/old.rs".to_string(), "src/main.rs".to_string()]
        );
    }

    #[test]
    fn parse_diff_stats_does_not_confuse_added_line_content_with_a_file_header() {
        // When an added line's own content starts with "++ " (a common case: reviewing a
        // diff/patch file itself), marker(+) + content makes the raw line "+++ ...", and
        // without hunk-body-state tracking this used to get mistaken for a new file header,
        // letting a fake path slip in while the line itself was dropped from the line count.
        let diff = "diff --git a/note.txt b/note.txt\n\
                     --- a/note.txt\n\
                     +++ b/note.txt\n\
                     @@ -1,2 +1,3 @@\n\
                      line one\n\
                      line two\n\
                     +++ TODO: fix this later\n";
        let (files, added, removed) = parse_diff_stats(diff);
        assert_eq!(
            files,
            vec!["note.txt".to_string()],
            "a fake path must not slip in"
        );
        assert_eq!(
            added, 1,
            "hunk body lines starting with +++ must also be counted as added lines"
        );
        assert_eq!(removed, 0);
    }

    #[test]
    fn parse_diff_stats_captures_pure_rename_with_no_hunks() {
        // A 100%-similarity rename has no hunk at all — --- / +++ never show up.
        let diff = "diff --git a/old_name.rs b/new_name.rs\n\
                     similarity index 100%\n\
                     rename from old_name.rs\n\
                     rename to new_name.rs\n";
        let (files, added, removed) = parse_diff_stats(diff);
        assert_eq!(files, vec!["new_name.rs".to_string()]);
        assert_eq!(added, 0);
        assert_eq!(removed, 0);
    }

    #[test]
    fn parse_diff_stats_captures_binary_file_with_no_hunks() {
        let diff = "diff --git a/logo.png b/logo.png\n\
                     index abc1234..def5678 100644\n\
                     Binary files a/logo.png and b/logo.png differ\n";
        let (files, _, _) = parse_diff_stats(diff);
        assert_eq!(files, vec!["logo.png".to_string()]);
    }

    #[test]
    fn parse_diff_stats_captures_new_empty_file_with_no_hunks() {
        let diff = "diff --git a/.gitkeep b/.gitkeep\n\
                     new file mode 100644\n\
                     index 0000000..e69de29\n";
        let (files, _, _) = parse_diff_stats(diff);
        assert_eq!(files, vec![".gitkeep".to_string()]);
    }
}
