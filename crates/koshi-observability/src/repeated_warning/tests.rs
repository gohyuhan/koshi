//! Tests for [`WarningRateLimiter`]: a repeated warning is written once per
//! [`LOG_WINDOW_DURATION`], and the first one at once.

use super::*;

#[test]
fn a_repeated_warning_is_written_once_per_window() {
    let mut warning_rate_limiter = WarningRateLimiter::new();
    let window_started_at = Instant::now();

    assert!(
        warning_rate_limiter.record_warning_if_due(window_started_at),
        "the first refusal says so"
    );
    for refusal_index in 0..50 {
        assert!(
            !warning_rate_limiter.record_warning_if_due(
                window_started_at
                    + Duration::from_secs(refusal_index % LOG_WINDOW_DURATION.as_secs())
            ),
            "refusal {refusal_index} inside the same window is silent"
        );
    }

    let after_window_end = window_started_at + LOG_WINDOW_DURATION + Duration::from_secs(1);
    assert!(
        warning_rate_limiter.record_warning_if_due(after_window_end),
        "a limit still refusing says so again"
    );
    assert!(
        !warning_rate_limiter.record_warning_if_due(after_window_end),
        "and then goes quiet again"
    );
}

#[test]
fn a_warning_that_has_never_been_written_is_due_at_once() {
    // No line has been written: there is no window to be inside of.
    let mut warning_rate_limiter = WarningRateLimiter::new();
    assert!(warning_rate_limiter.record_warning_if_due(Instant::now()));
}

#[test]
fn a_warning_written_exactly_one_window_ago_is_written_again() {
    // The quiet spell ends at LOG_WINDOW_DURATION, not one moment after it.
    let window_started_at = Instant::now();

    let mut warning_within_window = WarningRateLimiter::new();
    assert!(warning_within_window.record_warning_if_due(window_started_at));
    assert!(
        !warning_within_window.record_warning_if_due(
            window_started_at + LOG_WINDOW_DURATION - Duration::from_millis(1)
        ),
        "one millisecond before the window ends the warning is still silent"
    );

    let mut warning_at_window_end = WarningRateLimiter::new();
    assert!(warning_at_window_end.record_warning_if_due(window_started_at));
    assert!(
        warning_at_window_end.record_warning_if_due(window_started_at + LOG_WINDOW_DURATION),
        "at exactly {LOG_WINDOW_DURATION:?} the warning is written again"
    );
}
