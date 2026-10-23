//! Coverage for each subscriber's frame pace — coalescing, the frame grid,
//! the frame that goes at once after the client's own input, and the waits —
//! and for each pane's timed-render deadline. Time is synthetic: every case
//! builds its own timeline from one seed [`Instant`] plus fixed offsets, and
//! reads no clock after that seed.

use koshi_core::constant::{compute_frame_interval, DEFAULT_MAX_FRAMES_PER_SECOND};
use koshi_core::ids::ClientId;

use super::*;
use crate::runtime::delivery_queue::DeliveryReceiver;
use crate::server::Server;

/// The frame interval at the default 60 frames a second.
pub(crate) const DEFAULT_FRAME_INTERVAL_DURATION: Duration = Duration::from_nanos(16_666_667);

/// Subscribe `client_id` at 60 frames a second, mark the views that the
/// state built before the subscription changed
/// ([`Server::mark_changed_views`]), push the frame `client_id` is owed at
/// `current_time`, and hand back its queue, which must stay alive for the
/// subscription to stay. The frame pushed at `current_time` stays on the
/// queue.
pub(crate) fn subscribe_and_push_first_frame(
    server: &mut Server,
    client_id: ClientId,
    current_time: Instant,
) -> DeliveryReceiver {
    let (delivery_receiver, _writer_handoff) = server.subscribe(client_id, 60);
    server.mark_changed_views();
    server.push_frames(current_time);
    assert_eq!(
        mark_changed_views_then_compute_frame_wait(server, client_id, current_time),
        None
    );
    delivery_receiver
}

/// Mark the views that changed ([`Server::mark_changed_views`]), then return
/// how long until the frame of the first subscriber that views `client_id` is
/// due at `current_time`, as [`FramePace::compute_frame_wait`] reads it.
///
/// # Panics
/// When no subscriber views `client_id`.
pub(crate) fn mark_changed_views_then_compute_frame_wait(
    server: &mut Server,
    client_id: ClientId,
    current_time: Instant,
) -> Option<Duration> {
    server.mark_changed_views();
    server
        .subscriptions
        .iter()
        .find(|frame_subscription| frame_subscription.client_id == client_id)
        .expect("the client is subscribed")
        .frame_pace
        .compute_frame_wait(current_time)
}

/// Milliseconds after the seed instant.
fn build_instant_after_milliseconds(seed_instant: Instant, offset_milliseconds: u64) -> Instant {
    seed_instant + Duration::from_millis(offset_milliseconds)
}

/// A pace at the default 60 frames a second whose first frame went at
/// `first_frame_sent_at`.
fn build_pace_after_first_frame(first_frame_sent_at: Instant) -> FramePace {
    let mut frame_pace =
        FramePace::with_frame_interval(compute_frame_interval(DEFAULT_MAX_FRAMES_PER_SECOND));
    frame_pace.record_frame_sent(first_frame_sent_at);
    frame_pace
}

/// Drive a pace through one second in which the view changes every
/// millisecond, sending each frame at the first millisecond it is due, and
/// return the milliseconds the frames went at. `allow_now_at_millisecond`
/// names a millisecond at which the client's own input lets the next frame
/// go at once.
fn list_frame_milliseconds_over_one_second(
    max_frames_per_second: u16,
    allow_now_at_millisecond: Option<u64>,
) -> Vec<u64> {
    let timeline_start_time = Instant::now();
    let mut frame_pace =
        FramePace::with_frame_interval(compute_frame_interval(max_frames_per_second));
    let mut frame_milliseconds = Vec::new();
    for elapsed_milliseconds in 0..1000 {
        let current_time =
            build_instant_after_milliseconds(timeline_start_time, elapsed_milliseconds);
        frame_pace.mark_view_changed();
        if allow_now_at_millisecond == Some(elapsed_milliseconds) {
            frame_pace.allow_next_frame_now();
        }
        if frame_pace.compute_frame_wait(current_time) == Some(Duration::ZERO) {
            frame_pace.record_frame_sent(current_time);
            frame_milliseconds.push(elapsed_milliseconds);
        }
    }
    frame_milliseconds
}

