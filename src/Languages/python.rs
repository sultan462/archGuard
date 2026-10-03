use crate::DataModels::Models::{
    AnalysisOutcome, ArchGuardConfig, DependencyEdge, DependencyResolution, Diagnostic,
    FileOutcome, PipelineError, ProjectFile, SourceLocation,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use tree_sitter::{Node, Parser};

#[derive(Debug, PartialEq, Eq)]
enum ImportKind {
    Import(String),
    From { module: String, names: Vec<String> },
}
#[derive(Debug)]
struct ImportRequest {
    kind: ImportKind,
    line: usize,
    column: usize,
    statement: String,
}
struct ImportIndex<'a> {
    modules: BTreeMap<String, Vec<&'a ProjectFile>>,
    namespaces: BTreeSet<String>,
    project_tops: BTreeSet<String>,
    external: BTreeSet<String>,
    src_layout: bool,
}
#[derive(Debug)]
enum Resolution<'a> {
    File(&'a ProjectFile),
    Namespace,
    External,
    Unresolved(String),
}

/// Parse every non-ignored project file. Recoverable failures do not discard other findings.
pub fn analyze(config: &ArchGuardConfig) -> Result<AnalysisOutcome, PipelineError> {
    analyze_with_external(config, external_modules())
}

fn analyze_with_external(
    config: &ArchGuardConfig,
    external: BTreeSet<String>,
) -> Result<AnalysisOutcome, PipelineError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::language())
        .map_err(|error| {
            Diagnostic::error(
                "INTERNAL_ERROR",
                format!("Cannot initialize Python parser: {error}"),
                None,
            )
        })?;
    let index = ImportIndex::new(&config.project_files, external);
    let mut outcome = AnalysisOutcome::default();
    for file in config.project_files.iter().filter(|file| !file.ignored) {
        let source = match std::fs::read_to_string(config.project_root.join(&file.path)) {
            Ok(source) => source,
            Err(error) => {
                outcome.files.push(FileOutcome {
                    file: file.path.clone(),
                    succeeded: false,
                });
                outcome.diagnostics.push(Diagnostic::error(
                    "FILE_READ_FAILED",
                    format!("Cannot read Python source: {error}"),
                    Some(location(&file.path, None, None)),
                ));
                continue;
            }
        };
        let Some(tree) = parser.parse(&source, None) else {
            outcome.files.push(FileOutcome {
                file: file.path.clone(),
                succeeded: false,
            });
            outcome.diagnostics.push(Diagnostic::error(
                "PARSE_FAILED",
                "Python parser returned no syntax tree",
                Some(location(&file.path, None, None)),
            ));
            continue;
        };
        if tree.root_node().has_error() {
            let point = first_error(tree.root_node()).start_position();
            outcome.files.push(FileOutcome {
                file: file.path.clone(),
                succeeded: false,
            });
            outcome.diagnostics.push(Diagnostic::error(
                "PARSE_FAILED",
                "Python source contains invalid or unsupported syntax",
                Some(location(
                    &file.path,
                    Some(point.row + 1),
                    Some(point.column + 1),
                )),
            ));
            // Do not claim dependencies extracted from an unreliable syntax tree.
            continue;
        }
        outcome.files.push(FileOutcome {
            file: file.path.clone(),
            succeeded: true,
        });
        let mut requests = Vec::new();
        extract_imports(tree.root_node(), &source, &mut requests);
        let mut seen_targets = BTreeSet::new();
        let mut seen_unresolved = BTreeSet::new();
        for request in requests {
            for resolution in index.resolve(&request, &file.path) {
                let (resolution_kind, target, reason) = match resolution {
                    Resolution::File(target) => (DependencyResolution::Project, Some(target), None),
                    Resolution::Namespace => (DependencyResolution::Project, None, None),
                    Resolution::External => (DependencyResolution::External, None, None),
                    Resolution::Unresolved(reason) => {
                        (DependencyResolution::Unresolved, None, Some(reason))
                    }
                };
                if let Some(reason) = reason {
                    if !seen_unresolved.insert((request.line, request.column, reason.clone())) {
                        continue;
                    }
                    outcome.diagnostics.push(Diagnostic::error(
                        "IMPORT_UNRESOLVED",
                        reason,
                        Some(location(
                            &file.path,
                            Some(request.line),
                            Some(request.column),
                        )),
                    ));
                }
                if let Some(target) = target
                    && !seen_targets.insert((request.line, request.column, target.path.clone()))
                {
                    continue;
                }
                outcome.dependencies.push(DependencyEdge {
                    source_module: file.module.clone(),
                    source_file: file.path.clone(),
                    target_module: target.and_then(|target| target.module.clone()),
                    target_file: target.map(|target| target.path.clone()),
                    line: request.line,
                    column: request.column,
                    statement: request.statement.clone(),
                    resolution: resolution_kind,
                    target_ignored: target.is_some_and(|target| target.ignored),
                    is_allowed: true,
                    failed_constraints: Vec::new(),
                });
            }
        }
    }
    Ok(outcome)
}

