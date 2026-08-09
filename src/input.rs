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

/// Splits a diff into (file_path, block_text) pairs at `diff --git` boundaries. Any content
/// before the first such line (atypical, but don't lose it) becomes an unlabeled first block
/// that prioritize_and_cap_diff never reorders or drops.
fn split_into_file_blocks(diff: &str) -> Vec<(Option<String>, String)> {
    let mut blocks: Vec<(Option<String>, String)> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_lines: Vec<&str> = Vec::new();

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            if !current_lines.is_empty() {
                blocks.push((current_path.take(), current_lines.join("\n")));
                current_lines.clear();
            }
            // Same "b/" extraction as parse_diff_stats, for consistency.
            current_path = line
                .rfind(" b/")
                .map(|idx| line[idx + 3..].to_string())
                .filter(|p| !p.is_empty());
        }
        current_lines.push(line);
    }
    if !current_lines.is_empty() {
        blocks.push((current_path, current_lines.join("\n")));
    }
    blocks
}

const NOISY_FILENAMES: [&str; 8] = [
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "go.sum",
    "composer.lock",
    "Gemfile.lock",
    "poetry.lock",
];
const NOISY_PATH_SEGMENTS: [&str; 6] = [
    "/vendor/",
    "/generated/",
    "/dist/",
    "/build/",
    "/node_modules/",
    "/target/",
];

/// Lockfiles, vendored/generated/build output, and minified assets rarely need the same
/// attention as hand-written source changes, and are usually large relative to their actual
/// review value.
fn is_noisy_path(path: &str) -> bool {
    let filename = path.rsplit('/').next().unwrap_or(path);
    if NOISY_FILENAMES.contains(&filename) {
        return true;
    }
    if filename.ends_with(".min.js") || filename.ends_with(".min.css") {
        return true;
    }
    let normalized = format!("/{path}");
    NOISY_PATH_SEGMENTS
        .iter()
        .any(|seg| normalized.contains(seg))
}

/// Unlike DIFF_WARN_CHARS (a cost warning, see pipeline/review.rs), this is an actual limit —
/// past this, prioritize_and_cap_diff starts dropping the lowest-priority file-blocks so the
/// diff can't grow unbounded and risk exceeding the model's context window.
///
/// #142: an *approximate* limit, not an exact one — the trailing `[NOTE: ... omitted/truncated
/// ...]` text and the newlines from joining kept blocks are added after truncation, so the final
/// output can run a few hundred bytes past this value (see the `+ 500` tolerance in
/// `prioritize_and_cap_diff_truncates_a_lone_oversized_block_instead_of_returning_it_whole`
/// below). Also diff-only: `--requirements`/`--conventions` content has no cap of its own at
/// all (only the best-effort warning in `pipeline/review.rs` covers them).
const DIFF_HARD_CAP_CHARS: usize = 1_000_000;

/// #127: DIFF_HARD_CAP_CHARS/DIFF_WARN_CHARS protect against context-window overflow, but
/// they're measured in characters while the thing they're actually protecting against is
/// tokens — a wildly different ratio depending on the language/content (dense code vs.
/// natural-language-heavy diffs vs. non-Latin scripts). This isn't a per-provider/model tokenizer
/// (that's the bigger ask in #127, not done here) — just the commonly-cited ~4-chars-per-token
/// rule of thumb for English/code, good enough to put an approximate number next to the char
/// count in warnings so it reads as "roughly how much context this uses," not an exact figure.
pub(crate) fn estimate_tokens(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

/// Backs off from `max_bytes` to the nearest earlier UTF-8 char boundary, so truncating a
/// `&str` there never panics or produces invalid UTF-8.
fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Truncates `text` (a single file-block) to at most `max_bytes`, preferring to cut right
/// before the first `@@` hunk header that would no longer fully fit, instead of at a raw byte
/// offset (see #116). A byte-offset cut can land mid-function, mid-string-literal, mid-hunk, or
/// after removed lines but before their corresponding added lines — a structurally broken
/// fragment framed as a complete diff. Cutting at a hunk boundary means every hunk the LLM
/// sees is at least internally complete, even though later hunks in the same file are missing
/// entirely (same as before — that's still visible via the caller's truncation note).
///
/// Falls back to `truncate_at_char_boundary` (a raw byte cut) only when even the header plus
/// the first hunk alone exceeds `max_bytes`, or the block has no `@@` hunk markers at all
/// (e.g. a binary-file or pure-rename diff) — there's no better cut point available then.
fn truncate_at_hunk_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut hunk_starts: Vec<usize> = Vec::new();
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        if line.starts_with("@@") {
            hunk_starts.push(offset);
        }
        offset += line.len();
    }
    let best_cut = hunk_starts
        .into_iter()
        .rfind(|&start| start > 0 && start <= max_bytes);
    match best_cut {
        Some(cut) => &text[..cut],
        None => truncate_at_char_boundary(text, max_bytes),
    }
}

