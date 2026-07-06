//! Unit tests for [`build_env`]: per-shell bootstrap snapshots and `spec.env`
//! override precedence over koshi's own defaults.
//!
//! `build_env` returns only koshi's *overlay*; parent-env preservation and the
//! Windows case-fold are properties of the spawn path (`portable-pty` applies
//! the overlay over the un-cleared inherited env) and are covered there.

use super::*;
use std::path::PathBuf;

/// Build a minimal [`SpawnSpec`] for the given shell with the supplied env
/// overrides; program/args/cwd are irrelevant to `build_env`.
fn spec(shell_kind: ShellKind, env: BTreeMap<String, String>) -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from("/bin/sh"),
        args: Vec::new(),
        cwd: None,
        env,
        shell_kind,
    }
}

#[test]
fn universal_vars_set_for_every_shell() {
    for kind in [
        ShellKind::Zsh,
        ShellKind::Bash,
        ShellKind::Fish,
        ShellKind::PowerShell,
        ShellKind::Nu,
        ShellKind::Other("elvish".to_string()),
    ] {
        let env = build_env(&spec(kind.clone(), BTreeMap::new()));
        assert_eq!(
            env.get("TERM").map(String::as_str),
            Some("xterm-256color"),
            "TERM for {kind:?}"
        );
        assert_eq!(
            env.get("COLORTERM").map(String::as_str),
            Some("truecolor"),
            "COLORTERM for {kind:?}"
        );
    }
}

#[test]
fn zsh_sets_empty_prompt_eol_mark() {
    let env = build_env(&spec(ShellKind::Zsh, BTreeMap::new()));
    assert_eq!(
        env.get("PROMPT_EOL_MARK").map(String::as_str),
        Some(""),
        "zsh must set PROMPT_EOL_MARK to the empty string to suppress the `%`"
    );
}

#[test]
fn non_zsh_shells_have_no_prompt_eol_mark() {
    for kind in [
        ShellKind::Bash,
        ShellKind::Fish,
        ShellKind::PowerShell,
        ShellKind::Nu,
        ShellKind::Other("elvish".to_string()),
    ] {
        let env = build_env(&spec(kind.clone(), BTreeMap::new()));
        assert!(
            !env.contains_key("PROMPT_EOL_MARK"),
            "{kind:?} must not carry the zsh-specific PROMPT_EOL_MARK"
        );
    }
}

#[test]
fn overlay_carries_only_koshi_keys_and_spec_env() {
    // No parent env is mixed in: a vanilla bash overlay is exactly the two
    // universal keys, nothing inherited.
    let env = build_env(&spec(ShellKind::Bash, BTreeMap::new()));
    assert!(
        !env.contains_key("HOME"),
        "overlay must not invent parent keys"
    );
    assert!(
        !env.contains_key("PATH"),
        "overlay must not invent parent keys"
    );
}

#[test]
fn spec_env_overrides_koshi_default_and_adds_keys() {
    let mut overrides = BTreeMap::new();
    // Collides with a koshi default...
    overrides.insert("TERM".to_string(), "screen-256color".to_string());
    // ...and adds a brand-new key.
    overrides.insert("MY_VAR".to_string(), "custom".to_string());

    let env = build_env(&spec(ShellKind::Bash, overrides));
    assert_eq!(
        env.get("TERM").map(String::as_str),
        Some("screen-256color"),
        "explicit spec.env TERM must override koshi's default"
    );
    assert_eq!(env.get("MY_VAR").map(String::as_str), Some("custom"));
}

#[test]
fn spec_env_can_override_zsh_prompt_eol_mark() {
    let mut overrides = BTreeMap::new();
    overrides.insert("PROMPT_EOL_MARK".to_string(), "DONE".to_string());
    let env = build_env(&spec(ShellKind::Zsh, overrides));
    assert_eq!(
        env.get("PROMPT_EOL_MARK").map(String::as_str),
        Some("DONE"),
        "spec.env override must win over the zsh bootstrap default"
    );
}

#[test]
fn full_snapshot_zsh() {
    let env = build_env(&spec(ShellKind::Zsh, BTreeMap::new()));
    let mut expected = BTreeMap::new();
    expected.insert("TERM".to_string(), "xterm-256color".to_string());
    expected.insert("COLORTERM".to_string(), "truecolor".to_string());
    expected.insert("PROMPT_EOL_MARK".to_string(), String::new());
    assert_eq!(env, expected);
}

#[test]
fn full_snapshot_bash_has_no_shell_specific_keys() {
    let env = build_env(&spec(ShellKind::Bash, BTreeMap::new()));
    let mut expected = BTreeMap::new();
    expected.insert("TERM".to_string(), "xterm-256color".to_string());
    expected.insert("COLORTERM".to_string(), "truecolor".to_string());
    assert_eq!(env, expected);
}
