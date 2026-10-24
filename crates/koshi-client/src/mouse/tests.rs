//! Tests for what the client decides a mouse event means: the wheel's
//! precedence over one pane, which pane a tick targets, and the ticks that
//! decide nothing; what a press, a drag and a release do over each region —
//! focusing a tab or a pane, peeking the tab strip, moving a border, and
//! highlighting text; the run of clicks that picks a highlight's shape; the
//! capture that keeps a gesture with the pane its press landed on; plain pane
//! drags swap, Shift-pane drags insert, edge scroll a held text drag drives on
//! the clock; the top border hover that shows a tiled pane's handle, the
//! handle cells that mark it, the panes that show none, and the press, drag,
//! release or wheel tick that hides it; and the gestures a new frame ends. For floating panes: the
//! grab-handle move, the pin toggle, minimize and close buttons, a corner
//! resize, floating pane move mode, and dock/undock mode.
//!
//! Each test builds a frame by hand: one tab, its tiled panes and its floating
//! panes, and no chrome beyond the tabline and hint bar.

use super::*;

use koshi_config::layer::{PartialKoshiConfig, PartialMouseConfig};
use koshi_config::types::WheelScroll;
use koshi_core::action::ClientActionKind;
use koshi_core::command::{PanePlacementTarget, PlacePaneArgs};
use koshi_core::geometry::{Point, Rect, Size, SplitDirection};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::key::{BindingModifierFlags, Key, KeyChord, NamedKey};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseTracking};
use koshi_ipc::placement::PanePlacementPreviewTarget;
use koshi_layout::mode::LayoutMode;
use koshi_layout::regions::{solve_region_rects, Edge, RegionGeometry};
use koshi_layout::solver::{solve_layout_with_mode, PaneSizing, StackHeader};
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_renderer::placement::PlacementPreviewError;
use koshi_renderer::snapshot::{
    ClientSnapshot, CommittedRegions, FloatingPaneSlot, LinkStatus, MousePane, PaneSlot,
    PlacementSnapshot, PlacementTabSnapshot, Reconnecting, RenderSnapshot, SessionSnapshot,
    TabMetadata, TabSnapshot,
};
use koshi_test_support::fixtures::build_cell_rect;

use crate::input::KeyOutcome;
use crate::tests::{
    apply_frame_keeping_view, build_core_bound_action, build_expected_confirm_placement,
    build_test_client, build_test_placement_snapshot, float_placement_source_pane,
    get_source_tab_snapshot_mut, hold_placement_snapshot, NO_ROOM_TAB_SIZE, TEST_VIEWPORT_SIZE,
};
use crate::{
    Client, DockUndockMode, PlacementDrag, PlacementMode, PlacementModeLifetime, TiledPaneSelection,
};

/// A client on the stock settings whose mouse-select mode is on.
fn build_mouse_select_client() -> Client {
    let mut client = build_test_client();
    client.set_mouse_selection_enabled(true);
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
pub(crate) fn build_plain_mouse_pane(pane_id: PaneId) -> MousePane {
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
pub(crate) fn build_mouse_frame(
    mouse_panes: &[MousePane],
    focused_pane_id: Option<PaneId>,
) -> MouseFrame {
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
            floating_pane_slots: Vec::new(),
            floating_pane_ids: Vec::new(),
        },
        mouse_panes: mouse_panes.to_vec(),
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            viewport_size: TEST_VIEWPORT_SIZE,
            active_tab_id: tab_id,
            focused_pane_id,
            active_tab_focused_pane_id: focused_pane_id,
            lock_mode: LockMode::Normal,
            last_applied_lock_command_id: None,
            last_applied_floating_pane_move_command_id: None,
            last_applied_placement_command_id: None,
            is_mouse_selection_enabled: false,
        },
        committed_regions: CommittedRegions::build_core(TEST_VIEWPORT_SIZE, 0),
        viewer_chrome: ViewerChrome::default(),
    }
}

/// `mouse_frame` painted again with the chrome `client` builds for it now, as
/// the attach loop paints at the end of each pass.
fn repaint_mouse_frame(client: &Client, mouse_frame: &MouseFrame) -> MouseFrame {
    MouseFrame {
        viewer_chrome: client.build_viewer_chrome(mouse_frame.client_snapshot.active_tab_id),
        ..mouse_frame.clone()
    }
}

/// A frame holding one plain terminal pane, focused.
fn build_one_pane_mouse_frame(mouse_pane: MousePane) -> MouseFrame {
    let pane_id = mouse_pane.pane_id;
    build_mouse_frame(&[mouse_pane], Some(pane_id))
}

/// The action that forwards `mouse_input` to `pane_id`'s program at the pane's
/// 0-based content cell `pane_local_cell`.
fn build_forwarded_mouse_action(
    pane_id: PaneId,
    mouse_input: MouseInput,
    pane_local_cell: Point,
) -> MouseAction {
    MouseAction::Forward {
        pane_id,
        mouse_kind: mouse_input.mouse_kind,
        modifier_flags: mouse_input.modifier_flags,
        pane_local_cell,
    }
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
) -> PlacementSnapshot {
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut placement_snapshot =
        build_test_placement_snapshot(source_pane_id, active_tab_id, active_tab_id);
    placement_snapshot.destination_tab_snapshot = None;
    placement_snapshot.swap_tab_sizes.clear();
    let frame_tab_snapshot = mouse_frame.session_snapshot.active_tab_snapshot.clone();
    placement_snapshot.insertion_tab_sizes.destination_tab_size = Some(frame_tab_snapshot.tab_size);
    let source_tab_snapshot = get_source_tab_snapshot_mut(&mut placement_snapshot);
    source_tab_snapshot.layout_tree = layout_tree;
    source_tab_snapshot.tab_snapshot = frame_tab_snapshot;
    placement_snapshot
}

/// Build a cross-tab placement snapshot with destination slots matching a frame.
fn build_cross_tab_mouse_placement_snapshot(
    mouse_frame: &MouseFrame,
    source_pane_id: PaneId,
    destination_tab_id: TabId,
    destination_pane_ids: [PaneId; 2],
) -> PlacementSnapshot {
    let mut placement_snapshot = build_mouse_placement_snapshot(
        mouse_frame,
        source_pane_id,
        LayoutNode::Pane(source_pane_id),
    );
    get_source_tab_snapshot_mut(&mut placement_snapshot)
        .tab_snapshot
        .pane_slots
        .retain(|pane_slot| pane_slot.pane_id == source_pane_id);

    let frame_tab_snapshot = &mouse_frame.session_snapshot.active_tab_snapshot;
    let destination_pane_slots = frame_tab_snapshot
        .pane_slots
        .iter()
        .zip(destination_pane_ids)
        .map(|(pane_slot, destination_pane_id)| PaneSlot {
            pane_id: destination_pane_id,
            ..*pane_slot
        })
        .collect();
    let destination_tab_snapshot = PlacementTabSnapshot {
        layout_tree: LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Vertical,
            destination_pane_ids
                .into_iter()
                .map(LayoutNode::Pane)
                .collect(),
        )),
        focus_history: Vec::new(),
        tab_snapshot: TabSnapshot {
            tab_id: destination_tab_id,
            tab_name: "destination".to_owned(),
            pane_slots: destination_pane_slots,
            tab_size: frame_tab_snapshot.tab_size,
            stack_headers: Vec::new(),
            layout_mode: frame_tab_snapshot.layout_mode,
            is_every_pane_suppressed: frame_tab_snapshot.is_every_pane_suppressed,
            gap_cell_count: frame_tab_snapshot.gap_cell_count,
        },
    };
    placement_snapshot.destination_tab_id = destination_tab_id;
    placement_snapshot.destination_tab_snapshot = Some(destination_tab_snapshot);
    let placement_tab_sizes = koshi_layout::placement::PlacementTabSizes {
        destination_tab_size: Some(frame_tab_snapshot.tab_size),
        source_tab_size: Some(frame_tab_snapshot.tab_size),
    };
    placement_snapshot.swap_tab_sizes = destination_pane_ids
        .into_iter()
        .map(|target_pane_id| koshi_layout::placement::SwapTabSizes {
            target_pane_id,
            tab_sizes: placement_tab_sizes,
        })
        .collect();
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
        wheel_scroll: Some(WheelScroll::Ignore),
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
        Some(build_forwarded_mouse_action(
            pane_id,
            wheel_tick,
            Point { column: 1, row: 1 }
        ))
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

/// A one-pane frame whose tab strip holds the frame's own tab and seven more
/// named `long tab name number 1` to `long tab name number 7`. The 22-cell
/// names overflow the 80-column strip, so it scrolls.
fn build_overflowing_tab_strip_mouse_frame() -> MouseFrame {
    let mut mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    for tab_index in 1..8 {
        mouse_frame
            .session_snapshot
            .tabs_metadata
            .push(TabMetadata {
                tab_id: TabId::new(),
                tab_name: format!("long tab name number {tab_index}"),
                tab_index,
                is_active: false,
            });
    }
    mouse_frame
}

/// Send `client` two wheel-down ticks over the tab strip of `mouse_frame`, with
/// no repaint between them.
fn scroll_tab_strip_down_twice(client: &mut Client, mouse_frame: &MouseFrame) {
    for _ in 0..2 {
        client
            .handle_mouse_wheel(
                build_mouse_wheel(ScrollDirection::Down, Point { column: 40, row: 0 }),
                mouse_frame,
            )
            .expect("a wheel tick decides");
    }
}

#[test]
fn two_ticks_over_the_tabline_before_a_repaint_step_the_strip_twice() {
    let mouse_frame = build_overflowing_tab_strip_mouse_frame();
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_test_client();

    scroll_tab_strip_down_twice(&mut client, &mouse_frame);

    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(2)
    );
}

#[test]
fn a_tab_strip_drag_before_a_repaint_steps_from_the_strip_the_ticks_scrolled_to() {
    let mouse_frame = build_overflowing_tab_strip_mouse_frame();
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_test_client();
    let current_time = Instant::now();
    scroll_tab_strip_down_twice(&mut client, &mouse_frame);

    // Screen (0, 0) is the session block, which hit-tests as the bare tab strip.
    client.handle_mouse(
        build_left_mouse_press(Point { column: 0, row: 0 }),
        &mouse_frame,
        current_time,
    );
    // Six cells right of the anchor is one step toward the first tab.
    client.handle_mouse(
        build_left_mouse_drag(Point { column: 6, row: 0 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );

    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(1)
    );
}

/// The column of the cell on row `0` of `mouse_frame` that hit-tests as
/// `scroll_arrow_region`.
fn find_painted_scroll_arrow_column(
    mouse_frame: &MouseFrame,
    scroll_arrow_region: HitRegion,
) -> u16 {
    (0..mouse_frame.client_snapshot.viewport_size.column_count)
        .find(|&column| {
            resolve_hit_region(mouse_frame.build_frame_layout(), Point { column, row: 0 })
                == scroll_arrow_region
        })
        .expect("the painted tab strip draws the arrow")
}

#[test]
fn a_right_arrow_click_before_a_repaint_steps_from_the_strip_the_ticks_scrolled_to() {
    let mouse_frame = build_overflowing_tab_strip_mouse_frame();
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let right_scroll_arrow_column =
        find_painted_scroll_arrow_column(&mouse_frame, HitRegion::TablineScrollRight);
    let mut client = build_test_client();
    scroll_tab_strip_down_twice(&mut client, &mouse_frame);

    let press_actions = client.handle_mouse(
        build_left_mouse_press(Point {
            column: right_scroll_arrow_column,
            row: 0,
        }),
        &mouse_frame,
        Instant::now(),
    );

    assert_eq!(press_actions, Vec::new());
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(3)
    );
}

#[test]
fn a_left_arrow_click_before_a_repaint_steps_from_the_strip_the_ticks_scrolled_to() {
    let mut mouse_frame = build_overflowing_tab_strip_mouse_frame();
    mouse_frame.viewer_chrome.tabline_offset = Some(2);
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let left_scroll_arrow_column =
        find_painted_scroll_arrow_column(&mouse_frame, HitRegion::TablineScrollLeft);
    let mut client = build_test_client();
    scroll_tab_strip_down_twice(&mut client, &mouse_frame);
    scroll_tab_strip_down_twice(&mut client, &mouse_frame);

    let press_actions = client.handle_mouse(
        build_left_mouse_press(Point {
            column: left_scroll_arrow_column,
            row: 0,
        }),
        &mouse_frame,
        Instant::now(),
    );

    assert_eq!(press_actions, Vec::new());
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(3)
    );
}

#[test]
fn a_placement_drag_onto_the_right_arrow_before_a_repaint_steps_from_the_strip_the_ticks_scrolled_to(
) {
    let mouse_frame = build_overflowing_tab_strip_mouse_frame();
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let source_pane_id = mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[0].pane_id;
    let right_scroll_arrow_column =
        find_painted_scroll_arrow_column(&mouse_frame, HitRegion::TablineScrollRight);
    let mut client = build_test_client();
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(active_tab_id),
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    for _ in 0..2 {
        assert_eq!(
            client.handle_placement_mouse(
                build_mouse_wheel(ScrollDirection::Down, Point { column: 40, row: 0 }),
                &mouse_frame,
                Instant::now(),
            ),
            Some(PlacementInputAction::Consumed)
        );
    }

    let drag_action = client.handle_placement_mouse(
        build_left_mouse_drag(Point {
            column: right_scroll_arrow_column,
            row: 0,
        }),
        &mouse_frame,
        Instant::now(),
    );

    assert_eq!(drag_action, Some(PlacementInputAction::Consumed));
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(3)
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

/// A `mouse_kind` event at `position`, with `modifier_flags` held.
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
pub(crate) fn build_left_mouse_press(position: Point) -> MouseInput {
    build_mouse_event(
        MouseKind::Press(MouseButton::Left),
        position,
        BindingModifierFlags::NONE,
    )
}

/// A left drag to `position`.
pub(crate) fn build_left_mouse_drag(position: Point) -> MouseInput {
    build_mouse_event(
        MouseKind::Drag(MouseButton::Left),
        position,
        BindingModifierFlags::NONE,
    )
}

/// A left release at `position`.
pub(crate) fn build_left_mouse_release(position: Point) -> MouseInput {
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

/// The single `SetSelection` in `mouse_actions`, or `None` when it holds none.
fn find_set_selection_action(mouse_actions: &[MouseAction]) -> Option<SetSelectionArgs> {
    mouse_actions
        .iter()
        .find_map(|mouse_action| match mouse_action {
            MouseAction::Command(command) => match command.as_ref() {
                Command::Visual(VisualCommand::SetSelection(command_args)) => Some(*command_args),
                _ => None,
            },
            _ => None,
        })
}

/// The single `Copy` in `mouse_actions`, or `None` when it holds none.
fn find_copy_action(mouse_actions: &[MouseAction]) -> Option<CopyArgs> {
    mouse_actions
        .iter()
        .find_map(|mouse_action| match mouse_action {
            MouseAction::Command(command) => match command.as_ref() {
                Command::Visual(VisualCommand::Copy(command_args)) => Some(*command_args),
                _ => None,
            },
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
        vec![build_forwarded_mouse_action(
            pane_id,
            build_left_mouse_press(screen_point),
            Point { column: 1, row: 1 }
        )]
    );

    // Row 0 is the tabline — off the pane entirely. The capture still routes it.
    let outside_point = Point { column: 0, row: 0 };
    let drag_actions = client.handle_mouse(
        build_left_mouse_drag(outside_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    assert_eq!(
        drag_actions,
        vec![build_forwarded_mouse_action(
            pane_id,
            build_left_mouse_drag(outside_point),
            Point { column: 0, row: 0 }
        )],
        "the held button keeps the gesture on the pane it pressed"
    );

    let release_actions = client.handle_mouse(
        build_left_mouse_release(outside_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );
    assert_eq!(
        release_actions,
        vec![build_forwarded_mouse_action(
            pane_id,
            build_left_mouse_release(outside_point),
            Point { column: 0, row: 0 }
        )]
    );
    assert_eq!(
        client.take_mouse_capture_release(),
        None,
        "the release reached the program, so none is owed"
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
fn a_press_of_the_captured_button_after_a_lost_release_releases_it_at_its_last_cell_first() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.mouse_tracking = MouseTracking::ButtonMotion;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);
    let screen_point = get_content_cell(&mouse_frame, 0);
    let drag_point = Point {
        column: screen_point.column + 3,
        ..screen_point
    };
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(drag_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );

    // The release never arrived.
    let press_actions = client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );

    assert_eq!(
        press_actions,
        vec![
            MouseAction::Forward {
                pane_id,
                mouse_kind: MouseKind::Release(MouseButton::Left),
                modifier_flags: BindingModifierFlags::NONE,
                pane_local_cell: Point { column: 4, row: 1 },
            },
            build_forwarded_mouse_action(
                pane_id,
                build_left_mouse_press(screen_point),
                Point { column: 1, row: 1 }
            ),
        ],
        "the program hears the old gesture end where its last drag was, then the new press"
    );
    assert_eq!(
        client.mouse_capture,
        Some(MouseCapture {
            pane_id,
            button: MouseButton::Left,
            last_pane_local_cell: Point { column: 1, row: 1 },
        })
    );
}

#[test]
fn a_press_of_the_captured_button_on_koshis_chrome_releases_the_capture() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.mouse_tracking = MouseTracking::ButtonMotion;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);
    // The bare tab strip, past the one tab's ribbon.
    let tabline_point = Point { column: 40, row: 0 };
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(get_content_cell(&mouse_frame, 0)),
        &mouse_frame,
        current_time,
    );

    // The release never arrived, and the next left press lands on the strip.
    let press_actions = client.handle_mouse(
        build_left_mouse_press(tabline_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );

    assert_eq!(
        press_actions,
        vec![MouseAction::Forward {
            pane_id,
            mouse_kind: MouseKind::Release(MouseButton::Left),
            modifier_flags: BindingModifierFlags::NONE,
            pane_local_cell: Point { column: 1, row: 1 },
        }]
    );
    assert_eq!(client.mouse_capture, None);
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_release(tabline_point),
            &mouse_frame,
            advance_time_by_seconds(current_time, 2)
        ),
        Vec::new(),
        "the release ends the strip's peek-drag and reaches no program"
    );
}

#[test]
fn a_forwarded_press_of_another_button_releases_the_held_capture_first() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.mouse_tracking = MouseTracking::ButtonMotion;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);
    let screen_point = get_content_cell(&mouse_frame, 0);
    let right_point = Point {
        column: screen_point.column + 2,
        ..screen_point
    };
    let right_press = build_mouse_event(
        MouseKind::Press(MouseButton::Right),
        right_point,
        BindingModifierFlags::NONE,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );

    assert_eq!(
        client.handle_mouse(
            right_press,
            &mouse_frame,
            advance_time_by_seconds(current_time, 1)
        ),
        vec![
            MouseAction::Forward {
                pane_id,
                mouse_kind: MouseKind::Release(MouseButton::Left),
                modifier_flags: BindingModifierFlags::NONE,
                pane_local_cell: Point { column: 1, row: 1 },
            },
            build_forwarded_mouse_action(pane_id, right_press, Point { column: 3, row: 1 }),
        ],
        "one capture is held at a time, so the left one ends before the right press"
    );
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_release(right_point),
            &mouse_frame,
            advance_time_by_seconds(current_time, 2)
        ),
        vec![MouseAction::Forward {
            pane_id,
            mouse_kind: MouseKind::Release(MouseButton::Right),
            modifier_flags: BindingModifierFlags::NONE,
            pane_local_cell: Point { column: 3, row: 1 },
        }],
        "the next release ends the right capture and carries its button"
    );
    assert_eq!(
        client.handle_mouse(
            build_mouse_event(
                MouseKind::Release(MouseButton::Right),
                right_point,
                BindingModifierFlags::NONE,
            ),
            &mouse_frame,
            advance_time_by_seconds(current_time, 3)
        ),
        Vec::new(),
        "every press the program saw already has its release"
    );
}