fn location(path: &Path, line: Option<usize>, column: Option<usize>) -> SourceLocation {
    SourceLocation {
        file: path.to_string_lossy().replace('\\', "/"),
        line,
        column,
    }
}
fn first_error(node: Node<'_>) -> Node<'_> {
    if node.is_error() || node.is_missing() {
        return node;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.has_error() || child.is_missing() {
            return first_error(child);
        }
    }
    node
}
fn node_text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}
fn imported_name(node: Node<'_>, source: &str) -> String {
    let name = node.child_by_field_name("name").unwrap_or(node);
    // Python permits whitespace around dots; the AST identifies the name boundaries.
    node_text(name, source)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}
fn extract_imports(node: Node<'_>, source: &str, output: &mut Vec<ImportRequest>) {
    let point = node.start_position();
    let make = |kind| ImportRequest {
        kind,
        line: point.row + 1,
        // Tree-sitter columns are byte offsets. Reports use character columns.
        column: source[..node.start_byte()]
            .rsplit('\n')
            .next()
            .unwrap_or("")
            .chars()
            .count()
            + 1,
        statement: node_text(node, source).to_owned(),
    };
    match node.kind() {
        "import_statement" => {
            let mut cursor = node.walk();
            for name in node.children_by_field_name("name", &mut cursor) {
                output.push(make(ImportKind::Import(imported_name(name, source))));
            }
        }
        "import_from_statement" | "future_import_statement" => {
            let module = node
                .child_by_field_name("module_name")
                .map(|module| imported_name(module, source))
                .unwrap_or_else(|| "__future__".into());
            let mut cursor = node.walk();
            let mut names: Vec<_> = node
                .children_by_field_name("name", &mut cursor)
                .map(|name| imported_name(name, source))
                .collect();
            if names.is_empty() {
                names.push("*".into());
            }
            output.push(make(ImportKind::From { module, names }));
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                extract_imports(child, source, output);
            }
        }
    }
}

