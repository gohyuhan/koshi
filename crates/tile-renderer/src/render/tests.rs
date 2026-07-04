//! Tests for stock frame composition: the three zones render into a ratatui
//! buffer, tabs show their marker, the mode tag tracks the client lock mode,
//! pane borders draw with focus highlighting, terminal cells paint into pane
//! content rects with their styles and wide-glyph handling, collapsed stack
//! members render as inverted title strips, the focused pane's cursor cell is
//! reported (clamped inside its content area, and hidden for unfocused, plugin,
//! hidden, or app-hidden cursors), and degenerate sizes are safe.

use super::*;

use std::sync::Arc;

use tile_core::geometry::{Point, Size};
use tile_core::ids::{ClientId, PaneId, SessionId, TabId};
use tile_terminal::grid::state::{Cell, Grid};
use tile_terminal::style::{Color as TermColor, Style as TermStyle};

use crate::snapshot::{
    ClientSnapshot, CursorSnapshot, GridView, PaneSlot, PaneSnapshot, PluginUiSnapshot,
    ScrollbackMeta, SessionSnapshot, TabMeta, TabSnapshot,
};
use tile_layout::mode::LayoutMode;
use tile_layout::solver::StackHeader;
use tile_pane::pane::state::PaneKind;

/// A cell rect: origin `(x, y)`, size `cols x rows`.
fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Rect {
    Rect {
        origin: Point { x, y },
        size: Size { cols, rows },
    }
}

/// Build a snapshot from explicit pieces. `panes` are `(id, outer rect, visible)`;
/// a visible pane's content rect is the outer rect inset by its one-cell border.
fn build(
    session: &str,
    tabs: &[(&str, bool)],
    panes: &[(PaneId, Rect, bool)],
    focused: Option<PaneId>,
    lock_mode: LockMode,
    viewport: Size,
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

    let pane_snapshots = panes
        .iter()
        .map(|(id, _, _)| PaneSnapshot {
            id: *id,
            title: None,
            cursor: CursorSnapshot {
                row: 0,
                col: 0,
                visible: true,
                blink: false,
            },
            grid_view: None,
            reverse_video: false,
            scrollback: ScrollbackMeta {
                truncated: false,
                retained_lines: 0,
            },
        })
        .collect();

    let tabs_metadata = tabs
        .iter()
        .enumerate()
        .map(|(index, (name, active))| TabMeta {
            id: TabId::new(),
            name: (*name).to_string(),
            index,
            active: *active,
        })
        .collect();

    RenderSnapshot {
        session: SessionSnapshot {
            id: SessionId::new(),
            name: session.to_string(),
            active_tab: TabSnapshot {
                id: tab_id,
                name: "active".to_string(),
                layout_solved: slots,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                all_suppressed: false,
            },
            tabs_metadata,
        },
        panes: pane_snapshots,
        client: ClientSnapshot {
            id: ClientId::new(),
            viewport,
            active_tab: tab_id,
            focused_pane: focused,
            lock_mode,
        },
        plugin_ui: PluginUiSnapshot::default(),
    }
}

/// Render a snapshot into a fresh `w x h` buffer.
fn render(snapshot: &RenderSnapshot, w: u16, h: u16) -> Buffer {
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: w,
        height: h,
    };
    let mut buf = Buffer::empty(area);
    render_frame(snapshot, area, &mut buf);
    buf
}

/// The visible text of buffer row `y`.
fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area().width)
        .map(|x| buf[(x, y)].symbol().to_string())
        .collect()
}

#[test]
fn renders_tabline_pane_border_and_reserved_hint_bar() {
    let pane = PaneId::new();
    let snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    let buf = render(&snap, 40, 8);

    // Tabline (row 0): session + tab on the left, mode tag right-aligned.
    let tabline = row_text(&buf, 0);
    assert!(tabline.starts_with("sess"), "tabline: {tabline:?}");
    assert!(tabline.contains("1:shell"), "tabline: {tabline:?}");
    assert!(
        tabline.trim_end().ends_with("[BASE]"),
        "tabline: {tabline:?}"
    );

    // Pane border box on rows 1..=6, columns 0..=39.
    assert_eq!(buf[(0, 1)].symbol(), "┌");
    assert_eq!(buf[(39, 1)].symbol(), "┐");
    assert_eq!(buf[(0, 6)].symbol(), "└");
    assert_eq!(buf[(39, 6)].symbol(), "┘");
    assert_eq!(buf[(1, 1)].symbol(), "─");
    assert_eq!(buf[(0, 2)].symbol(), "│");

    // Bottom row (row 7): the keybind-hint bar is reserved and blank for now.
    assert!(
        row_text(&buf, 7).trim().is_empty(),
        "hint bar row: {:?}",
        row_text(&buf, 7)
    );
}

