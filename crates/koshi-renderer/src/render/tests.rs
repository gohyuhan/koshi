//! Tests for stock frame composition.
//!
//! The three zones render into a ratatui buffer. Tabs show their marker, and
//! the mode tag tracks the client lock mode, its mouse-select state and a
//! reconnecting viewer. Pane borders draw with focus and hover highlighting.
//! Terminal cells paint into pane content rects with their styles, wide-glyph
//! handling and highlight spans. Collapsed stack members render as
//! theme-filled title strips. The committed region solve decides which chrome
//! rows draw and where the pane rectangle sits.
//!
//! The focused pane's cursor cell is reported, clamped inside its content
//! area, and hidden for unfocused, plugin, hidden, or app-hidden cursors. The
//! cursor style follows the focused pane. A centered too-small overlay
//! replaces the frame when the tab has no room for any pane. A viewport larger
//! than the effective size centers the layout and letterboxes the margin, with
//! the cursor shifted to match. Degenerate sizes are safe, including a buffer
//! shorter than the laid-out frame.

use super::*;

use std::sync::Arc;

use koshi_core::geometry::{Point, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags};
use koshi_core::mouse::MouseTracking;
use koshi_terminal::grid::state::{Cell, Grid};
use koshi_terminal::style::{Color as TermColor, Style as TermStyle};

use koshi_terminal::state::CursorShape;

use crate::snapshot::{
    ClientSnapshot, CommittedRegions, CursorSnapshot, CursorStyle, GridView, KeymapHints, PaneSlot,
    PaneSnapshot, PluginUiSnapshot, ScrollbackMeta, SelectionSpans, SessionSnapshot, TabMeta,
    TabSnapshot, ViewerChrome,
};
use koshi_layout::mode::LayoutMode;
use koshi_layout::regions::{solve, Edge, RegionGeometry, RegionSolve};
use koshi_layout::solver::StackHeader;
use koshi_pane::pane::state::PaneKind;

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
            view_top_row: 0,
            id: *id,
            title: None,
            cursor: CursorSnapshot {
                row: 0,
                col: 0,
                visible: true,
                blink: false,
                shape: None,
            },
            grid_view: None,
            image_placements: Vec::new(),
            reverse_video: false,
            mouse_tracking: MouseTracking::Off,
            alt_scroll: false,
            on_alt_screen: false,
            selection: None,
            has_selection: false,
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
                effective_size: viewport,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                all_suppressed: false,
                gap: 0,
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
            mouse_select: false,
        },
        plugin_ui: PluginUiSnapshot::default(),
    }
}

/// The compiled-in region solve for a viewport-sized test area.
fn core_regions(w: u16, h: u16) -> CommittedRegions {
    CommittedRegions::core(Size { cols: w, rows: h }, 0)
}

/// The whole-area geometry: the top row as the first region, the bottom row as
/// the second, and the whole `w x h` viewport as the pane rectangle. A
/// one-row viewport gets an empty second region.
fn legacy_regions(w: u16, h: u16) -> CommittedRegions {
    let size = Size { cols: w, rows: h };
    let top = Rect::new(
        Point { x: 0, y: 0 },
        Size {
            cols: w,
            rows: h.min(1),
        },
    );
    let bottom = if h >= 2 {
        Rect::new(Point { x: 0, y: h - 1 }, Size { cols: w, rows: 1 })
    } else {
        Rect::zero()
    };
    CommittedRegions::new(
        size,
        RegionSolve {
            regions: vec![top, bottom],
            pane_rect: Rect::at_origin(size),
        },
        0,
    )
}

/// Render a snapshot into a fresh `w x h` buffer.
fn render(snapshot: &RenderSnapshot, w: u16, h: u16) -> Buffer {
    render_with(snapshot, &Theme::default(), w, h)
}

/// Paint `snapshot` in `theme`'s colors, for the tests that check which color
/// a surface takes rather than where it sits.
fn render_with(snapshot: &RenderSnapshot, theme: &Theme, w: u16, h: u16) -> Buffer {
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: w,
        height: h,
    };
    let mut buf = Buffer::empty(area);
    let regions = legacy_regions(w, h);
    render_frame(
        snapshot,
        &regions,
        theme,
        &KeymapHints::default(),
        None,
        ViewerChrome::default(),
        area,
        &mut buf,
    );
    buf
}

/// Paint `snapshot` with the viewer's tab strip peeking, for the tests that
/// check which tabs the strip shows.
fn render_peeking(snapshot: &RenderSnapshot, viewer: ViewerChrome, w: u16, h: u16) -> Buffer {
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: w,
        height: h,
    };
    let mut buf = Buffer::empty(area);
    let regions = legacy_regions(w, h);
    render_frame(
        snapshot,
        &regions,
        &Theme::default(),
        &KeymapHints::default(),
        None,
        viewer,
        area,
        &mut buf,
    );
    buf
}

/// Paint `snapshot` with the viewer's pointer over `hovered`, for the tests
/// that check which pane's border wears the hover color.
fn render_hovering(snapshot: &RenderSnapshot, hovered: Option<PaneId>, w: u16, h: u16) -> Buffer {
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: w,
        height: h,
    };
    let mut buf = Buffer::empty(area);
    let regions = legacy_regions(w, h);
    render_frame(
        snapshot,
        &regions,
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ViewerChrome {
            hovered_pane: hovered,
            tabline_offset: None,
            reconnecting: None,
        },
        area,
        &mut buf,
    );
    buf
}

/// Paint `snapshot` with `hints` in the bottom bar, for the tests that check
/// what the hint row says.
fn render_with_hints(snapshot: &RenderSnapshot, hints: &KeymapHints, w: u16, h: u16) -> Buffer {
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: w,
        height: h,
    };
    let mut buf = Buffer::empty(area);
    let regions = legacy_regions(w, h);
    render_frame(
        snapshot,
        &regions,
        &Theme::default(),
        hints,
        None,
        ViewerChrome::default(),
        area,
        &mut buf,
    );
    buf
}

/// Paint `snapshot` over `regions`' viewport with no hints.
fn render_with_regions(snapshot: &RenderSnapshot, regions: &CommittedRegions) -> Buffer {
    render_regions_with_hints(snapshot, regions, &KeymapHints::default())
}

/// Paint `snapshot` over `regions`' viewport with `hints` for the second
/// region, for the tests that check which chrome rows a region solve leaves
/// room for.
fn render_regions_with_hints(
    snapshot: &RenderSnapshot,
    regions: &CommittedRegions,
    hints: &KeymapHints,
) -> Buffer {
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: regions.viewport.cols,
        height: regions.viewport.rows,
    };
    let mut buf = Buffer::empty(area);
    render_frame(
        snapshot,
        regions,
        &Theme::default(),
        hints,
        None,
        ViewerChrome::default(),
        area,
        &mut buf,
    );
    buf
}

/// One `Ctrl + l` → `Lock` hint, the row the statusline draws when it has one.
fn one_hint() -> KeymapHints {
    KeymapHints {
        entries: Arc::new(vec![crate::snapshot::HintBinding {
            sequence: KeySequence::from(KeyChord::new(ModFlags::CTRL, Key::Char('l'))),
            label: "Lock".to_string(),
            user_set: false,
            pinned: false,
        }]),
        ..KeymapHints::default()
    }
}

#[test]
fn committed_core_regions_keep_the_default_frame_byte_identical() {
    let pane = PaneId::new();
    let viewport = Size { cols: 80, rows: 24 };
    let mut snapshot = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 0, 80, 22), true)],
        Some(pane),
        LockMode::Normal,
        viewport,
    );
    snapshot.session.active_tab.effective_size = Size { cols: 80, rows: 22 };

    let committed = core_regions(viewport.cols, viewport.rows);
    assert_eq!(
        render(&snapshot, 80, 24),
        render_with_regions(&snapshot, &committed)
    );
}

#[test]
fn committed_regions_keep_panes_and_cursor_inside_a_side_region() {
    let pane = PaneId::new();
    let viewport = Size {
        cols: 120,
        rows: 40,
    };
    let effective = Size {
        cols: 100,
        rows: 38,
    };
    let mut snapshot = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 0, effective.cols, effective.rows), true)],
        Some(pane),
        LockMode::Normal,
        viewport,
    );
    snapshot.session.active_tab.effective_size = effective;
    snapshot.panes[0].grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(36, 98, TermStyle::default())),
        view_offset: 0,
    });
    let regions = CommittedRegions::new(
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
        3,
    );

    let buf = render_with_regions(&snapshot, &regions);
    assert_eq!(buf[(20, 1)].symbol(), "┌");
    assert_eq!(buf[(10, 1)].symbol(), " ");
    assert_eq!(
        cursor_position(
            &snapshot,
            &regions,
            RatatuiRect {
                x: 0,
                y: 0,
                width: viewport.cols,
                height: viewport.rows,
            },
        ),
        Some(Position::new(21, 2))
    );
}

