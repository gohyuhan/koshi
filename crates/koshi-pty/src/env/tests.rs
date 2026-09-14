//! Unit tests for [`build_environment_overlay`]: per-shell bootstrap snapshots and explicit
//! environment-variable override precedence over koshi's own defaults.
//!
//! `build_environment_overlay` returns only koshi's *overlay*; parent-environment preservation and the
//! Windows case-fold are properties of the spawn path (`portable-pty` applies
//! the overlay over the un-cleared inherited environment) and are covered there.

use super::*;
use std::path::PathBuf;

/// Build a minimal [`SpawnSpec`] for the given shell with the supplied
/// environment variables; the program, arguments, and working directory are
/// irrelevant to `build_environment_overlay`.
fn build_spawn_spec(
    shell_kind: ShellKind,
    environment_variables: BTreeMap<String, String>,
) -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from("/bin/sh"),
        arguments: Vec::new(),
        working_directory: None,
        environment_variables,
        shell_kind,
    }
}

#[test]
fn terminal_environment_variables_are_set_for_every_shell() {
    for shell_kind in [
        ShellKind::Zsh,
        ShellKind::Bash,
        ShellKind::Fish,
        ShellKind::PowerShell,
        ShellKind::Nu,
        ShellKind::Other("elvish".to_string()),
    ] {
        let environment_overlay =
            build_environment_overlay(&build_spawn_spec(shell_kind.clone(), BTreeMap::new()));
        assert_eq!(
            environment_overlay.get("TERM").map(String::as_str),
            Some("xterm-256color"),
            "TERM for {shell_kind:?}"
        );
        assert_eq!(
            environment_overlay.get("COLORTERM").map(String::as_str),
            Some("truecolor"),
            "COLORTERM for {shell_kind:?}"
        );
    }
}

#[test]
fn zsh_sets_empty_prompt_eol_mark() {
    let environment_overlay =
        build_environment_overlay(&build_spawn_spec(ShellKind::Zsh, BTreeMap::new()));
    assert_eq!(
        environment_overlay
            .get("PROMPT_EOL_MARK")
            .map(String::as_str),
        Some(""),
        "zsh must set PROMPT_EOL_MARK to the empty string to suppress the `%`"
    );
}

#[test]
fn non_zsh_shells_have_no_prompt_eol_mark() {
    for shell_kind in [
        ShellKind::Bash,
        ShellKind::Fish,
        ShellKind::PowerShell,
        ShellKind::Nu,
        ShellKind::Other("elvish".to_string()),
    ] {
        let environment_overlay =
            build_environment_overlay(&build_spawn_spec(shell_kind.clone(), BTreeMap::new()));
        assert!(
            !environment_overlay.contains_key("PROMPT_EOL_MARK"),
            "{shell_kind:?} must not carry the zsh-specific PROMPT_EOL_MARK"
        );
    }
}

#[test]
fn overlay_carries_only_koshi_keys_and_spawn_spec_environment() {
    // No parent environment is mixed in: a vanilla bash overlay is exactly the two
    // universal keys, nothing inherited.
    let environment_overlay =
        build_environment_overlay(&build_spawn_spec(ShellKind::Bash, BTreeMap::new()));
    assert!(
        !environment_overlay.contains_key("HOME"),
        "overlay must not invent parent keys"
    );
    assert!(
        !environment_overlay.contains_key("PATH"),
        "overlay must not invent parent keys"
    );
}

#[test]
fn spawn_spec_environment_overrides_defaults_and_adds_keys() {
    let mut environment_overrides = BTreeMap::new();
    // Collides with a koshi default...
    environment_overrides.insert("TERM".to_string(), "screen-256color".to_string());
    // ...and adds a brand-new key.
    environment_overrides.insert("MY_VAR".to_string(), "custom".to_string());

    let environment_overlay =
        build_environment_overlay(&build_spawn_spec(ShellKind::Bash, environment_overrides));
    assert_eq!(
        environment_overlay.get("TERM").map(String::as_str),
        Some("screen-256color"),
        "an explicit environment variable TERM must override koshi's default"
    );
    assert_eq!(
        environment_overlay.get("MY_VAR").map(String::as_str),
        Some("custom")
    );
}

