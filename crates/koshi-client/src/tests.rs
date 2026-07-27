//! Tests for the viewer half: construction, viewport updates, the colors it
//! resolves from its own config, and discarding the subscribed event feed.

use std::sync::mpsc;

use koshi_config::conflict::keymap_layers;
use koshi_config::hints::KeymapHintCatalog;
use koshi_config::key::Leader;
use koshi_config::types::{KeybindingsConfig, RgbColor};
use koshi_core::event::{LayoutChanged, TabCreated};
use koshi_core::ids::TabId;
use koshi_core::registry::ActionRegistry;
use koshi_observability::cleanup::TerminalCleanupGuard;

use super::*;

fn new_client() -> (Client, mpsc::SyncSender<Event>) {
    with_config(ClientConfig::default())
}

fn with_config(config: ClientConfig) -> (Client, mpsc::SyncSender<Event>) {
    let (tx, rx) = mpsc::sync_channel(8);
    let keymap = KeymapHintCatalog::from_parts(
        &keymap_layers(None, Leader::default()),
        &KeybindingsConfig::default(),
        &ActionRegistry::new(),
    );
    let client = Client::new(
        ClientId::new(),
        Size { cols: 80, rows: 24 },
        rx,
        config,
        keymap,
        TerminalCleanupGuard::new(),
    );
    (client, tx)
}

/// A config whose focused-border role is `color`.
fn config_with_focused_border(color: RgbColor) -> ClientConfig {
    let mut config = ClientConfig::default();
    config.theme.colors.border_focused = color;
    config
}

#[test]
fn a_new_client_holds_its_id_viewport_and_guard() {
    let (client, _tx) = new_client();
    assert_eq!(client.viewport(), Size { cols: 80, rows: 24 });
    let _ = client.cleanup_guard();
}

#[test]
fn set_viewport_records_the_new_size() {
    let (mut client, _tx) = new_client();
    client.set_viewport(Size {
        cols: 120,
        rows: 40,
    });
    assert_eq!(
        client.viewport(),
        Size {
            cols: 120,
            rows: 40,
        }
    );
}

#[test]
fn a_new_client_resolves_its_colors_from_the_config_it_was_given() {
    let (client, _tx) = with_config(config_with_focused_border(RgbColor::new(1, 2, 3)));
    assert_eq!(
        client.theme().border_focused,
        ratatui::style::Color::Rgb(1, 2, 3)
    );
}

#[test]
fn a_default_config_client_paints_the_stock_colors() {
    let (client, _tx) = new_client();
    assert_eq!(*client.theme(), Theme::default());
}

#[test]
fn reloaded_settings_repaint_the_chrome_in_the_new_colors() {
    let (mut client, _tx) = new_client();
    assert_eq!(*client.theme(), Theme::default());

    client.set_config(config_with_focused_border(RgbColor::new(0xff, 0, 0)));

    assert_eq!(
        client.theme().border_focused,
        ratatui::style::Color::Rgb(0xff, 0, 0),
        "the new palette reaches the colors the next frame paints with"
    );
    assert_eq!(
        client.config().theme.colors.border_focused,
        RgbColor::new(0xff, 0, 0),
        "and the stored settings carry it too"
    );
}

#[test]
fn discard_events_drops_everything_delivered_and_counts_it() {
    let (mut client, tx) = new_client();
    let tab = TabId::new();
    tx.send(Event::TabCreated(TabCreated { tab_id: tab }))
        .expect("send into the subscription");
    tx.send(Event::LayoutChanged(LayoutChanged { tab_id: tab }))
        .expect("send into the subscription");

    assert_eq!(client.discard_events(), 2);
    assert_eq!(client.discard_events(), 0);

    // The queue is empty again: a later event is delivered and discarded anew.
    tx.send(Event::TabCreated(TabCreated { tab_id: tab }))
        .expect("send into the subscription");
    assert_eq!(client.discard_events(), 1);
}

#[test]
fn the_last_reload_wins_when_settings_are_swapped_twice() {
    // Reloads arrive one after another; the colors a frame paints with must
    // track the newest settings, not the first ones seen.
    let (mut client, _tx) = new_client();

    client.set_config(config_with_focused_border(RgbColor::new(1, 1, 1)));
    client.set_config(config_with_focused_border(RgbColor::new(2, 2, 2)));

    assert_eq!(
        client.theme().border_focused,
        ratatui::style::Color::Rgb(2, 2, 2)
    );
}

#[test]
fn reloading_the_settings_it_already_has_changes_nothing() {
    // A reload that resolves to the same settings leaves the client exactly as
    // it was — the palette is recomputed, not accumulated.
    let (mut client, _tx) = new_client();
    let before = *client.theme();

    client.set_config(ClientConfig::default());

    assert_eq!(*client.theme(), before);
    assert_eq!(*client.config(), ClientConfig::default());
}

#[test]
fn extreme_palette_values_survive_the_round_trip() {
    // The palette's endpoints are plain bytes; black and white must map
    // through unchanged rather than being clamped or shifted.
    let mut config = ClientConfig::default();
    config.theme.colors.border_focused = RgbColor::new(0, 0, 0);
    config.theme.colors.border_unfocused = RgbColor::new(0xff, 0xff, 0xff);
    config.theme.colors.ramp_start = RgbColor::new(0, 0, 0);
    config.theme.colors.ramp_end = RgbColor::new(0xff, 0xff, 0xff);

    let (client, _tx) = with_config(config);

    assert_eq!(
        client.theme().border_focused,
        ratatui::style::Color::Rgb(0, 0, 0)
    );
    assert_eq!(
        client.theme().border_unfocused,
        ratatui::style::Color::Rgb(0xff, 0xff, 0xff)
    );
    assert_eq!(client.theme().ramp_start, (0, 0, 0));
    assert_eq!(client.theme().ramp_end, (0xff, 0xff, 0xff));
}
