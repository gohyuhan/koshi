//! Tests for the viewer half: construction, viewport updates, the settings and
//! colors it reads from its own config files, the keymap it validates before
//! trusting, the hints one frame is painted from, and what it takes off its
//! subscription — live events, the frame it resumes from after a lag, and the
//! items meant for a client in another process.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc;

use koshi_config::hints::KeyMatch;
use koshi_config::key::Leader;
use koshi_config::layer::{PartialColorPalette, PartialLayoutDefaults};
use koshi_config::types::{
    BoundAction, KeybindingsConfig, ModeBindings, ModeName, RgbColor, WheelScroll,
};
use koshi_core::action::{ActionReference, MOUSE_SELECT_HINT, MOUSE_UNSELECT_HINT};
use koshi_core::event::{EventClass, InputModeChanged, MouseSelectChanged, SubscriberLagged};
use koshi_core::geometry::Direction;
use koshi_core::ids::{PaneId, SessionId, SubscriberId, TabId};
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseAnswer;
use koshi_core::resolve::ActionArgs;
use koshi_layout::mode::LayoutMode;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::snapshot::{
    ClientSnapshot, PluginUiSnapshot, RenderSnapshot, SessionSnapshot, TabSnapshot,
};

use super::*;

pub(crate) use koshi_test_support::fixtures::build_key_input_for_chord;

/// The terminal size every fixture in this crate's tests is built at.
pub(crate) const TEST_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// A viewer on the built-in defaults, and the sender of its subscription.
///
/// Every test module in this crate builds its clients from here. An attached
/// client's frames arrive over its connection instead, so a test standing in
/// for one drops the sender.
pub(crate) fn build_test_client_with_event_sender() -> (Client, mpsc::SyncSender<Delivery>) {
    let (tx, rx) = mpsc::sync_channel(8);
    let client = Client::from_client_id_and_viewport(
        ClientId::new(),
        TEST_VIEWPORT_SIZE,
        rx,
        TerminalCleanupGuard::new(),
    );
    (client, tx)
}

/// A viewer that read a theme file painting the focused border `color`.
fn with_focused_border(color: RgbColor) -> (Client, mpsc::SyncSender<Delivery>) {
    let (mut client, tx) = build_test_client_with_event_sender();
    client.load_startup_config(None, Some(focused_border(color)), None);
    (client, tx)
}

/// A frame naming `client_id` in `lock_mode` with mouse-selection mode
/// `is_mouse_selection_enabled`:
/// one empty tab, no panes, no plugin UI.
fn build_render_snapshot(
    client_id: ClientId,
    lock_mode: LockMode,
    is_mouse_selection_enabled: bool,
) -> Box<RenderSnapshot> {
    let active_tab_id = TabId::new();
    Box::new(RenderSnapshot {
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_revision: 0,
            session_name: String::from("session"),
            active_tab_snapshot: TabSnapshot {
                tab_id: active_tab_id,
                tab_name: String::from("tab"),
                pane_slots: Vec::new(),
                effective_cell_size: Size {
                    column_count: 80,
                    row_count: 24,
                },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                are_all_panes_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata: Vec::new(),
        },
        pane_snapshots: Vec::new(),
        client_snapshot: ClientSnapshot {
            client_id,
            client_revision: 0,
            viewport_size: Size {
                column_count: 80,
                row_count: 24,
            },
            active_tab_id,
            focused_pane_id: None,
            lock_mode,
            is_mouse_selection_enabled,
        },
        plugin_ui_snapshot: PluginUiSnapshot::default(),
    })
}

/// The frame the session sends a viewer whose queue overflowed, reporting
/// `client_id` in `lock_mode` with mouse-selection mode
/// `is_mouse_selection_enabled`, after
/// `dropped_count` events it will never see.
fn resync(
    client_id: ClientId,
    lock_mode: LockMode,
    is_mouse_selection_enabled: bool,
    dropped_count: u64,
) -> Delivery {
    resync_from(
        build_render_snapshot(client_id, lock_mode, is_mouse_selection_enabled),
        dropped_count,
    )
}

/// The same, resuming from `snapshot`, whose active tab the caller reads.
fn resync_from(snapshot: Box<RenderSnapshot>, dropped_count: u64) -> Delivery {
    Delivery::Snapshot {
        render_snapshot: snapshot,
        lag_report: SubscriberLagged {
            subscriber_id: SubscriberId::new(),
            dropped_event_count: dropped_count,
            event_class: EventClass::Critical,
        },
    }
}

/// A theme file whose focused-border role is `color`.
fn focused_border(color: RgbColor) -> PartialThemeConfig {
    PartialThemeConfig {
        theme_name: None,
        colors: Some(PartialColorPalette {
            border_focused: Some(color),
            ..PartialColorPalette::default()
        }),
    }
}