#[test]
fn spawn_spec_environment_can_override_zsh_prompt_eol_mark() {
    let mut environment_overrides = BTreeMap::new();
    environment_overrides.insert("PROMPT_EOL_MARK".to_string(), "DONE".to_string());
    let environment_overlay =
        build_environment_overlay(&build_spawn_spec(ShellKind::Zsh, environment_overrides));
    assert_eq!(
        environment_overlay
            .get("PROMPT_EOL_MARK")
            .map(String::as_str),
        Some("DONE"),
        "an explicit environment variable must win over the zsh bootstrap default"
    );
}

#[test]
fn zsh_overlay_matches_expected_snapshot() {
    let environment_overlay =
        build_environment_overlay(&build_spawn_spec(ShellKind::Zsh, BTreeMap::new()));
    let mut expected_environment_overlay = BTreeMap::new();
    expected_environment_overlay.insert("TERM".to_string(), "xterm-256color".to_string());
    expected_environment_overlay.insert("COLORTERM".to_string(), "truecolor".to_string());
    expected_environment_overlay.insert("PROMPT_EOL_MARK".to_string(), String::new());
    assert_eq!(environment_overlay, expected_environment_overlay);
}

#[test]
fn bash_overlay_matches_expected_snapshot_without_shell_specific_keys() {
    let environment_overlay =
        build_environment_overlay(&build_spawn_spec(ShellKind::Bash, BTreeMap::new()));
    let mut expected_environment_overlay = BTreeMap::new();
    expected_environment_overlay.insert("TERM".to_string(), "xterm-256color".to_string());
    expected_environment_overlay.insert("COLORTERM".to_string(), "truecolor".to_string());
    assert_eq!(environment_overlay, expected_environment_overlay);
}

#[test]
fn spawn_spec_environment_value_with_equals_sign_passes_through_unmodified() {
    // `build_environment_overlay` is a plain `BTreeMap<String, String>` builder with no
    // validation of its own: a value like `KEY=A=B` (an embedded `=`, e.g. a
    // `PATH`-like value with an `=` in one segment) must come out byte-for-byte
    // identical — `build_environment_overlay` does not split, escape, or reject it.
    let mut environment_overrides = BTreeMap::new();
    environment_overrides.insert("MY_VAR".to_string(), "A=B=C".to_string());
    let environment_overlay =
        build_environment_overlay(&build_spawn_spec(ShellKind::Bash, environment_overrides));
    assert_eq!(
        environment_overlay.get("MY_VAR").map(String::as_str),
        Some("A=B=C")
    );
}

#[test]
fn spawn_spec_environment_value_with_nul_byte_passes_through_unmodified() {
    // A NUL byte (`\0`) is valid inside a Rust `String` (any valid UTF-8 byte
    // sequence is legal there) even though the OS environment-variable ABI cannot carry
    // one. `build_environment_overlay` has no defense against it — that boundary lives at the
    // spawn call into `CommandBuilder`/the OS, not here — so this pins the
    // current contract: the NUL passes straight through unmodified.
    let mut environment_overrides = BTreeMap::new();
    environment_overrides.insert("MY_VAR".to_string(), "a\0b".to_string());
    let environment_overlay =
        build_environment_overlay(&build_spawn_spec(ShellKind::Bash, environment_overrides));
    assert_eq!(
        environment_overlay.get("MY_VAR").map(String::as_str),
        Some("a\0b")
    );
}

