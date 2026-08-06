use crate::cli::{Backend, Cli};
use crate::llm::Llm;
use anyhow::Result;

/// A (main model, cheap model) pair. If `--cheap-model` isn't specified, the cheap model is the
/// same as the main model, preserving existing behavior. Both share a single usage tracker to produce combined usage totals.
pub(crate) fn build_llm(cli: &Cli) -> Result<(Llm, Llm)> {
    // #118: neither backend redacts secrets/PII from the diff before sending it — a hardcoded
    // credential the review is supposed to catch goes out in the same request that's meant to
    // catch it. Applies to both backends equally (claude -p sends to Anthropic, openrouter to
    // whichever model is configured) — this is a one-line heads-up, not a gate, since refusing
    // to run would defeat the tool's entire premise.
    eprintln!(
        "Note: this sends the diff and any --requirements/--conventions content to the configured \
         LLM provider ({:?} backend). Don't run it against code containing secrets or restricted \
         data unless that's acceptable for your org.",
        cli.backend
    );
    let usage = Llm::new_usage_tracker();
    let cheap_model = cli.cheap_model.clone().or_else(|| cli.model.clone());
    let (main_llm, cheap_llm) = match cli.backend {
        Backend::Claude => (
            Llm::claude_cli(
                cli.claude_bin.clone(),
                cli.model.clone(),
                cli.retries,
                cli.verbose,
                usage.clone(),
            ),
            Llm::claude_cli(
                cli.claude_bin.clone(),
                cheap_model,
                cli.retries,
                cli.verbose,
                usage.clone(),
            ),
        ),
        Backend::Openrouter => (
            Llm::openrouter(cli.model.clone(), cli.retries, cli.verbose, usage.clone())?,
            Llm::openrouter(cheap_model, cli.retries, cli.verbose, usage.clone())?,
        ),
    };
    Ok((main_llm, cheap_llm))
}
