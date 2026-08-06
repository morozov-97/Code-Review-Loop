use crate::describe;
use crate::input;
use crate::llm::Llm;
use crate::pipeline::prepare_out;
use crate::report;
use crate::spec::Spec;
use anyhow::Result;
use std::path::{Path, PathBuf};

pub(crate) fn run_describe(
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
    let d = describe::run(llm, &sp, &inp)?;
    let todos = describe::todo_sections(&inp.diff);
    let path = report::write_describe(&out_dir, &d, &todos)?;
    println!("describe complete: {}", path.display());
    println!("{}", llm.usage().summary());
    Ok(())
}
