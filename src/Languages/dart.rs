//! Static Dart dependency analysis. No Dart executable or project code is invoked.
use crate::DataModels::Models::{
    AnalysisOutcome, ArchGuardConfig, DependencyEdge, DependencyResolution, Diagnostic,
    FileOutcome, PipelineError, ProjectFile, SourceLocation,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use tree_sitter::{Node, Parser};

type PackageMap = BTreeMap<String, PathBuf>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectiveKind {
    Import,
    Export,
    Part,
    PartOf,
}

#[derive(Debug)]
struct Directive {
    kind: DirectiveKind,
    uris: Vec<Result<String, String>>,
    named_owner: Option<String>,
    line: usize,
    column: usize,
    statement: String,
}

struct Unit<'a> {
    file: &'a ProjectFile,
    library_name: Option<String>,
    directives: Vec<Directive>,
}

#[derive(Debug)]
enum Resolution<'a> {
    Project(&'a ProjectFile),
    External,
    Unresolved(String),
}

struct Resolver<'a> {
    config: &'a ArchGuardConfig,
    root: PathBuf,
    lexical_files: BTreeMap<PathBuf, &'a ProjectFile>,
    canonical_files: BTreeMap<PathBuf, Vec<&'a ProjectFile>>,
    package_maps: BTreeMap<PathBuf, Result<PackageMap, String>>,
    pubspecs: BTreeMap<PathBuf, Result<serde_yaml::Value, String>>,
}

/// Return the existing language-independent analysis outcome, including recoverable failures.
pub fn analyze(config: &ArchGuardConfig) -> Result<AnalysisOutcome, PipelineError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_dart::language())
        .map_err(|error| {
            Diagnostic::error(
                "INTERNAL_ERROR",
                format!("Cannot initialize Dart parser: {error}"),
                None,
            )
        })?;
    let mut outcome = AnalysisOutcome::default();
    let mut units = Vec::new();
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
                    format!("Cannot read Dart source: {error}"),
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
                "Dart parser returned no syntax tree",
                Some(location(&file.path, None, None)),
            ));
            continue;
        };
        if tree.root_node().has_error() {
            let error = first_error(tree.root_node());
            outcome.files.push(FileOutcome {
                file: file.path.clone(),
                succeeded: false,
            });
            outcome.diagnostics.push(Diagnostic::error("PARSE_FAILED", "Dart source contains invalid syntax or syntax unsupported by the compatible Tree-sitter grammar", Some(location(&file.path, Some(error.start_position().row + 1), Some(character_column(&source, error.start_byte()))))));
            continue;
        }
        outcome.files.push(FileOutcome {
            file: file.path.clone(),
            succeeded: true,
        });
        let mut unit = Unit {
            file,
            library_name: None,
            directives: Vec::new(),
        };
        extract(tree.root_node(), &source, &mut unit);
        units.push(unit);
    }
    let mut resolver = Resolver::new(config);
    let mut part_links = BTreeSet::new();
    let mut named_libraries: BTreeMap<String, Vec<&ProjectFile>> = BTreeMap::new();
    for unit in &units {
        if let Some(name) = &unit.library_name {
            named_libraries
                .entry(name.clone())
                .or_default()
                .push(unit.file);
        }
        for directive in &unit.directives {
            if directive.kind == DirectiveKind::PartOf {
                continue;
            }
            let resolution = resolver.directive(directive, &unit.file.path);
            if directive.kind == DirectiveKind::Part
                && let Resolution::Project(target) = &resolution
            {
                part_links.insert((unit.file.path.clone(), target.path.clone()));
            }
            record(&mut outcome, unit.file, directive, resolution);
        }
    }
    // `part of` declares membership; it is not a reverse import or a transitive edge.
    // Validate its owner against the explicit inclusion without manufacturing a dependency.
    for unit in &units {
        for directive in unit
            .directives
            .iter()
            .filter(|d| d.kind == DirectiveKind::PartOf)
        {
            let owner = if let Some(name) = &directive.named_owner {
                match named_libraries.get(name).map(Vec::as_slice) {
                    Some([owner]) => Resolution::Project(owner),
                    Some(_) => Resolution::Unresolved(format!(
                        "Named part owner '{name}' matches multiple libraries"
                    )),
                    None => Resolution::Unresolved(format!(
                        "Cannot resolve named part owner '{name}' to an analyzed library"
                    )),
                }
            } else {
                resolver.directive(directive, &unit.file.path)
            };
            let problem = match owner {
                Resolution::Project(owner)
                    if owner.ignored
                        || part_links.contains(&(owner.path.clone(), unit.file.path.clone())) =>
                {
                    None
                }
                Resolution::Project(_) => Some(
                    "The part's owner does not have a resolved part directive including this file"
                        .into(),
                ),
                Resolution::External => Some(
                    "Cannot verify part ownership in an external library without analyzing it"
                        .into(),
                ),
                Resolution::Unresolved(message) => Some(message),
            };
            if let Some(message) = problem {
                record(
                    &mut outcome,
                    unit.file,
                    directive,
                    Resolution::Unresolved(message),
                );
            }
        }
    }
    Ok(outcome)
}

