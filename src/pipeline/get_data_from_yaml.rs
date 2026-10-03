use crate::DataModels::Models::{ArchGuardConfig, Diagnostic, PipelineError, SourceLocation};
use serde::Deserializer;
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_yaml::Value;
use std::fmt;
use std::path::Path;

pub fn get_data_from_yaml(file_path: &str) -> Result<ArchGuardConfig, PipelineError> {
    let input = Path::new(file_path);
    let label = input
        .file_name()
        .unwrap_or(input.as_os_str())
        .to_string_lossy()
        .into_owned();
    let path = std::path::absolute(input).map_err(|error| {
        Diagnostic::error(
            "INTERNAL_ERROR",
            format!("Cannot resolve configuration path: {error}"),
            None,
        )
    })?;
    let source = std::fs::read_to_string(&path).map_err(|error| {
        Diagnostic::error(
            "CONFIG_INVALID",
            format!("Cannot read configuration: {error}"),
            Some(SourceLocation {
                file: label.clone(),
                line: None,
                column: None,
            }),
        )
    })?;
    let value: Value = serde_yaml::from_str(&source).map_err(|error| yaml_error(&label, error))?;
    check_object(&value, &[], &source, &label, ObjectKind::Root)?;
    // Deserializing a Value enforces scalar types; serde_yaml's direct String deserializer
    // otherwise accepts numeric/boolean scalars as strings.
    let mut config: ArchGuardConfig =
        serde_yaml::from_value(value).map_err(|error| yaml_error(&label, error))?;
    config.project_root = path.parent().unwrap_or(Path::new(".")).to_owned();
    config.config_path = path;
    for (index, rule) in config.rules.iter_mut().flatten().enumerate() {
        let path = vec![
            Part::Key("rules".into()),
            Part::Index(index),
            Part::Key("module".into()),
        ];
        rule.source_line = locate(&source, &path).map(|(line, _)| line);
        let kinds: Vec<_> = rule
            .constraints()
            .into_iter()
            .filter_map(|(kind, values)| values.map(|_| kind))
            .collect();
        for kind in kinds {
            let path = vec![
                Part::Key("rules".into()),
                Part::Index(index),
                Part::Key(kind.as_str().into()),
            ];
            let (line, _) = locate(&source, &path).ok_or_else(|| {
                Diagnostic::error(
                    "INTERNAL_ERROR",
                    "Cannot recover a constraint's YAML source location",
                    Some(SourceLocation {
                        file: label.clone(),
                        line: None,
                        column: None,
                    }),
                )
            })?;
            rule.constraint_lines.insert(kind, line);
        }
    }
    Ok(config)
}

