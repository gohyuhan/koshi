//! Tests for viewer-side key resolution: what a chord means in each mode,
//! how an open sequence captures the keyboard, the ambiguity deadline, and the
//! input read times that deadline is judged against. Tests for the
//! viewer-local modes: pane placement mode, the move chooser, floating pane
//! move mode, and dock/undock mode, with the commands each one sends.

use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use koshi_config::conflict::{KeymapLayer, LayerOrigin};
use koshi_config::hints::KeymapHintCatalog;
use koshi_config::types::{KeybindingsConfig, ModeBindings, ModeName};
use koshi_core::command::{PanePlacementAnchor, PanePlacementTarget, TiledPlacementTarget};
use koshi_core::geometry::{Direction, Point, Rect, Size, SplitDirection};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::mouse::{MouseButton, MouseKind, MouseTracking};
use koshi_core::registry::ActionRegistry;
use koshi_ipc::frame::FrameSlot;
use koshi_ipc::placement::PanePlacementPaneSnapshot;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_renderer::snapshot::{
    ClientSnapshot, FloatingPaneMoveDraft, FloatingPaneSlot, MousePane,
};

use crate::mouse::tests::{
    add_floating_mouse_pane, build_floating_mouse_frame, build_floating_pane_move_mode,
    build_floating_pane_move_run_snapshot, build_left_mouse_drag, build_left_mouse_press,
    build_left_mouse_release, build_mouse_frame, build_plain_mouse_pane,
    build_render_snapshot_from_mouse_frame, find_drawn_floating_pane_top_left_cell,
    find_floating_pane_move_draft, FLOATING_PANE_OUTER_RECT,
};
use crate::mouse::MouseAction;
use crate::tests::{
    build_core_bound_action, build_test_client, float_placement_source_pane,
    get_source_tab_snapshot_mut,
};
use crate::{Client, PlacementMode, PlacementModeLifetime};

fn build_key_chord(modifier_flags: BindingModifierFlags, key_character: char) -> KeyChord {
    KeyChord::from_parts(modifier_flags, Key::Char(key_character))
}

/// A resolved keymap holding exactly `key_bindings` in `mode_name`, each
/// sequence paired with the core action of that name. Nothing else is bound.
fn build_keymap_for_mode(
    mode_name: &str,
    key_bindings: &[(KeySequence, &str)],
) -> KeymapHintCatalog {
    build_keymap_for_modes(&[(mode_name, key_bindings)])
}

/// A resolved keymap holding exactly the supplied bindings in each named mode.
fn build_keymap_for_modes(
    mode_binding_sets: &[(&str, &[(KeySequence, &str)])],
) -> KeymapHintCatalog {
    let mut mode_bindings_by_name = BTreeMap::new();
    for (mode_name, key_bindings) in mode_binding_sets {
        let key_binding_entries = key_bindings
            .iter()
            .map(|(key_sequence, action_name)| {
                (key_sequence.clone(), build_core_bound_action(action_name))
            })
            .collect();
        mode_bindings_by_name.insert(
            ModeName::from_text(*mode_name),
            ModeBindings {
                bound_action_by_key_sequence: key_binding_entries,
                removed_key_sequences: BTreeSet::new(),
            },
        );
    }
    KeymapHintCatalog::from_keymap_layers_config_and_registry(
        &[KeymapLayer {
            origin: LayerOrigin::Defaults,
            mode_bindings_by_name,
        }],
        &KeybindingsConfig::default(),
        &ActionRegistry::new(),
    )
}

#[test]
fn an_unbound_key_passes_through_in_normal_mode() {
    let mut client = build_test_client();
    assert_eq!(
        client.resolve_key(
            build_key_chord(BindingModifierFlags::NONE, 'a'),
            Instant::now()
        ),
        KeyOutcome::PassThrough(build_key_chord(BindingModifierFlags::NONE, 'a'))
    );
}

#[test]
fn a_prefix_chord_opens_a_sequence_and_types_nothing() {
    // `<C-p>` is the default pane prefix: it binds nothing on its own, holds
    // the keyboard, and never reaches the pane.
    let mut client = build_test_client();
    assert_eq!(
        client.resolve_key(
            build_key_chord(BindingModifierFlags::CTRL, 'p'),
            Instant::now()
        ),
        KeyOutcome::Pending
    );
    assert_eq!(
        client
            .get_pending_key_sequence()
            .map(|pending_key_sequence| pending_key_sequence.list_chords().to_vec()),
        Some(vec![build_key_chord(BindingModifierFlags::CTRL, 'p')])
    );
}

#[test]
fn completing_a_sequence_fires_its_binding_and_closes_it() {
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.resolve_key(
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );

    let key_outcome = client.resolve_key(
        build_key_chord(BindingModifierFlags::NONE, 'n'),
        current_time,
    );
    let KeyOutcome::Fire(bound_action) = key_outcome else {
        panic!("`<C-p> n` fires new-pane, got {key_outcome:?}");
    };
    assert_eq!(
        bound_action.action_reference,
        ActionReference::from_core_action_name("new-pane").expect("valid name")
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        None,
        "a completed sequence closes"
    );
}

#[test]
fn a_key_that_continues_nothing_is_swallowed_and_the_sequence_stands() {
    // `z` continues nothing after `<C-p>`.
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.resolve_key(
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );

    assert_eq!(
        client.resolve_key(
            build_key_chord(BindingModifierFlags::NONE, 'z'),
            current_time,
        ),
        KeyOutcome::Pending,
        "not PassThrough — the pane must not see it"
    );
    assert_eq!(
        client
            .get_pending_key_sequence()
            .map(|pending_key_sequence| pending_key_sequence.list_chords().to_vec()),
        Some(vec![build_key_chord(BindingModifierFlags::CTRL, 'p')]),
        "the sequence is unchanged"
    );
}

#[test]
fn escape_leaves_an_open_sequence_without_typing_it() {
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.resolve_key(
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );

    assert_eq!(
        client.resolve_key(ESCAPE_KEY_CHORD, current_time),
        KeyOutcome::Pending
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        None,
        "the sequence is gone"
    );
}

#[test]
fn the_unlock_chord_escapes_locked_mode_ahead_of_the_keymap() {
    let mut client = build_test_client();
    client.set_lock_mode(LockMode::Locked);

    let key_outcome = client.resolve_key(KeybindingsConfig::RESERVED_UNLOCK, Instant::now());
    let KeyOutcome::Fire(bound_action) = key_outcome else {
        panic!("the reserved unlock always fires, got {key_outcome:?}");
    };
    assert_eq!(
        bound_action.action_reference,
        ActionReference::from_core_action_name("unlock").expect("valid name")
    );
}

#[test]
fn the_unlock_chord_escapes_even_when_the_keymap_lost_its_unlock_binding() {
    // Strip locked mode's bindings out of the resolved keymap entirely — the
    // shape a keymap layer that shadowed or removed the unlock entry would
    // leave. The escape does not read the keymap, so it still fires.
    let mut client = build_test_client();
    client.set_lock_mode(LockMode::Locked);
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("locked"),
        ModeBindings {
            bound_action_by_key_sequence: BTreeMap::new(),
            removed_key_sequences: BTreeSet::new(),
        },
    );
    client.keymap_catalog = KeymapHintCatalog::from_keymap_layers_config_and_registry(
        &[KeymapLayer {
            origin: LayerOrigin::Defaults,
            mode_bindings_by_name,
        }],
        &KeybindingsConfig::default(),
        &ActionRegistry::new(),
    );

    let key_outcome = client.resolve_key(KeybindingsConfig::RESERVED_UNLOCK, Instant::now());

    let KeyOutcome::Fire(bound_action) = key_outcome else {
        panic!("the reserved unlock always fires, got {key_outcome:?}");
    };
    assert_eq!(
        bound_action.action_reference,
        ActionReference::from_core_action_name("unlock").expect("valid name")
    );
}

#[test]
fn placement_submode_owns_the_locked_keyboard_and_restores_locked_mode() {
    let mut client = build_test_client();
    client.set_lock_mode(LockMode::Locked);
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: PaneId::new(),
        source_tab_id: Some(TabId::new()),
        destination_tab_id: TabId::new(),
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command: None,
    });

    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert_eq!(client.get_active_input_mode(), LockMode::PanePlacement);
    assert_eq!(
        client.resolve_key(client.keymap_catalog.get_unlock_chord(), Instant::now()),
        KeyOutcome::Discard,
        "the placement keymap has priority over the locked keymap"
    );
    let KeyOutcome::Fire(bound_action) = client.resolve_key(ESCAPE_KEY_CHORD, Instant::now())
    else {
        panic!("the default placement cancel binding must fire");
    };
    assert_eq!(
        bound_action.action_reference,
        ActionReference::from_core_action_name("cancel-pane-placement").expect("valid name")
    );
    assert_eq!(
        client.apply_client_action(ClientActionKind::CancelPanePlacement, None),
        PlacementInputAction::Consumed
    );
    assert!(client.placement_state.placement_mode.is_none());
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert_eq!(client.get_active_input_mode(), LockMode::Locked);
}

#[test]
fn submitted_placement_consumes_placement_keys_and_keeps_its_command() {
    let mut client = build_test_client();
    let session_id = SessionId::new();
    let client_id = client.get_client_id();
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let placement_target = TiledPlacementTarget::Split {
        destination_tab_id,
        anchor: PanePlacementAnchor::Pane(PaneId::new()),
        direction: Direction::Right,
    };
    client.set_session_id(session_id);
    client.set_frame_view(
        source_tab_id,
        Some(source_pane_id),
        Some(source_pane_id),
        vec![source_tab_id, destination_tab_id],
    );
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(source_tab_id),
        destination_tab_id,
        placement_direction: Direction::Right,
        placement_target: Some(placement_target.clone()),
        pending_placement_command: None,
    });
    client.placement_state.placement_snapshot =
        Some(Arc::new(crate::tests::build_test_placement_snapshot(
            session_id,
            client_id,
            source_pane_id,
            source_tab_id,
            destination_tab_id,
            0,
            0,
        )));

    assert_eq!(
        client.submit_placement_command(),
        Some(crate::tests::build_expected_submit_placement(
            &client,
            Command::PlacePane(PlacePaneArgs {
                source_pane_id: Some(source_pane_id),
                placement_target: PanePlacementTarget::Tiled(placement_target.clone()),
                expected_placement_revision: Some(PlacementRevision {
                    session_revision: 0,
                    client_revision: 0,
                }),
            })
        ))
    );
    let submitted_placement_mode = client.placement_state.placement_mode.clone();

    for placement_action in [
        ClientActionKind::SelectNextPlacementTab,
        ClientActionKind::SelectPaneTarget(Direction::Right),
        ClientActionKind::CyclePanePlacementSpan,
        ClientActionKind::ConfirmPanePlacement,
    ] {
        assert_eq!(
            client.apply_client_action(placement_action, None),
            PlacementInputAction::Consumed
        );
        assert_eq!(
            client.placement_state.placement_mode,
            submitted_placement_mode
        );
    }
    assert_eq!(client.get_placement_target(), Some(placement_target));
}

#[test]
fn submitted_mouse_placement_returns_to_the_base_mode_and_ignores_cancel() {
    let mut client = build_test_client();
    client.set_lock_mode(LockMode::Locked);
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: PaneId::new(),
        source_tab_id: Some(TabId::new()),
        destination_tab_id: TabId::new(),
        placement_direction: Direction::Right,
        placement_target: Some(TiledPlacementTarget::Swap {
            target_pane_id: PaneId::new(),
        }),
        pending_placement_command: Some(crate::tests::build_pending_placement_command(
            CommandId::new(),
        )),
    });
    client.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilDragEnds;

    assert!(!client.is_placement_mode_active());
    assert!(client.is_placement_confirmation_pending());
    assert_eq!(client.get_active_input_mode(), LockMode::Locked);
    assert_eq!(
        client.apply_client_action(ClientActionKind::CancelPanePlacement, None),
        PlacementInputAction::Consumed
    );
    assert!(client.is_placement_confirmation_pending());
    assert_eq!(client.get_active_input_mode(), LockMode::Locked);
}

#[test]
fn submitted_keyboard_placement_keeps_pane_placement_mode_and_ignores_cancel() {
    let mut client = build_test_client();
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: PaneId::new(),
        source_tab_id: Some(TabId::new()),
        destination_tab_id: TabId::new(),
        placement_direction: Direction::Right,
        placement_target: Some(TiledPlacementTarget::Swap {
            target_pane_id: PaneId::new(),
        }),
        pending_placement_command: Some(crate::tests::build_pending_placement_command(
            CommandId::new(),
        )),
    });
    client.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilCancelled;

    assert!(client.is_placement_mode_active());
    assert_eq!(client.get_active_input_mode(), LockMode::PanePlacement);
    assert_eq!(
        client.apply_client_action(ClientActionKind::CancelPanePlacement, None),
        PlacementInputAction::Consumed
    );
    assert!(client.is_placement_mode_active());
    assert!(client.is_placement_confirmation_pending());
}

#[test]
fn changing_input_owner_cancels_an_unconfirmed_placement() {
    let mut client = build_test_client();
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: PaneId::new(),
        source_tab_id: Some(TabId::new()),
        destination_tab_id: TabId::new(),
        placement_direction: Direction::Right,
        placement_target: Some(TiledPlacementTarget::Swap {
            target_pane_id: PaneId::new(),
        }),
        pending_placement_command: None,
    });

    client.set_lock_mode(LockMode::Locked);

    assert!(!client.is_placement_mode_active());
    assert!(client.get_placement_target().is_none());

    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: PaneId::new(),
        source_tab_id: Some(TabId::new()),
        destination_tab_id: TabId::new(),
        placement_direction: Direction::Right,
        placement_target: Some(TiledPlacementTarget::Swap {
            target_pane_id: PaneId::new(),
        }),
        pending_placement_command: None,
    });
    client.set_mouse_selection_enabled(true);

    assert!(!client.is_placement_mode_active());
    assert!(client.get_placement_target().is_none());
}

#[test]
fn placement_accepts_the_dragged_pane_focus_frame_before_cancelling_other_focus() {
    let initial_focused_pane_id = PaneId::new();
    let dragged_pane_id = PaneId::new();
    let tab_id = TabId::new();
    let mut client = build_test_client();
    client.set_frame_view(
        tab_id,
        Some(initial_focused_pane_id),
        Some(initial_focused_pane_id),
        vec![tab_id],
    );
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: dragged_pane_id,
        source_tab_id: Some(tab_id),
        destination_tab_id: tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command: None,
    });

    client.set_frame_view(
        tab_id,
        Some(dragged_pane_id),
        Some(dragged_pane_id),
        vec![tab_id],
    );
    assert!(client.is_placement_mode_active());

    client.set_frame_view(
        tab_id,
        Some(initial_focused_pane_id),
        Some(initial_focused_pane_id),
        vec![tab_id],
    );
    assert!(!client.is_placement_mode_active());
}

