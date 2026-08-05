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
    std::env::split_paths(&path).find_map(|dir| {
        let full = dir.join(bin);
        if full.is_file() {
            Some(full)
        } else {
            None
        }
    })
}
