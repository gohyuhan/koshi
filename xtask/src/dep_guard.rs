//! Dependency-direction guard.
//!
//! [`run`] reads the workspace dependency graph with `cargo metadata` and
//! reports every edge these five rules forbid:
//!
//! - `koshi-core` depends on no crate whose name starts with `koshi-`.
//! - `koshi-plugin-manager` depends on none of `koshi-runtime`, `koshi-ipc`,
//!   `koshi-plugin-host`.
//! - `koshi-plugin-api` depends on neither `koshi-client` nor `koshi-renderer`.
//! - Only `koshi-plugin-host` depends on `wasmtime`.
//! - Only `koshi-pty` depends on `portable-pty`.
//!
//! Each rule reads the dependencies a crate declares in its own manifest, of
//! every kind (normal, dev, build), including optional and target-specific
//! ones. A renamed dependency counts under its package name, so
//! `wt = { package = "wasmtime" }` is an edge to `wasmtime`. A dependency
//! reached through another crate is not an edge here, so `koshi-runtime` ->
//! `koshi-plugin-host` -> `wasmtime` passes.

use std::collections::BTreeSet;
use std::process::ExitCode;

use cargo_metadata::{Metadata, MetadataCommand};

/// A crate name paired with the names of its direct dependencies.
type CrateDeps = (String, Vec<String>);

/// Runs `cargo metadata` from the current directory and checks every
/// workspace crate against the rules in the module doc.
///
/// No rule broken: prints `dep-guard: ok (N crates checked)` on stdout, where
/// `N` counts the workspace crates, and returns [`ExitCode::SUCCESS`].
///
/// Rules broken: prints one `dep-guard: forbidden edge: ...` line per
/// forbidden edge on stderr, then `dep-guard: N violation(s)`, and returns
/// [`ExitCode::FAILURE`].
///
/// A `cargo metadata` run that fails prints its error on stderr and returns
/// [`ExitCode::FAILURE`] with no rule checked.
pub fn run() -> ExitCode {
    let metadata = match MetadataCommand::new().exec() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("dep-guard: `cargo metadata` failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let graph = direct_deps(&metadata);
    let violations = check(&graph);
    if violations.is_empty() {
        println!("dep-guard: ok ({} crates checked)", graph.len());
        return ExitCode::SUCCESS;
    }

    for v in &violations {
        eprintln!("dep-guard: {v}");
    }
    eprintln!("dep-guard: {} violation(s)", violations.len());
    ExitCode::FAILURE
}

/// Returns every workspace crate paired with the names of the dependencies
/// its manifest declares, of every kind. Crates are sorted by name; each
/// dependency list is sorted and deduplicated.
fn direct_deps(metadata: &Metadata) -> Vec<CrateDeps> {
    let mut graph: Vec<CrateDeps> = metadata
        .workspace_packages()
        .iter()
        .map(|pkg| {
            let mut deps: Vec<String> = pkg
                .dependencies
                .iter()
                .map(|dep| dep.name.to_string())
                .collect();
            deps.sort();
            deps.dedup();
            (pkg.name.to_string(), deps)
        })
        .collect();
    graph.sort_by(|a, b| a.0.cmp(&b.0));
    graph
}

/// Returns one message per edge that a rule in the module doc forbids, sorted
/// and deduplicated. An empty vector means every edge in `graph` is allowed.
pub fn check(graph: &[CrateDeps]) -> Vec<String> {
    let mut violations = BTreeSet::new();

    for (krate, deps) in graph {
        for dep in deps {
            if krate == "koshi-core" && dep.starts_with("koshi-") {
                violations.insert(edge(
                    krate,
                    dep,
                    "koshi-core must not depend on internal crates",
                ));
            }
            if krate == "koshi-plugin-manager"
                && matches!(
                    dep.as_str(),
                    "koshi-runtime" | "koshi-ipc" | "koshi-plugin-host"
                )
            {
                violations.insert(edge(
                    krate,
                    dep,
                    "koshi-plugin-manager must not depend on runtime/ipc/host",
                ));
            }
            if krate == "koshi-plugin-api"
                && matches!(dep.as_str(), "koshi-client" | "koshi-renderer")
            {
                violations.insert(edge(
                    krate,
                    dep,
                    "koshi-plugin-api must not depend on client/renderer",
                ));
            }
            if dep == "wasmtime" && krate != "koshi-plugin-host" {
                violations.insert(edge(
                    krate,
                    dep,
                    "wasmtime is owned only by koshi-plugin-host",
                ));
            }
            if dep == "portable-pty" && krate != "koshi-pty" {
                violations.insert(edge(krate, dep, "portable-pty is owned only by koshi-pty"));
            }
        }
    }

    violations.into_iter().collect()
}

/// Formats `forbidden edge: {from} -> {to} ({rule})`.
fn edge(from: &str, to: &str, rule: &str) -> String {
    format!("forbidden edge: {from} -> {to} ({rule})")
}

#[cfg(test)]
mod tests;
