/// Cross-cutting run settings that aren't specific to any one diff/PR (unlike `Input`'s other
/// fields) but that prompt-building code needs — e.g. output language. Lives on `Input` as a
/// single nested field (`Input.config`) so every prompt builder gets it for free without a
/// separate parameter, while staying a distinct, discoverable place to add future global
/// settings instead of scattering more ad-hoc fields across `Input`.
#[derive(Debug, Clone, Default)]
pub struct RunConfig {
    /// Language the LLM writes findings/evidence/reasoning text in (e.g. "Korean"). None means
    /// no instruction is given, so the LLM defaults to English. Read only by
    /// `promptctx::shared_context`.
    pub language: Option<String>,
}
