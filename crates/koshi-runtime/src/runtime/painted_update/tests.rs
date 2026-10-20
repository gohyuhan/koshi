//! Tests for [`PaintedUpdateSender`] and [`PaintedUpdateReceiver`]: a first
//! update carries the JSON part and every row; a keystroke sends one row; a
//! scroll copies the rows that moved; an update that changes nothing is not
//! sent; a row rebuilt with the same bytes is kept; the JSON part travels again
//! only after it changes; a pane that returns, a resized window and the
//! alternate screen send every row again; every cursor shape travels; the
//! receiver refuses each malformed part; and a seeded run of terminal output
//! shows each client exactly what the session shows.

use koshi_core::ids::PaneId;
use koshi_core::process::PtySize;
use koshi_ipc::painted_update::{
    decode_session_binary_frame, PaintedUpdateOpening, PaintedUpdatePart, PaneCursor,
    PaneCursorShape, PaneHeader, PaneSection, PaneWindow, RowInstruction, SessionBinaryFrame,
};
use koshi_renderer::snapshot::{
    CursorSnapshot, GridView, PaneSnapshot, RenderSnapshot, ScrollbackMetadata,
};
use koshi_terminal::engine::TerminalEngine;
use koshi_terminal::row_encoding::RowDecodeError;
use koshi_terminal::state::{CursorShape, Screen};
use koshi_test_support::output_workloads::SeededSequence;

use super::*;
use crate::runtime::frame::build_wire_frame;
use crate::runtime::frame::tests::build_render_snapshot;

/// The pane `pane_id` shows for `terminal_engine` scrolled `scrollback_offset`
/// lines back, as the session's snapshot builder reads it: its cursor, rows,
/// scroll position, screen and modes, with no title, image or highlight.
fn build_engine_pane_snapshot(
    pane_id: PaneId,
    terminal_engine: &TerminalEngine,
    scrollback_offset: usize,
) -> PaneSnapshot {
    let terminal_state = terminal_engine.get_terminal_state();
    let (row_index, column_index) = terminal_state.get_active_cursor_position();
    let (row_snapshots, view_row_offset) = terminal_state.get_scrolled_view(scrollback_offset);
    let (_, column_count) = terminal_state.get_active_grid().get_grid_dimensions();
    let scrollback = terminal_state.get_scrollback();
    PaneSnapshot {
        pane_id,
        pane_title: None,
        cursor_snapshot: CursorSnapshot {
            row_index,
            column_index,
            is_visible: terminal_state.is_cursor_visible(),
            is_blinking: terminal_state.is_cursor_blink_enabled(),
            shape: terminal_state.get_cursor_shape(),
        },
        terminal_grid_view: Some(GridView {
            row_snapshots,
            column_count,
            view_row_offset,
        }),
        image_placement_snapshots: Vec::new(),
        is_reverse_video: terminal_state.is_reverse_video_enabled(),
        mouse_tracking: terminal_state.get_mouse_tracking(),
        is_alternate_scroll_enabled: terminal_state.is_alternate_scroll_enabled(),
        is_on_alternate_screen: terminal_state.get_active_screen() == Screen::Alternate,
        view_top_row_index: scrollback
            .get_total_pushed_line_count()
            .saturating_sub(view_row_offset as u64),
        selection_spans: None,
        has_selection: false,
        scrollback_metadata: ScrollbackMetadata {
            retained_line_count: scrollback.get_retained_line_count(),
        },
    }
}

/// `base_render_snapshot` with `pane_snapshots` as its panes.
fn build_render_snapshot_with_panes(
    base_render_snapshot: &RenderSnapshot,
    pane_snapshots: Vec<PaneSnapshot>,
) -> RenderSnapshot {
    RenderSnapshot {
        pane_snapshots,
        ..base_render_snapshot.clone()
    }
}

/// An engine of `column_count` × `row_count` cells that has read
/// `pty_output_bytes`.
fn build_terminal_engine(
    column_count: u16,
    row_count: u16,
    pty_output_bytes: &[u8],
) -> TerminalEngine {
    let mut terminal_engine = TerminalEngine::from_pty_size(PtySize {
        column_count,
        row_count,
    });
    feed_pty_output(&mut terminal_engine, pty_output_bytes);
    terminal_engine
}

/// Feed `pty_output_bytes` to `terminal_engine`. Panics when the output asks
/// for a reply.
fn feed_pty_output(terminal_engine: &mut TerminalEngine, pty_output_bytes: &[u8]) {
    assert_eq!(
        terminal_engine.process_pty_output(pty_output_bytes),
        Vec::<u8>::new()
    );
}

