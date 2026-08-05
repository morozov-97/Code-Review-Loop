use std::path::PathBuf;
use std::process::Command;

/// semgrep이 PATH에 있으면 변경 파일 대상으로 실행해 deterministic_results 형태로 변환.
/// 없거나 실행/파싱에 실패하면 None — 호출부는 기존 NOT_RUN 경로로 폴백한다(추측 결과를 지어내지 않음).
/// dependency_sca/dataflow_taint/api_deprecation은 `--config=auto` 기본 룰셋만으로는
/// 못 채우므로 건드리지 않는다(NOT_RUN 유지).
pub fn try_run(changed_files: &[String]) -> Option<serde_json::Value> {
    let bin = which("semgrep")?;
    let existing: Vec<&str> = changed_files
        .iter()
        .map(|s| s.as_str())
        .filter(|f| std::path::Path::new(f).exists())
        .collect();
    if existing.is_empty() {
        return None;
    }

    let output = Command::new(&bin)
        .arg("--config=auto")
        .arg("--json")
        .arg("--quiet")
        .args(&existing)
        .output()
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let results = v.get("results")?.as_array()?;
    let has_findings = !results.is_empty();
    let secrets_hit = results.iter().any(|r| {
        r.get("check_id")
            .and_then(|c| c.as_str())
            .map(|c| c.contains("secret") || c.contains("hardcoded"))
            .unwrap_or(false)
    });

    Some(serde_json::json!({
        "sast": {
            "status": if has_findings { "fail" } else { "pass" },
            "evidence": format!("semgrep --config=auto 자동 실행: {} findings", results.len()),
        },
        "secrets": {
            "status": if secrets_hit { "fail" } else { "pass" },
            "evidence": "semgrep --config=auto 결과 기반 — 전용 secrets 스캐너 아니므로 참고용",
        },
    }))
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| find_executable(&dir, bin))
}

/// `is_file()`만으로는 부족하다 — Unix는 실행 권한 비트가 없는 파일도 통과시키고(spawn 시점에야
/// 실패), Windows는 보통 확장자 없는 "semgrep"이 아니라 "semgrep.exe"/"semgrep.cmd"로 설치되므로
/// PATHEXT를 안 보면 아예 못 찾는다. 둘 다 실패하면 semgrep이 실제로 있어도 조용히 NOT_RUN으로 샌다.
#[cfg(unix)]
fn find_executable(dir: &std::path::Path, bin: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let full = dir.join(bin);
    let meta = full.metadata().ok()?;
    if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
        Some(full)
    } else {
        None
    }
}

#[cfg(windows)]
fn find_executable(dir: &std::path::Path, bin: &str) -> Option<PathBuf> {
    let exact = dir.join(bin);
    if exact.is_file() {
        return Some(exact);
    }
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string());
    pathext.split(';').find_map(|ext| {
        let candidate = dir.join(format!("{bin}{ext}"));
        candidate.is_file().then_some(candidate)
    })
}

#[cfg(not(any(unix, windows)))]
fn find_executable(dir: &std::path::Path, bin: &str) -> Option<PathBuf> {
    let full = dir.join(bin);
    full.is_file().then_some(full)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("codereview-loop-semgrep-which-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn find_executable_rejects_non_executable_file() {
        let dir = temp_dir("non-exec");
        let bin_path = dir.join("semgrep");
        std::fs::write(&bin_path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(find_executable(&dir, "semgrep").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_executable_accepts_executable_file() {
        let dir = temp_dir("exec");
        let bin_path = dir.join("semgrep");
        std::fs::write(&bin_path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(find_executable(&dir, "semgrep"), Some(bin_path));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
