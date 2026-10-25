//! Tests for mouse hit-testing: chrome rows win over the pane area, a click maps
//! to the pane content, its border side, a stack header, or a tab, a border
//! corner reads as its vertical side and names its horizontal side, the layout
//! is centered and the letterbox margin hits nothing, a pane's content rect and
//! the cell inside it — counted from one, or clamped from zero — follow that
//! centering, a committed left region moves all of that right, a commit holding
//! no region leaves every row to the panes, the gap between two panes and a
//! pane that is not drawn hit nothing, a border cell goes to the pane whose box
//! holds it, a pane with no content cells clamps to its own origin, a stack
//! header wins the cells it covers, a tiled pane's handle takes the three
//! cells after its top-left corner and its title the cells after them at
//! every width, a shown handle hits only those three cells, a hidden handle or
//! one under pane placement hits none, and a pane four columns wide has none, the tab strip reports its first visible tab, two
//! clients of different sizes hit-test independently, degenerate
//! frames are safe, and floating panes hit front to back over the tiled layer
//! and the letterbox margin, with their content, border, titlebar and controls
//! apart.

use super::*;

use koshi_core::geometry::{Direction, Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_layout::mode::LayoutMode;
use koshi_layout::regions::{solve_region_rects, Edge, RegionGeometry};
use koshi_layout::solver::StackHeader;
use koshi_test_support::fixtures::build_cell_rect;

use crate::render::tests::compute_version_badge_column_count;
use crate::snapshot::{
    ClientSnapshot, CommittedRegions, FloatingPaneMoveDraft, FloatingPaneSlot, MouseFrame,
    PaneSlot, PlacementHandleHover, RenderSnapshot, SessionSnapshot, TabMetadata, TabSnapshot,
    ViewerChrome,
};

fn build_screen_point(screen_column: u16, screen_row: u16) -> Point {
    Point {
        column: screen_column,
        row: screen_row,
    }
}

/// Build a snapshot from explicit pieces. `panes` are `(id, outer rect,
/// visible)` in tab-layout space; a visible pane's content rect is the
/// outer rect inset by its one-cell border. `tabs` are `(id, name)`, the first
/// marked active. The panes carry no content — hit-testing reads only the slot
/// geometry, never a pane's grid.
fn build_render_snapshot(
    viewport_size: Size,
    tab_size: Size,
    pane_layouts: &[(PaneId, Rect, bool)],
    stack_headers: &[StackHeader],
    tab_ids_and_names: &[(TabId, &str)],
) -> RenderSnapshot {
    let active_tab_id = TabId::new();

    let pane_slots = pane_layouts
        .iter()
        .map(|(pane_id, outer_rect, is_visible)| PaneSlot {
            pane_id: *pane_id,
            outer_rect: *outer_rect,
            content_rect: is_visible.then(|| outer_rect.compute_inner_with_border()),
            is_visible: *is_visible,
            is_suppressed: false,
        })
        .collect();

    let tabs_metadata = tab_ids_and_names
        .iter()
        .enumerate()
        .map(|(tab_index, (tab_id, tab_name))| TabMetadata {
            tab_id: *tab_id,
            tab_name: (*tab_name).to_string(),
            tab_index,
            is_active: tab_index == 0,
        })
        .collect();

    RenderSnapshot {
        is_recovery_notice_visible: false,
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_name: "s".to_string(),
            active_tab_snapshot: TabSnapshot {
                tab_id: active_tab_id,
                tab_name: "active".to_string(),
                pane_slots,
                tab_size,
                stack_headers: stack_headers.to_vec(),
                layout_mode: LayoutMode::Tiled,
                is_every_pane_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata,
            floating_pane_slots: Vec::new(),
            floating_panes: Vec::new(),
        },
        pane_snapshots: Vec::new(),
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            viewport_size,
            active_tab_id,
            focused_pane_id: None,
            active_tab_focused_pane_id: None,
            lock_mode: LockMode::Normal,
            last_applied_lock_command_id: None,
            last_applied_floating_pane_move_command_id: None,
            is_mouse_selection_enabled: false,
            last_applied_placement_command_id: None,
        },
        placement_preview: None,
    }
}

fn build_stack_header(pane_id: PaneId, header_rect: Rect) -> StackHeader {
    StackHeader {
        pane_id,
        header_rect,
        member_index: 0,
        member_count: 2,
    }
}

/// A pane filling the whole viewport: content, border sides, and the chrome rows
/// on top of it.
#[test]
fn full_pane_content_border_and_chrome() {
    let pane_id = PaneId::new();
    let render_snapshot = build_render_snapshot(
        Size {
            column_count: 40,
            row_count: 10,
        },
        Size {
            column_count: 40,
            row_count: 10,
        },
        &[(pane_id, build_cell_rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );

    // Inside the border → content.
    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(20, 5)
        ),
        HitRegion::PaneContent { pane_id }
    );
    // Left and right border columns.
    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(0, 5)
        ),
        HitRegion::PaneBorder {
            pane_id,
            side: Direction::Left,
            corner_horizontal_side: None,
        }
    );
    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(39, 5)
        ),
        HitRegion::PaneBorder {
            pane_id,
            side: Direction::Right,
            corner_horizontal_side: None,
        }
    );
    // Top row is the tabline (drawn over the pane), off any tab ribbon here.
    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(20, 0)
        ),
        HitRegion::Tabline
    );
    // Bottom row is the statusline.
    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(20, 9)
        ),
        HitRegion::Statusline
    );
}

/// A layout smaller than the viewport centers, exposing the top and bottom
/// border rows, and the surrounding margin hits nothing.
#[test]
fn centered_layout_exposes_top_bottom_borders_and_letterbox() {
    let pane_id = PaneId::new();
    // content_rect centers 40x10 in 44x14 at origin (2, 2).
    let render_snapshot = build_render_snapshot(
        Size {
            column_count: 44,
            row_count: 14,
        },
        Size {
            column_count: 40,
            row_count: 10,
        },
        &[(pane_id, build_cell_rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );

    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(22, 7)
        ),
        HitRegion::PaneContent { pane_id }
    );
    // Top border row of the pane, now below the tabline.
    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(22, 2)
        ),
        HitRegion::PaneBorder {
            pane_id,
            side: Direction::Up,
            corner_horizontal_side: None,
        }
    );
    // Bottom border row of the pane, above the statusline.
    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(22, 11)
        ),
        HitRegion::PaneBorder {
            pane_id,
            side: Direction::Down,
            corner_horizontal_side: None,
        }
    );
    // Left of the content rect → letterbox margin.
    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(0, 7)
        ),
        HitRegion::None
    );
    // A non-chrome row above the content rect → letterbox margin.
    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(22, 1)
        ),
        HitRegion::None
    );
}