/// A `keybinding.kdl` binding `<C-y>` to `core:new-tab` in `normal` mode.
fn binds_ctrl_y() -> PartialKeybindingsConfig {
    let mut bound_action_by_key_sequence = BTreeMap::new();
    bound_action_by_key_sequence.insert(
        KeySequence::from(KeyChord::from_parts(ModFlags::CTRL, Key::Char('y'))),
        BoundAction {
            action_reference: ActionReference::from_core_action_name("new-tab")
                .expect("valid core action name"),
            action_arguments: ActionArgs::None,
        },
    );
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("normal"),
        ModeBindings {
            bound_action_by_key_sequence,
            removed_key_sequences: BTreeSet::new(),
        },
    );
    PartialKeybindingsConfig {
        mode_bindings_by_name: Some(mode_bindings_by_name),
        ..PartialKeybindingsConfig::default()
    }
}

#[test]
fn a_new_client_holds_the_viewport_it_was_built_at() {
    let (client, _tx) = build_test_client_with_event_sender();
    assert_eq!(client.get_viewport_size(), TEST_VIEWPORT_SIZE);
}

#[test]
fn set_viewport_records_the_new_size() {
    let (mut client, _tx) = build_test_client_with_event_sender();
    client.set_viewport(Size {
        column_count: 120,
        row_count: 40,
    });
    assert_eq!(
        client.get_viewport_size(),
        Size {
            column_count: 120,
            row_count: 40,
        }
    );
}

#[test]
fn the_core_pane_area_reserves_the_two_chrome_rows() {
    assert_eq!(
        compute_core_pane_area(Size {
            column_count: 80,
            row_count: 24
        }),
        PaneArea::Reported(Size {
            column_count: 80,
            row_count: 22
        })
    );
    assert_eq!(
        compute_core_pane_area(Size {
            column_count: 80,
            row_count: 1
        }),
        PaneArea::Reported(Size {
            column_count: 80,
            row_count: 0
        })
    );
}

#[test]
fn a_theme_file_recolors_the_chrome_the_next_frame_paints_with() {
    let (client, _tx) = with_focused_border(RgbColor::from_channels(1, 2, 3));
    assert_eq!(
        client.get_theme().focused_border_color,
        ratatui::style::Color::Rgb(1, 2, 3)
    );
    assert_eq!(
        client.get_client_config().theme.colors.border_focused,
        RgbColor::from_channels(1, 2, 3),
        "and the stored settings carry it too"
    );
}

#[test]
fn a_theme_files_name_reaches_the_viewers_settings() {
    let (mut client, _tx) = build_test_client_with_event_sender();

    client.load_startup_config(
        None,
        Some(PartialThemeConfig {
            theme_name: Some("midnight".to_owned()),
            colors: Some(PartialColorPalette {
                accent: Some(RgbColor::from_channels(9, 8, 7)),
                ..PartialColorPalette::default()
            }),
        }),
        None,
    );

    assert_eq!(client.get_client_config().theme.theme_name, "midnight");
    assert_eq!(
        client.get_client_config().theme.colors.accent,
        RgbColor::from_channels(9, 8, 7)
    );
}

#[test]
fn a_default_config_client_paints_the_stock_colors() {
    let (client, _tx) = build_test_client_with_event_sender();
    assert_eq!(*client.get_theme(), Theme::default());
    assert_eq!(*client.get_client_config(), ClientConfig::default());
}

#[test]
fn koshi_kdls_viewer_owned_sections_reach_the_viewers_settings() {
    // `koshi.kdl` carries sections both halves read. The viewer must fold its
    // own out of the same file, or a configured split direction and wheel
    // behavior would silently never apply.
    let (mut client, _tx) = build_test_client_with_event_sender();
    assert_eq!(
        client.get_client_config().layout.new_pane_direction,
        Direction::Right,
        "the built-in default"
    );

    client.load_startup_config(
        Some(PartialKoshiConfig {
            layout: Some(PartialLayoutDefaults {
                new_pane_direction: Some(Direction::Down),
            }),
            mouse: Some(koshi_config::layer::PartialMouseConfig {
                can_resize_pane_border: None,
                scroll_line_count: Some(7),
                wheel: Some(WheelScroll::Ignore),
            }),
            ..PartialKoshiConfig::default()
        }),
        None,
        None,
    );

    assert_eq!(
        client.get_client_config().layout.new_pane_direction,
        Direction::Down
    );
    assert_eq!(client.get_client_config().mouse.scroll_line_count, 7);
    assert_eq!(client.get_client_config().mouse.wheel, WheelScroll::Ignore);
}

#[test]
fn the_last_load_wins_when_settings_are_read_twice() {
    // The colors a frame paints with must track the newest settings, not the
    // first ones seen.
    let (mut client, _tx) = build_test_client_with_event_sender();

    client.load_startup_config(
        None,
        Some(focused_border(RgbColor::from_channels(1, 1, 1))),
        None,
    );
    client.load_startup_config(
        None,
        Some(focused_border(RgbColor::from_channels(2, 2, 2))),
        None,
    );

    assert_eq!(
        client.get_theme().focused_border_color,
        ratatui::style::Color::Rgb(2, 2, 2)
    );
}

