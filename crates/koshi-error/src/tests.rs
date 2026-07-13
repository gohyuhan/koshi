//! Tests for error display, classification, and severity across all domain error types.
//!
//! Each domain error (Config, Cli, Ipc, Pty, Terminal, Layout, Plugin, Storage) is verified
//! to display correctly and be assigned the correct category and severity. The aggregate
//! `KoshiError` wrapper is also tested to ensure it transparently delegates these operations
//! without losing information.

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

// The tests above exercise `KoshiError` through only 2 of its 8 `#[from]`
// variants (Pty, Storage). The rest are covered here: every variant's `.into()`
// must produce the exact same `to_string()`/`category()`/`severity()` as
// calling those methods on the unwrapped inner error directly — proving the
// `#[error(transparent)]` + delegating `DomainError` impl actually wires up
// for that arm, not just compiles.

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
    // `aggregate_delegates_and_is_transparent` above only ever wraps
    // `StorageError::Corrupt` (SessionFatal). `StorageError`'s severity is NOT
    // constant per-type like the others above — it varies per variant — so the
    // `Io` arm must be checked through the aggregate too, or a bug that made
    // every wrapped `StorageError` report SessionFatal would slip through.
    let inner = StorageError::Io {
        detail: "disk full".into(),
    };
    let want = inner.to_string();
    let err: KoshiError = inner.into();
    assert_eq!(err.to_string(), want);
    assert_eq!(err.category(), DomainCategory::Storage);
    assert_eq!(err.severity(), Severity::Recoverable);
}

// `CliError::category()` is the one wrapped type whose category is NOT
// constant per-type: `UnknownCommand`/`UnknownAction`/`InvalidArgs` classify as
// `DomainCategory::Cli`, but `IpcUnavailable` classifies as `DomainCategory::Ipc`
// and `Runtime` classifies as `DomainCategory::Session` — despite ALL of them
// living inside `KoshiError::Cli(..)`. E.g. `KoshiError::from(CliError::IpcUnavailable
// { detail: "..." })` + `.category()` returns `DomainCategory::Ipc`, NOT
// `DomainCategory::Cli`, which is wrong to assume from the wrapping variant's
// name alone. Every arm is checked below so a regression that collapses this
// back to a constant `DomainCategory::Cli` is caught.

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
    // `#[error(transparent)]` forwards `source()` to the WRAPPED error's own
    // `source()`, not `Some(&wrapped)`. None of `koshi-error`'s wrapped enums
    // mark any field `#[source]`/`#[from]` inside their own variants, so their
    // own `.source()` is `None` — e.g. `PtyError::Spawn { .. }.source()` is
    // `None` because `PtyError` never wraps a further inner error. Therefore
    // `KoshiError::from(PtyError::Spawn { .. }).source()` must also be `None`,
    // not `Some(&PtyError::Spawn { .. })` — the two are easy to conflate and
    // only one is what `#[error(transparent)]` actually does.
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
