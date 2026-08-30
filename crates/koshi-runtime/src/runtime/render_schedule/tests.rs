//! Coverage for the render scheduler's coalescing, frame gating, and
//! idle-wakeup behavior. Time is synthetic: every case builds its own timeline
//! from one seed [`Instant`] plus fixed offsets, so the gate is exercised
//! without sleeping or reading the real clock.

use super::*;

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
fn the_default_scheduler_starts_like_a_new_one() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::default();
    assert!(!s.poll(t0));
    assert_eq!(s.next_wakeup(t0), None);
}

#[test]
fn the_first_invalidation_renders_immediately() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    s.invalidate();
    assert!(s.poll(t0));
}

#[test]
fn poll_clears_pending_so_an_immediate_second_poll_is_false() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    s.invalidate();
    assert!(s.poll(t0));
    assert!(!s.poll(t0));
}

#[test]
fn a_burst_of_invalidations_coalesces_into_one_render() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    s.invalidate();
    s.invalidate();
    s.invalidate();
    s.invalidate();
    assert!(s.poll(t0));
    assert!(!s.poll(t0));
}

#[test]
fn a_pending_change_gates_at_the_frame_interval() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    s.invalidate();
    assert!(s.poll(t0));

    s.invalidate();
    assert!(!s.poll(at(t0, 7)), "too soon: 7 ms < 8 ms frame interval");
    assert!(s.poll(at(t0, 8)), "8 ms frame interval elapsed");
}

#[test]
fn a_poll_earlier_than_the_last_render_is_not_due() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    s.invalidate();
    assert!(s.poll(at(t0, 100)));

    s.invalidate();
    assert!(!s.poll(t0));
    assert_eq!(s.next_wakeup(t0), Some(FRAME_INTERVAL));
}

#[test]
fn five_seconds_of_invalidations_render_at_the_frame_cadence() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    // Establish the baseline frame at t0, then measure the next 5 s.
    s.invalidate();
    assert!(s.poll(t0));

    // Poll every 50 ms — coarser than the 8 ms cadence, so every poll is due.
    let mut renders = 0;
    let mut ms = 50;
    while ms <= 5000 {
        s.invalidate();
        if s.poll(at(t0, ms)) {
            renders += 1;
        }
        ms += 50;
    }
    assert_eq!(renders, 100, "one render per 50 ms poll over 5 s");
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
    s.invalidate();
    assert_eq!(s.next_wakeup(t0), Some(Duration::ZERO));
}

#[test]
fn next_wakeup_reports_the_remaining_frame_time() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    s.invalidate();
    assert!(s.poll(t0));

    s.invalidate();
    assert_eq!(s.next_wakeup(at(t0, 3)), Some(Duration::from_millis(5)));
}

#[test]
fn next_wakeup_saturates_to_zero_when_already_due() {
    let t0 = Instant::now();
    let mut s = RenderScheduler::new();
    s.invalidate();
    assert!(s.poll(t0));

    s.invalidate();
    assert_eq!(s.next_wakeup(at(t0, 20)), Some(Duration::ZERO));
}
