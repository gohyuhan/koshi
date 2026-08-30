//! Tests for the display text, the category and the severity of every wrapped
//! domain error, plus the `Debug` text, the `source` and the boxed form of the
//! wrapper.
//!
//! The first tests check each domain error on its own. The tests below them
//! wrap an error in `KoshiError` and check that all three still match the inner
//! error.

use super::*;
use koshi_core::event::RejectReason;
use std::error::Error as StdError;

#[test]
fn config_error_classifies_and_displays() {
    let e = ConfigError::Validation {
        key: "layout".into(),
        detail: "unknown value".into(),
    };
    assert_eq!(e.to_string(), "invalid config key `layout`: unknown value");
    assert_eq!(e.category(), DomainCategory::Config);
    assert_eq!(e.severity(), Severity::Recoverable);
}

#[test]
fn cli_error_classifies_and_displays() {
    let e = CliError::UnknownCommand {
        name: "frobnicate".into(),
    };
    assert_eq!(e.to_string(), "unknown command: frobnicate");
    assert_eq!(e.category(), DomainCategory::Cli);
    assert_eq!(e.severity(), Severity::Recoverable);
}

#[test]
fn ipc_error_is_client_fatal() {
    let e = IpcError::Disconnected;
    assert_eq!(e.to_string(), "ipc peer disconnected");
    assert_eq!(e.category(), DomainCategory::Ipc);
    assert_eq!(e.severity(), Severity::ClientFatal);
}

#[test]
fn pty_failure_is_recoverable() {
    let e = PtyError::Spawn {
        detail: "no such shell".into(),
    };
    assert_eq!(e.to_string(), "failed to spawn pty: no such shell");
    assert_eq!(e.category(), DomainCategory::Pty);
    assert_eq!(e.severity(), Severity::Recoverable);
}

#[test]
fn terminal_error_is_recoverable() {
    let e = TerminalError::Parse {
        detail: "bad CSI".into(),
    };
    assert_eq!(e.to_string(), "terminal parse error: bad CSI");
    assert_eq!(e.category(), DomainCategory::Terminal);
    assert_eq!(e.severity(), Severity::Recoverable);
}

#[test]
fn layout_error_is_recoverable() {
    let e = LayoutError::MinSize {
        detail: "neighbor at min width 2".into(),
    };
    assert_eq!(
        e.to_string(),
        "layout minimum-size violation: neighbor at min width 2"
    );
    assert_eq!(e.category(), DomainCategory::Layout);
    assert_eq!(e.severity(), Severity::Recoverable);
}

#[test]
fn plugin_failure_is_recoverable() {
    let e = PluginError::Runtime {
        name: "statusbar".into(),
        detail: "trap".into(),
    };
    assert_eq!(e.to_string(), "plugin `statusbar` runtime error: trap");
    assert_eq!(e.category(), DomainCategory::Plugin);
    assert_eq!(e.severity(), Severity::Recoverable);
}

#[test]
fn storage_severity_varies_by_variant() {
    let io = StorageError::Io {
        detail: "disk full".into(),
    };
    assert_eq!(io.to_string(), "storage io error: disk full");
    assert_eq!(io.category(), DomainCategory::Storage);
    assert_eq!(io.severity(), Severity::Recoverable);

    let corrupt = StorageError::Corrupt {
        detail: "bad checksum".into(),
    };
    assert_eq!(corrupt.to_string(), "corrupt stored state: bad checksum");
    assert_eq!(corrupt.category(), DomainCategory::Storage);
    assert_eq!(corrupt.severity(), Severity::SessionFatal);
}

#[test]
fn aggregate_delegates_and_is_transparent() {
    let inner = PtyError::Io {
        detail: "read failed".into(),
    };
    let want = inner.to_string();
    let err: KoshiError = inner.into();
    // Display is transparent: the aggregate prints exactly the inner error.
    assert_eq!(err.to_string(), want);
    assert_eq!(err.category(), DomainCategory::Pty);
    assert_eq!(err.severity(), Severity::Recoverable);

    let corrupt_inner = StorageError::Corrupt { detail: "x".into() };
    let corrupt_want = corrupt_inner.to_string();
    let corrupt: KoshiError = corrupt_inner.into();
    assert_eq!(corrupt.to_string(), corrupt_want);
    assert_eq!(corrupt.category(), DomainCategory::Storage);
    assert_eq!(corrupt.severity(), Severity::SessionFatal);
}