#[test]
fn loading_no_files_at_all_leaves_the_built_in_settings() {
    // A run with no config files resolves to the same settings a fresh viewer
    // holds — the palette is recomputed, not accumulated.
    let (mut client, _tx) = build_test_client_with_event_sender();
    let original_theme = *client.get_theme();

    let report = client.load_startup_config(None, None, None);

    assert_eq!(report, None, "no keybinding file means no report");
    assert_eq!(*client.get_theme(), original_theme);
    assert_eq!(*client.get_client_config(), ClientConfig::default());
}

#[test]
fn extreme_palette_values_survive_the_round_trip() {
    // The palette's endpoints are plain bytes; black and white must map
    // through unchanged rather than being clamped or shifted.
    let (mut client, _tx) = build_test_client_with_event_sender();

    client.load_startup_config(
        None,
        Some(PartialThemeConfig {
            theme_name: None,
            colors: Some(PartialColorPalette {
                border_focused: Some(RgbColor::from_channels(0, 0, 0)),
                border_unfocused: Some(RgbColor::from_channels(0xff, 0xff, 0xff)),
                ramp_start: Some(RgbColor::from_channels(0, 0, 0)),
                ramp_end: Some(RgbColor::from_channels(0xff, 0xff, 0xff)),
                ..PartialColorPalette::default()
            }),
        }),
        None,
    );

    assert_eq!(
        client.get_theme().focused_border_color,
        ratatui::style::Color::Rgb(0, 0, 0)
    );
    assert_eq!(
        client.get_theme().unfocused_border_color,
        ratatui::style::Color::Rgb(0xff, 0xff, 0xff)
    );
    assert_eq!(client.get_theme().ramp_start, (0, 0, 0));
    assert_eq!(client.get_theme().ramp_end, (0xff, 0xff, 0xff));
}

#[test]
fn an_applied_keybinding_file_swaps_the_keymap_and_drops_an_open_sequence() {
    let (mut client, _tx) = build_test_client_with_event_sender();
    let ctrl_y = KeySequence::from(KeyChord::from_parts(ModFlags::CTRL, Key::Char('y')));
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_y)
            .exact_bound_action,
        None,
        "nothing is bound to `<C-y>` out of the box"
    );
    // `<C-p>` opens the shipped pane group, so the viewer is mid-sequence.
    client.resolve_key(
        KeyChord::from_parts(ModFlags::CTRL, Key::Char('p')),
        std::time::Instant::now(),
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        Some(&KeySequence::from(KeyChord::from_parts(
            ModFlags::CTRL,
            Key::Char('p')
        )))
    );

    let report = client.load_startup_config(None, None, Some(binds_ctrl_y()));

    assert_eq!(
        report.expect("a keybinding file was given").get_verdict(),
        KeymapVerdict::Apply
    );
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_y)
            .exact_bound_action,
        Some(BoundAction {
            action_reference: ActionReference::from_core_action_name("new-tab")
                .expect("valid core action name"),
            action_arguments: ActionArgs::None,
        })
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        None,
        "held chords reached for bindings the new keymap may not have"
    );
}

#[test]
fn a_keybinding_file_can_move_the_leader_the_defaults_hang_off() {
    let (mut client, _tx) = build_test_client_with_event_sender();
    let ctrl_p = KeySequence::from(KeyChord::from_parts(ModFlags::CTRL, Key::Char('p')));
    let alt_p = KeySequence::from(KeyChord::from_parts(ModFlags::ALT, Key::Char('p')));
    assert!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_p)
            .has_longer_key_sequence
    );

    client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            leader: Some(Leader::Mods(ModFlags::ALT)),
            ..PartialKeybindingsConfig::default()
        }),
    );

    assert!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &alt_p)
            .has_longer_key_sequence
    );
    assert!(
        !client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_p)
            .has_longer_key_sequence
    );
}

#[test]
fn a_refused_keybinding_file_leaves_both_the_keymap_and_the_settings_on_the_built_ins() {
    // `max_chord_depth` 0 would stop every binding from resolving, the
    // locked-mode unlock included, so the whole file is refused. The folded
    // settings must keep describing the keymap actually in use.
    let (mut client, _tx) = build_test_client_with_event_sender();
    let ctrl_p = KeySequence::from(KeyChord::from_parts(ModFlags::CTRL, Key::Char('p')));

    let report = client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            max_chord_depth: Some(0),
            ..PartialKeybindingsConfig::default()
        }),
    );

    assert_eq!(
        report.expect("a keybinding file was given").get_verdict(),
        KeymapVerdict::Reject
    );
    assert_eq!(
        client.get_client_config().keybindings,
        KeybindingsConfig::default(),
        "the refused file's settings must not describe the running keymap"
    );
    assert!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_p)
            .has_longer_key_sequence,
        "the shipped two-chord defaults still open under `<C-p>`"
    );
}

