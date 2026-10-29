//! Tests for the floating pane tray: what opens it and what closes it, the
//! tile its arrow keys select, the command its keys and clicks send, how a
//! new frame moves its selection or closes it, the mouse events it takes and
//! the ones it leaves to the normal path, and the keys its keymap mode binds.
//!
//! Every frame here is `80x24`: the tray button ` ⧉ ` takes `(71, 0)` to
//! `(73, 0)`, and the tray takes `(5, 1)` `69x10`, tile `i` at
//! `(7 + 11 * (i % 6), 4 + 3 * (i / 6))` `10x3` and the header label from
//! `(56, 2)`.

use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use koshi_config::conflict::KeymapVerdict;
use koshi_config::layer::PartialKeybindingsConfig;
use koshi_config::types::{ModeBindings, ModeName};
use koshi_core::action::ClientActionKind;
use koshi_core::command::{ClearSelectionArgs, FocusPaneArgs, FocusTarget, VisualCommand};
use koshi_core::ids::SessionId;
use koshi_core::key::{BindingModifierFlags, Key, KeyChord, KeySequence, NamedKey};
use koshi_core::mouse::{MouseTracking, ScrollDirection};
use koshi_renderer::snapshot::{
    FloatingPane, FloatingPaneMoveDraft, PlacementHandleHover, Reconnecting,
};

use crate::attach::tests::build_plain_chord;
use crate::input::tests::{build_client_viewing, find_submitted_command};
use crate::input::KeyOutcome;
use crate::mouse::tests::{
    add_floating_mouse_pane, build_left_mouse_press, build_mouse_event, build_mouse_frame,
    build_plain_mouse_pane, build_render_snapshot_from_mouse_frame,
};
use crate::mouse::WheelDecision;
use crate::tests::build_core_bound_action;
use crate::PaneMoveMode;

/// A frame of one focused tiled pane and one floating pane per entry of
/// `is_shown_by_tile`, in that order. The viewer shows floating pane `i`
/// when its entry is `true`, at pane-area `(2 + 10 * i, 12)` `9x6`, below the
/// tray, and minimized it otherwise. Returns the frame and the floating pane
/// ids.
pub(crate) fn build_tray_mouse_frame(is_shown_by_tile: &[bool]) -> (MouseFrame, Vec<PaneId>) {
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
                .push(FloatingPane::Minimized {
                    pane_id: floating_pane_id,
                    pane_title: None,
                });
        }
    }
    (mouse_frame, floating_pane_ids)
}

/// A client that applied `mouse_frame`, whose focused pane is tiled, and
/// opened its tray with a key, on tile `0`.
pub(crate) fn build_client_with_open_tray(mouse_frame: &MouseFrame) -> Client {
    let mut client = build_client_viewing(mouse_frame);
    client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, Some(mouse_frame));
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(0, None),
        "the tray opens on tile 0"
    );
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

/// `TogglePaneMinimized` of `pane_id`.
fn build_toggle_pane_minimized_command(pane_id: PaneId) -> Command {
    Command::TogglePaneMinimized(TogglePaneMinimizedArgs {
        pane_id: Some(pane_id),
    })
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
fn the_toggle_with_no_floating_pane_opens_the_tray_on_the_new_pane_tile() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[]);
    let mut client = build_client_viewing(&mouse_frame);

    assert_eq!(
        client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(0, None)
    );
}

