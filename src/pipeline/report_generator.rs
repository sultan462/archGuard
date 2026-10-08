use crate::DataModels::Models::*;
use std::fmt::Write;

pub fn report_generator(
    config: Option<&ArchGuardConfig>,
    dependencies: &[DependencyEdge],
    run: &RunContext,
) -> GeneratedReport {
    let mut violations: Vec<_> = dependencies
        .iter()
        .filter(|edge| !edge.is_allowed)
        .filter_map(|edge| {
            Some(Violation {
                source: ViolationSource {
                    module: edge.source_module.clone()?,
                    file: edge.source_file.to_string_lossy().replace('\\', "/"),
                    line: edge.line,
                    column: edge.column,
                    statement: edge.statement.clone(),
                },
                target: ViolationTarget {
                    module: edge.target_module.clone()?,
                    file: edge
                        .target_file
                        .as_ref()?
                        .to_string_lossy()
                        .replace('\\', "/"),
                },
                failed_constraints: edge.failed_constraints.clone(),
            })
        })
        .collect();
    violations.sort_by(|a, b| {
        (
            &a.source.file,
            a.source.line,
            a.source.column,
            &a.target.file,
        )
            .cmp(&(
                &b.source.file,
                b.source.line,
                b.source.column,
                &b.target.file,
            ))
    });
    let incomplete = run
        .analysis
        .as_ref()
        .is_some_and(|analysis| analysis.files_failed > 0 || analysis.unresolved_imports > 0);
    let status = if run.fatal {
        RunStatus::Error
    } else if incomplete {
        RunStatus::Incomplete
    } else if !violations.is_empty() {
        RunStatus::Fail
    } else {
        RunStatus::Pass
    };
    let mut diagnostics = run.diagnostics.clone();
    diagnostics.sort_by(|a, b| {
        let key = |d: &Diagnostic| {
            (
                d.location.as_ref().map(|loc| loc.file.clone()),
                d.location.as_ref().and_then(|loc| loc.line),
                d.location.as_ref().and_then(|loc| loc.column),
                d.code.clone(),
                d.message.clone(),
            )
        };
        key(a).cmp(&key(b))
    });
    let summary = Summary {
        modules: config.map(|config| config.modules.len()), rules: config.map(|config| config.rules.as_ref().map_or(0, Vec::len)),
        files_assigned: run.discovery.as_ref().map(|discovery| discovery.files_assigned),
        files_unassigned: run.discovery.as_ref().map(|discovery| discovery.files_unassigned),
        files_ignored: run.discovery.as_ref().map(|discovery| discovery.files_ignored),
        files_analyzed: run.analysis.as_ref().map(|analysis| analysis.files_analyzed),
        files_failed: run.analysis.as_ref().map(|analysis| analysis.files_failed),
        unresolved_imports: run.analysis.as_ref().map(|analysis| analysis.unresolved_imports),
        dependencies_checked: run.checked.then(|| dependencies.iter().filter(|edge| {
            edge.resolution == DependencyResolution::Project && !edge.target_ignored && edge.target_file.is_some()
                && matches!((&edge.source_module, &edge.target_module), (Some(source), Some(target)) if source != target)
        }).count()),
        violations: run.checked.then_some(violations.len()),
    };
    let mut coverage = run
        .discovery
        .as_ref()
        .map(|discovery| Coverage {
            unassigned_files: discovery.unassigned_files.clone(),
            empty_modules: discovery.empty_modules.clone(),
        })
        .unwrap_or_default();
    coverage.unassigned_files.sort();
    coverage.empty_modules.sort();
    let report = Report {
        report_version: "1",
        tool: ToolReport {
            name: "archguard",
            version: env!("CARGO_PKG_VERSION"),
        },
        project: ProjectReport {
            name: config.and_then(|config| config.name.clone()),
            config: run.config_label.clone(),
            language: config.map(|config| config.language.to_ascii_lowercase()),
        },
        run: RunReport {
            status,
            duration_ms: run.duration_ms,
        },
        summary,
        violations,
        coverage,
        diagnostics,
    };
    // This schema consists entirely of strings, integers, enums, sequences and string-keyed
    // objects, so serde_json has no fallible values (such as non-string map keys) to encode.
    let json =
        serde_json::to_string_pretty(&report).expect("the report schema is JSON-serializable");
    GeneratedReport {
        human: human_report(&report),
        json,
        exit_code: status.exit_code(),
    }
}
fn count(value: Option<usize>) -> String {
    value
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unavailable".into())
}
fn human_report(report: &Report) -> String {
    let mut text = String::new();
    let language = match report.project.language.as_deref() {
        Some("dart") => "Dart",
        Some("python") => "Python",
        Some(other) => other,
        None => "unknown language",
    };
    let _ = writeln!(
        text,
        "{}  {} · {}\n",
        report.run.status.label(),
        report
            .project
            .name
            .as_deref()
            .unwrap_or(&report.project.config),
        language
    );
    let _ = writeln!(
        text,
        "{} files analyzed · {} modules · {} dependencies checked",
        count(report.summary.files_analyzed),
        count(report.summary.modules),
        count(report.summary.dependencies_checked)
    );
    let analysis_errors = report
        .diagnostics
        .iter()
        .filter(|d| {
            matches!(
                d.code.as_str(),
                "FILE_READ_FAILED" | "PARSE_FAILED" | "IMPORT_UNRESOLVED"
            )
        })
        .count();
    let _ = writeln!(
        text,
        "{} violations · {} uncovered files · {} analysis errors",
        count(report.summary.violations),
        count(report.summary.files_unassigned),
        analysis_errors
    );
    for violation in &report.violations {
        let source = &violation.source;
        let _ = writeln!(
            text,
            "\n{}:{}:{}\n{} → {}",
            source.file, source.line, source.column, source.module, violation.target.module
        );
        for line in source.statement.lines() {
            let _ = writeln!(text, "  {line}");
        }
        for constraint in &violation.failed_constraints {
            let fallback = format!(
                "Rule {} ({})",
                constraint.rule_index, constraint.rule_module
            );
            let _ = writeln!(
                text,
                "  Rule: {}",
                constraint.rule_name.as_deref().unwrap_or(&fallback)
            );
            let label = match constraint.constraint {
                ConstraintKind::DenyTo => "Denied targets",
                ConstraintKind::AllowOnlyTo => "Allowed targets",
                ConstraintKind::DenyFrom => "Denied sources",
                ConstraintKind::AllowOnlyFrom => "Allowed sources",
            };
            let names = if constraint.configured_modules.is_empty() {
                "(none)".into()
            } else {
                constraint.configured_modules.join(", ")
            };
            let _ = writeln!(text, "  {label}: {names}");
            if let Some(reason) = &constraint.reason {
                let _ = writeln!(text, "  Reason: {reason}");
            }
            let _ = writeln!(
                text,
                "  Config: {}:{}",
                report.project.config, constraint.config_line
            );
        }
    }
    if !report.coverage.unassigned_files.is_empty() {
        let _ = writeln!(text, "\nCoverage gaps");
        for file in &report.coverage.unassigned_files {
            let _ = writeln!(text, "  {file}");
        }
    }
    if !report.coverage.empty_modules.is_empty() {
        let _ = writeln!(text, "\nEmpty modules");
        for module in &report.coverage.empty_modules {
            let _ = writeln!(text, "  {module}");
        }
    }
    if !report.diagnostics.is_empty() {
        let _ = writeln!(text, "\nDiagnostics");
        for diagnostic in &report.diagnostics {
            let mut at = String::new();
            if let Some(location) = &diagnostic.location {
                at.push_str(&location.file);
                if let Some(line) = location.line {
                    let _ = write!(at, ":{line}");
                }
                if let Some(column) = location.column {
                    let _ = write!(at, ":{column}");
                }
                at.push_str(": ");
            }
            let _ = writeln!(text, "  {at}{}: {}", diagnostic.code, diagnostic.message);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn error_report_preserves_unavailable_values_and_null_locations() {
        let run = RunContext {
            fatal: true,
            config_label: "missing.yaml".into(),
            diagnostics: vec![Diagnostic::error("INTERNAL_ERROR", "No location", None)],
            ..RunContext::default()
        };
        let output = report_generator(None, &[], &run);
        let json: serde_json::Value = serde_json::from_str(&output.json).unwrap();
        assert_eq!(output.exit_code, 2);
        assert_eq!(json["run"]["status"], "error");
        assert!(json["project"]["language"].is_null());
        assert!(
            json["summary"]
                .as_object()
                .unwrap()
                .values()
                .all(serde_json::Value::is_null)
        );
        assert!(json["diagnostics"][0]["location"].is_null());
        assert!(!output.human.contains("Empty modules"));
    }
}