#[test]
fn a_new_frame_owner_cancels_an_unconfirmed_placement() {
    let mut client = build_test_client();
    let source_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    client.set_frame_view(
        source_tab_id,
        Some(source_pane_id),
        Some(source_pane_id),
        vec![source_tab_id],
    );
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(source_tab_id),
        destination_tab_id: source_tab_id,
        placement_direction: Direction::Right,
        placement_target: Some(TiledPlacementTarget::Swap {
            target_pane_id: PaneId::new(),
        }),
        pending_placement_command: None,
    });

    client.set_frame_view(TabId::new(), None, None, Vec::new());

    assert!(!client.is_placement_mode_active());
    assert!(client.get_placement_target().is_none());
}

#[test]
fn locked_mode_still_passes_keys_it_does_not_bind() {
    let mut client = build_test_client();
    client.set_lock_mode(LockMode::Locked);
    assert_eq!(
        client.resolve_key(
            build_key_chord(BindingModifierFlags::NONE, 'a'),
            Instant::now()
        ),
        KeyOutcome::PassThrough(build_key_chord(BindingModifierFlags::NONE, 'a'))
    );
}

#[test]
fn keyboard_pane_placement_opens_on_the_active_tab_when_other_tabs_exist() {
    let mut client = build_test_client();
    let active_tab_id = TabId::new();
    let newer_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    client.set_frame_view(
        active_tab_id,
        Some(source_pane_id),
        Some(source_pane_id),
        vec![active_tab_id, newer_tab_id],
    );

    assert_eq!(
        client.apply_client_action(ClientActionKind::BeginPanePlacement, None),
        PlacementInputAction::ReadPlacement {
            pane_id_to_focus: None,
            source_pane_id,
            destination_tab_id: active_tab_id,
        }
    );
    assert_eq!(
        client.get_placement_destination_tab_id(),
        Some(active_tab_id)
    );
}

/// A keyboard move of `source_pane_id` on `source_tab_id` after Tab asked for a
/// preview of `destination_tab_id`, with that read in flight as request `1`.
/// Returns the client and the preview that answers the read.
fn build_client_reading_next_placement_tab(
    session_id: SessionId,
    source_pane_id: PaneId,
    source_tab_id: TabId,
    destination_tab_id: TabId,
) -> (Client, PanePlacementSnapshot) {
    let mut client = build_test_client();
    client.set_session_id(session_id);
    client.set_frame_view(
        source_tab_id,
        Some(source_pane_id),
        Some(source_pane_id),
        vec![source_tab_id, destination_tab_id],
    );
    client.apply_client_action(ClientActionKind::BeginPanePlacement, None);
    assert_eq!(
        client.apply_client_action(ClientActionKind::SelectNextPlacementTab, None),
        PlacementInputAction::ReadPlacement {
            pane_id_to_focus: None,
            source_pane_id,
            destination_tab_id,
        }
    );
    assert!(client.begin_placement_read(1, source_pane_id, destination_tab_id, Instant::now()));
    let placement_snapshot = crate::tests::build_test_placement_snapshot(
        session_id,
        client.get_client_id(),
        source_pane_id,
        source_tab_id,
        destination_tab_id,
        0,
        0,
    );
    (client, placement_snapshot)
}

#[test]
fn next_placement_tab_selects_insertion_right_of_that_whole_tab_and_enter_places_the_pane() {
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let (mut client, placement_snapshot) = build_client_reading_next_placement_tab(
        SessionId::new(),
        source_pane_id,
        source_tab_id,
        destination_tab_id,
    );
    let whole_tab_insertion_target = TiledPlacementTarget::Split {
        destination_tab_id,
        anchor: PanePlacementAnchor::Tab,
        direction: Direction::Right,
    };

    assert!(client.accept_placement_snapshot(1, placement_snapshot, Instant::now()));

    assert_eq!(
        client.get_placement_target(),
        Some(whole_tab_insertion_target.clone())
    );
    assert_eq!(
        client.apply_client_action(ClientActionKind::ConfirmPanePlacement, None),
        crate::tests::build_expected_submit_placement(
            &client,
            Command::PlacePane(PlacePaneArgs {
                source_pane_id: Some(source_pane_id),
                placement_target: PanePlacementTarget::Tiled(whole_tab_insertion_target),
                expected_placement_revision: Some(PlacementRevision {
                    session_revision: 0,
                    client_revision: 0,
                }),
            })
        )
    );
}

#[test]
fn previous_placement_tab_back_to_the_source_tab_selects_no_target() {
    let session_id = SessionId::new();
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let (mut client, placement_snapshot) = build_client_reading_next_placement_tab(
        session_id,
        source_pane_id,
        source_tab_id,
        destination_tab_id,
    );
    assert!(client.accept_placement_snapshot(1, placement_snapshot, Instant::now()));

    assert_eq!(
        client.apply_client_action(ClientActionKind::SelectPreviousPlacementTab, None),
        PlacementInputAction::ReadPlacement {
            pane_id_to_focus: None,
            source_pane_id,
            destination_tab_id: source_tab_id,
        }
    );
    assert!(client.begin_placement_read(2, source_pane_id, source_tab_id, Instant::now()));
    let mut source_tab_placement_snapshot = crate::tests::build_test_placement_snapshot(
        session_id,
        client.get_client_id(),
        source_pane_id,
        source_tab_id,
        source_tab_id,
        0,
        0,
    );
    source_tab_placement_snapshot.destination_tab_snapshot = None;
    assert!(client.accept_placement_snapshot(2, source_tab_placement_snapshot, Instant::now()));

    assert_eq!(client.get_placement_target(), None);
    assert_eq!(
        client.apply_client_action(ClientActionKind::ConfirmPanePlacement, None),
        PlacementInputAction::Consumed
    );
}

#[test]
fn a_next_tab_preview_accepted_during_a_drag_selects_no_target() {
    let source_pane_id = PaneId::new();
    let (mut client, placement_snapshot) = build_client_reading_next_placement_tab(
        SessionId::new(),
        source_pane_id,
        TabId::new(),
        TabId::new(),
    );
    client.begin_placement_drag(Point { column: 0, row: 0 }, false);

    assert!(client.accept_placement_snapshot(1, placement_snapshot, Instant::now()));

    assert_eq!(client.get_placement_target(), None);
}

#[test]
fn shift_arrow_on_the_whole_tab_target_selects_that_edge_of_the_tab() {
    let destination_tab_id = TabId::new();
    let (mut client, placement_snapshot) = build_client_reading_next_placement_tab(
        SessionId::new(),
        PaneId::new(),
        TabId::new(),
        destination_tab_id,
    );
    assert!(client.accept_placement_snapshot(1, placement_snapshot, Instant::now()));

    assert_eq!(
        client.apply_client_action(ClientActionKind::SelectPaneInsertion(Direction::Left), None),
        PlacementInputAction::Consumed
    );

    assert_eq!(
        client.get_placement_target(),
        Some(TiledPlacementTarget::Split {
            destination_tab_id,
            anchor: PanePlacementAnchor::Tab,
            direction: Direction::Left,
        })
    );
}

#[test]
fn arrow_on_the_whole_tab_target_selects_a_swap_in_the_previewed_tab() {
    let (mut client, placement_snapshot) = build_client_reading_next_placement_tab(
        SessionId::new(),
        PaneId::new(),
        TabId::new(),
        TabId::new(),
    );
    let destination_pane_id = placement_snapshot
        .destination_tab_snapshot
        .as_ref()
        .expect("a preview of another tab carries that tab")
        .pane_slots[0]
        .pane_id;
    assert!(client.accept_placement_snapshot(1, placement_snapshot, Instant::now()));

    assert_eq!(
        client.apply_client_action(ClientActionKind::SelectPaneTarget(Direction::Right), None),
        PlacementInputAction::Consumed
    );

    assert_eq!(
        client.get_placement_target(),
        Some(TiledPlacementTarget::Swap {
            target_pane_id: destination_pane_id,
        })
    );
}

#[test]
fn a_rejected_whole_tab_placement_selects_the_whole_tab_target_again_after_the_refresh() {
    let session_id = SessionId::new();
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let (mut client, placement_snapshot) = build_client_reading_next_placement_tab(
        session_id,
        source_pane_id,
        source_tab_id,
        destination_tab_id,
    );
    let whole_tab_insertion_target = TiledPlacementTarget::Split {
        destination_tab_id,
        anchor: PanePlacementAnchor::Tab,
        direction: Direction::Right,
    };
    assert!(client.accept_placement_snapshot(1, placement_snapshot.clone(), Instant::now()));
    assert_eq!(
        client.apply_client_action(ClientActionKind::ConfirmPanePlacement, None),
        crate::tests::build_expected_submit_placement(
            &client,
            Command::PlacePane(PlacePaneArgs {
                source_pane_id: Some(source_pane_id),
                placement_target: PanePlacementTarget::Tiled(whole_tab_insertion_target.clone()),
                expected_placement_revision: Some(PlacementRevision {
                    session_revision: 0,
                    client_revision: 0,
                }),
            })
        )
    );
    let command_id = client
        .get_pending_placement_command()
        .expect("Enter recorded a pending placement command")
        .command_id;

    assert!(client.reject_placement_command(command_id));
    assert_eq!(client.get_placement_target(), None);
    assert_eq!(
        client.take_placement_preview_refresh(),
        Some((source_pane_id, destination_tab_id))
    );
    assert!(client.begin_placement_read(2, source_pane_id, destination_tab_id, Instant::now()));
    assert!(client.accept_placement_snapshot(2, placement_snapshot, Instant::now()));

    assert_eq!(
        client.get_placement_target(),
        Some(whole_tab_insertion_target)
    );
}

/// A same-tab placement preview of `layout_tree` for moving `source_pane_id`,
/// with every slot and stack header where the tiled solve over the test
/// viewport puts it. A collapsed stack member's slot is its header strip and is
/// not visible.
fn build_solved_placement_snapshot(
    session_id: SessionId,
    client_id: ClientId,
    source_pane_id: PaneId,
    tab_id: TabId,
    layout_tree: LayoutNode,
) -> PanePlacementSnapshot {
    let mut placement_snapshot = crate::tests::build_test_placement_snapshot(
        session_id,
        client_id,
        source_pane_id,
        tab_id,
        tab_id,
        0,
        0,
    );
    placement_snapshot.destination_tab_snapshot = None;
    let layout_solve = solve_layout_with_mode(
        &layout_tree,
        LayoutMode::Tiled,
        Rect::from_size_at_origin(crate::tests::TEST_VIEWPORT_SIZE),
        PaneSizing {
            minimum_size: Size {
                column_count: 1,
                row_count: 1,
            },
            gap_cell_count: 0,
        },
    );
    let collapsed_pane_ids: Vec<PaneId> = layout_solve
        .stack_headers
        .iter()
        .map(|stack_header| stack_header.pane_id)
        .collect();
    let tab_snapshot = get_source_tab_snapshot_mut(&mut placement_snapshot);
    tab_snapshot.pane_slots = layout_solve
        .pane_rects
        .iter()
        .map(|&(pane_id, outer_rect)| {
            let is_visible = !collapsed_pane_ids.contains(&pane_id);
            FrameSlot {
                pane_id,
                outer_rect,
                content_rect: is_visible.then_some(outer_rect),
                is_visible,
                is_suppressed: false,
            }
        })
        .collect();
    tab_snapshot.pane_snapshots = layout_tree
        .list_leaf_pane_ids()
        .into_iter()
        .map(|pane_id| PanePlacementPaneSnapshot {
            pane_id,
            terminal_window: None,
            image_placement_snapshots: Vec::new(),
        })
        .collect();
    tab_snapshot.stack_headers = layout_solve.stack_headers;
    tab_snapshot.layout_tree = layout_tree;
    placement_snapshot
}

/// The panes of `H[Stack[S1, S2, S3 open], P]`: a stack on the left whose
/// third member is open below the headers of the first two, and a plain pane on
/// the right.
struct StackBesidePlainPane {
    first_stack_pane_id: PaneId,
    second_stack_pane_id: PaneId,
    open_stack_pane_id: PaneId,
    plain_pane_id: PaneId,
}

impl StackBesidePlainPane {
    fn new() -> Self {
        Self {
            first_stack_pane_id: PaneId::new(),
            second_stack_pane_id: PaneId::new(),
            open_stack_pane_id: PaneId::new(),
            plain_pane_id: PaneId::new(),
        }
    }

    fn build_layout_tree(&self) -> LayoutNode {
        LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutNode::Split(SplitNode::from_stacked_pane_ids(
                    vec![
                        self.first_stack_pane_id,
                        self.second_stack_pane_id,
                        self.open_stack_pane_id,
                    ],
                    2,
                )),
                LayoutNode::Pane(self.plain_pane_id),
            ],
        ))
    }

    /// A client in keyboard pane placement mode moving `source_pane_id` inside this
    /// layout's tab, with the solved preview retained.
    fn build_client_moving(&self, source_pane_id: PaneId) -> Client {
        let mut client = build_test_client();
        let session_id = SessionId::new();
        let tab_id = TabId::new();
        client.set_session_id(session_id);
        client.set_frame_view(
            tab_id,
            Some(source_pane_id),
            Some(source_pane_id),
            vec![tab_id],
        );
        client.placement_state.placement_mode = Some(PlacementMode {
            source_pane_id,
            source_tab_id: Some(tab_id),
            destination_tab_id: tab_id,
            placement_direction: Direction::Right,
            placement_target: None,
            pending_placement_command: None,
        });
        client.placement_state.placement_snapshot =
            Some(Arc::new(build_solved_placement_snapshot(
                session_id,
                client.get_client_id(),
                source_pane_id,
                tab_id,
                self.build_layout_tree(),
            )));
        client
    }
}

#[test]
fn arrows_from_the_open_stack_member_walk_the_collapsed_members_above_it() {
    let stack_layout = StackBesidePlainPane::new();
    let mut client = stack_layout.build_client_moving(stack_layout.open_stack_pane_id);

    client.apply_client_action(ClientActionKind::SelectPaneTarget(Direction::Up), None);
    assert_eq!(
        client.get_placement_target(),
        Some(TiledPlacementTarget::Swap {
            target_pane_id: stack_layout.second_stack_pane_id,
        })
    );
    client.apply_client_action(ClientActionKind::SelectPaneTarget(Direction::Up), None);
    assert_eq!(
        client.get_placement_target(),
        Some(TiledPlacementTarget::Swap {
            target_pane_id: stack_layout.first_stack_pane_id,
        })
    );
    client.apply_client_action(ClientActionKind::SelectPaneTarget(Direction::Down), None);
    assert_eq!(
        client.get_placement_target(),
        Some(TiledPlacementTarget::Swap {
            target_pane_id: stack_layout.second_stack_pane_id,
        })
    );
}