/// Reorders file-blocks so noisy/generated ones (see `is_noisy_path`) sort after everything
/// else, stable within each group — pure reordering, no information loss, always applied. If
/// the diff is still over DIFF_HARD_CAP_CHARS afterward, drops the lowest-priority blocks from
/// the tail until it fits (always keeping at least one block so the diff never ends up empty)
/// and returns which files got dropped, so the caller can surface that instead of silently
/// truncating (see #107 — this is the "no silent truncation" principle already used for
/// DIFF_WARN_CHARS, extended to an actual cap).
///
/// #111: the "always keep at least one block" rule above means that if the single
/// highest-priority block is itself bigger than DIFF_HARD_CAP_CHARS, the old version returned
/// it whole — the output could still exceed the cap despite the name. That's the only case
/// where this can happen (every block after the first only gets added if it fits, so once
/// `kept` holds more than the lone oversized block the total is already within budget) — so
/// after the drop loop, if exactly one oversized block remains, its own text gets truncated
/// too, with a visible note, making the cap an actual bound on the returned string's length.
fn prioritize_and_cap_diff(diff: &str) -> (String, Vec<String>) {
    let blocks = split_into_file_blocks(diff);

    let mut indexed: Vec<(usize, Option<String>, String)> = blocks
        .into_iter()
        .enumerate()
        .map(|(i, (path, text))| (i, path, text))
        .collect();
    indexed.sort_by_key(|(i, path, _)| {
        let noisy = path.as_deref().is_some_and(is_noisy_path);
        (noisy, *i)
    });

    let mut kept: Vec<String> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    let mut total = 0usize;
    for (_, path, text) in indexed {
        if total + text.len() > DIFF_HARD_CAP_CHARS && !kept.is_empty() {
            if let Some(p) = path {
                dropped.push(p);
            }
            continue;
        }
        total += text.len();
        kept.push(text);
    }

    let mut truncated = false;
    if kept.len() == 1 && kept[0].len() > DIFF_HARD_CAP_CHARS {
        kept[0] = truncate_at_hunk_boundary(&kept[0], DIFF_HARD_CAP_CHARS).to_string();
        truncated = true;
    }

    let mut out = kept.join("\n");
    if !dropped.is_empty() {
        out.push_str(&format!(
            "\n\n[NOTE: {} file(s) omitted from this diff due to the {}-char size cap — not reviewed: {}]\n",
            dropped.len(),
            DIFF_HARD_CAP_CHARS,
            dropped.join(", ")
        ));
    }
    if truncated {
        out.push_str(&format!(
            "\n\n[NOTE: this diff's remaining file was truncated to the {DIFF_HARD_CAP_CHARS}-char size cap — content past that point was not reviewed]\n"
        ));
    }
    (out, dropped)
}

/// Strips whole file-blocks matching `denied_path_patterns` (`spec.security.denied_path_patterns`)
/// out of the diff before anything else touches it — secrets/credentials files, infra configs,
/// or any other path a team decides must never leave the machine as diff content sent to an LLM
/// backend. Applied before `parse_diff_stats`, so a denied file also never appears in
/// `changed_files`/added/removed line counts, not just in what's sent to the model — the point
/// is that its content leaves no trace in anything this run produces, not merely that lens
/// prompts skip it.
fn strip_denied_paths(diff: &str, denied_path_patterns: &[String]) -> (String, Vec<String>) {
    if denied_path_patterns.is_empty() {
        return (diff.to_string(), Vec::new());
    }
    let blocks = split_into_file_blocks(diff);
    let mut kept: Vec<String> = Vec::new();
    let mut denied: Vec<String> = Vec::new();
    for (path, text) in blocks {
        let is_denied = path.as_deref().is_some_and(|p| {
            denied_path_patterns
                .iter()
                .any(|pat| crate::policy::matches_one(p, pat))
        });
        if is_denied {
            denied.push(path.unwrap());
        } else {
            kept.push(text);
        }
    }
    let mut out = kept.join("\n");
    if !denied.is_empty() {
        out.push_str(&format!(
            "\n\n[NOTE: {} file(s) excluded from this diff by security.denied_path_patterns policy — not sent to the LLM, not reviewed: {}]\n",
            denied.len(),
            denied.join(", ")
        ));
    }
    (out, denied)
}

