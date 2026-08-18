//! `koshi share` runs on the machine hosting the session and nowhere else.
//!
//! Two doorways could carry a share verb to another machine, and neither does.
//! This file holds the outer one, the `--remote` flag, refused two ways
//! depending on where the flag sits. The inner one — a share verb typed inside
//! a pane by a client viewing the session from another machine — is refused in
//! `koshi::share`, which detaches that client.
//!
//! Both refusals land before any connection is opened, so naming a server that
//! was never saved changes nothing about either answer.

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
        // check: `--remote` carries `attach` and the action verbs, and a share
        // verb is neither. Exit 2 is `CliExitCode::UsageOrConfig`, which
        // `CliError::InvalidArgs` maps to — the same code clap's own usage
        // error uses.
        assert_eq!(
            code,
            Some(2),
            "`koshi {}` is refused: {stderr}",
            args.join(" ")
        );
        assert_eq!(
            stderr.trim_end(),
            "koshi: invalid arguments: --remote needs a command, \
             such as `koshi attach --remote <server>`",
            "the refusal names what to type instead"
        );
        assert!(
            stdout.is_empty(),
            "a refused verb prints no answer: {stdout}"
        );
    }
}