#[test]
fn arrow_right_from_a_stack_member_selects_the_plain_pane_and_enter_swaps_them() {
    let stack_layout = StackBesidePlainPane::new();
    let mut client = stack_layout.build_client_moving(stack_layout.open_stack_pane_id);

    client.apply_client_action(ClientActionKind::SelectPaneTarget(Direction::Right), None);

    assert_eq!(
        client.apply_client_action(ClientActionKind::ConfirmPanePlacement, None),
        crate::tests::build_expected_submit_placement(
            &client,
            Command::PlacePane(PlacePaneArgs {
                source_pane_id: Some(stack_layout.open_stack_pane_id),
                placement_target: PanePlacementTarget::Tiled(TiledPlacementTarget::Swap {
                    target_pane_id: stack_layout.plain_pane_id,
                }),
                expected_placement_revision: Some(PlacementRevision {
                    session_revision: 0,
                    client_revision: 0,
                }),
            })
        )
    );
}

#[test]
fn a_plain_pane_reaches_a_collapsed_stack_member_and_enter_swaps_them() {
    let stack_layout = StackBesidePlainPane::new();
    let mut client = stack_layout.build_client_moving(stack_layout.plain_pane_id);

    client.apply_client_action(ClientActionKind::SelectPaneTarget(Direction::Left), None);
    assert_eq!(
        client.get_placement_target(),
        Some(TiledPlacementTarget::Swap {
            target_pane_id: stack_layout.open_stack_pane_id,
        })
    );
    client.apply_client_action(ClientActionKind::SelectPaneTarget(Direction::Up), None);

    assert_eq!(
        client.apply_client_action(ClientActionKind::ConfirmPanePlacement, None),
        crate::tests::build_expected_submit_placement(
            &client,
            Command::PlacePane(PlacePaneArgs {
                source_pane_id: Some(stack_layout.plain_pane_id),
                placement_target: PanePlacementTarget::Tiled(TiledPlacementTarget::Swap {
                    target_pane_id: stack_layout.second_stack_pane_id,
                }),
                expected_placement_revision: Some(PlacementRevision {
                    session_revision: 0,
                    client_revision: 0,
                }),
            })
        )
    );
}

#[test]
fn shift_arrow_toward_a_stack_selects_insertion_beside_the_whole_stack() {
    let stack_layout = StackBesidePlainPane::new();
    let mut client = stack_layout.build_client_moving(stack_layout.plain_pane_id);
    let tab_id = client
        .get_placement_destination_tab_id()
        .expect("pane placement mode names a destination tab");

    client.apply_client_action(ClientActionKind::SelectPaneInsertion(Direction::Left), None);

    assert_eq!(
        client.get_placement_target(),
        Some(TiledPlacementTarget::Split {
            destination_tab_id: tab_id,
            anchor: PanePlacementAnchor::Group(vec![
                stack_layout.first_stack_pane_id,
                stack_layout.second_stack_pane_id,
                stack_layout.open_stack_pane_id,
            ]),
            direction: Direction::Left,
        })
    );
}

#[test]
fn locked_mode_opens_pane_placement_without_changing_the_base_mode() {
    let mut client = build_test_client();
    let source_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    client.set_frame_view(
        source_tab_id,
        Some(source_pane_id),
        Some(source_pane_id),
        vec![source_tab_id],
    );
    client.set_lock_mode(LockMode::Locked);

    assert_eq!(
        client.resolve_key(
            KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('p')),
            Instant::now(),
        ),
        KeyOutcome::Pending
    );
    let KeyOutcome::Fire(bound_action) = client.resolve_key(
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('m')),
        Instant::now(),
    ) else {
        panic!("the locked pane placement opener must fire");
    };
    assert_eq!(
        bound_action.action_reference,
        ActionReference::from_core_action_name("begin-pane-placement").expect("valid action name")
    );

    assert_eq!(
        client.apply_client_action(ClientActionKind::BeginPanePlacement, None),
        PlacementInputAction::ReadPlacement {
            pane_id_to_focus: None,
            source_pane_id,
            destination_tab_id: source_tab_id,
        }
    );
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert_eq!(client.get_active_input_mode(), LockMode::PanePlacement);
}

#[test]
fn a_modal_mode_owns_the_keyboard_and_discards_what_it_does_not_bind() {
    let mut client = build_test_client();
    client.set_lock_mode(LockMode::Resize);
    assert_eq!(
        client.resolve_key(
            build_key_chord(BindingModifierFlags::NONE, 'a'),
            Instant::now()
        ),
        KeyOutcome::Discard,
        "a modal layer never leaks a key to the pane"
    );
}

#[test]
fn changing_mode_drops_an_open_sequence() {
    // A mode change drops the held chords.
    let mut client = build_test_client();
    client.resolve_key(
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        Instant::now(),
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        Some(&KeySequence::from(build_key_chord(
            BindingModifierFlags::CTRL,
            'p'
        )))
    );

    client.set_lock_mode(LockMode::Locked);
    assert_eq!(client.get_pending_key_sequence(), None);
}

#[test]
fn a_prefix_only_sequence_never_wakes_the_loop() {
    // Only exact-plus-longer ambiguity arms a deadline; a prefix that binds
    // nothing on its own waits for its next chord indefinitely.
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.resolve_key(
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );

    assert_eq!(client.compute_next_key_wakeup(current_time), None);
    assert_eq!(client.expire_key_sequence(current_time), None);
}

#[test]
fn expiring_without_a_deadline_fires_nothing() {
    let mut client = build_test_client();
    assert_eq!(client.expire_key_sequence(Instant::now()), None);
}

#[test]
fn a_continuous_binding_re_opens_its_prefix_so_the_last_chord_repeats() {
    // `<C-p> <Left>` fires `core:focus-pane-left`, which the action table marks
    // continuous: the prefix `<C-p>` comes straight back so a second `<Left>`
    // alone focuses left again, with no second `<C-p>`.
    let mut client = build_test_client();
    let current_time = Instant::now();
    let left_chord = KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Left));
    let focus_left_action =
        ActionReference::from_core_action_name("focus-pane-left").expect("valid name");

    assert_eq!(
        client.resolve_key(
            build_key_chord(BindingModifierFlags::CTRL, 'p'),
            current_time,
        ),
        KeyOutcome::Pending
    );

    let key_outcome = client.resolve_key(left_chord, current_time);
    let KeyOutcome::Fire(bound_action) = key_outcome else {
        panic!("`<C-p> <Left>` fires focus-pane-left, got {key_outcome:?}");
    };
    assert_eq!(bound_action.action_reference, focus_left_action);
    assert_eq!(
        client
            .get_pending_key_sequence()
            .map(|pending_key_sequence| pending_key_sequence.list_chords().to_vec()),
        Some(vec![build_key_chord(BindingModifierFlags::CTRL, 'p')]),
        "the prefix alone is held again, not the whole sequence"
    );
    assert_eq!(
        client.compute_next_key_wakeup(current_time),
        None,
        "the re-opened prefix waits for its next chord with no deadline"
    );

    let key_outcome = client.resolve_key(left_chord, current_time);
    let KeyOutcome::Fire(bound_action) = key_outcome else {
        panic!("the bare `<Left>` fires focus-pane-left again, got {key_outcome:?}");
    };
    assert_eq!(bound_action.action_reference, focus_left_action);
}

#[test]
fn placement_submode_takes_priority_over_a_normal_arrow_binding() {
    let mut client = build_test_client();
    let leader_chord = build_key_chord(BindingModifierFlags::CTRL, 'p');
    let right_chord = KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Right));
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[(
            KeySequence::from_first_and_rest(leader_chord, vec![right_chord]),
            "quit",
        )],
    );

    assert_eq!(
        client.resolve_key(leader_chord, Instant::now()),
        KeyOutcome::Pending
    );
    let normal_mode_outcome = client.resolve_key(right_chord, Instant::now());
    let KeyOutcome::Fire(bound_action) = normal_mode_outcome else {
        panic!("the custom normal-mode arrow binding must fire, got {normal_mode_outcome:?}");
    };
    assert_eq!(
        bound_action.action_reference,
        ActionReference::from_core_action_name("quit").expect("valid name")
    );

    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: PaneId::new(),
        source_tab_id: Some(TabId::new()),
        destination_tab_id: TabId::new(),
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command: None,
    });

    assert_eq!(
        client.resolve_key(right_chord, Instant::now()),
        KeyOutcome::Discard,
        "the active pane keymap is checked instead of the normal keymap"
    );

    client.keymap_catalog = build_keymap_for_modes(&[
        (
            "normal",
            &[(
                KeySequence::from_first_and_rest(leader_chord, vec![right_chord]),
                "quit",
            )],
        ),
        (
            "pane-placement",
            &[(KeySequence::from(right_chord), "cancel-pane-placement")],
        ),
    ]);
    assert_eq!(
        client.resolve_key(right_chord, Instant::now()),
        KeyOutcome::Fire(build_core_bound_action("cancel-pane-placement"))
    );
}

#[test]
fn a_one_chord_binding_of_a_continuous_action_opens_no_prefix() {
    // Only a multi-chord sequence has a prefix to re-open. `<C-y>` on its own
    // fires `core:focus-pane-left` and leaves the keyboard to the user.
    let mut client = build_test_client();
    let ctrl_y_chord = build_key_chord(BindingModifierFlags::CTRL, 'y');
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[(KeySequence::from(ctrl_y_chord), "focus-pane-left")],
    );

    let key_outcome = client.resolve_key(ctrl_y_chord, Instant::now());

    let KeyOutcome::Fire(bound_action) = key_outcome else {
        panic!("`<C-y>` fires focus-pane-left, got {key_outcome:?}");
    };
    assert_eq!(
        bound_action.action_reference,
        ActionReference::from_core_action_name("focus-pane-left").expect("valid name")
    );
    assert_eq!(client.get_pending_key_sequence(), None);
}

#[test]
fn an_input_event_read_and_not_yet_taken_holds_back_the_timer_judgement() {
    let input_read_clock = InputReadClock::default();
    let first_judged_after = Instant::now();
    let first_judge_time = input_read_clock
        .find_input_judge_time()
        .expect("no input event is on its way");
    assert!(first_judged_after <= first_judge_time && first_judge_time <= Instant::now());

    let read_at = input_read_clock.stamp_input_read(Instant::now());

    assert!(input_read_clock.has_untaken_input());
    assert_eq!(input_read_clock.find_input_judge_time(), None);

    input_read_clock.note_input_taken(read_at);

    assert!(!input_read_clock.has_untaken_input());
    let judged_after = Instant::now();
    let judge_time = input_read_clock
        .find_input_judge_time()
        .expect("the input event reached the loop");
    assert!(judged_after <= judge_time && judge_time <= Instant::now());
}

#[test]
fn each_input_stamp_is_greater_than_the_one_before() {
    let input_read_clock = InputReadClock::default();
    let clock_reading = Instant::now();

    let first_read_at = input_read_clock.stamp_input_read(clock_reading);
    let same_reading_read_at = input_read_clock.stamp_input_read(clock_reading);
    let earlier_reading_read_at = input_read_clock.stamp_input_read(
        clock_reading
            .checked_sub(Duration::from_secs(1))
            .expect("the clock reads past one second"),
    );
    let next_second_reading_read_at =
        input_read_clock.stamp_input_read(clock_reading + Duration::from_secs(1));

    assert_eq!(first_read_at, clock_reading);
    assert_eq!(
        same_reading_read_at,
        clock_reading + Duration::from_nanos(1)
    );
    assert_eq!(
        earlier_reading_read_at,
        clock_reading + Duration::from_nanos(2)
    );
    assert_eq!(
        next_second_reading_read_at,
        clock_reading + Duration::from_secs(1)
    );
}

#[test]
fn an_open_deadline_asks_for_no_wakeup_while_an_input_event_is_on_its_way() {
    let mut client = build_test_client();
    let ctrl_y_chord = build_key_chord(BindingModifierFlags::CTRL, 'y');
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[
            (KeySequence::from(ctrl_y_chord), "quit"),
            (
                KeySequence::from_first_and_rest(
                    ctrl_y_chord,
                    vec![build_key_chord(BindingModifierFlags::NONE, 'a')],
                ),
                "new-tab",
            ),
        ],
    );
    let chord_timeout = client.keymap_catalog.get_chord_timeout();
    let current_time = Instant::now();
    assert_eq!(
        client.resolve_key(ctrl_y_chord, current_time),
        KeyOutcome::Pending
    );

    let read_at = client
        .get_input_read_clock()
        .stamp_input_read(Instant::now());

    assert_eq!(client.compute_next_key_wakeup(current_time), None);

    client.get_input_read_clock().note_input_taken(read_at);

    assert_eq!(
        client.compute_next_key_wakeup(current_time),
        Some(chord_timeout)
    );
}

#[test]
fn a_sequence_that_is_both_a_binding_and_a_prefix_fires_on_its_deadline() {
    // `<C-y>` binds `core:quit` and also opens `<C-y> a`. At the deadline the
    // complete binding fires.
    let mut client = build_test_client();
    let ctrl_y_chord = build_key_chord(BindingModifierFlags::CTRL, 'y');
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[
            (KeySequence::from(ctrl_y_chord), "quit"),
            (
                KeySequence::from_first_and_rest(
                    ctrl_y_chord,
                    vec![build_key_chord(BindingModifierFlags::NONE, 'a')],
                ),
                "new-tab",
            ),
        ],
    );
    let chord_timeout = client.keymap_catalog.get_chord_timeout();
    let current_time = Instant::now();

    assert_eq!(
        client.resolve_key(ctrl_y_chord, current_time),
        KeyOutcome::Pending
    );
    assert_eq!(
        client.compute_next_key_wakeup(current_time),
        Some(chord_timeout),
        "the ambiguity arms a deadline one chord timeout out"
    );
    assert_eq!(
        client.expire_key_sequence(current_time),
        None,
        "nothing fires before the deadline"
    );

    let key_sequence_deadline = current_time + chord_timeout;
    let bound_action = client
        .expire_key_sequence(key_sequence_deadline)
        .expect("the deadline fires the complete binding");

    assert_eq!(
        bound_action.action_reference,
        ActionReference::from_core_action_name("quit").expect("valid name")
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        None,
        "the sequence is spent"
    );
    assert_eq!(
        client.compute_next_key_wakeup(key_sequence_deadline),
        None,
        "and it wakes the loop no more"
    );
}

#[test]
fn setting_the_mode_the_viewer_is_already_in_leaves_an_open_sequence_alone() {
    // The drop happens on a change of mode, so a report naming the mode the
    // viewer is already in leaves the chords it is holding where they are.
    let mut client = build_test_client();
    let prefix_chord = build_key_chord(BindingModifierFlags::CTRL, 'p');
    client.resolve_key(prefix_chord, Instant::now());

    client.set_lock_mode(LockMode::Normal);

    assert_eq!(
        client.get_pending_key_sequence(),
        Some(&KeySequence::from(prefix_chord))
    );
}

