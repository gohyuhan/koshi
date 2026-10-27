//! Tests for drawing the notification center and the toast: the center's
//! floating chrome, title, close button, clear-all label and cards, the
//! dropped count, scrolling and its cut to the last row, the selected card's
//! style, the severity styles, the center while it slides, the toast in
//! place and while it slides, its cut text and header, and a wide glyph the
//! slide edge cuts.

use super::*;

use std::collections::VecDeque;

use ratatui::style::{Color, Modifier};

use koshi_config::types::ToastPosition;
use koshi_core::geometry::Size;
use koshi_core::notification::{NotificationEntry, NotificationId, NotificationSeverity};

use crate::floating_pane_tray::tests::build_tray_test_render_snapshot;
use crate::images::ImageRenderMode;
use crate::notification::tests::{build_center_entries, build_test_notification_entry};
use crate::notification::{NotificationToastContent, ToastTextRowCache};
use crate::render::tests::{build_viewport_area, format_rendered_cells};
use crate::render::{build_frame_geometry, render_frame};
use crate::snapshot::{
    CommittedRegions, KeymapHints, NotificationToast, RenderSnapshot, ViewerChrome,
};

/// An `80x24` viewport: its pane area is `(0, 1)` `80x22`, and the center and
/// the toast are 24 columns wide.
const EIGHTY_BY_TWENTY_FOUR_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// Paint `render_snapshot` with `viewer_chrome` and `notification_view` over
/// the compiled-in region solve of its viewport into a fresh buffer, in the
/// default theme.
fn render_notification_frame(
    render_snapshot: &RenderSnapshot,
    viewer_chrome: ViewerChrome,
    notification_view: NotificationView<'_>,
) -> Buffer {
    let viewport_area = build_viewport_area(render_snapshot);
    let mut render_buffer = Buffer::empty(viewport_area);
    render_frame(
        &build_frame_geometry(
            render_snapshot,
            &CommittedRegions::build_core(render_snapshot.client_snapshot.viewport_size, 0),
            viewer_chrome,
            viewport_area,
        ),
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ImageRenderMode::Placeholder,
        &[],
        None,
        None,
        notification_view,
        DrawnRows::Every,
        &mut render_buffer,
    );
    render_buffer
}

/// The viewer chrome of a client whose center stands `hidden_per_mille`
/// thousandths past the pane area's right edge, scrolled to
/// `scroll_row_offset`, with the card of `selected_notification_id`
/// selected.
fn build_center_viewer_chrome(
    scroll_row_offset: usize,
    selected_notification_id: Option<NotificationId>,
    hidden_per_mille: u16,
) -> ViewerChrome {
    ViewerChrome {
        notification_center: Some(NotificationCenter {
            scroll_row_offset,
            selected_notification_id,
            hidden_per_mille,
        }),
        ..ViewerChrome::default()
    }
}

/// The viewer chrome of a client whose center is open in place at
/// `scroll_row_offset`, with no card selected.
fn build_open_center_viewer_chrome(scroll_row_offset: usize) -> ViewerChrome {
    build_center_viewer_chrome(scroll_row_offset, None, 0)
}

/// The viewer chrome of a client that shows a toast of `text_row_count` text
/// rows at `top-right`, `hidden_per_mille` thousandths past its edge.
fn build_top_right_toast_viewer_chrome(text_row_count: u16, hidden_per_mille: u16) -> ViewerChrome {
    ViewerChrome {
        notification_toast: Some(NotificationToast {
            toast_position: ToastPosition::TopRight,
            text_row_count,
            hidden_per_mille,
        }),
        ..ViewerChrome::default()
    }
}

