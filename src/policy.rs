use crate::input::Input;
use crate::spec::Spec;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub enum PolicyStatus {
    Pass,
    Fail,
    NotApplicable,
    NotConfigured,
}

impl PolicyStatus {
    pub fn label(&self) -> &'static str {
        match self {
            PolicyStatus::Pass => "PASS",
            PolicyStatus::Fail => "FAIL",
            PolicyStatus::NotApplicable => "N/A",
            PolicyStatus::NotConfigured => "NOT_CONFIGURED",
        }
    }
}

pub struct PolicyResult {
    pub title: String,
    pub status: PolicyStatus,
    pub evidence: String,
}

fn matches_any(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| matches_one(path, p))
}

fn is_word_boundary_char(c: char) -> bool {
    !c.is_alphanumeric()
}

/// Plain substring matching can't prevent a pattern from accidentally matching in the middle of
/// a different word — e.g. "tests" (a common typo for a trailing-slash-less pattern) matches
/// inside "contests/foo.rs" as "con[tests]/...", misclassifying a non-test file as a test file.
/// If the pattern's own first/last character is already punctuation (underscore, dot, etc.),
/// that itself acts as a boundary (e.g. "_test.", the ".md" following "README"), so the match is
/// only invalidated when the adjacent character on the path side is alphanumeric.
fn matches_word_boundary(path: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let pattern_starts_alnum = pattern.chars().next().is_some_and(|c| c.is_alphanumeric());
    let pattern_ends_alnum = pattern
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric());
    path.match_indices(pattern).any(|(idx, m)| {
        let left_ok = !pattern_starts_alnum
            || path[..idx]
                .chars()
                .next_back()
                .is_none_or(is_word_boundary_char);
        let right_ok = !pattern_ends_alnum
            || path[idx + m.len()..]
                .chars()
                .next()
                .is_none_or(is_word_boundary_char);
        left_ok && right_ok
    })
}

/// Patterns ending in a slash (directory-style), like "test/", are forced to match only at path
/// segment boundaries — otherwise "test/" would accidentally match mid-word inside
/// "contest/practice.rs" as "con[test/]...", misclassifying a non-test/doc file as a test file.
fn matches_one(path: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    match pattern.strip_suffix('/') {
        Some(dir) => {
            let wrapped = format!("/{path}/");
            let needle = format!("/{dir}/");
            wrapped.contains(&needle)
        }
        None => matches_word_boundary(path, pattern),
    }
}

