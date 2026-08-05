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

fn push_unique(files: &mut Vec<String>, p: String) {
    if !files.contains(&p) {
        files.push(p);
    }
}

/// unified diff에서 변경 파일 목록과 +/- 라인 수를 추출한다. `diff --git a/X b/X` 또는 `+++ b/X` 헤더 기준.
///
/// hunk body(각 파일의 첫 `@@ ... @@` 이후, 다음 `diff --git` 전까지)에 있는지를 명시적으로
/// 추적한다 — 추적 없이 순수 접두어 매칭만 하면, 추가/삭제된 줄의 **내용 자체**가 `++ `/`-- `
/// 로 시작할 때(마커 `+`/`-` + 그 내용 = raw 줄이 `+++ `/`--- `가 됨) hunk body 줄이 파일
/// 헤더로 오인돼 가짜 경로가 changed_files에 섞이고, 그 줄 자체는 라인카운트에서 누락된다.
/// `diff --git `/`@@`는 hunk body 줄(항상 `+`/`-`/` ` 마커로 시작)이 raw 상태로는 절대
/// 흉내 낼 수 없는 유일한 앵커라 이 모호성이 없다.
fn parse_diff_stats(diff: &str) -> (Vec<String>, usize, usize) {
    let mut files: Vec<String> = Vec::new();
    let mut added = 0usize;
    let mut removed = 0usize;
    // 삭제-only 파일은 "+++ /dev/null"이라 새 경로가 없다 — 직전 "--- a/X" 줄의 경로를
    // 대신 써야 changed_files가 비지 않는다(순수 삭제 diff가 파이프라인 시작조차 못 하던 버그).
    let mut pending_old_path: Option<String> = None;
    let mut in_hunk_body = false;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            in_hunk_body = false;
            pending_old_path = None;
            // rename/binary/신규 빈 파일은 hunk 자체가 없어 --- / +++ 가 아예 안 나온다
            // (git이 아예 그 줄들을 안 씀) — 유일하게 항상 존재하는 이 헤더에서 b/ 쪽
            // 경로를 미리 잡아둔다. --- / +++ 가 이어지면 거기서 다시 push하되
            // push_unique라 중복되지 않는다.
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
        // 여기부터는 diff --git 이후, 첫 @@ 이전 — 실제 헤더 구간.
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
            // trim_start_matches("b/")는 반복 제거라 실제 경로가 b/로 시작하는 레포(예: 최상위
            // 디렉터리명이 b)에서 diff 마커 b/까지 잘못 두 번 벗겨낸다 — strip_prefix로 1회만 제거.
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
        // index/similarity/rename from·to/new file mode/Binary files 등 메타데이터 — 무시.
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
        // 추가된 줄의 내용 자체가 "++ "로 시작하면(흔한 예: diff/patch 파일을 리뷰 대상으로
        // 다루는 경우) marker(+) + content = raw 줄이 "+++ ..."가 되어, hunk body 상태
        // 추적 없이는 이걸 새 파일 헤더로 오인해 가짜 경로가 섞이고 이 줄은 라인카운트에서
        // 누락됐다.
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
            "가짜 경로가 섞이면 안 됨"
        );
        assert_eq!(
            added, 1,
            "+++ 로 시작하는 hunk body 줄도 추가 라인으로 카운트돼야 함"
        );
        assert_eq!(removed, 0);
    }

    #[test]
    fn parse_diff_stats_captures_pure_rename_with_no_hunks() {
        // 100% 유사도 rename은 hunk 자체가 없다 — --- / +++ 가 아예 안 나온다.
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
