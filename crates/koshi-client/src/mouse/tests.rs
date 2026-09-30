//! Tests for what the client decides a mouse event means: the wheel's
//! precedence over one pane, which pane a tick targets, and the ticks that
//! decide nothing; what a press, a drag and a release do over each region —
//! focusing a tab or a pane, peeking the tab strip, moving a border, and
//! highlighting text; the run of clicks that picks a highlight's shape; the
//! capture that keeps a gesture with the pane its press landed on; plain pane
//! drags swap, Shift-pane drags insert, edge scroll a held text drag drives on
//! the clock; and the gestures a new frame ends.
//!
//! Each test builds a frame by hand — one tab, one or two panes, no chrome
//! beyond the tabline and hint bar.

use super::*;

use std::sync::Arc;

use koshi_config::layer::{PartialKoshiConfig, PartialMouseConfig};
use koshi_config::types::WheelScroll;
use koshi_core::event::{Event, MouseSelectChanged};
use koshi_core::geometry::{Point, Rect, Size, SplitDirection};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::key::BindingModifierFlags;
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseTracking};
use koshi_ipc::placement::{
    PanePlacementPaneSnapshot, PanePlacementSnapshot, PanePlacementTabSnapshot,
};
use koshi_layout::mode::LayoutMode;
use koshi_layout::regions::{solve_region_rects, Edge, RegionGeometry};
use koshi_layout::solver::{solve_layout_with_mode, PaneSizing, StackHeader};
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_renderer::snapshot::{
    ClientSnapshot, CommittedRegions, Delivery, MousePane, PaneSlot, SessionSnapshot, TabMetadata,
    TabSnapshot,
};

use crate::tests::TEST_VIEWPORT_SIZE;
use crate::{Client, PlacementMode, PlacementModeLifetime};

/// A client on the stock settings — `scroll_line_count` 3, `wheel` scroll-scrollback.
fn build_test_client() -> Client {
    crate::tests::build_test_client_with_event_sender().0
}

/// A client whose mouse-select mode the session turned on, delivered as a
/// `MouseSelectChanged` event on the client's own event queue.
fn build_mouse_select_client() -> Client {
    let (mut client, event_sender) = crate::tests::build_test_client_with_event_sender();
    event_sender
        .send(Delivery::Event(Event::MouseSelectChanged(
            MouseSelectChanged {
                client_id: client.get_client_id(),
                is_enabled: true,
            },
        )))
        .expect("the client's queue has room");
    client.apply_events();
    client
}

/// A stock client with its `mouse` settings overridden by `mouse_config`.
fn build_test_client_with_mouse_config(mouse_config: PartialMouseConfig) -> Client {
    let mut client = build_test_client();
    client.load_startup_config(
        Some(PartialKoshiConfig {
            mouse: Some(mouse_config),
            ..PartialKoshiConfig::default()
        }),
        None,
        None,
    );
    client
}

/// One pane in a fixture frame: a plain terminal pane, no highlight, no mouse
/// mode, on the primary screen.
fn build_plain_mouse_pane(pane_id: PaneId) -> MousePane {
    MousePane {
        pane_id,
        view_top_row_index: 0,
        mouse_tracking: MouseTracking::Off,
        is_alternate_scroll_enabled: false,
        is_on_alternate_screen: false,
        has_selection: false,
    }
}

/// A frame of `mouse_panes`, laid out as full-width horizontal bands between the
/// tabline (row 0) and the hint bar (last row), with `focused_pane_id` focused.
///
/// Two panes in an 80x24 viewport gives band rows 1..=11 and 12..=22, each with
/// a one-cell border ring, so `get_content_cell(&mouse_frame, 0)` lands inside the
/// first pane's content and `get_content_cell(&mouse_frame, 1)` inside the second's.
fn build_mouse_frame(mouse_panes: &[MousePane], focused_pane_id: Option<PaneId>) -> MouseFrame {
    let tab_id = TabId::new();
    let band_row_count =
        (TEST_VIEWPORT_SIZE.row_count - 2) / u16::try_from(mouse_panes.len()).expect("few panes");
    let pane_slots: Vec<PaneSlot> = mouse_panes
        .iter()
        .enumerate()
        .map(|(pane_index, mouse_pane)| {
            let top_row = band_row_count * u16::try_from(pane_index).expect("few panes");
            let outer_rect = Rect::from_origin_and_size(
                Point {
                    column: 0,
                    row: top_row,
                },
                Size {
                    column_count: TEST_VIEWPORT_SIZE.column_count,
                    row_count: band_row_count,
                },
            );
            PaneSlot {
                pane_id: mouse_pane.pane_id,
                outer_rect,
                content_rect: Some(Rect::from_origin_and_size(
                    Point {
                        column: 1,
                        row: top_row + 1,
                    },
                    Size {
                        column_count: TEST_VIEWPORT_SIZE.column_count - 2,
                        row_count: band_row_count - 2,
                    },
                )),
                is_visible: true,
                is_suppressed: false,
            }
        })
        .collect();
    MouseFrame {
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_revision: 0,
            session_name: "fixture".to_owned(),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: "one".to_owned(),
                pane_slots,
                tab_size: Size {
                    column_count: TEST_VIEWPORT_SIZE.column_count,
                    row_count: TEST_VIEWPORT_SIZE.row_count - 2,
                },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                is_every_pane_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata: vec![TabMetadata {
                tab_id,
                tab_name: "one".to_owned(),
                tab_index: 0,
                is_active: true,
            }],
        },
        mouse_panes: mouse_panes.to_vec(),
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            client_revision: 0,
            viewport_size: TEST_VIEWPORT_SIZE,
            active_tab_id: tab_id,
            focused_pane_id,
            lock_mode: LockMode::Normal,
            is_mouse_selection_enabled: false,
        },
        committed_regions: CommittedRegions::build_core(TEST_VIEWPORT_SIZE, 0),
    }
}

/// A frame holding one plain terminal pane, focused.
fn build_one_pane_mouse_frame(mouse_pane: MousePane) -> MouseFrame {
    let pane_id = mouse_pane.pane_id;
    build_mouse_frame(&[mouse_pane], Some(pane_id))
}

/// A cell inside the content of the `pane_index`-th pane in a fixture frame.
fn get_content_cell(mouse_frame: &MouseFrame, pane_index: usize) -> Point {
    let content_rect = mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[pane_index]
        .content_rect
        .expect("a visible pane");
    Point {
        column: content_rect.origin.column + 1,
        row: content_rect.origin.row + 2,
    }
}

/// Build a same-tab placement snapshot whose geometry and pane ids match `mouse_frame`.
fn build_mouse_placement_snapshot(
    mouse_frame: &MouseFrame,
    source_pane_id: PaneId,
    layout_tree: LayoutNode,
) -> PanePlacementSnapshot {
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut placement_snapshot = crate::tests::build_test_placement_snapshot(
        SessionId::new(),
        ClientId::new(),
        source_pane_id,
        active_tab_id,
        active_tab_id,
        0,
        0,
    );
    placement_snapshot.destination_tab_snapshot = None;
    placement_snapshot.source_tab_snapshot.layout_tree = layout_tree;
    let frame_tab_snapshot = mouse_frame.session_snapshot.active_tab_snapshot.clone();
    placement_snapshot.source_tab_snapshot.tab_name = frame_tab_snapshot.tab_name;
    placement_snapshot.source_tab_snapshot.pane_slots = frame_tab_snapshot
        .pane_slots
        .into_iter()
        .map(|pane_slot| koshi_ipc::frame::FrameSlot {
            pane_id: pane_slot.pane_id,
            outer_rect: pane_slot.outer_rect,
            content_rect: pane_slot.content_rect,
            is_visible: pane_slot.is_visible,
            is_suppressed: pane_slot.is_suppressed,
        })
        .collect();
    placement_snapshot.source_tab_snapshot.tab_size = frame_tab_snapshot.tab_size;
    placement_snapshot.source_tab_snapshot.stack_headers = frame_tab_snapshot.stack_headers;
    placement_snapshot.source_tab_snapshot.layout_mode = frame_tab_snapshot.layout_mode;
    placement_snapshot
        .source_tab_snapshot
        .is_every_pane_suppressed = frame_tab_snapshot.is_every_pane_suppressed;
    placement_snapshot.source_tab_snapshot.gap_cell_count = frame_tab_snapshot.gap_cell_count;
    placement_snapshot.source_tab_snapshot.pane_snapshots = placement_snapshot
        .source_tab_snapshot
        .layout_tree
        .list_leaf_pane_ids()
        .into_iter()
        .map(|pane_id| PanePlacementPaneSnapshot {
            pane_id,
            terminal_window: None,
            image_placement_snapshots: Vec::new(),
        })
        .collect();
    placement_snapshot
}

/// Build a cross-tab placement snapshot with destination slots matching a frame.
fn build_cross_tab_mouse_placement_snapshot(
    mouse_frame: &MouseFrame,
    source_pane_id: PaneId,
    destination_tab_id: TabId,
    destination_pane_ids: [PaneId; 2],
) -> PanePlacementSnapshot {
    let source_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut placement_snapshot = build_mouse_placement_snapshot(
        mouse_frame,
        source_pane_id,
        LayoutNode::Pane(source_pane_id),
    );
    placement_snapshot
        .source_tab_snapshot
        .pane_slots
        .retain(|pane_slot| pane_slot.pane_id == source_pane_id);
    placement_snapshot
        .source_tab_snapshot
        .pane_snapshots
        .retain(|pane_snapshot| pane_snapshot.pane_id == source_pane_id);

    let frame_tab_snapshot = &mouse_frame.session_snapshot.active_tab_snapshot;
    let destination_pane_slots = frame_tab_snapshot
        .pane_slots
        .iter()
        .zip(destination_pane_ids)
        .map(
            |(pane_slot, destination_pane_id)| koshi_ipc::frame::FrameSlot {
                pane_id: destination_pane_id,
                outer_rect: pane_slot.outer_rect,
                content_rect: pane_slot.content_rect,
                is_visible: pane_slot.is_visible,
                is_suppressed: pane_slot.is_suppressed,
            },
        )
        .collect();
    let destination_pane_snapshots = destination_pane_ids
        .into_iter()
        .map(|pane_id| PanePlacementPaneSnapshot {
            pane_id,
            terminal_window: None,
            image_placement_snapshots: Vec::new(),
        })
        .collect();
    let destination_tab_snapshot = PanePlacementTabSnapshot {
        tab_id: destination_tab_id,
        tab_name: "destination".to_owned(),
        layout_tree: LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Vertical,
            destination_pane_ids
                .into_iter()
                .map(LayoutNode::Pane)
                .collect(),
        )),
        pane_slots: destination_pane_slots,
        tab_size: frame_tab_snapshot.tab_size,
        stack_headers: Vec::new(),
        layout_mode: frame_tab_snapshot.layout_mode,
        is_every_pane_suppressed: frame_tab_snapshot.is_every_pane_suppressed,
        gap_cell_count: frame_tab_snapshot.gap_cell_count,
        pane_snapshots: destination_pane_snapshots,
    };
    placement_snapshot.source_tab_id = source_tab_id;
    placement_snapshot.destination_tab_id = destination_tab_id;
    placement_snapshot.destination_tab_snapshot = Some(destination_tab_snapshot);
    placement_snapshot
}

/// A wheel tick at `position`.
fn build_mouse_wheel(scroll_direction: ScrollDirection, position: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Scroll(scroll_direction),
        position,
        modifier_flags: BindingModifierFlags::NONE,
    }
}

#[test]
fn a_wheel_over_a_plain_pane_scrolls_by_the_viewers_own_line_count() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let mut client = build_test_client_with_mouse_config(PartialMouseConfig {
        scroll_line_count: Some(7),
        ..PartialMouseConfig::default()
    });

    let wheel_decision = client
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&mouse_frame, 0)),
            &mouse_frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(wheel_decision.hovered_pane_id, Some(pane_id));
    assert_eq!(
        wheel_decision.mouse_action,
        Some(MouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 7,
        })
    );
}

