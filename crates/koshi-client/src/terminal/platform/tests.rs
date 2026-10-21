//! Tests for the helpers both platform terminal backends share.

use super::*;

#[test]
fn the_shorter_timeout_wins_and_none_is_no_limit() {
    let shorter_timeout_duration = Some(Duration::from_millis(5));
    let longer_timeout_duration = Some(Duration::from_millis(25));
    assert_eq!(
        choose_shorter_timeout_duration(shorter_timeout_duration, longer_timeout_duration),
        shorter_timeout_duration
    );
    assert_eq!(
        choose_shorter_timeout_duration(longer_timeout_duration, shorter_timeout_duration),
        shorter_timeout_duration
    );
    assert_eq!(
        choose_shorter_timeout_duration(longer_timeout_duration, None),
        longer_timeout_duration
    );
    assert_eq!(
        choose_shorter_timeout_duration(None, shorter_timeout_duration),
        shorter_timeout_duration
    );
    assert_eq!(choose_shorter_timeout_duration(None, None), None);
}