#[test]
fn tabline_lists_tabs_with_active_marker() {
    let pane = PaneId::new();
    let snap = build(
        "sess",
        &[("code", true), ("logs", false)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    let buf = render(&snap, 40, 8);

    // "sess │ " is 7 columns, then "1:code" then " " then "2:logs".
    assert!(row_text(&buf, 0).contains("1:code 2:logs"));
    // The active tab is drawn inverted; the inactive one is not.
    assert!(buf[(7, 0)].modifier.contains(Modifier::REVERSED));
    assert!(!buf[(14, 0)].modifier.contains(Modifier::REVERSED));
}

#[test]
fn mode_tag_reflects_lock_mode() {
    let pane = PaneId::new();
    let make = |mode| {
        build(
            "sess",
            &[("shell", true)],
            &[(pane, rect(0, 1, 40, 6), true)],
            Some(pane),
            mode,
            Size { cols: 40, rows: 8 },
        )
    };

    let base = render(&make(LockMode::Normal), 40, 8);
    assert!(row_text(&base, 0).contains("[BASE]"));

    let locked = render(&make(LockMode::Locked), 40, 8);
    assert!(row_text(&locked, 0).contains("[LOCK]"));
    assert!(!row_text(&locked, 0).contains("[BASE]"));
}

#[test]
fn focused_pane_border_is_highlighted() {
    let left = PaneId::new();
    let right = PaneId::new();
    let snap = build(
        "sess",
        &[("shell", true)],
        &[
            (left, rect(0, 1, 20, 6), true),
            (right, rect(20, 1, 20, 6), true),
        ],
        Some(left),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    let buf = render(&snap, 40, 8);

    // Focused pane: cyan, bold border corner.
    assert_eq!(buf[(0, 1)].fg, Color::Cyan);
    assert!(buf[(0, 1)].modifier.contains(Modifier::BOLD));
    // Unfocused pane: dim border corner, no bold.
    assert_eq!(buf[(20, 1)].fg, Color::DarkGray);
    assert!(!buf[(20, 1)].modifier.contains(Modifier::BOLD));
}

#[test]
fn hidden_pane_draws_no_border() {
    let pane = PaneId::new();
    let snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), false)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    let buf = render(&snap, 40, 8);

    // No border cell where the box would have been.
    assert_eq!(buf[(0, 1)].symbol(), " ");
    assert_eq!(buf[(1, 1)].symbol(), " ");
}

#[test]
fn scroll_indicator_shown_only_when_scrolled_back() {
    let pane = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );

    // At the live tail (no grid view / offset 0): no scroll indicator.
    assert!(!row_text(&render(&snap, 40, 8), 0).contains("SCROLL"));

    // Scrolled back three lines with 100 retained: indicator appears.
    snap.panes[0].grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(6, 40, TermStyle::default())),
        view_offset: 3,
    });
    snap.panes[0].scrollback.retained_lines = 100;
    assert!(row_text(&render(&snap, 40, 8), 0).contains("SCROLL 3/100"));
}

#[test]
fn reused_buffer_is_blanked_before_painting() {
    let pane = PaneId::new();
    let snap = build(
        "s",
        &[("t", true)],
        &[(pane, rect(0, 1, 20, 4), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 24, rows: 6 },
    );

    // A buffer reused across frames holds the previous frame's cells; simulate
    // that with a full grid of stale glyphs before rendering.
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: 24,
        height: 6,
    };
    let mut buf = Buffer::empty(area);
    for y in 0..area.height {
        for x in 0..area.width {
            buf[(x, y)].set_symbol("X");
        }
    }

    render_frame(&snap, area, &mut buf);

    // Tabline gap between the left tab list and the right status: blanked.
    assert_eq!(buf[(12, 0)].symbol(), " ");
    // A cell outside every pane box: blanked, not the stale glyph.
    assert_eq!(buf[(22, 2)].symbol(), " ");
    // Reserved hint row (bottom): fully blank.
    assert!(row_text(&buf, 5).chars().all(|c| c == ' '));
}

