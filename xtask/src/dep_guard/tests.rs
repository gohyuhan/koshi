//! Tests for the workspace dependency-direction guard.

use super::*;

fn graph(items: &[(&str, &[&str])]) -> Vec<CrateDeps> {
    items
        .iter()
        .map(|(krate, deps)| {
            (
                (*krate).to_string(),
                deps.iter().map(|d| (*d).to_string()).collect(),
            )
        })
        .collect()
}

/// Builds metadata from workspace member IDs and `(name, dependencies)`
/// package tuples. Each dependency string is a JSON array in cargo metadata
/// format, and each package ID equals its package name.
fn metadata(members: &[&str], packages: &[(&str, &str)]) -> Metadata {
    let packages: Vec<String> = packages
        .iter()
        .map(|(name, dependencies)| {
            format!(
                r#"{{"name":"{name}","version":"0.1.0","id":"{name}",
                    "dependencies":{dependencies},"targets":[],"features":{{}},
                    "manifest_path":"/w/{name}/Cargo.toml"}}"#
            )
        })
        .collect();
    let members: Vec<String> = members.iter().map(|id| format!("\"{id}\"")).collect();
    let json = format!(
        r#"{{"packages":[{}],"workspace_members":[{}],"workspace_root":"/w",
            "target_directory":"/w/target","version":1}}"#,
        packages.join(","),
        members.join(",")
    );
    MetadataCommand::parse(json).expect("hand-written metadata parses")
}

/// Builds one dependency object in cargo metadata JSON format. `kind` is the
/// raw JSON value `null`, `"dev"`, or `"build"`; `optional` is a JSON boolean;
/// `target` is a `cfg(...)` string or `null`; and `rename` is an alias or
/// `null`. The object omits `source`, `registry`, and `path`.
fn dependency(
    name: &str,
    kind: &str,
    optional: bool,
    target: Option<&str>,
    rename: Option<&str>,
) -> String {
    let target = match target {
        Some(cfg) => format!("\"{cfg}\""),
        None => "null".to_string(),
    };
    let rename = match rename {
        Some(alias) => format!("\"{alias}\""),
        None => "null".to_string(),
    };
    format!(
        r#"{{"name":"{name}","req":"*","kind":{kind},"optional":{optional},
            "uses_default_features":true,"features":[],"target":{target},"rename":{rename}}}"#
    )
}

