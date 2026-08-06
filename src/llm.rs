use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
pub const OPENROUTER_DEFAULT_MODEL: &str = "openai/gpt-oss-120b";
/// ureq's default timeout is unlimited, so without setting one explicitly, the process can block
/// forever under network congestion. Especially fatal in automated environments like CI — tying
/// everything from DNS to receiving the response body to a single cap (rather than per-phase
/// caps) gives a more direct guarantee that "this will never hang past this duration", so we set
/// just one timeout_global.
///
/// 90 seconds was reproduced live while running the evals/ golden set against real OpenRouter —
/// a single discourse round call exceeded 90 seconds and failed the whole review with "json:
/// timeout: global" (even after retries). Relaxed to match CLAUDE_CLI_TIMEOUT (600s), which
/// serves the same purpose of "waiting for one LLM response" — there's no reason this needs to be
/// particularly tighter than the subprocess backend.
const HTTP_TIMEOUT_GLOBAL: Duration = Duration::from_secs(600);
/// The claude -p subprocess carries the same unlimited-wait risk as network calls (if the
/// external CLI hangs, the whole review stalls forever) — set generously to account for the
/// README's stated "seconds to minutes" duration.
const CLAUDE_CLI_TIMEOUT: Duration = Duration::from_secs(600);

/// LLM call backend. ClaudeCli = `claude -p` subprocess, OpenRouter = REST API,
/// Fixture = test-only (returns pre-set responses in order, no network/subprocess).
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

