use crate::improve;
use crate::input;
use crate::llm::Llm;
use crate::pipeline::prepare_out;
use crate::report;
use crate::spec::Spec;
use anyhow::Result;
use std::path::{Path, PathBuf};

pub(crate) fn run_improve(
    llm: &Llm,
    spec_path: &Path,
    diff_path: &Path,
    requirements_path: &Option<PathBuf>,
    conventions_path: &Option<PathBuf>,
    out: &Path,
    lang: &Option<String>,
) -> Result<()> {
    let sp = Spec::load(spec_path)?;
    let inp = input::normalize(
        diff_path,
        requirements_path,
        conventions_path,
        &None,
        lang.clone(),
    )?;
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
