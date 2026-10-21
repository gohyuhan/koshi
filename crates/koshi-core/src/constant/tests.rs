//! The heartbeat interval a link timeout gives, the frame interval a frame
//! rate gives, and the viewport size the cell limit leaves.

use super::*;

#[test]
fn the_heartbeat_interval_is_a_third_of_the_link_timeout() {
    assert_eq!(compute_heartbeat_interval(30_000), Duration::from_secs(10));
    assert_eq!(
        compute_heartbeat_interval(MIN_LINK_TIMEOUT_MS),
        Duration::from_nanos(333_333_333)
    );
    assert_eq!(
        compute_heartbeat_interval(u32::MAX),
        Duration::from_millis(1_431_655_765)
    );
}

#[test]
fn the_frame_interval_is_one_second_divided_by_the_frame_rate_rounded_up() {
    assert_eq!(
        compute_frame_interval(DEFAULT_MAX_FRAMES_PER_SECOND),
        Duration::from_nanos(16_666_667)
    );
    assert_eq!(
        compute_frame_interval(HIGHEST_MAX_FRAMES_PER_SECOND),
        Duration::from_nanos(8_333_334)
    );
    assert_eq!(
        compute_frame_interval(LOWEST_MAX_FRAMES_PER_SECOND),
        Duration::from_nanos(33_333_334)
    );
    assert_eq!(compute_frame_interval(100), Duration::from_millis(10));
}

#[test]
fn a_viewport_over_the_cell_limit_keeps_its_columns_and_loses_the_rows_past_it() {
    let build_size = |column_count: u16, row_count: u16| Size {
        column_count,
        row_count,
    };
    assert_eq!(
        limit_viewport_size(build_size(200, 50)),
        build_size(200, 50)
    );
    assert_eq!(
        limit_viewport_size(build_size(2048, 1024)),
        build_size(2048, 1024)
    );
    assert_eq!(
        limit_viewport_size(build_size(2048, 1025)),
        build_size(2048, 1024)
    );
    assert_eq!(
        limit_viewport_size(build_size(32, 65_535)),
        build_size(32, 65_535)
    );
    assert_eq!(
        limit_viewport_size(build_size(33, 65_535)),
        build_size(33, 63_550)
    );
    assert_eq!(
        limit_viewport_size(build_size(65_535, 65_535)),
        build_size(65_535, 32)
    );
    assert_eq!(
        limit_viewport_size(build_size(0, 65_535)),
        build_size(0, 65_535)
    );
}
