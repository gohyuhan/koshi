//! Tests for floating chrome geometry: the content rect inside the chrome of
//! the tray and of the notification center, never below zero; their
//! titlebar parts at every width where a part appears or drops out; a floating
//! pane's titlebar strip parts at every width where a part appears, at a moved
//! origin, and on a box with no row; and where a floating pane's layout area
//! starts.

use super::*;

use koshi_test_support::fixtures::build_cell_rect;

/// The titlebar parts of an `outer_column_count`-column, 12-row outer rect at
/// `(0, 0)`.
fn compute_titlebar_at_width(outer_column_count: u16) -> FloatingTitlebar {
    compute_floating_titlebar(build_cell_rect(0, 0, outer_column_count, 12))
}

/// A titlebar with no parts.
fn build_empty_titlebar() -> FloatingTitlebar {
    FloatingTitlebar {
        title_rect: None,
        close_button_rect: None,
    }
}

/// The strip parts of an `outer_column_count`-column, 12-row floating pane at
/// `(0, 0)`.
fn compute_strip_at_width(outer_column_count: u16) -> FloatingPaneStrip {
    compute_floating_pane_strip(build_cell_rect(0, 0, outer_column_count, 12))
}

/// A strip with no parts.
fn build_empty_strip() -> FloatingPaneStrip {
    FloatingPaneStrip {
        grab_handle_rect: None,
        pin_toggle_rect: None,
        minimize_button_rect: None,
        close_button_rect: None,
    }
}

#[test]
fn content_rect_starts_inside_left_border_below_separator() {
    assert_eq!(
        compute_floating_content_rect(build_cell_rect(24, 8, 72, 22)),
        build_cell_rect(25, 11, 70, 18)
    );
}

#[test]
fn content_rect_of_chrome_sized_outer_is_empty() {
    assert_eq!(
        compute_floating_content_rect(build_cell_rect(3, 2, 2, 4)),
        build_cell_rect(4, 5, 0, 0)
    );
}

#[test]
fn content_rect_origin_saturates_at_largest_cell() {
    assert_eq!(
        compute_floating_content_rect(build_cell_rect(u16::MAX, u16::MAX - 1, 2, 4)),
        build_cell_rect(u16::MAX, u16::MAX, 0, 0)
    );
}

#[test]
fn titlebar_at_width_five_has_one_title_cell_and_no_close_button() {
    assert_eq!(
        compute_titlebar_at_width(5),
        FloatingTitlebar {
            title_rect: Some(build_cell_rect(2, 1, 1, 1)),
            ..build_empty_titlebar()
        }
    );
}

#[test]
fn titlebar_at_width_six_shows_close_button_and_no_title() {
    assert_eq!(
        compute_titlebar_at_width(6),
        FloatingTitlebar {
            close_button_rect: Some(build_cell_rect(2, 1, 1, 1)),
            ..build_empty_titlebar()
        }
    );
}

#[test]
fn titlebar_at_width_nine_shows_one_title_cell_from_column_two() {
    assert_eq!(
        compute_titlebar_at_width(9),
        FloatingTitlebar {
            title_rect: Some(build_cell_rect(2, 1, 1, 1)),
            close_button_rect: Some(build_cell_rect(5, 1, 1, 1)),
        }
    );
}

#[test]
fn titlebar_at_width_twenty_four_matches_documented_layout() {
    assert_eq!(
        compute_titlebar_at_width(24),
        FloatingTitlebar {
            title_rect: Some(build_cell_rect(2, 1, 16, 1)),
            close_button_rect: Some(build_cell_rect(20, 1, 1, 1)),
        }
    );
}

#[test]
fn titlebar_of_outer_shorter_than_four_rows_has_no_parts() {
    assert_eq!(
        compute_floating_titlebar(build_cell_rect(0, 0, 24, 3)),
        build_empty_titlebar()
    );
}

#[test]
fn strip_at_width_two_has_no_parts() {
    assert_eq!(compute_strip_at_width(2), build_empty_strip());
}

#[test]
fn strip_at_width_three_has_only_the_grab_handle() {
    assert_eq!(
        compute_strip_at_width(3),
        FloatingPaneStrip {
            grab_handle_rect: Some(build_cell_rect(1, 0, 2, 1)),
            ..build_empty_strip()
        }
    );
}