#[test]
fn a_wheel_down_over_a_plain_pane_moves_the_view_toward_live() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));

    let wheel_decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Down, get_content_cell(&mouse_frame, 0)),
            &mouse_frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        wheel_decision.mouse_action,
        Some(MouseAction::Scroll {
            pane_id,
            is_scrolling_up: false,
            scroll_line_count: 3,
        })
    );
}

#[test]
fn a_horizontal_wheel_over_a_plain_pane_decides_nothing() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));

    for scroll_direction in [ScrollDirection::Left, ScrollDirection::Right] {
        let wheel_decision = build_test_client()
            .handle_mouse_wheel(
                build_mouse_wheel(scroll_direction, get_content_cell(&mouse_frame, 0)),
                &mouse_frame,
            )
            .expect("a wheel tick decides");

        assert_eq!(
            wheel_decision.hovered_pane_id,
            Some(pane_id),
            "{scroll_direction:?} still hovers"
        );
        assert_eq!(
            wheel_decision.mouse_action, None,
            "{scroll_direction:?} moves no vertical view"
        );
    }
}

#[test]
fn the_ignore_setting_leaves_a_plain_pane_alone() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let mut client = build_test_client_with_mouse_config(PartialMouseConfig {
        wheel: Some(WheelScroll::Ignore),
        ..PartialMouseConfig::default()
    });

    let wheel_decision = client
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&mouse_frame, 0)),
            &mouse_frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(wheel_decision.mouse_action, None);
}

#[test]
fn a_program_asking_for_the_mouse_gets_the_tick_forwarded() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.mouse_tracking = MouseTracking::Normal;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);
    let wheel_tick = build_mouse_wheel(ScrollDirection::Up, get_content_cell(&mouse_frame, 0));

    let wheel_decision = build_test_client()
        .handle_mouse_wheel(wheel_tick, &mouse_frame)
        .expect("a wheel tick decides");

    assert_eq!(
        wheel_decision.mouse_action,
        Some(MouseAction::Forward {
            pane_id,
            mouse_input: wheel_tick,
        })
    );
}

#[test]
fn x10_tracking_predates_the_wheel_so_the_tick_is_koshis() {
    // `?9` reports presses only. A wheel tick there is not the program's, so it
    // falls through to koshi's own scrollback.
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.mouse_tracking = MouseTracking::X10;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);

    let wheel_decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&mouse_frame, 0)),
            &mouse_frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        wheel_decision.mouse_action,
        Some(MouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        })
    );
}

#[test]
fn a_highlight_holds_the_view_even_over_a_mouse_reporting_program() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.mouse_tracking = MouseTracking::Normal;
    mouse_pane.has_selection = true;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);

    let wheel_decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&mouse_frame, 0)),
            &mouse_frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        wheel_decision.mouse_action,
        Some(MouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        }),
        "the highlight wins over the program's mouse mode"
    );
}

#[test]
fn the_alternate_screen_with_alternate_scroll_becomes_arrow_keys() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.is_on_alternate_screen = true;
    mouse_pane.is_alternate_scroll_enabled = true;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);

    let wheel_up_decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&mouse_frame, 0)),
            &mouse_frame,
        )
        .expect("a wheel tick decides");
    assert_eq!(
        wheel_up_decision.mouse_action,
        Some(MouseAction::AlternateScrollArrows {
            pane_id,
            is_scrolling_up: true,
            arrow_count: 3,
        })
    );

    let wheel_down_decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Down, get_content_cell(&mouse_frame, 0)),
            &mouse_frame,
        )
        .expect("a wheel tick decides");
    assert_eq!(
        wheel_down_decision.mouse_action,
        Some(MouseAction::AlternateScrollArrows {
            pane_id,
            is_scrolling_up: false,
            arrow_count: 3,
        })
    );
}

#[test]
fn alternate_scroll_off_the_alternate_screen_is_not_arrow_keys() {
    // `?1007` only translates the wheel while the alternate screen is up.
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.is_alternate_scroll_enabled = true;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);

    let wheel_decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&mouse_frame, 0)),
            &mouse_frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        wheel_decision.mouse_action,
        Some(MouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        })
    );
}

#[test]
fn a_horizontal_wheel_under_alternate_scroll_sends_no_arrows() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.is_on_alternate_screen = true;
    mouse_pane.is_alternate_scroll_enabled = true;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);

    let wheel_decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Left, get_content_cell(&mouse_frame, 0)),
            &mouse_frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(wheel_decision.mouse_action, None);
}

#[test]
fn the_tick_targets_the_pane_under_the_pointer_not_the_focused_one() {
    let focused_pane_id = PaneId::new();
    let other_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(focused_pane_id),
            build_plain_mouse_pane(other_pane_id),
        ],
        Some(focused_pane_id),
    );

    let wheel_decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&mouse_frame, 1)),
            &mouse_frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(wheel_decision.hovered_pane_id, Some(other_pane_id));
    assert_eq!(
        wheel_decision.mouse_action,
        Some(MouseAction::Scroll {
            pane_id: other_pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        })
    );
}

#[test]
fn a_tick_over_chrome_falls_through_to_the_focused_pane_and_hovers_nothing() {
    let focused_pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(focused_pane_id));

    // The hint bar on the bottom row is chrome: no pane sits under the pointer.
    let wheel_decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(
                ScrollDirection::Up,
                Point {
                    column: 40,
                    row: TEST_VIEWPORT_SIZE.row_count - 1,
                },
            ),
            &mouse_frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        wheel_decision.hovered_pane_id, None,
        "chrome hovers no pane"
    );
    assert_eq!(
        wheel_decision.mouse_action,
        Some(MouseAction::Scroll {
            pane_id: focused_pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        })
    );
}

#[test]
fn a_tick_over_the_tabline_steps_the_viewers_own_strip() {
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;

    let mut client = build_test_client();
    let wheel_down_decision = client
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Down, Point { column: 40, row: 0 }),
            &mouse_frame,
        )
        .expect("a wheel tick decides");
    assert_eq!(wheel_down_decision.hovered_pane_id, None);
    assert_eq!(
        wheel_down_decision.mouse_action, None,
        "the strip is the viewer's own to move"
    );
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(1)
    );

    let wheel_up_decision = client
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, Point { column: 40, row: 0 }),
            &mouse_frame,
        )
        .expect("a wheel tick decides");
    assert_eq!(wheel_up_decision.mouse_action, None);
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(0),
        "the first visible index saturates at zero"
    );
}

#[test]
fn a_tab_switch_cancels_the_strip_peek() {
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_test_client();

    client
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Down, Point { column: 40, row: 0 }),
            &mouse_frame,
        )
        .expect("a wheel tick decides");
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(1)
    );

    assert_eq!(
        client.build_viewer_chrome(TabId::new()).tabline_offset,
        None,
        "a peek belongs to the tab it was made on"
    );
}

#[test]
fn switching_away_and_back_does_not_bring_the_peek_out_again() {
    // Seeing a frame on another tab throws the peek away. Coming back to the
    // tab it was made on starts the strip at that tab.
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    let first_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let other_tab_id = TabId::new();
    let mut client = build_test_client();

    client
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Down, Point { column: 40, row: 0 }),
            &mouse_frame,
        )
        .expect("a wheel tick decides");
    assert_eq!(
        client.build_viewer_chrome(first_tab_id).tabline_offset,
        Some(1)
    );

    client.note_active_tab(other_tab_id);
    assert_eq!(
        client.build_viewer_chrome(other_tab_id).tabline_offset,
        None
    );

    client.note_active_tab(first_tab_id);
    assert_eq!(
        client.build_viewer_chrome(first_tab_id).tabline_offset,
        None,
        "the peek was thrown away on the switch, not just ignored"
    );
}

#[test]
fn a_mouse_event_on_another_tabs_frame_throws_the_peek_away() {
    // The client reads the active tab from the frame a mouse event is answered
    // against. A peek made on one tab is thrown away by an event on another.
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    let first_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut other_tab_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    other_tab_frame.client_snapshot.active_tab_id = TabId::new();
    let mut client = build_test_client();

    client
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Down, Point { column: 40, row: 0 }),
            &mouse_frame,
        )
        .expect("a wheel tick decides");
    assert_eq!(
        client.build_viewer_chrome(first_tab_id).tabline_offset,
        Some(1)
    );

    client.handle_mouse(
        MouseInput {
            mouse_kind: MouseKind::Motion,
            position: get_content_cell(&other_tab_frame, 0),
            modifier_flags: BindingModifierFlags::NONE,
        },
        &other_tab_frame,
        Instant::now(),
    );

    assert_eq!(
        client.build_viewer_chrome(first_tab_id).tabline_offset,
        None
    );
}

#[test]
fn every_kind_but_the_wheel_is_left_to_the_session() {
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    let screen_point = get_content_cell(&mouse_frame, 0);

    for mouse_kind in [
        MouseKind::Press(MouseButton::Left),
        MouseKind::Press(MouseButton::Middle),
        MouseKind::Press(MouseButton::Right),
        MouseKind::Release(MouseButton::Left),
        MouseKind::Drag(MouseButton::Left),
        MouseKind::Motion,
    ] {
        assert_eq!(
            build_test_client().handle_mouse_wheel(
                MouseInput {
                    mouse_kind,
                    position: screen_point,
                    modifier_flags: BindingModifierFlags::NONE,
                },
                &mouse_frame,
            ),
            None,
            "{mouse_kind:?} is not the viewer's to answer"
        );
    }
}

#[test]
fn a_tick_over_chrome_with_nothing_focused_decides_nothing() {
    // A tab with no focusable pane leaves a tick over chrome with no target.
    let mouse_frame = build_mouse_frame(&[build_plain_mouse_pane(PaneId::new())], None);

    let wheel_decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(
                ScrollDirection::Up,
                Point {
                    column: 40,
                    row: TEST_VIEWPORT_SIZE.row_count - 1,
                },
            ),
            &mouse_frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(wheel_decision.hovered_pane_id, None);
    assert_eq!(wheel_decision.mouse_action, None);
}

#[test]
fn a_tick_over_chrome_ignores_a_focused_pane_the_layout_does_not_place() {
    // The focused pane is not among this tab's solved slots, so nothing is
    // drawn for it and a tick that fell through to it targets nothing.
    let mut mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    mouse_frame.client_snapshot.focused_pane_id = Some(PaneId::new());

    let wheel_decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(
                ScrollDirection::Up,
                Point {
                    column: 40,
                    row: TEST_VIEWPORT_SIZE.row_count - 1,
                },
            ),
            &mouse_frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(wheel_decision.mouse_action, None);
}

#[test]
fn a_zero_line_scroll_setting_is_carried_through_as_zero() {
    // `mouse.scroll-lines 0` is carried as a zero-line movement.
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let mut client = build_test_client_with_mouse_config(PartialMouseConfig {
        scroll_line_count: Some(0),
        ..PartialMouseConfig::default()
    });

    let wheel_decision = client
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&mouse_frame, 0)),
            &mouse_frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        wheel_decision.mouse_action,
        Some(MouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 0,
        })
    );
}

