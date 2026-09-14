//! Tests for the workspace dependency-direction guard.

use super::*;

fn build_dependency_graph(crate_dependency_pairs: &[(&str, &[&str])]) -> Vec<CrateDependencies> {
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
fn build_metadata(members: &[&str], packages: &[(&str, &str)]) -> Metadata {
    let package_json_entries: Vec<String> = packages
        .iter()
        .map(|(package_name, dependency_json)| {
            format!(
                r#"{{"name":"{package_name}","version":"0.1.0","id":"{package_name}",
                    "dependencies":{dependency_json},"targets":[],"features":{{}},
                    "manifest_path":"/w/{package_name}/Cargo.toml"}}"#
            )
        })
        .collect();
    let workspace_member_json_entries: Vec<String> = members
        .iter()
        .map(|member_identifier| format!("\"{member_identifier}\""))
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
        (
            "koshi-plugin-host",
            &["koshi-core", "koshi-plugin-api", "wasmtime"],
        ),
        (
            "koshi-plugin-manager",
            &["koshi-core", "koshi-plugin-api", "koshi-storage"],
        ),
        ("koshi-plugin-api", &["koshi-core"]),
        // This graph has `koshi-runtime` -> `koshi-plugin-host` but no direct
        // `koshi-runtime` -> `wasmtime` edge.
        (
            "koshi-runtime",
            &["koshi-core", "koshi-plugin-manager", "koshi-plugin-host"],
        ),
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
fn plugin_manager_runtime_dep_is_named() {
    let crate_dependencies =
        build_dependency_graph(&[("koshi-plugin-manager", &["koshi-runtime"])]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        vec!["forbidden edge: koshi-plugin-manager -> koshi-runtime \
             (koshi-plugin-manager must not depend on runtime/ipc/host)"
            .to_string()]
    );
}

#[test]
fn plugin_manager_host_dep_is_named() {
    let crate_dependencies =
        build_dependency_graph(&[("koshi-plugin-manager", &["koshi-plugin-host"])]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        vec!["forbidden edge: koshi-plugin-manager -> koshi-plugin-host \
             (koshi-plugin-manager must not depend on runtime/ipc/host)"
            .to_string()]
    );
}

#[test]
fn plugin_api_client_dep_is_named() {
    let crate_dependencies = build_dependency_graph(&[("koshi-plugin-api", &["koshi-client"])]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        vec!["forbidden edge: koshi-plugin-api -> koshi-client \
             (koshi-plugin-api must not depend on client/renderer)"
            .to_string()]
    );
}

#[test]
fn plugin_api_renderer_dep_is_named() {
    let crate_dependencies = build_dependency_graph(&[("koshi-plugin-api", &["koshi-renderer"])]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        vec!["forbidden edge: koshi-plugin-api -> koshi-renderer \
             (koshi-plugin-api must not depend on client/renderer)"
            .to_string()]
    );
}

#[test]
fn a_crate_other_than_plugin_api_may_depend_on_client_and_renderer() {
    let crate_dependencies =
        build_dependency_graph(&[("koshi", &["koshi-client", "koshi-renderer"])]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        Vec::<String>::new()
    );
}