#[test]
fn strip_at_width_six_has_no_control() {
    assert_eq!(
        compute_strip_at_width(6),
        FloatingPaneStrip {
            grab_handle_rect: Some(build_cell_rect(1, 0, 2, 1)),
            ..build_empty_strip()
        }
    );
}

#[test]
fn strip_at_width_seven_adds_close_button_at_column_five() {
    assert_eq!(
        compute_strip_at_width(7),
        FloatingPaneStrip {
            grab_handle_rect: Some(build_cell_rect(1, 0, 2, 1)),
            close_button_rect: Some(build_cell_rect(5, 0, 1, 1)),
            ..build_empty_strip()
        }
    );
}

#[test]
fn strip_at_width_ten_adds_minimize_button_at_column_five() {
    assert_eq!(
        compute_strip_at_width(10),
        FloatingPaneStrip {
            grab_handle_rect: Some(build_cell_rect(1, 0, 2, 1)),
            minimize_button_rect: Some(build_cell_rect(5, 0, 1, 1)),
            close_button_rect: Some(build_cell_rect(8, 0, 1, 1)),
            ..build_empty_strip()
        }
    );
}

#[test]
fn strip_at_width_fourteen_has_no_pin_toggle() {
    assert_eq!(
        compute_strip_at_width(14),
        FloatingPaneStrip {
            grab_handle_rect: Some(build_cell_rect(1, 0, 2, 1)),
            minimize_button_rect: Some(build_cell_rect(9, 0, 1, 1)),
            close_button_rect: Some(build_cell_rect(12, 0, 1, 1)),
            ..build_empty_strip()
        }
    );
}

#[test]
fn strip_at_width_fifteen_adds_pin_toggle_at_column_five() {
    assert_eq!(
        compute_strip_at_width(15),
        FloatingPaneStrip {
            grab_handle_rect: Some(build_cell_rect(1, 0, 2, 1)),
            pin_toggle_rect: Some(build_cell_rect(5, 0, 4, 1)),
            minimize_button_rect: Some(build_cell_rect(10, 0, 1, 1)),
            close_button_rect: Some(build_cell_rect(13, 0, 1, 1)),
        }
    );
}

#[test]
fn strip_at_width_thirty_eight_matches_documented_layout() {
    assert_eq!(
        compute_strip_at_width(38),
        FloatingPaneStrip {
            grab_handle_rect: Some(build_cell_rect(1, 0, 2, 1)),
            pin_toggle_rect: Some(build_cell_rect(28, 0, 4, 1)),
            minimize_button_rect: Some(build_cell_rect(33, 0, 1, 1)),
            close_button_rect: Some(build_cell_rect(36, 0, 1, 1)),
        }
    );
}

#[test]
fn strip_parts_follow_outer_rect_origin() {
    assert_eq!(
        compute_floating_pane_strip(build_cell_rect(24, 8, 38, 12)),
        FloatingPaneStrip {
            grab_handle_rect: Some(build_cell_rect(25, 8, 2, 1)),
            pin_toggle_rect: Some(build_cell_rect(52, 8, 4, 1)),
            minimize_button_rect: Some(build_cell_rect(57, 8, 1, 1)),
            close_button_rect: Some(build_cell_rect(60, 8, 1, 1)),
        }
    );
}

#[test]
fn strip_of_outer_with_no_row_has_no_parts() {
    assert_eq!(
        compute_floating_pane_strip(build_cell_rect(0, 0, 38, 0)),
        build_empty_strip()
    );
}

#[test]
fn strip_of_one_row_outer_has_every_part() {
    assert_eq!(
        compute_floating_pane_strip(build_cell_rect(0, 0, 38, 1)),
        compute_strip_at_width(38)
    );
}

#[test]
fn layout_origin_is_the_first_cell_below_the_strip() {
    assert_eq!(
        compute_floating_pane_layout_origin(build_cell_rect(24, 8, 38, 12)),
        Point { column: 24, row: 9 }
    );
}

#[test]
fn layout_origin_row_saturates_at_largest_cell() {
    assert_eq!(
        compute_floating_pane_layout_origin(build_cell_rect(3, u16::MAX, 38, 0)),
        Point {
            column: 3,
            row: u16::MAX,
        }
    );
}
