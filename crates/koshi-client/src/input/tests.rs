//! Tests for viewer-side key resolution: what a chord means in each mode,
//! how an open sequence captures the keyboard, and the ambiguity deadline.

use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use koshi_config::conflict::{KeymapLayer, LayerOrigin};
use koshi_config::hints::KeymapHintCatalog;
use koshi_config::types::{KeybindingsConfig, ModeBindings, ModeName};
use koshi_core::command::{PanePlacementAnchor, PanePlacementTarget};
use koshi_core::geometry::{Direction, Point, Size, SplitDirection};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::registry::ActionRegistry;
use koshi_ipc::frame::FrameSlot;
use koshi_ipc::placement::PanePlacementPaneSnapshot;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_renderer::snapshot::PaneKind;

use crate::{Client, PlacementMode, PlacementModeLifetime};

/// A viewer on the built-in keymap.
fn build_test_client() -> Client {
    crate::tests::build_test_client_with_event_sender().0
}

fn build_key_chord(modifier_flags: ModFlags, key_character: char) -> KeyChord {
    KeyChord::from_parts(modifier_flags, Key::Char(key_character))
}

/// A resolved keymap holding exactly `bindings` in `mode`, each sequence
/// paired with the core action of that name. Nothing else is bound, so a case
/// the shipped table does not hold can be set up.
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
                (
                    key_sequence.clone(),
                    BoundAction {
                        action_reference: ActionReference::from_core_action_name(action_name)
                            .expect("valid core action name"),
                        action_arguments: ActionArgs::None,
                    },
                )
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
    KeymapHintCatalog::from_parts(
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
        client.resolve_key(build_key_chord(ModFlags::NONE, 'a'), Instant::now()),
        KeyOutcome::PassThrough(build_key_chord(ModFlags::NONE, 'a'))
    );
}

#[test]
fn a_prefix_chord_opens_a_sequence_and_types_nothing() {
    // `<C-p>` is the default pane prefix: it binds nothing on its own, so it
    // holds the keyboard rather than reaching the pane.
    let mut client = build_test_client();
    assert_eq!(
        client.resolve_key(build_key_chord(ModFlags::CTRL, 'p'), Instant::now()),
        KeyOutcome::Pending
    );
    assert_eq!(
        client
            .get_pending_key_sequence()
            .map(|pending_key_sequence| pending_key_sequence.list_chords().to_vec()),
        Some(vec![build_key_chord(ModFlags::CTRL, 'p')])
    );
}

#[test]
fn completing_a_sequence_fires_its_binding_and_closes_it() {
    let mut client = build_test_client();
    let current_instant = Instant::now();
    client.resolve_key(build_key_chord(ModFlags::CTRL, 'p'), current_instant);

    let key_outcome = client.resolve_key(build_key_chord(ModFlags::NONE, 'n'), current_instant);
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
    // The viewer is inside a koshi context: a key that context cannot use goes
    // nowhere rather than surprising the program underneath.
    let mut client = build_test_client();
    let current_instant = Instant::now();
    client.resolve_key(build_key_chord(ModFlags::CTRL, 'p'), current_instant);

    assert_eq!(
        client.resolve_key(build_key_chord(ModFlags::NONE, 'z'), current_instant,),
        KeyOutcome::Pending,
        "not PassThrough — the pane must not see it"
    );
    assert_eq!(
        client
            .get_pending_key_sequence()
            .map(|pending_key_sequence| pending_key_sequence.list_chords().to_vec()),
        Some(vec![build_key_chord(ModFlags::CTRL, 'p')]),
        "the sequence is unchanged"
    );
}