/// Build the update that turns what `painted_update_sender` sent last into
/// `render_snapshot`, decode each of its binary frames and apply them in order
/// with `painted_update_receiver`. Returns the decoded parts and the applied
/// update, or `None` when the sender sends nothing.
fn carry_painted_update(
    painted_update_sender: &mut PaintedUpdateSender,
    painted_update_receiver: &mut PaintedUpdateReceiver,
    render_snapshot: &RenderSnapshot,
    is_update_required: bool,
) -> Option<(Vec<PaintedUpdatePart>, ReceivedPaintedFrame)> {
    let part_payloads = painted_update_sender
        .build_update_frames(
            render_snapshot,
            &build_wire_frame(render_snapshot),
            is_update_required,
        )
        .expect("the painted update fits the frame cap")?;
    let mut painted_update_parts = Vec::new();
    let mut received_painted_frame = None;
    for part_payload in part_payloads {
        let SessionBinaryFrame::PaintedUpdatePart(painted_update_part) =
            decode_session_binary_frame(part_payload).expect("a sent part decodes")
        else {
            panic!("the sender sends painted update parts only");
        };
        painted_update_parts.push(painted_update_part.clone());
        received_painted_frame = painted_update_receiver
            .accept_painted_update_part(painted_update_part)
            .expect("a sent part applies");
    }
    Some((
        painted_update_parts,
        received_painted_frame.expect("the last part applies the update"),
    ))
}

/// Assert that `received_painted_frame` shows `render_snapshot`: the JSON part
/// [`build_wire_frame`] gives, and each pane's cursor, scroll position,
/// retained line count and rows.
fn assert_received_frame_shows(
    received_painted_frame: &ReceivedPaintedFrame,
    render_snapshot: &RenderSnapshot,
) {
    assert_eq!(
        *received_painted_frame.painted_frame,
        build_wire_frame(render_snapshot)
    );
    let expected_pane_views: Vec<ReceivedPaneView> = render_snapshot
        .pane_snapshots
        .iter()
        .map(|pane_snapshot| ReceivedPaneView {
            pane_id: pane_snapshot.pane_id,
            cursor_snapshot: pane_snapshot.cursor_snapshot,
            view_top_row_index: pane_snapshot.view_top_row_index,
            retained_line_count: pane_snapshot.scrollback_metadata.retained_line_count,
            terminal_grid_view: pane_snapshot.terminal_grid_view.clone(),
        })
        .collect();
    assert_eq!(received_painted_frame.pane_views, expected_pane_views);
}

/// The row instructions that send every row of `pane_snapshot` as a new row.
fn list_new_row_instructions(pane_snapshot: &PaneSnapshot) -> Vec<RowInstruction<Vec<u8>>> {
    pane_snapshot
        .terminal_grid_view
        .as_ref()
        .expect("the pane shows a window")
        .row_snapshots
        .iter()
        .map(|row_snapshot| RowInstruction::New {
            encoded_row_bytes: row_snapshot.get_encoded_row_bytes().to_vec(),
        })
        .collect()
}

/// The row instruction that sends row `row_index` of `pane_snapshot` as a new
/// row.
fn build_new_row_instruction(
    pane_snapshot: &PaneSnapshot,
    row_index: usize,
) -> RowInstruction<Vec<u8>> {
    RowInstruction::New {
        encoded_row_bytes: pane_snapshot
            .terminal_grid_view
            .as_ref()
            .expect("the pane shows a window")
            .row_snapshots[row_index]
            .get_encoded_row_bytes()
            .to_vec(),
    }
}

#[test]
fn a_first_update_carries_the_structure_and_every_row() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let pane_id = PaneId::new();
    let terminal_engine = build_terminal_engine(10, 3, b"hi");
    let render_snapshot = build_render_snapshot_with_panes(
        &base_render_snapshot,
        vec![build_engine_pane_snapshot(pane_id, &terminal_engine, 0)],
    );
    let mut painted_update_sender = PaintedUpdateSender::new();
    let mut painted_update_receiver = PaintedUpdateReceiver::new();

    let (painted_update_parts, received_painted_frame) = carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &render_snapshot,
        false,
    )
    .expect("a first update is sent");

    assert_eq!(
        painted_update_parts,
        vec![PaintedUpdatePart {
            update_opening: Some(PaintedUpdateOpening {
                update_sequence: 1,
                painted_frame: Some(Box::new(build_wire_frame(&render_snapshot))),
            }),
            is_last_part: true,
            pane_sections: vec![PaneSection {
                pane_id,
                pane_header: Some(PaneHeader {
                    cursor: PaneCursor {
                        row_index: 0,
                        column_index: 2,
                        is_visible: true,
                        is_blinking: false,
                        shape: None,
                    },
                    view_top_row_index: 0,
                    retained_line_count: 0,
                    pane_window: Some(PaneWindow {
                        column_count: 10,
                        row_count: 3,
                        view_row_offset: 0,
                        is_reset: true,
                    }),
                }),
                row_instructions: list_new_row_instructions(&render_snapshot.pane_snapshots[0]),
            }],
        }]
    );
    assert_eq!(received_painted_frame.update_sequence, 1);
    assert_received_frame_shows(&received_painted_frame, &render_snapshot);
}

#[test]
fn a_pane_with_no_window_travels_with_its_header_and_no_row() {
    let empty_pane_id = PaneId::new();
    let render_snapshot = build_render_snapshot(PaneId::new(), empty_pane_id);
    let mut painted_update_sender = PaintedUpdateSender::new();
    let mut painted_update_receiver = PaintedUpdateReceiver::new();

    let (painted_update_parts, received_painted_frame) = carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &render_snapshot,
        false,
    )
    .expect("a first update is sent");

    assert_eq!(
        painted_update_parts[0].pane_sections[1],
        PaneSection {
            pane_id: empty_pane_id,
            pane_header: Some(PaneHeader {
                cursor: PaneCursor {
                    row_index: 0,
                    column_index: 0,
                    is_visible: false,
                    is_blinking: false,
                    shape: None,
                },
                view_top_row_index: 0,
                retained_line_count: 0,
                pane_window: None,
            }),
            row_instructions: Vec::new(),
        }
    );
    assert_received_frame_shows(&received_painted_frame, &render_snapshot);
}