/// Whether tests accompany the change. NOT_CONFIGURED when spec.test_path_patterns is unset.
/// Assumption: a file change matching neither the test nor doc patterns is treated as a
/// "behavior change" (design choice, uncertain).
fn tests_included(spec: &Spec, input: &Input) -> PolicyResult {
    if spec.test_path_patterns.is_empty() {
        return PolicyResult {
            title: "Tests accompany behavior changes".into(),
            status: PolicyStatus::NotConfigured,
            evidence: "spec.test_path_patterns 미설정".into(),
        };
    }
    let test_files: Vec<&String> = input
        .changed_files
        .iter()
        .filter(|f| matches_any(f, &spec.test_path_patterns))
        .collect();
    let behavior_files: Vec<&String> = input
        .changed_files
        .iter()
        .filter(|f| {
            !matches_any(f, &spec.test_path_patterns) && !matches_any(f, &spec.doc_path_patterns)
        })
        .collect();
    if behavior_files.is_empty() {
        return PolicyResult {
            title: "Tests accompany behavior changes".into(),
            status: PolicyStatus::NotApplicable,
            evidence: "동작 코드 변경 없음(테스트/문서 파일만 변경)".into(),
        };
    }
    if !test_files.is_empty() {
        PolicyResult {
            title: "Tests accompany behavior changes".into(),
            status: PolicyStatus::Pass,
            evidence: format!(
                "테스트 파일 변경: {}",
                test_files
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    } else {
        PolicyResult {
            title: "Tests accompany behavior changes".into(),
            status: PolicyStatus::Fail,
            evidence: format!(
                "동작 변경 파일({})에 대응하는 테스트 파일 변경 없음",
                behavior_files
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

fn diff_size(spec: &Spec, input: &Input) -> PolicyResult {
    if spec.diff_size_limit == 0 {
        return PolicyResult {
            title: "Diff size within configured limit".into(),
            status: PolicyStatus::NotConfigured,
            evidence: "spec.diff_size_limit 미설정".into(),
        };
    }
    let total = input.added_lines + input.removed_lines;
    if total <= spec.diff_size_limit {
        PolicyResult {
            title: "Diff size within configured limit".into(),
            status: PolicyStatus::Pass,
            evidence: format!(
                "변경 {total}줄 (+{}/-{}) ≤ 임계값 {}",
                input.added_lines, input.removed_lines, spec.diff_size_limit
            ),
        }
    } else {
        PolicyResult {
            title: "Diff size within configured limit".into(),
            status: PolicyStatus::Fail,
            evidence: format!(
                "변경 {total}줄 (+{}/-{}) > 임계값 {}",
                input.added_lines, input.removed_lines, spec.diff_size_limit
            ),
        }
    }
}

/// Whether docs/changelog accompany the change. NOT_CONFIGURED when spec.doc_path_patterns is unset.
/// Assumption: "public API/config change" is approximated as a file change matching neither the
/// test nor doc patterns (design choice, uncertain).
fn docs_updated(spec: &Spec, input: &Input) -> PolicyResult {
    if spec.doc_path_patterns.is_empty() {
        return PolicyResult {
            title: "Changelog/documentation updated".into(),
            status: PolicyStatus::NotConfigured,
            evidence: "spec.doc_path_patterns 미설정".into(),
        };
    }
    let doc_files: Vec<&String> = input
        .changed_files
        .iter()
        .filter(|f| matches_any(f, &spec.doc_path_patterns))
        .collect();
    let public_surface_files: Vec<&String> = input
        .changed_files
        .iter()
        .filter(|f| {
            !matches_any(f, &spec.test_path_patterns) && !matches_any(f, &spec.doc_path_patterns)
        })
        .collect();
    if public_surface_files.is_empty() {
        return PolicyResult {
            title: "Changelog/documentation updated".into(),
            status: PolicyStatus::NotApplicable,
            evidence: "공개 표면 변경으로 근사되는 파일 없음".into(),
        };
    }
    if !doc_files.is_empty() {
        PolicyResult {
            title: "Changelog/documentation updated".into(),
            status: PolicyStatus::Pass,
            evidence: format!(
                "문서 파일 변경: {}",
                doc_files
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    } else {
        PolicyResult {
            title: "Changelog/documentation updated".into(),
            status: PolicyStatus::Fail,
            evidence: "공개 표면 변경 있으나 문서/changelog 갱신 없음".into(),
        }
    }
}

pub fn check_all(spec: &Spec, input: &Input) -> Vec<PolicyResult> {
    vec![
        tests_included(spec, input),
        diff_size(spec, input),
        docs_updated(spec, input),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_one_rejects_mid_word_substring_for_directory_style_patterns() {
        assert!(!matches_one("src/contest/practice.rs", "test/"));
        assert!(!matches_one("src/latest_utils.rs", "test/"));
    }

    #[test]
    fn matches_one_accepts_real_directory_segment_for_directory_style_patterns() {
        assert!(matches_one("src/test/foo.rs", "test/"));
        assert!(matches_one("test/foo.rs", "test/"));
        assert!(matches_one("a/b/tests/foo.rs", "tests/"));
    }

    #[test]
    fn matches_one_keeps_plain_substring_behavior_for_non_slash_patterns() {
        assert!(matches_one("src/foo_test.rs", "_test."));
        assert!(matches_one("docs/README.md", "README"));
        assert!(!matches_one("src/foo.rs", "_test."));
    }

    #[test]
    fn matches_one_rejects_mid_word_substring_for_bare_word_patterns_without_trailing_slash() {
        // A common typo omitting the trailing slash ("tests" instead of "test/") — this spelling
        // must not reproduce the same class of bug already fixed for slash-terminated patterns.
        assert!(!matches_one("src/contests/foo.rs", "tests"));
        assert!(!matches_one("src/latest.rs", "test"));
    }

    #[test]
    fn matches_one_still_matches_bare_word_pattern_at_a_real_boundary() {
        assert!(matches_one("src/tests/foo.rs", "tests"));
        assert!(matches_one("a/b/tests_helpers.rs", "tests"));
    }

    #[test]
    fn matches_one_rejects_empty_pattern() {
        assert!(!matches_one("src/anything.rs", ""));
    }
}
