//! Environment variable overlay for spawned child processes.
//!
//! Builds the universal terminal identity (`TERM=xterm-256color`, `COLORTERM=truecolor`)
//! and applies shell-specific bootstrap variables, with the caller's overrides on top.
//! This is an overlay only — applied over the inherited parent environment.

use std::collections::BTreeMap;

use koshi_core::process::{ShellKind, SpawnSpec};

/// Build koshi's environment *overlay* for a spawned child: the universal
/// terminal identity and a shell-specific bootstrap, with the caller's explicit
/// `specs.env` overrides layered on top.
///
/// The map is only the overlay, not the full environment. The caller applies
/// it over the inherited parent environment; each overlay key replaces the
/// inherited key of the same name, and on Windows `portable-pty` matches the
/// names case-insensitively.
pub fn build_env(specs: &SpawnSpec) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();

    // Universal terminal identity, set for every shell. `TERM` names the
    // terminal type whose feature set the child assumes, and `COLORTERM` names
    // the color depth it may use.
    env.insert("TERM".to_string(), "xterm-256color".to_string());
    env.insert("COLORTERM".to_string(), "truecolor".to_string());

    // Shell-specific bootstrap. zsh alone gets one: an empty `PROMPT_EOL_MARK`
    // turns off the inverse `%` that zsh's on-by-default `PROMPT_CR`/`PROMPT_SP`
    // options print after output with no trailing newline. Every other shell
    // gets no bootstrap key.
    match specs.shell_kind {
        ShellKind::Zsh => {
            env.insert("PROMPT_EOL_MARK".to_string(), String::new());
        }
        ShellKind::Bash
        | ShellKind::Fish
        | ShellKind::PowerShell
        | ShellKind::Nu
        | ShellKind::Other(_) => {}
    }

    // `specs.env` is applied last; each of its keys overwrites the koshi
    // default of the same name above.
    env.extend(specs.env.clone());
    env
}

#[cfg(test)]
mod tests;