/// Two entries, oldest first: the `Warning` `koshi.kdl: unknown key`, then
/// the `Error` `pane …a1b2 killed by signal 9` merged from two posts.
fn build_two_test_entries() -> VecDeque<NotificationEntry> {
    VecDeque::from([
        build_test_notification_entry(
            0,
            NotificationSeverity::Warning,
            "koshi.kdl: unknown key",
            1,
        ),
        build_test_notification_entry(
            1,
            NotificationSeverity::Error,
            "pane …a1b2 killed by signal 9",
            2,
        ),
    ])
}

/// The cells of the 24-column box at the right of `render_buffer`, columns 56
/// to 79, on each row of `row_range`.
fn format_right_box_rows(render_buffer: &Buffer, row_range: std::ops::Range<u16>) -> Vec<String> {
    row_range
        .map(|row_index| format_rendered_cells(render_buffer, row_index, 56..80))
        .collect()
}

/// The foreground and the modifiers of the cell at `(column, row)` in
/// `render_buffer`.
fn get_cell_color_and_modifier(render_buffer: &Buffer, column: u16, row: u16) -> (Color, Modifier) {
    let cell = &render_buffer[(column, row)];
    (cell.fg, cell.modifier)
}

#[test]
fn an_open_center_draws_one_card_per_entry_newest_first_then_the_wrapped_dropped_count() {
    let notification_entries = build_two_test_entries();
    let render_buffer = render_notification_frame(
        &build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]),
        build_open_center_viewer_chrome(0),
        NotificationView {
            center_entries: &build_center_entries(&notification_entries),
            dropped_entry_count: 3,
            shown_toast: None,
        },
    );

    assert_eq!(
        format_right_box_rows(&render_buffer, 1..23),
        [
            "┌──────────────────────┐",
            "│ Notifications     ✕  │",
            "├──────────────────────┤",
            "│┌──────────────── ✕ ─┐│",
            "││ [Koshi] Error ×2   ││",
            "││ pane …a1b2 killed  ││",
            "││ by signal 9        ││",
            "│└────────────────────┘│",
            "│┌──────────────── ✕ ─┐│",
            "││ [Koshi] Warning    ││",
            "││ koshi.kdl: unknown ││",
            "││ key                ││",
            "│└────────────────────┘│",
            "│ 3 older              │",
            "│ notifications were   │",
            "│ dropped              │",
            "│                      │",
            "│                      │",
            "│                      │",
            "│                      │",
            "│      Clear all       │",
            "└──────────────────────┘",
        ]
    );
}

/// Eleven `Info` entries, oldest first, with the texts `entry 0` to
/// `entry 10`: eleven 4-row cards, 44 body rows in a 24-column center.
fn build_eleven_test_entries() -> VecDeque<NotificationEntry> {
    (0..11)
        .map(|test_entry_index| {
            build_test_notification_entry(
                test_entry_index,
                NotificationSeverity::Info,
                &format!("entry {test_entry_index}"),
                1,
            )
        })
        .collect()
}

/// `text` on one card row of a 24-column center, padded to the card's 18 text
/// columns.
fn format_card_text_row(text: &str) -> String {
    format!("││ {text:<18} ││")
}

#[test]
fn a_scrolled_center_draws_its_body_from_the_scrolled_row() {
    let notification_entries = build_eleven_test_entries();
    let render_buffer = render_notification_frame(
        &build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]),
        build_open_center_viewer_chrome(5),
        NotificationView {
            center_entries: &build_center_entries(&notification_entries),
            dropped_entry_count: 0,
            shown_toast: None,
        },
    );

    // Body row 5 is the header of `entry 9`, the second newest card.
    assert_eq!(
        format_right_box_rows(&render_buffer, 4..8),
        [
            format_card_text_row("[Koshi] Info"),
            format_card_text_row("entry 9"),
            "│└────────────────────┘│".to_string(),
            "│┌──────────────── ✕ ─┐│".to_string(),
        ]
    );
    // The last body row, row 20, draws body row 21: the header of `entry 5`.
    assert_eq!(
        format_right_box_rows(&render_buffer, 20..22),
        [
            format_card_text_row("[Koshi] Info"),
            "│      Clear all       │".to_string(),
        ]
    );
}