#[test]
fn escape_leaves_an_open_sequence_without_typing_it() {
    let mut client = build_test_client();
    let current_instant = Instant::now();
    client.resolve_key(build_key_chord(ModFlags::CTRL, 'p'), current_instant);

    assert_eq!(
        client.resolve_key(ESCAPE_KEY_CHORD, current_instant),
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

    let outcome = client.resolve_key(KeybindingsConfig::RESERVED_UNLOCK, Instant::now());
    let KeyOutcome::Fire(bound) = outcome else {
        panic!("the reserved unlock always fires, got {outcome:?}");
    };
    assert_eq!(
        bound.action_reference,
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
    client.keymap_catalog = KeymapHintCatalog::from_parts(
        &[KeymapLayer {
            origin: LayerOrigin::Defaults,
            mode_bindings_by_name,
        }],
        &KeybindingsConfig::default(),
        &ActionRegistry::new(),
    );

    let outcome = client.resolve_key(KeybindingsConfig::RESERVED_UNLOCK, Instant::now());

    let KeyOutcome::Fire(bound) = outcome else {
        panic!("the reserved unlock always fires, got {outcome:?}");
    };
    assert_eq!(
        bound.action_reference,
        ActionReference::from_core_action_name("unlock").expect("valid name")
    );
}

#[test]
fn placement_submode_owns_the_locked_keyboard_and_restores_locked_mode() {
    let mut client = build_test_client();
    client.set_lock_mode(LockMode::Locked);
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: PaneId::new(),
        source_tab_id: TabId::new(),
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
        client.apply_client_action(ClientActionKind::CancelPanePlacement),
        PlacementInputAction::Consumed
    );
    assert!(client.placement_state.placement_mode.is_none());
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert_eq!(client.get_active_input_mode(), LockMode::Locked);
}

#[test]
fn submitted_placement_consumes_placement_keys_and_keeps_its_command() {
    let (mut client, _) = crate::tests::build_test_client_with_event_sender();
    let session_id = SessionId::new();
    let client_id = client.get_client_id();
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let placement_target = PanePlacementTarget::Split {
        destination_tab_id,
        anchor: PanePlacementAnchor::Pane(PaneId::new()),
        direction: Direction::Right,
    };
    client.set_session_id(session_id);
    client.set_frame_view(
        source_tab_id,
        Some(source_pane_id),
        vec![source_tab_id, destination_tab_id],
    );
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id,
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
                source_pane_id,
                placement_target: placement_target.clone(),
                expected_placement_revision: Some(PlacementRevision {
                    session_revision: 0,
                    client_revision: 0,
                }),
            })
        ))
    );
    let submitted_mode = client.placement_state.placement_mode.clone();

    for placement_action in [
        ClientActionKind::SelectNextPlacementTab,
        ClientActionKind::SelectPaneTarget(Direction::Right),
        ClientActionKind::CyclePanePlacementSpan,
        ClientActionKind::ConfirmPanePlacement,
    ] {
        assert_eq!(
            client.apply_client_action(placement_action),
            PlacementInputAction::Consumed
        );
        assert_eq!(client.placement_state.placement_mode, submitted_mode);
    }
    assert_eq!(client.get_placement_target(), Some(placement_target));
}

#[test]
fn submitted_mouse_placement_returns_to_the_base_mode_and_ignores_cancel() {
    let mut client = build_test_client();
    client.set_lock_mode(LockMode::Locked);
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: PaneId::new(),
        source_tab_id: TabId::new(),
        destination_tab_id: TabId::new(),
        placement_direction: Direction::Right,
        placement_target: Some(PanePlacementTarget::Swap {
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
        client.apply_client_action(ClientActionKind::CancelPanePlacement),
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
        source_tab_id: TabId::new(),
        destination_tab_id: TabId::new(),
        placement_direction: Direction::Right,
        placement_target: Some(PanePlacementTarget::Swap {
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
        client.apply_client_action(ClientActionKind::CancelPanePlacement),
        PlacementInputAction::Consumed
    );
    assert!(client.is_placement_mode_active());
    assert!(client.is_placement_confirmation_pending());
}

#[test]
fn changing_input_owner_cancels_an_unconfirmed_placement() {
    let (mut client, _) = crate::tests::build_test_client_with_event_sender();
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: PaneId::new(),
        source_tab_id: TabId::new(),
        destination_tab_id: TabId::new(),
        placement_direction: Direction::Right,
        placement_target: Some(PanePlacementTarget::Swap {
            target_pane_id: PaneId::new(),
        }),
        pending_placement_command: None,
    });

    client.set_lock_mode(LockMode::Locked);

    assert!(!client.is_placement_mode_active());
    assert!(client.get_placement_target().is_none());

    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: PaneId::new(),
        source_tab_id: TabId::new(),
        destination_tab_id: TabId::new(),
        placement_direction: Direction::Right,
        placement_target: Some(PanePlacementTarget::Swap {
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
    client.set_frame_view(tab_id, Some(initial_focused_pane_id), vec![tab_id]);
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: dragged_pane_id,
        source_tab_id: tab_id,
        destination_tab_id: tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command: None,
    });

    client.set_frame_view(tab_id, Some(dragged_pane_id), vec![tab_id]);
    assert!(client.is_placement_mode_active());

    client.set_frame_view(tab_id, Some(initial_focused_pane_id), vec![tab_id]);
    assert!(!client.is_placement_mode_active());
}

#[test]
fn a_new_frame_owner_cancels_an_unconfirmed_placement() {
    let mut client = build_test_client();
    let source_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    client.set_frame_view(source_tab_id, Some(source_pane_id), vec![source_tab_id]);
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id,
        destination_tab_id: source_tab_id,
        placement_direction: Direction::Right,
        placement_target: Some(PanePlacementTarget::Swap {
            target_pane_id: PaneId::new(),
        }),
        pending_placement_command: None,
    });

    client.set_frame_view(TabId::new(), None, Vec::new());

    assert!(!client.is_placement_mode_active());
    assert!(client.get_placement_target().is_none());
}