#[test]
fn a_capture_whose_pane_leaves_the_frame_releases_on_the_next_event() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.mouse_tracking = MouseTracking::ButtonMotion;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(get_content_cell(&mouse_frame, 0)),
        &mouse_frame,
        current_time,
    );

    // The pane is hidden: the next painted frame does not draw it.
    let replacement_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    client.drop_gestures_the_frame_ended(&replacement_frame);
    assert_eq!(client.mouse_capture, None);
    let mouse_actions = client.handle_mouse(
        build_mouse_motion(get_content_cell(&replacement_frame, 0)),
        &replacement_frame,
        advance_time_by_seconds(current_time, 1),
    );

    assert_eq!(
        mouse_actions,
        vec![MouseAction::Forward {
            pane_id,
            mouse_kind: MouseKind::Release(MouseButton::Left),
            modifier_flags: BindingModifierFlags::NONE,
            pane_local_cell: Point { column: 1, row: 1 },
        }]
    );
    assert_eq!(client.mouse_capture, None);
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
        vec![build_forwarded_mouse_action(
            focused_pane_id,
            build_mouse_motion(inside_point),
            Point { column: 1, row: 1 }
        )],
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
        vec![build_forwarded_mouse_action(
            pane_id,
            build_mouse_motion(Point { column: 21, row: 2 }),
            Point { column: 0, row: 0 }
        )]
    );
}

#[test]
fn a_painted_frame_leaves_the_hover_until_a_mouse_event_meets_a_frame_without_its_pane() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let other_tab_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_mouse_motion(get_content_cell(&mouse_frame, 0)),
        &mouse_frame,
        current_time,
    );

    // A key switches to a tab without the pane and back, with the pointer still.
    client.drop_gestures_the_frame_ended(&other_tab_frame);
    client.drop_gestures_the_frame_ended(&mouse_frame);
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).hovered_pane_id,
        Some(pane_id),
        "the pane under the still pointer stays hovered"
    );

    assert_eq!(
        client.handle_mouse(
            build_left_mouse_release(get_content_cell(&other_tab_frame, 0)),
            &other_tab_frame,
            advance_time_by_seconds(current_time, 1),
        ),
        Vec::new()
    );
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).hovered_pane_id,
        None,
        "a mouse event placed against a frame without the pane clears the hover"
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

    // The pane closes: the next painted frame does not draw it.
    let replacement_pane_id = PaneId::new();
    let replacement_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(replacement_pane_id));
    client.drop_gestures_the_frame_ended(&replacement_frame);
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
    // screen holds other text: the rows `vim` drew there.
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
    client.drop_gestures_the_frame_ended(&alternate_screen_mouse_frame);

    assert_eq!(
        client.handle_mouse(
            build_left_mouse_drag(drag_end_point),
            &alternate_screen_mouse_frame,
            advance_time_by_seconds(current_time, 2)
        ),
        Vec::new(),
        "the drag ended with the screen it was made on"
    );
    assert_eq!(client.left_button_gesture, None);
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
        "the tab's outer frame has no neighbor to resize against"
    );
}

#[test]
fn the_drag_anchor_only_walks_over_the_cells_the_session_accepted() {
    // The pointer travelled three cells, and the session took two: the pane
    // the drag shrinks hit its minimum size on the third. The anchor advances
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
        assert_eq!(
            client.left_button_gesture, None,
            "the release ends the drag"
        );
    }
}

#[test]
fn a_left_release_ends_a_border_drag_and_a_strip_peek_drag_and_sends_nothing() {
    let resized_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(PaneId::new()),
            build_plain_mouse_pane(resized_pane_id),
        ],
        None,
    );
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
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(divider_point),
        &mouse_frame,
        current_time,
    );
    assert_eq!(
        client.left_button_gesture,
        Some(LeftButtonGesture::BorderResize(ResizeDrag {
            pane_id: resized_pane_id,
            border_side: Direction::Up,
            corner_horizontal_side: None,
            last_accepted_position: divider_point,
        }))
    );
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_release(divider_point),
            &mouse_frame,
            advance_time_by_seconds(current_time, 1)
        ),
        Vec::new()
    );
    assert_eq!(client.left_button_gesture, None);

    client.handle_mouse(
        build_left_mouse_press(tabline_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );
    assert_eq!(
        client.left_button_gesture,
        Some(LeftButtonGesture::TablinePeek(TablineDrag {
            anchor_column: 40,
            anchor_first_visible_tab_index: 0,
        }))
    );
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_release(tabline_point),
            &mouse_frame,
            advance_time_by_seconds(current_time, 3)
        ),
        Vec::new()
    );
    assert_eq!(client.left_button_gesture, None);
}

#[test]
fn another_buttons_release_during_a_highlight_drag_copies_nothing_and_leaves_the_drag() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let screen_point = get_content_cell(&mouse_frame, 0);
    let drag_point = Point {
        column: screen_point.column + 4,
        ..screen_point
    };
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(drag_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );

    let right_release_actions = client.handle_mouse(
        build_mouse_event(
            MouseKind::Release(MouseButton::Right),
            drag_point,
            BindingModifierFlags::NONE,
        ),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );
    let left_release_actions = client.handle_mouse(
        build_left_mouse_release(drag_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 3),
    );

    assert_eq!(right_release_actions, Vec::new());
    assert_eq!(
        find_copy_action(&left_release_actions),
        Some(CopyArgs {
            pane_id,
            should_trim_trailing_whitespace: client
                .client_config
                .copy
                .should_trim_trailing_whitespace,
        })
    );
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
    let scroll_actions = client.expire_mouse_scroll(scroll_due_time, &mouse_frame);
    assert_eq!(
        scroll_actions,
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
fn a_drag_held_past_the_edge_asks_for_no_wakeup_while_an_input_event_is_on_its_way() {
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
    let drag_time = advance_time_by_seconds(current_time, 1);
    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(below_pane_point),
        &mouse_frame,
        drag_time,
    );

    let read_at = client
        .get_input_read_clock()
        .stamp_input_read(Instant::now());

    assert_eq!(client.compute_next_mouse_wakeup(drag_time), None);

    client.get_input_read_clock().note_input_taken(read_at);

    assert_eq!(
        client.compute_next_mouse_wakeup(drag_time),
        Some(Duration::from_millis(15))
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
        vec![MouseAction::Command(Box::new(Command::FocusPane(
            FocusPaneArgs {
                focus_target: FocusTarget::Pane(other_pane_id),
                client_id: Some(client.get_client_id()),
            }
        )))],
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
    client.set_frame_view(
        active_tab_id,
        Some(focused_pane_id),
        Some(focused_pane_id),
        vec![active_tab_id],
    );
    assert!(client.begin_placement_mode());

    let placement_action = client.handle_placement_mouse(
        build_left_mouse_press(get_content_cell(&mouse_frame, 1)),
        &mouse_frame,
        Instant::now(),
    );

    assert_eq!(
        placement_action,
        Some(PlacementInputAction::FocusPane {
            pane_id: dragged_pane_id,
        })
    );
    assert_eq!(
        client.find_placement_preview_target(),
        Some(PanePlacementPreviewTarget {
            source_pane_id: dragged_pane_id,
            destination_tab_id: active_tab_id,
        })
    );
    assert_eq!(client.get_placement_source_pane_id(), Some(dragged_pane_id));
    let viewer_chrome = client.build_viewer_chrome(active_tab_id);
    assert_eq!(viewer_chrome.hovered_pane_id, None);
    assert_eq!(viewer_chrome.placement_handle_hover, None);
}

#[test]
fn shifted_handle_press_in_normal_mode_does_not_start_pane_placement() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_test_client();
    client.set_frame_view(
        active_tab_id,
        Some(pane_id),
        Some(pane_id),
        vec![active_tab_id],
    );
    client.placement_handle_hover = Some(PlacementHandleHover::TiledHandle { pane_id });
    let mouse_frame = repaint_mouse_frame(&client, &mouse_frame);
    let handle_position = Point { column: 1, row: 1 };
    assert_eq!(
        resolve_hit_region(mouse_frame.build_frame_layout(), handle_position),
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
    client.set_frame_view(
        active_tab_id,
        Some(pane_id),
        Some(pane_id),
        vec![active_tab_id],
    );
    client.placement_handle_hover = Some(PlacementHandleHover::TiledHandle { pane_id });
    let mouse_frame = repaint_mouse_frame(&client, &mouse_frame);
    let handle_position = Point { column: 1, row: 1 };
    assert_eq!(
        resolve_hit_region(mouse_frame.build_frame_layout(), handle_position),
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
        Some(PlacementInputAction::FocusPane { pane_id })
    );
    assert_eq!(
        client.find_placement_preview_target(),
        Some(PanePlacementPreviewTarget {
            source_pane_id: pane_id,
            destination_tab_id: active_tab_id,
        })
    );
    assert!(client.is_placement_mode_active());
}

#[test]
fn a_tiled_handle_drag_while_locked_opens_pane_placement_and_keeps_the_client_locked() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_test_client();
    client.set_frame_view(
        active_tab_id,
        Some(pane_id),
        Some(pane_id),
        vec![active_tab_id],
    );
    client.set_lock_mode(LockMode::Locked);
    client.placement_handle_hover = Some(PlacementHandleHover::TiledHandle { pane_id });
    let mouse_frame = repaint_mouse_frame(&client, &mouse_frame);
    let handle_position = Point { column: 1, row: 1 };
    assert_eq!(
        resolve_hit_region(mouse_frame.build_frame_layout(), handle_position),
        HitRegion::PlacementHandle { pane_id }
    );

    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(handle_position),
            &mouse_frame,
            Instant::now(),
        ),
        Some(PlacementInputAction::FocusPane { pane_id })
    );
    assert_eq!(
        client.find_placement_preview_target(),
        Some(PanePlacementPreviewTarget {
            source_pane_id: pane_id,
            destination_tab_id: active_tab_id,
        })
    );
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert_eq!(client.get_active_input_mode(), LockMode::PanePlacement);

    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_release(handle_position),
            &mouse_frame,
            Instant::now(),
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(client.find_placement_preview_target(), None);
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert_eq!(client.get_active_input_mode(), LockMode::Locked);
}

/// Drag the upper pane `source_pane_id` of the two-pane `mouse_frame` onto the
/// lower pane `target_pane_id` and release it there. The drag starts with a
/// left press at `press_position` that returns `expected_press_action`, and
/// right after the press `client` holds the preview of the tab
/// `[source_pane_id / target_pane_id]`. Returns the id of the swap the release
/// confirmed.
fn drop_upper_pane_on_lower_pane(
    client: &mut Client,
    mouse_frame: &MouseFrame,
    source_pane_id: PaneId,
    target_pane_id: PaneId,
    press_position: Point,
    expected_press_action: PlacementInputAction,
) -> CommandId {
    let current_time = Instant::now();
    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(press_position),
            mouse_frame,
            current_time,
        ),
        Some(expected_press_action)
    );
    hold_placement_snapshot(
        client,
        build_mouse_placement_snapshot(
            mouse_frame,
            source_pane_id,
            build_two_band_layout_tree(source_pane_id, target_pane_id),
        ),
    );
    // Screen `(20, 16)` is inside the lower pane's content.
    let target_position = Point {
        column: 20,
        row: 16,
    };
    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_drag(target_position),
            mouse_frame,
            current_time,
        ),
        Some(PlacementInputAction::Consumed)
    );
    let release_action = client.handle_placement_mouse(
        build_left_mouse_release(target_position),
        mouse_frame,
        current_time,
    );
    assert_eq!(
        release_action,
        Some(build_expected_confirm_placement(
            client,
            source_pane_id,
            TiledPlacementTarget::Swap { target_pane_id },
        ))
    );
    client
        .get_pending_placement_command_id()
        .expect("the release confirmed a placement")
}

#[test]
fn a_handle_drag_drop_the_session_accepts_leaves_the_client_in_its_base_mode() {
    let typed_key_chord = KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('e'));
    for base_lock_mode in [LockMode::Normal, LockMode::Locked] {
        let [source_pane_id, target_pane_id] = [PaneId::new(), PaneId::new()];
        let mouse_frame = build_mouse_frame(
            &[
                build_plain_mouse_pane(source_pane_id),
                build_plain_mouse_pane(target_pane_id),
            ],
            Some(source_pane_id),
        );
        let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
        let mut client = build_test_client();
        assert!(
            client
                .client_config
                .should_stay_in_pane_placement_mode_after_placement
        );
        client.set_frame_view(
            active_tab_id,
            Some(source_pane_id),
            Some(source_pane_id),
            vec![active_tab_id],
        );
        client.set_lock_mode(base_lock_mode);
        client.placement_handle_hover = Some(PlacementHandleHover::TiledHandle {
            pane_id: source_pane_id,
        });
        let mouse_frame = repaint_mouse_frame(&client, &mouse_frame);
        let handle_position = Point { column: 1, row: 1 };
        assert_eq!(
            resolve_hit_region(mouse_frame.build_frame_layout(), handle_position),
            HitRegion::PlacementHandle {
                pane_id: source_pane_id
            }
        );

        let command_id = drop_upper_pane_on_lower_pane(
            &mut client,
            &mouse_frame,
            source_pane_id,
            target_pane_id,
            handle_position,
            PlacementInputAction::FocusPane {
                pane_id: source_pane_id,
            },
        );
        assert_eq!(client.get_active_input_mode(), base_lock_mode);
        apply_frame_keeping_view(&mut client, Some(CommandId::new()), None);
        assert!(client.is_pending_placement_command(command_id));
        assert_eq!(client.get_active_input_mode(), base_lock_mode);

        apply_frame_keeping_view(&mut client, Some(command_id), None);

        assert_eq!(
            client.placement_state.placement_mode, None,
            "base mode {base_lock_mode:?}"
        );
        assert_eq!(client.get_lock_mode(), base_lock_mode);
        assert_eq!(client.get_active_input_mode(), base_lock_mode);
        assert_eq!(
            client.resolve_key(typed_key_chord, Instant::now()),
            KeyOutcome::PassThrough(typed_key_chord)
        );
    }
}

#[test]
fn a_mouse_drop_the_session_accepts_keeps_pane_placement_mode_opened_by_a_key() {
    let [source_pane_id, target_pane_id] = [PaneId::new(), PaneId::new()];
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(source_pane_id),
            build_plain_mouse_pane(target_pane_id),
        ],
        Some(source_pane_id),
    );
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_test_client();
    assert!(
        client
            .client_config
            .should_stay_in_pane_placement_mode_after_placement
    );
    client.set_frame_view(
        active_tab_id,
        Some(source_pane_id),
        Some(source_pane_id),
        vec![active_tab_id],
    );
    assert!(client.begin_placement_mode());

    // Screen `(20, 5)` is inside the upper pane's content.
    let command_id = drop_upper_pane_on_lower_pane(
        &mut client,
        &mouse_frame,
        source_pane_id,
        target_pane_id,
        Point { column: 20, row: 5 },
        PlacementInputAction::Consumed,
    );
    apply_frame_keeping_view(&mut client, Some(command_id), None);

    assert!(client.is_placement_mode_active());
    assert_eq!(
        client.placement_state.placement_mode_lifetime,
        PlacementModeLifetime::UntilCancelled
    );
    assert_eq!(client.get_active_input_mode(), LockMode::PanePlacement);
    assert_eq!(client.get_placement_target(), None);
    assert!(!client.is_placement_confirmation_pending());
    let typed_key_chord = KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('e'));
    assert_eq!(
        client.resolve_key(typed_key_chord, Instant::now()),
        KeyOutcome::Discard
    );
}

#[test]
fn submitted_mouse_placement_consumes_mouse_input_until_the_frame_reconciles() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_test_client();
    client.set_frame_view(
        active_tab_id,
        Some(pane_id),
        Some(pane_id),
        vec![active_tab_id],
    );
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: pane_id,
        source_tab_id: Some(active_tab_id),
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: Some(TiledPlacementTarget::Swap {
            target_pane_id: PaneId::new(),
        }),
        pending_placement_command_id: Some(CommandId::new()),
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
        vec![build_forwarded_mouse_action(
            pane_id,
            build_left_mouse_press(screen_point),
            Point { column: 1, row: 1 }
        )]
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
        vec![MouseAction::Command(Box::new(Command::Visual(
            VisualCommand::ClearSelection(ClearSelectionArgs { pane_id })
        )))],
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
        vec![MouseAction::Command(Box::new(Command::Visual(
            VisualCommand::ClearSelection(ClearSelectionArgs { pane_id })
        )))],
        "Shift makes the press start a koshi selection"
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
        vec![MouseAction::Command(Box::new(Command::Visual(
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
        )))],
        "the drag extends the selection instead of reaching the program"
    );
}

#[test]
fn ending_the_gestures_drops_the_left_gesture_and_the_capture_and_leaves_the_pointer_and_the_strip_alone(
) {
    let hovered_pane_id = PaneId::new();
    let resized_pane_id = PaneId::new();
    let mut hovered_mouse_pane = build_plain_mouse_pane(hovered_pane_id);
    // Button-event tracking reports the right press the capture holds.
    hovered_mouse_pane.mouse_tracking = MouseTracking::ButtonMotion;
    let mouse_frame = build_mouse_frame(
        &[hovered_mouse_pane, build_plain_mouse_pane(resized_pane_id)],
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

    // A border drag held by the left button, and the right button captured to
    // the hovered pane's program.
    client.handle_mouse(
        build_left_mouse_press(divider_point),
        &mouse_frame,
        current_time,
    );
    let right_press = build_mouse_event(
        MouseKind::Press(MouseButton::Right),
        screen_point,
        BindingModifierFlags::NONE,
    );
    assert_eq!(
        client.handle_mouse(
            right_press,
            &mouse_frame,
            advance_time_by_seconds(current_time, 1)
        ),
        vec![build_forwarded_mouse_action(
            hovered_pane_id,
            right_press,
            Point { column: 1, row: 1 }
        )]
    );
    assert_eq!(
        client.left_button_gesture,
        Some(LeftButtonGesture::BorderResize(ResizeDrag {
            pane_id: resized_pane_id,
            border_side: Direction::Up,
            corner_horizontal_side: None,
            last_accepted_position: divider_point,
        }))
    );
    assert_eq!(
        client.mouse_capture,
        Some(MouseCapture {
            pane_id: hovered_pane_id,
            button: MouseButton::Right,
            last_pane_local_cell: Point { column: 1, row: 1 },
        })
    );

    client.end_mouse_gestures();

    assert_eq!(client.left_button_gesture, None);
    assert_eq!(client.mouse_capture, None);
    assert_eq!(
        client.take_mouse_capture_release(),
        None,
        "the new link owes no release for the right press"
    );
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
fn ending_the_gestures_drops_a_release_already_owed() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.mouse_tracking = MouseTracking::ButtonMotion;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);
    let mut client = build_test_client();
    client.handle_mouse(
        build_left_mouse_press(get_content_cell(&mouse_frame, 0)),
        &mouse_frame,
        Instant::now(),
    );
    // A painted frame that no longer draws the pane owes its program a release.
    client.drop_gestures_the_frame_ended(&build_one_pane_mouse_frame(build_plain_mouse_pane(
        PaneId::new(),
    )));
    assert_eq!(client.mouse_capture, None);
    assert_eq!(
        client.unreleased_mouse_capture,
        Some(MouseCapture {
            pane_id,
            button: MouseButton::Left,
            last_pane_local_cell: Point { column: 1, row: 1 },
        })
    );

    client.end_mouse_gestures();

    assert_eq!(
        client.take_mouse_capture_release(),
        None,
        "a new link sends no release the old link owed"
    );
}