fn location(file: &Path, line: Option<usize>, column: Option<usize>) -> SourceLocation {
    SourceLocation {
        file: file.to_string_lossy().replace('\\', "/"),
        line,
        column,
    }
}

fn record(
    outcome: &mut AnalysisOutcome,
    source: &ProjectFile,
    directive: &Directive,
    resolution: Resolution<'_>,
) {
    let (kind, target) = match resolution {
        Resolution::Project(target) => (DependencyResolution::Project, Some(target)),
        Resolution::External => (DependencyResolution::External, None),
        Resolution::Unresolved(message) => {
            outcome.diagnostics.push(Diagnostic::error(
                "IMPORT_UNRESOLVED",
                message,
                Some(location(
                    &source.path,
                    Some(directive.line),
                    Some(directive.column),
                )),
            ));
            (DependencyResolution::Unresolved, None)
        }
    };
    outcome.dependencies.push(DependencyEdge {
        source_module: source.module.clone(),
        source_file: source.path.clone(),
        target_module: target.and_then(|target| target.module.clone()),
        target_file: target.map(|target| target.path.clone()),
        line: directive.line,
        column: directive.column,
        statement: directive.statement.clone(),
        resolution: kind,
        target_ignored: target.is_some_and(|target| target.ignored),
        is_allowed: true,
        failed_constraints: Vec::new(),
    });
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

fn character_column(source: &str, offset: usize) -> usize {
    source[..offset]
        .rsplit('\n')
        .next()
        .unwrap_or("")
        .chars()
        .count()
        + 1
}

fn descendant<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    if node.kind() == kind {
        return Some(node);
    }
    if matches!(node.kind(), "annotation" | "marker_annotation") {
        return None;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find_map(|child| descendant(child, kind))
}

fn extract(node: Node<'_>, source: &str, unit: &mut Unit<'_>) {
    let kind = match node.kind() {
        "library_import" => Some((DirectiveKind::Import, "import")),
        "library_export" => Some((DirectiveKind::Export, "export")),
        "part_directive" => Some((DirectiveKind::Part, "part")),
        "part_of_directive" => Some((DirectiveKind::PartOf, "part")),
        _ => None,
    };
    if let Some((kind, keyword)) = kind {
        let start = descendant(node, keyword).unwrap_or(node);
        let mut uris = Vec::new();
        collect_uris(node, source, &mut uris);
        let named_owner = if kind == DirectiveKind::PartOf && uris.is_empty() {
            descendant(node, "dotted_identifier_list").map(|name| dotted_name(name, source))
        } else {
            None
        };
        unit.directives.push(Directive {
            kind,
            uris,
            named_owner,
            line: start.start_position().row + 1,
            column: character_column(source, start.start_byte()),
            statement: source[start.start_byte()..node.end_byte()].to_owned(),
        });
        return;
    }
    if node.kind() == "library_name" {
        unit.library_name =
            descendant(node, "dotted_identifier_list").map(|name| dotted_name(name, source));
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        extract(child, source, unit);
    }
}

fn dotted_name(node: Node<'_>, source: &str) -> String {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() == "identifier")
        .map(|child| &source[child.byte_range()])
        .collect::<Vec<_>>()
        .join(".")
}

fn collect_uris(node: Node<'_>, source: &str, uris: &mut Vec<Result<String, String>>) {
    if matches!(node.kind(), "annotation" | "marker_annotation") {
        return;
    }
    if node.kind() == "uri" {
        uris.push(if descendant(node, "template_substitution").is_some() {
            Err("Interpolated dependency URIs cannot be resolved statically".into())
        } else {
            decode_string(&source[node.byte_range()])
        });
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_uris(child, source, uris);
    }
}

impl<'a> Resolver<'a> {
    fn new(config: &'a ArchGuardConfig) -> Self {
        let root = normalize(&config.project_root);
        let mut lexical_files = BTreeMap::new();
        let mut canonical_files: BTreeMap<PathBuf, Vec<&ProjectFile>> = BTreeMap::new();
        for file in &config.project_files {
            let absolute = normalize(&root.join(&file.path));
            if let Ok(canonical) = absolute.canonicalize() {
                canonical_files.entry(canonical).or_default().push(file);
            }
            lexical_files.insert(absolute, file);
        }
        Self {
            config,
            root,
            lexical_files,
            canonical_files,
            package_maps: BTreeMap::new(),
            pubspecs: BTreeMap::new(),
        }
    }

    fn directive(&mut self, directive: &Directive, source: &Path) -> Resolution<'a> {
        if directive.uris.is_empty() {
            return Resolution::Unresolved("Dependency directive has no supported URI".into());
        }
        let mut resolutions = Vec::new();
        for uri in &directive.uris {
            let uri = match uri {
                Ok(uri) => uri,
                Err(message) => return Resolution::Unresolved(message.clone()),
            };
            let resolved = self.uri(uri, source);
            if let Resolution::Unresolved(_) = resolved {
                return resolved;
            }
            resolutions.push(resolved);
        }
        let first = resolutions.remove(0);
        if resolutions.iter().all(|other| match (&first, other) {
            (Resolution::External, Resolution::External) => true,
            (Resolution::Project(a), Resolution::Project(b)) => a.path == b.path,
            _ => false,
        }) {
            first
        } else {
            Resolution::Unresolved("Conditional dependency has different possible targets; the Dart compilation environment is unknown, so no branch was selected".into())
        }
    }

    fn uri(&mut self, uri: &str, source: &Path) -> Resolution<'a> {
        if let Some(library) = uri.strip_prefix("dart:") {
            return if sdk_library(library) {
                Resolution::External
            } else {
                Resolution::Unresolved(format!("Unknown or unsupported Dart SDK library '{uri}'"))
            };
        }
        if let Some(package_path) = uri.strip_prefix("package:") {
            let Some((package, path)) = package_path.split_once('/') else {
                return Resolution::Unresolved(format!(
                    "Package URI '{uri}' must include a library path"
                ));
            };
            if !valid_package_name(package) {
                return Resolution::Unresolved(format!("Invalid package name in '{uri}'"));
            }
            let base = match self.package_root(source, package) {
                Ok(base) => base,
                Err(error) => {
                    return Resolution::Unresolved(format!("Cannot resolve '{uri}': {error}"));
                }
            };
            let target = match local_uri(&base, path) {
                Ok(target)
                    if target.starts_with(&base)
                        && !path.starts_with('/')
                        && !path.contains(':') =>
                {
                    target
                }
                Ok(_) => {
                    return Resolution::Unresolved(format!(
                        "Package URI '{uri}' escapes its package library directory"
                    ));
                }
                Err(error) => {
                    return Resolution::Unresolved(format!("Cannot resolve '{uri}': {error}"));
                }
            };
            return self.file(&target, true);
        }
        let base = self
            .root
            .join(source)
            .parent()
            .unwrap_or(&self.root)
            .to_owned();
        match local_uri(&base, uri) {
            Ok(target) => self.file(&target, false),
            Err(error) => Resolution::Unresolved(format!("Cannot resolve '{uri}': {error}")),
        }
    }

    fn file(&self, target: &Path, package_evidence: bool) -> Resolution<'a> {
        if target
            .extension()
            .is_none_or(|extension| extension != "dart")
        {
            return Resolution::Unresolved("Dependency target is not a .dart source file".into());
        }
        if let Some(file) = self.lexical_files.get(target) {
            return Resolution::Project(file);
        }
        let canonical = match target.canonicalize() {
            Ok(path) if path.is_file() => path,
            Ok(_) => {
                return Resolution::Unresolved(format!(
                    "Dependency '{}' is not a file",
                    target.display()
                ));
            }
            Err(error) => {
                return Resolution::Unresolved(format!(
                    "Cannot locate dependency '{}': {error}",
                    target.display()
                ));
            }
        };
        match self.canonical_files.get(&canonical).map(Vec::as_slice) {
            Some([file]) => return Resolution::Project(file),
            Some(_) => return Resolution::Unresolved("Dependency has multiple project aliases with potentially different module membership".into()),
            None => {}
        }
        if target.starts_with(&self.root)
            || canonical.starts_with(
                self.root
                    .canonicalize()
                    .unwrap_or_else(|_| self.root.clone()),
            )
        {
            return Resolution::Unresolved(
                "Dependency is inside the project but was absent from file discovery".into(),
            );
        }
        if package_evidence {
            Resolution::External
        } else {
            Resolution::Unresolved(
                "Dependency is outside the discovered project and has no external package identity"
                    .into(),
            )
        }
    }

    fn package_root(&mut self, source: &Path, package: &str) -> Result<PathBuf, String> {
        let source_dir = self
            .config
            .project_root
            .join(source)
            .parent()
            .unwrap_or(&self.root)
            .to_owned();
        if let Some(path) = nearest(&source_dir, ".dart_tool/package_config.json")? {
            let packages = self
                .package_maps
                .entry(path.clone())
                .or_insert_with(|| load_package_map(&path));
            return packages
                .as_ref()
                .map_err(Clone::clone)?
                .get(package)
                .cloned()
                .ok_or_else(|| format!("Package '{package}' is absent from {}", path.display()));
        }
        // A pubspec identifies the package itself and explicit local path dependencies.
        // A hosted/git version declaration alone does not prove an external library exists.
        let pubspec = nearest(&source_dir, "pubspec.yaml")?
            .ok_or_else(|| "No package_config.json or pubspec.yaml was found".to_owned())?;
        let data = self
            .pubspecs
            .entry(pubspec.clone())
            .or_insert_with(|| {
                std::fs::read_to_string(&pubspec)
                    .map_err(|error| format!("Cannot read pubspec.yaml: {error}"))
                    .and_then(|source| {
                        serde_yaml::from_str(&source)
                            .map_err(|error| format!("Invalid pubspec.yaml: {error}"))
                    })
            })
            .as_ref()
            .map_err(Clone::clone)?;
        let directory = pubspec
            .parent()
            .ok_or("pubspec.yaml has no parent directory")?;
        if directory
            .join("pubspec_overrides.yaml")
            .try_exists()
            .map_err(|error| error.to_string())?
        {
            return Err("pubspec_overrides.yaml is present; package_config.json is required to resolve the effective package graph".into());
        }
        if data.get("name").and_then(serde_yaml::Value::as_str) == Some(package) {
            return Ok(normalize(&directory.join("lib")));
        }
        for section in ["dependency_overrides", "dependencies", "dev_dependencies"] {
            if let Some(dependency) = data.get(section).and_then(|section| section.get(package)) {
                let relative = dependency.get("path").and_then(serde_yaml::Value::as_str)
                    .ok_or_else(|| format!("Package '{package}' needs package_config.json to establish its installed location"))?;
                let package_dir = normalize(&directory.join(relative));
                let manifest =
                    std::fs::read_to_string(package_dir.join("pubspec.yaml")).map_err(|error| {
                        format!("Cannot verify path dependency '{package}': {error}")
                    })?;
                let manifest: serde_yaml::Value = serde_yaml::from_str(&manifest)
                    .map_err(|error| format!("Invalid path dependency pubspec.yaml: {error}"))?;
                if manifest.get("name").and_then(serde_yaml::Value::as_str) != Some(package) {
                    return Err(format!(
                        "Path dependency '{package}' has a different or missing package name"
                    ));
                }
                return Ok(package_dir.join("lib"));
            }
        }
        Err(format!(
            "No installed metadata or path dependency establishes package '{package}'"
        ))
    }
}