#[test]
fn a_frame_with_no_room_falls_through_to_the_focused_pane() {
    // Every pane is suppressed, so the frame is the "terminal too small"
    // overlay: no tab strip and no pane content is drawn, and nothing is
    // hit-testable.
    let pane_id = PaneId::new();
    let mut mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    mouse_frame
        .session_snapshot
        .active_tab_snapshot
        .is_every_pane_suppressed = true;

    for screen_point in [
        Point { column: 40, row: 0 },
        Point {
            column: 40,
            row: 10,
        },
        Point {
            column: 40,
            row: TEST_VIEWPORT_SIZE.row_count - 1,
        },
    ] {
        let wheel_decision = build_test_client()
            .handle_mouse_wheel(
                build_mouse_wheel(ScrollDirection::Down, screen_point),
                &mouse_frame,
            )
            .expect("a wheel tick decides");

        assert_eq!(
            wheel_decision.hovered_pane_id, None,
            "{screen_point:?} hovers no pane"
        );
        assert_eq!(
            wheel_decision.mouse_action,
            Some(MouseAction::Scroll {
                pane_id,
                is_scrolling_up: false,
                scroll_line_count: 3,
            }),
            "{screen_point:?} falls through to the focused pane, and never to the tab strip"
        );
    }
}

#[test]
fn a_tick_aimed_at_a_pane_the_frame_carries_no_content_for_decides_nothing() {
    // The slot is laid out but no `PaneSnapshot` came with it, so nothing is
    // known about the pane's modes and no decision can be made about it.
    let pane_id = PaneId::new();
    let mut mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    mouse_frame.mouse_panes.clear();

    let wheel_decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&mouse_frame, 0)),
            &mouse_frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        wheel_decision.hovered_pane_id,
        Some(pane_id),
        "the layout still places it"
    );
    assert_eq!(wheel_decision.mouse_action, None);
}

// ============================================================================
// Gestures other than the wheel
// ============================================================================

/// A press, drag, or release of the left button at `position`, with
/// `modifier_flags` held.
fn build_mouse_event(
    mouse_kind: MouseKind,
    position: Point,
    modifier_flags: BindingModifierFlags,
) -> MouseInput {
    MouseInput {
        mouse_kind,
        position,
        modifier_flags,
    }
}

/// A left press at `position`.
fn build_left_mouse_press(position: Point) -> MouseInput {
    build_mouse_event(
        MouseKind::Press(MouseButton::Left),
        position,
        BindingModifierFlags::NONE,
    )
}

/// A left drag to `position`.
fn build_left_mouse_drag(position: Point) -> MouseInput {
    build_mouse_event(
        MouseKind::Drag(MouseButton::Left),
        position,
        BindingModifierFlags::NONE,
    )
}

/// A left release at `position`.
fn build_left_mouse_release(position: Point) -> MouseInput {
    build_mouse_event(
        MouseKind::Release(MouseButton::Left),
        position,
        BindingModifierFlags::NONE,
    )
}

/// A buttonless move to `position`.
fn build_mouse_motion(position: Point) -> MouseInput {
    build_mouse_event(MouseKind::Motion, position, BindingModifierFlags::NONE)
}

/// An instant `second_count` seconds after `start_instant`.
fn advance_time_by_seconds(start_instant: Instant, second_count: u64) -> Instant {
    start_instant + Duration::from_secs(second_count)
}

/// The single `SetSelection` in `actions`, or `None` when it holds none.
fn find_set_selection_action(mouse_actions: &[MouseAction]) -> Option<SetSelectionArgs> {
    mouse_actions
        .iter()
        .find_map(|mouse_action| match mouse_action {
            MouseAction::Command(Command::Visual(VisualCommand::SetSelection(command_args))) => {
                Some(*command_args)
            }
            _ => None,
        })
}

/// The single `Copy` in `actions`, or `None` when it holds none.
fn find_copy_action(mouse_actions: &[MouseAction]) -> Option<CopyArgs> {
    mouse_actions
        .iter()
        .find_map(|mouse_action| match mouse_action {
            MouseAction::Command(Command::Visual(VisualCommand::Copy(command_args))) => {
                Some(*command_args)
            }
            _ => None,
        })
}

#[test]
fn compute_resize_cell_delta_grows_toward_each_border_and_ignores_the_other_axis() {
    let start_position = Point {
        column: 10,
        row: 10,
    };
    // Right border: pointer rightward grows, leftward shrinks.
    assert_eq!(
        compute_resize_cell_delta(
            Direction::Right,
            start_position,
            Point {
                column: 13,
                row: 10
            }
        ),
        3
    );
    assert_eq!(
        compute_resize_cell_delta(
            Direction::Right,
            start_position,
            Point { column: 8, row: 10 }
        ),
        -2
    );
    // Left border: pointer leftward grows.
    assert_eq!(
        compute_resize_cell_delta(
            Direction::Left,
            start_position,
            Point { column: 7, row: 10 }
        ),
        3
    );
    assert_eq!(
        compute_resize_cell_delta(
            Direction::Left,
            start_position,
            Point {
                column: 12,
                row: 10
            }
        ),
        -2
    );
    // Down border: pointer downward grows.
    assert_eq!(
        compute_resize_cell_delta(
            Direction::Down,
            start_position,
            Point {
                column: 10,
                row: 14
            }
        ),
        4
    );
    // Up border: pointer upward grows.
    assert_eq!(
        compute_resize_cell_delta(Direction::Up, start_position, Point { column: 10, row: 6 }),
        4
    );
    // A left/right border ignores vertical motion.
    assert_eq!(
        compute_resize_cell_delta(
            Direction::Right,
            start_position,
            Point {
                column: 10,
                row: 20
            }
        ),
        0
    );
}

#[test]
fn advance_resize_anchor_walks_the_anchor_the_way_the_answered_step_asked_and_saturates() {
    let start_position = Point { column: 3, row: 3 };
    // A positive step grows the pane: a right or down border walks away from
    // zero, a left or up border walks toward it.
    assert_eq!(
        advance_resize_anchor(Direction::Right, start_position, 1, 2),
        Point { column: 5, row: 3 }
    );
    assert_eq!(
        advance_resize_anchor(Direction::Left, start_position, 1, 2),
        Point { column: 1, row: 3 }
    );
    assert_eq!(
        advance_resize_anchor(Direction::Down, start_position, 1, 2),
        Point { column: 3, row: 5 }
    );
    assert_eq!(
        advance_resize_anchor(Direction::Up, start_position, 1, 2),
        Point { column: 3, row: 1 }
    );
    // A negative step shrinks it, so every border walks the other way.
    assert_eq!(
        advance_resize_anchor(Direction::Right, start_position, -1, 2),
        Point { column: 1, row: 3 }
    );
    assert_eq!(
        advance_resize_anchor(Direction::Left, start_position, -1, 2),
        Point { column: 5, row: 3 }
    );
    assert_eq!(
        advance_resize_anchor(Direction::Down, start_position, -1, 2),
        Point { column: 3, row: 1 }
    );
    assert_eq!(
        advance_resize_anchor(Direction::Up, start_position, -1, 2),
        Point { column: 3, row: 5 }
    );
    // The anchor lands exactly where `compute_resize_cell_delta` reads the move back.
    for border_side in [
        Direction::Left,
        Direction::Right,
        Direction::Up,
        Direction::Down,
    ] {
        for resize_step in [-1, 1] {
            assert_eq!(
                compute_resize_cell_delta(
                    border_side,
                    start_position,
                    advance_resize_anchor(border_side, start_position, resize_step, 2),
                ),
                resize_step * 2,
                "{border_side:?} answered a step of {resize_step}"
            );
        }
    }
    // Saturating: an anchor at an edge cannot wrap past either end.
    assert_eq!(
        advance_resize_anchor(Direction::Left, start_position, 1, 10),
        Point { column: 0, row: 3 }
    );
    assert_eq!(
        advance_resize_anchor(Direction::Up, start_position, 1, 10),
        Point { column: 3, row: 0 }
    );
    assert_eq!(
        advance_resize_anchor(
            Direction::Right,
            Point {
                column: u16::MAX - 1,
                row: 3
            },
            1,
            5
        ),
        Point {
            column: u16::MAX,
            row: 3
        }
    );
}

#[test]
fn a_press_names_the_line_the_frame_showed_on_that_row() {
    // The frame says which line the pane's top visible row is. The press names
    // that line plus the row it landed on, whatever the pane's live view has
    // done since.
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.view_top_row_index = 940;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);
    let mut client = build_test_client();
    let current_time = Instant::now();

    // The third visible row of the pane's content.
    let content_rect = mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[0]
        .content_rect
        .expect("a visible pane");
    let screen_point = Point {
        column: content_rect.origin.column + 4,
        row: content_rect.origin.row + 3,
    };
    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );
    let mouse_actions = client.handle_mouse(
        build_left_mouse_drag(Point {
            column: content_rect.origin.column + 9,
            row: content_rect.origin.row + 3,
        }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );

    let selection_args =
        find_set_selection_action(&mouse_actions).expect("the drag asked for a highlight");
    assert_eq!(selection_args.pane_id, pane_id);
    assert_eq!(
        selection_args.selection.anchor,
        GridPosition {
            row_index: 942,
            column_index: 4
        }
    );
    assert_eq!(
        selection_args.selection.cursor,
        GridPosition {
            row_index: 942,
            column_index: 9
        }
    );
}

#[test]
fn output_between_the_paint_and_the_press_does_not_move_what_it_names() {
    // The same press against the same painted frame names the same line even
    // once the pane has pushed more output: the frame's line numbers are
    // absolute, so nothing about them shifts.
    let pane_id = PaneId::new();
    let mut scrolled_mouse_pane = build_plain_mouse_pane(pane_id);
    scrolled_mouse_pane.view_top_row_index = 500;
    let painted_mouse_frame = build_one_pane_mouse_frame(scrolled_mouse_pane);
    let content_rect = painted_mouse_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[0]
        .content_rect
        .expect("a visible pane");
    let screen_point = Point {
        column: content_rect.origin.column,
        row: content_rect.origin.row + 4,
    };
    let current_time = Instant::now();

    let mut client = build_test_client();
    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &painted_mouse_frame,
        current_time,
    );
    let mouse_actions = client.handle_mouse(
        build_left_mouse_drag(Point {
            column: content_rect.origin.column + 2,
            row: content_rect.origin.row + 4,
        }),
        &painted_mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );

    assert_eq!(
        find_set_selection_action(&mouse_actions)
            .expect("a highlight")
            .selection
            .anchor,
        GridPosition {
            row_index: 503,
            column_index: 0
        },
        "the press names the line the user saw on that row"
    );
}

#[test]
fn a_second_press_inside_the_threshold_selects_whole_words() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let screen_point = get_content_cell(&mouse_frame, 0);
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );
    let mouse_actions = client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time + Duration::from_millis(120),
    );

    assert_eq!(
        find_set_selection_action(&mouse_actions)
            .expect("a word highlight")
            .selection
            .selection_kind,
        SelectionKind::Word,
        "the second press in the run names a word"
    );
}

#[test]
fn a_third_press_selects_whole_lines_and_a_fourth_starts_over() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let screen_point = get_content_cell(&mouse_frame, 0);
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time + Duration::from_millis(100),
    );
    let third_press_actions = client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time + Duration::from_millis(200),
    );
    assert_eq!(
        find_set_selection_action(&third_press_actions)
            .expect("a line highlight")
            .selection
            .selection_kind,
        SelectionKind::Line
    );

    // A fourth press starts the run over, and a single click names a point, so
    // it highlights nothing until the pointer moves.
    let fourth_press_actions = client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time + Duration::from_millis(300),
    );
    assert_eq!(
        find_set_selection_action(&fourth_press_actions),
        None,
        "the run began again"
    );
    let drag_actions = client.handle_mouse(
        build_left_mouse_drag(Point {
            column: screen_point.column + 3,
            ..screen_point
        }),
        &mouse_frame,
        current_time + Duration::from_millis(320),
    );
    assert_eq!(
        find_set_selection_action(&drag_actions)
            .expect("a highlight")
            .selection
            .selection_kind,
        SelectionKind::Character
    );
}