#[test]
fn stack_headers_render_collapsed_strips() {
    let active = PaneId::new();
    let b = PaneId::new();
    let c = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[
            (active, rect(0, 3, 30, 4), true),
            (b, rect(0, 1, 30, 1), false),
            (c, rect(0, 2, 30, 1), false),
        ],
        Some(active),
        LockMode::Normal,
        Size { cols: 30, rows: 8 },
    );
    snap.panes[1].title = Some("editor".to_string());
    snap.panes[2].title = Some("logs".to_string());
    snap.session.active_tab.stack_headers = vec![
        StackHeader {
            pane: b,
            rect: rect(0, 1, 30, 1),
            position: 1,
            total: 3,
        },
        StackHeader {
            pane: c,
            rect: rect(0, 2, 30, 1),
            position: 2,
            total: 3,
        },
    ];
    let buf = render(&snap, 30, 8);

    // Row 1: B's strip — arrow + title on the left, [2/3] right-aligned.
    let strip_b = row_text(&buf, 1);
    assert!(strip_b.starts_with("▸ editor"), "strip: {strip_b:?}");
    assert!(strip_b.trim_end().ends_with("[2/3]"), "strip: {strip_b:?}");
    // Row 2: C's strip.
    let strip_c = row_text(&buf, 2);
    assert!(strip_c.starts_with("▸ logs"), "strip: {strip_c:?}");
    assert!(strip_c.trim_end().ends_with("[3/3]"), "strip: {strip_c:?}");

    // The whole strip row is inverted (the tile-owned marker), gap included.
    for x in 0..30 {
        assert!(
            buf[(x, 1)].modifier.contains(Modifier::REVERSED),
            "col {x} of strip not inverted"
        );
    }
}

#[test]
fn five_child_stack_shows_n_minus_one_headers() {
    let active = PaneId::new();
    let m1 = PaneId::new();
    let m2 = PaneId::new();
    let m3 = PaneId::new();
    let m4 = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[
            (active, rect(0, 5, 30, 3), true),
            (m1, rect(0, 1, 30, 1), false),
            (m2, rect(0, 2, 30, 1), false),
            (m3, rect(0, 3, 30, 1), false),
            (m4, rect(0, 4, 30, 1), false),
        ],
        Some(active),
        LockMode::Normal,
        Size { cols: 30, rows: 10 },
    );
    let members = [m1, m2, m3, m4];
    snap.session.active_tab.stack_headers = members
        .iter()
        .enumerate()
        .map(|(i, &pane)| StackHeader {
            pane,
            rect: rect(0, (i + 1) as u16, 30, 1),
            position: i + 1,
            total: 5,
        })
        .collect();
    let buf = render(&snap, 30, 10);

    // Four collapsed strips (rows 1..=4), each labelled [k/5]; the active member
    // keeps its content area below.
    for (i, k) in (2..=5).enumerate() {
        let row = row_text(&buf, (i + 1) as u16);
        assert!(
            row.trim_end().ends_with(&format!("[{k}/5]")),
            "row {}: {row:?}",
            i + 1
        );
    }
}

#[test]
fn stack_header_without_title_still_shows_arrow_and_indicator() {
    let active = PaneId::new();
    let member = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[
            (active, rect(0, 2, 30, 4), true),
            (member, rect(0, 1, 30, 1), false),
        ],
        Some(active),
        LockMode::Normal,
        Size { cols: 30, rows: 8 },
    );
    // The collapsed member carries no title (None from `build`).
    snap.session.active_tab.stack_headers = vec![StackHeader {
        pane: member,
        rect: rect(0, 1, 30, 1),
        position: 0,
        total: 2,
    }];
    let buf = render(&snap, 30, 8);

    let row = row_text(&buf, 1);
    assert!(row.starts_with("▸ "), "row: {row:?}");
    assert!(row.trim_end().ends_with("[1/2]"), "row: {row:?}");
}

