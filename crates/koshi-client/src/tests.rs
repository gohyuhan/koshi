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
use koshi_core::action::{ActionRef, MOUSE_SELECT_HINT, MOUSE_UNSELECT_HINT};
use koshi_core::event::{EventClass, InputModeChanged, MouseSelectChanged, SubscriberLagged};
use koshi_core::geometry::Direction;
use koshi_core::ids::{PaneId, SessionId, SubscriberId};
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags};
use koshi_core::mouse::MouseAnswer;
use koshi_core::resolve::ActionArgs;
use koshi_layout::mode::LayoutMode;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::snapshot::{
    ClientSnapshot, PluginUiSnapshot, RenderSnapshot, SessionSnapshot, TabSnapshot,
};

use super::*;

fn new_client() -> (Client, mpsc::SyncSender<Delivery>) {
    let (tx, rx) = mpsc::sync_channel(8);
    let client = Client::new(
        ClientId::new(),
        Size { cols: 80, rows: 24 },
        rx,
        TerminalCleanupGuard::new(),
    );
    (client, tx)
}

/// A viewer that read a theme file painting the focused border `color`.
fn with_focused_border(color: RgbColor) -> (Client, mpsc::SyncSender<Delivery>) {
    let (mut client, tx) = new_client();
    client.load_startup_config(None, Some(focused_border(color)), None);
    (client, tx)
}

/// A frame naming `client_id` in `lock_mode` with mouse-select `mouse_select`:
/// one empty tab, no panes, no plugin UI.
fn frame(client_id: ClientId, lock_mode: LockMode, mouse_select: bool) -> Box<RenderSnapshot> {
    let tab = TabId::new();
    Box::new(RenderSnapshot {
        session: SessionSnapshot {
            id: SessionId::new(),
            name: String::from("session"),
            active_tab: TabSnapshot {
                id: tab,
                name: String::from("tab"),
                layout_solved: Vec::new(),
                effective_size: Size { cols: 80, rows: 24 },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                all_suppressed: false,
                gap: 0,
            },
            tabs_metadata: Vec::new(),
        },
        panes: Vec::new(),
        client: ClientSnapshot {
            id: client_id,
            viewport: Size { cols: 80, rows: 24 },
            active_tab: tab,
            focused_pane: None,
            lock_mode,
            mouse_select,
        },
        plugin_ui: PluginUiSnapshot::default(),
    })
}

/// The frame the session sends a viewer whose queue overflowed, reporting
/// `client_id` in `lock_mode` with mouse-select `mouse_select`, after
/// `dropped_count` events it will never see.
fn resync(
    client_id: ClientId,
    lock_mode: LockMode,
    mouse_select: bool,
    dropped_count: u64,
) -> Delivery {
    Delivery::Snapshot {
        snapshot: frame(client_id, lock_mode, mouse_select),
        lagged: SubscriberLagged {
            subscriber_id: SubscriberId::new(),
            dropped_count,
            event_class: EventClass::Critical,
        },
    }
}

/// A theme file whose focused-border role is `color`.
fn focused_border(color: RgbColor) -> PartialThemeConfig {
    PartialThemeConfig {
        name: None,
        colors: Some(PartialColorPalette {
            border_focused: Some(color),
            ..PartialColorPalette::default()
        }),
    }
}