#[test]
fn the_unlock_chord_takes_a_sequence_open_in_locked_mode_with_it() {
    // A locked-mode keymap can hold a multi-chord binding, so the unlock chord
    // can arrive with chords already held. It is resolved before the sequence
    // buffer, and the held chords go with it.
    let mut client = build_test_client();
    let prefix_chord = build_key_chord(BindingModifierFlags::CTRL, 'p');
    client.keymap_catalog = build_keymap_for_mode(
        "locked",
        &[(
            KeySequence::from_first_and_rest(
                prefix_chord,
                vec![build_key_chord(BindingModifierFlags::NONE, 'n')],
            ),
            "new-pane",
        )],
    );
    client.set_lock_mode(LockMode::Locked);
    let current_time = Instant::now();
    assert_eq!(
        client.resolve_key(prefix_chord, current_time),
        KeyOutcome::Pending
    );

    let key_outcome = client.resolve_key(client.keymap_catalog.get_unlock_chord(), current_time);

    let KeyOutcome::Fire(bound_action) = key_outcome else {
        panic!("the reserved unlock always fires, got {key_outcome:?}");
    };
    assert_eq!(
        bound_action.action_reference,
        ActionReference::from_core_action_name("unlock").expect("valid name")
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        None,
        "the held chord is dropped, never typed at the pane"
    );
}

#[test]
fn a_deadline_already_past_asks_the_loop_to_wake_at_once() {
    let mut client = build_test_client();
    let ctrl_y_chord = build_key_chord(BindingModifierFlags::CTRL, 'y');
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[
            (KeySequence::from(ctrl_y_chord), "quit"),
            (
                KeySequence::from_first_and_rest(
                    ctrl_y_chord,
                    vec![build_key_chord(BindingModifierFlags::NONE, 'a')],
                ),
                "new-tab",
            ),
        ],
    );
    let chord_timeout = client.keymap_catalog.get_chord_timeout();
    let current_time = Instant::now();
    assert_eq!(
        client.resolve_key(ctrl_y_chord, current_time),
        KeyOutcome::Pending
    );

    assert_eq!(
        client.compute_next_key_wakeup(current_time + chord_timeout + Duration::from_secs(1)),
        Some(Duration::ZERO),
        "a deadline already behind the clock asks for no further wait"
    );
}

#[test]
fn a_keymap_that_retired_the_binding_drops_the_waiting_sequence_instead_of_firing() {
    // A reloaded keymap retires the binding while the sequence waits. The held
    // chords resolve to nothing.
    let mut client = build_test_client();
    let ctrl_y_chord = build_key_chord(BindingModifierFlags::CTRL, 'y');
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[
            (KeySequence::from(ctrl_y_chord), "quit"),
            (
                KeySequence::from_first_and_rest(
                    ctrl_y_chord,
                    vec![build_key_chord(BindingModifierFlags::NONE, 'a')],
                ),
                "new-tab",
            ),
        ],
    );
    let chord_timeout = client.keymap_catalog.get_chord_timeout();
    let current_time = Instant::now();
    assert_eq!(
        client.resolve_key(ctrl_y_chord, current_time),
        KeyOutcome::Pending
    );

    client.keymap_catalog = build_keymap_for_mode("normal", &[]);

    assert_eq!(
        client.expire_key_sequence(current_time + chord_timeout),
        None
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        None,
        "the held chord is dropped, never typed at the pane"
    );
}

#[test]
fn a_three_chord_binding_fires_only_on_its_third_chord() {
    let mut client = build_test_client();
    let ctrl_y_chord = build_key_chord(BindingModifierFlags::CTRL, 'y');
    let first_following_chord = build_key_chord(BindingModifierFlags::NONE, 'a');
    let second_following_chord = build_key_chord(BindingModifierFlags::NONE, 'b');
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[(
            KeySequence::from_first_and_rest(
                ctrl_y_chord,
                vec![first_following_chord, second_following_chord],
            ),
            "quit",
        )],
    );
    let current_time = Instant::now();

    assert_eq!(
        client.resolve_key(ctrl_y_chord, current_time),
        KeyOutcome::Pending
    );
    assert_eq!(
        client.resolve_key(first_following_chord, current_time),
        KeyOutcome::Pending
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        Some(&KeySequence::from_first_and_rest(
            ctrl_y_chord,
            vec![first_following_chord],
        )),
        "both chords are held, in the order they were typed"
    );

    let key_outcome = client.resolve_key(second_following_chord, current_time);

    let KeyOutcome::Fire(bound_action) = key_outcome else {
        panic!("`<C-y> a b` fires quit, got {key_outcome:?}");
    };
    assert_eq!(
        bound_action.action_reference,
        ActionReference::from_core_action_name("quit").expect("valid name")
    );
    assert_eq!(client.get_pending_key_sequence(), None);
}

#[test]
fn a_continuous_three_chord_binding_re_opens_its_two_chord_prefix() {
    // The prefix that comes back is everything but the last chord, so the last
    // chord alone repeats the action.
    let mut client = build_test_client();
    let ctrl_y_chord = build_key_chord(BindingModifierFlags::CTRL, 'y');
    let first_following_chord = build_key_chord(BindingModifierFlags::NONE, 'a');
    let second_following_chord = build_key_chord(BindingModifierFlags::NONE, 'b');
    let focus_left_action =
        ActionReference::from_core_action_name("focus-pane-left").expect("valid name");
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[(
            KeySequence::from_first_and_rest(
                ctrl_y_chord,
                vec![first_following_chord, second_following_chord],
            ),
            "focus-pane-left",
        )],
    );
    let current_time = Instant::now();
    client.resolve_key(ctrl_y_chord, current_time);
    client.resolve_key(first_following_chord, current_time);

    let key_outcome = client.resolve_key(second_following_chord, current_time);

    let KeyOutcome::Fire(bound_action) = key_outcome else {
        panic!("`<C-y> a b` fires focus-pane-left, got {key_outcome:?}");
    };
    assert_eq!(bound_action.action_reference, focus_left_action);
    assert_eq!(
        client.get_pending_key_sequence(),
        Some(&KeySequence::from_first_and_rest(
            ctrl_y_chord,
            vec![first_following_chord],
        )),
        "the two-chord prefix is held again, not the whole sequence"
    );
    assert_eq!(client.compute_next_key_wakeup(current_time), None);

    let key_outcome = client.resolve_key(second_following_chord, current_time);

    let KeyOutcome::Fire(bound_action) = key_outcome else {
        panic!("the bare `b` fires focus-pane-left again, got {key_outcome:?}");
    };
    assert_eq!(bound_action.action_reference, focus_left_action);
}

// --- The move chooser and floating pane move mode ---------------------------

/// The size of each floating pane [`build_three_floating_pane_frame`] draws.
const SMALL_FLOATING_PANE_SIZE: Size = Size {
    column_count: 20,
    row_count: 8,
};

/// A frame of one focused tiled pane, with unpinned floating panes `a`, `b`
/// and `c` made in that order and drawn back to front at columns `0`, `30` and
/// `60` of pane-area row `2`. Returns the frame, the tiled pane id and
/// `[a, b, c]`.
fn build_three_floating_pane_frame() -> (MouseFrame, PaneId, [PaneId; 3]) {
    let tiled_pane_id = PaneId::new();
    let mut mouse_frame = build_mouse_frame(
        &[build_plain_mouse_pane(tiled_pane_id)],
        Some(tiled_pane_id),
    );
    let floating_pane_ids = [PaneId::new(), PaneId::new(), PaneId::new()];
    for (floating_pane_id, column) in floating_pane_ids.into_iter().zip([0, 30, 60]) {
        add_floating_mouse_pane(
            &mut mouse_frame,
            build_plain_mouse_pane(floating_pane_id),
            Rect::from_origin_and_size(Point { column, row: 2 }, SMALL_FLOATING_PANE_SIZE),
            false,
        );
    }
    (mouse_frame, tiled_pane_id, floating_pane_ids)
}

/// A frame of tiled pane `tiled_pane_id` with one unpinned floating pane
/// `floating_pane_id` drawn in `outer_rect`, which the client focuses.
fn build_one_floating_pane_frame(
    tiled_pane_id: PaneId,
    floating_pane_id: PaneId,
    outer_rect: Rect,
) -> MouseFrame {
    let mut mouse_frame = build_mouse_frame(
        &[build_plain_mouse_pane(tiled_pane_id)],
        Some(floating_pane_id),
    );
    mouse_frame.client_snapshot.active_tab_focused_pane_id = Some(tiled_pane_id);
    add_floating_mouse_pane(
        &mut mouse_frame,
        build_plain_mouse_pane(floating_pane_id),
        outer_rect,
        false,
    );
    mouse_frame
}

/// A viewer that painted `mouse_frame`: it took the frame's view and dropped
/// the gestures the frame ended, as the attach loop does.
fn build_client_viewing(mouse_frame: &MouseFrame) -> Client {
    let mut client = build_test_client();
    client.apply_render_snapshot(&build_render_snapshot_from_mouse_frame(mouse_frame));
    client.drop_gestures_the_frame_ended(mouse_frame);
    client
}

/// The command `placement_input_action` sends, or `None` when it sends none.
fn find_submitted_command(placement_input_action: PlacementInputAction) -> Option<Command> {
    match placement_input_action {
        PlacementInputAction::SubmitCommand { command, .. } => Some(command),
        PlacementInputAction::ReadPlacement { .. } | PlacementInputAction::Consumed => None,
    }
}

/// The `FocusPane` that `client`'s key binding sends for `pane_id`.
fn build_focus_pane_key_command(client: &Client, pane_id: PaneId) -> Command {
    Command::FocusPane(FocusPaneArgs {
        focus_target: FocusTarget::Pane(pane_id),
        client_id: Some(client.get_client_id()),
    })
}

/// The floating pane that floating pane move mode selected, or `None` outside
/// that mode.
fn find_selected_floating_pane_id(client: &Client) -> Option<PaneId> {
    match client.pane_move_mode {
        Some(PaneMoveMode::FloatingPaneMove(floating_pane_move_selection)) => {
            Some(floating_pane_move_selection.selected_pane_id)
        }
        Some(PaneMoveMode::Choice) | None => None,
    }
}

#[test]
fn begin_pane_placement_opens_the_move_chooser_while_an_unpinned_floating_pane_is_drawn() {
    let (mouse_frame, _tiled_pane_id, _floating_pane_ids) = build_three_floating_pane_frame();
    let mut client = build_client_viewing(&mouse_frame);
    client.pending_key_sequence = Some(PendingKeySequence {
        sequence: KeySequence::from(build_key_chord(BindingModifierFlags::CTRL, 'p')),
        ambiguity_deadline: None,
    });

    assert_eq!(
        client.apply_client_action(ClientActionKind::BeginPanePlacement, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.pane_move_mode, Some(PaneMoveMode::Choice));
    assert_eq!(client.get_active_input_mode(), LockMode::PaneMoveChoice);
    assert_eq!(
        client
            .build_viewer_chrome(mouse_frame.client_snapshot.active_tab_id)
            .active_input_mode,
        Some(LockMode::PaneMoveChoice)
    );
    assert_eq!(client.get_pending_key_sequence(), None);
    assert_eq!(client.get_placement_source_pane_id(), None);
}

#[test]
fn begin_pane_placement_previews_the_tiled_pane_when_no_unpinned_floating_pane_is_drawn() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let no_floating_pane_frame = build_mouse_frame(
        &[build_plain_mouse_pane(tiled_pane_id)],
        Some(tiled_pane_id),
    );
    let pinned_floating_pane_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        true,
        floating_pane_id,
    );
    // Each case: the frame the action reads, and the frame the viewer painted.
    for (read_mouse_frame, painted_mouse_frame) in [
        (Some(&no_floating_pane_frame), &no_floating_pane_frame),
        (
            Some(&pinned_floating_pane_frame),
            &pinned_floating_pane_frame,
        ),
        (None, &pinned_floating_pane_frame),
    ] {
        let mut client = build_client_viewing(painted_mouse_frame);

        assert_eq!(
            client.apply_client_action(ClientActionKind::BeginPanePlacement, read_mouse_frame),
            PlacementInputAction::ReadPlacement {
                pane_id_to_focus: None,
                source_pane_id: tiled_pane_id,
                destination_tab_id: painted_mouse_frame.client_snapshot.active_tab_id,
            }
        );
        assert_eq!(client.get_active_input_mode(), LockMode::PanePlacement);
        assert_eq!(client.pane_move_mode, None);
    }
}

#[test]
fn begin_pane_placement_while_pane_placement_shows_changes_nothing() {
    let (mouse_frame, tiled_pane_id, _floating_pane_ids) = build_three_floating_pane_frame();
    let mut client = build_client_viewing(&mouse_frame);
    assert_eq!(
        client.begin_placement_mode(),
        Some((tiled_pane_id, mouse_frame.client_snapshot.active_tab_id))
    );

    assert_eq!(
        client.apply_client_action(ClientActionKind::BeginPanePlacement, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.pane_move_mode, None);
    assert_eq!(client.get_active_input_mode(), LockMode::PanePlacement);
    assert_eq!(client.get_placement_source_pane_id(), Some(tiled_pane_id));
}

#[test]
fn the_move_choosers_tiled_key_places_the_tiled_pane_while_a_floating_pane_is_focused() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_client_viewing(&mouse_frame);
    client.apply_client_action(ClientActionKind::BeginPanePlacement, Some(&mouse_frame));
    assert_eq!(client.get_active_input_mode(), LockMode::PaneMoveChoice);

    assert_eq!(
        client.apply_client_action(
            ClientActionKind::BeginTiledPanePlacement,
            Some(&mouse_frame)
        ),
        PlacementInputAction::ReadPlacement {
            pane_id_to_focus: None,
            source_pane_id: tiled_pane_id,
            destination_tab_id: mouse_frame.client_snapshot.active_tab_id,
        }
    );
    assert_eq!(client.get_active_input_mode(), LockMode::PanePlacement);
    assert_eq!(client.pane_move_mode, None);
    assert_eq!(
        client
            .build_frame_layout_with_current_chrome(&mouse_frame)
            .list_drawn_floating_pane_slots()
            .count(),
        0
    );

    assert_eq!(
        client.apply_client_action(ClientActionKind::CancelPanePlacement, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.get_active_input_mode(), LockMode::Normal);
    assert_eq!(
        client
            .build_frame_layout_with_current_chrome(&mouse_frame)
            .list_drawn_floating_pane_slots()
            .map(|floating_pane_slot| floating_pane_slot.pane_id)
            .collect::<Vec<PaneId>>(),
        [floating_pane_id]
    );
}

#[test]
fn the_move_choosers_tiled_key_with_no_tiled_focus_closes_the_chooser() {
    let floating_pane_id = PaneId::new();
    let mut mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    mouse_frame.client_snapshot.active_tab_focused_pane_id = None;
    let mut client = build_client_viewing(&mouse_frame);
    client.apply_client_action(ClientActionKind::BeginPanePlacement, Some(&mouse_frame));

    assert_eq!(
        client.apply_client_action(
            ClientActionKind::BeginTiledPanePlacement,
            Some(&mouse_frame)
        ),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.pane_move_mode, None);
    assert_eq!(client.get_active_input_mode(), LockMode::Normal);
}

#[test]
fn the_move_choosers_enter_moves_the_focused_pane_by_its_kind() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let floating_focus_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_client_viewing(&floating_focus_frame);
    client.apply_client_action(
        ClientActionKind::BeginPanePlacement,
        Some(&floating_focus_frame),
    );

    let placement_input_action = client.apply_client_action(
        ClientActionKind::ConfirmPaneMoveChoice,
        Some(&floating_focus_frame),
    );
    assert_eq!(
        find_submitted_command(placement_input_action),
        Some(build_focus_pane_key_command(&client, floating_pane_id))
    );
    assert_eq!(
        client.pane_move_mode,
        Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
            selected_pane_id: floating_pane_id,
            draft_top_left_cell: None,
            pending_move_command_id: None,
        }))
    );

    let tiled_focus_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        tiled_pane_id,
    );
    let mut client = build_client_viewing(&tiled_focus_frame);
    client.apply_client_action(
        ClientActionKind::BeginPanePlacement,
        Some(&tiled_focus_frame),
    );

    assert_eq!(
        client.apply_client_action(
            ClientActionKind::ConfirmPaneMoveChoice,
            Some(&tiled_focus_frame)
        ),
        PlacementInputAction::ReadPlacement {
            pane_id_to_focus: None,
            source_pane_id: tiled_pane_id,
            destination_tab_id: tiled_focus_frame.client_snapshot.active_tab_id,
        }
    );
    assert_eq!(client.pane_move_mode, None);
}

