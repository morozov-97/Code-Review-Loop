use crate::discourse::Resolution;
use crate::input::Input;
use crate::lens::Finding;
use crate::policy::{PolicyResult, PolicyStatus};
use crate::requirements::RequirementCheck;
use std::collections::HashMap;

pub struct QuantSummary {
    pub verdict: String, // APPROVE|COMMENT|REQUEST_CHANGES|NEEDS_CONTEXT
    pub score: i64,      // 0-100
    pub score_deductions: Vec<String>,
    pub estimated_effort_1_5: u8,
    pub time_best_min: u32,
    pub time_average_min: u32,
    pub time_worst_min: u32,
}

fn severity_penalty(severity: &str) -> i64 {
    match severity {
        "P0" => 25,
        "P1" => 12,
        "P2" => 5,
        "P3" => 1,
        _ => 0,
    }
}

/// 확인된(CONFIRMED) finding만으로 100점에서 감점. 감점 근거를 문자열로 함께 남긴다.
/// 가정: 감점폭(P0=25/P1=12/P2=5/P3=1)은 설계 판단 — 실제 심각도 배분은 팀 정책에 맞춰 조정 필요(불확실).
fn score(findings: &[Finding], resolved: &HashMap<String, Resolution>) -> (i64, Vec<String>) {
    let mut total = 100i64;
    let mut deductions = Vec::new();
    for f in findings {
        if resolved.get(&f.id).map(|r| r.status.as_str()) == Some("CONFIRMED") {
            let p = severity_penalty(&f.severity);
            total -= p;
            deductions.push(format!(
                "[{}] {}:{} -{}점 — {}",
                f.severity, f.file, f.line, p, f.claim
            ));
        }
    }
    (total.max(0), deductions)
}

/// 변경 규모 기반 리뷰 노력 추정치. 가정: 임계값은 설계 판단(불확실), 팀 규모에 맞춰 조정 필요.
fn effort_and_time(input: &Input, lens_count: usize) -> (u8, u32, u32, u32) {
    let lines = input.added_lines + input.removed_lines;
    let mut effort: u8 = match lines {
        0..=50 => 1,
        51..=200 => 2,
        201..=500 => 3,
        501..=1000 => 4,
        _ => 5,
    };
    if input.changed_files.len() > 10 && effort < 5 {
        effort += 1;
    }
    if lens_count >= 4 && effort < 5 {
        effort += 1;
    }
    let effort = effort.min(5);
    let best = effort as u32 * 5;
    let average = effort as u32 * 15;
    let worst = effort as u32 * 40;
    (effort, best, average, worst)
}

fn verdict(
    findings: &[Finding],
    resolved: &HashMap<String, Resolution>,
    policies: &[PolicyResult],
    requirements: &Option<Vec<RequirementCheck>>,
) -> String {
    let confirmed: Vec<&Finding> = findings
        .iter()
        .filter(|f| resolved.get(&f.id).map(|r| r.status.as_str()) == Some("CONFIRMED"))
        .collect();

    if confirmed.iter().any(|f| f.severity == "P0") {
        return "REQUEST_CHANGES".to_string();
    }
    if policies.iter().any(|p| p.status == PolicyStatus::Fail) {
        return "REQUEST_CHANGES".to_string();
    }
    if confirmed.iter().any(|f| f.severity == "P1") {
        return "COMMENT".to_string();
    }
    if let Some(reqs) = requirements {
        if reqs
            .iter()
            .any(|r| r.status == "MISSING" || r.status == "AMBIGUOUS")
        {
            return "NEEDS_CONTEXT".to_string();
        }
    }
    if confirmed.is_empty() {
        "APPROVE".to_string()
    } else {
        "COMMENT".to_string()
    }
}

pub fn summarize(
    input: &Input,
    findings: &[Finding],
    resolved: &HashMap<String, Resolution>,
    policies: &[PolicyResult],
    requirements: &Option<Vec<RequirementCheck>>,
    lens_count: usize,
) -> QuantSummary {
    let (sc, deductions) = score(findings, resolved);
    let (effort, best, average, worst) = effort_and_time(input, lens_count);
    let v = verdict(findings, resolved, policies, requirements);
    QuantSummary {
        verdict: v,
        score: sc,
        score_deductions: deductions,
        estimated_effort_1_5: effort,
        time_best_min: best,
        time_average_min: average,
        time_worst_min: worst,
    }
}