#[test]
fn narrow_stack_header_indicator_does_not_bleed_left() {
    let active = PaneId::new();
    let member = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[
            (active, rect(0, 2, 20, 4), true),
            (member, rect(10, 1, 3, 1), false),
        ],
        Some(active),
        LockMode::Normal,
        Size { cols: 20, rows: 8 },
    );
    // A 3-wide strip at x=10 with a 7-wide indicator "[10/10]".
    snap.session.active_tab.stack_headers = vec![StackHeader {
        pane: member,
        rect: rect(10, 1, 3, 1),
        position: 9,
        total: 10,
    }];
    let buf = render(&snap, 20, 8);

    // The indicator clips inside the strip: nothing is written left of x=10.
    for x in 0..10 {
        assert_eq!(buf[(x, 1)].symbol(), " ", "col {x} written outside strip");
        assert!(
            !buf[(x, 1)].modifier.contains(Modifier::REVERSED),
            "col {x} inverted outside strip"
        );
    }
    // The strip's own cells (x=10..13) are inverted.
    for x in 10..13 {
        assert!(buf[(x, 1)].modifier.contains(Modifier::REVERSED));
    }
}

/// A one-pane snapshot whose single visible pane shows `grid`.
fn content_snap(grid: Grid, outer: Rect, reverse_video: bool, viewport: Size) -> RenderSnapshot {
    let pane = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, outer, true)],
        Some(pane),
        LockMode::Normal,
        viewport,
    );
    snap.panes[0].grid_view = Some(GridView {
        grid: Arc::new(grid),
        view_offset: 0,
    });
    snap.panes[0].reverse_video = reverse_video;
    snap
}

#[test]
fn pane_cells_render_with_glyphs_and_styles() {
    let mut grid = Grid::blank(4, 38, TermStyle::default());
    let mut style = TermStyle::default();
    style.set_fg(TermColor::Rgb(10, 20, 30));
    style.set_bg(TermColor::Indexed(4));
    style.set_bold(true);
    style.set_italic(true);
    *grid.cell_mut(0, 0).unwrap() = Cell::new('A', 1, style);
    let snap = content_snap(grid, rect(0, 1, 40, 6), false, Size { cols: 40, rows: 8 });
    let buf = render(&snap, 40, 8);

    // Styled glyph at the content origin (inside the one-cell border).
    assert_eq!(buf[(1, 2)].symbol(), "A");
    assert_eq!(buf[(1, 2)].fg, Color::Rgb(10, 20, 30));
    assert_eq!(buf[(1, 2)].bg, Color::Indexed(4));
    assert!(buf[(1, 2)]
        .modifier
        .contains(Modifier::BOLD | Modifier::ITALIC));

    // A default blank grid cell: a space in the terminal-default (reset) colors.
    assert_eq!(buf[(2, 2)].symbol(), " ");
    assert_eq!(buf[(2, 2)].fg, Color::Reset);
    assert_eq!(buf[(2, 2)].bg, Color::Reset);
}

#[test]
fn wide_glyph_spans_two_columns_without_splitting() {
    let mut grid = Grid::blank(4, 38, TermStyle::default());
    *grid.cell_mut(0, 0).unwrap() = Cell::new('中', 2, TermStyle::default());
    // The continuation half of the wide glyph (width 0).
    *grid.cell_mut(0, 1).unwrap() = Cell::new(' ', 0, TermStyle::default());
    *grid.cell_mut(0, 2).unwrap() = Cell::new('x', 1, TermStyle::default());
    let snap = content_snap(grid, rect(0, 1, 40, 6), false, Size { cols: 40, rows: 8 });
    let buf = render(&snap, 40, 8);

    // The wide glyph sits whole in its base column; its continuation column is
    // left blank, and the next real cell keeps its own grid column (no drift).
    assert_eq!(buf[(1, 2)].symbol(), "中");
    assert_eq!(buf[(2, 2)].symbol(), " ");
    assert_eq!(buf[(3, 2)].symbol(), "x");
}

