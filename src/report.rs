use crate::describe::Describe;
use crate::discourse::{DiscourseAudit, Resolution};
use crate::fixcheck::FixStatus;
use crate::improve::Suggestion;
use crate::input::Input;
use crate::lens::{Finding, GoodThing};
use crate::policy::PolicyResult;
use crate::quantify::QuantSummary;
use crate::requirements::RequirementCheck;
use crate::spec::Spec;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 마크다운 테이블 셀에 넣기 전에 이스케이프한다 — 테이블은 파이프로 열을 나누고 줄 단위로
/// 행을 나누므로, 셀 내용에 `|`가 있으면 열이 밀리고 개행이 있으면 행 자체가 깨진다.
/// LLM/외부 도구가 만든 문자열(evidence, claim 등)은 이 두 문자를 포함할 수 있다.
fn escape_table_cell(s: &str) -> String {
    let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
    normalized.replace('|', "\\|").replace('\n', "<br>")
}

fn severity_rank(s: &str) -> u8 {
    match s {
        "P0" => 0,
        "P1" => 1,
        "P2" => 2,
        "P3" => 3,
        _ => 4,
    }
}

fn deterministic_table(spec: &Spec, results: &Option<serde_json::Value>) -> String {
    let mut md = String::new();
    md.push_str("| Check | Expected tool | Status | Evidence |\n|---|---|---|---|\n");
    for c in &spec.deterministic_checks {
        let (status, evidence) = match results {
            None => ("NOT_RUN".to_string(), String::new()),
            Some(v) => {
                let entry = v.get(&c.id);
                let status = entry
                    .and_then(|e| e.get("status"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("NOT_RUN")
                    .to_string();
                let evidence = entry
                    .and_then(|e| e.get("evidence"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                (status, evidence)
            }
        };
        md.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            c.title,
            c.tool,
            escape_table_cell(&status),
            escape_table_cell(&evidence)
        ));
    }
    md
}

/// review 서브커맨드 결과를 렌더링하는 데 필요한 모든 입력. 필드가 많아 구조체로 묶는다.
pub struct ReportCtx<'a> {
    pub out_dir: &'a Path,
    pub spec: &'a Spec,
    pub input: &'a Input,
    pub selected_lenses: &'a [String],
    pub round: usize,
    pub findings: &'a [Finding],
    pub resolved: &'a HashMap<String, Resolution>,
    pub unverified: &'a [(String, String)],
    pub good_things: &'a [GoodThing],
    pub policies: &'a [PolicyResult],
    pub requirements: &'a Option<Vec<RequirementCheck>>,
    pub audit: &'a [DiscourseAudit],
    pub quant: &'a QuantSummary,
    pub fix_results: &'a [FixStatus],
    pub human_voice: Option<&'a str>,
    /// 렌즈 리뷰/good_things/requirements 등 부분 실패 허용 단계의 에러 메시지들 —
    /// 조용히 무시하지 않고 리포트에 남긴다.
    pub stage_errors: &'a [String],
}

pub fn write(ctx: ReportCtx) -> Result<PathBuf> {
    let ReportCtx {
        out_dir,
        spec,
        input,
        selected_lenses,
        round,
        findings,
        resolved,
        unverified,
        good_things,
        policies,
        requirements,
        audit,
        quant,
        fix_results,
        human_voice,
        stage_errors,
    } = ctx;

    let mut md = String::new();

    md.push_str(&format!(
        "# 코드 리뷰 — {} (round {})\n\n",
        spec.name, round
    ));
    md.push_str(&format!(
        "**Verdict: {}**  ·  Score: {}/100  ·  Effort: {}/5  ·  변경 파일 {}개 (+{}/-{})\n\n",
        quant.verdict,
        quant.score,
        quant.estimated_effort_1_5,
        input.changed_files.len(),
        input.added_lines,
        input.removed_lines,
    ));
    md.push_str(&format!("선택 렌즈: {}\n\n", selected_lenses.join(", ")));

    if !stage_errors.is_empty() {
        md.push_str(&format!(
            "## ⚠ 일부 단계 실패 ({}건)\n\n아래 단계가 실패해 이 결과는 부분적입니다 — \
             해당 단계의 관점/판정은 findings·requirements 결과에 반영되지 않았습니다.\n\n",
            stage_errors.len()
        ));
        for e in stage_errors {
            md.push_str(&format!("- {}\n", e));
        }
        md.push('\n');
    }

    if !fix_results.is_empty() {
        md.push_str("## 이전 라운드 대비\n\n| Finding | Status | Evidence |\n|---|---|---|\n");
        for f in fix_results {
            md.push_str(&format!(
                "| {} | {} | {} |\n",
                f.finding_id,
                f.status,
                escape_table_cell(&f.evidence)
            ));
        }
        md.push('\n');
    }

    md.push_str("## Policy checks\n\n| Policy | Status | Evidence |\n|---|---|---|\n");
    for p in policies {
        md.push_str(&format!(
            "| {} | {} | {} |\n",
            p.title,
            p.status.label(),
            escape_table_cell(&p.evidence)
        ));
    }
    md.push('\n');

    md.push_str("## 정량 요약\n\n");
    md.push_str(&format!(
        "- estimated_effort_to_review: {}/5\n- review time cost: best {}분 / average {}분 / worst {}분\n",
        quant.estimated_effort_1_5, quant.time_best_min, quant.time_average_min, quant.time_worst_min
    ));
    if quant.score_deductions.is_empty() {
        md.push_str("- 감점 없음 (CONFIRMED finding 없음)\n\n");
    } else {
        md.push_str("- 감점 근거:\n");
        for d in &quant.score_deductions {
            md.push_str(&format!("  - {}\n", d));
        }
        md.push('\n');
    }

    md.push_str("## Requirements Verification\n\n");
    match requirements {
        None => md.push_str("(요구사항 미제공 — 검증 생략)\n\n"),
        Some(reqs) if reqs.is_empty() => md.push_str("(요구사항 없음)\n\n"),
        Some(reqs) => {
            md.push_str("| Requirement | Status | Evidence or gap |\n|---|---|---|\n");
            for r in reqs {
                md.push_str(&format!(
                    "| {} | {} | {} |\n",
                    escape_table_cell(&r.requirement),
                    r.status,
                    escape_table_cell(&r.evidence)
                ));
            }
            md.push('\n');
        }
    }

    let mut confirmed: Vec<&Finding> = findings
        .iter()
        .filter(|f| resolved.get(&f.id).map(|r| r.status.as_str()) == Some("CONFIRMED"))
        .collect();
    confirmed.sort_by_key(|f| severity_rank(&f.severity));

    md.push_str("## Findings\n\n");
    md.push_str(&format!("허용 label: {}\n\n", spec.labels_prompt()));
    md.push_str("| ID | Priority | Label | Lens | Reviewer | File:line | Evidence | Impact | Recommendation | Discourse result |\n|---|---|---|---|---|---|---|---|---|---|\n");
    for f in &confirmed {
        let r = resolved.get(&f.id);
        let discourse_result = r.map(|r| r.reason.as_str()).unwrap_or("");
        md.push_str(&format!(
            "| {} | {} | {} | {} | {} | {}:{} | {} | {} | {} | {} |\n",
            f.id,
            f.severity,
            escape_table_cell(&f.label),
            f.lens,
            escape_table_cell(&f.reviewer),
            escape_table_cell(&f.file),
            f.line,
            escape_table_cell(&f.evidence),
            escape_table_cell(&f.impact),
            escape_table_cell(&f.recommendation),
            escape_table_cell(discourse_result)
        ));
    }
    md.push('\n');

    let rejected: Vec<&Finding> = findings
        .iter()
        .filter(|f| resolved.get(&f.id).map(|r| r.status.as_str()) == Some("REJECTED"))
        .collect();
    if !rejected.is_empty() {
        md.push_str("### 기각된 후보\n\n");
        for f in &rejected {
            let reason = resolved.get(&f.id).map(|r| r.reason.as_str()).unwrap_or("");
            md.push_str(&format!(
                "- {} ({}:{}) — {}\n",
                f.id, f.file, f.line, reason
            ));
        }
        md.push('\n');
    }

    if !unverified.is_empty() {
        md.push_str("### 검증 필요 사항 (근거 부족으로 finding 미승격)\n\n");
        for (lens_id, item) in unverified {
            md.push_str(&format!("- [{}] {}\n", lens_id, item));
        }
        md.push('\n');
    }

    md.push_str("## Good Things\n\n");
    if good_things.is_empty() {
        md.push_str("관찰되지 않음\n\n");
    } else {
        md.push_str("| File:line | Good practice | Why it should be preserved |\n|---|---|---|\n");
        for g in good_things {
            md.push_str(&format!(
                "| {} | {} | {} |\n",
                escape_table_cell(&g.file_line),
                escape_table_cell(&g.practice),
                escape_table_cell(&g.why)
            ));
        }
        md.push('\n');
    }

    md.push_str("## Deterministic checks\n\n");
    md.push_str(&deterministic_table(spec, &input.deterministic_results));
    md.push('\n');

    md.push_str("## Discourse audit\n\n");
    md.push_str(
        "| Round | Move | Lens | Target | Detail | New evidence |\n|---|---|---|---|---|---|\n",
    );
    for a in audit {
        for m in &a.moves {
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                a.round,
                m.kind,
                escape_table_cell(&m.lens),
                escape_table_cell(&m.target),
                escape_table_cell(&m.detail),
                escape_table_cell(&m.new_evidence)
            ));
        }
    }

    if let Some(hv) = human_voice {
        md.push_str("\n## Human-voice Review\n\n");
        md.push_str(hv);
        md.push('\n');
    }

    let path = out_dir.join("report.md");
    std::fs::write(&path, md).with_context(|| format!("{} 쓰기 실패", path.display()))?;
    Ok(path)
}

