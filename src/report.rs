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

/// #123: renders a finding's file:line cell, appending a visible marker when
/// `evidence::verify` couldn't match the citation against an actual line in the diff — so a
/// reader knows to double check that citation before trusting it.
fn file_line_cell(f: &Finding) -> String {
    let base = format!(
        "{}:{}",
        escape_table_cell(&f.file),
        escape_table_cell(&f.line)
    );
    if f.evidence_unverified {
        format!("{base} ⚠️ unverified")
    } else {
        base
    }
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
        "# Code Review — {} (round {})\n\n",
        spec.name, round
    ));
    // #112/#115: verdict/score are computed purely from whatever findings survived — if a stage
    // failed (a lens erroring out, etc.), that's recorded in stage_errors but was previously
    // never reflected in the verdict itself, so a partial review could read as a clean,
    // fully-confident one unless you scrolled down to the separate stage-errors section below.
    // This puts the reliability signal right on the verdict line, where it's actually seen.
    // Reads from quant.completeness (not stage_errors directly) so this marker and the
    // programmatic signal on QuantSummary can never disagree with each other.
    let partial_marker = match quant.completeness {
        crate::quantify::ReviewCompleteness::Complete => String::new(),
        crate::quantify::ReviewCompleteness::Partial => " (PARTIAL — see ⚠ below)".to_string(),
        // Failed: every selected lens errored out — the verdict below reflects zero
        // defect-finding coverage, not "clean, just missing a supplementary stage".
        crate::quantify::ReviewCompleteness::Failed => {
            " (FAILED — no lens completed, see ⚠ below)".to_string()
        }
    };
    md.push_str(&format!(
        "**Verdict: {} _({})_{}**  ·  Score: {}/100  ·  Effort: {}/5  ·  {} files changed (+{}/-{})\n\n",
        quant.verdict,
        quant.verdict_reason.as_slug(),
        partial_marker,
        quant.score,
        quant.estimated_effort_1_5,
        input.changed_files.len(),
        input.added_lines,
        input.removed_lines,
    ));
    md.push_str(&format!(
        "Selected lenses: {}\n\n",
        selected_lenses.join(", ")
    ));

    if !stage_errors.is_empty() {
        md.push_str(&format!(
            "## ⚠ Some Stages Failed ({})\n\nThe stages below failed, so this result is partial — \
             the affected stage's perspective is not reflected in the findings or requirements results.\n\n",
            stage_errors.len()
        ));
        for e in stage_errors {
            md.push_str(&format!("- {}\n", e));
        }
        md.push('\n');
    }

    if !fix_results.is_empty() {
        md.push_str(
            "## Compared to Previous Round\n\n| Finding | Status | Evidence |\n|---|---|---|\n",
        );
        for f in fix_results {
            // superseded_by is always attached explicitly by the code rather than relying solely
            // on the LLM's free-form text (evidence) — even if the evidence wording is missing
            // or ambiguous, which finding replaced it must always be verifiable in the report.
            let evidence = if f.status == "SUPERSEDED" && !f.superseded_by.is_empty() {
                format!("[Superseded by {}] {}", f.superseded_by, f.evidence)
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

    md.push_str("## Policy Checks\n\n| Policy | Status | Evidence |\n|---|---|---|\n");
    for p in policies {
        md.push_str(&format!(
            "| {} | {} | {} |\n",
            p.title,
            p.status.label(),
            escape_table_cell(&p.evidence)
        ));
    }
    md.push('\n');

    md.push_str("## Quantitative Summary\n\n");
    md.push_str(&format!(
        "- Estimated review effort: {}/5\n- Estimated review time: best {} min, average {} min, worst {} min\n",
        quant.estimated_effort_1_5, quant.time_best_min, quant.time_average_min, quant.time_worst_min
    ));
    if quant.score_deductions.is_empty() {
        md.push_str("- No deductions (no CONFIRMED findings)\n\n");
    } else {
        md.push_str("- Deduction evidence:\n");
        for d in &quant.score_deductions {
            md.push_str(&format!("  - {}\n", d));
        }
        md.push('\n');
    }

    md.push_str("## Requirements Verification\n\n");
    match requirements {
        None => md.push_str("(No requirements provided — verification skipped)\n\n"),
        Some(reqs) if reqs.is_empty() => md.push_str("(No requirements)\n\n"),
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
    md.push_str(&format!("Allowed labels: {}\n\n", spec.labels_prompt()));
    md.push_str("| ID | Priority | Label | Lens | Reviewer | File:line | Evidence | Impact | Recommendation | Reason |\n|---|---|---|---|---|---|---|---|---|---|\n");
    for f in &confirmed {
        let r = resolved.get(&f.id);
        let discourse_result = r.map(|r| r.reason.as_str()).unwrap_or("");
        md.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            f.id,
            f.severity,
            escape_table_cell(&f.label),
            f.lens,
            escape_table_cell(&f.reviewer),
            file_line_cell(f),
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
        md.push_str("### Rejected Candidates\n\n");
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
        md.push_str("### Needs Verification (insufficient evidence to promote to finding)\n\n");
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
            "### Needs Human Review (neither confirmed nor rejected — not reflected in score/verdict)\n\n\
             These are items where discourse failed to reach consensus (UNCERTAIN) or that were merged \
             into another finding (MERGED). Multiple lenses may have independently flagged the same \
             issue, so manual review is recommended.\n\n\
             | ID | Priority | Label | File:line | Claim | Status | Reason |\n|---|---|---|---|---|---|---|\n",
        );
        for f in &needs_human_look {
            let r = resolved.get(&f.id);
            let status = r.map(|r| r.status.as_str()).unwrap_or("UNRESOLVED");
            let reason = r.map(|r| r.reason.as_str()).unwrap_or("");
            let reason = if status == "MERGED" {
                let target = r.map(|r| r.merged_into.as_str()).unwrap_or("");
                format!("Merged into {target}: {reason}")
            } else {
                reason.to_string()
            };
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                f.id,
                f.severity,
                escape_table_cell(&f.label),
                file_line_cell(f),
                escape_table_cell(&f.claim),
                escape_table_cell(status),
                escape_table_cell(&reason)
            ));
        }
        md.push('\n');
    }

    md.push_str("## Good Things\n\n");
    if good_things.is_empty() {
        md.push_str("None observed\n\n");
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

    md.push_str("## Deterministic Checks\n\n");
    md.push_str(&deterministic_table(spec, &input.deterministic_results));
    md.push('\n');

    md.push_str("## Discourse Audit\n\n");
    md.push_str(
        "| Round | Move | Confidence | Challenge axis | Lens | Target | Detail | New evidence |\n|---|---|---|---|---|---|---|---|\n",
    );
    for a in audit {
        for m in &a.moves {
            let axis = if m.kind == "CHALLENGE" {
                m.challenge_axis.as_str()
            } else {
                ""
            };
            // #163: AGREE/CHALLENGE carry a self-reported confidence that confidence_weight()
            // uses to compute vote_net — previously invisible outside the raw JSON, so neither a
            // human auditing a report nor any offline analysis (e.g.
            // evals/szz-bench/calibrate_confidence.py) could check it against anything.
            let confidence = if m.kind == "AGREE" || m.kind == "CHALLENGE" {
                m.confidence.as_str()
            } else {
                ""
            };
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
                a.round,
                escape_table_cell(&m.kind),
                escape_table_cell(confidence),
                escape_table_cell(axis),
                escape_table_cell(&m.lens),
                escape_table_cell(&m.target),
                escape_table_cell(&m.detail),
                escape_table_cell(&m.new_evidence)
            ));
        }
    }
    if audit.iter().any(|a| {
        a.moves
            .iter()
            .any(|m| m.kind == "AGREE" || m.kind == "CHALLENGE")
    }) {
        // Measured (not assumed): confidence tiers here are only weakly correlated with actual
        // correctness (see README's Real-world validation section) — surfaced right next to the
        // table itself, since a reader of one PR's report won't see the repo-level README caveat.
        md.push_str(
            "\n_Confidence above is self-reported by the discourse round, not independently \
verified — measured to be only weakly correlated with actual correctness. Don't use it alone to \
decide what to read first._\n",
        );
    }

    if let Some(hv) = human_voice {
        md.push_str("\n## Human-voice Review\n\n");
        md.push_str(hv);
        md.push('\n');
    }

    let path = out_dir.join("report.md");
    std::fs::write(&path, md).with_context(|| format!("failed to write {}", path.display()))?;
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
        "## Can Be Split?\n\n{} — {}\n\n",
        d.can_be_split, d.can_be_split_note
    ));
    md.push_str("## TODO/FIXME (new lines, deterministic scan)\n\n");
    if todos.is_empty() {
        md.push_str("None\n");
    } else {
        for t in todos {
            md.push_str(&format!("- {}\n", t));
        }
    }
    let path = out_dir.join("describe.md");
    std::fs::write(&path, md).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