#[test]
fn mouse_hit_testing_stays_on_the_painted_region_revision() {
    let pane_id = PaneId::new();
    let viewport_size = Size {
        column_count: 120,
        row_count: 40,
    };
    let tab_size = Size {
        column_count: 100,
        row_count: 38,
    };
    let render_snapshot = build_render_snapshot(
        viewport_size,
        tab_size,
        &[(
            pane_id,
            build_cell_rect(0, 0, tab_size.column_count, tab_size.row_count),
            true,
        )],
        &[],
        &[],
    );
    let default_regions = CommittedRegions::build_core(viewport_size, 4);
    let side_regions = CommittedRegions::from_solved_regions(
        viewport_size,
        solve_region_rects(
            viewport_size,
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
        5,
    );

    let painted_mouse_frame = MouseFrame::from_snapshot(
        &render_snapshot,
        default_regions.clone(),
        ViewerChrome::default(),
    );
    assert_eq!(
        resolve_hit_region(
            painted_mouse_frame.build_frame_layout(),
            build_screen_point(12, 2)
        ),
        HitRegion::PaneContent { pane_id }
    );
    assert_eq!(
        resolve_hit_region(
            painted_mouse_frame.build_frame_layout(),
            build_screen_point(1, 0)
        ),
        HitRegion::Tabline
    );
    assert_eq!(
        resolve_hit_region(
            painted_mouse_frame.build_frame_layout(),
            build_screen_point(1, 39)
        ),
        HitRegion::Statusline
    );
    assert_eq!(
        painted_mouse_frame.committed_regions.region_input_revision,
        4
    );

    let replacement_mouse_frame =
        MouseFrame::from_snapshot(&render_snapshot, side_regions, ViewerChrome::default());
    assert_eq!(
        resolve_hit_region(
            replacement_mouse_frame.build_frame_layout(),
            build_screen_point(12, 2)
        ),
        HitRegion::None
    );
    assert_eq!(
        painted_mouse_frame.committed_regions.region_input_revision,
        4
    );
}

/// A left region moves the pane area right: the pane box, its content rect and
/// the cell inside it all follow the committed solve, and the region's own cells
/// belong to no pane.
#[test]
fn a_left_region_shifts_the_pane_area_right() {
    let pane_id = PaneId::new();
    let viewport_size = Size {
        column_count: 120,
        row_count: 40,
    };
    let tab_size = Size {
        column_count: 100,
        row_count: 38,
    };
    let render_snapshot = build_render_snapshot(
        viewport_size,
        tab_size,
        &[(
            pane_id,
            build_cell_rect(0, 0, tab_size.column_count, tab_size.row_count),
            true,
        )],
        &[],
        &[],
    );
    let painted_mouse_frame = MouseFrame::from_snapshot(
        &render_snapshot,
        CommittedRegions::from_solved_regions(
            viewport_size,
            solve_region_rects(
                viewport_size,
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
        ),
        ViewerChrome::default(),
    );
    let resolve_screen_hit_region = |screen_column, screen_row| {
        resolve_hit_region(
            painted_mouse_frame.build_frame_layout(),
            build_screen_point(screen_column, screen_row),
        )
    };

    // The pane rectangle left by the solve starts at (20, 1).
    assert_eq!(
        resolve_screen_hit_region(20, 1),
        HitRegion::PaneBorder {
            pane_id,
            side: Direction::Left,
            corner_horizontal_side: Some(Direction::Up),
        }
    );
    assert_eq!(
        resolve_screen_hit_region(21, 2),
        HitRegion::PaneContent { pane_id }
    );
    // The left region's own cells.
    assert_eq!(resolve_screen_hit_region(19, 20), HitRegion::None);
    // Both chrome rows still span the whole width.
    assert_eq!(resolve_screen_hit_region(0, 0), HitRegion::Tabline);
    assert_eq!(resolve_screen_hit_region(0, 39), HitRegion::Statusline);

    assert_eq!(
        find_pane_content_rect(painted_mouse_frame.build_frame_layout(), pane_id),
        Some(build_cell_rect(21, 2, 98, 36))
    );
    assert_eq!(
        compute_clamped_pane_cell(
            painted_mouse_frame.build_frame_layout(),
            pane_id,
            build_screen_point(21, 2)
        ),
        Some(Point { column: 0, row: 0 })
    );
    assert_eq!(
        compute_clamped_pane_cell(
            painted_mouse_frame.build_frame_layout(),
            pane_id,
            build_screen_point(0, 0)
        ),
        Some(Point { column: 0, row: 0 })
    );
}

/// The blank column between two panes belongs to neither of them.
#[test]
fn the_gap_between_two_panes_hits_nothing() {
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let render_snapshot = build_render_snapshot(
        Size {
            column_count: 40,
            row_count: 10,
        },
        Size {
            column_count: 40,
            row_count: 10,
        },
        &[
            (left_pane_id, build_cell_rect(0, 0, 19, 10), true),
            (right_pane_id, build_cell_rect(21, 0, 19, 10), true),
        ],
        &[],
        &[],
    );
    let resolve_screen_hit_region = |screen_column, screen_row| {
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(screen_column, screen_row),
        )
    };

    assert_eq!(
        resolve_screen_hit_region(10, 5),
        HitRegion::PaneContent {
            pane_id: left_pane_id,
        }
    );
    assert_eq!(
        resolve_screen_hit_region(30, 5),
        HitRegion::PaneContent {
            pane_id: right_pane_id,
        }
    );
    // Column 20 lies between the two boxes.
    assert_eq!(resolve_screen_hit_region(20, 5), HitRegion::None);
}

/// A committed solve holding no region at all leaves every row to the panes:
/// nothing is chrome, and the tab strip reports no window.
#[test]
fn a_committed_solve_with_no_regions_leaves_every_row_to_the_panes() {
    let pane_id = PaneId::new();
    let viewport_size = Size {
        column_count: 40,
        row_count: 10,
    };
    let render_snapshot = build_render_snapshot(
        viewport_size,
        viewport_size,
        &[(pane_id, build_cell_rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );
    let painted_mouse_frame = MouseFrame::from_snapshot(
        &render_snapshot,
        CommittedRegions::from_solved_regions(
            viewport_size,
            solve_region_rects(viewport_size, &[]),
            3,
        ),
        ViewerChrome::default(),
    );
    let resolve_screen_hit_region = |screen_column, screen_row| {
        resolve_hit_region(
            painted_mouse_frame.build_frame_layout(),
            build_screen_point(screen_column, screen_row),
        )
    };

    // The rows the tabline and the statusline would own are the pane's own top
    // and bottom border rows.
    assert_eq!(
        resolve_screen_hit_region(20, 0),
        HitRegion::PaneBorder {
            pane_id,
            side: Direction::Up,
            corner_horizontal_side: None,
        }
    );
    assert_eq!(
        resolve_screen_hit_region(20, 9),
        HitRegion::PaneBorder {
            pane_id,
            side: Direction::Down,
            corner_horizontal_side: None,
        }
    );
    assert_eq!(
        resolve_screen_hit_region(20, 5),
        HitRegion::PaneContent { pane_id }
    );
    assert_eq!(
        find_first_visible_tab_index(painted_mouse_frame.build_frame_layout()),
        None
    );
}

/// A pane box two columns wide and one row tall insets to a content area of no
/// cells: it names no cell of its own, every clamped cell is its `(0, 0)`, and
/// both its columns hit-test as border.
#[test]
fn a_pane_with_a_zero_cell_content_area_clamps_every_cell_to_its_origin() {
    let pane_id = PaneId::new();
    let render_snapshot = build_render_snapshot(
        Size {
            column_count: 40,
            row_count: 10,
        },
        Size {
            column_count: 40,
            row_count: 10,
        },
        &[(pane_id, build_cell_rect(0, 5, 2, 1), true)],
        &[],
        &[],
    );
    let build_frame_layout = || render_snapshot.build_frame_layout(ViewerChrome::default());

    // The outer box spans (0, 5)-(1, 5); the inset content is empty at (1, 6).
    assert_eq!(
        find_pane_content_rect(build_frame_layout(), pane_id),
        Some(build_cell_rect(1, 6, 0, 0)),
        "an empty content rect keeps the inset origin"
    );
    assert_eq!(
        compute_clamped_pane_cell(build_frame_layout(), pane_id, build_screen_point(1, 6)),
        Some(Point { column: 0, row: 0 })
    );
    assert_eq!(
        compute_clamped_pane_cell(build_frame_layout(), pane_id, build_screen_point(39, 9)),
        Some(Point { column: 0, row: 0 })
    );
    assert_eq!(
        resolve_hit_region(build_frame_layout(), build_screen_point(0, 5)),
        HitRegion::PaneBorder {
            pane_id,
            side: Direction::Left,
            corner_horizontal_side: Some(Direction::Up),
        }
    );
    assert_eq!(
        resolve_hit_region(build_frame_layout(), build_screen_point(1, 5)),
        HitRegion::PaneBorder {
            pane_id,
            side: Direction::Right,
            corner_horizontal_side: Some(Direction::Up),
        }
    );
}

/// A pane that is not drawn is skipped even when its rectangle covers the cell,
/// and the visible pane behind it answers.
#[test]
fn a_hidden_pane_does_not_swallow_a_click_on_the_pane_behind_it() {
    let hidden_pane_id = PaneId::new();
    let visible_pane_id = PaneId::new();
    let render_snapshot = build_render_snapshot(
        Size {
            column_count: 40,
            row_count: 10,
        },
        Size {
            column_count: 40,
            row_count: 10,
        },
        &[
            (hidden_pane_id, build_cell_rect(0, 0, 40, 10), false),
            (visible_pane_id, build_cell_rect(0, 0, 40, 10), true),
        ],
        &[],
        &[],
    );

    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(20, 5)
        ),
        HitRegion::PaneContent {
            pane_id: visible_pane_id,
        }
    );
}

/// A stack header strip drawn over a pane wins the cells it covers.
#[test]
fn a_stack_header_wins_a_cell_over_the_pane_under_it() {
    let pane_id = PaneId::new();
    let member_pane_id = PaneId::new();
    let render_snapshot = build_render_snapshot(
        Size {
            column_count: 40,
            row_count: 10,
        },
        Size {
            column_count: 40,
            row_count: 10,
        },
        &[(pane_id, build_cell_rect(0, 0, 40, 10), true)],
        &[build_stack_header(
            member_pane_id,
            build_cell_rect(1, 5, 38, 1),
        )],
        &[],
    );
    let resolve_screen_hit_region = |screen_column, screen_row| {
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(screen_column, screen_row),
        )
    };

    assert_eq!(
        resolve_screen_hit_region(20, 5),
        HitRegion::StackHeader {
            pane_id: member_pane_id,
        }
    );
    // One row above the strip is the pane's own content again.
    assert_eq!(
        resolve_screen_hit_region(20, 4),
        HitRegion::PaneContent { pane_id }
    );
}