#[test]
fn allowed_graph_has_no_violations() {
    let g = graph(&[
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
    assert_eq!(check(&g), Vec::<String>::new());
}

#[test]
fn core_internal_dep_is_named() {
    let g = graph(&[("koshi-core", &["koshi-pty"])]);
    assert_eq!(
        check(&g),
        vec!["forbidden edge: koshi-core -> koshi-pty \
             (koshi-core must not depend on internal crates)"
            .to_string()]
    );
}

#[test]
fn plugin_manager_runtime_dep_is_named() {
    let g = graph(&[("koshi-plugin-manager", &["koshi-runtime"])]);
    assert_eq!(
        check(&g),
        vec!["forbidden edge: koshi-plugin-manager -> koshi-runtime \
             (koshi-plugin-manager must not depend on runtime/ipc/host)"
            .to_string()]
    );
}

#[test]
fn plugin_manager_host_dep_is_named() {
    let g = graph(&[("koshi-plugin-manager", &["koshi-plugin-host"])]);
    assert_eq!(
        check(&g),
        vec!["forbidden edge: koshi-plugin-manager -> koshi-plugin-host \
             (koshi-plugin-manager must not depend on runtime/ipc/host)"
            .to_string()]
    );
}

#[test]
fn plugin_api_client_dep_is_named() {
    let g = graph(&[("koshi-plugin-api", &["koshi-client"])]);
    assert_eq!(
        check(&g),
        vec!["forbidden edge: koshi-plugin-api -> koshi-client \
             (koshi-plugin-api must not depend on client/renderer)"
            .to_string()]
    );
}

#[test]
fn plugin_api_renderer_dep_is_named() {
    let g = graph(&[("koshi-plugin-api", &["koshi-renderer"])]);
    assert_eq!(
        check(&g),
        vec!["forbidden edge: koshi-plugin-api -> koshi-renderer \
             (koshi-plugin-api must not depend on client/renderer)"
            .to_string()]
    );
}

#[test]
fn a_crate_other_than_plugin_api_may_depend_on_client_and_renderer() {
    let g = graph(&[("koshi", &["koshi-client", "koshi-renderer"])]);
    assert_eq!(check(&g), Vec::<String>::new());
}

#[test]
fn wasmtime_outside_host_is_named() {
    let g = graph(&[("koshi-runtime", &["wasmtime"])]);
    assert_eq!(
        check(&g),
        vec!["forbidden edge: koshi-runtime -> wasmtime \
             (wasmtime is owned only by koshi-plugin-host)"
            .to_string()]
    );
}

#[test]
fn portable_pty_outside_pty_is_named() {
    let g = graph(&[("koshi-pane", &["portable-pty"])]);
    assert_eq!(
        check(&g),
        vec!["forbidden edge: koshi-pane -> portable-pty \
             (portable-pty is owned only by koshi-pty)"
            .to_string()]
    );
}

#[test]
fn plugin_manager_ipc_dep_is_named() {
    let g = graph(&[("koshi-plugin-manager", &["koshi-ipc"])]);
    assert_eq!(
        check(&g),
        vec!["forbidden edge: koshi-plugin-manager -> koshi-ipc \
             (koshi-plugin-manager must not depend on runtime/ipc/host)"
            .to_string()]
    );
}

#[test]
fn each_broken_rule_reports_its_own_text_and_the_list_is_sorted() {
    let g = graph(&[
        ("koshi-runtime", &["wasmtime"]),
        ("koshi-core", &["koshi-pty"]),
        ("koshi-pane", &["portable-pty"]),
        ("koshi-plugin-manager", &["koshi-plugin-host"]),
    ]);
    assert_eq!(
        check(&g),
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
    let g = graph(&[
        ("koshi-runtime", &["wasmtime", "wasmtime"]),
        ("koshi-runtime", &["wasmtime"]),
    ]);
    assert_eq!(
        check(&g),
        vec!["forbidden edge: koshi-runtime -> wasmtime \
             (wasmtime is owned only by koshi-plugin-host)"
            .to_string()]
    );
}

#[test]
fn koshi_core_may_depend_on_crates_outside_the_workspace() {
    let g = graph(&[("koshi-core", &["thiserror", "serde", "koshi"])]);
    assert_eq!(check(&g), Vec::<String>::new());
}

#[test]
fn a_crate_whose_name_only_starts_with_wasmtime_is_allowed_outside_the_host() {
    let g = graph(&[("koshi-runtime", &["wasmtime-wasi"])]);
    assert_eq!(check(&g), Vec::<String>::new());
}

#[test]
fn empty_graph_has_no_violations() {
    assert_eq!(check(&[]), Vec::<String>::new());
}

#[test]
fn a_crate_with_no_dependencies_has_no_violations() {
    let g = graph(&[("koshi-plugin-manager", &[])]);
    assert_eq!(check(&g), Vec::<String>::new());
}

#[test]
fn plugin_host_may_depend_on_wasmtime() {
    let g = graph(&[("koshi-plugin-host", &["wasmtime"])]);
    assert_eq!(check(&g), Vec::<String>::new());
}

#[test]
fn pty_crate_may_depend_on_portable_pty() {
    let g = graph(&[("koshi-pty", &["portable-pty"])]);
    assert_eq!(check(&g), Vec::<String>::new());
}

#[test]
fn a_crate_other_than_plugin_manager_may_depend_on_runtime_ipc_and_host() {
    let g = graph(&[(
        "koshi",
        &["koshi-runtime", "koshi-ipc", "koshi-plugin-host"],
    )]);
    assert_eq!(check(&g), Vec::<String>::new());
}

#[test]
fn every_forbidden_dependency_of_one_crate_is_named() {
    let g = graph(&[("koshi-core", &["wasmtime", "koshi-pty", "koshi-ipc"])]);
    assert_eq!(
        check(&g),
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
    let m = metadata(
        &["koshi-pty", "koshi-core"],
        &[
            (
                "koshi-pty",
                &format!("[{}]", dependency("koshi-core", "null", false, None, None)),
            ),
            (
                "tokio",
                &format!("[{}]", dependency("mio", "null", false, None, None)),
            ),
            ("koshi-core", "[]"),
        ],
    );
    assert_eq!(
        direct_deps(&m),
        vec![
            ("koshi-core".to_string(), vec![]),
            ("koshi-pty".to_string(), vec!["koshi-core".to_string()]),
        ]
    );
}

#[test]
fn direct_deps_uses_package_name_for_renamed_dependencies() {
    let dependency = dependency("koshi-renderer", "null", false, None, Some("renderer"));
    let metadata = metadata(
        &["koshi-plugin-api"],
        &[("koshi-plugin-api", &format!("[{dependency}]"))],
    );

    assert_eq!(
        direct_deps(&metadata),
        vec![(
            "koshi-plugin-api".to_string(),
            vec!["koshi-renderer".to_string()]
        )]
    );
}

#[test]
fn direct_deps_sorts_and_deduplicates_dependencies_of_every_kind() {
    let deps = [
        dependency("tokio", "\"dev\"", false, None, None),
        dependency("portable-pty", "null", false, None, None),
        dependency("cc", "\"build\"", true, Some("cfg(windows)"), None),
        dependency("tokio", "null", false, None, None),
    ]
    .join(",");
    let m = metadata(&["koshi-pty"], &[("koshi-pty", &format!("[{deps}]"))]);
    assert_eq!(
        direct_deps(&m),
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
