//! Tests for the floating pane tray: what opens it and what closes it, the
//! tile its arrow keys select, the command its keys and clicks send, how a
//! new frame moves its selection or closes it, the mouse events it takes and
//! the ones it leaves to the normal path, and the keys its keymap mode binds.
//!
//! Every frame here is `80x24`: the tray button ` ⧉ ` takes `(71, 0)` to
//! `(73, 0)`, and the tray takes `(17, 1)` `57x8`, tile `i` at
//! `(19 + 9 * (i % 6), 2 + 3 * (i / 6))` `8x3` and the header label from
//! `(58, 1)`.

use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use koshi_config::conflict::KeymapVerdict;
use koshi_config::layer::PartialKeybindingsConfig;
use koshi_config::types::{ModeBindings, ModeName};
use koshi_core::action::ClientActionKind;
use koshi_core::command::{FocusPaneArgs, FocusTarget};
use koshi_core::ids::{ClientId, SessionId};
use koshi_core::key::{BindingModifierFlags, Key, KeyChord, KeySequence, NamedKey};
use koshi_core::mouse::{MouseTracking, ScrollDirection};
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::snapshot::Reconnecting;

use crate::input::KeyOutcome;
use crate::mouse::tests::{
    add_floating_mouse_pane, build_mouse_frame, build_plain_mouse_pane,
    build_render_snapshot_from_mouse_frame,
};
use crate::tests::{build_core_bound_action, TEST_VIEWPORT_SIZE};
use crate::PaneMoveMode;

/// A frame of one focused tiled pane and one floating pane per entry of
/// `is_shown_by_tile`, in that order. The viewer shows floating pane `i`
/// when its entry is `true`, at pane-area `(2 + 10 * i, 12)` `9x6`, below the
/// tray, and minimized it otherwise. Returns the frame and the floating pane
/// ids.
fn build_tray_mouse_frame(is_shown_by_tile: &[bool]) -> (MouseFrame, Vec<PaneId>) {
    let tiled_pane_id = PaneId::new();
    let mut mouse_frame = build_mouse_frame(
        &[build_plain_mouse_pane(tiled_pane_id)],
        Some(tiled_pane_id),
    );
    let mut floating_pane_ids = Vec::new();
    for (tile_index, &is_shown) in (0_u16..).zip(is_shown_by_tile) {
        let floating_pane_id = PaneId::new();
        floating_pane_ids.push(floating_pane_id);
        if is_shown {
            add_floating_mouse_pane(
                &mut mouse_frame,
                build_plain_mouse_pane(floating_pane_id),
                Rect::from_origin_and_size(
                    Point {
                        column: 2 + 10 * tile_index,
                        row: 12,
                    },
                    Size {
                        column_count: 9,
                        row_count: 6,
                    },
                ),
                false,
            );
        } else {
            mouse_frame
                .session_snapshot
                .floating_panes
                .push(FloatingPane {
                    pane_id: floating_pane_id,
                    pane_title: None,
                    is_minimized: true,
                });
        }
    }
    (mouse_frame, floating_pane_ids)
}

/// A client `client_id` that recorded the session of `mouse_frame` and
/// applied `mouse_frame`.
fn build_client_viewing_with_id(mouse_frame: &MouseFrame, client_id: ClientId) -> Client {
    let mut client = Client::from_client_id_and_viewport_size(
        client_id,
        TEST_VIEWPORT_SIZE,
        TerminalCleanupGuard::new(),
    );
    client.set_session_id(mouse_frame.session_snapshot.session_id);
    client.apply_render_snapshot(&build_render_snapshot_from_mouse_frame(mouse_frame));
    client
}

/// A client that recorded the session of `mouse_frame` and applied
/// `mouse_frame`.
fn build_client_viewing(mouse_frame: &MouseFrame) -> Client {
    build_client_viewing_with_id(mouse_frame, ClientId::new())
}