#[test]
fn a_keystroke_sends_the_one_row_it_changed_and_keeps_the_others() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let pane_id = PaneId::new();
    let mut terminal_engine = build_terminal_engine(10, 3, b"hi");
    let mut painted_update_sender = PaintedUpdateSender::new();
    let mut painted_update_receiver = PaintedUpdateReceiver::new();
    carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &build_render_snapshot_with_panes(
            &base_render_snapshot,
            vec![build_engine_pane_snapshot(pane_id, &terminal_engine, 0)],
        ),
        false,
    )
    .expect("a first update is sent");

    feed_pty_output(&mut terminal_engine, b"x");
    let render_snapshot = build_render_snapshot_with_panes(
        &base_render_snapshot,
        vec![build_engine_pane_snapshot(pane_id, &terminal_engine, 0)],
    );
    let (painted_update_parts, received_painted_frame) = carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &render_snapshot,
        false,
    )
    .expect("the keystroke is sent");

    assert_eq!(
        painted_update_parts,
        vec![PaintedUpdatePart {
            update_opening: Some(PaintedUpdateOpening {
                update_sequence: 2,
                painted_frame: None,
            }),
            is_last_part: true,
            pane_sections: vec![PaneSection {
                pane_id,
                pane_header: Some(PaneHeader {
                    cursor: PaneCursor {
                        row_index: 0,
                        column_index: 3,
                        is_visible: true,
                        is_blinking: false,
                        shape: None,
                    },
                    view_top_row_index: 0,
                    retained_line_count: 0,
                    pane_window: Some(PaneWindow {
                        column_count: 10,
                        row_count: 3,
                        view_row_offset: 0,
                        is_reset: false,
                    }),
                }),
                row_instructions: vec![
                    build_new_row_instruction(&render_snapshot.pane_snapshots[0], 0),
                    RowInstruction::Keep { row_count: 2 },
                ],
            }],
        }]
    );
    assert_received_frame_shows(&received_painted_frame, &render_snapshot);
}

#[test]
fn a_three_line_scroll_copies_the_two_rows_that_moved_and_sends_three_new_rows() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let pane_id = PaneId::new();
    let mut terminal_engine = build_terminal_engine(10, 5, b"1\r\n2\r\n3\r\n4\r\n5");
    let mut painted_update_sender = PaintedUpdateSender::new();
    let mut painted_update_receiver = PaintedUpdateReceiver::new();
    carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &build_render_snapshot_with_panes(
            &base_render_snapshot,
            vec![build_engine_pane_snapshot(pane_id, &terminal_engine, 0)],
        ),
        false,
    )
    .expect("a first update is sent");

    feed_pty_output(&mut terminal_engine, b"\r\n6\r\n7\r\n8");
    let render_snapshot = build_render_snapshot_with_panes(
        &base_render_snapshot,
        vec![build_engine_pane_snapshot(pane_id, &terminal_engine, 0)],
    );
    let (painted_update_parts, received_painted_frame) = carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &render_snapshot,
        false,
    )
    .expect("the scroll is sent");

    let pane_snapshot = &render_snapshot.pane_snapshots[0];
    assert_eq!(
        painted_update_parts[0].pane_sections[0].row_instructions,
        vec![
            RowInstruction::Copy {
                source_row_index: 3,
                row_count: 2,
            },
            build_new_row_instruction(pane_snapshot, 2),
            build_new_row_instruction(pane_snapshot, 3),
            build_new_row_instruction(pane_snapshot, 4),
        ]
    );
    assert_eq!(
        painted_update_parts[0].pane_sections[0]
            .pane_header
            .map(|pane_header| (
                pane_header.view_top_row_index,
                pane_header.retained_line_count
            )),
        Some((3, 3))
    );
    assert_received_frame_shows(&received_painted_frame, &render_snapshot);
}

#[test]
fn an_update_that_changes_nothing_is_not_sent_unless_it_is_required() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let pane_id = PaneId::new();
    let terminal_engine = build_terminal_engine(10, 3, b"hi");
    let render_snapshot = build_render_snapshot_with_panes(
        &base_render_snapshot,
        vec![build_engine_pane_snapshot(pane_id, &terminal_engine, 0)],
    );
    let mut painted_update_sender = PaintedUpdateSender::new();
    let mut painted_update_receiver = PaintedUpdateReceiver::new();
    carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &render_snapshot,
        false,
    )
    .expect("a first update is sent");

    assert_eq!(
        painted_update_sender
            .build_update_frames(&render_snapshot, &build_wire_frame(&render_snapshot), false)
            .expect("the painted update fits the frame cap"),
        None
    );

    let (painted_update_parts, received_painted_frame) = carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &render_snapshot,
        true,
    )
    .expect("a required update is sent");
    assert_eq!(
        painted_update_parts[0].update_opening,
        Some(PaintedUpdateOpening {
            update_sequence: 2,
            painted_frame: None,
        })
    );
    assert_eq!(
        painted_update_parts[0].pane_sections[0].row_instructions,
        vec![RowInstruction::Keep { row_count: 3 }]
    );
    assert_eq!(received_painted_frame.update_sequence, 2);
    assert_received_frame_shows(&received_painted_frame, &render_snapshot);
}

