//! Tests for the dependency-direction guard.

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

/// Parses a `cargo metadata` document whose workspace members are `members`
/// and whose package list is `packages`, each written as
/// `(name, dependencies)` with `dependencies` a JSON array of
/// [`dependency`] entries. A package's id is its name.
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

/// One entry of a package's `dependencies` array, in the shape `cargo
/// metadata` prints, without the `source`, `rename`, `registry`, and `path`
/// fields. `kind` is the JSON value of the `kind` field: `null` for a normal
/// dependency, `"dev"`, or `"build"`. `target` is the `cfg(...)` string of a
/// target-specific dependency.
fn dependency(name: &str, kind: &str, optional: bool, target: Option<&str>) -> String {
    let target = match target {
        Some(cfg) => format!("\"{cfg}\""),
        None => "null".to_string(),
    };
    format!(
        r#"{{"name":"{name}","req":"*","kind":{kind},"optional":{optional},
            "uses_default_features":true,"features":[],"target":{target}}}"#
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
        // Reaches wasmtime only through koshi-plugin-host, not as a direct
        // dependency.
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
                &format!("[{}]", dependency("koshi-core", "null", false, None)),
            ),
            (
                "tokio",
                &format!("[{}]", dependency("mio", "null", false, None)),
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
fn direct_deps_sorts_and_deduplicates_dependencies_of_every_kind() {
    let deps = [
        dependency("tokio", "\"dev\"", false, None),
        dependency("portable-pty", "null", false, None),
        dependency("cc", "\"build\"", true, Some("cfg(windows)")),
        dependency("tokio", "null", false, None),
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