/// A client that applied `mouse_frame` and opened its tray with a key.
fn build_client_with_open_tray(mouse_frame: &MouseFrame) -> Client {
    let mut client = build_client_viewing(mouse_frame);
    client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, Some(mouse_frame));
    assert!(client.floating_pane_tray.is_some(), "the tray opens");
    client
}

/// The tray state of a client whose tray is open on `selected_tile_index`,
/// with `hovered_tile_index` under the pointer.
fn build_open_floating_pane_tray(
    selected_tile_index: usize,
    hovered_tile_index: Option<usize>,
) -> Option<FloatingPaneTray> {
    Some(FloatingPaneTray {
        selected_tile_index,
        hovered_tile_index,
    })
}

/// The mouse event `mouse_kind` at screen `(column, row)` with no modifier
/// held.
fn build_mouse_input(mouse_kind: MouseKind, column: u16, row: u16) -> MouseInput {
    MouseInput {
        mouse_kind,
        position: Point { column, row },
        modifier_flags: BindingModifierFlags::NONE,
    }
}

/// A left press at screen `(column, row)`.
fn build_left_press(column: u16, row: u16) -> MouseInput {
    build_mouse_input(MouseKind::Press(MouseButton::Left), column, row)
}

/// The command `placement_input_action` sends, or `None` when it sends none.
fn find_submitted_command(placement_input_action: PlacementInputAction) -> Option<Command> {
    match placement_input_action {
        PlacementInputAction::SubmitCommand { command, .. } => Some(command),
        PlacementInputAction::FocusPane { .. }
        | PlacementInputAction::ConfirmPlacement { .. }
        | PlacementInputAction::Consumed => None,
    }
}

/// `SetPaneMinimized` of `pane_id` to `is_minimized`.
fn build_set_pane_minimized_command(pane_id: PaneId, is_minimized: bool) -> Command {
    Command::SetPaneMinimized(SetPaneMinimizedArgs {
        pane_id: Some(pane_id),
        is_minimized,
    })
}

/// `SetAllFloatingPanesMinimized` to `is_minimized`.
fn build_set_all_minimized_command(is_minimized: bool) -> Command {
    Command::SetAllFloatingPanesMinimized(SetAllFloatingPanesMinimizedArgs { is_minimized })
}

/// A redial in progress: the first attempt, the next in 2 seconds.
fn build_redialing_link_status() -> LinkStatus {
    LinkStatus::Redialing(Reconnecting {
        attempt: 1,
        retry_in_seconds: Some(2),
    })
}

/// The chord of `named_key` with no modifier held.
fn build_named_key_chord(named_key: NamedKey) -> KeyChord {
    KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(named_key))
}

/// The chord of `key_character` with no modifier held.
fn build_character_key_chord(key_character: char) -> KeyChord {
    KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char(key_character))
}

#[test]
fn the_toggle_opens_the_tray_on_the_focused_floating_panes_tile_and_closes_it_again() {
    let (mut mouse_frame, floating_pane_ids) = build_tray_mouse_frame(&[true, true, false]);
    mouse_frame.client_snapshot.focused_pane_id = Some(floating_pane_ids[1]);
    let mut client = build_client_viewing(&mouse_frame);

    assert_eq!(
        client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(1, None)
    );
    assert_eq!(client.get_active_input_mode(), LockMode::FloatingPaneTray);
    assert_eq!(client.get_lock_mode(), LockMode::Normal);

    assert_eq!(
        client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.floating_pane_tray, None);
    assert_eq!(client.get_active_input_mode(), LockMode::Normal);
}

#[test]
fn a_focused_tiled_pane_opens_the_tray_on_tile_zero() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[false, true]);

    let client = build_client_with_open_tray(&mouse_frame);

    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(0, None)
    );
}

#[test]
fn the_toggle_with_no_floating_pane_or_no_painted_frame_opens_nothing() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[]);
    let mut client = build_client_viewing(&mouse_frame);

    assert_eq!(
        client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.floating_pane_tray, None);
    assert_eq!(
        client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, None),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.floating_pane_tray, None);
}