/// Cumulative token/cost usage. If multiple Llm instances (e.g. main model + cheap model) share
/// the same Arc, you get totals aggregated across the whole run.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// Only populated when the claude CLI provides it (absent from OpenRouter responses).
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
            "LLM calls: {} — input {} / output {} / cache_read {} / cache_write {}{}",
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
    /// Share this across multiple Llm instances to track aggregated usage for the whole run.
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

    /// Requires the `OPENROUTER_API_KEY` env var. Defaults to the 120B open model when model is unspecified.
    pub fn openrouter(
        model: Option<String>,
        retries: u32,
        verbose: bool,
        usage: Arc<Mutex<Usage>>,
    ) -> Result<Self> {
        let api_key = std::env::var("OPENROUTER_API_KEY").context(
            "OPENROUTER_API_KEY environment variable not set (export OPENROUTER_API_KEY=...)",
        )?;
        Ok(Llm {
            provider: Provider::OpenRouter { api_key },
            model: Some(model.unwrap_or_else(|| OPENROUTER_DEFAULT_MODEL.to_string())),
            retries,
            verbose,
            usage,
        })
    }

    /// Test-only — returns `responses` one by one in call order (no network/subprocess).
    /// Only deterministic when concurrency=1, since call order then matches source code order,
    /// so E2E tests must run with concurrency=1.
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

    /// Snapshot of usage accumulated so far (from the shared tracker). Even if another thread
    /// panics while holding the lock and poisons it (the accumulated total could be wrong), this doesn't panic again here.
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
                    anyhow!("fixture response queue is empty — more LLM calls than expected")
                })?;
                Ok(CallResult {
                    text,
                    usage: CallUsage::default(),
                })
            }
        }
    }

    /// Takes `ctx` (a stable prefix repeated across multiple calls: project context,
    /// conventions, requirements, diff) separately from `task` (the instruction that varies per
    /// call). On the OpenRouter backend, cache_control(ephemeral) is attached to ctx to aim for
    /// cache hits when the same ctx is called repeatedly. The claude-cli backend gets no caching
    /// benefit since each call is a fresh subprocess, so it just concatenates them.
    pub fn text_ctx(&self, ctx: Option<&str>, task: &str, system: Option<&str>) -> Result<String> {
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..=self.retries {
            match self.call_once(ctx, task, system) {
                Ok(r) => {
                    self.record_usage(&r.usage);
                    if !r.text.trim().is_empty() {
                        return Ok(r.text);
                    }
                    last = Some(anyhow!("empty response"));
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
        Err(last.unwrap_or_else(|| anyhow!("unknown failure")))
    }

    /// JSON-enforcing variant of [`Llm::text_ctx`].
    pub fn json_ctx(
        &self,
        ctx: Option<&str>,
        task: &str,
        system: Option<&str>,
    ) -> Result<serde_json::Value> {
        self.json_ctx_typed(ctx, task, system)
    }

    /// Like [`Llm::json_ctx`], but also validates the response against `T`'s schema before
    /// counting an attempt as successful. Before this existed, callers deserialized the
    /// `Value` json_ctx returned *outside* the retry loop — syntactically valid JSON that
    /// didn't match the expected schema (e.g. a field with the wrong type) skipped every retry
    /// and failed the whole call immediately. Folding the schema check into the same loop that
    /// already retries on JSON-parse failure means a schema-mismatched response gets exactly
    /// the same retry treatment as a malformed one, instead of none.
    pub fn json_ctx_typed<T: serde::de::DeserializeOwned>(
        &self,
        ctx: Option<&str>,
        task: &str,
        system: Option<&str>,
    ) -> Result<T> {
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
            let parsed = extract_json(&raw).and_then(|v| {
                serde_json::from_value::<T>(v).context("response does not match expected schema")
            });
            match parsed {
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
        Err(last.unwrap_or_else(|| anyhow!("JSON response failed")))
    }
}

/// `child.wait_with_output()` consumes self, so it can't be mixed with a polling loop — read
/// stdout/stderr on separate threads first (to prevent the child from blocking on a full pipe),
/// then poll with `try_wait()` and kill on timeout.
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
                "claude CLI call unresponsive for over {}s, force-killed",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    let stdout = stdout_handle
        .join()
        .map_err(|_| anyhow!("stdout reader thread panicked"))?;
    let stderr = stderr_handle
        .join()
        .map_err(|_| anyhow!("stderr reader thread panicked"))?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// wait_with_timeout drains stdout/stderr on threads before polling starts, preventing the child
/// from blocking on a full pipe — doing the stdin write (which can be up to several hundred KB,
/// including the whole diff) synchronously before that poll even begins would not get the same
/// protection. If the child doesn't read stdin right away due to startup delay etc., write_all
/// could block indefinitely regardless of CLAUDE_CLI_TIMEOUT, so stdin writing is also done on a
/// separate thread, symmetric with stdout/stderr.
fn write_stdin_and_wait(
    mut child: std::process::Child,
    stdin_data: Vec<u8>,
    timeout: Duration,
) -> Result<std::process::Output> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open stdin"))?;
    let stdin_handle =
        std::thread::spawn(move || -> std::io::Result<()> { stdin.write_all(&stdin_data) });

    let out = wait_with_timeout(child, timeout)?;
    // Only treat a write error as a real problem after a normal exit (i.e. not a timeout kill).
    // A broken pipe after the process was killed by timeout is expected and doesn't need separate
    // error reporting (the timeout itself was already returned as an error above) — even without
    // joining, the thread will terminate naturally soon.
    match stdin_handle.join() {
        Ok(Ok(())) => Ok(out),
        Ok(Err(e)) => Err(anyhow!("failed to write stdin: {e}")),
        Err(_) => Err(anyhow!("stdin writer thread panicked")),
    }
}

/// The prompt is passed via stdin (avoids argument length limits). Since this is a subprocess
/// call, no caching applies, so ctx+task are simply concatenated (order only: stable context first, variable instructions after).
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

    let child = cmd
        .spawn()
        .with_context(|| format!("failed to run `{bin}` (check installation and PATH)"))?;

    let mut stdin_data = ctx.map(|c| c.as_bytes().to_vec()).unwrap_or_default();
    stdin_data.extend_from_slice(task.as_bytes());
    let out = write_stdin_and_wait(child, stdin_data, CLAUDE_CLI_TIMEOUT)
        .with_context(|| format!("failed waiting for `{bin}` to finish"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "claude exited with code {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).with_context(|| {
        format!(
            "failed to parse claude JSON output: {}",
            truncate(&stdout, 400)
        )
    })?;
    if v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false) {
        return Err(anyhow!(
            "claude returned an error response: {}",
            truncate(&stdout, 400)
        ));
    }
    let result = v
        .get("result")
        .and_then(|r| r.as_str())
        .ok_or_else(|| anyhow!("response missing result field: {}", truncate(&stdout, 400)))?;

    // The usage/cost fields may or may not exist, and their names may differ, depending on the
    // claude CLI version, so parse leniently (default to 0 instead of failing — only the result field is treated as a contract).
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

/// cache_control(ephemeral) is an Anthropic Messages API extension, so it's only meaningful for
/// Claude-family models — for other models (including OPENROUTER_DEFAULT_MODEL) there's no
/// caching benefit, so there's no reason to bother attaching it; if the model name doesn't
/// contain "claude", send the same single-string content as before.
fn supports_prompt_caching(model: &str) -> bool {
    model.to_ascii_lowercase().contains("claude")
}

/// A single call to the OpenRouter chat completions API. If ctx is given and the target model is
/// Claude-family, it's split into a separate content block with cache_control(ephemeral) attached
/// — an optimization aiming for cache hits when the same ctx is called repeatedly (e.g. per-lens
/// reviews). Otherwise, sends a single-string content as before.
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

    // ureq 3.x: AgentBuilder was replaced by Config/ConfigBuilder. http_status_as_error(false)
    // makes 4xx/5xx come back as Ok(response) instead of Err, so we can still include both the
    // status code and body in our own error message as before (with the default, you'd get only
    // an Err with no body, unable to read it).
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

    let mut resp = result.map_err(|e| anyhow!("openrouter call failed: {e}"))?;
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let body_text = resp.body_mut().read_to_string().unwrap_or_default();
        return Err(anyhow!(
            "openrouter response code {code}: {}",
            truncate(&body_text, 400)
        ));
    }

    let v: serde_json::Value = resp
        .body_mut()
        .read_json()
        .context("failed to parse openrouter response JSON")?;
    let content = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| {
            anyhow!(
                "openrouter response missing content: {}",
                truncate(&v.to_string(), 400)
            )
        })?;

    // OpenAI-compatible usage schema (prompt_tokens/completion_tokens). cost is absent from the response, so it's left at 0.
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