fn yaml_error(label: &str, error: serde_yaml::Error) -> PipelineError {
    Diagnostic::error(
        "CONFIG_INVALID",
        error.to_string(),
        Some(SourceLocation {
            file: label.into(),
            line: error.location().map(|loc| loc.line()),
            column: error.location().map(|loc| loc.column()),
        }),
    )
    .into()
}
#[derive(Clone)]
enum Part {
    Key(String),
    Index(usize),
}
#[derive(Clone, Copy)]
enum ObjectKind {
    Root,
    Module,
    Rule,
}
fn invalid(source: &str, label: &str, path: &[Part], message: String) -> PipelineError {
    let position = locate(source, path);
    Diagnostic::error(
        "CONFIG_INVALID",
        message,
        Some(SourceLocation {
            file: label.into(),
            line: position.map(|p| p.0),
            column: position.map(|p| p.1),
        }),
    )
    .into()
}
fn check_object(
    value: &Value,
    path: &[Part],
    source: &str,
    label: &str,
    kind: ObjectKind,
) -> Result<(), PipelineError> {
    let map = value.as_mapping().ok_or_else(|| {
        invalid(
            source,
            label,
            path,
            "Expected an object; null is not allowed".into(),
        )
    })?;
    let (allowed, required): (&[&str], &[&str]) = match kind {
        ObjectKind::Root => (
            &["version", "name", "language", "modules", "rules", "ignore"],
            &["version", "language", "modules"],
        ),
        ObjectKind::Module => (&["name", "path"], &["name", "path"]),
        ObjectKind::Rule => (
            &[
                "module",
                "name",
                "reason",
                "deny_to",
                "allow_only_to",
                "deny_from",
                "allow_only_from",
            ],
            &["module"],
        ),
    };
    for required in required {
        if !map.contains_key(Value::String((*required).into())) {
            return Err(invalid(
                source,
                label,
                path,
                format!("Missing required field '{required}'"),
            ));
        }
    }
    for (key, value) in map {
        let Some(key) = key.as_str() else {
            return Err(invalid(
                source,
                label,
                path,
                "Object keys must be strings".into(),
            ));
        };
        let mut field_path = path.to_vec();
        field_path.push(Part::Key(key.into()));
        if !allowed.contains(&key) {
            return Err(invalid(
                source,
                label,
                &field_path,
                format!("Unknown field '{key}'"),
            ));
        }
        match key {
            "modules" | "rules" => {
                let list = value.as_sequence().ok_or_else(|| {
                    invalid(
                        source,
                        label,
                        &field_path,
                        format!("'{key}' must be an array; null is not allowed"),
                    )
                })?;
                for (index, item) in list.iter().enumerate() {
                    let mut item_path = field_path.clone();
                    item_path.push(Part::Index(index));
                    check_object(
                        item,
                        &item_path,
                        source,
                        label,
                        if key == "modules" {
                            ObjectKind::Module
                        } else {
                            ObjectKind::Rule
                        },
                    )?;
                }
            }
            "ignore" | "deny_to" | "allow_only_to" | "deny_from" | "allow_only_from" => {
                if !value
                    .as_sequence()
                    .is_some_and(|items| items.iter().all(Value::is_string))
                {
                    return Err(invalid(
                        source,
                        label,
                        &field_path,
                        format!("'{key}' must be an array of strings; null is not allowed"),
                    ));
                }
            }
            _ if !value.is_string() => {
                return Err(invalid(
                    source,
                    label,
                    &field_path,
                    format!("'{key}' must be a string; null is not allowed"),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

// serde_yaml exposes marks on errors, not on Values. Probe the already-validated YAML
// with a seed that deliberately errors at the selected key's visitor. The parser then
// supplies the real token mark (including flow mappings, comments, aliases and quoted keys).
// This avoids guessing line numbers with text searches or introducing another YAML parser.
fn locate(source: &str, path: &[Part]) -> Option<(usize, usize)> {
    let error = Probe(path)
        .deserialize(serde_yaml::Deserializer::from_str(source))
        .err()?;
    if !error.to_string().contains("archguard source mark") {
        return None;
    }
    error.location().map(|loc| (loc.line(), loc.column()))
}
struct Probe<'a>(&'a [Part]);
impl<'de> DeserializeSeed<'de> for Probe<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        if self.0.is_empty() {
            return deserializer.deserialize_any(Mark);
        }
        deserializer.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Probe<'_> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a configuration container")
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
        while let Some(key) = map.next_key_seed(KeyProbe(self.0))? {
            if matches!(&self.0[0], Part::Key(wanted) if *wanted == key) {
                map.next_value_seed(Probe(&self.0[1..]))?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }
    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<(), S::Error> {
        let mut index = 0;
        loop {
            let exists = if matches!(self.0[0], Part::Index(wanted) if wanted == index) {
                seq.next_element_seed(Probe(&self.0[1..]))?.is_some()
            } else {
                seq.next_element::<IgnoredAny>()?.is_some()
            };
            if !exists {
                break;
            }
            index += 1;
        }
        Ok(())
    }
}
struct KeyProbe<'a>(&'a [Part]);
impl<'de> DeserializeSeed<'de> for KeyProbe<'_> {
    type Value = String;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<String, D::Error> {
        deserializer.deserialize_string(self)
    }
}
impl Visitor<'_> for KeyProbe<'_> {
    type Value = String;
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a field name")
    }
    fn visit_str<E: de::Error>(self, key: &str) -> Result<String, E> {
        if self.0.len() == 1 && matches!(&self.0[0], Part::Key(wanted) if wanted == key) {
            Err(E::custom("archguard source mark"))
        } else {
            Ok(key.into())
        }
    }
}
struct Mark;
impl<'de> Visitor<'de> for Mark {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a source mark")
    }
    fn visit_map<M: MapAccess<'de>>(self, _: M) -> Result<(), M::Error> {
        Err(de::Error::custom("archguard source mark"))
    }
    fn visit_seq<S: SeqAccess<'de>>(self, _: S) -> Result<(), S::Error> {
        Err(de::Error::custom("archguard source mark"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DataModels::Models::ConstraintKind;
    fn load(text: &str) -> Result<ArchGuardConfig, PipelineError> {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp.path().join("archguard.yaml");
        std::fs::write(&file, text).unwrap();
        get_data_from_yaml(file.to_str().unwrap())
    }
    #[test]
    fn strict_types_nulls_and_unknown_fields() {
        for text in [
            "version: 1\nlanguage: python\nmodules: []",
            "version: '1'\nlanguage: true\nmodules: []",
            "version: '1'\nlanguage: python\nmodules: []\nname: null",
            "version: '1'\nlanguage: python\nmodules: []\nrules: null",
            "version: '1'\nlanguage: python\nmodules: []\nignore: null",
            "version: '1'\nlanguage: python\nmodules: []\nextra: x",
            "version: '1'\nlanguage: python\nmodules: [{name: a, path: '*.py', files: []}]",
            "version: '1'\nlanguage: python\nmodules: []\nrules: [{module: a, deny_to: null}]",
            "version: '1'\nlanguage: python\nmodules: []\nrules: [{module: a, deny_to: a}]",
            "version: '1'\nlanguage: python\nmodules: []\nrules: [{module: a, deny_to: [1]}]",
            "version: '1'\nlanguage: python\nmodules: []\nrules: [{module: a, typo: []}]",
            "version: '1'\nmodules: []",
            "[invalid",
            "version: '1'\nversion: '1'\nlanguage: python\nmodules: []",
        ] {
            assert!(load(text).is_err(), "Accepted: {text}");
        }
    }
    #[test]
    fn preserves_omitted_empty_and_real_constraint_locations() {
        let config = load("version: '1'\nlanguage: Python\nmodules: [{name: a, path: '*.py'}]\nrules:\n  # comment\n  - module: a\n    name: |\n      deny_to: is only text here\n    'deny_to': []\n    allow_only_from: []\n  - {module: a, allow_only_to: []}\n").unwrap();
        let rules = config.rules.unwrap();
        assert_eq!(rules[0].deny_to, Some(vec![]));
        assert_eq!(rules[0].allow_only_to, None);
        assert_eq!(rules[0].constraint_lines[&ConstraintKind::DenyTo], 9);
        assert_eq!(
            rules[0].constraint_lines[&ConstraintKind::AllowOnlyFrom],
            10
        );
        assert_eq!(rules[1].constraint_lines[&ConstraintKind::AllowOnlyTo], 11);
        assert!(
            load("version: '1'\nlanguage: python\nmodules: []")
                .unwrap()
                .rules
                .is_none()
        );
    }
    #[test]
    fn yaml_anchors_keep_definition_locations() {
        let config = load("version: '1'\nlanguage: python\nmodules: []\nrules:\n  - &rule {module: a, deny_to: []}\n  - *rule\n").unwrap();
        assert_eq!(
            config.rules.unwrap()[1].constraint_lines[&ConstraintKind::DenyTo],
            5
        );
    }
}
