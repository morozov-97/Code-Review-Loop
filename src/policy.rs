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

/// 순수 substring 매칭은 패턴이 다른 단어 중간에서 우연히 매치되는 걸 못 막는다 — 예:
/// "tests"(트레일링 슬래시를 빠뜨린 흔한 오타)가 "contests/foo.rs"의 "con[tests]/..."처럼
/// 단어 중간에서 매치돼 테스트 파일이 아닌 걸 테스트 파일로 오판한다. 패턴 자신의 첫/끝
/// 글자가 이미 구두점(밑줄·점 등)이면 그 자체가 경계 역할을 하므로("_test.", "README" 뒤에
/// 오는 ".md") path 쪽 인접 글자가 영숫자일 때만 매치를 무효화한다.
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

/// "test/"처럼 슬래시로 끝나는(디렉터리 스타일) 패턴은 경로 세그먼트 경계에서만 매치되게
/// 강제한다 — 안 그러면 "test/"가 "contest/practice.rs"의 "con[test/]..."처럼 단어 중간에서
/// 우연히 매치돼 테스트/문서 파일이 아닌 것을 테스트 파일로 오판한다.
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

/// 테스트 동반 여부. spec.test_path_patterns 미설정 시 NOT_CONFIGURED.
/// 가정: 테스트/문서 패턴 둘 다 아닌 파일 변경 = "동작 변경"으로 간주(설계 판단, 불확실).
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

/// 문서/변경이력 동반 여부. spec.doc_path_patterns 미설정 시 NOT_CONFIGURED.
/// 가정: "공개 API/설정 변경"을 테스트·문서 패턴 둘 다 아닌 파일 변경으로 근사(설계 판단, 불확실).
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
        // 트레일링 슬래시를 빠뜨린 흔한 오타("test/" 대신 "tests") — 이미 고친
        // 슬래시-종료 패턴 버그와 동일 클래스가 이 스펠링에도 재현되면 안 된다.
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
