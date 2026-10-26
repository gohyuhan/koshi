//! Tests for where the notification center, the toast and the tabline's
//! notification button sit, and for the texts and rows they draw: the panel
//! width rule, the center's parts, each toast corner and its slide, the
//! button text, the headers, word wrapping, and the center's body rows.

use super::*;

use std::time::SystemTime;

use koshi_core::ids::SessionId;
use koshi_core::notification::{NotificationEpoch, NotificationId, NotificationQueueOwner};

use crate::floating_pane_tray::tests::build_tray_test_render_snapshot;
use crate::snapshot::{
    CommittedRegions, NotificationCenter, NotificationToast, RenderSnapshot, ViewerChrome,
};

/// An `80x24` viewport: its pane area is `(0, 1)` `80x22`.
const EIGHTY_BY_TWENTY_FOUR_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// A frame with a `viewport_size` viewport, one tiled pane and no floating
/// pane.
fn build_notification_test_render_snapshot(viewport_size: Size) -> RenderSnapshot {
    build_tray_test_render_snapshot(viewport_size, &[])
}

/// The viewer chrome of a client whose notification center is open and
/// focused, scrolled to its top.
fn build_open_center_viewer_chrome() -> ViewerChrome {
    ViewerChrome {
        notification_center: Some(NotificationCenter {
            scroll_row_offset: 0,
            is_focused: true,
        }),
        ..ViewerChrome::default()
    }
}

/// The viewer chrome of a client that shows a toast of `text_row_count` text
/// rows in `toast_position`, `hidden_per_mille` thousandths past its edge.
fn build_toast_viewer_chrome(
    toast_position: ToastPosition,
    text_row_count: u16,
    hidden_per_mille: u16,
) -> ViewerChrome {
    ViewerChrome {
        notification_toast: Some(NotificationToast {
            toast_position,
            text_row_count,
            hidden_per_mille,
        }),
        ..ViewerChrome::default()
    }
}

/// [`find_drawn_notification_center_layout`] of `render_snapshot` with
/// `viewer_chrome`, over the compiled-in region solve of its viewport.
fn find_test_center_layout(
    render_snapshot: &RenderSnapshot,
    viewer_chrome: ViewerChrome,
) -> Option<NotificationCenterLayout> {
    let committed_regions =
        CommittedRegions::build_core(render_snapshot.client_snapshot.viewport_size, 0);
    find_drawn_notification_center_layout(
        render_snapshot.build_painted_frame_layout(viewer_chrome, &committed_regions),
    )
}

/// [`find_drawn_notification_toast_layout`] of `render_snapshot` with
/// `viewer_chrome`, over the compiled-in region solve of its viewport.
fn find_test_toast_layout(
    render_snapshot: &RenderSnapshot,
    viewer_chrome: ViewerChrome,
) -> Option<NotificationToastLayout> {
    let committed_regions =
        CommittedRegions::build_core(render_snapshot.client_snapshot.viewport_size, 0);
    find_drawn_notification_toast_layout(
        render_snapshot.build_painted_frame_layout(viewer_chrome, &committed_regions),
    )
}

/// A one-row rect at `(column, row)`, `column_count` columns wide.
fn build_test_row_rect(column: u16, row: u16, column_count: u16) -> Rect {
    build_test_rect(column, row, column_count, 1)
}

/// A rect at `(column, row)`, `column_count x row_count`.
fn build_test_rect(column: u16, row: u16, column_count: u16, row_count: u16) -> Rect {
    Rect::from_origin_and_size(
        Point { column, row },
        Size {
            column_count,
            row_count,
        },
    )
}

/// Each of `entries`, oldest first, as the center lists it.
pub(crate) fn build_center_entries(
    entries: &VecDeque<NotificationEntry>,
) -> VecDeque<NotificationCenterEntry> {
    entries
        .iter()
        .cloned()
        .map(NotificationCenterEntry::from_entry)
        .collect()
}

/// The notification view of a viewer that holds no entry and shows no toast.
pub(crate) const EMPTY_NOTIFICATION_VIEW: NotificationView<'static> = NotificationView {
    entries: &VecDeque::new(),
    dropped_entry_count: 0,
    toast_content: None,
};