fn nearest(start: &Path, suffix: &str) -> Result<Option<PathBuf>, String> {
    for directory in start.ancestors() {
        let candidate = directory.join(suffix);
        match candidate.try_exists() {
            Ok(true) => return Ok(Some(candidate)),
            Ok(false) => {}
            Err(error) => {
                return Err(format!(
                    "Cannot inspect metadata '{}': {error}",
                    candidate.display()
                ));
            }
        }
    }
    Ok(None)
}

fn load_package_map(path: &Path) -> Result<PackageMap, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    let data: serde_json::Value = serde_json::from_str(&source)
        .map_err(|error| format!("Invalid package_config.json: {error}"))?;
    if data
        .get("configVersion")
        .and_then(serde_json::Value::as_u64)
        != Some(2)
    {
        return Err("Only package_config.json configVersion 2 is supported".into());
    }
    let entries = data
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or("package_config.json requires a packages array")?;
    let mut packages = BTreeMap::new();
    for entry in entries {
        let name = entry
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or("Package entry requires a string name")?;
        if !valid_package_name(name) {
            return Err(format!(
                "Invalid package name '{name}' in package_config.json"
            ));
        }
        let root_uri = entry
            .get("rootUri")
            .and_then(serde_json::Value::as_str)
            .ok_or("Package entry requires a string rootUri")?;
        let root = local_uri(
            path.parent().ok_or("Package config has no directory")?,
            root_uri,
        )?;
        let library_uri = match entry.get("packageUri") {
            None => "",
            Some(value) => value.as_str().ok_or("packageUri must be a string")?,
        };
        if library_uri.starts_with('/') || library_uri.contains(':') {
            return Err("packageUri must be relative to rootUri".into());
        }
        let library = local_uri(&root, library_uri)?;
        if !library.starts_with(&root) {
            return Err("packageUri escapes rootUri".into());
        }
        if packages.insert(name.to_owned(), library).is_some() {
            return Err(format!("Duplicate package '{name}' in package_config.json"));
        }
    }
    Ok(packages)
}

