//! Dependency-direction guard.
//!
//! Loads the workspace dependency graph via `cargo metadata` and enforces the
//! architecture's load-bearing invariants. These four hold regardless of how
//! the crates evolve, so the guard needs no per-crate allow-list to maintain:
//!
//! - `tile-core` has zero internal `tile-*` dependencies (it is the foundation).
//! - `tile-plugin-manager` does not depend on `tile-runtime`, `tile-ipc`, or
//!   `tile-plugin-host` (one-way arrow: runtime/cli depend on the manager).
//! - `wasmtime` is a direct dependency of `tile-plugin-host` only.
//! - `portable-pty` is a direct dependency of `tile-pty` only.
//!
//! Containment is checked on *direct* dependencies, not transitively: crates
//! legitimately reach the heavy dependency through its rightful owner (e.g.
//! `tile-runtime` -> `tile-plugin-host` -> `wasmtime`), and the real failure
//! mode this guards against is a stray `cargo add` in the wrong crate.
//!
//! The architecture's full per-crate dependency matrix is intentionally *not*
//! encoded here: it would require editing this file on every PR that adds a
//! legitimate internal dependency, and several of its edges are interpretation.
//! Cargo already rejects dependency cycles; this guard covers the two things it
//! does not — foundation isolation and heavy-dependency containment.
//!
//! Both regular and dev-dependencies are checked, so test-only coupling cannot
//! quietly cross an isolation line either. A dev-dependency on a test-support
//! crate is fine wherever the direction rules above allow it.

use std::collections::BTreeSet;
use std::process::ExitCode;

use cargo_metadata::{Metadata, MetadataCommand};

/// One workspace crate and the names of its direct dependencies.
type CrateDeps = (String, Vec<String>);

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

/// Direct dependencies declared by each workspace crate, sorted for determinism.
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

/// Returns one message per forbidden edge; empty means the graph is allowed.
pub fn check(graph: &[CrateDeps]) -> Vec<String> {
    let mut violations = BTreeSet::new();

    for (krate, deps) in graph {
        for dep in deps {
            // tile-core is the foundation: no internal dependencies.
            if krate == "tile-core" && is_internal_crate(dep) {
                violations.insert(edge(
                    krate,
                    dep,
                    "tile-core must not depend on internal crates",
                ));
            }
            // Plugin manager arrow is one-way: runtime/cli depend on it, never the reverse.
            if krate == "tile-plugin-manager"
                && matches!(
                    dep.as_str(),
                    "tile-runtime" | "tile-ipc" | "tile-plugin-host"
                )
            {
                violations.insert(edge(
                    krate,
                    dep,
                    "tile-plugin-manager must not depend on runtime/ipc/host",
                ));
            }
            // Heavy dependencies are owned by exactly one crate.
            if dep == "wasmtime" && krate != "tile-plugin-host" {
                violations.insert(edge(
                    krate,
                    dep,
                    "wasmtime is owned only by tile-plugin-host",
                ));
            }
            if dep == "portable-pty" && krate != "tile-pty" {
                violations.insert(edge(krate, dep, "portable-pty is owned only by tile-pty"));
            }
        }
    }

    violations.into_iter().collect()
}

fn is_internal_crate(name: &str) -> bool {
    name.starts_with("tile-")
}

fn edge(from: &str, to: &str, why: &str) -> String {
    format!("forbidden edge: {from} -> {to} ({why})")
}

#[cfg(test)]
mod tests;
