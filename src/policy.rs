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
    patterns.iter().any(|p| path.contains(p.as_str()))
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
