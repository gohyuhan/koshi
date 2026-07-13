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

#[test]
fn spec_env_value_containing_equals_sign_passes_through_unmodified() {
    // `build_env` is a plain `BTreeMap<String, String>` builder with no
    // validation of its own: a value like `KEY=A=B` (an embedded `=`, e.g. a
    // `PATH`-like value with an `=` in one segment) must come out byte-for-byte
    // identical — `build_env` does not split, escape, or reject it.
    let mut overrides = BTreeMap::new();
    overrides.insert("MY_VAR".to_string(), "A=B=C".to_string());
    let env = build_env(&spec(ShellKind::Bash, overrides));
    assert_eq!(env.get("MY_VAR").map(String::as_str), Some("A=B=C"));
}

#[test]
fn spec_env_value_containing_nul_byte_passes_through_unmodified() {
    // A NUL byte (`\0`) is valid inside a Rust `String` (any valid UTF-8 byte
    // sequence is legal there) even though the OS env-var ABI cannot carry
    // one. `build_env` has no defense against it — that boundary lives at the
    // spawn call into `CommandBuilder`/the OS, not here — so this pins the
    // current contract: the NUL passes straight through unmodified.
    let mut overrides = BTreeMap::new();
    overrides.insert("MY_VAR".to_string(), "a\0b".to_string());
    let env = build_env(&spec(ShellKind::Bash, overrides));
    assert_eq!(env.get("MY_VAR").map(String::as_str), Some("a\0b"));
}

#[test]
fn spec_env_key_containing_equals_sign_is_kept_as_a_distinct_key() {
    // A key with an embedded `=` (e.g. `"A=B"`) is legal in a `BTreeMap<String,
    // String>` even though it can never be expressed as `KEY=VALUE` in a real
    // process environment. `build_env` does not reject or split it — it is
    // carried through as one opaque map key, same as any other string.
    let mut overrides = BTreeMap::new();
    overrides.insert("A=B".to_string(), "value".to_string());
    let env = build_env(&spec(ShellKind::Bash, overrides));
    assert_eq!(env.get("A=B").map(String::as_str), Some("value"));
    assert_eq!(env.len(), 3, "TERM + COLORTERM + the one A=B override");
}