#[test]
fn a_refused_keybinding_file_leaves_an_open_sequence_alone() {
    // Only a keymap that actually swapped retires the bindings the held chords
    // reach for. A refusal changes no binding, so the sequence being typed
    // still means what it meant and stays open.
    let (mut client, _tx) = build_test_client_with_event_sender();
    client.resolve_key(
        KeyChord::from_parts(ModFlags::CTRL, Key::Char('p')),
        std::time::Instant::now(),
    );

    client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            max_chord_depth: Some(0),
            ..PartialKeybindingsConfig::default()
        }),
    );

    assert_eq!(
        client
            .get_pending_key_sequence()
            .map(|sequence| sequence.list_chords().to_vec()),
        Some(vec![KeyChord::from_parts(ModFlags::CTRL, Key::Char('p'))])
    );
}

#[test]
fn a_good_keybinding_file_still_applies_after_a_refused_one() {
    // A refusal must leave the viewer usable, not wedged: the next file it
    // reads applies normally.
    let (mut client, _tx) = build_test_client_with_event_sender();
    let ctrl_y = KeySequence::from(KeyChord::from_parts(ModFlags::CTRL, Key::Char('y')));

    client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            max_chord_depth: Some(0),
            ..PartialKeybindingsConfig::default()
        }),
    );
    let report = client.load_startup_config(None, None, Some(binds_ctrl_y()));

    assert_eq!(
        report.expect("a keybinding file was given").get_verdict(),
        KeymapVerdict::Apply
    );
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_y)
            .exact_bound_action,
        Some(BoundAction {
            action_reference: ActionReference::from_core_action_name("new-tab")
                .expect("valid core action name"),
            action_arguments: ActionArgs::None,
        })
    );
}

#[test]
fn a_keybinding_file_cannot_smuggle_colors_in_through_koshi_kdl() {
    // `koshi.kdl`'s theme section is dropped, so with no theme file present the
    // viewer paints the built-in palette rather than the app file's colors.
    let (mut client, _tx) = build_test_client_with_event_sender();

    client.load_startup_config(
        Some(PartialKoshiConfig {
            theme: Some(PartialThemeConfig {
                theme_name: Some("smuggled".to_owned()),
                colors: Some(PartialColorPalette {
                    border_focused: Some(RgbColor::from_channels(9, 9, 9)),
                    ..PartialColorPalette::default()
                }),
            }),
            ..PartialKoshiConfig::default()
        }),
        None,
        None,
    );

    assert_eq!(*client.get_theme(), Theme::default());
    assert_eq!(
        client.get_client_config().theme,
        ClientConfig::default().theme
    );
}

#[test]
fn a_refused_keybinding_file_keeps_the_reserved_unlock_firing() {
    // Binding the reserved unlock chord in locked mode is fatal: the file is
    // refused whole and the guaranteed escape stays live.
    let (mut client, _tx) = build_test_client_with_event_sender();
    let unlock_key = KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK);
    let mut bound_action_by_key_sequence = BTreeMap::new();
    bound_action_by_key_sequence.insert(
        unlock_key.clone(),
        BoundAction {
            action_reference: ActionReference::from_core_action_name("new-tab")
                .expect("valid core action name"),
            action_arguments: ActionArgs::None,
        },
    );
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("locked"),
        ModeBindings {
            bound_action_by_key_sequence,
            removed_key_sequences: BTreeSet::new(),
        },
    );

    let report = client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            mode_bindings_by_name: Some(mode_bindings_by_name),
            ..PartialKeybindingsConfig::default()
        }),
    );

    assert_eq!(
        report.expect("a keybinding file was given").get_verdict(),
        KeymapVerdict::Reject
    );
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Locked, &unlock_key)
            .exact_bound_action,
        Some(BoundAction {
            action_reference: ActionReference::from_core_action_name("unlock")
                .expect("valid core action name"),
            action_arguments: ActionArgs::None,
        })
    );
}

#[test]
fn a_refused_keybinding_file_still_applies_the_theme_beside_it() {
    // The three files are read in one call; one being refused must not take
    // the others down with it.
    let (mut client, _tx) = build_test_client_with_event_sender();

    client.load_startup_config(
        None,
        Some(focused_border(RgbColor::from_channels(4, 5, 6))),
        Some(PartialKeybindingsConfig {
            max_chord_depth: Some(0),
            ..PartialKeybindingsConfig::default()
        }),
    );

    assert_eq!(
        client.get_theme().focused_border_color,
        ratatui::style::Color::Rgb(4, 5, 6)
    );
}

#[test]
fn a_second_keybinding_file_fully_replaces_the_firsts_bindings() {
    let (mut client, _tx) = build_test_client_with_event_sender();
    let ctrl_y = KeySequence::from(KeyChord::from_parts(ModFlags::CTRL, Key::Char('y')));

    client.load_startup_config(None, None, Some(binds_ctrl_y()));
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_y)
            .exact_bound_action,
        Some(BoundAction {
            action_reference: ActionReference::from_core_action_name("new-tab")
                .expect("valid core action name"),
            action_arguments: ActionArgs::None,
        })
    );

    client.load_startup_config(None, None, Some(PartialKeybindingsConfig::default()));

    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_y)
            .exact_bound_action,
        None,
        "the first file's binding must not survive the second"
    );
}