#[test]
fn the_toggle_with_no_painted_frame_opens_nothing() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[]);
    let mut client = build_client_viewing(&mouse_frame);

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
    client.handle_mouse(
        build_left_mouse_press(Point {
            column: 40,
            row: 20,
        }),
        &mouse_frame,
        Instant::now(),
    );

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
fn a_viewer_action_outside_the_tray_closes_it() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true, false]);
    let mut client = build_client_with_open_tray(&mouse_frame);

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

    let mut client = build_client_viewing(&mouse_frame);
    client.set_session_id(session_id);
    client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, Some(&mouse_frame));
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

    // Six slots per row: tiles 0 to 5, then tiles 6 and 7 and the `+ new`
    // tile 8.
    for (direction, expected_selected_tile_index) in [
        (Direction::Left, 0),
        (Direction::Up, 0),
        (Direction::Right, 1),
        (Direction::Down, 7),
        (Direction::Down, 7),
        (Direction::Right, 8),
        (Direction::Right, 8),
        (Direction::Down, 8),
        (Direction::Up, 2),
        (Direction::Down, 8),
        (Direction::Up, 2),
        (Direction::Right, 3),
        (Direction::Down, 3),
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
fn activating_a_pane_tile_sends_a_minimize_toggle_for_a_shown_and_a_minimized_pane() {
    let (mouse_frame, floating_pane_ids) = build_tray_mouse_frame(&[true, false]);
    let mut client = build_client_with_open_tray(&mouse_frame);

    assert_eq!(
        find_submitted_command(client.apply_client_action(
            ClientActionKind::ActivateSelectedTrayTile,
            Some(&mouse_frame)
        )),
        Some(build_toggle_pane_minimized_command(floating_pane_ids[0]))
    );
    client.apply_client_action(
        ClientActionKind::SelectTrayTile(Direction::Right),
        Some(&mouse_frame),
    );
    assert_eq!(
        find_submitted_command(client.apply_client_action(
            ClientActionKind::ActivateSelectedTrayTile,
            Some(&mouse_frame)
        )),
        Some(build_toggle_pane_minimized_command(floating_pane_ids[1]))
    );

    client.apply_client_action(ClientActionKind::CloseFloatingPaneTray, Some(&mouse_frame));
    assert_eq!(client.floating_pane_tray, None);
    assert_eq!(
        client.apply_client_action(
            ClientActionKind::ActivateSelectedTrayTile,
            Some(&mouse_frame)
        ),
        PlacementInputAction::Consumed
    );
}

#[test]
fn a_frame_with_fewer_floating_panes_moves_the_selection_to_the_last_tile_and_keeps_the_tray_open()
{
    let (three_pane_mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true; 3]);
    let (two_pane_mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true; 2]);
    let (no_pane_mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[]);
    let tray_rect = Rect::from_origin_and_size(
        Point { column: 5, row: 1 },
        Size {
            column_count: 69,
            row_count: 10,
        },
    );
    let mut client = build_client_with_open_tray(&three_pane_mouse_frame);
    client.floating_pane_tray = build_open_floating_pane_tray(3, Some(3));

    assert_eq!(
        client
            .update_floating_pane_tray(
                &build_render_snapshot_from_mouse_frame(&two_pane_mouse_frame),
                &two_pane_mouse_frame.committed_regions,
            )
            .map(|tray_layout| tray_layout.tray_rect),
        Some(tray_rect)
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(2, None),
        "the `+ new` tile of three panes moves to the `+ new` tile of two"
    );

    assert_eq!(
        client
            .update_floating_pane_tray(
                &build_render_snapshot_from_mouse_frame(&no_pane_mouse_frame),
                &no_pane_mouse_frame.committed_regions,
            )
            .map(|tray_layout| tray_layout.tray_rect),
        Some(tray_rect)
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(0, None)
    );
}

#[test]
fn the_new_pane_tile_closes_the_tray_and_sends_the_new_floating_pane_command() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true, false]);
    let mut client = build_client_with_open_tray(&mouse_frame);
    client.apply_client_action(
        ClientActionKind::SelectTrayTile(Direction::Right),
        Some(&mouse_frame),
    );
    client.apply_client_action(
        ClientActionKind::SelectTrayTile(Direction::Right),
        Some(&mouse_frame),
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(2, None)
    );

    assert_eq!(
        find_submitted_command(client.apply_client_action(
            ClientActionKind::ActivateSelectedTrayTile,
            Some(&mouse_frame)
        )),
        Some(build_new_floating_pane_command())
    );
    assert_eq!(client.floating_pane_tray, None);
}