#[test]
fn a_region_solve_with_one_region_paints_no_statusline() {
    // The statusline draws in the solve's second region. A solve that names only
    // the tabline leaves the bottom row to the pane area, and the hints the
    // caller passes are drawn nowhere.
    let pane = PaneId::new();
    let snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 0, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    let regions = CommittedRegions::new(
        Size { cols: 40, rows: 8 },
        RegionSolve {
            regions: vec![Rect::new(Point { x: 0, y: 0 }, Size { cols: 40, rows: 1 })],
            pane_rect: Rect::new(Point { x: 0, y: 1 }, Size { cols: 40, rows: 7 }),
        },
        0,
    );
    let buf = render_regions_with_hints(&snap, &regions, &one_hint());

    assert_eq!(row_text(&buf, 0), sess_shell_tabline(40));
    // Rows 1..=6 are the pane box, shifted into the pane rectangle, and row 7
    // is the bottom of the pane area: every row is blank of the `Lock` hint.
    assert_eq!(row_text(&buf, 1), format!("┌{}┐", "─".repeat(38)));
    for y in 2..=5 {
        assert_eq!(
            row_text(&buf, y),
            format!("│{}│", " ".repeat(38)),
            "row {y}"
        );
    }
    assert_eq!(row_text(&buf, 6), format!("└{}┘", "─".repeat(38)));
    assert_eq!(row_text(&buf, 7), " ".repeat(40));
}

#[test]
fn an_empty_region_solve_paints_neither_chrome_row() {
    // No regions at all: the pane rectangle is the whole viewport, and both the
    // tabline and the statusline are skipped.
    let pane = PaneId::new();
    let snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    let regions = CommittedRegions::new(
        Size { cols: 40, rows: 8 },
        RegionSolve {
            regions: Vec::new(),
            pane_rect: Rect::at_origin(Size { cols: 40, rows: 8 }),
        },
        0,
    );
    let buf = render_regions_with_hints(&snap, &regions, &one_hint());

    assert_eq!(row_text(&buf, 0), " ".repeat(40));
    assert_eq!(row_text(&buf, 7), " ".repeat(40));
    // The pane box still draws, in the whole-viewport pane rectangle.
    assert_eq!(buf[(0, 1)].symbol(), "┌");
    assert_eq!(buf[(39, 6)].symbol(), "┘");
}

#[test]
fn a_solve_that_leaves_no_pane_rectangle_letterboxes_everything_but_the_chrome() {
    // A solve can hand the regions the whole viewport and leave a zero-size
    // pane rectangle. The panes still draw where the layout put them, then the
    // letterbox fills the whole frame around a zero-size content rect, and the
    // two chrome rows paint their own bar background over it.
    let pane = PaneId::new();
    let snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    let regions = CommittedRegions::new(
        Size { cols: 40, rows: 8 },
        RegionSolve {
            regions: vec![
                Rect::new(Point { x: 0, y: 0 }, Size { cols: 40, rows: 1 }),
                Rect::new(Point { x: 0, y: 7 }, Size { cols: 40, rows: 1 }),
            ],
            pane_rect: Rect::zero(),
        },
        0,
    );
    let buf = render_with_regions(&snap, &regions);

    // The pane box is drawn, and its cells wear the letterbox background.
    assert_eq!(buf[(0, 1)].symbol(), "┌");
    assert_eq!(buf[(0, 1)].bg, Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(
        row_text(&buf, 2),
        format!("│{}│", " ".repeat(38)),
        "a pane box row"
    );
    // Both chrome rows paint over the fill with the bar background.
    assert_eq!(buf[(0, 0)].bg, Color::Rgb(0x00, 0x00, 0x00));
    assert_eq!(buf[(0, 7)].bg, Color::Rgb(0x00, 0x00, 0x00));
}

/// The client's viewport as an origin-`(0, 0)` render area, matching what
/// [`render`] paints into — the `area` [`cursor_position`] takes.
fn viewport_area(snapshot: &RenderSnapshot) -> RatatuiRect {
    RatatuiRect {
        x: 0,
        y: 0,
        width: snapshot.client.viewport.cols,
        height: snapshot.client.viewport.rows,
    }
}

/// The cursor cell [`cursor_position`] reports for `snapshot` over the
/// whole-area geometry, in the client's own viewport.
fn legacy_cursor(snapshot: &RenderSnapshot) -> Option<Position> {
    let area = viewport_area(snapshot);
    let regions = legacy_regions(area.width, area.height);
    cursor_position(snapshot, &regions, area)
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
    let cols = badge_cols() + 31;
    let snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, cols, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols, rows: 8 },
    );
    let buf = render(&snap, cols, 8);

    // Tabline (row 0): the session block ` sess ` and the version badge, one
    // gap cell, the tab ribbon ` #1  shell `, blanks, then the right-aligned
    // ` BASE ` mode tag filling the last six cells.
    assert_eq!(row_text(&buf, 0), sess_shell_tabline(cols));

    // Pane border box on rows 1..=6, spanning the full width.
    assert_eq!(buf[(0, 1)].symbol(), "┌");
    assert_eq!(buf[(cols - 1, 1)].symbol(), "┐");
    assert_eq!(buf[(0, 6)].symbol(), "└");
    assert_eq!(buf[(cols - 1, 6)].symbol(), "┘");
    assert_eq!(buf[(1, 1)].symbol(), "─");
    assert_eq!(buf[(0, 2)].symbol(), "│");

    // Bottom row (row 7): the statusline row is koshi-owned chrome. This
    // snapshot carries no hint data, and every cell of the row is a space.
    assert_eq!(row_text(&buf, 7), " ".repeat(cols as usize));
}

#[test]
fn hint_bar_paints_the_bottom_row_from_the_hints_it_is_given() {
    let pane = PaneId::new();
    let snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    let hints = KeymapHints {
        entries: Arc::new(vec![crate::snapshot::HintBinding {
            sequence: KeySequence::from(KeyChord::new(ModFlags::CTRL, Key::Char('l'))),
            label: "Lock".to_string(),
            user_set: false,
            pinned: false,
        }]),
        ..KeymapHints::default()
    };
    let buf = render_with_hints(&snap, &hints, 40, 8);

    // Hint row is outside pane area: border bottom remains intact above it.
    assert_eq!(
        row_text(&buf, 7),
        format!(" Ctrl +  l  Lock{}", " ".repeat(24))
    );
    assert_eq!(buf[(0, 6)].symbol(), "└");
    assert_eq!(buf[(39, 6)].symbol(), "┘");
}

#[test]
fn two_rows_is_enough_for_both_chrome_rows() {
    let snap = build(
        "sess",
        &[("shell", true)],
        &[],
        None,
        LockMode::Normal,
        Size { cols: 40, rows: 2 },
    );
    let hints = KeymapHints {
        entries: Arc::new(vec![crate::snapshot::HintBinding {
            sequence: KeySequence::from(KeyChord::new(ModFlags::CTRL, Key::Char('l'))),
            label: "Lock".to_string(),
            user_set: false,
            pinned: false,
        }]),
        ..KeymapHints::default()
    };
    let buf = render_with_hints(&snap, &hints, 40, 2);

    // Row 0 is the tabline, row 1 the hint row: the last height that fits both.
    assert_eq!(row_text(&buf, 0), sess_shell_tabline(40));
    assert_eq!(
        row_text(&buf, 1),
        format!(" Ctrl +  l  Lock{}", " ".repeat(24))
    );
}