#[test]
fn keyboard_placement_ends_the_held_gestures_and_the_edge_scroll_and_owes_the_capture_a_release() {
    let selection_pane_id = PaneId::new();
    let mut selection_mouse_pane = build_plain_mouse_pane(selection_pane_id);
    // Button-event tracking reports the right press the capture holds; the
    // left press holds Shift, so it selects.
    selection_mouse_pane.mouse_tracking = MouseTracking::ButtonMotion;
    let mouse_frame = build_mouse_frame(
        &[selection_mouse_pane, build_plain_mouse_pane(PaneId::new())],
        Some(selection_pane_id),
    );
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let selection_point = get_content_cell(&mouse_frame, 0);
    let current_time = Instant::now();
    let mut client = build_test_client();
    client.set_frame_view(
        active_tab_id,
        Some(selection_pane_id),
        Some(selection_pane_id),
        vec![active_tab_id],
    );

    // A selection drag held below its pane, and the right button captured to
    // the program in it.
    client.handle_mouse(
        build_mouse_event(
            MouseKind::Press(MouseButton::Left),
            selection_point,
            BindingModifierFlags::SHIFT,
        ),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(get_content_cell(&mouse_frame, 1)),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    let right_press = build_mouse_event(
        MouseKind::Press(MouseButton::Right),
        selection_point,
        BindingModifierFlags::NONE,
    );
    assert_eq!(
        client.handle_mouse(
            right_press,
            &mouse_frame,
            advance_time_by_seconds(current_time, 1)
        ),
        vec![build_forwarded_mouse_action(
            selection_pane_id,
            right_press,
            Point { column: 1, row: 1 }
        )]
    );
    assert_eq!(
        client.compute_next_mouse_wakeup(advance_time_by_seconds(current_time, 1)),
        Some(Duration::from_millis(15))
    );

    assert!(client.begin_placement_mode());

    assert_eq!(client.left_button_gesture, None);
    assert_eq!(client.mouse_capture, None);
    assert_eq!(
        client.take_mouse_capture_release(),
        Some(MouseAction::Forward {
            pane_id: selection_pane_id,
            mouse_kind: MouseKind::Release(MouseButton::Right),
            modifier_flags: BindingModifierFlags::NONE,
            pane_local_cell: Point { column: 1, row: 1 },
        })
    );
    assert_eq!(
        client.expire_mouse_scroll(advance_time_by_seconds(current_time, 4), &mouse_frame),
        Vec::new(),
        "the pane under the ended selection drag no longer scrolls"
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
fn ending_the_selection_leaves_a_border_drag_and_the_capture_alone() {
    // A key that reaches the pane's program ends the highlight gesture only.
    let selection_pane_id = PaneId::new();
    let resized_pane_id = PaneId::new();
    let mut selection_mouse_pane = build_plain_mouse_pane(selection_pane_id);
    selection_mouse_pane.mouse_tracking = MouseTracking::ButtonMotion;
    let mouse_frame = build_mouse_frame(
        &[
            selection_mouse_pane,
            build_plain_mouse_pane(resized_pane_id),
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
    let current_time = Instant::now();
    let mut client = build_test_client();
    let right_capture = Some(MouseCapture {
        pane_id: selection_pane_id,
        button: MouseButton::Right,
        last_pane_local_cell: Point { column: 1, row: 1 },
    });

    // A Shift selection drag, and the right button captured to the program.
    client.handle_mouse(
        build_mouse_event(
            MouseKind::Press(MouseButton::Left),
            screen_point,
            BindingModifierFlags::SHIFT,
        ),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_mouse_event(
            MouseKind::Press(MouseButton::Right),
            screen_point,
            BindingModifierFlags::NONE,
        ),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );

    client.end_mouse_selection();

    assert_eq!(client.left_button_gesture, None);
    assert_eq!(client.mouse_capture, right_capture);
    assert_eq!(
        client.take_mouse_capture_release(),
        None,
        "the capture is still held, so no release is owed"
    );

    // A left press on the divider leaves the right capture held.
    client.handle_mouse(
        build_left_mouse_press(divider_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );
    client.end_mouse_selection();

    assert_eq!(
        client.left_button_gesture,
        Some(LeftButtonGesture::BorderResize(ResizeDrag {
            pane_id: resized_pane_id,
            border_side: Direction::Up,
            corner_horizontal_side: None,
            last_accepted_position: divider_point,
        })),
        "a border drag is not a selection"
    );
    assert_eq!(client.mouse_capture, right_capture);
}

#[test]
fn a_left_press_after_a_lost_release_ends_the_stale_selection_without_copying_it() {
    let selection_pane_id = PaneId::new();
    let other_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(selection_pane_id),
            build_plain_mouse_pane(other_pane_id),
        ],
        Some(selection_pane_id),
    );
    let screen_point = get_content_cell(&mouse_frame, 0);
    let other_pane_point = get_content_cell(&mouse_frame, 1);
    let mut client = build_test_client();
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

    // The release never arrived. A click on the other pane only focuses it.
    let press_actions = client.handle_mouse(
        build_left_mouse_press(other_pane_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );

    assert_eq!(
        press_actions,
        vec![build_focus_pane_mouse_action(&client, other_pane_id)]
    );
    assert_eq!(client.left_button_gesture, None);
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_release(other_pane_point),
            &mouse_frame,
            advance_time_by_seconds(current_time, 3)
        ),
        Vec::new(),
        "the stale highlight is not copied"
    );
}

#[test]
fn a_left_press_after_a_lost_release_ends_the_stale_border_drag() {
    let resized_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(PaneId::new()),
            build_plain_mouse_pane(resized_pane_id),
        ],
        None,
    );
    let resized_pane_point = get_content_cell(&mouse_frame, 1);
    let divider_point = Point {
        column: 10,
        row: mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[1]
            .outer_rect
            .origin
            .row
            + 1,
    };
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(divider_point),
        &mouse_frame,
        current_time,
    );

    // The release never arrived. A click on an unfocused pane only focuses it.
    let press_actions = client.handle_mouse(
        build_left_mouse_press(resized_pane_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );

    assert_eq!(
        press_actions,
        vec![build_focus_pane_mouse_action(&client, resized_pane_id)]
    );
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_drag(Point {
                column: resized_pane_point.column,
                row: resized_pane_point.row + 3,
            }),
            &mouse_frame,
            advance_time_by_seconds(current_time, 2)
        ),
        Vec::new(),
        "no border moves"
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
        source_tab_id: Some(active_tab_id),
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    hold_placement_snapshot(&mut client, placement_snapshot);
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
        Some(TiledPlacementTarget::Swap { target_pane_id })
    );
    assert_eq!(
        client.find_placement_target_at(target_border_point, &mouse_frame),
        Some(TiledPlacementTarget::Swap { target_pane_id }),
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
        Some(TiledPlacementTarget::Split {
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
        source_tab_id: Some(active_tab_id),
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    hold_placement_snapshot(&mut client, placement_snapshot);
    client.begin_placement_drag(target_point, false);

    assert_eq!(
        client.find_placement_target_at(target_point, &moved_preview_frame),
        Some(TiledPlacementTarget::Swap { target_pane_id })
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
        source_tab_id: Some(source_tab_id),
        destination_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    hold_placement_snapshot(&mut client, placement_snapshot);
    client.begin_placement_drag(target_point, false);

    assert_eq!(
        client.find_placement_target_at(target_point, &mouse_frame),
        Some(TiledPlacementTarget::Swap {
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
        source_tab_id: Some(source_tab_id),
        destination_tab_id: source_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
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
    client.expire_placement_tab_hover(hover_started_at + crate::PLACEMENT_TAB_HOVER_DELAY_DURATION);
    assert_eq!(
        client.find_placement_preview_target(),
        Some(PanePlacementPreviewTarget {
            source_pane_id,
            destination_tab_id,
        })
    );
    hold_placement_snapshot(&mut client, placement_snapshot);
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
    let placement_target = TiledPlacementTarget::Split {
        destination_tab_id,
        anchor: PanePlacementAnchor::Pane(destination_second_pane_id),
        direction: Direction::Up,
    };
    assert_eq!(
        client.get_placement_target().cloned(),
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
        Some(build_expected_confirm_placement(
            &client,
            source_pane_id,
            placement_target
        ))
    );
}

#[test]
fn a_handle_drag_dropped_on_a_pane_of_a_hovered_tab_returns_to_the_base_mode_once_accepted() {
    let source_pane_id = PaneId::new();
    let destination_first_pane_id = PaneId::new();
    let destination_second_pane_id = PaneId::new();
    let source_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(source_pane_id),
            build_plain_mouse_pane(PaneId::new()),
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
    let destination_point = get_content_cell(&destination_frame, 1);
    let hover_started_at = Instant::now();
    let mut client = build_test_client();
    client.set_frame_view(
        source_tab_id,
        Some(source_pane_id),
        Some(source_pane_id),
        vec![source_tab_id, destination_tab_id],
    );
    client.placement_handle_hover = Some(PlacementHandleHover::TiledHandle {
        pane_id: source_pane_id,
    });
    let source_frame = repaint_mouse_frame(&client, &source_frame);
    let handle_position = Point { column: 1, row: 1 };
    assert_eq!(
        resolve_hit_region(source_frame.build_frame_layout(), handle_position),
        HitRegion::PlacementHandle {
            pane_id: source_pane_id
        }
    );

    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(handle_position),
            &source_frame,
            hover_started_at,
        ),
        Some(PlacementInputAction::FocusPane {
            pane_id: source_pane_id
        })
    );
    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_drag(Point { column: 2, row: 0 }),
            &source_frame,
            hover_started_at,
        ),
        Some(PlacementInputAction::Consumed)
    );
    client.update_placement_tab_hover(Some(destination_tab_id), hover_started_at);
    client.expire_placement_tab_hover(hover_started_at + crate::PLACEMENT_TAB_HOVER_DELAY_DURATION);
    assert_eq!(
        client.find_placement_preview_target(),
        Some(PanePlacementPreviewTarget {
            source_pane_id,
            destination_tab_id,
        })
    );
    hold_placement_snapshot(&mut client, placement_snapshot);
    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_drag(destination_point),
            &destination_frame,
            hover_started_at,
        ),
        Some(PlacementInputAction::Consumed)
    );
    let placement_target = TiledPlacementTarget::Swap {
        target_pane_id: destination_second_pane_id,
    };
    assert_eq!(
        client.get_placement_target().cloned(),
        Some(placement_target.clone())
    );
    let release_action = client.handle_placement_mouse(
        build_left_mouse_release(destination_point),
        &destination_frame,
        hover_started_at,
    );
    assert_eq!(
        release_action,
        Some(build_expected_confirm_placement(
            &client,
            source_pane_id,
            placement_target
        ))
    );
    let command_id = client
        .get_pending_placement_command_id()
        .expect("the release confirmed a placement");

    apply_frame_keeping_view(&mut client, Some(command_id), None);

    assert_eq!(client.placement_state.placement_mode, None);
    assert_eq!(client.get_active_input_mode(), LockMode::Normal);
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
        source_tab_id: Some(active_tab_id),
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Up,
        placement_target: None,
        pending_placement_command_id: None,
    });
    hold_placement_snapshot(&mut client, placement_snapshot);
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
        Some(TiledPlacementTarget::Split {
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
        Some(TiledPlacementTarget::Swap {
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
        source_tab_id: Some(active_tab_id),
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    let stack_header_point = Point { column: 2, row: 22 };

    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(stack_header_point),
            &mouse_frame,
            Instant::now(),
        ),
        Some(PlacementInputAction::FocusPane {
            pane_id: collapsed_stack_pane_id,
        })
    );
    assert_eq!(
        client.find_placement_preview_target(),
        Some(PanePlacementPreviewTarget {
            source_pane_id: collapsed_stack_pane_id,
            destination_tab_id: active_tab_id,
        })
    );

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
    placement_target: Option<TiledPlacementTarget>,
) -> Client {
    let previewed_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_test_client();
    client.active_tab_id = Some(active_tab_id);
    client.visible_tab_ids = vec![active_tab_id, previewed_tab_id];
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(active_tab_id),
        destination_tab_id: previewed_tab_id,
        placement_direction: Direction::Right,
        placement_target,
        pending_placement_command_id: None,
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
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(
        client.find_placement_preview_target(),
        Some(PanePlacementPreviewTarget {
            source_pane_id: previewed_pane_id,
            destination_tab_id: previewed_tab_id,
        })
    );
    assert_eq!(
        client
            .placement_state
            .placement_mode
            .as_ref()
            .and_then(|placement_mode| placement_mode.source_tab_id),
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
    let placement_target = TiledPlacementTarget::Split {
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
    let layout_solve =
        solve_layout_with_mode(&layout_tree, LayoutMode::Tiled, tab_rect, pane_sizing);
    for pane_slot in &mut mouse_frame.session_snapshot.active_tab_snapshot.pane_slots {
        let pane_rect = layout_solve
            .find_pane_rect(pane_slot.pane_id)
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
    let group_left_rect = layout_solve
        .find_pane_rect(group_left_pane_id)
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
        source_tab_id: Some(active_tab_id),
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    hold_placement_snapshot(&mut client, placement_snapshot);

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
        Some(TiledPlacementTarget::Split {
            destination_tab_id: active_tab_id,
            anchor: PanePlacementAnchor::Group(vec![group_left_pane_id, group_right_pane_id,]),
            direction: Direction::Up,
        })
    );
}

#[test]
fn a_placement_drag_released_over_a_target_with_no_room_sends_nothing() {
    let [source_pane_id, target_pane_id] = [PaneId::new(), PaneId::new()];
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(source_pane_id),
            build_plain_mouse_pane(target_pane_id),
        ],
        Some(source_pane_id),
    );
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut placement_snapshot = build_mouse_placement_snapshot(
        &mouse_frame,
        source_pane_id,
        build_two_band_layout_tree(source_pane_id, target_pane_id),
    );
    placement_snapshot.insertion_tab_sizes.destination_tab_size = Some(NO_ROOM_TAB_SIZE);
    let swap_target = TiledPlacementTarget::Swap { target_pane_id };
    assert_eq!(
        placement_snapshot.build_placed_preview(&swap_target),
        Err(PlacementPreviewError::NoRoom {
            tab_id: active_tab_id
        })
    );
    let placement_mode = PlacementMode {
        source_pane_id,
        source_tab_id: Some(active_tab_id),
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    };

    // Screen `(20, 5)` is inside the source pane, and `(20, 16)` inside the
    // target pane below it.
    for (placement_mode_lifetime, expected_placement_mode) in [
        (
            PlacementModeLifetime::UntilCancelled,
            Some(PlacementMode {
                placement_target: Some(swap_target.clone()),
                ..placement_mode.clone()
            }),
        ),
        (PlacementModeLifetime::UntilDragEnds, None),
    ] {
        let mut client = build_test_client();
        client.visible_tab_ids = vec![active_tab_id];
        client.placement_state.placement_mode = Some(placement_mode.clone());
        client.placement_state.placement_mode_lifetime = placement_mode_lifetime;
        hold_placement_snapshot(&mut client, placement_snapshot.clone());
        let current_time = Instant::now();
        client.handle_placement_mouse(
            build_left_mouse_press(Point { column: 20, row: 5 }),
            &mouse_frame,
            current_time,
        );
        client.handle_placement_mouse(
            build_left_mouse_drag(Point {
                column: 20,
                row: 16,
            }),
            &mouse_frame,
            current_time,
        );
        assert_eq!(
            client.get_placement_target().cloned(),
            Some(swap_target.clone())
        );

        assert_eq!(
            client.handle_placement_mouse(
                build_left_mouse_release(Point {
                    column: 20,
                    row: 16,
                }),
                &mouse_frame,
                current_time,
            ),
            Some(PlacementInputAction::Consumed)
        );
        assert_eq!(
            client.placement_state.placement_mode,
            expected_placement_mode
        );
    }
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
        source_tab_id: Some(tab_id),
        destination_tab_id: tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
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

/// The box of the floating pane in [`build_floating_mouse_frame`], counted
/// from the pane area at screen `(0, 1)`: screen columns 10–49, rows 5–16.
/// Its content is screen `(11, 8)` `38x8`. On its titlebar row, screen row 6,
/// the grab handle takes columns 12 and 13, the pin toggle 38–41, the minimize
/// button 43 and the close button 46.
pub(crate) const FLOATING_PANE_OUTER_RECT: Rect = Rect {
    origin: Point { column: 10, row: 4 },
    size: Size {
        column_count: 40,
        row_count: 12,
    },
};

/// A frame of `tiled_mouse_pane` filling the `80x22` pane area, with
/// `floating_mouse_pane` drawn over it in [`FLOATING_PANE_OUTER_RECT`],
/// pinned when `is_pinned`, and `focused_pane_id` focused. The active tab
/// focuses `tiled_mouse_pane`.
pub(crate) fn build_floating_mouse_frame(
    tiled_mouse_pane: MousePane,
    floating_mouse_pane: MousePane,
    is_pinned: bool,
    focused_pane_id: PaneId,
) -> MouseFrame {
    let tiled_pane_id = tiled_mouse_pane.pane_id;
    let mut mouse_frame = build_mouse_frame(&[tiled_mouse_pane], Some(focused_pane_id));
    mouse_frame.client_snapshot.active_tab_focused_pane_id = Some(tiled_pane_id);
    add_floating_mouse_pane(
        &mut mouse_frame,
        floating_mouse_pane,
        FLOATING_PANE_OUTER_RECT,
        is_pinned,
    );
    mouse_frame
}

/// Draw `floating_mouse_pane` in `outer_rect` in front of the floating panes
/// `mouse_frame` already draws, pinned when `is_pinned`, and list it after
/// them as the newest floating pane.
pub(crate) fn add_floating_mouse_pane(
    mouse_frame: &mut MouseFrame,
    floating_mouse_pane: MousePane,
    outer_rect: Rect,
    is_pinned: bool,
) {
    mouse_frame
        .session_snapshot
        .floating_pane_ids
        .push(floating_mouse_pane.pane_id);
    mouse_frame
        .session_snapshot
        .floating_pane_slots
        .push(FloatingPaneSlot::from_outer_rect(
            floating_mouse_pane.pane_id,
            outer_rect,
            is_pinned,
        ));
    mouse_frame.mouse_panes.push(floating_mouse_pane);
}

/// The render snapshot `mouse_frame` was painted from, with no pane content.
pub(crate) fn build_render_snapshot_from_mouse_frame(mouse_frame: &MouseFrame) -> RenderSnapshot {
    RenderSnapshot {
        is_recovery_notice_visible: false,
        session_snapshot: mouse_frame.session_snapshot.clone(),
        pane_snapshots: Vec::new(),
        client_snapshot: mouse_frame.client_snapshot.clone(),
        placement_preview: None,
    }
}

/// The frame `mouse_frame` that names `command_id` as the last floating pane
/// move the session ran for its client.
pub(crate) fn build_floating_pane_move_run_snapshot(
    mouse_frame: &MouseFrame,
    command_id: CommandId,
) -> RenderSnapshot {
    let mut render_snapshot = build_render_snapshot_from_mouse_frame(mouse_frame);
    render_snapshot
        .client_snapshot
        .last_applied_floating_pane_move_command_id = Some(command_id);
    render_snapshot
}

/// Where `client` draws a floating pane ahead of the session, read from the
/// chrome it paints `mouse_frame` with.
pub(crate) fn find_floating_pane_move_draft(
    client: &Client,
    mouse_frame: &MouseFrame,
) -> Option<FloatingPaneMoveDraft> {
    client
        .build_viewer_chrome(mouse_frame.client_snapshot.active_tab_id)
        .floating_pane_move_draft
}

/// The top-left cell `client` draws the floating pane `pane_id` at when it
/// paints `mouse_frame` again, in pane-area cells, or `None` when it draws no
/// such pane.
pub(crate) fn find_drawn_floating_pane_top_left_cell(
    client: &Client,
    mouse_frame: &MouseFrame,
    pane_id: PaneId,
) -> Option<Point> {
    let painted_mouse_frame = repaint_mouse_frame(client, mouse_frame);
    painted_mouse_frame
        .build_frame_layout()
        .find_drawn_floating_pane_slot(pane_id)
        .map(|floating_pane_slot| floating_pane_slot.outer_rect.origin)
}

/// The `FocusPane` a press sends for `pane_id`.
fn build_focus_pane_mouse_action(client: &Client, pane_id: PaneId) -> MouseAction {
    MouseAction::Command(Box::new(Command::FocusPane(FocusPaneArgs {
        focus_target: FocusTarget::Pane(pane_id),
        client_id: Some(client.get_client_id()),
    })))
}

/// Press the grab handle of the floating pane in `mouse_frame` at screen
/// `(12, 6)`, drag it to screen `(17, 7)`, and release it there. Returns what
/// the release decided.
fn drag_floating_pane_handle_by_five_and_one(
    client: &mut Client,
    mouse_frame: &MouseFrame,
    start_time: Instant,
) -> Vec<MouseAction> {
    client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        mouse_frame,
        start_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(Point { column: 17, row: 7 }),
        mouse_frame,
        advance_time_by_seconds(start_time, 1),
    );
    client.handle_mouse(
        build_left_mouse_release(Point { column: 17, row: 7 }),
        mouse_frame,
        advance_time_by_seconds(start_time, 2),
    )
}

/// The command id of the one `MoveFloatingPane` in `mouse_actions`, after
/// checking that it moves `pane_id` to `top_left_cell`.
fn get_floating_pane_move_command_id(
    mouse_actions: &[MouseAction],
    pane_id: PaneId,
    top_left_cell: Point,
) -> CommandId {
    let [MouseAction::MoveFloatingPane {
        command_id,
        command_args,
    }] = mouse_actions
    else {
        panic!("expected one floating pane move, got {mouse_actions:?}");
    };
    assert_eq!(
        *command_args,
        MoveFloatingPaneArgs {
            pane_id,
            top_left_cell
        }
    );
    *command_id
}

#[test]
fn a_press_on_an_unfocused_floating_panes_content_only_focuses_it() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let mut floating_mouse_pane = build_plain_mouse_pane(floating_pane_id);
    floating_mouse_pane.mouse_tracking = MouseTracking::Normal;
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        floating_mouse_pane,
        false,
        tiled_pane_id,
    );
    let mut client = build_test_client();

    let mouse_actions = client.handle_mouse(
        build_left_mouse_press(Point {
            column: 16,
            row: 10,
        }),
        &mouse_frame,
        Instant::now(),
    );

    assert_eq!(
        mouse_actions,
        vec![build_focus_pane_mouse_action(&client, floating_pane_id)]
    );
}