#[test]
fn a_second_startup_load_with_no_keybinding_file_resets_the_keymap_to_the_built_ins() {
    // An absent `keybinding.kdl` means its defaults stand, so a binding an
    // earlier load installed stops resolving.
    let (mut client, _tx) = build_test_client_with_event_sender();
    let ctrl_y = KeySequence::from(KeyChord::from_parts(ModFlags::CTRL, Key::Char('y')));

    client.load_startup_config(None, None, Some(binds_ctrl_y()));
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_y)
            .exact_bound_action,
        Some(BoundAction {
            action_reference: ActionReference::from_core_action_name("new-tab")
                .expect("valid core action name"),
            action_arguments: ActionArgs::None,
        })
    );

    let report = client.load_startup_config(None, None, None);

    assert_eq!(report, None, "no keybinding file means no report");
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_y)
            .exact_bound_action,
        None,
        "the earlier file's binding must not outlive it"
    );
    assert_eq!(*client.get_client_config(), ClientConfig::default());
}

#[test]
fn a_low_chord_depth_drops_every_binding_longer_than_it() {
    let (mut client, _tx) = build_test_client_with_event_sender();
    let long_key_sequence = KeySequence::from_first_and_rest(
        KeyChord::from_parts(ModFlags::CTRL, Key::Char('y')),
        vec![KeyChord::from_parts(ModFlags::NONE, Key::Char('x'))],
    );
    let mut bound_action_by_key_sequence = BTreeMap::new();
    bound_action_by_key_sequence.insert(
        long_key_sequence.clone(),
        BoundAction {
            action_reference: ActionReference::from_core_action_name("new-tab")
                .expect("valid core action name"),
            action_arguments: ActionArgs::None,
        },
    );
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("normal"),
        ModeBindings {
            bound_action_by_key_sequence,
            removed_key_sequences: BTreeSet::new(),
        },
    );

    let report = client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            max_chord_depth: Some(1),
            mode_bindings_by_name: Some(mode_bindings_by_name),
            ..PartialKeybindingsConfig::default()
        }),
    );

    // Depth 1 applies — with a warning naming the unreachable binding.
    assert_eq!(
        report.expect("a keybinding file was given").get_verdict(),
        KeymapVerdict::Apply
    );
    // The overlong binding is transparent: no exact match, and its first chord
    // is not a live prefix, so it falls through to the pane.
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &long_key_sequence),
        KeyMatch::default()
    );
    // The shipped two-chord defaults fall the same way.
    assert_eq!(
        client.keymap_catalog.match_sequence(
            LockMode::Normal,
            &KeySequence::from(KeyChord::from_parts(ModFlags::CTRL, Key::Char('p')))
        ),
        KeyMatch::default()
    );
    // The one-chord unlock is untouched.
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(
                LockMode::Locked,
                &KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)
            )
            .exact_bound_action,
        Some(BoundAction {
            action_reference: ActionReference::from_core_action_name("unlock")
                .expect("valid core action name"),
            action_arguments: ActionArgs::None,
        })
    );
}

#[test]
fn a_keybinding_file_removes_a_default_binding_only_in_the_mode_that_declares_it() {
    let (mut client, _tx) = build_test_client_with_event_sender();
    let quit = KeySequence::from(KeyChord::from_parts(ModFlags::CTRL, Key::Char('q')));
    let quit_action = BoundAction {
        action_reference: ActionReference::from_core_action_name("quit")
            .expect("valid core action name"),
        action_arguments: ActionArgs::None,
    };
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &quit)
            .exact_bound_action,
        Some(quit_action.clone())
    );
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Locked, &quit)
            .exact_bound_action,
        Some(quit_action.clone())
    );

    let mut removed_key_sequences = BTreeSet::new();
    removed_key_sequences.insert(quit.clone());
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("normal"),
        ModeBindings {
            bound_action_by_key_sequence: BTreeMap::new(),
            removed_key_sequences,
        },
    );
    let report = client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            mode_bindings_by_name: Some(mode_bindings_by_name),
            ..PartialKeybindingsConfig::default()
        }),
    );

    assert_eq!(
        report.expect("a keybinding file was given").get_verdict(),
        KeymapVerdict::Apply
    );
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &quit),
        KeyMatch::default()
    );
    assert!(client
        .build_frame_hints(client.get_lock_mode(), false)
        .removed_key_sequences
        .contains(&quit));
    // Locked mode's own quit binding is untouched: removal is scoped to the
    // mode that declares it.
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Locked, &quit)
            .exact_bound_action,
        Some(quit_action)
    );
}

/// How many hint bindings in `keymap_hints` carry `action_display_name`.
fn count_hint_bindings_with_action_display_name(
    keymap_hints: &KeymapHints,
    action_display_name: &str,
) -> usize {
    keymap_hints
        .hint_bindings
        .iter()
        .filter(|hint_binding| hint_binding.action_display_name == action_display_name)
        .count()
}