#[test]
fn wasmtime_outside_host_is_named() {
    let crate_dependencies = build_dependency_graph(&[("koshi-runtime", &["wasmtime"])]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        vec!["forbidden edge: koshi-runtime -> wasmtime \
             (wasmtime is owned only by koshi-plugin-host)"
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
fn plugin_manager_ipc_dep_is_named() {
    let crate_dependencies = build_dependency_graph(&[("koshi-plugin-manager", &["koshi-ipc"])]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        vec!["forbidden edge: koshi-plugin-manager -> koshi-ipc \
             (koshi-plugin-manager must not depend on runtime/ipc/host)"
            .to_string()]
    );
}

#[test]
fn each_broken_rule_reports_its_own_text_and_the_list_is_sorted() {
    let crate_dependencies = build_dependency_graph(&[
        ("koshi-runtime", &["wasmtime"]),
        ("koshi-core", &["koshi-pty"]),
        ("koshi-pane", &["portable-pty"]),
        ("koshi-plugin-manager", &["koshi-plugin-host"]),
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
            "forbidden edge: koshi-plugin-manager -> koshi-plugin-host \
             (koshi-plugin-manager must not depend on runtime/ipc/host)"
                .to_string(),
            "forbidden edge: koshi-runtime -> wasmtime \
             (wasmtime is owned only by koshi-plugin-host)"
                .to_string(),
        ]
    );
}

#[test]
fn the_same_forbidden_edge_listed_twice_is_reported_once() {
    let crate_dependencies = build_dependency_graph(&[
        ("koshi-runtime", &["wasmtime", "wasmtime"]),
        ("koshi-runtime", &["wasmtime"]),
    ]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        vec!["forbidden edge: koshi-runtime -> wasmtime \
             (wasmtime is owned only by koshi-plugin-host)"
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
fn a_crate_whose_name_only_starts_with_wasmtime_is_allowed_outside_the_host() {
    let crate_dependencies = build_dependency_graph(&[("koshi-runtime", &["wasmtime-wasi"])]);
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
    let crate_dependencies = build_dependency_graph(&[("koshi-plugin-manager", &[])]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        Vec::<String>::new()
    );
}

#[test]
fn plugin_host_may_depend_on_wasmtime() {
    let crate_dependencies = build_dependency_graph(&[("koshi-plugin-host", &["wasmtime"])]);
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
fn a_crate_other_than_plugin_manager_may_depend_on_runtime_ipc_and_host() {
    let crate_dependencies = build_dependency_graph(&[(
        "koshi",
        &["koshi-runtime", "koshi-ipc", "koshi-plugin-host"],
    )]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        Vec::<String>::new()
    );
}

#[test]
fn every_forbidden_dependency_of_one_crate_is_named() {
    let crate_dependencies =
        build_dependency_graph(&[("koshi-core", &["wasmtime", "koshi-pty", "koshi-ipc"])]);
    assert_eq!(
        validate_dependency_edges(&crate_dependencies),
        vec![
            "forbidden edge: koshi-core -> koshi-ipc \
             (koshi-core must not depend on internal crates)"
                .to_string(),
            "forbidden edge: koshi-core -> koshi-pty \
             (koshi-core must not depend on internal crates)"
                .to_string(),
            "forbidden edge: koshi-core -> wasmtime \
             (wasmtime is owned only by koshi-plugin-host)"
                .to_string(),
        ]
    );
}

#[test]
fn direct_deps_keeps_only_workspace_members_sorted_by_name() {
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
fn direct_deps_uses_package_name_for_renamed_dependencies() {
    let dependency_json =
        format_dependency_json("koshi-renderer", "null", false, None, Some("renderer"));
    let workspace_metadata = build_metadata(
        &["koshi-plugin-api"],
        &[("koshi-plugin-api", &format!("[{dependency_json}]"))],
    );

    assert_eq!(
        list_direct_dependencies(&workspace_metadata),
        vec![(
            "koshi-plugin-api".to_string(),
            vec!["koshi-renderer".to_string()]
        )]
    );
}

#[test]
fn direct_deps_sorts_and_deduplicates_dependencies_of_every_kind() {
    let deps = [
        format_dependency_json("tokio", "\"dev\"", false, None, None),
        format_dependency_json("portable-pty", "null", false, None, None),
        format_dependency_json("cc", "\"build\"", true, Some("cfg(windows)"), None),
        format_dependency_json("tokio", "null", false, None, None),
    ]
    .join(",");
    let workspace_metadata = build_metadata(&["koshi-pty"], &[("koshi-pty", &format!("[{deps}]"))]);
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