#[test]
fn a_press_on_the_focused_floating_panes_content_reaches_its_program_at_its_own_cell() {
    let floating_pane_id = PaneId::new();
    let mut floating_mouse_pane = build_plain_mouse_pane(floating_pane_id);
    floating_mouse_pane.mouse_tracking = MouseTracking::Normal;
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        floating_mouse_pane,
        false,
        floating_pane_id,
    );
    let mut client = build_test_client();
    let press_input = build_left_mouse_press(Point {
        column: 16,
        row: 10,
    });

    let mouse_actions = client.handle_mouse(press_input, &mouse_frame, Instant::now());

    // Screen (16, 10) is pane-area (16, 9); the content starts at pane-area
    // (11, 7).
    assert_eq!(
        mouse_actions,
        vec![build_forwarded_mouse_action(
            floating_pane_id,
            press_input,
            Point { column: 5, row: 2 }
        )]
    );
}

#[test]
fn a_move_over_a_floating_pane_does_not_reach_the_focused_tiled_pane_under_it() {
    let tiled_pane_id = PaneId::new();
    let mut tiled_mouse_pane = build_plain_mouse_pane(tiled_pane_id);
    tiled_mouse_pane.mouse_tracking = MouseTracking::AnyMotion;
    let mouse_frame = build_floating_mouse_frame(
        tiled_mouse_pane,
        build_plain_mouse_pane(PaneId::new()),
        false,
        tiled_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();

    assert_eq!(
        client.handle_mouse(
            build_mouse_motion(Point {
                column: 16,
                row: 10
            }),
            &mouse_frame,
            current_time
        ),
        Vec::new(),
        "the floating pane covers the tiled pane's content there"
    );
    let uncovered_motion = build_mouse_motion(Point {
        column: 60,
        row: 10,
    });
    assert_eq!(
        client.handle_mouse(
            uncovered_motion,
            &mouse_frame,
            advance_time_by_seconds(current_time, 1)
        ),
        vec![build_forwarded_mouse_action(
            tiled_pane_id,
            uncovered_motion,
            Point { column: 59, row: 8 }
        )]
    );
}

#[test]
fn a_selection_drag_inside_a_floating_pane_selects_its_text_and_outlives_the_next_event() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(Point {
            column: 16,
            row: 10,
        }),
        &mouse_frame,
        current_time,
    );
    let first_drag_actions = client.handle_mouse(
        build_left_mouse_drag(Point {
            column: 20,
            row: 11,
        }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    let second_drag_actions = client.handle_mouse(
        build_left_mouse_drag(Point {
            column: 21,
            row: 11,
        }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );

    let first_selection_args =
        find_set_selection_action(&first_drag_actions).expect("the drag asked for a highlight");
    assert_eq!(first_selection_args.pane_id, floating_pane_id);
    assert_eq!(
        first_selection_args.selection.anchor,
        GridPosition {
            row_index: 2,
            column_index: 5
        }
    );
    assert_eq!(
        first_selection_args.selection.cursor,
        GridPosition {
            row_index: 3,
            column_index: 9
        }
    );
    let second_selection_args =
        find_set_selection_action(&second_drag_actions).expect("the drag is still under way");
    assert_eq!(
        second_selection_args.selection.cursor,
        GridPosition {
            row_index: 3,
            column_index: 10
        }
    );
}

#[test]
fn a_wheel_over_chrome_scrolls_the_focused_floating_pane() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );

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
        wheel_decision.mouse_action,
        Some(MouseAction::Scroll {
            pane_id: floating_pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        })
    );
}

#[test]
fn a_wheel_over_a_floating_panes_titlebar_border_handle_or_buttons_scrolls_that_pane() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        tiled_pane_id,
    );
    let floating_pane_chrome_points = [
        Point { column: 12, row: 6 },
        Point { column: 20, row: 6 },
        Point { column: 20, row: 7 },
        Point { column: 39, row: 6 },
        Point { column: 43, row: 6 },
        Point { column: 46, row: 6 },
        Point { column: 10, row: 9 },
        Point { column: 49, row: 9 },
        Point { column: 20, row: 5 },
        Point {
            column: 20,
            row: 16,
        },
    ];

    for screen_point in floating_pane_chrome_points {
        assert_eq!(
            build_test_client()
                .handle_mouse_wheel(
                    build_mouse_wheel(ScrollDirection::Up, screen_point),
                    &mouse_frame
                )
                .expect("a wheel tick decides"),
            WheelDecision {
                hovered_pane_id: None,
                mouse_action: Some(MouseAction::Scroll {
                    pane_id: floating_pane_id,
                    is_scrolling_up: true,
                    scroll_line_count: 3,
                }),
            },
            "{screen_point:?}"
        );
    }
}

#[test]
fn a_wheel_over_a_tiled_panes_border_scrolls_the_focused_pane() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );

    let wheel_decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, Point { column: 0, row: 9 }),
            &mouse_frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        wheel_decision,
        WheelDecision {
            hovered_pane_id: None,
            mouse_action: Some(MouseAction::Scroll {
                pane_id: floating_pane_id,
                is_scrolling_up: true,
                scroll_line_count: 3,
            }),
        }
    );
}

#[test]
fn dragging_a_floating_panes_handle_draws_it_locally_and_sends_one_move_on_release() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        tiled_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();

    let press_actions = client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );
    assert_eq!(
        press_actions,
        vec![build_focus_pane_mouse_action(&client, floating_pane_id)],
        "the press focuses and raises the pane"
    );
    let drag_actions = client.handle_mouse(
        build_left_mouse_drag(Point { column: 17, row: 7 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    assert_eq!(drag_actions, Vec::new(), "the drag reaches no session");
    let moved_draft = Some(FloatingPaneMoveDraft {
        pane_id: floating_pane_id,
        top_left_cell: Point { column: 15, row: 5 },
    });
    assert_eq!(
        find_floating_pane_move_draft(&client, &mouse_frame),
        moved_draft
    );

    let release_actions = client.handle_mouse(
        build_left_mouse_release(Point { column: 17, row: 7 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );

    get_floating_pane_move_command_id(
        &release_actions,
        floating_pane_id,
        Point { column: 15, row: 5 },
    );
    assert_eq!(
        find_floating_pane_move_draft(&client, &mouse_frame),
        moved_draft,
        "the pane stays at the draft until the session answers"
    );
}

#[test]
fn a_floating_pane_handle_drag_draws_and_sends_the_pane_inside_the_pane_area() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );

    // A 40x12 pane in the 80x22 pane area is drawn with its top-left cell
    // between (0, 0) and (40, 10).
    client.handle_mouse(
        build_left_mouse_drag(Point {
            column: 79,
            row: 23,
        }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    assert_eq!(
        find_drawn_floating_pane_top_left_cell(&client, &mouse_frame, floating_pane_id),
        Some(Point {
            column: 40,
            row: 10
        })
    );
    client.handle_mouse(
        build_left_mouse_drag(Point { column: 0, row: 0 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );
    assert_eq!(
        find_drawn_floating_pane_top_left_cell(&client, &mouse_frame, floating_pane_id),
        Some(Point { column: 0, row: 0 })
    );
    let release_actions = client.handle_mouse(
        build_left_mouse_release(Point {
            column: 79,
            row: 23,
        }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 3),
    );

    get_floating_pane_move_command_id(
        &release_actions,
        floating_pane_id,
        Point {
            column: 40,
            row: 10,
        },
    );
    assert_eq!(
        find_floating_pane_move_draft(&client, &mouse_frame),
        Some(FloatingPaneMoveDraft {
            pane_id: floating_pane_id,
            top_left_cell: Point {
                column: 40,
                row: 10
            },
        })
    );
}

#[test]
fn a_held_handle_draws_and_sends_the_pane_inside_a_pane_area_that_shrank() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let small_viewport_size = Size {
        column_count: 60,
        row_count: 20,
    };
    let mut small_mouse_frame = mouse_frame.clone();
    small_mouse_frame.client_snapshot.viewport_size = small_viewport_size;
    small_mouse_frame.committed_regions = CommittedRegions::build_core(small_viewport_size, 0);
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(Point {
            column: 42,
            row: 12,
        }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    assert_eq!(
        find_drawn_floating_pane_top_left_cell(&client, &mouse_frame, floating_pane_id),
        Some(Point {
            column: 40,
            row: 10
        })
    );

    // The 40x12 pane in the 60x18 pane area is drawn at (20, 6) at most.
    assert_eq!(
        find_drawn_floating_pane_top_left_cell(&client, &small_mouse_frame, floating_pane_id),
        Some(Point { column: 20, row: 6 })
    );
    let release_actions = client.handle_mouse(
        build_left_mouse_release(Point {
            column: 42,
            row: 12,
        }),
        &small_mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );

    get_floating_pane_move_command_id(
        &release_actions,
        floating_pane_id,
        Point { column: 20, row: 6 },
    );
}

#[test]
fn a_handle_released_where_it_was_pressed_sends_no_move() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(Point { column: 17, row: 7 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    // No drag reports the pointer's way back: the release names the cell.
    let release_actions = client.handle_mouse(
        build_left_mouse_release(Point { column: 12, row: 6 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );

    assert_eq!(release_actions, Vec::new());
    assert_eq!(find_floating_pane_move_draft(&client, &mouse_frame), None);
}

#[test]
fn a_handle_released_where_it_was_pressed_sends_no_move_after_the_pane_area_shrinks() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let small_viewport_size = Size {
        column_count: 45,
        row_count: 20,
    };
    let mut small_mouse_frame = mouse_frame.clone();
    small_mouse_frame.client_snapshot.viewport_size = small_viewport_size;
    small_mouse_frame.committed_regions = CommittedRegions::build_core(small_viewport_size, 0);
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );
    // The 40x12 pane in the 45x18 pane area is drawn at (5, 4).
    assert_eq!(
        find_drawn_floating_pane_top_left_cell(&client, &small_mouse_frame, floating_pane_id),
        Some(Point { column: 5, row: 4 })
    );

    let release_actions = client.handle_mouse(
        build_left_mouse_release(Point { column: 12, row: 6 }),
        &small_mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );

    assert_eq!(release_actions, Vec::new());
    assert_eq!(
        find_floating_pane_move_draft(&client, &small_mouse_frame),
        None
    );
}

#[test]
fn a_handle_release_that_leaves_the_pane_drawn_where_it_was_sends_no_move() {
    let floating_pane_id = PaneId::new();
    let mut mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    // The 40x12 pane at (40, 4) touches the right edge of the 80x22 pane area.
    mouse_frame.session_snapshot.floating_pane_slots = vec![FloatingPaneSlot::from_outer_rect(
        floating_pane_id,
        Rect {
            origin: Point { column: 40, row: 4 },
            ..FLOATING_PANE_OUTER_RECT
        },
        false,
    )];
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(Point { column: 42, row: 6 }),
        &mouse_frame,
        current_time,
    );

    // Three cells right is past the right edge: the pane stays drawn at (40, 4).
    let release_actions = client.handle_mouse(
        build_left_mouse_release(Point { column: 45, row: 6 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );

    assert_eq!(release_actions, Vec::new());
    assert_eq!(find_floating_pane_move_draft(&client, &mouse_frame), None);
}

#[test]
fn a_press_hits_a_moved_floating_pane_where_the_painted_frame_still_draws_it() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        tiled_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );
    let release_actions = client.handle_mouse(
        build_left_mouse_release(Point { column: 42, row: 6 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    get_floating_pane_move_command_id(
        &release_actions,
        floating_pane_id,
        Point { column: 40, row: 4 },
    );

    // No paint has drawn the move: screen (16, 10) still shows the floating
    // pane's content.
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_press(Point {
                column: 16,
                row: 10
            }),
            &mouse_frame,
            advance_time_by_seconds(current_time, 3),
        ),
        vec![build_focus_pane_mouse_action(&client, floating_pane_id)]
    );

    // A paint draws the pane at (40, 4): screen (16, 10) shows the tiled pane.
    let painted_mouse_frame = repaint_mouse_frame(&client, &mouse_frame);
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_press(Point {
                column: 16,
                row: 10
            }),
            &painted_mouse_frame,
            advance_time_by_seconds(current_time, 5),
        ),
        vec![MouseAction::Command(Box::new(Command::Visual(
            VisualCommand::ClearSelection(ClearSelectionArgs {
                pane_id: tiled_pane_id
            })
        )))]
    );
}

#[test]
fn a_handle_release_moves_the_pane_by_the_release_cell_not_the_last_drag() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(Point { column: 17, row: 7 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );

    let release_actions = client.handle_mouse(
        build_left_mouse_release(Point { column: 20, row: 9 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );

    get_floating_pane_move_command_id(
        &release_actions,
        floating_pane_id,
        Point { column: 18, row: 7 },
    );
}

#[test]
fn another_buttons_release_during_a_handle_drag_leaves_the_drag_held() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(Point { column: 17, row: 7 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );

    let right_press_actions = client.handle_mouse(
        build_mouse_event(
            MouseKind::Press(MouseButton::Right),
            Point { column: 17, row: 7 },
            BindingModifierFlags::NONE,
        ),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );
    let right_release_actions = client.handle_mouse(
        build_mouse_event(
            MouseKind::Release(MouseButton::Right),
            Point { column: 17, row: 7 },
            BindingModifierFlags::NONE,
        ),
        &mouse_frame,
        advance_time_by_seconds(current_time, 3),
    );
    client.handle_mouse(
        build_left_mouse_drag(Point { column: 20, row: 8 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 4),
    );
    let left_release_actions = client.handle_mouse(
        build_left_mouse_release(Point { column: 20, row: 8 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 5),
    );

    assert_eq!(right_press_actions, Vec::new());
    assert_eq!(right_release_actions, Vec::new());
    get_floating_pane_move_command_id(
        &left_release_actions,
        floating_pane_id,
        Point { column: 18, row: 6 },
    );
}

#[test]
fn escape_during_a_handle_drag_drops_the_move() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(Point { column: 17, row: 7 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );

    assert_eq!(
        client.resolve_key(
            KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Esc)),
            advance_time_by_seconds(current_time, 2),
        ),
        KeyOutcome::Discard
    );
    assert_eq!(find_floating_pane_move_draft(&client, &mouse_frame), None);
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_release(Point { column: 17, row: 7 }),
            &mouse_frame,
            advance_time_by_seconds(current_time, 3),
        ),
        Vec::new()
    );
}

#[test]
fn escape_during_a_handle_drag_also_drops_an_open_key_sequence() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(Point { column: 17, row: 7 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    assert_eq!(
        client.resolve_key(
            KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('p')),
            advance_time_by_seconds(current_time, 2),
        ),
        KeyOutcome::Pending
    );

    assert_eq!(
        client.resolve_key(
            KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Esc)),
            advance_time_by_seconds(current_time, 3),
        ),
        KeyOutcome::Discard
    );
    assert_eq!(client.get_pending_key_sequence(), None);
    assert_eq!(find_floating_pane_move_draft(&client, &mouse_frame), None);
}

#[test]
fn keyboard_placement_drops_a_held_handle_drag() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_test_client();
    client.set_frame_view(
        active_tab_id,
        Some(floating_pane_id),
        Some(tiled_pane_id),
        vec![active_tab_id],
    );
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(Point { column: 17, row: 7 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    assert_eq!(
        find_floating_pane_move_draft(&client, &mouse_frame),
        Some(FloatingPaneMoveDraft {
            pane_id: floating_pane_id,
            top_left_cell: Point { column: 15, row: 5 },
        })
    );

    assert!(client.begin_placement_mode());
    assert_eq!(find_floating_pane_move_draft(&client, &mouse_frame), None);
    client.cancel_placement_mode();
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_release(Point { column: 17, row: 7 }),
            &mouse_frame,
            advance_time_by_seconds(current_time, 2),
        ),
        Vec::new(),
        "the release after placement mode ends sends no move"
    );
}

#[test]
fn losing_the_outer_terminals_focus_drops_a_handle_drag_and_keeps_a_sent_move() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(Point { column: 17, row: 7 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );

    client.handle_outer_terminal_focus_lost();
    assert_eq!(client.left_button_gesture, None);
    assert_eq!(find_floating_pane_move_draft(&client, &mouse_frame), None);
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_release(Point { column: 17, row: 7 }),
            &mouse_frame,
            advance_time_by_seconds(current_time, 2),
        ),
        Vec::new()
    );

    let release_actions = drag_floating_pane_handle_by_five_and_one(
        &mut client,
        &mouse_frame,
        advance_time_by_seconds(current_time, 3),
    );
    get_floating_pane_move_command_id(
        &release_actions,
        floating_pane_id,
        Point { column: 15, row: 5 },
    );
    client.handle_outer_terminal_focus_lost();
    assert_eq!(
        find_floating_pane_move_draft(&client, &mouse_frame),
        Some(FloatingPaneMoveDraft {
            pane_id: floating_pane_id,
            top_left_cell: Point { column: 15, row: 5 },
        })
    );
}

#[test]
fn a_handle_drag_ends_with_no_move_when_its_pane_leaves_the_frame() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let frame_without_floating_pane = build_mouse_frame(
        &[build_plain_mouse_pane(tiled_pane_id)],
        Some(tiled_pane_id),
    );
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );

    client.drop_gestures_the_frame_ended(&frame_without_floating_pane);
    client.handle_mouse(
        build_left_mouse_drag(Point { column: 17, row: 7 }),
        &frame_without_floating_pane,
        advance_time_by_seconds(current_time, 1),
    );
    // The pane is drawn again by the release.
    client.drop_gestures_the_frame_ended(&mouse_frame);
    let release_actions = client.handle_mouse(
        build_left_mouse_release(Point { column: 17, row: 7 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );

    assert_eq!(release_actions, Vec::new());
    assert_eq!(find_floating_pane_move_draft(&client, &mouse_frame), None);
}

#[test]
fn ending_a_handle_drag_leaves_a_border_drag_held() {
    // Losing the outer terminal's focus ends a grab-handle drag only.
    let resized_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(PaneId::new()),
            build_plain_mouse_pane(resized_pane_id),
        ],
        None,
    );
    let divider_point = Point {
        column: 10,
        row: mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[1]
            .outer_rect
            .origin
            .row
            + 1,
    };
    let mut client = build_test_client();
    client.handle_mouse(
        build_left_mouse_press(divider_point),
        &mouse_frame,
        Instant::now(),
    );

    assert!(!client.cancel_floating_pane_move_drag());
    assert_eq!(
        client.left_button_gesture,
        Some(LeftButtonGesture::BorderResize(ResizeDrag {
            pane_id: resized_pane_id,
            border_side: Direction::Up,
            corner_horizontal_side: None,
            last_accepted_position: divider_point,
        }))
    );
}

#[test]
fn a_left_press_after_a_lost_release_drops_the_stale_handle_drag_and_sends_no_move() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    // Tiled pane content the floating pane does not cover.
    let tiled_point = Point {
        column: 60,
        row: 10,
    };
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_left_mouse_drag(Point { column: 17, row: 7 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );

    // The release never arrived.
    let press_actions = client.handle_mouse(
        build_left_mouse_press(tiled_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );

    assert_eq!(
        press_actions,
        vec![build_focus_pane_mouse_action(&client, tiled_pane_id)]
    );
    assert_eq!(find_floating_pane_move_draft(&client, &mouse_frame), None);
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_release(tiled_point),
            &mouse_frame,
            advance_time_by_seconds(current_time, 3)
        ),
        Vec::new(),
        "the release sends no move"
    );
}