/// A session entry at `sequence` with `severity`, `text` and `repeat_count`.
pub(crate) fn build_test_notification_entry(
    sequence: u64,
    severity: NotificationSeverity,
    text: &str,
    repeat_count: u32,
) -> NotificationEntry {
    NotificationEntry {
        id: NotificationId {
            owner: NotificationQueueOwner::Session(SessionId::new()),
            epoch: NotificationEpoch::new(),
            sequence,
        },
        source: NotificationSource::Core,
        severity,
        text: text.to_string(),
        created_at: SystemTime::UNIX_EPOCH,
        repeat_count,
    }
}

#[test]
fn the_panel_takes_a_third_of_the_viewport_from_24_to_48_columns() {
    assert_eq!(compute_notification_panel_column_count(80), 26);
    assert_eq!(compute_notification_panel_column_count(75), 25);
    assert_eq!(compute_notification_panel_column_count(60), 24);
    assert_eq!(compute_notification_panel_column_count(144), 48);
    assert_eq!(compute_notification_panel_column_count(200), 48);
}

#[test]
fn the_panel_takes_the_full_width_of_a_viewport_narrower_than_24_columns() {
    assert_eq!(compute_notification_panel_column_count(24), 24);
    assert_eq!(compute_notification_panel_column_count(23), 23);
    assert_eq!(compute_notification_panel_column_count(20), 20);
    assert_eq!(compute_notification_panel_column_count(0), 0);
}

#[test]
fn an_open_center_on_an_80_by_24_viewport_takes_the_right_26_columns_of_the_pane_area() {
    let render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);

    assert_eq!(
        find_test_center_layout(&render_snapshot, build_open_center_viewer_chrome()),
        Some(NotificationCenterLayout {
            center_rect: build_test_rect(54, 1, 26, 22),
            title_rect: build_test_row_rect(56, 1, 18),
            close_rect: build_test_row_rect(75, 1, 3),
            clear_rect: build_test_row_rect(56, 22, 7),
            body_rect: build_test_rect(56, 2, 22, 20),
        })
    );
}

#[test]
fn a_closed_center_has_no_layout() {
    let render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);

    assert_eq!(
        find_test_center_layout(&render_snapshot, ViewerChrome::default()),
        None
    );
}

#[test]
fn an_open_center_has_no_layout_while_every_pane_is_suppressed() {
    let mut render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .is_every_pane_suppressed = true;

    assert_eq!(
        find_test_center_layout(&render_snapshot, build_open_center_viewer_chrome()),
        None
    );
}

#[test]
fn an_open_center_has_no_layout_on_a_viewport_narrower_than_10_columns() {
    let render_snapshot = build_notification_test_render_snapshot(Size {
        column_count: 9,
        row_count: 24,
    });

    assert_eq!(
        find_test_center_layout(&render_snapshot, build_open_center_viewer_chrome()),
        None
    );
}

#[test]
fn an_open_center_has_no_layout_in_a_pane_area_shorter_than_3_rows() {
    let render_snapshot = build_notification_test_render_snapshot(Size {
        column_count: 80,
        row_count: 4,
    });

    assert_eq!(
        find_test_center_layout(&render_snapshot, build_open_center_viewer_chrome()),
        None
    );
}

#[test]
fn a_10_column_center_cuts_its_clear_label_before_the_last_border_cells() {
    let render_snapshot = build_notification_test_render_snapshot(Size {
        column_count: 10,
        row_count: 8,
    });

    assert_eq!(
        find_test_center_layout(&render_snapshot, build_open_center_viewer_chrome()),
        Some(NotificationCenterLayout {
            center_rect: build_test_rect(0, 1, 10, 6),
            title_rect: build_test_row_rect(2, 1, 2),
            close_rect: build_test_row_rect(5, 1, 3),
            clear_rect: build_test_row_rect(2, 6, 6),
            body_rect: build_test_rect(2, 2, 6, 4),
        })
    );
}

