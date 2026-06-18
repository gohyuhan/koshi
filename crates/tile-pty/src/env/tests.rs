//! Unit tests for [`build_env`]: per-shell bootstrap snapshots, parent-env
//! preservation, and `spec.env` override precedence.

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

/// A representative parent environment carrying a key tile never touches.
fn parent() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("HOME".to_string(), "/home/user".to_string());
    env.insert("PATH".to_string(), "/usr/bin".to_string());
    env
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
        let env = build_env(&spec(kind.clone(), BTreeMap::new()), &parent());
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
    let env = build_env(&spec(ShellKind::Zsh, BTreeMap::new()), &parent());
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
        let env = build_env(&spec(kind.clone(), BTreeMap::new()), &parent());
        assert!(
            !env.contains_key("PROMPT_EOL_MARK"),
            "{kind:?} must not carry the zsh-specific PROMPT_EOL_MARK"
        );
    }
}

#[test]
fn parent_env_preserved_when_not_overridden() {
    let env = build_env(&spec(ShellKind::Bash, BTreeMap::new()), &parent());
    assert_eq!(env.get("HOME").map(String::as_str), Some("/home/user"));
    assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
}

#[test]
fn spec_env_overrides_win_over_defaults_and_parent() {
    let mut overrides = BTreeMap::new();
    // Collides with a tile default...
    overrides.insert("TERM".to_string(), "screen-256color".to_string());
    // ...with an inherited parent key...
    overrides.insert("PATH".to_string(), "/opt/bin".to_string());
    // ...and adds a brand-new key.
    overrides.insert("MY_VAR".to_string(), "custom".to_string());

    let env = build_env(&spec(ShellKind::Bash, overrides), &parent());
    assert_eq!(
        env.get("TERM").map(String::as_str),
        Some("screen-256color"),
        "explicit spec.env TERM must override tile's default"
    );
    assert_eq!(
        env.get("PATH").map(String::as_str),
        Some("/opt/bin"),
        "explicit spec.env must override the inherited parent value"
    );
    assert_eq!(env.get("MY_VAR").map(String::as_str), Some("custom"));
}

#[test]
fn spec_env_can_override_zsh_prompt_eol_mark() {
    let mut overrides = BTreeMap::new();
    overrides.insert("PROMPT_EOL_MARK".to_string(), "DONE".to_string());
    let env = build_env(&spec(ShellKind::Zsh, overrides), &parent());
    assert_eq!(
        env.get("PROMPT_EOL_MARK").map(String::as_str),
        Some("DONE"),
        "spec.env override must win over the zsh bootstrap default"
    );
}

#[test]
fn full_snapshot_zsh() {
    let env = build_env(&spec(ShellKind::Zsh, BTreeMap::new()), &parent());
    let mut expected = BTreeMap::new();
    expected.insert("HOME".to_string(), "/home/user".to_string());
    expected.insert("PATH".to_string(), "/usr/bin".to_string());
    expected.insert("TERM".to_string(), "xterm-256color".to_string());
    expected.insert("COLORTERM".to_string(), "truecolor".to_string());
    expected.insert("PROMPT_EOL_MARK".to_string(), String::new());
    assert_eq!(env, expected);
}

#[test]
fn full_snapshot_bash_has_no_shell_specific_keys() {
    let env = build_env(&spec(ShellKind::Bash, BTreeMap::new()), &parent());
    let mut expected = BTreeMap::new();
    expected.insert("HOME".to_string(), "/home/user".to_string());
    expected.insert("PATH".to_string(), "/usr/bin".to_string());
    expected.insert("TERM".to_string(), "xterm-256color".to_string());
    expected.insert("COLORTERM".to_string(), "truecolor".to_string());
    assert_eq!(env, expected);
}