#[test]
fn wide_glyph_at_right_edge_is_padded() {
    // The content rect is 5 wide (outer 7 minus borders); a wide glyph in the
    // last column has no room for its second half.
    let mut grid = Grid::blank(1, 5, TermStyle::default());
    *grid.cell_mut(0, 4).unwrap() = Cell::new('中', 2, TermStyle::default());
    let snap = content_snap(grid, rect(0, 1, 7, 3), false, Size { cols: 7, rows: 4 });
    let buf = render(&snap, 7, 4);

    // Padded to a blank; a half-glyph never bleeds onto the right border.
    assert_eq!(buf[(5, 2)].symbol(), " ");
    assert_eq!(buf[(6, 2)].symbol(), "│");
}

#[test]
fn combining_marks_join_the_base_into_one_symbol() {
    let mut grid = Grid::blank(4, 38, TermStyle::default());
    let mut cell = Cell::new('e', 1, TermStyle::default());
    cell.push_combining('\u{0301}'); // combining acute accent
    *grid.cell_mut(0, 0).unwrap() = cell;
    let snap = content_snap(grid, rect(0, 1, 40, 6), false, Size { cols: 40, rows: 8 });
    let buf = render(&snap, 40, 8);

    assert_eq!(buf[(1, 2)].symbol(), "e\u{0301}");
}

#[test]
fn reverse_video_toggles_reverse_per_cell() {
    let mut grid = Grid::blank(4, 38, TermStyle::default());
    *grid.cell_mut(0, 0).unwrap() = Cell::new('a', 1, TermStyle::default());
    let mut reversed = TermStyle::default();
    reversed.set_reverse(true);
    *grid.cell_mut(0, 1).unwrap() = Cell::new('b', 1, reversed);
    let snap = content_snap(grid, rect(0, 1, 40, 6), true, Size { cols: 40, rows: 8 });
    let buf = render(&snap, 40, 8);

    // Screen reverse (DECSCNM) reverses a plain cell...
    assert!(buf[(1, 2)].modifier.contains(Modifier::REVERSED));
    // ...and cancels a cell that is already reversed (reverse XOR reverse).
    assert!(!buf[(2, 2)].modifier.contains(Modifier::REVERSED));
}

#[test]
fn visible_pane_without_grid_draws_no_content() {
    let pane = PaneId::new();
    let snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    // `grid_view` is None (a plugin pane or an empty slot): interior stays blank.
    let buf = render(&snap, 40, 8);
    assert_eq!(buf[(1, 2)].symbol(), " ");
    assert_eq!(buf[(1, 2)].fg, Color::Reset);
}

#[test]
fn grid_larger_than_content_rect_clips_without_bleeding() {
    // A grid wider and taller than the content rect: only the cells that fit are
    // drawn and nothing writes onto the border or past the pane.
    let mut grid = Grid::blank(20, 100, TermStyle::default());
    for col in 0..100u16 {
        *grid.cell_mut(0, col).unwrap() = Cell::new('#', 1, TermStyle::default());
    }
    let snap = content_snap(grid, rect(0, 1, 40, 6), false, Size { cols: 40, rows: 8 });
    let buf = render(&snap, 40, 8);

    // Content fills the content columns (1..=38 of the first content row)...
    assert_eq!(buf[(1, 2)].symbol(), "#");
    assert_eq!(buf[(38, 2)].symbol(), "#");
    // ...and the right border (col 39) is untouched.
    assert_eq!(buf[(39, 2)].symbol(), "│");
}

#[test]
fn grid_smaller_than_content_rect_leaves_remainder_blank() {
    let mut grid = Grid::blank(1, 2, TermStyle::default());
    *grid.cell_mut(0, 0).unwrap() = Cell::new('h', 1, TermStyle::default());
    *grid.cell_mut(0, 1).unwrap() = Cell::new('i', 1, TermStyle::default());
    let snap = content_snap(grid, rect(0, 1, 40, 6), false, Size { cols: 40, rows: 8 });
    let buf = render(&snap, 40, 8);

    assert_eq!(buf[(1, 2)].symbol(), "h");
    assert_eq!(buf[(2, 2)].symbol(), "i");
    // Beyond the two-cell grid the content rect stays blank.
    assert_eq!(buf[(3, 2)].symbol(), " ");
    assert_eq!(buf[(1, 3)].symbol(), " ");
}

