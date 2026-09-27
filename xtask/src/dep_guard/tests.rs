//! Tests for the workspace dependency-direction guard.

use super::*;

fn build_dependency_graph(
    crate_dependency_pairs: &[(&str, &[&str])],
) -> Vec<WorkspaceCrateDependencies> {
    crate_dependency_pairs
        .iter()
        .map(|(crate_name, dependency_names)| {
            (
                (*crate_name).to_string(),
                dependency_names
                    .iter()
                    .map(|dependency_name| (*dependency_name).to_string())
                    .collect(),
            )
        })
        .collect()
}

/// Builds metadata from workspace member IDs and `(name, dependencies)`
/// package tuples. Each dependency string is a JSON array in cargo metadata
/// format, and each package ID equals its package name.
fn build_metadata(
    workspace_member_names: &[&str],
    package_metadata_entries: &[(&str, &str)],
) -> Metadata {
    let package_json_entries: Vec<String> = package_metadata_entries
        .iter()
        .map(|(package_name, dependency_json)| {
            format!(
                r#"{{"name":"{package_name}","version":"0.1.0","id":"{package_name}",
                    "dependencies":{dependency_json},"targets":[],"features":{{}},
                    "manifest_path":"/w/{package_name}/Cargo.toml"}}"#
            )
        })
        .collect();
    let workspace_member_json_entries: Vec<String> = workspace_member_names
        .iter()
        .map(|workspace_member_name| format!("\"{workspace_member_name}\""))
        .collect();
    let json = format!(
        r#"{{"packages":[{}],"workspace_members":[{}],"workspace_root":"/w",
            "target_directory":"/w/target","version":1}}"#,
        package_json_entries.join(","),
        workspace_member_json_entries.join(",")
    );
    MetadataCommand::parse(json).expect("hand-written metadata parses")
}

/// Builds one dependency object in cargo metadata JSON format. The dependency
/// kind is `null`, `"dev"`, or `"build"`; the optional flag is a JSON boolean;
/// the target configuration is a `cfg(...)` string or `null`; and the rename
/// value is an alias or `null`. The object omits `source`, `registry`, and
/// `path`.
fn format_dependency_json(
    dependency_name: &str,
    dependency_kind: &str,
    is_optional: bool,
    target_configuration: Option<&str>,
    dependency_alias: Option<&str>,
) -> String {
    let target_configuration_json = match target_configuration {
        Some(target_configuration) => format!("\"{target_configuration}\""),
        None => "null".to_string(),
    };
    let dependency_alias_json = match dependency_alias {
        Some(dependency_alias) => format!("\"{dependency_alias}\""),
        None => "null".to_string(),
    };
    format!(
        r#"{{"name":"{dependency_name}","req":"*","kind":{dependency_kind},"optional":{is_optional},
            "uses_default_features":true,"features":[],"target":{target_configuration_json},"rename":{dependency_alias_json}}}"#
    )
}

#[test]
fn allowed_graph_has_no_violations() {
    let crate_dependencies = build_dependency_graph(&[
        ("koshi-core", &[]),
        ("koshi-pty", &["koshi-core", "portable-pty"]),
        // This graph has `koshi-daemon` -> `koshi-pty` but no direct
        // `koshi-daemon` -> `portable-pty` edge.
        ("koshi-daemon", &["koshi-core", "koshi-pty"]),
    ]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        Vec::<String>::new()
    );
}

#[test]
fn core_internal_dep_is_named() {
    let crate_dependencies = build_dependency_graph(&[("koshi-core", &["koshi-pty"])]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        vec!["forbidden edge: koshi-core -> koshi-pty \
             (koshi-core must not depend on internal crates)"
            .to_string()]
    );
}

#[test]
fn portable_pty_outside_pty_is_named() {
    let crate_dependencies = build_dependency_graph(&[("koshi-pane", &["portable-pty"])]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        vec!["forbidden edge: koshi-pane -> portable-pty \
             (portable-pty is owned only by koshi-pty)"
            .to_string()]
    );
}

#[test]
fn each_broken_rule_reports_its_own_text_and_the_list_is_sorted() {
    let crate_dependencies = build_dependency_graph(&[
        ("koshi-runtime", &["portable-pty"]),
        ("koshi-core", &["koshi-pty"]),
        ("koshi-pane", &["portable-pty"]),
    ]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        vec![
            "forbidden edge: koshi-core -> koshi-pty \
             (koshi-core must not depend on internal crates)"
                .to_string(),
            "forbidden edge: koshi-pane -> portable-pty \
             (portable-pty is owned only by koshi-pty)"
                .to_string(),
            "forbidden edge: koshi-runtime -> portable-pty \
             (portable-pty is owned only by koshi-pty)"
                .to_string(),
        ]
    );
}