#[test]
fn tabline_lists_tabs_with_active_marker() {
    let pane = PaneId::new();
    let cols = badge_cols() + 51;
    let snap = build(
        "sess",
        &[("code", true), ("logs", false)],
        &[(pane, rect(0, 1, cols, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols, rows: 8 },
    );
    let buf = render(&snap, cols, 8);

    // The session block ` sess `, then the version badge, a gap, each padded
    // tab with one blank cell between them, blanks, and the ` BASE ` mode tag
    // on the last six cells.
    let badge = crate::render::version_badge();
    assert_eq!(
        row_text(&buf, 0),
        format!(" sess {badge}  #1  code   #2  logs {}BASE ", " ".repeat(18))
    );

    // Where each tab landed, read from the same solve the paint used, so the
    // badge's width never has to be spelled out here.
    let tabs = tabline_layout(
        snap.layout(ViewerChrome::default()).tabline(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 60,
            height: 1,
        },
    )
    .tabs;
    let (active, inactive) = (tabs[0].1 + 1, tabs[1].1 + 1);

    // The active tab is inverted: its ramp stop as the TEXT color over the
    // bar background the row is filled with; an inactive tab's blocks sit on
    // its dimmed stop. Two tabs → the stops are the ramp's purple and blue
    // ends.
    assert_eq!(buf[(active, 0)].fg, Color::Rgb(0xd0, 0xa5, 0xff));
    assert_eq!(buf[(active, 0)].bg, Color::Rgb(0x00, 0x00, 0x00));
    assert_eq!(buf[(inactive, 0)].bg, Color::Rgb(0x44, 0x67, 0x8c));
}

#[test]
fn tabline_scrolls_overflowing_tabs_behind_a_right_arrow() {
    let pane = PaneId::new();
    let cols = badge_cols() + 31;
    let snap = build(
        "sess",
        &[
            ("alpha", true),
            ("bravo", false),
            ("charlie", false),
            ("delta", false),
            ("echo", false),
        ],
        &[(pane, rect(0, 1, cols, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols, rows: 8 },
    );
    let buf = render(&snap, cols, 8);

    // The session block and the mode tag always render whole. The active tab
    // (alpha, index 0) fits from the left, so the window starts there and the
    // four tabs hidden off the right sit behind a `▶` scroll arrow. The blank
    // cell where a `◀` would sit stays blank: nothing is hidden to the left.
    let badge = crate::render::version_badge();
    assert_eq!(
        row_text(&buf, 0),
        format!(" sess {badge}   #1  alpha      ▶ BASE ")
    );
}

/// Cells the tabline's version badge takes, measured from the badge the tabline
/// actually paints. A semver version is ASCII, so counting characters counts
/// display cells.
///
/// A test that needs room beside the badge asks for `badge_cols() + <room>`
/// rather than a fixed total: the room beside the badge stays the same however
/// long the version string is.
fn badge_cols() -> u16 {
    crate::render::version_badge().chars().count() as u16
}

/// The whole tabline row a session named `sess` with the single active tab
/// `shell` paints into a `cols`-wide row, with ` BASE ` as the mode tag.
///
/// `cols` must leave room for all of it — at least `badge_cols() + 24`.
fn sess_shell_tabline(cols: u16) -> String {
    sess_shell_tabline_tagged(cols, " BASE ")
}

/// The whole tabline row a session named `sess` with the single active tab
/// `shell` paints into a `cols`-wide row: the ` sess ` block, the version
/// badge, one gap cell, the ` #1  shell ` ribbon, blank cells, then `tag`
/// right-aligned on the last `tag.chars().count()` cells.
///
/// `tag` is the mode block with its own padding spaces, such as ` BASE ` or
/// ` LOCK `. `cols` must leave room for all of it.
fn sess_shell_tabline_tagged(cols: u16, tag: &str) -> String {
    let badge = crate::render::version_badge();
    let blanks = cols as usize - 6 - badge.chars().count() - 1 - 11 - tag.chars().count();
    [
        " sess ".to_string(),
        badge,
        " ".to_string(),
        " #1  shell ".to_string(),
        " ".repeat(blanks),
        tag.to_string(),
    ]
    .concat()
}

/// Overflowing tabs, offset unset: the window scrolls to reveal the active tab
/// even when it lands deep in the tail, and both sides show a scroll arrow.
#[test]
fn tabline_follows_focus_into_the_overflow() {
    let pane = PaneId::new();
    let snap = build(
        "s",
        &[
            ("t0", false),
            ("t1", false),
            ("t2", false),
            ("t3", false),
            ("t4", false),
            ("t5", true),
            ("t6", false),
            ("t7", false),
        ],
        &[(pane, rect(0, 1, badge_cols() + 21, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            cols: badge_cols() + 21,
            rows: 8,
        },
    );
    let tabline = row_text(&render(&snap, badge_cols() + 21, 8), 0);

    // Only the active tab `t5` fits, as tab six: `t0`..`t4` sit behind the `◀`
    // arrow and `t6`, `t7` behind the `▶` one.
    let badge = crate::render::version_badge();
    assert_eq!(tabline, format!(" s {badge} ◀ #6  t5  ▶ BASE "));
}

/// A peek offset windows the strip from that index, not the active tab: the
/// active tab may stay hidden while peeking, and a left offset of 0 shows no
/// left arrow.
#[test]
fn tabline_peek_offset_ignores_the_active_tab() {
    let pane = PaneId::new();
    let snap = build(
        "s",
        &[
            ("t0", false),
            ("t1", false),
            ("t2", false),
            ("t3", false),
            ("t4", false),
            ("t5", true),
            ("t6", false),
            ("t7", false),
        ],
        &[(pane, rect(0, 1, badge_cols() + 21, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            cols: badge_cols() + 21,
            rows: 8,
        },
    );
    let peeking = ViewerChrome {
        hovered_pane: None,
        tabline_offset: Some(0),
        reconnecting: None,
    };
    let tabline = row_text(&render_peeking(&snap, peeking, badge_cols() + 21, 8), 0);

    // The strip windows from index 0, so only `t0` shows and the active `t5`
    // stays hidden behind the `▶` arrow. The `◀` cell stays blank: nothing is
    // hidden to the left of index 0.
    let badge = crate::render::version_badge();
    assert_eq!(tabline, format!(" s {badge}   #1  t0  ▶ BASE "));
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
    assert_eq!(row_text(&base, 0), sess_shell_tabline_tagged(40, " BASE "));

    // The lock tag replaces the base one in the same six right-aligned cells.
    let locked = render(&make(LockMode::Locked), 40, 8);
    assert_eq!(
        row_text(&locked, 0),
        sess_shell_tabline_tagged(40, " LOCK ")
    );
}

#[test]
fn a_reconnecting_viewer_puts_the_dial_tag_in_the_tabline() {
    // The mode block is right-aligned and takes whatever room it needs. The
    // reconnecting tag is 37 cells wide, so on a 46-cell-plus-badge row it
    // leaves nothing for the tab ribbon.
    let pane = PaneId::new();
    let cols = badge_cols() + 46;
    let snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, cols, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols, rows: 8 },
    );
    let dialing = ViewerChrome {
        hovered_pane: None,
        tabline_offset: None,
        reconnecting: Some(Reconnecting {
            attempt: 3,
            retry_in_seconds: 8,
        }),
    };
    let buf = render_peeking(&snap, dialing, cols, 8);

    let badge = crate::render::version_badge();
    assert_eq!(
        row_text(&buf, 0),
        format!(" sess {badge}  RECONNECTING (attempt 3, retry in 8s) ")
    );
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

    // Focused pane: the theme's focus color, bold border corner.
    assert_eq!(buf[(0, 1)].fg, Color::Rgb(0x00, 0xaf, 0xd7));
    assert_eq!(buf[(0, 1)].modifier, Modifier::BOLD);
    // Unfocused pane: dim border corner, no modifier at all.
    assert_eq!(buf[(20, 1)].fg, Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(buf[(20, 1)].modifier, Modifier::empty());
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

    // No border cell anywhere the box would have been: rows 1..=6 are blank.
    for y in 1..=6 {
        assert_eq!(row_text(&buf, y), " ".repeat(40), "row {y}");
    }
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

    // At the live tail (offset 0): the pane's bottom border (row 6 — the box
    // spans rows 1..=6) is unbroken, and the tabline carries no indicator.
    let tail = render(&snap, 40, 8);
    assert_eq!(row_text(&tail, 6), format!("└{}┘", "─".repeat(38)));
    assert_eq!(row_text(&tail, 0), sess_shell_tabline(40));

    // Scrolled back three lines with 100 retained: the count sits right-aligned
    // in this pane's own bottom border. The tabline keeps the ` BASE ` mode tag.
    snap.panes[0].grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(6, 40, TermStyle::default())),
        view_offset: 3,
    });
    snap.panes[0].scrollback.retained_lines = 100;
    let buf = render(&snap, 40, 8);
    assert_eq!(
        row_text(&buf, 6),
        format!("└{} 3/100 ┘", "─".repeat(31)),
        "the count is right-aligned and the corner glyph survives"
    );
    assert_eq!(
        row_text(&buf, 0),
        sess_shell_tabline(40),
        "no global indicator"
    );
}

#[test]
fn each_pane_shows_its_own_scroll_position() {
    let a = PaneId::new();
    let b = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[(a, rect(0, 1, 20, 6), true), (b, rect(20, 1, 20, 6), true)],
        Some(a),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    // A is scrolled 3 up of 100; B is scrolled 7 up of 50 — different views.
    snap.panes[0].grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(6, 20, TermStyle::default())),
        view_offset: 3,
    });
    snap.panes[0].scrollback.retained_lines = 100;
    snap.panes[1].grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(6, 20, TermStyle::default())),
        view_offset: 7,
    });
    snap.panes[1].scrollback.retained_lines = 50;

    // Both bottom borders are row 6; each carries its own count, right-aligned
    // in its own box.
    assert_eq!(
        row_text(&render(&snap, 40, 8), 6),
        format!("└{} 3/100 ┘└{} 7/50 ┘", "─".repeat(11), "─".repeat(12))
    );
}

/// A scrolled-back pane whose box is `width` wide, in a viewport of the same
/// width: one tabline row, four box rows, one hint row.
fn narrow_scrolled_snap(width: u16) -> RenderSnapshot {
    let pane = PaneId::new();
    let mut snap = build(
        "s",
        &[("t", true)],
        &[(pane, rect(0, 1, width, 4), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            cols: width,
            rows: 6,
        },
    );
    snap.panes[0].grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(2, width - 2, TermStyle::default())),
        view_offset: 3,
    });
    snap.panes[0].scrollback.retained_lines = 100;
    snap
}

#[test]
fn a_box_too_narrow_for_the_scroll_position_shows_none_of_it() {
    // ` 3/100 ` takes seven cells and never covers a corner glyph, so it needs
    // a box nine cells wide. An eight-wide box keeps its bottom border whole.
    let buf = render(&narrow_scrolled_snap(8), 8, 6);
    assert_eq!(row_text(&buf, 4), "└──────┘");

    // One cell wider, and it sits between the two corners.
    let buf = render(&narrow_scrolled_snap(9), 9, 6);
    assert_eq!(row_text(&buf, 4), "└ 3/100 ┘");
}

#[test]
fn a_scrolled_pane_that_retained_nothing_shows_a_zero_total() {
    // The indicator reports the pane's own retained-line count verbatim. A pane
    // scrolled three lines up whose scrollback retained none reads ` 3/0 `.
    let pane = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    snap.panes[0].grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(4, 38, TermStyle::default())),
        view_offset: 3,
    });
    snap.panes[0].scrollback.retained_lines = 0;
    let buf = render(&snap, 40, 8);

    assert_eq!(row_text(&buf, 6), format!("└{} 3/0 ┘", "─".repeat(33)));
}