#[test]
fn the_move_choosers_cancel_returns_to_the_base_mode_and_leaves_floating_pane_move_mode_on() {
    let (mouse_frame, _tiled_pane_id, [_, _, front_floating_pane_id]) =
        build_three_floating_pane_frame();
    let mut client = build_client_viewing(&mouse_frame);
    client.set_lock_mode(LockMode::Locked);
    client.apply_client_action(ClientActionKind::BeginPanePlacement, Some(&mouse_frame));

    assert_eq!(
        client.apply_client_action(ClientActionKind::CancelPaneMoveChoice, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.pane_move_mode, None);
    assert_eq!(client.get_active_input_mode(), LockMode::Locked);

    client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));
    assert_eq!(
        client.apply_client_action(ClientActionKind::CancelPaneMoveChoice, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(
        find_selected_floating_pane_id(&client),
        Some(front_floating_pane_id)
    );
}

#[test]
fn the_move_modes_resolve_their_own_keys_and_discard_the_rest() {
    let (mouse_frame, _tiled_pane_id, _floating_pane_ids) = build_three_floating_pane_frame();
    let mut client = build_client_viewing(&mouse_frame);
    let current_time = Instant::now();
    client.apply_client_action(ClientActionKind::BeginPanePlacement, Some(&mouse_frame));

    assert_eq!(
        client.resolve_key(
            build_key_chord(BindingModifierFlags::NONE, 'a'),
            current_time
        ),
        KeyOutcome::Discard
    );
    assert_eq!(
        client.resolve_key(
            build_key_chord(BindingModifierFlags::NONE, 't'),
            current_time
        ),
        KeyOutcome::Fire(build_core_bound_action("begin-tiled-pane-placement"))
    );

    client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));
    assert_eq!(
        client.resolve_key(
            build_key_chord(BindingModifierFlags::NONE, 'a'),
            current_time
        ),
        KeyOutcome::Discard
    );
    assert_eq!(
        client.resolve_key(
            KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Right)),
            current_time
        ),
        KeyOutcome::Fire(build_core_bound_action("move-floating-pane-right"))
    );
}

#[test]
fn a_lock_mode_or_mouse_select_change_ends_the_move_modes() {
    let (mouse_frame, _tiled_pane_id, _floating_pane_ids) = build_three_floating_pane_frame();
    for opening_action_kind in [
        ClientActionKind::BeginPanePlacement,
        ClientActionKind::BeginFloatingPaneMove,
    ] {
        let mut client = build_client_viewing(&mouse_frame);
        client.apply_client_action(opening_action_kind, Some(&mouse_frame));
        let opened_pane_move_mode = client.pane_move_mode;
        client.set_lock_mode(LockMode::Normal);
        client.set_mouse_selection_enabled(false);
        assert_eq!(
            client.pane_move_mode, opened_pane_move_mode,
            "an unchanged mode keeps it on"
        );

        client.set_lock_mode(LockMode::Locked);
        assert_eq!(client.pane_move_mode, None);

        client.apply_client_action(opening_action_kind, Some(&mouse_frame));
        client.set_mouse_selection_enabled(true);
        assert_eq!(client.pane_move_mode, None);
    }
}

#[test]
fn the_move_chooser_owns_the_keyboard_only_while_linked() {
    let (mouse_frame, _tiled_pane_id, _floating_pane_ids) = build_three_floating_pane_frame();
    let mut client = build_client_viewing(&mouse_frame);
    client.apply_client_action(ClientActionKind::BeginPanePlacement, Some(&mouse_frame));

    client.set_link_status(LinkStatus::AwaitingRestartedSession);
    assert_eq!(client.get_active_input_mode(), LockMode::Normal);
    assert_eq!(
        client.find_viewer_local_input_mode(),
        Some(LockMode::PaneMoveChoice)
    );

    client.set_link_status(LinkStatus::Linked);
    assert_eq!(client.get_active_input_mode(), LockMode::PaneMoveChoice);
}

#[test]
fn begin_floating_pane_move_selects_the_focused_floating_pane() {
    let floating_pane_id = PaneId::new();
    let mouse_frame =
        build_one_floating_pane_frame(PaneId::new(), floating_pane_id, FLOATING_PANE_OUTER_RECT);
    let mut client = build_client_viewing(&mouse_frame);

    let placement_input_action =
        client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));

    assert_eq!(
        find_submitted_command(placement_input_action),
        Some(build_focus_pane_key_command(&client, floating_pane_id)),
        "the mode focuses the pane the frame focuses too"
    );
    assert_eq!(
        client.pane_move_mode,
        Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
            selected_pane_id: floating_pane_id,
            draft_top_left_cell: None,
            pending_move_command_id: None,
        }))
    );
    assert_eq!(client.get_active_input_mode(), LockMode::FloatingPaneMove);
    assert_eq!(
        client
            .build_viewer_chrome(mouse_frame.client_snapshot.active_tab_id)
            .active_input_mode,
        Some(LockMode::FloatingPaneMove)
    );
}

#[test]
fn begin_floating_pane_move_selects_the_front_floating_pane_and_focuses_it() {
    let (mouse_frame, _tiled_pane_id, [_, _, front_floating_pane_id]) =
        build_three_floating_pane_frame();
    let mut client = build_client_viewing(&mouse_frame);

    let placement_input_action =
        client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));

    assert_eq!(
        find_submitted_command(placement_input_action),
        Some(build_focus_pane_key_command(
            &client,
            front_floating_pane_id
        ))
    );
    assert_eq!(
        find_selected_floating_pane_id(&client),
        Some(front_floating_pane_id)
    );
}

#[test]
fn begin_floating_pane_move_passes_over_pinned_floating_panes() {
    let (mut mouse_frame, _tiled_pane_id, [back_floating_pane_id, middle_floating_pane_id, _]) =
        build_three_floating_pane_frame();
    mouse_frame.session_snapshot.floating_pane_slots[0].is_pinned = true;
    mouse_frame.session_snapshot.floating_pane_slots[2].is_pinned = true;
    mouse_frame.client_snapshot.focused_pane_id = Some(back_floating_pane_id);
    let mut client = build_client_viewing(&mouse_frame);

    let placement_input_action =
        client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));

    assert_eq!(
        find_submitted_command(placement_input_action),
        Some(build_focus_pane_key_command(
            &client,
            middle_floating_pane_id
        ))
    );
    assert_eq!(
        find_selected_floating_pane_id(&client),
        Some(middle_floating_pane_id)
    );
}

#[test]
fn begin_floating_pane_move_with_no_movable_floating_pane_closes_the_chooser() {
    let floating_pane_id = PaneId::new();
    let pinned_floating_pane_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        true,
        floating_pane_id,
    );
    for read_mouse_frame in [Some(&pinned_floating_pane_frame), None] {
        let mut client = build_client_viewing(&pinned_floating_pane_frame);
        client.pane_move_mode = Some(PaneMoveMode::Choice);

        assert_eq!(
            client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, read_mouse_frame),
            PlacementInputAction::Consumed
        );
        assert_eq!(client.pane_move_mode, None);
        assert_eq!(client.get_active_input_mode(), LockMode::Normal);
    }
}

#[test]
fn begin_floating_pane_move_ends_pane_placement_and_waits_for_a_sent_placement() {
    let (mouse_frame, _tiled_pane_id, [_, _, front_floating_pane_id]) =
        build_three_floating_pane_frame();
    let mut client = build_client_viewing(&mouse_frame);
    client.begin_placement_mode();

    client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));
    assert!(!client.is_placement_mode_active());
    assert_eq!(
        find_selected_floating_pane_id(&client),
        Some(front_floating_pane_id)
    );

    let mut client = build_client_viewing(&mouse_frame);
    client.begin_placement_mode();
    client
        .placement_state
        .placement_mode
        .as_mut()
        .expect("pane placement mode is on")
        .pending_placement_command = Some(PendingPlacementCommand {
        command_id: CommandId::new(),
        is_committed: false,
    });

    assert_eq!(
        client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.pane_move_mode, None);
    assert!(client.is_placement_confirmation_pending());
}

#[test]
fn begin_floating_pane_move_ends_the_capture_a_held_button_holds() {
    let tiled_pane_id = PaneId::new();
    let tracking_tiled_mouse_pane = MousePane {
        mouse_tracking: MouseTracking::Normal,
        ..build_plain_mouse_pane(tiled_pane_id)
    };
    let mouse_frame = build_floating_mouse_frame(
        tracking_tiled_mouse_pane,
        build_plain_mouse_pane(PaneId::new()),
        false,
        tiled_pane_id,
    );
    let mut client = build_client_viewing(&mouse_frame);
    client.handle_mouse(
        build_left_mouse_press(Point {
            column: 70,
            row: 20,
        }),
        &mouse_frame,
        Instant::now(),
    );

    client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));

    assert_eq!(
        client.take_mouse_capture_release(),
        Some(MouseAction::Forward {
            pane_id: tiled_pane_id,
            mouse_kind: MouseKind::Release(MouseButton::Left),
            modifier_flags: BindingModifierFlags::NONE,
            pane_local_cell: Point {
                column: 69,
                row: 18,
            },
        })
    );
}

#[test]
fn an_arrow_moves_the_selected_floating_pane_one_cell_in_this_viewer_only() {
    let floating_pane_id = PaneId::new();
    let mouse_frame =
        build_one_floating_pane_frame(PaneId::new(), floating_pane_id, FLOATING_PANE_OUTER_RECT);
    let mut client = build_client_viewing(&mouse_frame);
    client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));

    for (direction, expected_top_left_cell) in [
        (Direction::Right, Point { column: 11, row: 4 }),
        (Direction::Down, Point { column: 11, row: 5 }),
        (Direction::Left, Point { column: 10, row: 5 }),
        (Direction::Up, Point { column: 10, row: 4 }),
    ] {
        assert_eq!(
            client.apply_client_action(
                ClientActionKind::MoveFloatingPane(direction),
                Some(&mouse_frame)
            ),
            PlacementInputAction::Consumed
        );
        assert_eq!(
            find_drawn_floating_pane_top_left_cell(&client, &mouse_frame, floating_pane_id),
            Some(expected_top_left_cell)
        );
    }
    assert_eq!(client.sent_floating_pane_move, None);
}

#[test]
fn an_arrow_stops_at_the_pane_area_edges() {
    // A `40x12` pane in the `80x22` pane area reaches from `(0, 0)` to
    // `(40, 10)`.
    let floating_pane_id = PaneId::new();
    for (top_left_cell, edge_directions) in [
        (
            Point { column: 0, row: 0 },
            [Direction::Left, Direction::Up],
        ),
        (
            Point {
                column: 40,
                row: 10,
            },
            [Direction::Right, Direction::Down],
        ),
    ] {
        let mouse_frame = build_one_floating_pane_frame(
            PaneId::new(),
            floating_pane_id,
            Rect::from_origin_and_size(top_left_cell, FLOATING_PANE_OUTER_RECT.size),
        );
        let mut client = build_client_viewing(&mouse_frame);
        client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));

        for direction in edge_directions {
            client.apply_client_action(
                ClientActionKind::MoveFloatingPane(direction),
                Some(&mouse_frame),
            );
            assert_eq!(
                find_drawn_floating_pane_top_left_cell(&client, &mouse_frame, floating_pane_id),
                Some(top_left_cell)
            );
        }
    }
}

#[test]
fn an_arrow_moves_nothing_while_a_handle_drag_is_held_or_a_sent_move_waits() {
    let floating_pane_id = PaneId::new();
    let mouse_frame =
        build_one_floating_pane_frame(PaneId::new(), floating_pane_id, FLOATING_PANE_OUTER_RECT);
    let mut client = build_client_viewing(&mouse_frame);
    let current_time = Instant::now();
    client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));
    client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(Point { column: 17, row: 7 }),
        &mouse_frame,
        current_time,
    );

    assert_eq!(
        client.apply_client_action(
            ClientActionKind::MoveFloatingPane(Direction::Right),
            Some(&mouse_frame)
        ),
        PlacementInputAction::Consumed
    );
    assert_eq!(
        client.apply_client_action(
            ClientActionKind::ConfirmFloatingPaneMove,
            Some(&mouse_frame)
        ),
        PlacementInputAction::Consumed
    );
    assert_eq!(
        find_floating_pane_move_draft(&client, &mouse_frame),
        Some(FloatingPaneMoveDraft {
            pane_id: floating_pane_id,
            top_left_cell: Point { column: 15, row: 5 },
        })
    );

    let release_actions = client.handle_mouse(
        build_left_mouse_release(Point { column: 17, row: 7 }),
        &mouse_frame,
        current_time,
    );
    let [MouseAction::MoveFloatingPane { command_args, .. }] = release_actions.as_slice() else {
        panic!("the release sends one move, got {release_actions:?}");
    };
    assert_eq!(
        *command_args,
        MoveFloatingPaneArgs {
            pane_id: floating_pane_id,
            top_left_cell: Point { column: 15, row: 5 },
        }
    );
    client.apply_client_action(
        ClientActionKind::MoveFloatingPane(Direction::Right),
        Some(&mouse_frame),
    );
    assert_eq!(
        find_drawn_floating_pane_top_left_cell(&client, &mouse_frame, floating_pane_id),
        Some(Point { column: 15, row: 5 })
    );
}