/// Returns the normalized `Input`, the list of files `prioritize_and_cap_diff` had to drop from
/// what's actually sent to the LLM (empty if nothing was dropped — #129 surfaces this in
/// `manifest.json` as structured data instead of only the in-diff text note), and the list of
/// files excluded up front by `denied_path_patterns`.
pub fn normalize(
    diff_path: &Path,
    requirements_path: &Option<std::path::PathBuf>,
    conventions_path: &Option<std::path::PathBuf>,
    deterministic_results_path: &Option<std::path::PathBuf>,
    language: Option<String>,
    denied_path_patterns: &[String],
) -> Result<(Input, Vec<String>, Vec<String>)> {
    let diff = std::fs::read_to_string(diff_path)
        .with_context(|| format!("failed to read diff file: {}", diff_path.display()))?;
    anyhow::ensure!(!diff.trim().is_empty(), "diff is empty");

    // Run before parse_diff_stats/changed_files below, not just before prioritize_and_cap_diff —
    // a denied file must leave no trace anywhere this run produces (stats, manifest changed-file
    // list, report), not merely be skipped by lens prompts.
    let (diff, denied_files) = strip_denied_paths(&diff, denied_path_patterns);
    if !denied_files.is_empty() {
        eprintln!(
            "security.denied_path_patterns excluded {} file(s) from this diff, not sent to the LLM: {}",
            denied_files.len(),
            denied_files.join(", ")
        );
    }

    let (changed_files, added_lines, removed_lines) = parse_diff_stats(&diff);
    anyhow::ensure!(
        !changed_files.is_empty(),
        "no changed files found in diff (check unified diff format, or whether \
         denied_path_patterns excluded everything)"
    );

    // changed_files/added_lines/removed_lines above reflect the post-denylist diff (accurate
    // stats for what's actually reviewable) even if prioritize_and_cap_diff below drops further
    // file-blocks from what's actually sent to the LLM.
    let (diff, dropped_files) = prioritize_and_cap_diff(&diff);
    if !dropped_files.is_empty() {
        eprintln!(
            "Warning: diff exceeded the {DIFF_HARD_CAP_CHARS}-char hard cap — {} file(s) dropped from what's sent to the LLM: {}",
            dropped_files.len(),
            dropped_files.join(", ")
        );
    }

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

    Ok((
        Input {
            diff,
            changed_files,
            added_lines,
            removed_lines,
            requirements,
            conventions,
            deterministic_results,
            config: RunConfig { language },
        },
        dropped_files,
        denied_files,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- estimate_tokens() ---

    #[test]
    fn estimate_tokens_rounds_up_to_the_nearest_whole_token() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1); // 3 chars -> ceil(3/4) = 1
        assert_eq!(estimate_tokens("abcd"), 1); // exactly 4 chars -> 1
        assert_eq!(estimate_tokens("abcde"), 2); // 5 chars -> ceil(5/4) = 2
    }

    #[test]
    fn estimate_tokens_counts_unicode_scalars_not_bytes() {
        // A multi-byte-per-char string must not inflate the estimate just because it's more
        // bytes — chars().count() is the deliberate choice here, not s.len().
        let s = "가나다라"; // 4 Korean chars, 12 bytes in UTF-8
        assert_eq!(estimate_tokens(s), 1);
    }

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

    // --- truncate_at_hunk_boundary() ---

    #[test]
    fn truncate_at_hunk_boundary_cuts_before_the_first_hunk_that_no_longer_fully_fits() {
        // #116: must not land inside hunk 2's body — the cut has to fall exactly at the start
        // of hunk 2's "@@" line, keeping hunk 1 (and the header before it) fully intact.
        let header = "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n";
        let hunk1 = "@@ -1,2 +1,2 @@\n-old1\n+new1\n";
        let hunk2 = "@@ -10,2 +10,2 @@\n-old2\n+new2\n";
        let text = format!("{header}{hunk1}{hunk2}");
        // Budget covers header+hunk1 exactly, but not header+hunk1+hunk2.
        let budget = header.len() + hunk1.len();
        let out = truncate_at_hunk_boundary(&text, budget);
        assert_eq!(out, format!("{header}{hunk1}"));
        assert!(
            !out.contains("@@ -10"),
            "hunk 2 must not appear at all, not even partially"
        );
    }

    #[test]
    fn truncate_at_hunk_boundary_falls_back_to_byte_cut_when_even_the_first_hunk_does_not_fit() {
        let header = "diff --git a/x.rs b/x.rs\n";
        let huge_hunk = format!("@@ -1,1 +1,1 @@\n+{}\n", "x".repeat(1000));
        let text = format!("{header}{huge_hunk}");
        let budget = header.len() + 50; // far smaller than the single hunk alone
        let out = truncate_at_hunk_boundary(&text, budget);
        assert!(
            out.len() <= budget,
            "must still respect the byte budget via the byte-cut fallback"
        );
        assert!(out.starts_with("diff --git a/x.rs"));
    }

    #[test]
    fn truncate_at_hunk_boundary_falls_back_to_byte_cut_when_there_are_no_hunks_at_all() {
        // A binary-file or pure-rename diff has no "@@" markers to cut at.
        let text = format!(
            "diff --git a/logo.png b/logo.png\nBinary files differ\n{}",
            "x".repeat(1000)
        );
        let out = truncate_at_hunk_boundary(&text, 50);
        assert!(out.len() <= 50);
    }

    #[test]
    fn truncate_at_hunk_boundary_is_a_no_op_when_already_within_budget() {
        let text = "diff --git a/x.rs b/x.rs\n@@ -1,1 +1,1 @@\n+x\n";
        assert_eq!(truncate_at_hunk_boundary(text, text.len() + 100), text);
    }

    // --- is_noisy_path() ---

    #[test]
    fn is_noisy_path_flags_known_lockfiles() {
        for f in ["Cargo.lock", "package-lock.json", "yarn.lock", "go.sum"] {
            assert!(is_noisy_path(f), "{f} should be noisy");
            assert!(
                is_noisy_path(&format!("nested/dir/{f}")),
                "{f} should be noisy when nested"
            );
        }
    }

    #[test]
    fn is_noisy_path_flags_generated_and_vendor_paths() {
        assert!(is_noisy_path("vendor/lib/thing.go"));
        assert!(is_noisy_path("web/dist/bundle.js"));
        assert!(is_noisy_path("web/node_modules/react/index.js"));
        assert!(is_noisy_path("assets/app.min.js"));
    }

    #[test]
    fn is_noisy_path_leaves_ordinary_source_files_alone() {
        assert!(!is_noisy_path("src/main.rs"));
        assert!(!is_noisy_path("src/vendor_utils.rs")); // "vendor" substring but not the /vendor/ path segment
    }

    // --- split_into_file_blocks() ---

    #[test]
    fn split_into_file_blocks_splits_at_each_diff_git_header() {
        let diff = "diff --git a/a.rs b/a.rs\n+x\n\
                     diff --git a/b.rs b/b.rs\n+y\n";
        let blocks = split_into_file_blocks(diff);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0.as_deref(), Some("a.rs"));
        assert_eq!(blocks[1].0.as_deref(), Some("b.rs"));
    }

    // --- prioritize_and_cap_diff() ---

    #[test]
    fn prioritize_and_cap_diff_moves_noisy_files_after_normal_ones_without_dropping_anything() {
        let diff = "diff --git a/Cargo.lock b/Cargo.lock\n+lockfile change\n\
                     diff --git a/src/main.rs b/src/main.rs\n+real change\n";
        let (out, dropped) = prioritize_and_cap_diff(diff);
        assert!(dropped.is_empty());
        // src/main.rs's block must now come before Cargo.lock's, even though Cargo.lock was first originally.
        let main_pos = out.find("src/main.rs").unwrap();
        let lock_pos = out.find("Cargo.lock").unwrap();
        assert!(
            main_pos < lock_pos,
            "non-noisy file should be reordered ahead of the noisy one:\n{out}"
        );
    }

    #[test]
    fn prioritize_and_cap_diff_preserves_relative_order_within_the_same_priority_group() {
        let diff = "diff --git a/a.rs b/a.rs\n+a\n\
                     diff --git a/b.rs b/b.rs\n+b\n";
        let (out, dropped) = prioritize_and_cap_diff(diff);
        assert!(dropped.is_empty());
        assert!(out.find("a.rs").unwrap() < out.find("b.rs").unwrap());
    }

    #[test]
    fn prioritize_and_cap_diff_is_a_no_op_on_a_single_file_diff() {
        // Content is unchanged; a trailing newline may be stripped by the block-join
        // (harmless — this text only ever gets fenced into an LLM prompt), so compare trimmed.
        let diff = "diff --git a/a.rs b/a.rs\n+a\n";
        let (out, dropped) = prioritize_and_cap_diff(diff);
        assert_eq!(out.trim_end(), diff.trim_end());
        assert!(dropped.is_empty());
    }

    // --- strip_denied_paths() ---

    #[test]
    fn strip_denied_paths_is_a_no_op_when_no_patterns_are_configured() {
        let diff = "diff --git a/secrets.env b/secrets.env\n+API_KEY=x\n";
        let (out, denied) = strip_denied_paths(diff, &[]);
        assert_eq!(out, diff);
        assert!(denied.is_empty());
    }

    #[test]
    fn strip_denied_paths_removes_a_matching_file_block_and_its_content() {
        let diff = "diff --git a/src/main.rs b/src/main.rs\n+fn main() {}\n\
             diff --git a/secrets.env b/secrets.env\n+API_KEY=super-secret-value\n";
        let (out, denied) = strip_denied_paths(diff, &["secrets.env".to_string()]);
        assert_eq!(denied, vec!["secrets.env".to_string()]);
        assert!(out.contains("fn main()"), "the kept file must survive");
        assert!(
            !out.contains("super-secret-value"),
            "a denied file's content must never appear in what's returned"
        );
        assert!(out.contains("[NOTE: 1 file(s) excluded"));
        assert!(out.contains("security.denied_path_patterns"));
    }

    #[test]
    fn strip_denied_paths_leaves_a_non_matching_diff_untouched() {
        let diff = "diff --git a/src/main.rs b/src/main.rs\n+fn main() {}\n";
        let (out, denied) = strip_denied_paths(diff, &["secrets.env".to_string()]);
        assert_eq!(out.trim_end(), diff.trim_end());
        assert!(denied.is_empty());
    }

    #[test]
    fn strip_denied_paths_matches_a_directory_style_pattern_at_a_real_segment_boundary_only() {
        // Same matches_one semantics as test_path_patterns/doc_path_patterns: "secrets/" must
        // match a real path segment, not the middle of "my_secrets_config.rs".
        let diff = "diff --git a/config/secrets/db.toml b/config/secrets/db.toml\n+password=x\n\
             diff --git a/src/my_secrets_config.rs b/src/my_secrets_config.rs\n+fn f() {}\n";
        let (out, denied) = strip_denied_paths(diff, &["secrets/".to_string()]);
        assert_eq!(denied, vec!["config/secrets/db.toml".to_string()]);
        assert!(out.contains("my_secrets_config.rs"));
        assert!(!out.contains("password=x"));
    }

    #[test]
    fn prioritize_and_cap_diff_drops_lowest_priority_tail_past_the_hard_cap_and_reports_it() {
        // Two huge files, each over half the cap on its own — together they exceed
        // DIFF_HARD_CAP_CHARS, so the noisy one (sorted last) must be dropped, not the real one.
        let real_content = "a".repeat(DIFF_HARD_CAP_CHARS / 2 + 10);
        let lock_content = "l".repeat(DIFF_HARD_CAP_CHARS / 2 + 10);
        let diff = format!(
            "diff --git a/src/main.rs b/src/main.rs\n+{real_content}\n\
             diff --git a/Cargo.lock b/Cargo.lock\n+{lock_content}\n"
        );
        let (out, dropped) = prioritize_and_cap_diff(&diff);
        assert_eq!(dropped, vec!["Cargo.lock".to_string()]);
        assert!(out.contains("src/main.rs"));
        assert!(
            out.contains(&real_content),
            "the kept file's actual content must survive"
        );
        assert!(
            !out.contains(&lock_content),
            "the dropped file's content must not survive"
        );
        assert!(out.contains("[NOTE: 1 file(s) omitted"));
        assert!(
            out.contains("Cargo.lock"),
            "the note must name the dropped file"
        );
    }

    #[test]
    fn prioritize_and_cap_diff_never_drops_everything_even_if_the_first_block_alone_exceeds_the_cap(
    ) {
        let big = "x".repeat(DIFF_HARD_CAP_CHARS + 1000);
        let diff = format!("diff --git a/src/main.rs b/src/main.rs\n+{big}\n");
        let (out, dropped) = prioritize_and_cap_diff(&diff);
        assert!(
            dropped.is_empty(),
            "the only block must be kept even though it's over the cap alone"
        );
        assert!(out.contains("src/main.rs"));
    }

    #[test]
    fn prioritize_and_cap_diff_truncates_a_lone_oversized_block_instead_of_returning_it_whole() {
        // #111: DIFF_HARD_CAP_CHARS claimed to be an actual limit, but the "always keep at
        // least one block" rule meant a single block bigger than the cap on its own still made
        // it through whole — the output could exceed the cap despite the name. The header/start
        // of the block (where the filename lives) must survive truncation since we cut from the
        // end, not the start.
        let big = "x".repeat(DIFF_HARD_CAP_CHARS + 1000);
        let diff = format!("diff --git a/src/main.rs b/src/main.rs\n+{big}\n");
        let (out, dropped) = prioritize_and_cap_diff(&diff);
        assert!(dropped.is_empty());
        assert!(
            out.len() <= DIFF_HARD_CAP_CHARS + 500,
            "output ({} chars) must be bounded near the cap, not the original {} chars",
            out.len(),
            diff.len()
        );
        assert!(
            out.contains("src/main.rs"),
            "the filename header must survive truncation"
        );
        assert!(
            out.contains("[NOTE:") && out.contains("truncated"),
            "truncation must be visible, not silent"
        );
    }

    #[test]
    fn prioritize_and_cap_diff_truncation_never_splits_a_hunk_in_half() {
        // #116, end to end through prioritize_and_cap_diff (not just the helper directly):
        // build one oversized file with many small, complete hunks and confirm every hunk that
        // survives truncation is whole — never cut off partway through its body.
        let header = "diff --git a/src/big.rs b/src/big.rs\n--- a/src/big.rs\n+++ b/src/big.rs\n";
        let hunk = |n: usize| format!("@@ -{n},1 +{n},1 @@\n-old{n}\n+new{n}\n");
        let mut diff = header.to_string();
        // Enough ~30-byte hunks to comfortably exceed DIFF_HARD_CAP_CHARS.
        for n in 0..(DIFF_HARD_CAP_CHARS / 25 + 100) {
            diff.push_str(&hunk(n));
        }

        let (out, dropped) = prioritize_and_cap_diff(&diff);
        assert!(
            dropped.is_empty(),
            "single oversized file must never be dropped"
        );
        assert!(out.contains("[NOTE:") && out.contains("truncated"));

        // Every "@@" line in the output must be followed by both of its body lines — if a hunk
        // got cut mid-body, a "-oldN"/"+newN" pair would be incomplete or missing.
        let content_before_note = out.split("\n\n[NOTE:").next().unwrap();
        let mut lines = content_before_note.lines().peekable();
        let mut hunk_count = 0;
        while let Some(line) = lines.next() {
            if line.starts_with("@@") {
                hunk_count += 1;
                let removed = lines.next();
                let added = lines.next();
                assert!(
                    removed.is_some_and(|l| l.starts_with('-'))
                        && added.is_some_and(|l| l.starts_with('+')),
                    "hunk starting at {line:?} is missing its body — truncation cut mid-hunk"
                );
            }
        }
        assert!(hunk_count > 0, "at least some hunks should have survived");
    }
}
