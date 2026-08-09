use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

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
/// Custom = #156: any other OpenAI-compatible endpoint (self-hosted vLLM/Ollama/internal
/// gateway) — same request/response shape as OpenRouter, just a different base URL and an
/// optional (rather than required) API key, Fixture = test-only (returns pre-set responses in
/// order, no network/subprocess).
#[derive(Clone, Debug)]
pub enum Provider {
    ClaudeCli {
        bin: String,
    },
    OpenRouter {
        api_key: String,
        agent: Arc<ureq::Agent>,
    },
    Custom {
        base_url: String,
        api_key: Option<String>,
        agent: Arc<ureq::Agent>,
    },
    #[cfg(test)]
    Fixture(Arc<Mutex<std::collections::VecDeque<String>>>),
}

/// #171: shared across every call an `Llm` instance makes (previously, `call_openai_compatible`
/// built a fresh `ureq::Agent` on every single call), so a run's 5-13+ logical LLM calls reuse
/// the same underlying connection pool instead of paying TLS/TCP setup each time. No
/// `timeout_global` set here — that stays per-request (via `RequestBuilder::config()` in
/// `call_openai_compatible`), since different calls in the same run can have different effective
/// timeouts once a `--deadline-minutes` budget is shrinking; ureq 3's request-level config
/// override makes that possible on a shared agent instead of needing one agent per timeout value.
fn new_http_agent() -> Arc<ureq::Agent> {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build();
    Arc::new(config.into())
}

/// #166: bounds the total number of simultaneous LLM calls across every call site — lens
/// `par_map` workers, and (once other call sites overlap too, see #168/#170) lens selection,
/// requirements, human_voice — instead of each call site's own thread count, which undercounts
/// real total in-flight requests once more than one site can be active at the same time. Share
/// one `Arc<CallGate>` across every `Llm` instance in a run (main model and cheap model both) via
/// [`Llm::with_gate`] to get a real global cap, not just a per-stage worker-count limit.
#[derive(Debug)]
pub struct CallGate {
    max: usize,
    state: Mutex<usize>,
    cv: Condvar,
}

impl CallGate {
    pub fn new(max: usize) -> Arc<Self> {
        Arc::new(CallGate {
            max: max.max(1),
            state: Mutex::new(0),
            cv: Condvar::new(),
        })
    }

    fn acquire(self: &Arc<Self>) -> GatePermit {
        let mut n = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while *n >= self.max {
            n = self.cv.wait(n).unwrap_or_else(|e| e.into_inner());
        }
        *n += 1;
        GatePermit {
            gate: Arc::clone(self),
        }
    }
}

struct GatePermit {
    gate: Arc<CallGate>,
}

impl Drop for GatePermit {
    fn drop(&mut self) {
        let mut n = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        *n = n.saturating_sub(1);
        self.gate.cv.notify_one();
    }
}

/// Cumulative token/cost usage. If multiple Llm instances (e.g. main model + cheap model) share
/// the same Arc, you get totals aggregated across the whole run.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Usage {
    /// Successful calls only (incremented by `record_usage`, after a response comes back) --
    /// this is the number token/cost totals below are actually derived from.
    pub calls: u64,
    /// Every real attempt at reaching a provider -- success, HTTP/subprocess error, timeout, or
    /// retry -- incremented atomically (under the same lock as the `max_calls` check) right
    /// before the attempt is made. `max_calls` is enforced against this field, not `calls`:
    /// `calls` alone let a failing/retried call make real provider requests indefinitely without
    /// ever counting against the budget it was supposed to be checked against.
    pub attempted_calls: u64,
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

#[derive(Debug)]
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
    /// #119: an overall deadline (see `with_deadline`) — None means each call always gets its
    /// full per-call timeout, unchanged from before this field existed.
    deadline: Option<Instant>,
    /// #166: None means uncapped (existing behavior, unchanged) — set via `with_gate`.
    gate: Option<Arc<CallGate>>,
    /// #175: sent as `max_tokens` on OpenAI-compatible requests (OpenRouter/Custom) — None
    /// means no cap is sent (existing behavior, unchanged). Ignored by the claude-cli backend.
    max_output_tokens: Option<u32>,
    /// Sent as `temperature` on OpenAI-compatible requests (OpenRouter/Custom) — None means no
    /// value is sent at all (existing behavior, unchanged: whatever the provider/model defaults
    /// to, typically not 0). Ignored by the claude-cli backend, which has no such knob exposed
    /// through the `claude -p` CLI. Real, measured non-determinism (the same diff can produce a
    /// different verdict on a repeat run) is why this exists — see README's "Path to
    /// production" — but no default is silently changed here: a team has to opt in.
    temperature: Option<f64>,
    /// #175: hard ceiling on total provider calls (shared `usage.calls` — main and cheap model
    /// combined when they share a usage tracker) — None means uncapped (existing behavior,
    /// unchanged).
    max_calls: Option<u64>,
    /// #172: per-logical-call latency/attempt-count telemetry — None means not collected
    /// (existing behavior for any caller that doesn't opt in via `with_calls_log`, e.g. tests
    /// constructing `Llm::fixture` directly). Shared across main/cheap the same way `usage` is,
    /// so a manifest built from it sees every call in the run, not just one model's.
    calls_log: Option<Arc<Mutex<Vec<CallRecord>>>>,
}