#[test]
fn the_tray_opens_only_while_no_other_local_mode_is_on_and_the_viewer_is_linked() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true]);
    let mut client = build_client_viewing(&mouse_frame);
    client.pane_move_mode = Some(PaneMoveMode::Choice);

    client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, Some(&mouse_frame));
    assert_eq!(client.floating_pane_tray, None);
    assert_eq!(client.get_active_input_mode(), LockMode::PaneMoveChoice);

    client.pane_move_mode = None;
    client.set_link_status(build_redialing_link_status());
    client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, Some(&mouse_frame));
    assert_eq!(client.floating_pane_tray, None);
}

#[test]
fn opening_the_tray_ends_a_held_capture_and_owes_its_program_a_release() {
    let (mut mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true]);
    mouse_frame.mouse_panes[0].mouse_tracking = MouseTracking::Normal;
    let tiled_pane_id = mouse_frame.mouse_panes[0].pane_id;
    let mut client = build_client_viewing(&mouse_frame);
    // The tiled pane's content starts at screen (1, 2).
    client.handle_mouse(build_left_press(40, 20), &mouse_frame, Instant::now());

    client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, Some(&mouse_frame));

    assert_eq!(
        client.take_mouse_capture_release(),
        Some(MouseAction::Forward {
            pane_id: tiled_pane_id,
            mouse_kind: MouseKind::Release(MouseButton::Left),
            modifier_flags: BindingModifierFlags::NONE,
            pane_local_cell: Point {
                column: 39,
                row: 18
            },
        })
    );
}

#[test]
fn a_viewer_action_outside_the_tray_closes_it_and_the_minimize_all_toggle_keeps_it_open() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true, false]);
    let mut client = build_client_with_open_tray(&mouse_frame);

    assert_eq!(
        find_submitted_command(client.apply_client_action(
            ClientActionKind::ToggleMinimizeAllFloatingPanes,
            Some(&mouse_frame)
        )),
        Some(build_set_all_minimized_command(true))
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(0, None)
    );

    assert_eq!(
        client.apply_client_action(ClientActionKind::CancelDockUndock, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.floating_pane_tray, None);
}

#[test]
fn a_lock_mouse_select_focus_link_or_session_change_closes_the_tray() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true]);
    let session_id = mouse_frame.session_snapshot.session_id;
    type ClientChange = fn(&mut Client);
    let tray_closers: [(&str, ClientChange); 5] = [
        ("a lock mode change", |client| {
            client.set_lock_mode(LockMode::Locked)
        }),
        ("a mouse-select change", |client| {
            client.set_mouse_selection_enabled(true)
        }),
        ("a loss of the outer terminal's focus", |client| {
            client.handle_outer_terminal_focus_lost()
        }),
        ("a lost link", |client| {
            client.set_link_status(build_redialing_link_status())
        }),
        ("another session", |client| {
            client.set_session_id(SessionId::new())
        }),
    ];

    for (change_name, close_tray) in tray_closers {
        let mut client = build_client_with_open_tray(&mouse_frame);
        close_tray(&mut client);
        assert_eq!(client.floating_pane_tray, None, "{change_name}");
    }

    let mut client = build_client_with_open_tray(&mouse_frame);
    client.set_session_id(session_id);
    client.set_lock_mode(LockMode::Normal);
    client.set_mouse_selection_enabled(false);
    client.set_link_status(LinkStatus::Linked);
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(0, None),
        "a change to the state the viewer already holds keeps the tray open"
    );
}

