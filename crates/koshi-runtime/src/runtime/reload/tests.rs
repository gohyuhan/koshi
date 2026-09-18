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

fn build_test_server() -> (Server, ClientId) {
    let mut server = build_test_server_without_sessions();
    let client_id = server
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    (server, client_id)
}

/// A runtime with no bootstrapped session — zero live clients to notify.
fn build_test_server_without_sessions() -> Server {
    let (event_sender, event_receiver) = mpsc::channel();
    Server::from_runtime_parts(
        Arc::new(FakePtyBackend::new()),
        event_receiver,
        event_sender,
    )
}

fn get_only_session_id(server: &Server) -> SessionId {
    *server.session_by_id.keys().next().expect("one session")
}

#[test]
fn load_startup_config_applies_the_app_layer_before_genesis() {
    let mut runtime = build_test_server_without_sessions();

    runtime.load_startup_config(Some(PartialKoshiConfig {
        pane: Some(PartialPaneConfig {
            minimum_column_count: Some(11),
            minimum_row_count: None,
            gap_cell_count: None,
        }),
        scrollback: Some(PartialScrollbackConfig {
            maximum_line_count: None,
            maximum_byte_count: None,
            should_scroll_to_input: Some(false),
        }),
        ..PartialKoshiConfig::default()
    }));

    assert_eq!(runtime.config.pane.minimum_column_count, 11);
    assert!(!runtime.client_config.scrollback.should_scroll_to_input);
}

#[test]
fn load_startup_config_without_a_file_leaves_the_built_in_defaults() {
    let mut runtime = build_test_server_without_sessions();

    runtime.load_startup_config(None);

    assert_eq!(runtime.config, ServerConfig::default());
    assert_eq!(runtime.client_config, ClientConfig::default());
}

#[test]
fn app_config_reload_replaces_the_startup_settings_and_notifies_each_session() {
    let (mut runtime, _client_id) = build_test_server();
    let session_id = get_only_session_id(&runtime);
    assert_eq!(
        runtime.config.pane.minimum_column_count, 2,
        "the built-in floor"
    );

    let reloaded_events = runtime.reload_app_config(PartialKoshiConfig {
        pane: Some(PartialPaneConfig {
            minimum_column_count: Some(7),
            minimum_row_count: None,
            gap_cell_count: None,
        }),
        ..PartialKoshiConfig::default()
    });
    assert_eq!(runtime.config.pane.minimum_column_count, 7);
    assert_eq!(
        reloaded_events,
        vec![Event::ConfigReloaded(ConfigReloaded { session_id })]
    );

    // An empty `koshi.kdl` replaces the whole app layer, so the floor falls
    // back to the built-in default rather than the previous file's value.
    runtime.reload_app_config(PartialKoshiConfig::default());
    assert_eq!(runtime.config.pane.minimum_column_count, 2);
}

#[test]
fn app_config_reload_drops_theme_and_keybinding_sections() {
    let (mut runtime, _client_id) = build_test_server();

    runtime.reload_app_config(PartialKoshiConfig {
        theme: Some(PartialThemeConfig {
            theme_name: None,
            colors: Some(PartialColorPalette {
                ramp_start: Some(RgbColor::from_channels(0xff, 0x00, 0x00)),
                ..PartialColorPalette::default()
            }),
        }),
        keybindings: Some(PartialKeybindingsConfig {
            max_chord_depth: Some(0),
            ..PartialKeybindingsConfig::default()
        }),
        ..PartialKoshiConfig::default()
    });

    // Both foreign sections were dropped: the stored layer holds neither, and
    // each side's effective config is exactly what it was, palette included.
    assert_eq!(runtime.app_layer.theme, None);
    assert_eq!(runtime.app_layer.keybindings, None);
    assert_eq!(runtime.config, ServerConfig::default());
    assert_eq!(runtime.client_config, ClientConfig::default());
}

#[test]
fn a_reload_notifies_every_live_session_in_session_id_order() {
    let mut runtime = build_test_server_without_sessions();
    let first_session_id = SessionId::new();
    let second_session_id = SessionId::new();
    for (session_id, session_name) in [(first_session_id, "alpha"), (second_session_id, "beta")] {
        runtime
            .bootstrap_local_named(
                session_id,
                session_name.to_owned(),
                Size {
                    column_count: 80,
                    row_count: 24,
                },
                SystemTime::UNIX_EPOCH,
            )
            .expect("bootstrap");
    }

    let reloaded_events = runtime.reload_app_config(PartialKoshiConfig::default());

    let mut ordered_session_ids = [first_session_id, second_session_id];
    ordered_session_ids.sort_unstable();
    assert_eq!(
        reloaded_events,
        vec![
            Event::ConfigReloaded(ConfigReloaded {
                session_id: ordered_session_ids[0],
            }),
            Event::ConfigReloaded(ConfigReloaded {
                session_id: ordered_session_ids[1],
            }),
        ]
    );
}

#[test]
fn app_config_reload_lands_the_session_owned_sections_on_the_server() {
    // The other reload tests assert the server config is *unchanged*, which
    // stays true even when the fold never runs. This one pins the opposite
    // direction: `koshi.kdl`'s session-owned sections reach the session config.
    let (mut runtime, _client_id) = build_test_server();
    assert_eq!(
        runtime.config.pane.minimum_column_count, 2,
        "the built-in floor"
    );
    assert_eq!(runtime.config.terminal.term, "xterm-256color");

    runtime.reload_app_config(PartialKoshiConfig {
        pane: Some(PartialPaneConfig {
            minimum_column_count: Some(20),
            minimum_row_count: Some(5),
            gap_cell_count: None,
        }),
        scrollback: Some(PartialScrollbackConfig {
            maximum_line_count: Some(50_000),
            maximum_byte_count: None,
            should_scroll_to_input: Some(false),
        }),
        terminal: Some(PartialTerminalConfig {
            term: Some("screen-256color".to_owned()),
            colorterm: None,
            default_shell: Some(Some("/bin/fish".to_owned())),
            extended_keys_mode: None,
        }),
        ..PartialKoshiConfig::default()
    });

    assert_eq!(runtime.config.pane.minimum_column_count, 20);
    assert_eq!(runtime.config.pane.minimum_row_count, 5);
    assert_eq!(runtime.config.scrollback.maximum_line_count, 50_000);
    assert_eq!(runtime.config.terminal.term, "screen-256color");
    assert_eq!(
        runtime.config.terminal.default_shell,
        Some("/bin/fish".to_owned())
    );

    // The same file's viewer-owned section went to the viewer config the
    // session folds.
    assert!(!runtime.client_config.scrollback.should_scroll_to_input);
}

#[test]
fn reload_with_no_live_sessions_emits_no_events_but_still_applies() {
    let mut runtime = build_test_server_without_sessions();

    let reloaded_events = runtime.reload_app_config(PartialKoshiConfig {
        pane: Some(PartialPaneConfig {
            minimum_column_count: Some(9),
            minimum_row_count: None,
            gap_cell_count: None,
        }),
        ..PartialKoshiConfig::default()
    });

    // No session means no one to notify, but the config still swaps.
    assert_eq!(reloaded_events, Vec::new());
    assert_eq!(runtime.config.pane.minimum_column_count, 9);
}
