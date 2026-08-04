//! Tests for [`wire_frame`]: a cell travels with its character, its combining
//! marks, its width and every style field; equal neighbouring cells fold into
//! one run; a row travels the grid's full width; a pane with no terminal
//! content sends no window; and the session, tab, slot, client and per-pane
//! scalars come across unchanged.

use std::sync::Arc;

use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_ipc::frame::FrameRun;
use koshi_layout::mode::LayoutMode;
use koshi_pane::pane::state::PaneKind;
use koshi_renderer::snapshot::{
    ClientSnapshot, CursorSnapshot, GridView, PaneSlot, PaneSnapshot, PluginUiSnapshot,
    RenderSnapshot, ScrollbackMeta, SelectionSpans, SessionSnapshot, TabMeta, TabSnapshot,
};

use super::*;

/// The style the one written cell carries: every field set away from its
/// default, so a field dropped on the way across shows up.
fn style() -> Style {
    let mut style = Style::default();
    style.set_fg(Color::Indexed(4));
    style.set_bg(Color::Rgb(10, 20, 30));
    style.set_underline_color(Some(Color::Indexed(9)));
    style.set_bold(true);
    style.set_italic(true);
    style.set_reverse(true);
    style.set_faint(true);
    style.set_blink(true);
    style.set_conceal(true);
    style.set_strike(true);
    style.set_overline(true);
    style.set_underline(UnderlineStyle::Curly);
    style
}

/// A 1×3 grid: a styled `e` carrying a combining acute accent (U+0301), then
/// two blank cells in the default style.
fn grid() -> Grid {
    let mut grid = Grid::blank(1, 3, Style::default());
    let cell = grid.cell_mut(0, 0).expect("the grid has a cell at (0, 0)");
    *cell = Cell::new('e', 1, style());
    cell.push_combining('\u{301}');
    grid
}

/// The cell [`grid`] writes at (0, 0), as it travels.
fn wire_written_cell() -> FrameCell {
    FrameCell {
        ch: 'e',
        combining: vec!['\u{301}'],
        width: 1,
        style: FrameStyle {
            fg: FrameColor::Indexed(4),
            bg: FrameColor::Rgb(10, 20, 30),
            underline_color: Some(FrameColor::Indexed(9)),
            attrs: FrameAttrs {
                bold: true,
                italic: true,
                reverse: true,
                faint: true,
                blink: true,
                conceal: true,
                strike: true,
                overline: true,
                underline: FrameUnderline::Curly,
            },
        },
    }
}

/// The two blank cells [`grid`] leaves at (0, 1) and (0, 2), as they travel.
fn wire_blank_cell() -> FrameCell {
    FrameCell {
        ch: ' ',
        combining: Vec::new(),
        width: 1,
        style: FrameStyle {
            fg: FrameColor::Default,
            bg: FrameColor::Default,
            underline_color: None,
            attrs: FrameAttrs {
                bold: false,
                italic: false,
                reverse: false,
                faint: false,
                blink: false,
                conceal: false,
                strike: false,
                overline: false,
                underline: FrameUnderline::None,
            },
        },
    }
}

/// One pane holding [`grid`], scrolled 7 lines back, reporting any-motion mouse
/// tracking and a truncated scrollback of 500 retained lines.
fn pane(id: PaneId) -> PaneSnapshot {
    PaneSnapshot {
        id,
        title: Some(String::from("~/work")),
        cursor: CursorSnapshot {
            row: 0,
            col: 2,
            visible: true,
            blink: true,
            shape: Some(CursorShape::Bar),
        },
        grid_view: Some(GridView {
            grid: Arc::new(grid()),
            view_offset: 7,
        }),
        reverse_video: true,
        mouse_tracking: MouseTracking::AnyMotion,
        alt_scroll: true,
        on_alt_screen: false,
        view_top_row: 493,
        selection: Some(SelectionSpans {
            rows: vec![(0, 1, 2)],
        }),
        has_selection: true,
        scrollback: ScrollbackMeta {
            truncated: true,
            retained_lines: 500,
        },
    }
}

