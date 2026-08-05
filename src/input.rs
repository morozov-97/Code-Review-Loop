use anyhow::{Context, Result};
use std::path::Path;

/// 정규화된 입력. 없는 정보는 None으로 남기고 UNKNOWN 취급은 호출부(report)에서 표시한다.
pub struct Input {
    pub diff: String,
    pub changed_files: Vec<String>,
    pub added_lines: usize,
    pub removed_lines: usize,
    pub requirements: Option<String>,
    pub conventions: Option<String>,
    /// 결정론적 도구 결과. check id -> (status, evidence). 없으면 spec의 모든 항목 NOT_RUN.
    pub deterministic_results: Option<serde_json::Value>,
}

fn read_opt(p: &Option<std::path::PathBuf>) -> Result<Option<String>> {
    match p {
        None => Ok(None),
        Some(path) => {
            let s = std::fs::read_to_string(path)
                .with_context(|| format!("파일 읽기 실패: {}", path.display()))?;
            Ok(Some(s))
        }
    }
}

/// unified diff에서 변경 파일 목록과 +/- 라인 수를 추출한다. `diff --git a/X b/X` 또는 `+++ b/X` 헤더 기준.
fn parse_diff_stats(diff: &str) -> (Vec<String>, usize, usize) {
    let mut files: Vec<String> = Vec::new();
    let mut added = 0usize;
    let mut removed = 0usize;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            // trim_start_matches("b/")는 반복 제거라 실제 경로가 b/로 시작하는 레포(예: 최상위
            // 디렉터리명이 b)에서 diff 마커 b/까지 잘못 두 번 벗겨낸다 — strip_prefix로 1회만 제거.
            let path = rest.strip_prefix("b/").unwrap_or(rest);
            if path != "/dev/null" && !files.contains(&path.to_string()) {
                files.push(path.to_string());
            }
            continue;
        }
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (files, added, removed)
}

pub fn normalize(
    diff_path: &Path,
    requirements_path: &Option<std::path::PathBuf>,
    conventions_path: &Option<std::path::PathBuf>,
    deterministic_results_path: &Option<std::path::PathBuf>,
) -> Result<Input> {
    let diff = std::fs::read_to_string(diff_path)
        .with_context(|| format!("diff 파일 읽기 실패: {}", diff_path.display()))?;
    anyhow::ensure!(!diff.trim().is_empty(), "diff가 비어 있음");
    let (changed_files, added_lines, removed_lines) = parse_diff_stats(&diff);
    anyhow::ensure!(
        !changed_files.is_empty(),
        "diff에서 변경 파일을 찾지 못함(unified diff 형식 확인)"
    );

    let requirements = read_opt(requirements_path)?;
    let conventions = read_opt(conventions_path)?;
    let deterministic_results = match deterministic_results_path {
        None => None,
        Some(p) => {
            let s = std::fs::read_to_string(p)
                .with_context(|| format!("결정론 결과 파일 읽기 실패: {}", p.display()))?;
            Some(
                serde_json::from_str(&s)
                    .with_context(|| format!("결정론 결과 JSON 파싱 실패: {}", p.display()))?,
            )
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
}
