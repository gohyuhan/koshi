//! The crate's two entry points read one frame the same way: the region
//! [`hit_test`] reports for a cell is the region [`render_frame`] painted
//! there.
//!
//! Each test paints one frame into a buffer and classifies the same frame cell
//! by cell, then checks the two against each other: a chrome row is on the row
//! its committed region owns, a pane's content cells sit inside the rect
//! [`pane_content_rect`] gives for that pane, a border cell carries a box
//! glyph, a stack header carries the header background, and every unclassified
//! cell carries the letterbox backdrop.

use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as RatatuiRect;

use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_layout::mode::LayoutMode;
use koshi_layout::regions::RegionSolve;
use koshi_layout::solver::StackHeader;
use koshi_renderer::snapshot::{
    ClientSnapshot, CommittedRegions, CursorSnapshot, GridView, KeymapHints, MouseFrame, PaneKind,
    PaneSlot, PaneSnapshot, PluginUiSnapshot, RenderSnapshot, ScrollbackMeta, SessionSnapshot,
    TabMeta, TabSnapshot, ViewerChrome,
};
use koshi_renderer::theme::Theme;
use koshi_renderer::{cursor_position, hit_test, pane_content_rect, render_frame, HitRegion};
use koshi_terminal::grid::state::{Cell, Grid};
use koshi_terminal::style::Style as CellStyle;

/// The client viewport every frame here is painted and hit-tested in.
const VIEWPORT: Size = Size { cols: 60, rows: 14 };

/// The size the tiled layout is solved for. Smaller than [`VIEWPORT`] on both
/// axes, so the frame carries a letterbox margin on all four sides.
const EFFECTIVE: Size = Size { cols: 50, rows: 10 };

/// The names of the three tabs every frame here carries, the first active.
const TAB_NAMES: [&str; 3] = ["one", "two", "three"];

/// The glyphs `Block` draws a full border ring with.
const BORDER_GLYPHS: [&str; 6] = ["┌", "┐", "└", "┘", "─", "│"];

fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Rect {
    Rect {
        origin: Point { x, y },
        size: Size { cols, rows },
    }
}

/// A visible pane slot whose content area is `outer` inset by its one-cell
/// border.
fn slot(pane_id: PaneId, outer: Rect) -> PaneSlot {
    PaneSlot {
        pane_id,
        rect: outer,
        inner_rect: Some(outer.inner_with_border()),
        kind: PaneKind::Terminal,
        visible: true,
        suppressed: false,
        dead: false,
    }
}

/// A pane whose every cell holds `fill`, sized `cols x rows`.
fn filled_grid(cols: u16, rows: u16, fill: char) -> GridView {
    let cells = vec![vec![Cell::new(fill, 1, CellStyle::default()); cols as usize]; rows as usize];
    GridView {
        grid: Arc::new(Grid::from_rows(cells, cols, CellStyle::default())),
        view_offset: 0,
    }
}

/// One pane's content with the cursor at `(row, col)` of its content area.
fn pane(pane_id: PaneId, grid_view: Option<GridView>, row: u16, col: u16) -> PaneSnapshot {
    PaneSnapshot {
        id: pane_id,
        title: None,
        cursor: CursorSnapshot {
            row,
            col,
            visible: true,
            blink: false,
            shape: None,
        },
        grid_view,
        reverse_video: false,
        mouse_tracking: MouseTracking::Off,
        alt_scroll: false,
        on_alt_screen: false,
        view_top_row: 0,
        selection: None,
        has_selection: false,
        scrollback: ScrollbackMeta {
            truncated: false,
            retained_lines: 0,
        },
    }
}

/// A frame with `slots` laid out for `effective`, `headers` on top of them, and
/// three tabs.
fn snapshot(
    effective: Size,
    slots: Vec<PaneSlot>,
    panes: Vec<PaneSnapshot>,
    headers: Vec<StackHeader>,
    focused: Option<PaneId>,
) -> RenderSnapshot {
    let tab_id = TabId::new();
    let tabs_metadata = TAB_NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| TabMeta {
            id: TabId::new(),
            name: (*name).to_string(),
            index,
            active: index == 0,
        })
        .collect();
    RenderSnapshot {
        session: SessionSnapshot {
            id: SessionId::new(),
            name: "work".to_string(),
            active_tab: TabSnapshot {
                id: tab_id,
                name: "one".to_string(),
                layout_solved: slots,
                effective_size: effective,
                stack_headers: headers,
                layout_mode: LayoutMode::Tiled,
                all_suppressed: false,
                gap: 0,
            },
            tabs_metadata,
        },
        panes,
        client: ClientSnapshot {
            id: ClientId::new(),
            viewport: VIEWPORT,
            active_tab: tab_id,
            focused_pane: focused,
            lock_mode: LockMode::Normal,
            mouse_select: false,
        },
        plugin_ui: PluginUiSnapshot::default(),
    }
}