#[test]
fn a_center_scrolled_past_its_last_row_draws_its_last_row_on_its_last_body_row() {
    let notification_entries = build_eleven_test_entries();
    let notification_view = NotificationView {
        center_entries: &build_center_entries(&notification_entries),
        dropped_entry_count: 0,
        shown_toast: None,
    };
    let render_snapshot = build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]);

    // 44 body rows in 17 body rows: the last row is drawn from row 27.
    let at_the_last_row = render_notification_frame(
        &render_snapshot,
        build_open_center_viewer_chrome(27),
        notification_view,
    );
    let past_the_last_row = render_notification_frame(
        &render_snapshot,
        build_open_center_viewer_chrome(100),
        notification_view,
    );

    assert_eq!(
        format_right_box_rows(&at_the_last_row, 17..21),
        [
            "│┌──────────────── ✕ ─┐│".to_string(),
            format_card_text_row("[Koshi] Info"),
            format_card_text_row("entry 0"),
            "│└────────────────────┘│".to_string(),
        ]
    );
    assert_eq!(
        format_right_box_rows(&past_the_last_row, 1..23),
        format_right_box_rows(&at_the_last_row, 1..23)
    );
}

#[test]
fn an_empty_center_reads_no_notifications() {
    let notification_entries = VecDeque::new();
    let render_buffer = render_notification_frame(
        &build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]),
        build_open_center_viewer_chrome(0),
        NotificationView {
            center_entries: &build_center_entries(&notification_entries),
            dropped_entry_count: 0,
            shown_toast: None,
        },
    );

    assert_eq!(
        format_right_box_rows(&render_buffer, 4..6),
        ["│ No notifications     │", "│                      │"]
    );
}

#[test]
fn the_selected_card_takes_the_hover_border_style_and_the_center_the_focused_style() {
    let theme = Theme::default();
    let notification_entries = build_two_test_entries();
    let render_buffer = render_notification_frame(
        &build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]),
        build_center_viewer_chrome(0, Some(notification_entries[0].notification_id), 0),
        NotificationView {
            center_entries: &build_center_entries(&notification_entries),
            dropped_entry_count: 0,
            shown_toast: None,
        },
    );
    let hover_border = (theme.hover_border_color, Modifier::BOLD);
    let unfocused_border = (theme.unfocused_border_color, Modifier::empty());

    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 56, 1),
        (theme.focused_border_color, Modifier::BOLD)
    );
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 76, 2),
        (theme.focused_border_color, Modifier::BOLD)
    );
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 63, 21),
        (theme.focused_border_color, Modifier::BOLD)
    );
    // The newest card, rows 4 to 8, is not selected.
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 57, 4),
        unfocused_border
    );
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 75, 4),
        unfocused_border
    );
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 78, 6),
        unfocused_border
    );
    // The selected card, rows 9 to 13.
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 57, 9),
        hover_border
    );
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 75, 9),
        hover_border
    );
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 57, 11),
        hover_border
    );
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 78, 13),
        hover_border
    );
}

