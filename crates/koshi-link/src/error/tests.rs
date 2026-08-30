//! Exit-code mapping, message rendering, and domain classification for
//! [`CliError`].

use super::*;

#[test]
fn maps_each_error_class_to_its_exit_code() {
    assert_eq!(
        CliExitCode::from(&CliError::UnknownCommand { name: "x".into() }),
        CliExitCode::UsageOrConfig
    );
    assert_eq!(
        CliExitCode::from(&CliError::UnknownAction { name: "x".into() }),
        CliExitCode::UsageOrConfig
    );
    assert_eq!(
        CliExitCode::from(&CliError::InvalidArgs { detail: "x".into() }),
        CliExitCode::UsageOrConfig
    );
    assert_eq!(
        CliExitCode::from(&CliError::Config { detail: "x".into() }),
        CliExitCode::UsageOrConfig
    );
    assert_eq!(
        CliExitCode::from(&CliError::InSessionEnv { detail: "x".into() }),
        CliExitCode::UsageOrConfig
    );
    assert_eq!(
        CliExitCode::from(&CliError::IpcUnavailable { detail: "x".into() }),
        CliExitCode::IpcUnavailable
    );
    assert_eq!(
        CliExitCode::from(&CliError::Runtime { detail: "x".into() }),
        CliExitCode::RuntimeAction
    );
    assert_eq!(
        CliExitCode::from(&CliError::SessionNotFound {
            session: "session-x".into()
        }),
        CliExitCode::SessionNotFound
    );
    assert_eq!(
        CliExitCode::from(&CliError::CommandRejected {
            reason: RejectReason::Unauthorized,
            help: None
        }),
        CliExitCode::RuntimeAction
    );
}

#[test]
fn exit_codes_are_the_documented_numbers() {
    assert_eq!(
        CliExitCode::from(&CliError::InvalidArgs { detail: "x".into() }).code(),
        2
    );
    assert_eq!(
        CliExitCode::from(&CliError::Config { detail: "x".into() }).code(),
        2
    );
    assert_eq!(
        CliExitCode::from(&CliError::IpcUnavailable { detail: "x".into() }).code(),
        4
    );
    assert_eq!(
        CliExitCode::from(&CliError::Runtime { detail: "x".into() }).code(),
        1
    );
    assert_eq!(
        CliExitCode::from(&CliError::SessionNotFound {
            session: "session-x".into()
        })
        .code(),
        3
    );
    assert_eq!(
        CliExitCode::from(&CliError::CommandRejected {
            reason: RejectReason::Unauthorized,
            help: None
        })
        .code(),
        1
    );
}

#[test]
fn a_rejected_command_renders_its_reason_and_help_line() {
    assert_eq!(
        CliError::CommandRejected {
            reason: RejectReason::Unauthorized,
            help: Some("run this command from an active Koshi client".into()),
        }
        .to_string(),
        "command not permitted\n  run this command from an active Koshi client"
    );
    assert_eq!(
        CliError::CommandRejected {
            reason: RejectReason::TargetGone,
            help: None,
        }
        .to_string(),
        "target no longer exists"
    );
    assert_eq!(
        CliError::SessionNotFound {
            session: "session-x".into()
        }
        .to_string(),
        "session session-x is not running"
    );
}

#[test]
fn messages_render_without_a_koshi_prefix() {
    assert_eq!(
        CliError::UnknownAction {
            name: "new-pane".into()
        }
        .to_string(),
        "unknown action: new-pane"
    );
    assert_eq!(
        CliError::IpcUnavailable {
            detail: "no koshi daemon is reachable".into()
        }
        .to_string(),
        "IPC unavailable: no koshi daemon is reachable"
    );
    assert_eq!(
        CliError::Runtime {
            detail: "boom".into()
        }
        .to_string(),
        "boom"
    );
    assert_eq!(
        CliError::Config {
            detail: "bad key".into()
        }
        .to_string(),
        "config failed: bad key"
    );
}