#[test]
fn a_row_rebuilt_with_the_same_bytes_is_kept() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let pane_id = PaneId::new();
    let terminal_engine = build_terminal_engine(10, 3, b"hi");
    let render_snapshot = build_render_snapshot_with_panes(
        &base_render_snapshot,
        vec![build_engine_pane_snapshot(pane_id, &terminal_engine, 0)],
    );
    let mut rebuilt_render_snapshot = render_snapshot.clone();
    let rebuilt_grid_view = rebuilt_render_snapshot.pane_snapshots[0]
        .terminal_grid_view
        .as_mut()
        .expect("the pane shows a window");
    rebuilt_grid_view.row_snapshots[0] = Arc::new(
        RowSnapshot::from_encoded_row_bytes(
            rebuilt_grid_view.row_snapshots[0]
                .get_encoded_row_bytes()
                .to_vec(),
            10,
        )
        .expect("the row's own bytes decode"),
    );
    let mut painted_update_sender = PaintedUpdateSender::new();
    let mut painted_update_receiver = PaintedUpdateReceiver::new();
    carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &render_snapshot,
        false,
    )
    .expect("a first update is sent");

    let (painted_update_parts, received_painted_frame) = carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &rebuilt_render_snapshot,
        true,
    )
    .expect("a required update is sent");

    assert_eq!(
        painted_update_parts[0].pane_sections[0].row_instructions,
        vec![RowInstruction::Keep { row_count: 3 }]
    );
    assert_received_frame_shows(&received_painted_frame, &rebuilt_render_snapshot);
}

#[test]
fn the_structure_travels_again_only_after_it_changes() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let pane_id = PaneId::new();
    let terminal_engine = build_terminal_engine(10, 3, b"hi");
    let mut render_snapshot = build_render_snapshot_with_panes(
        &base_render_snapshot,
        vec![build_engine_pane_snapshot(pane_id, &terminal_engine, 0)],
    );
    let mut painted_update_sender = PaintedUpdateSender::new();
    let mut painted_update_receiver = PaintedUpdateReceiver::new();
    carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &render_snapshot,
        false,
    )
    .expect("a first update is sent");

    render_snapshot.pane_snapshots[0].pane_title = Some(String::from("~/work"));
    let (painted_update_parts, received_painted_frame) = carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &render_snapshot,
        false,
    )
    .expect("the new title is sent");

    assert_eq!(
        painted_update_parts[0].update_opening,
        Some(PaintedUpdateOpening {
            update_sequence: 2,
            painted_frame: Some(Box::new(build_wire_frame(&render_snapshot))),
        })
    );
    assert_eq!(
        painted_update_parts[0].pane_sections[0].row_instructions,
        vec![RowInstruction::Keep { row_count: 3 }]
    );
    assert_received_frame_shows(&received_painted_frame, &render_snapshot);
    assert_eq!(
        painted_update_sender
            .build_update_frames(&render_snapshot, &build_wire_frame(&render_snapshot), false)
            .expect("the painted update fits the frame cap"),
        None
    );
}

#[test]
fn a_pane_that_leaves_and_returns_is_sent_every_row_again() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let first_terminal_engine = build_terminal_engine(10, 3, b"first");
    let second_terminal_engine = build_terminal_engine(8, 2, b"second");
    let both_panes_render_snapshot = build_render_snapshot_with_panes(
        &base_render_snapshot,
        vec![
            build_engine_pane_snapshot(first_pane_id, &first_terminal_engine, 0),
            build_engine_pane_snapshot(second_pane_id, &second_terminal_engine, 0),
        ],
    );
    let first_pane_render_snapshot = build_render_snapshot_with_panes(
        &base_render_snapshot,
        vec![build_engine_pane_snapshot(
            first_pane_id,
            &first_terminal_engine,
            0,
        )],
    );
    let mut painted_update_sender = PaintedUpdateSender::new();
    let mut painted_update_receiver = PaintedUpdateReceiver::new();
    carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &both_panes_render_snapshot,
        false,
    )
    .expect("a first update is sent");
    let (_, first_pane_received_frame) = carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &first_pane_render_snapshot,
        false,
    )
    .expect("the pane that left is sent");
    assert_received_frame_shows(&first_pane_received_frame, &first_pane_render_snapshot);

    let (painted_update_parts, received_painted_frame) = carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &both_panes_render_snapshot,
        false,
    )
    .expect("the pane that returned is sent");

    let returned_pane_section = &painted_update_parts[0].pane_sections[1];
    assert_eq!(returned_pane_section.pane_id, second_pane_id);
    assert_eq!(
        returned_pane_section
            .pane_header
            .and_then(|pane_header| pane_header.pane_window),
        Some(PaneWindow {
            column_count: 8,
            row_count: 2,
            view_row_offset: 0,
            is_reset: true,
        })
    );
    assert_eq!(
        returned_pane_section.row_instructions,
        list_new_row_instructions(&both_panes_render_snapshot.pane_snapshots[1])
    );
    assert_eq!(
        painted_update_parts[0].pane_sections[0].row_instructions,
        vec![RowInstruction::Keep { row_count: 3 }]
    );
    assert_received_frame_shows(&received_painted_frame, &both_panes_render_snapshot);
}