#[test]
fn a_toast_in_place_sits_in_each_corner_of_the_pane_area() {
    let render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);
    let corner_cases = [
        (ToastPosition::TopRight, 54, 1),
        (ToastPosition::TopLeft, 0, 1),
        (ToastPosition::BottomRight, 54, 18),
        (ToastPosition::BottomLeft, 0, 18),
    ];

    for (toast_position, toast_column, toast_row) in corner_cases {
        let toast_rect = build_test_rect(toast_column, toast_row, 26, 5);
        assert_eq!(
            find_test_toast_layout(
                &render_snapshot,
                build_toast_viewer_chrome(toast_position, 2, 0)
            ),
            Some(NotificationToastLayout {
                toast_rect,
                drawn_rect: toast_rect,
                first_drawn_box_column: 0,
                close_rect: Some(build_test_row_rect(toast_column + 21, toast_row, 3)),
            }),
            "{toast_position:?}"
        );
    }
}

#[test]
fn a_half_hidden_right_toast_draws_its_left_columns_and_no_close_control() {
    let render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);

    assert_eq!(
        find_test_toast_layout(
            &render_snapshot,
            build_toast_viewer_chrome(ToastPosition::TopRight, 0, 500)
        ),
        Some(NotificationToastLayout {
            toast_rect: build_test_rect(54, 1, 26, 3),
            drawn_rect: build_test_rect(67, 1, 13, 3),
            first_drawn_box_column: 0,
            close_rect: None,
        })
    );
}

#[test]
fn a_half_hidden_left_toast_draws_its_right_columns_and_its_close_control() {
    let render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);

    assert_eq!(
        find_test_toast_layout(
            &render_snapshot,
            build_toast_viewer_chrome(ToastPosition::BottomLeft, 0, 500)
        ),
        Some(NotificationToastLayout {
            toast_rect: build_test_rect(0, 20, 26, 3),
            drawn_rect: build_test_rect(0, 20, 13, 3),
            first_drawn_box_column: 13,
            close_rect: Some(build_test_row_rect(8, 20, 3)),
        })
    );
}

#[test]
fn a_toast_hidden_by_one_column_keeps_its_close_control_at_the_moved_columns() {
    let render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);

    // 26 columns x 39 thousandths is 1.014 columns, which rounds to 1.
    assert_eq!(
        find_test_toast_layout(
            &render_snapshot,
            build_toast_viewer_chrome(ToastPosition::TopRight, 0, 39)
        ),
        Some(NotificationToastLayout {
            toast_rect: build_test_rect(54, 1, 26, 3),
            drawn_rect: build_test_rect(55, 1, 25, 3),
            first_drawn_box_column: 0,
            close_rect: Some(build_test_row_rect(76, 1, 3)),
        })
    );
}

#[test]
fn a_fully_hidden_toast_has_no_layout() {
    let render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);

    for hidden_per_mille in [1000, 999, u16::MAX] {
        assert_eq!(
            find_test_toast_layout(
                &render_snapshot,
                build_toast_viewer_chrome(ToastPosition::TopRight, 1, hidden_per_mille)
            ),
            None,
            "{hidden_per_mille}"
        );
    }
}

#[test]
fn a_toast_taller_than_the_pane_area_has_no_layout() {
    let render_snapshot = build_notification_test_render_snapshot(Size {
        column_count: 80,
        row_count: 6,
    });

    assert_eq!(
        find_test_toast_layout(
            &render_snapshot,
            build_toast_viewer_chrome(ToastPosition::TopRight, 2, 0)
        ),
        None
    );
    assert_eq!(
        find_test_toast_layout(
            &render_snapshot,
            build_toast_viewer_chrome(ToastPosition::TopRight, 1, 0)
        )
        .map(|toast_layout| toast_layout.toast_rect),
        Some(build_test_rect(54, 1, 26, 4))
    );
}

#[test]
fn no_toast_has_no_layout_and_a_suppressed_frame_draws_none() {
    let mut render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);
    assert_eq!(
        find_test_toast_layout(&render_snapshot, ViewerChrome::default()),
        None
    );

    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .is_every_pane_suppressed = true;
    assert_eq!(
        find_test_toast_layout(
            &render_snapshot,
            build_toast_viewer_chrome(ToastPosition::TopRight, 1, 0)
        ),
        None
    );
}