#[test]
fn each_header_takes_its_severity_style_and_the_status_text_is_dim() {
    let theme = Theme::default();
    let notification_entries = VecDeque::from([
        build_test_notification_entry(0, NotificationSeverity::Info, "plugin resumed", 1),
        build_test_notification_entry(1, NotificationSeverity::Warning, "unknown key", 1),
        build_test_notification_entry(2, NotificationSeverity::Error, "killed", 1),
    ]);
    let render_buffer = render_notification_frame(
        &build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]),
        build_open_center_viewer_chrome(0),
        NotificationView {
            center_entries: &build_center_entries(&notification_entries),
            dropped_entry_count: 1,
            shown_toast: None,
        },
    );

    assert_eq!(
        format_rendered_cells(&render_buffer, 5, 59..72),
        "[Koshi] Error"
    );
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 59, 5),
        (theme.accent_color, Modifier::BOLD | Modifier::REVERSED)
    );
    assert_eq!(format_rendered_cells(&render_buffer, 6, 59..65), "killed");
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 59, 6),
        (Color::Reset, Modifier::empty())
    );
    assert_eq!(
        format_rendered_cells(&render_buffer, 9, 59..74),
        "[Koshi] Warning"
    );
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 59, 9),
        (theme.hover_border_color, Modifier::BOLD)
    );
    assert_eq!(
        format_rendered_cells(&render_buffer, 13, 59..71),
        "[Koshi] Info"
    );
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 59, 13),
        (theme.focused_border_color, Modifier::BOLD)
    );
    assert_eq!(
        format_right_box_rows(&render_buffer, 16..18),
        ["│ 1 older notification │", "│ was dropped          │"]
    );
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 58, 16),
        (theme.unfocused_border_color, Modifier::empty())
    );
}

#[test]
fn a_narrow_center_ends_its_title_in_an_ellipsis() {
    let notification_entries = VecDeque::new();
    let render_buffer = render_notification_frame(
        &build_tray_test_render_snapshot(
            Size {
                column_count: 20,
                row_count: 10,
            },
            &[],
        ),
        build_open_center_viewer_chrome(0),
        NotificationView {
            center_entries: &build_center_entries(&notification_entries),
            dropped_entry_count: 0,
            shown_toast: None,
        },
    );

    assert_eq!(
        format_rendered_cells(&render_buffer, 2, 0..20),
        "│ Notificatio…  ✕  │"
    );
}

#[test]
fn a_half_hidden_center_draws_its_left_columns_at_the_pane_area_edge() {
    let notification_entries = build_two_test_entries();
    let render_snapshot = build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]);
    let notification_view = NotificationView {
        center_entries: &build_center_entries(&notification_entries),
        dropped_entry_count: 0,
        shown_toast: None,
    };
    let plain_render_buffer =
        render_notification_frame(&render_snapshot, ViewerChrome::default(), notification_view);

    let render_buffer = render_notification_frame(
        &render_snapshot,
        build_center_viewer_chrome(0, None, 500),
        notification_view,
    );

    assert_eq!(
        format_rendered_cells(&render_buffer, 1, 56..80),
        format!(
            "{}┌───────────",
            format_rendered_cells(&plain_render_buffer, 1, 56..68)
        )
    );
    assert_eq!(
        format_rendered_cells(&render_buffer, 2, 68..80),
        "│ Notificati"
    );
    assert_eq!(
        format_rendered_cells(&render_buffer, 5, 68..80),
        "││ [Koshi] E"
    );
    assert_eq!(
        format_rendered_cells(&render_buffer, 21, 68..80),
        "│      Clear"
    );
}

#[test]
fn a_fully_hidden_center_draws_no_cell() {
    let notification_entries = build_two_test_entries();
    let render_snapshot = build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]);
    let notification_view = NotificationView {
        center_entries: &build_center_entries(&notification_entries),
        dropped_entry_count: 0,
        shown_toast: None,
    };

    assert_eq!(
        render_notification_frame(
            &render_snapshot,
            build_center_viewer_chrome(0, None, 1000),
            notification_view
        ),
        render_notification_frame(&render_snapshot, ViewerChrome::default(), notification_view)
    );
}

