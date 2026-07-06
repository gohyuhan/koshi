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
        // Legitimately reaches wasmtime via the host; not a direct dep here.
        (
            "koshi-runtime",
            &["koshi-core", "koshi-plugin-manager", "koshi-plugin-host"],
        ),
    ]);
    assert!(check(&g).is_empty());
}

#[test]
fn core_internal_dep_is_named() {
    let g = graph(&[("koshi-core", &["koshi-pty"])]);
    let v = check(&g);
    assert_eq!(v.len(), 1);
    assert!(v[0].contains("koshi-core -> koshi-pty"), "{}", v[0]);
}

#[test]
fn plugin_manager_runtime_dep_is_named() {
    let g = graph(&[("koshi-plugin-manager", &["koshi-runtime"])]);
    let v = check(&g);
    assert_eq!(v.len(), 1);
    assert!(
        v[0].contains("koshi-plugin-manager -> koshi-runtime"),
        "{}",
        v[0]
    );
}

#[test]
fn plugin_manager_host_dep_is_named() {
    let g = graph(&[("koshi-plugin-manager", &["koshi-plugin-host"])]);
    let v = check(&g);
    assert_eq!(v.len(), 1);
    assert!(
        v[0].contains("koshi-plugin-manager -> koshi-plugin-host"),
        "{}",
        v[0]
    );
}

#[test]
fn wasmtime_outside_host_is_named() {
    let g = graph(&[("koshi-runtime", &["wasmtime"])]);
    let v = check(&g);
    assert_eq!(v.len(), 1);
    assert!(v[0].contains("koshi-runtime -> wasmtime"), "{}", v[0]);
}

#[test]
fn portable_pty_outside_pty_is_named() {
    let g = graph(&[("koshi-pane", &["portable-pty"])]);
    let v = check(&g);
    assert_eq!(v.len(), 1);
    assert!(v[0].contains("koshi-pane -> portable-pty"), "{}", v[0]);
}