#[test]
fn spawn_spec_environment_key_with_equals_sign_is_kept_as_a_distinct_key() {
    // A key with an embedded `=` (e.g. `"A=B"`) is legal in a `BTreeMap<String,
    // String>` even though it can never be expressed as `KEY=VALUE` in a real
    // process environment. `build_environment_overlay` does not reject or split it — it is
    // carried through as one opaque map key, same as any other string.
    let mut environment_overrides = BTreeMap::new();
    environment_overrides.insert("A=B".to_string(), "value".to_string());
    let environment_overlay =
        build_environment_overlay(&build_spawn_spec(ShellKind::Bash, environment_overrides));
    let mut expected_environment_overlay = BTreeMap::new();
    expected_environment_overlay.insert("TERM".to_string(), "xterm-256color".to_string());
    expected_environment_overlay.insert("COLORTERM".to_string(), "truecolor".to_string());
    expected_environment_overlay.insert("A=B".to_string(), "value".to_string());
    assert_eq!(environment_overlay, expected_environment_overlay);
}

#[test]
fn every_non_zsh_shell_overlay_has_only_the_two_universal_keys() {
    let mut expected_environment_overlay = BTreeMap::new();
    expected_environment_overlay.insert("TERM".to_string(), "xterm-256color".to_string());
    expected_environment_overlay.insert("COLORTERM".to_string(), "truecolor".to_string());
    for shell_kind in [
        ShellKind::Bash,
        ShellKind::Fish,
        ShellKind::PowerShell,
        ShellKind::Nu,
        ShellKind::Other("elvish".to_string()),
        ShellKind::Other(String::new()),
    ] {
        let environment_overlay =
            build_environment_overlay(&build_spawn_spec(shell_kind.clone(), BTreeMap::new()));
        assert_eq!(
            environment_overlay, expected_environment_overlay,
            "overlay for {shell_kind:?}"
        );
    }
}

#[test]
fn zsh_overlay_with_environment_overrides_matches_expected_snapshot() {
    let mut environment_overrides = BTreeMap::new();
    environment_overrides.insert("TERM".to_string(), "tmux-256color".to_string());
    environment_overrides.insert("PROMPT_EOL_MARK".to_string(), "%".to_string());
    environment_overrides.insert("EDITOR".to_string(), "vi".to_string());

    let environment_overlay =
        build_environment_overlay(&build_spawn_spec(ShellKind::Zsh, environment_overrides));

    let mut expected_environment_overlay = BTreeMap::new();
    expected_environment_overlay.insert("TERM".to_string(), "tmux-256color".to_string());
    expected_environment_overlay.insert("COLORTERM".to_string(), "truecolor".to_string());
    expected_environment_overlay.insert("PROMPT_EOL_MARK".to_string(), "%".to_string());
    expected_environment_overlay.insert("EDITOR".to_string(), "vi".to_string());
    assert_eq!(environment_overlay, expected_environment_overlay);
}

#[test]
fn spawn_spec_environment_can_blank_a_default() {
    let mut environment_overrides = BTreeMap::new();
    environment_overrides.insert("COLORTERM".to_string(), String::new());
    let environment_overlay =
        build_environment_overlay(&build_spawn_spec(ShellKind::Bash, environment_overrides));
    assert_eq!(
        environment_overlay.get("COLORTERM").map(String::as_str),
        Some("")
    );
}

#[test]
fn an_empty_key_is_kept_as_a_key() {
    let mut environment_overrides = BTreeMap::new();
    environment_overrides.insert(String::new(), "value".to_string());
    let environment_overlay =
        build_environment_overlay(&build_spawn_spec(ShellKind::Bash, environment_overrides));
    assert_eq!(
        environment_overlay.get("").map(String::as_str),
        Some("value")
    );
}

#[test]
fn non_ascii_key_and_value_pass_through_unmodified() {
    let mut environment_overrides = BTreeMap::new();
    environment_overrides.insert("ÜBER_変数".to_string(), "値 🐚".to_string());
    let environment_overlay =
        build_environment_overlay(&build_spawn_spec(ShellKind::Bash, environment_overrides));
    assert_eq!(
        environment_overlay.get("ÜBER_変数").map(String::as_str),
        Some("値 🐚")
    );
}