impl<'a> ImportIndex<'a> {
    fn new(files: &'a [ProjectFile], external: BTreeSet<String>) -> Self {
        let mut index = Self {
            modules: BTreeMap::new(),
            namespaces: BTreeSet::new(),
            project_tops: BTreeSet::new(),
            external,
            src_layout: !files
                .iter()
                .any(|file| file.path == Path::new("src/__init__.py")),
        };
        for file in files {
            let mut paths = vec![file.path.as_path()];
            if index.src_layout
                && let Ok(path) = file.path.strip_prefix("src")
            {
                paths.push(path);
            }
            for path in paths {
                let stem = path.with_extension("");
                let mut parts: Vec<_> = stem
                    .iter()
                    .map(|part| part.to_string_lossy().into_owned())
                    .collect();
                if parts.last().is_some_and(|part| part == "__init__") {
                    parts.pop();
                }
                if parts.is_empty() {
                    continue;
                }
                index.project_tops.insert(parts[0].clone());
                let name = parts.join(".");
                let targets = index.modules.entry(name).or_default();
                if !targets.iter().any(|target| target.path == file.path) {
                    targets.push(file);
                }
                for length in 1..parts.len() {
                    index.namespaces.insert(parts[..length].join("."));
                }
            }
        }
        index
    }
    fn lookup(&self, module: &str) -> Resolution<'a> {
        // A concrete module cannot contain submodules, and ambiguous parent packages
        // make their children ambiguous too. Do not guess Python's runtime search order.
        for (offset, _) in module.match_indices('.') {
            let parent = &module[..offset];
            if let Some(targets) = self.modules.get(parent)
                && (targets.len() != 1
                    || targets[0]
                        .path
                        .file_name()
                        .is_none_or(|name| name != "__init__.py"))
            {
                return Resolution::Unresolved(format!(
                    "Import '{module}' has an ambiguous or non-package parent '{parent}'"
                ));
            }
        }
        if let Some(targets) = self.modules.get(module) {
            if targets.len() == 1 {
                return Resolution::File(targets[0]);
            }
            return Resolution::Unresolved(format!(
                "Import '{module}' matches multiple project files"
            ));
        }
        if self.namespaces.contains(module) {
            return Resolution::Namespace;
        }
        let top = module.split('.').next().unwrap_or(module);
        if !self.project_tops.contains(top) && self.external.contains(module) {
            return Resolution::External;
        }
        Resolution::Unresolved(format!(
            "Cannot reliably resolve import '{module}' to a project file or confirmed external module"
        ))
    }
    fn resolve(&self, request: &ImportRequest, source: &Path) -> Vec<Resolution<'a>> {
        match &request.kind {
            ImportKind::Import(module) => vec![self.lookup(module)],
            ImportKind::From { module, names } => {
                let relative = module.starts_with('.');
                let absolute = if relative {
                    let level = module.chars().take_while(|c| *c == '.').count();
                    let source = if self.src_layout {
                        source.strip_prefix("src").unwrap_or(source)
                    } else {
                        source
                    };
                    let mut package: Vec<_> = source
                        .parent()
                        .unwrap_or(Path::new(""))
                        .iter()
                        .map(|part| part.to_string_lossy().into_owned())
                        .collect();
                    if level > package.len() {
                        return vec![Resolution::Unresolved(format!(
                            "Relative import '{module}' escapes its package"
                        ))];
                    }
                    package.truncate(package.len() + 1 - level);
                    let suffix = &module[level..];
                    if !suffix.is_empty() {
                        package.push(suffix.to_owned());
                    }
                    package.join(".")
                } else {
                    module.clone()
                };
                names
                    .iter()
                    .map(|name| {
                        let candidate = format!("{absolute}.{name}");
                        if name != "*"
                            && (self.modules.contains_key(&candidate)
                                || self.namespaces.contains(&candidate))
                        {
                            return self.lookup(&candidate);
                        }
                        match self.lookup(&absolute) {
                            Resolution::Namespace => Resolution::Unresolved(format!(
                                "Namespace package '{absolute}' has no resolvable member '{name}'"
                            )),
                            Resolution::External if relative => Resolution::Unresolved(format!(
                                "Relative import '{module}' has no project target"
                            )),
                            other => other,
                        }
                    })
                    .collect()
            }
        }
    }
}

