//! #5 (deterministic-tool plugin groundwork): `semgrep.rs` and `cargo_audit.rs` each carried an
//! identical copy of "find a binary on PATH, respecting PATHEXT/exec-bit" and "run a subprocess
//! with a hard timeout that doesn't deadlock on full pipes" — the actual PATH/timeout mechanics
//! don't differ between deterministic tools, only what command line to build and how to parse
//! the output do. Pulled out here so a third tool (or the eventual trait-based plugin interface
//! this is a step toward) doesn't need its own third copy of subprocess plumbing.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// `is_file()` alone isn't enough — on Unix, files without the executable permission bit would
/// pass this check (and only fail at spawn time), while on Windows, a tool is typically installed
/// as `name.exe`/`name.cmd` rather than extension-less `name`, so it wouldn't be found at all
/// without checking PATHEXT. If both fail, the caller silently falls through to NOT_RUN even when
/// the tool is actually present.
pub(crate) fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| find_executable(&dir, bin))
}

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

/// Drains stdout/stderr on separate threads before polling starts (prevents the child from
/// blocking on a full pipe), then polls with `try_wait()` and kills on timeout. Returns None on
/// timeout or any I/O error — every deterministic-tool caller here treats every failure mode
/// identically (fall back to NOT_RUN rather than fabricating a guessed result).
pub(crate) fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> Option<std::process::Output> {
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = stdout_pipe {
            let _ = std::io::Read::read_to_end(&mut p, &mut buf);
        }
        buf
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = stderr_pipe {
            let _ = std::io::Read::read_to_end(&mut p, &mut buf);
        }
        buf
    });

    let start = std::time::Instant::now();
    let status = loop {
        if let Ok(Some(status)) = child.try_wait() {
            break status;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    Some(std::process::Output {
        status,
        stdout: stdout_handle.join().ok()?,
        stderr: stderr_handle.join().ok()?,
    })
}

/// Spawns `bin` with `args`, piping stdout/stderr, and waits with `wait_with_timeout`. The one
/// piece every deterministic-tool `try_run` needs beyond `which`/`wait_with_timeout` themselves —
/// factored out so a new tool's `try_run` is just "build args, call this, parse the JSON."
pub(crate) fn spawn_and_wait(
    bin: &std::path::Path,
    args: &[&str],
    timeout: Duration,
) -> Option<std::process::Output> {
    let child = Command::new(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    wait_with_timeout(child, timeout)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn wait_with_timeout_returns_output_when_process_finishes_in_time() {
        let child = Command::new("sh")
            .args(["-c", "echo hi"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = wait_with_timeout(child, Duration::from_secs(5)).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }

    #[test]
    fn wait_with_timeout_kills_and_returns_none_when_the_process_hangs() {
        let child = Command::new("sh")
            .args(["-c", "sleep 5"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = std::time::Instant::now();
        let out = wait_with_timeout(child, Duration::from_millis(300));
        assert!(out.is_none(), "a hanging process must time out to None");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "must return promptly around the timeout, not wait for the full sleep"
        );
    }

    #[test]
    fn find_executable_rejects_non_executable_file() {
        let dir = std::env::temp_dir().join("codereview-loop-procutil-find-exec-test-1");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not-executable");
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&path, perms).unwrap();

        assert!(find_executable(&dir, "not-executable").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_executable_accepts_executable_file() {
        let dir = std::env::temp_dir().join("codereview-loop-procutil-find-exec-test-2");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("is-executable");
        std::fs::write(&path, "#!/bin/sh\necho hi\n").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();

        assert_eq!(find_executable(&dir, "is-executable"), Some(path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawn_and_wait_runs_a_real_binary_and_returns_its_output() {
        let out = spawn_and_wait(
            std::path::Path::new("/bin/sh"),
            &["-c", "echo from-spawn-and-wait"],
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "from-spawn-and-wait"
        );
    }
}
