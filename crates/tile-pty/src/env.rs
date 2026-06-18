use std::collections::BTreeMap;

use tile_core::process::{ShellKind, SpawnSpec};

/// Build the environment a child is spawned with: the inherited parent env,
/// plus tile's universal terminal identity and a shell-specific bootstrap, with
/// the caller's explicit `spec.env` overrides layered on top.
pub fn build_env(
    specs: &SpawnSpec,
    parent_env: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env = parent_env.clone();
    let specs_env = specs.env.clone();

    // Universal terminal identity. `xterm-256color` is a *compatibility
    // bootstrap*, not a permanent identity (TILE_07 staged plan): claiming
    // xterm-like behaviour is safe today, whereas `TERM=tile` would break many
    // apps because no `tile` terminfo entry is shipped. Only revisit once a
    // terminfo is published.
    env.insert("TERM".to_string(), "xterm-256color".to_string());
    env.insert("COLORTERM".to_string(), "truecolor".to_string());

    // Shell-specific bootstrap, applied per shell so a hack never leaks to a
    // shell that does not need it. Only zsh needs one today: an empty
    // `PROMPT_EOL_MARK` suppresses the inverse `%` zsh prints — via the
    // on-by-default `PROMPT_CR`/`PROMPT_SP` options — for output that lacks a
    // trailing newline. The match is exhaustive so adding a `ShellKind` forces
    // a deliberate decision about its bootstrap rather than silently inheriting
    // none.
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

    // Explicit `spec.env` overrides win over both the inherited parent env and
    // tile's own defaults above, so they are applied last.
    env.extend(specs_env);
    env
}

#[cfg(test)]
mod tests;