/// Two panes tiling [`EFFECTIVE`] side by side, a one-row stack header covering
/// the right pane's first content row, and the left pane filled with `a`.
///
/// Returns the frame, the left pane, the right pane, and the header's pane.
fn tiled_frame() -> (RenderSnapshot, PaneId, PaneId, PaneId) {
    let left = PaneId::new();
    let right = PaneId::new();
    let collapsed = PaneId::new();
    let left_outer = rect(0, 0, 25, 10);
    let right_outer = rect(25, 0, 25, 10);
    let header = StackHeader {
        pane: collapsed,
        rect: rect(26, 1, 23, 1),
        position: 0,
        total: 2,
    };
    let frame = snapshot(
        EFFECTIVE,
        vec![slot(left, left_outer), slot(right, right_outer)],
        vec![
            pane(left, Some(filled_grid(23, 8, 'a')), 3, 7),
            pane(right, None, 0, 0),
        ],
        vec![header],
        Some(left),
    );
    (frame, left, right, collapsed)
}

/// The whole viewport as a ratatui area at the origin.
fn area() -> RatatuiRect {
    RatatuiRect::new(0, 0, VIEWPORT.cols, VIEWPORT.rows)
}

/// Paint `frame` with `regions` into a fresh viewport-sized buffer.
fn paint(frame: &RenderSnapshot, regions: &CommittedRegions) -> Buffer {
    let area = area();
    let mut buf = Buffer::empty(area);
    render_frame(
        frame,
        regions,
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ViewerChrome::default(),
        area,
        &mut buf,
    );
    buf
}

/// The text painted across the half-open column span `[from, to)` of `row`.
fn row_text(buf: &Buffer, row: u16, from: u16, to: u16) -> String {
    (from..to).map(|x| buf[(x, row)].symbol()).collect()
}

#[test]
fn every_cell_classifies_as_what_was_painted() {
    let (frame, left, right, collapsed) = tiled_frame();
    let regions = CommittedRegions::core(VIEWPORT, 0);
    let buf = paint(&frame, &regions);
    let theme = Theme::default();
    let mouse = MouseFrame::with_regions(frame, regions);
    let layout = mouse.layout(ViewerChrome::default());

    let statusline_row = VIEWPORT.rows - 1;
    let mut seen_content = 0_u32;
    let mut seen_border = 0_u32;
    let mut seen_header = 0_u32;
    let mut seen_letterbox = 0_u32;

    for y in 0..VIEWPORT.rows {
        for x in 0..VIEWPORT.cols {
            let at = Point { x, y };
            let cell = &buf[(x, y)];
            match hit_test(layout, at) {
                HitRegion::Tabline
                | HitRegion::Tab { .. }
                | HitRegion::TablineScrollLeft { .. }
                | HitRegion::TablineScrollRight { .. } => {
                    assert_eq!(y, 0, "tabline classified off the tabline row at ({x}, {y})");
                }
                HitRegion::Statusline => {
                    assert_eq!(
                        y, statusline_row,
                        "statusline classified off its row at ({x}, {y})"
                    );
                    assert_eq!(
                        cell.bg, theme.bar_bg,
                        "statusline cell ({x}, {y}) is not the bar background"
                    );
                }
                HitRegion::PaneContent { pane_id } => {
                    let content = pane_content_rect(layout, pane_id)
                        .unwrap_or_else(|| panic!("pane {pane_id:?} has no content rect"));
                    assert!(
                        content.contains(at),
                        "content hit at ({x}, {y}) is outside {content:?}"
                    );
                    if pane_id == left {
                        assert_eq!(
                            cell.symbol(),
                            "a",
                            "left pane content cell ({x}, {y}) was not painted"
                        );
                    } else {
                        assert_eq!(pane_id, right, "unexpected pane hit at ({x}, {y})");
                    }
                    seen_content += 1;
                }
                HitRegion::PaneBorder { pane_id, .. } => {
                    let content = pane_content_rect(layout, pane_id)
                        .unwrap_or_else(|| panic!("pane {pane_id:?} has no content rect"));
                    assert!(
                        !content.contains(at),
                        "border hit at ({x}, {y}) is inside {content:?}"
                    );
                    assert!(
                        BORDER_GLYPHS.contains(&cell.symbol()),
                        "border hit at ({x}, {y}) painted {:?}",
                        cell.symbol()
                    );
                    seen_border += 1;
                }
                HitRegion::StackHeader { pane_id } => {
                    assert_eq!(pane_id, collapsed);
                    assert_eq!(
                        cell.bg, theme.stack_header_bg,
                        "stack header cell ({x}, {y}) is not the header background"
                    );
                    seen_header += 1;
                }
                HitRegion::None => {
                    assert_eq!(
                        cell.bg, theme.letterbox,
                        "unclassified cell ({x}, {y}) is not letterbox margin"
                    );
                    seen_letterbox += 1;
                }
            }
        }
    }

    // Both panes' content minus the row the stack header takes.
    assert_eq!(seen_content, 2 * 23 * 8 - 23);
    assert_eq!(seen_border, 2 * (25 * 10 - 23 * 8));
    assert_eq!(seen_header, 23);
    // The 60x14 viewport less the two chrome rows and the centered 50x10 layout.
    assert_eq!(seen_letterbox, 60 * 12 - 50 * 10);
}

