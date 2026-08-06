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

/// Escapes text before putting it in a markdown table cell — tables split columns on pipes and
/// rows on lines, so a `|` in the cell content shifts columns and a newline breaks the row itself.
/// Strings produced by the LLM/external tools (evidence, claim, etc.) may contain either character.
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
            escape_table_cell(&c.title),
            escape_table_cell(&c.tool),
            escape_table_cell(&status),
            escape_table_cell(&evidence)
        ));
    }
    md
}

/// All inputs needed to render the review subcommand's result. Grouped into a struct since there are many fields.
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
    /// Error messages from stages that tolerate partial failure, like lens review/good_things/requirements —
    /// kept in the report instead of silently ignored.
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
            // superseded_by is always attached explicitly by the code rather than relying solely
            // on the LLM's free-form text (evidence) — even if the evidence wording is missing
            // or ambiguous, which finding replaced it must always be verifiable in the report.
            let evidence = if f.status == "SUPERSEDED" && !f.superseded_by.is_empty() {
                format!("[{}로 대체됨] {}", f.superseded_by, f.evidence)
            } else {
                f.evidence.clone()
            };
            md.push_str(&format!(
                "| {} | {} | {} |\n",
                escape_table_cell(&f.finding_id),
                escape_table_cell(&f.status),
                escape_table_cell(&evidence)
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
            escape_table_cell(&f.line),
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

    // MERGED/UNCERTAIN (or otherwise unresolved) findings aren't reflected in score/verdict, and
    // used to be invisible everywhere in the report — they vanished entirely for being neither
    // CONFIRMED nor REJECTED. But when multiple lenses independently flag the same issue and
    // discourse fails to reach consensus (UNCERTAIN), or it gets absorbed into another finding
    // (MERGED), that's actually a signal a human should look at directly — we've actually seen a
    // real case where an SQL injection vanished entirely through this path.
    let needs_human_look: Vec<&Finding> = findings
        .iter()
        .filter(|f| {
            !matches!(
                resolved.get(&f.id).map(|r| r.status.as_str()),
                Some("CONFIRMED") | Some("REJECTED")
            )
        })
        .collect();
    if !needs_human_look.is_empty() {
        md.push_str(
            "### 사람이 봐야 할 항목 (확정도 기각도 아님 — score/verdict 미반영)\n\n\
             discourse가 합의에 이르지 못했거나(UNCERTAIN) 다른 finding에 병합(MERGED)된 \
             항목이다. 여러 렌즈가 독립적으로 같은 문제를 지적했을 수 있으니 직접 확인 권장.\n\n\
             | ID | Priority | Label | File:line | Claim | 상태 | 사유 |\n|---|---|---|---|---|---|---|\n",
        );
        for f in &needs_human_look {
            let r = resolved.get(&f.id);
            let status = r.map(|r| r.status.as_str()).unwrap_or("UNRESOLVED");
            let reason = r.map(|r| r.reason.as_str()).unwrap_or("");
            let reason = if status == "MERGED" {
                let target = r.map(|r| r.merged_into.as_str()).unwrap_or("");
                format!("{target}로 병합: {reason}")
            } else {
                reason.to_string()
            };
            md.push_str(&format!(
                "| {} | {} | {} | {}:{} | {} | {} | {} |\n",
                f.id,
                f.severity,
                escape_table_cell(&f.label),
                escape_table_cell(&f.file),
                escape_table_cell(&f.line),
                escape_table_cell(&f.claim),
                escape_table_cell(status),
                escape_table_cell(&reason)
            ));
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
        "| Round | Move | Challenge axis | Lens | Target | Detail | New evidence |\n|---|---|---|---|---|---|---|\n",
    );
    for a in audit {
        for m in &a.moves {
            let axis = if m.kind == "CHALLENGE" {
                m.challenge_axis.as_str()
            } else {
                ""
            };
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                a.round,
                escape_table_cell(&m.kind),
                escape_table_cell(axis),
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

    fn test_spec() -> Spec {
        Spec {
            name: "test".to_string(),
            context: String::new(),
            lenses: Vec::new(),
            deterministic_checks: Vec::new(),
            labels: vec!["security".to_string()],
            diff_size_limit: 0,
            test_path_patterns: Vec::new(),
            doc_path_patterns: Vec::new(),
        }
    }

    fn test_input() -> Input {
        Input {
            diff: "diff --git a/x b/x\n+++ b/x\n".to_string(),
            changed_files: vec!["x".to_string()],
            added_lines: 1,
            removed_lines: 0,
            requirements: None,
            conventions: None,
            deterministic_results: None,
        }
    }

    fn test_quant() -> QuantSummary {
        QuantSummary {
            verdict: "REQUEST_CHANGES".to_string(),
            score: 99,
            score_deductions: Vec::new(),
            estimated_effort_1_5: 1,
            time_best_min: 5,
            time_average_min: 15,
            time_worst_min: 40,
        }
    }

    fn test_finding(id: &str, claim: &str) -> Finding {
        Finding {
            id: id.to_string(),
            file: "src/users.rs".to_string(),
            line: "12".to_string(),
            claim: claim.to_string(),
            evidence: "format! 로 SQL 문자열 조립".to_string(),
            impact: String::new(),
            severity: "P1".to_string(),
            label: "security".to_string(),
            confidence: "high".to_string(),
            recommendation: String::new(),
            lens: "security".to_string(),
            reviewer: "Reviewer".to_string(),
        }
    }

    #[test]
    fn write_shows_uncertain_and_merged_findings_that_score_ignores() {
        // Real-world repro: 4 lenses independently flagged the same SQL injection, but discourse
        // couldn't land on CONFIRMED or REJECTED, so it was invisible everywhere in the report.
        let findings = vec![
            test_finding("security-r1-1", "raw SQL injection"),
            test_finding("security-r1-2", "동일 SQL injection, 다른 렌즈"),
        ];
        let mut resolved = HashMap::new();
        resolved.insert(
            "security-r1-1".to_string(),
            Resolution {
                finding_id: "security-r1-1".to_string(),
                status: "UNCERTAIN".to_string(),
                merged_into: String::new(),
                reason: "합의 실패(net=0.30)".to_string(),
            },
        );
        resolved.insert(
            "security-r1-2".to_string(),
            Resolution {
                finding_id: "security-r1-2".to_string(),
                status: "MERGED".to_string(),
                merged_into: "security-r1-1".to_string(),
                reason: "동일 근본 원인".to_string(),
            },
        );
        let spec = test_spec();
        let input = test_input();
        let quant = test_quant();
        let dir = std::env::temp_dir().join("codereview-loop-report-uncertain-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = write(ReportCtx {
            out_dir: &dir,
            spec: &spec,
            input: &input,
            selected_lenses: &["security".to_string()],
            round: 1,
            findings: &findings,
            resolved: &resolved,
            unverified: &[],
            good_things: &[],
            policies: &[],
            requirements: &None,
            audit: &[],
            quant: &quant,
            fix_results: &[],
            human_voice: None,
            stage_errors: &[],
        })
        .unwrap();
        let md = std::fs::read_to_string(&path).unwrap();

        let findings_section = md
            .split("## Findings")
            .nth(1)
            .unwrap()
            .split("### 사람이 봐야 할 항목")
            .next()
            .unwrap();
        assert!(
            !findings_section.contains("security-r1-1")
                && !findings_section.contains("security-r1-2"),
            "UNCERTAIN/MERGED finding이 CONFIRMED Findings 테이블에 섞이면 안 됨"
        );
        assert!(
            md.contains("사람이 봐야 할 항목"),
            "새 가시성 섹션이 렌더링돼야 함"
        );
        assert!(
            md.contains("security-r1-1"),
            "UNCERTAIN finding이 보여야 함"
        );
        assert!(md.contains("security-r1-2"), "MERGED finding이 보여야 함");
        assert!(
            md.contains("security-r1-1로 병합"),
            "MERGED 사유에 병합 대상이 보여야 함"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