#[test]
fn a_resized_window_and_the_alternate_screen_send_every_row_again() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let pane_id = PaneId::new();
    let mut terminal_engine = build_terminal_engine(10, 3, b"hi");
    let mut painted_update_sender = PaintedUpdateSender::new();
    let mut painted_update_receiver = PaintedUpdateReceiver::new();
    carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &build_render_snapshot_with_panes(
            &base_render_snapshot,
            vec![build_engine_pane_snapshot(pane_id, &terminal_engine, 0)],
        ),
        false,
    )
    .expect("a first update is sent");

    terminal_engine.resize_terminal_state(PtySize {
        column_count: 12,
        row_count: 3,
    });
    let resized_render_snapshot = build_render_snapshot_with_panes(
        &base_render_snapshot,
        vec![build_engine_pane_snapshot(pane_id, &terminal_engine, 0)],
    );
    let (resized_update_parts, resized_received_frame) = carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &resized_render_snapshot,
        false,
    )
    .expect("the resize is sent");
    assert_eq!(
        resized_update_parts[0].pane_sections[0]
            .pane_header
            .and_then(|pane_header| pane_header.pane_window),
        Some(PaneWindow {
            column_count: 12,
            row_count: 3,
            view_row_offset: 0,
            is_reset: true,
        })
    );
    assert_eq!(
        resized_update_parts[0].pane_sections[0].row_instructions,
        list_new_row_instructions(&resized_render_snapshot.pane_snapshots[0])
    );
    assert_received_frame_shows(&resized_received_frame, &resized_render_snapshot);

    feed_pty_output(&mut terminal_engine, b"\x1b[?1049h");
    let alternate_render_snapshot = build_render_snapshot_with_panes(
        &base_render_snapshot,
        vec![build_engine_pane_snapshot(pane_id, &terminal_engine, 0)],
    );
    let (alternate_update_parts, alternate_received_frame) = carry_painted_update(
        &mut painted_update_sender,
        &mut painted_update_receiver,
        &alternate_render_snapshot,
        false,
    )
    .expect("the alternate screen is sent");
    assert_eq!(
        alternate_update_parts[0].pane_sections[0]
            .pane_header
            .and_then(|pane_header| pane_header.pane_window)
            .map(|pane_window| pane_window.is_reset),
        Some(true)
    );
    assert_eq!(
        alternate_update_parts[0].pane_sections[0].row_instructions,
        list_new_row_instructions(&alternate_render_snapshot.pane_snapshots[0])
    );
    assert_received_frame_shows(&alternate_received_frame, &alternate_render_snapshot);
}

#[test]
fn every_cursor_shape_and_blink_travel() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let pane_id = PaneId::new();
    let mut terminal_engine = build_terminal_engine(10, 3, b"");
    let mut painted_update_sender = PaintedUpdateSender::new();
    let mut painted_update_receiver = PaintedUpdateReceiver::new();
    for (decscusr_bytes, expected_shape, expected_cursor_shape, is_blinking) in [
        (
            &b"\x1b[1 q"[..],
            PaneCursorShape::Block,
            CursorShape::Block,
            true,
        ),
        (
            &b"\x1b[4 q"[..],
            PaneCursorShape::Underline,
            CursorShape::Underline,
            false,
        ),
        (
            &b"\x1b[5 q"[..],
            PaneCursorShape::Bar,
            CursorShape::Bar,
            true,
        ),
    ] {
        feed_pty_output(&mut terminal_engine, decscusr_bytes);
        let render_snapshot = build_render_snapshot_with_panes(
            &base_render_snapshot,
            vec![build_engine_pane_snapshot(pane_id, &terminal_engine, 0)],
        );

        let (painted_update_parts, received_painted_frame) = carry_painted_update(
            &mut painted_update_sender,
            &mut painted_update_receiver,
            &render_snapshot,
            false,
        )
        .expect("the cursor change is sent");

        assert_eq!(
            painted_update_parts[0].pane_sections[0]
                .pane_header
                .map(|pane_header| pane_header.cursor),
            Some(PaneCursor {
                row_index: 0,
                column_index: 0,
                is_visible: true,
                is_blinking,
                shape: Some(expected_shape),
            })
        );
        assert_eq!(
            received_painted_frame.pane_views[0].cursor_snapshot,
            CursorSnapshot {
                row_index: 0,
                column_index: 0,
                is_visible: true,
                is_blinking,
                shape: Some(expected_cursor_shape),
            }
        );
    }
}