#[test]
fn cursor_at_focused_pane_maps_to_content_cell() {
    // Pane box (0,1) 40x6 → content origin (1,2). Cursor at row 2, col 5 within
    // the content area → absolute buffer cell (1+5, 2+2).
    let mut snap = content_snap(
        Grid::blank(4, 38, TermStyle::default()),
        rect(0, 1, 40, 6),
        false,
        Size { cols: 40, rows: 8 },
    );
    snap.panes[0].cursor = CursorSnapshot {
        row: 2,
        col: 5,
        visible: true,
        blink: false,
    };
    assert_eq!(cursor_position(&snap), Some(Position::new(6, 4)));
}

#[test]
fn cursor_past_content_rect_is_clamped_inside_it() {
    // A frozen cursor (e.g. a dead pane whose content rect later shrank) beyond
    // the content area: the returned cell is clamped to the last cell inside the
    // rect, never onto the border or a neighbour. Content rect origin (1,2),
    // 38x4 → last cell (38, 5).
    let mut snap = content_snap(
        Grid::blank(4, 38, TermStyle::default()),
        rect(0, 1, 40, 6),
        false,
        Size { cols: 40, rows: 8 },
    );
    snap.panes[0].cursor = CursorSnapshot {
        row: 99,
        col: 99,
        visible: true,
        blink: false,
    };
    assert_eq!(cursor_position(&snap), Some(Position::new(38, 5)));
}

#[test]
fn hidden_cursor_places_nothing() {
    let mut snap = content_snap(
        Grid::blank(4, 38, TermStyle::default()),
        rect(0, 1, 40, 6),
        false,
        Size { cols: 40, rows: 8 },
    );
    snap.panes[0].cursor.visible = false;
    assert_eq!(cursor_position(&snap), None);
}

#[test]
fn no_focused_pane_places_no_cursor() {
    let pane = PaneId::new();
    let snap = build(
        "s",
        &[("t", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        None,
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    assert_eq!(cursor_position(&snap), None);
}

#[test]
fn plugin_pane_places_no_cursor() {
    // A visible focused pane with a visible cursor but no grid is a plugin pane:
    // no cursor here (that waits on the plugin UI API).
    let pane = PaneId::new();
    let snap = build(
        "s",
        &[("t", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    assert!(snap.panes[0].grid_view.is_none());
    assert!(snap.panes[0].cursor.visible);
    assert_eq!(cursor_position(&snap), None);
}

#[test]
fn invisible_focused_pane_places_no_cursor() {
    // Focused pane suppressed / hidden (no content rect): nowhere to place it.
    let pane = PaneId::new();
    let snap = build(
        "s",
        &[("t", true)],
        &[(pane, rect(0, 1, 40, 6), false)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    assert_eq!(cursor_position(&snap), None);
}

#[test]
fn cursor_follows_focus_and_never_leaks_to_unfocused_panes() {
    let a = PaneId::new();
    let b = PaneId::new();
    let mut snap = build(
        "s",
        &[("t", true)],
        &[(a, rect(0, 1, 20, 6), true), (b, rect(20, 1, 20, 6), true)],
        Some(b),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    // Both panes carry a grid and a visible cursor at their own content origin.
    for pane in &mut snap.panes {
        pane.grid_view = Some(GridView {
            grid: Arc::new(Grid::blank(4, 18, TermStyle::default())),
            view_offset: 0,
        });
    }

    // Focused on B (content origin (21,2)): the cursor sits in B, never in A.
    assert_eq!(cursor_position(&snap), Some(Position::new(21, 2)));

    // Refocus A (content origin (1,2)): the cursor jumps to A.
    snap.client.focused_pane = Some(a);
    assert_eq!(cursor_position(&snap), Some(Position::new(1, 2)));
}

#[test]
fn small_and_zero_size_areas_are_safe() {
    let pane = PaneId::new();
    let snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 1 },
    );

    // One row tall: only the tabline, no bottom row, no panic.
    let one_row = render(&snap, 40, 1);
    assert!(row_text(&one_row, 0).contains("sess"));

    // Widths narrower than the tabline content (the mode tag is 6 cells): the
    // right-aligned segment saturates and clips instead of underflowing.
    for width in [1, 2, 3, 6] {
        let _ = render(&snap, width, 4);
    }

    // Zero area: nothing drawn, no panic.
    let mut empty = Buffer::empty(RatatuiRect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    });
    render_frame(
        &snap,
        RatatuiRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
        &mut empty,
    );
}
