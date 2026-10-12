//! The heartbeat interval a link timeout gives.

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