/// #172: one entry per logical call (a whole `text_ctx`/`json_ctx_typed` invocation, including
/// all its retries — not one entry per raw HTTP/subprocess attempt). `manifest.rs`'s own header
/// comment previously scoped this out as needing "instrumenting Llm itself" — this is that
/// instrumentation. Deliberately doesn't carry a `stage` label: every call site (lens review,
/// discourse, requirements, ...) would need to thread one through, a larger change than this
/// pass; a `CallRecord`'s position in the shared log combined with `manifest.rs`'s own per-stage
/// wall-clock timings is enough to roughly correlate the two without that.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CallRecord {
    pub attempts: u32,
    pub latency_ms: u128,
    pub success: bool,
    /// #172 follow-up: `backend_factory::build_llm` shares one `calls_log` across the main and
    /// cheap `Llm` instances (see its own doc comment) so a manifest sees every call in a run —
    /// but that meant there was no way to tell which of the two models made a given call. Set
    /// from `self.model` at record time, so it reflects whichever `Llm` (main or cheap) the call
    /// actually went through.
    pub model: Option<String>,
}

/// An HTTP-ish failure that carries its status code as data, not just baked into a message
/// string — lets `is_retryable` classify it without parsing rendered text (see #119).
#[derive(Debug)]
struct HttpError {
    code: u16,
    body: String,
    /// #171: parsed from the response's Retry-After header when present (seconds form only —
    /// the HTTP-date form isn't handled, same effect as if the header were absent). Previously
    /// the response headers weren't captured at all, so even a well-behaved 429 that specified
    /// exactly how long to wait got the same generic exponential backoff as any other retry.
    retry_after: Option<Duration>,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "openrouter response code {}: {}", self.code, self.body)
    }
}

impl std::error::Error for HttpError {}

/// #175: a distinct marker (rather than a plain `anyhow!(...)` string) so `is_retryable`
/// classifies a call-budget refusal as permanent — retrying it burns backoff sleeps for no
/// reason since `usage.calls` isn't going to shrink between attempts.
#[derive(Debug)]
struct CallBudgetExceeded(u64);

impl std::fmt::Display for CallBudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "provider call budget exceeded ({} calls) — raise --max-provider-calls if this run genuinely needs more",
            self.0
        )
    }
}

impl std::error::Error for CallBudgetExceeded {}

/// #119: retries used to treat every failure identically — a permanent 401 (bad API key) got
/// the same "retry `--retries` more times" treatment as a transient 429/5xx, wasting the whole
/// retry budget on something no amount of retrying fixes. Only downgrades the clear-cut case
/// (a classified HTTP 4xx that isn't 429); anything else — network errors, 5xx, 429, the claude
/// CLI backend's exit-code errors, JSON parse/schema-mismatch failures — keeps retrying exactly
/// as before. Defaulting unclassified errors to "retryable" is the safe direction: at worst it
/// costs one extra wasted attempt, never skips a retry that might have succeeded.
fn is_retryable(e: &anyhow::Error) -> bool {
    if e.downcast_ref::<CallBudgetExceeded>().is_some() {
        return false;
    }
    match e.downcast_ref::<HttpError>() {
        Some(HttpError { code, .. }) => *code == 429 || *code >= 500,
        None => true,
    }
}

/// #119: retries used to fire back-to-back with no delay — fine against a one-off blip, but
/// against a 429 or a provider having a bad moment, hammering the same endpoint immediately
/// doesn't help. `attempt` is 0-indexed (the attempt that just failed); backoff doubles per
/// attempt, capped at 6 doublings (~32s base) so a high --retries count doesn't produce
/// absurd waits. No `rand` dependency — jitter comes from the current time's nanosecond
/// component, which is plenty for spreading retries across concurrent callers, not for
/// cryptographic use.
fn backoff_delay(attempt: u32) -> Duration {
    let base_ms = 500u64.saturating_mul(1u64 << attempt.min(6));
    let jitter_seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let jitter_ms = u64::from(jitter_seed % (base_ms / 2).max(1) as u32);
    Duration::from_millis(base_ms + jitter_ms)
}

/// #171: a 429 with an explicit Retry-After takes priority over the generic exponential
/// backoff — the provider is telling us exactly how long it wants, and guessing shorter than
/// that just produces another 429 tomorrow, no wiser than today.
fn retry_delay(attempt: u32, error: &anyhow::Error) -> Duration {
    match error.downcast_ref::<HttpError>() {
        Some(HttpError {
            retry_after: Some(d),
            ..
        }) => *d,
        _ => backoff_delay(attempt),
    }
}