fn valid_package_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn sdk_library(name: &str) -> bool {
    // Public Dart and Flutter SDK library identities; unknown dart: names are not guessed.
    // https://api.dart.dev/ and https://api.flutter.dev/flutter/dart-ui/
    matches!(
        name,
        "async"
            | "cli"
            | "collection"
            | "concurrent"
            | "convert"
            | "core"
            | "developer"
            | "ffi"
            | "html"
            | "indexed_db"
            | "io"
            | "isolate"
            | "js"
            | "js_interop"
            | "js_interop_unsafe"
            | "js_util"
            | "math"
            | "mirrors"
            | "svg"
            | "typed_data"
            | "ui"
            | "ui_web"
            | "web_audio"
            | "web_gl"
            | "web_sql"
    )
}

fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// The supported URI subset is local file references, including percent-encoded UTF-8.
/// Other schemes, authorities, queries and fragments are diagnosed, never fetched.
fn local_uri(base: &Path, uri: &str) -> Result<PathBuf, String> {
    if uri.starts_with("//") {
        return Err("Network-path URI references are unsupported".into());
    }
    if uri.contains(['?', '#', '\\']) || uri.chars().any(char::is_control) {
        return Err(
            "URI queries, fragments, backslashes and control characters are unsupported".into(),
        );
    }
    let path = if let Some(file) = uri.strip_prefix("file:") {
        if let Some(authority) = file.strip_prefix("//") {
            if authority.starts_with('/') {
                authority
            } else if let Some(local) = authority.strip_prefix("localhost/") {
                // Keep the leading slash when stripping the localhost authority.
                &file[file.len() - local.len() - 1..]
            } else {
                return Err("Remote file URI authorities are unsupported".into());
            }
        } else {
            file
        }
    } else {
        if uri.split('/').next().is_some_and(|part| part.contains(':')) {
            return Err("Unsupported dependency URI scheme".into());
        }
        uri
    };
    let decoded = percent_decode(path)?;
    let path =
        if cfg!(windows) && decoded.starts_with('/') && decoded.as_bytes().get(2) == Some(&b':') {
            Path::new(&decoded[1..])
        } else {
            Path::new(&decoded)
        };
    if uri.starts_with("file:") && !path.is_absolute() {
        return Err("A file: URI must have an absolute local path".into());
    }
    Ok(normalize(&base.join(path)))
}