/// Inspect the interpreter's library paths without importing any target package or project code.
/// -I excludes cwd/PYTHONPATH and -S prevents site startup hooks. A missing interpreter simply
/// leaves unknown imports unresolved. Do not infer that every non-project name is external.
fn external_modules() -> BTreeSet<String> {
    const SCRIPT: &str = r#"
import json, os, sys, sysconfig
names = set(sys.builtin_module_names)
names.update(getattr(sys, 'stdlib_module_names', ()))
roots = set()
for key in ('stdlib', 'platstdlib', 'purelib', 'platlib'):
    value = sysconfig.get_path(key)
    if value:
        roots.add(value)
# Python <3.14 resets a virtual environment's prefix under -S. Inspect its standard
# site-packages directory directly, without processing .pth files or startup hooks.
executable_prefix = os.path.dirname(os.path.dirname(sys.executable))
if os.path.isfile(os.path.join(executable_prefix, 'pyvenv.cfg')):
    roots.add(os.path.join(executable_prefix, 'lib', 'python%d.%d' % sys.version_info[:2], 'site-packages'))
    roots.add(os.path.join(executable_prefix, 'Lib', 'site-packages'))
stdlib = sysconfig.get_path('stdlib')
if stdlib:
    roots.add(os.path.join(stdlib, 'lib-dynload'))
for root in sorted(roots):
    if not os.path.isdir(root):
        continue
    for directory, dirs, files in os.walk(root):
        dirs[:] = sorted(d for d in dirs if d.isidentifier() and d not in ('__pycache__', 'site_packages', 'test', 'tests'))
        relative = os.path.relpath(directory, root)
        prefix = '' if relative == '.' else relative.replace(os.sep, '.') + '.'
        for filename in files:
            if filename.endswith(('.py', '.so', '.pyd')):
                stem = filename.split('.')[0]
                if stem == '__init__':
                    if prefix:
                        names.add(prefix[:-1])
                elif stem.isidentifier():
                    names.add(prefix + stem)
# File-backed descendants also establish external namespace-package containers.
for name in list(names):
    parts = name.split('.')
    for length in range(1, len(parts)):
        names.add('.'.join(parts[:length]))
print(json.dumps(sorted(names)))
"#;
    for executable in ["python3", "python"] {
        if let Ok(output) = Command::new(executable)
            .args(["-I", "-S", "-c", SCRIPT])
            .output()
            && output.status.success()
            && let Ok(names) = serde_json::from_slice(&output.stdout)
        {
            return names;
        }
    }
    BTreeSet::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn requests(source: &str) -> Vec<ImportRequest> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::language())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let mut result = Vec::new();
        extract_imports(tree.root_node(), source, &mut result);
        result
    }
    fn config(root: &Path, files: &[(&str, Option<&str>, bool)]) -> ArchGuardConfig {
        ArchGuardConfig {
            version: "1".into(),
            name: None,
            language: "python".into(),
            modules: vec![],
            rules: None,
            ignore: None,
            config_path: root.join("archguard.yaml"),
            project_root: root.to_owned(),
            project_files: files
                .iter()
                .map(|(path, module, ignored)| ProjectFile {
                    path: (*path).into(),
                    module: module.map(str::to_owned),
                    ignored: *ignored,
                })
                .collect(),
        }
    }
    fn put(root: &Path, path: &str, source: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    #[test]
    fn ast_extracts_aliases_lists_multiline_relative_nested_and_future_imports() {
        let imports = requests(
            "# import fake\ntext = 'import fake'\nimport a.b as c, d\nfrom ..pkg import (\n    x as y,\n    z,\n)\ndef f():\n    import os\nfrom __future__ import annotations\nfrom pkg import *\n",
        );
        assert_eq!(imports.len(), 6);
        assert_eq!(imports[0].kind, ImportKind::Import("a.b".into()));
        assert_eq!(imports[1].kind, ImportKind::Import("d".into()));
        assert_eq!(
            imports[2].kind,
            ImportKind::From {
                module: "..pkg".into(),
                names: vec!["x".into(), "z".into()]
            }
        );
        assert_eq!((imports[2].line, imports[2].column), (4, 1));
        assert_eq!((imports[3].line, imports[3].column), (9, 5));
        assert!(imports[2].statement.contains("x as y"));
        assert_eq!(
            imports[4].kind,
            ImportKind::From {
                module: "__future__".into(),
                names: vec!["annotations".into()]
            }
        );
    }
    #[test]
    fn resolution_keeps_project_targets_outside_modules_and_deduplicates_occurrences() {
        let temp = TempDir::new().unwrap();
        put(
            temp.path(),
            "src/app/main.py",
            "import domain.model as model\nfrom domain.model import X, Y\nfrom . import helper\nimport loose\nimport ignored\nimport os\nimport missing\n",
        );
        for path in [
            "src/domain/model.py",
            "src/app/helper.py",
            "loose.py",
            "ignored.py",
        ] {
            put(temp.path(), path, "");
        }
        let config = config(
            temp.path(),
            &[
                ("src/app/main.py", Some("app"), false),
                ("src/domain/model.py", Some("domain"), false),
                ("src/app/helper.py", Some("app"), false),
                ("loose.py", None, false),
                ("ignored.py", None, true),
            ],
        );
        let output = analyze_with_external(&config, BTreeSet::from(["os".into()])).unwrap();
        assert_eq!(output.files.len(), 4);
        assert!(output.files.iter().all(|file| file.succeeded));
        assert_eq!(output.dependencies.len(), 7);
        assert_eq!(
            output.dependencies[1].target_file.as_deref(),
            Some(Path::new("src/domain/model.py"))
        );
        assert_eq!(
            output.dependencies[2].target_file.as_deref(),
            Some(Path::new("src/app/helper.py"))
        );
        assert_eq!(
            output.dependencies[3].resolution,
            DependencyResolution::Project
        );
        assert!(output.dependencies[3].target_module.is_none());
        assert!(output.dependencies[4].target_ignored);
        assert_eq!(
            output.dependencies[5].resolution,
            DependencyResolution::External
        );
        assert_eq!(output.diagnostics.len(), 1);
        assert_eq!(output.diagnostics[0].code, "IMPORT_UNRESOLVED");
    }
    #[test]
    fn package_imports_ambiguity_and_relative_escape() {
        let files = vec![
            ProjectFile {
                path: "pkg/__init__.py".into(),
                module: Some("pkg".into()),
                ignored: false,
            },
            ProjectFile {
                path: "pkg/sub.py".into(),
                module: Some("pkg".into()),
                ignored: false,
            },
            ProjectFile {
                path: "duplicate.py".into(),
                module: None,
                ignored: false,
            },
            ProjectFile {
                path: "src/duplicate.py".into(),
                module: None,
                ignored: false,
            },
        ];
        let index = ImportIndex::new(&files, BTreeSet::from(["pkg.missing".into()]));
        assert!(matches!(index.lookup("pkg"), Resolution::File(_)));
        assert!(matches!(
            index.lookup("pkg.missing"),
            Resolution::Unresolved(_)
        ));
        assert!(matches!(
            index.lookup("duplicate"),
            Resolution::Unresolved(_)
        ));
        let imports = requests("from pkg import sub\nfrom .. import sub\n");
        assert!(
            matches!(index.resolve(&imports[0], Path::new("main.py"))[0], Resolution::File(file) if file.path == Path::new("pkg/sub.py"))
        );
        assert!(matches!(
            index.resolve(&imports[1], Path::new("pkg/main.py"))[0],
            Resolution::Unresolved(_)
        ));
    }

    #[test]
    fn regular_src_package_and_non_package_parents_are_distinguished() {
        let files = vec![
            ProjectFile {
                path: "src/__init__.py".into(),
                module: Some("source".into()),
                ignored: false,
            },
            ProjectFile {
                path: "src/helper.py".into(),
                module: Some("source".into()),
                ignored: false,
            },
            ProjectFile {
                path: "module.py".into(),
                module: None,
                ignored: false,
            },
            ProjectFile {
                path: "module/child.py".into(),
                module: None,
                ignored: false,
            },
        ];
        let index = ImportIndex::new(&files, BTreeSet::new());
        let import = requests("from . import helper\n");
        assert!(
            matches!(index.resolve(&import[0], Path::new("src/__init__.py"))[0], Resolution::File(file) if file.path == Path::new("src/helper.py"))
        );
        assert!(matches!(index.lookup("helper"), Resolution::Unresolved(_)));
        assert!(matches!(
            index.lookup("module.child"),
            Resolution::Unresolved(_)
        ));
    }

    #[test]
    fn external_names_require_exact_evidence_and_project_names_take_precedence() {
        let files = vec![ProjectFile {
            path: "requests.py".into(),
            module: None,
            ignored: false,
        }];
        let index = ImportIndex::new(
            &files,
            BTreeSet::from([
                "requests".into(),
                "requests.sessions".into(),
                "thirdparty.client".into(),
                "os".into(),
            ]),
        );
        assert!(matches!(index.lookup("requests"), Resolution::File(_)));
        assert!(matches!(
            index.lookup("requests.sessions"),
            Resolution::Unresolved(_)
        ));
        assert!(matches!(
            index.lookup("thirdparty.client"),
            Resolution::External
        ));
        assert!(matches!(
            index.lookup("thirdparty.missing"),
            Resolution::Unresolved(_)
        ));
        assert!(matches!(index.lookup("os"), Resolution::External));
    }
    #[test]
    fn read_and_parse_failures_preserve_other_files() {
        let temp = TempDir::new().unwrap();
        put(temp.path(), "ok.py", "import target\n");
        put(temp.path(), "target.py", "");
        put(temp.path(), "broken.py", "def broken(:\n");
        let config = config(
            temp.path(),
            &[
                ("ok.py", Some("a"), false),
                ("target.py", Some("b"), false),
                ("missing.py", Some("a"), false),
                ("broken.py", Some("a"), false),
            ],
        );
        let output = analyze_with_external(&config, BTreeSet::new()).unwrap();
        assert_eq!(output.files.iter().filter(|file| file.succeeded).count(), 2);
        assert_eq!(
            output.files.iter().filter(|file| !file.succeeded).count(),
            2
        );
        assert_eq!(output.dependencies.len(), 1);
        assert_eq!(
            output
                .diagnostics
                .iter()
                .map(|d| d.code.as_str())
                .collect::<Vec<_>>(),
            ["FILE_READ_FAILED", "PARSE_FAILED"]
        );
    }
}
