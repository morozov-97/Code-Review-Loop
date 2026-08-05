use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 리뷰 렌즈(Google eng-practices 12축 중 diff 성격에 맞게 선택되는 항목).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Lens {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub guide: String,
    /// true면 렌즈 선정 단계에서 매번 강제 포함(Functionality, Good Things).
    #[serde(default)]
    pub always: bool,
    /// 이 렌즈를 고르게 하는 신호(선택 프롬프트에 그대로 삽입).
    #[serde(default)]
    pub signal: String,
    /// 캐릭터화 페르소나 이름(비우면 무페르소나). 동조성(sycophancy) 억제 목적.
    #[serde(default)]
    pub persona_name: String,
    /// 페르소나의 관점/원칙 한 줄.
    #[serde(default)]
    pub persona_voice: String,
    /// generalist | specialist | famous_engineer | custom. 표시용, 로직에 관여하지 않음.
    #[serde(default)]
    pub tier: String,
}

/// 결정론적 도구(Semgrep/CodeQL 등) 체크리스트 항목. LLM이 판정하지 않고 외부 결과를 그대로 표시.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeterministicCheck {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub tool: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Spec {
    pub name: String,
    /// 심사 맥락(도메인/조직 배경). 프롬프트에 그대로 삽입.
    #[serde(default)]
    pub context: String,
    pub lenses: Vec<Lens>,
    #[serde(default)]
    pub deterministic_checks: Vec<DeterministicCheck>,
    /// findings에 허용되는 label 목록.
    pub labels: Vec<String>,
    /// diff 총 변경 라인 수가 이 값을 넘으면 policy `diff_size` FAIL. 0이면 미설정(N/A).
    #[serde(default)]
    pub diff_size_limit: usize,
    /// 테스트 파일로 인정하는 경로 패턴(부분일치).
    #[serde(default)]
    pub test_path_patterns: Vec<String>,
    /// 문서/변경이력 파일로 인정하는 경로 패턴(부분일치).
    #[serde(default)]
    pub doc_path_patterns: Vec<String>,
}

impl Spec {
    pub fn load(path: &Path) -> Result<Spec> {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("스펙 파일 읽기 실패: {}", path.display()))?;
        let spec: Spec = toml::from_str(&s)
            .with_context(|| format!("스펙 TOML 파싱 실패: {}", path.display()))?;
        anyhow::ensure!(!spec.lenses.is_empty(), "lenses 비어 있음");
        anyhow::ensure!(!spec.labels.is_empty(), "labels 비어 있음");

        // lens_by_id()는 첫 매치만 반환한다 — id가 비어있거나 중복되면 그 id를 참조하는
        // --lenses/선정 결과가 조용히 엉뚱한(또는 의도와 다른) 렌즈로 매핑될 수 있다.
        // TOML 파싱 시점에 바로 잡아야지 런타임에 예측 불가하게 새면 안 된다.
        let mut seen_ids = std::collections::HashSet::new();
        for l in &spec.lenses {
            anyhow::ensure!(
                !l.id.trim().is_empty(),
                "lenses에 id가 빈 항목이 있음(title=\"{}\")",
                l.title
            );
            anyhow::ensure!(
                seen_ids.insert(l.id.clone()),
                "lenses에 중복 id 있음: \"{}\"",
                l.id
            );
        }

        Ok(spec)
    }

    pub fn lens_by_id(&self, id: &str) -> Option<&Lens> {
        self.lenses.iter().find(|l| l.id == id)
    }

    pub fn always_lenses(&self) -> Vec<&Lens> {
        self.lenses.iter().filter(|l| l.always).collect()
    }

    pub fn optional_lenses(&self) -> Vec<&Lens> {
        self.lenses.iter().filter(|l| !l.always).collect()
    }

    pub fn labels_prompt(&self) -> String {
        self.labels
            .iter()
            .map(|l| format!("\"{l}\""))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_spec(name: &str, toml: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("codereview-loop-spec-load-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, toml).unwrap();
        path
    }

    #[test]
    fn load_rejects_duplicate_lens_ids() {
        let path = write_spec(
            "dup.toml",
            r#"
name = "t"
labels = ["bug"]
[[lenses]]
id = "design"
title = "Design"
[[lenses]]
id = "design"
title = "Design again"
"#,
        );
        let err = Spec::load(&path).expect_err("duplicate lens id must be rejected");
        assert!(err.to_string().contains("중복"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_rejects_empty_lens_id() {
        let path = write_spec(
            "empty-id.toml",
            r#"
name = "t"
labels = ["bug"]
[[lenses]]
id = ""
title = "No id"
"#,
        );
        let err = Spec::load(&path).expect_err("empty lens id must be rejected");
        assert!(err.to_string().contains("빈"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_accepts_valid_unique_lens_ids() {
        let path = write_spec(
            "ok.toml",
            r#"
name = "t"
labels = ["bug"]
[[lenses]]
id = "design"
title = "Design"
[[lenses]]
id = "tests"
title = "Tests"
"#,
        );
        let spec = Spec::load(&path).expect("valid spec should load");
        assert_eq!(spec.lenses.len(), 2);
        let _ = std::fs::remove_file(&path);
    }
}