#[test]
fn a_pane_with_no_grid_shows_no_scroll_position() {
    // The scroll position comes from the pane's grid view. A pane that carries
    // scrollback metadata but no grid — a plugin pane — reads as the live tail,
    // so its bottom border stays unbroken.
    let pane = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    snap.panes[0].scrollback.retained_lines = 100;
    assert_eq!(snap.panes[0].grid_view, None);
    let buf = render(&snap, 40, 8);

    assert_eq!(row_text(&buf, 6), format!("└{}┘", "─".repeat(38)));
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
        Size {
            cols: badge_cols() + 15,
            rows: 6,
        },
    );

    // A buffer reused across frames holds the previous frame's cells; simulate
    // that with a full grid of stale glyphs before rendering.
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: badge_cols() + 15,
        height: 6,
    };
    let mut buf = Buffer::empty(area);
    for y in 0..area.height {
        for x in 0..area.width {
            buf[(x, y)].set_symbol("X");
        }
    }

    let regions = legacy_regions(area.width, area.height);
    render_frame(
        &snap,
        &regions,
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ViewerChrome::default(),
        area,
        &mut buf,
    );

    // Tabline gap between the left tab list and the right status: blanked.
    assert_eq!(buf[(badge_cols() + 3, 0)].symbol(), " ");
    // A cell outside every pane box: blanked, not the stale glyph.
    assert_eq!(buf[(badge_cols() + 13, 2)].symbol(), " ");
    // Reserved hint row (bottom): every cell a space.
    assert_eq!(row_text(&buf, 5), " ".repeat(area.width as usize));
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
    assert_eq!(
        row_text(&buf, 1),
        format!("▸ editor{}[2/3]", " ".repeat(17))
    );
    // Row 2: C's strip.
    assert_eq!(row_text(&buf, 2), format!("▸ logs{}[3/3]", " ".repeat(19)));

    // The whole strip row carries the theme's strip colors (the koshi-owned
    // marker), gap included.
    for x in 0..30 {
        assert_eq!(
            buf[(x, 1)].fg,
            Color::Rgb(0xf4, 0xf1, 0xfa),
            "col {x} of strip"
        );
        assert_eq!(
            buf[(x, 1)].bg,
            Color::Rgb(0x30, 0x0f, 0x4a),
            "col {x} of strip"
        );
    }
}