#[test]
fn a_click_on_the_new_pane_tile_closes_the_tray_and_sends_the_new_floating_pane_command() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true, false]);
    let mut client = build_client_with_open_tray(&mouse_frame);

    // Slot 2, the `+ new` tile, is screen columns 29 to 38 of rows 4 to 6.
    assert_eq!(
        client.handle_floating_pane_tray_mouse(
            build_left_mouse_press(Point { column: 33, row: 5 }),
            &mouse_frame
        ),
        Some(vec![MouseAction::Command(Box::new(
            build_new_floating_pane_command()
        ))])
    );
    assert_eq!(client.floating_pane_tray, None);
}

#[test]
fn the_pointer_over_the_new_pane_tile_hovers_it() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true, false]);
    let mut client = build_client_with_open_tray(&mouse_frame);

    assert_eq!(
        client.handle_floating_pane_tray_mouse(
            build_mouse_event(
                MouseKind::Motion,
                Point { column: 33, row: 5 },
                BindingModifierFlags::NONE
            ),
            &mouse_frame
        ),
        Some(Vec::new())
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(0, Some(2))
    );
}

#[test]
fn a_tray_of_twelve_floating_panes_has_no_new_pane_tile() {
    let (mouse_frame, floating_pane_ids) = build_tray_mouse_frame(&[false; 12]);
    let mut client = build_client_with_open_tray(&mouse_frame);
    client.floating_pane_tray = build_open_floating_pane_tray(11, None);

    client.apply_client_action(
        ClientActionKind::SelectTrayTile(Direction::Right),
        Some(&mouse_frame),
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(11, None)
    );
    assert_eq!(
        find_submitted_command(client.apply_client_action(
            ClientActionKind::ActivateSelectedTrayTile,
            Some(&mouse_frame)
        )),
        Some(build_toggle_pane_minimized_command(floating_pane_ids[11]))
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(11, None)
    );
}

#[test]
fn a_frame_that_fills_the_last_slot_keeps_the_selected_new_pane_tile_index_on_the_twelfth_pane() {
    let (eleven_pane_mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true; 11]);
    let (twelve_pane_mouse_frame, floating_pane_ids) = build_tray_mouse_frame(&[true; 12]);
    let mut client = build_client_with_open_tray(&eleven_pane_mouse_frame);
    client.floating_pane_tray = build_open_floating_pane_tray(11, Some(11));

    assert_eq!(
        client
            .update_floating_pane_tray(
                &build_render_snapshot_from_mouse_frame(&twelve_pane_mouse_frame),
                &twelve_pane_mouse_frame.committed_regions,
            )
            .map(|tray_layout| tray_layout.tray_rect),
        Some(Rect::from_origin_and_size(
            Point { column: 5, row: 1 },
            Size {
                column_count: 69,
                row_count: 10,
            },
        ))
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(11, Some(11))
    );
    assert_eq!(
        find_submitted_command(client.apply_client_action(
            ClientActionKind::ActivateSelectedTrayTile,
            Some(&twelve_pane_mouse_frame)
        )),
        Some(build_toggle_pane_minimized_command(floating_pane_ids[11]))
    );
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

    assert_eq!(
        client.update_floating_pane_tray(
            &short_render_snapshot,
            &CommittedRegions::build_core(short_viewport_size, 1),
        ),
        None
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
        client.handle_mouse(
            build_left_mouse_press(Point { column: 72, row: 0 }),
            &mouse_frame,
            Instant::now()
        ),
        Vec::new()
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(0, None)
    );
    assert_eq!(client.get_pending_key_sequence(), None);

    assert_eq!(
        client.handle_floating_pane_tray_mouse(
            build_left_mouse_press(Point { column: 71, row: 0 }),
            &mouse_frame
        ),
        Some(Vec::new())
    );
    assert_eq!(client.floating_pane_tray, None);
}