/// A `keybinding.kdl` binding `<C-y>` to `core:new-tab` in `normal` mode.
fn binds_ctrl_y() -> PartialKeybindingsConfig {
    let mut keys = BTreeMap::new();
    keys.insert(
        KeySequence::from(KeyChord::new(ModFlags::CTRL, Key::Char('y'))),
        BoundAction {
            action: ActionRef::core("new-tab").expect("valid core action name"),
            args: ActionArgs::None,
        },
    );
    let mut modes = BTreeMap::new();
    modes.insert(
        ModeName::new("normal"),
        ModeBindings {
            keys,
            removed: BTreeSet::new(),
        },
    );
    PartialKeybindingsConfig {
        modes: Some(modes),
        ..PartialKeybindingsConfig::default()
    }
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
fn pane_area_support_is_recorded_for_the_attached_session() {
    let (mut client, _tx) = new_client();
    assert!(client.pane_area_supported);

    client.set_pane_area_supported(false);
    assert!(!client.pane_area_supported);

    client.set_pane_area_supported(true);
    assert!(client.pane_area_supported);
}

#[test]
fn the_core_pane_area_reserves_the_two_chrome_rows() {
    assert_eq!(
        core_pane_area(Size { cols: 80, rows: 24 }),
        PaneArea::Reported(Size { cols: 80, rows: 22 })
    );
    assert_eq!(
        core_pane_area(Size { cols: 80, rows: 1 }),
        PaneArea::Reported(Size { cols: 80, rows: 0 })
    );
}

#[test]
fn a_theme_file_recolors_the_chrome_the_next_frame_paints_with() {
    let (client, _tx) = with_focused_border(RgbColor::new(1, 2, 3));
    assert_eq!(
        client.theme().border_focused,
        ratatui::style::Color::Rgb(1, 2, 3)
    );
    assert_eq!(
        client.config().theme.colors.border_focused,
        RgbColor::new(1, 2, 3),
        "and the stored settings carry it too"
    );
}

#[test]
fn a_theme_files_name_reaches_the_viewers_settings() {
    let (mut client, _tx) = new_client();

    client.load_startup_config(
        None,
        Some(PartialThemeConfig {
            name: Some("midnight".to_owned()),
            colors: Some(PartialColorPalette {
                accent: Some(RgbColor::new(9, 8, 7)),
                ..PartialColorPalette::default()
            }),
        }),
        None,
    );

    assert_eq!(client.config().theme.name, "midnight");
    assert_eq!(client.config().theme.colors.accent, RgbColor::new(9, 8, 7));
}

#[test]
fn a_default_config_client_paints_the_stock_colors() {
    let (client, _tx) = new_client();
    assert_eq!(*client.theme(), Theme::default());
    assert_eq!(*client.config(), ClientConfig::default());
}

#[test]
fn koshi_kdls_viewer_owned_sections_reach_the_viewers_settings() {
    // `koshi.kdl` carries sections both halves read. The viewer must fold its
    // own out of the same file, or a configured split direction and wheel
    // behavior would silently never apply.
    let (mut client, _tx) = new_client();
    assert_eq!(
        client.config().layout.new_pane_direction,
        Direction::Right,
        "the built-in default"
    );

    client.load_startup_config(
        Some(PartialKoshiConfig {
            layout: Some(PartialLayoutDefaults {
                new_pane_direction: Some(Direction::Down),
            }),
            mouse: Some(koshi_config::layer::PartialMouseConfig {
                border_resize: None,
                scroll_lines: Some(7),
                wheel: Some(WheelScroll::Ignore),
            }),
            ..PartialKoshiConfig::default()
        }),
        None,
        None,
    );

    assert_eq!(client.config().layout.new_pane_direction, Direction::Down);
    assert_eq!(client.config().mouse.scroll_lines, 7);
    assert_eq!(client.config().mouse.wheel, WheelScroll::Ignore);
}

#[test]
fn the_last_load_wins_when_settings_are_read_twice() {
    // The colors a frame paints with must track the newest settings, not the
    // first ones seen.
    let (mut client, _tx) = new_client();

    client.load_startup_config(None, Some(focused_border(RgbColor::new(1, 1, 1))), None);
    client.load_startup_config(None, Some(focused_border(RgbColor::new(2, 2, 2))), None);

    assert_eq!(
        client.theme().border_focused,
        ratatui::style::Color::Rgb(2, 2, 2)
    );
}

#[test]
fn loading_no_files_at_all_leaves_the_built_in_settings() {
    // A run with no config files resolves to the same settings a fresh viewer
    // holds — the palette is recomputed, not accumulated.
    let (mut client, _tx) = new_client();
    let before = *client.theme();

    let report = client.load_startup_config(None, None, None);

    assert!(report.is_none(), "no keybinding file means no report");
    assert_eq!(*client.theme(), before);
    assert_eq!(*client.config(), ClientConfig::default());
}

#[test]
fn extreme_palette_values_survive_the_round_trip() {
    // The palette's endpoints are plain bytes; black and white must map
    // through unchanged rather than being clamped or shifted.
    let (mut client, _tx) = new_client();

    client.load_startup_config(
        None,
        Some(PartialThemeConfig {
            name: None,
            colors: Some(PartialColorPalette {
                border_focused: Some(RgbColor::new(0, 0, 0)),
                border_unfocused: Some(RgbColor::new(0xff, 0xff, 0xff)),
                ramp_start: Some(RgbColor::new(0, 0, 0)),
                ramp_end: Some(RgbColor::new(0xff, 0xff, 0xff)),
                ..PartialColorPalette::default()
            }),
        }),
        None,
    );

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

#[test]
fn an_applied_keybinding_file_swaps_the_keymap_and_drops_an_open_sequence() {
    let (mut client, _tx) = new_client();
    let ctrl_y = KeySequence::from(KeyChord::new(ModFlags::CTRL, Key::Char('y')));
    assert_eq!(
        client
            .keymap
            .match_sequence(LockMode::Normal, &ctrl_y)
            .exact,
        None,
        "nothing is bound to `<C-y>` out of the box"
    );
    // `<C-p>` opens the shipped pane group, so the viewer is mid-sequence.
    client.resolve_key(
        KeyChord::new(ModFlags::CTRL, Key::Char('p')),
        std::time::Instant::now(),
    );
    assert!(client.pending_sequence().is_some());

    let report = client.load_startup_config(None, None, Some(binds_ctrl_y()));

    assert_eq!(
        report.expect("a keybinding file was given").verdict(),
        KeymapVerdict::Apply
    );
    assert_eq!(
        client
            .keymap
            .match_sequence(LockMode::Normal, &ctrl_y)
            .exact,
        Some(BoundAction {
            action: ActionRef::core("new-tab").expect("valid core action name"),
            args: ActionArgs::None,
        })
    );
    assert!(
        client.pending_sequence().is_none(),
        "held chords reached for bindings the new keymap may not have"
    );
}

#[test]
fn a_keybinding_file_can_move_the_leader_the_defaults_hang_off() {
    let (mut client, _tx) = new_client();
    let ctrl_p = KeySequence::from(KeyChord::new(ModFlags::CTRL, Key::Char('p')));
    let alt_p = KeySequence::from(KeyChord::new(ModFlags::ALT, Key::Char('p')));
    assert!(
        client
            .keymap
            .match_sequence(LockMode::Normal, &ctrl_p)
            .prefix
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
            .keymap
            .match_sequence(LockMode::Normal, &alt_p)
            .prefix
    );
    assert!(
        !client
            .keymap
            .match_sequence(LockMode::Normal, &ctrl_p)
            .prefix
    );
}

#[test]
fn a_refused_keybinding_file_leaves_both_the_keymap_and_the_settings_on_the_built_ins() {
    // `max_chord_depth` 0 would stop every binding from resolving, the
    // locked-mode unlock included, so the whole file is refused. The folded
    // settings must keep describing the keymap actually in use.
    let (mut client, _tx) = new_client();
    let ctrl_p = KeySequence::from(KeyChord::new(ModFlags::CTRL, Key::Char('p')));

    let report = client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            max_chord_depth: Some(0),
            ..PartialKeybindingsConfig::default()
        }),
    );

    assert_eq!(
        report.expect("a keybinding file was given").verdict(),
        KeymapVerdict::Reject
    );
    assert_eq!(
        client.config().keybindings,
        KeybindingsConfig::default(),
        "the refused file's settings must not describe the running keymap"
    );
    assert!(
        client
            .keymap
            .match_sequence(LockMode::Normal, &ctrl_p)
            .prefix,
        "the shipped two-chord defaults still open under `<C-p>`"
    );
}