/// A pane with no terminal content: no window, a hidden cursor, nothing
/// highlighted.
fn empty_pane(id: PaneId) -> PaneSnapshot {
    PaneSnapshot {
        id,
        title: None,
        cursor: CursorSnapshot {
            row: 0,
            col: 0,
            visible: false,
            blink: false,
            shape: None,
        },
        grid_view: None,
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

/// A frame with one tab, two slots, and the two panes above.
fn snapshot(content: PaneId, empty: PaneId) -> RenderSnapshot {
    let tab = TabId::new();
    let other_tab = TabId::new();
    let client = ClientId::new();
    RenderSnapshot {
        session: SessionSnapshot {
            id: SessionId::new(),
            name: String::from("session"),
            active_tab: TabSnapshot {
                id: tab,
                name: String::from("tab"),
                layout_solved: vec![
                    PaneSlot {
                        pane_id: content,
                        rect: Rect {
                            origin: Point { x: 0, y: 0 },
                            size: Size { cols: 5, rows: 3 },
                        },
                        inner_rect: Some(Rect {
                            origin: Point { x: 1, y: 1 },
                            size: Size { cols: 3, rows: 1 },
                        }),
                        kind: PaneKind::Terminal,
                        visible: true,
                        suppressed: false,
                        dead: false,
                    },
                    PaneSlot {
                        pane_id: empty,
                        rect: Rect {
                            origin: Point { x: 5, y: 0 },
                            size: Size { cols: 5, rows: 3 },
                        },
                        inner_rect: None,
                        kind: PaneKind::Terminal,
                        visible: false,
                        suppressed: true,
                        dead: true,
                    },
                ],
                effective_size: Size { cols: 10, rows: 3 },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Fullscreen { focused: content },
                all_suppressed: false,
            },
            tabs_metadata: vec![
                TabMeta {
                    id: tab,
                    name: String::from("tab"),
                    index: 0,
                    active: true,
                },
                TabMeta {
                    id: other_tab,
                    name: String::from("other"),
                    index: 1,
                    active: false,
                },
            ],
        },
        panes: vec![pane(content), empty_pane(empty)],
        client: ClientSnapshot {
            id: client,
            viewport: Size { cols: 20, rows: 6 },
            active_tab: tab,
            focused_pane: Some(content),
            lock_mode: LockMode::Locked,
            mouse_select: true,
        },
        plugin_ui: PluginUiSnapshot::default(),
    }
}

#[test]
fn a_cell_travels_with_its_character_marks_width_and_every_style_field() {
    let content = PaneId::new();
    let frame = wire_frame(&snapshot(content, PaneId::new()));

    let window = frame.panes[0]
        .window
        .as_ref()
        .expect("the pane carries a grid");
    assert_eq!(window.rows[0].cells()[0], wire_written_cell());
}

#[test]
fn equal_neighbouring_cells_fold_into_one_run() {
    let content = PaneId::new();
    let frame = wire_frame(&snapshot(content, PaneId::new()));

    // "e" then two default blanks: one run of 1, then one run of 2.
    let window = frame.panes[0]
        .window
        .as_ref()
        .expect("the pane carries a grid");
    assert_eq!(window.cols, 3);
    assert_eq!(window.rows.len(), 1);
    assert_eq!(
        window.rows[0].runs,
        vec![
            FrameRun {
                count: 1,
                cell: wire_written_cell(),
            },
            FrameRun {
                count: 2,
                cell: wire_blank_cell(),
            },
        ]
    );
}

#[test]
fn a_wire_row_is_as_wide_as_the_grid() {
    let grid = grid();
    let (_, cols) = grid.dimensions();

    let cells = wire_row(&grid, 0, cols).cells();

    assert_eq!(cells.len(), cols as usize);
    assert_eq!(
        cells,
        vec![wire_written_cell(), wire_blank_cell(), wire_blank_cell()]
    );
}

#[test]
fn a_pane_with_no_grid_travels_with_no_window() {
    let empty = PaneId::new();
    let frame = wire_frame(&snapshot(PaneId::new(), empty));

    assert_eq!(frame.panes[1].id, empty);
    assert_eq!(frame.panes[1].window, None);
}

#[test]
fn the_view_offset_mouse_mode_and_scrollback_scalars_come_through_unchanged() {
    let content = PaneId::new();
    let frame = wire_frame(&snapshot(content, PaneId::new()));

    let pane = &frame.panes[0];
    assert_eq!(
        pane.window
            .as_ref()
            .expect("the pane carries a grid")
            .view_offset,
        7
    );
    assert_eq!(pane.mouse_tracking, MouseTracking::AnyMotion);
    assert!(pane.scrollback.truncated);
    assert_eq!(pane.scrollback.retained_lines, 500);
    assert_eq!(pane.view_top_row, 493);
    assert_eq!(pane.title, Some(String::from("~/work")));
    assert!(pane.reverse_video);
    assert!(pane.alt_scroll);
    assert!(!pane.on_alt_screen);
    assert!(pane.has_selection);
    assert_eq!(
        pane.selection,
        Some(FrameSelection {
            rows: vec![(0, 1, 2)],
        })
    );
    assert_eq!(
        pane.cursor,
        FrameCursor {
            row: 0,
            col: 2,
            visible: true,
            blink: true,
            shape: Some(FrameCursorShape::Bar),
        }
    );
}

#[test]
fn the_session_tab_slot_and_client_fields_copy_straight_across() {
    let content = PaneId::new();
    let empty = PaneId::new();
    let snapshot = snapshot(content, empty);

    let frame = wire_frame(&snapshot);

    assert_eq!(frame.session.id, snapshot.session.id);
    assert_eq!(frame.session.name, String::from("session"));
    assert_eq!(frame.session.active_tab.id, snapshot.session.active_tab.id);
    assert_eq!(frame.session.active_tab.name, String::from("tab"));
    assert_eq!(
        frame.session.active_tab.effective_size,
        Size { cols: 10, rows: 3 }
    );
    assert_eq!(frame.session.active_tab.stack_headers, Vec::new());
    assert_eq!(
        frame.session.active_tab.layout_mode,
        LayoutMode::Fullscreen { focused: content }
    );
    assert!(!frame.session.active_tab.all_suppressed);
    assert_eq!(
        frame.session.active_tab.slots,
        vec![
            FrameSlot {
                pane_id: content,
                rect: Rect {
                    origin: Point { x: 0, y: 0 },
                    size: Size { cols: 5, rows: 3 },
                },
                inner_rect: Some(Rect {
                    origin: Point { x: 1, y: 1 },
                    size: Size { cols: 3, rows: 1 },
                }),
                kind: PaneKind::Terminal,
                visible: true,
                suppressed: false,
                dead: false,
            },
            FrameSlot {
                pane_id: empty,
                rect: Rect {
                    origin: Point { x: 5, y: 0 },
                    size: Size { cols: 5, rows: 3 },
                },
                inner_rect: None,
                kind: PaneKind::Terminal,
                visible: false,
                suppressed: true,
                dead: true,
            },
        ]
    );
    assert_eq!(
        frame.session.tabs,
        vec![
            FrameTabMeta {
                id: snapshot.session.active_tab.id,
                name: String::from("tab"),
                index: 0,
                active: true,
            },
            FrameTabMeta {
                id: snapshot.session.tabs_metadata[1].id,
                name: String::from("other"),
                index: 1,
                active: false,
            },
        ]
    );
    assert_eq!(
        frame.client,
        FrameClient {
            id: snapshot.client.id,
            viewport: Size { cols: 20, rows: 6 },
            active_tab: snapshot.session.active_tab.id,
            focused_pane: Some(content),
            lock_mode: LockMode::Locked,
            mouse_select: true,
        }
    );
}