#[test]
fn category_classifies_by_variant() {
    assert_eq!(
        CliError::UnknownCommand { name: "x".into() }.category(),
        DomainCategory::Cli
    );
    assert_eq!(
        CliError::UnknownAction { name: "x".into() }.category(),
        DomainCategory::Cli
    );
    assert_eq!(
        CliError::InvalidArgs { detail: "x".into() }.category(),
        DomainCategory::Cli
    );
    assert_eq!(
        CliError::Config { detail: "x".into() }.category(),
        DomainCategory::Cli
    );
    assert_eq!(
        CliError::InSessionEnv { detail: "x".into() }.category(),
        DomainCategory::Cli
    );
    assert_eq!(
        CliError::IpcUnavailable { detail: "x".into() }.category(),
        DomainCategory::Ipc
    );
    assert_eq!(
        CliError::Runtime { detail: "x".into() }.category(),
        DomainCategory::Session
    );
}

#[test]
fn severity_is_recoverable_for_every_variant() {
    assert_eq!(
        CliError::UnknownCommand { name: "x".into() }.severity(),
        Severity::Recoverable
    );
    assert_eq!(
        CliError::UnknownAction { name: "x".into() }.severity(),
        Severity::Recoverable
    );
    assert_eq!(
        CliError::InvalidArgs { detail: "x".into() }.severity(),
        Severity::Recoverable
    );
    assert_eq!(
        CliError::Config { detail: "x".into() }.severity(),
        Severity::Recoverable
    );
    assert_eq!(
        CliError::InSessionEnv { detail: "x".into() }.severity(),
        Severity::Recoverable
    );
    assert_eq!(
        CliError::IpcUnavailable { detail: "x".into() }.severity(),
        Severity::Recoverable
    );
    assert_eq!(
        CliError::Runtime { detail: "x".into() }.severity(),
        Severity::Recoverable
    );
}

#[test]
fn unknown_command_and_invalid_args_messages_are_exact() {
    assert_eq!(
        CliError::UnknownCommand {
            name: "frobnicate".into()
        }
        .to_string(),
        "unknown command: frobnicate"
    );
    assert_eq!(
        CliError::InvalidArgs {
            detail: "missing --pane".into()
        }
        .to_string(),
        "invalid arguments: missing --pane"
    );
}

#[test]
fn an_unbound_key_and_a_bad_keymap_file_exit_as_usage_problems() {
    assert_eq!(
        CliExitCode::from(&CliError::UnboundKey {
            sequence: "<C-t> g".into()
        })
        .code(),
        2
    );
    assert_eq!(
        CliExitCode::from(&CliError::InvalidKeymapFile {
            path: "keybinding.kdl".into()
        })
        .code(),
        2
    );
    assert_eq!(
        CliError::UnboundKey {
            sequence: "<C-t> g".into()
        }
        .category(),
        DomainCategory::Cli
    );
    assert_eq!(
        CliError::InvalidKeymapFile {
            path: "keybinding.kdl".into()
        }
        .category(),
        DomainCategory::Cli
    );
}

#[test]
fn no_running_session_exits_the_same_as_a_named_session_that_is_gone() {
    assert_eq!(
        CliExitCode::from(&CliError::NoSessions),
        CliExitCode::SessionNotFound
    );
    assert_eq!(CliExitCode::from(&CliError::NoSessions).code(), 3);
    assert_eq!(CliError::NoSessions.category(), DomainCategory::Session);
}

#[test]
fn a_failed_update_exits_as_a_runtime_failure() {
    assert_eq!(
        CliExitCode::from(&CliError::Update {
            detail: "the download stopped halfway".into()
        }),
        CliExitCode::RuntimeAction
    );
    assert_eq!(
        CliExitCode::from(&CliError::Update {
            detail: "the download stopped halfway".into()
        })
        .code(),
        1
    );
    assert_eq!(
        CliError::Update {
            detail: "the download stopped halfway".into()
        }
        .category(),
        DomainCategory::Session
    );
}

