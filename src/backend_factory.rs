use crate::cli::{Backend, Cli};
use crate::llm::Llm;
use anyhow::{Context, Result};

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
        Backend::Custom => {
            let base_url = cli
                .base_url
                .clone()
                .context("--backend custom requires --base-url")?;
            let model = cli
                .model
                .clone()
                .context("--backend custom requires --model (no universal default for an arbitrary endpoint)")?;
            // Optional on purpose (see Provider::Custom's doc comment) — many self-hosted
            // endpoints (e.g. a local Ollama) don't require one.
            let api_key = std::env::var("CODEREVIEW_API_KEY").ok();
            let cheap = cheap_model.unwrap_or_else(|| model.clone());
            (
                Llm::custom_endpoint(
                    base_url.clone(),
                    api_key.clone(),
                    model,
                    cli.retries,
                    cli.verbose,
                    usage.clone(),
                ),
                Llm::custom_endpoint(
                    base_url,
                    api_key,
                    cheap,
                    cli.retries,
                    cli.verbose,
                    usage.clone(),
                ),
            )
        }
    };
    Ok((main_llm, cheap_llm))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        let mut full = vec!["codereview"];
        full.extend_from_slice(args);
        Cli::try_parse_from(full).expect("args should parse")
    }

    #[test]
    fn build_llm_rejects_custom_backend_without_base_url() {
        let cli = parse(&[
            "--backend",
            "custom",
            "--model",
            "some-model",
            "review",
            "--spec",
            "s.toml",
            "--diff",
            "d.patch",
        ]);
        let err = build_llm(&cli).expect_err("missing --base-url must be rejected");
        assert!(err.to_string().contains("--base-url"));
    }

    #[test]
    fn build_llm_rejects_custom_backend_without_model() {
        let cli = parse(&[
            "--backend",
            "custom",
            "--base-url",
            "http://localhost:11434/v1/chat/completions",
            "review",
            "--spec",
            "s.toml",
            "--diff",
            "d.patch",
        ]);
        let err = build_llm(&cli).expect_err("missing --model must be rejected");
        assert!(err.to_string().contains("--model"));
    }

    #[test]
    fn build_llm_accepts_custom_backend_with_base_url_and_model() {
        let cli = parse(&[
            "--backend",
            "custom",
            "--base-url",
            "http://localhost:11434/v1/chat/completions",
            "--model",
            "llama3",
            "review",
            "--spec",
            "s.toml",
            "--diff",
            "d.patch",
        ]);
        let (main_llm, cheap_llm) = build_llm(&cli).expect("should build successfully");
        assert_eq!(main_llm.model.as_deref(), Some("llama3"));
        assert_eq!(cheap_llm.model.as_deref(), Some("llama3"));
    }

    #[test]
    fn build_llm_custom_backend_uses_cheap_model_when_given() {
        let cli = parse(&[
            "--backend",
            "custom",
            "--base-url",
            "http://localhost:11434/v1/chat/completions",
            "--model",
            "llama3-70b",
            "--cheap-model",
            "llama3-8b",
            "review",
            "--spec",
            "s.toml",
            "--diff",
            "d.patch",
        ]);
        let (main_llm, cheap_llm) = build_llm(&cli).expect("should build successfully");
        assert_eq!(main_llm.model.as_deref(), Some("llama3-70b"));
        assert_eq!(cheap_llm.model.as_deref(), Some("llama3-8b"));
    }
}