#[test]
fn the_hover_color_marks_an_unfocused_pane_but_never_the_focused_one() {
    let focused = PaneId::new();
    let other = PaneId::new();
    let snap = build(
        "sess",
        &[("shell", true)],
        &[
            (focused, rect(0, 1, 20, 6), true),
            (other, rect(20, 1, 20, 6), true),
        ],
        Some(focused),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );

    // Hovering the focused pane changes nothing: it keeps the focus color.
    let buf = render_hovering(&snap, Some(focused), 40, 8);
    assert_eq!(
        buf[(0, 1)].fg,
        Theme::default().border_focused,
        "the focused pane keeps its focus color even when hovered"
    );

    // Hovering the unfocused pane paints its border the hover color, and the
    // focused pane is untouched.
    let buf = render_hovering(&snap, Some(other), 40, 8);
    assert_eq!(
        buf[(20, 1)].fg,
        Theme::default().border_hover,
        "an unfocused pane under the pointer takes the hover color"
    );
    assert_eq!(
        buf[(0, 1)].fg,
        Theme::default().border_focused,
        "the focused pane's border is unaffected by hovering elsewhere"
    );
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

    // Four collapsed strips (rows 1..=4). None of the members carries a title,
    // so each reads as the arrow, blanks, then its own right-aligned [k/5].
    for (i, k) in (2..=5).enumerate() {
        assert_eq!(
            row_text(&buf, (i + 1) as u16),
            format!("▸ {}[{k}/5]", " ".repeat(23)),
            "row {}",
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

    assert_eq!(row_text(&buf, 1), format!("▸ {}[1/2]", " ".repeat(23)));
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

    // The indicator clips inside the strip: its first three cells land on
    // x=10..13 and nothing is written left of x=10.
    assert_eq!(
        row_text(&buf, 1),
        format!("{}[10{}", " ".repeat(10), " ".repeat(7))
    );
    for x in 0..10 {
        assert_ne!(
            buf[(x, 1)].bg,
            Color::Rgb(0x30, 0x0f, 0x4a),
            "col {x} styled outside strip"
        );
    }
    // The strip's own cells (x=10..13) carry the strip background.
    for x in 10..13 {
        assert_eq!(buf[(x, 1)].bg, Color::Rgb(0x30, 0x0f, 0x4a));
    }
}

#[test]
fn a_stack_header_naming_a_pane_the_frame_dropped_shows_an_empty_title() {
    // A header can name a pane id absent from `panes` (the pane exited and was
    // pruned between the layout solve and the snapshot build): the title falls
    // back to empty and the strip still draws its arrow and indicator.
    let active = PaneId::new();
    let pruned = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[(active, rect(0, 2, 30, 4), true)],
        Some(active),
        LockMode::Normal,
        Size { cols: 30, rows: 8 },
    );
    snap.session.active_tab.stack_headers = vec![StackHeader {
        pane: pruned,
        rect: rect(0, 1, 30, 1),
        position: 0,
        total: 2,
    }];
    let buf = render(&snap, 30, 8);

    assert_eq!(row_text(&buf, 1), format!("▸ {}[1/2]", " ".repeat(23)));
    assert_eq!(buf[(0, 1)].bg, Color::Rgb(0x30, 0x0f, 0x4a));
}

#[test]
fn a_zero_size_stack_header_strip_draws_nothing() {
    // A strip solved to zero columns or zero rows is skipped whole: its row
    // keeps the blank cells and the default background it was cleared to.
    let active = PaneId::new();
    let member = PaneId::new();
    for strip in [rect(0, 1, 0, 1), rect(0, 1, 30, 0)] {
        let mut snap = build(
            "sess",
            &[("shell", true)],
            &[(active, rect(0, 2, 30, 4), true)],
            Some(active),
            LockMode::Normal,
            Size { cols: 30, rows: 8 },
        );
        snap.session.active_tab.stack_headers = vec![StackHeader {
            pane: member,
            rect: strip,
            position: 0,
            total: 2,
        }];
        let buf = render(&snap, 30, 8);

        assert_eq!(row_text(&buf, 1), " ".repeat(30), "strip {strip:?}");
        assert_eq!(buf[(0, 1)].bg, Color::Reset, "strip {strip:?}");
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
    assert_eq!(buf[(1, 2)].modifier, Modifier::BOLD | Modifier::ITALIC);

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
fn several_marks_join_one_base_into_one_symbol_in_push_order() {
    let mut grid = Grid::blank(4, 38, TermStyle::default());
    let mut cell = Cell::new('e', 1, TermStyle::default());
    cell.push_combining('\u{0301}'); // combining acute accent
    cell.push_combining('\u{0308}'); // combining diaeresis
    *grid.cell_mut(0, 0).unwrap() = cell;
    let snap = content_snap(grid, rect(0, 1, 40, 6), false, Size { cols: 40, rows: 8 });
    let buf = render(&snap, 40, 8);

    assert_eq!(buf[(1, 2)].symbol(), "e\u{0301}\u{0308}");
}

#[test]
fn every_cell_attribute_maps_to_its_own_modifier() {
    let mut grid = Grid::blank(4, 38, TermStyle::default());
    let mut every = TermStyle::default();
    every.set_bold(true);
    every.set_faint(true);
    every.set_italic(true);
    every.set_underline(UnderlineStyle::Single);
    every.set_blink(true);
    every.set_conceal(true);
    every.set_strike(true);
    every.set_reverse(true);
    *grid.cell_mut(0, 0).unwrap() = Cell::new('a', 1, every);

    // A curly underline is one of the five underline styles ratatui cannot tell
    // apart; it draws as the single underline ratatui has.
    let mut curly = TermStyle::default();
    curly.set_underline(UnderlineStyle::Curly);
    *grid.cell_mut(0, 1).unwrap() = Cell::new('b', 1, curly);

    // Overline and underline color have no ratatui modifier and draw nothing.
    let mut lines = TermStyle::default();
    lines.set_overline(true);
    lines.set_underline_color(Some(TermColor::Indexed(9)));
    *grid.cell_mut(0, 2).unwrap() = Cell::new('c', 1, lines);

    let snap = content_snap(grid, rect(0, 1, 40, 6), false, Size { cols: 40, rows: 8 });
    let buf = render(&snap, 40, 8);

    assert_eq!(
        buf[(1, 2)].modifier,
        Modifier::BOLD
            | Modifier::DIM
            | Modifier::ITALIC
            | Modifier::UNDERLINED
            | Modifier::SLOW_BLINK
            | Modifier::HIDDEN
            | Modifier::CROSSED_OUT
            | Modifier::REVERSED
    );
    assert_eq!(buf[(2, 2)].modifier, Modifier::UNDERLINED);
    assert_eq!(buf[(3, 2)].modifier, Modifier::empty());
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
    assert_eq!(buf[(1, 2)].modifier, Modifier::REVERSED);
    // ...and cancels a cell that is already reversed (reverse XOR reverse).
    assert_eq!(buf[(2, 2)].modifier, Modifier::empty());
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
    assert_eq!(row_text(&buf, 2), format!("│{}│", " ".repeat(38)));
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
        shape: None,
    };
    assert_eq!(legacy_cursor(&snap), Some(Position::new(6, 4)));
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
        shape: None,
    };
    assert_eq!(legacy_cursor(&snap), Some(Position::new(38, 5)));
}

#[test]
fn cursor_style_reports_the_focused_panes_shape_and_blink() {
    // vim in insert mode asked for a blinking bar; the caller passes that style
    // out to the terminal koshi is running in.
    let mut snap = content_snap(
        Grid::blank(4, 38, TermStyle::default()),
        rect(0, 1, 40, 6),
        false,
        Size { cols: 40, rows: 8 },
    );
    snap.panes[0].cursor = CursorSnapshot {
        row: 0,
        col: 0,
        visible: true,
        blink: true,
        shape: Some(CursorShape::Bar),
    };
    assert_eq!(
        cursor_style(&snap),
        Some(CursorStyle::Shaped {
            shape: CursorShape::Bar,
            blink: true
        })
    );
}

#[test]
fn a_pane_that_asked_for_no_shape_leaves_the_users_own_cursor_alone() {
    // A plain shell never sends DECSCUSR. Focusing it must NOT stamp a block
    // over the cursor the user configured in their own terminal — it hands the
    // cursor back to them.
    let snap = content_snap(
        Grid::blank(4, 38, TermStyle::default()),
        rect(0, 1, 40, 6),
        false,
        Size { cols: 40, rows: 8 },
    );
    assert_eq!(snap.panes[0].cursor.shape, None);
    assert_eq!(cursor_style(&snap), Some(CursorStyle::UserDefault));
}

#[test]
fn cursor_style_is_none_without_a_focused_terminal_pane() {
    let mut snap = content_snap(
        Grid::blank(4, 38, TermStyle::default()),
        rect(0, 1, 40, 6),
        false,
        Size { cols: 40, rows: 8 },
    );
    // No focused pane: nobody speaks for the cursor, so it is left as it is.
    let focused = snap.client.focused_pane.take();
    assert_eq!(cursor_style(&snap), None);

    // A plugin pane has no terminal, so it has no opinion on the cursor either.
    snap.client.focused_pane = focused;
    snap.panes[0].grid_view = None;
    assert_eq!(cursor_style(&snap), None);
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
    assert_eq!(legacy_cursor(&snap), None);
}

#[test]
fn a_scrolled_back_view_places_no_cursor() {
    // The app's cursor is visible, but the view is scrolled into history, so the
    // live cursor cell is off-screen and no hardware cursor is placed.
    let mut snap = content_snap(
        Grid::blank(4, 38, TermStyle::default()),
        rect(0, 1, 40, 6),
        false,
        Size { cols: 40, rows: 8 },
    );
    assert!(snap.panes[0].cursor.visible);
    snap.panes[0].grid_view.as_mut().unwrap().view_offset = 3;
    assert_eq!(legacy_cursor(&snap), None);
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
    assert_eq!(legacy_cursor(&snap), None);
}

#[test]
fn plugin_pane_places_no_cursor() {
    // A visible focused pane with a visible cursor but no grid is a plugin
    // pane: it places no cursor.
    let pane = PaneId::new();
    let snap = build(
        "s",
        &[("t", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    assert_eq!(snap.panes[0].grid_view, None);
    assert!(snap.panes[0].cursor.visible);
    assert_eq!(legacy_cursor(&snap), None);
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
    assert_eq!(legacy_cursor(&snap), None);
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
    assert_eq!(legacy_cursor(&snap), Some(Position::new(21, 2)));

    // Refocus A (content origin (1,2)): the cursor jumps to A.
    snap.client.focused_pane = Some(a);
    assert_eq!(legacy_cursor(&snap), Some(Position::new(1, 2)));
}

#[test]
fn cursor_style_follows_focus_between_panes() {
    // Pane A runs vim in insert mode (it asked for a blinking bar); pane B runs
    // a plain shell (it asked for nothing). The style belongs to the outer
    // terminal, not to a pane's cells: moving focus hands it the newly focused
    // pane's answer, so focusing the shell drops vim's bar.
    let a = PaneId::new();
    let b = PaneId::new();
    let mut snap = build(
        "s",
        &[("t", true)],
        &[(a, rect(0, 1, 20, 6), true), (b, rect(20, 1, 20, 6), true)],
        Some(a),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    for pane in &mut snap.panes {
        pane.grid_view = Some(GridView {
            grid: Arc::new(Grid::blank(4, 18, TermStyle::default())),
            view_offset: 0,
        });
    }
    snap.panes[0].cursor.shape = Some(CursorShape::Bar);
    snap.panes[0].cursor.blink = true;
    snap.panes[1].cursor.shape = None;

    assert_eq!(
        cursor_style(&snap),
        Some(CursorStyle::Shaped {
            shape: CursorShape::Bar,
            blink: true
        })
    );

    snap.client.focused_pane = Some(b);
    assert_eq!(cursor_style(&snap), Some(CursorStyle::UserDefault));
}

/// A snapshot whose active tab has no room for any pane: every slot suppressed
/// and `all_suppressed` set, as the layout solver produces on a too-small tab.
fn too_small_snap(viewport: Size) -> RenderSnapshot {
    let pane = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), false)],
        Some(pane),
        LockMode::Normal,
        viewport,
    );
    snap.session.active_tab.all_suppressed = true;
    snap.session.active_tab.layout_solved[0].suppressed = true;
    snap
}

#[test]
fn too_small_overlay_shown_when_all_suppressed() {
    let snap = too_small_snap(Size { cols: 60, rows: 10 });
    let buf = render(&snap, 60, 10);

    // Centered on the middle row (10/2 = 5); the 35-wide message is horizontally
    // centered, starting at col (60-35)/2 = 12, and drawn bold.
    assert_eq!(
        row_text(&buf, 5),
        format!(
            "{}Terminal too small — enlarge window{}",
            " ".repeat(12),
            " ".repeat(13)
        )
    );
    assert_eq!(buf[(12, 5)].modifier, Modifier::BOLD);
}

#[test]
fn too_small_overlay_replaces_tabline_and_panes() {
    let snap = too_small_snap(Size { cols: 60, rows: 10 });
    let buf = render(&snap, 60, 10);

    // The overlay owns row 5 alone. Every other row is blank: no tabline, no
    // statusline, and no pane border anywhere.
    for y in (0..10).filter(|y| *y != 5) {
        assert_eq!(row_text(&buf, y), " ".repeat(60), "row {y}");
    }
}

#[test]
fn too_small_frame_places_no_cursor() {
    // Every pane is suppressed (no content area), so the overlay frame shows no
    // hardware cursor.
    let snap = too_small_snap(Size { cols: 60, rows: 10 });
    assert_eq!(legacy_cursor(&snap), None);
}

#[test]
fn too_small_overlay_clips_on_narrow_screen() {
    // Viewport narrower than the 35-wide message: it clips to the width with no
    // panic and no write past the right edge.
    let snap = too_small_snap(Size { cols: 10, rows: 4 });
    let buf = render(&snap, 10, 4);

    // Centered on row 2; the message saturates to col 0 and shows its 10-cell
    // clipped prefix.
    assert_eq!(row_text(&buf, 2), "Terminal t");
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
    assert_eq!(row_text(&one_row, 0), sess_shell_tabline(40));

    // Widths narrower than the tabline content (the mode tag is 6 cells): the
    // right-aligned segment saturates to col 0 and clips instead of
    // underflowing, and it takes the row whole — no room is left for a tab.
    for (width, expected) in [(1, " "), (2, " B"), (3, " BA"), (6, " BASE ")] {
        assert_eq!(
            row_text(&render(&snap, width, 4), 0),
            expected,
            "width {width}"
        );
    }

    // Zero area: nothing drawn, no panic.
    let mut empty = Buffer::empty(RatatuiRect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    });
    let regions = legacy_regions(0, 0);
    render_frame(
        &snap,
        &regions,
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ViewerChrome::default(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
        &mut empty,
    );
}

/// A letterbox snapshot: a client `viewport` larger than the effective middle
/// pane region, with one visible pane laid out from that region's origin.
fn letterbox_snap(pane: PaneId, viewport: Size, effective: Size) -> RenderSnapshot {
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 0, effective.cols, effective.rows), true)],
        Some(pane),
        LockMode::Normal,
        viewport,
    );
    snap.session.active_tab.effective_size = effective;
    snap
}

#[test]
fn larger_viewport_centers_layout_and_letterboxes_margin() {
    let pane = PaneId::new();
    let snap = letterbox_snap(
        pane,
        Size { cols: 60, rows: 12 },
        Size { cols: 40, rows: 8 },
    );
    let buf = render(&snap, 60, 12);

    // Effective 40x8 pane region centered in 60x12 → offset (10, 2).
    assert_eq!(buf[(10, 2)].symbol(), "┌");
    assert_eq!(buf[(49, 2)].symbol(), "┐");
    assert_eq!(buf[(10, 9)].symbol(), "└");

    // Chrome stays on outer rows, independent of centered pane geometry.
    assert_eq!(row_text(&buf, 0), sess_shell_tabline(60));

    // Margin cells around the pane region carry dim letterbox fill.
    for (x, y) in [(30, 1), (9, 5), (50, 5), (30, 10)] {
        assert_eq!(buf[(x, y)].symbol(), " ", "margin ({x},{y})");
        assert_eq!(
            buf[(x, y)].bg,
            Color::Rgb(0x58, 0x58, 0x58),
            "margin ({x},{y})"
        );
    }

    // A cell inside the content rect keeps the default background: the fill
    // lands only in the margin, never over the layout.
    assert_eq!(buf[(11, 3)].bg, Color::Reset);
}