#[test]
fn the_button_text_counts_unread_entries_up_to_99() {
    let format_button_text = |unread_entry_count: usize| {
        format_notification_button_text(NotificationButton { unread_entry_count })
    };

    assert_eq!(format_button_text(0), " ● ");
    assert_eq!(format_button_text(3), " ●3 ");
    assert_eq!(format_button_text(99), " ●99 ");
    assert_eq!(format_button_text(100), " ●99+ ");
}

#[test]
fn the_button_starts_right_before_its_end_column_when_the_row_has_room() {
    let tabline_area = RatatuiRect::new(0, 0, 80, 1);
    let three_unread_button = Some(NotificationButton {
        unread_entry_count: 3,
    });

    assert_eq!(
        find_notification_button_columns(three_unread_button, tabline_area, 74),
        Some(70..74)
    );
    assert_eq!(
        find_notification_button_columns(three_unread_button, tabline_area, 4),
        Some(0..4)
    );
    assert_eq!(
        find_notification_button_columns(three_unread_button, tabline_area, 3),
        None
    );
    assert_eq!(
        find_notification_button_columns(None, tabline_area, 74),
        None
    );
}

#[test]
fn an_entry_header_names_its_source_and_severity_and_counts_repeats() {
    assert_eq!(
        format_notification_header(&build_test_notification_entry(
            0,
            NotificationSeverity::Error,
            "pane …a1b2 killed by signal 9",
            1
        )),
        "[Koshi] Error"
    );
    assert_eq!(
        format_notification_header(&build_test_notification_entry(
            0,
            NotificationSeverity::Warning,
            "koshi.kdl: unknown key",
            3
        )),
        "[Koshi] Warning ×3"
    );
    assert_eq!(
        format_notification_header(&build_test_notification_entry(
            0,
            NotificationSeverity::Info,
            "plugin resumed",
            1
        )),
        "[Koshi] Info"
    );
}

#[test]
fn the_summary_and_dropped_texts_agree_with_their_counts() {
    assert_eq!(
        format_toast_summary_text(1),
        "1 more notification in the center"
    );
    assert_eq!(
        format_toast_summary_text(36),
        "36 more notifications in the center"
    );
    assert_eq!(
        format_dropped_notifications_text(1),
        "1 older notification was dropped"
    );
    assert_eq!(
        format_dropped_notifications_text(3),
        "3 older notifications were dropped"
    );
}

#[test]
fn a_summary_toast_reads_as_koshi_at_its_loudest_severity() {
    let summary_content = NotificationToastContent::Summary {
        notification_count: 36,
        severity: NotificationSeverity::Error,
    };

    assert_eq!(summary_content.format_header(), "[Koshi] Error");
    assert_eq!(
        summary_content.format_text(),
        "36 more notifications in the center"
    );
    assert_eq!(summary_content.get_severity(), NotificationSeverity::Error);
}

#[test]
fn an_entry_toast_reads_as_its_entry() {
    let entry = build_test_notification_entry(
        4,
        NotificationSeverity::Warning,
        "koshi.kdl: unknown key",
        2,
    );
    let entry_content = NotificationToastContent::Entry(&entry);

    assert_eq!(entry_content.format_header(), "[Koshi] Warning ×2");
    assert_eq!(entry_content.format_text(), "koshi.kdl: unknown key");
    assert_eq!(entry_content.get_severity(), NotificationSeverity::Warning);
}

#[test]
fn wrapping_fills_each_row_with_the_words_that_fit() {
    assert_eq!(
        wrap_text_rows("pane …a1b2 killed by signal 9", 10),
        ["pane …a1b2", "killed by", "signal 9"]
    );
    assert_eq!(wrap_text_rows("abc def", 7), ["abc def"]);
    assert_eq!(wrap_text_rows("abc def", 6), ["abc", "def"]);
    assert_eq!(wrap_text_rows("  abc   def  ", 20), ["abc def"]);
}

#[test]
fn wrapping_cuts_a_word_wider_than_a_row_where_the_row_is_full() {
    assert_eq!(wrap_text_rows("abcdefghijkl", 10), ["abcdefghij", "kl"]);
    assert_eq!(
        wrap_text_rows("ab abcdefghijkl c", 10),
        ["ab", "abcdefghij", "kl c"]
    );
}