#[test]
fn floating_pane_move_keys_do_nothing_outside_floating_pane_move_mode() {
    let (mouse_frame, _tiled_pane_id, _floating_pane_ids) = build_three_floating_pane_frame();
    let mut client = build_client_viewing(&mouse_frame);
    for client_action_kind in [
        ClientActionKind::MoveFloatingPane(Direction::Right),
        ClientActionKind::SelectNextFloatingPane,
        ClientActionKind::SelectPreviousFloatingPane,
        ClientActionKind::ConfirmFloatingPaneMove,
        ClientActionKind::CancelFloatingPaneMove,
        ClientActionKind::CancelPaneMoveChoice,
    ] {
        assert_eq!(
            client.apply_client_action(client_action_kind, Some(&mouse_frame)),
            PlacementInputAction::Consumed
        );
        assert_eq!(client.pane_move_mode, None);
        assert_eq!(find_floating_pane_move_draft(&client, &mouse_frame), None);
    }
}

#[test]
fn enter_sends_exactly_one_move_to_the_cell_the_arrows_reached() {
    let floating_pane_id = PaneId::new();
    let mouse_frame =
        build_one_floating_pane_frame(PaneId::new(), floating_pane_id, FLOATING_PANE_OUTER_RECT);
    let mut client = build_client_viewing(&mouse_frame);
    client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));
    for _ in 0..2 {
        client.apply_client_action(
            ClientActionKind::MoveFloatingPane(Direction::Right),
            Some(&mouse_frame),
        );
    }

    let placement_input_action = client.apply_client_action(
        ClientActionKind::ConfirmFloatingPaneMove,
        Some(&mouse_frame),
    );

    let PlacementInputAction::SubmitCommand {
        command_id,
        command,
    } = placement_input_action
    else {
        panic!("Enter sends the move, got {placement_input_action:?}");
    };
    assert_eq!(
        command,
        Command::MoveFloatingPane(MoveFloatingPaneArgs {
            pane_id: floating_pane_id,
            top_left_cell: Point { column: 12, row: 4 },
        })
    );
    assert_eq!(
        client.pane_move_mode,
        Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
            selected_pane_id: floating_pane_id,
            draft_top_left_cell: None,
            pending_move_command_id: Some(command_id),
        }))
    );
    assert_eq!(
        find_floating_pane_move_draft(&client, &mouse_frame),
        Some(FloatingPaneMoveDraft {
            pane_id: floating_pane_id,
            top_left_cell: Point { column: 12, row: 4 },
        }),
        "the viewer draws the sent move ahead of the session"
    );
    assert_eq!(
        client.apply_client_action(
            ClientActionKind::ConfirmFloatingPaneMove,
            Some(&mouse_frame)
        ),
        PlacementInputAction::Consumed,
        "a second Enter sends nothing"
    );

    assert!(client.reject_placement_command(command_id));
    assert_eq!(find_floating_pane_move_draft(&client, &mouse_frame), None);
}

/// A viewer of `mouse_frame` with `stay-in-pane-placement-mode-after-placement`
/// set to `should_stay`, whose floating pane move mode moved its selected pane
/// one row down and sent the move with Enter, and the command id of that move.
fn build_client_waiting_for_floating_pane_move(
    mouse_frame: &MouseFrame,
    should_stay: bool,
) -> (Client, CommandId) {
    let mut client = build_client_viewing(mouse_frame);
    client
        .client_config
        .should_stay_in_pane_placement_mode_after_placement = should_stay;
    client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(mouse_frame));
    client.apply_client_action(
        ClientActionKind::MoveFloatingPane(Direction::Down),
        Some(mouse_frame),
    );
    let placement_input_action =
        client.apply_client_action(ClientActionKind::ConfirmFloatingPaneMove, Some(mouse_frame));
    let PlacementInputAction::SubmitCommand { command_id, .. } = placement_input_action else {
        panic!("Enter sends the move, got {placement_input_action:?}");
    };
    (client, command_id)
}

#[test]
fn a_floating_pane_move_the_session_runs_ends_the_mode_only_with_the_stay_setting_off() {
    let floating_pane_id = PaneId::new();
    let mouse_frame =
        build_one_floating_pane_frame(PaneId::new(), floating_pane_id, FLOATING_PANE_OUTER_RECT);
    for (should_stay, expected_pane_move_mode) in [
        (
            true,
            build_floating_pane_move_mode(floating_pane_id, None, None),
        ),
        (false, None),
    ] {
        let (mut client, command_id) =
            build_client_waiting_for_floating_pane_move(&mouse_frame, should_stay);
        assert_eq!(
            client.pane_move_mode,
            build_floating_pane_move_mode(floating_pane_id, None, Some(command_id))
        );

        client.apply_render_snapshot(&build_render_snapshot_from_mouse_frame(&mouse_frame));
        assert_eq!(
            client.pane_move_mode,
            build_floating_pane_move_mode(floating_pane_id, None, Some(command_id)),
            "a frame that does not name the move keeps the mode waiting"
        );

        client.apply_render_snapshot(&build_floating_pane_move_run_snapshot(
            &mouse_frame,
            command_id,
        ));
        assert_eq!(client.pane_move_mode, expected_pane_move_mode);
    }
}

#[test]
fn a_rejected_floating_pane_move_keeps_the_mode_on_with_either_stay_setting() {
    let floating_pane_id = PaneId::new();
    let mouse_frame =
        build_one_floating_pane_frame(PaneId::new(), floating_pane_id, FLOATING_PANE_OUTER_RECT);
    for should_stay in [true, false] {
        let (mut client, command_id) =
            build_client_waiting_for_floating_pane_move(&mouse_frame, should_stay);
        assert_eq!(
            find_drawn_floating_pane_top_left_cell(&client, &mouse_frame, floating_pane_id),
            Some(Point { column: 10, row: 5 })
        );

        assert!(client.reject_placement_command(command_id));

        assert_eq!(
            client.pane_move_mode,
            build_floating_pane_move_mode(floating_pane_id, None, None)
        );
        assert_eq!(
            find_drawn_floating_pane_top_left_cell(&client, &mouse_frame, floating_pane_id),
            Some(Point { column: 10, row: 4 }),
            "the pane goes back to the cell the session places it at"
        );
    }
}

#[test]
fn a_lost_answer_leaves_the_next_frame_to_show_whether_the_floating_pane_move_ran() {
    let floating_pane_id = PaneId::new();
    let mouse_frame =
        build_one_floating_pane_frame(PaneId::new(), floating_pane_id, FLOATING_PANE_OUTER_RECT);
    let answer_losses: [fn(&mut Client); 2] = [
        Client::forget_sent_floating_pane_move,
        Client::end_mouse_gestures,
    ];
    for lose_answer in answer_losses {
        for is_move_named in [true, false] {
            let (mut client, command_id) =
                build_client_waiting_for_floating_pane_move(&mouse_frame, false);

            lose_answer(&mut client);
            assert_eq!(
                client.pane_move_mode,
                build_floating_pane_move_mode(floating_pane_id, None, Some(command_id)),
                "the mode waits until the next frame"
            );

            let next_snapshot = if is_move_named {
                build_floating_pane_move_run_snapshot(&mouse_frame, command_id)
            } else {
                build_render_snapshot_from_mouse_frame(&mouse_frame)
            };
            client.apply_render_snapshot(&next_snapshot);

            let expected_pane_move_mode = if is_move_named {
                None
            } else {
                build_floating_pane_move_mode(floating_pane_id, None, None)
            };
            assert_eq!(client.pane_move_mode, expected_pane_move_mode);
        }
    }
}

#[test]
fn keys_while_a_floating_pane_move_waits_change_nothing() {
    let (mouse_frame, _tiled_pane_id, [_, _, third_floating_pane_id]) =
        build_three_floating_pane_frame();
    let keep_answer: fn(&mut Client) = |_client| {};
    // A resync and a new link drop the move drawn ahead of the session, and
    // the mode waits on until the next frame.
    let answer_losses: [fn(&mut Client); 3] = [
        keep_answer,
        Client::forget_sent_floating_pane_move,
        Client::end_mouse_gestures,
    ];
    let expected_top_left_cells = [
        Point { column: 60, row: 3 },
        Point { column: 60, row: 2 },
        Point { column: 60, row: 2 },
    ];
    for (lose_answer, expected_top_left_cell) in
        answer_losses.into_iter().zip(expected_top_left_cells)
    {
        let (mut client, command_id) =
            build_client_waiting_for_floating_pane_move(&mouse_frame, false);
        lose_answer(&mut client);
        assert_eq!(
            client.pane_move_mode,
            build_floating_pane_move_mode(third_floating_pane_id, None, Some(command_id))
        );

        for client_action_kind in [
            ClientActionKind::MoveFloatingPane(Direction::Right),
            ClientActionKind::ConfirmFloatingPaneMove,
            ClientActionKind::SelectNextFloatingPane,
            ClientActionKind::SelectPreviousFloatingPane,
            ClientActionKind::CancelFloatingPaneMove,
            ClientActionKind::BeginPanePlacement,
            ClientActionKind::BeginTiledPanePlacement,
            ClientActionKind::BeginFloatingPaneMove,
            ClientActionKind::ConfirmPaneMoveChoice,
            ClientActionKind::CancelPaneMoveChoice,
        ] {
            assert_eq!(
                client.apply_client_action(client_action_kind, Some(&mouse_frame)),
                PlacementInputAction::Consumed,
                "{client_action_kind:?}"
            );
            assert_eq!(
                client.pane_move_mode,
                build_floating_pane_move_mode(third_floating_pane_id, None, Some(command_id)),
                "{client_action_kind:?}"
            );
            assert!(!client.is_placement_mode_active(), "{client_action_kind:?}");
        }
        assert_eq!(
            find_drawn_floating_pane_top_left_cell(&client, &mouse_frame, third_floating_pane_id),
            Some(expected_top_left_cell)
        );
    }
}

#[test]
fn a_lock_mouse_select_or_focus_change_while_a_floating_pane_move_waits_keeps_the_mode() {
    let floating_pane_id = PaneId::new();
    let mouse_frame =
        build_one_floating_pane_frame(PaneId::new(), floating_pane_id, FLOATING_PANE_OUTER_RECT);
    let (mut client, command_id) = build_client_waiting_for_floating_pane_move(&mouse_frame, false);

    client.set_lock_mode(LockMode::Locked);
    assert_eq!(
        client.pane_move_mode,
        build_floating_pane_move_mode(floating_pane_id, None, Some(command_id))
    );
    assert_eq!(client.get_active_input_mode(), LockMode::FloatingPaneMove);

    client.set_mouse_selection_enabled(true);
    assert_eq!(
        client.pane_move_mode,
        build_floating_pane_move_mode(floating_pane_id, None, Some(command_id))
    );

    client.close_pane_move_mode();
    assert_eq!(
        client.pane_move_mode,
        build_floating_pane_move_mode(floating_pane_id, None, Some(command_id))
    );

    client.apply_render_snapshot(&build_floating_pane_move_run_snapshot(
        &mouse_frame,
        command_id,
    ));
    assert_eq!(client.pane_move_mode, None);
}

#[test]
fn a_lock_or_mouse_select_change_in_the_frame_that_runs_the_floating_pane_move_ends_the_mode() {
    let floating_pane_id = PaneId::new();
    let mouse_frame =
        build_one_floating_pane_frame(PaneId::new(), floating_pane_id, FLOATING_PANE_OUTER_RECT);
    let change_frame_views: [fn(&mut ClientSnapshot); 2] = [
        |client_snapshot| client_snapshot.lock_mode = LockMode::Locked,
        |client_snapshot| client_snapshot.is_mouse_selection_enabled = true,
    ];
    for change_frame_view in change_frame_views {
        let (mut client, command_id) =
            build_client_waiting_for_floating_pane_move(&mouse_frame, true);
        let mut move_run_snapshot = build_floating_pane_move_run_snapshot(&mouse_frame, command_id);
        change_frame_view(&mut move_run_snapshot.client_snapshot);

        client.apply_render_snapshot(&move_run_snapshot);

        assert_eq!(client.pane_move_mode, None);
    }
}

#[test]
fn a_floating_pane_move_sent_before_the_mode_opened_leaves_the_mode_on_when_it_runs() {
    let floating_pane_id = PaneId::new();
    let mouse_frame =
        build_one_floating_pane_frame(PaneId::new(), floating_pane_id, FLOATING_PANE_OUTER_RECT);
    let mut client = build_client_viewing(&mouse_frame);
    client
        .client_config
        .should_stay_in_pane_placement_mode_after_placement = false;
    let command_id =
        client.record_sent_floating_pane_move(floating_pane_id, Point { column: 11, row: 4 });
    client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));
    assert_eq!(
        client.pane_move_mode,
        build_floating_pane_move_mode(floating_pane_id, None, None)
    );

    client.apply_render_snapshot(&build_floating_pane_move_run_snapshot(
        &mouse_frame,
        command_id,
    ));

    assert_eq!(
        client.pane_move_mode,
        build_floating_pane_move_mode(floating_pane_id, None, None)
    );
}

#[test]
fn enter_with_no_draft_or_a_draft_back_at_the_session_cell_sends_nothing() {
    let floating_pane_id = PaneId::new();
    let mouse_frame =
        build_one_floating_pane_frame(PaneId::new(), floating_pane_id, FLOATING_PANE_OUTER_RECT);
    let mut client = build_client_viewing(&mouse_frame);
    client
        .client_config
        .should_stay_in_pane_placement_mode_after_placement = false;
    client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));
    assert_eq!(
        client.apply_client_action(
            ClientActionKind::ConfirmFloatingPaneMove,
            Some(&mouse_frame)
        ),
        PlacementInputAction::Consumed
    );

    client.apply_client_action(
        ClientActionKind::MoveFloatingPane(Direction::Right),
        Some(&mouse_frame),
    );
    client.apply_client_action(
        ClientActionKind::MoveFloatingPane(Direction::Left),
        Some(&mouse_frame),
    );

    assert_eq!(
        client.apply_client_action(
            ClientActionKind::ConfirmFloatingPaneMove,
            Some(&mouse_frame)
        ),
        PlacementInputAction::Consumed
    );
    assert_eq!(
        client.pane_move_mode,
        Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
            selected_pane_id: floating_pane_id,
            draft_top_left_cell: None,
            pending_move_command_id: None,
        })),
        "a move that sends nothing keeps the mode on"
    );
    assert_eq!(client.sent_floating_pane_move, None);
}