/// A one-row viewport is all tabline: no statusline row exists, and a cell past
/// the single row hits nothing.
#[test]
fn a_one_row_viewport_is_all_tabline() {
    let tab_id = TabId::new();
    let render_snapshot = build_render_snapshot(
        Size {
            column_count: 40,
            row_count: 1,
        },
        Size {
            column_count: 40,
            row_count: 1,
        },
        &[],
        &[],
        &[(tab_id, "t")],
    );
    let resolve_screen_hit_region = |screen_column, screen_row| {
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(screen_column, screen_row),
        )
    };

    // The session block on the left and the mode tag on the right.
    assert_eq!(resolve_screen_hit_region(0, 0), HitRegion::Tabline);
    assert_eq!(resolve_screen_hit_region(39, 0), HitRegion::Tabline);
    // There is no second row to hit.
    assert_eq!(resolve_screen_hit_region(20, 1), HitRegion::None);
    assert_eq!(
        find_first_visible_tab_index(render_snapshot.build_frame_layout(ViewerChrome::default())),
        Some(0)
    );
}

/// A collapsed stack member's strip hit-tests to its pane.
#[test]
fn stack_header_hits_its_pane() {
    let member_pane_id = PaneId::new();
    let render_snapshot = build_render_snapshot(
        Size {
            column_count: 40,
            row_count: 10,
        },
        Size {
            column_count: 40,
            row_count: 10,
        },
        &[],
        &[build_stack_header(
            member_pane_id,
            build_cell_rect(0, 3, 40, 1),
        )],
        &[],
    );
    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(20, 3)
        ),
        HitRegion::StackHeader {
            pane_id: member_pane_id,
        }
    );
}

/// Tabs map to their own ids by column; the session block and the gaps between
/// tabs are the bare tabline.
#[test]
fn tabs_hit_by_column() {
    use crate::render::solve_tabline_layout;
    use ratatui::layout::Rect as RatatuiRect;

    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    // The session block and its version badge hold the left, then each 7-cell
    // tab ribbon with a one-cell gap between them. The row is the badge's
    // width plus 31 columns, and the tab columns come from the same solve the
    // paint uses.
    let tabline_column_count = compute_version_badge_column_count() + 31;
    let render_snapshot = build_render_snapshot(
        Size {
            column_count: tabline_column_count,
            row_count: 10,
        },
        Size {
            column_count: tabline_column_count,
            row_count: 10,
        },
        &[],
        &[],
        &[(first_tab_id, "a"), (second_tab_id, "b")],
    );
    let visible_tab_spans = solve_tabline_layout(
        render_snapshot
            .build_frame_layout(ViewerChrome::default())
            .get_tabline_inputs(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: tabline_column_count,
            height: 1,
        },
    )
    .visible_tab_spans;
    assert_eq!(visible_tab_spans.len(), 2);

    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(visible_tab_spans[0].start_column + 1, 0),
        ),
        HitRegion::Tab {
            tab_id: first_tab_id,
        }
    );
    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(visible_tab_spans[1].start_column + 1, 0),
        ),
        HitRegion::Tab {
            tab_id: second_tab_id,
        }
    );
    // The one-cell gap between the two ribbons.
    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(visible_tab_spans[1].start_column - 1, 0),
        ),
        HitRegion::Tabline
    );
    // The session block on the left.
    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(1, 0)
        ),
        HitRegion::Tabline
    );
}

/// The cell of each scroll arrow hit-tests to that arrow.
#[test]
fn scroll_arrows_hit_test_to_their_arrows() {
    use crate::render::solve_tabline_layout;
    use ratatui::layout::Rect as RatatuiRect;

    let tab_ids: Vec<TabId> = (0..8).map(|_| TabId::new()).collect();
    let tab_ids_and_names: Vec<(TabId, &str)> =
        tab_ids.iter().map(|tab_id| (*tab_id, "tab")).collect();
    // The row is the version badge's width plus 21 columns for the tab strip.
    let tabline_column_count = compute_version_badge_column_count() + 21;
    let render_snapshot = build_render_snapshot(
        Size {
            column_count: tabline_column_count,
            row_count: 8,
        },
        Size {
            column_count: tabline_column_count,
            row_count: 8,
        },
        &[],
        &[],
        &tab_ids_and_names,
    );
    // Peek from index 2, so tabs are hidden off both sides.
    let viewer_chrome = ViewerChrome {
        tabline_offset: Some(2),
        ..ViewerChrome::default()
    };

    let tabline_area = RatatuiRect {
        x: 0,
        y: 0,
        width: tabline_column_count,
        height: 8,
    };
    let tabline_layout = solve_tabline_layout(
        render_snapshot
            .build_frame_layout(viewer_chrome)
            .get_tabline_inputs(),
        tabline_area,
    );
    let left_scroll_arrow_column = tabline_layout
        .left_scroll_arrow_column
        .expect("tabs hidden off the left");
    let right_scroll_arrow_column = tabline_layout
        .right_scroll_arrow_column
        .expect("tabs hidden off the right");

    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(viewer_chrome),
            build_screen_point(left_scroll_arrow_column, 0),
        ),
        HitRegion::TablineScrollLeft
    );
    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(viewer_chrome),
            build_screen_point(right_scroll_arrow_column, 0),
        ),
        HitRegion::TablineScrollRight
    );
}