#[test]
fn a_refused_keybinding_file_leaves_an_open_sequence_alone() {
    // Only a keymap that actually swapped retires the bindings the held chords
    // reach for. A refusal changes no binding, so the sequence being typed
    // still means what it meant and stays open.
    let (mut client, _tx) = new_client();
    client.resolve_key(
        KeyChord::new(ModFlags::CTRL, Key::Char('p')),
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
            .pending_sequence()
            .map(|sequence| sequence.chords().to_vec()),
        Some(vec![KeyChord::new(ModFlags::CTRL, Key::Char('p'))])
    );
}

#[test]
fn a_good_keybinding_file_still_applies_after_a_refused_one() {
    // A refusal must leave the viewer usable, not wedged: the next file it
    // reads applies normally.
    let (mut client, _tx) = new_client();
    let ctrl_y = KeySequence::from(KeyChord::new(ModFlags::CTRL, Key::Char('y')));

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
        report.expect("a keybinding file was given").verdict(),
        KeymapVerdict::Apply
    );
    assert_eq!(
        client
            .keymap
            .match_sequence(LockMode::Normal, &ctrl_y)
            .exact,
        Some(BoundAction {
            action: ActionRef::core("new-tab").expect("valid core action name"),
            args: ActionArgs::None,
        })
    );
}