/// The opening part of update `update_sequence` that carries `painted_frame`
/// and `pane_sections`, and is its last part.
fn build_whole_update_part(
    update_sequence: u64,
    painted_frame: Option<&PaintedFrame>,
    pane_sections: Vec<PaneSection>,
) -> PaintedUpdatePart {
    PaintedUpdatePart {
        update_opening: Some(PaintedUpdateOpening {
            update_sequence,
            painted_frame: painted_frame.map(|painted_frame| Box::new(painted_frame.clone())),
        }),
        is_last_part: true,
        pane_sections,
    }
}

/// The header of a pane whose window is `column_count` × `row_count` cells.
fn build_window_pane_header(column_count: u16, row_count: u16, is_reset: bool) -> PaneHeader {
    PaneHeader {
        cursor: PaneCursor {
            row_index: 0,
            column_index: 0,
            is_visible: true,
            is_blinking: false,
            shape: None,
        },
        view_top_row_index: 0,
        retained_line_count: 0,
        pane_window: Some(PaneWindow {
            column_count,
            row_count,
            view_row_offset: 0,
            is_reset,
        }),
    }
}

#[test]
fn the_receiver_refuses_every_malformed_part() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let pane_id = PaneId::new();
    let terminal_engine = build_terminal_engine(4, 2, b"ab");
    let render_snapshot = build_render_snapshot_with_panes(
        &base_render_snapshot,
        vec![build_engine_pane_snapshot(pane_id, &terminal_engine, 0)],
    );
    let painted_frame = build_wire_frame(&render_snapshot);
    let four_column_row_bytes = render_snapshot.pane_snapshots[0]
        .terminal_grid_view
        .as_ref()
        .expect("the pane shows a window")
        .row_snapshots[0]
        .get_encoded_row_bytes()
        .to_vec();
    let five_column_row_bytes = build_terminal_engine(5, 1, b"ab")
        .get_terminal_state()
        .get_active_grid()
        .get_row_snapshot(0)
        .expect("the row exists")
        .get_encoded_row_bytes()
        .to_vec();
    let build_section = |pane_header: Option<PaneHeader>,
                         row_instructions: Vec<RowInstruction<Vec<u8>>>| {
        PaneSection {
            pane_id,
            pane_header,
            row_instructions,
        }
    };
    let build_new_row = |encoded_row_bytes: &Vec<u8>| RowInstruction::New {
        encoded_row_bytes: encoded_row_bytes.clone(),
    };
    let first_update_part = build_whole_update_part(
        1,
        Some(&painted_frame),
        vec![build_section(
            Some(build_window_pane_header(4, 2, true)),
            vec![
                build_new_row(&four_column_row_bytes),
                build_new_row(&four_column_row_bytes),
            ],
        )],
    );
    let malformed_part_cases: Vec<(&str, Vec<PaintedUpdatePart>, PaintedUpdateError)> = vec![
        (
            "an update out of sequence",
            vec![build_whole_update_part(2, Some(&painted_frame), Vec::new())],
            PaintedUpdateError::UpdateOutOfSequence {
                expected_update_sequence: 1,
                received_update_sequence: 2,
            },
        ),
        (
            "a first update with no JSON part",
            vec![build_whole_update_part(1, None, Vec::new())],
            PaintedUpdateError::MissingStructure,
        ),
        (
            "a part with no update open",
            vec![PaintedUpdatePart {
                update_opening: None,
                is_last_part: true,
                pane_sections: Vec::new(),
            }],
            PaintedUpdateError::MissingUpdateOpening,
        ),
        (
            "an update that opens while another is open",
            vec![
                PaintedUpdatePart {
                    is_last_part: false,
                    ..build_whole_update_part(1, Some(&painted_frame), Vec::new())
                },
                build_whole_update_part(2, Some(&painted_frame), Vec::new()),
            ],
            PaintedUpdateError::UpdateAlreadyOpen,
        ),
        (
            "a section that goes on with no pane open",
            vec![build_whole_update_part(
                1,
                Some(&painted_frame),
                vec![build_section(None, Vec::new())],
            )],
            PaintedUpdateError::SectionContinuesNoPane { pane_id },
        ),
        (
            "a pane that is not the JSON part's next pane",
            vec![build_whole_update_part(
                1,
                Some(&painted_frame),
                vec![PaneSection {
                    pane_id: PaneId::new(),
                    pane_header: Some(build_window_pane_header(4, 2, true)),
                    row_instructions: Vec::new(),
                }],
            )],
            PaintedUpdateError::PaneListMismatch,
        ),
        (
            "an update that leaves out a pane of its JSON part",
            vec![build_whole_update_part(1, Some(&painted_frame), Vec::new())],
            PaintedUpdateError::PaneListMismatch,
        ),
        (
            "a keep on a pane the receiver holds nothing for",
            vec![build_whole_update_part(
                1,
                Some(&painted_frame),
                vec![build_section(
                    Some(build_window_pane_header(4, 2, false)),
                    vec![RowInstruction::Keep { row_count: 2 }],
                )],
            )],
            PaintedUpdateError::HeldRowOutOfRange { pane_id },
        ),
        (
            "a copy on a reset pane",
            vec![build_whole_update_part(
                1,
                Some(&painted_frame),
                vec![build_section(
                    Some(build_window_pane_header(4, 2, true)),
                    vec![RowInstruction::Copy {
                        source_row_index: 0,
                        row_count: 1,
                    }],
                )],
            )],
            PaintedUpdateError::HeldRowOutOfRange { pane_id },
        ),
        (
            "a keep on a pane whose rows the update resets",
            vec![
                first_update_part.clone(),
                build_whole_update_part(
                    2,
                    None,
                    vec![build_section(
                        Some(build_window_pane_header(4, 2, true)),
                        vec![RowInstruction::Keep { row_count: 2 }],
                    )],
                ),
            ],
            PaintedUpdateError::HeldRowOutOfRange { pane_id },
        ),
        (
            "a keep on a pane whose held rows are of another width",
            vec![
                first_update_part.clone(),
                build_whole_update_part(
                    2,
                    None,
                    vec![build_section(
                        Some(build_window_pane_header(5, 2, false)),
                        vec![RowInstruction::Keep { row_count: 2 }],
                    )],
                ),
            ],
            PaintedUpdateError::HeldRowOutOfRange { pane_id },
        ),
        (
            "rows past the window, refused at the instruction that passes it",
            vec![
                first_update_part.clone(),
                build_whole_update_part(
                    2,
                    None,
                    vec![build_section(
                        Some(build_window_pane_header(4, 2, false)),
                        vec![
                            RowInstruction::Keep { row_count: 2 },
                            RowInstruction::Copy {
                                source_row_index: 0,
                                row_count: 2,
                            },
                            RowInstruction::Copy {
                                source_row_index: 0,
                                row_count: 2,
                            },
                        ],
                    )],
                ),
            ],
            PaintedUpdateError::PaneRowCountMismatch {
                pane_id,
                expected_row_count: 2,
                received_row_count: 4,
            },
        ),
        (
            "a new row wider than its window",
            vec![build_whole_update_part(
                1,
                Some(&painted_frame),
                vec![build_section(
                    Some(build_window_pane_header(4, 2, true)),
                    vec![build_new_row(&five_column_row_bytes)],
                )],
            )],
            PaintedUpdateError::UndecodableRow {
                pane_id,
                row_decode_error: RowDecodeError::ValueOutOfRange {
                    field_name: "column_count",
                    decoded_value: 5,
                },
            },
        ),
        (
            "fewer rows than the window holds",
            vec![build_whole_update_part(
                1,
                Some(&painted_frame),
                vec![build_section(
                    Some(build_window_pane_header(4, 2, true)),
                    vec![build_new_row(&four_column_row_bytes)],
                )],
            )],
            PaintedUpdateError::PaneRowCountMismatch {
                pane_id,
                expected_row_count: 2,
                received_row_count: 1,
            },
        ),
        (
            "more rows than the window holds",
            vec![build_whole_update_part(
                1,
                Some(&painted_frame),
                vec![build_section(
                    Some(build_window_pane_header(4, 1, true)),
                    vec![
                        build_new_row(&four_column_row_bytes),
                        build_new_row(&four_column_row_bytes),
                    ],
                )],
            )],
            PaintedUpdateError::PaneRowCountMismatch {
                pane_id,
                expected_row_count: 1,
                received_row_count: 2,
            },
        ),
        (
            "rows for a pane that shows no window",
            vec![build_whole_update_part(
                1,
                Some(&painted_frame),
                vec![build_section(
                    Some(PaneHeader {
                        pane_window: None,
                        ..build_window_pane_header(4, 2, true)
                    }),
                    vec![build_new_row(&four_column_row_bytes)],
                )],
            )],
            PaintedUpdateError::RowsWithoutWindow { pane_id },
        ),
    ];

    for (case_name, painted_update_parts, expected_error) in malformed_part_cases {
        let mut painted_update_receiver = PaintedUpdateReceiver::new();
        let (last_part, earlier_parts) = painted_update_parts
            .split_last()
            .expect("every case holds a part");
        for earlier_part in earlier_parts {
            if let Err(painted_update_error) =
                painted_update_receiver.accept_painted_update_part(earlier_part.clone())
            {
                panic!("{case_name}: an earlier part applies: {painted_update_error}");
            }
        }
        assert_eq!(
            painted_update_receiver.accept_painted_update_part(last_part.clone()),
            Err(expected_error),
            "{case_name}"
        );
    }
}

