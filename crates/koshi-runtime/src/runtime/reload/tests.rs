//! Tests for the `koshi.kdl` transaction: what each side's effective config
//! folds to, that the file's foreign sections are dropped, and the events one
//! apply publishes.

use super::*;

use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use koshi_config::layer::{
    PartialColorPalette, PartialKeybindingsConfig, PartialPaneConfig, PartialScrollbackConfig,
    PartialTerminalConfig, PartialThemeConfig,
};
use koshi_config::types::RgbColor;
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, SessionId};
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage};

fn runtime() -> (Server, ClientId) {
    let mut runtime = runtime_with_no_sessions();
    let client = runtime
        .bootstrap_local(
            SessionId::new(),
            Size { cols: 80, rows: 24 },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    (runtime, client)
}

/// A runtime with no bootstrapped session — zero live clients to notify.
fn runtime_with_no_sessions() -> Server {
    let (tx, rx) = mpsc::channel();
    Server::new(
        Arc::new(FakePtyBackend::new()),
        Arc::new(NullSnapshotProvider),
        Arc::new(NullStorage),
        rx,
        tx,
    )
}

fn only_session_id(runtime: &Server) -> SessionId {
    *runtime.sessions.keys().next().expect("one session")
}

#[test]
fn load_startup_config_applies_the_app_layer_before_genesis() {
    let mut runtime = runtime_with_no_sessions();

    runtime.load_startup_config(Some(PartialKoshiConfig {
        pane: Some(PartialPaneConfig {
            min_cols: Some(11),
            min_rows: None,
            gap: None,
        }),
        scrollback: Some(PartialScrollbackConfig {
            max_lines: None,
            max_bytes: None,
            scroll_on_input: Some(false),
        }),
        ..PartialKoshiConfig::default()
    }));

    assert_eq!(runtime.config.pane.min_cols, 11);
    assert!(!runtime.client_config.scrollback.scroll_on_input);
}

#[test]
fn load_startup_config_without_a_file_leaves_the_built_in_defaults() {
    let mut runtime = runtime_with_no_sessions();

    runtime.load_startup_config(None);

    assert_eq!(runtime.config, ServerConfig::default());
    assert_eq!(runtime.client_config, ClientConfig::default());
}

#[test]
fn app_config_reload_replaces_the_startup_settings_and_notifies_each_session() {
    let (mut runtime, _client) = runtime();
    let session_id = only_session_id(&runtime);
    assert_eq!(runtime.config.pane.min_cols, 2, "the built-in floor");

    let events = runtime.reload_app_config(PartialKoshiConfig {
        pane: Some(PartialPaneConfig {
            min_cols: Some(7),
            min_rows: None,
            gap: None,
        }),
        ..PartialKoshiConfig::default()
    });
    assert_eq!(runtime.config.pane.min_cols, 7);
    assert_eq!(
        events,
        vec![Event::ConfigReloaded(ConfigReloaded { session_id })]
    );

    // An empty `koshi.kdl` replaces the whole app layer, so the floor falls
    // back to the built-in default rather than the previous file's value.
    runtime.reload_app_config(PartialKoshiConfig::default());
    assert_eq!(runtime.config.pane.min_cols, 2);
}

#[test]
fn app_config_reload_drops_theme_and_keybinding_sections() {
    let (mut runtime, _client) = runtime();

    runtime.reload_app_config(PartialKoshiConfig {
        theme: Some(PartialThemeConfig {
            name: None,
            colors: Some(PartialColorPalette {
                ramp_start: Some(RgbColor::new(0xff, 0x00, 0x00)),
                ..PartialColorPalette::default()
            }),
        }),
        keybindings: Some(PartialKeybindingsConfig {
            max_chord_depth: Some(0),
            ..PartialKeybindingsConfig::default()
        }),
        ..PartialKoshiConfig::default()
    });

    // Both foreign sections were dropped: each side's effective config is
    // exactly what it was, palette included.
    assert_eq!(runtime.config, ServerConfig::default());
    assert_eq!(runtime.client_config, ClientConfig::default());
}

#[test]
fn app_config_reload_lands_the_session_owned_sections_on_the_server() {
    // The other reload tests all assert the server config is *unchanged*, which
    // stays true even if the fold never runs. This one pins the opposite
    // direction: `koshi.kdl`'s session-owned sections must actually reach the
    // session, or every pane would spawn the stock shell at the stock size
    // while the suite stayed green.
    let (mut runtime, _client) = runtime();
    assert_eq!(runtime.config.pane.min_cols, 2, "the built-in floor");
    assert_eq!(runtime.config.terminal.term, "xterm-256color");

    runtime.reload_app_config(PartialKoshiConfig {
        pane: Some(PartialPaneConfig {
            min_cols: Some(20),
            min_rows: Some(5),
            gap: None,
        }),
        scrollback: Some(PartialScrollbackConfig {
            max_lines: Some(50_000),
            max_bytes: None,
            scroll_on_input: Some(false),
        }),
        terminal: Some(PartialTerminalConfig {
            term: Some("screen-256color".to_owned()),
            colorterm: None,
            default_shell: Some(Some("/bin/fish".to_owned())),
        }),
        ..PartialKoshiConfig::default()
    });

    assert_eq!(runtime.config.pane.min_cols, 20);
    assert_eq!(runtime.config.pane.min_rows, 5);
    assert_eq!(runtime.config.scrollback.max_lines, 50_000);
    assert_eq!(runtime.config.terminal.term, "screen-256color");
    assert_eq!(
        runtime.config.terminal.default_shell,
        Some("/bin/fish".to_owned())
    );

    // The same file's viewer-owned section went to the transitional viewer
    // config the session still folds.
    assert!(!runtime.client_config.scrollback.scroll_on_input);
}

#[test]
fn reload_with_no_live_sessions_emits_no_events_but_still_applies() {
    let mut runtime = runtime_with_no_sessions();

    let events = runtime.reload_app_config(PartialKoshiConfig {
        pane: Some(PartialPaneConfig {
            min_cols: Some(9),
            min_rows: None,
            gap: None,
        }),
        ..PartialKoshiConfig::default()
    });

    // No session means no one to notify, but the config still swaps.
    assert_eq!(events, Vec::new());
    assert_eq!(runtime.config.pane.min_cols, 9);
}