#[test]
fn frame_hints_flip_the_mouse_select_label_only_while_it_is_on() {
    let (client, _tx) = build_test_client_with_event_sender();

    let mouse_selection_disabled_hints = client.build_frame_hints(client.get_lock_mode(), false);
    let mouse_selection_enabled_hints = client.build_frame_hints(client.get_lock_mode(), true);

    assert_eq!(
        count_hint_bindings_with_action_display_name(
            &mouse_selection_disabled_hints,
            MOUSE_SELECT_HINT,
        ),
        1
    );
    assert_eq!(
        count_hint_bindings_with_action_display_name(
            &mouse_selection_disabled_hints,
            MOUSE_UNSELECT_HINT,
        ),
        0
    );
    assert_eq!(
        count_hint_bindings_with_action_display_name(
            &mouse_selection_enabled_hints,
            MOUSE_UNSELECT_HINT,
        ),
        1
    );
    assert_eq!(
        count_hint_bindings_with_action_display_name(
            &mouse_selection_enabled_hints,
            MOUSE_SELECT_HINT,
        ),
        0
    );
    // Only that one entry changes: everything else is the same list.
    assert_eq!(
        mouse_selection_disabled_hints.hint_bindings.len(),
        mouse_selection_enabled_hints.hint_bindings.len()
    );
    assert_eq!(
        mouse_selection_disabled_hints.removed_key_sequences,
        mouse_selection_enabled_hints.removed_key_sequences
    );
    assert_eq!(
        mouse_selection_disabled_hints.is_reverted_to_defaults,
        mouse_selection_enabled_hints.is_reverted_to_defaults
    );
}

#[test]
fn frame_hints_follow_the_viewers_own_mode() {
    let (mut client, _tx) = build_test_client_with_event_sender();
    let normal_mode_hints = client.build_frame_hints(client.get_lock_mode(), false);
    client.set_lock_mode(LockMode::Locked);
    let locked_mode_hints = client.build_frame_hints(client.get_lock_mode(), false);

    assert_eq!(
        normal_mode_hints.hint_bindings.len(),
        22,
        "the shipped normal-mode bindings"
    );
    // The reserved unlock (pinned) plus the quit and mouse-select chords.
    assert_eq!(locked_mode_hints.hint_bindings.len(), 3);
    assert!(locked_mode_hints.hint_bindings.iter().any(|hint_binding| {
        hint_binding.action_display_name == "Unlock" && hint_binding.is_pinned
    }));
    assert!(locked_mode_hints.hint_bindings.iter().any(|hint_binding| {
        hint_binding.action_display_name == "Quit" && !hint_binding.is_pinned
    }));
}

#[test]
fn a_mouse_select_report_for_this_viewer_flips_its_own_copy() {
    // The viewer routes a mouse press against its own copy of the mode, so the
    // session's report is what has to move it — both ways.
    let (mut client, tx) = build_test_client_with_event_sender();
    assert!(
        !client.is_mouse_selection_enabled(),
        "a fresh viewer selects nothing"
    );

    tx.send(Delivery::Event(Event::MouseSelectChanged(
        MouseSelectChanged {
            client_id: client.get_client_id(),
            is_enabled: true,
        },
    )))
    .expect("the viewer's queue has room");
    assert_eq!(client.apply_events(), 1);
    assert!(
        client.is_mouse_selection_enabled(),
        "the report turned it on"
    );

    tx.send(Delivery::Event(Event::MouseSelectChanged(
        MouseSelectChanged {
            client_id: client.get_client_id(),
            is_enabled: false,
        },
    )))
    .expect("the viewer's queue has room");
    assert_eq!(client.apply_events(), 1);
    assert!(
        !client.is_mouse_selection_enabled(),
        "the second report turned it off"
    );
}

#[test]
fn a_mouse_select_report_for_another_viewer_is_ignored() {
    // Mouse select is client-scoped: two viewers of one session hold their own,
    // and a subscription carries every client's events.
    let (mut client, tx) = build_test_client_with_event_sender();

    tx.send(Delivery::Event(Event::MouseSelectChanged(
        MouseSelectChanged {
            client_id: ClientId::new(),
            is_enabled: true,
        },
    )))
    .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 1, "the event was seen");
    assert!(
        !client.is_mouse_selection_enabled(),
        "and it was not applied here"
    );
}

#[test]
fn a_lock_report_for_this_viewer_moves_its_own_mode_both_ways() {
    // The viewer decides what a key means against its own copy of the mode, so
    // `koshi lock --client` reaches it as this report and nothing else.
    let (mut client, tx) = build_test_client_with_event_sender();
    assert_eq!(client.get_lock_mode(), LockMode::Normal);

    tx.send(Delivery::Event(Event::InputModeChanged(InputModeChanged {
        client_id: client.get_client_id(),
        lock_mode: LockMode::Locked,
    })))
    .expect("the viewer's queue has room");
    assert_eq!(client.apply_events(), 1);
    assert_eq!(client.get_lock_mode(), LockMode::Locked);

    tx.send(Delivery::Event(Event::InputModeChanged(InputModeChanged {
        client_id: client.get_client_id(),
        lock_mode: LockMode::Normal,
    })))
    .expect("the viewer's queue has room");
    assert_eq!(client.apply_events(), 1);
    assert_eq!(client.get_lock_mode(), LockMode::Normal);
}