#[test]
fn the_arrow_keys_move_the_selection_inside_the_grid_without_wrapping() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[false; 8]);
    let mut client = build_client_with_open_tray(&mouse_frame);

    // Six slots per row: tiles 0 to 5, then tiles 6 and 7.
    for (direction, expected_selected_tile_index) in [
        (Direction::Left, 0),
        (Direction::Up, 0),
        (Direction::Right, 1),
        (Direction::Down, 7),
        (Direction::Down, 7),
        (Direction::Right, 7),
        (Direction::Up, 1),
        (Direction::Right, 2),
        (Direction::Down, 2),
        (Direction::Right, 3),
        (Direction::Right, 4),
        (Direction::Right, 5),
        (Direction::Right, 5),
        (Direction::Down, 5),
    ] {
        assert_eq!(
            client.apply_client_action(
                ClientActionKind::SelectTrayTile(direction),
                Some(&mouse_frame)
            ),
            PlacementInputAction::Consumed
        );
        assert_eq!(
            client.floating_pane_tray,
            build_open_floating_pane_tray(expected_selected_tile_index, None),
            "{direction:?}"
        );
    }
}

#[test]
fn the_tile_toggle_minimizes_a_shown_pane_and_restores_a_minimized_one() {
    let (mouse_frame, floating_pane_ids) = build_tray_mouse_frame(&[true, false]);
    let mut client = build_client_with_open_tray(&mouse_frame);

    assert_eq!(
        find_submitted_command(
            client
                .apply_client_action(ClientActionKind::ToggleSelectedTrayTile, Some(&mouse_frame))
        ),
        Some(build_set_pane_minimized_command(floating_pane_ids[0], true))
    );
    client.apply_client_action(
        ClientActionKind::SelectTrayTile(Direction::Right),
        Some(&mouse_frame),
    );
    assert_eq!(
        find_submitted_command(
            client
                .apply_client_action(ClientActionKind::ToggleSelectedTrayTile, Some(&mouse_frame))
        ),
        Some(build_set_pane_minimized_command(
            floating_pane_ids[1],
            false
        ))
    );

    client.apply_client_action(ClientActionKind::CloseFloatingPaneTray, Some(&mouse_frame));
    assert_eq!(client.floating_pane_tray, None);
    assert_eq!(
        client.apply_client_action(ClientActionKind::ToggleSelectedTrayTile, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
}

#[test]
fn the_minimize_all_toggle_restores_only_while_every_floating_pane_is_minimized() {
    let (some_shown_mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[false, true]);
    let (none_shown_mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[false, false]);
    let (no_pane_mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[]);
    let mut client = build_client_viewing(&some_shown_mouse_frame);

    assert_eq!(
        find_submitted_command(client.apply_client_action(
            ClientActionKind::ToggleMinimizeAllFloatingPanes,
            Some(&some_shown_mouse_frame)
        )),
        Some(build_set_all_minimized_command(true))
    );
    assert_eq!(
        find_submitted_command(client.apply_client_action(
            ClientActionKind::ToggleMinimizeAllFloatingPanes,
            Some(&none_shown_mouse_frame)
        )),
        Some(build_set_all_minimized_command(false))
    );
    assert_eq!(
        client.apply_client_action(
            ClientActionKind::ToggleMinimizeAllFloatingPanes,
            Some(&no_pane_mouse_frame)
        ),
        PlacementInputAction::Consumed
    );
    assert_eq!(
        client.apply_client_action(ClientActionKind::ToggleMinimizeAllFloatingPanes, None),
        PlacementInputAction::Consumed
    );
}

#[test]
fn a_frame_with_fewer_floating_panes_moves_the_selection_and_one_with_none_closes_the_tray() {
    let (three_pane_mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true; 3]);
    let (two_pane_mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true; 2]);
    let (no_pane_mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[]);
    let mut client = build_client_with_open_tray(&three_pane_mouse_frame);
    client.floating_pane_tray = build_open_floating_pane_tray(2, Some(2));

    client.update_floating_pane_tray(
        &build_render_snapshot_from_mouse_frame(&two_pane_mouse_frame),
        &two_pane_mouse_frame.committed_regions,
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(1, None)
    );

    client.update_floating_pane_tray(
        &build_render_snapshot_from_mouse_frame(&no_pane_mouse_frame),
        &no_pane_mouse_frame.committed_regions,
    );
    assert_eq!(client.floating_pane_tray, None);
}

