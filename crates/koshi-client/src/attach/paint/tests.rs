//! Tests for [`to_snapshot`]: a frame that travels and is read back is the
//! frame that was sent — every style field, a wide glyph and its continuation
//! half, a combining mark, both cursor states, the scroll offset, the mouse
//! mode, a pane with no grid at all, and the highlight rows.

use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_layout::mode::LayoutMode;
use koshi_renderer::snapshot::PaneKind;
use koshi_runtime::runtime::frame::wire_frame;

use super::*;

/// Every style field set away from its default, so a field lost on the way
/// there or back shows up.
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

/// A 2×4 grid whose first row holds, left to right: a styled `e` carrying a
/// combining acute accent (U+0301), the wide glyph `漢` at width 2, the blank
/// continuation half it occupies at width 0, and one default blank. The second
/// row is all default blanks.
fn grid() -> Grid {
    let mut grid = Grid::blank(2, 4, Style::default());
    let accented = grid.cell_mut(0, 0).expect("the grid has a cell at (0, 0)");
    *accented = Cell::new('e', 1, style());
    accented.push_combining('\u{301}');
    *grid.cell_mut(0, 1).expect("the grid has a cell at (0, 1)") = Cell::new('漢', 2, style());
    *grid.cell_mut(0, 2).expect("the grid has a cell at (0, 2)") = Cell::new(' ', 0, style());
    grid
}