/// The too-small overlay and a zero-size viewport hit nothing.
#[test]
fn degenerate_frames_hit_nothing() {
    let pane_id = PaneId::new();
    let mut suppressed = build_render_snapshot(
        Size {
            column_count: 40,
            row_count: 10,
        },
        Size {
            column_count: 40,
            row_count: 10,
        },
        &[(pane_id, build_cell_rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );
    suppressed
        .session_snapshot
        .active_tab_snapshot
        .is_every_pane_suppressed = true;
    assert_eq!(
        resolve_hit_region(
            suppressed.build_frame_layout(ViewerChrome::default()),
            build_screen_point(20, 5)
        ),
        HitRegion::None
    );

    let zero_size_render_snapshot = build_render_snapshot(
        Size {
            column_count: 0,
            row_count: 0,
        },
        Size {
            column_count: 0,
            row_count: 0,
        },
        &[],
        &[],
        &[],
    );
    assert_eq!(
        resolve_hit_region(
            zero_size_render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(0, 0)
        ),
        HitRegion::None
    );
}

/// A border corner cell reads as the left or right edge and names the top or
/// bottom edge it also lies on.
#[test]
fn a_border_corner_reads_as_its_vertical_side_and_names_its_horizontal_side() {
    let pane_id = PaneId::new();
    // Centered, so the pane's own top and bottom rows sit clear of the chrome
    // rows and all four corners are reachable: the box spans (2, 2)–(41, 11).
    let render_snapshot = build_render_snapshot(
        Size {
            column_count: 44,
            row_count: 14,
        },
        Size {
            column_count: 40,
            row_count: 10,
        },
        &[(pane_id, build_cell_rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );
    let resolve_border_corner_hit_region = |screen_column, screen_row| {
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(screen_column, screen_row),
        )
    };

    assert_eq!(
        resolve_border_corner_hit_region(2, 2),
        HitRegion::PaneBorder {
            pane_id,
            side: Direction::Left,
            corner_horizontal_side: Some(Direction::Up),
        }
    );
    assert_eq!(
        resolve_border_corner_hit_region(2, 11),
        HitRegion::PaneBorder {
            pane_id,
            side: Direction::Left,
            corner_horizontal_side: Some(Direction::Down),
        }
    );
    assert_eq!(
        resolve_border_corner_hit_region(41, 2),
        HitRegion::PaneBorder {
            pane_id,
            side: Direction::Right,
            corner_horizontal_side: Some(Direction::Up),
        }
    );
    assert_eq!(
        resolve_border_corner_hit_region(41, 11),
        HitRegion::PaneBorder {
            pane_id,
            side: Direction::Right,
            corner_horizontal_side: Some(Direction::Down),
        }
    );
}

/// The top border parts of an `outer_column_count`-column, 10-row tiled pane
/// at `(0, 0)`.
fn compute_top_border_at_width(outer_column_count: u16) -> TiledPaneTopBorder {
    compute_tiled_pane_top_border(build_cell_rect(0, 0, outer_column_count, 10))
}

#[test]
fn a_tiled_top_border_places_the_handle_from_five_columns_and_the_title_from_seven() {
    let handle_rect = Some(build_cell_rect(1, 0, 3, 1));
    for (outer_column_count, expected_top_border) in [
        (
            4,
            TiledPaneTopBorder {
                placement_handle_rect: None,
                title_rect: None,
            },
        ),
        (
            5,
            TiledPaneTopBorder {
                placement_handle_rect: handle_rect,
                title_rect: None,
            },
        ),
        (
            6,
            TiledPaneTopBorder {
                placement_handle_rect: handle_rect,
                title_rect: None,
            },
        ),
        (
            7,
            TiledPaneTopBorder {
                placement_handle_rect: handle_rect,
                title_rect: Some(build_cell_rect(4, 0, 1, 1)),
            },
        ),
        (
            20,
            TiledPaneTopBorder {
                placement_handle_rect: handle_rect,
                title_rect: Some(build_cell_rect(4, 0, 14, 1)),
            },
        ),
    ] {
        assert_eq!(
            compute_top_border_at_width(outer_column_count),
            expected_top_border,
            "{outer_column_count} columns"
        );
    }
    assert_eq!(
        compute_tiled_pane_top_border(build_cell_rect(10, 3, 20, 10)),
        TiledPaneTopBorder {
            placement_handle_rect: Some(build_cell_rect(11, 3, 3, 1)),
            title_rect: Some(build_cell_rect(14, 3, 14, 1)),
        }
    );
}

#[test]
fn a_tiled_top_border_never_puts_the_handle_and_the_title_on_one_cell() {
    for outer_column_count in 0..=300 {
        let top_border = compute_top_border_at_width(outer_column_count);
        if let Some(handle_rect) = top_border.placement_handle_rect {
            assert_eq!(handle_rect.origin.column, 1, "{outer_column_count} columns");
            assert!(
                handle_rect.origin.column + handle_rect.size.column_count < outer_column_count,
                "{outer_column_count} columns"
            );
        }
        if let Some(title_rect) = top_border.title_rect {
            assert_eq!(title_rect.origin.column, 4, "{outer_column_count} columns");
            assert_eq!(
                title_rect.origin.column + title_rect.size.column_count,
                outer_column_count - 2,
                "{outer_column_count} columns"
            );
        }
    }
}

/// A tiled pane whose handle shows hits the handle on the three cells after
/// its top-left corner and its top border on every title cell after them. A
/// hidden handle, and a handle while pane placement shows, hit no handle
/// cell.
#[test]
fn a_shown_tiled_handle_hits_its_three_cells_and_the_title_cells_hit_the_top_border() {
    let pane_id = PaneId::new();
    // Centered: the box spans (2, 2)–(21, 11).
    let render_snapshot = build_render_snapshot(
        Size {
            column_count: 24,
            row_count: 14,
        },
        Size {
            column_count: 20,
            row_count: 10,
        },
        &[(pane_id, build_cell_rect(0, 0, 20, 10), true)],
        &[],
        &[],
    );
    let viewer_chrome = ViewerChrome {
        placement_handle_hover: Some(PlacementHandleHover::TiledTopBorder { pane_id }),
        ..ViewerChrome::default()
    };

    let top_row_hit_regions: Vec<HitRegion> = (2..22)
        .map(|screen_column| {
            resolve_hit_region(
                render_snapshot.build_frame_layout(viewer_chrome),
                build_screen_point(screen_column, 2),
            )
        })
        .collect();

    let mut expected_hit_regions = vec![HitRegion::PaneBorder {
        pane_id,
        side: Direction::Left,
        corner_horizontal_side: Some(Direction::Up),
    }];
    expected_hit_regions.extend([HitRegion::PlacementHandle { pane_id }; 3]);
    expected_hit_regions.extend(
        [HitRegion::PaneBorder {
            pane_id,
            side: Direction::Up,
            corner_horizontal_side: None,
        }; 15],
    );
    expected_hit_regions.push(HitRegion::PaneBorder {
        pane_id,
        side: Direction::Right,
        corner_horizontal_side: Some(Direction::Up),
    });
    assert_eq!(top_row_hit_regions, expected_hit_regions);
    for hidden_handle_viewer_chrome in [
        ViewerChrome::default(),
        ViewerChrome {
            placement_handle_hover: Some(PlacementHandleHover::TiledHandle {
                pane_id: PaneId::new(),
            }),
            ..ViewerChrome::default()
        },
    ] {
        assert_eq!(
            resolve_hit_region(
                render_snapshot.build_frame_layout(hidden_handle_viewer_chrome),
                build_screen_point(3, 2),
            ),
            HitRegion::PaneBorder {
                pane_id,
                side: Direction::Up,
                corner_horizontal_side: None,
            }
        );
    }
}

/// A tiled pane four columns wide draws no handle, so its top border has no
/// handle cell even while the viewer names it.
#[test]
fn a_tiled_pane_too_narrow_for_the_handle_has_no_handle_cell() {
    let pane_id = PaneId::new();
    // Centered: the box spans (2, 2)–(5, 11).
    let render_snapshot = build_render_snapshot(
        Size {
            column_count: 8,
            row_count: 14,
        },
        Size {
            column_count: 4,
            row_count: 10,
        },
        &[(pane_id, build_cell_rect(0, 0, 4, 10), true)],
        &[],
        &[],
    );
    let viewer_chrome = ViewerChrome {
        placement_handle_hover: Some(PlacementHandleHover::TiledHandle { pane_id }),
        ..ViewerChrome::default()
    };

    let top_row_hit_regions: Vec<HitRegion> = (2..6)
        .map(|screen_column| {
            resolve_hit_region(
                render_snapshot.build_frame_layout(viewer_chrome),
                build_screen_point(screen_column, 2),
            )
        })
        .collect();

    let top_border_hit_region = HitRegion::PaneBorder {
        pane_id,
        side: Direction::Up,
        corner_horizontal_side: None,
    };
    assert_eq!(
        top_row_hit_regions,
        [
            HitRegion::PaneBorder {
                pane_id,
                side: Direction::Left,
                corner_horizontal_side: Some(Direction::Up),
            },
            top_border_hit_region,
            top_border_hit_region,
            HitRegion::PaneBorder {
                pane_id,
                side: Direction::Right,
                corner_horizontal_side: Some(Direction::Up),
            },
        ]
    );
}

/// In a top and bottom split, a border cell on the column both boxes share goes
/// to the pane whose box holds the cell.
#[test]
fn a_border_cell_goes_to_the_pane_whose_box_holds_it() {
    let top_pane_id = PaneId::new();
    let bottom_pane_id = PaneId::new();
    let render_snapshot = build_render_snapshot(
        Size {
            column_count: 40,
            row_count: 10,
        },
        Size {
            column_count: 40,
            row_count: 10,
        },
        &[
            (top_pane_id, build_cell_rect(0, 0, 40, 5), true),
            (bottom_pane_id, build_cell_rect(0, 5, 40, 5), true),
        ],
        &[],
        &[],
    );
    let resolve_screen_hit_region = |screen_column, screen_row| {
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(screen_column, screen_row),
        )
    };

    assert_eq!(
        resolve_screen_hit_region(0, 7),
        HitRegion::PaneBorder {
            pane_id: bottom_pane_id,
            side: Direction::Left,
            corner_horizontal_side: None,
        }
    );
    assert_eq!(
        resolve_screen_hit_region(0, 4),
        HitRegion::PaneBorder {
            pane_id: top_pane_id,
            side: Direction::Left,
            corner_horizontal_side: Some(Direction::Down),
        }
    );
}

/// A 40x10 layout centered in a 44x14 viewport, with one visible pane filling
/// it and one hidden pane beside it.
fn build_centered_render_snapshot(
    visible_pane_id: PaneId,
    hidden_pane_id: PaneId,
) -> RenderSnapshot {
    build_render_snapshot(
        Size {
            column_count: 44,
            row_count: 14,
        },
        Size {
            column_count: 40,
            row_count: 10,
        },
        &[
            (visible_pane_id, build_cell_rect(0, 0, 40, 10), true),
            (hidden_pane_id, build_cell_rect(0, 0, 6, 4), false),
        ],
        &[],
        &[],
    )
}

/// A pane's content rect is its border inset, shifted into the centered layout.
#[test]
fn a_pane_content_rect_is_its_border_inset_shifted_into_the_centered_layout() {
    let pane_id = PaneId::new();
    let hidden_pane_id = PaneId::new();
    let render_snapshot = build_centered_render_snapshot(pane_id, hidden_pane_id);

    // The layout origin is (2, 2); the pane's content starts one cell further
    // in on both axes and loses one cell on each side.
    assert_eq!(
        find_pane_content_rect(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            pane_id
        ),
        Some(build_cell_rect(3, 3, 38, 8))
    );
    // A hidden pane and a pane that is not in this frame have no content rect.
    assert_eq!(
        find_pane_content_rect(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            hidden_pane_id
        ),
        None
    );
    assert_eq!(
        find_pane_content_rect(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            PaneId::new()
        ),
        None
    );

    // The too-small overlay draws no pane at all.
    let mut suppressed = build_centered_render_snapshot(pane_id, hidden_pane_id);
    suppressed
        .session_snapshot
        .active_tab_snapshot
        .is_every_pane_suppressed = true;
    assert_eq!(
        find_pane_content_rect(
            suppressed.build_frame_layout(ViewerChrome::default()),
            pane_id
        ),
        None
    );

    // A zero-size viewport has nowhere to put it.
    let mut zero_size_render_snapshot = build_centered_render_snapshot(pane_id, hidden_pane_id);
    zero_size_render_snapshot.client_snapshot.viewport_size = Size {
        column_count: 0,
        row_count: 0,
    };
    assert_eq!(
        find_pane_content_rect(
            zero_size_render_snapshot.build_frame_layout(ViewerChrome::default()),
            pane_id,
        ),
        None
    );
}

/// A cell outside a pane's content is pulled to the nearest edge cell of it,
/// counted from `(0, 0)`.
#[test]
fn a_pane_cell_clamped_pulls_an_outside_cell_to_the_nearest_edge() {
    let pane_id = PaneId::new();
    let render_snapshot = build_centered_render_snapshot(pane_id, PaneId::new());
    let compute_clamped_screen_point_pane_cell = |screen_column, screen_row| {
        compute_clamped_pane_cell(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            pane_id,
            build_screen_point(screen_column, screen_row),
        )
    };

    // The content rect spans columns 3–40 and rows 3–10, so its own cells run
    // (0, 0) to (37, 7).
    assert_eq!(
        compute_clamped_screen_point_pane_cell(3, 3),
        Some(Point { column: 0, row: 0 })
    );
    assert_eq!(
        compute_clamped_screen_point_pane_cell(40, 10),
        Some(Point { column: 37, row: 7 })
    );
    // Past the far corner, and before the near corner: both pull inside.
    assert_eq!(
        compute_clamped_screen_point_pane_cell(200, 200),
        Some(Point { column: 37, row: 7 })
    );
    assert_eq!(
        compute_clamped_screen_point_pane_cell(0, 0),
        Some(Point { column: 0, row: 0 })
    );
    // Off one axis only: that axis clamps, the other keeps its cell.
    assert_eq!(
        compute_clamped_screen_point_pane_cell(1, 7),
        Some(Point { column: 0, row: 4 })
    );
    assert_eq!(
        compute_clamped_screen_point_pane_cell(20, 13),
        Some(Point { column: 17, row: 7 })
    );
    // A pane that is not drawn this frame names no cell at all.
    assert_eq!(
        compute_clamped_pane_cell(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            PaneId::new(),
            build_screen_point(3, 3),
        ),
        None
    );
}

/// The first-visible index is the first tab the tabline draws: the viewer's
/// offset clamped to the last tab, or the active tab's own index.
#[test]
fn tabline_first_visible_reports_the_first_tab_drawn() {
    let tab_ids: Vec<TabId> = (0..8).map(|_| TabId::new()).collect();
    let tab_ids_and_names: Vec<(TabId, &str)> =
        tab_ids.iter().map(|tab_id| (*tab_id, "tab")).collect();
    // The same row width the scroll-arrow test uses: eight tabs do not fit, so
    // the strip scrolls.
    let tabline_column_count = compute_version_badge_column_count() + 21;
    let render_snapshot = build_render_snapshot(
        Size {
            column_count: tabline_column_count,
            row_count: 8,
        },
        Size {
            column_count: tabline_column_count,
            row_count: 8,
        },
        &[],
        &[],
        &tab_ids_and_names,
    );
    let build_viewer_chrome = |tabline_offset| ViewerChrome {
        tabline_offset: Some(tabline_offset),
        ..ViewerChrome::default()
    };

    assert_eq!(
        find_first_visible_tab_index(render_snapshot.build_frame_layout(build_viewer_chrome(2))),
        Some(2)
    );
    // An index past the last tab clamps to it.
    assert_eq!(
        find_first_visible_tab_index(render_snapshot.build_frame_layout(build_viewer_chrome(99))),
        Some(7)
    );
    // Following the active tab, which is the first one, starts at the start.
    assert_eq!(
        find_first_visible_tab_index(render_snapshot.build_frame_layout(ViewerChrome::default())),
        Some(0)
    );
}

/// A frame that draws no tabline has no first-visible index: every pane
/// suppressed for want of room, or a zero-size viewport.
#[test]
fn tabline_first_visible_is_none_when_no_tabline_is_drawn() {
    let tab_id = TabId::new();
    let mut suppressed = build_render_snapshot(
        Size {
            column_count: 80,
            row_count: 24,
        },
        Size {
            column_count: 80,
            row_count: 24,
        },
        &[],
        &[],
        &[(tab_id, "tab")],
    );
    suppressed
        .session_snapshot
        .active_tab_snapshot
        .is_every_pane_suppressed = true;
    assert_eq!(
        find_first_visible_tab_index(suppressed.build_frame_layout(ViewerChrome::default())),
        None
    );

    let zero_size_render_snapshot = build_render_snapshot(
        Size {
            column_count: 0,
            row_count: 0,
        },
        Size {
            column_count: 0,
            row_count: 0,
        },
        &[],
        &[],
        &[(tab_id, "tab")],
    );
    assert_eq!(
        find_first_visible_tab_index(
            zero_size_render_snapshot.build_frame_layout(ViewerChrome::default())
        ),
        None
    );
}

/// Two clients viewing the same layout at different sizes hit-test in their own
/// coordinate spaces.
#[test]
fn two_clients_hit_test_independently() {
    let pane_id = PaneId::new();
    let small_client_render_snapshot = build_render_snapshot(
        Size {
            column_count: 40,
            row_count: 10,
        },
        Size {
            column_count: 40,
            row_count: 10,
        },
        &[(pane_id, build_cell_rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );
    let large_client_render_snapshot = build_render_snapshot(
        Size {
            column_count: 44,
            row_count: 14,
        },
        Size {
            column_count: 40,
            row_count: 10,
        },
        &[(pane_id, build_cell_rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );

    // The small client fills the viewport: (22, 7) is content.
    assert_eq!(
        resolve_hit_region(
            small_client_render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(22, 7)
        ),
        HitRegion::PaneContent { pane_id }
    );
    // The large client centers the layout: the same cell is content too, but a
    // cell in its margin — where the small client had content — hits nothing.
    assert_eq!(
        resolve_hit_region(
            large_client_render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(22, 7)
        ),
        HitRegion::PaneContent { pane_id }
    );
    assert_eq!(
        resolve_hit_region(
            large_client_render_snapshot.build_frame_layout(ViewerChrome::default()),
            build_screen_point(1, 7)
        ),
        HitRegion::None
    );
}

/// A `120x40` viewport holding one tiled pane that fills a tab of `tab_size`.
/// [`build_core_mouse_frame`] gives it the tabline on row 0 and the
/// statusline on row 39, so the tab centers in a `120x38` pane area that
/// starts at screen `(0, 1)`.
fn build_floating_frame(tiled_pane_id: PaneId, tab_size: Size) -> RenderSnapshot {
    build_render_snapshot(
        Size {
            column_count: 120,
            row_count: 40,
        },
        tab_size,
        &[(
            tiled_pane_id,
            Rect::from_origin_and_size(Point { column: 0, row: 0 }, tab_size),
            true,
        )],
        &[],
        &[],
    )
}

/// The frame `render_snapshot` paints with the tabline on its first row and
/// the statusline on its last, painted with `viewer_chrome`.
fn build_core_mouse_frame(
    render_snapshot: &RenderSnapshot,
    viewer_chrome: ViewerChrome,
) -> MouseFrame {
    MouseFrame::from_snapshot(
        render_snapshot,
        CommittedRegions::build_core(render_snapshot.client_snapshot.viewport_size, 0),
        viewer_chrome,
    )
}

/// The tab size that fills the whole `120x38` pane area of
/// [`build_floating_frame`].
const FULL_PANE_AREA_TAB_SIZE: Size = Size {
    column_count: 120,
    row_count: 38,
};

/// Add the floating pane `pane_id` in the box `outer_rect`, counted in
/// pane-area cells, on top of every floating pane already in the frame.
fn push_floating_pane_slot(
    render_snapshot: &mut RenderSnapshot,
    pane_id: PaneId,
    outer_rect: Rect,
) {
    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .push(FloatingPaneSlot::from_outer_rect(
            pane_id, outer_rect, false,
        ));
}

/// Float `B` over float `A` over the tiled pane `T`: each cell goes to the
/// topmost pane drawn there.
#[test]
fn a_cell_goes_to_the_topmost_floating_pane_drawn_over_it() {
    let tiled_pane_id = PaneId::new();
    let lower_floating_pane_id = PaneId::new();
    let upper_floating_pane_id = PaneId::new();
    let mut render_snapshot = build_floating_frame(tiled_pane_id, FULL_PANE_AREA_TAB_SIZE);
    push_floating_pane_slot(
        &mut render_snapshot,
        lower_floating_pane_id,
        build_cell_rect(10, 5, 40, 15),
    );
    push_floating_pane_slot(
        &mut render_snapshot,
        upper_floating_pane_id,
        build_cell_rect(30, 10, 40, 15),
    );
    let resolve_screen_hit_region = |screen_column, screen_row| {
        resolve_hit_region(
            build_core_mouse_frame(&render_snapshot, ViewerChrome::default()).build_frame_layout(),
            build_screen_point(screen_column, screen_row),
        )
    };

    // Pane-area (35, 15) lies in both floats and in the tiled pane.
    assert_eq!(
        resolve_screen_hit_region(35, 16),
        HitRegion::PaneContent {
            pane_id: upper_floating_pane_id
        }
    );
    // Pane-area (15, 15) lies in the lower float and the tiled pane.
    assert_eq!(
        resolve_screen_hit_region(15, 16),
        HitRegion::PaneContent {
            pane_id: lower_floating_pane_id
        }
    );
    // Pane-area (100, 30) lies in the tiled pane only.
    assert_eq!(
        resolve_screen_hit_region(100, 31),
        HitRegion::PaneContent {
            pane_id: tiled_pane_id
        }
    );
    // The upper float's left border covers the lower float's content.
    assert_eq!(
        resolve_screen_hit_region(30, 16),
        HitRegion::PaneBorder {
            pane_id: upper_floating_pane_id,
            side: Direction::Left,
            corner_horizontal_side: None,
        }
    );
}

/// A floating pane drawn in the letterbox margin is hit there, and the same
/// cell without it is margin.
#[test]
fn a_floating_pane_in_the_letterbox_margin_is_hit() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    // A 60x20 tab centered in the 120x38 pane area spans screen columns 30–89.
    let mut render_snapshot = build_floating_frame(
        tiled_pane_id,
        Size {
            column_count: 60,
            row_count: 20,
        },
    );
    assert_eq!(
        resolve_hit_region(
            build_core_mouse_frame(&render_snapshot, ViewerChrome::default()).build_frame_layout(),
            build_screen_point(5, 6)
        ),
        HitRegion::None
    );

    push_floating_pane_slot(
        &mut render_snapshot,
        floating_pane_id,
        build_cell_rect(0, 0, 20, 8),
    );

    assert_eq!(
        resolve_hit_region(
            build_core_mouse_frame(&render_snapshot, ViewerChrome::default()).build_frame_layout(),
            build_screen_point(5, 6)
        ),
        HitRegion::PaneContent {
            pane_id: floating_pane_id
        }
    );
    assert_eq!(
        resolve_hit_region(
            build_core_mouse_frame(&render_snapshot, ViewerChrome::default()).build_frame_layout(),
            build_screen_point(5, 1)
        ),
        HitRegion::PaneBorder {
            pane_id: floating_pane_id,
            side: Direction::Up,
            corner_horizontal_side: None,
        }
    );
}

/// No floating pane is drawn, so none is hit, while the pane placement preview
/// shows or while every pane is suppressed.
#[test]
fn no_floating_pane_is_hit_while_placement_shows_or_every_pane_is_suppressed() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let mut render_snapshot = build_floating_frame(tiled_pane_id, FULL_PANE_AREA_TAB_SIZE);
    push_floating_pane_slot(
        &mut render_snapshot,
        floating_pane_id,
        build_cell_rect(10, 5, 40, 15),
    );
    let placement_viewer_chrome = ViewerChrome {
        is_pane_placement_visible: true,
        ..ViewerChrome::default()
    };

    assert_eq!(
        resolve_hit_region(
            build_core_mouse_frame(&render_snapshot, placement_viewer_chrome).build_frame_layout(),
            build_screen_point(15, 16)
        ),
        HitRegion::PaneContent {
            pane_id: tiled_pane_id
        }
    );
    assert_eq!(
        find_pane_content_rect(
            build_core_mouse_frame(&render_snapshot, placement_viewer_chrome).build_frame_layout(),
            floating_pane_id
        ),
        None
    );

    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .is_every_pane_suppressed = true;
    assert_eq!(
        resolve_hit_region(
            build_core_mouse_frame(&render_snapshot, ViewerChrome::default()).build_frame_layout(),
            build_screen_point(15, 16)
        ),
        HitRegion::None
    );
    assert_eq!(
        find_pane_content_rect(
            build_core_mouse_frame(&render_snapshot, ViewerChrome::default()).build_frame_layout(),
            floating_pane_id
        ),
        None
    );
}

/// The titlebar row's controls sit where the floating pane's chrome draws
/// them; its other cells and the separator row are titlebar.
#[test]
fn a_floating_panes_titlebar_parts_hit_where_they_are_drawn() {
    let floating_pane_id = PaneId::new();
    let mut render_snapshot = build_floating_frame(PaneId::new(), FULL_PANE_AREA_TAB_SIZE);
    push_floating_pane_slot(
        &mut render_snapshot,
        floating_pane_id,
        build_cell_rect(0, 0, 48, 12),
    );
    // Each pane-area cell is one screen row lower.
    let resolve_pane_area_hit_region = |pane_area_column: u16, pane_area_row: u16| {
        resolve_hit_region(
            build_core_mouse_frame(&render_snapshot, ViewerChrome::default()).build_frame_layout(),
            build_screen_point(pane_area_column, pane_area_row + 1),
        )
    };
    let pane_id = floating_pane_id;

    for (pane_area_column, pane_area_row, expected_hit_region) in [
        (2, 1, HitRegion::PlacementHandle { pane_id }),
        (3, 1, HitRegion::PlacementHandle { pane_id }),
        (4, 1, HitRegion::PaneTitlebar { pane_id }),
        (5, 1, HitRegion::PaneTitlebar { pane_id }),
        (6, 1, HitRegion::PaneTitlebar { pane_id }),
        (35, 1, HitRegion::PaneTitlebar { pane_id }),
        (36, 1, HitRegion::PanePinToggle { pane_id }),
        (39, 1, HitRegion::PanePinToggle { pane_id }),
        (40, 1, HitRegion::PaneTitlebar { pane_id }),
        (41, 1, HitRegion::PaneMinimizeButton { pane_id }),
        (42, 1, HitRegion::PaneTitlebar { pane_id }),
        (44, 1, HitRegion::PaneCloseButton { pane_id }),
        (46, 1, HitRegion::PaneTitlebar { pane_id }),
        (5, 2, HitRegion::PaneTitlebar { pane_id }),
        (5, 3, HitRegion::PaneContent { pane_id }),
        (
            0,
            1,
            HitRegion::PaneBorder {
                pane_id,
                side: Direction::Left,
                corner_horizontal_side: None,
            },
        ),
        (
            0,
            2,
            HitRegion::PaneBorder {
                pane_id,
                side: Direction::Left,
                corner_horizontal_side: None,
            },
        ),
        (
            47,
            1,
            HitRegion::PaneBorder {
                pane_id,
                side: Direction::Right,
                corner_horizontal_side: None,
            },
        ),
        (
            5,
            0,
            HitRegion::PaneBorder {
                pane_id,
                side: Direction::Up,
                corner_horizontal_side: None,
            },
        ),
        (
            5,
            11,
            HitRegion::PaneBorder {
                pane_id,
                side: Direction::Down,
                corner_horizontal_side: None,
            },
        ),
        (
            0,
            0,
            HitRegion::PaneBorder {
                pane_id,
                side: Direction::Left,
                corner_horizontal_side: Some(Direction::Up),
            },
        ),
        (
            47,
            11,
            HitRegion::PaneBorder {
                pane_id,
                side: Direction::Right,
                corner_horizontal_side: Some(Direction::Down),
            },
        ),
    ] {
        assert_eq!(
            resolve_pane_area_hit_region(pane_area_column, pane_area_row),
            expected_hit_region,
            "pane-area cell ({pane_area_column}, {pane_area_row})"
        );
    }
}

/// A floating pane's content rect counts from the pane-area origin, with no
/// letterbox offset.
#[test]
fn a_floating_panes_content_rect_takes_no_letterbox_offset() {
    let floating_pane_id = PaneId::new();
    let mut render_snapshot = build_floating_frame(
        PaneId::new(),
        Size {
            column_count: 60,
            row_count: 20,
        },
    );
    push_floating_pane_slot(
        &mut render_snapshot,
        floating_pane_id,
        build_cell_rect(24, 8, 72, 22),
    );

    assert_eq!(
        find_pane_content_rect(
            build_core_mouse_frame(&render_snapshot, ViewerChrome::default()).build_frame_layout(),
            floating_pane_id
        ),
        Some(build_cell_rect(25, 12, 70, 18))
    );
    assert_eq!(
        compute_clamped_pane_cell(
            build_core_mouse_frame(&render_snapshot, ViewerChrome::default()).build_frame_layout(),
            floating_pane_id,
            build_screen_point(30, 14)
        ),
        Some(Point { column: 5, row: 2 })
    );
}

/// The move draft draws the floating pane it names at the draft's top-left
/// cell, and the cells it left go to the pane under them.
#[test]
fn a_move_draft_moves_the_floating_pane_it_names() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let mut render_snapshot = build_floating_frame(tiled_pane_id, FULL_PANE_AREA_TAB_SIZE);
    push_floating_pane_slot(
        &mut render_snapshot,
        floating_pane_id,
        build_cell_rect(10, 5, 40, 15),
    );
    let draft_viewer_chrome = ViewerChrome {
        floating_pane_move_draft: Some(FloatingPaneMoveDraft {
            pane_id: floating_pane_id,
            top_left_cell: Point {
                column: 60,
                row: 20,
            },
        }),
        ..ViewerChrome::default()
    };

    assert_eq!(
        resolve_hit_region(
            build_core_mouse_frame(&render_snapshot, draft_viewer_chrome).build_frame_layout(),
            build_screen_point(15, 16)
        ),
        HitRegion::PaneContent {
            pane_id: tiled_pane_id
        }
    );
    assert_eq!(
        resolve_hit_region(
            build_core_mouse_frame(&render_snapshot, draft_viewer_chrome).build_frame_layout(),
            build_screen_point(65, 31)
        ),
        HitRegion::PaneContent {
            pane_id: floating_pane_id
        }
    );
    assert_eq!(
        find_pane_content_rect(
            build_core_mouse_frame(&render_snapshot, draft_viewer_chrome).build_frame_layout(),
            floating_pane_id
        ),
        Some(build_cell_rect(61, 24, 38, 11))
    );
}

#[test]
fn an_open_tray_hits_its_tiles_its_label_and_its_body_over_the_floating_pane_under_it() {
    use crate::floating_pane_tray::tests::{
        build_open_tray_viewer_chrome, build_tray_test_render_snapshot,
        show_tray_test_floating_pane,
    };

    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let mut render_snapshot = build_tray_test_render_snapshot(viewport_size, &["htop", "logs"]);
    // Outer box at screen (16, 5) 48x13: its content starts at screen row 8,
    // the tray's bottom border.
    let htop_pane_id =
        show_tray_test_floating_pane(&mut render_snapshot, 0, build_cell_rect(16, 4, 48, 13));
    let committed_regions = CommittedRegions::build_core(viewport_size, 0);
    let open_frame_layout = render_snapshot
        .build_painted_frame_layout(build_open_tray_viewer_chrome(0), &committed_regions);

    for (screen_point, expected_hit_region) in [
        (
            build_screen_point(20, 3),
            HitRegion::FloatingPaneTrayTile { tile_index: 0 },
        ),
        (
            build_screen_point(35, 4),
            HitRegion::FloatingPaneTrayTile { tile_index: 1 },
        ),
        (build_screen_point(37, 2), HitRegion::FloatingPaneTray),
        (build_screen_point(20, 6), HitRegion::FloatingPaneTray),
        (build_screen_point(27, 3), HitRegion::FloatingPaneTray),
        (
            build_screen_point(58, 1),
            HitRegion::FloatingPaneTrayToggleMinimizeAll,
        ),
        (
            build_screen_point(71, 1),
            HitRegion::FloatingPaneTrayToggleMinimizeAll,
        ),
        (build_screen_point(57, 1), HitRegion::FloatingPaneTray),
        (build_screen_point(17, 1), HitRegion::FloatingPaneTray),
        (build_screen_point(20, 8), HitRegion::FloatingPaneTray),
        (build_screen_point(73, 8), HitRegion::FloatingPaneTray),
        (build_screen_point(72, 0), HitRegion::FloatingPaneTrayButton),
        (
            build_screen_point(20, 9),
            HitRegion::PaneContent {
                pane_id: htop_pane_id,
            },
        ),
    ] {
        assert_eq!(
            resolve_hit_region(open_frame_layout, screen_point),
            expected_hit_region,
            "{screen_point:?}"
        );
    }
}

#[test]
fn a_closed_tray_leaves_its_cells_to_the_panes_and_keeps_its_button() {
    use crate::floating_pane_tray::tests::{
        build_tray_test_render_snapshot, show_tray_test_floating_pane,
    };

    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let mut render_snapshot = build_tray_test_render_snapshot(viewport_size, &["htop", "logs"]);
    let htop_pane_id =
        show_tray_test_floating_pane(&mut render_snapshot, 0, build_cell_rect(16, 4, 48, 13));
    let tiled_pane_id = render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[0]
        .pane_id;
    let committed_regions = CommittedRegions::build_core(viewport_size, 0);
    let closed_frame_layout =
        render_snapshot.build_painted_frame_layout(ViewerChrome::default(), &committed_regions);

    for (screen_point, expected_hit_region) in [
        (
            build_screen_point(20, 3),
            HitRegion::PaneContent {
                pane_id: tiled_pane_id,
            },
        ),
        (
            build_screen_point(20, 8),
            HitRegion::PaneContent {
                pane_id: htop_pane_id,
            },
        ),
        (build_screen_point(71, 0), HitRegion::FloatingPaneTrayButton),
        (build_screen_point(72, 0), HitRegion::FloatingPaneTrayButton),
        (build_screen_point(73, 0), HitRegion::FloatingPaneTrayButton),
        (build_screen_point(74, 0), HitRegion::Tabline),
    ] {
        assert_eq!(
            resolve_hit_region(closed_frame_layout, screen_point),
            expected_hit_region,
            "{screen_point:?}"
        );
    }
}

#[test]
fn a_session_with_no_floating_pane_leaves_the_button_cells_to_the_tabline() {
    use crate::floating_pane_tray::tests::build_tray_test_render_snapshot;

    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let render_snapshot = build_tray_test_render_snapshot(viewport_size, &[]);
    let committed_regions = CommittedRegions::build_core(viewport_size, 0);

    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_painted_frame_layout(ViewerChrome::default(), &committed_regions),
            build_screen_point(72, 0)
        ),
        HitRegion::Tabline
    );
}