#[test]
fn a_keybinding_file_cannot_smuggle_colors_in_through_koshi_kdl() {
    // `koshi.kdl`'s theme section is dropped, so with no theme file present the
    // viewer paints the built-in palette rather than the app file's colors.
    let (mut client, _tx) = new_client();

    client.load_startup_config(
        Some(PartialKoshiConfig {
            theme: Some(PartialThemeConfig {
                name: Some("smuggled".to_owned()),
                colors: Some(PartialColorPalette {
                    border_focused: Some(RgbColor::new(9, 9, 9)),
                    ..PartialColorPalette::default()
                }),
            }),
            ..PartialKoshiConfig::default()
        }),
        None,
        None,
    );

    assert_eq!(*client.theme(), Theme::default());
    assert_eq!(client.config().theme, ClientConfig::default().theme);
}

#[test]
fn a_refused_keybinding_file_keeps_the_reserved_unlock_firing() {
    // Binding the reserved unlock chord in locked mode is fatal: the file is
    // refused whole and the guaranteed escape stays live.
    let (mut client, _tx) = new_client();
    let unlock_key = KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK);
    let mut keys = BTreeMap::new();
    keys.insert(
        unlock_key.clone(),
        BoundAction {
            action: ActionRef::core("new-tab").expect("valid core action name"),
            args: ActionArgs::None,
        },
    );
    let mut modes = BTreeMap::new();
    modes.insert(
        ModeName::new("locked"),
        ModeBindings {
            keys,
            removed: BTreeSet::new(),
        },
    );

    let report = client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            modes: Some(modes),
            ..PartialKeybindingsConfig::default()
        }),
    );

    assert_eq!(
        report.expect("a keybinding file was given").verdict(),
        KeymapVerdict::Reject
    );
    assert_eq!(
        client
            .keymap
            .match_sequence(LockMode::Locked, &unlock_key)
            .exact,
        Some(BoundAction {
            action: ActionRef::core("unlock").expect("valid core action name"),
            args: ActionArgs::None,
        })
    );
}

#[test]
fn a_refused_keybinding_file_still_applies_the_theme_beside_it() {
    // The three files are read in one call; one being refused must not take
    // the others down with it.
    let (mut client, _tx) = new_client();

    client.load_startup_config(
        None,
        Some(focused_border(RgbColor::new(4, 5, 6))),
        Some(PartialKeybindingsConfig {
            max_chord_depth: Some(0),
            ..PartialKeybindingsConfig::default()
        }),
    );

    assert_eq!(
        client.theme().border_focused,
        ratatui::style::Color::Rgb(4, 5, 6)
    );
}