#[test]
fn the_same_forbidden_edge_listed_twice_is_reported_once() {
    let crate_dependencies = build_dependency_graph(&[
        ("koshi-runtime", &["portable-pty", "portable-pty"]),
        ("koshi-runtime", &["portable-pty"]),
    ]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        vec!["forbidden edge: koshi-runtime -> portable-pty \
             (portable-pty is owned only by koshi-pty)"
            .to_string()]
    );
}

#[test]
fn koshi_core_may_depend_on_crates_outside_the_workspace() {
    let crate_dependencies =
        build_dependency_graph(&[("koshi-core", &["thiserror", "serde", "koshi"])]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        Vec::<String>::new()
    );
}

#[test]
fn a_crate_whose_name_only_starts_with_portable_pty_is_allowed_outside_the_pty_crate() {
    let crate_dependencies = build_dependency_graph(&[("koshi-runtime", &["portable-pty-extras"])]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        Vec::<String>::new()
    );
}

#[test]
fn empty_graph_has_no_violations() {
    assert_eq!(validate_dependency_edges(&[]), Vec::<String>::new());
}

#[test]
fn a_crate_with_no_dependencies_has_no_violations() {
    let crate_dependencies = build_dependency_graph(&[("koshi-runtime", &[])]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        Vec::<String>::new()
    );
}

#[test]
fn pty_crate_may_depend_on_portable_pty() {
    let crate_dependencies = build_dependency_graph(&[("koshi-pty", &["portable-pty"])]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        Vec::<String>::new()
    );
}

#[test]
fn every_forbidden_dependency_of_one_crate_is_named() {
    let crate_dependencies =
        build_dependency_graph(&[("koshi-core", &["portable-pty", "koshi-pty", "koshi-ipc"])]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        vec![
            "forbidden edge: koshi-core -> koshi-ipc \
             (koshi-core must not depend on internal crates)"
                .to_string(),
            "forbidden edge: koshi-core -> koshi-pty \
             (koshi-core must not depend on internal crates)"
                .to_string(),
            "forbidden edge: koshi-core -> portable-pty \
             (portable-pty is owned only by koshi-pty)"
                .to_string(),
        ]
    );
}

#[test]
fn list_direct_dependencies_keeps_only_workspace_crates_sorted_by_name() {
    let workspace_metadata = build_metadata(
        &["koshi-pty", "koshi-core"],
        &[
            (
                "koshi-pty",
                &format!(
                    "[{}]",
                    format_dependency_json("koshi-core", "null", false, None, None)
                ),
            ),
            (
                "tokio",
                &format!(
                    "[{}]",
                    format_dependency_json("mio", "null", false, None, None)
                ),
            ),
            ("koshi-core", "[]"),
        ],
    );
    assert_eq!(
        list_direct_dependencies(&workspace_metadata),
        vec![
            ("koshi-core".to_string(), vec![]),
            ("koshi-pty".to_string(), vec!["koshi-core".to_string()]),
        ]
    );
}

#[test]
fn list_direct_dependencies_uses_package_names_for_renamed_dependencies() {
    let dependency_json = format_dependency_json("portable-pty", "null", false, None, Some("pty"));
    let workspace_metadata = build_metadata(
        &["koshi-runtime"],
        &[("koshi-runtime", &format!("[{dependency_json}]"))],
    );

    assert_eq!(
        list_direct_dependencies(&workspace_metadata),
        vec![(
            "koshi-runtime".to_string(),
            vec!["portable-pty".to_string()]
        )]
    );
}

#[test]
fn list_direct_dependencies_sorts_and_deduplicates_every_dependency_kind() {
    let dependency_metadata_entries = [
        format_dependency_json("tokio", "\"dev\"", false, None, None),
        format_dependency_json("portable-pty", "null", false, None, None),
        format_dependency_json("cc", "\"build\"", true, Some("cfg(windows)"), None),
        format_dependency_json("tokio", "null", false, None, None),
    ]
    .join(",");
    let workspace_metadata = build_metadata(
        &["koshi-pty"],
        &[("koshi-pty", &format!("[{dependency_metadata_entries}]"))],
    );
    assert_eq!(
        list_direct_dependencies(&workspace_metadata),
        vec![(
            "koshi-pty".to_string(),
            vec![
                "cc".to_string(),
                "portable-pty".to_string(),
                "tokio".to_string()
            ]
        )]
    );
}
