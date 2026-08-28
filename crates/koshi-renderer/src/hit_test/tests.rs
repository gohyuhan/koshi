//! Tests for mouse hit-testing: chrome rows win over the pane area, a click maps
//! to the pane content, its border side, a stack header, or a tab, a border
//! corner reads as its vertical side, the layout is centered and the letterbox
//! margin hits nothing, a pane's content rect and the cell inside it — counted
//! from one, or clamped from zero — follow that centering, the tab strip reports
//! the window it draws, two clients of different sizes hit-test independently,
//! and degenerate frames are safe.

use super::*;

use koshi_core::geometry::{Direction, Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_layout::mode::LayoutMode;
use koshi_layout::regions::{solve, Edge, RegionGeometry};
use koshi_layout::solver::StackHeader;
use koshi_pane::pane::state::PaneKind;

use crate::snapshot::{CommittedRegions, MouseFrame, ViewerChrome};

/// Cells the tabline's version badge takes, measured from the badge the tabline
/// actually paints.
///
/// A test that needs room beside the badge asks for `badge_cols() + <room>`
/// rather than a fixed total, so a longer version string moves the layout
/// instead of failing the test. A semver version is ASCII, so counting
/// characters counts display cells.
fn badge_cols() -> u16 {
    crate::render::version_badge().chars().count() as u16
}

/// A viewer with nothing hovered and its tab strip following the active tab.
fn chrome() -> ViewerChrome {
    ViewerChrome::default()
}

use crate::snapshot::{
    ClientSnapshot, PaneSlot, PluginUiSnapshot, RenderSnapshot, SessionSnapshot, TabMeta,
    TabSnapshot,
};

/// A cell rect: origin `(x, y)`, size `cols x rows`.
fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Rect {
    Rect {
        origin: Point { x, y },
        size: Size { cols, rows },
    }
}

fn at(x: u16, y: u16) -> Point {
    Point { x, y }
}

/// Build a snapshot from explicit pieces. `panes` are `(id, outer rect,
/// visible)` in effective-layout space; a visible pane's content rect is the
/// outer rect inset by its one-cell border. `tabs` are `(id, name)`, the first
/// marked active. The panes carry no content — hit-testing reads only the slot
/// geometry, never a pane's grid.
fn snap(
    viewport: Size,
    effective: Size,
    panes: &[(PaneId, Rect, bool)],
    headers: &[StackHeader],
    tabs: &[(TabId, &str)],
) -> RenderSnapshot {
    let tab_id = TabId::new();

    let slots = panes
        .iter()
        .map(|(id, outer, visible)| PaneSlot {
            pane_id: *id,
            rect: *outer,
            inner_rect: visible.then(|| outer.inner_with_border()),
            kind: PaneKind::Terminal,
            visible: *visible,
            suppressed: false,
            dead: false,
        })
        .collect();

    let tabs_metadata = tabs
        .iter()
        .enumerate()
        .map(|(index, (id, name))| TabMeta {
            id: *id,
            name: (*name).to_string(),
            index,
            active: index == 0,
        })
        .collect();

    RenderSnapshot {
        session: SessionSnapshot {
            id: SessionId::new(),
            name: "s".to_string(),
            active_tab: TabSnapshot {
                id: tab_id,
                name: "active".to_string(),
                layout_solved: slots,
                effective_size: effective,
                stack_headers: headers.to_vec(),
                layout_mode: LayoutMode::Tiled,
                all_suppressed: false,
            },
            tabs_metadata,
        },
        panes: Vec::new(),
        client: ClientSnapshot {
            id: ClientId::new(),
            viewport,
            active_tab: tab_id,
            focused_pane: None,
            lock_mode: LockMode::Normal,
            mouse_select: false,
        },
        plugin_ui: PluginUiSnapshot::default(),
    }
}

fn header(pane: PaneId, r: Rect) -> StackHeader {
    StackHeader {
        pane,
        rect: r,
        position: 0,
        total: 2,
    }
}

/// A pane filling the whole viewport: content, border sides, and the chrome rows
/// on top of it.
#[test]
fn full_pane_content_border_and_chrome() {
    let pane = PaneId::new();
    let s = snap(
        Size { cols: 40, rows: 10 },
        Size { cols: 40, rows: 10 },
        &[(pane, rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );

    // Inside the border → content.
    assert_eq!(
        hit_test(s.layout(chrome()), at(20, 5)),
        HitRegion::PaneContent { pane_id: pane }
    );
    // Left and right border columns.
    assert_eq!(
        hit_test(s.layout(chrome()), at(0, 5)),
        HitRegion::PaneBorder {
            pane_id: pane,
            side: Direction::Left
        }
    );
    assert_eq!(
        hit_test(s.layout(chrome()), at(39, 5)),
        HitRegion::PaneBorder {
            pane_id: pane,
            side: Direction::Right
        }
    );
    // Top row is the tabline (drawn over the pane), off any tab ribbon here.
    assert_eq!(hit_test(s.layout(chrome()), at(20, 0)), HitRegion::Tabline);
    // Bottom row is the hint bar.
    assert_eq!(
        hit_test(s.layout(chrome()), at(20, 9)),
        HitRegion::Statusline
    );
}

/// A layout smaller than the viewport centers, exposing the top and bottom
/// border rows, and the surrounding margin hits nothing.
#[test]
fn centered_layout_exposes_top_bottom_borders_and_letterbox() {
    let pane = PaneId::new();
    // content_rect centers 40x10 in 44x14 at origin (2, 2).
    let s = snap(
        Size { cols: 44, rows: 14 },
        Size { cols: 40, rows: 10 },
        &[(pane, rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );

    assert_eq!(
        hit_test(s.layout(chrome()), at(22, 7)),
        HitRegion::PaneContent { pane_id: pane }
    );
    // Top border row of the pane, now below the tabline.
    assert_eq!(
        hit_test(s.layout(chrome()), at(22, 2)),
        HitRegion::PaneBorder {
            pane_id: pane,
            side: Direction::Up
        }
    );
    // Bottom border row of the pane, above the hint bar.
    assert_eq!(
        hit_test(s.layout(chrome()), at(22, 11)),
        HitRegion::PaneBorder {
            pane_id: pane,
            side: Direction::Down
        }
    );
    // Left of the content rect → letterbox margin.
    assert_eq!(hit_test(s.layout(chrome()), at(0, 7)), HitRegion::None);
    // A non-chrome row above the content rect → letterbox margin.
    assert_eq!(hit_test(s.layout(chrome()), at(22, 1)), HitRegion::None);
}

#[test]
fn mouse_hit_testing_stays_on_the_painted_region_revision() {
    let pane = PaneId::new();
    let viewport = Size {
        cols: 120,
        rows: 40,
    };
    let effective = Size {
        cols: 100,
        rows: 38,
    };
    let snapshot = snap(
        viewport,
        effective,
        &[(pane, rect(0, 0, effective.cols, effective.rows), true)],
        &[],
        &[],
    );
    let default_regions = CommittedRegions::core(viewport, 4);
    let side_regions = CommittedRegions::new(
        viewport,
        solve(
            viewport,
            &[
                RegionGeometry {
                    edge: Edge::Top,
                    extent: 1,
                },
                RegionGeometry {
                    edge: Edge::Bottom,
                    extent: 1,
                },
                RegionGeometry {
                    edge: Edge::Left,
                    extent: 20,
                },
            ],
        ),
        5,
    );

    let painted = MouseFrame::with_regions(snapshot.clone(), default_regions.clone());
    assert_eq!(
        hit_test(painted.layout(chrome()), at(12, 2)),
        HitRegion::PaneContent { pane_id: pane }
    );
    assert_eq!(
        hit_test(painted.layout(chrome()), at(1, 0)),
        HitRegion::Tabline
    );
    assert_eq!(
        hit_test(painted.layout(chrome()), at(1, 39)),
        HitRegion::Statusline
    );
    assert_eq!(painted.committed_regions.input_revision, 4);

    let replacement = MouseFrame::with_regions(snapshot, side_regions);
    assert_eq!(
        hit_test(replacement.layout(chrome()), at(12, 2)),
        HitRegion::None
    );
    assert_eq!(painted.committed_regions.input_revision, 4);
}

/// A collapsed stack member's strip hit-tests to its pane.
#[test]
fn stack_header_hits_its_pane() {
    let member = PaneId::new();
    let s = snap(
        Size { cols: 40, rows: 10 },
        Size { cols: 40, rows: 10 },
        &[],
        &[header(member, rect(0, 3, 40, 1))],
        &[],
    );
    assert_eq!(
        hit_test(s.layout(chrome()), at(20, 3)),
        HitRegion::StackHeader { pane_id: member }
    );
}

/// Tabs map to their own ids by column; the session block and the gaps between
/// tabs are the bare tabline.
#[test]
fn tabs_hit_by_column() {
    use crate::render::tabline_layout;
    use ratatui::layout::Rect as RatatuiRect;

    let a = TabId::new();
    let b = TabId::new();
    // The session block and its version badge hold the left, then each 7-cell
    // tab ribbon with a one-cell gap between them. The columns come from the
    // same solve the paint uses, so the badge's width is never spelled out.
    // The row is sized from the badge, so a longer version string widens the
    // row instead of squeezing the tabs out of it.
    let cols = badge_cols() + 31;
    let s = snap(
        Size { cols, rows: 10 },
        Size { cols, rows: 10 },
        &[],
        &[],
        &[(a, "a"), (b, "b")],
    );
    let tabs = tabline_layout(
        s.layout(chrome()),
        RatatuiRect {
            x: 0,
            y: 0,
            width: cols,
            height: 1,
        },
    )
    .tabs;
    assert_eq!(tabs.len(), 2);

    assert_eq!(
        hit_test(s.layout(chrome()), at(tabs[0].1 + 1, 0)),
        HitRegion::Tab { tab_id: a }
    );
    assert_eq!(
        hit_test(s.layout(chrome()), at(tabs[1].1 + 1, 0)),
        HitRegion::Tab { tab_id: b }
    );
    // The one-cell gap between the two ribbons.
    assert_eq!(
        hit_test(s.layout(chrome()), at(tabs[1].1 - 1, 0)),
        HitRegion::Tabline
    );
    // The session block on the left.
    assert_eq!(hit_test(s.layout(chrome()), at(1, 0)), HitRegion::Tabline);
}

/// Scroll arrows hit-test to their scroll targets, and those targets step one
/// tab off the current first-visible index.
#[test]
fn scroll_arrows_hit_test_to_their_targets() {
    use crate::render::tabline_layout;
    use ratatui::layout::Rect as RatatuiRect;

    let ids: Vec<TabId> = (0..8).map(|_| TabId::new()).collect();
    let tabs: Vec<(TabId, &str)> = ids.iter().map(|&id| (id, "tab")).collect();
    // Sized from the version badge, so a longer version string widens the row
    // instead of starving the tab strip the arrows are measured against.
    let cols = badge_cols() + 21;
    let s = snap(
        Size { cols, rows: 8 },
        Size { cols, rows: 8 },
        &[],
        &[],
        &tabs,
    );
    // Peek from index 2, so tabs are hidden off both sides.
    let peeking = ViewerChrome {
        tabline_offset: Some(2),
        ..ViewerChrome::default()
    };

    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: cols,
        height: 8,
    };
    let layout = tabline_layout(s.layout(peeking), area);
    let (left_x, left_to) = layout.left_arrow.expect("tabs hidden off the left");
    let (right_x, right_to) = layout.right_arrow.expect("tabs hidden off the right");

    assert_eq!(left_to, 1, "left arrow steps one tab toward the start");
    assert_eq!(right_to, 3, "right arrow steps one tab toward the end");
    assert_eq!(
        hit_test(s.layout(peeking), at(left_x, 0)),
        HitRegion::TablineScrollLeft { to: 1 }
    );
    assert_eq!(
        hit_test(s.layout(peeking), at(right_x, 0)),
        HitRegion::TablineScrollRight { to: 3 }
    );
}

/// The too-small overlay and a zero-size viewport hit nothing.
#[test]
fn degenerate_frames_hit_nothing() {
    let pane = PaneId::new();
    let mut suppressed = snap(
        Size { cols: 40, rows: 10 },
        Size { cols: 40, rows: 10 },
        &[(pane, rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );
    suppressed.session.active_tab.all_suppressed = true;
    assert_eq!(
        hit_test(suppressed.layout(chrome()), at(20, 5)),
        HitRegion::None
    );

    let zero = snap(
        Size { cols: 0, rows: 0 },
        Size { cols: 0, rows: 0 },
        &[],
        &[],
        &[],
    );
    assert_eq!(hit_test(zero.layout(chrome()), at(0, 0)), HitRegion::None);
}

/// A border corner cell reads as the left or right edge, never the top or
/// bottom one.
#[test]
fn a_border_corner_reads_as_its_vertical_side() {
    let pane = PaneId::new();
    // Centered, so the pane's own top and bottom rows sit clear of the chrome
    // rows and all four corners are reachable: the box spans (2, 2)–(41, 11).
    let s = snap(
        Size { cols: 44, rows: 14 },
        Size { cols: 40, rows: 10 },
        &[(pane, rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );
    let corner = |x, y| hit_test(s.layout(chrome()), at(x, y));

    assert_eq!(
        corner(2, 2),
        HitRegion::PaneBorder {
            pane_id: pane,
            side: Direction::Left
        }
    );
    assert_eq!(
        corner(2, 11),
        HitRegion::PaneBorder {
            pane_id: pane,
            side: Direction::Left
        }
    );
    assert_eq!(
        corner(41, 2),
        HitRegion::PaneBorder {
            pane_id: pane,
            side: Direction::Right
        }
    );
    assert_eq!(
        corner(41, 11),
        HitRegion::PaneBorder {
            pane_id: pane,
            side: Direction::Right
        }
    );
}

/// A 40x10 layout centered in a 44x14 viewport, with one visible pane filling
/// it and one hidden pane beside it.
fn centered_snap(visible: PaneId, hidden: PaneId) -> RenderSnapshot {
    snap(
        Size { cols: 44, rows: 14 },
        Size { cols: 40, rows: 10 },
        &[
            (visible, rect(0, 0, 40, 10), true),
            (hidden, rect(0, 0, 6, 4), false),
        ],
        &[],
        &[],
    )
}

/// A pane's content rect is its border inset, shifted into the centered layout.
#[test]
fn a_pane_content_rect_is_its_border_inset_shifted_into_the_centered_layout() {
    let pane = PaneId::new();
    let hidden = PaneId::new();
    let s = centered_snap(pane, hidden);

    // The layout origin is (2, 2); the pane's content starts one cell further
    // in on both axes and loses one cell on each side.
    assert_eq!(
        pane_content_rect(s.layout(chrome()), pane),
        Some(rect(3, 3, 38, 8))
    );
    // A hidden pane and a pane that is not in this frame have no content rect.
    assert_eq!(pane_content_rect(s.layout(chrome()), hidden), None);
    assert_eq!(pane_content_rect(s.layout(chrome()), PaneId::new()), None);

    // The too-small overlay draws no pane at all.
    let mut suppressed = centered_snap(pane, hidden);
    suppressed.session.active_tab.all_suppressed = true;
    assert_eq!(pane_content_rect(suppressed.layout(chrome()), pane), None);

    // A zero-size viewport has nowhere to put it.
    let mut zero = centered_snap(pane, hidden);
    zero.client.viewport = Size { cols: 0, rows: 0 };
    assert_eq!(pane_content_rect(zero.layout(chrome()), pane), None);
}

/// A cell inside a pane's content names the program's own cell, counting from
/// `(1, 1)`; a cell outside that content names none.
#[test]
fn a_pane_local_cell_counts_from_one_and_refuses_a_cell_outside_the_pane() {
    let pane = PaneId::new();
    let s = centered_snap(pane, PaneId::new());
    let local = |x, y| pane_local_cell(s.layout(chrome()), pane, at(x, y));

    // The content rect spans columns 3–40 and rows 3–10.
    assert_eq!(local(3, 3), Some((1, 1)));
    assert_eq!(local(40, 10), Some((38, 8)));
    // One cell past each far edge, and one cell before each near edge.
    assert_eq!(local(41, 10), None);
    assert_eq!(local(40, 11), None);
    assert_eq!(local(2, 3), None);
    assert_eq!(local(3, 2), None);
}

/// A cell outside a pane's content is pulled to the nearest edge cell of it,
/// counted from `(0, 0)`.
#[test]
fn a_pane_cell_clamped_pulls_an_outside_cell_to_the_nearest_edge() {
    let pane = PaneId::new();
    let s = centered_snap(pane, PaneId::new());
    let clamped = |x, y| pane_cell_clamped(s.layout(chrome()), pane, at(x, y));

    // The content rect spans columns 3–40 and rows 3–10, so its own cells run
    // (0, 0) to (37, 7).
    assert_eq!(clamped(3, 3), Some((0, 0)));
    assert_eq!(clamped(40, 10), Some((37, 7)));
    // Past the far corner, and before the near corner: both pull inside.
    assert_eq!(clamped(200, 200), Some((37, 7)));
    assert_eq!(clamped(0, 0), Some((0, 0)));
    // Off one axis only: that axis clamps, the other keeps its cell.
    assert_eq!(clamped(1, 7), Some((0, 4)));
    assert_eq!(clamped(20, 13), Some((17, 7)));
    // A pane that is not drawn this frame names no cell at all.
    assert_eq!(
        pane_cell_clamped(s.layout(chrome()), PaneId::new(), at(3, 3)),
        None
    );
}

/// The first-visible index is the window the tabline actually draws: the peek
/// the viewer set, clamped to the last tab, or the active tab's own window.
#[test]
fn tabline_first_visible_reports_the_window_the_strip_draws() {
    let ids: Vec<TabId> = (0..8).map(|_| TabId::new()).collect();
    let tabs: Vec<(TabId, &str)> = ids.iter().map(|&id| (id, "tab")).collect();
    // The same row width the scroll-arrow test uses: eight tabs do not fit, so
    // the strip scrolls.
    let cols = badge_cols() + 21;
    let s = snap(
        Size { cols, rows: 8 },
        Size { cols, rows: 8 },
        &[],
        &[],
        &tabs,
    );
    let peek = |index| ViewerChrome {
        tabline_offset: Some(index),
        ..ViewerChrome::default()
    };

    assert_eq!(tabline_first_visible(s.layout(peek(2))), Some(2));
    // An index past the last tab clamps to it.
    assert_eq!(tabline_first_visible(s.layout(peek(99))), Some(7));
    // Following the active tab, which is the first one, starts at the start.
    assert_eq!(tabline_first_visible(s.layout(chrome())), Some(0));
}

/// A frame that draws no tabline has no first-visible index: every pane
/// suppressed for want of room, or a zero-size viewport.
#[test]
fn tabline_first_visible_is_none_when_no_tabline_is_drawn() {
    let tab = TabId::new();
    let mut suppressed = snap(
        Size { cols: 80, rows: 24 },
        Size { cols: 80, rows: 24 },
        &[],
        &[],
        &[(tab, "tab")],
    );
    suppressed.session.active_tab.all_suppressed = true;
    assert_eq!(tabline_first_visible(suppressed.layout(chrome())), None);

    let zero = snap(
        Size { cols: 0, rows: 0 },
        Size { cols: 0, rows: 0 },
        &[],
        &[],
        &[(tab, "tab")],
    );
    assert_eq!(tabline_first_visible(zero.layout(chrome())), None);
}

/// Two clients viewing the same layout at different sizes hit-test in their own
/// coordinate spaces.
#[test]
fn two_clients_hit_test_independently() {
    let pane = PaneId::new();
    let small = snap(
        Size { cols: 40, rows: 10 },
        Size { cols: 40, rows: 10 },
        &[(pane, rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );
    let large = snap(
        Size { cols: 44, rows: 14 },
        Size { cols: 40, rows: 10 },
        &[(pane, rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );

    // The small client fills the viewport: (22, 7) is content.
    assert_eq!(
        hit_test(small.layout(chrome()), at(22, 7)),
        HitRegion::PaneContent { pane_id: pane }
    );
    // The large client centers the layout: the same cell is content too, but a
    // cell in its margin — where the small client had content — hits nothing.
    assert_eq!(
        hit_test(large.layout(chrome()), at(22, 7)),
        HitRegion::PaneContent { pane_id: pane }
    );
    assert_eq!(hit_test(large.layout(chrome()), at(1, 7)), HitRegion::None);
}