#[test]
fn escape_ends_floating_pane_move_mode_and_drops_the_draft() {
    let floating_pane_id = PaneId::new();
    let mouse_frame =
        build_one_floating_pane_frame(PaneId::new(), floating_pane_id, FLOATING_PANE_OUTER_RECT);
    let mut client = build_client_viewing(&mouse_frame);
    client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));
    client.apply_client_action(
        ClientActionKind::MoveFloatingPane(Direction::Right),
        Some(&mouse_frame),
    );

    assert_eq!(
        client.resolve_key(
            KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Esc)),
            Instant::now()
        ),
        KeyOutcome::Fire(build_core_bound_action("cancel-floating-pane-move"))
    );
    assert_eq!(
        client.apply_client_action(ClientActionKind::CancelFloatingPaneMove, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.pane_move_mode, None);
    assert_eq!(client.get_active_input_mode(), LockMode::Normal);
    assert_eq!(
        find_drawn_floating_pane_top_left_cell(&client, &mouse_frame, floating_pane_id),
        Some(FLOATING_PANE_OUTER_RECT.origin)
    );
    assert_eq!(client.sent_floating_pane_move, None);
}

#[test]
fn tab_and_shift_tab_cycle_the_floating_panes_in_the_order_they_became_floating() {
    let (
        mouse_frame,
        _tiled_pane_id,
        [first_floating_pane_id, second_floating_pane_id, third_floating_pane_id],
    ) = build_three_floating_pane_frame();
    let mut client = build_client_viewing(&mouse_frame);
    client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));
    assert_eq!(
        find_selected_floating_pane_id(&client),
        Some(third_floating_pane_id)
    );

    for expected_pane_id in [
        second_floating_pane_id,
        first_floating_pane_id,
        third_floating_pane_id,
    ] {
        let placement_input_action = client.apply_client_action(
            ClientActionKind::SelectPreviousFloatingPane,
            Some(&mouse_frame),
        );
        assert_eq!(
            find_submitted_command(placement_input_action),
            Some(build_focus_pane_key_command(&client, expected_pane_id))
        );
        assert_eq!(
            find_selected_floating_pane_id(&client),
            Some(expected_pane_id)
        );
    }

    let placement_input_action =
        client.apply_client_action(ClientActionKind::SelectNextFloatingPane, Some(&mouse_frame));
    assert_eq!(
        find_submitted_command(placement_input_action),
        Some(build_focus_pane_key_command(
            &client,
            first_floating_pane_id
        ))
    );
    assert_eq!(
        find_selected_floating_pane_id(&client),
        Some(first_floating_pane_id)
    );
}

#[test]
fn a_new_selection_focuses_its_pane_also_when_the_painted_frame_focuses_it() {
    let (mut mouse_frame, _tiled_pane_id, [_, second_floating_pane_id, third_floating_pane_id]) =
        build_three_floating_pane_frame();
    mouse_frame.client_snapshot.focused_pane_id = Some(third_floating_pane_id);
    let mut client = build_client_viewing(&mouse_frame);
    client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));

    // No frame arrives between the keys: the painted frame still focuses the
    // third pane when the second key selects it again.
    for (client_action_kind, expected_pane_id) in [
        (
            ClientActionKind::SelectPreviousFloatingPane,
            second_floating_pane_id,
        ),
        (
            ClientActionKind::SelectNextFloatingPane,
            third_floating_pane_id,
        ),
    ] {
        let placement_input_action =
            client.apply_client_action(client_action_kind, Some(&mouse_frame));
        assert_eq!(
            find_submitted_command(placement_input_action),
            Some(build_focus_pane_key_command(&client, expected_pane_id)),
            "{client_action_kind:?}"
        );
        assert_eq!(
            find_selected_floating_pane_id(&client),
            Some(expected_pane_id),
            "{client_action_kind:?}"
        );
    }
}

#[test]
fn the_selection_passes_over_pinned_and_hidden_floating_panes() {
    let (mut mouse_frame, _tiled_pane_id, [first_floating_pane_id, _, third_floating_pane_id]) =
        build_three_floating_pane_frame();
    mouse_frame.session_snapshot.floating_pane_slots[1].is_pinned = true;
    // A minimized floating pane is listed but not drawn.
    mouse_frame
        .session_snapshot
        .floating_pane_ids
        .push(PaneId::new());
    let mut client = build_client_viewing(&mouse_frame);
    client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));

    for expected_pane_id in [first_floating_pane_id, third_floating_pane_id] {
        client.apply_client_action(ClientActionKind::SelectNextFloatingPane, Some(&mouse_frame));
        assert_eq!(
            find_selected_floating_pane_id(&client),
            Some(expected_pane_id)
        );
    }
    client.apply_client_action(
        ClientActionKind::SelectPreviousFloatingPane,
        Some(&mouse_frame),
    );
    assert_eq!(
        find_selected_floating_pane_id(&client),
        Some(first_floating_pane_id)
    );
}

#[test]
fn the_selection_with_one_movable_floating_pane_keeps_the_draft() {
    let floating_pane_id = PaneId::new();
    let mouse_frame =
        build_one_floating_pane_frame(PaneId::new(), floating_pane_id, FLOATING_PANE_OUTER_RECT);
    let mut client = build_client_viewing(&mouse_frame);
    client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));
    client.apply_client_action(
        ClientActionKind::MoveFloatingPane(Direction::Right),
        Some(&mouse_frame),
    );

    for client_action_kind in [
        ClientActionKind::SelectNextFloatingPane,
        ClientActionKind::SelectPreviousFloatingPane,
    ] {
        assert_eq!(
            client.apply_client_action(client_action_kind, Some(&mouse_frame)),
            PlacementInputAction::Consumed
        );
        assert_eq!(
            client.pane_move_mode,
            Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
                selected_pane_id: floating_pane_id,
                draft_top_left_cell: Some(Point { column: 11, row: 4 }),
                pending_move_command_id: None,
            }))
        );
    }
}

#[test]
fn a_new_selection_drops_the_draft_of_the_pane_before_it() {
    let (mouse_frame, _tiled_pane_id, [first_floating_pane_id, _, third_floating_pane_id]) =
        build_three_floating_pane_frame();
    let mut client = build_client_viewing(&mouse_frame);
    client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));
    client.apply_client_action(
        ClientActionKind::MoveFloatingPane(Direction::Left),
        Some(&mouse_frame),
    );
    assert_eq!(
        find_drawn_floating_pane_top_left_cell(&client, &mouse_frame, third_floating_pane_id),
        Some(Point { column: 59, row: 2 })
    );

    client.apply_client_action(ClientActionKind::SelectNextFloatingPane, Some(&mouse_frame));

    assert_eq!(
        client.pane_move_mode,
        Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
            selected_pane_id: first_floating_pane_id,
            draft_top_left_cell: None,
            pending_move_command_id: None,
        }))
    );
    assert_eq!(
        find_drawn_floating_pane_top_left_cell(&client, &mouse_frame, third_floating_pane_id),
        Some(Point { column: 60, row: 2 })
    );
}

#[test]
fn a_selected_pane_that_cannot_move_hands_the_selection_to_the_first_or_last_movable_pane() {
    let (
        mouse_frame,
        _tiled_pane_id,
        [first_floating_pane_id, second_floating_pane_id, third_floating_pane_id],
    ) = build_three_floating_pane_frame();
    let mut pinned_third_pane_frame = mouse_frame.clone();
    pinned_third_pane_frame.session_snapshot.floating_pane_slots[2].is_pinned = true;
    for (client_action_kind, expected_pane_id) in [
        (
            ClientActionKind::SelectNextFloatingPane,
            first_floating_pane_id,
        ),
        (
            ClientActionKind::SelectPreviousFloatingPane,
            second_floating_pane_id,
        ),
    ] {
        let mut client = build_client_viewing(&mouse_frame);
        client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame));
        assert_eq!(
            find_selected_floating_pane_id(&client),
            Some(third_floating_pane_id)
        );

        client.apply_client_action(client_action_kind, Some(&pinned_third_pane_frame));

        assert_eq!(
            find_selected_floating_pane_id(&client),
            Some(expected_pane_id)
        );
    }
}

#[test]
fn begin_dock_undock_selects_the_focused_tiled_pane_and_enter_floats_it_once() {
    let tiled_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[build_plain_mouse_pane(tiled_pane_id)],
        Some(tiled_pane_id),
    );
    let mut client = build_client_viewing(&mouse_frame);

    assert_eq!(
        client.apply_client_action(ClientActionKind::BeginDockUndock, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.get_active_input_mode(), LockMode::DockUndock);
    assert_eq!(
        client.dock_undock_mode,
        Some(DockUndockMode {
            tiled_pane_selection: Some(TiledPaneSelection {
                pane_id: tiled_pane_id,
                pending_undock_command_id: None,
            }),
        })
    );

    let PlacementInputAction::SubmitCommand {
        command_id,
        command,
    } = client.apply_client_action(ClientActionKind::ConfirmDockUndock, Some(&mouse_frame))
    else {
        panic!("Enter on a selected tiled pane sends the undock");
    };
    assert_eq!(
        command,
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: Some(tiled_pane_id),
            placement_target: PanePlacementTarget::Floating {
                top_left_cell: None,
            },
            expected_placement_revision: None,
        })
    );
    assert_eq!(
        client.dock_undock_mode,
        Some(DockUndockMode {
            tiled_pane_selection: Some(TiledPaneSelection {
                pane_id: tiled_pane_id,
                pending_undock_command_id: Some(command_id),
            }),
        })
    );
    assert_eq!(
        client.apply_client_action(ClientActionKind::ConfirmDockUndock, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(
        client.apply_client_action(ClientActionKind::CancelDockUndock, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.get_active_input_mode(), LockMode::DockUndock);
}

#[test]
fn begin_dock_undock_on_a_focused_floating_pane_selects_the_dock_right_of_the_whole_tab() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_one_floating_pane_frame(
        tiled_pane_id,
        floating_pane_id,
        Rect::from_origin_and_size(Point { column: 10, row: 4 }, SMALL_FLOATING_PANE_SIZE),
    );
    let tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_client_viewing(&mouse_frame);

    assert_eq!(
        client.apply_client_action(ClientActionKind::BeginDockUndock, Some(&mouse_frame)),
        PlacementInputAction::ReadPlacement {
            pane_id_to_focus: None,
            source_pane_id: floating_pane_id,
            destination_tab_id: tab_id,
        }
    );
    assert_eq!(client.get_active_input_mode(), LockMode::DockUndock);
    assert_eq!(client.dock_undock_mode, Some(DockUndockMode::default()));
    assert_eq!(
        client
            .placement_state
            .placement_mode
            .as_ref()
            .map(|placement_mode| placement_mode.source_tab_id),
        Some(None)
    );
    let whole_tab_target = TiledPlacementTarget::Split {
        destination_tab_id: tab_id,
        anchor: PanePlacementAnchor::Tab,
        direction: Direction::Right,
    };
    assert_eq!(
        client.get_placement_target(),
        Some(whole_tab_target.clone())
    );
    assert!(client.is_pane_placement_visible());

    assert_eq!(
        find_submitted_command(
            client.apply_client_action(ClientActionKind::ConfirmDockUndock, Some(&mouse_frame))
        ),
        Some(Command::PlacePane(PlacePaneArgs {
            source_pane_id: Some(floating_pane_id),
            placement_target: PanePlacementTarget::Tiled(whole_tab_target),
            expected_placement_revision: Some(PlacementRevision {
                session_revision: 0,
                client_revision: 0,
            }),
        }))
    );
}

#[test]
fn begin_dock_undock_with_no_focused_pane_selects_nothing_and_esc_leaves_the_mode() {
    let mut client = build_test_client();
    client.set_frame_view(TabId::new(), None, None, Vec::new());

    assert_eq!(
        client.apply_client_action(ClientActionKind::BeginDockUndock, None),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.dock_undock_mode, Some(DockUndockMode::default()));
    assert_eq!(
        client.apply_client_action(ClientActionKind::ConfirmDockUndock, None),
        PlacementInputAction::Consumed
    );
    assert_eq!(
        client.apply_client_action(ClientActionKind::CancelDockUndock, None),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.dock_undock_mode, None);
    assert_eq!(client.get_active_input_mode(), LockMode::Normal);
}

#[test]
fn the_move_chooser_and_tiled_pane_placement_are_refused_in_dock_undock_mode() {
    let (tiled_pane_id, floating_pane_id) = (PaneId::new(), PaneId::new());
    let mouse_frame = build_one_floating_pane_frame(
        tiled_pane_id,
        floating_pane_id,
        Rect::from_origin_and_size(Point { column: 10, row: 4 }, SMALL_FLOATING_PANE_SIZE),
    );
    let mut tiled_selection_client = build_client_viewing(&mouse_frame);
    tiled_selection_client.dock_undock_mode = Some(DockUndockMode {
        tiled_pane_selection: Some(TiledPaneSelection {
            pane_id: tiled_pane_id,
            pending_undock_command_id: None,
        }),
    });
    let mut floating_selection_client = build_client_docking(
        floating_pane_id,
        mouse_frame.client_snapshot.active_tab_id,
        tiled_pane_id,
        PaneId::new(),
    );

    for client in [&mut tiled_selection_client, &mut floating_selection_client] {
        let dock_undock_mode_before = client.dock_undock_mode;
        let placement_mode_before = client.placement_state.placement_mode.clone();
        for client_action_kind in [
            ClientActionKind::BeginPanePlacement,
            ClientActionKind::BeginTiledPanePlacement,
        ] {
            assert_eq!(
                client.apply_client_action(client_action_kind, Some(&mouse_frame)),
                PlacementInputAction::Consumed
            );
            assert_eq!(client.dock_undock_mode, dock_undock_mode_before);
            assert_eq!(client.placement_state.placement_mode, placement_mode_before);
            assert_eq!(client.pane_move_mode, None);
            assert_eq!(client.get_active_input_mode(), LockMode::DockUndock);
        }
    }
}

#[test]
fn floating_pane_move_ends_dock_undock_mode_unless_the_mode_waits_for_an_undock() {
    let (tiled_pane_id, floating_pane_id) = (PaneId::new(), PaneId::new());
    let mouse_frame = build_one_floating_pane_frame(
        tiled_pane_id,
        floating_pane_id,
        Rect::from_origin_and_size(Point { column: 10, row: 4 }, SMALL_FLOATING_PANE_SIZE),
    );
    let mut client = build_client_viewing(&mouse_frame);
    let waiting_dock_undock_mode = DockUndockMode {
        tiled_pane_selection: Some(TiledPaneSelection {
            pane_id: tiled_pane_id,
            pending_undock_command_id: Some(CommandId::new()),
        }),
    };
    client.dock_undock_mode = Some(waiting_dock_undock_mode);

    assert_eq!(
        client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.dock_undock_mode, Some(waiting_dock_undock_mode));
    assert_eq!(client.pane_move_mode, None);

    client.dock_undock_mode = Some(DockUndockMode {
        tiled_pane_selection: Some(TiledPaneSelection {
            pane_id: tiled_pane_id,
            pending_undock_command_id: None,
        }),
    });
    assert_eq!(
        find_submitted_command(
            client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(&mouse_frame))
        ),
        Some(build_focus_pane_key_command(&client, floating_pane_id))
    );
    assert_eq!(client.dock_undock_mode, None);
    assert_eq!(
        find_selected_floating_pane_id(&client),
        Some(floating_pane_id)
    );
    assert_eq!(client.get_active_input_mode(), LockMode::FloatingPaneMove);
}