#[test]
fn cursor_shifts_into_centered_content() {
    let pane = PaneId::new();
    let mut snap = letterbox_snap(
        pane,
        Size { cols: 60, rows: 12 },
        Size { cols: 40, rows: 8 },
    );
    snap.panes[0].grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(4, 38, TermStyle::default())),
        view_offset: 0,
    });
    snap.panes[0].cursor = CursorSnapshot {
        row: 2,
        col: 5,
        visible: true,
        blink: false,
        shape: None,
    };

    // Content origin offset (10,2); pane inner origin (1,1) places to (11,3);
    // cursor row 2, col 5 lands at (16,5).
    assert_eq!(legacy_cursor(&snap), Some(Position::new(16, 5)));
}

#[test]
fn a_pane_whose_content_rect_holds_no_cells_places_no_cursor() {
    // A pane box of two columns insets to a zero-width content rect and is
    // still marked visible: there is no cell inside it to put the cursor on.
    let pane = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(4, 5, 2, 1), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 10 },
    );
    snap.panes[0].grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(4, 1, TermStyle::default())),
        view_offset: 0,
    });
    assert_eq!(
        snap.session.active_tab.layout_solved[0]
            .inner_rect
            .expect("the slot is visible")
            .size,
        Size { cols: 0, rows: 0 }
    );

    assert_eq!(legacy_cursor(&snap), None);
}

#[test]
fn letterbox_clips_to_a_buffer_smaller_than_the_area() {
    // A resize race can hand render_frame an `area` larger than the buffer. The
    // letterbox fill must clip to the buffer, not index out of bounds.
    let pane = PaneId::new();
    let snap = letterbox_snap(
        pane,
        Size { cols: 60, rows: 12 },
        Size { cols: 40, rows: 8 },
    );
    let mut buf = Buffer::empty(RatatuiRect {
        x: 0,
        y: 0,
        width: 30,
        height: 6,
    });
    let regions = core_regions(60, 12);
    render_frame(
        &snap,
        &regions,
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ViewerChrome::default(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 60,
            height: 12,
        },
        &mut buf,
    );

    // No panic, and a margin cell inside the smaller buffer still got the
    // fill (row 0 is the tabline's, so probe the margin band below it).
    assert_eq!(buf[(0, 1)].bg, Color::Rgb(0x58, 0x58, 0x58));
}

#[test]
fn an_area_smaller_than_the_committed_regions_letterboxes_nothing_below_it() {
    // A terminal shrink between the session's last viewport report and this
    // paint: the committed solve is for 60x12, the render area only 30x6, so
    // the centered content rect reaches past the area's bottom and right.
    let pane = PaneId::new();
    let snap = letterbox_snap(
        pane,
        Size { cols: 60, rows: 12 },
        Size { cols: 40, rows: 8 },
    );
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: 30,
        height: 6,
    };
    let mut buf = Buffer::empty(area);
    let regions = core_regions(60, 12);
    render_frame(
        &snap,
        &regions,
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ViewerChrome::default(),
        area,
        &mut buf,
    );

    // The content rect starts at column 10, row 2, so the band left of it
    // carries the fill and the cells inside it do not.
    assert_eq!(buf[(9, 5)].bg, Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(buf[(10, 5)].bg, Color::Reset);
}

#[test]
fn chrome_below_a_shrunk_buffer_is_skipped_not_panicked() {
    // Resize race: the snapshot's layout was solved for a taller frame than the
    // current buffer. Chrome rows (stack-header strips) laid out below the buffer
    // must be skipped, not written out of bounds.
    let active = PaneId::new();
    let collapsed = PaneId::new();
    let cols = badge_cols() + 16;
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[
            (active, rect(0, 3, cols, 6), true),
            (collapsed, rect(0, 8, cols, 1), false),
        ],
        Some(active),
        LockMode::Normal,
        Size { cols, rows: 10 },
    );
    snap.panes[1].title = Some("logs".to_string());
    // A strip at row 8 — below a buffer only 5 rows tall.
    snap.session.active_tab.stack_headers = vec![StackHeader {
        pane: collapsed,
        rect: rect(0, 8, cols, 1),
        position: 1,
        total: 2,
    }];

    // Buffer shorter than the solved layout; area matches the buffer.
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: cols,
        height: 5,
    };
    let mut buf = Buffer::empty(area);
    let regions = legacy_regions(cols, 5);
    render_frame(
        &snap,
        &regions,
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ViewerChrome::default(),
        area,
        &mut buf,
    );

    // No panic, and the strip laid out at row 8 wrote nothing: row 0 is the
    // tabline (too narrow for the tab, so only the `▶` arrow and the mode tag),
    // row 3 the pane box's top border, row 4 the blanked hint row, and the rest
    // blank.
    let badge = crate::render::version_badge();
    assert_eq!(row_text(&buf, 0), format!(" sess {badge}   ▶ BASE "));
    assert_eq!(row_text(&buf, 1), " ".repeat(cols as usize));
    assert_eq!(row_text(&buf, 2), " ".repeat(cols as usize));
    assert_eq!(
        row_text(&buf, 3),
        format!("┌{}┐", "─".repeat(cols as usize - 2))
    );
    assert_eq!(row_text(&buf, 4), " ".repeat(cols as usize));
}

#[test]
fn equal_viewport_draws_no_letterbox() {
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

    // Effective size equals the viewport: the layout fills the frame and no cell
    // carries the letterbox background.
    for y in 0..8 {
        for x in 0..40 {
            assert_ne!(
                buf[(x, y)].bg,
                Color::Rgb(0x58, 0x58, 0x58),
                "cell ({x},{y})"
            );
        }
    }
}

#[test]
fn an_effective_size_larger_than_the_pane_area_draws_no_letterbox() {
    // A client smaller than the size the tab was solved for: the content rect
    // is clamped to the pane area, so it fills the frame and no margin is left.
    let pane = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    snap.session.active_tab.effective_size = Size { cols: 80, rows: 20 };
    let buf = render(&snap, 40, 8);

    // The layout is not shifted, and no cell carries the letterbox fill.
    assert_eq!(buf[(0, 1)].symbol(), "┌");
    assert_eq!(buf[(39, 6)].symbol(), "┘");
    for y in 0..8 {
        for x in 0..40 {
            assert_ne!(
                buf[(x, y)].bg,
                Color::Rgb(0x58, 0x58, 0x58),
                "cell ({x},{y})"
            );
        }
    }
}