#[test]
fn a_pinned_floating_panes_handle_focuses_it_and_moves_nothing() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        true,
        tiled_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();

    let press_actions = client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );
    let drag_actions = client.handle_mouse(
        build_left_mouse_drag(Point { column: 17, row: 7 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    let draft_while_dragging = find_floating_pane_move_draft(&client, &mouse_frame);
    let release_actions = client.handle_mouse(
        build_left_mouse_release(Point { column: 17, row: 7 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 2),
    );

    assert_eq!(
        press_actions,
        vec![build_focus_pane_mouse_action(&client, floating_pane_id)]
    );
    assert_eq!(drag_actions, Vec::new());
    assert_eq!(draft_while_dragging, None);
    assert_eq!(release_actions, Vec::new());
}

#[test]
fn a_sent_move_stays_drawn_until_an_applied_frame_names_it() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_test_client();
    let release_actions =
        drag_floating_pane_handle_by_five_and_one(&mut client, &mouse_frame, Instant::now());
    let command_id = get_floating_pane_move_command_id(
        &release_actions,
        floating_pane_id,
        Point { column: 15, row: 5 },
    );
    let moved_draft = Some(FloatingPaneMoveDraft {
        pane_id: floating_pane_id,
        top_left_cell: Point { column: 15, row: 5 },
    });
    let mut render_snapshot = build_render_snapshot_from_mouse_frame(&mouse_frame);

    // A frame built before the session ran the move names no move.
    client.apply_render_snapshot(&render_snapshot);
    assert_eq!(
        find_floating_pane_move_draft(&client, &mouse_frame),
        moved_draft
    );
    render_snapshot
        .client_snapshot
        .last_applied_floating_pane_move_command_id = Some(CommandId::new());
    client.apply_render_snapshot(&render_snapshot);
    assert_eq!(
        find_floating_pane_move_draft(&client, &mouse_frame),
        moved_draft,
        "a frame that names another move leaves this one drawn"
    );
    render_snapshot
        .client_snapshot
        .last_applied_floating_pane_move_command_id = Some(command_id);

    client.apply_render_snapshot(&render_snapshot);

    assert_eq!(find_floating_pane_move_draft(&client, &mouse_frame), None);
    assert!(
        !client.reject_placement_command(command_id),
        "a settled move takes no rejection"
    );
}

#[test]
fn a_handle_press_while_a_sent_move_waits_only_focuses_and_keeps_the_sent_move_drawn() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        tiled_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();
    let release_actions =
        drag_floating_pane_handle_by_five_and_one(&mut client, &mouse_frame, current_time);
    get_floating_pane_move_command_id(
        &release_actions,
        floating_pane_id,
        Point { column: 15, row: 5 },
    );
    let moved_draft = Some(FloatingPaneMoveDraft {
        pane_id: floating_pane_id,
        top_left_cell: Point { column: 15, row: 5 },
    });

    // The repaint draws the pane at its draft, five columns right and one row
    // down, so its grab handle starts at screen (17, 7).
    let painted_mouse_frame = repaint_mouse_frame(&client, &mouse_frame);
    assert_eq!(
        resolve_hit_region(
            painted_mouse_frame.build_frame_layout(),
            Point { column: 17, row: 7 }
        ),
        HitRegion::PlacementHandle {
            pane_id: floating_pane_id
        }
    );
    let press_actions = client.handle_mouse(
        build_left_mouse_press(Point { column: 17, row: 7 }),
        &painted_mouse_frame,
        advance_time_by_seconds(current_time, 3),
    );
    let drag_actions = client.handle_mouse(
        build_left_mouse_drag(Point { column: 22, row: 9 }),
        &painted_mouse_frame,
        advance_time_by_seconds(current_time, 4),
    );
    let draft_after_drag = find_floating_pane_move_draft(&client, &mouse_frame);
    let second_release_actions = client.handle_mouse(
        build_left_mouse_release(Point { column: 22, row: 9 }),
        &painted_mouse_frame,
        advance_time_by_seconds(current_time, 5),
    );

    assert_eq!(
        press_actions,
        vec![build_focus_pane_mouse_action(&client, floating_pane_id)]
    );
    assert_eq!(drag_actions, Vec::new());
    assert_eq!(draft_after_drag, moved_draft);
    assert_eq!(second_release_actions, Vec::new());
    assert_eq!(
        find_floating_pane_move_draft(&client, &mouse_frame),
        moved_draft
    );
}

#[test]
fn a_rejected_move_is_drawn_where_the_session_places_the_pane_again() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_test_client();
    let release_actions =
        drag_floating_pane_handle_by_five_and_one(&mut client, &mouse_frame, Instant::now());
    let command_id = get_floating_pane_move_command_id(
        &release_actions,
        floating_pane_id,
        Point { column: 15, row: 5 },
    );

    assert!(!client.reject_placement_command(CommandId::new()));
    assert!(client.reject_placement_command(command_id));
    assert_eq!(find_floating_pane_move_draft(&client, &mouse_frame), None);
    assert!(
        !client.reject_placement_command(command_id),
        "the rejected move is gone"
    );
}

#[test]
fn dropped_events_forget_a_sent_move_and_leave_a_handle_drag() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();
    drag_floating_pane_handle_by_five_and_one(&mut client, &mouse_frame, current_time);

    client.forget_sent_floating_pane_move();
    assert_eq!(find_floating_pane_move_draft(&client, &mouse_frame), None);

    client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 3),
    );
    client.forget_sent_floating_pane_move();
    assert_eq!(
        find_floating_pane_move_draft(&client, &mouse_frame),
        Some(FloatingPaneMoveDraft {
            pane_id: floating_pane_id,
            top_left_cell: Point { column: 10, row: 4 },
        })
    );
}

#[test]
fn a_pinned_floating_pane_resizes_only_from_its_right_and_bottom_borders() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        true,
        floating_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();
    let mut drag_border = |press_position: Point, drag_position: Point, step_index: u64| {
        let press_actions = client.handle_mouse(
            build_left_mouse_press(press_position),
            &mouse_frame,
            advance_time_by_seconds(current_time, 3 * step_index),
        );
        let drag_actions = client.handle_mouse(
            build_left_mouse_drag(drag_position),
            &mouse_frame,
            advance_time_by_seconds(current_time, 3 * step_index + 1),
        );
        client.handle_mouse(
            build_left_mouse_release(drag_position),
            &mouse_frame,
            advance_time_by_seconds(current_time, 3 * step_index + 2),
        );
        assert_eq!(
            press_actions,
            vec![build_focus_pane_mouse_action(&client, floating_pane_id)],
            "a border press focuses the pane the frame focuses too"
        );
        drag_actions
    };

    assert_eq!(
        drag_border(
            Point {
                column: 10,
                row: 10
            },
            Point { column: 5, row: 10 },
            0
        ),
        Vec::new()
    );
    assert_eq!(
        drag_border(
            Point { column: 20, row: 5 },
            Point { column: 20, row: 3 },
            1
        ),
        Vec::new()
    );
    assert_eq!(
        drag_border(
            Point {
                column: 49,
                row: 10
            },
            Point {
                column: 52,
                row: 10
            },
            2
        ),
        vec![MouseAction::Resize {
            pane_id: floating_pane_id,
            border_side: Direction::Right,
            resize_step: 1,
            requested_cell_count: 3,
        }]
    );
    assert_eq!(
        drag_border(
            Point {
                column: 20,
                row: 16
            },
            Point {
                column: 20,
                row: 18
            },
            3
        ),
        vec![MouseAction::Resize {
            pane_id: floating_pane_id,
            border_side: Direction::Down,
            resize_step: 1,
            requested_cell_count: 2,
        }]
    );
}

#[test]
fn a_floating_panes_border_press_focuses_it_and_drags_any_side_when_unpinned() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        tiled_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();

    let press_actions = client.handle_mouse(
        build_left_mouse_press(Point {
            column: 10,
            row: 10,
        }),
        &mouse_frame,
        current_time,
    );
    let drag_actions = client.handle_mouse(
        build_left_mouse_drag(Point { column: 5, row: 10 }),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );

    assert_eq!(
        press_actions,
        vec![build_focus_pane_mouse_action(&client, floating_pane_id)]
    );
    assert_eq!(
        drag_actions,
        vec![MouseAction::Resize {
            pane_id: floating_pane_id,
            border_side: Direction::Left,
            resize_step: 1,
            requested_cell_count: 5,
        }]
    );
}

#[test]
fn a_floating_panes_corner_drag_resizes_both_of_its_borders() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        tiled_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();
    // The float's outer box spans screen columns 10–49 and rows 5–16.
    let bottom_right_corner = Point {
        column: 49,
        row: 16,
    };

    let press_actions = client.handle_mouse(
        build_left_mouse_press(bottom_right_corner),
        &mouse_frame,
        current_time,
    );
    assert_eq!(
        press_actions,
        vec![build_focus_pane_mouse_action(&client, floating_pane_id)]
    );
    assert_eq!(
        client.left_button_gesture,
        Some(LeftButtonGesture::BorderResize(ResizeDrag {
            pane_id: floating_pane_id,
            border_side: Direction::Right,
            corner_horizontal_side: Some(Direction::Down),
            last_accepted_position: bottom_right_corner,
        }))
    );
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_drag(Point {
                column: 52,
                row: 18
            }),
            &mouse_frame,
            advance_time_by_seconds(current_time, 1),
        ),
        vec![
            MouseAction::Resize {
                pane_id: floating_pane_id,
                border_side: Direction::Right,
                resize_step: 1,
                requested_cell_count: 3,
            },
            MouseAction::Resize {
                pane_id: floating_pane_id,
                border_side: Direction::Down,
                resize_step: 1,
                requested_cell_count: 2,
            },
        ]
    );

    // Each answer moves the anchor along its own border's axis only.
    client.note_resize_applied(floating_pane_id, Direction::Down, 1, 2);
    client.note_resize_applied(floating_pane_id, Direction::Right, 1, 1);
    assert_eq!(
        client.left_button_gesture,
        Some(LeftButtonGesture::BorderResize(ResizeDrag {
            pane_id: floating_pane_id,
            border_side: Direction::Right,
            corner_horizontal_side: Some(Direction::Down),
            last_accepted_position: Point {
                column: 50,
                row: 18
            },
        }))
    );
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_drag(Point {
                column: 52,
                row: 18
            }),
            &mouse_frame,
            advance_time_by_seconds(current_time, 2),
        ),
        vec![MouseAction::Resize {
            pane_id: floating_pane_id,
            border_side: Direction::Right,
            resize_step: 1,
            requested_cell_count: 2,
        }],
        "the bottom border reached the pointer's row, so only the right border moves"
    );
}

#[test]
fn a_pinned_floating_panes_corner_drag_moves_only_its_right_and_bottom_borders() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        true,
        floating_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();
    let mut drag_corner = |press_position: Point, drag_position: Point, step_index: u64| {
        client.handle_mouse(
            build_left_mouse_press(press_position),
            &mouse_frame,
            advance_time_by_seconds(current_time, 3 * step_index),
        );
        let drag_actions = client.handle_mouse(
            build_left_mouse_drag(drag_position),
            &mouse_frame,
            advance_time_by_seconds(current_time, 3 * step_index + 1),
        );
        client.handle_mouse(
            build_left_mouse_release(drag_position),
            &mouse_frame,
            advance_time_by_seconds(current_time, 3 * step_index + 2),
        );
        drag_actions
    };
    let build_grow_move = |border_side: Direction, requested_cell_count: u16| MouseAction::Resize {
        pane_id: floating_pane_id,
        border_side,
        resize_step: 1,
        requested_cell_count,
    };

    assert_eq!(
        drag_corner(Point { column: 10, row: 5 }, Point { column: 8, row: 3 }, 0),
        Vec::new()
    );
    assert_eq!(
        drag_corner(
            Point { column: 49, row: 5 },
            Point { column: 51, row: 3 },
            1
        ),
        vec![build_grow_move(Direction::Right, 2)]
    );
    assert_eq!(
        drag_corner(
            Point {
                column: 10,
                row: 16
            },
            Point { column: 8, row: 18 },
            2
        ),
        vec![build_grow_move(Direction::Down, 2)]
    );
    assert_eq!(
        drag_corner(
            Point {
                column: 49,
                row: 16
            },
            Point {
                column: 51,
                row: 18
            },
            3
        ),
        vec![
            build_grow_move(Direction::Right, 2),
            build_grow_move(Direction::Down, 2),
        ]
    );
}

#[test]
fn a_tiled_panes_corner_drag_moves_each_border_with_a_pane_beside_it() {
    let resized_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(PaneId::new()),
            build_plain_mouse_pane(resized_pane_id),
        ],
        None,
    );
    // The second band's top-left corner: no pane lies left of it, and the first
    // band lies above it.
    let top_left_corner = Point {
        column: 0,
        row: mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[1]
            .outer_rect
            .origin
            .row
            + 1,
    };
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(top_left_corner),
        &mouse_frame,
        current_time,
    );
    assert_eq!(
        client.left_button_gesture,
        Some(LeftButtonGesture::BorderResize(ResizeDrag {
            pane_id: resized_pane_id,
            border_side: Direction::Up,
            corner_horizontal_side: None,
            last_accepted_position: top_left_corner,
        }))
    );
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_drag(Point {
                column: 3,
                row: top_left_corner.row + 2,
            }),
            &mouse_frame,
            advance_time_by_seconds(current_time, 1),
        ),
        vec![MouseAction::Resize {
            pane_id: resized_pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 2,
        }]
    );
}

#[test]
fn a_tiled_panes_corner_drag_moves_both_borders_when_a_pane_lies_beside_each() {
    let resized_pane_id = PaneId::new();
    let mut mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(resized_pane_id),
            build_plain_mouse_pane(PaneId::new()),
            build_plain_mouse_pane(PaneId::new()),
        ],
        None,
    );
    // `A | B` above a full-width `C`: A's bottom-right corner has B on its
    // right and C below it.
    for (pane_slot, outer_rect) in mouse_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter_mut()
        .zip([
            build_cell_rect(0, 0, 40, 11),
            build_cell_rect(40, 0, 40, 11),
            build_cell_rect(0, 11, 80, 11),
        ])
    {
        pane_slot.outer_rect = outer_rect;
        pane_slot.content_rect = Some(build_cell_rect(
            outer_rect.origin.column + 1,
            outer_rect.origin.row + 1,
            outer_rect.size.column_count - 2,
            outer_rect.size.row_count - 2,
        ));
    }
    // Pane-area cell (39, 10) is one screen row lower.
    let bottom_right_corner = Point {
        column: 39,
        row: 11,
    };
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(bottom_right_corner),
        &mouse_frame,
        current_time,
    );
    assert_eq!(
        client.left_button_gesture,
        Some(LeftButtonGesture::BorderResize(ResizeDrag {
            pane_id: resized_pane_id,
            border_side: Direction::Right,
            corner_horizontal_side: Some(Direction::Down),
            last_accepted_position: bottom_right_corner,
        }))
    );
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_drag(Point {
                column: 41,
                row: 13
            }),
            &mouse_frame,
            advance_time_by_seconds(current_time, 1),
        ),
        vec![
            MouseAction::Resize {
                pane_id: resized_pane_id,
                border_side: Direction::Right,
                resize_step: 1,
                requested_cell_count: 2,
            },
            MouseAction::Resize {
                pane_id: resized_pane_id,
                border_side: Direction::Down,
                resize_step: 1,
                requested_cell_count: 2,
            },
        ]
    );
}

#[test]
fn a_floating_panes_titlebar_controls_act_without_focusing_it() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let unpinned_mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        tiled_pane_id,
    );
    let pinned_mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        true,
        tiled_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();
    let mut press_titlebar = |mouse_frame: &MouseFrame, column: u16, step_index: u64| {
        client.handle_mouse(
            build_left_mouse_press(Point { column, row: 6 }),
            mouse_frame,
            advance_time_by_seconds(current_time, step_index),
        )
    };

    assert_eq!(
        press_titlebar(&unpinned_mouse_frame, 39, 0),
        vec![MouseAction::Command(Box::new(Command::SetPanePinned(
            SetPanePinnedArgs {
                pane_id: Some(floating_pane_id),
                is_pinned: true,
            }
        )))]
    );
    assert_eq!(
        press_titlebar(&pinned_mouse_frame, 38, 1),
        vec![MouseAction::Command(Box::new(Command::SetPanePinned(
            SetPanePinnedArgs {
                pane_id: Some(floating_pane_id),
                is_pinned: false,
            }
        )))]
    );
    assert_eq!(
        press_titlebar(&unpinned_mouse_frame, 43, 2),
        vec![MouseAction::Command(Box::new(Command::SetPaneMinimized(
            SetPaneMinimizedArgs {
                pane_id: Some(floating_pane_id),
                is_minimized: true,
            }
        )))]
    );
    assert_eq!(
        press_titlebar(&unpinned_mouse_frame, 46, 3),
        vec![MouseAction::Command(Box::new(Command::ClosePane(
            ClosePaneArgs {
                pane_id: Some(floating_pane_id),
                should_force_close: false,
                should_kill_process_tree: false,
            }
        )))]
    );
}

#[test]
fn a_floating_panes_titlebar_focuses_it_also_when_the_painted_frame_focuses_it() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let unfocused_mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        tiled_pane_id,
    );
    let focused_mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();

    // Column 20 of the titlebar row is the title; row 7 is the separator.
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_press(Point { column: 20, row: 6 }),
            &unfocused_mouse_frame,
            current_time
        ),
        vec![build_focus_pane_mouse_action(&client, floating_pane_id)]
    );
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_press(Point { column: 20, row: 7 }),
            &focused_mouse_frame,
            advance_time_by_seconds(current_time, 1)
        ),
        vec![build_focus_pane_mouse_action(&client, floating_pane_id)]
    );
}

#[test]
fn hovering_a_floating_panes_grab_handle_marks_its_handle_and_its_top_border_shows_none() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;

    client.handle_mouse(
        build_mouse_motion(Point { column: 13, row: 6 }),
        &mouse_frame,
        current_time,
    );
    assert_eq!(
        client
            .build_viewer_chrome(active_tab_id)
            .placement_handle_hover,
        Some(PlacementHandleHover::FloatingHandle {
            pane_id: floating_pane_id,
            is_pinned: false,
        })
    );
    for (screen_point, time_offset_seconds) in [
        (Point { column: 20, row: 6 }, 1),
        (Point { column: 20, row: 5 }, 2),
    ] {
        client.handle_mouse(
            build_mouse_motion(screen_point),
            &mouse_frame,
            advance_time_by_seconds(current_time, time_offset_seconds),
        );
        assert_eq!(
            client
                .build_viewer_chrome(active_tab_id)
                .placement_handle_hover,
            None,
            "{screen_point:?}"
        );
    }
}

/// The placement handle hover `client` shows after each of `mouse_inputs`
/// over `mouse_frame`, in order. The frame is painted again with `client`'s
/// chrome after each event, as the attach loop paints it.
fn list_placement_handle_hovers(
    client: &mut Client,
    mouse_frame: &MouseFrame,
    mouse_inputs: &[MouseInput],
) -> Vec<Option<PlacementHandleHover>> {
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut painted_mouse_frame = repaint_mouse_frame(client, mouse_frame);
    let mut placement_handle_hovers = Vec::new();
    for mouse_input in mouse_inputs {
        client.handle_mouse(*mouse_input, &painted_mouse_frame, Instant::now());
        placement_handle_hovers.push(
            client
                .build_viewer_chrome(active_tab_id)
                .placement_handle_hover,
        );
        painted_mouse_frame = repaint_mouse_frame(client, mouse_frame);
    }
    placement_handle_hovers
}

/// One pointer motion to each `(column, row)` cell of `screen_cells`.
fn build_mouse_motions(screen_cells: &[(u16, u16)]) -> Vec<MouseInput> {
    screen_cells
        .iter()
        .map(|&(column, row)| build_mouse_motion(Point { column, row }))
        .collect()
}

/// A visible tiled pane slot whose content is `outer_rect` inset by its
/// one-cell border.
fn build_visible_pane_slot(pane_id: PaneId, outer_rect: Rect) -> PaneSlot {
    PaneSlot {
        pane_id,
        outer_rect,
        content_rect: Some(outer_rect.compute_inner_with_border()),
        is_visible: true,
        is_suppressed: false,
    }
}

#[test]
fn moving_along_a_tiled_top_border_shows_the_handle_and_marks_it_only_on_its_cells() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let mut client = build_test_client();
    let top_border = Some(PlacementHandleHover::TiledTopBorder { pane_id });
    let handle = Some(PlacementHandleHover::TiledHandle { pane_id });

    // The pane's 80-column box starts at screen (0, 1): the title, the handle,
    // the left corner, the last cell before the right corner, the right
    // corner, the content, the top border again, and the tabline row.
    let placement_handle_hovers = list_placement_handle_hovers(
        &mut client,
        &mouse_frame,
        &build_mouse_motions(&[
            (10, 1),
            (4, 1),
            (3, 1),
            (2, 1),
            (1, 1),
            (0, 1),
            (78, 1),
            (79, 1),
            (10, 2),
            (10, 1),
            (10, 0),
        ]),
    );

    assert_eq!(
        placement_handle_hovers,
        [
            top_border, top_border, handle, handle, handle, None, top_border, None, None,
            top_border, None
        ]
    );
}

