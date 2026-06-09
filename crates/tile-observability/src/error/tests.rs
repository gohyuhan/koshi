use super::*;

// Display snapshots + severity assignments for every domain error, exercised
// both directly and after wrapping into the aggregate (`TILE_12`).

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
    let err: TileError = inner.into();
    // Display is transparent: the aggregate prints exactly the inner error.
    assert_eq!(err.to_string(), want);
    assert_eq!(err.category(), DomainCategory::Pty);
    assert_eq!(err.severity(), Severity::Recoverable);

    let corrupt: TileError = StorageError::Corrupt { detail: "x".into() }.into();
    assert_eq!(corrupt.severity(), Severity::SessionFatal);
}