#[test]
fn a_new_pace_owes_its_first_frame_at_once_and_nothing_after_it() {
    let timeline_start_time = Instant::now();
    let mut frame_pace =
        FramePace::with_frame_interval(compute_frame_interval(DEFAULT_MAX_FRAMES_PER_SECOND));

    assert_eq!(
        frame_pace.compute_frame_wait(timeline_start_time),
        Some(Duration::ZERO)
    );
    frame_pace.record_frame_sent(timeline_start_time);
    assert_eq!(frame_pace.compute_frame_wait(timeline_start_time), None);
}

#[test]
fn a_burst_of_changes_before_a_frame_gives_one_frame() {
    let timeline_start_time = Instant::now();
    let mut frame_pace = build_pace_after_first_frame(timeline_start_time);
    let frame_time = build_instant_after_milliseconds(timeline_start_time, 20);

    frame_pace.mark_view_changed();
    frame_pace.mark_view_changed();
    frame_pace.mark_view_changed();

    assert_eq!(
        frame_pace.compute_frame_wait(frame_time),
        Some(Duration::ZERO)
    );
    frame_pace.record_frame_sent(frame_time);
    assert_eq!(frame_pace.compute_frame_wait(frame_time), None);
}

#[test]
fn a_change_right_after_a_frame_waits_out_the_frame_interval() {
    let timeline_start_time = Instant::now();
    let mut frame_pace = build_pace_after_first_frame(timeline_start_time);
    frame_pace.mark_view_changed();

    assert_eq!(
        frame_pace.compute_frame_wait(timeline_start_time),
        Some(DEFAULT_FRAME_INTERVAL_DURATION)
    );
    assert_eq!(
        frame_pace.compute_frame_wait(build_instant_after_milliseconds(timeline_start_time, 3)),
        Some(Duration::from_nanos(13_666_667))
    );
    assert_eq!(
        frame_pace.compute_frame_wait(timeline_start_time + DEFAULT_FRAME_INTERVAL_DURATION),
        Some(Duration::ZERO)
    );
    assert_eq!(
        frame_pace.compute_frame_wait(build_instant_after_milliseconds(timeline_start_time, 40)),
        Some(Duration::ZERO)
    );
}

#[test]
fn a_view_that_changes_every_millisecond_gets_each_clients_own_frame_rate() {
    let frames_at_sixty = list_frame_milliseconds_over_one_second(60, None);
    assert_eq!(frames_at_sixty.len(), 60);
    assert_eq!(frames_at_sixty[..4], [0, 17, 34, 51]);

    let frames_at_one_hundred_twenty = list_frame_milliseconds_over_one_second(120, None);
    assert_eq!(frames_at_one_hundred_twenty.len(), 120);
    assert_eq!(frames_at_one_hundred_twenty[..4], [0, 9, 17, 26]);

    assert_eq!(list_frame_milliseconds_over_one_second(30, None).len(), 30);
}

#[test]
fn the_clients_own_input_sends_its_next_frame_at_once() {
    let frames_with_input = list_frame_milliseconds_over_one_second(60, Some(500));
    let frames_without_input = list_frame_milliseconds_over_one_second(60, None);

    assert!(!frames_without_input.contains(&500));
    assert!(frames_with_input.contains(&500));
    // The frame at 500 ms starts the grid again: the next one goes at 517 ms.
    let input_frame_index = frames_with_input
        .iter()
        .position(|frame_millisecond| *frame_millisecond == 500)
        .expect("a frame went at 500 ms");
    assert_eq!(frames_with_input[input_frame_index + 1], 517);
}

#[test]
fn allowing_the_next_frame_now_sends_nothing_while_the_view_is_unchanged() {
    let timeline_start_time = Instant::now();
    let mut frame_pace = build_pace_after_first_frame(timeline_start_time);
    let input_time = build_instant_after_milliseconds(timeline_start_time, 2);

    frame_pace.allow_next_frame_now();
    assert_eq!(frame_pace.compute_frame_wait(input_time), None);

    // The echo of the key changes the view at 5 ms: its frame goes at once.
    let echo_time = build_instant_after_milliseconds(timeline_start_time, 5);
    frame_pace.mark_view_changed();
    assert_eq!(
        frame_pace.compute_frame_wait(echo_time),
        Some(Duration::ZERO)
    );
    frame_pace.record_frame_sent(echo_time);

    // The allowance is used up: the next change waits a whole interval.
    frame_pace.mark_view_changed();
    assert_eq!(
        frame_pace.compute_frame_wait(echo_time),
        Some(DEFAULT_FRAME_INTERVAL_DURATION)
    );
}

