//! Coverage for the render scheduler's coalescing, frame gating, and
//! idle-wakeup behavior. Time is synthetic: every case builds its own timeline
//! from one seed [`Instant`] plus fixed offsets, so the gate is exercised
//! without sleeping or reading the real clock.

use super::*;

/// Milliseconds after the seed instant.
fn instant_after_milliseconds(seed_instant: Instant, offset_milliseconds: u64) -> Instant {
    seed_instant + Duration::from_millis(offset_milliseconds)
}

#[test]
fn fresh_scheduler_has_nothing_pending() {
    let timeline_start_time = Instant::now();
    let mut render_scheduler = RenderScheduler::new();
    assert!(!render_scheduler.poll(timeline_start_time));
    assert_eq!(render_scheduler.next_wakeup(timeline_start_time), None);
}

#[test]
fn the_default_scheduler_starts_like_a_new_one() {
    let timeline_start_time = Instant::now();
    let mut render_scheduler = RenderScheduler::default();
    assert!(!render_scheduler.poll(timeline_start_time));
    assert_eq!(render_scheduler.next_wakeup(timeline_start_time), None);
}

#[test]
fn the_first_invalidation_renders_immediately() {
    let timeline_start_time = Instant::now();
    let mut render_scheduler = RenderScheduler::new();
    render_scheduler.invalidate();
    assert!(render_scheduler.poll(timeline_start_time));
}

#[test]
fn poll_clears_pending_so_an_immediate_second_poll_is_false() {
    let timeline_start_time = Instant::now();
    let mut render_scheduler = RenderScheduler::new();
    render_scheduler.invalidate();
    assert!(render_scheduler.poll(timeline_start_time));
    assert!(!render_scheduler.poll(timeline_start_time));
}

#[test]
fn a_burst_of_invalidations_coalesces_into_one_render() {
    let timeline_start_time = Instant::now();
    let mut render_scheduler = RenderScheduler::new();
    render_scheduler.invalidate();
    render_scheduler.invalidate();
    render_scheduler.invalidate();
    render_scheduler.invalidate();
    assert!(render_scheduler.poll(timeline_start_time));
    assert!(!render_scheduler.poll(timeline_start_time));
}

#[test]
fn a_pending_change_gates_at_the_frame_interval() {
    let timeline_start_time = Instant::now();
    let mut render_scheduler = RenderScheduler::new();
    render_scheduler.invalidate();
    assert!(render_scheduler.poll(timeline_start_time));

    render_scheduler.invalidate();
    assert!(
        !render_scheduler.poll(instant_after_milliseconds(timeline_start_time, 7)),
        "too soon: 7 ms < 8 ms frame interval"
    );
    assert!(
        render_scheduler.poll(instant_after_milliseconds(timeline_start_time, 8)),
        "8 ms frame interval elapsed"
    );
}

#[test]
fn a_poll_earlier_than_the_last_render_is_not_due() {
    let timeline_start_time = Instant::now();
    let mut render_scheduler = RenderScheduler::new();
    render_scheduler.invalidate();
    assert!(render_scheduler.poll(instant_after_milliseconds(timeline_start_time, 100)));

    render_scheduler.invalidate();
    assert!(!render_scheduler.poll(timeline_start_time));
    assert_eq!(
        render_scheduler.next_wakeup(timeline_start_time),
        Some(FRAME_INTERVAL_DURATION)
    );
}

#[test]
fn five_seconds_of_invalidations_render_at_the_frame_cadence() {
    let timeline_start_time = Instant::now();
    let mut render_scheduler = RenderScheduler::new();
    // Establish the baseline frame at t0, then measure the next 5 s.
    render_scheduler.invalidate();
    assert!(render_scheduler.poll(timeline_start_time));

    // Poll every 50 ms — coarser than the 8 ms cadence, so every poll is due.
    let mut render_count = 0;
    let mut elapsed_milliseconds = 50;
    while elapsed_milliseconds <= 5000 {
        render_scheduler.invalidate();
        if render_scheduler.poll(instant_after_milliseconds(
            timeline_start_time,
            elapsed_milliseconds,
        )) {
            render_count += 1;
        }
        elapsed_milliseconds += 50;
    }
    assert_eq!(render_count, 100, "one render per 50 ms poll over 5 s");
}

#[test]
fn next_wakeup_is_none_when_nothing_is_pending() {
    let timeline_start_time = Instant::now();
    let render_scheduler = RenderScheduler::new();
    assert_eq!(render_scheduler.next_wakeup(timeline_start_time), None);
}

#[test]
fn next_wakeup_is_zero_before_the_first_render() {
    let timeline_start_time = Instant::now();
    let mut render_scheduler = RenderScheduler::new();
    render_scheduler.invalidate();
    assert_eq!(
        render_scheduler.next_wakeup(timeline_start_time),
        Some(Duration::ZERO)
    );
}

#[test]
fn next_wakeup_reports_the_remaining_frame_time() {
    let timeline_start_time = Instant::now();
    let mut render_scheduler = RenderScheduler::new();
    render_scheduler.invalidate();
    assert!(render_scheduler.poll(timeline_start_time));

    render_scheduler.invalidate();
    assert_eq!(
        render_scheduler.next_wakeup(instant_after_milliseconds(timeline_start_time, 3)),
        Some(Duration::from_millis(5))
    );
}

#[test]
fn next_wakeup_saturates_to_zero_when_already_due() {
    let timeline_start_time = Instant::now();
    let mut render_scheduler = RenderScheduler::new();
    render_scheduler.invalidate();
    assert!(render_scheduler.poll(timeline_start_time));

    render_scheduler.invalidate();
    assert_eq!(
        render_scheduler.next_wakeup(instant_after_milliseconds(timeline_start_time, 20)),
        Some(Duration::ZERO)
    );
}