#[test]
fn an_odd_letterbox_margin_is_one_cell_wider_right_and_below() {
    // 41x9 centered in 60x12 splits 19 spare columns and 3 spare rows unevenly:
    // the halves round down, so the left margin is 9 and the right 10, the top
    // margin 1 row and the bottom 2.
    let pane = PaneId::new();
    let snap = letterbox_snap(
        pane,
        Size { cols: 60, rows: 12 },
        Size { cols: 41, rows: 9 },
    );
    let buf = render(&snap, 60, 12);

    // The pane box starts at (9, 1) and ends at (49, 9).
    assert_eq!(buf[(9, 1)].symbol(), "┌");
    assert_eq!(buf[(49, 1)].symbol(), "┐");
    assert_eq!(buf[(49, 9)].symbol(), "┘");

    // Last margin column on the left, first on the right, and the first margin
    // row below the content.
    assert_eq!(buf[(8, 5)].bg, Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(buf[(50, 5)].bg, Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(buf[(30, 10)].bg, Color::Rgb(0x58, 0x58, 0x58));
    // The border column just inside the left margin keeps the default fill.
    assert_eq!(buf[(9, 5)].bg, Color::Reset);
}

/// A non-default palette on the snapshot recolors every chrome element the
/// theme names; the same frame under the default theme paints none of these
/// custom colors.
#[test]
fn a_custom_theme_recolors_the_chrome() {
    let left = PaneId::new();
    let right = PaneId::new();
    let snap = build(
        "sess",
        &[("shell", true), ("logs", false)],
        &[
            (left, rect(0, 1, (badge_cols() + 31) / 2, 6), true),
            (
                right,
                rect(
                    (badge_cols() + 31) / 2,
                    1,
                    badge_cols() + 31 - (badge_cols() + 31) / 2,
                    6,
                ),
                true,
            ),
        ],
        Some(left),
        LockMode::Normal,
        Size {
            cols: badge_cols() + 31,
            rows: 8,
        },
    );
    let theme = Theme {
        ramp_start: (0xff, 0x00, 0x00),
        ramp_end: (0x00, 0x00, 0xff),
        border_focused: Color::Rgb(0xff, 0x88, 0x00),
        border_unfocused: Color::Rgb(0x11, 0x22, 0x33),
        ..Theme::default()
    };
    let cols = badge_cols() + 31;
    let buf = render_with(&snap, &theme, cols, 8);

    // Borders take the theme's border colors.
    assert_eq!(buf[(0, 1)].fg, Color::Rgb(0xff, 0x88, 0x00));
    assert_eq!(buf[(cols / 2, 1)].fg, Color::Rgb(0x11, 0x22, 0x33));
    // The session name takes the custom ramp's start end, the mode tag its
    // other end.
    assert_eq!(buf[(1, 0)].fg, Color::Rgb(0xff, 0x00, 0x00));
    assert_eq!(buf[(cols - 2, 0)].fg, Color::Rgb(0x00, 0x00, 0xff));
    // The first tab's ribbon sits on the custom ramp's start stop.
    let tab_x = (0..cols)
        .find(|&x| buf[(x, 0)].symbol() == "#")
        .expect("tab marker drawn");
    assert_eq!(buf[(tab_x, 0)].fg, Color::Rgb(0xff, 0x00, 0x00));
}

#[test]
fn overlapping_panes_draw_in_layout_order_last_wins() {
    // The layout solver normally tiles panes without overlap; this snapshot
    // forces two visible pane rects to overlap to pin down what the renderer
    // actually does with that input: later slots in `layout_solved` paint
    // over earlier ones, for both the border and the pane content.
    let a = PaneId::new();
    let b = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[(a, rect(0, 1, 20, 6), true), (b, rect(15, 1, 20, 6), true)],
        Some(a),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    let mut grid_a = Grid::blank(4, 18, TermStyle::default());
    *grid_a.cell_mut(0, 0).unwrap() = Cell::new('Z', 1, TermStyle::default());
    *grid_a.cell_mut(0, 15).unwrap() = Cell::new('X', 1, TermStyle::default());
    snap.panes[0].grid_view = Some(GridView {
        grid: Arc::new(grid_a),
        view_offset: 0,
    });
    let mut grid_b = Grid::blank(4, 18, TermStyle::default());
    *grid_b.cell_mut(0, 0).unwrap() = Cell::new('Y', 1, TermStyle::default());
    *grid_b.cell_mut(0, 17).unwrap() = Cell::new('W', 1, TermStyle::default());
    snap.panes[1].grid_view = Some(GridView {
        grid: Arc::new(grid_b),
        view_offset: 0,
    });
    let buf = render(&snap, 40, 8);

    // A's own corner (outside B's rect) survives untouched...
    assert_eq!(buf[(0, 1)].symbol(), "┌");
    assert_eq!(buf[(0, 1)].fg, Color::Rgb(0x00, 0xaf, 0xd7));
    assert_eq!(buf[(0, 1)].modifier, Modifier::BOLD);
    // ...but B (drawn second) overwrites A's right border where they overlap
    // (A's right border sits at x=19, inside B's top-border row): the glyph
    // and color are B's. The BOLD modifier is untouched by B's style (a
    // ratatui `Style` with no `add_modifier` patches, not replaces, so it
    // does not clear a modifier a previous style already set).
    assert_eq!(buf[(19, 1)].symbol(), "─");
    assert_eq!(buf[(19, 1)].fg, Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(buf[(19, 1)].modifier, Modifier::BOLD);

    // Content: each pane's own, non-overlapping cell keeps its own glyph...
    assert_eq!(buf[(1, 2)].symbol(), "Z");
    assert_eq!(buf[(33, 2)].symbol(), "W");
    // ...but in the overlap region (screen x=16..19) B's cell wins over A's.
    assert_eq!(buf[(16, 2)].symbol(), "Y");
}

#[test]
fn pane_title_skipped_when_box_is_four_wide() {
    // `rect.width <= 4` guards the `rect.width - 4` subtraction the title
    // clip uses; at exactly 4 there is no room for the ` title ` padding.
    let pane = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 4, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 10, rows: 8 },
    );
    snap.panes[0].title = Some("editor".to_string());
    let buf = render(&snap, 10, 8);

    // Title drawing never ran: the top border keeps a plain dash at column 2,
    // the column a title starts on.
    assert_eq!(buf[(2, 1)].symbol(), "─");
}

#[test]
fn pane_title_drawn_when_box_is_five_wide() {
    // One cell wider crosses the `<= 4` threshold: the title's leading space
    // takes column 2, in place of the dash.
    let pane = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 5, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 10, rows: 8 },
    );
    snap.panes[0].title = Some("editor".to_string());
    let buf = render(&snap, 10, 8);

    assert_eq!(buf[(2, 1)].symbol(), " ");
}

#[test]
fn a_title_wider_than_the_box_is_clipped_short_of_the_corners() {
    // A 10-wide box gives the title six cells: it starts two cells in and
    // stops four short of the box width, so ` abcdefghij ` shows as ` abcde`
    // and the two corner glyphs plus the dash before them survive.
    let pane = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 10, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 10, rows: 8 },
    );
    snap.panes[0].title = Some("abcdefghij".to_string());
    let buf = render(&snap, 10, 8);

    assert_eq!(row_text(&buf, 1), "┌─ abcde─┐");
}

#[test]
fn an_empty_pane_title_leaves_the_top_border_whole() {
    let pane = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    snap.panes[0].title = Some(String::new());
    let buf = render(&snap, 40, 8);

    assert_eq!(row_text(&buf, 1), format!("┌{}┐", "─".repeat(38)));
}

#[test]
fn orphan_pane_slot_with_no_matching_snapshot_draws_border_only() {
    // A slot can reference a pane id absent from `panes` (e.g. the pane
    // exited and was pruned between layout solve and snapshot build).
    // `draw_panes` never looks up the pane for its box, so the border still
    // draws; `draw_pane_contents` must skip content without panicking.
    let pane = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    snap.panes.clear();
    let buf = render(&snap, 40, 8);

    assert_eq!(row_text(&buf, 1), format!("┌{}┐", "─".repeat(38)));
    assert_eq!(row_text(&buf, 2), format!("│{}│", " ".repeat(38)));
}

#[test]
fn cursor_position_with_focused_pane_absent_from_layout_returns_none() {
    // The client's focused_pane id still has a PaneSnapshot in `panes` (so
    // `find_pane` alone would not catch a missing-slot bug), but its slot was
    // dropped from `layout_solved` this frame (a stale handle after the
    // layout re-solved without it): the layout lookup itself finds nothing.
    let visible = PaneId::new();
    let orphaned = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[
            (visible, rect(0, 1, 20, 6), true),
            (orphaned, rect(20, 1, 20, 6), true),
        ],
        Some(orphaned),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    snap.session
        .active_tab
        .layout_solved
        .retain(|slot| slot.pane_id != orphaned);
    // The orphaned pane still carries a live, visible-cursor grid, so a
    // lookup bug that silently grabs a different slot would still produce a
    // `Some` position (using the wrong slot's rect) rather than `None` by
    // coincidence of some other, unrelated guard.
    snap.panes[1].grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(4, 18, TermStyle::default())),
        view_offset: 0,
    });
    assert_eq!(legacy_cursor(&snap), None);
}

#[test]
fn a_visible_slot_with_no_content_rect_places_no_cursor() {
    // `visible` and `inner_rect` are separate fields on the wire. A slot that
    // says it is visible but carries no content rect has nowhere to put the
    // cursor, so none is placed.
    let mut snap = content_snap(
        Grid::blank(4, 38, TermStyle::default()),
        rect(0, 1, 40, 6),
        false,
        Size { cols: 40, rows: 8 },
    );
    snap.session.active_tab.layout_solved[0].inner_rect = None;
    assert!(snap.session.active_tab.layout_solved[0].visible);
    assert!(snap.panes[0].cursor.visible);
    assert_eq!(legacy_cursor(&snap), None);
}

#[test]
fn a_slot_whose_pane_snapshot_is_gone_places_no_cursor() {
    // The focused pane still has a visible slot with a content rect, but the
    // frame carries no pane snapshot for it: nothing says where the cursor is,
    // so none is placed.
    let mut snap = content_snap(
        Grid::blank(4, 38, TermStyle::default()),
        rect(0, 1, 40, 6),
        false,
        Size { cols: 40, rows: 8 },
    );
    snap.panes.clear();
    // Every guard before the pane lookup passes: the slot is there, visible,
    // and carries a content rect.
    assert!(snap.session.active_tab.layout_solved[0].visible);
    assert_eq!(
        snap.session.active_tab.layout_solved[0].inner_rect,
        Some(rect(1, 2, 38, 4))
    );
    assert_eq!(legacy_cursor(&snap), None);
}

#[test]
fn cursor_style_is_none_when_the_focused_pane_has_no_snapshot() {
    // The focused id names a pane the frame carries no content for: nothing
    // speaks for the cursor, so the outer terminal keeps the style it has.
    let mut snap = content_snap(
        Grid::blank(4, 38, TermStyle::default()),
        rect(0, 1, 40, 6),
        false,
        Size { cols: 40, rows: 8 },
    );
    snap.panes.clear();
    assert_eq!(cursor_style(&snap), None);
}

#[test]
fn one_by_one_viewport_draws_without_panicking() {
    // The smallest possible non-zero area: content_rect and the tabline draw
    // must degrade gracefully rather than underflow or panic. The mode tag
    // saturates the whole 1-cell row, leaving no room for the tab strip, so the
    // single cell falls to the mode block's clipped leading cell — a space.
    let pane = PaneId::new();
    let snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 1, rows: 1 },
    );
    let buf = render(&snap, 1, 1);

    assert_eq!(buf[(0, 0)].symbol(), " ");
}

// ============================================================================
// Drawing the highlight
// ============================================================================

/// `content_snap` with `rows` highlighted.
fn highlighted_snap(grid: Grid, spans: Vec<(u16, u16, u16)>) -> RenderSnapshot {
    let mut snap = content_snap(grid, rect(0, 1, 40, 6), false, Size { cols: 40, rows: 8 });
    snap.panes[0].selection = Some(SelectionSpans { rows: spans });
    snap
}

/// A grid whose row 0 reads `abcdef`.
fn abcdef_grid() -> Grid {
    let mut grid = Grid::blank(4, 38, TermStyle::default());
    for (col, ch) in "abcdef".chars().enumerate() {
        *grid.cell_mut(0, col as u16).unwrap() = Cell::new(ch, 1, TermStyle::default());
    }
    grid
}

