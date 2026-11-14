//! Tests for the number a client's `color_index` prints as.

use super::*;

#[test]
fn every_color_index_prints_as_one_more_than_itself() {
    for (color_index, expected_client_number) in
        [(0, 1), (1, 2), (11, 12), (u32::MAX, 4_294_967_296)]
    {
        assert_eq!(
            compute_client_number(color_index),
            expected_client_number,
            "number of color index {color_index}"
        );
    }
}
