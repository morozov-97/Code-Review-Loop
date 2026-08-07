use crate::improve;
use crate::input;
use crate::llm::Llm;
use crate::pipeline::{enforce_secret_scan, prepare_out};
use crate::report;
use crate::spec::Spec;
use anyhow::Result;
use std::path::{Path, PathBuf};

// #122 added allow_sensitive_input, pushing this over clippy's 7-arg default. Not bundling into
// a config struct like ReviewArgs (#104) — describe/improve take half as many params and don't
// carry review's growth history, so a struct here would be premature.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_improve(
    llm: &Llm,
    spec_path: &Path,
    diff_path: &Path,
    requirements_path: &Option<PathBuf>,
    conventions_path: &Option<PathBuf>,
    out: &Path,
    lang: &Option<String>,
    allow_sensitive_input: bool,
) -> Result<()> {
    let sp = Spec::load(spec_path)?;
    let (inp, _dropped_files) = input::normalize(
        diff_path,
        requirements_path,
        conventions_path,
        &None,
        lang.clone(),
    )?;
    enforce_secret_scan(&inp, allow_sensitive_input)?;
    let out_dir = prepare_out(out)?;
    let suggestions = improve::run(llm, &sp, &inp)?;
    let path = report::write_improve(&out_dir, &suggestions)?;
    println!(
        "improve complete: {} suggestions — {}",
        suggestions.len(),
        path.display()
    );
    println!("{}", llm.usage().summary());
    Ok(())
}
