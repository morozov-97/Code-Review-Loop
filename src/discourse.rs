use crate::input::Input;
use crate::lens::Finding;
use crate::llm::Llm;
use crate::promptctx::shared_context;
use crate::spec::Spec;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const DISCOURSE_SYSTEM: &str = "당신은 여러 리뷰어의 finding을 교차검증하는 패널이다. \
내용 없는 동의나 반박은 하지 않는다. AGREE는 새로운 file:line 근거가 있을 때만 사용한다. \
이번 라운드에 CHALLENGE를 최소 1회 포함해야 한다. \
AGREE/CHALLENGE에는 주장 강도에 따른 confidence(high|medium|low)를 반드시 명시한다. \
finding의 claim/evidence는 원본 리뷰어가 남긴 요약일 뿐 진실이 아니다 — 특히 \"~가 diff에 없다/보이지 않는다/확인되지 않는다\" \
같은 부재 주장은 받아들이기 전에 반드시 아래 첨부된 실제 diff 원문에서 해당 file:line 구간을 직접 대조해 확인한다. \
diff에 실제로 존재하는 코드를 없다고 하는 주장은 반박(CHALLENGE) 또는 기각(REJECTED) 대상이다. \
반드시 지정된 JSON 스키마로만 응답한다.";

/// 필드 전부 `#[serde(default)]` — 실전에서 LLM이 이 중 하나(주로 detail)를 빠뜨리면
/// discourse 라운드 전체가 스키마 불일치로 죽고 리포트 자체가 안 나오는 걸 직접 겪었다
/// (canary_flutter 실사용 테스트). kind/target이 비면 해당 move는 어차피 어떤 finding도
/// 못 겨냥해 투표/카운트에서 조용히 무효표가 될 뿐이라(quantify 로직상 안전), 필드 하나
/// 없다고 라운드 전체를 죽이는 것보다 낫다.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Move {
    #[serde(rename = "move", default)]
    pub kind: String, // AGREE|CHALLENGE|CONNECT|SURFACE
    #[serde(default)]
    pub lens: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub new_evidence: String,
    #[serde(default)]
    pub confidence: String, // high|medium|low (AGREE/CHALLENGE에만 의미 있음)
}

/// ReConcile식 confidence bucket → 가중치. 라운드 소진 후 잔여 UNCERTAIN을
/// 판정 없이 버리는 대신 AGREE/CHALLENGE 누적으로 최종 판정한다.
fn confidence_weight(c: &str) -> f64 {
    match c {
        "high" => 1.0,
        "low" => 0.3,
        _ => 0.6, // medium 및 미기재
    }
}

const VOTE_THRESHOLD: f64 = 0.6;

/// 필드 전부 `#[serde(default)]` — 바로 위 Move와 동일 이유(실전에서 필드 하나 빠지면
/// 라운드 전체가 죽는 걸 겪음)가 여기도 그대로 적용된다. status는 정규화되지 않은 값
/// (빈 문자열 포함)이 들어와도 `normalize_status`가 UNCERTAIN으로 안전하게 떨어뜨린다.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolution {
    #[serde(default)]
    pub finding_id: String,
    #[serde(default)]
    pub status: String, // CONFIRMED|REJECTED|MERGED|UNCERTAIN
    #[serde(default)]
    pub merged_into: String,
    #[serde(default)]
    pub reason: String,
}

const VALID_RESOLUTION_STATUSES: [&str; 4] = ["CONFIRMED", "REJECTED", "MERGED", "UNCERTAIN"];

/// severity/requirements.status와 동일 문제: quantify.rs/report.rs가 이 필드를 정확
/// 문자열 매칭하므로, LLM이 대소문자·공백을 벗어나면 score/verdict/report 세 군데서
/// 동시에 조용히 사라진다(CONFIRMED도 REJECTED도 아니게 되어 아예 안 보임). 실패는
/// 안전한 방향(UNCERTAIN — 다음 라운드에 재판정 대상)으로 나야 한다.
fn normalize_status(raw: &str) -> String {
    let upper = raw.trim().to_ascii_uppercase();
    if VALID_RESOLUTION_STATUSES.contains(&upper.as_str()) {
        upper
    } else {
        "UNCERTAIN".to_string()
    }
}