#[test]
fn a_second_keybinding_file_fully_replaces_the_firsts_bindings() {
    let (mut client, _tx) = new_client();
    let ctrl_y = KeySequence::from(KeyChord::new(ModFlags::CTRL, Key::Char('y')));

    client.load_startup_config(None, None, Some(binds_ctrl_y()));
    assert!(client
        .keymap
        .match_sequence(LockMode::Normal, &ctrl_y)
        .exact
        .is_some());

    client.load_startup_config(None, None, Some(PartialKeybindingsConfig::default()));

    assert_eq!(
        client
            .keymap
            .match_sequence(LockMode::Normal, &ctrl_y)
            .exact,
        None,
        "the first file's binding must not survive the second"
    );
}

#[test]
fn a_second_startup_load_with_no_keybinding_file_resets_the_keymap_to_the_built_ins() {
    // An absent `keybinding.kdl` means its defaults stand, so a binding an
    // earlier load installed stops resolving.
    let (mut client, _tx) = new_client();
    let ctrl_y = KeySequence::from(KeyChord::new(ModFlags::CTRL, Key::Char('y')));

    client.load_startup_config(None, None, Some(binds_ctrl_y()));
    assert_eq!(
        client
            .keymap
            .match_sequence(LockMode::Normal, &ctrl_y)
            .exact,
        Some(BoundAction {
            action: ActionRef::core("new-tab").expect("valid core action name"),
            args: ActionArgs::None,
        })
    );

    let report = client.load_startup_config(None, None, None);

    assert!(report.is_none(), "no keybinding file means no report");
    assert_eq!(
        client
            .keymap
            .match_sequence(LockMode::Normal, &ctrl_y)
            .exact,
        None,
        "the earlier file's binding must not outlive it"
    );
    assert_eq!(*client.config(), ClientConfig::default());
}

#[test]
fn a_low_chord_depth_drops_every_binding_longer_than_it() {
    let (mut client, _tx) = new_client();
    let long = KeySequence::new(
        KeyChord::new(ModFlags::CTRL, Key::Char('y')),
        vec![KeyChord::new(ModFlags::NONE, Key::Char('x'))],
    );
    let mut keys = BTreeMap::new();
    keys.insert(
        long.clone(),
        BoundAction {
            action: ActionRef::core("new-tab").expect("valid core action name"),
            args: ActionArgs::None,
        },
    );
    let mut modes = BTreeMap::new();
    modes.insert(
        ModeName::new("normal"),
        ModeBindings {
            keys,
            removed: BTreeSet::new(),
        },
    );

    let report = client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            max_chord_depth: Some(1),
            modes: Some(modes),
            ..PartialKeybindingsConfig::default()
        }),
    );

    // Depth 1 applies — with a warning naming the unreachable binding.
    assert_eq!(
        report.expect("a keybinding file was given").verdict(),
        KeymapVerdict::Apply
    );
    // The overlong binding is transparent: no exact match, and its first chord
    // is not a live prefix, so it falls through to the pane.
    assert_eq!(
        client.keymap.match_sequence(LockMode::Normal, &long),
        KeyMatch::default()
    );
    // The shipped two-chord defaults fall the same way.
    assert_eq!(
        client.keymap.match_sequence(
            LockMode::Normal,
            &KeySequence::from(KeyChord::new(ModFlags::CTRL, Key::Char('p')))
        ),
        KeyMatch::default()
    );
    // The one-chord unlock is untouched.
    assert_eq!(
        client
            .keymap
            .match_sequence(
                LockMode::Locked,
                &KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)
            )
            .exact,
        Some(BoundAction {
            action: ActionRef::core("unlock").expect("valid core action name"),
            args: ActionArgs::None,
        })
    );
}