#[test]
fn wrapping_keeps_wide_characters_whole_and_gives_a_too_wide_one_a_row_alone() {
    assert_eq!(wrap_text_rows("世界世界", 3), ["世", "界", "世", "界"]);
    assert_eq!(wrap_text_rows("世 a", 1), ["世", "a"]);
}

#[test]
fn wrapping_keeps_a_combining_mark_with_the_character_before_it() {
    assert_eq!(wrap_text_rows("e\u{301}x", 1), ["e\u{301}", "x"]);
}

#[test]
fn wrapping_cuts_a_long_word_between_graphemes_and_keeps_an_emoji_sequence_whole() {
    let family_emoji = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
    let word = format!("ab{family_emoji}cd");

    assert_eq!(
        wrap_text_rows(&word, 3),
        [
            "ab".to_string(),
            format!("{family_emoji}c"),
            "d".to_string()
        ]
    );
}

#[test]
fn wrapping_reads_a_control_character_as_a_word_break() {
    assert_eq!(wrap_text_rows("ab\ncd\tef", 10), ["ab cd ef"]);
}

#[test]
fn text_with_no_word_wraps_into_no_row() {
    assert_eq!(wrap_text_rows("", 10), Vec::<String>::new());
    assert_eq!(wrap_text_rows("   ", 10), Vec::<String>::new());
}

#[test]
fn a_toast_holds_from_0_to_2_text_rows() {
    assert_eq!(
        count_toast_text_rows("pane …a1b2 killed by signal 9", 26),
        2
    );
    assert_eq!(count_toast_text_rows("pane exited", 26), 1);
    assert_eq!(count_toast_text_rows("", 26), 0);
    assert_eq!(count_toast_text_rows("a b c d e f g h i j k l", 10), 2);
}

#[test]
fn center_rows_list_each_entry_newest_first_then_the_dropped_count() {
    let entries = VecDeque::from([
        build_test_notification_entry(0, NotificationSeverity::Warning, "a", 1),
        build_test_notification_entry(1, NotificationSeverity::Error, "b c", 1),
    ]);

    let center_entries = build_center_entries(&entries);
    let build_view = |dropped_entry_count| NotificationView {
        entries: &center_entries,
        dropped_entry_count,
        toast_content: None,
    };

    assert_eq!(
        list_notification_center_rows(build_view(2), 40, 0).collect::<Vec<_>>(),
        [
            NotificationCenterRow::EntryHeader(&entries[1]),
            NotificationCenterRow::EntryText("b c".to_string()),
            NotificationCenterRow::EntryHeader(&entries[0]),
            NotificationCenterRow::EntryText("a".to_string()),
            NotificationCenterRow::StatusText("2 older notifications were dropped".to_string()),
        ]
    );
    assert_eq!(
        list_notification_center_rows(build_view(0), 2, 0).collect::<Vec<_>>(),
        [
            NotificationCenterRow::EntryHeader(&entries[1]),
            NotificationCenterRow::EntryText("b".to_string()),
            NotificationCenterRow::EntryText("c".to_string()),
            NotificationCenterRow::EntryHeader(&entries[0]),
            NotificationCenterRow::EntryText("a".to_string()),
        ]
    );
}

#[test]
fn center_rows_from_a_row_skip_whole_entries_then_the_rows_before_it() {
    let entries = VecDeque::from([
        build_test_notification_entry(0, NotificationSeverity::Warning, "a", 1),
        build_test_notification_entry(1, NotificationSeverity::Error, "b c", 1),
    ]);
    let center_entries = build_center_entries(&entries);
    let list_rows_from = |dropped_entry_count, body_column_count, first_row_index| {
        let notification_view = NotificationView {
            entries: &center_entries,
            dropped_entry_count,
            toast_content: None,
        };
        list_notification_center_rows(notification_view, body_column_count, first_row_index)
            .collect::<Vec<_>>()
    };

    assert_eq!(
        list_rows_from(0, 2, 1),
        [
            NotificationCenterRow::EntryText("b".to_string()),
            NotificationCenterRow::EntryText("c".to_string()),
            NotificationCenterRow::EntryHeader(&entries[0]),
            NotificationCenterRow::EntryText("a".to_string()),
        ]
    );
    assert_eq!(
        list_rows_from(0, 2, 3),
        [
            NotificationCenterRow::EntryHeader(&entries[0]),
            NotificationCenterRow::EntryText("a".to_string()),
        ]
    );
    assert_eq!(
        list_rows_from(0, 2, 4),
        [NotificationCenterRow::EntryText("a".to_string())]
    );
    assert_eq!(list_rows_from(0, 2, 5), []);
    assert_eq!(
        list_rows_from(2, 40, 4),
        [NotificationCenterRow::StatusText(
            "2 older notifications were dropped".to_string()
        )]
    );
    assert_eq!(list_rows_from(2, 40, 5), []);
}