#[test]
fn a_frame_after_a_quiet_stretch_starts_the_grid_at_its_own_time() {
    let timeline_start_time = Instant::now();
    let mut frame_pace = build_pace_after_first_frame(timeline_start_time);
    let quiet_end_time = build_instant_after_milliseconds(timeline_start_time, 100);

    frame_pace.mark_view_changed();
    assert_eq!(
        frame_pace.compute_frame_wait(quiet_end_time),
        Some(Duration::ZERO)
    );
    frame_pace.record_frame_sent(quiet_end_time);
    frame_pace.mark_view_changed();

    assert_eq!(
        frame_pace.compute_frame_wait(quiet_end_time),
        Some(DEFAULT_FRAME_INTERVAL_DURATION)
    );
}

#[test]
fn a_frame_one_whole_interval_after_its_allowed_time_starts_the_grid_at_its_own_time() {
    let timeline_start_time = Instant::now();
    let frame_allowed_at = timeline_start_time + DEFAULT_FRAME_INTERVAL_DURATION;

    let mut kept_grid_pace = build_pace_after_first_frame(timeline_start_time);
    let last_grid_keeping_sent_at =
        frame_allowed_at + DEFAULT_FRAME_INTERVAL_DURATION - Duration::from_nanos(1);
    kept_grid_pace.record_frame_sent(last_grid_keeping_sent_at);
    kept_grid_pace.mark_view_changed();
    assert_eq!(
        kept_grid_pace.compute_frame_wait(last_grid_keeping_sent_at),
        Some(Duration::from_nanos(1))
    );

    let mut restarted_grid_pace = build_pace_after_first_frame(timeline_start_time);
    let first_grid_starting_sent_at = frame_allowed_at + DEFAULT_FRAME_INTERVAL_DURATION;
    restarted_grid_pace.record_frame_sent(first_grid_starting_sent_at);
    restarted_grid_pace.mark_view_changed();
    assert_eq!(
        restarted_grid_pace.compute_frame_wait(first_grid_starting_sent_at),
        Some(DEFAULT_FRAME_INTERVAL_DURATION)
    );
}

#[test]
fn a_cleared_change_owes_no_frame_and_keeps_the_grid() {
    let timeline_start_time = Instant::now();
    let mut frame_pace = build_pace_after_first_frame(timeline_start_time);
    frame_pace.mark_view_changed();

    frame_pace.clear_view_change();
    assert_eq!(frame_pace.compute_frame_wait(timeline_start_time), None);

    frame_pace.mark_view_changed();
    assert_eq!(
        frame_pace.compute_frame_wait(timeline_start_time),
        Some(DEFAULT_FRAME_INTERVAL_DURATION)
    );
}

#[test]
fn a_grid_frame_time_is_the_allowed_time_plus_whole_intervals() {
    let timeline_start_time = Instant::now();
    let new_frame_pace =
        FramePace::with_frame_interval(compute_frame_interval(DEFAULT_MAX_FRAMES_PER_SECOND));
    assert_eq!(
        new_frame_pace.find_grid_frame_time(timeline_start_time),
        timeline_start_time
    );

    // The first frame at 83333333 ns allows the next one at 100 ms.
    let mut frame_pace =
        build_pace_after_first_frame(timeline_start_time + Duration::from_nanos(83_333_333));
    assert_eq!(
        frame_pace.find_grid_frame_time(build_instant_after_milliseconds(timeline_start_time, 90)),
        build_instant_after_milliseconds(timeline_start_time, 100)
    );
    assert_eq!(
        frame_pace.find_grid_frame_time(build_instant_after_milliseconds(timeline_start_time, 100)),
        build_instant_after_milliseconds(timeline_start_time, 100)
    );
    assert_eq!(
        frame_pace.find_grid_frame_time(build_instant_after_milliseconds(timeline_start_time, 105)),
        timeline_start_time + Duration::from_nanos(116_666_667)
    );
    assert_eq!(
        frame_pace.find_grid_frame_time(build_instant_after_milliseconds(timeline_start_time, 120)),
        timeline_start_time + Duration::from_nanos(133_333_334)
    );

    frame_pace.allow_next_frame_now();
    let input_time = build_instant_after_milliseconds(timeline_start_time, 90);
    assert_eq!(frame_pace.find_grid_frame_time(input_time), input_time);
}