#[test]
fn a_keybinding_file_removes_a_default_binding_only_in_the_mode_that_declares_it() {
    let (mut client, _tx) = new_client();
    let quit = KeySequence::from(KeyChord::new(ModFlags::CTRL, Key::Char('q')));
    assert!(client
        .keymap
        .match_sequence(LockMode::Normal, &quit)
        .exact
        .is_some());
    assert!(client
        .keymap
        .match_sequence(LockMode::Locked, &quit)
        .exact
        .is_some());

    let mut removed = BTreeSet::new();
    removed.insert(quit.clone());
    let mut modes = BTreeMap::new();
    modes.insert(
        ModeName::new("normal"),
        ModeBindings {
            keys: BTreeMap::new(),
            removed,
        },
    );
    let report = client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            modes: Some(modes),
            ..PartialKeybindingsConfig::default()
        }),
    );

    assert_eq!(
        report.expect("a keybinding file was given").verdict(),
        KeymapVerdict::Apply
    );
    assert_eq!(
        client.keymap.match_sequence(LockMode::Normal, &quit),
        KeyMatch::default()
    );
    assert!(client.keymap_hints().removed.contains(&quit));
    // Locked mode's own quit binding is untouched: removal is scoped to the
    // mode that declares it.
    assert!(client
        .keymap
        .match_sequence(LockMode::Locked, &quit)
        .exact
        .is_some());
}

#[test]
fn frame_hints_flip_the_mouse_select_label_only_while_it_is_on() {
    let (client, _tx) = new_client();

    let off = client.frame_hints(false);
    let on = client.frame_hints(true);

    assert!(off
        .entries
        .iter()
        .any(|entry| entry.label == MOUSE_SELECT_HINT));
    assert!(!off
        .entries
        .iter()
        .any(|entry| entry.label == MOUSE_UNSELECT_HINT));
    assert!(on
        .entries
        .iter()
        .any(|entry| entry.label == MOUSE_UNSELECT_HINT));
    assert!(!on
        .entries
        .iter()
        .any(|entry| entry.label == MOUSE_SELECT_HINT));
    // Only that one entry changes: everything else is the same list.
    assert_eq!(off.entries.len(), on.entries.len());
    assert_eq!(off.removed, on.removed);
    assert_eq!(off.reverted, on.reverted);
}

#[test]
fn frame_hints_follow_the_viewers_own_mode() {
    let (mut client, _tx) = new_client();
    let normal = client.frame_hints(false);
    client.set_lock_mode(LockMode::Locked);
    let locked = client.frame_hints(false);

    assert_eq!(normal.entries.len(), 22, "the shipped normal-mode bindings");
    // The reserved unlock (pinned) plus the quit and mouse-select chords.
    assert_eq!(locked.entries.len(), 3);
    assert!(locked
        .entries
        .iter()
        .any(|entry| entry.label == "Unlock" && entry.pinned));
    assert!(locked
        .entries
        .iter()
        .any(|entry| entry.label == "Quit" && !entry.pinned));
}

#[test]
fn a_mouse_select_report_for_this_viewer_flips_its_own_copy() {
    // The viewer routes a mouse press against its own copy of the mode, so the
    // session's report is what has to move it — both ways.
    let (mut client, tx) = new_client();
    assert!(!client.mouse_select(), "a fresh viewer selects nothing");

    tx.send(Delivery::Event(Event::MouseSelectChanged(
        MouseSelectChanged {
            client_id: client.id(),
            on: true,
        },
    )))
    .expect("the viewer's queue has room");
    assert_eq!(client.apply_events(), 1);
    assert!(client.mouse_select(), "the report turned it on");

    tx.send(Delivery::Event(Event::MouseSelectChanged(
        MouseSelectChanged {
            client_id: client.id(),
            on: false,
        },
    )))
    .expect("the viewer's queue has room");
    assert_eq!(client.apply_events(), 1);
    assert!(!client.mouse_select(), "the second report turned it off");
}

#[test]
fn a_mouse_select_report_for_another_viewer_is_ignored() {
    // Mouse select is client-scoped: two viewers of one session hold their own,
    // and a subscription carries every client's events.
    let (mut client, tx) = new_client();

    tx.send(Delivery::Event(Event::MouseSelectChanged(
        MouseSelectChanged {
            client_id: ClientId::new(),
            on: true,
        },
    )))
    .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 1, "the event was seen");
    assert!(!client.mouse_select(), "and it was not applied here");
}

