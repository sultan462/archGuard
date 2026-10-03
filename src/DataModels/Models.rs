use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchGuardConfig {
    pub version: String,
    pub name: Option<String>,
    pub language: String,
    pub modules: Vec<ModuleConfig>,
    pub rules: Option<Vec<RuleConfig>>,
    pub ignore: Option<Vec<String>>,
    #[serde(skip)]
    pub config_path: PathBuf,
    #[serde(skip)]
    pub project_root: PathBuf,
    #[serde(skip)]
    pub project_files: Vec<ProjectFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleConfig {
    pub name: String,
    pub path: String,
    #[serde(skip)]
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    pub name: Option<String>,
    pub module: String,
    pub reason: Option<String>,
    pub deny_to: Option<Vec<String>>,
    pub allow_only_to: Option<Vec<String>>,
    pub deny_from: Option<Vec<String>>,
    pub allow_only_from: Option<Vec<String>>,
    #[serde(skip)]
    pub source_line: Option<usize>,
    #[serde(skip)]
    pub constraint_lines: BTreeMap<ConstraintKind, usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintKind {
    DenyTo,
    AllowOnlyTo,
    DenyFrom,
    AllowOnlyFrom,
}

impl ConstraintKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DenyTo => "deny_to",
            Self::AllowOnlyTo => "allow_only_to",
            Self::DenyFrom => "deny_from",
            Self::AllowOnlyFrom => "allow_only_from",
        }
    }
}
impl RuleConfig {
    pub fn constraints(&self) -> [(ConstraintKind, Option<&[String]>); 4] {
        [
            (ConstraintKind::DenyTo, self.deny_to.as_deref()),
            (ConstraintKind::AllowOnlyTo, self.allow_only_to.as_deref()),
            (ConstraintKind::DenyFrom, self.deny_from.as_deref()),
            (
                ConstraintKind::AllowOnlyFrom,
                self.allow_only_from.as_deref(),
            ),
        ]
    }
}

#[derive(Debug, Clone)]
pub struct ProjectFile {
    /// Relative to the configuration directory, including ignored resolution targets.
    pub path: PathBuf,
    pub module: Option<String>,
    pub ignored: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyResolution {
    Project,
    External,
    Unresolved,
}

#[derive(Debug, Clone)]
pub struct DependencyEdge {
    pub source_module: Option<String>,
    pub source_file: PathBuf,
    pub target_module: Option<String>,
    pub target_file: Option<PathBuf>,
    pub line: usize,
    pub column: usize,
    pub statement: String,
    pub resolution: DependencyResolution,
    pub target_ignored: bool,
    pub is_allowed: bool,
    pub failed_constraints: Vec<FailedConstraint>,
}
#[derive(Debug, Clone, Serialize)]
pub struct FailedConstraint {
    pub rule_index: usize,
    pub rule_name: Option<String>,
    pub rule_module: String,
    pub constraint: ConstraintKind,
    pub configured_modules: Vec<String>,
    pub reason: Option<String>,
    pub config_line: usize,
}
#[derive(Debug, Clone, Serialize)]
pub struct SourceLocation {
    pub file: String,
    pub line: Option<usize>,
    pub column: Option<usize>,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    // Reserved by report schema v1; current diagnostics are errors.
    #[allow(dead_code)]
    Warning,
    Error,
}
#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: String,
    pub message: String,
    pub location: Option<SourceLocation>,
}
impl Diagnostic {
    pub fn error(code: &str, message: impl Into<String>, location: Option<SourceLocation>) -> Self {
        Self {
            severity: Severity::Error,
            code: code.into(),
            message: message.into(),
            location,
        }
    }
}
#[derive(Debug)]
pub struct PipelineError {
    pub diagnostics: Vec<Diagnostic>,
}
impl From<Diagnostic> for PipelineError {
    fn from(diagnostic: Diagnostic) -> Self {
        Self {
            diagnostics: vec![diagnostic],
        }
    }
}
#[derive(Debug, Default)]
pub struct DiscoveryOutcome {
    pub files_assigned: usize,
    pub files_unassigned: usize,
    pub files_ignored: usize,
    pub unassigned_files: Vec<String>,
    pub empty_modules: Vec<String>,
}
#[derive(Debug)]
pub struct FileOutcome {
    pub file: PathBuf,
    pub succeeded: bool,
}
#[derive(Debug, Default)]
pub struct AnalysisOutcome {
    pub dependencies: Vec<DependencyEdge>,
    pub diagnostics: Vec<Diagnostic>,
    pub files: Vec<FileOutcome>,
}
#[derive(Debug, Default)]
pub struct AnalysisCounts {
    pub files_analyzed: usize,
    pub files_failed: usize,
    pub unresolved_imports: usize,
}
#[derive(Debug, Default)]
pub struct RunContext {
    pub config_label: String,
    pub duration_ms: u64,
    pub diagnostics: Vec<Diagnostic>,
    pub fatal: bool,
    pub discovery: Option<DiscoveryOutcome>,
    pub analysis: Option<AnalysisCounts>,
    pub checked: bool,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub report_version: &'static str,
    pub tool: ToolReport,
    pub project: ProjectReport,
    pub run: RunReport,
    pub summary: Summary,
    pub violations: Vec<Violation>,
    pub coverage: Coverage,
    pub diagnostics: Vec<Diagnostic>,
}
#[derive(Debug, Serialize)]
pub struct ToolReport {
    pub name: &'static str,
    pub version: &'static str,
}
#[derive(Debug, Serialize)]
pub struct ProjectReport {
    pub name: Option<String>,
    pub config: String,
    pub language: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct RunReport {
    pub status: RunStatus,
    pub duration_ms: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Pass,
    Fail,
    Incomplete,
    Error,
}
impl RunStatus {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Pass => 0,
            Self::Fail => 1,
            Self::Incomplete | Self::Error => 2,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Incomplete => "INCOMPLETE",
            Self::Error => "ERROR",
        }
    }
}
#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub files_analyzed: Option<usize>,
    pub files_assigned: Option<usize>,
    pub files_unassigned: Option<usize>,
    pub files_ignored: Option<usize>,
    pub files_failed: Option<usize>,
    pub modules: Option<usize>,
    pub rules: Option<usize>,
    pub dependencies_checked: Option<usize>,
    pub violations: Option<usize>,
    pub unresolved_imports: Option<usize>,
}
#[derive(Debug, Serialize)]
pub struct Violation {
    pub source: ViolationSource,
    pub target: ViolationTarget,
    pub failed_constraints: Vec<FailedConstraint>,
}
#[derive(Debug, Serialize)]
pub struct ViolationSource {
    pub module: String,
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub statement: String,
}
#[derive(Debug, Serialize)]
pub struct ViolationTarget {
    pub module: String,
    pub file: String,
}
#[derive(Debug, Default, Serialize)]
pub struct Coverage {
    pub unassigned_files: Vec<String>,
    pub empty_modules: Vec<String>,
}
#[derive(Debug)]
pub struct GeneratedReport {
    pub json: String,
    pub human: String,
    pub exit_code: u8,
}
