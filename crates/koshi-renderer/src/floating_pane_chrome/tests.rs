//! Tests for floating pane chrome geometry: the content size and rect inside
//! the chrome, never below zero, and the titlebar parts at every width where a
//! part appears or drops out, at a moved origin, and on a pane too short for a
//! titlebar.

use super::*;

use koshi_test_support::fixtures::build_cell_rect;

/// The titlebar parts of an `outer_column_count`-column, 12-row outer rect at
/// `(0, 0)`.
fn compute_titlebar_at_width(outer_column_count: u16) -> FloatingPaneTitlebar {
    compute_floating_pane_titlebar(build_cell_rect(0, 0, outer_column_count, 12))
}

/// A titlebar with no parts.
fn build_empty_titlebar() -> FloatingPaneTitlebar {
    FloatingPaneTitlebar {
        grab_handle_rect: None,
        title_rect: None,
        pin_toggle_rect: None,
        minimize_button_rect: None,
        close_button_rect: None,
    }
}

#[test]
fn content_size_removes_chrome_from_each_axis() {
    assert_eq!(
        compute_floating_pane_content_size(Size {
            column_count: 40,
            row_count: 12,
        }),
        Size {
            column_count: 38,
            row_count: 8,
        }
    );
}

#[test]
fn content_size_of_outer_smaller_than_chrome_is_zero() {
    assert_eq!(
        compute_floating_pane_content_size(Size {
            column_count: 1,
            row_count: 3,
        }),
        Size {
            column_count: 0,
            row_count: 0,
        }
    );
}

#[test]
fn content_rect_starts_inside_left_border_below_separator() {
    assert_eq!(
        compute_floating_pane_content_rect(build_cell_rect(24, 8, 72, 22)),
        build_cell_rect(25, 11, 70, 18)
    );
}

#[test]
fn content_rect_of_chrome_sized_outer_is_empty() {
    assert_eq!(
        compute_floating_pane_content_rect(build_cell_rect(3, 2, 2, 4)),
        build_cell_rect(4, 5, 0, 0)
    );
}

#[test]
fn content_rect_origin_saturates_at_largest_cell() {
    assert_eq!(
        compute_floating_pane_content_rect(build_cell_rect(u16::MAX, u16::MAX - 1, 2, 4)),
        build_cell_rect(u16::MAX, u16::MAX, 0, 0)
    );
}

#[test]
fn titlebar_at_width_four_has_no_parts() {
    assert_eq!(compute_titlebar_at_width(4), build_empty_titlebar());
}

#[test]
fn titlebar_at_width_five_has_only_the_grab_handle() {
    assert_eq!(
        compute_titlebar_at_width(5),
        FloatingPaneTitlebar {
            grab_handle_rect: Some(build_cell_rect(2, 1, 2, 1)),
            ..build_empty_titlebar()
        }
    );
}

#[test]
fn titlebar_at_width_eight_has_no_title_room() {
    assert_eq!(
        compute_titlebar_at_width(8),
        FloatingPaneTitlebar {
            grab_handle_rect: Some(build_cell_rect(2, 1, 2, 1)),
            ..build_empty_titlebar()
        }
    );
}

#[test]
fn titlebar_at_width_nine_has_one_title_cell_and_no_control() {
    assert_eq!(
        compute_titlebar_at_width(9),
        FloatingPaneTitlebar {
            grab_handle_rect: Some(build_cell_rect(2, 1, 2, 1)),
            title_rect: Some(build_cell_rect(6, 1, 1, 1)),
            ..build_empty_titlebar()
        }
    );
}

#[test]
fn titlebar_at_width_ten_shows_close_button_and_no_title() {
    assert_eq!(
        compute_titlebar_at_width(10),
        FloatingPaneTitlebar {
            grab_handle_rect: Some(build_cell_rect(2, 1, 2, 1)),
            close_button_rect: Some(build_cell_rect(6, 1, 1, 1)),
            ..build_empty_titlebar()
        }
    );
}

#[test]
fn titlebar_at_width_twelve_keeps_close_button_only() {
    assert_eq!(
        compute_titlebar_at_width(12),
        FloatingPaneTitlebar {
            grab_handle_rect: Some(build_cell_rect(2, 1, 2, 1)),
            close_button_rect: Some(build_cell_rect(8, 1, 1, 1)),
            ..build_empty_titlebar()
        }
    );
}