fn percent_decode(input: &str) -> Result<String, String> {
    let bytes = input.as_bytes();
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset] == b'%' {
            let digits = bytes
                .get(offset + 1..offset + 3)
                .ok_or("Incomplete URI percent escape")?;
            let digits = std::str::from_utf8(digits).map_err(|_| "Invalid URI percent escape")?;
            let value = u8::from_str_radix(digits, 16).map_err(|_| "Invalid URI percent escape")?;
            if matches!(value, 0 | b'/' | b'\\') {
                return Err("Encoded path separators or NUL are unsupported in file URIs".into());
            }
            result.push(value);
            offset += 3;
        } else {
            result.push(bytes[offset]);
            offset += 1;
        }
    }
    String::from_utf8(result).map_err(|_| "URI path is not valid UTF-8".into())
}

/// Decode literal contents after Tree-sitter has identified a URI AST node.
fn decode_string(source: &str) -> Result<String, String> {
    let mut rest = source.trim();
    let mut result = String::new();
    while !rest.is_empty() {
        rest = skip_trivia(rest)?;
        if rest.is_empty() {
            break;
        }
        let raw = rest.starts_with('r');
        if raw {
            rest = &rest[1..];
        }
        let quote = rest.chars().next().ok_or("Missing URI string literal")?;
        if quote != '\'' && quote != '"' {
            return Err("Unsupported dependency string syntax".into());
        }
        let triple = rest.starts_with(&quote.to_string().repeat(3));
        let delimiter = quote.to_string().repeat(if triple { 3 } else { 1 });
        rest = &rest[delimiter.len()..];
        loop {
            if rest.starts_with(&delimiter) {
                rest = &rest[delimiter.len()..];
                break;
            }
            let character = rest
                .chars()
                .next()
                .ok_or("Unterminated dependency string literal")?;
            rest = &rest[character.len_utf8()..];
            if character == '$' && !raw {
                return Err("Interpolated dependency URIs cannot be resolved statically".into());
            }
            if character == '\\' && !raw {
                let escaped = rest.chars().next().ok_or("Incomplete string escape")?;
                rest = &rest[escaped.len_utf8()..];
                let decoded = match escaped {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'v' => '\u{b}',
                    '\\' | '\'' | '"' | '$' => escaped,
                    'x' | 'u' => {
                        let braced = escaped == 'u' && rest.starts_with('{');
                        if braced {
                            rest = &rest[1..];
                        }
                        let count = if braced {
                            rest.find('}').ok_or("Incomplete Unicode escape")?
                        } else if escaped == 'x' {
                            2
                        } else {
                            4
                        };
                        if count == 0 || count > 6 {
                            return Err("Unsupported Unicode escape".into());
                        }
                        let digits = rest.get(..count).ok_or("Incomplete Unicode escape")?;
                        let value =
                            u32::from_str_radix(digits, 16).map_err(|_| "Invalid string escape")?;
                        rest = &rest[count + usize::from(braced)..];
                        char::from_u32(value).ok_or("String escape is not a Unicode scalar")?
                    }
                    _ => return Err(format!("Unsupported string escape \\{escaped}")),
                };
                result.push(decoded);
            } else {
                result.push(character);
            }
        }
        rest = rest.trim_start();
    }
    Ok(result)
}

