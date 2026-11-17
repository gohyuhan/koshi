//! Tests for the client chip pieces that the tab strip and the pane chips
//! share: the width of a chip and of the `+N` mark, the packing of the chips
//! into a room, and the group that `solve_client_chip_group` places.

use super::*;

#[test]
fn a_chip_width_counts_the_digits_of_the_client_number_and_two_blank_cells() {
    assert_eq!(
        [0, 9, 10, u64::MAX].map(count_decimal_digits),
        [1, 1, 2, 20]
    );
    assert_eq!(
        [0, 8, 9, 11, u32::MAX].map(compute_client_chip_width),
        [3, 3, 4, 4, 12]
    );
}

#[test]
fn a_hidden_client_mark_takes_the_plus_sign_and_the_digits_of_the_count() {
    assert_eq!(
        [1, 9, 10, 99, 100].map(compute_hidden_client_mark_width),
        [2, 2, 3, 3, 4]
    );
}

#[test]
fn packing_stops_at_the_first_chip_that_does_not_fit() {
    assert_eq!(
        pack_client_chips([(0, true), (9, false), (10, false)].into_iter(), 9),
        (vec![(0, true, 0), (9, false, 4)], 8)
    );
    assert_eq!(
        pack_client_chips([(0, true), (1, false)].into_iter(), 2),
        (Vec::new(), 0)
    );
}

#[test]
fn a_group_with_no_client_is_none() {
    assert_eq!(solve_client_chip_group(std::iter::empty(), 0, 20, 40), None);
}

#[test]
fn a_group_whose_chips_fit_ends_at_the_end_column_with_no_mark() {
    assert_eq!(
        solve_client_chip_group([(1, false), (3, false)].into_iter(), 2, 10, 40),
        Some(ClientChipGroup {
            start_column: 33,
            client_chip_spans: vec![
                ClientChipSpan {
                    color_index: 1,
                    is_viewer_chip: false,
                    start_column: 33,
                },
                ClientChipSpan {
                    color_index: 3,
                    is_viewer_chip: false,
                    start_column: 37,
                },
            ],
            hidden_client_mark: None,
        })
    );
}

#[test]
fn a_room_too_small_for_every_chip_drops_chips_from_the_end_into_the_mark() {
    assert_eq!(
        solve_client_chip_group([(1, false), (2, false), (3, false)].into_iter(), 3, 6, 40),
        Some(ClientChipGroup {
            start_column: 34,
            client_chip_spans: vec![ClientChipSpan {
                color_index: 1,
                is_viewer_chip: false,
                start_column: 34,
            }],
            hidden_client_mark: Some(HiddenClientMark {
                start_column: 38,
                client_count: 2,
            }),
        })
    );
}

#[test]
fn a_room_for_the_mark_alone_counts_every_client_and_a_smaller_room_gives_none() {
    assert_eq!(
        solve_client_chip_group([(1, false), (2, false)].into_iter(), 2, 2, 40),
        Some(ClientChipGroup {
            start_column: 38,
            client_chip_spans: Vec::new(),
            hidden_client_mark: Some(HiddenClientMark {
                start_column: 38,
                client_count: 2,
            }),
        })
    );
    assert_eq!(
        solve_client_chip_group([(1, false), (2, false)].into_iter(), 2, 1, 40),
        None
    );
}