#[test]
fn a_toast_in_place_draws_its_close_control_its_header_and_its_text() {
    let notification_entries = build_two_test_entries();
    let render_buffer = render_notification_frame(
        &build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]),
        build_top_right_toast_viewer_chrome(2, 0),
        NotificationView {
            center_entries: &build_center_entries(&notification_entries),
            dropped_entry_count: 0,
            shown_toast: Some(NotificationToastView {
                toast_content: NotificationToastContent::Entry(&notification_entries[1]),
                toast_text_row_cache: &ToastTextRowCache::default(),
            }),
        },
    );

    assert_eq!(
        format_right_box_rows(&render_buffer, 1..6),
        [
            "┌────────────────── ✕ ─┐",
            "│ [Koshi] Error ×2     │",
            "│ pane …a1b2 killed by │",
            "│ signal 9             │",
            "└──────────────────────┘",
        ]
    );
}

#[test]
fn a_toast_ends_its_last_text_row_in_an_ellipsis_when_more_text_follows() {
    let notification_entries = VecDeque::from([build_test_notification_entry(
        0,
        NotificationSeverity::Info,
        "one two three four five six seven eight nine ten eleven",
        1,
    )]);
    let render_buffer = render_notification_frame(
        &build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]),
        build_top_right_toast_viewer_chrome(2, 0),
        NotificationView {
            center_entries: &build_center_entries(&notification_entries),
            dropped_entry_count: 0,
            shown_toast: Some(NotificationToastView {
                toast_content: NotificationToastContent::Entry(&notification_entries[0]),
                toast_text_row_cache: &ToastTextRowCache::default(),
            }),
        },
    );

    assert_eq!(
        format_right_box_rows(&render_buffer, 3..5),
        ["│ one two three four   │", "│ five six seven eigh… │"]
    );
}

#[test]
fn a_toast_header_wider_than_its_row_ends_in_an_ellipsis() {
    let notification_entries = VecDeque::from([build_test_notification_entry(
        0,
        NotificationSeverity::Warning,
        "unknown key",
        123_456,
    )]);
    let render_buffer = render_notification_frame(
        &build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]),
        build_top_right_toast_viewer_chrome(1, 0),
        NotificationView {
            center_entries: &build_center_entries(&notification_entries),
            dropped_entry_count: 0,
            shown_toast: Some(NotificationToastView {
                toast_content: NotificationToastContent::Entry(&notification_entries[0]),
                toast_text_row_cache: &ToastTextRowCache::default(),
            }),
        },
    );

    assert_eq!(
        format_right_box_rows(&render_buffer, 2..3),
        ["│ [Koshi] Warning ×12… │"]
    );
}

#[test]
fn a_summary_toast_draws_the_count_at_its_loudest_severity() {
    let notification_entries = VecDeque::new();
    let render_buffer = render_notification_frame(
        &build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]),
        build_top_right_toast_viewer_chrome(2, 0),
        NotificationView {
            center_entries: &build_center_entries(&notification_entries),
            dropped_entry_count: 0,
            shown_toast: Some(NotificationToastView {
                toast_content: NotificationToastContent::Summary {
                    notification_count: 36,
                    severity: NotificationSeverity::Error,
                },
                toast_text_row_cache: &ToastTextRowCache::default(),
            }),
        },
    );

    assert_eq!(
        format_right_box_rows(&render_buffer, 2..5),
        [
            "│ [Koshi] Error        │",
            "│ 36 more              │",
            "│ notifications in th… │",
        ]
    );
}

#[test]
fn a_half_hidden_right_toast_draws_its_left_columns_at_the_pane_area_edge() {
    let notification_entries = build_two_test_entries();
    let render_buffer = render_notification_frame(
        &build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]),
        build_top_right_toast_viewer_chrome(2, 500),
        NotificationView {
            center_entries: &build_center_entries(&notification_entries),
            dropped_entry_count: 0,
            shown_toast: Some(NotificationToastView {
                toast_content: NotificationToastContent::Entry(&notification_entries[1]),
                toast_text_row_cache: &ToastTextRowCache::default(),
            }),
        },
    );

    assert_eq!(
        format_right_box_rows(&render_buffer, 1..4),
        [
            "────────────┌───────────",
            "            │ [Koshi] Er",
            "            │ pane …a1b2",
        ]
    );
}

