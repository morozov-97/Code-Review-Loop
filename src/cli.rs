use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// `review`'s exit code has never reflected `verdict` — the process exits 0
/// on any successful run regardless of what it found, so "wire this into CI" already meant
/// "shadow mode" by default, whether or not that was documented as a deliberate choice (see
/// README's "Recommended CI integration"). This is the explicit opt-in for a team that's done
/// running in shadow mode and wants an actual gate: `--fail-on` maps `verdict` to the process
/// exit code, at whatever severity the team picks. Default (`never`) keeps today's behavior
/// exactly as-is.
#[derive(clap::ValueEnum, Clone, Debug, Default, PartialEq)]
pub(crate) enum FailOn {
    /// Exit 0 regardless of verdict (default, unchanged from today's behavior).
    #[default]
    Never,
    /// Exit 1 on COMMENT or worse (any confirmed defect, or an unrelated policy failure).
    Comment,
    /// Exit 1 on NEEDS_CONTEXT or worse.
    NeedsContext,
    /// Exit 1 only on REQUEST_CHANGES (a confirmed P0, or an unresolved high-severity finding).
    RequestChanges,
}

impl FailOn {
    fn threshold_rank(&self) -> Option<u8> {
        match self {
            FailOn::Never => None,
            FailOn::Comment => Some(1),
            FailOn::NeedsContext => Some(2),
            FailOn::RequestChanges => Some(3),
        }
    }

    /// An unrecognized verdict string used to be silently treated like APPROVE (rank 0), on the
    /// reasoning that it "shouldn't ever see this in practice" — which is exactly backwards for
    /// a flag whose whole job is gating CI on verdict: "I don't recognize this verdict" and "no
    /// problems found" are not the same claim, and the first should never be allowed to pass as
    /// the second. Errors instead, so an unrecognized value is loud, not a silent pass-through.
    fn verdict_rank(verdict: &str) -> Result<u8> {
        match verdict {
            "APPROVE" => Ok(0),
            "COMMENT" => Ok(1),
            "NEEDS_CONTEXT" => Ok(2),
            "REQUEST_CHANGES" => Ok(3),
            other => anyhow::bail!(
                "unrecognized verdict {other:?} — refusing to guess whether --fail-on should trigger on it"
            ),
        }
    }

    pub(crate) fn triggers(&self, verdict: &str) -> Result<bool> {
        match self.threshold_rank() {
            None => Ok(false),
            Some(threshold) => Ok(Self::verdict_rank(verdict)? >= threshold),
        }
    }
}

#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
pub(crate) enum Backend {
    /// claude -p subprocess
    Claude,
    /// OpenRouter REST API (requires OPENROUTER_API_KEY)
    Openrouter,
    /// #156: any other OpenAI-compatible chat completions endpoint — self-hosted vLLM/Ollama/an
    /// internal gateway. Requires --base-url and --model; CODEREVIEW_API_KEY is optional (many
    /// self-hosted endpoints don't require one).
    Custom,
}