#[test]
fn highlighted_cells_are_drawn_in_reverse_and_the_rest_are_not() {
    // Highlight columns 1..=3 of row 0: `bcd` of `abcdef`.
    let snap = highlighted_snap(abcdef_grid(), vec![(0, 1, 3)]);
    let buf = render(&snap, 40, 8);

    // Content origin is (1, 2): the one-cell border offsets it.
    assert_eq!(
        buf[(1, 2)].modifier,
        Modifier::empty(),
        "`a` is outside the highlight"
    );
    for x in 2..=4 {
        assert_eq!(
            buf[(x, 2)].modifier,
            Modifier::REVERSED,
            "column {x} is highlighted"
        );
    }
    assert_eq!(
        buf[(5, 2)].modifier,
        Modifier::empty(),
        "`e` is past the highlight"
    );
}

#[test]
fn a_pane_with_no_highlight_draws_nothing_in_reverse() {
    let snap = content_snap(
        abcdef_grid(),
        rect(0, 1, 40, 6),
        false,
        Size { cols: 40, rows: 8 },
    );
    let buf = render(&snap, 40, 8);

    for x in 1..=6 {
        assert_eq!(buf[(x, 2)].modifier, Modifier::empty(), "column {x}");
    }
}

#[test]
fn a_highlight_span_that_ends_before_it_starts_highlights_nothing() {
    // A span is `(row, first column, last column)`, both inclusive. `(0, 3, 1)`
    // names an empty range, and no cell on the row is drawn in reverse.
    let snap = highlighted_snap(abcdef_grid(), vec![(0, 3, 1)]);
    let buf = render(&snap, 40, 8);

    for x in 1..=6 {
        assert_eq!(buf[(x, 2)].modifier, Modifier::empty(), "column {x}");
    }
}

#[test]
fn only_the_highlighted_row_is_reversed() {
    let mut grid = abcdef_grid();
    for (col, ch) in "ghijkl".chars().enumerate() {
        *grid.cell_mut(1, col as u16).unwrap() = Cell::new(ch, 1, TermStyle::default());
    }
    // Row 1 is highlighted; row 0 is not.
    let snap = highlighted_snap(grid, vec![(1, 0, 2)]);
    let buf = render(&snap, 40, 8);

    assert_eq!(
        buf[(1, 2)].modifier,
        Modifier::empty(),
        "row 0 is untouched"
    );
    assert_eq!(
        buf[(1, 3)].modifier,
        Modifier::REVERSED,
        "row 1 is highlighted"
    );
}

#[test]
fn highlighting_a_cell_that_is_already_reverse_swaps_it_back() {
    // The highlight combines with the cell's own reverse by exclusive-or, so
    // highlighted reverse text still reads against its surroundings rather than
    // vanishing into them.
    let mut grid = Grid::blank(4, 38, TermStyle::default());
    let mut style = TermStyle::default();
    style.set_reverse(true);
    *grid.cell_mut(0, 0).unwrap() = Cell::new('a', 1, style);
    let snap = highlighted_snap(grid, vec![(0, 0, 0)]);
    let buf = render(&snap, 40, 8);

    assert_eq!(
        buf[(1, 2)].modifier,
        Modifier::empty(),
        "already-reverse text highlighted swaps back to normal"
    );
}

#[test]
fn a_highlight_under_screen_wide_reverse_video_swaps_back() {
    // DECSCNM reverses the whole screen; a highlight on top of it swaps those
    // cells back, by the same exclusive-or.
    let mut snap = content_snap(
        abcdef_grid(),
        rect(0, 1, 40, 6),
        true,
        Size { cols: 40, rows: 8 },
    );
    snap.panes[0].selection = Some(SelectionSpans {
        rows: vec![(0, 0, 1)],
    });
    let buf = render(&snap, 40, 8);

    assert_eq!(
        buf[(1, 2)].modifier,
        Modifier::empty(),
        "highlighted, so swapped back out of the screen-wide reverse"
    );
    assert_eq!(
        buf[(3, 2)].modifier,
        Modifier::REVERSED,
        "not highlighted, so still reverse from DECSCNM"
    );
}

#[test]
fn a_highlight_span_wider_than_the_grid_draws_only_real_cells() {
    // A span naming columns past the grid's width cannot paint outside it.
    let snap = highlighted_snap(abcdef_grid(), vec![(0, 0, 200)]);
    let buf = render(&snap, 40, 8);

    // The pane's content is 38 wide from x=1, so x=38 is its last column.
    assert_eq!(buf[(38, 2)].modifier, Modifier::REVERSED);
    // The border column past it keeps the focused border's own bold style.
    assert_eq!(buf[(39, 2)].modifier, Modifier::BOLD);
    assert_eq!(buf[(39, 2)].symbol(), "│");
}

#[test]
fn mode_indicator_joins_active_mode_labels() {
    let pane = PaneId::new();
    let mut snap = build(
        "s",
        &[("t", true)],
        &[(pane, rect(0, 1, 20, 4), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 20, rows: 6 },
    );

    // Plain mode with the mouse ungrabbed reads BASE.
    assert_eq!(
        mode_tags(snap.client.lock_mode, snap.client.mouse_select, None),
        "BASE"
    );

    // Mouse-select alone reads SELECT.
    snap.client.mouse_select = true;
    assert_eq!(
        mode_tags(snap.client.lock_mode, snap.client.mouse_select, None),
        "SELECT"
    );

    // Locked and grabbing reads both, joined by ` · `.
    snap.client.lock_mode = LockMode::Locked;
    assert_eq!(
        mode_tags(snap.client.lock_mode, snap.client.mouse_select, None),
        "LOCK · SELECT"
    );

    // Locked alone reads LOCK.
    snap.client.mouse_select = false;
    assert_eq!(
        mode_tags(snap.client.lock_mode, snap.client.mouse_select, None),
        "LOCK"
    );
}

#[test]
fn the_mode_indicator_names_every_lock_mode() {
    assert_eq!(mode_tags(LockMode::Normal, false, None), "BASE");
    assert_eq!(mode_tags(LockMode::Locked, false, None), "LOCK");
    assert_eq!(mode_tags(LockMode::Resize, false, None), "RESIZE");
    assert_eq!(mode_tags(LockMode::PaneMode, false, None), "PANE");
    assert_eq!(mode_tags(LockMode::TabMode, false, None), "TAB");
    assert_eq!(mode_tags(LockMode::ScrollMode, false, None), "SCROLL");

    // Every non-plain mode joins the mouse-select tag the same way.
    assert_eq!(mode_tags(LockMode::Resize, true, None), "RESIZE · SELECT");
    assert_eq!(mode_tags(LockMode::PaneMode, true, None), "PANE · SELECT");
    assert_eq!(mode_tags(LockMode::TabMode, true, None), "TAB · SELECT");
    assert_eq!(
        mode_tags(LockMode::ScrollMode, true, None),
        "SCROLL · SELECT"
    );
}

#[test]
fn mode_indicator_puts_the_reconnecting_tag_first_and_replaces_base() {
    let pane = PaneId::new();
    let mut snap = build(
        "s",
        &[("t", true)],
        &[(pane, rect(0, 1, 20, 4), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 20, rows: 6 },
    );

    let dialing = Some(Reconnecting {
        attempt: 3,
        retry_in_seconds: 8,
    });

    // A reconnecting client in plain mode reads the link tag, never BASE, and
    // the tag carries the dial it waits for and the seconds left before it.
    assert_eq!(
        mode_tags(snap.client.lock_mode, snap.client.mouse_select, dialing,),
        "RECONNECTING (attempt 3, retry in 8s)"
    );

    // Reconnecting while locked and grabbing puts the link tag ahead of both.
    snap.client.lock_mode = LockMode::Locked;
    snap.client.mouse_select = true;
    assert_eq!(
        mode_tags(snap.client.lock_mode, snap.client.mouse_select, dialing,),
        "RECONNECTING (attempt 3, retry in 8s) · LOCK · SELECT"
    );
}

#[test]
fn text_width_counts_display_cells_not_bytes_or_chars() {
    // Chrome text is placed in terminal cells, so measuring uses display
    // width. "漢字" is 2 chars and 6 bytes but occupies 4 cells; an emoji is
    // 1 char and 4 bytes but occupies 2; a combining mark adds none.
    assert_eq!(text_width("漢字"), 4);
    assert_eq!(text_width("🦀"), 2);
    assert_eq!(
        text_width("e\u{0301}"),
        1,
        "e + combining acute is one cell"
    );
    assert_eq!(text_width(""), 0);

    // Past `u16::MAX` cells the count is held there, which every width
    // comparison reads as wider than the row.
    let huge = "x".repeat(usize::from(u16::MAX) + 64);
    assert_eq!(text_width(&huge), u16::MAX);
}

#[test]
fn line_width_sums_span_display_cells_and_saturates() {
    // Spans add up in display cells, and styles never change the count.
    let line = Line::from(vec![
        Span::styled("漢字", Style::default().fg(Color::Red)),
        Span::raw("🦀"),
        Span::raw("e\u{0301}"),
    ]);
    assert_eq!(line_width(&line), 7);
    assert_eq!(line_width(&Line::from("")), 0);

    // Two spans that together pass `u16::MAX` cells are held at `u16::MAX`,
    // never wrapped to a small number that would read as fitting.
    let half = "x".repeat(usize::from(u16::MAX));
    let huge = Line::from(vec![Span::raw(half.clone()), Span::raw(half)]);
    assert_eq!(line_width(&huge), u16::MAX);
}