fn skip_trivia(mut source: &str) -> Result<&str, String> {
    loop {
        source = source.trim_start();
        if let Some(comment) = source.strip_prefix("//") {
            source = comment
                .find('\n')
                .map(|end| &comment[end + 1..])
                .unwrap_or("");
        } else if let Some(mut comment) = source.strip_prefix("/*") {
            let mut depth = 1;
            while depth > 0 {
                if let Some(tail) = comment.strip_prefix("/*") {
                    depth += 1;
                    comment = tail;
                } else if let Some(tail) = comment.strip_prefix("*/") {
                    depth -= 1;
                    comment = tail;
                } else {
                    let next = comment.chars().next().ok_or("Unterminated URI comment")?;
                    comment = &comment[next.len_utf8()..];
                }
            }
            source = comment;
        } else {
            return Ok(source);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Encode absolute UTF-8 test paths without treating native separators as URI text.
    fn file_uri(path: &Path) -> String {
        use std::fmt::Write;
        use std::path::Prefix;

        assert!(path.is_absolute());
        let mut uri = String::from("file://");
        for component in path.components() {
            match component {
                Component::Prefix(prefix) => match prefix.kind() {
                    Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                        write!(uri, "/{}:", char::from(drive)).unwrap();
                    }
                    _ => panic!("Test file URIs require a local drive"),
                },
                Component::RootDir => uri.push('/'),
                Component::Normal(name) => {
                    if !uri.ends_with('/') {
                        uri.push('/');
                    }
                    for byte in name.to_str().expect("UTF-8 test path").bytes() {
                        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                            uri.push(char::from(byte));
                        } else {
                            write!(uri, "%{byte:02X}").unwrap();
                        }
                    }
                }
                _ => panic!("Test file URIs require normalized paths"),
            }
        }
        uri
    }

    fn put(root: &Path, path: &str, content: &str) {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, content).unwrap();
    }
    fn config(root: &Path, files: &[(&str, Option<&str>, bool)]) -> ArchGuardConfig {
        ArchGuardConfig {
            version: "1".into(),
            name: None,
            language: "dart".into(),
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
    #[test]
    fn ast_extracts_directives_aliases_deferred_combinators_and_literal_variants() {
        let temp = TempDir::new().unwrap();
        put(
            temp.path(),
            "main.dart",
            "// import 'fake.dart';\nimport r'a.dart' as a show A, B hide C;\nimport 'b' '.dart' deferred as b;\nexport \"a.dart\" show A;\nimport 'a\\x2edart';\nimport 'dart:async';\nvoid main() { var text = \"import 'fake.dart';\"; }\n",
        );
        put(temp.path(), "a.dart", "class A {}\n");
        put(temp.path(), "b.dart", "class B {}\n");
        let cfg = config(
            temp.path(),
            &[
                ("main.dart", Some("main"), false),
                ("a.dart", Some("a"), false),
                ("b.dart", Some("b"), false),
            ],
        );
        let outcome = analyze(&cfg).unwrap();
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
        assert_eq!(outcome.dependencies.len(), 5);
        assert_eq!(outcome.dependencies[0].line, 2);
        assert_eq!(outcome.dependencies[0].column, 1);
        assert_eq!(
            outcome.dependencies[0].statement,
            "import r'a.dart' as a show A, B hide C;"
        );
        assert_eq!(
            outcome.dependencies[1].target_file.as_deref(),
            Some(Path::new("b.dart"))
        );
        assert_eq!(
            outcome.dependencies[4].resolution,
            DependencyResolution::External
        );
    }
    #[test]
    fn package_metadata_resolves_project_external_ignored_and_unassigned_targets() {
        let temp = TempDir::new().unwrap();
        let project = temp.path().join("project");
        put(&project, "pubspec.yaml", "name: app\n");
        put(
            &project,
            ".dart_tool/package_config.json",
            r#"{"configVersion":2,"packages":[{"name":"app","rootUri":"../","packageUri":"lib/"},{"name":"vendor","rootUri":"../../vendor/","packageUri":"lib/"}]}"#,
        );
        put(temp.path(), "vendor/lib/api.dart", "class Api {}\n");
        put(
            &project,
            "lib/main.dart",
            "import 'package:app/model.dart' as model;\nimport 'package:vendor/api.dart';\nimport '../loose.dart';\nimport 'ignored.dart';\nimport 'package:unknown/api.dart';\n",
        );
        put(&project, "lib/model.dart", "");
        put(&project, "loose.dart", "");
        put(&project, "lib/ignored.dart", "deliberately invalid");
        let cfg = config(
            &project,
            &[
                ("lib/main.dart", Some("app"), false),
                ("lib/model.dart", Some("model"), false),
                ("loose.dart", None, false),
                ("lib/ignored.dart", None, true),
            ],
        );
        let output = analyze(&cfg).unwrap();
        assert_eq!(output.files.len(), 3);
        assert_eq!(output.dependencies.len(), 5);
        assert_eq!(
            output.dependencies[0].target_file.as_deref(),
            Some(Path::new("lib/model.dart"))
        );
        assert_eq!(
            output.dependencies[1].resolution,
            DependencyResolution::External
        );
        assert_eq!(
            output.dependencies[2].resolution,
            DependencyResolution::Project
        );
        assert!(output.dependencies[2].target_module.is_none());
        assert!(output.dependencies[3].target_ignored);
        assert_eq!(output.diagnostics.len(), 1);
        assert_eq!(output.diagnostics[0].code, "IMPORT_UNRESOLVED");
    }
    #[test]
    fn pubspec_self_and_path_dependencies_require_real_targets() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("app");
        put(
            &root,
            "pubspec.yaml",
            "name: demo\ndependencies:\n  local:\n    path: ../local\n  remote: ^1.0.0\n",
        );
        put(temp.path(), "local/pubspec.yaml", "name: local\n");
        put(temp.path(), "local/lib/api.dart", "");
        put(
            &root,
            "lib/a.dart",
            "import 'package:demo/b.dart';\nimport 'package:local/api.dart';\nimport 'package:remote/api.dart';\nimport 'package:local/missing.dart';\n",
        );
        put(&root, "lib/b.dart", "");
        let cfg = config(
            &root,
            &[
                ("lib/a.dart", Some("a"), false),
                ("lib/b.dart", Some("b"), false),
            ],
        );
        let output = analyze(&cfg).unwrap();
        assert_eq!(
            output.dependencies[0].resolution,
            DependencyResolution::Project
        );
        assert_eq!(
            output.dependencies[1].resolution,
            DependencyResolution::External
        );
        assert_eq!(output.diagnostics.len(), 2);
    }
    #[test]
    fn parts_have_forward_edges_and_uri_or_named_ownership() {
        let temp = TempDir::new().unwrap();
        put(
            temp.path(),
            "owner.dart",
            "library demo.lib;\npart 'uri.dart';\npart 'named.dart';\n",
        );
        put(
            temp.path(),
            "uri.dart",
            "part of 'owner.dart';\nclass A {}\n",
        );
        put(temp.path(), "named.dart", "part of demo.lib;\nclass B {}\n");
        let cfg = config(
            temp.path(),
            &[
                ("owner.dart", Some("owner"), false),
                ("uri.dart", Some("part"), false),
                ("named.dart", Some("part"), false),
            ],
        );
        let output = analyze(&cfg).unwrap();
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        assert_eq!(output.dependencies.len(), 2);
        assert!(
            output
                .dependencies
                .iter()
                .all(|edge| edge.source_file == Path::new("owner.dart"))
        );
    }
    #[test]
    fn conditionals_do_not_guess_a_compilation_environment() {
        let temp = TempDir::new().unwrap();
        put(
            temp.path(),
            "main.dart",
            "import 'a.dart';\nexport 'a.dart' if (dart.library.io) 'b.dart';\nimport 'dart:io' if (dart.library.html) 'dart:html';\nimport 'a.dart' if (dart.library.io) 'a.dart';\n",
        );
        put(temp.path(), "a.dart", "");
        put(temp.path(), "b.dart", "");
        let cfg = config(
            temp.path(),
            &[
                ("main.dart", Some("main"), false),
                ("a.dart", Some("a"), false),
                ("b.dart", Some("b"), false),
            ],
        );
        let output = analyze(&cfg).unwrap();
        assert_eq!(output.dependencies.len(), 4);
        assert_eq!(
            output.dependencies[0].resolution,
            DependencyResolution::Project
        );
        assert_eq!(
            output.dependencies[1].resolution,
            DependencyResolution::Unresolved
        );
        assert_eq!(
            output.dependencies[2].resolution,
            DependencyResolution::External
        );
        assert_eq!(
            output.dependencies[3].resolution,
            DependencyResolution::Project
        );
        assert_eq!(output.diagnostics.len(), 1);
    }
    #[test]
    fn failures_keep_successful_files_and_do_not_misclassify_unknown_uris() {
        let temp = TempDir::new().unwrap();
        put(
            temp.path(),
            "ok.dart",
            "import 'target.dart';\nimport 'dart:made_up';\nimport 'https://example.com/api.dart';\nimport 'missing.dart';\n",
        );
        put(temp.path(), "target.dart", "");
        put(temp.path(), "broken.dart", "void broken( {\n");
        put(temp.path(), "new_syntax.dart", "final pair = (1, 2);\n");
        let cfg = config(
            temp.path(),
            &[
                ("ok.dart", Some("a"), false),
                ("target.dart", Some("b"), false),
                ("broken.dart", None, false),
                ("missing.dart", None, false),
                ("new_syntax.dart", None, false),
            ],
        );
        let output = analyze(&cfg).unwrap();
        assert_eq!(output.files.iter().filter(|f| f.succeeded).count(), 2);
        assert_eq!(output.files.iter().filter(|f| !f.succeeded).count(), 3);
        assert_eq!(
            output
                .diagnostics
                .iter()
                .filter(|d| d.code == "PARSE_FAILED")
                .count(),
            2
        );
        assert_eq!(
            output
                .diagnostics
                .iter()
                .filter(|d| d.code == "FILE_READ_FAILED")
                .count(),
            1
        );
        assert_eq!(
            output
                .diagnostics
                .iter()
                .filter(|d| d.code == "IMPORT_UNRESOLVED")
                .count(),
            2
        );
        assert_eq!(
            output.dependencies[0].target_file.as_deref(),
            Some(Path::new("target.dart"))
        );
    }
    #[test]
    fn malformed_package_metadata_is_diagnosed_without_losing_relative_edges() {
        let temp = TempDir::new().unwrap();
        put(
            temp.path(),
            "main.dart",
            "import 'target.dart';\nimport 'package:app/target.dart';\n",
        );
        put(temp.path(), "target.dart", "");
        let cfg = config(
            temp.path(),
            &[
                ("main.dart", Some("a"), false),
                ("target.dart", Some("b"), false),
            ],
        );
        for metadata in [
            "{broken",
            r#"{"configVersion":1,"packages":[]}"#,
            r#"{"configVersion":2,"packages":[{"name":"app","rootUri":null}]}"#,
            r#"{"configVersion":2,"packages":[{"name":"app","rootUri":"../","packageUri":"../../"}]}"#,
            r#"{"configVersion":2,"packages":[{"name":"app","rootUri":"../"},{"name":"app","rootUri":"../"}]}"#,
        ] {
            put(temp.path(), ".dart_tool/package_config.json", metadata);
            let output = analyze(&cfg).unwrap();
            assert!(output.files.iter().all(|file| file.succeeded));
            assert_eq!(
                output.dependencies[0].resolution,
                DependencyResolution::Project
            );
            assert_eq!(
                output.dependencies[1].resolution,
                DependencyResolution::Unresolved
            );
            assert_eq!(output.diagnostics.len(), 1, "{metadata}");
        }
    }

    #[test]
    fn annotations_unicode_columns_and_percent_paths_do_not_create_fake_imports() {
        let temp = TempDir::new().unwrap();
        put(
            temp.path(),
            "main.dart",
            "/* λ */ import 'space%20name.dart';\n@Deprecated('not_a_dependency.dart') export 'space name.dart';\nimport 'a${value}.dart';\n",
        );
        put(temp.path(), "space name.dart", "");
        let cfg = config(
            temp.path(),
            &[
                ("main.dart", Some("main"), false),
                ("space name.dart", Some("target"), false),
            ],
        );
        let output = analyze(&cfg).unwrap();
        assert_eq!(output.dependencies.len(), 3);
        assert_eq!(output.dependencies[0].column, 9);
        assert_eq!(
            output.dependencies[0].target_file.as_deref(),
            Some(Path::new("space name.dart"))
        );
        assert_eq!(
            output.dependencies[1].statement,
            "export 'space name.dart';"
        );
        assert_eq!(output.diagnostics.len(), 1);
        assert!(output.diagnostics[0].message.contains("Interpolated"));
    }

    #[test]
    fn missing_and_ambiguous_part_owners_are_unresolved() {
        let temp = TempDir::new().unwrap();
        put(temp.path(), "a.dart", "library same.name;\n");
        put(temp.path(), "b.dart", "library same.name;\n");
        put(temp.path(), "named.dart", "part of same.name;\n");
        put(temp.path(), "uri.dart", "part of 'a.dart';\n");
        let cfg = config(
            temp.path(),
            &[
                ("a.dart", None, false),
                ("b.dart", None, false),
                ("named.dart", None, false),
                ("uri.dart", None, false),
            ],
        );
        let output = analyze(&cfg).unwrap();
        assert_eq!(output.diagnostics.len(), 2);
        assert!(
            output
                .diagnostics
                .iter()
                .all(|d| d.code == "IMPORT_UNRESOLVED")
        );
        assert!(
            output
                .dependencies
                .iter()
                .all(|edge| edge.resolution == DependencyResolution::Unresolved)
        );
    }

    #[test]
    fn ancestor_workspace_metadata_preserves_internal_package_membership() {
        let temp = TempDir::new().unwrap();
        put(
            temp.path(),
            ".dart_tool/package_config.json",
            r#"{"configVersion":2,"packages":[{"name":"a","rootUri":"../packages/a","packageUri":"lib/"},{"name":"b","rootUri":"../packages/b","packageUri":"lib/"}]}"#,
        );
        put(
            temp.path(),
            "packages/a/lib/main.dart",
            "import 'package:b/api.dart';\n",
        );
        put(temp.path(), "packages/b/lib/api.dart", "");
        let cfg = config(
            temp.path(),
            &[
                ("packages/a/lib/main.dart", Some("a"), false),
                ("packages/b/lib/api.dart", Some("b"), false),
            ],
        );
        let output = analyze(&cfg).unwrap();
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        assert_eq!(
            output.dependencies[0].resolution,
            DependencyResolution::Project
        );
        assert_eq!(output.dependencies[0].target_module.as_deref(), Some("b"));
    }

    #[test]
    fn literal_and_uri_decoding_handles_escaping_without_executing_code() {
        assert_eq!(decode_string("'a' /* note */ r'.dart'").unwrap(), "a.dart");
        assert_eq!(decode_string("'a\\u002edart'").unwrap(), "a.dart");
        assert_eq!(decode_string("'a\\u{2e}dart'").unwrap(), "a.dart");
        assert!(decode_string("'${name}.dart'").is_err());
        let temp = TempDir::new().unwrap();
        assert_eq!(
            local_uri(temp.path(), "a%20b.dart").unwrap(),
            temp.path().join("a b.dart")
        );
        assert!(local_uri(temp.path(), "a.dart?query").is_err());
        assert!(local_uri(temp.path(), "%2fetc.dart").is_err());
        assert!(local_uri(temp.path(), "%xx").is_err());
        let file_uri = file_uri(&temp.path().join("a.dart"));
        assert_eq!(
            local_uri(temp.path(), &file_uri).unwrap(),
            temp.path().join("a.dart")
        );
    }

    #[test]
    fn file_uris_encode_spaces_reserved_characters_and_unicode() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("a b#%?[]é.dart");
        let uri = file_uri(&path);
        assert!(uri.starts_with("file:///"));
        assert!(uri.ends_with("/a%20b%23%25%3F%5B%5D%C3%A9.dart"));
        assert_eq!(local_uri(temp.path(), &uri).unwrap(), path);

        for invalid in [
            r"file://C:\Users\runner\a.dart",
            "file:///tmp/a.dart?query",
            "file:///tmp/a.dart#fragment",
            "file:///tmp/a%5Cb.dart",
            "file:///tmp/a%00.dart",
            "file:///tmp/a\n.dart",
            "file://server/share/a.dart",
        ] {
            assert!(local_uri(temp.path(), invalid).is_err(), "{invalid:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn file_uris_round_trip_windows_drive_paths() {
        for path in [
            r"C:\Program Files\Dart\a #%.dart",
            "C:/Program Files/Dart/a #%.dart",
            r"\\?\C:\Program Files\Dart\a #%.dart",
        ] {
            let uri = file_uri(Path::new(path));
            assert_eq!(uri, "file:///C:/Program%20Files/Dart/a%20%23%25.dart");
            assert_eq!(
                local_uri(Path::new(r"D:\project"), &uri).unwrap(),
                Path::new(r"C:\Program Files\Dart\a #%.dart")
            );
        }
    }
}
