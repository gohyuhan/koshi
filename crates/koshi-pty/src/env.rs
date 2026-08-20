//! Environment variable overlay for spawned child processes.
//!
//! Builds the universal terminal identity (`TERM=xterm-256color`, `COLORTERM=truecolor`)
//! and applies shell-specific bootstrap variables, with the caller's overrides on top.
//! This is an overlay only — applied over the inherited parent environment.

use std::collections::BTreeMap;

use koshi_core::process::{ShellKind, SpawnSpec};

/// Build koshi's environment *overlay* for a spawned child: the universal
/// terminal identity and a shell-specific bootstrap, with the caller's explicit
/// `spec.env` overrides layered on top.
///
/// This is only the overlay, not the full environment — the caller applies it
/// over the inherited parent env (which `CommandBuilder` keeps), so parent vars
/// survive and each overlay key overwrites its inherited counterpart. On Windows
/// `portable-pty` folds names case-insensitively, so an override replaces a
/// differently-cased inherited key.
pub fn build_env(specs: &SpawnSpec) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();

    // Universal terminal identity, set for every shell. `TERM` names the
    // terminal type whose feature set the child assumes, and `COLORTERM` names
    // the color depth it may use.
    env.insert("TERM".to_string(), "xterm-256color".to_string());
    env.insert("COLORTERM".to_string(), "truecolor".to_string());

    // Shell-specific bootstrap. zsh alone gets one: an empty `PROMPT_EOL_MARK`
    // stops the inverse `%` that zsh's on-by-default `PROMPT_CR`/`PROMPT_SP`
    // options print after output with no trailing newline. Every other shell
    // gets no bootstrap key. The match lists every `ShellKind`.
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

    // `spec.env` is applied last, so each of its keys overwrites the koshi
    // default of the same name above.
    for (key, value) in &specs.env {
        env.insert(key.to_string(), value.to_string());
    }
    env
}

#[cfg(test)]
mod tests;