/// #171: a schema/parse failure isn't a transient server problem — the model made a mistake in
/// its own output, and resending the identical prompt tends to just reproduce it. Builds a
/// follow-up task that includes the prior bad response and the specific validation error, so the
/// model has something concrete to correct instead of guessing again from scratch.
fn build_repair_task(original_task: &str, bad_response: &str, error: &anyhow::Error) -> String {
    format!(
        "{original_task}\n\n\
         ## Your previous response failed schema validation\n\
         Previous response:\n{prev}\n\n\
         Validation error: {error}\n\n\
         Fix the JSON to match the schema exactly and respond with corrected JSON only — no code fences, no commentary.",
        prev = truncate(bad_response, 2000),
    )
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
            deadline: None,
            gate: None,
            max_output_tokens: None,
            temperature: None,
            max_calls: None,
            calls_log: None,
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
            provider: Provider::OpenRouter {
                api_key,
                agent: new_http_agent(),
            },
            model: Some(model.unwrap_or_else(|| OPENROUTER_DEFAULT_MODEL.to_string())),
            retries,
            verbose,
            usage,
            deadline: None,
            gate: None,
            max_output_tokens: None,
            temperature: None,
            max_calls: None,
            calls_log: None,
        })
    }

    /// #156: any OpenAI-compatible endpoint that isn't OpenRouter — self-hosted vLLM/Ollama/an
    /// internal gateway. Unlike `openrouter()`, `model` is required here: there's no sensible
    /// universal default model for an arbitrary self-hosted endpoint the way
    /// `OPENROUTER_DEFAULT_MODEL` is for OpenRouter specifically. `api_key` is optional since
    /// many self-hosted endpoints (e.g. a local Ollama) don't require one.
    pub fn custom_endpoint(
        base_url: String,
        api_key: Option<String>,
        model: String,
        retries: u32,
        verbose: bool,
        usage: Arc<Mutex<Usage>>,
    ) -> Self {
        Llm {
            provider: Provider::Custom {
                base_url,
                api_key,
                agent: new_http_agent(),
            },
            model: Some(model),
            retries,
            verbose,
            usage,
            deadline: None,
            gate: None,
            max_output_tokens: None,
            temperature: None,
            max_calls: None,
            calls_log: None,
        }
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
            deadline: None,
            gate: None,
            max_output_tokens: None,
            temperature: None,
            max_calls: None,
            calls_log: None,
        }
    }

    /// Snapshot of usage accumulated so far (from the shared tracker). Even if another thread
    /// panics while holding the lock and poisons it (the accumulated total could be wrong), this doesn't panic again here.
    pub fn usage(&self) -> Usage {
        self.usage.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// #119: without this, --deadline-minutes only stopped new *stages* from starting — a call
    /// already in flight (or one started right as the deadline passed) could still run its full
    /// per-call timeout (600s) regardless. Attaching a deadline makes each individual call's
    /// own timeout shrink to whatever's actually left of the budget, so the deadline becomes a
    /// real wall-clock bound instead of only a between-stage checkpoint.
    pub fn with_deadline(mut self, deadline: Option<Instant>) -> Self {
        self.deadline = deadline;
        self
    }

    /// #166: share the same `Arc<CallGate>` across every `Llm` instance in a run (main model and
    /// cheap model both — `backend_factory::build_llm` does this) to cap real total in-flight
    /// calls, not just one call site's own thread count.
    pub fn with_gate(mut self, gate: Option<Arc<CallGate>>) -> Self {
        self.gate = gate;
        self
    }

    /// #175: applies to OpenAI-compatible backends only (OpenRouter/Custom) — see the field's
    /// doc comment.
    pub fn with_max_output_tokens(mut self, max_output_tokens: Option<u32>) -> Self {
        self.max_output_tokens = max_output_tokens;
        self
    }

    /// Applies to OpenAI-compatible backends only (OpenRouter/Custom) — see the field's doc
    /// comment.
    pub fn with_temperature(mut self, temperature: Option<f64>) -> Self {
        self.temperature = temperature;
        self
    }

    /// #175: checked in call_once before every provider call — see the field's doc comment.
    pub fn with_max_calls(mut self, max_calls: Option<u64>) -> Self {
        self.max_calls = max_calls;
        self
    }

    /// #172: share this across multiple Llm instances (main + cheap) to collect one combined
    /// per-call log for the whole run, the same pattern as `new_usage_tracker`.
    pub fn new_calls_log() -> Arc<Mutex<Vec<CallRecord>>> {
        Arc::new(Mutex::new(Vec::new()))
    }

    pub fn with_calls_log(mut self, calls_log: Option<Arc<Mutex<Vec<CallRecord>>>>) -> Self {
        self.calls_log = calls_log;
        self
    }

    /// Snapshot of every call recorded so far, in call order. Empty if no log was attached via
    /// `with_calls_log`.
    pub fn calls(&self) -> Vec<CallRecord> {
        self.calls_log
            .as_ref()
            .map(|log| log.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .unwrap_or_default()
    }

    fn record_call(&self, attempts: u32, started: Instant, success: bool) {
        if let Some(log) = &self.calls_log {
            log.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(CallRecord {
                    attempts,
                    latency_ms: started.elapsed().as_millis(),
                    success,
                    model: self.model.clone(),
                });
        }
    }

    /// Caps `base` at whatever's left until `self.deadline`, if set. Floors at 1s so a deadline
    /// that's already passed by the time a call starts still gets a real (if short) attempt
    /// instead of a zero/negative timeout reaching a syscall.
    fn effective_timeout(&self, base: Duration) -> Duration {
        match self.deadline {
            None => base,
            Some(d) => base
                .min(d.saturating_duration_since(Instant::now()))
                .max(Duration::from_secs(1)),
        }
    }

    /// #143: true once `self.deadline` has passed. Used by the retry loops to stop retrying
    /// entirely, instead of still burning a sleep + another attempt (with effective_timeout's
    /// 1s floor) after the budget is already gone.
    fn deadline_passed(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }

    /// Sleeps `base`, capped at whatever's left until `self.deadline` (if set). The plain
    /// backoff sleep used to run its full duration regardless of the deadline, undermining the
    /// "wall-clock bound" `--deadline-minutes` is documented to provide (`effective_timeout`
    /// only shrinks the *call* timeout, not the sleep between calls).
    fn deadline_aware_sleep(&self, base: Duration) {
        let capped = match self.deadline {
            None => base,
            Some(d) => base.min(d.saturating_duration_since(Instant::now())),
        };
        if !capped.is_zero() {
            std::thread::sleep(capped);
        }
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

    /// Checks `max_calls` against `attempted_calls` and increments it in the same critical
    /// section, so two threads racing this at once can't both observe "under budget" before
    /// either has recorded its own attempt. Called from `call_once`, before the gate permit and
    /// before any actual provider request -- every real attempt (success, error, or retry) goes
    /// through here exactly once, unlike the old check against `calls` (successful calls only),
    /// which a failing/retried call could burn indefinitely without ever counting against.
    fn reserve_call_slot(&self) -> Result<()> {
        let Some(max) = self.max_calls else {
            return Ok(());
        };
        let mut g = self.usage.lock().unwrap_or_else(|e| e.into_inner());
        if g.attempted_calls >= max {
            return Err(CallBudgetExceeded(max).into());
        }
        g.attempted_calls += 1;
        Ok(())
    }

    fn call_once(&self, ctx: Option<&str>, task: &str, system: Option<&str>) -> Result<CallResult> {
        // #175/hardening: checked (and reserved, atomically) before acquiring a gate permit or
        // making any actual call — a misconfigured invocation (e.g. --lenses listing every
        // optional lens) hits this and fails fast instead of burning the provider call anyway.
        self.reserve_call_slot()?;
        // #166: held for the whole call (network round trip or subprocess) — dropped at the end
        // of this function, freeing the slot for the next waiting caller.
        let _permit = self.gate.as_ref().map(|g| g.acquire());
        match &self.provider {
            Provider::ClaudeCli { bin } => call_claude(
                bin,
                self.model.as_deref(),
                ctx,
                task,
                system,
                self.effective_timeout(CLAUDE_CLI_TIMEOUT),
            ),
            Provider::OpenRouter { api_key, agent } => call_openai_compatible(
                agent,
                OPENROUTER_URL,
                Some(api_key),
                self.model.as_deref(),
                ctx,
                task,
                system,
                self.effective_timeout(HTTP_TIMEOUT_GLOBAL),
                self.max_output_tokens,
                self.temperature,
            ),
            Provider::Custom {
                base_url,
                api_key,
                agent,
            } => call_openai_compatible(
                agent,
                base_url,
                api_key.as_deref(),
                self.model.as_deref(),
                ctx,
                task,
                system,
                self.effective_timeout(HTTP_TIMEOUT_GLOBAL),
                self.max_output_tokens,
                self.temperature,
            ),
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
        let started = Instant::now();
        let mut last: Option<anyhow::Error> = None;
        let mut attempts_made = 0u32;
        for attempt in 0..=self.retries {
            attempts_made = attempt + 1;
            let mut retryable = true;
            match self.call_once(ctx, task, system) {
                Ok(r) => {
                    self.record_usage(&r.usage);
                    if !r.text.trim().is_empty() {
                        self.record_call(attempt + 1, started, true);
                        return Ok(r.text);
                    }
                    last = Some(anyhow!("empty response"));
                }
                Err(e) => {
                    retryable = is_retryable(&e);
                    last = Some(e);
                }
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
            // #119: a permanent failure (e.g. a 401) won't succeed no matter how many times
            // it's retried — stop burning the retry budget on it instead of looping to the end.
            if !retryable {
                break;
            }
            // #143: once the deadline's passed, another attempt (even at effective_timeout's
            // 1s floor) is budget --deadline-minutes was supposed to have cut off already.
            if self.deadline_passed() {
                break;
            }
            if attempt < self.retries {
                let delay = last
                    .as_ref()
                    .map_or_else(|| backoff_delay(attempt), |e| retry_delay(attempt, e));
                self.deadline_aware_sleep(delay);
            }
        }
        self.record_call(attempts_made, started, false);
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
        let started = Instant::now();
        let mut last: Option<anyhow::Error> = None;
        let mut attempts_made = 0u32;
        // #171: retries used to treat a transport failure and a schema-mismatched response
        // identically — resend the exact same ctx/task and sleep an exponential backoff either
        // way. A schema failure isn't a transient server problem, so backoff doesn't help it,
        // and resending an unmodified prompt tends to just reproduce the same mistake. At most
        // one attempt (not the whole --retries budget) becomes a targeted repair instead: same
        // ctx, but the task is augmented with the prior bad response and the specific validation
        // error. If the repair itself still fails, any remaining attempts fall back to a plain
        // resend with normal backoff, same as before — this doesn't add attempts beyond
        // self.retries, it only changes what a schema-failure retry looks like.
        let mut current_task: std::borrow::Cow<str> = std::borrow::Cow::Borrowed(task);
        let mut repair_used = false;
        for attempt in 0..=self.retries {
            attempts_made = attempt + 1;
            let raw = match self.call_once(ctx, &current_task, system) {
                Ok(r) => {
                    self.record_usage(&r.usage);
                    r.text
                }
                Err(e) => {
                    // #119: same permanent-vs-transient distinction as text_ctx — a classified
                    // 401/403 here won't succeed on retry no matter how many attempts remain.
                    let retryable = is_retryable(&e);
                    if self.verbose {
                        eprintln!("[json retry {}/{}] {e}", attempt + 1, self.retries);
                    }
                    if !retryable {
                        last = Some(e);
                        break;
                    }
                    if self.deadline_passed() {
                        last = Some(e);
                        break;
                    }
                    if attempt < self.retries {
                        self.deadline_aware_sleep(retry_delay(attempt, &e));
                    }
                    last = Some(e);
                    continue;
                }
            };
            let parsed = extract_json(&raw).and_then(|v| {
                serde_json::from_value::<T>(v).context("response does not match expected schema")
            });
            match parsed {
                Ok(v) => {
                    self.record_call(attempts_made, started, true);
                    return Ok(v);
                }
                Err(e) => {
                    if self.verbose {
                        eprintln!("[json retry {}/{}] {e}", attempt + 1, self.retries);
                    }
                    if self.deadline_passed() {
                        last = Some(e);
                        break;
                    }
                    if attempt < self.retries {
                        if !repair_used {
                            repair_used = true;
                            current_task =
                                std::borrow::Cow::Owned(build_repair_task(task, &raw, &e));
                            // No backoff sleep here: a schema mistake isn't a rate-limit or
                            // server-load problem, so there's nothing gained by waiting before
                            // the model gets a chance to correct it.
                        } else {
                            // Repair already attempted once and still failed — fall back to a
                            // plain resend (original task, normal backoff) for any remaining
                            // budget rather than compounding repair attempts.
                            current_task = std::borrow::Cow::Borrowed(task);
                            self.deadline_aware_sleep(backoff_delay(attempt));
                        }
                    }
                    last = Some(e);
                }
            }
        }
        self.record_call(attempts_made, started, false);
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
    timeout: Duration,
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
    let out = write_stdin_and_wait(child, stdin_data, timeout)
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

/// A single call to an OpenAI-compatible chat completions endpoint — OpenRouter, or (#156) any
/// other such endpoint (self-hosted vLLM/Ollama/an internal gateway) via `Provider::Custom`.
/// `api_key` is optional since not every self-hosted endpoint requires one; when absent, no
/// `Authorization` header is sent at all rather than sending an empty/bogus one. If ctx is given
/// and the target model is Claude-family, it's split into a separate content block with
/// cache_control(ephemeral) attached — an optimization aiming for cache hits when the same ctx
/// is called repeatedly (e.g. per-lens reviews). Otherwise, sends a single-string content as
/// before.
#[allow(clippy::too_many_arguments)]
fn call_openai_compatible(
    agent: &ureq::Agent,
    url: &str,
    api_key: Option<&str>,
    model: Option<&str>,
    ctx: Option<&str>,
    task: &str,
    system: Option<&str>,
    timeout: Duration,
    max_output_tokens: Option<u32>,
    temperature: Option<f64>,
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

    let mut body = serde_json::json!({
        "model": resolved_model,
        "messages": messages,
        // #175: opts into OpenRouter's extended usage accounting (cost, and potentially
        // provider-reported cache token counts) — without this, those fields are omitted from
        // the response entirely on OpenRouter. An endpoint that doesn't recognize this field
        // (e.g. some Custom/self-hosted backends) should just ignore the extra top-level key.
        "usage": {"include": true},
    });
    // #175: previously no output cap was sent at all — a verbose response had nothing bounding
    // its length beyond whatever the provider defaults to.
    if let Some(max) = max_output_tokens {
        body["max_tokens"] = serde_json::json!(max);
    }
    if let Some(t) = temperature {
        body["temperature"] = serde_json::json!(t);
    }

    // #171: timeout is set per-request (via RequestBuilder::config()), not on the agent itself
    // — `agent` is shared and reused across every call this Llm instance makes (built once in
    // new_http_agent), while `timeout` varies call to call as --deadline-minutes shrinks.
    // http_status_as_error(false) (set once, at agent construction) makes 4xx/5xx come back as
    // Ok(response) instead of Err, so we can still include both the status code and body in our
    // own error message as before (with the default, you'd get only an Err with no body, unable
    // to read it).
    let mut req = agent
        .post(url)
        .config()
        .timeout_global(Some(timeout))
        .build()
        .header("Content-Type", "application/json");
    if let Some(key) = api_key {
        req = req.header("Authorization", &format!("Bearer {key}"));
    }
    let result = req.send_json(body);

    let mut resp = result.map_err(|e| anyhow!("openrouter call failed: {e}"))?;
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        // #171: previously the response headers weren't captured at all — a 429 with an
        // explicit Retry-After got the same generic exponential backoff as any other retry.
        // Seconds form only; the HTTP-date form of the header falls back to None (same effect
        // as if the header were absent, not an error).
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body_text = resp.body_mut().read_to_string().unwrap_or_default();
        // #119: HttpError carries the status code through as a typed error (instead of only
        // baking it into a string) so the retry loop can tell a permanent 401/403 apart from a
        // retry-worthy 429/5xx, instead of treating every failure the same.
        return Err(HttpError {
            code,
            body: truncate(&body_text, 400),
            retry_after,
        }
        .into());
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

    // OpenAI-compatible usage schema (prompt_tokens/completion_tokens).
    let usage_obj = v.get("usage");
    let get_u64 = |key: &str| {
        usage_obj
            .and_then(|u| u.get(key))
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
    };
    // #175: best-effort — OpenRouter's cache/cost reporting isn't uniformly documented across
    // every provider it proxies, and this hasn't been validated against real traffic for every
    // shape it might take. Tries a couple of known field shapes (Anthropic-style pass-through
    // for cache tokens, OpenAI-style nested `prompt_tokens_details` for the read side) and
    // defaults to 0 either way if none match — same fallback as before, just no longer
    // hardcoded to 0 unconditionally.
    let cache_read_tokens = get_u64("cache_read_input_tokens").max(
        usage_obj
            .and_then(|u| u.get("prompt_tokens_details"))
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
    );
    let cache_creation_tokens = get_u64("cache_creation_input_tokens");
    let cost_usd = usage_obj
        .and_then(|u| u.get("cost"))
        .and_then(|c| c.as_f64())
        .unwrap_or(0.0);
    Ok(CallResult {
        text: content.to_string(),
        usage: CallUsage {
            input_tokens: get_u64("prompt_tokens"),
            output_tokens: get_u64("completion_tokens"),
            cache_read_tokens,
            cache_creation_tokens,
            cost_usd,
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

    fn test_llm() -> Llm {
        Llm::fixture(vec![], 0, Llm::new_usage_tracker())
    }

    // --- #166: CallGate ---

    #[test]
    fn call_gate_never_lets_more_than_max_permits_be_held_at_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let gate = CallGate::new(2);
        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|s| {
            for _ in 0..6 {
                let gate = Arc::clone(&gate);
                let current = Arc::clone(&current);
                let peak = Arc::clone(&peak);
                s.spawn(move || {
                    let _permit = gate.acquire();
                    let n = current.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    current.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "never more than 2 permits held simultaneously"
        );
        assert_eq!(
            peak.load(Ordering::SeqCst),
            2,
            "should actually allow up to its max, not be overly conservative"
        );
    }

    #[test]
    fn with_gate_none_leaves_calls_uncapped_same_as_before_the_field_existed() {
        // A gate is opt-in — an Llm with no gate configured must behave exactly like before
        // #166, with call_once never blocking on a permit.
        let llm = test_llm();
        assert!(llm.call_once(None, "task", None).is_err()); // fixture queue is empty — just proves this returns promptly, not hangs on a gate wait.
    }

    // --- #175: max_calls ---

    #[test]
    fn text_ctx_refuses_once_max_calls_is_reached_without_touching_the_provider() {
        // Goes through text_ctx (not call_once directly) since record_usage — which the
        // max_calls check reads from — is called by text_ctx/json_ctx_typed after a successful
        // call_once, not by call_once itself.
        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(vec!["a".to_string(), "b".to_string()], 0, usage.clone())
            .with_max_calls(Some(1));
        assert_eq!(llm.text_ctx(None, "task", None).unwrap(), "a");
        let err = llm
            .text_ctx(None, "task", None)
            .expect_err("must refuse the second call once max_calls(1) is reached");
        assert!(err.to_string().contains("provider call budget exceeded"));
        // The second fixture entry was never touched — proves the refusal happened before ever
        // reaching the provider, not that the provider itself happened to fail.
        assert_eq!(usage.lock().unwrap().calls, 1);
    }

    #[test]
    fn text_ctx_is_uncapped_when_max_calls_is_none() {
        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(vec!["a".to_string(), "b".to_string()], 0, usage);
        assert_eq!(llm.text_ctx(None, "task", None).unwrap(), "a");
        assert_eq!(llm.text_ctx(None, "task", None).unwrap(), "b");
    }

    #[test]
    fn call_once_counts_a_failed_attempt_against_max_calls_not_just_successes() {
        // Real gap this closes: max_calls used to be checked against `usage.calls`, which only
        // record_usage (success path) incremented -- a call that fails inside call_once itself
        // (here: an empty fixture queue) burned a real attempt without ever counting toward the
        // budget it was supposed to be checked against.
        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(vec![], 0, usage.clone()).with_max_calls(Some(2));
        let e1 = llm.call_once(None, "task", None).unwrap_err();
        assert!(
            e1.to_string().contains("fixture response queue is empty"),
            "first attempt should fail for the underlying reason, not the budget: {e1}"
        );
        let e2 = llm.call_once(None, "task", None).unwrap_err();
        assert!(
            e2.to_string().contains("fixture response queue is empty"),
            "second attempt should also still fail for the underlying reason: {e2}"
        );
        let e3 = llm.call_once(None, "task", None).unwrap_err();
        assert!(
            e3.to_string().contains("provider call budget exceeded"),
            "third attempt must be refused by the budget, having counted the two prior failures: {e3}"
        );
        assert_eq!(usage.lock().unwrap().attempted_calls, 2);
    }

    #[test]
    fn call_once_never_lets_concurrent_callers_exceed_max_calls_even_under_a_race() {
        // Real gap this closes: the old check-then-increment (read usage.calls, release the
        // lock, increment later in record_usage) let N concurrently racing threads all observe
        // "under budget" before any of them had recorded an attempt. reserve_call_slot folds the
        // check and increment into one locked critical section instead.
        let usage = Llm::new_usage_tracker();
        let responses: Vec<String> = (0..5).map(|i| format!("r{i}")).collect();
        let llm = Llm::fixture(responses, 0, usage.clone()).with_max_calls(Some(5));
        let handles: Vec<_> = (0..20)
            .map(|_| {
                let llm = llm.clone();
                std::thread::spawn(move || llm.call_once(None, "task", None).is_ok())
            })
            .collect();
        let successes = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|ok| *ok)
            .count();
        assert_eq!(
            successes, 5,
            "exactly max_calls attempts should ever be let through, regardless of how many raced in concurrently"
        );
        assert_eq!(usage.lock().unwrap().attempted_calls, 5);
    }

    // --- #172: per-call telemetry ---

    #[test]
    fn calls_is_empty_when_no_log_is_attached() {
        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(vec!["a".to_string()], 0, usage);
        let _ = llm.text_ctx(None, "task", None);
        assert!(llm.calls().is_empty());
    }

    #[test]
    fn text_ctx_records_one_call_with_attempts_1_on_first_try_success() {
        let usage = Llm::new_usage_tracker();
        let log = Llm::new_calls_log();
        let llm = Llm::fixture(vec!["a".to_string()], 0, usage).with_calls_log(Some(log));
        llm.text_ctx(None, "task", None).unwrap();
        let calls = llm.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].attempts, 1);
        assert!(calls[0].success);
    }

    #[test]
    fn text_ctx_records_attempts_greater_than_1_after_a_retryable_failure_then_success() {
        let usage = Llm::new_usage_tracker();
        let log = Llm::new_calls_log();
        // Empty text first (triggers "empty response" retry per text_ctx's own logic), then a
        // real response — with retries=1, exactly 2 attempts should be recorded.
        let llm =
            Llm::fixture(vec!["".to_string(), "a".to_string()], 1, usage).with_calls_log(Some(log));
        llm.text_ctx(None, "task", None).unwrap();
        let calls = llm.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].attempts, 2);
        assert!(calls[0].success);
    }

    #[test]
    fn text_ctx_records_a_failed_call_with_success_false() {
        let usage = Llm::new_usage_tracker();
        let log = Llm::new_calls_log();
        let llm = Llm::fixture(vec![], 0, usage).with_calls_log(Some(log));
        let _ = llm.text_ctx(None, "task", None);
        let calls = llm.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].attempts, 1);
        assert!(!calls[0].success);
    }

    #[test]
    fn calls_log_is_shared_across_clones_via_with_calls_log() {
        // backend_factory shares one log between the main and cheap Llm the same way it already
        // shares `usage` — proving here that two Llm values built from the same shared log
        // (as if they were main_llm/cheap_llm) both contribute to and can both read the same
        // combined call history.
        let log = Llm::new_calls_log();
        let main = Llm::fixture(vec!["a".to_string()], 0, Llm::new_usage_tracker())
            .with_calls_log(Some(log.clone()));
        let cheap = Llm::fixture(vec!["b".to_string()], 0, Llm::new_usage_tracker())
            .with_calls_log(Some(log));
        main.text_ctx(None, "task", None).unwrap();
        cheap.text_ctx(None, "task", None).unwrap();
        assert_eq!(main.calls().len(), 2);
        assert_eq!(cheap.calls().len(), 2);
    }

    #[test]
    fn calls_recorded_via_a_shared_log_are_attributed_to_the_right_model() {
        // Extends the sharing test above: two Llm values contributing to one shared calls_log
        // (as backend_factory::build_llm sets up main_llm/cheap_llm) must still be distinguishable
        // by which one actually made each call.
        let log = Llm::new_calls_log();
        let mut main = Llm::fixture(vec!["a".to_string()], 0, Llm::new_usage_tracker())
            .with_calls_log(Some(log.clone()));
        main.model = Some("main-model".to_string());
        let mut cheap = Llm::fixture(vec!["b".to_string()], 0, Llm::new_usage_tracker())
            .with_calls_log(Some(log));
        cheap.model = Some("cheap-model".to_string());
        main.text_ctx(None, "task", None).unwrap();
        cheap.text_ctx(None, "task", None).unwrap();

        let calls = main.calls();
        assert_eq!(calls.len(), 2);
        let models: Vec<_> = calls.iter().map(|c| c.model.as_deref()).collect();
        assert!(models.contains(&Some("main-model")));
        assert!(models.contains(&Some("cheap-model")));
    }

    #[test]
    fn effective_timeout_returns_base_when_no_deadline_is_set() {
        let llm = test_llm();
        assert_eq!(
            llm.effective_timeout(Duration::from_secs(600)),
            Duration::from_secs(600)
        );
    }

    #[test]
    fn effective_timeout_shrinks_to_the_remaining_deadline_when_it_is_less_than_the_base() {
        let llm = test_llm().with_deadline(Some(Instant::now() + Duration::from_secs(10)));
        let effective = llm.effective_timeout(Duration::from_secs(600));
        assert!(
            effective <= Duration::from_secs(10) && effective >= Duration::from_secs(9),
            "expected ~10s, got {effective:?}"
        );
    }

    #[test]
    fn effective_timeout_does_not_shrink_the_base_when_deadline_is_further_away() {
        let llm = test_llm().with_deadline(Some(Instant::now() + Duration::from_secs(3600)));
        assert_eq!(
            llm.effective_timeout(Duration::from_secs(600)),
            Duration::from_secs(600)
        );
    }

    #[test]
    fn effective_timeout_floors_at_one_second_when_the_deadline_already_passed() {
        // #119: a deadline in the past must not hand a zero/negative timeout to a syscall.
        let llm = test_llm().with_deadline(Some(Instant::now() - Duration::from_secs(5)));
        assert_eq!(
            llm.effective_timeout(Duration::from_secs(600)),
            Duration::from_secs(1)
        );
    }

    // --- #143: deadline_passed() / deadline_aware_sleep() ---

    #[test]
    fn deadline_passed_is_false_when_no_deadline_is_set() {
        assert!(!test_llm().deadline_passed());
    }

    #[test]
    fn deadline_passed_is_false_before_the_deadline() {
        let llm = test_llm().with_deadline(Some(Instant::now() + Duration::from_secs(60)));
        assert!(!llm.deadline_passed());
    }

    #[test]
    fn deadline_passed_is_true_once_the_deadline_is_behind_now() {
        let llm = test_llm().with_deadline(Some(Instant::now() - Duration::from_secs(1)));
        assert!(llm.deadline_passed());
    }

    #[test]
    fn deadline_aware_sleep_does_not_block_past_a_deadline_that_has_already_passed() {
        // #143: the previous unconditional std::thread::sleep(backoff_delay(attempt)) would
        // have slept the full ~500ms+ here regardless of the deadline already being gone.
        let llm = test_llm().with_deadline(Some(Instant::now() - Duration::from_secs(1)));
        let started = Instant::now();
        llm.deadline_aware_sleep(Duration::from_secs(5));
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "expected an immediate return, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn deadline_aware_sleep_caps_at_the_remaining_budget_instead_of_the_full_backoff() {
        let llm = test_llm().with_deadline(Some(Instant::now() + Duration::from_millis(50)));
        let started = Instant::now();
        llm.deadline_aware_sleep(Duration::from_secs(5));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "expected to be capped near 50ms, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn is_retryable_treats_429_and_5xx_as_retryable() {
        for code in [429, 500, 502, 503] {
            let e: anyhow::Error = HttpError {
                code,
                body: String::new(),
                retry_after: None,
            }
            .into();
            assert!(is_retryable(&e), "{code} should be retryable");
        }
    }

    #[test]
    fn is_retryable_treats_other_4xx_as_permanent() {
        for code in [400, 401, 403, 404, 422] {
            let e: anyhow::Error = HttpError {
                code,
                body: String::new(),
                retry_after: None,
            }
            .into();
            assert!(!is_retryable(&e), "{code} should not be retryable");
        }
    }

    #[test]
    fn is_retryable_defaults_unclassified_errors_to_retryable() {
        // #119: network errors, JSON parse/schema-mismatch failures, claude CLI exit-code
        // errors — anything that isn't a classified HttpError keeps the pre-existing
        // always-retry behavior, since defaulting to "don't retry" would be the unsafe direction.
        let e = anyhow!("some other failure that isn't an HttpError");
        assert!(is_retryable(&e));
    }

    #[test]
    fn is_retryable_treats_call_budget_exceeded_as_permanent() {
        // #175: usage.calls isn't going to shrink between attempts, so retrying this just burns
        // backoff sleeps for no reason.
        let e: anyhow::Error = CallBudgetExceeded(3).into();
        assert!(!is_retryable(&e));
    }

    #[test]
    fn backoff_delay_grows_with_attempt_number() {
        // #119: exponential-ish growth, not a fixed delay every time.
        assert!(backoff_delay(0) < backoff_delay(3));
        assert!(backoff_delay(3) < backoff_delay(6));
    }

    #[test]
    fn backoff_delay_growth_is_capped_so_high_retry_counts_stay_bounded() {
        // attempt.min(6) caps the exponent — attempt 6 and attempt 20 must produce the same
        // base delay (jitter aside), not an absurdly long wait for a high --retries value.
        let at_cap = backoff_delay(6).as_millis();
        let past_cap = backoff_delay(20).as_millis();
        // Both draw jitter from a base of the same size, so they should land in the same
        // order of magnitude — this just guards against unbounded exponent growth, not exact
        // equality (jitter differs run to run).
        assert!(
            past_cap < at_cap * 2,
            "attempt 20's delay ({past_cap}ms) should be capped near attempt 6's ({at_cap}ms), not keep doubling"
        );
    }

    // --- #171: retry_delay() / build_repair_task() ---

    #[test]
    fn retry_delay_prefers_an_explicit_retry_after_over_the_generic_backoff() {
        let e: anyhow::Error = HttpError {
            code: 429,
            body: String::new(),
            retry_after: Some(Duration::from_secs(7)),
        }
        .into();
        assert_eq!(retry_delay(0, &e), Duration::from_secs(7));
    }

    #[test]
    fn retry_delay_falls_back_to_backoff_when_no_retry_after_is_present() {
        let e: anyhow::Error = HttpError {
            code: 500,
            body: String::new(),
            retry_after: None,
        }
        .into();
        // backoff_delay() draws jitter from the current instant, so two independent calls
        // aren't bit-for-bit equal — assert the result lands in backoff_delay(2)'s known range
        // (base 2000ms, jitter up to 1000ms) instead of comparing against a second live call.
        let delay = retry_delay(2, &e);
        assert!(
            delay >= Duration::from_millis(2000) && delay < Duration::from_millis(3000),
            "expected retry_delay to fall back to backoff_delay(2)'s ~2000-3000ms range, got {delay:?}"
        );
    }

    #[test]
    fn retry_delay_falls_back_to_backoff_for_a_non_http_error() {
        let e = anyhow!("some transport failure that isn't an HttpError");
        // base 1000ms, jitter up to 500ms — see the note above.
        let delay = retry_delay(1, &e);
        assert!(
            delay >= Duration::from_millis(1000) && delay < Duration::from_millis(1500),
            "expected retry_delay to fall back to backoff_delay(1)'s ~1000-1500ms range, got {delay:?}"
        );
    }

    #[test]
    fn build_repair_task_includes_the_prior_response_and_the_validation_error() {
        let e = anyhow!("missing field `finding_id`");
        let task = build_repair_task("# Task\nDo the thing.", "{\"oops\": true}", &e);
        assert!(task.contains("# Task\nDo the thing."));
        assert!(task.contains("{\"oops\": true}"));
        assert!(task.contains("missing field `finding_id`"));
    }

    #[derive(serde::Deserialize)]
    struct RepairTestOut {
        ok: bool,
    }

    #[test]
    fn json_ctx_typed_recovers_after_one_schema_failure_when_a_retry_is_available() {
        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(
            vec!["not json at all".to_string(), r#"{"ok":true}"#.to_string()],
            1,
            usage,
        );
        let out: RepairTestOut = llm
            .json_ctx_typed(None, "task", None)
            .expect("should recover via the repair attempt");
        assert!(out.ok);
    }

    #[test]
    fn json_ctx_typed_does_not_spend_a_repair_attempt_when_retries_is_zero() {
        let usage = Llm::new_usage_tracker();
        let llm = Llm::fixture(
            vec!["not json at all".to_string(), r#"{"ok":true}"#.to_string()],
            0,
            usage,
        );
        let err = llm.json_ctx_typed::<RepairTestOut>(None, "task", None);
        assert!(
            err.is_err(),
            "must fail outright — no repair budget available with retries=0"
        );
        // The second fixture entry must still be untouched — prove it by consuming it directly.
        let second = llm
            .call_once(None, "task", None)
            .expect("second fixture entry must still be queued, unconsumed by a repair attempt");
        assert_eq!(second.text, r#"{"ok":true}"#);
    }

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
