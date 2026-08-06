use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
pub(crate) enum Backend {
    /// claude -p subprocess
    Claude,
    /// OpenRouter REST API (requires OPENROUTER_API_KEY)
    Openrouter,
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