#[test]
fn moving_between_two_side_by_side_top_borders_shows_only_the_handle_under_the_pointer() {
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let mut mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(left_pane_id),
            build_plain_mouse_pane(right_pane_id),
        ],
        Some(left_pane_id),
    );
    mouse_frame.session_snapshot.active_tab_snapshot.pane_slots = vec![
        build_visible_pane_slot(left_pane_id, build_cell_rect(0, 0, 40, 22)),
        build_visible_pane_slot(right_pane_id, build_cell_rect(40, 0, 40, 22)),
    ];
    let mut client = build_test_client();

    // Both top borders are on screen row 1; the corners meet at columns 39
    // and 40.
    let placement_handle_hovers = list_placement_handle_hovers(
        &mut client,
        &mouse_frame,
        &build_mouse_motions(&[
            (30, 1),
            (38, 1),
            (39, 1),
            (40, 1),
            (41, 1),
            (50, 1),
            (30, 1),
        ]),
    );

    assert_eq!(
        placement_handle_hovers,
        [
            Some(PlacementHandleHover::TiledTopBorder {
                pane_id: left_pane_id
            }),
            Some(PlacementHandleHover::TiledTopBorder {
                pane_id: left_pane_id
            }),
            None,
            None,
            Some(PlacementHandleHover::TiledHandle {
                pane_id: right_pane_id
            }),
            Some(PlacementHandleHover::TiledTopBorder {
                pane_id: right_pane_id
            }),
            Some(PlacementHandleHover::TiledTopBorder {
                pane_id: left_pane_id
            }),
        ]
    );
}

#[test]
fn a_collapsed_stack_header_and_a_pane_four_columns_wide_show_no_handle() {
    let collapsed_pane_id = PaneId::new();
    let open_pane_id = PaneId::new();
    let narrow_pane_id = PaneId::new();
    let mut mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(collapsed_pane_id),
            build_plain_mouse_pane(open_pane_id),
            build_plain_mouse_pane(narrow_pane_id),
        ],
        Some(open_pane_id),
    );
    let header_rect = build_cell_rect(0, 0, 76, 1);
    let active_tab_snapshot = &mut mouse_frame.session_snapshot.active_tab_snapshot;
    active_tab_snapshot.pane_slots = vec![
        PaneSlot {
            pane_id: collapsed_pane_id,
            outer_rect: header_rect,
            content_rect: None,
            is_visible: false,
            is_suppressed: false,
        },
        build_visible_pane_slot(open_pane_id, build_cell_rect(0, 1, 76, 21)),
        build_visible_pane_slot(narrow_pane_id, build_cell_rect(76, 0, 4, 22)),
    ];
    active_tab_snapshot.stack_headers = vec![StackHeader {
        pane_id: collapsed_pane_id,
        header_rect,
        member_index: 0,
        member_count: 2,
    }];
    let mut client = build_test_client();

    // Screen row 1 is the header and the narrow pane's top border; screen
    // row 2 is the open member's top border.
    let placement_handle_hovers = list_placement_handle_hovers(
        &mut client,
        &mouse_frame,
        &build_mouse_motions(&[(10, 1), (2, 1), (10, 2), (2, 2), (77, 1), (78, 1)]),
    );

    assert_eq!(
        placement_handle_hovers,
        [
            None,
            None,
            Some(PlacementHandleHover::TiledTopBorder {
                pane_id: open_pane_id
            }),
            Some(PlacementHandleHover::TiledHandle {
                pane_id: open_pane_id
            }),
            None,
            None,
        ]
    );
}

#[test]
fn a_press_a_drag_a_release_or_a_wheel_tick_hides_a_shown_tiled_handle_until_the_next_motion() {
    let upper_pane_id = PaneId::new();
    let lower_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(upper_pane_id),
            build_plain_mouse_pane(lower_pane_id),
        ],
        Some(upper_pane_id),
    );
    let mut client = build_test_client();
    let lower_top_border = Some(PlacementHandleHover::TiledTopBorder {
        pane_id: lower_pane_id,
    });

    // The lower pane's top border is screen row 12, the divider a border
    // drag moves.
    let placement_handle_hovers = list_placement_handle_hovers(
        &mut client,
        &mouse_frame,
        &[
            build_mouse_motion(Point {
                column: 10,
                row: 12,
            }),
            build_left_mouse_press(Point {
                column: 10,
                row: 12,
            }),
            build_left_mouse_drag(Point {
                column: 20,
                row: 12,
            }),
            build_left_mouse_release(Point {
                column: 20,
                row: 12,
            }),
            build_mouse_motion(Point {
                column: 20,
                row: 12,
            }),
            build_mouse_wheel(
                ScrollDirection::Up,
                Point {
                    column: 20,
                    row: 12,
                },
            ),
            build_mouse_motion(Point {
                column: 30,
                row: 12,
            }),
        ],
    );

    assert_eq!(
        placement_handle_hovers,
        [
            lower_top_border,
            None,
            None,
            None,
            lower_top_border,
            None,
            lower_top_border
        ]
    );
}

/// The placement handle hovers `client` shows after pointer motions to
/// `(60, 1)` and `(2, 1)` on the top border of the tiled pane that
/// [`build_floating_mouse_frame`] draws at `(0, 1)`, and the region that the
/// frame painted after them hit-tests at the handle cell `(2, 1)`. Column `60`
/// is right of the floating pane, which covers columns `10` to `49`.
fn list_tiled_top_border_hovers_and_handle_cell_region(
    client: &mut Client,
    mouse_frame: &MouseFrame,
) -> (Vec<Option<PlacementHandleHover>>, HitRegion) {
    let placement_handle_hovers = list_placement_handle_hovers(
        client,
        mouse_frame,
        &build_mouse_motions(&[(60, 1), (2, 1)]),
    );
    let handle_cell_region = resolve_hit_region(
        repaint_mouse_frame(client, mouse_frame).build_frame_layout(),
        Point { column: 2, row: 1 },
    );
    (placement_handle_hovers, handle_cell_region)
}

/// The answer of [`list_tiled_top_border_hovers_and_handle_cell_region`] while
/// the tiled pane `tiled_pane_id` shows its handle.
fn build_shown_tiled_handle_answer(
    tiled_pane_id: PaneId,
) -> (Vec<Option<PlacementHandleHover>>, HitRegion) {
    (
        vec![
            Some(PlacementHandleHover::TiledTopBorder {
                pane_id: tiled_pane_id,
            }),
            Some(PlacementHandleHover::TiledHandle {
                pane_id: tiled_pane_id,
            }),
        ],
        HitRegion::PlacementHandle {
            pane_id: tiled_pane_id,
        },
    )
}

/// The answer of [`list_tiled_top_border_hovers_and_handle_cell_region`] while
/// the tiled pane `tiled_pane_id` shows no handle: no hover, and the handle
/// cell is top border.
fn build_hidden_tiled_handle_answer(
    tiled_pane_id: PaneId,
) -> (Vec<Option<PlacementHandleHover>>, HitRegion) {
    (
        vec![None, None],
        HitRegion::PaneBorder {
            pane_id: tiled_pane_id,
            side: Direction::Up,
            corner_horizontal_side: None,
        },
    )
}

/// A client that took `mouse_frame` in, with no mode on.
fn build_viewing_client(mouse_frame: &MouseFrame) -> Client {
    let mut client = build_test_client();
    client.apply_render_snapshot(&build_render_snapshot_from_mouse_frame(mouse_frame));
    client.drop_gestures_the_frame_ended(mouse_frame);
    client
}

#[test]
fn dock_undock_mode_shows_the_tiled_handle_whose_cells_start_the_undock_drag_until_it_waits() {
    let tiled_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(PaneId::new()),
        false,
        tiled_pane_id,
    );
    let mut client = build_viewing_client(&mouse_frame);
    assert_eq!(
        client.apply_client_action(ClientActionKind::BeginDockUndock, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(client.get_active_input_mode(), LockMode::DockUndock);
    assert_eq!(
        list_tiled_top_border_hovers_and_handle_cell_region(&mut client, &mouse_frame),
        build_shown_tiled_handle_answer(tiled_pane_id)
    );
    let painted_mouse_frame = repaint_mouse_frame(&client, &mouse_frame);
    let handle_position = Point { column: 2, row: 1 };
    let drag_position = Point { column: 3, row: 1 };
    let current_time = Instant::now();

    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(handle_position),
            &painted_mouse_frame,
            current_time,
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(
        client.left_button_gesture,
        Some(LeftButtonGesture::FloatingPaneMove(
            FloatingPaneHandleDrag {
                pane_id: tiled_pane_id,
                top_left_cell: Point { column: 0, row: 0 },
                press_position: handle_position,
                press_top_left_cell: Point { column: 0, row: 0 },
            }
        ))
    );
    let drag_action = client.handle_placement_mouse(
        build_left_mouse_drag(drag_position),
        &painted_mouse_frame,
        current_time,
    );
    let Some(PlacementInputAction::SubmitCommand {
        command_id,
        command,
    }) = drag_action
    else {
        panic!("the first move of the drag sends the undock, got {drag_action:?}");
    };
    assert_eq!(
        command,
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: Some(tiled_pane_id),
            placement_target: PanePlacementTarget::Floating {
                top_left_cell: None,
            },
        })
    );
    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_release(drag_position),
            &painted_mouse_frame,
            current_time,
        ),
        Some(PlacementInputAction::Consumed)
    );

    // While the undock waits for the session, no tiled pane shows its handle.
    assert_eq!(
        list_tiled_top_border_hovers_and_handle_cell_region(&mut client, &mouse_frame),
        build_hidden_tiled_handle_answer(tiled_pane_id)
    );
    assert!(client.reject_undock_command(command_id));
    assert_eq!(
        repaint_mouse_frame(&client, &mouse_frame)
            .viewer_chrome
            .placement_handle_hover,
        Some(PlacementHandleHover::TiledHandle {
            pane_id: tiled_pane_id,
        })
    );
}

#[test]
fn pane_placement_mode_shows_no_tiled_handle() {
    let tiled_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(PaneId::new()),
        false,
        tiled_pane_id,
    );
    let mut client = build_viewing_client(&mouse_frame);
    assert!(client.begin_placement_mode());
    assert_eq!(client.get_active_input_mode(), LockMode::PanePlacement);

    assert_eq!(
        list_tiled_top_border_hovers_and_handle_cell_region(&mut client, &mouse_frame),
        build_hidden_tiled_handle_answer(tiled_pane_id)
    );
}

#[test]
fn the_move_chooser_shows_both_handles_and_a_press_on_either_starts_a_drag() {
    let [tiled_pane_id, floating_pane_id] = [PaneId::new(), PaneId::new()];
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        tiled_pane_id,
    );
    let mut client = build_viewing_client(&mouse_frame);
    client.apply_client_action(ClientActionKind::BeginPanePlacement, Some(&mouse_frame));
    assert_eq!(client.get_active_input_mode(), LockMode::PaneMoveChoice);

    assert_eq!(
        list_tiled_top_border_hovers_and_handle_cell_region(&mut client, &mouse_frame),
        build_shown_tiled_handle_answer(tiled_pane_id)
    );
    assert_eq!(
        find_grab_handle_hover(&mut client, &mouse_frame),
        Some(PlacementHandleHover::FloatingHandle {
            pane_id: floating_pane_id,
            is_pinned: false,
        })
    );
    let painted_mouse_frame = repaint_mouse_frame(&client, &mouse_frame);
    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(GRAB_HANDLE_POSITION),
            &painted_mouse_frame,
            Instant::now(),
        ),
        None
    );
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_press(GRAB_HANDLE_POSITION),
            &painted_mouse_frame,
            Instant::now(),
        ),
        vec![build_focus_pane_action(
            client.get_client_id(),
            floating_pane_id
        )]
    );
    assert_eq!(
        client.left_button_gesture,
        build_grab_handle_press_gesture(floating_pane_id)
    );
}

#[test]
fn a_viewer_with_no_link_shows_no_tiled_handle_until_it_is_linked_again() {
    let tiled_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(PaneId::new()),
        false,
        tiled_pane_id,
    );
    let mut client = build_viewing_client(&mouse_frame);
    assert_eq!(
        list_tiled_top_border_hovers_and_handle_cell_region(&mut client, &mouse_frame),
        build_shown_tiled_handle_answer(tiled_pane_id)
    );

    for link_status in [
        LinkStatus::AwaitingRestartedSession,
        LinkStatus::Redialing(Reconnecting {
            attempt: 1,
            retry_in_seconds: Some(5),
        }),
    ] {
        client.set_link_status(link_status);
        let painted_mouse_frame = repaint_mouse_frame(&client, &mouse_frame);
        assert_eq!(
            painted_mouse_frame.viewer_chrome.placement_handle_hover, None,
            "{link_status:?}"
        );
        assert_eq!(
            resolve_hit_region(
                painted_mouse_frame.build_frame_layout(),
                Point { column: 2, row: 1 },
            ),
            build_hidden_tiled_handle_answer(tiled_pane_id).1,
            "{link_status:?}"
        );
    }

    client.set_link_status(LinkStatus::Linked);
    assert_eq!(
        repaint_mouse_frame(&client, &mouse_frame)
            .viewer_chrome
            .placement_handle_hover,
        Some(PlacementHandleHover::TiledHandle {
            pane_id: tiled_pane_id,
        })
    );
}

/// The placement handle hover `client` shows after a pointer motion to
/// `(13, 6)`, the grab handle `⣿⣿` of the floating pane that
/// [`build_floating_mouse_frame`] draws at pane-area cell `(10, 4)`.
fn find_grab_handle_hover(
    client: &mut Client,
    mouse_frame: &MouseFrame,
) -> Option<PlacementHandleHover> {
    list_placement_handle_hovers(client, mouse_frame, &build_mouse_motions(&[(13, 6)]))[0]
}

/// The screen cell of the grab handle that [`find_grab_handle_hover`] moves to.
const GRAB_HANDLE_POSITION: Point = Point { column: 13, row: 6 };

/// The grab-handle drag a left press on [`GRAB_HANDLE_POSITION`] starts for
/// floating pane `floating_pane_id`, drawn at pane-area cell `(10, 4)`.
fn build_grab_handle_press_gesture(floating_pane_id: PaneId) -> Option<LeftButtonGesture> {
    Some(LeftButtonGesture::FloatingPaneMove(
        FloatingPaneHandleDrag {
            pane_id: floating_pane_id,
            top_left_cell: FLOATING_PANE_OUTER_RECT.origin,
            press_position: GRAB_HANDLE_POSITION,
            press_top_left_cell: FLOATING_PANE_OUTER_RECT.origin,
        },
    ))
}

#[test]
fn a_pinned_floating_pane_shows_its_grab_handle_hover_only_in_dock_undock_mode() {
    let [tiled_pane_id, floating_pane_id] = [PaneId::new(), PaneId::new()];
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        true,
        tiled_pane_id,
    );
    let mut client = build_viewing_client(&mouse_frame);
    assert_eq!(find_grab_handle_hover(&mut client, &mouse_frame), None);
    let painted_mouse_frame = repaint_mouse_frame(&client, &mouse_frame);
    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(GRAB_HANDLE_POSITION),
            &painted_mouse_frame,
            Instant::now(),
        ),
        None
    );
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_press(GRAB_HANDLE_POSITION),
            &painted_mouse_frame,
            Instant::now(),
        ),
        vec![build_focus_pane_action(
            client.get_client_id(),
            floating_pane_id
        )]
    );
    assert_eq!(client.left_button_gesture, None);
    client.handle_mouse(
        build_left_mouse_release(GRAB_HANDLE_POSITION),
        &painted_mouse_frame,
        Instant::now(),
    );

    assert_eq!(
        client.apply_client_action(ClientActionKind::BeginDockUndock, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(
        find_grab_handle_hover(&mut client, &mouse_frame),
        Some(PlacementHandleHover::FloatingHandle {
            pane_id: floating_pane_id,
            is_pinned: true,
        })
    );
    let painted_mouse_frame = repaint_mouse_frame(&client, &mouse_frame);
    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(GRAB_HANDLE_POSITION),
            &painted_mouse_frame,
            Instant::now(),
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(
        client.left_button_gesture,
        build_grab_handle_press_gesture(floating_pane_id)
    );
}

#[test]
fn a_sent_floating_pane_move_hides_the_grab_handle_hover_until_the_session_answers() {
    let [tiled_pane_id, floating_pane_id] = [PaneId::new(), PaneId::new()];
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        tiled_pane_id,
    );
    let mut client = build_viewing_client(&mouse_frame);
    let unpinned_grab_handle_hover = Some(PlacementHandleHover::FloatingHandle {
        pane_id: floating_pane_id,
        is_pinned: false,
    });
    assert_eq!(
        find_grab_handle_hover(&mut client, &mouse_frame),
        unpinned_grab_handle_hover
    );

    client.sent_floating_pane_move = Some(SentFloatingPaneMove {
        pane_id: floating_pane_id,
        top_left_cell: FLOATING_PANE_OUTER_RECT.origin,
        command_id: CommandId::new(),
    });
    assert_eq!(find_grab_handle_hover(&mut client, &mouse_frame), None);
    let painted_mouse_frame = repaint_mouse_frame(&client, &mouse_frame);
    client.handle_mouse(
        build_left_mouse_press(GRAB_HANDLE_POSITION),
        &painted_mouse_frame,
        Instant::now(),
    );
    assert_eq!(client.left_button_gesture, None);

    client.sent_floating_pane_move = None;
    assert_eq!(
        repaint_mouse_frame(&client, &mouse_frame)
            .viewer_chrome
            .placement_handle_hover,
        unpinned_grab_handle_hover
    );
}

#[test]
fn floating_pane_move_mode_shows_the_grab_handle_hover_until_its_move_waits() {
    let [tiled_pane_id, floating_pane_id] = [PaneId::new(), PaneId::new()];
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        tiled_pane_id,
    );
    let mut client = build_floating_pane_move_client(&mouse_frame);
    assert_eq!(client.get_active_input_mode(), LockMode::FloatingPaneMove);
    assert_eq!(
        find_grab_handle_hover(&mut client, &mouse_frame),
        Some(PlacementHandleHover::FloatingHandle {
            pane_id: floating_pane_id,
            is_pinned: false,
        })
    );

    client.pane_move_mode =
        build_floating_pane_move_mode(floating_pane_id, None, Some(CommandId::new()));
    assert_eq!(find_grab_handle_hover(&mut client, &mouse_frame), None);
    client.handle_mouse(
        build_left_mouse_press(GRAB_HANDLE_POSITION),
        &repaint_mouse_frame(&client, &mouse_frame),
        Instant::now(),
    );
    assert_eq!(client.left_button_gesture, None);
}

#[test]
fn a_dock_undock_wait_or_a_link_drop_hides_the_grab_handle_hover() {
    let [tiled_pane_id, floating_pane_id] = [PaneId::new(), PaneId::new()];
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(floating_pane_id),
        false,
        tiled_pane_id,
    );
    let mut client = build_viewing_client(&mouse_frame);
    let unpinned_grab_handle_hover = Some(PlacementHandleHover::FloatingHandle {
        pane_id: floating_pane_id,
        is_pinned: false,
    });
    for link_status in [
        LinkStatus::AwaitingRestartedSession,
        LinkStatus::Redialing(Reconnecting {
            attempt: 1,
            retry_in_seconds: Some(5),
        }),
    ] {
        assert_eq!(
            find_grab_handle_hover(&mut client, &mouse_frame),
            unpinned_grab_handle_hover
        );
        client.set_link_status(link_status);
        assert_eq!(
            repaint_mouse_frame(&client, &mouse_frame)
                .viewer_chrome
                .placement_handle_hover,
            None,
            "{link_status:?}"
        );
        client.set_link_status(LinkStatus::Linked);
    }

    assert_eq!(
        client.apply_client_action(ClientActionKind::BeginDockUndock, Some(&mouse_frame)),
        PlacementInputAction::Consumed
    );
    assert_eq!(
        find_grab_handle_hover(&mut client, &mouse_frame),
        unpinned_grab_handle_hover
    );
    let PlacementInputAction::SubmitCommand { command_id, .. } =
        client.submit_undock_command(tiled_pane_id)
    else {
        panic!("an undock sends its command");
    };
    assert_eq!(find_grab_handle_hover(&mut client, &mouse_frame), None);
    let painted_mouse_frame = repaint_mouse_frame(&client, &mouse_frame);
    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(GRAB_HANDLE_POSITION),
            &painted_mouse_frame,
            Instant::now(),
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(client.left_button_gesture, None);

    assert!(client.reject_undock_command(command_id));
    assert_eq!(
        repaint_mouse_frame(&client, &mouse_frame)
            .viewer_chrome
            .placement_handle_hover,
        unpinned_grab_handle_hover
    );
}

