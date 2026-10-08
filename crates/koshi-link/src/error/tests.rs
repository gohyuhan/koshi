//! Exit-code mapping and message rendering for [`CliError`].

use super::*;

/// One of each [`CliError`] variant, with the message it renders and the exit
/// code it maps to.
fn list_every_error_class() -> Vec<(CliError, &'static str, i32)> {
    vec![
        (
            CliError::UnknownAction {
                action_name: "new-pane".into(),
            },
            "unknown action: new-pane",
            2,
        ),
        (
            CliError::InvalidArgs {
                detail: "missing --pane".into(),
            },
            "invalid arguments: missing --pane",
            2,
        ),
        (
            CliError::UnboundKey {
                key_sequence_text: "<C-t> g".into(),
            },
            "nothing is bound on `<C-t> g` in any mode",
            2,
        ),
        (
            CliError::InvalidKeybindingFile {
                keybinding_file_path: "/home/user/.config/koshi/keybinding.kdl".into(),
            },
            "keybinding file /home/user/.config/koshi/keybinding.kdl failed validation",
            2,
        ),
        (
            CliError::Config {
                detail: "bad key".into(),
            },
            "config failed: bad key",
            2,
        ),
        (
            CliError::InSessionEnv {
                detail: "`KOSHI` is set but `KOSHI_SESSION_ID` is missing".into(),
            },
            "broken in-session environment: `KOSHI` is set but `KOSHI_SESSION_ID` is missing",
            2,
        ),
        (
            CliError::IpcUnavailable {
                detail: "no koshi daemon is reachable".into(),
            },
            "IPC unavailable: no koshi daemon is reachable",
            4,
        ),
        (
            CliError::ProtocolVersionRefused {
                detail: "the client speaks protocol version 4 to 4, this session speaks 5 to 5"
                    .into(),
            },
            "IPC unavailable: the client speaks protocol version 4 to 4, this session speaks 5 to 5",
            4,
        ),
        (
            CliError::PreviousReleaseServer {
                detail: "the server answered in the format of koshi 0.4.0 or older, which this \
                         koshi cannot talk to"
                    .into(),
            },
            "IPC unavailable: the server answered in the format of koshi 0.4.0 or older, which \
             this koshi cannot talk to; the user who started it runs: koshi restart-servers",
            4,
        ),
        (
            CliError::SessionAnswerTimedOut,
            "IPC unavailable: the session did not answer in time",
            4,
        ),
        (
            CliError::SessionNotFound {
                session_name: "session-x".into(),
            },
            "session session-x is not running",
            3,
        ),
        (CliError::NoSessions, "no koshi session is running", 3),
        (
            CliError::CommandRejected {
                reason: RejectReason::Unauthorized,
                help: Some("run this command from an active Koshi client".into()),
            },
            "command not permitted\n  run this command from an active Koshi client",
            1,
        ),
        (
            CliError::Runtime {
                detail: "boom".into(),
            },
            "boom",
            1,
        ),
        (
            CliError::Update {
                detail: "the download stopped halfway".into(),
            },
            "update failed: the download stopped halfway",
            1,
        ),
    ]
}

#[test]
fn every_error_class_renders_its_exact_message() {
    for (cli_error, expected_message, _) in list_every_error_class() {
        assert_eq!(cli_error.to_string(), expected_message);
    }
}

#[test]
fn every_error_class_exits_with_its_documented_number() {
    for (cli_error, _, expected_exit_code) in list_every_error_class() {
        assert_eq!(
            CliExitCode::from(&cli_error).get_exit_code(),
            expected_exit_code,
            "{cli_error}"
        );
    }
}

#[test]
fn messages_render_an_empty_or_unicode_field_verbatim() {
    assert_eq!(
        CliError::UnknownAction {
            action_name: String::new()
        }
        .to_string(),
        "unknown action: "
    );
    assert_eq!(
        CliError::UnknownAction {
            action_name: "日本語".into()
        }
        .to_string(),
        "unknown action: 日本語"
    );
    assert_eq!(
        CliError::Runtime {
            detail: String::new()
        }
        .to_string(),
        ""
    );
}

#[test]
fn every_rejection_reason_renders_its_own_sentence() {
    for (reason, sentence) in [
        (RejectReason::TargetGone, "target no longer exists"),
        (
            RejectReason::TargetAmbiguous,
            "target matched more than one; specify an explicit id",
        ),
        (RejectReason::TargetNotFound, "no target matched"),
        (
            RejectReason::SourceClientStale,
            "source client has detached",
        ),
        (RejectReason::Unauthorized, "command not permitted"),
        (RejectReason::InvalidState, "invalid in the current state"),
        (RejectReason::MinimumSize, "below minimum size"),
    ] {
        assert_eq!(
            CliError::CommandRejected { reason, help: None }.to_string(),
            sentence
        );
    }
}

#[test]
fn an_empty_help_hint_still_renders_its_own_line() {
    assert_eq!(
        CliError::CommandRejected {
            reason: RejectReason::Unauthorized,
            help: Some(String::new()),
        }
        .to_string(),
        "command not permitted\n  "
    );
}

#[test]
fn a_help_hint_of_several_lines_indents_only_its_first_line() {
    assert_eq!(
        CliError::CommandRejected {
            reason: RejectReason::TargetNotFound,
            help: Some("no running session has tab tab-1\ncheck `koshi list`".into()),
        }
        .to_string(),
        "no target matched\n  no running session has tab tab-1\ncheck `koshi list`"
    );
}
