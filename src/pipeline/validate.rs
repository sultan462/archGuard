use crate::DataModels::Models::{ArchGuardConfig, Diagnostic, PipelineError, SourceLocation};
use globset::GlobBuilder;
use std::collections::HashSet;
use std::path::{Component, Path};

pub fn validate(config: &ArchGuardConfig) -> Result<(), PipelineError> {
    let mut diagnostics = Vec::new();
    let label = config
        .config_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let location = |line| {
        Some(SourceLocation {
            file: label.clone(),
            line,
            column: None,
        })
    };
    if config.version != "1" {
        diagnostics.push(Diagnostic::error(
            "CONFIG_INVALID",
            "Unsupported version; expected string \"1\"",
            location(None),
        ));
    }
    if !config.language.eq_ignore_ascii_case("python")
        && !config.language.eq_ignore_ascii_case("dart")
    {
        diagnostics.push(Diagnostic::error(
            "LANGUAGE_UNSUPPORTED",
            format!(
                "Unsupported language '{}'; supported values: python, dart",
                config.language
            ),
            location(None),
        ));
    }
    let mut modules = HashSet::new();
    for module in &config.modules {
        if !modules.insert(module.name.as_str()) {
            diagnostics.push(Diagnostic::error(
                "CONFIG_INVALID",
                format!("Duplicate module name '{}'", module.name),
                location(None),
            ));
        }
        if let Err(message) = validate_pattern(&module.path) {
            diagnostics.push(Diagnostic::error(
                "CONFIG_INVALID",
                format!("Module '{}': {message}", module.name),
                location(None),
            ));
        }
    }
    for pattern in config.ignore.iter().flatten() {
        if let Err(message) = validate_pattern(pattern) {
            diagnostics.push(Diagnostic::error("CONFIG_INVALID", message, location(None)));
        }
    }
    for (index, rule) in config.rules.iter().flatten().enumerate() {
        if !modules.contains(rule.module.as_str()) {
            diagnostics.push(Diagnostic::error(
                "CONFIG_INVALID",
                format!(
                    "Rule {index} references undeclared module '{}'",
                    rule.module
                ),
                location(rule.source_line),
            ));
        }
        if rule
            .constraints()
            .iter()
            .all(|(_, values)| values.is_none())
        {
            diagnostics.push(Diagnostic::error(
                "CONFIG_INVALID",
                format!("Rule {index} must contain at least one constraint"),
                location(rule.source_line),
            ));
        }
        for (kind, names) in rule.constraints() {
            for name in names.into_iter().flatten() {
                if !modules.contains(name.as_str()) {
                    diagnostics.push(Diagnostic::error(
                        "CONFIG_INVALID",
                        format!(
                            "Rule {index} {} references undeclared module '{name}'",
                            kind.as_str()
                        ),
                        location(rule.constraint_lines.get(&kind).copied()),
                    ));
                }
            }
        }
    }
    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(PipelineError { diagnostics })
    }
}
fn validate_pattern(pattern: &str) -> Result<(), String> {
    if Path::new(pattern).is_absolute()
        || Path::new(pattern)
            .components()
            .any(|part| part == Component::ParentDir)
    {
        return Err(format!(
            "Pattern '{pattern}' must be relative and stay within the configuration directory"
        ));
    }
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|_| ())
        .map_err(|error| format!("Invalid glob '{pattern}': {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(tail: &str) -> ArchGuardConfig {
        serde_yaml::from_str(&format!("version: '1'\nlanguage: python\nmodules:\n  - {{name: A, path: 'a/*.py'}}\n  - {{name: a, path: 'b/*.py'}}\n{tail}")).unwrap()
    }
    #[test]
    fn validates_references_empty_constraints_and_case_sensitivity() {
        assert!(validate(&config("rules: [{module: A, deny_to: [a]}]")).is_ok());
        assert!(validate(&config("rules: [{module: A, allow_only_to: []}]")).is_ok());
        for tail in [
            "rules: [{module: A}]",
            "rules: [{module: missing, deny_to: []}]",
            "rules: [{module: A, deny_to: [MISSING]}]",
        ] {
            assert!(validate(&config(tail)).is_err());
        }
        let mut config = config("");
        config.language = "PyThOn".into();
        assert!(validate(&config).is_ok());
        config.language = "typescript".into();
        assert_eq!(
            validate(&config).unwrap_err().diagnostics[0].code,
            "LANGUAGE_UNSUPPORTED"
        );
        config.language = "python".into();
        config.version = "1.0".into();
        assert!(validate(&config).is_err());
        config.version = "1".into();
        config.modules[1].name = "A".into();
        assert!(validate(&config).is_err());
    }
}
