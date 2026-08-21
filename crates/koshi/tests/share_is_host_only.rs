//! `koshi share` runs on the machine hosting the session and nowhere else.
//!
//! Two doorways could carry a share verb to another machine, and neither does.
//! This file holds the outer one, the `--remote` flag, refused two ways
//! depending on where the flag sits. The inner one — a verb run while this
//! machine serves anyone from another machine — is refused in `koshi::share`,
//! and the last test here pins which terminals that refusal reaches: a verb
//! run in a pane, and never a verb run outside every pane.
//!
//! The two `--remote` refusals land before any connection is opened, so naming
//! a server that was never saved changes nothing about either answer.

use std::path::Path;
use std::process::Command;

/// Every `share` verb, without the `--remote` flag.
const VERBS: [&[&str]; 3] = [
    &["share", "grant", "bob"],
    &["share", "revoke", "bob"],
    &["share", "list"],
];

/// A server name this machine has not saved.
const SERVER: &str = "some-other-box";

/// Run the koshi binary with `args` and hand back `(exit code, stdout, stderr)`.
fn koshi(args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_koshi"))
        .args(args)
        .output()
        .expect("the koshi binary runs");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn a_remote_flag_before_a_share_verb_is_a_usage_error() {
    for verb in VERBS {
        let mut args = vec!["--remote", SERVER];
        args.extend_from_slice(verb);
        let (code, stdout, stderr) = koshi(&args);

        // The root flags conflict with every subcommand
        // (`args_conflicts_with_subcommands`), so clap refuses this spelling
        // before koshi's own code runs. Exit 2 is clap's usage error.
        assert_eq!(
            code,
            Some(2),
            "`koshi {}` is a usage error: {stderr}",
            args.join(" ")
        );
        assert!(
            stderr.contains("cannot be used with '--remote <SERVER>'"),
            "the usage error names the flag it conflicts with: {stderr}"
        );
        assert!(
            stdout.is_empty(),
            "a refused verb prints no answer: {stdout}"
        );
    }
}

#[test]
fn a_remote_flag_after_a_share_verb_is_refused_before_anything_is_asked() {
    for verb in VERBS {
        let mut args = verb.to_vec();
        args.extend_from_slice(&["--remote", SERVER]);
        let (code, stdout, stderr) = koshi(&args);

        // `--remote` is global, so this spelling parses and reaches koshi's own
        // check: `--remote` carries `attach`, `list-sessions`, and the action
        // verbs, and a share verb is none of them. Exit 2 is
        // `CliExitCode::UsageOrConfig`, which `CliError::InvalidArgs` maps to
        // — the same code clap's own usage error uses.
        assert_eq!(
            code,
            Some(2),
            "`koshi {}` is refused: {stderr}",
            args.join(" ")
        );
        assert_eq!(
            stderr.trim_end(),
            "koshi: invalid arguments: --remote works with `attach`, `list-sessions`, \
             and the action verbs, such as `koshi attach --remote <server>`",
            "the refusal names what to type instead"
        );
        assert!(
            stdout.is_empty(),
            "a refused verb prints no answer: {stdout}"
        );
    }
}

/// A session name no running session carries.
const GHOST: &str = "ghost-session";

/// The session the pane variables name; no session server answers for it.
const PANE_SESSION: &str = "11111111-1111-4111-8111-111111111111";

/// The pane the pane variables name.
const PANE: &str = "22222222-2222-4222-8222-222222222222";

/// Run `koshi share list --session ghost-session` and hand back
/// `(exit code, stdout, stderr)`.
///
/// `KOSHI_RUNTIME_DIR` names a directory nothing creates, so the run finds no
/// session endpoint and no router socket, and starts no router.
///
/// `in_pane` true sets the variables a session server exports into a pane, so
/// the run carries a pane environment; false clears them, so it carries none
/// even when the test suite itself runs in a pane.
fn share_list(in_pane: bool) -> (Option<i32>, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_koshi"));
    command
        .args(["share", "list", "--session", GHOST])
        .env(
            "KOSHI_RUNTIME_DIR",
            Path::new(env!("CARGO_TARGET_TMPDIR")).join("share-gate-has-no-runtime-dir"),
        )
        .env_remove("KOSHI")
        .env_remove("KOSHI_SESSION_ID")
        .env_remove("KOSHI_CLIENT_ID")
        .env_remove("KOSHI_PANE_ID")
        .env_remove("KOSHI_SOCKET");
    if in_pane {
        command
            .env("KOSHI", "1")
            .env("KOSHI_SESSION_ID", PANE_SESSION)
            .env("KOSHI_PANE_ID", PANE);
    }
    let output = command.output().expect("the koshi binary runs");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn a_verb_outside_every_pane_passes_the_gate_and_a_verb_in_a_pane_meets_it() {
    let (code, stdout, stderr) = share_list(false);

    // Outside every pane the verb walks past the gate and stops at the session
    // the `--session` flag names. Exit 3 is `CliExitCode::SessionNotFound`,
    // which `CliError::SessionNotFound` maps to.
    assert_eq!(
        code,
        Some(3),
        "a verb outside every pane reaches the session lookup: {stderr}"
    );
    assert_eq!(
        stderr.trim_end(),
        "koshi: session ghost-session is not running",
        "the session lookup answers, and no refusal does"
    );
    assert!(
        stdout.is_empty(),
        "a verb that names no running session prints no answer: {stdout}"
    );

    let (code, stdout, stderr) = share_list(true);

    // In a pane the gate runs first, and the session it asks answers nothing,
    // so the verb is refused before the `--session` flag is resolved. Exit 1 is
    // `CliExitCode::RuntimeAction`, which `CliError::CommandRejected` maps to.
    assert_eq!(code, Some(1), "a verb in a pane meets the gate: {stderr}");
    assert!(
        stderr.contains("this session could not say who is attached to it"),
        "the gate refuses a pane whose session cannot say who watches it: {stderr}"
    );
    assert!(
        stdout.is_empty(),
        "a refused verb prints no answer: {stdout}"
    );
}