#[test]
fn a_press_past_the_threshold_is_another_single_click() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let screen_point = get_content_cell(&mouse_frame, 0);
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time + Duration::from_millis(400),
    );
    let mouse_actions = client.handle_mouse(
        build_left_mouse_drag(Point {
            column: screen_point.column + 3,
            ..screen_point
        }),
        &mouse_frame,
        current_time + Duration::from_millis(420),
    );

    assert_eq!(
        find_set_selection_action(&mouse_actions)
            .expect("a highlight")
            .selection
            .selection_kind,
        SelectionKind::Character,
        "exactly 400ms is no longer inside the threshold"
    );
}

#[test]
fn alt_held_at_the_press_makes_a_block_whatever_the_run_of_clicks_was() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let screen_point = get_content_cell(&mouse_frame, 0);
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );
    let mouse_actions = client.handle_mouse(
        build_mouse_event(
            MouseKind::Press(MouseButton::Left),
            screen_point,
            BindingModifierFlags::ALT,
        ),
        &mouse_frame,
        current_time + Duration::from_millis(120),
    );
    // A block names a point, so the press alone highlights nothing.
    assert_eq!(find_set_selection_action(&mouse_actions), None);

    let drag_actions = client.handle_mouse(
        build_left_mouse_drag(Point {
            column: screen_point.column + 2,
            ..screen_point
        }),
        &mouse_frame,
        current_time + Duration::from_millis(140),
    );
    assert_eq!(
        find_set_selection_action(&drag_actions)
            .expect("a highlight")
            .selection
            .selection_kind,
        SelectionKind::Block
    );
}

#[test]
fn a_captured_drag_that_leaves_the_pane_still_reaches_it_and_the_release_ends_it() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    // Button-event tracking reports presses, drags, and releases.
    mouse_pane.mouse_tracking = MouseTracking::ButtonMotion;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);
    let screen_point = get_content_cell(&mouse_frame, 0);
    let mut client = build_test_client();
    let current_time = Instant::now();

    let press_actions = client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );
    assert_eq!(
        press_actions,
        vec![MouseAction::Forward {
            pane_id,
            mouse_input: build_left_mouse_press(screen_point),
        }]
    );
    // The session wrote that press to the pane, which is what captures the
    // gesture; the loop reports it back.
    client.note_press_forwarded(pane_id, MouseButton::Left);

    // Row 0 is the tabline — off the pane entirely. The capture still routes it.
    let outside_point = Point { column: 0, row: 0 };
    let drag_actions = client.handle_mouse(
        build_left_mouse_drag(outside_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    assert_eq!(
        drag_actions,
        vec![MouseAction::Forward {
            pane_id,
            mouse_input: build_left_mouse_drag(outside_point),
        }],
        "the held button keeps the gesture on the pane it pressed"
    );

    let release_actions = client.handle_mouse(
        build_left_mouse_release(outside_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );
    assert_eq!(
        release_actions,
        vec![MouseAction::Forward {
            pane_id,
            mouse_input: build_left_mouse_release(outside_point),
        }]
    );

    // The capture is over: a further drag reaches no program.
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_drag(outside_point),
            &mouse_frame,
            advance_time_by_seconds(current_time, 3)
        ),
        Vec::new(),
        "a drag with no press behind it forwards nothing"
    );
}

#[test]
fn a_bare_move_off_the_focused_panes_content_reaches_no_program() {
    // No button is held, so there is no capture to route by: a move only
    // reaches a program when it lands inside that program's own content. Both
    // panes are drawn, the upper one is focused, and the pointer is in the
    // lower one.
    let focused_pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(focused_pane_id);
    // Any-event tracking asks for every move, so the position is the only thing
    // keeping this one out.
    mouse_pane.mouse_tracking = MouseTracking::AnyMotion;
    let mouse_frame = build_mouse_frame(
        &[mouse_pane, build_plain_mouse_pane(PaneId::new())],
        Some(focused_pane_id),
    );
    let inside_point = get_content_cell(&mouse_frame, 0);
    let outside_point = get_content_cell(&mouse_frame, 1);
    let current_time = Instant::now();
    let mut client = build_test_client();

    assert_eq!(
        client.handle_mouse(build_mouse_motion(inside_point), &mouse_frame, current_time),
        vec![MouseAction::Forward {
            pane_id: focused_pane_id,
            mouse_input: build_mouse_motion(inside_point),
        }],
        "a move inside the focused pane's content is the program's"
    );

    assert_eq!(
        client.handle_mouse(
            build_mouse_motion(outside_point),
            &mouse_frame,
            advance_time_by_seconds(current_time, 1)
        ),
        Vec::new(),
        "the same move one band down names no cell in the focused pane"
    );
}

#[test]
fn mouse_routing_uses_the_region_solve_committed_with_the_frame() {
    let pane_id = PaneId::new();
    let mut watched_mouse_pane = build_plain_mouse_pane(pane_id);
    watched_mouse_pane.mouse_tracking = MouseTracking::AnyMotion;
    let mut mouse_frame = build_mouse_frame(&[watched_mouse_pane], Some(pane_id));
    mouse_frame.session_snapshot.active_tab_snapshot.tab_size = Size {
        column_count: 60,
        row_count: 22,
    };
    mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[0] = PaneSlot {
        pane_id,
        outer_rect: Rect::from_origin_and_size(
            Point { column: 0, row: 0 },
            Size {
                column_count: 60,
                row_count: 22,
            },
        ),
        content_rect: Some(Rect::from_origin_and_size(
            Point { column: 1, row: 1 },
            Size {
                column_count: 58,
                row_count: 20,
            },
        )),
        is_visible: true,
        is_suppressed: false,
    };
    mouse_frame.committed_regions = CommittedRegions::from_solved_regions(
        TEST_VIEWPORT_SIZE,
        solve_region_rects(
            TEST_VIEWPORT_SIZE,
            &[
                RegionGeometry {
                    edge: Edge::Top,
                    extent_cell_count: 1,
                },
                RegionGeometry {
                    edge: Edge::Bottom,
                    extent_cell_count: 1,
                },
                RegionGeometry {
                    edge: Edge::Left,
                    extent_cell_count: 20,
                },
            ],
        ),
        9,
    );
    let mut client = build_test_client();

    assert_eq!(
        client.handle_mouse(
            build_mouse_motion(Point { column: 12, row: 2 }),
            &mouse_frame,
            Instant::now()
        ),
        Vec::new(),
        "the committed left region is not pane content"
    );
    assert_eq!(
        client.handle_mouse(
            build_mouse_motion(Point { column: 21, row: 2 }),
            &mouse_frame,
            Instant::now()
        ),
        vec![MouseAction::Forward {
            pane_id,
            mouse_input: build_mouse_motion(Point { column: 21, row: 2 }),
        }]
    );
}

#[test]
fn a_gesture_is_dropped_when_its_pane_leaves_the_frame() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let screen_point = get_content_cell(&mouse_frame, 0);
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );

    // The pane closes: the next frame does not draw it.
    let replacement_pane_id = PaneId::new();
    let replacement_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(replacement_pane_id));
    let mouse_actions = client.handle_mouse(
        build_left_mouse_drag(get_content_cell(&replacement_frame, 0)),
        &replacement_frame,
        advance_time_by_seconds(current_time, 1),
    );

    assert_eq!(
        mouse_actions,
        Vec::new(),
        "the drag's pane is not on the frame, so the gesture ended with it"
    );
}

#[test]
fn a_pane_swapping_to_the_alternate_screen_ends_the_selection_drag() {
    // The anchor names a line of the primary screen's text. The alternate
    // screen's rows are different text, so extending onto them would highlight
    // whatever `vim` just drew there.
    let pane_id = PaneId::new();
    let primary_screen_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let screen_point = get_content_cell(&primary_screen_frame, 0);
    let drag_end_point = Point {
        column: screen_point.column + 4,
        ..screen_point
    };
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &primary_screen_frame,
        current_time,
    );
    assert_eq!(
        find_set_selection_action(&client.handle_mouse(
            build_left_mouse_drag(drag_end_point),
            &primary_screen_frame,
            advance_time_by_seconds(current_time, 1),
        )),
        Some(SetSelectionArgs {
            pane_id,
            selection: Selection {
                selection_kind: SelectionKind::Character,
                anchor: GridPosition {
                    row_index: 1,
                    column_index: 1
                },
                cursor: GridPosition {
                    row_index: 1,
                    column_index: 5
                },
            },
        }),
        "the drag extends while the pane stays on the screen the press landed on"
    );

    // The program in the pane entered the alternate screen.
    let mut alternate_screen_mouse_pane = build_plain_mouse_pane(pane_id);
    alternate_screen_mouse_pane.is_on_alternate_screen = true;
    let alternate_screen_mouse_frame = build_one_pane_mouse_frame(alternate_screen_mouse_pane);

    assert_eq!(
        client.handle_mouse(
            build_left_mouse_drag(drag_end_point),
            &alternate_screen_mouse_frame,
            advance_time_by_seconds(current_time, 2)
        ),
        Vec::new(),
        "the drag ended with the screen it was made on"
    );
    assert_eq!(client.selection_drag, None);
}

#[test]
fn a_drag_held_past_the_top_edge_scrolls_the_view_back_into_history() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.view_top_row_index = 100;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);
    let screen_point = get_content_cell(&mouse_frame, 0);
    let content_rect = mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[0]
        .content_rect
        .expect("a visible pane");
    // Above the pane's first content row, where the earlier lines are.
    let above_pane_point = Point {
        column: screen_point.column,
        row: 0,
    };
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(above_pane_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    let scroll_due_time = advance_time_by_seconds(current_time, 1) + Duration::from_millis(15);

    assert_eq!(
        client.expire_mouse_scroll(scroll_due_time, &mouse_frame),
        vec![MouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 1,
        }],
        "past the top edge the view moves up, not down"
    );

    // The session says the view moved back one line; the highlight follows it
    // to the pane's first row.
    let mouse_actions = client.note_scroll_applied(pane_id, Some(99), &mouse_frame);
    let command_args =
        find_set_selection_action(&mouse_actions).expect("the scroll re-extends the highlight");
    assert_eq!(
        command_args.selection.cursor,
        GridPosition {
            row_index: 99,
            column_index: screen_point.column - content_rect.origin.column,
        },
        "the moving end sits on the first row of the view the scroll revealed"
    );
}

