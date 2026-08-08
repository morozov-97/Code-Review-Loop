use crate::cli::{Backend, Cli, Cmd};
use crate::llm::{CallGate, Llm};
use anyhow::{Context, Result};

/// #166: the `review` subcommand's own `--concurrency` is the natural size for the shared
/// call gate — `describe`/`improve` don't have that flag (they only ever make one main-model
/// call at a time), so a generous fixed default there is fine; it's not actually load-bearing
/// for those subcommands.
fn concurrency_hint(cmd: &Cmd) -> usize {
    match cmd {
        Cmd::Review { concurrency, .. } => *concurrency,
        _ => 4,
    }
}

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
    // #166: one gate shared by both models — a lens par_map call and a good_things/requirements/
    // human_voice call made on the cheap model at the same moment both draw from the same total
    // budget instead of each having their own uncounted allowance.
    let gate = CallGate::new(concurrency_hint(&cli.cmd));
    // #172: one shared per-call log, same reasoning as the shared gate/usage — a manifest built
    // from it after the run sees every call made on either model, not just one.
    let calls_log = Llm::new_calls_log();
    // #175: max_output_tokens is only meaningful for the OpenAI-compatible backends (harmless,
    // ignored, for ClaudeCli/Fixture) — applied unconditionally regardless of backend since
    // with_max_output_tokens is a no-op where it doesn't apply. max_provider_calls is shared via
    // the same `usage` tracker both models already share, so it's a real combined budget.
    Ok((
        main_llm
            .with_gate(Some(gate.clone()))
            .with_max_output_tokens(Some(cli.max_output_tokens))
            .with_temperature(cli.temperature)
            .with_max_calls(cli.max_provider_calls)
            .with_calls_log(Some(calls_log.clone())),
        cheap_llm
            .with_gate(Some(gate))
            .with_max_output_tokens(Some(cli.max_output_tokens))
            .with_temperature(cli.temperature)
            .with_max_calls(cli.max_provider_calls)
            .with_calls_log(Some(calls_log)),
    ))
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

    #[test]
    fn build_llm_defaults_max_output_tokens_to_8192() {
        let cli = parse(&["review", "--spec", "s.toml", "--diff", "d.patch"]);
        assert_eq!(cli.max_output_tokens, 8192);
    }

    #[test]
    fn build_llm_leaves_max_provider_calls_uncapped_by_default() {
        let cli = parse(&["review", "--spec", "s.toml", "--diff", "d.patch"]);
        assert_eq!(cli.max_provider_calls, None);
    }

    #[test]
    fn build_llm_wires_max_provider_calls_and_max_output_tokens_into_both_models() {
        let cli = parse(&[
            "--max-provider-calls",
            "5",
            "--max-output-tokens",
            "1234",
            "review",
            "--spec",
            "s.toml",
            "--diff",
            "d.patch",
        ]);
        let (main_llm, cheap_llm) = build_llm(&cli).expect("should build successfully");
        // max_calls/max_output_tokens are private Llm fields — Debug (auto-derived) is the only
        // externally observable way to check they were actually set, short of a real call.
        let main_debug = format!("{main_llm:?}");
        let cheap_debug = format!("{cheap_llm:?}");
        assert!(main_debug.contains("max_calls: Some(5)"));
        assert!(main_debug.contains("max_output_tokens: Some(1234)"));
        assert!(cheap_debug.contains("max_calls: Some(5)"));
        assert!(cheap_debug.contains("max_output_tokens: Some(1234)"));
    }

    #[test]
    fn build_llm_leaves_temperature_unset_by_default() {
        let cli = parse(&["review", "--spec", "s.toml", "--diff", "d.patch"]);
        assert_eq!(cli.temperature, None);
        let (main_llm, cheap_llm) = build_llm(&cli).expect("should build successfully");
        assert!(format!("{main_llm:?}").contains("temperature: None"));
        assert!(format!("{cheap_llm:?}").contains("temperature: None"));
    }

    #[test]
    fn build_llm_wires_a_configured_temperature_into_both_models() {
        let cli = parse(&[
            "--temperature",
            "0.2",
            "review",
            "--spec",
            "s.toml",
            "--diff",
            "d.patch",
        ]);
        let (main_llm, cheap_llm) = build_llm(&cli).expect("should build successfully");
        assert!(format!("{main_llm:?}").contains("temperature: Some(0.2)"));
        assert!(format!("{cheap_llm:?}").contains("temperature: Some(0.2)"));
    }
}
