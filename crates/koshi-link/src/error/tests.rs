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