#[derive(Parser, Debug)]
#[command(
    name = "codereview",
    version,
    about = "Multi-angle (multi-lens) code review pipeline — independent per-lens review followed by discourse cross-verification"
)]
pub(crate) struct Cli {
    #[arg(long, default_value = "claude", global = true)]
    pub(crate) claude_bin: String,
    #[arg(long, value_enum, default_value = "claude", global = true)]
    pub(crate) backend: Backend,
    /// #156: base URL for --backend custom (e.g. http://localhost:11434/v1/chat/completions for
    /// a local Ollama). Ignored by the other backends.
    #[arg(long, global = true)]
    pub(crate) base_url: Option<String>,
    #[arg(long, global = true)]
    pub(crate) model: Option<String>,
    /// Low-cost model used for simple judgment stages like lens selection, good things,
    /// requirements verification, fix check, etc. Defaults to --model when unset (preserves existing behavior).
    #[arg(long, global = true)]
    pub(crate) cheap_model: Option<String>,
    #[arg(long, default_value_t = 2, global = true)]
    pub(crate) retries: u32,
    #[arg(long, global = true)]
    pub(crate) verbose: bool,
    /// #122: the diff (and --requirements/--conventions) is sent verbatim to an external LLM
    /// provider. By default, a local pattern-based scan refuses to run if it spots something
    /// that looks like a credential in an added line. Pass this flag to send it anyway.
    #[arg(long, global = true)]
    pub(crate) allow_sensitive_input: bool,
    /// #175: sent as max_tokens on OpenAI-compatible requests (OpenRouter/Custom backends) —
    /// the request body previously had no output cap at all. Ignored by the claude-cli backend
    /// (that CLI has its own limits this flag doesn't reach). Large enough for this project's
    /// own JSON schemas (findings/discourse/requirements arrays) while still bounding
    /// worst-case per-call output cost.
    #[arg(long, default_value_t = 8192, global = true)]
    pub(crate) max_output_tokens: u32,
    /// #175: hard ceiling on total provider calls (main + cheap model combined) across the whole
    /// run — a backstop against a misconfigured invocation (e.g. --lenses listing every optional
    /// lens, or a discourse loop that keeps re-requesting) rather than a normal-path limit.
    /// Unset means uncapped (existing behavior, unchanged).
    #[arg(long, global = true)]
    pub(crate) max_provider_calls: Option<u64>,
    /// Sent as `temperature` on OpenAI-compatible requests (OpenRouter/Custom backends) — unset
    /// (default) sends no value at all, i.e. whatever the provider/model defaults to (existing
    /// behavior, unchanged). A lower value (e.g. 0.0-0.2) trades some review nuance for more
    /// reproducible verdicts on a repeat run of the same diff — see README's "Path to
    /// production" for why this matters. Ignored by the claude-cli backend.
    #[arg(long, global = true)]
    pub(crate) temperature: Option<f64>,

    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Cmd {
    /// Independent per-lens review + discourse cross-verification (default pipeline)
    Review {
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        diff: PathBuf,
        #[arg(long)]
        requirements: Option<PathBuf>,
        #[arg(long)]
        conventions: Option<PathBuf>,
        #[arg(long)]
        deterministic_results: Option<PathBuf>,
        /// Manually specify lenses (comma-separated). If unset, the LLM picks based on the diff's nature.
        #[arg(long)]
        lenses: Option<String>,
        #[arg(long, default_value = "runs")]
        out: PathBuf,
        /// Per-lens reviews (review_lens) are independent of each other and can run in parallel —
        /// default is 3 (sized for 1-3 selected lenses + 1 always lens) to avoid running serially by default.
        #[arg(long, default_value_t = 3)]
        concurrency: usize,
        /// Maximum number of discourse rounds
        #[arg(long, default_value_t = 2)]
        max_rounds: usize,
        /// Previous round's --out directory (state.json). When set, adds FIXED/STILL_OPEN verdicts for previously confirmed findings.
        #[arg(long)]
        prior: Option<PathBuf>,
        /// Rewrite confirmed findings/good things in a human reviewer comment tone and attach to the report
        #[arg(long)]
        human_voice: bool,
        /// Language the LLM writes findings/evidence/reasoning text in (e.g. "Korean", "Japanese").
        /// Unset means English. report.md's own labels/headers are unaffected — only LLM-generated text.
        #[arg(long)]
        lang: Option<String>,
        /// Overall wall-clock budget across every remaining stage (discourse, fix check,
        /// requirements, human-voice). Each LLM call already has its own per-call timeout —
        /// this bounds the whole run instead, for automation waiting on this process. Checked
        /// between stages, not mid-call: an in-flight call still finishes or hits its own
        /// timeout first. Unset means no overall deadline (existing behavior, unchanged).
        #[arg(long)]
        deadline_minutes: Option<u64>,
        /// Maps `verdict` to the process exit code, for a CI job that wants an actual gate
        /// instead of shadow mode. Default (`never`) exits 0 regardless of verdict — today's
        /// existing behavior, unchanged.
        #[arg(long, value_enum, default_value = "never")]
        fail_on: FailOn,
    },
    /// PR title/summary/walkthrough/labels/splittability + TODO scan
    Describe {
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        diff: PathBuf,
        #[arg(long)]
        requirements: Option<PathBuf>,
        #[arg(long)]
        conventions: Option<PathBuf>,
        #[arg(long, default_value = "runs")]
        out: PathBuf,
        /// Language the LLM writes the description text in (e.g. "Korean"). Unset means English.
        #[arg(long)]
        lang: Option<String>,
    },
    /// Concrete code improvement suggestions (based on diff snippets)
    Improve {
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        diff: PathBuf,
        #[arg(long)]
        requirements: Option<PathBuf>,
        #[arg(long)]
        conventions: Option<PathBuf>,
        #[arg(long, default_value = "runs")]
        out: PathBuf,
        /// Language the LLM writes suggestion text in (e.g. "Korean"). Unset means English.
        #[arg(long)]
        lang: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fail_on_never_does_not_trigger_even_on_request_changes() {
        assert!(!FailOn::Never.triggers("REQUEST_CHANGES").unwrap());
    }

    #[test]
    fn fail_on_never_does_not_even_check_an_unrecognized_verdict() {
        // threshold_rank() is None for Never, short-circuiting before verdict_rank ever runs --
        // "never gate on anything" must hold even for a verdict string this binary doesn't
        // recognize, not just for the four known ones.
        assert!(!FailOn::Never.triggers("SOME_FUTURE_VERDICT").unwrap());
    }

    #[test]
    fn fail_on_comment_triggers_on_comment_and_worse() {
        assert!(!FailOn::Comment.triggers("APPROVE").unwrap());
        assert!(FailOn::Comment.triggers("COMMENT").unwrap());
        assert!(FailOn::Comment.triggers("NEEDS_CONTEXT").unwrap());
        assert!(FailOn::Comment.triggers("REQUEST_CHANGES").unwrap());
    }

    #[test]
    fn fail_on_needs_context_does_not_trigger_on_a_mere_comment() {
        assert!(!FailOn::NeedsContext.triggers("COMMENT").unwrap());
        assert!(FailOn::NeedsContext.triggers("NEEDS_CONTEXT").unwrap());
        assert!(FailOn::NeedsContext.triggers("REQUEST_CHANGES").unwrap());
    }

    #[test]
    fn fail_on_request_changes_only_triggers_on_request_changes_itself() {
        assert!(!FailOn::RequestChanges.triggers("APPROVE").unwrap());
        assert!(!FailOn::RequestChanges.triggers("COMMENT").unwrap());
        assert!(!FailOn::RequestChanges.triggers("NEEDS_CONTEXT").unwrap());
        assert!(FailOn::RequestChanges.triggers("REQUEST_CHANGES").unwrap());
    }

    #[test]
    fn fail_on_default_is_never() {
        assert_eq!(FailOn::default(), FailOn::Never);
    }

    #[test]
    fn fail_on_errors_instead_of_silently_passing_an_unrecognized_verdict() {
        // Real gap this closes: an unrecognized verdict string used to be silently ranked like
        // APPROVE, so "I don't know what this verdict means" and "no problems found" were
        // indistinguishable to a CI gate. Any threshold that isn't Never must now surface this
        // loudly instead of quietly declining to trigger.
        let err = FailOn::Comment
            .triggers("SOME_FUTURE_VERDICT")
            .expect_err("an unrecognized verdict must not be allowed to silently pass a gate");
        assert!(err.to_string().contains("unrecognized verdict"));
    }
}
