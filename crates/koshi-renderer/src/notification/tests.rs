//! Tests for where the notification center, the toast and the tabline's
//! notification button sit, and for the texts and rows they draw: the panel
//! width rule, the center's parts and its slide, each toast corner, its
//! slide and its fit, the button text, the headers, word wrapping, the
//! center's cards and body rows, and the card under a cell.

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

/// The viewer chrome of a client whose notification center is open in place,
/// scrolled to its top, with no card selected.
fn build_open_center_viewer_chrome() -> ViewerChrome {
    build_sliding_center_viewer_chrome(0)
}

/// The viewer chrome of a client whose notification center stands
/// `hidden_per_mille` thousandths past the pane area's right edge.
fn build_sliding_center_viewer_chrome(hidden_per_mille: u16) -> ViewerChrome {
    ViewerChrome {
        notification_center: Some(NotificationCenter {
            scroll_row_offset: 0,
            selected_notification_id: None,
            hidden_per_mille,
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

/// Each of `notification_entries`, oldest first, as the center lists it.
pub(crate) fn build_center_entries(
    notification_entries: &VecDeque<NotificationEntry>,
) -> NotificationCenterEntries {
    let mut center_entries = NotificationCenterEntries::default();
    for notification_entry in notification_entries {
        center_entries.add_newest_center_entry(NotificationCenterEntry::from_notification_entry(
            notification_entry.clone(),
        ));
    }
    center_entries
}

/// The notification view of a viewer that holds `center_entries`, counts no
/// dropped entry and shows no toast.
pub(crate) fn build_test_notification_view(
    center_entries: &NotificationCenterEntries,
) -> NotificationView<'_> {
    NotificationView {
        center_entries,
        dropped_entry_count: 0,
        shown_toast: None,
    }
}

/// A session entry at `sequence` with `severity`, `text` and `repeat_count`.
pub(crate) fn build_test_notification_entry(
    sequence: u64,
    severity: NotificationSeverity,
    text: &str,
    repeat_count: u32,
) -> NotificationEntry {
    NotificationEntry {
        notification_id: NotificationId {
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
fn the_panel_takes_a_tenth_of_the_pane_area_from_24_to_48_columns() {
    assert_eq!(compute_notification_panel_column_count(80), 24);
    assert_eq!(compute_notification_panel_column_count(249), 24);
    assert_eq!(compute_notification_panel_column_count(250), 25);
    assert_eq!(compute_notification_panel_column_count(300), 30);
    assert_eq!(compute_notification_panel_column_count(480), 48);
    assert_eq!(compute_notification_panel_column_count(600), 48);
}

#[test]
fn the_panel_takes_the_full_width_of_a_pane_area_narrower_than_24_columns() {
    assert_eq!(compute_notification_panel_column_count(24), 24);
    assert_eq!(compute_notification_panel_column_count(23), 23);
    assert_eq!(compute_notification_panel_column_count(20), 20);
    assert_eq!(compute_notification_panel_column_count(0), 0);
}

#[test]
fn an_open_center_on_an_80_by_24_viewport_rests_on_the_right_24_columns_of_the_pane_area() {
    let render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);

    assert_eq!(
        find_test_center_layout(&render_snapshot, build_open_center_viewer_chrome()),
        Some(NotificationCenterLayout {
            center_rect: build_test_rect(56, 1, 24, 22),
            drawn_rect: build_test_rect(56, 1, 24, 22),
            title_rect: Some(build_test_row_rect(58, 2, 16)),
            body_rect: build_test_rect(57, 4, 22, 17),
            clear_all_rect: build_test_row_rect(63, 21, 9),
        })
    );
}

#[test]
fn a_half_hidden_center_draws_its_left_columns_moved_right_and_keeps_its_parts_at_rest() {
    let render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);

    let center_layout =
        find_test_center_layout(&render_snapshot, build_sliding_center_viewer_chrome(500))
            .expect("a half hidden center is drawn");

    assert_eq!(center_layout.center_rect, build_test_rect(56, 1, 24, 22));
    assert_eq!(center_layout.drawn_rect, build_test_rect(68, 1, 12, 22));
    assert_eq!(
        compute_floating_titlebar(
            center_layout.center_rect,
            FloatingTitlebarControls::CLOSE_BUTTON_ONLY
        )
        .close_button_rect,
        Some(build_test_row_rect(76, 2, 1))
    );
    assert_eq!(
        center_layout.find_resting_point(Point { column: 70, row: 5 }),
        Some(Point { column: 58, row: 5 })
    );
    assert_eq!(
        center_layout.find_resting_point(Point { column: 67, row: 5 }),
        None
    );
}

#[test]
fn a_center_in_place_rests_where_it_is_drawn() {
    let render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);

    let center_layout =
        find_test_center_layout(&render_snapshot, build_open_center_viewer_chrome())
            .expect("an open center is drawn");

    assert_eq!(
        center_layout.find_resting_point(Point { column: 56, row: 1 }),
        Some(Point { column: 56, row: 1 })
    );
    assert_eq!(
        center_layout.find_resting_point(Point { column: 55, row: 1 }),
        None
    );
}

#[test]
fn a_fully_hidden_center_draws_no_cell() {
    let render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);

    for hidden_per_mille in [1000, u16::MAX] {
        let center_layout = find_test_center_layout(
            &render_snapshot,
            build_sliding_center_viewer_chrome(hidden_per_mille),
        )
        .expect("a sliding center has a layout");
        assert_eq!(
            center_layout.drawn_rect,
            build_test_rect(80, 1, 0, 22),
            "{hidden_per_mille}"
        );
    }
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
fn an_open_center_has_no_layout_in_a_pane_area_shorter_than_6_rows() {
    let five_row_pane_area = build_notification_test_render_snapshot(Size {
        column_count: 80,
        row_count: 7,
    });
    let six_row_pane_area = build_notification_test_render_snapshot(Size {
        column_count: 80,
        row_count: 8,
    });

    assert_eq!(
        find_test_center_layout(&five_row_pane_area, build_open_center_viewer_chrome()),
        None
    );
    assert_eq!(
        find_test_center_layout(&six_row_pane_area, build_open_center_viewer_chrome())
            .map(|center_layout| center_layout.body_rect),
        Some(build_test_rect(57, 4, 22, 1))
    );
}

#[test]
fn a_10_column_center_cuts_its_clear_all_label_to_the_content_columns() {
    let render_snapshot = build_notification_test_render_snapshot(Size {
        column_count: 10,
        row_count: 8,
    });

    assert_eq!(
        find_test_center_layout(&render_snapshot, build_open_center_viewer_chrome()),
        Some(NotificationCenterLayout {
            center_rect: build_test_rect(0, 1, 10, 6),
            drawn_rect: build_test_rect(0, 1, 10, 6),
            title_rect: Some(build_test_row_rect(2, 2, 2)),
            body_rect: build_test_rect(1, 4, 8, 1),
            clear_all_rect: build_test_row_rect(1, 5, 8),
        })
    );
}

#[test]
fn a_toast_in_place_sits_in_each_corner_of_the_pane_area() {
    let render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);
    let corner_cases = [
        (ToastPosition::TopRight, 56, 1),
        (ToastPosition::TopLeft, 0, 1),
        (ToastPosition::BottomRight, 56, 18),
        (ToastPosition::BottomLeft, 0, 18),
    ];

    for (toast_position, toast_column, toast_row) in corner_cases {
        let toast_rect = build_test_rect(toast_column, toast_row, 24, 5);
        assert_eq!(
            find_test_toast_layout(
                &render_snapshot,
                build_toast_viewer_chrome(toast_position, 2, 0)
            ),
            Some(NotificationToastLayout {
                toast_rect,
                drawn_rect: toast_rect,
                first_drawn_box_column: 0,
                close_rect: Some(build_test_row_rect(toast_column + 19, toast_row, 3)),
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
            toast_rect: build_test_rect(56, 1, 24, 3),
            drawn_rect: build_test_rect(68, 1, 12, 3),
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
            toast_rect: build_test_rect(0, 20, 24, 3),
            drawn_rect: build_test_rect(0, 20, 12, 3),
            first_drawn_box_column: 12,
            close_rect: Some(build_test_row_rect(7, 20, 3)),
        })
    );
}

#[test]
fn a_toast_hidden_by_one_column_keeps_its_close_control_at_the_moved_columns() {
    let render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);

    // 24 columns x 39 thousandths is 0.936 columns, which rounds to 1.
    assert_eq!(
        find_test_toast_layout(
            &render_snapshot,
            build_toast_viewer_chrome(ToastPosition::TopRight, 0, 39)
        ),
        Some(NotificationToastLayout {
            toast_rect: build_test_rect(56, 1, 24, 3),
            drawn_rect: build_test_rect(57, 1, 23, 3),
            first_drawn_box_column: 0,
            close_rect: Some(build_test_row_rect(76, 1, 3)),
        })
    );
}

#[test]
fn a_fully_hidden_toast_has_no_layout() {
    let render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);

    for hidden_per_mille in [1000, 980, u16::MAX] {
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
fn a_toast_taller_than_the_pane_area_keeps_the_text_rows_that_fit_and_at_least_one() {
    let four_row_pane_area = build_notification_test_render_snapshot(Size {
        column_count: 80,
        row_count: 6,
    });
    let three_row_pane_area = build_notification_test_render_snapshot(Size {
        column_count: 80,
        row_count: 5,
    });

    assert_eq!(
        find_test_toast_layout(
            &four_row_pane_area,
            build_toast_viewer_chrome(ToastPosition::BottomRight, 2, 0)
        )
        .map(|toast_layout| toast_layout.toast_rect),
        Some(build_test_rect(56, 1, 24, 4))
    );
    assert_eq!(
        find_test_toast_layout(
            &three_row_pane_area,
            build_toast_viewer_chrome(ToastPosition::TopRight, 2, 0)
        ),
        None,
        "a pane area with no row for the first text row draws no toast"
    );
    assert_eq!(
        find_test_toast_layout(
            &three_row_pane_area,
            build_toast_viewer_chrome(ToastPosition::TopRight, 0, 0)
        )
        .map(|toast_layout| toast_layout.toast_rect),
        Some(build_test_rect(56, 1, 24, 3))
    );
}

#[test]
fn a_toast_room_needs_a_10_column_box_for_the_header_and_the_first_text_row_and_a_shown_pane() {
    let has_room_at = |column_count, row_count, viewer_chrome| {
        let render_snapshot = build_notification_test_render_snapshot(Size {
            column_count,
            row_count,
        });
        let committed_regions =
            CommittedRegions::build_core(render_snapshot.client_snapshot.viewport_size, 0);
        has_notification_toast_room(
            render_snapshot.build_painted_frame_layout(viewer_chrome, &committed_regions),
        )
    };
    let no_text_toast = build_toast_viewer_chrome(ToastPosition::TopRight, 0, 0);
    let two_text_row_toast = build_toast_viewer_chrome(ToastPosition::TopRight, 2, 0);

    assert!(has_room_at(80, 5, ViewerChrome::default()));
    assert!(has_room_at(80, 5, no_text_toast));
    assert!(!has_room_at(80, 5, two_text_row_toast));
    assert!(has_room_at(80, 6, two_text_row_toast));
    assert!(!has_room_at(80, 4, no_text_toast));
    assert!(has_room_at(10, 24, two_text_row_toast));
    assert!(!has_room_at(9, 24, two_text_row_toast));
    assert!(!has_room_at(0, 0, two_text_row_toast));
}

#[test]
fn a_toast_with_no_room_has_no_layout_whatever_its_slide() {
    let two_row_pane_area = build_notification_test_render_snapshot(Size {
        column_count: 80,
        row_count: 4,
    });

    assert_eq!(
        find_test_toast_layout(
            &two_row_pane_area,
            build_toast_viewer_chrome(ToastPosition::TopRight, 0, 0)
        ),
        None
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
    let committed_regions =
        CommittedRegions::build_core(render_snapshot.client_snapshot.viewport_size, 0);
    assert_eq!(
        find_test_toast_layout(
            &render_snapshot,
            build_toast_viewer_chrome(ToastPosition::TopRight, 1, 0)
        ),
        None
    );
    assert!(!has_notification_toast_room(
        render_snapshot.build_painted_frame_layout(ViewerChrome::default(), &committed_regions)
    ));
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
    let three_unread_button = NotificationButton {
        unread_entry_count: 3,
    };

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
    let toast_entry = build_test_notification_entry(
        4,
        NotificationSeverity::Warning,
        "koshi.kdl: unknown key",
        2,
    );
    let toast_content = NotificationToastContent::Entry(&toast_entry);

    assert_eq!(toast_content.format_header(), "[Koshi] Warning ×2");
    assert_eq!(toast_content.format_text(), "koshi.kdl: unknown key");
    assert_eq!(toast_content.get_severity(), NotificationSeverity::Warning);
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
fn a_toast_holds_up_to_2_text_rows_and_marks_text_that_needs_more() {
    assert_eq!(
        ToastTextRows::from_text("pane …a1b2 killed by signal 9", 26),
        ToastTextRows {
            text_rows: vec!["pane …a1b2 killed by".to_string(), "signal 9".to_string()],
            has_more_rows: false,
        }
    );
    assert_eq!(
        ToastTextRows::from_text("pane exited", 26),
        ToastTextRows {
            text_rows: vec!["pane exited".to_string()],
            has_more_rows: false,
        }
    );
    assert_eq!(ToastTextRows::from_text("", 26), ToastTextRows::default());
    assert_eq!(
        ToastTextRows::from_text("a b c d e f g h i j k l", 10),
        ToastTextRows {
            text_rows: vec!["a b c".to_string(), "d e f".to_string()],
            has_more_rows: true,
        }
    );
}

#[test]
fn a_toast_wraps_its_text_only_up_to_the_row_after_its_shown_rows() {
    let long_text = "word ".repeat(819);

    assert_eq!(wrap_text_rows_up_to(&long_text, 22, 3).len(), 3);
    assert_eq!(wrap_text_rows(&long_text, 22).len(), 205);
    assert_eq!(
        ToastTextRows::from_text(&long_text, 26),
        ToastTextRows {
            text_rows: vec![
                "word word word word".to_string(),
                "word word word word".to_string(),
            ],
            has_more_rows: true,
        }
    );
}

#[test]
fn the_toast_text_row_cache_wraps_again_only_for_another_text_or_width() {
    let toast_text_row_cache = ToastTextRowCache::default();
    let kept_rows = ToastTextRows {
        text_rows: vec!["kept".to_string()],
        has_more_rows: false,
    };
    *toast_text_row_cache.cached_text_rows.borrow_mut() = CachedToastTextRows {
        text: "pane exited".to_string(),
        toast_column_count: 26,
        toast_text_rows: kept_rows.clone(),
    };

    let same_ask = toast_text_row_cache
        .compute_toast_text_rows("pane exited", 26)
        .clone();
    let other_width = toast_text_row_cache
        .compute_toast_text_rows("pane exited", 14)
        .clone();
    let other_text = toast_text_row_cache
        .compute_toast_text_rows("pane …a1b2 killed by signal 9", 14)
        .clone();

    assert_eq!(same_ask, kept_rows);
    assert_eq!(other_width, ToastTextRows::from_text("pane exited", 14));
    assert_eq!(
        other_text,
        ToastTextRows {
            text_rows: vec!["pane …a1b2".to_string(), "killed by".to_string()],
            has_more_rows: true,
        }
    );
}

#[test]
fn a_new_toast_text_row_cache_holds_the_rows_of_an_empty_text() {
    let toast_text_row_cache = ToastTextRowCache::default();

    assert_eq!(
        *toast_text_row_cache.compute_toast_text_rows("", 0),
        ToastTextRows::from_text("", 0)
    );
}

#[test]
fn center_rows_list_one_card_per_entry_newest_first_then_the_dropped_count() {
    let notification_entries = VecDeque::from([
        build_test_notification_entry(0, NotificationSeverity::Warning, "a", 1),
        build_test_notification_entry(1, NotificationSeverity::Error, "b c", 1),
    ]);

    let center_entries = build_center_entries(&notification_entries);
    let build_view = |dropped_entry_count| NotificationView {
        center_entries: &center_entries,
        dropped_entry_count,
        shown_toast: None,
    };

    assert_eq!(
        list_notification_center_rows(
            build_view(2),
            40,
            center_entries.compute_center_row_start(40, 0)
        )
        .collect::<Vec<_>>(),
        [
            NotificationCenterRow::CardTopBorder(&notification_entries[1]),
            NotificationCenterRow::CardHeader(&notification_entries[1]),
            NotificationCenterRow::CardText {
                center_entry: &center_entries[1],
                text_row_index: 0,
            },
            NotificationCenterRow::CardBottomBorder(&notification_entries[1]),
            NotificationCenterRow::CardTopBorder(&notification_entries[0]),
            NotificationCenterRow::CardHeader(&notification_entries[0]),
            NotificationCenterRow::CardText {
                center_entry: &center_entries[0],
                text_row_index: 0,
            },
            NotificationCenterRow::CardBottomBorder(&notification_entries[0]),
            NotificationCenterRow::StatusText("2 older notifications were dropped".to_string()),
        ]
    );
    assert_eq!(
        list_notification_center_rows(
            build_view(0),
            6,
            center_entries.compute_center_row_start(6, 0)
        )
        .collect::<Vec<_>>(),
        [
            NotificationCenterRow::CardTopBorder(&notification_entries[1]),
            NotificationCenterRow::CardHeader(&notification_entries[1]),
            NotificationCenterRow::CardText {
                center_entry: &center_entries[1],
                text_row_index: 0,
            },
            NotificationCenterRow::CardText {
                center_entry: &center_entries[1],
                text_row_index: 1,
            },
            NotificationCenterRow::CardBottomBorder(&notification_entries[1]),
            NotificationCenterRow::CardTopBorder(&notification_entries[0]),
            NotificationCenterRow::CardHeader(&notification_entries[0]),
            NotificationCenterRow::CardText {
                center_entry: &center_entries[0],
                text_row_index: 0,
            },
            NotificationCenterRow::CardBottomBorder(&notification_entries[0]),
        ]
    );
    assert_eq!(*center_entries[1].list_text_rows(2), ["b", "c"]);
}

#[test]
fn center_rows_from_a_row_skip_whole_cards_then_the_rows_before_it() {
    let notification_entries = VecDeque::from([
        build_test_notification_entry(0, NotificationSeverity::Warning, "a", 1),
        build_test_notification_entry(1, NotificationSeverity::Error, "b c", 1),
    ]);
    let center_entries = build_center_entries(&notification_entries);
    let list_rows_from = |dropped_entry_count, first_row_index| {
        let notification_view = NotificationView {
            center_entries: &center_entries,
            dropped_entry_count,
            shown_toast: None,
        };
        list_notification_center_rows(
            notification_view,
            6,
            center_entries.compute_center_row_start(6, first_row_index),
        )
        .collect::<Vec<_>>()
    };

    assert_eq!(
        list_rows_from(0, 3),
        [
            NotificationCenterRow::CardText {
                center_entry: &center_entries[1],
                text_row_index: 1,
            },
            NotificationCenterRow::CardBottomBorder(&notification_entries[1]),
            NotificationCenterRow::CardTopBorder(&notification_entries[0]),
            NotificationCenterRow::CardHeader(&notification_entries[0]),
            NotificationCenterRow::CardText {
                center_entry: &center_entries[0],
                text_row_index: 0,
            },
            NotificationCenterRow::CardBottomBorder(&notification_entries[0]),
        ]
    );
    assert_eq!(
        list_rows_from(0, 5),
        [
            NotificationCenterRow::CardTopBorder(&notification_entries[0]),
            NotificationCenterRow::CardHeader(&notification_entries[0]),
            NotificationCenterRow::CardText {
                center_entry: &center_entries[0],
                text_row_index: 0,
            },
            NotificationCenterRow::CardBottomBorder(&notification_entries[0]),
        ]
    );
    assert_eq!(
        list_rows_from(0, 8),
        [NotificationCenterRow::CardBottomBorder(
            &notification_entries[0]
        )]
    );
    assert_eq!(list_rows_from(0, 9), []);
    assert_eq!(
        list_rows_from(1, 9),
        ["1", "olde", "r", "noti", "fica", "tion", "was", "drop", "ped"]
            .map(|status_row| NotificationCenterRow::StatusText(status_row.to_string()))
    );
}

#[test]
fn a_center_entry_wraps_its_text_once_per_width_asked_for() {
    let center_entry =
        NotificationCenterEntry::from_notification_entry(build_test_notification_entry(
            0,
            NotificationSeverity::Error,
            "pane …a1b2 killed by signal 9",
            1,
        ));
    *center_entry.wrapped_text.borrow_mut() = Some(WrappedEntryText {
        text_column_count: 10,
        text_rows: vec!["kept".to_string()],
    });

    let same_width_rows = center_entry.list_text_rows(10).to_vec();
    let other_width_rows = center_entry.list_text_rows(40).to_vec();

    assert_eq!(same_width_rows, ["kept"]);
    assert_eq!(other_width_rows, ["pane …a1b2 killed by signal 9"]);
    assert_eq!(
        *center_entry.list_text_rows(10),
        ["pane …a1b2", "killed by", "signal 9"]
    );
}

#[test]
fn a_card_takes_its_borders_its_header_and_its_text_rows() {
    let center_entry =
        NotificationCenterEntry::from_notification_entry(build_test_notification_entry(
            0,
            NotificationSeverity::Error,
            "pane …a1b2 killed by signal 9",
            1,
        ));

    assert_eq!(compute_card_text_column_count(22), 18);
    assert_eq!(center_entry.count_card_rows(22), 5);
    assert_eq!(center_entry.count_card_rows(44), 4);
    assert_eq!(
        *center_entry.list_text_rows(18),
        ["pane …a1b2 killed", "by signal 9"]
    );
}

#[test]
fn an_empty_center_lists_no_notifications_or_only_the_wrapped_dropped_count() {
    let center_entries = NotificationCenterEntries::default();
    let build_view = |dropped_entry_count| NotificationView {
        dropped_entry_count,
        ..build_test_notification_view(&center_entries)
    };

    assert_eq!(
        list_notification_center_rows(
            build_view(0),
            22,
            center_entries.compute_center_row_start(22, 0)
        )
        .collect::<Vec<_>>(),
        [NotificationCenterRow::StatusText(
            "No notifications".to_string()
        )]
    );
    assert_eq!(
        list_notification_center_rows(
            build_view(5),
            22,
            center_entries.compute_center_row_start(22, 0)
        )
        .collect::<Vec<_>>(),
        [
            NotificationCenterRow::StatusText("5 older".to_string()),
            NotificationCenterRow::StatusText("notifications were".to_string()),
            NotificationCenterRow::StatusText("dropped".to_string()),
        ]
    );
}

#[test]
fn the_toast_close_control_ends_before_the_last_border_cells() {
    assert_eq!(
        compute_toast_close_rect(build_test_rect(56, 1, 24, 5)),
        build_test_row_rect(75, 1, 3)
    );
    assert_eq!(
        compute_toast_close_rect(build_test_rect(0, 0, 10, 3)),
        build_test_row_rect(5, 0, 3)
    );
}

#[test]
fn a_card_row_range_counts_the_rows_of_every_newer_card() {
    let notification_entries = VecDeque::from([
        build_test_notification_entry(0, NotificationSeverity::Warning, "a", 1),
        build_test_notification_entry(
            1,
            NotificationSeverity::Error,
            "pane …a1b2 killed by signal 9",
            1,
        ),
    ]);
    let center_entries = build_center_entries(&notification_entries);
    let notification_view = NotificationView {
        center_entries: &center_entries,
        dropped_entry_count: 0,
        shown_toast: None,
    };
    let missing_entry = build_test_notification_entry(7, NotificationSeverity::Info, "b", 1);

    assert_eq!(
        find_notification_card_row_range(
            notification_view,
            22,
            notification_entries[1].notification_id
        ),
        Some(0..5)
    );
    assert_eq!(
        center_entries.counted_center_rows.borrow().card_row_ends,
        [5],
        "the newest card is the only card counted"
    );
    assert_eq!(
        find_notification_card_row_range(
            notification_view,
            22,
            notification_entries[0].notification_id
        ),
        Some(5..9)
    );
    assert_eq!(
        find_notification_card_row_range(notification_view, 22, missing_entry.notification_id),
        None
    );
}

#[test]
fn the_card_rows_name_the_drawn_card_of_each_body_row_and_none_on_the_center_text() {
    let render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);
    let center_layout =
        find_test_center_layout(&render_snapshot, build_open_center_viewer_chrome())
            .expect("an open center is drawn");
    let notification_entries = VecDeque::from([
        build_test_notification_entry(0, NotificationSeverity::Warning, "a", 1),
        build_test_notification_entry(1, NotificationSeverity::Error, "b", 1),
    ]);
    let center_entries = build_center_entries(&notification_entries);
    let notification_view = NotificationView {
        center_entries: &center_entries,
        dropped_entry_count: 0,
        shown_toast: None,
    };
    let build_card_row = |entry_index: usize, is_top_border| {
        Some(NotificationCardRow {
            notification_id: notification_entries[entry_index].notification_id,
            is_top_border,
        })
    };
    let dropped_view = NotificationView {
        center_entries: &center_entries,
        dropped_entry_count: 3,
        shown_toast: None,
    };
    let body_rect = center_layout.body_rect;

    let card_rows = notification_view
        .compute_center_body_rows(body_rect, 0)
        .card_rows;

    assert_eq!(
        *card_rows,
        [
            build_card_row(1, true),
            build_card_row(1, false),
            build_card_row(1, false),
            build_card_row(1, false),
            build_card_row(0, true),
            build_card_row(0, false),
            build_card_row(0, false),
            build_card_row(0, false),
        ]
    );
    assert_eq!(
        notification_view
            .compute_center_body_rows(body_rect, 4)
            .card_rows,
        card_rows,
        "a scroll past the maximum offset lists the rows drawn from it"
    );
    assert_eq!(
        dropped_view
            .compute_center_body_rows(body_rect, 0)
            .card_rows[8..],
        [None, None, None],
        "the 3 rows of `3 older notifications were dropped` name no card"
    );
}

#[test]
fn the_center_body_rows_start_after_the_whole_cards_and_rows_above_the_scroll() {
    let notification_entries: VecDeque<NotificationEntry> = (0..30)
        .map(|test_entry_index| {
            build_test_notification_entry(
                test_entry_index,
                NotificationSeverity::Info,
                &format!("entry {test_entry_index}"),
                1,
            )
        })
        .collect();
    let center_entries = build_center_entries(&notification_entries);
    let notification_view = build_test_notification_view(&center_entries);
    let body_rect = build_test_rect(57, 4, 22, 17);

    assert_eq!(
        notification_view
            .compute_center_body_rows(body_rect, 7)
            .row_start,
        NotificationCenterRowStart {
            skipped_entry_count: 1,
            skipped_row_count: 3,
        },
        "a scroll of 7 skips the newest card of 4 rows and 3 rows of the next"
    );
    assert_eq!(
        notification_view
            .compute_center_body_rows(body_rect, 8)
            .row_start,
        NotificationCenterRowStart {
            skipped_entry_count: 2,
            skipped_row_count: 0,
        },
        "a scroll of 8 skips the newest 2 cards whole"
    );
    assert_eq!(
        notification_view
            .compute_center_body_rows(body_rect, 500)
            .row_start,
        NotificationCenterRowStart {
            skipped_entry_count: 25,
            skipped_row_count: 3,
        },
        "a scroll past the maximum offset 103 starts at row 103"
    );
    assert_eq!(
        notification_view
            .compute_center_body_rows(body_rect, 0)
            .row_start,
        NotificationCenterRowStart {
            skipped_entry_count: 0,
            skipped_row_count: 0,
        }
    );
}

#[test]
fn the_drawn_center_scroll_counts_only_the_cards_that_fill_the_body_below_it() {
    let notification_entries: VecDeque<NotificationEntry> = (0..30)
        .map(|test_entry_index| {
            build_test_notification_entry(
                test_entry_index,
                NotificationSeverity::Info,
                &format!("entry {test_entry_index}"),
                1,
            )
        })
        .collect();
    let mut center_entries = build_center_entries(&notification_entries);
    let body_rect = build_test_rect(57, 4, 22, 17);
    let compute_drawn_scroll = |center_entries: &NotificationCenterEntries,
                                dropped_entry_count,
                                body_rect,
                                scroll_row_offset| {
        NotificationView {
            center_entries,
            dropped_entry_count,
            shown_toast: None,
        }
        .compute_drawn_center_scroll_row_offset(body_rect, scroll_row_offset)
    };
    let count_counted_cards = |center_entries: &NotificationCenterEntries| {
        center_entries
            .counted_center_rows
            .borrow()
            .card_row_ends
            .len()
    };
    let list_wrapped_entry_indexes = |center_entries: &NotificationCenterEntries| {
        (0..center_entries.count_center_entries())
            .filter(|&entry_index| center_entries[entry_index].wrapped_text.borrow().is_some())
            .collect::<Vec<_>>()
    };

    assert_eq!(compute_drawn_scroll(&center_entries, 0, body_rect, 0), 0);
    assert_eq!(count_counted_cards(&center_entries), 0);
    assert_eq!(
        build_test_notification_view(&center_entries)
            .compute_center_body_rows(body_rect, 0)
            .card_rows
            .len(),
        17
    );
    assert_eq!(
        list_wrapped_entry_indexes(&center_entries),
        (25..30).collect::<Vec<_>>(),
        "a body of 17 rows at scroll 0 wraps only the 5 newest cards it draws"
    );

    assert_eq!(compute_drawn_scroll(&center_entries, 0, body_rect, 7), 7);
    assert_eq!(
        count_counted_cards(&center_entries),
        6,
        "rows 7 to 23 need the newest 6 cards of 4 rows"
    );
    assert_eq!(compute_drawn_scroll(&center_entries, 0, body_rect, 8), 8);
    assert_eq!(count_counted_cards(&center_entries), 7);
    assert_eq!(
        compute_drawn_scroll(&center_entries, 0, body_rect, 500),
        103,
        "120 card rows less a body of 17"
    );
    assert_eq!(count_counted_cards(&center_entries), 30);
    assert_eq!(
        compute_drawn_scroll(&center_entries, 3, body_rect, 500),
        106,
        "`3 older notifications were dropped` adds 3 rows"
    );
    assert_eq!(
        compute_drawn_scroll(&center_entries, 0, build_test_rect(57, 4, 22, 5), 500),
        115,
        "a body of 5 rows at the same width reads the counted cards"
    );

    compute_drawn_scroll(&center_entries, 0, build_test_rect(0, 4, 30, 17), 7);
    assert_eq!(
        count_counted_cards(&center_entries),
        6,
        "a body of another width counts its cards again"
    );

    compute_drawn_scroll(&center_entries, 0, body_rect, 500);
    center_entries.remove_center_entry_at(0);
    assert_eq!(count_counted_cards(&center_entries), 0);
    assert_eq!(
        compute_drawn_scroll(&center_entries, 0, body_rect, 500),
        99,
        "a removed card of 4 rows takes 4 from the last row drawn from"
    );
    center_entries.add_newest_center_entry(NotificationCenterEntry::from_notification_entry(
        notification_entries[0].clone(),
    ));
    assert_eq!(count_counted_cards(&center_entries), 0);
    assert_eq!(
        compute_drawn_scroll(&center_entries, 0, body_rect, 500),
        103
    );
    center_entries.clear_center_entries();
    assert_eq!(
        compute_drawn_scroll(&center_entries, 0, body_rect, 50),
        0,
        "a cleared center lists `No notifications` and draws from row 0"
    );
}

#[test]
fn center_body_rows_are_reused_until_the_body_size_scroll_dropped_count_or_entries_change() {
    let notification_entries = VecDeque::from([
        build_test_notification_entry(0, NotificationSeverity::Warning, "a", 1),
        build_test_notification_entry(1, NotificationSeverity::Error, "b", 1),
        build_test_notification_entry(2, NotificationSeverity::Info, "c", 1),
    ]);
    let mut center_entries =
        build_center_entries(&notification_entries.range(..2).cloned().collect());
    let body_rect = build_test_rect(57, 4, 22, 17);
    let compute_card_rows = |center_entries: &NotificationCenterEntries,
                             dropped_entry_count,
                             body_rect,
                             scroll_row_offset| {
        let notification_view = NotificationView {
            center_entries,
            dropped_entry_count,
            shown_toast: None,
        };
        notification_view
            .compute_center_body_rows(body_rect, scroll_row_offset)
            .card_rows
    };
    let build_card_rows = |entry_indexes: &[usize]| -> Vec<Option<NotificationCardRow>> {
        entry_indexes
            .iter()
            .flat_map(|&entry_index| {
                [true, false, false, false].map(|is_top_border| {
                    Some(NotificationCardRow {
                        notification_id: notification_entries[entry_index].notification_id,
                        is_top_border,
                    })
                })
            })
            .collect()
    };

    let first_card_rows = compute_card_rows(&center_entries, 0, body_rect, 0);
    assert_eq!(*first_card_rows, *build_card_rows(&[1, 0]));
    assert!(
        Arc::ptr_eq(
            &compute_card_rows(&center_entries, 0, body_rect, 0),
            &first_card_rows
        ),
        "a second ask with nothing changed reads the kept rows"
    );
    assert!(
        Arc::ptr_eq(
            &compute_card_rows(&center_entries, 0, build_test_rect(0, 1, 22, 17), 0),
            &first_card_rows
        ),
        "a body of the same size in another place reads the kept rows"
    );

    let scrolled_card_rows = compute_card_rows(&center_entries, 0, body_rect, 4);
    assert!(!Arc::ptr_eq(&scrolled_card_rows, &first_card_rows));
    assert_eq!(
        scrolled_card_rows, first_card_rows,
        "a scroll past the maximum offset 0 draws from row 0"
    );

    // Each step below changes one input after an ask with the first inputs.
    compute_card_rows(&center_entries, 0, body_rect, 0);
    assert_eq!(
        *compute_card_rows(&center_entries, 3, body_rect, 0),
        *[build_card_rows(&[1, 0]), vec![None, None, None]].concat(),
        "a new dropped count adds the 3 rows of `3 older notifications were dropped`"
    );
    compute_card_rows(&center_entries, 0, body_rect, 0);
    assert_eq!(
        *compute_card_rows(&center_entries, 0, build_test_rect(57, 4, 22, 5), 0),
        build_card_rows(&[1, 0])[..5],
        "a body of 5 rows draws 5 rows"
    );

    compute_card_rows(&center_entries, 0, body_rect, 0);
    center_entries.add_newest_center_entry(NotificationCenterEntry::from_notification_entry(
        notification_entries[2].clone(),
    ));
    assert_eq!(
        *compute_card_rows(&center_entries, 0, body_rect, 0),
        *build_card_rows(&[2, 1, 0]),
        "an added entry drops the kept rows"
    );
    center_entries.remove_center_entry_at(0);
    assert_eq!(
        *compute_card_rows(&center_entries, 0, body_rect, 0),
        *build_card_rows(&[2, 1]),
        "a removed entry drops the kept rows"
    );
    center_entries.clear_center_entries();
    assert_eq!(
        *compute_card_rows(&center_entries, 0, body_rect, 0),
        [None],
        "a clear drops the kept rows: the center lists `No notifications`"
    );
}

#[test]
fn the_drawn_card_under_a_body_cell_names_its_entry_and_its_dismiss_button() {
    let render_snapshot = build_notification_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE);
    let center_layout =
        find_test_center_layout(&render_snapshot, build_open_center_viewer_chrome())
            .expect("an open center is drawn");
    let newer_notification_id =
        build_test_notification_entry(1, NotificationSeverity::Error, "b", 1).notification_id;
    let older_notification_id =
        build_test_notification_entry(0, NotificationSeverity::Warning, "a", 1).notification_id;
    let card_rows = [
        (newer_notification_id, true),
        (newer_notification_id, false),
        (newer_notification_id, false),
        (newer_notification_id, false),
        (older_notification_id, true),
    ]
    .map(|(notification_id, is_top_border)| {
        Some(NotificationCardRow {
            notification_id,
            is_top_border,
        })
    });
    let find_card_at =
        |column, row| find_notification_card_at(&card_rows, &center_layout, Point { column, row });

    assert_eq!(
        compute_card_dismiss_button_column(center_layout.body_rect),
        75
    );
    assert_eq!(
        find_card_at(75, 4),
        Some(NotificationCardHit {
            notification_id: newer_notification_id,
            is_on_dismiss_button: true,
        })
    );
    assert_eq!(
        find_card_at(74, 4),
        Some(NotificationCardHit {
            notification_id: newer_notification_id,
            is_on_dismiss_button: false,
        })
    );
    assert_eq!(
        find_card_at(75, 5),
        Some(NotificationCardHit {
            notification_id: newer_notification_id,
            is_on_dismiss_button: false,
        })
    );
    assert_eq!(
        find_card_at(75, 8),
        Some(NotificationCardHit {
            notification_id: older_notification_id,
            is_on_dismiss_button: true,
        })
    );
    assert_eq!(find_card_at(60, 9), None, "body row 5 has no item");
    assert_eq!(find_card_at(60, 2), None, "the titlebar is off the body");
}

#[test]
fn the_drawn_center_scroll_stops_at_the_row_that_puts_the_last_row_on_the_last_body_row() {
    let notification_entries: VecDeque<NotificationEntry> = (0..30)
        .map(|test_entry_index| {
            build_test_notification_entry(
                test_entry_index,
                NotificationSeverity::Info,
                &format!("entry {test_entry_index}"),
                1,
            )
        })
        .collect();
    let center_entries = build_center_entries(&notification_entries);
    let body_rect = build_test_rect(57, 4, 22, 17);

    let thirty_entries = NotificationView {
        center_entries: &center_entries,
        dropped_entry_count: 0,
        shown_toast: None,
    }
    .compute_drawn_center_scroll_row_offset(body_rect, usize::MAX);
    let thirty_entries_and_a_dropped_count = NotificationView {
        center_entries: &center_entries,
        dropped_entry_count: 3,
        shown_toast: None,
    }
    .compute_drawn_center_scroll_row_offset(body_rect, usize::MAX);
    let two_entries = NotificationView {
        center_entries: &build_center_entries(&notification_entries.range(..2).cloned().collect()),
        dropped_entry_count: 0,
        shown_toast: None,
    }
    .compute_drawn_center_scroll_row_offset(body_rect, usize::MAX);

    assert_eq!(thirty_entries, 103);
    assert_eq!(thirty_entries_and_a_dropped_count, 106);
    assert_eq!(two_entries, 0);
}