#[test]
fn a_closed_tray_leaves_every_mouse_event_to_the_normal_path_and_another_mode_keeps_it_closed() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true]);
    let mut client = build_client_viewing(&mouse_frame);

    for mouse_input in [
        build_left_mouse_press(Point {
            column: 40,
            row: 20,
        }),
        build_left_mouse_press(Point { column: 72, row: 0 }),
        build_mouse_event(
            MouseKind::Press(MouseButton::Right),
            Point { column: 72, row: 0 },
            BindingModifierFlags::NONE,
        ),
        build_mouse_event(
            MouseKind::Motion,
            Point { column: 72, row: 0 },
            BindingModifierFlags::NONE,
        ),
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
        client.handle_mouse(
            build_left_mouse_press(Point { column: 72, row: 0 }),
            &mouse_frame,
            Instant::now()
        ),
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
            build_mouse_event(
                MouseKind::Motion,
                Point { column: 20, row: 5 },
                BindingModifierFlags::NONE
            ),
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
            build_mouse_event(
                MouseKind::Motion,
                Point { column: 60, row: 2 },
                BindingModifierFlags::NONE
            ),
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
            build_mouse_event(
                MouseKind::Motion,
                Point {
                    column: 40,
                    row: 20
                },
                BindingModifierFlags::NONE
            ),
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
        client.handle_floating_pane_tray_mouse(
            build_left_mouse_press(Point { column: 20, row: 5 }),
            &mouse_frame
        ),
        Some(vec![MouseAction::Command(Box::new(
            build_toggle_pane_minimized_command(floating_pane_ids[1])
        ))])
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(1, None)
    );

    assert_eq!(
        client.handle_floating_pane_tray_mouse(
            build_left_mouse_press(Point { column: 10, row: 5 }),
            &mouse_frame
        ),
        Some(vec![MouseAction::Command(Box::new(
            build_toggle_pane_minimized_command(floating_pane_ids[0])
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
        client.handle_floating_pane_tray_mouse(
            build_left_mouse_press(Point { column: 60, row: 2 }),
            &mouse_frame
        ),
        Some(vec![MouseAction::Command(Box::new(
            Command::ToggleAllFloatingPanesMinimized
        ))])
    );
    assert_eq!(
        client.floating_pane_tray,
        build_open_floating_pane_tray(0, None)
    );
}

#[test]
fn a_left_press_on_the_close_button_closes_the_tray_and_sends_nothing() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true, false]);
    let mut client = build_client_with_open_tray(&mouse_frame);
    // The tray's close button `✕` is screen cell (70, 2).
    let close_button_cell = Point { column: 70, row: 2 };

    assert_eq!(
        client.handle_floating_pane_tray_mouse(
            build_mouse_event(
                MouseKind::Press(MouseButton::Right),
                close_button_cell,
                BindingModifierFlags::NONE,
            ),
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
            build_left_mouse_press(close_button_cell),
            &mouse_frame
        ),
        Some(Vec::new())
    );
    assert_eq!(client.floating_pane_tray, None);
}