/// The pane holding [`grid`]: scrolled 7 lines back, reporting any-motion mouse
/// tracking, showing a shaped blinking cursor, and highlighting the first row's
/// columns 1 to 2.
fn content_pane(id: PaneId) -> PaneSnapshot {
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

/// A pane with no terminal content: no grid, a hidden and unshaped cursor,
/// nothing highlighted.
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

/// A frame with one tab, two slots, and the two panes handed in.
fn snapshot(panes: Vec<PaneSnapshot>) -> RenderSnapshot {
    let content = panes[0].id;
    let empty = panes[1].id;
    let tab = TabId::new();
    let other_tab = TabId::new();
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
                            size: Size { cols: 6, rows: 4 },
                        },
                        inner_rect: Some(Rect {
                            origin: Point { x: 1, y: 1 },
                            size: Size { cols: 4, rows: 2 },
                        }),
                        kind: PaneKind::Terminal,
                        visible: true,
                        suppressed: false,
                        dead: false,
                    },
                    PaneSlot {
                        pane_id: empty,
                        rect: Rect {
                            origin: Point { x: 6, y: 0 },
                            size: Size { cols: 6, rows: 4 },
                        },
                        inner_rect: None,
                        kind: PaneKind::Terminal,
                        visible: false,
                        suppressed: true,
                        dead: true,
                    },
                ],
                effective_size: Size { cols: 12, rows: 4 },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Fullscreen { focused: content },
                all_suppressed: false,
                gap: 0,
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
        panes,
        client: ClientSnapshot {
            id: ClientId::new(),
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
fn a_frame_that_travels_and_is_read_back_is_the_frame_that_was_sent() {
    let sent = snapshot(vec![content_pane(PaneId::new()), empty_pane(PaneId::new())]);
    assert_eq!(to_snapshot(&wire_frame(&sent)), sent);
}

#[test]
fn the_tabs_gap_arrives_with_the_frame() {
    let sent = snapshot(vec![content_pane(PaneId::new()), empty_pane(PaneId::new())]);
    let mut wire = wire_frame(&sent);
    wire.session.active_tab.gap = 2;

    assert_eq!(to_snapshot(&wire).session.active_tab.gap, 2);
}

#[test]
fn a_soft_wrapped_row_arrives_still_soft_wrapped() {
    // A shell printing a line longer than the pane is wide leaves the first row
    // soft-wrapped and the last row hard-ended. Without the row-end state on the
    // wire every row reads as hard, and copying the text out breaks the line at
    // the wrap.
    let mut grid = Grid::from_rows(
        vec![
            vec![Cell::new('a', 1, Style::default())],
            vec![Cell::new('b', 1, Style::default())],
            vec![Cell::new('c', 1, Style::default())],
        ],
        1,
        Style::default(),
    );
    grid.set_row_end(0, RowEnd::Soft);
    grid.set_row_end(1, RowEnd::SoftWide);

    let mut pane = content_pane(PaneId::new());
    pane.grid_view = Some(GridView {
        grid: Arc::new(grid),
        view_offset: 0,
    });
    let sent = snapshot(vec![pane, empty_pane(PaneId::new())]);

    let read_back = to_snapshot(&wire_frame(&sent));

    let arrived = read_back.panes[0]
        .grid_view
        .as_ref()
        .expect("the pane carries a grid");
    assert_eq!(arrived.grid.row_end(0), RowEnd::Soft);
    assert_eq!(arrived.grid.row_end(1), RowEnd::SoftWide);
    assert_eq!(arrived.grid.row_end(2), RowEnd::Hard);
    assert_eq!(read_back, sent);
}

#[test]
fn a_highlight_scrolled_entirely_off_screen_arrives_as_no_highlight() {
    // The session resolves the highlight to the rows this frame shows before
    // it builds the pane, so a highlight above every visible row leaves
    // `selection` empty while `has_selection` still reports it exists.
    let mut off_screen = content_pane(PaneId::new());
    off_screen.selection = None;
    off_screen.has_selection = true;
    let sent = snapshot(vec![off_screen, empty_pane(PaneId::new())]);

    let read_back = to_snapshot(&wire_frame(&sent));

    assert_eq!(read_back.panes[0].selection, None);
    assert!(read_back.panes[0].has_selection);
    assert_eq!(read_back, sent);
}

/// The session decides this viewer's lock mode and whether mouse-select is on,
/// so a painted frame is where both are read from, and the same frame cut down
/// to [`MouseFrame`] is what the next mouse event is placed against.
#[test]
fn adopting_a_frame_takes_the_viewer_state_the_session_decided() {
    let (_events_tx, events_rx) = std::sync::mpsc::sync_channel(8);
    let mut client = crate::Client::new(
        ClientId::new(),
        Size { cols: 20, rows: 6 },
        events_rx,
        koshi_observability::cleanup::TerminalCleanupGuard::new(),
    );
    // A fresh viewer is unlocked with mouse-select off. The frame carries both
    // the other way.
    assert_eq!(client.lock_mode(), LockMode::Normal);
    assert!(!client.mouse_select());

    let sent = snapshot(vec![content_pane(PaneId::new()), empty_pane(PaneId::new())]);
    let active_tab = sent.client.active_tab;
    let focused_pane = sent.client.focused_pane;

    super::super::adopt_frame(&mut client, &sent);

    assert_eq!(client.lock_mode(), LockMode::Locked);
    assert!(client.mouse_select());

    let frame = koshi_renderer::snapshot::MouseFrame::from(sent);
    assert_eq!(frame.client.active_tab, active_tab);
    assert_eq!(frame.client.focused_pane, focused_pane);
}

/// Each underline style has its own wire spelling, so a variant read back as
/// another one shows up here. The cell also carries a default foreground, which
/// is the one color whose wire spelling names no value.
#[test]
fn every_underline_style_reads_back_as_itself() {
    for underline in [
        UnderlineStyle::None,
        UnderlineStyle::Single,
        UnderlineStyle::Double,
        UnderlineStyle::Curly,
        UnderlineStyle::Dotted,
        UnderlineStyle::Dashed,
    ] {
        let mut cell_style = Style::default();
        cell_style.set_fg(Color::Default);
        cell_style.set_underline(underline);
        let grid = Grid::from_rows(
            vec![vec![Cell::new('x', 1, cell_style)]],
            1,
            Style::default(),
        );
        let mut pane = content_pane(PaneId::new());
        pane.grid_view = Some(GridView {
            grid: Arc::new(grid),
            view_offset: 0,
        });
        let sent = snapshot(vec![pane, empty_pane(PaneId::new())]);

        let read_back = to_snapshot(&wire_frame(&sent));

        let arrived = read_back.panes[0]
            .grid_view
            .as_ref()
            .expect("the pane carries a grid");
        let cell = arrived
            .grid
            .cell(0, 0)
            .expect("the grid has a cell at (0, 0)");
        assert_eq!(cell.style().attrs().underline(), underline);
        assert_eq!(cell.style().fg(), Color::Default);
        assert_eq!(read_back, sent);
    }
}

/// Each cursor shape has its own wire spelling, and a pane that named none
/// reads back naming none.
#[test]
fn every_cursor_shape_reads_back_as_itself() {
    for shape in [
        Some(CursorShape::Block),
        Some(CursorShape::Underline),
        Some(CursorShape::Bar),
        None,
    ] {
        let mut pane = content_pane(PaneId::new());
        pane.cursor.shape = shape;
        let sent = snapshot(vec![pane, empty_pane(PaneId::new())]);

        let read_back = to_snapshot(&wire_frame(&sent));

        assert_eq!(read_back.panes[0].cursor.shape, shape);
        assert_eq!(read_back, sent);
    }
}

/// A run stands for every cell it covers, so a blank 80-column row travels as
/// one run of 80 and rebuilds into 80 cells.
#[test]
fn a_blank_eighty_column_row_travels_as_one_run_and_rebuilds_eighty_cells() {
    let mut pane = content_pane(PaneId::new());
    pane.grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(1, 80, Style::default())),
        view_offset: 0,
    });
    let sent = snapshot(vec![pane, empty_pane(PaneId::new())]);

    let wire = wire_frame(&sent);
    let window = wire.panes[0]
        .window
        .as_ref()
        .expect("the pane carries a window");
    assert_eq!(window.cols, 80);
    assert_eq!(window.rows.len(), 1);
    assert_eq!(window.rows[0].runs.len(), 1);
    assert_eq!(window.rows[0].runs[0].count, 80);

    let read_back = to_snapshot(&wire);
    let arrived = read_back.panes[0]
        .grid_view
        .as_ref()
        .expect("the pane carries a grid");
    assert_eq!(arrived.grid.dimensions(), (1, 80));
    assert_eq!(arrived.grid.rows()[0].len(), 80);
    assert_eq!(read_back, sent);
}

/// A frame whose panes all closed carries no panes, and reads back carrying
/// none.
#[test]
fn a_frame_carrying_no_panes_reads_back_with_no_panes() {
    let mut sent = snapshot(vec![content_pane(PaneId::new()), empty_pane(PaneId::new())]);
    sent.panes = Vec::new();

    let read_back = to_snapshot(&wire_frame(&sent));

    assert_eq!(read_back.panes, Vec::<PaneSnapshot>::new());
    assert_eq!(read_back, sent);
}