#[test]
fn a_center_entry_counts_its_text_rows_at_the_width_asked_for() {
    let center_entry = NotificationCenterEntry::from_entry(build_test_notification_entry(
        0,
        NotificationSeverity::Error,
        "pane …a1b2 killed by signal 9",
        1,
    ));

    let row_counts =
        [10, 10, 40, 10].map(|body_column_count| center_entry.count_text_rows(body_column_count));

    assert_eq!(row_counts, [3, 3, 1, 3]);
}

#[test]
fn an_empty_center_lists_no_notifications_or_only_the_wrapped_dropped_count() {
    let build_view = |dropped_entry_count| NotificationView {
        dropped_entry_count,
        ..EMPTY_NOTIFICATION_VIEW
    };

    assert_eq!(
        list_notification_center_rows(build_view(0), 20, 0).collect::<Vec<_>>(),
        [NotificationCenterRow::StatusText(
            "No notifications".to_string()
        )]
    );
    assert_eq!(
        list_notification_center_rows(build_view(5), 20, 0).collect::<Vec<_>>(),
        [
            NotificationCenterRow::StatusText("5 older".to_string()),
            NotificationCenterRow::StatusText("notifications were".to_string()),
            NotificationCenterRow::StatusText("dropped".to_string()),
        ]
    );
}

#[test]
fn border_labels_take_the_title_cells_and_the_close_control_before_the_last_border_cells() {
    let twenty_six_column_labels = compute_border_label_rects(build_test_rect(54, 1, 26, 5));
    assert_eq!(
        twenty_six_column_labels.title_rect,
        build_test_row_rect(56, 1, 18)
    );
    assert_eq!(
        twenty_six_column_labels.close_rect,
        build_test_row_rect(75, 1, 3)
    );

    let ten_column_labels = compute_border_label_rects(build_test_rect(0, 0, 10, 3));
    assert_eq!(ten_column_labels.title_rect, build_test_row_rect(2, 0, 2));
    assert_eq!(ten_column_labels.close_rect, build_test_row_rect(5, 0, 3));
}

#[test]
fn the_maximum_center_scroll_row_offset_puts_the_last_row_on_the_last_body_row() {
    let entries: VecDeque<NotificationCenterEntry> = (0..30)
        .map(|entry_index| {
            NotificationCenterEntry::from_entry(build_test_notification_entry(
                entry_index,
                NotificationSeverity::Info,
                &format!("entry {entry_index}"),
                1,
            ))
        })
        .collect();
    let body_rect = build_test_rect(56, 2, 22, 20);

    let thirty_entries = compute_maximum_center_scroll_row_offset(
        NotificationView {
            entries: &entries,
            dropped_entry_count: 0,
            toast_content: None,
        },
        body_rect,
    );
    let thirty_entries_and_a_dropped_count = compute_maximum_center_scroll_row_offset(
        NotificationView {
            entries: &entries,
            dropped_entry_count: 3,
            toast_content: None,
        },
        body_rect,
    );
    let two_entries = compute_maximum_center_scroll_row_offset(
        NotificationView {
            entries: &entries.range(..2).cloned().collect(),
            dropped_entry_count: 0,
            toast_content: None,
        },
        body_rect,
    );

    assert_eq!(thirty_entries, 40);
    assert_eq!(thirty_entries_and_a_dropped_count, 42);
    assert_eq!(two_entries, 0);
}