#[test]
fn every_other_event_on_the_open_tray_is_consumed_and_sends_nothing() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true, false]);
    let mut client = build_client_with_open_tray(&mouse_frame);

    for mouse_input in [
        // Slot 3 shows no tile.
        build_left_mouse_press(Point { column: 44, row: 5 }),
        // The tray's left border.
        build_left_mouse_press(Point { column: 5, row: 4 }),
        build_mouse_event(
            MouseKind::Press(MouseButton::Right),
            Point { column: 20, row: 5 },
            BindingModifierFlags::NONE,
        ),
        build_mouse_event(
            MouseKind::Drag(MouseButton::Left),
            Point { column: 20, row: 5 },
            BindingModifierFlags::NONE,
        ),
        build_mouse_event(
            MouseKind::Release(MouseButton::Left),
            Point { column: 20, row: 5 },
            BindingModifierFlags::NONE,
        ),
        build_mouse_event(
            MouseKind::Scroll(ScrollDirection::Down),
            Point { column: 20, row: 5 },
            BindingModifierFlags::NONE,
        ),
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
        build_mouse_event(
            MouseKind::Scroll(ScrollDirection::Up),
            Point {
                column: 40,
                row: 20,
            },
            BindingModifierFlags::NONE,
        ),
        build_mouse_event(
            MouseKind::Release(MouseButton::Left),
            Point {
                column: 40,
                row: 20,
            },
            BindingModifierFlags::NONE,
        ),
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
    let mut tray_client = build_client_with_open_tray(&mouse_frame);
    let mut plain_client = build_client_viewing(&mouse_frame);
    // Floating pane 0's titlebar row is screen row 14, its title cells from
    // screen column 8.
    let titlebar_press = build_left_mouse_press(Point { column: 8, row: 14 });
    let press_time = Instant::now();

    assert_eq!(
        tray_client.handle_floating_pane_tray_mouse(titlebar_press, &mouse_frame),
        None
    );
    assert_eq!(tray_client.floating_pane_tray, None);
    let build_expected_mouse_actions = |client: &Client| {
        vec![MouseAction::Command(Box::new(Command::FocusPane(
            FocusPaneArgs {
                focus_target: FocusTarget::Pane(floating_pane_ids[0]),
                client_id: Some(client.client_id),
            },
        )))]
    };
    assert_eq!(
        plain_client.handle_mouse(titlebar_press, &mouse_frame, press_time),
        build_expected_mouse_actions(&plain_client)
    );
    assert_eq!(
        tray_client.handle_mouse(titlebar_press, &mouse_frame, press_time),
        build_expected_mouse_actions(&tray_client)
    );
}

#[test]
fn a_tray_closed_before_its_frame_repaints_leaves_its_cells_to_the_panes_under_them() {
    let (mut mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true]);
    let tiled_pane_id = mouse_frame
        .client_snapshot
        .focused_pane_id
        .expect("the tiled pane is focused");
    let mut client = build_client_with_open_tray(&mouse_frame);
    mouse_frame.viewer_chrome.floating_pane_tray = client.floating_pane_tray;
    client.close_floating_pane_tray();
    // Tile 0 takes (7, 4) 10x3 in the painted frame, over the tiled pane.
    let tile_cell = Point { column: 8, row: 5 };
    assert_eq!(
        resolve_hit_region(mouse_frame.build_frame_layout(), tile_cell),
        HitRegion::FloatingPaneTrayTile { tile_index: 0 }
    );
    let event_time = Instant::now();

    client.handle_mouse(
        build_mouse_event(MouseKind::Motion, tile_cell, BindingModifierFlags::NONE),
        &mouse_frame,
        event_time,
    );
    assert_eq!(client.hovered_pane_id, Some(tiled_pane_id));
    assert_eq!(
        client.handle_mouse_wheel(
            build_mouse_event(
                MouseKind::Scroll(ScrollDirection::Up),
                tile_cell,
                BindingModifierFlags::NONE
            ),
            &mouse_frame
        ),
        Some(WheelDecision {
            hovered_pane_id: Some(tiled_pane_id),
            mouse_action: Some(MouseAction::Scroll {
                pane_id: tiled_pane_id,
                is_scrolling_up: true,
                scroll_line_count: usize::from(client.client_config.mouse.scroll_line_count),
            }),
        })
    );
    assert_eq!(
        client.handle_mouse(build_left_mouse_press(tile_cell), &mouse_frame, event_time),
        vec![MouseAction::Command(Box::new(Command::Visual(
            VisualCommand::ClearSelection(ClearSelectionArgs {
                pane_id: tiled_pane_id
            })
        )))]
    );

    // The tiled pane's content starts at (1, 2), so (8, 5) is its cell (7, 3).
    mouse_frame
        .mouse_panes
        .iter_mut()
        .find(|mouse_pane| mouse_pane.pane_id == tiled_pane_id)
        .expect("the tiled pane is in the frame")
        .mouse_tracking = MouseTracking::AnyMotion;
    let mut tracking_client = build_client_viewing(&mouse_frame);
    assert_eq!(
        tracking_client.handle_mouse(build_left_mouse_press(tile_cell), &mouse_frame, event_time),
        vec![MouseAction::Forward {
            pane_id: tiled_pane_id,
            mouse_kind: MouseKind::Press(MouseButton::Left),
            modifier_flags: BindingModifierFlags::NONE,
            pane_local_cell: Point { column: 7, row: 3 },
        }]
    );
}

