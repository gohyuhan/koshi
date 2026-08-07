//! Tests for the display text, the category and the severity of every wrapped
//! domain error.
//!
//! The first tests check each domain error on its own. The later tests wrap an
//! error in `KoshiError` and check that all three still match the inner error.

use super::*;
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

    let corrupt: KoshiError = StorageError::Corrupt { detail: "x".into() }.into();
    assert_eq!(corrupt.severity(), Severity::SessionFatal);
}

// The tests above wrap only 2 of the 8 `#[from]` variants (Pty, Storage). The
// tests below cover the rest. Each one checks that `.into()` gives the same
// `to_string()`, `category()` and `severity()` as the unwrapped inner error.

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
    // `Severity::Recoverable` and `Corrupt` is `Severity::SessionFatal`. The
    // aggregate test above wraps only `Corrupt`, so this test wraps `Io`.
    let inner = StorageError::Io {
        detail: "disk full".into(),
    };
    let want = inner.to_string();
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), want);
    assert_eq!(err.category(), DomainCategory::Storage);
    assert_eq!(err.severity(), Severity::Recoverable);
}

// `CliError::category()` varies per variant. `UnknownCommand`, `UnknownAction`
// and `InvalidArgs` report `DomainCategory::Cli`. `IpcUnavailable` reports
// `DomainCategory::Ipc`, and `Runtime` reports `DomainCategory::Session`. All
// of them sit inside `KoshiError::Cli(..)`, so the tests below check the
// category through the wrapper, one test per variant.

#[test]
fn aggregate_cli_unknown_command_classifies_as_cli() {
    let err: KoshiError = CliError::UnknownCommand {
        name: "frobnicate".into(),
    }
    .into();
    assert_eq!(err.category(), DomainCategory::Cli);
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

#[test]
fn aggregate_source_is_none_for_a_transparent_variant_with_no_sourced_inner() {
    // `#[error(transparent)]` forwards `source()` to the wrapped error's own
    // `source()`. No wrapped enum marks a field `#[source]` or `#[from]` inside
    // its own variants, so `PtyError::Spawn { .. }.source()` is `None`, and
    // `KoshiError::from(PtyError::Spawn { .. }).source()` is `None` too.
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
