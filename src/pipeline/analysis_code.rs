use crate::DataModels::Models::{AnalysisOutcome, ArchGuardConfig, Diagnostic, PipelineError};

pub fn analysis_code(config: &ArchGuardConfig) -> Result<AnalysisOutcome, PipelineError> {
    if config.language.eq_ignore_ascii_case("python") {
        python_analysis(config)
    } else {
        Err(Diagnostic::error(
            "LANGUAGE_UNSUPPORTED",
            format!(
                "Unsupported language '{}'; supported values: python",
                config.language
            ),
            None,
        )
        .into())
    }
}
fn python_analysis(config: &ArchGuardConfig) -> Result<AnalysisOutcome, PipelineError> {
    crate::Languages::python::analyze(config)
}