#[test]
fn a_dock_press_on_a_tray_closed_before_its_frame_repaints_drags_the_pane_under_it() {
    let (mut mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true]);
    let tiled_pane_id = mouse_frame
        .client_snapshot
        .focused_pane_id
        .expect("the tiled pane is focused");
    mouse_frame.viewer_chrome.floating_pane_tray = build_open_floating_pane_tray(0, None);
    let mut client = build_client_viewing(&mouse_frame);
    client.apply_client_action(ClientActionKind::BeginDockUndock, Some(&mouse_frame));
    // Tile 0 takes (7, 4) 10x3 in the painted frame, over the tiled pane,
    // which the frame draws at (0, 0).
    let tile_cell = Point { column: 8, row: 5 };

    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(tile_cell),
            &mouse_frame,
            Instant::now()
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(
        client
            .build_viewer_chrome(
                mouse_frame.client_snapshot.active_tab_id,
                &mouse_frame.committed_regions
            )
            .floating_pane_move_draft,
        Some(FloatingPaneMoveDraft {
            pane_id: tiled_pane_id,
            top_left_cell: Point { column: 0, row: 0 },
        })
    );
}

#[test]
fn a_press_on_a_placement_handle_under_a_tray_closed_before_its_frame_repaints_starts_placement() {
    let (mut mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true]);
    let tiled_pane_id = mouse_frame
        .client_snapshot
        .focused_pane_id
        .expect("the tiled pane is focused");
    let tiled_pane_slot = mouse_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter_mut()
        .find(|pane_slot| pane_slot.pane_id == tiled_pane_id)
        .expect("the tiled pane has a slot");
    tiled_pane_slot.outer_rect = Rect::from_origin_and_size(
        Point { column: 20, row: 0 },
        Size {
            column_count: 60,
            row_count: 22,
        },
    );
    tiled_pane_slot.content_rect = Some(Rect::from_origin_and_size(
        Point { column: 21, row: 1 },
        Size {
            column_count: 58,
            row_count: 20,
        },
    ));
    mouse_frame.viewer_chrome.placement_handle_hover = Some(PlacementHandleHover::TiledHandle {
        pane_id: tiled_pane_id,
    });
    mouse_frame.viewer_chrome.floating_pane_tray = build_open_floating_pane_tray(0, None);
    let mut client = build_client_viewing(&mouse_frame);
    // The tiled pane's handle takes (21, 1) to (23, 1), under the header row
    // of the painted tray.
    let handle_cell = Point { column: 22, row: 1 };
    assert_eq!(
        resolve_hit_region(mouse_frame.build_frame_layout(), handle_cell),
        HitRegion::FloatingPaneTray
    );

    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(handle_cell),
            &mouse_frame,
            Instant::now()
        ),
        Some(PlacementInputAction::FocusPane {
            pane_id: tiled_pane_id
        })
    );
}

