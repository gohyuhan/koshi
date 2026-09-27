//! Checks workspace dependency direction with `cargo metadata`.
//!
//! The guard rejects these direct edges:
//!
//! - `koshi-core` to any name that starts with `koshi-`.
//! - Any workspace crate other than `koshi-pty` to `portable-pty`.
//!
//! The guard reads each workspace crate's declared dependencies in all kinds:
//! normal, dev, build, optional, and target-specific. Cargo reports the
//! package name for a renamed dependency, so `pty = { package = "portable-pty" }`
//! is an edge to `portable-pty`. Transitive edges are not checked; for example,
//! `koshi-daemon` -> `koshi-pty` -> `portable-pty` passes.

use std::collections::BTreeSet;
use std::process::ExitCode;

use cargo_metadata::{Metadata, MetadataCommand};

/// A crate name paired with its direct dependency names.
type CrateDependencies = (String, Vec<String>);

/// Runs `cargo metadata` in the current directory and checks every workspace
/// crate against the module's rules.
///
/// If all edges pass, prints `dep-guard: ok (N crates checked)` on stdout,
/// where `N` is the number of workspace crates, and returns
/// [`ExitCode::SUCCESS`].
///
/// If an edge fails, prints one `dep-guard: forbidden edge: ...` line per
/// violation on stderr, then `dep-guard: N violation(s)`, and returns
/// [`ExitCode::FAILURE`].
///
/// If `cargo metadata` fails, prints ``dep-guard: `cargo metadata` failed: ...``
/// on stderr, returns [`ExitCode::FAILURE`], and checks no edge.
pub fn run_dependency_guard() -> ExitCode {
    let metadata = match MetadataCommand::new().exec() {
        Ok(metadata) => metadata,
        Err(metadata_error) => {
            eprintln!("dep-guard: `cargo metadata` failed: {metadata_error}");
            return ExitCode::FAILURE;
        }
    };

    let crate_dependencies = list_direct_dependencies(&metadata);
    let violations = validate_dependency_edges(&crate_dependencies);
    if violations.is_empty() {
        println!(
            "dep-guard: ok ({} crates checked)",
            crate_dependencies.len()
        );
        return ExitCode::SUCCESS;
    }

    for violation in &violations {
        eprintln!("dep-guard: {violation}");
    }
    eprintln!("dep-guard: {} violation(s)", violations.len());
    ExitCode::FAILURE
}

/// Returns one `(crate, dependencies)` pair for each workspace crate.
/// Dependencies include normal, dev, build, optional, and target-specific
/// manifest entries. Crates and dependency names are sorted, and duplicate
/// dependency names occur once.
fn list_direct_dependencies(metadata: &Metadata) -> Vec<CrateDependencies> {
    let mut crate_dependencies: Vec<CrateDependencies> = metadata
        .workspace_packages()
        .iter()
        .map(|workspace_package| {
            let mut dependency_names: Vec<String> = workspace_package
                .dependencies
                .iter()
                .map(|dependency| dependency.name.to_string())
                .collect();
            dependency_names.sort();
            dependency_names.dedup();
            (workspace_package.name.to_string(), dependency_names)
        })
        .collect();
    crate_dependencies.sort_by(|left_crate, right_crate| left_crate.0.cmp(&right_crate.0));
    crate_dependencies
}

/// Returns sorted, duplicate-free messages for forbidden edges in the crate dependency graph.
/// Returns an empty vector when every edge is allowed.
fn validate_dependency_edges(crate_dependencies: &[CrateDependencies]) -> Vec<String> {
    let mut violations = BTreeSet::new();

    for (crate_name, dependency_names) in crate_dependencies {
        for dependency_name in dependency_names {
            if crate_name == "koshi-core" && dependency_name.starts_with("koshi-") {
                violations.insert(format_forbidden_edge(
                    crate_name,
                    dependency_name,
                    "koshi-core must not depend on internal crates",
                ));
            }
            if dependency_name == "portable-pty" && crate_name != "koshi-pty" {
                violations.insert(format_forbidden_edge(
                    crate_name,
                    dependency_name,
                    "portable-pty is owned only by koshi-pty",
                ));
            }
        }
    }

    violations.into_iter().collect()
}

/// Formats one forbidden edge with its source crate, dependency crate, and guard rule.
fn format_forbidden_edge(
    source_crate_name: &str,
    dependency_crate_name: &str,
    guard_rule: &str,
) -> String {
    format!("forbidden edge: {source_crate_name} -> {dependency_crate_name} ({guard_rule})")
}

#[cfg(test)]
mod tests;