#[test]
fn a_frame_whose_pane_area_is_too_short_for_the_tray_closes_it() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true]);
    let mut client = build_client_with_open_tray(&mouse_frame);
    // A 9-row viewport leaves a 7-row pane area for an 8-row tray.
    let short_viewport_size = Size {
        column_count: 80,
        row_count: 9,
    };
    let mut short_render_snapshot = build_render_snapshot_from_mouse_frame(&mouse_frame);
    short_render_snapshot.client_snapshot.viewport_size = short_viewport_size;

    client.update_floating_pane_tray(
        &short_render_snapshot,
        &CommittedRegions::build_core(short_viewport_size, 1),
    );

    assert_eq!(client.floating_pane_tray, None);
}

#[test]
fn a_left_click_on_the_button_opens_the_tray_and_drops_an_open_sequence() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true]);
    let mut client = build_client_viewing(&mouse_frame);
    assert_eq!(
        client.resolve_key(
            KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('p')),
            Instant::now()
        ),
        KeyOutcome::Pending
    );

    assert_eq!(
        client.handle_mouse(build_left_press(72, 0), &mouse_frame, Instant::now()),
        Vec::new()
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(0, None)
    );
    assert_eq!(client.get_pending_key_sequence(), None);

    assert_eq!(
        client.handle_floating_pane_tray_mouse(build_left_press(71, 0), &mouse_frame),
        Some(Vec::new())
    );
    assert_eq!(client.floating_pane_tray, None);
}

#[test]
fn a_closed_tray_leaves_every_mouse_event_to_the_normal_path_and_another_mode_keeps_it_closed() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true]);
    let mut client = build_client_viewing(&mouse_frame);

    for mouse_input in [
        build_left_press(40, 20),
        build_left_press(72, 0),
        build_mouse_input(MouseKind::Press(MouseButton::Right), 72, 0),
        build_mouse_input(MouseKind::Motion, 72, 0),
    ] {
        assert_eq!(
            client.handle_floating_pane_tray_mouse(mouse_input, &mouse_frame),
            None,
            "{mouse_input:?}"
        );
    }
    assert_eq!(client.floating_pane_tray, None);

    client.pane_move_mode = Some(PaneMoveMode::Choice);
    assert_eq!(
        client.handle_mouse(build_left_press(72, 0), &mouse_frame, Instant::now()),
        Vec::new()
    );
    assert_eq!(client.floating_pane_tray, None);
}

#[test]
fn the_pointer_over_a_tile_hovers_it_and_no_pane() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true, false]);
    let tiled_pane_id = mouse_frame.mouse_panes[0].pane_id;
    let mut client = build_client_with_open_tray(&mouse_frame);
    client.hovered_pane_id = Some(tiled_pane_id);

    assert_eq!(
        client.handle_floating_pane_tray_mouse(
            build_mouse_input(MouseKind::Motion, 30, 3),
            &mouse_frame
        ),
        Some(Vec::new())
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(0, Some(1))
    );
    assert_eq!(client.hovered_pane_id, None);

    assert_eq!(
        client.handle_floating_pane_tray_mouse(
            build_mouse_input(MouseKind::Motion, 60, 1),
            &mouse_frame
        ),
        Some(Vec::new())
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(0, None)
    );

    assert_eq!(
        client.handle_floating_pane_tray_mouse(
            build_mouse_input(MouseKind::Motion, 40, 20),
            &mouse_frame
        ),
        None
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(0, None)
    );
}

#[test]
fn a_click_on_a_tile_selects_it_and_toggles_its_pane() {
    let (mouse_frame, floating_pane_ids) = build_tray_mouse_frame(&[true, false]);
    let mut client = build_client_with_open_tray(&mouse_frame);

    assert_eq!(
        client.handle_floating_pane_tray_mouse(build_left_press(30, 3), &mouse_frame),
        Some(vec![MouseAction::Command(Box::new(
            build_set_pane_minimized_command(floating_pane_ids[1], false)
        ))])
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(1, None)
    );

    assert_eq!(
        client.handle_floating_pane_tray_mouse(build_left_press(20, 3), &mouse_frame),
        Some(vec![MouseAction::Command(Box::new(
            build_set_pane_minimized_command(floating_pane_ids[0], true)
        ))])
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(0, None)
    );
}