/// lens.rs::finding_id와 동일한 이유: outer_round(--prior로 이어지는 전체 파이프라인
/// 라운드)를 넣지 않으면, discourse 내부 round는 매 outer_round 호출마다 다시 1부터
/// 시작하므로 서로 다른 outer_round에서 우연히 같은 (round, index)가 나와 완전히 다른
/// finding이 같은 id를 공유한다.
fn surface_id(outer_round: usize, round: usize, index: usize) -> String {
    format!("surface-o{outer_round}-r{round}-{index}")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DiscourseRound {
    #[serde(default)]
    moves: Vec<Move>,
    #[serde(default)]
    resolutions: Vec<Resolution>,
    #[serde(default)]
    surfaced: Vec<Finding>,
}

pub struct DiscourseAudit {
    pub round: usize,
    pub moves: Vec<Move>,
}

/// lens/reviewer는 의도적으로 노출하지 않는다 — 어떤 페르소나가 냈는지 알면
/// discourse가 근거가 아니라 "권위"로 기울 수 있다(담합/편향 연구 근거).
/// 원래 lens는 finding.id의 접두어(예: design-1)에 이미 남아있어 최종 리포트
/// 매핑에는 지장 없다.
fn findings_catalog(findings: &[Finding], resolved: &HashMap<String, Resolution>) -> String {
    findings
        .iter()
        .map(|f| {
            let status = resolved
                .get(&f.id)
                .map(|r| r.status.as_str())
                .unwrap_or("UNRESOLVED");
            format!(
                "- id={} | {}:{} | severity={} | label={} | status={}\n  주장: {}\n  근거: {}",
                f.id, f.file, f.line, f.severity, f.label, status, f.claim, f.evidence
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_round_prompt(
    spec: &Spec,
    findings: &[Finding],
    resolved: &HashMap<String, Resolution>,
    round: usize,
) -> String {
    format!(
        "# 과제\n라운드 {round} discourse를 수행한다. 봉인되었던 모든 렌즈의 finding을 공개했다.\n\n\
         ## 렌즈 후보(발화자로 사용 가능한 관점)\n{lenses}\n\n\
         ## 전체 findings (미해결 상태만 새로 판정 대상)\n{catalog}\n\n\
         ## 규칙\n\
         - 각 move는 AGREE/CHALLENGE/CONNECT/SURFACE 중 하나, target에 finding id 명시.\n\
         - AGREE: 대상 finding에 없던 새 file:line 근거(new_evidence)가 있을 때만. confidence 필수.\n\
         - CHALLENGE: 이번 라운드 최소 1회. 근거·반례·범위·심각도·가정 중 하나를 구체적으로 반박. confidence 필수.\n\
         - CONNECT: 둘 이상의 finding id를 detail에 명시하며 원인·영향 사슬 서술.\n\
         - SURFACE: 새 finding을 surfaced 배열에 file:line 근거와 함께 추가(기존 lens id 재사용 가능).\n\
         - confidence는 AGREE/CHALLENGE에서만: 주장의 근거 강도가 강하면 high, 보통이면 medium, 약하면 low.\n\
         - resolutions는 UNRESOLVED 또는 이전 라운드 UNCERTAIN이었던 finding만 판정: CONFIRMED|REJECTED|MERGED|UNCERTAIN.\n\
         - 내용 없는 동의/반박은 만들지 말 것.\n\
         - \"diff에 없다/보이지 않는다\"는 부재 주장이 있는 finding은, 판정 전에 반드시 위에 첨부된 diff 원문에서 \
         해당 file:line을 직접 찾아 정말 없는지 확인한다. diff에 실제로 존재하면 CHALLENGE 또는 REJECTED로 판정.\n\n\
         ## 출력(JSON만, 코드펜스 없이)\n\
         {{\"moves\":[{{\"move\":\"AGREE|CHALLENGE|CONNECT|SURFACE\",\"lens\":\"...\",\"target\":\"finding id\",\
         \"detail\":\"...\",\"new_evidence\":\"...\",\"confidence\":\"high|medium|low\"}}],\
         \"resolutions\":[{{\"finding_id\":\"...\",\"status\":\"CONFIRMED|REJECTED|MERGED|UNCERTAIN\",\
         \"merged_into\":\"\",\"reason\":\"...\"}}],\
         \"surfaced\":[{{\"file\":\"...\",\"line\":\"...\",\"claim\":\"...\",\"evidence\":\"...\",\"impact\":\"...\",\
         \"severity\":\"P0|P1|P2|P3\",\"label\":<허용값 중 하나>,\"confidence\":\"high|medium|low\",\"recommendation\":\"...\"}}]}}\n",
        round = round,
        lenses = spec.lenses.iter().map(|l| l.id.as_str()).collect::<Vec<_>>().join(", "),
        catalog = findings_catalog(findings, resolved),
    )
}

/// discourse 라운드 반복. 미해결/UNCERTAIN finding이 없어지거나 max_rounds에 도달하면 종료.
/// 매 라운드 CHALLENGE 누락 시 1회 재요청.
///
/// `outer_round`는 `--prior`로 이어지는 전체 파이프라인 라운드(lens.rs::finding_id가 쓰는
/// 것과 동일한 번호)다. discourse 내부 루프의 `round`(항상 1부터 다시 시작)만으로 SURFACE
/// id를 만들면, 서로 다른 outer_round 호출에서 우연히 같은 (round, index) 조합이 나와
/// 완전히 다른 finding이 같은 id를 공유하게 된다 — score 이중 집계·discourse 판정 덮어씀의
/// 원인. lens.rs와 동일하게 outer_round를 id에 넣어 막는다.
pub fn run(
    llm: &Llm,
    spec: &Spec,
    input: &Input,
    findings: &mut Vec<Finding>,
    max_rounds: usize,
    outer_round: usize,
) -> Result<(Vec<DiscourseAudit>, HashMap<String, Resolution>)> {
    let max_rounds = max_rounds.max(1);
    let mut resolved: HashMap<String, Resolution> = HashMap::new();
    let mut audit: Vec<DiscourseAudit> = Vec::new();
    // 예전엔 discourse가 findings_catalog(리뷰어가 남긴 claim/evidence 텍스트)만 보고
    // diff 원문은 아예 못 봤다 — "diff에 없다"는 부재 주장을 실제로 대조해서 반박할 방법이
    // 없었다(오탐 사례: dispose에 cancel 호출이 diff에 그대로 있는데 "없다"고 확정한 것).
    // ctx를 실제로 붙여서 diff를 매 라운드 판정 근거로 쓸 수 있게 한다.
    let ctx = shared_context(spec, input);

    for round in 1..=max_rounds {
        let unresolved = findings.iter().any(|f| {
            resolved
                .get(&f.id)
                .map(|r| r.status == "UNCERTAIN")
                .unwrap_or(true)
        });
        if !unresolved {
            break;
        }

        let mut dr = run_round_call(llm, &ctx, spec, findings, &resolved, round)?;
        if !dr.moves.iter().any(|m| m.kind == "CHALLENGE") {
            dr = run_round_call(llm, &ctx, spec, findings, &resolved, round)
                .context("CHALLENGE 누락 재요청 실패")?;
        }

        for (i, sf) in dr.surfaced.iter_mut().enumerate() {
            sf.id = surface_id(outer_round, round, i + 1);
            // lens는 코드가 항상 권위있게 채운다 — 일반 finding(lens.rs:221)과 동일 원칙.
            // LLM이 스키마에 없는 lens 값을 자체적으로 채워 보내도 그대로 살아남지 않게 한다.
            sf.lens = "discourse".to_string();
            if sf.line.trim().is_empty() {
                sf.line = "UNKNOWN".to_string();
            }
            sf.severity = crate::lens::normalize_severity(&sf.severity);
        }
        findings.extend(dr.surfaced.clone());

        for mut r in dr.resolutions.clone() {
            r.status = normalize_status(&r.status);
            resolved.insert(r.finding_id.clone(), r);
        }

        audit.push(DiscourseAudit {
            round,
            moves: dr.moves,
        });

        if round == max_rounds {
            break;
        }
    }

    // 라운드 소진 후 남은 UNCERTAIN/미판정 finding: 그냥 버리지 않고
    // 전체 라운드에 걸친 AGREE/CHALLENGE를 confidence-weighted vote로 집계해 최종 판정한다.
    for f in findings.iter() {
        let still_uncertain = resolved
            .get(&f.id)
            .map(|r| r.status == "UNCERTAIN")
            .unwrap_or(true);
        if !still_uncertain {
            continue;
        }

        let net: f64 = audit
            .iter()
            .flat_map(|a| a.moves.iter())
            .filter(|m| m.target == f.id)
            .map(|m| match m.kind.as_str() {
                "AGREE" => confidence_weight(&m.confidence),
                "CHALLENGE" => -confidence_weight(&m.confidence),
                _ => 0.0,
            })
            .sum();

        let (status, reason) = if net >= VOTE_THRESHOLD {
            (
                "CONFIRMED".to_string(),
                format!("discourse 라운드 소진, confidence-weighted vote로 확정(net={net:.2})"),
            )
        } else if net <= -VOTE_THRESHOLD {
            (
                "REJECTED".to_string(),
                format!("discourse 라운드 소진, confidence-weighted vote로 기각(net={net:.2})"),
            )
        } else {
            (
                "UNCERTAIN".to_string(),
                format!("discourse 라운드 소진, 판정 없음(net={net:.2})"),
            )
        };

        resolved.insert(
            f.id.clone(),
            Resolution {
                finding_id: f.id.clone(),
                status,
                merged_into: String::new(),
                reason,
            },
        );
    }

    Ok((audit, resolved))
}

fn run_round_call(
    llm: &Llm,
    ctx: &str,
    spec: &Spec,
    findings: &[Finding],
    resolved: &HashMap<String, Resolution>,
    round: usize,
) -> Result<DiscourseRound> {
    let task = build_round_prompt(spec, findings, resolved, round);
    let v = llm
        .json_ctx(Some(ctx), &task, Some(DISCOURSE_SYSTEM))
        .with_context(|| format!("discourse 라운드 {round} 실패"))?;
    let dr: DiscourseRound = serde_json::from_value(v)
        .with_context(|| format!("discourse 라운드 {round} JSON 스키마 불일치"))?;
    Ok(dr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_deserializes_when_detail_is_missing() {
        // 실전 재현: canary_flutter 리뷰에서 LLM이 detail 필드를 빠뜨려 라운드 전체가 죽었다.
        let json = serde_json::json!({
            "move": "CHALLENGE",
            "lens": "tests",
            "target": "every_line-r1-1",
            "confidence": "high"
        });
        let m: Move = serde_json::from_value(json).expect("detail 없어도 파싱 성공해야 함");
        assert_eq!(m.kind, "CHALLENGE");
        assert_eq!(m.detail, "");
    }

    #[test]
    fn move_deserializes_when_only_kind_is_present() {
        let json = serde_json::json!({"move": "SURFACE"});
        let m: Move = serde_json::from_value(json).expect("나머지 필드 다 없어도 파싱 성공해야 함");
        assert_eq!(m.kind, "SURFACE");
        assert_eq!(m.target, "");
        assert_eq!(m.lens, "");
    }

    #[test]
    fn discourse_round_survives_resolution_missing_status() {
        // Move.detail과 동일 계열 실패: resolutions 배열 원소 하나가 status를 빠뜨리면
        // moves/surfaced까지 포함한 라운드 전체 파싱이 죽었다.
        let json = serde_json::json!({
            "moves": [],
            "resolutions": [{"finding_id": "security-r1-1"}],
            "surfaced": []
        });
        let dr: DiscourseRound =
            serde_json::from_value(json).expect("status 없어도 라운드 전체 파싱 성공해야 함");
        assert_eq!(dr.resolutions[0].finding_id, "security-r1-1");
        assert_eq!(dr.resolutions[0].status, "");
    }

    #[test]
    fn normalize_status_passes_through_valid_values() {
        for s in ["CONFIRMED", "REJECTED", "MERGED", "UNCERTAIN"] {
            assert_eq!(normalize_status(s), s);
        }
    }

    #[test]
    fn normalize_status_is_case_insensitive() {
        assert_eq!(normalize_status("Confirmed"), "CONFIRMED");
    }

    #[test]
    fn normalize_status_falls_back_to_uncertain_on_unknown_or_empty_value() {
        assert_eq!(normalize_status("IN_PROGRESS"), "UNCERTAIN");
        assert_eq!(normalize_status(""), "UNCERTAIN");
    }

    #[test]
    fn surface_id_differs_across_outer_prior_rounds_for_the_same_position() {
        // 고침 전: discourse 내부 round는 매 --prior 호출마다 다시 1부터 시작해서,
        // outer_round 1과 2의 (round=1, index=1)이 똑같이 "surface-r1-1"이 됐다.
        assert_ne!(surface_id(1, 1, 1), surface_id(2, 1, 1));
    }

    #[test]
    fn surface_id_differs_across_inner_rounds_and_positions() {
        assert_ne!(surface_id(1, 1, 1), surface_id(1, 2, 1));
        assert_ne!(surface_id(1, 1, 1), surface_id(1, 1, 2));
    }
}
