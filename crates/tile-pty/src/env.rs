use std::collections::BTreeMap;

use tile_core::process::{ShellKind, SpawnSpec};

/// Build tile's environment *overlay* for a spawned child: the universal
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

    // Explicit `spec.env` overrides win over tile's own defaults above, so they
    // are applied last. They also win over the inherited parent env because the
    // caller layers this whole overlay on top of it at spawn time.
    for (key, value) in &specs.env {
        env.insert(key.to_string(), value.to_string());
    }
    env
}

#[cfg(test)]
mod tests;
