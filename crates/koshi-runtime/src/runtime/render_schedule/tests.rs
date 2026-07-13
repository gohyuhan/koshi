//! Coverage for the render scheduler's coalescing, dual-cadence gating, and
//! idle-wakeup behavior. Time is synthetic: every case builds its own timeline
//! from one seed [`Instant`] plus fixed offsets, so the gate is exercised
//! without sleeping or reading the real clock.

use super::*;

/// Every reason the scheduler accepts, for exhaustive per-reason coverage.
const ALL_REASONS: [InvalidationReason; 8] = [
    InvalidationReason::PtyOutput,
    InvalidationReason::LayoutChanged,
    InvalidationReason::FocusChanged,
    InvalidationReason::TabChanged,
    InvalidationReason::TerminalResize,
    InvalidationReason::StatusChanged,
    InvalidationReason::PluginUiUpdated,
    InvalidationReason::BlinkTick,
];

/// Milliseconds after the seed instant.
fn at(seed: Instant, ms: u64) -> Instant {
    seed + Duration::from_millis(ms)
}

#[test]
fn fresh_scheduler_has_nothing_pending() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    assert!(!s.poll(t0));
    assert_eq!(s.next_wakeup(t0), None);
}

#[test]
fn every_reason_renders_immediately_on_first_invalidation() {
    let t0 = Instant::now();
    for reason in ALL_REASONS {
        let mut s = RenderScheduler::new();
        s.invalidate(reason);
        assert!(s.poll(t0), "{reason:?} should render on the first poll");
    }
}

#[test]
fn poll_clears_pending_so_an_immediate_second_poll_is_false() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    s.invalidate(InvalidationReason::PtyOutput);
    assert!(s.poll(t0));
    assert!(!s.poll(t0));
}

#[test]
fn a_burst_of_invalidations_coalesces_into_one_render() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    s.invalidate(InvalidationReason::PtyOutput);
    s.invalidate(InvalidationReason::PtyOutput);
    s.invalidate(InvalidationReason::PtyOutput);
    s.invalidate(InvalidationReason::LayoutChanged);
    assert!(s.poll(t0));
    assert!(!s.poll(t0));
}

#[test]
fn a_real_reason_gates_at_the_frame_interval() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    s.invalidate(InvalidationReason::PtyOutput);
    assert!(s.poll(t0));

    s.invalidate(InvalidationReason::PtyOutput);
    assert!(!s.poll(at(t0, 7)), "too soon: 7 ms < 8 ms frame interval");
    assert!(s.poll(at(t0, 8)), "8 ms frame interval elapsed");
}

#[test]
fn blink_only_gates_at_the_slow_blink_interval() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    s.invalidate(InvalidationReason::BlinkTick);
    assert!(s.poll(t0));

    s.invalidate(InvalidationReason::BlinkTick);
    assert!(
        !s.poll(at(t0, 8)),
        "frame interval must not fire a blink-only render"
    );
    assert!(
        !s.poll(at(t0, 249)),
        "too soon: 249 ms < 250 ms blink interval"
    );
    assert!(s.poll(at(t0, 250)), "250 ms blink interval elapsed");
}

#[test]
fn a_real_reason_alongside_blink_uses_the_fast_cadence() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    s.invalidate(InvalidationReason::PtyOutput);
    assert!(s.poll(t0));

    s.invalidate(InvalidationReason::BlinkTick);
    s.invalidate(InvalidationReason::PtyOutput);
    assert!(!s.poll(at(t0, 7)));
    assert!(
        s.poll(at(t0, 8)),
        "a real reason forces the 8 ms gate even with blink pending"
    );
}

#[test]
fn idle_five_seconds_of_blink_ticks_renders_twenty_times() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    // Establish the baseline frame at t0, then measure the next 5 s.
    s.invalidate(InvalidationReason::BlinkTick);
    assert!(s.poll(t0));

    // Poll every 50 ms — finer than the blink cadence, so the count measures
    // the gate rather than the sampling grain.
    let mut renders = 0;
    let mut ms = 50;
    while ms <= 5000 {
        s.invalidate(InvalidationReason::BlinkTick);
        if s.poll(at(t0, ms)) {
            renders += 1;
        }
        ms += 50;
    }
    assert_eq!(
        renders, 20,
        "blink coalesces to one render per 250 ms over 5 s"
    );
}

#[test]
fn next_wakeup_is_none_when_nothing_is_pending() {
    let t0 = Instant::now();
    let s = RenderScheduler::new();
    assert_eq!(s.next_wakeup(t0), None);
}

#[test]
fn next_wakeup_is_zero_before_the_first_render() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    s.invalidate(InvalidationReason::PtyOutput);
    assert_eq!(s.next_wakeup(t0), Some(Duration::ZERO));
}

#[test]
fn next_wakeup_reports_the_remaining_frame_time() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    s.invalidate(InvalidationReason::PtyOutput);
    assert!(s.poll(t0));

    s.invalidate(InvalidationReason::PtyOutput);
    assert_eq!(s.next_wakeup(at(t0, 3)), Some(Duration::from_millis(5)));
}

#[test]
fn next_wakeup_reports_the_remaining_blink_time() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    s.invalidate(InvalidationReason::BlinkTick);
    assert!(s.poll(t0));

    s.invalidate(InvalidationReason::BlinkTick);
    assert_eq!(s.next_wakeup(at(t0, 100)), Some(Duration::from_millis(150)));
}

#[test]
fn next_wakeup_saturates_to_zero_when_already_due() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    s.invalidate(InvalidationReason::PtyOutput);
    assert!(s.poll(t0));

    s.invalidate(InvalidationReason::PtyOutput);
    assert_eq!(s.next_wakeup(at(t0, 20)), Some(Duration::ZERO));
}