/// Extracts just the JSON object (or array) from a response mixed with code fences/chatter.
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
    Err(anyhow!("failed to extract JSON: {}", truncate(t, 400)))
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
        assert!(err.to_string().contains("unresponsive for over"));
    }

    #[test]
    fn write_stdin_and_wait_returns_output_for_a_process_that_echoes_stdin() {
        let child = Command::new("sh")
            .args(["-c", "cat"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = write_stdin_and_wait(child, b"hello".to_vec(), Duration::from_secs(5)).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hello");
    }

    #[test]
    fn write_stdin_and_wait_times_out_promptly_instead_of_blocking_on_a_large_write() {
        // Regression guard: if stdin writing ran (synchronously) before wait_with_timeout's poll
        // loop, writing data larger than the pipe buffer to this child, which never reads stdin
        // at all, would block indefinitely regardless of CLAUDE_CLI_TIMEOUT. If fixed,
        // wait_with_timeout's timeout (1 second here) should correctly kick in first and the
        // whole call should end around there — if it regresses to a synchronous write, it blocks for the child's sleep 10 (or longer).
        let child = Command::new("sh")
            .args(["-c", "sleep 10"]) // never reads stdin
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let large_payload = vec![b'x'; 4 * 1024 * 1024]; // 4MB, larger than any OS pipe buffer
        let start = std::time::Instant::now();
        let err = write_stdin_and_wait(child, large_payload, Duration::from_secs(1))
            .expect_err("a process that never reads stdin must be terminated by timeout");
        assert!(err.to_string().contains("unresponsive for over"));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "should finish around wait_with_timeout's 1s timeout, but took {:?} \
             (may have regressed to a synchronous write)",
            start.elapsed()
        );
    }
}
