//! Builds the environment overlay for a spawned child.
//!
//! The overlay sets the terminal identity, adds the zsh bootstrap variable, and
//! applies the caller's overrides over the inherited parent environment.

use std::collections::BTreeMap;

use koshi_core::process::{ShellKind, SpawnSpec};

/// Build the environment overlay for a spawned child.
///
/// The overlay sets `TERM=xterm-256color` and `COLORTERM=truecolor`. A zsh
/// spec also gets an empty `PROMPT_EOL_MARK`; other shell kinds do not. Entries
/// in `spec.env` are applied last and replace defaults with the same key.
///
/// The returned map contains only overlay entries. The caller applies it over
/// the inherited parent environment; on Windows, `portable-pty` matches names
/// case-insensitively.
pub fn build_env(spec: &SpawnSpec) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();

    // Set the terminal identity for every shell.
    env.insert("TERM".to_string(), "xterm-256color".to_string());
    env.insert("COLORTERM".to_string(), "truecolor".to_string());

    // zsh gets an empty `PROMPT_EOL_MARK`, which removes the inverse `%` that
    // its `PROMPT_CR`/`PROMPT_SP` options print after output with no newline.
    // Other shell kinds get no bootstrap key.
    match spec.shell_kind {
        ShellKind::Zsh => {
            env.insert("PROMPT_EOL_MARK".to_string(), String::new());
        }
        ShellKind::Bash
        | ShellKind::Fish
        | ShellKind::PowerShell
        | ShellKind::Nu
        | ShellKind::Other(_) => {}
    }

    // Apply explicit entries last so they replace matching defaults.
    env.extend(spec.env.clone());
    env
}

#[cfg(test)]
mod tests;