#[test]
fn a_click_on_the_label_toggles_every_floating_pane() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true, false]);
    let mut client = build_client_with_open_tray(&mouse_frame);

    assert_eq!(
        client.handle_floating_pane_tray_mouse(build_left_press(60, 1), &mouse_frame),
        Some(vec![MouseAction::Command(Box::new(
            build_set_all_minimized_command(true)
        ))])
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(0, None)
    );
}

#[test]
fn every_other_event_on_the_open_tray_is_consumed_and_sends_nothing() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true, false]);
    let mut client = build_client_with_open_tray(&mouse_frame);

    for mouse_input in [
        // Slot 2 shows no pane.
        build_left_press(38, 3),
        // The tray's left border.
        build_left_press(17, 4),
        build_mouse_input(MouseKind::Press(MouseButton::Right), 30, 3),
        build_mouse_input(MouseKind::Drag(MouseButton::Left), 30, 3),
        build_mouse_input(MouseKind::Release(MouseButton::Left), 30, 3),
        build_mouse_input(MouseKind::Scroll(ScrollDirection::Down), 30, 3),
    ] {
        assert_eq!(
            client.handle_floating_pane_tray_mouse(mouse_input, &mouse_frame),
            Some(Vec::new()),
            "{mouse_input:?}"
        );
        assert_eq!(
            client.floating_pane_tray,
            build_open_floating_pane_tray(0, None),
            "{mouse_input:?}"
        );
    }
}

#[test]
fn a_wheel_or_a_release_off_the_open_tray_takes_the_normal_path_and_keeps_it_open() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true]);
    let mut client = build_client_with_open_tray(&mouse_frame);

    for mouse_input in [
        build_mouse_input(MouseKind::Scroll(ScrollDirection::Up), 40, 20),
        build_mouse_input(MouseKind::Release(MouseButton::Left), 40, 20),
    ] {
        assert_eq!(
            client.handle_floating_pane_tray_mouse(mouse_input, &mouse_frame),
            None,
            "{mouse_input:?}"
        );
        assert_eq!(
            client.floating_pane_tray,
            build_open_floating_pane_tray(0, None),
            "{mouse_input:?}"
        );
    }
}

#[test]
fn a_click_outside_the_open_tray_closes_it_and_acts_as_with_no_tray() {
    let (mouse_frame, floating_pane_ids) = build_tray_mouse_frame(&[true, false]);
    let client_id = ClientId::new();
    let mut tray_client = build_client_viewing_with_id(&mouse_frame, client_id);
    tray_client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, Some(&mouse_frame));
    let mut plain_client = build_client_viewing_with_id(&mouse_frame, client_id);
    // Floating pane 0's titlebar row is screen row 14, its title cells from
    // screen column 8.
    let titlebar_press = build_left_press(8, 14);
    let press_time = Instant::now();

    assert_eq!(
        tray_client.handle_floating_pane_tray_mouse(titlebar_press, &mouse_frame),
        None
    );
    assert_eq!(tray_client.floating_pane_tray, None);
    let expected_mouse_actions = vec![MouseAction::Command(Box::new(Command::FocusPane(
        FocusPaneArgs {
            focus_target: FocusTarget::Pane(floating_pane_ids[0]),
            client_id: Some(client_id),
        },
    )))];
    assert_eq!(
        plain_client.handle_mouse(titlebar_press, &mouse_frame, press_time),
        expected_mouse_actions
    );
    assert_eq!(
        tray_client.handle_mouse(titlebar_press, &mouse_frame, press_time),
        expected_mouse_actions
    );
}