#[test]
fn a_wide_glyph_the_slide_edge_cuts_is_drawn_as_a_blank() {
    let notification_entries = VecDeque::from([build_test_notification_entry(
        0,
        NotificationSeverity::Info,
        "世界世界",
        1,
    )]);
    // 24 columns x 808 thousandths is 19.392 columns, which rounds to 19: 5
    // columns are drawn, and `界` would take box columns 4 and 5.
    let render_buffer = render_notification_frame(
        &build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]),
        build_top_right_toast_viewer_chrome(1, 808),
        NotificationView {
            center_entries: &build_center_entries(&notification_entries),
            dropped_entry_count: 0,
            shown_toast: Some(NotificationToastView {
                toast_content: NotificationToastContent::Entry(&notification_entries[0]),
                toast_text_row_cache: &ToastTextRowCache::default(),
            }),
        },
    );

    assert_eq!(format_rendered_cells(&render_buffer, 3, 75..80), "│ 世  ");
}

#[test]
fn no_toast_content_draws_no_toast() {
    let notification_entries = build_two_test_entries();
    let render_snapshot = build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]);
    let toast_render_buffer = render_notification_frame(
        &render_snapshot,
        build_top_right_toast_viewer_chrome(2, 0),
        NotificationView {
            center_entries: &build_center_entries(&notification_entries),
            dropped_entry_count: 0,
            shown_toast: None,
        },
    );
    let plain_render_buffer = render_notification_frame(
        &render_snapshot,
        ViewerChrome::default(),
        NotificationView {
            center_entries: &build_center_entries(&notification_entries),
            dropped_entry_count: 0,
            shown_toast: None,
        },
    );

    assert_eq!(toast_render_buffer, plain_render_buffer);
}

#[test]
fn the_cursor_under_the_open_center_or_the_drawn_toast_is_hidden() {
    use koshi_terminal::grid::state::Grid;
    use koshi_terminal::style::Style as TerminalStyle;
    use ratatui::layout::{Position, Rect as RatatuiRect};

    use crate::render::get_cursor_position;
    use crate::snapshot::GridView;

    let mut render_snapshot = build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]);
    // The tiled pane's content starts at screen (1, 2): its cursor at row 0,
    // column 60 sits at (61, 2), under the center and under a toast in place,
    // and left of the 12 columns a half-hidden center or toast draws from
    // column 68.
    let tiled_pane_snapshot = &mut render_snapshot.pane_snapshots[0];
    tiled_pane_snapshot.terminal_grid_view = Some(GridView::from_grid(
        &Grid::build_blank(20, 78, TerminalStyle::default()),
        0,
    ));
    tiled_pane_snapshot.cursor_snapshot.column_index = 60;
    tiled_pane_snapshot.cursor_snapshot.is_visible = true;
    let committed_regions = CommittedRegions::build_core(EIGHTY_BY_TWENTY_FOUR_SIZE, 0);
    let viewport_area = RatatuiRect::new(0, 0, 80, 24);
    let find_cursor_position = |viewer_chrome: ViewerChrome| {
        get_cursor_position(&build_frame_geometry(
            &render_snapshot,
            &committed_regions,
            viewer_chrome,
            viewport_area,
        ))
    };

    assert_eq!(
        find_cursor_position(build_open_center_viewer_chrome(0)),
        None
    );
    assert_eq!(
        find_cursor_position(build_center_viewer_chrome(0, None, 500)),
        Some(Position::new(61, 2))
    );
    assert_eq!(
        find_cursor_position(build_top_right_toast_viewer_chrome(2, 0)),
        None
    );
    assert_eq!(
        find_cursor_position(build_top_right_toast_viewer_chrome(2, 500)),
        Some(Position::new(61, 2))
    );
    assert_eq!(
        find_cursor_position(ViewerChrome::default()),
        Some(Position::new(61, 2))
    );
}
