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
    // `severity` is an unconditional constant today; assert it per variant so
    // a future per-variant split (e.g. a fatal class) fails this test instead
    // of shipping silently.
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
    // Boundary (empty string) and encoding (multi-byte) cases: the message
    // formats the field exactly as given, with no escaping or substitution.
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