#[test]
fn the_tray_mode_resolves_its_own_keys_and_discards_the_rest() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true]);
    let mut client = build_client_with_open_tray(&mouse_frame);
    let current_time = Instant::now();

    for (key_chord, expected_key_outcome) in [
        (
            build_named_key_chord(NamedKey::Enter),
            KeyOutcome::Fire(build_core_bound_action("toggle-selected-tray-tile")),
        ),
        (
            build_named_key_chord(NamedKey::Right),
            KeyOutcome::Fire(build_core_bound_action("select-tray-tile-right")),
        ),
        (
            build_character_key_chord('a'),
            KeyOutcome::Fire(build_core_bound_action(
                "toggle-minimize-all-floating-panes",
            )),
        ),
        (
            build_named_key_chord(NamedKey::Esc),
            KeyOutcome::Fire(build_core_bound_action("close-floating-pane-tray")),
        ),
        (build_named_key_chord(NamedKey::Space), KeyOutcome::Discard),
        (build_character_key_chord('q'), KeyOutcome::Discard),
    ] {
        assert_eq!(
            client.resolve_key(key_chord, current_time),
            expected_key_outcome,
            "{key_chord:?}"
        );
    }
}

#[test]
fn a_tile_toggle_bound_to_x_fires_from_x() {
    let (mouse_frame, floating_pane_ids) = build_tray_mouse_frame(&[true]);
    let mut client = build_client_viewing(&mouse_frame);
    let mut bound_action_by_key_sequence = BTreeMap::new();
    bound_action_by_key_sequence.insert(
        KeySequence::from(build_character_key_chord('x')),
        build_core_bound_action("toggle-selected-tray-tile"),
    );
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("floating-pane-tray"),
        ModeBindings {
            bound_action_by_key_sequence,
            removed_key_sequences: BTreeSet::new(),
        },
    );
    let keymap_conflict_report = client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            mode_bindings_by_name: Some(mode_bindings_by_name),
            ..PartialKeybindingsConfig::default()
        }),
    );
    assert_eq!(
        keymap_conflict_report
            .expect("a keybinding file was given")
            .get_verdict(),
        KeymapVerdict::Apply
    );
    client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, Some(&mouse_frame));

    assert_eq!(
        client.resolve_key(build_character_key_chord('x'), Instant::now()),
        KeyOutcome::Fire(build_core_bound_action("toggle-selected-tray-tile"))
    );
    assert_eq!(
        find_submitted_command(
            client
                .apply_client_action(ClientActionKind::ToggleSelectedTrayTile, Some(&mouse_frame))
        ),
        Some(build_set_pane_minimized_command(floating_pane_ids[0], true))
    );
}

#[test]
fn the_anchor_is_the_cell_under_the_closed_trays_glyph_or_the_open_trays_tile() {
    let (mouse_frame, floating_pane_ids) = build_tray_mouse_frame(&[true, false]);
    let render_snapshot = build_render_snapshot_from_mouse_frame(&mouse_frame);
    let mut client = build_client_viewing(&mouse_frame);

    assert_eq!(
        client.find_floating_pane_tray_anchor_rect(
            &render_snapshot,
            &mouse_frame.committed_regions,
            floating_pane_ids[1]
        ),
        Some(Rect::from_origin_and_size(
            Point { column: 72, row: 0 },
            Size {
                column_count: 1,
                row_count: 1,
            },
        ))
    );

    client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, Some(&mouse_frame));
    assert_eq!(
        client.find_floating_pane_tray_anchor_rect(
            &render_snapshot,
            &mouse_frame.committed_regions,
            floating_pane_ids[1]
        ),
        Some(Rect::from_origin_and_size(
            Point { column: 28, row: 1 },
            Size {
                column_count: 8,
                row_count: 3,
            },
        ))
    );
    assert_eq!(
        client.find_floating_pane_tray_anchor_rect(
            &render_snapshot,
            &mouse_frame.committed_regions,
            PaneId::new()
        ),
        None
    );
}
