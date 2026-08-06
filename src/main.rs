mod backend_factory;
mod cli;
mod core;
mod describe;
mod discourse;
mod fixcheck;
mod humanvoice;
mod improve;
mod input;
mod lens;
mod llm;
mod pipeline;
mod policy;
mod promptctx;
mod quantify;
mod report;
mod requirements;
mod semgrep;
mod spec;
mod state;

use anyhow::Result;
use backend_factory::build_llm;
use clap::Parser;
use cli::{Cli, Cmd};
use pipeline::describe::run_describe;
use pipeline::improve::run_improve;
use pipeline::review::{run_review, ReviewArgs};

fn main() {
    if let Err(e) = real_main() {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let (llm, cheap_llm) = build_llm(&cli)?;

    match &cli.cmd {
        Cmd::Review {
            spec,
            diff,
            requirements,
            conventions,
            deterministic_results,
            lenses,
            out,
            concurrency,
            max_rounds,
            prior,
            human_voice,
            lang,
            deadline_minutes,
        } => run_review(
            &llm,
            &cheap_llm,
            &ReviewArgs {
                spec_path: spec,
                diff_path: diff,
                requirements_path: requirements,
                conventions_path: conventions,
                deterministic_results_path: deterministic_results,
                lenses_arg: lenses,
                out,
                concurrency: *concurrency,
                max_rounds: *max_rounds,
                prior,
                human_voice: *human_voice,
                lang,
                deadline_minutes: *deadline_minutes,
            },
        ),
        Cmd::Describe {
            spec,
            diff,
            requirements,
            conventions,
            out,
            lang,
        } => run_describe(&llm, spec, diff, requirements, conventions, out, lang),
        Cmd::Improve {
            spec,
            diff,
            requirements,
            conventions,
            out,
            lang,
        } => run_improve(&llm, spec, diff, requirements, conventions, out, lang),
    }
}