// Each test below wraps one `#[from]` variant and checks that `.into()` gives
// the same `to_string()`, `category()` and `severity()` as the unwrapped inner
// error.

#[test]
fn aggregate_wraps_and_delegates_config_error() {
    let inner = ConfigError::NotFound {
        path: "/etc/koshi/config.kdl".into(),
    };
    let want = inner.to_string();
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), want);
    assert_eq!(err.category(), DomainCategory::Config);
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn aggregate_wraps_and_delegates_ipc_error() {
    let inner = IpcError::Transport {
        detail: "socket reset".into(),
    };
    let want = inner.to_string();
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), want);
    assert_eq!(err.category(), DomainCategory::Ipc);
    assert_eq!(err.severity(), Severity::ClientFatal);
}

// `IpcError::severity()` varies per variant: most variants are
// `Severity::ClientFatal`, `EndpointFileWrite` is `Severity::SessionFatal` and
// `MalformedFrame` is `Severity::Recoverable`. The two tests below wrap the two
// variants that differ from the rest.

#[test]
fn aggregate_wraps_ipc_endpoint_file_write_as_session_fatal() {
    let inner = IpcError::EndpointFileWrite {
        path: "/run/koshi/endpoint.json".into(),
        detail: "read-only file system".into(),
    };
    let want = inner.to_string();
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), want);
    assert_eq!(err.category(), DomainCategory::Ipc);
    assert_eq!(err.severity(), Severity::SessionFatal);
}

#[test]
fn aggregate_wraps_ipc_malformed_frame_as_recoverable() {
    let inner = IpcError::MalformedFrame {
        detail: "truncated header".into(),
    };
    let want = inner.to_string();
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), want);
    assert_eq!(err.category(), DomainCategory::Ipc);
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn aggregate_wraps_and_delegates_terminal_error() {
    let inner = TerminalError::Parse {
        detail: "unterminated CSI".into(),
    };
    let want = inner.to_string();
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), want);
    assert_eq!(err.category(), DomainCategory::Terminal);
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn aggregate_wraps_and_delegates_layout_error() {
    let inner = LayoutError::Solve {
        detail: "no feasible split".into(),
    };
    let want = inner.to_string();
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), want);
    assert_eq!(err.category(), DomainCategory::Layout);
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn aggregate_wraps_and_delegates_plugin_error() {
    let inner = PluginError::Load {
        name: "statusbar".into(),
        detail: "missing manifest".into(),
    };
    let want = inner.to_string();
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), want);
    assert_eq!(err.category(), DomainCategory::Plugin);
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn aggregate_wraps_storage_io_variant_as_recoverable() {
    // `StorageError::severity()` varies per variant: `Io` is
    // `Severity::Recoverable` and `Corrupt` is `Severity::SessionFatal`.
    let inner = StorageError::Io {
        detail: "disk full".into(),
    };
    let want = inner.to_string();
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), want);
    assert_eq!(err.category(), DomainCategory::Storage);
    assert_eq!(err.severity(), Severity::Recoverable);
}

// `CliError::category()` maps its variants onto three categories:
// `DomainCategory::Cli`, `DomainCategory::Ipc` and `DomainCategory::Session`.
// All of them sit inside `KoshiError::Cli(..)`. Each test below wraps one
// variant and checks the category the wrapper reports.