#[test]
fn a_tray_opened_before_its_frame_repaints_leaves_the_cells_beside_it_to_the_panes_under_them() {
    let top_pane_id = PaneId::new();
    let bottom_pane_id = PaneId::new();
    let mut mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(top_pane_id),
            build_plain_mouse_pane(bottom_pane_id),
        ],
        Some(bottom_pane_id),
    );
    mouse_frame
        .session_snapshot
        .floating_panes
        .push(FloatingPane::Minimized {
            pane_id: PaneId::new(),
            pane_title: None,
        });
    mouse_frame.client_snapshot.is_mouse_selection_enabled = true;
    let mut client = build_client_with_open_tray(&mouse_frame);
    // The painted tabline ends in ` SELECT `, and a tray drawn on it takes
    // (3, 1) 69x8. The open tray's tabline ends in ` TRAY · SELECT `, and the
    // tray takes (7, 1) 58x11. (68, 3) is a content cell of the top pane.
    let beside_tray_cell = Point { column: 68, row: 3 };
    let mut painted_tray_frame_layout = mouse_frame.build_frame_layout();
    painted_tray_frame_layout.viewer_chrome.floating_pane_tray = client.floating_pane_tray;
    assert_eq!(
        resolve_hit_region(painted_tray_frame_layout, beside_tray_cell),
        HitRegion::FloatingPaneTray
    );
    let wheel_up = build_mouse_event(
        MouseKind::Scroll(ScrollDirection::Up),
        beside_tray_cell,
        BindingModifierFlags::NONE,
    );
    assert_eq!(
        client.handle_floating_pane_tray_mouse(wheel_up, &mouse_frame),
        None
    );

    assert_eq!(
        client.handle_mouse_wheel(wheel_up, &mouse_frame),
        Some(WheelDecision {
            hovered_pane_id: Some(top_pane_id),
            mouse_action: Some(MouseAction::Scroll {
                pane_id: top_pane_id,
                is_scrolling_up: true,
                scroll_line_count: usize::from(client.client_config.mouse.scroll_line_count),
            }),
        })
    );
    client.handle_mouse(
        build_mouse_event(
            MouseKind::Motion,
            beside_tray_cell,
            BindingModifierFlags::NONE,
        ),
        &mouse_frame,
        Instant::now(),
    );
    assert_eq!(client.hovered_pane_id, Some(top_pane_id));
}

#[test]
fn a_press_where_only_the_open_trays_frame_draws_the_button_closes_the_tray_and_passes_on() {
    let (mut mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[false]);
    mouse_frame.client_snapshot.is_mouse_selection_enabled = true;
    let mut client = build_client_with_open_tray(&mouse_frame);
    let floating_pane_tray = client.floating_pane_tray.expect("the tray is open");
    // The painted tabline ends in ` SELECT ` and shows the tray button at
    // (69, 0) to (71, 0). The open tray's tabline ends in ` TRAY · SELECT `
    // and puts the button at (62, 0) to (64, 0).
    let open_tray_button_cell = Point { column: 63, row: 0 };
    assert_eq!(
        resolve_hit_region(
            build_tray_frame_layout(&mouse_frame, floating_pane_tray),
            open_tray_button_cell
        ),
        HitRegion::FloatingPaneTrayButton
    );
    assert_eq!(
        resolve_hit_region(mouse_frame.build_frame_layout(), open_tray_button_cell),
        HitRegion::Tabline
    );

    assert_eq!(
        client.handle_floating_pane_tray_mouse(
            build_left_mouse_press(open_tray_button_cell),
            &mouse_frame
        ),
        None
    );
    assert_eq!(client.floating_pane_tray, None);
}

#[test]
fn the_tray_mode_resolves_its_own_keys_and_discards_the_rest() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true]);
    let mut client = build_client_with_open_tray(&mouse_frame);
    let current_time = Instant::now();

    for (key_chord, expected_key_outcome) in [
        (
            build_named_key_chord(NamedKey::Enter),
            KeyOutcome::Fire(build_core_bound_action("activate-selected-tray-tile")),
        ),
        (
            build_named_key_chord(NamedKey::Right),
            KeyOutcome::Fire(build_core_bound_action("select-tray-tile-right")),
        ),
        (
            build_plain_chord('a'),
            KeyOutcome::Fire(build_core_bound_action(
                "toggle-minimize-all-floating-panes",
            )),
        ),
        (
            build_named_key_chord(NamedKey::Esc),
            KeyOutcome::Fire(build_core_bound_action("close-floating-pane-tray")),
        ),
        (build_named_key_chord(NamedKey::Space), KeyOutcome::Discard),
        (build_plain_chord('q'), KeyOutcome::Discard),
    ] {
        assert_eq!(
            client.resolve_key(key_chord, current_time),
            expected_key_outcome,
            "{key_chord:?}"
        );
    }
}