/// A preview of floating pane `floating_pane_id` docking into tab `tab_id`,
/// which holds `layout_tree`, with every slot where the tiled solve over the
/// test viewport puts it.
fn build_floating_source_placement_snapshot(
    session_id: SessionId,
    client_id: ClientId,
    floating_pane_id: PaneId,
    tab_id: TabId,
    layout_tree: LayoutNode,
) -> PanePlacementSnapshot {
    let mut placement_snapshot = build_solved_placement_snapshot(
        session_id,
        client_id,
        floating_pane_id,
        tab_id,
        layout_tree,
    );
    float_placement_source_pane(&mut placement_snapshot);
    placement_snapshot.client_snapshot.focused_pane_id = Some(floating_pane_id);
    placement_snapshot
}

/// A client in dock/undock mode placing floating pane `floating_pane_id` into
/// tab `tab_id` of `[left_pane_id | right_pane_id]`, with the preview retained
/// and no target selected.
fn build_client_docking(
    floating_pane_id: PaneId,
    tab_id: TabId,
    left_pane_id: PaneId,
    right_pane_id: PaneId,
) -> Client {
    let mut client = build_test_client();
    let session_id = SessionId::new();
    client.set_session_id(session_id);
    client.set_frame_view(
        tab_id,
        Some(floating_pane_id),
        Some(left_pane_id),
        vec![tab_id],
    );
    client.dock_undock_mode = Some(DockUndockMode::default());
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: floating_pane_id,
        source_tab_id: None,
        destination_tab_id: tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command: None,
    });
    client.placement_state.placement_snapshot =
        Some(Arc::new(build_floating_source_placement_snapshot(
            session_id,
            client.get_client_id(),
            floating_pane_id,
            tab_id,
            LayoutNode::Split(SplitNode::with_equal_weights(
                SplitDirection::Horizontal,
                vec![
                    LayoutNode::Pane(left_pane_id),
                    LayoutNode::Pane(right_pane_id),
                ],
            )),
        )));
    client
}

#[test]
fn the_stack_toggle_switches_a_swap_to_a_stack_onto_the_same_pane_and_back() {
    let stack_layout = StackBesidePlainPane::new();
    let mut client = stack_layout.build_client_moving(stack_layout.open_stack_pane_id);
    client.apply_client_action(ClientActionKind::SelectPaneTarget(Direction::Right), None);
    assert_eq!(
        client.get_placement_target(),
        Some(TiledPlacementTarget::Swap {
            target_pane_id: stack_layout.plain_pane_id,
        })
    );

    assert_eq!(
        client.apply_client_action(ClientActionKind::TogglePanePlacementStack, None),
        PlacementInputAction::Consumed
    );
    assert_eq!(
        client.get_placement_target(),
        Some(TiledPlacementTarget::Stack {
            target_pane_id: stack_layout.plain_pane_id,
        })
    );
    client.apply_client_action(ClientActionKind::TogglePanePlacementStack, None);
    assert_eq!(
        client.get_placement_target(),
        Some(TiledPlacementTarget::Swap {
            target_pane_id: stack_layout.plain_pane_id,
        })
    );
}

#[test]
fn the_stack_toggle_turns_an_insertion_beside_a_pane_into_a_stack_and_keeps_a_whole_tab_target() {
    let stack_layout = StackBesidePlainPane::new();
    let mut client = stack_layout.build_client_moving(stack_layout.open_stack_pane_id);
    let tab_id = client
        .placement_state
        .placement_mode
        .as_ref()
        .map(|placement_mode| placement_mode.destination_tab_id)
        .expect("placement mode is on");
    let set_placement_target = |client: &mut Client, placement_target: TiledPlacementTarget| {
        if let Some(placement_mode) = client.placement_state.placement_mode.as_mut() {
            placement_mode.placement_target = Some(placement_target);
        }
    };

    set_placement_target(
        &mut client,
        TiledPlacementTarget::Split {
            destination_tab_id: tab_id,
            anchor: PanePlacementAnchor::Pane(stack_layout.plain_pane_id),
            direction: Direction::Down,
        },
    );
    client.apply_client_action(ClientActionKind::TogglePanePlacementStack, None);
    assert_eq!(
        client.get_placement_target(),
        Some(TiledPlacementTarget::Stack {
            target_pane_id: stack_layout.plain_pane_id,
        })
    );

    let whole_tab_target = TiledPlacementTarget::Split {
        destination_tab_id: tab_id,
        anchor: PanePlacementAnchor::Tab,
        direction: Direction::Right,
    };
    set_placement_target(&mut client, whole_tab_target.clone());
    assert_eq!(
        client.apply_client_action(ClientActionKind::TogglePanePlacementStack, None),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.get_placement_target(), Some(whole_tab_target));
}

#[test]
fn a_floating_source_arrow_selects_an_insertion_and_the_stack_toggle_switches_it_back_and_forth() {
    let floating_pane_id = PaneId::new();
    let tab_id = TabId::new();
    let [left_pane_id, right_pane_id] = [PaneId::new(), PaneId::new()];
    let mut client = build_client_docking(floating_pane_id, tab_id, left_pane_id, right_pane_id);
    let insertion_right_of_right_pane = TiledPlacementTarget::Split {
        destination_tab_id: tab_id,
        anchor: PanePlacementAnchor::Pane(right_pane_id),
        direction: Direction::Right,
    };

    client.apply_client_action(ClientActionKind::SelectPaneTarget(Direction::Right), None);
    assert_eq!(
        client.get_placement_target(),
        Some(insertion_right_of_right_pane.clone())
    );
    client.apply_client_action(ClientActionKind::TogglePanePlacementStack, None);
    assert_eq!(
        client.get_placement_target(),
        Some(TiledPlacementTarget::Stack {
            target_pane_id: right_pane_id,
        })
    );
    client.apply_client_action(ClientActionKind::TogglePanePlacementStack, None);
    assert_eq!(
        client.get_placement_target(),
        Some(insertion_right_of_right_pane)
    );

    // With no target, an Arrow that reaches no pane selects the first pane.
    if let Some(placement_mode) = client.placement_state.placement_mode.as_mut() {
        placement_mode.placement_target = None;
    }
    client.apply_client_action(ClientActionKind::SelectPaneTarget(Direction::Left), None);
    assert_eq!(
        client.get_placement_target(),
        Some(TiledPlacementTarget::Split {
            destination_tab_id: tab_id,
            anchor: PanePlacementAnchor::Pane(left_pane_id),
            direction: Direction::Left,
        })
    );
}

#[test]
fn a_floating_source_never_leaves_the_layout_as_it_is() {
    let floating_pane_id = PaneId::new();
    let tab_id = TabId::new();
    let [left_pane_id, right_pane_id] = [PaneId::new(), PaneId::new()];
    let client = build_client_docking(floating_pane_id, tab_id, left_pane_id, right_pane_id);
    let placement_snapshot = client
        .placement_state
        .placement_snapshot
        .as_deref()
        .expect("the preview is retained");

    assert!(!is_same_tab_placement_noop(
        placement_snapshot,
        &TiledPlacementTarget::Stack {
            target_pane_id: left_pane_id,
        }
    ));
    assert!(!is_same_tab_placement_noop(
        placement_snapshot,
        &TiledPlacementTarget::Split {
            destination_tab_id: tab_id,
            anchor: PanePlacementAnchor::Tab,
            direction: Direction::Right,
        }
    ));
}

#[test]
fn a_floating_source_preview_of_the_active_tab_selects_no_whole_tab_target_and_another_tab_does() {
    let floating_pane_id = PaneId::new();
    let tab_id = TabId::new();
    let [left_pane_id, right_pane_id] = [PaneId::new(), PaneId::new()];
    let mut client = build_client_docking(floating_pane_id, tab_id, left_pane_id, right_pane_id);

    client.select_whole_tab_insertion_target();
    assert_eq!(client.get_placement_target(), None);
    assert!(client.is_floating_dock_selection_idle());
    assert!(!client.is_pane_placement_visible());

    // The viewer draws another tab than `tab_id`, the tab the preview shows.
    client.active_tab_id = Some(TabId::new());
    client.select_whole_tab_insertion_target();
    assert_eq!(
        client.get_placement_target(),
        Some(TiledPlacementTarget::Split {
            destination_tab_id: tab_id,
            anchor: PanePlacementAnchor::Tab,
            direction: Direction::Right,
        })
    );
    assert!(!client.is_floating_dock_selection_idle());
    assert!(client.is_pane_placement_visible());
}

#[test]
fn a_rejected_undock_keeps_the_tiled_pane_selected_and_enter_sends_it_again() {
    let tiled_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[build_plain_mouse_pane(tiled_pane_id)],
        Some(tiled_pane_id),
    );
    let mut client = build_client_viewing(&mouse_frame);
    client.apply_client_action(ClientActionKind::BeginDockUndock, Some(&mouse_frame));
    let PlacementInputAction::SubmitCommand { command_id, .. } =
        client.apply_client_action(ClientActionKind::ConfirmDockUndock, Some(&mouse_frame))
    else {
        panic!("Enter on a selected tiled pane sends the undock");
    };

    // The mode waits for the answer: Esc and a lock change leave it open.
    client.close_dock_undock_mode();
    client.set_lock_mode(LockMode::Locked);
    assert!(client.is_dock_undock_command_pending());
    assert_eq!(client.get_active_input_mode(), LockMode::DockUndock);

    assert!(!client.reject_placement_command(CommandId::new()));
    assert!(client.reject_placement_command(command_id));
    assert_eq!(
        client.dock_undock_mode,
        Some(DockUndockMode {
            tiled_pane_selection: Some(TiledPaneSelection {
                pane_id: tiled_pane_id,
                pending_undock_command_id: None,
            }),
        })
    );
    assert_eq!(
        find_submitted_command(
            client.apply_client_action(ClientActionKind::ConfirmDockUndock, Some(&mouse_frame))
        ),
        Some(Command::PlacePane(PlacePaneArgs {
            source_pane_id: Some(tiled_pane_id),
            placement_target: PanePlacementTarget::Floating {
                top_left_cell: None,
            },
            expected_placement_revision: None,
        }))
    );
}

#[test]
fn a_frame_that_floats_the_selected_tiled_pane_makes_it_the_dock_source_with_no_target() {
    let [tiled_pane_id, undocked_pane_id] = [PaneId::new(), PaneId::new()];
    let tiled_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(tiled_pane_id),
            build_plain_mouse_pane(undocked_pane_id),
        ],
        Some(undocked_pane_id),
    );
    let mut client = build_client_viewing(&tiled_frame);
    client.apply_client_action(ClientActionKind::BeginDockUndock, Some(&tiled_frame));
    let PlacementInputAction::SubmitCommand { .. } =
        client.apply_client_action(ClientActionKind::ConfirmDockUndock, Some(&tiled_frame))
    else {
        panic!("Enter on a selected tiled pane sends the undock");
    };
    client.prepare_placement_reconciliation();

    let mut floating_frame = tiled_frame.clone();
    floating_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .retain(|pane_slot| pane_slot.pane_id == tiled_pane_id);
    floating_frame
        .session_snapshot
        .floating_pane_ids
        .push(undocked_pane_id);
    floating_frame
        .session_snapshot
        .floating_pane_slots
        .push(FloatingPaneSlot::from_outer_rect(
            undocked_pane_id,
            Rect::from_origin_and_size(Point { column: 40, row: 0 }, SMALL_FLOATING_PANE_SIZE),
            false,
        ));
    floating_frame.client_snapshot.active_tab_focused_pane_id = Some(tiled_pane_id);
    client.apply_render_snapshot(&build_render_snapshot_from_mouse_frame(&floating_frame));

    assert_eq!(client.dock_undock_mode, Some(DockUndockMode::default()));
    assert_eq!(
        client.placement_state.placement_mode,
        Some(PlacementMode {
            source_pane_id: undocked_pane_id,
            source_tab_id: None,
            destination_tab_id: tiled_frame.client_snapshot.active_tab_id,
            placement_direction: Direction::Right,
            placement_target: None,
            pending_placement_command: None,
        })
    );
    assert_eq!(
        client.placement_state.placement_mode_lifetime,
        PlacementModeLifetime::UntilCancelled
    );
    assert_eq!(
        client.take_placement_preview_refresh(),
        Some((undocked_pane_id, tiled_frame.client_snapshot.active_tab_id))
    );
    assert!(client.is_floating_dock_selection_idle());
    assert!(!client.is_pane_placement_visible());
    assert_eq!(client.get_active_input_mode(), LockMode::DockUndock);
}

#[test]
fn a_frame_whose_active_tab_draws_no_slot_of_the_selected_tiled_pane_drops_the_selection() {
    let tiled_pane_id = PaneId::new();
    let tiled_frame = build_mouse_frame(
        &[build_plain_mouse_pane(tiled_pane_id)],
        Some(tiled_pane_id),
    );
    let mut client = build_client_viewing(&tiled_frame);
    client.apply_client_action(ClientActionKind::BeginDockUndock, Some(&tiled_frame));

    let other_pane_id = PaneId::new();
    let other_tab_frame = build_mouse_frame(
        &[build_plain_mouse_pane(other_pane_id)],
        Some(other_pane_id),
    );
    client.apply_render_snapshot(&build_render_snapshot_from_mouse_frame(&other_tab_frame));

    assert_eq!(client.dock_undock_mode, Some(DockUndockMode::default()));
    assert_eq!(client.placement_state.placement_mode, None);
    assert_eq!(client.get_active_input_mode(), LockMode::DockUndock);
}

#[test]
fn a_committed_dock_ends_pane_placement_and_keeps_dock_undock_mode_open() {
    let [tiled_pane_id, floating_pane_id] = [PaneId::new(), PaneId::new()];
    let mouse_frame = build_one_floating_pane_frame(
        tiled_pane_id,
        floating_pane_id,
        Rect::from_origin_and_size(Point { column: 10, row: 4 }, SMALL_FLOATING_PANE_SIZE),
    );
    let mut client = build_client_viewing(&mouse_frame);
    client
        .client_config
        .should_stay_in_pane_placement_mode_after_placement = true;
    client.apply_client_action(ClientActionKind::BeginDockUndock, Some(&mouse_frame));
    let PlacementInputAction::SubmitCommand { command_id, .. } =
        client.apply_client_action(ClientActionKind::ConfirmDockUndock, Some(&mouse_frame))
    else {
        panic!("Enter on a floating pane with a target sends the dock");
    };
    assert!(client.is_dock_undock_command_pending());

    assert!(client.note_placement_command_committed(command_id));
    client.set_placement_revisions(1, 1);

    assert_eq!(client.placement_state.placement_mode, None);
    assert_eq!(client.dock_undock_mode, Some(DockUndockMode::default()));
    assert!(!client.is_dock_undock_command_pending());
    assert_eq!(client.get_active_input_mode(), LockMode::DockUndock);
}