#[test]
fn a_lock_report_for_another_viewer_is_ignored() {
    // The input mode is client-scoped, and a subscription carries every
    // client's events. Locking one viewer must not lock the terminal beside it.
    let (mut client, tx) = build_test_client_with_event_sender();

    tx.send(Delivery::Event(Event::InputModeChanged(InputModeChanged {
        client_id: ClientId::new(),
        lock_mode: LockMode::Locked,
    })))
    .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 1, "the event was seen");
    assert_eq!(
        client.get_lock_mode(),
        LockMode::Normal,
        "and it was not applied here"
    );
}

#[test]
fn setting_mouse_select_moves_the_viewers_copy_both_ways() {
    // An attached viewer reads the mode off the frame its own connection
    // carries, so the setter is the only thing that moves its copy.
    let (mut client, _tx) = build_test_client_with_event_sender();

    client.set_mouse_selection_enabled(true);
    assert!(
        client.is_mouse_selection_enabled(),
        "the setter turned it on"
    );

    client.set_mouse_selection_enabled(false);
    assert!(
        !client.is_mouse_selection_enabled(),
        "and the next call turned it off"
    );
}

#[test]
fn a_resync_frame_replaces_the_viewers_stale_lock_and_mouse_select() {
    // The reports that moved these two are exactly what a lagging subscriber
    // misses, so the frame's copies are the only ones left that are current.
    let (mut client, tx) = build_test_client_with_event_sender();
    client.set_lock_mode(LockMode::Locked);
    tx.send(Delivery::Event(Event::MouseSelectChanged(
        MouseSelectChanged {
            client_id: client.get_client_id(),
            is_enabled: true,
        },
    )))
    .expect("the viewer's queue has room");
    assert_eq!(client.apply_events(), 1);
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert!(client.is_mouse_selection_enabled());

    tx.send(resync(client.get_client_id(), LockMode::Normal, false, 7))
        .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 1, "the frame was seen");
    assert_eq!(client.get_lock_mode(), LockMode::Normal);
    assert!(!client.is_mouse_selection_enabled());
}

#[test]
fn a_painted_frame_is_counted_and_moves_nothing_in_the_viewer() {
    // A frame composed for a client in another process rides the same queue.
    // This viewer paints from its own build, so it takes nothing from it.
    let (mut client, tx) = build_test_client_with_event_sender();
    client.set_lock_mode(LockMode::Locked);
    let client_id = client.get_client_id();
    let viewport = client.get_viewport_size();

    tx.send(Delivery::Frame(build_render_snapshot(
        ClientId::new(),
        LockMode::Normal,
        true,
    )))
    .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 1, "the frame was seen");
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert!(!client.is_mouse_selection_enabled());
    assert_eq!(client.get_viewport_size(), viewport);
    assert_eq!(client.get_client_id(), client_id);
}

#[test]
fn a_mouse_answer_is_counted_and_moves_nothing_in_the_viewer() {
    // A round's answers belong to the attached viewer that asked for the round
    // and reach it over its own connection.
    let (mut client, tx) = build_test_client_with_event_sender();
    client.set_lock_mode(LockMode::Locked);
    let client_id = client.get_client_id();

    tx.send(Delivery::MouseAnswer {
        request_id: 6,
        mouse_answers: vec![MouseAnswer::Resized {
            pane_id: PaneId::new(),
            border_side: Direction::Up,
            resize_step: -1,
            applied_cell_count: 2,
        }],
    })
    .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 1, "the answer was seen");
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert!(!client.is_mouse_selection_enabled());
    assert_eq!(client.get_client_id(), client_id);
}

#[test]
fn a_session_switch_is_counted_and_moves_nothing_in_the_viewer() {
    // The switch belongs to the attached viewer that moves, and reaches it over
    // its own connection.
    let (mut client, tx) = build_test_client_with_event_sender();
    client.set_lock_mode(LockMode::Locked);
    let client_id = client.get_client_id();
    let viewport = client.get_viewport_size();

    tx.send(Delivery::SwitchTo(SessionId::new()))
        .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 1, "the switch was seen");
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert!(!client.is_mouse_selection_enabled());
    assert_eq!(client.get_viewport_size(), viewport);
    assert_eq!(client.get_client_id(), client_id);
}

#[test]
fn an_empty_queue_leaves_the_viewer_exactly_as_it_was() {
    // The pump calls this every pass, so the common case is nothing waiting.
    let (mut client, _tx) = build_test_client_with_event_sender();
    client.set_lock_mode(LockMode::Locked);

    assert_eq!(client.apply_events(), 0);

    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert!(!client.is_mouse_selection_enabled());
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "a frame names the client its subscriber views")]
fn a_frame_naming_another_viewer_trips_the_debug_assertion() {
    // The session builds each frame for the client its subscriber views, so a
    // frame naming anyone else means the subscription was recorded wrong.
    let (mut client, tx) = build_test_client_with_event_sender();
    tx.send(resync(ClientId::new(), LockMode::Locked, true, 1))
        .expect("the viewer's queue has room");

    let _ = client.apply_events();
}