#[test]
fn dragging_the_bare_tab_strip_peeks_one_tab_every_six_cells() {
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    // The bare tab strip, past the one tab's ribbon.
    let tabline_point = Point { column: 40, row: 0 };
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(tabline_point),
        &mouse_frame,
        current_time,
    );

    // Five cells left of the anchor is short of one whole step.
    client.handle_mouse(
        build_left_mouse_drag(Point { column: 35, row: 0 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(0)
    );

    // Twelve cells left of the anchor is two steps toward the last tab.
    client.handle_mouse(
        build_left_mouse_drag(Point { column: 28, row: 0 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(2)
    );

    // The whole distance is measured from the anchor, so dragging back past it
    // steps the other way and stops at the first tab.
    client.handle_mouse(
        build_left_mouse_drag(Point { column: 52, row: 0 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 3),
    );
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(0),
        "the first visible index saturates at zero"
    );
}

#[test]
fn a_border_press_starts_no_resize_when_the_viewer_turned_it_off() {
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(left_pane_id),
            build_plain_mouse_pane(right_pane_id),
        ],
        Some(left_pane_id),
    );
    // The shared divider between the two bands: the second pane's top edge.
    let divider_point = Point {
        column: 10,
        row: mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[1]
            .outer_rect
            .origin
            .row
            + 1,
    };
    let current_time = Instant::now();

    let mut border_resize_off_client = build_test_client_with_mouse_config(PartialMouseConfig {
        can_resize_pane_border: Some(false),
        ..PartialMouseConfig::default()
    });
    border_resize_off_client.handle_mouse(
        build_left_mouse_press(divider_point),
        &mouse_frame,
        current_time,
    );
    assert_eq!(
        border_resize_off_client.handle_mouse(
            build_left_mouse_drag(Point {
                row: divider_point.row + 3,
                ..divider_point
            }),
            &mouse_frame,
            advance_time_by_seconds(current_time, 1)
        ),
        Vec::new(),
        "border resize is off, so the drag moves nothing"
    );

    // The same gesture with the setting on does move the border.
    let mut border_resize_on_client = build_test_client();
    border_resize_on_client.handle_mouse(
        build_left_mouse_press(divider_point),
        &mouse_frame,
        current_time,
    );
    let mouse_actions = border_resize_on_client.handle_mouse(
        build_left_mouse_drag(Point {
            row: divider_point.row + 3,
            ..divider_point
        }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    assert_eq!(
        mouse_actions,
        vec![MouseAction::Resize {
            pane_id: right_pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 3,
        }],
        "three cells of travel away from the grabbed top border"
    );
}

#[test]
fn grabbing_a_border_with_no_pane_beside_it_starts_no_resize() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    // The pane's own left edge: the tab's outer frame, with nothing beyond it.
    let outer_edge_point = Point {
        column: mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[0]
            .outer_rect
            .origin
            .column,
        row: 5,
    };
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(outer_edge_point),
        &mouse_frame,
        current_time,
    );

    assert_eq!(
        client.handle_mouse(
            build_left_mouse_drag(Point {
                column: outer_edge_point.column + 3,
                ..outer_edge_point
            }),
            &mouse_frame,
            advance_time_by_seconds(current_time, 1)
        ),
        Vec::new(),
        "the tab's outer frame has no neighbour to resize against"
    );
}

#[test]
fn the_drag_anchor_only_walks_over_the_cells_the_session_accepted() {
    // The pointer travelled three cells, the session took two — the pane it
    // would have shrunk hit its minimum size on the third. The anchor advances
    // by those two, so the very next move asks for the one cell still owed.
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(left_pane_id),
            build_plain_mouse_pane(right_pane_id),
        ],
        Some(left_pane_id),
    );
    let divider_point = Point {
        column: 10,
        row: mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[1]
            .outer_rect
            .origin
            .row
            + 1,
    };
    let three_rows_down_point = Point {
        row: divider_point.row + 3,
        ..divider_point
    };
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(divider_point),
        &mouse_frame,
        current_time,
    );
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_drag(three_rows_down_point),
            &mouse_frame,
            advance_time_by_seconds(current_time, 1)
        ),
        vec![MouseAction::Resize {
            pane_id: right_pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 3,
        }]
    );

    // An answer for another pane's border leaves this drag where it is.
    client.note_resize_applied(PaneId::new(), Direction::Up, -1, 1);
    // So does an answer for another side of the very border being dragged.
    client.note_resize_applied(right_pane_id, Direction::Down, -1, 1);
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_drag(three_rows_down_point),
            &mouse_frame,
            advance_time_by_seconds(current_time, 2)
        ),
        vec![MouseAction::Resize {
            pane_id: right_pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 3,
        }],
        "neither answer named this border, so the whole distance stands"
    );

    client.note_resize_applied(right_pane_id, Direction::Up, -1, 2);

    assert_eq!(
        client.handle_mouse(
            build_left_mouse_drag(three_rows_down_point),
            &mouse_frame,
            advance_time_by_seconds(current_time, 3)
        ),
        vec![MouseAction::Resize {
            pane_id: right_pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 1,
        }],
        "two cells were taken, so one is still owed"
    );
}

#[test]
fn releasing_a_highlight_copies_it_with_the_viewers_own_trim_setting() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let screen_point = get_content_cell(&mouse_frame, 0);

    for should_trim_trailing_whitespace in [true, false] {
        let mut client = build_test_client();
        client.client_config.copy.should_trim_trailing_whitespace = should_trim_trailing_whitespace;
        let current_time = Instant::now();

        client.handle_mouse(
            build_left_mouse_press(screen_point),
            &mouse_frame,
            current_time,
        );
        client.handle_mouse(
            build_left_mouse_drag(Point {
                column: screen_point.column + 4,
                ..screen_point
            }),
            &mouse_frame,
            advance_time_by_seconds(current_time, 1),
        );
        let mouse_actions = client.handle_mouse(
            build_left_mouse_release(Point {
                column: screen_point.column + 4,
                ..screen_point
            }),
            &mouse_frame,
            advance_time_by_seconds(current_time, 2),
        );

        assert_eq!(
            find_copy_action(&mouse_actions).expect("the release is the copy"),
            CopyArgs {
                pane_id,
                should_trim_trailing_whitespace,
            },
            "trim {should_trim_trailing_whitespace}"
        );
    }
}

#[test]
fn a_drag_held_past_the_bottom_edge_scrolls_on_the_clock() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.view_top_row_index = 100;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);
    let screen_point = get_content_cell(&mouse_frame, 0);
    let content_rect = mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[0]
        .content_rect
        .expect("a visible pane");
    let below_pane_point = Point {
        column: screen_point.column,
        row: content_rect.origin.row + content_rect.size.row_count + 5,
    };
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(below_pane_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    let scroll_due_time = advance_time_by_seconds(current_time, 1) + Duration::from_millis(15);
    assert_eq!(
        client.compute_next_mouse_wakeup(advance_time_by_seconds(current_time, 1)),
        Some(Duration::from_millis(15)),
        "the pointer past the edge asks the loop to wake"
    );

    // The firing asks for one line of scroll and nothing else yet.
    let fired = client.expire_mouse_scroll(scroll_due_time, &mouse_frame);
    assert_eq!(
        fired,
        vec![MouseAction::Scroll {
            pane_id,
            is_scrolling_up: false,
            scroll_line_count: 1,
        }]
    );

    // The session says the view moved on by one line; the highlight follows it
    // to the pane's last row.
    let mouse_actions = client.note_scroll_applied(pane_id, Some(101), &mouse_frame);
    let command_args =
        find_set_selection_action(&mouse_actions).expect("the scroll re-extends the highlight");
    assert_eq!(
        command_args.selection.cursor,
        GridPosition {
            row_index: 101 + u64::from(content_rect.size.row_count - 1),
            column_index: screen_point.column - content_rect.origin.column,
        },
        "the moving end sits on the last row of the view the scroll revealed"
    );
}

#[test]
fn a_scroll_that_moved_nothing_disarms_the_timer() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.view_top_row_index = 100;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);
    let screen_point = get_content_cell(&mouse_frame, 0);
    let content_rect = mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[0]
        .content_rect
        .expect("a visible pane");
    let below_pane_point = Point {
        column: screen_point.column,
        row: content_rect.origin.row + content_rect.size.row_count + 5,
    };
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(below_pane_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    let scroll_due_time = advance_time_by_seconds(current_time, 1) + Duration::from_millis(15);
    client.expire_mouse_scroll(scroll_due_time, &mouse_frame);

    // The view was already at its limit: the top line did not move.
    let mouse_actions = client.note_scroll_applied(pane_id, Some(100), &mouse_frame);

    assert_eq!(
        mouse_actions,
        Vec::new(),
        "nothing revealed, nothing to extend"
    );
    assert_eq!(
        client.compute_next_mouse_wakeup(scroll_due_time),
        None,
        "a firing that moved nothing disarms the timer"
    );
}

#[test]
fn a_wheel_scroll_never_re_extends_a_highlight() {
    // `note_scroll_applied` answers only the scroll the edge timer asked for; a
    // wheel tick's scroll leaves it with nothing to do.
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let mut client = build_test_client();

    client.handle_mouse(
        build_mouse_wheel(ScrollDirection::Up, get_content_cell(&mouse_frame, 0)),
        &mouse_frame,
        Instant::now(),
    );

    assert_eq!(
        client.note_scroll_applied(pane_id, Some(7), &mouse_frame),
        Vec::new()
    );
}

#[test]
fn a_press_on_an_unfocused_pane_only_focuses_it() {
    let focused_pane_id = PaneId::new();
    let other_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(focused_pane_id),
            build_plain_mouse_pane(other_pane_id),
        ],
        Some(focused_pane_id),
    );
    let mut client = build_test_client();

    let mouse_actions = client.handle_mouse(
        build_left_mouse_press(get_content_cell(&mouse_frame, 1)),
        &mouse_frame,
        Instant::now(),
    );

    assert_eq!(
        mouse_actions,
        vec![MouseAction::Command(Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(other_pane_id),
            client_id: Some(client.get_client_id()),
        }))],
        "the first click focuses and nothing else"
    );
}

#[test]
fn dragging_a_pane_focuses_the_source_and_hides_pointer_chrome() {
    let focused_pane_id = PaneId::new();
    let dragged_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(focused_pane_id),
            build_plain_mouse_pane(dragged_pane_id),
        ],
        Some(focused_pane_id),
    );
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_test_client();
    client.set_frame_view(active_tab_id, Some(focused_pane_id), vec![active_tab_id]);
    assert_eq!(
        client.begin_placement_mode(),
        Some((focused_pane_id, active_tab_id))
    );

    let placement_action = client.handle_placement_mouse(
        build_left_mouse_press(get_content_cell(&mouse_frame, 1)),
        &mouse_frame,
        Instant::now(),
    );

    assert_eq!(
        placement_action,
        Some(PlacementInputAction::ReadPlacement {
            pane_id_to_focus: Some(dragged_pane_id),
            source_pane_id: dragged_pane_id,
            destination_tab_id: active_tab_id,
        })
    );
    assert_eq!(client.get_placement_source_pane_id(), Some(dragged_pane_id));
    let viewer_chrome = client.build_viewer_chrome(active_tab_id);
    assert_eq!(viewer_chrome.hovered_pane_id, None);
    assert_eq!(viewer_chrome.placement_handle_pane_id, None);
}

#[test]
fn shifted_handle_press_in_normal_mode_does_not_start_pane_placement() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_test_client();
    client.set_frame_view(active_tab_id, Some(pane_id), vec![active_tab_id]);
    client.placement_handle_pane_id = Some(pane_id);
    let handle_position = Point { column: 1, row: 1 };
    assert_eq!(
        resolve_hit_region(client.build_frame_layout(&mouse_frame), handle_position),
        HitRegion::PlacementHandle { pane_id }
    );

    assert_eq!(
        client.handle_placement_mouse(
            build_mouse_event(
                MouseKind::Press(MouseButton::Left),
                handle_position,
                BindingModifierFlags::SHIFT,
            ),
            &mouse_frame,
            Instant::now(),
        ),
        None
    );
    assert_eq!(
        client.handle_mouse(
            build_mouse_event(
                MouseKind::Press(MouseButton::Left),
                handle_position,
                BindingModifierFlags::SHIFT,
            ),
            &mouse_frame,
            Instant::now(),
        ),
        Vec::new()
    );
    assert!(!client.is_placement_mode_active());
}

#[test]
fn plain_handle_press_in_normal_mode_starts_pane_placement() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_test_client();
    client.set_frame_view(active_tab_id, Some(pane_id), vec![active_tab_id]);
    client.placement_handle_pane_id = Some(pane_id);
    let handle_position = Point { column: 1, row: 1 };
    assert_eq!(
        resolve_hit_region(client.build_frame_layout(&mouse_frame), handle_position),
        HitRegion::PlacementHandle { pane_id }
    );

    assert_eq!(
        client.handle_placement_mouse(
            build_mouse_event(
                MouseKind::Press(MouseButton::Left),
                handle_position,
                BindingModifierFlags::NONE,
            ),
            &mouse_frame,
            Instant::now(),
        ),
        Some(PlacementInputAction::ReadPlacement {
            pane_id_to_focus: Some(pane_id),
            source_pane_id: pane_id,
            destination_tab_id: active_tab_id,
        })
    );
    assert!(client.is_placement_mode_active());
}