#[test]
fn a_locked_viewer_shows_and_marks_the_tiled_handle_the_same_way() {
    let pane_id = PaneId::new();
    let mouse_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane_id));
    let mut client = build_test_client();
    client.set_lock_mode(LockMode::Locked);

    let placement_handle_hovers = list_placement_handle_hovers(
        &mut client,
        &mouse_frame,
        &build_mouse_motions(&[(10, 1), (2, 1)]),
    );

    assert_eq!(
        placement_handle_hovers,
        [
            Some(PlacementHandleHover::TiledTopBorder { pane_id }),
            Some(PlacementHandleHover::TiledHandle { pane_id })
        ]
    );
}

#[test]
fn a_locked_viewer_moves_and_pins_a_floating_pane_the_same_way() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_test_client();
    client.set_lock_mode(LockMode::Locked);
    let current_time = Instant::now();

    let release_actions =
        drag_floating_pane_handle_by_five_and_one(&mut client, &mouse_frame, current_time);
    // The repaint draws the pane at its draft, five columns right and one row
    // down, so its pin toggle starts at screen (43, 7).
    let painted_mouse_frame = repaint_mouse_frame(&client, &mouse_frame);
    let pin_actions = client.handle_mouse(
        build_left_mouse_press(Point { column: 43, row: 7 }),
        &painted_mouse_frame,
        advance_time_by_seconds(current_time, 3),
    );

    get_floating_pane_move_command_id(
        &release_actions,
        floating_pane_id,
        Point { column: 15, row: 5 },
    );
    assert_eq!(
        pin_actions,
        vec![MouseAction::Command(Box::new(Command::SetPanePinned(
            SetPanePinnedArgs {
                pane_id: Some(floating_pane_id),
                is_pinned: true,
            }
        )))]
    );
}

#[test]
fn ending_the_gestures_drops_a_held_or_sent_floating_pane_move() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_left_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );
    client.end_mouse_gestures();
    assert_eq!(find_floating_pane_move_draft(&client, &mouse_frame), None);

    drag_floating_pane_handle_by_five_and_one(
        &mut client,
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );
    client.end_mouse_gestures();
    assert_eq!(find_floating_pane_move_draft(&client, &mouse_frame), None);
}

#[test]
fn a_captured_release_is_forwarded_when_the_frame_reports_no_releases() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    // X10 compatibility reports presses only.
    mouse_pane.mouse_tracking = MouseTracking::X10;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);
    let screen_point = get_content_cell(&mouse_frame, 0);
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );

    let release_actions = client.handle_mouse(
        build_left_mouse_release(screen_point),
        &mouse_frame,
        advance_time_by_seconds(current_time, 1),
    );

    assert_eq!(
        release_actions,
        vec![build_forwarded_mouse_action(
            pane_id,
            build_left_mouse_release(screen_point),
            Point { column: 1, row: 1 }
        )],
        "the session hears the release and ends the press it holds"
    );
    assert_eq!(client.mouse_capture, None);
}

#[test]
fn a_captured_release_whose_pane_has_no_content_cell_goes_out_at_the_capture_cell() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.mouse_tracking = MouseTracking::ButtonMotion;
    let mouse_frame = build_one_pane_mouse_frame(mouse_pane);
    let screen_point = get_content_cell(&mouse_frame, 0);
    let mut client = build_test_client();
    let current_time = Instant::now();
    client.handle_mouse(
        build_left_mouse_press(screen_point),
        &mouse_frame,
        current_time,
    );
    // The pane is still drawn, but its frame gives it no content cell.
    let mut frame_without_content = mouse_frame.clone();
    frame_without_content
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[0]
        .content_rect = None;
    client.drop_gestures_the_frame_ended(&frame_without_content);
    let release_point = Point {
        column: screen_point.column + 5,
        ..screen_point
    };

    let release_actions = client.handle_mouse(
        build_left_mouse_release(release_point),
        &frame_without_content,
        advance_time_by_seconds(current_time, 1),
    );

    assert_eq!(
        release_actions,
        vec![build_forwarded_mouse_action(
            pane_id,
            build_left_mouse_release(release_point),
            Point { column: 1, row: 1 }
        )],
        "the release goes out at the cell of the press, not under the pointer"
    );
    assert_eq!(client.mouse_capture, None);
}

// --- Floating pane move mode and the move chooser --------------------------

/// A frame of tiled pane `tiled_mouse_pane`, focused in its tab, with
/// unpinned floating pane `floating_pane_id` in [`FLOATING_PANE_OUTER_RECT`]
/// and floating pane `small_floating_pane_id` in a `20x8` box at pane-area
/// `(60, 2)`, pinned when `is_small_pane_pinned`. The client focuses
/// `floating_pane_id`.
fn build_two_floating_pane_frame(
    tiled_mouse_pane: MousePane,
    floating_pane_id: PaneId,
    small_floating_pane_id: PaneId,
    is_small_pane_pinned: bool,
) -> MouseFrame {
    let mut mouse_frame = build_floating_mouse_frame(
        tiled_mouse_pane,
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    add_floating_mouse_pane(
        &mut mouse_frame,
        build_plain_mouse_pane(small_floating_pane_id),
        Rect::from_origin_and_size(
            Point { column: 60, row: 2 },
            Size {
                column_count: 20,
                row_count: 8,
            },
        ),
        is_small_pane_pinned,
    );
    mouse_frame
}

/// A viewer that painted `mouse_frame` and opened floating pane move mode
/// with a key.
fn build_floating_pane_move_client(mouse_frame: &MouseFrame) -> Client {
    let mut client = build_test_client();
    client.apply_render_snapshot(&build_render_snapshot_from_mouse_frame(mouse_frame));
    client.drop_gestures_the_frame_ended(mouse_frame);
    client.apply_client_action(ClientActionKind::BeginFloatingPaneMove, Some(mouse_frame));
    client
}

/// Floating pane move mode that selects `selected_pane_id`, draws it at the
/// arrow keys' cell `draft_top_left_cell`, and waits for the move sent under
/// `pending_move_command_id`.
pub(crate) fn build_floating_pane_move_mode(
    selected_pane_id: PaneId,
    draft_top_left_cell: Option<Point>,
    pending_move_command_id: Option<CommandId>,
) -> Option<PaneMoveMode> {
    Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
        selected_pane_id,
        draft_top_left_cell,
        pending_move_command_id,
    }))
}

#[test]
fn floating_pane_move_mode_drags_any_unpinned_floating_pane_by_its_content() {
    let floating_pane_id = PaneId::new();
    let small_floating_pane_id = PaneId::new();
    let mouse_frame = build_two_floating_pane_frame(
        build_plain_mouse_pane(PaneId::new()),
        floating_pane_id,
        small_floating_pane_id,
        false,
    );
    for (should_stay, expected_pane_move_mode) in [
        (
            true,
            build_floating_pane_move_mode(small_floating_pane_id, None, None),
        ),
        (false, None),
    ] {
        let mut client = build_floating_pane_move_client(&mouse_frame);
        client
            .client_config
            .should_stay_in_pane_placement_mode_after_placement = should_stay;
        let current_time = Instant::now();
        assert_eq!(
            client.pane_move_mode,
            build_floating_pane_move_mode(floating_pane_id, None, None)
        );

        // Screen `(65, 7)` is pane-area `(65, 6)`, inside the small pane's
        // content.
        assert_eq!(
            client.handle_mouse(
                build_left_mouse_press(Point { column: 65, row: 7 }),
                &mouse_frame,
                current_time,
            ),
            vec![build_focus_pane_mouse_action(
                &client,
                small_floating_pane_id
            )]
        );
        assert_eq!(
            client.pane_move_mode,
            build_floating_pane_move_mode(small_floating_pane_id, None, None)
        );
        assert_eq!(
            client.handle_mouse(
                build_left_mouse_drag(Point { column: 60, row: 9 }),
                &mouse_frame,
                current_time,
            ),
            Vec::new()
        );
        assert_eq!(
            find_drawn_floating_pane_top_left_cell(&client, &mouse_frame, small_floating_pane_id),
            Some(Point { column: 55, row: 4 })
        );

        let release_actions = client.handle_mouse(
            build_left_mouse_release(Point { column: 60, row: 9 }),
            &mouse_frame,
            current_time,
        );

        let [MouseAction::MoveFloatingPane {
            command_id,
            command_args,
        }] = release_actions.as_slice()
        else {
            panic!("the release sends one move, got {release_actions:?}");
        };
        assert_eq!(
            *command_args,
            MoveFloatingPaneArgs {
                pane_id: small_floating_pane_id,
                top_left_cell: Point { column: 55, row: 4 },
            }
        );
        assert_eq!(
            client.pane_move_mode,
            Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
                selected_pane_id: small_floating_pane_id,
                draft_top_left_cell: None,
                pending_move_command_id: Some(*command_id),
            })),
            "the mode waits for the session's answer"
        );

        client.apply_render_snapshot(&build_floating_pane_move_run_snapshot(
            &mouse_frame,
            *command_id,
        ));

        assert_eq!(client.pane_move_mode, expected_pane_move_mode);
    }
}

#[test]
fn floating_pane_move_mode_hands_no_event_to_a_pane_or_a_pinned_floating_pane() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let tracking_tiled_mouse_pane = MousePane {
        mouse_tracking: MouseTracking::AnyMotion,
        ..build_plain_mouse_pane(tiled_pane_id)
    };
    let mouse_frame = build_two_floating_pane_frame(
        tracking_tiled_mouse_pane,
        floating_pane_id,
        PaneId::new(),
        true,
    );
    let mut client = build_floating_pane_move_client(&mouse_frame);
    let tiled_content_cell = Point {
        column: 70,
        row: 20,
    };
    let pinned_content_cell = Point { column: 65, row: 7 };

    for mouse_input in [
        build_left_mouse_press(tiled_content_cell),
        build_left_mouse_drag(tiled_content_cell),
        build_left_mouse_release(tiled_content_cell),
        build_mouse_event(
            MouseKind::Press(MouseButton::Right),
            tiled_content_cell,
            BindingModifierFlags::NONE,
        ),
        build_mouse_event(
            MouseKind::Release(MouseButton::Right),
            tiled_content_cell,
            BindingModifierFlags::NONE,
        ),
        build_mouse_wheel(ScrollDirection::Up, tiled_content_cell),
        build_mouse_motion(tiled_content_cell),
        build_left_mouse_press(pinned_content_cell),
        build_left_mouse_release(pinned_content_cell),
    ] {
        assert_eq!(
            client.handle_mouse(mouse_input, &mouse_frame, Instant::now()),
            Vec::new(),
            "{mouse_input:?} sends nothing"
        );
    }
    assert_eq!(client.mouse_capture, None);
    assert_eq!(client.left_button_gesture, None);
    assert_eq!(
        client.pane_move_mode,
        build_floating_pane_move_mode(floating_pane_id, None, None)
    );
}

#[test]
fn floating_pane_move_mode_starts_no_drag_while_a_sent_move_waits() {
    let floating_pane_id = PaneId::new();
    let small_floating_pane_id = PaneId::new();
    let mouse_frame = build_two_floating_pane_frame(
        build_plain_mouse_pane(PaneId::new()),
        floating_pane_id,
        small_floating_pane_id,
        false,
    );
    let keep_answer: fn(&mut Client) = |_client| {};
    // A resync and a new link drop the move drawn ahead of the session, and
    // the mode waits on until the next frame.
    let answer_losses: [fn(&mut Client); 3] = [
        keep_answer,
        Client::forget_sent_floating_pane_move,
        Client::end_mouse_gestures,
    ];
    for lose_answer in answer_losses {
        let mut client = build_floating_pane_move_client(&mouse_frame);
        let command_id =
            client.record_sent_floating_pane_move(floating_pane_id, Point { column: 20, row: 6 });
        lose_answer(&mut client);

        // Screen `(22, 8)` is inside the selected pane at either cell, and
        // `(65, 7)` is inside the small pane.
        for press_position in [Point { column: 22, row: 8 }, Point { column: 65, row: 7 }] {
            assert_eq!(
                client.handle_mouse(
                    build_left_mouse_press(press_position),
                    &mouse_frame,
                    Instant::now(),
                ),
                Vec::new(),
                "{press_position:?}"
            );
            assert_eq!(client.left_button_gesture, None, "{press_position:?}");
            assert_eq!(
                client.pane_move_mode,
                Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
                    selected_pane_id: floating_pane_id,
                    draft_top_left_cell: None,
                    pending_move_command_id: Some(command_id),
                })),
                "{press_position:?}"
            );
        }
    }
}

#[test]
fn a_press_in_floating_pane_move_mode_focuses_its_pane_also_when_the_painted_frame_focuses_it() {
    let floating_pane_id = PaneId::new();
    let small_floating_pane_id = PaneId::new();
    let mouse_frame = build_two_floating_pane_frame(
        build_plain_mouse_pane(PaneId::new()),
        floating_pane_id,
        small_floating_pane_id,
        false,
    );
    let mut client = build_floating_pane_move_client(&mouse_frame);
    let current_time = Instant::now();

    // No frame arrives between the presses: the painted frame still focuses
    // the large pane when the second press selects it again.
    for (press_position, expected_pane_id) in [
        (Point { column: 65, row: 7 }, small_floating_pane_id),
        (
            Point {
                column: 30,
                row: 10,
            },
            floating_pane_id,
        ),
    ] {
        assert_eq!(
            client.handle_mouse(
                build_left_mouse_press(press_position),
                &mouse_frame,
                current_time,
            ),
            vec![build_focus_pane_mouse_action(&client, expected_pane_id)]
        );
        assert_eq!(
            client.handle_mouse(
                build_left_mouse_release(press_position),
                &mouse_frame,
                current_time,
            ),
            Vec::new()
        );
        assert_eq!(
            client.pane_move_mode,
            build_floating_pane_move_mode(expected_pane_id, None, None)
        );
    }
}

#[test]
fn a_press_and_release_on_the_selected_pane_keeps_the_arrow_keys_draft() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_floating_pane_move_client(&mouse_frame);
    let current_time = Instant::now();
    client.apply_client_action(
        ClientActionKind::MoveFloatingPane(Direction::Right),
        Some(&mouse_frame),
    );

    // The grab handle moved one column right with the draft.
    let handle_cell = Point { column: 13, row: 6 };
    client.handle_mouse(
        build_left_mouse_press(handle_cell),
        &mouse_frame,
        current_time,
    );
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_release(handle_cell),
            &mouse_frame,
            current_time
        ),
        Vec::new()
    );

    assert_eq!(
        client.pane_move_mode,
        build_floating_pane_move_mode(floating_pane_id, Some(Point { column: 11, row: 4 }), None)
    );
    assert_eq!(
        find_drawn_floating_pane_top_left_cell(&client, &mouse_frame, floating_pane_id),
        Some(Point { column: 11, row: 4 })
    );
}

#[test]
fn floating_pane_move_mode_shows_no_tiled_handle_and_a_press_on_its_cells_opens_no_pane_placement()
{
    let tiled_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(PaneId::new()),
        false,
        tiled_pane_id,
    );
    let mut client = build_floating_pane_move_client(&mouse_frame);
    assert_eq!(
        list_tiled_top_border_hovers_and_handle_cell_region(&mut client, &mouse_frame),
        build_hidden_tiled_handle_answer(tiled_pane_id)
    );
    let mouse_frame = repaint_mouse_frame(&client, &mouse_frame);
    let handle_position = Point { column: 2, row: 1 };

    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(handle_position),
            &mouse_frame,
            Instant::now(),
        ),
        None
    );
    assert_eq!(
        client.handle_mouse(
            build_left_mouse_press(handle_position),
            &mouse_frame,
            Instant::now(),
        ),
        Vec::new()
    );
    assert!(!client.is_placement_mode_active());
    assert_eq!(client.get_active_input_mode(), LockMode::FloatingPaneMove);
}

#[test]
fn a_tiled_placement_handle_press_in_the_move_chooser_opens_pane_placement() {
    let tiled_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(tiled_pane_id),
        build_plain_mouse_pane(PaneId::new()),
        false,
        tiled_pane_id,
    );
    let mut client = build_test_client();
    client.apply_render_snapshot(&build_render_snapshot_from_mouse_frame(&mouse_frame));
    client.apply_client_action(ClientActionKind::BeginPanePlacement, Some(&mouse_frame));
    assert_eq!(client.pane_move_mode, Some(PaneMoveMode::Choice));
    client.placement_handle_hover = Some(PlacementHandleHover::TiledHandle {
        pane_id: tiled_pane_id,
    });
    let mouse_frame = repaint_mouse_frame(&client, &mouse_frame);

    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(Point { column: 1, row: 1 }),
            &mouse_frame,
            Instant::now(),
        ),
        Some(PlacementInputAction::FocusPane {
            pane_id: tiled_pane_id,
        })
    );
    assert_eq!(
        client.find_placement_preview_target(),
        Some(PanePlacementPreviewTarget {
            source_pane_id: tiled_pane_id,
            destination_tab_id: mouse_frame.client_snapshot.active_tab_id,
        })
    );
    assert_eq!(client.pane_move_mode, None);
    assert_eq!(client.get_active_input_mode(), LockMode::PanePlacement);
}

#[test]
fn a_frame_that_stops_drawing_the_selected_pane_or_pins_it_drops_the_draft() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut hidden_pane_frame = mouse_frame.clone();
    hidden_pane_frame
        .session_snapshot
        .floating_pane_slots
        .clear();
    let mut pinned_pane_frame = mouse_frame.clone();
    pinned_pane_frame.session_snapshot.floating_pane_slots[0].is_pinned = true;
    let draft_top_left_cell = Point { column: 11, row: 4 };

    let mut client = build_floating_pane_move_client(&mouse_frame);
    client.apply_client_action(
        ClientActionKind::MoveFloatingPane(Direction::Right),
        Some(&mouse_frame),
    );
    client.drop_gestures_the_frame_ended(&mouse_frame);
    assert_eq!(
        client.pane_move_mode,
        build_floating_pane_move_mode(floating_pane_id, Some(draft_top_left_cell), None),
        "a frame that draws the pane unpinned keeps the draft"
    );

    for ending_mouse_frame in [&hidden_pane_frame, &pinned_pane_frame] {
        client.set_floating_pane_move_draft(Some(draft_top_left_cell));

        client.drop_gestures_the_frame_ended(ending_mouse_frame);

        assert_eq!(
            client.pane_move_mode,
            build_floating_pane_move_mode(floating_pane_id, None, None)
        );
    }
}

#[test]
fn a_new_link_drops_the_arrow_keys_draft_and_keeps_floating_pane_move_mode() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_floating_pane_move_client(&mouse_frame);
    client.apply_client_action(
        ClientActionKind::MoveFloatingPane(Direction::Right),
        Some(&mouse_frame),
    );

    client.end_mouse_gestures();

    assert_eq!(
        client.pane_move_mode,
        build_floating_pane_move_mode(floating_pane_id, None, None)
    );
    assert_eq!(client.get_active_input_mode(), LockMode::FloatingPaneMove);
}

#[test]
fn escape_during_a_handle_drag_in_floating_pane_move_mode_ends_only_the_drag() {
    let floating_pane_id = PaneId::new();
    let mouse_frame = build_floating_mouse_frame(
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(floating_pane_id),
        false,
        floating_pane_id,
    );
    let mut client = build_floating_pane_move_client(&mouse_frame);
    let current_time = Instant::now();
    let escape_chord = KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Esc));
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
        client.resolve_key(escape_chord, current_time),
        KeyOutcome::Discard
    );
    assert_eq!(client.left_button_gesture, None);
    assert_eq!(
        client.pane_move_mode,
        build_floating_pane_move_mode(floating_pane_id, None, None)
    );
    assert_eq!(
        client.resolve_key(escape_chord, current_time),
        KeyOutcome::Fire(build_core_bound_action("cancel-floating-pane-move"))
    );
}

// --- Dock/undock mode ----------------------------------------------------------

/// A frame of tiled panes `upper_pane_id` and `lower_pane_id` as two bands,
/// with floating pane `floating_pane_id` drawn over them in
/// [`FLOATING_PANE_OUTER_RECT`] and taking the input. The active tab focuses
/// `upper_pane_id`.
fn build_dock_mouse_frame(
    upper_pane_id: PaneId,
    lower_pane_id: PaneId,
    floating_pane_id: PaneId,
) -> MouseFrame {
    let mut mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(upper_pane_id),
            build_plain_mouse_pane(lower_pane_id),
        ],
        Some(floating_pane_id),
    );
    mouse_frame.client_snapshot.active_tab_focused_pane_id = Some(upper_pane_id);
    add_floating_mouse_pane(
        &mut mouse_frame,
        build_plain_mouse_pane(floating_pane_id),
        FLOATING_PANE_OUTER_RECT,
        false,
    );
    mouse_frame
}

