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
            c.title, c.tool, status, evidence
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
    /// 렌즈 리뷰가 실패한 경우의 에러 메시지들 — 조용히 무시하지 않고 리포트에 남긴다.
    pub lens_errors: &'a [String],
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
        lens_errors,
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

    if !lens_errors.is_empty() {
        md.push_str(&format!(
            "## ⚠ 렌즈 리뷰 실패 ({}건)\n\n일부 렌즈가 실패해 이 결과는 부분적입니다 — \
             아래 실패한 렌즈의 관점은 findings에 반영되지 않았습니다.\n\n",
            lens_errors.len()
        ));
        for e in lens_errors {
            md.push_str(&format!("- {}\n", e));
        }
        md.push('\n');
    }

    if !fix_results.is_empty() {
        md.push_str("## 이전 라운드 대비\n\n| Finding | Status | Evidence |\n|---|---|---|\n");
        for f in fix_results {
            md.push_str(&format!(
                "| {} | {} | {} |\n",
                f.finding_id, f.status, f.evidence
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
            p.evidence
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
                    r.requirement, r.status, r.evidence
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
            f.label,
            f.lens,
            f.reviewer,
            f.file,
            f.line,
            f.evidence,
            f.impact,
            f.recommendation,
            discourse_result
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
                g.file_line, g.practice, g.why
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
                a.round, m.kind, m.lens, m.target, m.detail, m.new_evidence
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