#[test]
fn a_lock_report_for_this_viewer_moves_its_own_mode_both_ways() {
    // The viewer decides what a key means against its own copy of the mode, so
    // `koshi lock --client` reaches it as this report and nothing else.
    let (mut client, tx) = new_client();
    assert_eq!(client.lock_mode(), LockMode::Normal);

    tx.send(Delivery::Event(Event::InputModeChanged(InputModeChanged {
        client_id: client.id(),
        mode: InputMode::Locked,
    })))
    .expect("the viewer's queue has room");
    assert_eq!(client.apply_events(), 1);
    assert_eq!(client.lock_mode(), LockMode::Locked);

    tx.send(Delivery::Event(Event::InputModeChanged(InputModeChanged {
        client_id: client.id(),
        mode: InputMode::Normal,
    })))
    .expect("the viewer's queue has room");
    assert_eq!(client.apply_events(), 1);
    assert_eq!(client.lock_mode(), LockMode::Normal);
}

#[test]
fn a_lock_report_for_another_viewer_is_ignored() {
    // The input mode is client-scoped, and a subscription carries every
    // client's events. Locking one viewer must not lock the terminal beside it.
    let (mut client, tx) = new_client();

    tx.send(Delivery::Event(Event::InputModeChanged(InputModeChanged {
        client_id: ClientId::new(),
        mode: InputMode::Locked,
    })))
    .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 1, "the event was seen");
    assert_eq!(
        client.lock_mode(),
        LockMode::Normal,
        "and it was not applied here"
    );
}

#[test]
fn setting_mouse_select_moves_the_viewers_copy_both_ways() {
    // An attached viewer reads the mode off the frame its own connection
    // carries, so the setter is the only thing that moves its copy.
    let (mut client, _tx) = new_client();

    client.set_mouse_select(true);
    assert!(client.mouse_select(), "the setter turned it on");

    client.set_mouse_select(false);
    assert!(!client.mouse_select(), "and the next call turned it off");
}

#[test]
fn a_resync_frame_replaces_the_viewers_stale_lock_and_mouse_select() {
    // The reports that moved these two are exactly what a lagging subscriber
    // misses, so the frame's copies are the only ones left that are current.
    let (mut client, tx) = new_client();
    client.set_lock_mode(LockMode::Locked);
    tx.send(Delivery::Event(Event::MouseSelectChanged(
        MouseSelectChanged {
            client_id: client.id(),
            on: true,
        },
    )))
    .expect("the viewer's queue has room");
    assert_eq!(client.apply_events(), 1);
    assert_eq!(client.lock_mode(), LockMode::Locked);
    assert!(client.mouse_select());

    tx.send(resync(client.id(), LockMode::Normal, false, 7))
        .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 1, "the frame was seen");
    assert_eq!(client.lock_mode(), LockMode::Normal);
    assert!(!client.mouse_select());
}

#[test]
fn a_painted_frame_is_counted_and_moves_nothing_in_the_viewer() {
    // A frame composed for a client in another process rides the same queue.
    // This viewer paints from its own build, so it takes nothing from it.
    let (mut client, tx) = new_client();
    client.set_lock_mode(LockMode::Locked);
    let id = client.id();
    let viewport = client.viewport();

    tx.send(Delivery::Frame(frame(
        ClientId::new(),
        LockMode::Normal,
        true,
    )))
    .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 1, "the frame was seen");
    assert_eq!(client.lock_mode(), LockMode::Locked);
    assert!(!client.mouse_select());
    assert_eq!(client.viewport(), viewport);
    assert_eq!(client.id(), id);
}

#[test]
fn a_mouse_answer_is_counted_and_moves_nothing_in_the_viewer() {
    // A round's answers belong to the attached viewer that asked for the round
    // and reach it over its own connection.
    let (mut client, tx) = new_client();
    client.set_lock_mode(LockMode::Locked);
    let id = client.id();

    tx.send(Delivery::MouseAnswer {
        request_id: 6,
        answers: vec![MouseAnswer::Resized {
            pane: PaneId::new(),
            side: Direction::Up,
            step: -1,
            applied: 2,
        }],
    })
    .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 1, "the answer was seen");
    assert_eq!(client.lock_mode(), LockMode::Locked);
    assert!(!client.mouse_select());
    assert_eq!(client.id(), id);
}