#[test]
fn the_key_keymap_no_sessions_and_update_messages_are_exact() {
    assert_eq!(
        CliError::UnboundKey {
            sequence: "<C-t> g".into()
        }
        .to_string(),
        "nothing is bound on `<C-t> g` in any mode"
    );
    assert_eq!(
        CliError::InvalidKeymapFile {
            path: "/home/u/.config/koshi/keybinding.kdl".into()
        }
        .to_string(),
        "keybinding file /home/u/.config/koshi/keybinding.kdl failed validation"
    );
    assert_eq!(
        CliError::NoSessions.to_string(),
        "no koshi session is running"
    );
    assert_eq!(
        CliError::Update {
            detail: "the download stopped halfway".into()
        }
        .to_string(),
        "update failed: the download stopped halfway"
    );
}

#[test]
fn messages_render_an_empty_or_unicode_field_verbatim() {
    // The message formats the field exactly as given, with no escaping and no
    // substitution.
    assert_eq!(
        CliError::UnknownCommand {
            name: String::new()
        }
        .to_string(),
        "unknown command: "
    );
    assert_eq!(
        CliError::UnknownAction {
            name: "日本語".into()
        }
        .to_string(),
        "unknown action: 日本語"
    );
}

#[test]
fn a_missing_session_and_a_rejected_command_are_session_domain() {
    assert_eq!(
        CliError::SessionNotFound {
            session: "session-x".into()
        }
        .category(),
        DomainCategory::Session
    );
    assert_eq!(
        CliError::CommandRejected {
            reason: RejectReason::Unauthorized,
            help: None
        }
        .category(),
        DomainCategory::Session
    );
}

#[test]
fn severity_is_recoverable_for_the_key_session_and_update_variants() {
    assert_eq!(
        CliError::UnboundKey {
            sequence: "<C-t> g".into()
        }
        .severity(),
        Severity::Recoverable
    );
    assert_eq!(
        CliError::InvalidKeymapFile {
            path: "keybinding.kdl".into()
        }
        .severity(),
        Severity::Recoverable
    );
    assert_eq!(
        CliError::SessionNotFound {
            session: "session-x".into()
        }
        .severity(),
        Severity::Recoverable
    );
    assert_eq!(CliError::NoSessions.severity(), Severity::Recoverable);
    assert_eq!(
        CliError::CommandRejected {
            reason: RejectReason::MinSize,
            help: None
        }
        .severity(),
        Severity::Recoverable
    );
    assert_eq!(
        CliError::Update { detail: "x".into() }.severity(),
        Severity::Recoverable
    );
}

#[test]
fn a_broken_in_session_environment_renders_its_detail() {
    assert_eq!(
        CliError::InSessionEnv {
            detail: "`KOSHI` is set but `KOSHI_SESSION_ID` is missing".into()
        }
        .to_string(),
        "broken in-session environment: `KOSHI` is set but `KOSHI_SESSION_ID` is missing"
    );
    assert_eq!(
        CliExitCode::from(&CliError::InSessionEnv { detail: "x".into() }).code(),
        2
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
        (RejectReason::MinSize, "below minimum size"),
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
fn every_error_class_exits_with_its_documented_number() {
    for (error, code) in [
        (CliError::UnknownCommand { name: "x".into() }, 2),
        (CliError::UnknownAction { name: "x".into() }, 2),
        (CliError::InvalidArgs { detail: "x".into() }, 2),
        (
            CliError::UnboundKey {
                sequence: "<C-t> g".into(),
            },
            2,
        ),
        (
            CliError::InvalidKeymapFile {
                path: "keybinding.kdl".into(),
            },
            2,
        ),
        (CliError::Config { detail: "x".into() }, 2),
        (CliError::InSessionEnv { detail: "x".into() }, 2),
        (CliError::IpcUnavailable { detail: "x".into() }, 4),
        (
            CliError::SessionNotFound {
                session: "session-x".into(),
            },
            3,
        ),
        (CliError::NoSessions, 3),
        (
            CliError::CommandRejected {
                reason: RejectReason::Unauthorized,
                help: None,
            },
            1,
        ),
        (CliError::Runtime { detail: "x".into() }, 1),
        (CliError::Update { detail: "x".into() }, 1),
    ] {
        assert_eq!(CliExitCode::from(&error).code(), code, "{error}");
    }
}

#[test]
fn a_runtime_error_with_an_empty_detail_renders_an_empty_message() {
    assert_eq!(
        CliError::Runtime {
            detail: String::new()
        }
        .to_string(),
        ""
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
