use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
pub const OPENROUTER_DEFAULT_MODEL: &str = "openai/gpt-oss-120b";
/// ureq는 타임아웃 기본값이 무제한이라 명시적으로 안 걸면 네트워크 정체 시 프로세스가
/// 영원히 블록될 수 있다. CI 등 자동화 환경에서 특히 치명적 — DNS부터 응답 본문 수신까지
/// 전체를 하나의 상한으로 묶는 게(개별 구간별 상한보다) "절대 이 시간 넘게 안 멈춘다"는
/// 보장이 더 직접적이라 timeout_global 하나로 건다.
const HTTP_TIMEOUT_GLOBAL: Duration = Duration::from_secs(90);
/// claude -p 서브프로세스도 network 콜과 마찬가지로 무제한 대기 위험이 있다(외부 CLI가
/// hang하면 리뷰 전체가 영원히 멈춤) — README상 "초 단위~분 단위" 소요를 감안해 넉넉히 잡음.
const CLAUDE_CLI_TIMEOUT: Duration = Duration::from_secs(600);

/// LLM 호출 백엔드. ClaudeCli = `claude -p` 서브프로세스, OpenRouter = REST API,
/// Fixture = 테스트 전용(네트워크/서브프로세스 없이 미리 정해둔 응답을 순서대로 반환).
#[derive(Clone, Debug)]
pub enum Provider {
    ClaudeCli {
        bin: String,
    },
    OpenRouter {
        api_key: String,
    },
    #[cfg(test)]
    Fixture(Arc<Mutex<std::collections::VecDeque<String>>>),
}

/// 누적 토큰/비용 사용량. 여러 Llm 인스턴스(예: 본 모델 + 저비용 모델)가
/// 같은 Arc를 공유하면 실행 전체 기준 합산치를 얻는다.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// claude CLI가 제공하는 경우만 채워짐(OpenRouter 응답엔 없음).
    pub cost_usd: f64,
}

impl Usage {
    pub fn summary(&self) -> String {
        let cost = if self.cost_usd > 0.0 {
            format!(", cost ${:.4}", self.cost_usd)
        } else {
            String::new()
        };
        format!(
            "LLM 호출 {}회 — input {} / output {} / cache_read {} / cache_write {}{}",
            self.calls,
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_creation_tokens,
            cost
        )
    }
}

#[derive(Debug, Default)]
struct CallUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    cost_usd: f64,
}

struct CallResult {
    text: String,
    usage: CallUsage,
}

#[derive(Clone, Debug)]
pub struct Llm {
    pub provider: Provider,
    pub model: Option<String>,
    pub retries: u32,
    pub verbose: bool,
    usage: Arc<Mutex<Usage>>,
}

impl Llm {
    /// 여러 Llm 인스턴스에 공유시켜 실행 전체의 합산 사용량을 추적한다.
    pub fn new_usage_tracker() -> Arc<Mutex<Usage>> {
        Arc::new(Mutex::new(Usage::default()))
    }

    pub fn claude_cli(
        bin: String,
        model: Option<String>,
        retries: u32,
        verbose: bool,
        usage: Arc<Mutex<Usage>>,
    ) -> Self {
        Llm {
            provider: Provider::ClaudeCli { bin },
            model,
            retries,
            verbose,
            usage,
        }
    }

    /// `OPENROUTER_API_KEY` 환경변수 필요. model 미지정 시 120B 오픈모델 기본값 사용.
    pub fn openrouter(
        model: Option<String>,
        retries: u32,
        verbose: bool,
        usage: Arc<Mutex<Usage>>,
    ) -> Result<Self> {
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .context("OPENROUTER_API_KEY 환경변수 없음 (export OPENROUTER_API_KEY=...)")?;
        Ok(Llm {
            provider: Provider::OpenRouter { api_key },
            model: Some(model.unwrap_or_else(|| OPENROUTER_DEFAULT_MODEL.to_string())),
            retries,
            verbose,
            usage,
        })
    }

    /// 테스트 전용 — `responses`를 호출 순서대로 하나씩 반환한다(네트워크/서브프로세스 없음).
    /// concurrency=1일 때만 호출 순서가 소스 코드 순서와 일치해 결정적이므로, E2E 테스트는
    /// 반드시 concurrency=1로 돌려야 한다.
    #[cfg(test)]
    pub fn fixture(responses: Vec<String>, retries: u32, usage: Arc<Mutex<Usage>>) -> Self {
        Llm {
            provider: Provider::Fixture(Arc::new(Mutex::new(responses.into_iter().collect()))),
            model: None,
            retries,
            verbose: false,
            usage,
        }
    }