/// A viewer that painted `mouse_frame` and opened dock/undock mode on
/// floating pane `floating_pane_id`, holding the dock preview of the frame's
/// tab, which holds `layout_tree`.
fn build_docking_mouse_client(
    mouse_frame: &MouseFrame,
    floating_pane_id: PaneId,
    layout_tree: LayoutNode,
) -> Client {
    let mut client = build_test_client();
    client.apply_render_snapshot(&build_render_snapshot_from_mouse_frame(mouse_frame));
    client.drop_gestures_the_frame_ended(mouse_frame);
    assert_eq!(
        client.apply_client_action(ClientActionKind::BeginDockUndock, Some(mouse_frame)),
        PlacementInputAction::Consumed
    );
    let mut placement_snapshot =
        build_mouse_placement_snapshot(mouse_frame, floating_pane_id, layout_tree);
    float_placement_source_pane(&mut placement_snapshot);
    hold_placement_snapshot(&mut client, placement_snapshot);
    client
}

/// The tree `V[upper_pane_id, lower_pane_id]`.
fn build_two_band_layout_tree(upper_pane_id: PaneId, lower_pane_id: PaneId) -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            LayoutNode::Pane(upper_pane_id),
            LayoutNode::Pane(lower_pane_id),
        ],
    ))
}

#[test]
fn a_drag_of_the_selected_floating_pane_released_over_a_pane_docks_it_on_the_nearest_edge() {
    let [upper_pane_id, lower_pane_id, floating_pane_id] =
        [PaneId::new(), PaneId::new(), PaneId::new()];
    let mouse_frame = build_dock_mouse_frame(upper_pane_id, lower_pane_id, floating_pane_id);
    let tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_docking_mouse_client(
        &mouse_frame,
        floating_pane_id,
        build_two_band_layout_tree(upper_pane_id, lower_pane_id),
    );
    let current_time = Instant::now();
    let dock_right_of_lower_pane = TiledPlacementTarget::Split {
        destination_tab_id: tab_id,
        anchor: PanePlacementAnchor::Pane(lower_pane_id),
        direction: Direction::Right,
    };

    // Screen `(20, 8)` is inside the floating pane's content.
    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(Point { column: 20, row: 8 }),
            &mouse_frame,
            current_time,
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(client.get_placement_target(), None);
    // Screen `(78, 17)` is one column inside the lower pane's right border.
    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_drag(Point {
                column: 78,
                row: 17
            }),
            &mouse_frame,
            current_time,
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(
        client.get_placement_target().cloned(),
        Some(dock_right_of_lower_pane.clone())
    );

    let release_action = client.handle_placement_mouse(
        build_left_mouse_release(Point {
            column: 78,
            row: 17,
        }),
        &mouse_frame,
        current_time,
    );
    assert_eq!(
        release_action,
        Some(build_expected_confirm_placement(
            &client,
            floating_pane_id,
            dock_right_of_lower_pane
        ))
    );
    assert!(client.is_dock_undock_command_pending());
}

#[test]
fn a_pinned_selected_floating_pane_stays_at_its_cell_and_a_release_off_every_pane_sends_nothing() {
    let [upper_pane_id, lower_pane_id, floating_pane_id] =
        [PaneId::new(), PaneId::new(), PaneId::new()];
    let mut mouse_frame = build_dock_mouse_frame(upper_pane_id, lower_pane_id, floating_pane_id);
    mouse_frame.session_snapshot.floating_pane_slots[0].is_pinned = true;
    let mut client = build_docking_mouse_client(
        &mouse_frame,
        floating_pane_id,
        build_two_band_layout_tree(upper_pane_id, lower_pane_id),
    );
    let current_time = Instant::now();
    client.handle_placement_mouse(
        build_left_mouse_press(Point { column: 20, row: 8 }),
        &mouse_frame,
        current_time,
    );
    client.handle_placement_mouse(
        build_left_mouse_drag(Point { column: 25, row: 0 }),
        &mouse_frame,
        current_time,
    );

    assert_eq!(
        client
            .build_frame_layout_with_current_chrome(&mouse_frame)
            .find_drawn_floating_pane_slot(floating_pane_id),
        Some(FloatingPaneSlot::from_outer_rect(
            floating_pane_id,
            FLOATING_PANE_OUTER_RECT,
            true,
        ))
    );
    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_release(Point { column: 25, row: 0 }),
            &mouse_frame,
            current_time,
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(client.sent_floating_pane_move, None);
    assert_eq!(client.dock_undock_mode, Some(DockUndockMode::default()));
    assert_eq!(
        client.get_placement_source_pane_id(),
        Some(floating_pane_id)
    );
}

#[test]
fn a_drag_of_the_selected_floating_pane_released_off_every_pane_moves_it_and_keeps_the_mode() {
    let [upper_pane_id, lower_pane_id, floating_pane_id] =
        [PaneId::new(), PaneId::new(), PaneId::new()];
    let mouse_frame = build_dock_mouse_frame(upper_pane_id, lower_pane_id, floating_pane_id);
    let mut client = build_docking_mouse_client(
        &mouse_frame,
        floating_pane_id,
        build_two_band_layout_tree(upper_pane_id, lower_pane_id),
    );
    let current_time = Instant::now();
    client.handle_placement_mouse(
        build_left_mouse_press(Point { column: 20, row: 8 }),
        &mouse_frame,
        current_time,
    );
    client.handle_placement_mouse(
        build_left_mouse_drag(Point { column: 25, row: 0 }),
        &mouse_frame,
        current_time,
    );

    // The pointer travelled `(+5, -8)` from the press: the pane's top-left
    // cell `(10, 4)` moves to `(15, 0)`, its top row held at the pane area's.
    let release_action = client.handle_placement_mouse(
        build_left_mouse_release(Point { column: 25, row: 0 }),
        &mouse_frame,
        current_time,
    );
    let Some(PlacementInputAction::SubmitCommand { command, .. }) = release_action else {
        panic!("the release off every pane sends a move, got {release_action:?}");
    };
    assert_eq!(
        command,
        Command::MoveFloatingPane(MoveFloatingPaneArgs {
            pane_id: floating_pane_id,
            top_left_cell: Point { column: 15, row: 0 },
        })
    );
    assert_eq!(client.dock_undock_mode, Some(DockUndockMode::default()));
    assert_eq!(
        client.get_placement_source_pane_id(),
        Some(floating_pane_id)
    );
    assert_eq!(client.get_placement_target(), None);
}

#[test]
fn a_drag_of_the_selected_floating_pane_released_over_a_target_with_no_room_moves_it_instead() {
    let [upper_pane_id, lower_pane_id, floating_pane_id] =
        [PaneId::new(), PaneId::new(), PaneId::new()];
    let mouse_frame = build_dock_mouse_frame(upper_pane_id, lower_pane_id, floating_pane_id);
    let tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_docking_mouse_client(
        &mouse_frame,
        floating_pane_id,
        build_two_band_layout_tree(upper_pane_id, lower_pane_id),
    );
    let mut placement_snapshot = client
        .get_placement_snapshot()
        .expect("the dock preview is held")
        .clone();
    placement_snapshot.insertion_tab_sizes.destination_tab_size = Some(NO_ROOM_TAB_SIZE);
    hold_placement_snapshot(&mut client, placement_snapshot);
    let current_time = Instant::now();
    client.handle_placement_mouse(
        build_left_mouse_press(Point { column: 20, row: 8 }),
        &mouse_frame,
        current_time,
    );
    client.handle_placement_mouse(
        build_left_mouse_drag(Point {
            column: 78,
            row: 17,
        }),
        &mouse_frame,
        current_time,
    );
    assert_eq!(
        client.get_placement_target().cloned(),
        Some(TiledPlacementTarget::Split {
            destination_tab_id: tab_id,
            anchor: PanePlacementAnchor::Pane(lower_pane_id),
            direction: Direction::Right,
        })
    );

    // The pointer travelled `(+58, +9)` from the press: the pane's top-left
    // cell `(10, 4)` moves to `(68, 13)`, then left and up to `(40, 10)`, the
    // last cell that keeps the `40x12` pane inside the `80x22` pane area.
    let release_action = client.handle_placement_mouse(
        build_left_mouse_release(Point {
            column: 78,
            row: 17,
        }),
        &mouse_frame,
        current_time,
    );
    let Some(PlacementInputAction::SubmitCommand { command, .. }) = release_action else {
        panic!("the release over a refused target sends a move, got {release_action:?}");
    };
    assert_eq!(
        command,
        Command::MoveFloatingPane(MoveFloatingPaneArgs {
            pane_id: floating_pane_id,
            top_left_cell: Point {
                column: 40,
                row: 10
            },
        })
    );
    assert!(!client.is_dock_undock_command_pending());
    assert_eq!(client.dock_undock_mode, Some(DockUndockMode::default()));
    assert_eq!(
        client.get_placement_source_pane_id(),
        Some(floating_pane_id)
    );
    assert_eq!(client.get_placement_target(), None);
}

#[test]
fn escape_during_a_dock_drag_puts_the_pane_back_and_a_second_escape_leaves_the_mode() {
    let [upper_pane_id, lower_pane_id, floating_pane_id] =
        [PaneId::new(), PaneId::new(), PaneId::new()];
    let mouse_frame = build_dock_mouse_frame(upper_pane_id, lower_pane_id, floating_pane_id);
    let mut client = build_docking_mouse_client(
        &mouse_frame,
        floating_pane_id,
        build_two_band_layout_tree(upper_pane_id, lower_pane_id),
    );
    let current_time = Instant::now();
    let escape_chord = KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Esc));
    client.handle_placement_mouse(
        build_left_mouse_press(Point { column: 20, row: 8 }),
        &mouse_frame,
        current_time,
    );
    client.handle_placement_mouse(
        build_left_mouse_drag(Point {
            column: 78,
            row: 17,
        }),
        &mouse_frame,
        current_time,
    );

    assert_eq!(
        client.resolve_key(escape_chord, current_time),
        KeyOutcome::Discard
    );
    assert_eq!(client.left_button_gesture, None);
    assert_eq!(client.placement_state.placement_drag, None);
    assert_eq!(client.get_placement_target(), None);
    assert_eq!(
        find_drawn_floating_pane_top_left_cell(&client, &mouse_frame, floating_pane_id),
        Some(FLOATING_PANE_OUTER_RECT.origin)
    );
    assert_eq!(client.dock_undock_mode, Some(DockUndockMode::default()));
    assert_eq!(
        client.resolve_key(escape_chord, current_time),
        KeyOutcome::Fire(build_core_bound_action("cancel-dock-undock"))
    );
}

#[test]
fn a_drag_that_starts_on_a_tiled_pane_in_dock_undock_mode_floats_that_pane_once() {
    let [upper_pane_id, lower_pane_id] = [PaneId::new(), PaneId::new()];
    let mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(upper_pane_id),
            build_plain_mouse_pane(lower_pane_id),
        ],
        Some(lower_pane_id),
    );
    let mut client = build_test_client();
    client.apply_render_snapshot(&build_render_snapshot_from_mouse_frame(&mouse_frame));
    client.apply_client_action(ClientActionKind::BeginDockUndock, Some(&mouse_frame));
    let current_time = Instant::now();
    let press_point = get_content_cell(&mouse_frame, 0);
    let drag_point = Point {
        column: press_point.column + 5,
        row: press_point.row + 1,
    };

    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(press_point),
            &mouse_frame,
            current_time,
        ),
        Some(PlacementInputAction::Consumed)
    );
    let drag_action = client.handle_placement_mouse(
        build_left_mouse_drag(drag_point),
        &mouse_frame,
        current_time,
    );
    let Some(PlacementInputAction::SubmitCommand {
        command_id,
        command,
    }) = drag_action
    else {
        panic!("the first move of the drag sends the undock, got {drag_action:?}");
    };
    assert_eq!(
        command,
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: Some(upper_pane_id),
            placement_target: PanePlacementTarget::Floating {
                top_left_cell: None,
            },
        })
    );
    let selection_after_undock = Some(DockUndockMode {
        tiled_pane_selection: Some(TiledPaneSelection {
            pane_id: upper_pane_id,
            pending_undock_command_id: Some(command_id),
        }),
    });
    assert_eq!(client.dock_undock_mode, selection_after_undock);

    // While the undock waits, a further move and the release send nothing.
    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_drag(Point {
                column: drag_point.column + 1,
                row: drag_point.row,
            }),
            &mouse_frame,
            current_time,
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_release(drag_point),
            &mouse_frame,
            current_time,
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(client.left_button_gesture, None);
    assert_eq!(client.dock_undock_mode, selection_after_undock);
}

#[test]
fn a_drag_that_starts_on_a_collapsed_stack_header_in_dock_undock_mode_floats_that_member() {
    let [open_stack_pane_id, collapsed_stack_pane_id] = [PaneId::new(), PaneId::new()];
    let mut mouse_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(open_stack_pane_id),
            build_plain_mouse_pane(collapsed_stack_pane_id),
        ],
        Some(open_stack_pane_id),
    );
    // `stack(open, collapsed)`: the collapsed member is drawn as its header
    // strip on layout row 21, and shows no content.
    let header_rect = Rect::from_origin_and_size(
        Point { column: 0, row: 21 },
        Size {
            column_count: TEST_VIEWPORT_SIZE.column_count,
            row_count: 1,
        },
    );
    let active_tab_snapshot = &mut mouse_frame.session_snapshot.active_tab_snapshot;
    active_tab_snapshot.pane_slots[1] = PaneSlot {
        pane_id: collapsed_stack_pane_id,
        outer_rect: header_rect,
        content_rect: None,
        is_visible: false,
        is_suppressed: false,
    };
    active_tab_snapshot.stack_headers = vec![StackHeader {
        pane_id: collapsed_stack_pane_id,
        header_rect,
        member_index: 1,
        member_count: 2,
    }];
    let mut client = build_test_client();
    client.apply_render_snapshot(&build_render_snapshot_from_mouse_frame(&mouse_frame));
    client.apply_client_action(ClientActionKind::BeginDockUndock, Some(&mouse_frame));
    let current_time = Instant::now();
    // Layout row 21 is screen row 22, below the tabline row.
    let header_press_point = Point { column: 2, row: 22 };

    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(header_press_point),
            &mouse_frame,
            current_time,
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(
        client.left_button_gesture,
        Some(LeftButtonGesture::FloatingPaneMove(
            FloatingPaneHandleDrag {
                pane_id: collapsed_stack_pane_id,
                top_left_cell: Point { column: 0, row: 21 },
                press_position: header_press_point,
                press_top_left_cell: Point { column: 0, row: 21 },
            }
        ))
    );
    let drag_action = client.handle_placement_mouse(
        build_left_mouse_drag(Point {
            column: header_press_point.column + 5,
            row: header_press_point.row - 2,
        }),
        &mouse_frame,
        current_time,
    );
    let Some(PlacementInputAction::SubmitCommand {
        command_id,
        command,
    }) = drag_action
    else {
        panic!("the first move of the header drag sends the undock, got {drag_action:?}");
    };
    assert_eq!(
        command,
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: Some(collapsed_stack_pane_id),
            placement_target: PanePlacementTarget::Floating {
                top_left_cell: None,
            },
        })
    );
    assert_eq!(
        client.dock_undock_mode,
        Some(DockUndockMode {
            tiled_pane_selection: Some(TiledPaneSelection {
                pane_id: collapsed_stack_pane_id,
                pending_undock_command_id: Some(command_id),
            }),
        })
    );
}

#[test]
fn a_frame_that_floats_a_dragged_tiled_pane_keeps_the_drag_from_where_the_frame_draws_it() {
    let [upper_pane_id, lower_pane_id] = [PaneId::new(), PaneId::new()];
    let tiled_frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(upper_pane_id),
            build_plain_mouse_pane(lower_pane_id),
        ],
        Some(lower_pane_id),
    );
    let mut client = build_test_client();
    client.apply_render_snapshot(&build_render_snapshot_from_mouse_frame(&tiled_frame));
    client.apply_client_action(ClientActionKind::BeginDockUndock, Some(&tiled_frame));
    let current_time = Instant::now();
    let press_point = get_content_cell(&tiled_frame, 0);
    let drag_point = Point {
        column: press_point.column + 5,
        row: press_point.row + 1,
    };
    client.handle_placement_mouse(
        build_left_mouse_press(press_point),
        &tiled_frame,
        current_time,
    );
    client.handle_placement_mouse(
        build_left_mouse_drag(drag_point),
        &tiled_frame,
        current_time,
    );

    let mut floating_frame = tiled_frame.clone();
    floating_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .retain(|pane_slot| pane_slot.pane_id == lower_pane_id);
    floating_frame
        .session_snapshot
        .floating_pane_ids
        .push(upper_pane_id);
    floating_frame
        .session_snapshot
        .floating_pane_slots
        .push(FloatingPaneSlot::from_outer_rect(
            upper_pane_id,
            FLOATING_PANE_OUTER_RECT,
            false,
        ));
    client.apply_render_snapshot(&build_render_snapshot_from_mouse_frame(&floating_frame));

    assert_eq!(client.dock_undock_mode, Some(DockUndockMode::default()));
    assert_eq!(client.get_placement_source_pane_id(), Some(upper_pane_id));
    assert_eq!(
        client.placement_state.placement_drag,
        Some(PlacementDrag {
            start_position: press_point,
            has_pointer_moved: true,
            is_insertion_drag: true,
        })
    );
    // The pointer travelled `(+5, +1)` from the press: the frame's floating
    // cell `(10, 4)` is drawn at `(15, 5)`.
    assert_eq!(
        find_drawn_floating_pane_top_left_cell(&client, &floating_frame, upper_pane_id),
        Some(Point { column: 15, row: 5 })
    );

    let release_point = Point {
        column: drag_point.column + 2,
        row: drag_point.row,
    };
    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_drag(release_point),
            &floating_frame,
            current_time,
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(
        find_drawn_floating_pane_top_left_cell(&client, &floating_frame, upper_pane_id),
        Some(Point { column: 17, row: 5 })
    );
    let release_action = client.handle_placement_mouse(
        build_left_mouse_release(release_point),
        &floating_frame,
        current_time,
    );
    let Some(PlacementInputAction::SubmitCommand { command, .. }) = release_action else {
        panic!("the release off every target sends a move, got {release_action:?}");
    };
    assert_eq!(
        command,
        Command::MoveFloatingPane(MoveFloatingPaneArgs {
            pane_id: upper_pane_id,
            top_left_cell: Point { column: 17, row: 5 },
        })
    );
    assert_eq!(client.dock_undock_mode, Some(DockUndockMode::default()));
}

#[test]
fn a_press_on_another_floating_pane_in_dock_undock_mode_selects_it_and_previews_it() {
    let floating_pane_id = PaneId::new();
    let small_floating_pane_id = PaneId::new();
    let mouse_frame = build_two_floating_pane_frame(
        build_plain_mouse_pane(PaneId::new()),
        floating_pane_id,
        small_floating_pane_id,
        false,
    );
    let tab_id = mouse_frame.client_snapshot.active_tab_id;
    let mut client = build_test_client();
    client.apply_render_snapshot(&build_render_snapshot_from_mouse_frame(&mouse_frame));
    client.apply_client_action(ClientActionKind::BeginDockUndock, Some(&mouse_frame));
    assert_eq!(
        client.get_placement_source_pane_id(),
        Some(floating_pane_id)
    );

    // Screen `(65, 7)` is inside the small floating pane's content.
    assert_eq!(
        client.handle_placement_mouse(
            build_left_mouse_press(Point { column: 65, row: 7 }),
            &mouse_frame,
            Instant::now(),
        ),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(
        client.find_placement_preview_target(),
        Some(PanePlacementPreviewTarget {
            source_pane_id: small_floating_pane_id,
            destination_tab_id: tab_id,
        })
    );
    assert_eq!(client.dock_undock_mode, Some(DockUndockMode::default()));
    assert_eq!(
        client
            .placement_state
            .placement_mode
            .as_ref()
            .map(|placement_mode| (placement_mode.source_pane_id, placement_mode.source_tab_id)),
        Some((small_floating_pane_id, None))
    );
    assert_eq!(client.get_placement_target(), None);
}