#[test]
fn submitted_mouse_placement_consumes_mouse_input_until_the_frame_reconciles() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_test_client();
    client.set_frame_view(active_tab_id, Some(pane_id), vec![active_tab_id]);
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: pane_id,
        source_tab_id: active_tab_id,
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: Some(PanePlacementTarget::Swap {
            target_pane_id: PaneId::new(),
        }),
        pending_placement_command: Some(crate::tests::build_pending_placement_command(
            CommandId::new(),
        )),
    });
    client.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilDragEnds;

    assert_eq!(
        client.handle_placement_mouse(
            build_mouse_motion(get_content_cell(&mouse_frame, 0)),
            &mouse_frame,
            Instant::now(),
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert!(client.is_placement_confirmation_pending());
}

#[test]
fn mouse_select_mode_takes_a_drag_back_from_a_mouse_aware_program() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.mouse_tracking = MouseTracking::ButtonMotion;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);
    let screen_point = get_content_cell(&mouse_frame, 0);
    let current_time = Instant::now();

    // With mouse-select off the press is the program's.
    let mut mouse_select_off_client = build_test_client();
    assert_eq!(
        mouse_select_off_client.handle_mouse(
            build_left_mouse_press(screen_point),
            &mouse_frame,
            current_time
        ),
        vec![MouseAction::Forward {
            pane_id,
            mouse_input: build_left_mouse_press(screen_point),
        }]
    );

    // With it on, the same press begins a koshi highlight instead.
    let mut mouse_select_on_client = build_mouse_select_client();
    let mouse_actions = mouse_select_on_client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );
    assert_eq!(
        mouse_actions,
        vec![MouseAction::Command(Command::Visual(
            VisualCommand::ClearSelection(ClearSelectionArgs { pane_id })
        ))],
        "the press drops the old highlight and arms a drag"
    );
}

#[test]
fn shift_drag_selects_text_from_a_mouse_aware_program() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.mouse_tracking = MouseTracking::ButtonMotion;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);
    let screen_point = get_content_cell(&mouse_frame, 0);
    let drag_end_point = Point {
        column: screen_point.column + 4,
        ..screen_point
    };
    let current_time = Instant::now();
    let mut client = build_test_client();

    let shifted_press = build_mouse_event(
        MouseKind::Press(MouseButton::Left),
        screen_point,
        BindingModifierFlags::SHIFT,
    );
    assert_eq!(
        client.handle_mouse(shifted_press, &mouse_frame, current_time),
        vec![MouseAction::Command(Command::Visual(
            VisualCommand::ClearSelection(ClearSelectionArgs { pane_id })
        ))],
        "Shift makes the press start Koshi selection"
    );

    let shifted_drag = build_mouse_event(
        MouseKind::Drag(MouseButton::Left),
        drag_end_point,
        BindingModifierFlags::SHIFT,
    );
    assert_eq!(
        client.handle_mouse(
            shifted_drag,
            &mouse_frame,
            advance_time_by_seconds(current_time, 1)
        ),
        vec![MouseAction::Command(Command::Visual(
            VisualCommand::SetSelection(SetSelectionArgs {
                pane_id,
                selection: Selection {
                    selection_kind: SelectionKind::Character,
                    anchor: GridPosition {
                        row_index: 1,
                        column_index: 1
                    },
                    cursor: GridPosition {
                        row_index: 1,
                        column_index: 5
                    },
                },
            })
        ))],
        "the drag extends the selection instead of reaching the program"
    );
}

#[test]
fn ending_the_gestures_drops_all_four_and_leaves_the_pointer_and_the_strip_alone() {
    let hovered_pane_id = PaneId::new();
    let resized_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(hovered_pane_id),
            build_plain_mouse_pane(resized_pane_id),
        ],
        Some(hovered_pane_id),
    );
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let screen_point = get_content_cell(&mouse_frame, 0);
    // The shared divider between the two bands: the second pane's top edge.
    let divider_point = Point {
        column: 10,
        row: mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[1]
            .outer_rect
            .origin
            .row
            + 1,
    };
    // The bare tab strip, past the one tab's ribbon.
    let tabline_point = Point { column: 40, row: 0 };
    let current_time = Instant::now();
    let mut client = build_test_client();

    // A peeked strip and a hovered pane. Neither is a gesture.
    client
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Down, tabline_point),
            &mouse_frame,
        )
        .expect("a wheel tick decides");
    client.handle_mouse(build_mouse_motion(screen_point), &mouse_frame, current_time);
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(1)
    );
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).hovered_pane_id,
        Some(hovered_pane_id)
    );

    // All four gestures under way at once: the presses that began them are
    // spaced so no two read as one double click.
    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_press(divider_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    client.handle_mouse(
        build_left_mouse_press(tabline_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );
    client.note_press_forwarded(resized_pane_id, MouseButton::Left);
    assert_eq!(
        client
            .selection_drag
            .map(|selection_drag| selection_drag.pane_id),
        Some(hovered_pane_id)
    );
    assert_eq!(
        client.resize_drag.map(|resize_drag| resize_drag.pane_id),
        Some(resized_pane_id)
    );
    assert_eq!(
        client
            .tabline_drag
            .map(|tabline_drag| tabline_drag.anchor_column),
        Some(tabline_point.column)
    );
    assert_eq!(
        client.mouse_capture,
        Some(MouseCapture {
            pane_id: resized_pane_id,
            button: MouseButton::Left,
        })
    );

    client.end_mouse_gestures();

    assert_eq!(client.selection_drag, None);
    assert_eq!(client.resize_drag, None);
    assert_eq!(client.tabline_drag, None);
    assert_eq!(client.mouse_capture, None);
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(1),
        "the strip peek stands"
    );
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).hovered_pane_id,
        Some(hovered_pane_id),
        "the hovered pane stands"
    );
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_drag(Point {
                column: screen_point.column + 4,
                ..screen_point
            }),
            &mouse_frame,
            advance_time_by_seconds(current_time, 3),
        ),
        Vec::new(),
        "no gesture is under way, so the drag decides nothing"
    );
}

#[test]
fn a_neighbor_across_the_gap_counts_as_adjacent() {
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let mut mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(left_pane_id),
            build_plain_mouse_pane(right_pane_id),
        ],
        Some(left_pane_id),
    );
    mouse_frame
        .session_snapshot
        .active_tab_snapshot
        .gap_cell_count = 2;
    mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[0].outer_rect =
        Rect::from_origin_and_size(
            Point { column: 0, row: 0 },
            Size {
                column_count: 40,
                row_count: 20,
            },
        );
    mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[1].outer_rect =
        Rect::from_origin_and_size(
            Point { column: 42, row: 0 },
            Size {
                column_count: 38,
                row_count: 20,
            },
        );

    assert!(
        has_border_neighbor(&mouse_frame, left_pane_id, Direction::Right),
        "the right pane starts 2 cells past column 39"
    );
    assert!(
        has_border_neighbor(&mouse_frame, right_pane_id, Direction::Left),
        "the left pane ends 2 cells before column 42"
    );

    mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[1]
        .outer_rect
        .origin
        .column = 43;

    assert!(
        !has_border_neighbor(&mouse_frame, left_pane_id, Direction::Right),
        "a 3-cell distance is not the tab's gap"
    );
    assert!(!has_border_neighbor(
        &mouse_frame,
        right_pane_id,
        Direction::Left
    ));

    mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[1]
        .outer_rect
        .origin
        .column = 41;

    assert!(
        !has_border_neighbor(&mouse_frame, left_pane_id, Direction::Right),
        "a 1-cell distance is not the tab's gap"
    );
    assert!(!has_border_neighbor(
        &mouse_frame,
        right_pane_id,
        Direction::Left
    ));
}

#[test]
fn a_neighbor_below_the_gap_counts_as_adjacent() {
    let top_pane_id = PaneId::new();
    let bottom_pane_id = PaneId::new();
    let mut mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(top_pane_id),
            build_plain_mouse_pane(bottom_pane_id),
        ],
        Some(top_pane_id),
    );
    mouse_frame
        .session_snapshot
        .active_tab_snapshot
        .gap_cell_count = 2;
    mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[0].outer_rect =
        Rect::from_origin_and_size(
            Point { column: 0, row: 0 },
            Size {
                column_count: 40,
                row_count: 20,
            },
        );
    mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[1].outer_rect =
        Rect::from_origin_and_size(
            Point { column: 0, row: 22 },
            Size {
                column_count: 40,
                row_count: 18,
            },
        );

    assert!(
        has_border_neighbor(&mouse_frame, top_pane_id, Direction::Down),
        "the bottom pane starts 2 cells past row 19"
    );
    assert!(
        has_border_neighbor(&mouse_frame, bottom_pane_id, Direction::Up),
        "the top pane ends 2 cells before row 22"
    );

    mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[1]
        .outer_rect
        .origin
        .row = 23;

    assert!(
        !has_border_neighbor(&mouse_frame, top_pane_id, Direction::Down),
        "a 3-cell distance is not the tab's gap"
    );
    assert!(!has_border_neighbor(
        &mouse_frame,
        bottom_pane_id,
        Direction::Up
    ));

    mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[1]
        .outer_rect
        .origin
        .row = 21;

    assert!(
        !has_border_neighbor(&mouse_frame, top_pane_id, Direction::Down),
        "a 1-cell distance is not the tab's gap"
    );
    assert!(!has_border_neighbor(
        &mouse_frame,
        bottom_pane_id,
        Direction::Up
    ));
}

#[test]
fn ending_the_selection_leaves_the_other_three_gestures_alone() {
    // A key that reaches the pane's program ends the highlight gesture only.
    // The border drag, the strip peek-drag and the captured pane are all still
    // under way.
    let selection_pane_id = PaneId::new();
    let captured_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(selection_pane_id),
            build_plain_mouse_pane(captured_pane_id),
        ],
        Some(selection_pane_id),
    );
    let screen_point = get_content_cell(&mouse_frame, 0);
    let divider_point = Point {
        column: 10,
        row: mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[1]
            .outer_rect
            .origin
            .row
            + 1,
    };
    let tabline_point = Point { column: 40, row: 0 };
    let current_time = Instant::now();
    let mut client = build_test_client();

    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_press(divider_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    client.handle_mouse(
        build_left_mouse_press(tabline_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );
    client.note_press_forwarded(captured_pane_id, MouseButton::Left);

    client.end_mouse_selection();

    assert_eq!(client.selection_drag, None);
    assert_eq!(
        client.resize_drag.map(|resize_drag| resize_drag.pane_id),
        Some(captured_pane_id)
    );
    assert_eq!(
        client
            .tabline_drag
            .map(|tabline_drag| tabline_drag.anchor_column),
        Some(tabline_point.column)
    );
    assert_eq!(
        client.mouse_capture,
        Some(MouseCapture {
            pane_id: captured_pane_id,
            button: MouseButton::Left,
        })
    );
}

#[test]
fn a_horizontal_wheel_over_the_tab_strip_steps_it_the_same_way() {
    // Left steps toward the first tab, right toward the last — the same two
    // steps up and down make over the strip.
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let tabline_point = Point { column: 40, row: 0 };
    let mut client = build_test_client();

    let wheel_decision = client
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Right, tabline_point),
            &mouse_frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        wheel_decision,
        WheelDecision {
            hovered_pane_id: None,
            mouse_action: None,
        },
        "the strip is this viewer's own; nothing goes to the session"
    );
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(1)
    );

    client
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Left, tabline_point),
            &mouse_frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(0)
    );

    client
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Left, tabline_point),
            &mouse_frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(0),
        "the first tab is as far left as the strip goes"
    );
}