    /// 현재까지 누적된 사용량 스냅샷(공유 tracker 기준). 다른 스레드가 lock을 쥔 채
    /// panic해 poison되어도(누적치가 잘못될 수는 있어도) 여기서 또 panic하지는 않는다.
    pub fn usage(&self) -> Usage {
        self.usage.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn record_usage(&self, u: &CallUsage) {
        let mut g = self.usage.lock().unwrap_or_else(|e| e.into_inner());
        g.calls += 1;
        g.input_tokens += u.input_tokens;
        g.output_tokens += u.output_tokens;
        g.cache_read_tokens += u.cache_read_tokens;
        g.cache_creation_tokens += u.cache_creation_tokens;
        g.cost_usd += u.cost_usd;
    }

    fn call_once(&self, ctx: Option<&str>, task: &str, system: Option<&str>) -> Result<CallResult> {
        match &self.provider {
            Provider::ClaudeCli { bin } => {
                call_claude(bin, self.model.as_deref(), ctx, task, system)
            }
            Provider::OpenRouter { api_key } => {
                call_openrouter(api_key, self.model.as_deref(), ctx, task, system)
            }
            #[cfg(test)]
            Provider::Fixture(queue) => {
                let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                let text = q.pop_front().ok_or_else(|| {
                    anyhow!("fixture 응답 큐가 비었음 — 예상보다 LLM 호출이 많음")
                })?;
                Ok(CallResult {
                    text,
                    usage: CallUsage::default(),
                })
            }
        }
    }

    /// `ctx`(여러 호출에서 반복되는 안정적 프리픽스: 프로젝트 맥락·컨벤션·요구사항·diff)를
    /// `task`(호출별로 달라지는 지시문)와 분리해서 받는다. OpenRouter 백엔드에서는 ctx에
    /// cache_control(ephemeral)을 붙여 동일 ctx로 반복 호출될 때 캐시 히트를 노린다.
    /// claude-cli 백엔드는 매 호출이 새 서브프로세스라 캐싱 효과가 없어 단순 이어붙인다.
    pub fn text_ctx(&self, ctx: Option<&str>, task: &str, system: Option<&str>) -> Result<String> {
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..=self.retries {
            match self.call_once(ctx, task, system) {
                Ok(r) => {
                    self.record_usage(&r.usage);
                    if !r.text.trim().is_empty() {
                        return Ok(r.text);
                    }
                    last = Some(anyhow!("빈 응답"));
                }
                Err(e) => last = Some(e),
            }
            if self.verbose {
                match last.as_ref() {
                    Some(error) => eprintln!("[retry {}/{}] {error}", attempt + 1, self.retries),
                    None => eprintln!(
                        "[retry {}/{}] unknown retry error",
                        attempt + 1,
                        self.retries
                    ),
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("알 수 없는 실패")))
    }

    /// [`Llm::text_ctx`]의 JSON 강제 버전.
    pub fn json_ctx(
        &self,
        ctx: Option<&str>,
        task: &str,
        system: Option<&str>,
    ) -> Result<serde_json::Value> {
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..=self.retries {
            let raw = match self.call_once(ctx, task, system) {
                Ok(r) => {
                    self.record_usage(&r.usage);
                    r.text
                }
                Err(e) => {
                    last = Some(e);
                    if self.verbose {
                        match last.as_ref() {
                            Some(error) => {
                                eprintln!("[json retry {}/{}] {error}", attempt + 1, self.retries)
                            }
                            None => {
                                eprintln!(
                                    "[json retry {}/{}] unknown json retry error",
                                    attempt + 1,
                                    self.retries
                                );
                            }
                        }
                    }
                    continue;
                }
            };
            match extract_json(&raw) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    last = Some(e);
                    if self.verbose {
                        match last.as_ref() {
                            Some(error) => {
                                eprintln!("[json retry {}/{}] {error}", attempt + 1, self.retries)
                            }
                            None => {
                                eprintln!(
                                    "[json retry {}/{}] unknown json retry error",
                                    attempt + 1,
                                    self.retries
                                );
                            }
                        }
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("JSON 응답 실패")))
    }
}

/// `child.wait_with_output()`은 self를 소비해서 폴링 루프와 못 섞는다 — stdout/stderr를
/// 먼저 별도 스레드로 읽어들이고(파이프가 꽉 차 자식이 블록되는 걸 방지), `try_wait()`으로
/// 폴링하다 타임아웃되면 kill한다.
fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<std::process::Output> {
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = stdout_pipe {
            let _ = std::io::Read::read_to_end(&mut p, &mut buf);
        }
        buf
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = stderr_pipe {
            let _ = std::io::Read::read_to_end(&mut p, &mut buf);
        }
        buf
    });

    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!(
                "claude CLI 호출이 {}초 넘게 응답 없어 강제 종료함",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    let stdout = stdout_handle
        .join()
        .map_err(|_| anyhow!("stdout 리더 스레드 panic"))?;
    let stderr = stderr_handle
        .join()
        .map_err(|_| anyhow!("stderr 리더 스레드 panic"))?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// 프롬프트는 stdin으로 전달(인자 길이 제한 회피). 서브프로세스 호출이라 캐싱은 적용되지
/// 않으므로 ctx+task를 그냥 이어붙인다(순서만: 안정적 맥락 먼저, 가변 지시문 나중).
fn call_claude(
    bin: &str,
    model: Option<&str>,
    ctx: Option<&str>,
    task: &str,
    system: Option<&str>,
) -> Result<CallResult> {
    let mut cmd = Command::new(bin);
    cmd.arg("-p").arg("--output-format").arg("json");
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }
    if let Some(s) = system {
        cmd.arg("--append-system-prompt").arg(s);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("`{bin}` 실행 실패 (설치 및 PATH 확인)"))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("stdin 열기 실패"))?;
        if let Some(c) = ctx {
            stdin.write_all(c.as_bytes())?;
        }
        stdin.write_all(task.as_bytes())?;
    }
    drop(child.stdin.take());

    let out = wait_with_timeout(child, CLAUDE_CLI_TIMEOUT)
        .with_context(|| format!("`{bin}` 실행 대기 실패"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "claude 종료코드 {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .with_context(|| format!("claude JSON 출력 파싱 실패: {}", truncate(&stdout, 400)))?;
    if v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false) {
        return Err(anyhow!("claude가 에러 응답: {}", truncate(&stdout, 400)));
    }
    let result = v
        .get("result")
        .and_then(|r| r.as_str())
        .ok_or_else(|| anyhow!("응답에 result 필드 없음: {}", truncate(&stdout, 400)))?;

    // usage/cost 필드는 claude CLI 버전에 따라 존재 여부·이름이 다를 수 있어 관대하게 파싱한다
    // (없으면 0으로 두고 실패시키지 않음 — result 필드만 계약으로 취급).
    let usage_obj = v.get("usage");
    let get_u64 = |key: &str| {
        usage_obj
            .and_then(|u| u.get(key))
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
    };
    let cost_usd = v
        .get("total_cost_usd")
        .or_else(|| v.get("cost_usd"))
        .and_then(|c| c.as_f64())
        .unwrap_or(0.0);
    Ok(CallResult {
        text: result.to_string(),
        usage: CallUsage {
            input_tokens: get_u64("input_tokens"),
            output_tokens: get_u64("output_tokens"),
            cache_read_tokens: get_u64("cache_read_input_tokens"),
            cache_creation_tokens: get_u64("cache_creation_input_tokens"),
            cost_usd,
        },
    })
}