#[test]
fn a_binary_frame_that_does_not_decode_is_a_malformed_frame() {
    let mut painted_update_receiver = PaintedUpdateReceiver::new();

    assert_eq!(
        painted_update_receiver
            .accept_session_frame::<()>(ReceivedFrame::Binary(Vec::new()))
            .map_err(|ipc_error| ipc_error.to_string()),
        Err(String::from(
            "ipc frame is not a readable message: the binary frame holds no kind byte"
        ))
    );
}

#[test]
fn a_binary_frame_of_an_unknown_kind_is_dropped_and_a_json_frame_passes_through() {
    let mut painted_update_receiver = PaintedUpdateReceiver::new();

    assert_eq!(
        painted_update_receiver
            .accept_session_frame::<u8>(ReceivedFrame::Binary(vec![9, 1, 2]))
            .map_err(|ipc_error| ipc_error.to_string()),
        Ok(None)
    );
    assert_eq!(
        painted_update_receiver
            .accept_session_frame(ReceivedFrame::Message(7_u8))
            .map_err(|ipc_error| ipc_error.to_string()),
        Ok(Some(SessionFrame::Event(7)))
    );
}

/// One step of terminal output for the seeded run: printable text, a line
/// break, a cursor move, an erase, a color, a wide or combined character, a
/// scroll region with inserted or deleted lines, or a switch between the
/// screens.
fn build_seeded_output_bytes(seeded_sequence: &mut SeededSequence) -> Vec<u8> {
    match seeded_sequence.compute_next_index(10) {
        0 | 1 => {
            let character_count = seeded_sequence.compute_next_in_range(1, 9);
            (0..character_count)
                .map(|_| b'a' + seeded_sequence.compute_next_index(26) as u8)
                .collect()
        }
        2 => b"\r\n".to_vec(),
        3 => format!(
            "\x1b[{};{}H",
            seeded_sequence.compute_next_in_range(1, 6),
            seeded_sequence.compute_next_in_range(1, 12)
        )
        .into_bytes(),
        4 => {
            [&b"\x1b[K"[..], b"\x1b[2J", b"\x1b[1K"][seeded_sequence.compute_next_index(3)].to_vec()
        }
        5 => format!("\x1b[3{}m", seeded_sequence.compute_next_index(8)).into_bytes(),
        6 => ["あ", "e\u{301}", "😀"][seeded_sequence.compute_next_index(3)]
            .as_bytes()
            .to_vec(),
        7 => format!(
            "\x1b[2;{}r\x1b[2H\x1b[{}{}\x1b[r",
            seeded_sequence.compute_next_in_range(3, 6),
            seeded_sequence.compute_next_in_range(1, 2),
            ["L", "M"][seeded_sequence.compute_next_index(2)]
        )
        .into_bytes(),
        8 => [&b"\x1b[?1049h"[..], b"\x1b[?1049l"][seeded_sequence.compute_next_index(2)].to_vec(),
        _ => b"\n\n\n".to_vec(),
    }
}

