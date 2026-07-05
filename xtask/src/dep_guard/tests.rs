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
        ("tile-core", &[]),
        ("tile-pty", &["tile-core", "portable-pty"]),
        (
            "tile-plugin-host",
            &["tile-core", "tile-plugin-api", "wasmtime"],
        ),
        (
            "tile-plugin-manager",
            &["tile-core", "tile-plugin-api", "tile-storage"],
        ),
        // Legitimately reaches wasmtime via the host; not a direct dep here.
        (
            "tile-runtime",
            &["tile-core", "tile-plugin-manager", "tile-plugin-host"],
        ),
    ]);
    assert!(check(&g).is_empty());
}

#[test]
fn core_internal_dep_is_named() {
    let g = graph(&[("tile-core", &["tile-pty"])]);
    let v = check(&g);
    assert_eq!(v.len(), 1);
    assert!(v[0].contains("tile-core -> tile-pty"), "{}", v[0]);
}

#[test]
fn plugin_manager_runtime_dep_is_named() {
    let g = graph(&[("tile-plugin-manager", &["tile-runtime"])]);
    let v = check(&g);
    assert_eq!(v.len(), 1);
    assert!(
        v[0].contains("tile-plugin-manager -> tile-runtime"),
        "{}",
        v[0]
    );
}

#[test]
fn plugin_manager_host_dep_is_named() {
    let g = graph(&[("tile-plugin-manager", &["tile-plugin-host"])]);
    let v = check(&g);
    assert_eq!(v.len(), 1);
    assert!(
        v[0].contains("tile-plugin-manager -> tile-plugin-host"),
        "{}",
        v[0]
    );
}

#[test]
fn wasmtime_outside_host_is_named() {
    let g = graph(&[("tile-runtime", &["wasmtime"])]);
    let v = check(&g);
    assert_eq!(v.len(), 1);
    assert!(v[0].contains("tile-runtime -> wasmtime"), "{}", v[0]);
}

#[test]
fn portable_pty_outside_pty_is_named() {
    let g = graph(&[("tile-pane", &["portable-pty"])]);
    let v = check(&g);
    assert_eq!(v.len(), 1);
    assert!(v[0].contains("tile-pane -> portable-pty"), "{}", v[0]);
}