pub fn write_improve(out_dir: &Path, suggestions: &[Suggestion]) -> Result<PathBuf> {
    let mut md = String::new();
    md.push_str("# Code Improvement Suggestions\n\n");
    if suggestions.is_empty() {
        md.push_str("No suggestions\n");
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
    std::fs::write(&path, md).with_context(|| format!("failed to write {}", path.display()))?;
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
            ignored_path_patterns: Vec::new(),
            scoring: Default::default(),
            discourse: Default::default(),
            security: Default::default(),
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
            config: crate::core::RunConfig::default(),
        }
    }

    fn test_quant() -> QuantSummary {
        QuantSummary {
            verdict: "REQUEST_CHANGES".to_string(),
            verdict_reason: crate::quantify::VerdictReason::ConfirmedP0Defect,
            score: 99,
            score_deductions: Vec::new(),
            estimated_effort_1_5: 1,
            time_best_min: 5,
            time_average_min: 15,
            time_worst_min: 40,
            completeness: crate::quantify::ReviewCompleteness::Complete,
        }
    }

    fn test_finding(id: &str, claim: &str) -> Finding {
        Finding {
            id: id.to_string(),
            file: "src/users.rs".to_string(),
            line: "12".to_string(),
            claim: claim.to_string(),
            evidence: "SQL string built via format!".to_string(),
            impact: String::new(),
            severity: "P1".to_string(),
            label: "security".to_string(),
            confidence: "high".to_string(),
            recommendation: String::new(),
            lens: "security".to_string(),
            reviewer: "Reviewer".to_string(),
            evidence_unverified: false,
        }
    }

    #[test]
    fn write_shows_uncertain_and_merged_findings_that_score_ignores() {
        // Real-world repro: 4 lenses independently flagged the same SQL injection, but discourse
        // couldn't land on CONFIRMED or REJECTED, so it was invisible everywhere in the report.
        let findings = vec![
            test_finding("security-r1-1", "raw SQL injection"),
            test_finding("security-r1-2", "same SQL injection, different lens"),
        ];
        let mut resolved = HashMap::new();
        resolved.insert(
            "security-r1-1".to_string(),
            Resolution {
                finding_id: "security-r1-1".to_string(),
                status: "UNCERTAIN".to_string(),
                merged_into: String::new(),
                reason: "Consensus failed (net=0.30)".to_string(),
            },
        );
        resolved.insert(
            "security-r1-2".to_string(),
            Resolution {
                finding_id: "security-r1-2".to_string(),
                status: "MERGED".to_string(),
                merged_into: "security-r1-1".to_string(),
                reason: "Same root cause".to_string(),
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
            .split("### Needs Human Review")
            .next()
            .unwrap();
        assert!(
            !findings_section.contains("security-r1-1")
                && !findings_section.contains("security-r1-2"),
            "UNCERTAIN/MERGED findings must not appear in the CONFIRMED Findings table"
        );
        assert!(
            md.contains("Needs Human Review"),
            "The new visibility section should render"
        );
        assert!(
            md.contains("security-r1-1"),
            "UNCERTAIN finding should be visible"
        );
        assert!(
            md.contains("security-r1-2"),
            "MERGED finding should be visible"
        );
        assert!(
            md.contains("Merged into security-r1-1"),
            "MERGED reason should show the merge target"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_marks_the_verdict_line_partial_when_a_stage_failed() {
        // #112: verdict/score are computed purely from whatever findings survived a partial
        // run — this must be visible right on the verdict line itself, not only in the
        // separate stage-errors section further down where a quick glance would miss it.
        let spec = test_spec();
        let input = test_input();
        let mut quant = test_quant();
        quant.completeness = crate::quantify::ReviewCompleteness::Partial;
        let dir = std::env::temp_dir().join("codereview-loop-report-partial-verdict-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = write(ReportCtx {
            out_dir: &dir,
            spec: &spec,
            input: &input,
            selected_lenses: &["security".to_string()],
            round: 1,
            findings: &[],
            resolved: &HashMap::new(),
            unverified: &[],
            good_things: &[],
            policies: &[],
            requirements: &None,
            audit: &[],
            quant: &quant,
            fix_results: &[],
            human_voice: None,
            stage_errors: &["lens review failed: security".to_string()],
        })
        .unwrap();
        let md = std::fs::read_to_string(&path).unwrap();

        let verdict_line = md.lines().find(|l| l.starts_with("**Verdict:")).unwrap();
        assert!(
            verdict_line.contains("(PARTIAL"),
            "verdict line must carry a partial-review marker when stage_errors is non-empty:\n{verdict_line}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_renders_a_moves_confidence_in_the_discourse_audit_table() {
        // #163: a move's self-reported confidence (what confidence_weight() actually uses to
        // compute vote_net) used to be invisible outside the raw JSON — neither a human auditing
        // a report nor any offline analysis could check it against anything.
        let spec = test_spec();
        let input = test_input();
        let quant = test_quant();
        let dir = std::env::temp_dir().join("codereview-loop-report-discourse-confidence-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let audit = vec![DiscourseAudit {
            round: 1,
            moves: vec![crate::discourse::Move {
                kind: "AGREE".to_string(),
                lens: "reviewer".to_string(),
                target: "design-r1-1".to_string(),
                new_evidence: "corroborating".to_string(),
                confidence: "high".to_string(),
                ..Default::default()
            }],
        }];

        let path = write(ReportCtx {
            out_dir: &dir,
            spec: &spec,
            input: &input,
            selected_lenses: &["design".to_string()],
            round: 1,
            findings: &[],
            resolved: &HashMap::new(),
            unverified: &[],
            good_things: &[],
            policies: &[],
            requirements: &None,
            audit: &audit,
            quant: &quant,
            fix_results: &[],
            human_voice: None,
            stage_errors: &[],
        })
        .unwrap();
        let md = std::fs::read_to_string(&path).unwrap();

        let audit_line = md.lines().find(|l| l.contains("AGREE")).unwrap();
        assert!(
            audit_line.contains("high"),
            "discourse audit row must carry the move's confidence:\n{audit_line}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_renders_a_confidence_reliability_caveat_when_the_audit_has_a_confidence_bearing_move()
    {
        // A reader of one PR's report never sees the repo-level README caveat about confidence
        // being weakly correlated with correctness -- it needs to be right next to the table.
        let spec = test_spec();
        let input = test_input();
        let quant = test_quant();
        let dir = std::env::temp_dir().join("codereview-loop-report-confidence-caveat-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let audit = vec![DiscourseAudit {
            round: 1,
            moves: vec![crate::discourse::Move {
                kind: "AGREE".to_string(),
                lens: "reviewer".to_string(),
                target: "design-r1-1".to_string(),
                new_evidence: "corroborating".to_string(),
                confidence: "high".to_string(),
                ..Default::default()
            }],
        }];

        let path = write(ReportCtx {
            out_dir: &dir,
            spec: &spec,
            input: &input,
            selected_lenses: &["design".to_string()],
            round: 1,
            findings: &[],
            resolved: &HashMap::new(),
            unverified: &[],
            good_things: &[],
            policies: &[],
            requirements: &None,
            audit: &audit,
            quant: &quant,
            fix_results: &[],
            human_voice: None,
            stage_errors: &[],
        })
        .unwrap();
        let md = std::fs::read_to_string(&path).unwrap();

        assert!(
            md.contains("weakly correlated with actual correctness"),
            "report must warn readers not to triage by confidence alone:\n{md}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_does_not_render_a_confidence_caveat_when_the_audit_has_no_confidence_bearing_move() {
        // CONNECT/SURFACE moves carry no confidence -- the caveat would be noise with nothing to
        // warn about.
        let spec = test_spec();
        let input = test_input();
        let quant = test_quant();
        let dir = std::env::temp_dir().join("codereview-loop-report-no-confidence-caveat-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let audit = vec![DiscourseAudit {
            round: 1,
            moves: vec![crate::discourse::Move {
                kind: "SURFACE".to_string(),
                lens: "reviewer".to_string(),
                target: "".to_string(),
                new_evidence: "".to_string(),
                confidence: "".to_string(),
                ..Default::default()
            }],
        }];

        let path = write(ReportCtx {
            out_dir: &dir,
            spec: &spec,
            input: &input,
            selected_lenses: &["design".to_string()],
            round: 1,
            findings: &[],
            resolved: &HashMap::new(),
            unverified: &[],
            good_things: &[],
            policies: &[],
            requirements: &None,
            audit: &audit,
            quant: &quant,
            fix_results: &[],
            human_voice: None,
            stage_errors: &[],
        })
        .unwrap();
        let md = std::fs::read_to_string(&path).unwrap();

        assert!(
            !md.contains("weakly correlated with actual correctness"),
            "no confidence-bearing move happened, so the caveat has nothing to warn about:\n{md}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_renders_the_verdict_reason_slug_on_the_verdict_line() {
        // #189: the verdict line alone used to be unable to say whether REQUEST_CHANGES meant
        // a confirmed defect or an unrelated policy failure.
        let spec = test_spec();
        let input = test_input();
        let quant = test_quant(); // VerdictReason::ConfirmedP0Defect
        let dir = std::env::temp_dir().join("codereview-loop-report-verdict-reason-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = write(ReportCtx {
            out_dir: &dir,
            spec: &spec,
            input: &input,
            selected_lenses: &["security".to_string()],
            round: 1,
            findings: &[],
            resolved: &HashMap::new(),
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

        let verdict_line = md.lines().find(|l| l.starts_with("**Verdict:")).unwrap();
        assert!(
            verdict_line.contains("confirmed_p0_defect"),
            "verdict line must carry the verdict_reason slug:\n{verdict_line}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_leaves_the_verdict_line_unmarked_when_no_stage_failed() {
        let spec = test_spec();
        let input = test_input();
        let quant = test_quant();
        let dir = std::env::temp_dir().join("codereview-loop-report-clean-verdict-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = write(ReportCtx {
            out_dir: &dir,
            spec: &spec,
            input: &input,
            selected_lenses: &["security".to_string()],
            round: 1,
            findings: &[],
            resolved: &HashMap::new(),
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

        let verdict_line = md.lines().find(|l| l.starts_with("**Verdict:")).unwrap();
        assert!(
            !verdict_line.contains("PARTIAL"),
            "verdict line must not be marked partial when no stage failed:\n{verdict_line}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_marks_the_verdict_line_failed_not_partial_when_completeness_is_failed() {
        // #115 follow-up: Failed (zero lens coverage) must read differently from Partial (some
        // other stage failed but the core review still has real findings behind it) — a
        // reader glancing only at the verdict line needs to tell "trust this a little less"
        // apart from "there's essentially nothing here".
        let spec = test_spec();
        let input = test_input();
        let mut quant = test_quant();
        quant.completeness = crate::quantify::ReviewCompleteness::Failed;
        let dir = std::env::temp_dir().join("codereview-loop-report-failed-verdict-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = write(ReportCtx {
            out_dir: &dir,
            spec: &spec,
            input: &input,
            selected_lenses: &["security".to_string()],
            round: 1,
            findings: &[],
            resolved: &HashMap::new(),
            unverified: &[],
            good_things: &[],
            policies: &[],
            requirements: &None,
            audit: &[],
            quant: &quant,
            fix_results: &[],
            human_voice: None,
            stage_errors: &["lens review failed: security".to_string()],
        })
        .unwrap();
        let md = std::fs::read_to_string(&path).unwrap();

        let verdict_line = md.lines().find(|l| l.starts_with("**Verdict:")).unwrap();
        assert!(
            verdict_line.contains("(FAILED"),
            "verdict line must say FAILED, not just PARTIAL, when no lens completed:\n{verdict_line}"
        );
        assert!(!verdict_line.contains("(PARTIAL"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