/// cache_control(ephemeral)은 Anthropic Messages API 확장이라 Claude 계열 모델에서만 의미가
/// 있다 — 그 외 모델(OPENROUTER_DEFAULT_MODEL 포함)에서는 캐싱 이득이 없는데도 굳이 붙일
/// 이유가 없으므로, 모델명에 "claude"가 없으면 기존과 동일한 단일 문자열 content로 보낸다.
fn supports_prompt_caching(model: &str) -> bool {
    model.to_ascii_lowercase().contains("claude")
}

/// OpenRouter 채팅 완성 API 1회 호출. ctx가 주어지고 대상 모델이 Claude 계열이면 별도
/// content 블록으로 분리해 cache_control(ephemeral)을 붙인다 — 동일 ctx로 반복 호출될 때
/// (예: 렌즈별 리뷰) 캐시 히트를 노리는 최적화. 그 외에는 기존처럼 단일 문자열 content를 보낸다.
fn call_openrouter(
    api_key: &str,
    model: Option<&str>,
    ctx: Option<&str>,
    task: &str,
    system: Option<&str>,
) -> Result<CallResult> {
    let mut messages: Vec<serde_json::Value> = Vec::new();
    if let Some(s) = system {
        messages.push(serde_json::json!({"role": "system", "content": s}));
    }
    let resolved_model = model.unwrap_or(OPENROUTER_DEFAULT_MODEL);
    let cacheable_ctx = ctx.filter(|c| !c.is_empty() && supports_prompt_caching(resolved_model));
    let user_content = match cacheable_ctx {
        Some(c) => serde_json::json!([
            {"type": "text", "text": c, "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": task},
        ]),
        None => {
            let combined = match ctx {
                Some(c) if !c.is_empty() => format!("{c}{task}"),
                _ => task.to_string(),
            };
            serde_json::json!(combined)
        }
    };
    messages.push(serde_json::json!({"role": "user", "content": user_content}));

    let body = serde_json::json!({
        "model": resolved_model,
        "messages": messages,
    });

    // ureq 3.x: AgentBuilder가 Config/ConfigBuilder로 바뀌었다. http_status_as_error(false)로
    // 4xx/5xx도 Err가 아니라 Ok(response)로 받아서, 기존과 동일하게 상태코드+본문을 함께
    // 우리 에러 메시지에 담을 수 있게 한다(기본값이면 본문 없이 Err만 와서 body를 못 읽음).
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT_GLOBAL))
        .http_status_as_error(false)
        .build();
    let agent: ureq::Agent = config.into();
    let result = agent
        .post(OPENROUTER_URL)
        .header("Authorization", &format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .send_json(body);

    let mut resp = result.map_err(|e| anyhow!("openrouter 호출 실패: {e}"))?;
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let body_text = resp.body_mut().read_to_string().unwrap_or_default();
        return Err(anyhow!(
            "openrouter 응답 코드 {code}: {}",
            truncate(&body_text, 400)
        ));
    }

    let v: serde_json::Value = resp
        .body_mut()
        .read_json()
        .context("openrouter 응답 JSON 파싱 실패")?;
    let content = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| {
            anyhow!(
                "openrouter 응답에 content 없음: {}",
                truncate(&v.to_string(), 400)
            )
        })?;

    // OpenAI 호환 usage 스키마(prompt_tokens/completion_tokens). cost는 응답에 없어 0으로 둔다.
    let usage_obj = v.get("usage");
    let get_u64 = |key: &str| {
        usage_obj
            .and_then(|u| u.get(key))
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
    };
    Ok(CallResult {
        text: content.to_string(),
        usage: CallUsage {
            input_tokens: get_u64("prompt_tokens"),
            output_tokens: get_u64("completion_tokens"),
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cost_usd: 0.0,
        },
    })
}