#[test]
fn the_wheel_decision_answers_nothing_for_an_event_that_is_not_a_wheel_tick() {
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    let screen_point = get_content_cell(&mouse_frame, 0);
    let mut client = build_test_client();

    assert_eq!(
        client.handle_mouse_wheel(build_left_mouse_press(screen_point), &mouse_frame),
        None
    );
    assert_eq!(
        client.handle_mouse_wheel(build_left_mouse_drag(screen_point), &mouse_frame),
        None
    );
    assert_eq!(
        client.handle_mouse_wheel(build_left_mouse_release(screen_point), &mouse_frame),
        None
    );
    assert_eq!(
        client.handle_mouse_wheel(build_mouse_motion(screen_point), &mouse_frame),
        None
    );
}

#[test]
fn a_viewer_with_no_selection_drag_asks_the_loop_for_no_wakeup() {
    let client = build_test_client();

    assert_eq!(client.compute_next_mouse_wakeup(Instant::now()), None);
}

#[test]
fn a_scroll_step_already_due_asks_the_loop_to_wake_at_once() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let screen_point = get_content_cell(&mouse_frame, 0);
    // One row above the pane's content, which is where the edge scroll arms.
    let above_pane_point = Point {
        column: screen_point.column,
        row: 0,
    };
    let current_time = Instant::now();
    let mut client = build_test_client();

    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(above_pane_point),
        &mouse_frame,
        current_time,
    );

    assert_eq!(
        client.compute_next_mouse_wakeup(current_time + Duration::from_secs(1)),
        Some(Duration::ZERO),
        "a step already behind the clock asks for no further wait"
    );
}

#[test]
fn plain_drag_swaps_and_shift_drag_inserts_at_the_same_pane() {
    let source_pane_id = PaneId::new();
    let target_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(source_pane_id),
            build_plain_mouse_pane(target_pane_id),
        ],
        Some(source_pane_id),
    );
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let placement_snapshot = build_mouse_placement_snapshot(
        &mouse_frame,
        source_pane_id,
        LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Vertical,
            vec![
                LayoutNode::Pane(source_pane_id),
                LayoutNode::Pane(target_pane_id),
            ],
        )),
    );

    let mut client = build_test_client();
    client.visible_tab_ids = vec![active_tab_id];
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: active_tab_id,
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command: None,
    });
    client.placement_state.placement_snapshot = Some(Arc::new(placement_snapshot));
    let source_content_rect = mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[0]
        .content_rect
        .expect("the source pane has content");
    let source_point = Point {
        column: source_content_rect.origin.column + 1,
        row: source_content_rect.origin.row + source_content_rect.size.row_count - 1,
    };
    let target_point = get_content_cell(&mouse_frame, 1);
    let target_outer_rect =
        mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[1].outer_rect;
    let target_border_point = Point {
        column: target_outer_rect.origin.column,
        row: target_outer_rect.origin.row + 1,
    };
    let statusline_point = Point {
        column: 0,
        row: TEST_VIEWPORT_SIZE.row_count - 1,
    };

    assert_eq!(
        client.handle_placement_mouse(
            build_mouse_event(
                MouseKind::Press(MouseButton::Left),
                source_point,
                BindingModifierFlags::NONE,
            ),
            &mouse_frame,
            Instant::now(),
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(
        client.find_placement_target_at(source_point, &mouse_frame),
        None,
        "the dragged source cannot become its own destination"
    );
    client.handle_placement_mouse(
        build_mouse_event(
            MouseKind::Drag(MouseButton::Left),
            target_point,
            BindingModifierFlags::SHIFT,
        ),
        &mouse_frame,
        Instant::now(),
    );
    assert_eq!(
        client.find_placement_target_at(target_point, &mouse_frame),
        Some(PanePlacementTarget::Swap { target_pane_id })
    );
    assert_eq!(
        client.find_placement_target_at(target_border_point, &mouse_frame),
        Some(PanePlacementTarget::Swap { target_pane_id }),
        "the target pane border names the same pane as its content"
    );
    assert_eq!(
        client.find_placement_target_at(statusline_point, &mouse_frame),
        None,
        "the statusline does not name a pane target"
    );
    client.end_placement_drag();
    client.clear_mouse_placement_target();

    client.handle_placement_mouse(
        build_mouse_event(
            MouseKind::Press(MouseButton::Left),
            source_point,
            BindingModifierFlags::SHIFT,
        ),
        &mouse_frame,
        Instant::now(),
    );
    client.handle_placement_mouse(
        build_mouse_event(
            MouseKind::Drag(MouseButton::Left),
            target_point,
            BindingModifierFlags::NONE,
        ),
        &mouse_frame,
        Instant::now(),
    );
    assert_eq!(
        client.find_placement_target_at(target_point, &mouse_frame),
        Some(PanePlacementTarget::Split {
            destination_tab_id: active_tab_id,
            anchor: PanePlacementAnchor::Pane(target_pane_id),
            direction: Direction::Up,
        })
    );
}

#[test]
fn placement_target_uses_fixed_base_geometry_while_the_preview_moves_panes() {
    let source_pane_id = PaneId::new();
    let target_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(source_pane_id),
            build_plain_mouse_pane(target_pane_id),
        ],
        Some(source_pane_id),
    );
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let placement_snapshot = build_mouse_placement_snapshot(
        &mouse_frame,
        source_pane_id,
        LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Vertical,
            vec![
                LayoutNode::Pane(source_pane_id),
                LayoutNode::Pane(target_pane_id),
            ],
        )),
    );
    let target_point = get_content_cell(&mouse_frame, 1);
    let mut moved_preview_frame = mouse_frame.clone();
    let first_pane_slot = moved_preview_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[0]
        .clone();
    let second_pane_slot = moved_preview_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[1]
        .clone();
    moved_preview_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[0]
        .outer_rect = second_pane_slot.outer_rect;
    moved_preview_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[0]
        .content_rect = second_pane_slot.content_rect;
    moved_preview_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[1]
        .outer_rect = first_pane_slot.outer_rect;
    moved_preview_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[1]
        .content_rect = first_pane_slot.content_rect;

    let mut client = build_test_client();
    client.visible_tab_ids = vec![active_tab_id];
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: active_tab_id,
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command: None,
    });
    client.placement_state.placement_snapshot = Some(Arc::new(placement_snapshot));
    client.begin_placement_drag(target_point, false);

    assert_eq!(
        client.find_placement_target_at(target_point, &moved_preview_frame),
        Some(PanePlacementTarget::Swap { target_pane_id })
    );
}

#[test]
fn cross_tab_mouse_target_uses_the_destination_pane_under_the_pointer() {
    let source_pane_id = PaneId::new();
    let destination_first_pane_id = PaneId::new();
    let destination_second_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(source_pane_id),
            build_plain_mouse_pane(PaneId::new()),
        ],
        Some(source_pane_id),
    );
    let source_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let destination_tab_id = TabId::new();
    let placement_snapshot = build_cross_tab_mouse_placement_snapshot(
        &mouse_frame,
        source_pane_id,
        destination_tab_id,
        [destination_first_pane_id, destination_second_pane_id],
    );
    let target_point = get_content_cell(&mouse_frame, 1);
    let mut client = build_test_client();
    client.visible_tab_ids = vec![source_tab_id, destination_tab_id];
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id,
        destination_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command: None,
    });
    client.placement_state.placement_snapshot = Some(Arc::new(placement_snapshot));
    client.begin_placement_drag(target_point, false);

    assert_eq!(
        client.find_placement_target_at(target_point, &mouse_frame),
        Some(PanePlacementTarget::Swap {
            target_pane_id: destination_second_pane_id,
        })
    );
}

#[test]
fn shifted_drag_across_a_tab_submits_insertion_when_drop_matches_press_position() {
    let source_pane_id = PaneId::new();
    let destination_first_pane_id = PaneId::new();
    let destination_second_pane_id = PaneId::new();
    let source_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(PaneId::new()),
            build_plain_mouse_pane(source_pane_id),
        ],
        Some(source_pane_id),
    );
    let source_tab_id = source_frame.client_snapshot.active_tab_id;
    let destination_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(destination_first_pane_id),
            build_plain_mouse_pane(destination_second_pane_id),
        ],
        Some(destination_first_pane_id),
    );
    let destination_tab_id = destination_frame.client_snapshot.active_tab_id;
    let placement_snapshot = build_cross_tab_mouse_placement_snapshot(
        &source_frame,
        source_pane_id,
        destination_tab_id,
        [destination_first_pane_id, destination_second_pane_id],
    );
    let source_point = get_content_cell(&source_frame, 1);
    let destination_point = get_content_cell(&destination_frame, 1);
    assert_eq!(source_point, destination_point);
    let hover_started_at = Instant::now();
    let mut client = build_test_client();
    client.visible_tab_ids = vec![source_tab_id, destination_tab_id];
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id,
        destination_tab_id: source_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command: None,
    });

    assert_eq!(
        client.handle_placement_mouse(
            build_mouse_event(
                MouseKind::Press(MouseButton::Left),
                source_point,
                BindingModifierFlags::SHIFT,
            ),
            &source_frame,
            hover_started_at,
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(
        client.handle_placement_mouse(
            build_mouse_event(
                MouseKind::Drag(MouseButton::Left),
                Point { column: 2, row: 0 },
                BindingModifierFlags::NONE,
            ),
            &source_frame,
            hover_started_at,
        ),
        Some(PlacementInputAction::Consumed)
    );
    client.update_placement_tab_hover(Some(destination_tab_id), hover_started_at);
    assert_eq!(
        client.expire_placement_tab_hover(
            hover_started_at + crate::PLACEMENT_TAB_HOVER_DELAY_DURATION,
        ),
        Some((source_pane_id, destination_tab_id))
    );
    client.placement_state.placement_snapshot = Some(Arc::new(placement_snapshot));
    assert_eq!(
        client.handle_placement_mouse(
            build_mouse_event(
                MouseKind::Drag(MouseButton::Left),
                destination_point,
                BindingModifierFlags::NONE,
            ),
            &destination_frame,
            hover_started_at,
        ),
        Some(PlacementInputAction::Consumed)
    );
    let placement_target = PanePlacementTarget::Split {
        destination_tab_id,
        anchor: PanePlacementAnchor::Pane(destination_second_pane_id),
        direction: Direction::Up,
    };
    assert_eq!(
        client.get_placement_target(),
        Some(placement_target.clone())
    );

    let release_action = client.handle_placement_mouse(
        build_mouse_event(
            MouseKind::Release(MouseButton::Left),
            destination_point,
            BindingModifierFlags::NONE,
        ),
        &destination_frame,
        hover_started_at,
    );
    assert_eq!(
        release_action,
        Some(crate::tests::build_expected_submit_placement(
            &client,
            Command::PlacePane(koshi_core::command::PlacePaneArgs {
                source_pane_id,
                placement_target,
                expected_placement_revision: Some(koshi_core::command::PlacementRevision {
                    session_revision: 0,
                    client_revision: 0,
                }),
            }),
        ))
    );
}

#[test]
fn stack_header_drag_selects_group_for_insertion_and_pane_for_swap() {
    let source_pane_id = PaneId::new();
    let expanded_stack_pane_id = PaneId::new();
    let collapsed_stack_pane_id = PaneId::new();
    let mut mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(source_pane_id),
            build_plain_mouse_pane(expanded_stack_pane_id),
            build_plain_mouse_pane(collapsed_stack_pane_id),
        ],
        Some(source_pane_id),
    );
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let stack_header_rect = Rect::from_origin_and_size(
        Point { column: 0, row: 21 },
        Size {
            column_count: TEST_VIEWPORT_SIZE.column_count,
            row_count: 1,
        },
    );
    mouse_frame
        .session_snapshot
        .active_tab_snapshot
        .stack_headers = vec![StackHeader {
        pane_id: collapsed_stack_pane_id,
        header_rect: stack_header_rect,
        member_index: 1,
        member_count: 2,
    }];
    let placement_snapshot = build_mouse_placement_snapshot(
        &mouse_frame,
        source_pane_id,
        LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Vertical,
            vec![
                LayoutNode::Pane(source_pane_id),
                LayoutNode::Split(SplitNode::from_stacked_pane_ids(
                    vec![expanded_stack_pane_id, collapsed_stack_pane_id],
                    0,
                )),
            ],
        )),
    );

    let mut client = build_test_client();
    client.visible_tab_ids = vec![active_tab_id];
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: active_tab_id,
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Up,
        placement_target: None,
        pending_placement_command: None,
    });
    client.placement_state.placement_snapshot = Some(Arc::new(placement_snapshot));
    let source_point = get_content_cell(&mouse_frame, 0);
    let stack_header_point = Point { column: 2, row: 22 };

    assert_eq!(
        client.handle_placement_mouse(
            build_mouse_event(
                MouseKind::Press(MouseButton::Left),
                source_point,
                BindingModifierFlags::SHIFT,
            ),
            &mouse_frame,
            Instant::now(),
        ),
        Some(PlacementInputAction::Consumed)
    );
    client.handle_placement_mouse(
        build_mouse_event(
            MouseKind::Drag(MouseButton::Left),
            stack_header_point,
            BindingModifierFlags::NONE,
        ),
        &mouse_frame,
        Instant::now(),
    );

    assert_eq!(
        client.find_placement_target_at(stack_header_point, &mouse_frame),
        Some(PanePlacementTarget::Split {
            destination_tab_id: active_tab_id,
            anchor: PanePlacementAnchor::Group(vec![
                expanded_stack_pane_id,
                collapsed_stack_pane_id,
            ]),
            direction: Direction::Down,
        })
    );

    client.end_placement_drag();
    client.clear_mouse_placement_target();
    client.handle_placement_mouse(
        build_mouse_event(
            MouseKind::Press(MouseButton::Left),
            source_point,
            BindingModifierFlags::NONE,
        ),
        &mouse_frame,
        Instant::now(),
    );
    client.handle_placement_mouse(
        build_mouse_event(
            MouseKind::Drag(MouseButton::Left),
            stack_header_point,
            BindingModifierFlags::NONE,
        ),
        &mouse_frame,
        Instant::now(),
    );

    assert_eq!(
        client.find_placement_target_at(stack_header_point, &mouse_frame),
        Some(PanePlacementTarget::Swap {
            target_pane_id: collapsed_stack_pane_id,
        })
    );
}