#[test]
fn a_seeded_run_of_output_shows_each_client_what_the_session_shows() {
    let mut copy_instruction_count = 0_usize;
    let mut keep_instruction_count = 0_usize;
    for seed in 0..40_u64 {
        let mut seeded_sequence = SeededSequence::from_seed(seed);
        let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
        let first_pane_id = PaneId::new();
        let second_pane_id = PaneId::new();
        let mut first_terminal_engine = build_terminal_engine(9, 5, b"");
        let second_terminal_engine = build_terminal_engine(6, 2, b"still");
        let mut every_frame_sender = PaintedUpdateSender::new();
        let mut every_frame_receiver = PaintedUpdateReceiver::new();
        let mut third_frame_sender = PaintedUpdateSender::new();
        let mut third_frame_receiver = PaintedUpdateReceiver::new();
        for step_index in 0..60_usize {
            match seeded_sequence.compute_next_index(12) {
                0 => first_terminal_engine.resize_terminal_state(PtySize {
                    column_count: seeded_sequence.compute_next_in_range(2, 12) as u16,
                    row_count: seeded_sequence.compute_next_in_range(2, 6) as u16,
                }),
                _ => {
                    let pty_output_bytes = build_seeded_output_bytes(&mut seeded_sequence);
                    feed_pty_output(&mut first_terminal_engine, &pty_output_bytes);
                }
            }
            let scrollback_offset = seeded_sequence.compute_next_index(4);
            let mut pane_snapshots = vec![build_engine_pane_snapshot(
                first_pane_id,
                &first_terminal_engine,
                scrollback_offset,
            )];
            if seeded_sequence.compute_next_index(3) != 0 {
                pane_snapshots.push(build_engine_pane_snapshot(
                    second_pane_id,
                    &second_terminal_engine,
                    0,
                ));
            }
            let render_snapshot =
                build_render_snapshot_with_panes(&base_render_snapshot, pane_snapshots);

            let mut carried_update_parts = Vec::new();
            if let Some((painted_update_parts, received_painted_frame)) = carry_painted_update(
                &mut every_frame_sender,
                &mut every_frame_receiver,
                &render_snapshot,
                false,
            ) {
                assert_received_frame_shows(&received_painted_frame, &render_snapshot);
                carried_update_parts.extend(painted_update_parts);
            }
            if step_index % 3 == 0 {
                let (painted_update_parts, received_painted_frame) = carry_painted_update(
                    &mut third_frame_sender,
                    &mut third_frame_receiver,
                    &render_snapshot,
                    true,
                )
                .expect("a required update is sent");
                assert_received_frame_shows(&received_painted_frame, &render_snapshot);
                carried_update_parts.extend(painted_update_parts);
            }
            for row_instruction in carried_update_parts
                .iter()
                .flat_map(|painted_update_part| &painted_update_part.pane_sections)
                .flat_map(|pane_section| &pane_section.row_instructions)
            {
                match row_instruction {
                    RowInstruction::Keep { .. } => keep_instruction_count += 1,
                    RowInstruction::Copy { .. } => copy_instruction_count += 1,
                    RowInstruction::New { .. } => {}
                }
            }
        }
    }
    assert!(copy_instruction_count > 0, "the run copies rows");
    assert!(keep_instruction_count > 0, "the run keeps rows");
}