#[test]
fn the_earliest_deadline_is_the_first_one_due_and_the_other_one_stays() {
    let timeline_start_time = Instant::now();
    let mut pane_render_deadlines = PaneRenderDeadlines::default();
    let second_due_pane_id = PaneId::new();
    let first_due_pane_id = PaneId::new();
    pane_render_deadlines.set_pane_render_deadline(
        second_due_pane_id,
        Some(build_instant_after_milliseconds(timeline_start_time, 5)),
    );
    pane_render_deadlines.set_pane_render_deadline(
        first_due_pane_id,
        Some(build_instant_after_milliseconds(timeline_start_time, 2)),
    );

    assert_eq!(
        pane_render_deadlines.find_earliest_render_deadline(),
        Some(build_instant_after_milliseconds(timeline_start_time, 2))
    );
    assert_eq!(
        pane_render_deadlines
            .take_due_pane_ids(build_instant_after_milliseconds(timeline_start_time, 3)),
        vec![first_due_pane_id]
    );
    assert_eq!(
        pane_render_deadlines.find_earliest_render_deadline(),
        Some(build_instant_after_milliseconds(timeline_start_time, 5))
    );
    assert_eq!(
        pane_render_deadlines
            .take_due_pane_ids(build_instant_after_milliseconds(timeline_start_time, 5)),
        vec![second_due_pane_id]
    );
    assert_eq!(pane_render_deadlines.find_earliest_render_deadline(), None);
}

#[test]
fn setting_a_pane_deadline_replaces_its_old_one_and_none_removes_it() {
    let timeline_start_time = Instant::now();
    let mut pane_render_deadlines = PaneRenderDeadlines::default();
    let pane_id = PaneId::new();
    pane_render_deadlines.set_pane_render_deadline(
        pane_id,
        Some(build_instant_after_milliseconds(timeline_start_time, 2)),
    );

    pane_render_deadlines.set_pane_render_deadline(
        pane_id,
        Some(build_instant_after_milliseconds(timeline_start_time, 9)),
    );

    assert_eq!(
        pane_render_deadlines
            .take_due_pane_ids(build_instant_after_milliseconds(timeline_start_time, 8)),
        Vec::<PaneId>::new()
    );
    assert_eq!(
        pane_render_deadlines
            .list_render_deadlines()
            .collect::<Vec<(Instant, PaneId)>>(),
        vec![(
            build_instant_after_milliseconds(timeline_start_time, 9),
            pane_id
        )]
    );
    pane_render_deadlines.set_pane_render_deadline(pane_id, None);
    assert_eq!(pane_render_deadlines.find_earliest_render_deadline(), None);
    assert!(pane_render_deadlines.render_deadline_by_pane_id.is_empty());
    assert!(pane_render_deadlines.pane_ids_by_render_deadline.is_empty());
}

#[test]
fn panes_due_at_the_same_instant_are_all_taken() {
    let timeline_start_time = Instant::now();
    let mut pane_render_deadlines = PaneRenderDeadlines::default();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let shared_deadline = build_instant_after_milliseconds(timeline_start_time, 4);
    pane_render_deadlines.set_pane_render_deadline(first_pane_id, Some(shared_deadline));
    pane_render_deadlines.set_pane_render_deadline(second_pane_id, Some(shared_deadline));

    let mut due_pane_ids = pane_render_deadlines.take_due_pane_ids(shared_deadline);
    due_pane_ids.sort();
    let mut expected_pane_ids = vec![first_pane_id, second_pane_id];
    expected_pane_ids.sort();

    assert_eq!(due_pane_ids, expected_pane_ids);
    assert_eq!(pane_render_deadlines.find_earliest_render_deadline(), None);
}