#[test]
fn titlebar_at_width_thirteen_adds_minimize_button() {
    assert_eq!(
        compute_titlebar_at_width(13),
        FloatingPaneTitlebar {
            grab_handle_rect: Some(build_cell_rect(2, 1, 2, 1)),
            minimize_button_rect: Some(build_cell_rect(6, 1, 1, 1)),
            close_button_rect: Some(build_cell_rect(9, 1, 1, 1)),
            ..build_empty_titlebar()
        }
    );
}

#[test]
fn titlebar_at_width_seventeen_fits_two_title_cells_before_minimize() {
    assert_eq!(
        compute_titlebar_at_width(17),
        FloatingPaneTitlebar {
            grab_handle_rect: Some(build_cell_rect(2, 1, 2, 1)),
            title_rect: Some(build_cell_rect(6, 1, 2, 1)),
            minimize_button_rect: Some(build_cell_rect(10, 1, 1, 1)),
            close_button_rect: Some(build_cell_rect(13, 1, 1, 1)),
            ..build_empty_titlebar()
        }
    );
}

#[test]
fn titlebar_at_width_eighteen_adds_pin_toggle_and_drops_title() {
    assert_eq!(
        compute_titlebar_at_width(18),
        FloatingPaneTitlebar {
            grab_handle_rect: Some(build_cell_rect(2, 1, 2, 1)),
            title_rect: None,
            pin_toggle_rect: Some(build_cell_rect(6, 1, 4, 1)),
            minimize_button_rect: Some(build_cell_rect(11, 1, 1, 1)),
            close_button_rect: Some(build_cell_rect(14, 1, 1, 1)),
        }
    );
}

#[test]
fn titlebar_at_width_twenty_has_every_control_and_no_title() {
    assert_eq!(
        compute_titlebar_at_width(20),
        FloatingPaneTitlebar {
            grab_handle_rect: Some(build_cell_rect(2, 1, 2, 1)),
            title_rect: None,
            pin_toggle_rect: Some(build_cell_rect(8, 1, 4, 1)),
            minimize_button_rect: Some(build_cell_rect(13, 1, 1, 1)),
            close_button_rect: Some(build_cell_rect(16, 1, 1, 1)),
        }
    );
}

#[test]
fn titlebar_at_width_twenty_one_shows_title_with_every_control() {
    assert_eq!(
        compute_titlebar_at_width(21),
        FloatingPaneTitlebar {
            grab_handle_rect: Some(build_cell_rect(2, 1, 2, 1)),
            title_rect: Some(build_cell_rect(6, 1, 1, 1)),
            pin_toggle_rect: Some(build_cell_rect(9, 1, 4, 1)),
            minimize_button_rect: Some(build_cell_rect(14, 1, 1, 1)),
            close_button_rect: Some(build_cell_rect(17, 1, 1, 1)),
        }
    );
}

#[test]
fn titlebar_at_width_forty_eight_matches_documented_layout() {
    assert_eq!(
        compute_titlebar_at_width(48),
        FloatingPaneTitlebar {
            grab_handle_rect: Some(build_cell_rect(2, 1, 2, 1)),
            title_rect: Some(build_cell_rect(6, 1, 28, 1)),
            pin_toggle_rect: Some(build_cell_rect(36, 1, 4, 1)),
            minimize_button_rect: Some(build_cell_rect(41, 1, 1, 1)),
            close_button_rect: Some(build_cell_rect(44, 1, 1, 1)),
        }
    );
}

#[test]
fn titlebar_parts_follow_outer_rect_origin() {
    assert_eq!(
        compute_floating_pane_titlebar(build_cell_rect(24, 8, 48, 12)),
        FloatingPaneTitlebar {
            grab_handle_rect: Some(build_cell_rect(26, 9, 2, 1)),
            title_rect: Some(build_cell_rect(30, 9, 28, 1)),
            pin_toggle_rect: Some(build_cell_rect(60, 9, 4, 1)),
            minimize_button_rect: Some(build_cell_rect(65, 9, 1, 1)),
            close_button_rect: Some(build_cell_rect(68, 9, 1, 1)),
        }
    );
}

#[test]
fn titlebar_of_outer_shorter_than_four_rows_has_no_parts() {
    assert_eq!(
        compute_floating_pane_titlebar(build_cell_rect(0, 0, 48, 3)),
        build_empty_titlebar()
    );
}

#[test]
fn titlebar_of_four_row_outer_has_every_part() {
    assert_eq!(
        compute_floating_pane_titlebar(build_cell_rect(0, 0, 48, 4)),
        compute_titlebar_at_width(48)
    );
}