#[test]
fn aggregate_cli_unknown_command_classifies_as_cli() {
    let inner = CliError::UnknownCommand {
        name: "frobnicate".into(),
    };
    let want = inner.to_string();
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), want);
    assert_eq!(err.category(), DomainCategory::Cli);
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn aggregate_cli_unknown_action_classifies_as_cli() {
    let inner = CliError::UnknownAction {
        name: "pane.split".into(),
    };
    let want = inner.to_string();
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), want);
    assert_eq!(err.category(), DomainCategory::Cli);
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn aggregate_cli_invalid_args_classifies_as_cli() {
    let inner = CliError::InvalidArgs {
        detail: "missing --session".into(),
    };
    let want = inner.to_string();
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), want);
    assert_eq!(err.category(), DomainCategory::Cli);
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn aggregate_cli_ipc_unavailable_classifies_as_ipc_not_cli() {
    let inner = CliError::IpcUnavailable {
        detail: "no socket".into(),
    };
    let want = inner.to_string();
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), want);
    assert_eq!(err.category(), DomainCategory::Ipc);
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn aggregate_cli_runtime_classifies_as_session_not_cli() {
    let inner = CliError::Runtime {
        detail: "action rejected".into(),
    };
    let want = inner.to_string();
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), want);
    assert_eq!(err.category(), DomainCategory::Session);
    assert_eq!(err.severity(), Severity::Recoverable);
}

// Each test below wraps a payload holding braces, non-ASCII text, an empty
// string, a newline, or an integer field at its maximum, and checks the
// wrapper prints it byte for byte.

#[test]
fn aggregate_display_keeps_braces_and_non_ascii_in_a_detail() {
    let inner = PtyError::Io {
        detail: "権限がありません {0} {}".into(),
    };
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), "pty io error: 権限がありません {0} {}");
    assert_eq!(err.category(), DomainCategory::Pty);
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn aggregate_display_keeps_an_empty_detail() {
    let inner = TerminalError::Parse {
        detail: String::new(),
    };
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), "terminal parse error: ");
    assert_eq!(err.category(), DomainCategory::Terminal);
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn aggregate_display_keeps_the_two_line_rejected_command_message() {
    let inner = CliError::CommandRejected {
        reason: RejectReason::Unauthorized,
        help: Some("attach first".into()),
    };
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), "command not permitted\n  attach first");
    assert_eq!(err.category(), DomainCategory::Session);
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn aggregate_display_keeps_integer_fields_at_their_maximum() {
    let inner = IpcError::FrameTooLarge {
        len: u64::MAX,
        max: u32::MAX,
    };
    let err: KoshiError = inner.into();
    assert_eq!(
        err.to_string(),
        "ipc frame of 18446744073709551615 bytes exceeds the 4294967295-byte limit"
    );
    assert_eq!(err.category(), DomainCategory::Ipc);
    assert_eq!(err.severity(), Severity::ClientFatal);
}

#[test]
fn aggregate_boxes_into_a_send_sync_std_error_and_keeps_its_display() {
    let boxed: Box<dyn StdError + Send + Sync + 'static> =
        Box::new(KoshiError::from(StorageError::Corrupt {
            detail: "bad checksum".into(),
        }));
    assert_eq!(boxed.to_string(), "corrupt stored state: bad checksum");
}

#[test]
fn aggregate_debug_names_the_wrapping_variant_while_display_does_not() {
    let err: KoshiError = PtyError::Spawn {
        detail: "no such shell".into(),
    }
    .into();
    assert_eq!(
        format!("{err:?}"),
        "Pty(Spawn { detail: \"no such shell\" })"
    );
    assert_eq!(err.to_string(), "failed to spawn pty: no such shell");
}

#[test]
fn aggregate_source_is_none_for_a_transparent_variant_with_no_sourced_inner() {
    // `#[error(transparent)]` forwards `source()` to the wrapped error's own
    // `source()`. No wrapped enum marks a field `#[source]` or `#[from]` in its
    // variants, so every wrapped error's `source()` is `None`.
    let err: KoshiError = PtyError::Spawn {
        detail: "no such shell".into(),
    }
    .into();
    assert!(
        err.source().is_none(),
        "transparent source must delegate to the inner error's own source(), \
         which PtyError has none of"
    );
}