pub fn write_describe(out_dir: &Path, d: &Describe, todos: &[String]) -> Result<PathBuf> {
    let mut md = String::new();
    md.push_str(&format!("# {}\n\n{}\n\n", d.title, d.summary));
    md.push_str("## Walkthrough\n\n");
    for w in &d.walkthrough {
        md.push_str(&format!("- {}\n", w));
    }
    md.push_str(&format!("\n## Labels\n\n{}\n\n", d.labels.join(", ")));
    md.push_str(&format!(
        "## can_be_split\n\n{} — {}\n\n",
        d.can_be_split, d.can_be_split_note
    ));
    md.push_str("## TODO/FIXME (신규 라인, 결정론적 스캔)\n\n");
    if todos.is_empty() {
        md.push_str("없음\n");
    } else {
        for t in todos {
            md.push_str(&format!("- {}\n", t));
        }
    }
    let path = out_dir.join("describe.md");
    std::fs::write(&path, md).with_context(|| format!("{} 쓰기 실패", path.display()))?;
    Ok(path)
}

pub fn write_improve(out_dir: &Path, suggestions: &[Suggestion]) -> Result<PathBuf> {
    let mut md = String::new();
    md.push_str("# 코드 개선 제안\n\n");
    if suggestions.is_empty() {
        md.push_str("제안 없음\n");
    }
    for s in suggestions {
        md.push_str(&format!(
            "## {} — {} [{}]\n\n",
            s.relevant_file, s.one_sentence_summary, s.label
        ));
        md.push_str(&format!("{}\n\n", s.suggestion_content));
        md.push_str(&format!(
            "{}\n\n",
            crate::promptctx::fenced(&s.language, &format!("// before\n{}", s.existing_code))
        ));
        md.push_str(&format!(
            "{}\n\n",
            crate::promptctx::fenced(&s.language, &format!("// after\n{}", s.improved_code))
        ));
    }
    let path = out_dir.join("improve.md");
    std::fs::write(&path, md).with_context(|| format!("{} 쓰기 실패", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_table_cell_escapes_pipe() {
        assert_eq!(
            escape_table_cell("value with | pipe"),
            "value with \\| pipe"
        );
    }

    #[test]
    fn escape_table_cell_converts_newlines_to_br() {
        assert_eq!(escape_table_cell("line1\nline2"), "line1<br>line2");
        assert_eq!(escape_table_cell("line1\r\nline2"), "line1<br>line2");
    }

    #[test]
    fn escape_table_cell_leaves_plain_text_untouched() {
        assert_eq!(
            escape_table_cell("nothing special here"),
            "nothing special here"
        );
    }

    #[test]
    fn escape_table_cell_handles_both_at_once() {
        assert_eq!(escape_table_cell("a | b\nc | d"), "a \\| b<br>c \\| d");
    }
}