#[test]
fn a_stack_header_press_in_pane_placement_mode_picks_that_collapsed_member_as_the_source() {
    let plain_pane_id = PaneId::new();
    let open_stack_pane_id = PaneId::new();
    let collapsed_stack_pane_id = PaneId::new();
    let mut mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(plain_pane_id),
            build_plain_mouse_pane(open_stack_pane_id),
            build_plain_mouse_pane(collapsed_stack_pane_id),
        ],
        Some(plain_pane_id),
    );
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    mouse_frame
        .session_snapshot
        .active_tab_snapshot
        .stack_headers = vec![StackHeader {
        pane_id: collapsed_stack_pane_id,
        header_rect: Rect::from_origin_and_size(
            Point { column: 0, row: 21 },
            Size {
                column_count: TEST_VIEWPORT_SIZE.column_count,
                row_count: 1,
            },
        ),
        member_index: 1,
        member_count: 2,
    }];
    let mut client = build_test_client();
    client.active_tab_id = Some(active_tab_id);
    client.visible_tab_ids = vec![active_tab_id];
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: plain_pane_id,
        source_tab_id: active_tab_id,
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command: None,
    });
    let stack_header_point = Point { column: 2, row: 22 };

    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(stack_header_point),
            &mouse_frame,
            Instant::now(),
        ),
        Some(PlacementInputAction::ReadPlacement {
            pane_id_to_focus: Some(collapsed_stack_pane_id),
            source_pane_id: collapsed_stack_pane_id,
            destination_tab_id: active_tab_id,
        })
    );
    client.set_placement_revisions(1, 1);

    assert_eq!(
        client.get_placement_source_pane_id(),
        Some(collapsed_stack_pane_id)
    );
    assert!(client.update_placement_drag_movement(Point { column: 40, row: 3 }));
}

/// A client on tab `active_tab_id` in placement mode that places
/// `source_pane_id` and previews the tab `mouse_frame` shows, with
/// `placement_target` selected.
fn build_client_previewing_mouse_frame_tab(
    mouse_frame: &MouseFrame,
    active_tab_id: TabId,
    source_pane_id: PaneId,
    placement_target: Option<PanePlacementTarget>,
) -> Client {
    let previewed_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_test_client();
    client.active_tab_id = Some(active_tab_id);
    client.visible_tab_ids = vec![active_tab_id, previewed_tab_id];
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: active_tab_id,
        destination_tab_id: previewed_tab_id,
        placement_direction: Direction::Right,
        placement_target,
        pending_placement_command: None,
    });
    client
}

#[test]
fn a_pane_press_on_a_previewed_tab_picks_that_pane_without_focusing_it() {
    let source_pane_id = PaneId::new();
    let previewed_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(source_pane_id),
            build_plain_mouse_pane(previewed_pane_id),
        ],
        Some(source_pane_id),
    );
    let previewed_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client =
        build_client_previewing_mouse_frame_tab(&mouse_frame, TabId::new(), source_pane_id, None);
    let previewed_pane_content_point = Point { column: 2, row: 14 };

    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(previewed_pane_content_point),
            &mouse_frame,
            Instant::now(),
        ),
        Some(PlacementInputAction::ReadPlacement {
            pane_id_to_focus: None,
            source_pane_id: previewed_pane_id,
            destination_tab_id: previewed_tab_id,
        })
    );
    assert_eq!(
        client
            .placement_state
            .placement_mode
            .as_ref()
            .map(|placement_mode| placement_mode.source_tab_id),
        Some(previewed_tab_id)
    );
}

#[test]
fn a_press_on_the_source_pane_in_a_previewed_tab_keeps_its_tab_and_target() {
    let source_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(source_pane_id),
            build_plain_mouse_pane(PaneId::new()),
        ],
        Some(source_pane_id),
    );
    let placement_target = PanePlacementTarget::Split {
        destination_tab_id: mouse_frame.client_snapshot.active_tab_id,
        anchor: PanePlacementAnchor::Tab,
        direction: Direction::Right,
    };
    let mut client = build_client_previewing_mouse_frame_tab(
        &mouse_frame,
        TabId::new(),
        source_pane_id,
        Some(placement_target),
    );
    let placement_mode_before_press = client.placement_state.placement_mode.clone();
    let source_pane_content_point = Point { column: 2, row: 3 };

    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(source_pane_content_point),
            &mouse_frame,
            Instant::now(),
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(
        client.placement_state.placement_mode,
        placement_mode_before_press
    );
}

#[test]
fn shift_drag_into_a_group_gap_selects_the_group_anchor() {
    let source_pane_id = PaneId::new();
    let group_left_pane_id = PaneId::new();
    let group_right_pane_id = PaneId::new();
    let remaining_pane_id = PaneId::new();
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            LayoutNode::Pane(source_pane_id),
            LayoutNode::Split(SplitNode::with_equal_weights(
                SplitDirection::Horizontal,
                vec![
                    LayoutNode::Pane(group_left_pane_id),
                    LayoutNode::Pane(group_right_pane_id),
                ],
            )),
            LayoutNode::Pane(remaining_pane_id),
        ],
    ));
    let mut mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(source_pane_id),
            build_plain_mouse_pane(group_left_pane_id),
            build_plain_mouse_pane(group_right_pane_id),
            build_plain_mouse_pane(remaining_pane_id),
        ],
        Some(source_pane_id),
    );
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let pane_sizing = PaneSizing {
        gap_cell_count: 1,
        ..PaneSizing::default()
    };
    let tab_rect =
        Rect::from_size_at_origin(mouse_frame.session_snapshot.active_tab_snapshot.tab_size);
    let layout_solution =
        solve_layout_with_mode(&layout_tree, LayoutMode::Tiled, tab_rect, pane_sizing);
    for pane_slot in &mut mouse_frame.session_snapshot.active_tab_snapshot.pane_slots {
        let pane_rect = layout_solution
            .pane_rects
            .iter()
            .find(|(pane_id, _)| *pane_id == pane_slot.pane_id)
            .map(|(_, pane_rect)| *pane_rect)
            .expect("the layout solves every test pane");
        pane_slot.outer_rect = pane_rect;
        pane_slot.content_rect = Some(Rect::from_origin_and_size(
            Point {
                column: pane_rect.origin.column.saturating_add(1),
                row: pane_rect.origin.row.saturating_add(1),
            },
            Size {
                column_count: pane_rect.size.column_count.saturating_sub(2),
                row_count: pane_rect.size.row_count.saturating_sub(2),
            },
        ));
    }
    let mut placement_snapshot =
        build_mouse_placement_snapshot(&mouse_frame, source_pane_id, layout_tree);
    placement_snapshot.pane_sizing.gap_cell_count = pane_sizing.gap_cell_count;
    let group_left_rect = layout_solution
        .pane_rects
        .iter()
        .find(|(pane_id, _)| *pane_id == group_left_pane_id)
        .map(|(_, pane_rect)| *pane_rect)
        .expect("the left group pane is solved");
    let group_gap_point = Point {
        column: group_left_rect
            .origin
            .column
            .saturating_add(group_left_rect.size.column_count),
        row: group_left_rect.origin.row.saturating_add(1),
    };
    let mut client = build_test_client();
    client.visible_tab_ids = vec![active_tab_id];
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: active_tab_id,
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command: None,
    });
    client.placement_state.placement_snapshot = Some(Arc::new(placement_snapshot));

    let source_point = get_content_cell(&mouse_frame, 0);
    assert_eq!(
        client.handle_placement_mouse(
            build_mouse_event(
                MouseKind::Press(MouseButton::Left),
                source_point,
                BindingModifierFlags::SHIFT
            ),
            &mouse_frame,
            Instant::now(),
        ),
        Some(PlacementInputAction::Consumed)
    );
    client.handle_placement_mouse(
        build_mouse_event(
            MouseKind::Drag(MouseButton::Left),
            Point {
                column: group_gap_point.column,
                row: group_gap_point.row.saturating_add(1),
            },
            BindingModifierFlags::NONE,
        ),
        &mouse_frame,
        Instant::now(),
    );

    assert_eq!(
        client.find_placement_target_at(
            Point {
                column: group_gap_point.column,
                row: group_gap_point.row.saturating_add(1),
            },
            &mouse_frame,
        ),
        Some(PanePlacementTarget::Split {
            destination_tab_id: active_tab_id,
            anchor: PanePlacementAnchor::Group(vec![group_left_pane_id, group_right_pane_id,]),
            direction: Direction::Up,
        })
    );
}

#[test]
fn a_non_left_placement_release_cancels_mouse_capture_without_submitting() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_test_client();
    client.visible_tab_ids = vec![tab_id];
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: pane_id,
        source_tab_id: tab_id,
        destination_tab_id: tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command: None,
    });
    client.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilCancelled;
    client.begin_placement_drag(Point { column: 4, row: 5 }, false);

    assert_eq!(
        client.handle_placement_mouse(
            MouseInput {
                mouse_kind: MouseKind::Release(MouseButton::Right),
                position: Point {
                    column: 12,
                    row: 14
                },
                modifier_flags: BindingModifierFlags::NONE,
            },
            &mouse_frame,
            Instant::now(),
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(client.placement_state.placement_drag, None);
    assert!(client.is_placement_mode_active());
    assert!(!client.is_placement_confirmation_pending());
}
