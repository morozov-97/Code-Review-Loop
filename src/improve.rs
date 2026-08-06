use crate::input::Input;
use crate::llm::Llm;
use crate::promptctx::shared_context;
use crate::spec::Spec;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const IMPROVE_SYSTEM: &str = "You are a reviewer who proposes concrete code improvements. \
Only suggest changes for lines added (+) in this diff. Don't suggest anything already addressed, and don't suggest docstring/type-hint/comment/unused-import changes. \
Respond only in the specified JSON schema.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub relevant_file: String,
    #[serde(default)]
    pub language: String,
    pub existing_code: String,
    pub suggestion_content: String,
    pub improved_code: String,
    pub one_sentence_summary: String,
    pub label: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ImproveOutput {
    #[serde(default)]
    suggestions: Vec<Suggestion>,
}

pub fn run(llm: &Llm, spec: &Spec, input: &Input) -> Result<Vec<Suggestion>> {
    let ctx = shared_context(spec, input);
    let task = format!(
        "# Task\nPropose concrete code improvements for the new (+) lines in this diff.\n\n\
         ## Rules\n\
         - existing_code/improved_code must quote/edit the actual code from the diff verbatim.\n\
         - one_sentence_summary must be under 6 words.\n\
         - label must be exactly one of: {labels}\n\n\
         ## Output (JSON only, no code fences)\n\
         {{\"suggestions\":[{{\"relevant_file\":\"...\",\"language\":\"...\",\"existing_code\":\"...\",\
         \"suggestion_content\":\"...\",\"improved_code\":\"...\",\"one_sentence_summary\":\"...\",\
         \"label\":<one of the allowed values>}}]}}\n",
        labels = spec.labels_prompt(),
    );
    let out: ImproveOutput = llm
        .json_ctx_typed(Some(&ctx), &task, Some(IMPROVE_SYSTEM))
        .context("improve failed")?;
    Ok(out.suggestions)
}