#[test]
fn a_session_switch_is_counted_and_moves_nothing_in_the_viewer() {
    // The switch belongs to the attached viewer that moves, and reaches it over
    // its own connection.
    let (mut client, tx) = new_client();
    client.set_lock_mode(LockMode::Locked);
    let id = client.id();
    let viewport = client.viewport();

    tx.send(Delivery::SwitchTo(SessionId::new()))
        .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 1, "the switch was seen");
    assert_eq!(client.lock_mode(), LockMode::Locked);
    assert!(!client.mouse_select());
    assert_eq!(client.viewport(), viewport);
    assert_eq!(client.id(), id);
}

#[test]
fn an_empty_queue_leaves_the_viewer_exactly_as_it_was() {
    // The pump calls this every pass, so the common case is nothing waiting.
    let (mut client, _tx) = new_client();
    client.set_lock_mode(LockMode::Locked);

    assert_eq!(client.apply_events(), 0);

    assert_eq!(client.lock_mode(), LockMode::Locked);
    assert!(!client.mouse_select());
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "a frame names the client its subscriber views")]
fn a_frame_naming_another_viewer_trips_the_debug_assertion() {
    // The session builds each frame for the client its subscriber views, so a
    // frame naming anyone else means the subscription was recorded wrong.
    let (mut client, tx) = new_client();
    tx.send(resync(ClientId::new(), LockMode::Locked, true, 1))
        .expect("the viewer's queue has room");

    let _ = client.apply_events();
}

#[test]
fn the_later_of_two_queued_resync_frames_wins() {
    // A resync blocked by a full queue is retried with a newer frame, so two
    // frames can sit in one drain; the last one is the current state.
    let (mut client, tx) = new_client();
    tx.send(resync(client.id(), LockMode::Locked, true, 2))
        .expect("the viewer's queue has room");
    tx.send(resync(client.id(), LockMode::Normal, false, 5))
        .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 2, "both frames were seen");
    assert_eq!(client.lock_mode(), LockMode::Normal);
    assert!(!client.mouse_select());
}

#[test]
fn an_event_queued_after_a_resync_frame_applies_on_top_of_it() {
    // Both ride one queue in order, so the frame is the state the events that
    // follow it move from. The frame turns mouse-select on and locks the
    // viewer; the event behind it unlocks, and only the lock moves.
    let (mut client, tx) = new_client();
    tx.send(resync(client.id(), LockMode::Locked, true, 3))
        .expect("the viewer's queue has room");
    tx.send(Delivery::Event(Event::InputModeChanged(InputModeChanged {
        client_id: client.id(),
        mode: InputMode::Normal,
    })))
    .expect("the viewer's queue has room");

    assert_eq!(client.apply_events(), 2, "the frame and the event");
    assert_eq!(client.lock_mode(), LockMode::Normal, "the event won");
    assert!(client.mouse_select(), "and the frame's own value stands");
}

#[test]
fn dialing_again_shows_on_the_chrome_the_viewer_paints_and_comes_back_off() {
    let (mut client, _tx) = new_client();
    let tab = TabId::new();
    assert_eq!(
        client.chrome(tab).reconnecting,
        None,
        "a joined viewer is linked"
    );

    let dialing = Reconnecting {
        attempt: 1,
        retry_in_seconds: 5,
    };
    client.set_reconnecting(Some(dialing));
    assert_eq!(client.chrome(tab).reconnecting, Some(dialing));

    client.set_reconnecting(None);
    assert_eq!(client.chrome(tab).reconnecting, None);
}

#[test]
fn taking_a_new_client_id_moves_the_id_the_viewers_commands_carry() {
    let (mut client, _tx) = new_client();
    let minted = ClientId::new();
    assert_ne!(client.id(), minted);

    client.set_id(minted);

    assert_eq!(client.id(), minted);
}
