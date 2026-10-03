#[allow(non_snake_case)]
mod DataModels {
    pub mod Models;
}
#[allow(non_snake_case)]
mod Languages {
    pub mod python;
}
mod pipeline {
    pub mod analysis_code;
    pub mod checker;
    pub mod files_getter;
    pub mod get_data_from_yaml;
    pub mod report_generator;
    pub mod validate;
}

use DataModels::Models::{AnalysisCounts, Diagnostic, GeneratedReport, PipelineError, RunContext};
use clap::{Parser, ValueEnum};
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

#[derive(Parser)]
#[command(
    name = "archguard",
    version,
    about = "Check project dependencies and imports against architectural boundaries"
)]
struct Cli {
    /// YAML configuration; all project paths are relative to its directory.
    config: String,
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    format: OutputFormat,
}
#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    Human,
    Json,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let generated = start_pipeline(&cli.config);
    let output = match cli.format {
        OutputFormat::Human => &generated.human,
        OutputFormat::Json => &generated.json,
    };
    if let Err(error) = writeln!(std::io::stdout().lock(), "{output}") {
        eprintln!("INTERNAL_ERROR: Cannot write report: {error}");
        return ExitCode::from(2);
    }
    ExitCode::from(generated.exit_code)
}

fn start_pipeline(file_path: &str) -> GeneratedReport {
    let started = Instant::now();
    let path = Path::new(file_path);
    let mut run = RunContext {
        config_label: path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned(),
        ..RunContext::default()
    };
    let mut config = None;
    let mut dependencies = Vec::new();
    let result = (|| -> Result<(), PipelineError> {
        config = Some(pipeline::get_data_from_yaml::get_data_from_yaml(file_path)?);
        let config = config.as_mut().expect("config was just loaded");
        pipeline::validate::validate(config)?;
        run.discovery = Some(pipeline::files_getter::files_getter(config)?);
        let analysis = pipeline::analysis_code::analysis_code(config)?;
        run.analysis = Some(AnalysisCounts {
            files_analyzed: analysis.files.iter().filter(|file| file.succeeded).count(),
            files_failed: analysis.files.iter().filter(|file| !file.succeeded).count(),
            unresolved_imports: analysis
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.code == "IMPORT_UNRESOLVED")
                .count(),
        });
        debug_assert!(analysis.files.iter().all(|file| !file.file.is_absolute()));
        run.diagnostics.extend(analysis.diagnostics);
        dependencies = analysis.dependencies;
        pipeline::checker::checker(config, &mut dependencies)
            .map_err(|message| Diagnostic::error("INTERNAL_ERROR", message, None))?;
        run.checked = true;
        Ok(())
    })();
    if let Err(error) = result {
        run.fatal = true;
        run.diagnostics.extend(error.diagnostics);
    }
    run.duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    pipeline::report_generator::report_generator(config.as_ref(), &dependencies, &run)
}