#[test]
fn a_tab_ribbon_spells_the_tab_it_hits() {
    let (frame, ..) = tiled_frame();
    let regions = CommittedRegions::core(VIEWPORT, 0);
    let buf = paint(&frame, &regions);
    let tabs = frame.session.tabs_metadata.clone();
    let mouse = MouseFrame::with_regions(frame, regions);
    let layout = mouse.layout(ViewerChrome::default());

    for meta in &tabs {
        let columns: Vec<u16> = (0..VIEWPORT.cols)
            .filter(|&x| hit_test(layout, Point { x, y: 0 }) == HitRegion::Tab { tab_id: meta.id })
            .collect();
        let first = *columns.first().expect("every tab fits this row");
        let last = *columns.last().expect("every tab fits this row");
        assert_eq!(
            columns.len() as u16,
            last - first + 1,
            "tab {} hits a broken column run",
            meta.name
        );
        assert_eq!(
            row_text(&buf, 0, first, last + 1),
            format!(" #{}  {} ", meta.index + 1, meta.name),
            "tab {} hits columns that spell something else",
            meta.name
        );
    }
}

#[test]
fn the_cursor_lands_in_the_focused_pane_content() {
    let (frame, left, ..) = tiled_frame();
    let regions = CommittedRegions::core(VIEWPORT, 0);
    let position =
        cursor_position(&frame, &regions, area()).expect("the focused pane has a cursor");
    let mouse = MouseFrame::with_regions(frame, regions);
    let layout = mouse.layout(ViewerChrome::default());

    let content = pane_content_rect(layout, left).expect("the focused pane is drawn");
    let at = Point {
        x: position.x,
        y: position.y,
    };
    assert!(content.contains(at), "cursor {at:?} is outside {content:?}");
    assert_eq!(at.x, content.origin.x + 7);
    assert_eq!(at.y, content.origin.y + 3);
    assert_eq!(
        hit_test(layout, at),
        HitRegion::PaneContent { pane_id: left },
        "the cursor cell does not hit the pane it belongs to"
    );
}

#[test]
fn a_zero_height_chrome_region_paints_nothing() {
    let only = PaneId::new();
    let frame = snapshot(
        VIEWPORT,
        vec![slot(only, rect(0, 0, VIEWPORT.cols, VIEWPORT.rows))],
        vec![pane(only, None, 0, 0)],
        Vec::new(),
        None,
    );
    let flat = Rect::new(
        Point { x: 0, y: 0 },
        Size {
            cols: VIEWPORT.cols,
            rows: 0,
        },
    );
    let regions = CommittedRegions::new(
        VIEWPORT,
        RegionSolve {
            regions: vec![flat, flat],
            pane_rect: Rect::at_origin(VIEWPORT),
        },
        0,
    );
    let buf = paint(&frame, &regions);
    let mouse = MouseFrame::with_regions(frame, regions);
    let layout = mouse.layout(ViewerChrome::default());

    let top = row_text(&buf, 0, 0, VIEWPORT.cols);
    assert!(
        !top.contains("work") && !top.contains("BASE"),
        "a zero-height tabline region painted the tab bar: {top:?}"
    );
    assert_eq!(
        top,
        format!("┌{}┐", "─".repeat(usize::from(VIEWPORT.cols) - 2))
    );
    assert_eq!(
        hit_test(layout, Point { x: 0, y: 0 }),
        HitRegion::PaneBorder {
            pane_id: only,
            side: koshi_core::geometry::Direction::Left,
        }
    );
}