#[test]
fn the_later_of_two_queued_resync_frames_wins() {
    // A resync blocked by a full queue is retried with a newer frame, so two
    // frames can sit in one drain; the last one is the current state.
    let (mut client, tx) = build_test_client_with_event_sender();
    tx.send(resync(client.get_client_id(), LockMode::Locked, true, 2))
        .expect("the viewer's queue has room");
    tx.send(resync(client.get_client_id(), LockMode::Normal, false, 5))
        .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 2, "both frames were seen");
    assert_eq!(client.get_lock_mode(), LockMode::Normal);
    assert!(!client.is_mouse_selection_enabled());
}

#[test]
fn an_event_queued_after_a_resync_frame_applies_on_top_of_it() {
    // Both ride one queue in order, so the frame is the state the events that
    // follow it move from. The frame turns mouse-select on and locks the
    // viewer; the event behind it unlocks, and only the lock moves.
    let (mut client, tx) = build_test_client_with_event_sender();
    tx.send(resync(client.get_client_id(), LockMode::Locked, true, 3))
        .expect("the viewer's queue has room");
    tx.send(Delivery::Event(Event::InputModeChanged(InputModeChanged {
        client_id: client.get_client_id(),
        lock_mode: LockMode::Normal,
    })))
    .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 2, "the frame and the event");
    assert_eq!(client.get_lock_mode(), LockMode::Normal, "the event won");
    assert!(
        client.is_mouse_selection_enabled(),
        "and the frame's own value stands"
    );
}

#[test]
fn a_resync_frame_throws_away_a_tab_strip_peek_made_on_another_tab() {
    // The viewer learns a tab switch from the frames it sees, and a resync
    // frame is one of them.
    let (mut client, tx) = build_test_client_with_event_sender();
    let peeked_tab_id = TabId::new();
    client.tabline_peek = Some(TablinePeek {
        active_tab_id: peeked_tab_id,
        first_visible_tab_index: 3,
    });
    tx.send(resync_from(
        build_render_snapshot(client.get_client_id(), LockMode::Normal, false),
        4,
    ))
    .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 1);

    assert_eq!(client.tabline_peek, None);
    assert_eq!(
        client.build_viewer_chrome(peeked_tab_id).tabline_offset,
        None,
        "switching back to the peeked tab starts the strip at its own first tab"
    );
}

#[test]
fn a_resync_frame_keeps_a_tab_strip_peek_made_on_the_tab_it_names() {
    let (mut client, tx) = build_test_client_with_event_sender();
    let snapshot = build_render_snapshot(client.get_client_id(), LockMode::Normal, false);
    let active_tab_id = snapshot.client_snapshot.active_tab_id;
    client.tabline_peek = Some(TablinePeek {
        active_tab_id,
        first_visible_tab_index: 3,
    });
    tx.send(resync_from(snapshot, 4))
        .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 1);

    assert_eq!(
        client.tabline_peek,
        Some(TablinePeek {
            active_tab_id,
            first_visible_tab_index: 3,
        })
    );
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(3)
    );
}

#[test]
fn dialing_again_shows_on_the_chrome_the_viewer_paints_and_comes_back_off() {
    let (mut client, _tx) = build_test_client_with_event_sender();
    let active_tab_id = TabId::new();
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).reconnecting,
        None,
        "a joined viewer is linked"
    );

    let dialing = Reconnecting {
        attempt: 1,
        retry_in_seconds: 5,
    };
    client.set_reconnecting(Some(dialing));
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).reconnecting,
        Some(dialing)
    );

    client.set_reconnecting(None);
    assert_eq!(client.build_viewer_chrome(active_tab_id).reconnecting, None);
}

#[test]
fn taking_a_new_client_id_moves_the_id_the_viewers_commands_carry() {
    let (mut client, _tx) = build_test_client_with_event_sender();
    let minted = ClientId::new();
    assert_ne!(client.get_client_id(), minted);

    client.set_client_id(minted);

    assert_eq!(client.get_client_id(), minted);
}

#[test]
fn terminal_bytes_are_counted_and_move_nothing_in_the_viewer() {
    // Bytes a pane aimed at a terminal belong to the attached viewer that owns
    // that terminal, and reach it over its own connection.
    let (mut client, tx) = build_test_client_with_event_sender();
    client.set_lock_mode(LockMode::Locked);
    let client_id = client.get_client_id();
    let viewport = client.get_viewport_size();

    tx.send(Delivery::HostWrite(vec![0x1b, b']', b'5', b'2']))
        .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 1, "the write was seen");
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert!(!client.is_mouse_selection_enabled());
    assert_eq!(client.get_viewport_size(), viewport);
    assert_eq!(client.get_client_id(), client_id);
}
