use crate::DataModels::Models::{
    ArchGuardConfig, Diagnostic, DiscoveryOutcome, PipelineError, ProjectFile, SourceLocation,
};
use globset::{GlobBuilder, GlobMatcher};
use walkdir::WalkDir;

pub fn files_getter(config: &mut ArchGuardConfig) -> Result<DiscoveryOutcome, PipelineError> {
    let matchers = config
        .modules
        .iter()
        .map(|module| matcher(&module.path))
        .collect::<Result<Vec<_>, _>>()?;
    let ignored = config
        .ignore
        .iter()
        .flatten()
        .map(|pattern| matcher(pattern))
        .collect::<Result<Vec<_>, _>>()?;
    for module in &mut config.modules {
        module.files.clear();
    }
    config.project_files.clear();
    let mut files = Vec::new();
    for entry in WalkDir::new(&config.project_root)
        .follow_links(true)
        .sort_by_file_name()
    {
        let entry = entry.map_err(|error| {
            Diagnostic::error(
                "INTERNAL_ERROR",
                format!("Cannot enumerate project source files: {error}"),
                error
                    .path()
                    .and_then(|path| path.strip_prefix(&config.project_root).ok())
                    .map(|path| SourceLocation {
                        file: path.to_string_lossy().replace('\\', "/"),
                        line: None,
                        column: None,
                    }),
            )
        })?;
        if entry.file_type().is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension == source_extension(&config.language))
        {
            let relative = entry
                .path()
                .strip_prefix(&config.project_root)
                .map_err(|error| Diagnostic::error("INTERNAL_ERROR", error.to_string(), None))?;
            files.push(relative.to_owned());
        }
    }
    files.sort();
    let mut outcome = DiscoveryOutcome::default();
    let mut diagnostics = Vec::new();
    for path in files {
        let label = path.to_string_lossy().replace('\\', "/");
        if ignored.iter().any(|matcher| matcher.is_match(&path)) {
            outcome.files_ignored += 1;
            config.project_files.push(ProjectFile {
                path,
                module: None,
                ignored: true,
            });
            continue;
        }
        let memberships: Vec<_> = matchers
            .iter()
            .enumerate()
            .filter_map(|(index, matcher)| matcher.is_match(&path).then_some(index))
            .collect();
        let module = match memberships.as_slice() {
            [] => {
                outcome.files_unassigned += 1;
                outcome.unassigned_files.push(label.clone());
                None
            }
            [index] => {
                outcome.files_assigned += 1;
                config.modules[*index].files.push(path.clone());
                Some(config.modules[*index].name.clone())
            }
            _ => {
                diagnostics.push(Diagnostic::error(
                    "MODULE_OVERLAP",
                    format!(
                        "File matches multiple modules: {}",
                        memberships
                            .iter()
                            .map(|index| config.modules[*index].name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    Some(SourceLocation {
                        file: label,
                        line: None,
                        column: None,
                    }),
                ));
                None
            }
        };
        config.project_files.push(ProjectFile {
            path,
            module,
            ignored: false,
        });
    }
    outcome.empty_modules = config
        .modules
        .iter()
        .filter(|module| module.files.is_empty())
        .map(|module| module.name.clone())
        .collect();
    outcome.empty_modules.sort();
    if diagnostics.is_empty() {
        Ok(outcome)
    } else {
        Err(PipelineError { diagnostics })
    }
}
fn source_extension(language: &str) -> &'static str {
    if language.eq_ignore_ascii_case("dart") {
        "dart"
    } else {
        "py"
    }
}

fn matcher(pattern: &str) -> Result<GlobMatcher, PipelineError> {
    let mut pattern = pattern;
    while let Some(rest) = pattern.strip_prefix("./") {
        pattern = rest;
    }
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| {
            Diagnostic::error(
                "CONFIG_INVALID",
                format!("Invalid glob '{pattern}': {error}"),
                None,
            )
            .into()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn glob_semantics_and_ignore_before_overlap() {
        let temp = tempfile::TempDir::new().unwrap();
        for file in [
            "src/domain/a.py",
            "src/domain/nested/b.py",
            "src/domain/ignored.py",
            "src/domain/no.txt",
            "gap.py",
        ] {
            let path = temp.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let mut config: ArchGuardConfig = serde_yaml::from_str("version: '1'\nlanguage: python\nmodules:\n  - {name: domain, path: 'src/domain/**/*.py'}\n  - {name: overlap, path: 'src/domain/ignored.py'}\n  - {name: empty, path: 'missing/**'}\nignore: ['src/domain/ignored.py']").unwrap();
        config.project_root = temp.path().to_owned();
        let output = files_getter(&mut config).unwrap();
        assert_eq!(
            (
                output.files_assigned,
                output.files_unassigned,
                output.files_ignored
            ),
            (2, 1, 1)
        );
        assert_eq!(output.unassigned_files, ["gap.py"]);
        assert_eq!(output.empty_modules, ["empty", "overlap"]);
        assert!(
            matcher("src/domain/*.py")
                .unwrap()
                .is_match("src/domain/a.py")
        );
        assert!(
            !matcher("src/domain/*.py")
                .unwrap()
                .is_match("src/domain/nested/b.py")
        );
        assert!(
            matcher("src/domain/**")
                .unwrap()
                .is_match("src/domain/nested/b.py")
        );
        config.ignore = None;
        assert_eq!(
            files_getter(&mut config).unwrap_err().diagnostics[0].code,
            "MODULE_OVERLAP"
        );
    }
}
