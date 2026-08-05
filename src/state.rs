use crate::discourse::Resolution;
use crate::lens::Finding;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 라운드 종료 시점의 findings·판정 스냅샷. 다음 라운드(--prior)가 이어받는다.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub round: usize,
    pub findings: Vec<Finding>,
    pub resolved: HashMap<String, Resolution>,
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
    serde_json::from_str(&s).with_context(|| format!("{} 파싱 실패", path.display()))
}
