use crate::DataModels::Models::{
    ArchGuardConfig, ConstraintKind, DependencyEdge, DependencyResolution, FailedConstraint,
};

pub fn checker(
    config: &ArchGuardConfig,
    dependencies: &mut [DependencyEdge],
) -> Result<(), String> {
    for edge in dependencies {
        edge.is_allowed = true;
        edge.failed_constraints.clear();
        let (Some(source), Some(target)) = (&edge.source_module, &edge.target_module) else {
            continue;
        };
        if edge.resolution != DependencyResolution::Project
            || edge.target_ignored
            || edge.target_file.is_none()
            || source == target
        {
            continue;
        }
        for (rule_index, rule) in config.rules.iter().flatten().enumerate() {
            for (kind, names) in rule.constraints() {
                let Some(names) = names else {
                    continue;
                };
                let (subject, other, allowlist) = match kind {
                    ConstraintKind::DenyTo => (source, target, false),
                    ConstraintKind::AllowOnlyTo => (source, target, true),
                    ConstraintKind::DenyFrom => (target, source, false),
                    ConstraintKind::AllowOnlyFrom => (target, source, true),
                };
                if subject == &rule.module && (names.contains(other) != allowlist) {
                    let config_line =
                        rule.constraint_lines.get(&kind).copied().ok_or_else(|| {
                            format!(
                                "Missing source location for rule {rule_index} {}",
                                kind.as_str()
                            )
                        })?;
                    edge.failed_constraints.push(FailedConstraint {
                        rule_index,
                        rule_name: rule.name.clone(),
                        rule_module: rule.module.clone(),
                        constraint: kind,
                        configured_modules: names.to_vec(),
                        reason: rule.reason.clone(),
                        config_line,
                    });
                }
            }
        }
        edge.is_allowed = edge.failed_constraints.is_empty();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::get_data_from_yaml::get_data_from_yaml;
    fn config(rules: &str) -> ArchGuardConfig {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("archguard.yaml");
        std::fs::write(&path, format!("version: '1'\nlanguage: python\nmodules:\n  - {{name: A, path: 'a/**'}}\n  - {{name: B, path: 'b/**'}}\n  - {{name: C, path: 'c/**'}}\nrules:\n{rules}")).unwrap();
        get_data_from_yaml(path.to_str().unwrap()).unwrap()
    }
    fn edge(source: &str, target: &str) -> DependencyEdge {
        DependencyEdge {
            source_module: Some(source.into()),
            source_file: "source.py".into(),
            target_module: Some(target.into()),
            target_file: Some("target.py".into()),
            line: 1,
            column: 1,
            statement: "import target".into(),
            resolution: DependencyResolution::Project,
            target_ignored: false,
            is_allowed: true,
            failed_constraints: vec![],
        }
    }
    #[test]
    fn checks_all_four_constraints_and_collects_every_failure() {
        let config = config(
            "  - {module: A, deny_to: [B], allow_only_to: []}\n  - {module: B, deny_from: [A], allow_only_from: []}",
        );
        let mut edges = [edge("A", "B"), edge("B", "A")];
        checker(&config, &mut edges).unwrap();
        assert!(!edges[0].is_allowed);
        assert_eq!(edges[0].failed_constraints.len(), 4);
        assert_eq!(
            edges[0]
                .failed_constraints
                .iter()
                .map(|f| f.rule_index)
                .collect::<Vec<_>>(),
            [0, 0, 1, 1]
        );
        assert!(edges[1].is_allowed);
    }
    #[test]
    fn allowlists_intersect_and_do_not_override_denial_in_any_order() {
        for rules in [
            "  - {module: A, allow_only_to: [B, C]}\n  - {module: A, allow_only_to: [B]}\n  - {module: B, deny_from: [A]}",
            "  - {module: B, deny_from: [A]}\n  - {module: A, allow_only_to: [B]}\n  - {module: A, allow_only_to: [B, C]}",
        ] {
            let mut edges = [edge("A", "B"), edge("A", "C")];
            checker(&config(rules), &mut edges).unwrap();
            assert!(
                edges
                    .iter()
                    .all(|edge| !edge.is_allowed && edge.failed_constraints.len() == 1)
            );
        }
        let mut edges = [edge("A", "C"), edge("B", "C")];
        checker(
            &config(
                "  - {module: C, allow_only_from: [A, B]}\n  - {module: C, allow_only_from: [A]}",
            ),
            &mut edges,
        )
        .unwrap();
        assert!(edges[0].is_allowed);
        assert!(!edges[1].is_allowed);
    }
    #[test]
    fn exemptions_default_allow_and_empty_denylists() {
        let mut edges = [
            edge("A", "A"),
            edge("A", "B"),
            edge("A", "B"),
            edge("A", "B"),
            edge("A", "B"),
        ];
        edges[1].target_ignored = true;
        edges[2].source_module = None;
        edges[3].target_module = None;
        edges[4].resolution = DependencyResolution::External;
        checker(
            &config("  - {module: A, allow_only_to: [], allow_only_from: []}"),
            &mut edges,
        )
        .unwrap();
        assert!(edges.iter().all(|edge| edge.is_allowed));
        let mut edges = [edge("A", "B")];
        checker(
            &config("  - {module: A, deny_to: [], deny_from: []}"),
            &mut edges,
        )
        .unwrap();
        assert!(edges[0].is_allowed);
        checker(&config("  []"), &mut edges).unwrap();
        assert!(edges[0].is_allowed);
    }
}