/// 코드펜스/잡설이 섞인 응답에서 JSON 오브젝트(또는 배열)만 추출.
pub fn extract_json(raw: &str) -> Result<serde_json::Value> {
    let t = raw.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
        return Ok(v);
    }
    if let Some(start) = t.find("```") {
        let after = &t[start + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        if let Some(end) = after.find("```") {
            let body = after[..end].trim();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
                return Ok(v);
            }
        }
    }
    if let (Some(s), Some(e)) = (t.find('{'), t.rfind('}')) {
        if s < e {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t[s..=e]) {
                return Ok(v);
            }
        }
    }
    if let (Some(s), Some(e)) = (t.find('['), t.rfind(']')) {
        if s < e {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t[s..=e]) {
                return Ok(v);
            }
        }
    }
    Err(anyhow!("JSON 추출 실패: {}", truncate(t, 400)))
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_with_timeout_returns_output_when_process_finishes_in_time() {
        let child = Command::new("sh")
            .args(["-c", "echo hi"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = wait_with_timeout(child, Duration::from_secs(5)).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }

    #[test]
    fn wait_with_timeout_kills_and_errors_when_process_hangs() {
        let child = Command::new("sh")
            .args(["-c", "sleep 5"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let err = wait_with_timeout(child, Duration::from_millis(300))
            .expect_err("hanging process must time out");
        assert!(err.to_string().contains("초 넘게"));
    }
}