#[test]
fn locked_mode_still_passes_keys_it_does_not_bind() {
    // Locked mode is pass-through: that is the whole point of it.
    let mut client = build_test_client();
    client.set_lock_mode(LockMode::Locked);
    assert_eq!(
        client.resolve_key(build_key_chord(ModFlags::NONE, 'a'), Instant::now()),
        KeyOutcome::PassThrough(build_key_chord(ModFlags::NONE, 'a'))
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
        vec![active_tab_id, newer_tab_id],
    );

    assert_eq!(
        client.apply_client_action(ClientActionKind::BeginPanePlacement),
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
        vec![source_tab_id, destination_tab_id],
    );
    client.apply_client_action(ClientActionKind::BeginPanePlacement);
    assert_eq!(
        client.apply_client_action(ClientActionKind::SelectNextPlacementTab),
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
    let whole_tab_insertion_target = PanePlacementTarget::Split {
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
        client.apply_client_action(ClientActionKind::ConfirmPanePlacement),
        crate::tests::build_expected_submit_placement(
            &client,
            Command::PlacePane(PlacePaneArgs {
                source_pane_id,
                placement_target: whole_tab_insertion_target,
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
        client.apply_client_action(ClientActionKind::SelectPreviousPlacementTab),
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
        client.apply_client_action(ClientActionKind::ConfirmPanePlacement),
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
        client.apply_client_action(ClientActionKind::SelectPaneInsertion(Direction::Left)),
        PlacementInputAction::Consumed
    );

    assert_eq!(
        client.get_placement_target(),
        Some(PanePlacementTarget::Split {
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
        client.apply_client_action(ClientActionKind::SelectPaneTarget(Direction::Right)),
        PlacementInputAction::Consumed
    );

    assert_eq!(
        client.get_placement_target(),
        Some(PanePlacementTarget::Swap {
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
    let whole_tab_insertion_target = PanePlacementTarget::Split {
        destination_tab_id,
        anchor: PanePlacementAnchor::Tab,
        direction: Direction::Right,
    };
    assert!(client.accept_placement_snapshot(1, placement_snapshot.clone(), Instant::now()));
    assert_eq!(
        client.apply_client_action(ClientActionKind::ConfirmPanePlacement),
        crate::tests::build_expected_submit_placement(
            &client,
            Command::PlacePane(PlacePaneArgs {
                source_pane_id,
                placement_target: whole_tab_insertion_target.clone(),
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
    let tab_snapshot = &mut placement_snapshot.source_tab_snapshot;
    tab_snapshot.pane_slots = layout_solve
        .pane_rects
        .iter()
        .map(|&(pane_id, outer_rect)| {
            let is_visible = !collapsed_pane_ids.contains(&pane_id);
            FrameSlot {
                pane_id,
                outer_rect,
                content_rect: is_visible.then_some(outer_rect),
                pane_kind: PaneKind::Terminal,
                is_visible,
                is_suppressed: false,
                is_dead: false,
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
        client.set_frame_view(tab_id, Some(source_pane_id), vec![tab_id]);
        client.placement_state.placement_mode = Some(PlacementMode {
            source_pane_id,
            source_tab_id: tab_id,
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

    client.apply_client_action(ClientActionKind::SelectPaneTarget(Direction::Up));
    assert_eq!(
        client.get_placement_target(),
        Some(PanePlacementTarget::Swap {
            target_pane_id: stack_layout.second_stack_pane_id,
        })
    );
    client.apply_client_action(ClientActionKind::SelectPaneTarget(Direction::Up));
    assert_eq!(
        client.get_placement_target(),
        Some(PanePlacementTarget::Swap {
            target_pane_id: stack_layout.first_stack_pane_id,
        })
    );
    client.apply_client_action(ClientActionKind::SelectPaneTarget(Direction::Down));
    assert_eq!(
        client.get_placement_target(),
        Some(PanePlacementTarget::Swap {
            target_pane_id: stack_layout.second_stack_pane_id,
        })
    );
}

#[test]
fn arrow_right_from_a_stack_member_selects_the_plain_pane_and_enter_swaps_them() {
    let stack_layout = StackBesidePlainPane::new();
    let mut client = stack_layout.build_client_moving(stack_layout.open_stack_pane_id);

    client.apply_client_action(ClientActionKind::SelectPaneTarget(Direction::Right));

    assert_eq!(
        client.apply_client_action(ClientActionKind::ConfirmPanePlacement),
        crate::tests::build_expected_submit_placement(
            &client,
            Command::PlacePane(PlacePaneArgs {
                source_pane_id: stack_layout.open_stack_pane_id,
                placement_target: PanePlacementTarget::Swap {
                    target_pane_id: stack_layout.plain_pane_id,
                },
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

    client.apply_client_action(ClientActionKind::SelectPaneTarget(Direction::Left));
    assert_eq!(
        client.get_placement_target(),
        Some(PanePlacementTarget::Swap {
            target_pane_id: stack_layout.open_stack_pane_id,
        })
    );
    client.apply_client_action(ClientActionKind::SelectPaneTarget(Direction::Up));

    assert_eq!(
        client.apply_client_action(ClientActionKind::ConfirmPanePlacement),
        crate::tests::build_expected_submit_placement(
            &client,
            Command::PlacePane(PlacePaneArgs {
                source_pane_id: stack_layout.plain_pane_id,
                placement_target: PanePlacementTarget::Swap {
                    target_pane_id: stack_layout.second_stack_pane_id,
                },
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

    client.apply_client_action(ClientActionKind::SelectPaneInsertion(Direction::Left));

    assert_eq!(
        client.get_placement_target(),
        Some(PanePlacementTarget::Split {
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
    client.set_frame_view(source_tab_id, Some(source_pane_id), vec![source_tab_id]);
    client.set_lock_mode(LockMode::Locked);

    assert_eq!(
        client.resolve_key(
            KeyChord::from_parts(ModFlags::CTRL, Key::Char('p')),
            Instant::now(),
        ),
        KeyOutcome::Pending
    );
    let KeyOutcome::Fire(bound_action) = client.resolve_key(
        KeyChord::from_parts(ModFlags::NONE, Key::Char('m')),
        Instant::now(),
    ) else {
        panic!("the locked pane placement opener must fire");
    };
    assert_eq!(
        bound_action.action_reference,
        ActionReference::from_core_action_name("begin-pane-placement").expect("valid action name")
    );

    assert!(matches!(
        client.apply_client_action(ClientActionKind::BeginPanePlacement),
        PlacementInputAction::ReadPlacement {
            pane_id_to_focus: None,
            source_pane_id: actual_source_pane_id,
            destination_tab_id: actual_destination_tab_id,
        } if actual_source_pane_id == source_pane_id
            && actual_destination_tab_id == source_tab_id
    ));
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert_eq!(client.get_active_input_mode(), LockMode::PanePlacement);
}

#[test]
fn a_modal_mode_owns_the_keyboard_and_discards_what_it_does_not_bind() {
    let mut client = build_test_client();
    client.set_lock_mode(LockMode::Resize);
    assert_eq!(
        client.resolve_key(build_key_chord(ModFlags::NONE, 'a'), Instant::now()),
        KeyOutcome::Discard,
        "a modal layer never leaks a key to the pane"
    );
}

#[test]
fn changing_mode_drops_an_open_sequence() {
    // Held chords were typed at koshi; a mode change is not a request to type
    // them at the pane.
    let mut client = build_test_client();
    client.resolve_key(build_key_chord(ModFlags::CTRL, 'p'), Instant::now());
    assert_eq!(
        client.get_pending_key_sequence(),
        Some(&KeySequence::from(build_key_chord(ModFlags::CTRL, 'p')))
    );

    client.set_lock_mode(LockMode::Locked);
    assert_eq!(client.get_pending_key_sequence(), None);
}

#[test]
fn a_prefix_only_sequence_never_wakes_the_loop() {
    // Only exact-plus-longer ambiguity arms a deadline; a prefix that binds
    // nothing on its own waits for its next chord indefinitely.
    let mut client = build_test_client();
    let current_instant = Instant::now();
    client.resolve_key(build_key_chord(ModFlags::CTRL, 'p'), current_instant);

    assert_eq!(client.next_key_wakeup(current_instant), None);
    assert_eq!(client.expire_key_sequence(current_instant), None);
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
    let current_instant = Instant::now();
    let left_chord = KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Left));
    let focus_left_action =
        ActionReference::from_core_action_name("focus-pane-left").expect("valid name");

    assert_eq!(
        client.resolve_key(build_key_chord(ModFlags::CTRL, 'p'), current_instant,),
        KeyOutcome::Pending
    );

    let key_outcome = client.resolve_key(left_chord, current_instant);
    let KeyOutcome::Fire(bound_action) = key_outcome else {
        panic!("`<C-p> <Left>` fires focus-pane-left, got {key_outcome:?}");
    };
    assert_eq!(bound_action.action_reference, focus_left_action);
    assert_eq!(
        client
            .get_pending_key_sequence()
            .map(|pending_key_sequence| pending_key_sequence.list_chords().to_vec()),
        Some(vec![build_key_chord(ModFlags::CTRL, 'p')]),
        "the prefix alone is held again, not the whole sequence"
    );
    assert_eq!(
        client.next_key_wakeup(current_instant),
        None,
        "the re-opened prefix waits for its next chord with no deadline"
    );

    let key_outcome = client.resolve_key(left_chord, current_instant);
    let KeyOutcome::Fire(bound_action) = key_outcome else {
        panic!("the bare `<Left>` fires focus-pane-left again, got {key_outcome:?}");
    };
    assert_eq!(bound_action.action_reference, focus_left_action);
}

#[test]
fn placement_submode_takes_priority_over_a_normal_arrow_binding() {
    let mut client = build_test_client();
    let leader_chord = build_key_chord(ModFlags::CTRL, 'p');
    let right_chord = KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Right));
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
        source_tab_id: TabId::new(),
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
        KeyOutcome::Fire(BoundAction {
            action_reference: ActionReference::from_core_action_name("cancel-pane-placement")
                .expect("valid name"),
            action_arguments: ActionArgs::None,
        })
    );
}

#[test]
fn a_one_chord_binding_of_a_continuous_action_opens_no_prefix() {
    // Only a multi-chord sequence has a prefix to re-open. `<C-y>` on its own
    // fires `core:focus-pane-left` and leaves the keyboard to the user.
    let mut client = build_test_client();
    let ctrl_y = build_key_chord(ModFlags::CTRL, 'y');
    client.keymap_catalog =
        build_keymap_for_mode("normal", &[(KeySequence::from(ctrl_y), "focus-pane-left")]);

    let outcome = client.resolve_key(ctrl_y, Instant::now());

    let KeyOutcome::Fire(bound) = outcome else {
        panic!("`<C-y>` fires focus-pane-left, got {outcome:?}");
    };
    assert_eq!(
        bound.action_reference,
        ActionReference::from_core_action_name("focus-pane-left").expect("valid name")
    );
    assert_eq!(client.get_pending_key_sequence(), None);
}

#[test]
fn a_sequence_that_is_both_a_binding_and_a_prefix_fires_on_its_deadline() {
    // `<C-y>` binds `core:quit` and also opens `<C-y> a`. The viewer cannot
    // know which the user meant until the deadline passes, and then the
    // complete binding is the answer.
    let mut client = build_test_client();
    let ctrl_y = build_key_chord(ModFlags::CTRL, 'y');
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[
            (KeySequence::from(ctrl_y), "quit"),
            (
                KeySequence::from_first_and_rest(
                    ctrl_y,
                    vec![build_key_chord(ModFlags::NONE, 'a')],
                ),
                "new-tab",
            ),
        ],
    );
    let timeout = client.keymap_catalog.get_chord_timeout();
    let now = Instant::now();

    assert_eq!(client.resolve_key(ctrl_y, now), KeyOutcome::Pending);
    assert_eq!(
        client.next_key_wakeup(now),
        Some(timeout),
        "the ambiguity arms a deadline one chord timeout out"
    );
    assert_eq!(
        client.expire_key_sequence(now),
        None,
        "nothing fires before the deadline"
    );

    let due = now + timeout;
    let bound = client
        .expire_key_sequence(due)
        .expect("the deadline fires the complete binding");

    assert_eq!(
        bound.action_reference,
        ActionReference::from_core_action_name("quit").expect("valid name")
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        None,
        "the sequence is spent"
    );
    assert_eq!(
        client.next_key_wakeup(due),
        None,
        "and it wakes the loop no more"
    );
}

#[test]
fn the_hint_bar_follows_the_viewers_own_mode() {
    let mut client = build_test_client();
    let normal_mode_hints = client.build_frame_hints(client.get_lock_mode(), false);
    client.set_lock_mode(LockMode::Locked);
    let locked_mode_hints = client.build_frame_hints(client.get_lock_mode(), false);

    assert_ne!(
        normal_mode_hints.hint_bindings, locked_mode_hints.hint_bindings,
        "each mode shows its own bindings"
    );
    assert!(
        locked_mode_hints
            .hint_bindings
            .iter()
            .any(|hint_binding| hint_binding.is_pinned),
        "locked mode pins the unlock hint so truncation cannot drop the escape"
    );
}

#[test]
fn setting_the_mode_the_viewer_is_already_in_leaves_an_open_sequence_alone() {
    // The drop happens on a change of mode, so a report naming the mode the
    // viewer is already in leaves the chords it is holding where they are.
    let mut client = build_test_client();
    let prefix = build_key_chord(ModFlags::CTRL, 'p');
    client.resolve_key(prefix, Instant::now());

    client.set_lock_mode(LockMode::Normal);

    assert_eq!(
        client.get_pending_key_sequence(),
        Some(&KeySequence::from(prefix))
    );
}

#[test]
fn the_unlock_chord_takes_a_sequence_open_in_locked_mode_with_it() {
    // A locked-mode keymap can hold a multi-chord binding, so the unlock chord
    // can arrive with chords already held. It is resolved before the sequence
    // buffer, and the held chords go with it.
    let mut client = build_test_client();
    let prefix = build_key_chord(ModFlags::CTRL, 'p');
    client.keymap_catalog = build_keymap_for_mode(
        "locked",
        &[(
            KeySequence::from_first_and_rest(prefix, vec![build_key_chord(ModFlags::NONE, 'n')]),
            "new-pane",
        )],
    );
    client.set_lock_mode(LockMode::Locked);
    let now = Instant::now();
    assert_eq!(client.resolve_key(prefix, now), KeyOutcome::Pending);

    let outcome = client.resolve_key(client.keymap_catalog.get_unlock_chord(), now);

    let KeyOutcome::Fire(bound) = outcome else {
        panic!("the reserved unlock always fires, got {outcome:?}");
    };
    assert_eq!(
        bound.action_reference,
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
    let ctrl_y = build_key_chord(ModFlags::CTRL, 'y');
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[
            (KeySequence::from(ctrl_y), "quit"),
            (
                KeySequence::from_first_and_rest(
                    ctrl_y,
                    vec![build_key_chord(ModFlags::NONE, 'a')],
                ),
                "new-tab",
            ),
        ],
    );
    let timeout = client.keymap_catalog.get_chord_timeout();
    let now = Instant::now();
    assert_eq!(client.resolve_key(ctrl_y, now), KeyOutcome::Pending);

    assert_eq!(
        client.next_key_wakeup(now + timeout + Duration::from_secs(1)),
        Some(Duration::ZERO),
        "a deadline already behind the clock asks for no further wait"
    );
}

#[test]
fn a_keymap_that_retired_the_binding_drops_the_waiting_sequence_instead_of_firing() {
    // The deadline was armed because the sequence was itself a complete
    // binding. A keymap the user reloaded can retire it while it waits, and
    // then the held chords resolve to nothing.
    let mut client = build_test_client();
    let ctrl_y = build_key_chord(ModFlags::CTRL, 'y');
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[
            (KeySequence::from(ctrl_y), "quit"),
            (
                KeySequence::from_first_and_rest(
                    ctrl_y,
                    vec![build_key_chord(ModFlags::NONE, 'a')],
                ),
                "new-tab",
            ),
        ],
    );
    let timeout = client.keymap_catalog.get_chord_timeout();
    let now = Instant::now();
    assert_eq!(client.resolve_key(ctrl_y, now), KeyOutcome::Pending);

    client.keymap_catalog = build_keymap_for_mode("normal", &[]);

    assert_eq!(client.expire_key_sequence(now + timeout), None);
    assert_eq!(
        client.get_pending_key_sequence(),
        None,
        "the held chord is dropped, never typed at the pane"
    );
}

#[test]
fn a_three_chord_binding_fires_only_on_its_third_chord() {
    let mut client = build_test_client();
    let ctrl_y = build_key_chord(ModFlags::CTRL, 'y');
    let first_following_chord = build_key_chord(ModFlags::NONE, 'a');
    let second_following_chord = build_key_chord(ModFlags::NONE, 'b');
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[(
            KeySequence::from_first_and_rest(
                ctrl_y,
                vec![first_following_chord, second_following_chord],
            ),
            "quit",
        )],
    );
    let now = Instant::now();

    assert_eq!(client.resolve_key(ctrl_y, now), KeyOutcome::Pending);
    assert_eq!(
        client.resolve_key(first_following_chord, now),
        KeyOutcome::Pending
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        Some(&KeySequence::from_first_and_rest(
            ctrl_y,
            vec![first_following_chord],
        )),
        "both chords are held, in the order they were typed"
    );

    let outcome = client.resolve_key(second_following_chord, now);

    let KeyOutcome::Fire(bound) = outcome else {
        panic!("`<C-y> a b` fires quit, got {outcome:?}");
    };
    assert_eq!(
        bound.action_reference,
        ActionReference::from_core_action_name("quit").expect("valid name")
    );
    assert_eq!(client.get_pending_key_sequence(), None);
}

#[test]
fn a_continuous_three_chord_binding_re_opens_its_two_chord_prefix() {
    // The prefix that comes back is everything but the last chord, so the last
    // chord alone repeats the action.
    let mut client = build_test_client();
    let ctrl_y = build_key_chord(ModFlags::CTRL, 'y');
    let first_following_chord = build_key_chord(ModFlags::NONE, 'a');
    let second_following_chord = build_key_chord(ModFlags::NONE, 'b');
    let focus_left = ActionReference::from_core_action_name("focus-pane-left").expect("valid name");
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[(
            KeySequence::from_first_and_rest(
                ctrl_y,
                vec![first_following_chord, second_following_chord],
            ),
            "focus-pane-left",
        )],
    );
    let now = Instant::now();
    client.resolve_key(ctrl_y, now);
    client.resolve_key(first_following_chord, now);

    let outcome = client.resolve_key(second_following_chord, now);

    let KeyOutcome::Fire(bound) = outcome else {
        panic!("`<C-y> a b` fires focus-pane-left, got {outcome:?}");
    };
    assert_eq!(bound.action_reference, focus_left);
    assert_eq!(
        client.get_pending_key_sequence(),
        Some(&KeySequence::from_first_and_rest(
            ctrl_y,
            vec![first_following_chord],
        )),
        "the two-chord prefix is held again, not the whole sequence"
    );
    assert_eq!(client.next_key_wakeup(now), None);

    let outcome = client.resolve_key(second_following_chord, now);

    let KeyOutcome::Fire(bound) = outcome else {
        panic!("the bare `b` fires focus-pane-left again, got {outcome:?}");
    };
    assert_eq!(bound.action_reference, focus_left);
}
