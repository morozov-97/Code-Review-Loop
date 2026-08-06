//! #123: a finding's `file`/`line` is whatever the LLM claims — nothing checks it actually
//! exists in the diff. A hallucinated or mistyped citation currently looks identical to a real
//! one everywhere downstream (score, verdict, report). This does a local, deterministic check
//! against the diff itself and flags (never drops) findings whose citation doesn't check out,
//! so a human reading the report knows which citations to double check first.

use crate::lens::Finding;
use std::collections::HashSet;

/// Sets `evidence_unverified` on every finding whose `file:line` doesn't correspond to an
/// actual line on the diff's new (post-change) side — i.e. neither an added line nor a
/// context line shown in a hunk. Findings whose `line` isn't a parseable number are also
/// flagged, since there's nothing to check against.
pub(crate) fn verify(findings: &mut [Finding], diff: &str) {
    let lines = diff_line_set(diff);
    for f in findings.iter_mut() {
        let ok = first_line_number(&f.line)
            .map(|n| lines.contains(&(f.file.clone(), n)))
            .unwrap_or(false);
        f.evidence_unverified = !ok;
    }
}

/// Every (file, new-side line number) pair actually visible in the diff's hunks — added lines
/// and context lines both count as "shown", since evidence can legitimately cite either.
fn diff_line_set(diff: &str) -> HashSet<(String, u32)> {
    let mut set = HashSet::new();
    let mut current_file = String::new();
    let mut new_line_no: u32 = 0;
    let mut in_hunk = false;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            in_hunk = false;
            if let Some(idx) = rest.rfind(" b/") {
                current_file = rest[idx + 3..].to_string();
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            let path = rest.strip_prefix("b/").unwrap_or(rest);
            if path != "/dev/null" {
                current_file = path.to_string();
            }
            continue;
        }
        if let Some(new_start) = parse_hunk_new_start(line) {
            in_hunk = true;
            new_line_no = new_start;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if line.starts_with('+') {
            set.insert((current_file.clone(), new_line_no));
            new_line_no += 1;
        } else if line.starts_with('-') {
            // Old-side only — doesn't occupy a new-side line number.
        } else if line.starts_with(' ') || line.is_empty() {
            set.insert((current_file.clone(), new_line_no));
            new_line_no += 1;
        }
        // "\ No newline at end of file" and similar metadata: ignored, no line consumed.
    }
    set
}

/// Parses the new-file start line from a hunk header like `@@ -12,5 +34,6 @@ fn foo() {`.
fn parse_hunk_new_start(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("@@ ")?;
    let plus_group = rest.split_whitespace().find(|s| s.starts_with('+'))?;
    let num_part = plus_group.trim_start_matches('+').split(',').next()?;
    num_part.parse().ok()
}

/// Extracts the first run of ASCII digits in `s` (e.g. "42" from "42", "L42", "42-45",
/// "src/x.rs:42"). Returns None if there's no digit run at all (e.g. "UNKNOWN").
fn first_line_number(s: &str) -> Option<u32> {
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            let digits: String = chars.by_ref().take_while(|c| c.is_ascii_digit()).collect();
            return digits.parse().ok();
        }
        chars.next();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(file: &str, line: &str) -> Finding {
        Finding {
            id: "f1".to_string(),
            file: file.to_string(),
            line: line.to_string(),
            claim: String::new(),
            evidence: String::new(),
            impact: String::new(),
            severity: "P1".to_string(),
            label: String::new(),
            confidence: String::new(),
            recommendation: String::new(),
            lens: String::new(),
            reviewer: String::new(),
            evidence_unverified: false,
        }
    }

    const DIFF: &str = "diff --git a/src/main.rs b/src/main.rs\n\
                         --- a/src/main.rs\n\
                         +++ b/src/main.rs\n\
                         @@ -10,3 +10,4 @@ fn foo() {\n\
                          context line 10\n\
                         -old line 11\n\
                         +new line 11\n\
                         +new line 12\n\
                          context line 13\n";

    #[test]
    fn verify_accepts_a_citation_on_an_added_line() {
        let mut findings = vec![finding("src/main.rs", "11")];
        verify(&mut findings, DIFF);
        assert!(!findings[0].evidence_unverified);
    }

    #[test]
    fn verify_accepts_a_citation_on_a_context_line() {
        let mut findings = vec![finding("src/main.rs", "10")];
        verify(&mut findings, DIFF);
        assert!(!findings[0].evidence_unverified);
    }

    #[test]
    fn verify_flags_a_line_number_that_does_not_exist_in_the_diff() {
        let mut findings = vec![finding("src/main.rs", "999")];
        verify(&mut findings, DIFF);
        assert!(findings[0].evidence_unverified);
    }

    #[test]
    fn verify_flags_a_file_that_is_not_in_the_diff_at_all() {
        let mut findings = vec![finding("src/other.rs", "11")];
        verify(&mut findings, DIFF);
        assert!(findings[0].evidence_unverified);
    }

    #[test]
    fn verify_flags_a_non_numeric_line() {
        let mut findings = vec![finding("src/main.rs", "UNKNOWN")];
        verify(&mut findings, DIFF);
        assert!(findings[0].evidence_unverified);
    }

    #[test]
    fn verify_extracts_the_leading_number_from_a_decorated_line_field() {
        let mut findings = vec![finding("src/main.rs", "L11")];
        verify(&mut findings, DIFF);
        assert!(!findings[0].evidence_unverified);
    }

    #[test]
    fn verify_rejects_an_old_side_only_line_number() {
        // Line 11 on the *old* side ("old line 11") no longer exists post-change — only the
        // new-side line 11/12 ("new line 11"/"new line 12") should verify.
        let diff = "diff --git a/x.rs b/x.rs\n+++ b/x.rs\n@@ -1,2 +1,1 @@\n-line one\n-line two\n+line one merged\n";
        let mut findings = vec![finding("x.rs", "2")];
        verify(&mut findings, diff);
        assert!(findings[0].evidence_unverified);
    }
}
