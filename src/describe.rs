use crate::input::Input;
use crate::llm::Llm;
use crate::promptctx::shared_context;
use crate::spec::Spec;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DESCRIBE_SYSTEM: &str =
    "You are a reviewer who writes PR descriptions. Don't invent anything that isn't in the diff. \
Respond only in the specified JSON schema.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Describe {
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub walkthrough: Vec<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    pub can_be_split: String, // yes|no|unknown
    #[serde(default)]
    pub can_be_split_note: String,
}

pub fn run(llm: &Llm, spec: &Spec, input: &Input) -> Result<Describe> {
    let ctx = shared_context(spec, input);
    let task = "# Task\nWrite a PR description for the diff below.\n\n\
         ## Output (JSON only, no code fences)\n\
         {\"title\":\"one line, under 50 characters\",\"summary\":\"2-4 sentences\",\
         \"walkthrough\":[\"summary of changes per file/area, one line per item\"],\
         \"labels\":[\"whichever apply: feature|fix|refactor|chore|docs|test\"],\
         \"can_be_split\":\"yes|no|unknown\",\"can_be_split_note\":\"rationale\"}\n";
    llm.json_ctx_typed(Some(&ctx), task, Some(DESCRIBE_SYSTEM))
        .context("describe failed")
}

/// Scans only lines newly added (+) by the diff for TODO/FIXME/XXX. Deterministic (no LLM used).
///
/// If we distinguish hunk body lines from file headers using only `!l.starts_with("+++")`,
/// then when an added line's own content starts with "++ " (making the raw line "+++ ...",
/// the same collision that input.rs::parse_diff_stats ran into), a genuine TODO line gets
/// mistaken for a header and dropped from the scan. Explicitly tracking hunk-body status via
/// the `@@`/`diff --git` anchors removes this ambiguity (hunk body lines always start with a
/// `+`/`-`/` ` marker, so they can never impersonate this anchor string in raw form).
pub fn todo_sections(diff: &str) -> Vec<String> {
    let markers = ["TODO", "FIXME", "XXX"];
    let mut in_hunk_body = false;
    let mut out = Vec::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_hunk_body = false;
            continue;
        }
        if line.starts_with("@@") {
            in_hunk_body = true;
            continue;
        }
        if !in_hunk_body || !line.starts_with('+') {
            continue;
        }
        let content = line.strip_prefix('+').unwrap_or(line);
        if markers.iter().any(|m| content.contains(m)) {
            out.push(content.trim().to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_sections_keeps_a_leading_plus_that_belongs_to_the_code_itself() {
        // added line's own content is itself a markdown "+ " bullet (common list marker),
        // so the diff line is "++ TODO ..." — the outer '+' is only the diff marker.
        let diff = "diff --git a/CHANGELOG.md b/CHANGELOG.md\n\
                     --- a/CHANGELOG.md\n\
                     +++ b/CHANGELOG.md\n\
                     @@ -1,1 +1,2 @@\n\
                      existing line\n\
                     ++ TODO: revisit this bullet\n";
        let sections = todo_sections(diff);
        assert_eq!(sections, vec!["+ TODO: revisit this bullet".to_string()]);
    }

    #[test]
    fn todo_sections_finds_todo_even_when_content_collides_with_a_file_header() {
        // An even deeper collision: when an added line's content starts with "++ ", the raw
        // line becomes "+++ ..." (a prefix indistinguishable from the file header itself),
        // so it used to be dropped from the TODO scan entirely.
        let diff = "diff --git a/note.txt b/note.txt\n\
                     --- a/note.txt\n\
                     +++ b/note.txt\n\
                     @@ -1,1 +1,2 @@\n\
                      existing line\n\
                     +++ TODO: fix this later\n";
        let sections = todo_sections(diff);
        assert_eq!(sections, vec!["++ TODO: fix this later".to_string()]);
    }
}
