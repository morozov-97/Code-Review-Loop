use crate::discourse::Resolution;
use crate::lens::Finding;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// state.json schema version. Bump this when fields are added/removed/change meaning —
/// that way, running an old --out directory through --prior with a new binary gives a clear
/// error instead of silently misinterpreting it.
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// Snapshot of findings and verdicts at the end of a round. Picked up by the next round (--prior).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    /// state.json files from before versioning was introduced (below v1) lacked this field,
    /// so it defaults to 0 — load() explicitly rejects it if this differs from STATE_SCHEMA_VERSION.
    #[serde(default)]
    pub schema_version: u32,
    pub round: usize,
    pub findings: Vec<Finding>,
    pub resolved: HashMap<String, Resolution>,
}

impl State {
    pub fn new(
        round: usize,
        findings: Vec<Finding>,
        resolved: HashMap<String, Resolution>,
    ) -> Self {
        State {
            schema_version: STATE_SCHEMA_VERSION,
            round,
            findings,
            resolved,
        }
    }
}

pub fn write(out_dir: &Path, state: &State) -> Result<PathBuf> {
    let path = out_dir.join("state.json");
    std::fs::write(&path, serde_json::to_string_pretty(state)?)
        .with_context(|| format!("{} 쓰기 실패", path.display()))?;
    Ok(path)
}

pub fn load(dir: &Path) -> Result<State> {
    let path = if dir.is_dir() {
        dir.join("state.json")
    } else {
        dir.to_path_buf()
    };
    let s = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "{} 읽기 실패 (--prior는 이전 --out 디렉터리)",
            path.display()
        )
    })?;
    let state: State =
        serde_json::from_str(&s).with_context(|| format!("{} 파싱 실패", path.display()))?;
    anyhow::ensure!(
        state.schema_version == STATE_SCHEMA_VERSION,
        "{} 의 schema_version({})이 현재 버전({})과 다름 — 호환되지 않는 codereview 버전 간 \
         --prior는 지원하지 않는다. --prior 없이 새로 시작하거나, state.json을 만든 버전과 \
         같은 버전으로 다시 실행할 것.",
        path.display(),
        state.schema_version,
        STATE_SCHEMA_VERSION
    );
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_rejects_state_json_with_stale_or_missing_schema_version() {
        let dir = std::env::temp_dir().join("codereview-loop-state-schema-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Simulates a state.json that predates versioning and lacks the schema_version field entirely.
        std::fs::write(
            dir.join("state.json"),
            r#"{"round":1,"findings":[],"resolved":{}}"#,
        )
        .unwrap();

        let err = load(&dir).expect_err("missing/mismatched schema_version must be rejected");
        assert!(err.to_string().contains("schema_version"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_accepts_state_json_with_current_schema_version() {
        let dir = std::env::temp_dir().join("codereview-loop-state-schema-test-ok");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, &State::new(1, Vec::new(), HashMap::new())).unwrap();

        let loaded = load(&dir).expect("current schema_version must load fine");
        assert_eq!(loaded.round, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