#[test]
fn a_tile_activation_bound_to_x_fires_from_x() {
    let (mouse_frame, floating_pane_ids) = build_tray_mouse_frame(&[true]);
    let mut client = build_client_viewing(&mouse_frame);
    let mut bound_action_by_key_sequence = BTreeMap::new();
    bound_action_by_key_sequence.insert(
        KeySequence::from(build_plain_chord('x')),
        build_core_bound_action("activate-selected-tray-tile"),
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
        client.resolve_key(build_plain_chord('x'), Instant::now()),
        KeyOutcome::Fire(build_core_bound_action("activate-selected-tray-tile"))
    );
    assert_eq!(
        find_submitted_command(client.apply_client_action(
            ClientActionKind::ActivateSelectedTrayTile,
            Some(&mouse_frame)
        )),
        Some(build_toggle_pane_minimized_command(floating_pane_ids[0]))
    );
}

#[test]
fn the_anchor_is_the_cell_under_the_closed_trays_glyph_or_the_open_trays_tile() {
    let (mouse_frame, floating_pane_ids) = build_tray_mouse_frame(&[true, false]);
    let render_snapshot = build_render_snapshot_from_mouse_frame(&mouse_frame);
    let mut client = build_client_viewing(&mouse_frame);
    let glyph_cell_rect = Rect::from_origin_and_size(
        Point { column: 72, row: 0 },
        Size {
            column_count: 1,
            row_count: 1,
        },
    );

    let closed_tray_anchor = client
        .find_floating_pane_tray_anchor(&render_snapshot, &mouse_frame.committed_regions, None)
        .expect("the frame draws the tray button");
    assert_eq!(
        closed_tray_anchor,
        FloatingPaneTrayAnchor::ClosedTrayGlyphCell(glyph_cell_rect)
    );
    assert_eq!(
        closed_tray_anchor
            .find_anchor_rect(&render_snapshot.session_snapshot, floating_pane_ids[1]),
        Some(glyph_cell_rect)
    );

    client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, Some(&mouse_frame));
    let open_tray_layout = client
        .update_floating_pane_tray(&render_snapshot, &mouse_frame.committed_regions)
        .expect("the open tray fits the frame");
    assert_eq!(
        open_tray_layout.tray_rect,
        Rect::from_origin_and_size(
            Point { column: 5, row: 1 },
            Size {
                column_count: 69,
                row_count: 10,
            },
        )
    );
    let open_tray_anchor = client
        .find_floating_pane_tray_anchor(
            &render_snapshot,
            &mouse_frame.committed_regions,
            Some(open_tray_layout),
        )
        .expect("the open tray gives the anchor");
    assert_eq!(
        open_tray_anchor,
        FloatingPaneTrayAnchor::OpenTray {
            tray_layout: open_tray_layout,
            pane_area_origin: Point { column: 0, row: 1 },
        }
    );
    assert_eq!(
        open_tray_anchor.find_anchor_rect(&render_snapshot.session_snapshot, floating_pane_ids[1]),
        Some(Rect::from_origin_and_size(
            Point { column: 18, row: 3 },
            Size {
                column_count: 10,
                row_count: 3,
            },
        ))
    );
    assert_eq!(
        open_tray_anchor.find_anchor_rect(&render_snapshot.session_snapshot, PaneId::new()),
        None
    );
    assert_eq!(
        client.find_floating_pane_tray_anchor(
            &render_snapshot,
            &mouse_frame.committed_regions,
            None
        ),
        None
    );
}
