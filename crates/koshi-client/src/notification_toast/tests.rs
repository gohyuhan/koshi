//! Tests for `NotificationToastQueue`: the normal and backlog phases, the
//! cap a new successor sets and the deadline its removal restores, flood
//! control, repeats, the close control, eviction, the pause and the wakeups.

use std::num::NonZeroU32;
use std::time::SystemTime;

use koshi_core::ids::{ClientId, SessionId};
use koshi_core::notification::{NotificationQueue, NotificationQueueOwner};

use super::*;

/// The phase lengths of the default settings: a 3 s stay, with motion.
const DEFAULT_TOAST_TIMING: ToastTiming = ToastTiming {
    stay_duration: Duration::from_secs(3),
    should_reduce_motion: false,
};

/// `own_entry_count` `Warning` entries of one viewer's own queue, oldest first,
/// with the texts `entry 0`, `entry 1`, ….
fn post_own_entries(own_entry_count: usize) -> Vec<NotificationEntry> {
    let mut client_queue =
        NotificationQueue::from_owner(NotificationQueueOwner::Client(ClientId::new()));
    (0..own_entry_count)
        .map(|own_entry_index| {
            client_queue
                .post_core_entry(
                    NotificationSeverity::Warning,
                    &format!("entry {own_entry_index}"),
                    NonZeroU32::MIN,
                    SystemTime::UNIX_EPOCH,
                )
                .clone()
        })
        .collect()
}

/// `millisecond_count` milliseconds after `start`.
fn compute_instant_after_milliseconds(start: Instant, millisecond_count: u64) -> Instant {
    start + Duration::from_millis(millisecond_count)
}

/// The shown preview of `toast_queue` and the thousandths of its width past
/// its side edge.
fn clone_shown_preview(toast_queue: &NotificationToastQueue) -> Option<(ToastPreview, u16)> {
    toast_queue
        .find_shown_preview()
        .map(|(toast_preview, hidden_per_mille)| (toast_preview.clone(), hidden_per_mille))
}

/// `posted_entry` shown with `hidden_per_mille` thousandths of its width past its
/// side edge.
fn build_shown_entry_preview(
    posted_entry: &NotificationEntry,
    hidden_per_mille: u16,
) -> Option<(ToastPreview, u16)> {
    Some((ToastPreview::Entry(posted_entry.clone()), hidden_per_mille))
}

/// A queue that previews `posted_entries` in order.
fn build_previewing_queue(posted_entries: &[NotificationEntry]) -> NotificationToastQueue {
    let mut toast_queue = NotificationToastQueue::default();
    for posted_entry in posted_entries {
        toast_queue.queue_preview(posted_entry);
    }
    toast_queue
}

#[test]
fn one_toast_runs_one_second_of_appear_three_of_stay_and_one_of_withdraw() {
    let posted_entries = post_own_entries(1);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let start = Instant::now();

    let mut slide_positions = Vec::new();
    for millisecond_count in [0, 500, 1000, 3999, 4000, 4500] {
        assert!(!toast_queue.advance(
            compute_instant_after_milliseconds(start, millisecond_count),
            DEFAULT_TOAST_TIMING,
            false
        ));
        slide_positions.push(clone_shown_preview(&toast_queue));
    }
    let is_preview_changed = toast_queue.advance(
        compute_instant_after_milliseconds(start, 5000),
        DEFAULT_TOAST_TIMING,
        false,
    );

    assert_eq!(
        slide_positions,
        vec![
            build_shown_entry_preview(&posted_entries[0], 1000),
            build_shown_entry_preview(&posted_entries[0], 500),
            build_shown_entry_preview(&posted_entries[0], 0),
            build_shown_entry_preview(&posted_entries[0], 0),
            build_shown_entry_preview(&posted_entries[0], 0),
            build_shown_entry_preview(&posted_entries[0], 500),
        ]
    );
    assert!(is_preview_changed);
    assert_eq!(clone_shown_preview(&toast_queue), None);
}

#[test]
fn a_waiting_preview_runs_each_phase_of_the_shown_toast_for_half_a_second() {
    let posted_entries = post_own_entries(2);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);

    toast_queue.advance(
        compute_instant_after_milliseconds(start, 250),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let first_at_250 = clone_shown_preview(&toast_queue);
    let is_preview_changed = toast_queue.advance(
        compute_instant_after_milliseconds(start, 1500),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let second_at_1500 = clone_shown_preview(&toast_queue);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 2000),
        DEFAULT_TOAST_TIMING,
        false,
    );

    assert_eq!(
        first_at_250,
        build_shown_entry_preview(&posted_entries[0], 500)
    );
    assert!(is_preview_changed);
    assert_eq!(
        second_at_1500,
        build_shown_entry_preview(&posted_entries[1], 1000)
    );
    assert_eq!(
        clone_shown_preview(&toast_queue),
        build_shown_entry_preview(&posted_entries[1], 500)
    );
}

#[test]
fn a_new_successor_caps_the_shown_phase_at_half_a_second_from_its_start() {
    let posted_entries = post_own_entries(2);
    let mut toast_queue = build_previewing_queue(&posted_entries[..1]);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 200),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let before_the_successor = clone_shown_preview(&toast_queue);

    toast_queue.queue_preview(&posted_entries[1]);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 300),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let after_the_successor = clone_shown_preview(&toast_queue);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 500),
        DEFAULT_TOAST_TIMING,
        false,
    );

    assert_eq!(
        before_the_successor,
        build_shown_entry_preview(&posted_entries[0], 800)
    );
    assert_eq!(
        after_the_successor,
        build_shown_entry_preview(&posted_entries[0], 400)
    );
    assert_eq!(
        clone_shown_preview(&toast_queue),
        build_shown_entry_preview(&posted_entries[0], 0)
    );
}

#[test]
fn a_successor_past_the_capped_deadline_ends_the_shown_phase_at_the_next_advance() {
    let posted_entries = post_own_entries(2);
    let mut toast_queue = build_previewing_queue(&posted_entries[..1]);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 800),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let before_the_successor = clone_shown_preview(&toast_queue);

    toast_queue.queue_preview(&posted_entries[1]);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 800),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let in_the_capped_stay = clone_shown_preview(&toast_queue);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 1299),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let at_the_end_of_the_capped_stay = clone_shown_preview(&toast_queue);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 1550),
        DEFAULT_TOAST_TIMING,
        false,
    );

    assert_eq!(
        before_the_successor,
        build_shown_entry_preview(&posted_entries[0], 200)
    );
    assert_eq!(
        in_the_capped_stay,
        build_shown_entry_preview(&posted_entries[0], 0)
    );
    assert_eq!(
        at_the_end_of_the_capped_stay,
        build_shown_entry_preview(&posted_entries[0], 0)
    );
    assert_eq!(
        clone_shown_preview(&toast_queue),
        build_shown_entry_preview(&posted_entries[0], 500)
    );
}

#[test]
fn successors_that_arrive_late_in_a_long_stay_each_show_after_the_shown_toast_withdraws() {
    let posted_entries = post_own_entries(4);
    let mut toast_queue = build_previewing_queue(&posted_entries[..1]);
    let toast_timing = ToastTiming {
        stay_duration: Duration::from_secs(60),
        should_reduce_motion: false,
    };
    let start = Instant::now();
    toast_queue.advance(start, toast_timing, false);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 1000),
        toast_timing,
        false,
    );

    for posted_entry in &posted_entries[1..] {
        toast_queue.queue_preview(posted_entry);
    }
    let mut shown_previews = Vec::new();
    for millisecond_count in [50_000, 50_250, 50_500, 52_000, 53_500, 54_000] {
        toast_queue.advance(
            compute_instant_after_milliseconds(start, millisecond_count),
            toast_timing,
            false,
        );
        shown_previews.push(clone_shown_preview(&toast_queue));
    }

    assert_eq!(
        shown_previews,
        vec![
            build_shown_entry_preview(&posted_entries[0], 0),
            build_shown_entry_preview(&posted_entries[0], 500),
            build_shown_entry_preview(&posted_entries[1], 1000),
            build_shown_entry_preview(&posted_entries[2], 1000),
            build_shown_entry_preview(&posted_entries[3], 1000),
            build_shown_entry_preview(&posted_entries[3], 500),
        ]
    );
}

#[test]
fn a_successor_after_an_earlier_backlog_cleared_caps_the_phase_from_its_own_arrival() {
    let posted_entries = post_own_entries(3);
    let mut toast_queue = build_previewing_queue(&posted_entries[..2]);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 2500),
        DEFAULT_TOAST_TIMING,
        false,
    );

    toast_queue.queue_preview(&posted_entries[2]);
    let mut shown_previews = Vec::new();
    for millisecond_count in [4000, 4250, 4500] {
        toast_queue.advance(
            compute_instant_after_milliseconds(start, millisecond_count),
            DEFAULT_TOAST_TIMING,
            false,
        );
        shown_previews.push(clone_shown_preview(&toast_queue));
    }

    assert_eq!(
        shown_previews,
        vec![
            build_shown_entry_preview(&posted_entries[1], 0),
            build_shown_entry_preview(&posted_entries[1], 500),
            build_shown_entry_preview(&posted_entries[2], 1000),
        ]
    );
}

#[test]
fn a_successor_queued_while_the_clock_is_paused_caps_the_stay_when_the_clock_runs_again() {
    let posted_entries = post_own_entries(2);
    let mut toast_queue = build_previewing_queue(&posted_entries[..1]);
    let toast_timing = ToastTiming {
        stay_duration: Duration::from_secs(60),
        should_reduce_motion: false,
    };
    let start = Instant::now();
    toast_queue.advance(start, toast_timing, false);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 1000),
        toast_timing,
        false,
    );
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 10_000),
        toast_timing,
        true,
    );

    toast_queue.queue_preview(&posted_entries[1]);
    let mut shown_previews = Vec::new();
    for millisecond_count in [30_000, 30_250, 30_500] {
        toast_queue.advance(
            compute_instant_after_milliseconds(start, millisecond_count),
            toast_timing,
            false,
        );
        shown_previews.push(clone_shown_preview(&toast_queue));
    }

    assert_eq!(
        shown_previews,
        vec![
            build_shown_entry_preview(&posted_entries[0], 0),
            build_shown_entry_preview(&posted_entries[0], 500),
            build_shown_entry_preview(&posted_entries[1], 1000),
        ]
    );
}

#[test]
fn reduced_motion_with_a_successor_holds_the_toast_half_a_second_then_shows_the_successor() {
    let posted_entries = post_own_entries(2);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let toast_timing = ToastTiming {
        should_reduce_motion: true,
        ..DEFAULT_TOAST_TIMING
    };
    let start = Instant::now();

    let mut shown_previews = Vec::new();
    for millisecond_count in [0, 499, 500] {
        toast_queue.advance(
            compute_instant_after_milliseconds(start, millisecond_count),
            toast_timing,
            false,
        );
        shown_previews.push(clone_shown_preview(&toast_queue));
    }

    assert_eq!(
        shown_previews,
        vec![
            build_shown_entry_preview(&posted_entries[0], 0),
            build_shown_entry_preview(&posted_entries[0], 0),
            build_shown_entry_preview(&posted_entries[1], 0),
        ]
    );
}

#[test]
fn a_withdraw_requested_while_a_successor_waits_slides_out_in_half_a_second_from_where_it_stands() {
    let posted_entries = post_own_entries(2);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 300),
        DEFAULT_TOAST_TIMING,
        false,
    );

    toast_queue.request_withdraw();
    let mut shown_previews = Vec::new();
    for millisecond_count in [300, 550, 800] {
        toast_queue.advance(
            compute_instant_after_milliseconds(start, millisecond_count),
            DEFAULT_TOAST_TIMING,
            false,
        );
        shown_previews.push(clone_shown_preview(&toast_queue));
    }

    assert_eq!(
        shown_previews,
        vec![
            build_shown_entry_preview(&posted_entries[0], 400),
            build_shown_entry_preview(&posted_entries[0], 700),
            build_shown_entry_preview(&posted_entries[1], 1000),
        ]
    );
}

#[test]
fn an_advance_an_hour_late_ends_the_shown_preview_and_starts_the_next_at_that_advance() {
    let posted_entries = post_own_entries(5);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let toast_timing = ToastTiming {
        stay_duration: Duration::from_millis(u64::from(
            koshi_config::types::LOWEST_TOAST_DURATION_MS,
        )),
        should_reduce_motion: false,
    };
    let start = Instant::now();
    toast_queue.advance(start, toast_timing, false);

    let is_preview_changed = toast_queue.advance(
        compute_instant_after_milliseconds(start, 3_600_000),
        toast_timing,
        false,
    );
    let next_wakeup = toast_queue.find_next_wakeup(
        compute_instant_after_milliseconds(start, 3_600_000),
        toast_timing,
    );
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 3_600_250),
        toast_timing,
        false,
    );

    assert!(is_preview_changed);
    assert_eq!(next_wakeup, Some(Duration::from_millis(50)));
    assert_eq!(
        clone_shown_preview(&toast_queue),
        build_shown_entry_preview(&posted_entries[1], 500)
    );
    assert_eq!(toast_queue.waiting_entries.len(), 2);
}

#[test]
fn a_successor_first_seen_after_the_shown_phase_ended_keeps_that_phase_end() {
    let posted_entries = post_own_entries(2);
    let mut toast_queue = build_previewing_queue(&posted_entries[..1]);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 1000),
        DEFAULT_TOAST_TIMING,
        false,
    );

    toast_queue.queue_preview(&posted_entries[1]);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 4100),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let in_the_withdraw = clone_shown_preview(&toast_queue);
    let is_preview_changed = toast_queue.advance(
        compute_instant_after_milliseconds(start, 4500),
        DEFAULT_TOAST_TIMING,
        false,
    );

    assert_eq!(
        in_the_withdraw,
        build_shown_entry_preview(&posted_entries[0], 200)
    );
    assert!(is_preview_changed);
    assert_eq!(
        clone_shown_preview(&toast_queue),
        build_shown_entry_preview(&posted_entries[1], 1000)
    );
}

#[test]
fn removing_every_successor_restores_the_normal_deadline_of_the_unfinished_phase() {
    let posted_entries = post_own_entries(2);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 300),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let with_the_successor = clone_shown_preview(&toast_queue);

    toast_queue.remove_previews(posted_entries[1].notification_id);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 300),
        DEFAULT_TOAST_TIMING,
        false,
    );

    assert_eq!(
        with_the_successor,
        build_shown_entry_preview(&posted_entries[0], 400)
    );
    assert_eq!(
        clone_shown_preview(&toast_queue),
        build_shown_entry_preview(&posted_entries[0], 700)
    );
}

#[test]
fn removing_every_successor_never_rewinds_a_finished_phase() {
    let posted_entries = post_own_entries(2);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 600),
        DEFAULT_TOAST_TIMING,
        false,
    );

    toast_queue.remove_previews(posted_entries[1].notification_id);
    let mut slide_positions = Vec::new();
    for millisecond_count in [700, 3499, 3500, 4000] {
        toast_queue.advance(
            compute_instant_after_milliseconds(start, millisecond_count),
            DEFAULT_TOAST_TIMING,
            false,
        );
        slide_positions.push(clone_shown_preview(&toast_queue));
    }

    assert_eq!(
        slide_positions,
        vec![
            build_shown_entry_preview(&posted_entries[0], 0),
            build_shown_entry_preview(&posted_entries[0], 0),
            build_shown_entry_preview(&posted_entries[0], 0),
            build_shown_entry_preview(&posted_entries[0], 500),
        ]
    );
}

#[test]
fn a_toast_duration_of_five_seconds_holds_the_toast_five_seconds() {
    let posted_entries = post_own_entries(1);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let toast_timing = ToastTiming {
        stay_duration: Duration::from_secs(5),
        should_reduce_motion: false,
    };
    let start = Instant::now();
    toast_queue.advance(start, toast_timing, false);

    toast_queue.advance(
        compute_instant_after_milliseconds(start, 5999),
        toast_timing,
        false,
    );
    let before_the_withdraw = clone_shown_preview(&toast_queue);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 6500),
        toast_timing,
        false,
    );
    let in_the_withdraw = clone_shown_preview(&toast_queue);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 7000),
        toast_timing,
        false,
    );

    assert_eq!(
        before_the_withdraw,
        build_shown_entry_preview(&posted_entries[0], 0)
    );
    assert_eq!(
        in_the_withdraw,
        build_shown_entry_preview(&posted_entries[0], 500)
    );
    assert_eq!(clone_shown_preview(&toast_queue), None);
}

#[test]
fn reduced_motion_puts_the_toast_in_place_in_one_frame_and_removes_it_in_one() {
    let posted_entries = post_own_entries(1);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let toast_timing = ToastTiming {
        should_reduce_motion: true,
        ..DEFAULT_TOAST_TIMING
    };
    let start = Instant::now();

    toast_queue.advance(start, toast_timing, false);
    let first_frame = clone_shown_preview(&toast_queue);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 2999),
        toast_timing,
        false,
    );
    let last_stay_frame = clone_shown_preview(&toast_queue);
    let is_preview_changed = toast_queue.advance(
        compute_instant_after_milliseconds(start, 3000),
        toast_timing,
        false,
    );

    assert_eq!(
        first_frame,
        build_shown_entry_preview(&posted_entries[0], 0)
    );
    assert_eq!(
        last_stay_frame,
        build_shown_entry_preview(&posted_entries[0], 0)
    );
    assert!(is_preview_changed);
    assert_eq!(clone_shown_preview(&toast_queue), None);
}

#[test]
fn forty_entries_give_four_previews_then_one_summary_that_counts_thirty_six() {
    let posted_entries = post_own_entries(40);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let start = Instant::now();

    let mut shown_previews = Vec::new();
    for millisecond_count in [0, 1500, 3000, 4500, 6000, 9999] {
        toast_queue.advance(
            compute_instant_after_milliseconds(start, millisecond_count),
            DEFAULT_TOAST_TIMING,
            false,
        );
        shown_previews.push(clone_shown_preview(&toast_queue));
    }
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 11_000),
        DEFAULT_TOAST_TIMING,
        false,
    );

    let summary_members: Vec<(NotificationId, NotificationSeverity)> = posted_entries[4..]
        .iter()
        .map(|posted_entry| (posted_entry.notification_id, posted_entry.severity))
        .collect();
    assert_eq!(summary_members.len(), 36);
    assert_eq!(
        shown_previews,
        vec![
            build_shown_entry_preview(&posted_entries[0], 1000),
            build_shown_entry_preview(&posted_entries[1], 1000),
            build_shown_entry_preview(&posted_entries[2], 1000),
            build_shown_entry_preview(&posted_entries[3], 1000),
            Some((ToastPreview::Summary(summary_members.clone()), 1000)),
            Some((ToastPreview::Summary(summary_members), 0)),
        ]
    );
    assert_eq!(clone_shown_preview(&toast_queue), None);
}

#[test]
fn an_entry_that_arrives_while_the_summary_shows_waits_behind_it_and_shows_after_it() {
    let posted_entries = post_own_entries(6);
    let mut toast_queue = build_previewing_queue(&posted_entries[..5]);
    let start = Instant::now();
    for millisecond_count in [0, 1500, 3000, 4500, 6000, 8000] {
        toast_queue.advance(
            compute_instant_after_milliseconds(start, millisecond_count),
            DEFAULT_TOAST_TIMING,
            false,
        );
    }
    let summary_in_its_stay = clone_shown_preview(&toast_queue);

    toast_queue.queue_preview(&posted_entries[5]);
    let summary_after_the_arrival = clone_shown_preview(&toast_queue);
    let waiting_entries_after_the_arrival = toast_queue.waiting_entries.clone();
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 8000),
        DEFAULT_TOAST_TIMING,
        false,
    );
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 8250),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let summary_halfway_out = clone_shown_preview(&toast_queue);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 8500),
        DEFAULT_TOAST_TIMING,
        false,
    );

    let summary_preview = Some((
        ToastPreview::Summary(vec![(
            posted_entries[4].notification_id,
            posted_entries[4].severity,
        )]),
        0,
    ));
    assert_eq!(summary_in_its_stay, summary_preview);
    assert_eq!(summary_after_the_arrival, summary_preview);
    assert_eq!(
        waiting_entries_after_the_arrival,
        VecDeque::from([posted_entries[5].clone()])
    );
    assert_eq!(
        summary_halfway_out,
        Some((
            ToastPreview::Summary(vec![(
                posted_entries[4].notification_id,
                posted_entries[4].severity
            )]),
            500
        ))
    );
    assert_eq!(
        clone_shown_preview(&toast_queue),
        build_shown_entry_preview(&posted_entries[5], 1000)
    );
}

#[test]
fn a_repeat_updates_its_shown_and_its_waiting_preview_and_queues_nothing() {
    let posted_entries = post_own_entries(2);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let mut repeated_first_entry = posted_entries[0].clone();
    repeated_first_entry.repeat_count = 2;
    let mut repeated_second_entry = posted_entries[1].clone();
    repeated_second_entry.repeat_count = 2;
    let start = Instant::now();

    toast_queue.queue_preview(&repeated_first_entry);
    toast_queue.queue_preview(&repeated_second_entry);
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    let first_preview = clone_shown_preview(&toast_queue);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 1500),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let second_preview = clone_shown_preview(&toast_queue);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 6500),
        DEFAULT_TOAST_TIMING,
        false,
    );

    assert_eq!(
        first_preview,
        build_shown_entry_preview(&repeated_first_entry, 1000)
    );
    assert_eq!(
        second_preview,
        build_shown_entry_preview(&repeated_second_entry, 1000)
    );
    assert_eq!(clone_shown_preview(&toast_queue), None);
}

#[test]
fn a_repeat_counted_by_a_summary_counts_once() {
    let posted_entries = post_own_entries(5);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let mut repeated_last_entry = posted_entries[4].clone();
    repeated_last_entry.repeat_count = 2;

    toast_queue.queue_preview(&repeated_last_entry);

    assert_eq!(
        toast_queue.waiting_summary,
        Some(vec![(
            posted_entries[4].notification_id,
            posted_entries[4].severity
        )])
    );
}

#[test]
fn a_repeat_after_its_toast_ended_shows_a_new_preview() {
    let posted_entries = post_own_entries(1);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 5000),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let mut repeated_entry = posted_entries[0].clone();
    repeated_entry.repeat_count = 2;

    toast_queue.queue_preview(&repeated_entry);

    assert_eq!(
        clone_shown_preview(&toast_queue),
        build_shown_entry_preview(&repeated_entry, 1000)
    );
}

#[test]
fn the_close_control_withdraws_the_toast_from_where_it_stands() {
    let posted_entries = post_own_entries(1);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 300),
        DEFAULT_TOAST_TIMING,
        false,
    );

    toast_queue.request_withdraw();
    let wakeup_after_the_press = toast_queue.find_next_wakeup(
        compute_instant_after_milliseconds(start, 300),
        DEFAULT_TOAST_TIMING,
    );
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 300),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let at_the_press = clone_shown_preview(&toast_queue);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 800),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let halfway_out = clone_shown_preview(&toast_queue);
    let is_preview_changed = toast_queue.advance(
        compute_instant_after_milliseconds(start, 1300),
        DEFAULT_TOAST_TIMING,
        false,
    );

    assert_eq!(wakeup_after_the_press, Some(Duration::ZERO));
    assert_eq!(
        at_the_press,
        build_shown_entry_preview(&posted_entries[0], 700)
    );
    assert_eq!(
        halfway_out,
        build_shown_entry_preview(&posted_entries[0], 850)
    );
    assert!(is_preview_changed);
    assert_eq!(clone_shown_preview(&toast_queue), None);
}

#[test]
fn the_close_control_on_a_toast_not_yet_drawn_ends_it_at_once() {
    let posted_entries = post_own_entries(2);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let start = Instant::now();

    toast_queue.request_withdraw();
    let is_preview_changed = toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);

    assert!(is_preview_changed);
    assert_eq!(
        clone_shown_preview(&toast_queue),
        build_shown_entry_preview(&posted_entries[1], 1000)
    );
}

#[test]
fn the_close_control_during_a_withdraw_changes_nothing() {
    let posted_entries = post_own_entries(1);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 4500),
        DEFAULT_TOAST_TIMING,
        false,
    );

    toast_queue.request_withdraw();
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 4750),
        DEFAULT_TOAST_TIMING,
        false,
    );

    assert_eq!(
        clone_shown_preview(&toast_queue),
        build_shown_entry_preview(&posted_entries[0], 750)
    );
}

#[test]
fn an_evicted_shown_entry_withdraws_in_half_a_second() {
    let posted_entries = post_own_entries(1);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 2000),
        DEFAULT_TOAST_TIMING,
        false,
    );

    toast_queue.remove_previews(posted_entries[0].notification_id);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 2000),
        DEFAULT_TOAST_TIMING,
        false,
    );
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 2250),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let halfway_out = clone_shown_preview(&toast_queue);
    let is_preview_changed = toast_queue.advance(
        compute_instant_after_milliseconds(start, 2500),
        DEFAULT_TOAST_TIMING,
        false,
    );

    assert_eq!(
        halfway_out,
        build_shown_entry_preview(&posted_entries[0], 500)
    );
    assert!(is_preview_changed);
    assert_eq!(clone_shown_preview(&toast_queue), None);
}

#[test]
fn an_evicted_waiting_entry_loses_its_preview() {
    let posted_entries = post_own_entries(3);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);

    toast_queue.remove_previews(posted_entries[1].notification_id);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 1500),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let next_preview = clone_shown_preview(&toast_queue);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 2000),
        DEFAULT_TOAST_TIMING,
        false,
    );

    assert_eq!(
        next_preview,
        build_shown_entry_preview(&posted_entries[2], 1000)
    );
    assert_eq!(
        clone_shown_preview(&toast_queue),
        build_shown_entry_preview(&posted_entries[2], 500)
    );
}

#[test]
fn an_evicted_summary_member_leaves_the_summary_and_an_empty_summary_goes() {
    let posted_entries = post_own_entries(5);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);

    toast_queue.remove_previews(posted_entries[4].notification_id);
    for millisecond_count in [1500, 3000, 4500, 6000] {
        toast_queue.advance(
            compute_instant_after_milliseconds(start, millisecond_count),
            DEFAULT_TOAST_TIMING,
            false,
        );
    }

    assert_eq!(toast_queue.waiting_summary, None);
    assert_eq!(
        clone_shown_preview(&toast_queue),
        build_shown_entry_preview(&posted_entries[3], 0)
    );
}

#[test]
fn an_evicted_last_member_of_the_shown_summary_ends_it_at_once() {
    let posted_entries = post_own_entries(5);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let start = Instant::now();
    for millisecond_count in [0, 1500, 3000, 4500, 6000] {
        toast_queue.advance(
            compute_instant_after_milliseconds(start, millisecond_count),
            DEFAULT_TOAST_TIMING,
            false,
        );
    }
    let shown_summary = clone_shown_preview(&toast_queue);

    toast_queue.remove_previews(posted_entries[4].notification_id);

    assert_eq!(
        shown_summary,
        Some((
            ToastPreview::Summary(vec![(
                posted_entries[4].notification_id,
                posted_entries[4].severity
            )]),
            1000
        ))
    );
    assert_eq!(clone_shown_preview(&toast_queue), None);
}

#[test]
fn clearing_the_previews_drops_the_shown_and_every_waiting_preview() {
    let posted_entries = post_own_entries(5);
    let mut toast_queue = build_previewing_queue(&posted_entries);

    toast_queue.clear_previews();

    assert_eq!(
        toast_queue,
        NotificationToastQueue {
            shown_toast_count: 1,
            ..NotificationToastQueue::default()
        }
    );
}

#[test]
fn a_paused_clock_holds_the_toast_and_resumes_where_it_stopped() {
    let posted_entries = post_own_entries(1);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 300),
        DEFAULT_TOAST_TIMING,
        false,
    );

    let is_preview_changed = toast_queue.advance(
        compute_instant_after_milliseconds(start, 400),
        DEFAULT_TOAST_TIMING,
        true,
    );
    let while_paused = clone_shown_preview(&toast_queue);
    let wakeup_while_paused = toast_queue.find_next_wakeup(
        compute_instant_after_milliseconds(start, 400),
        DEFAULT_TOAST_TIMING,
    );
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 10_000),
        DEFAULT_TOAST_TIMING,
        true,
    );
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 10_400),
        DEFAULT_TOAST_TIMING,
        false,
    );

    assert!(!is_preview_changed);
    assert_eq!(
        while_paused,
        build_shown_entry_preview(&posted_entries[0], 700)
    );
    assert_eq!(wakeup_while_paused, None);
    assert_eq!(
        clone_shown_preview(&toast_queue),
        build_shown_entry_preview(&posted_entries[0], 600)
    );
}

#[test]
fn a_toast_that_arrives_while_the_clock_is_paused_starts_when_it_runs_again() {
    let posted_entries = post_own_entries(1);
    let mut toast_queue = NotificationToastQueue::default();
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, true);

    toast_queue.queue_preview(&posted_entries[0]);
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 5000),
        DEFAULT_TOAST_TIMING,
        false,
    );
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 5500),
        DEFAULT_TOAST_TIMING,
        false,
    );

    assert_eq!(
        clone_shown_preview(&toast_queue),
        build_shown_entry_preview(&posted_entries[0], 500)
    );
}

#[test]
fn the_next_wakeup_is_the_next_slide_step_or_the_end_of_the_stay() {
    let posted_entries = post_own_entries(2);
    let mut toast_queue = build_previewing_queue(&posted_entries[..1]);
    let start = Instant::now();

    let before_the_first_advance = toast_queue.find_next_wakeup(start, DEFAULT_TOAST_TIMING);
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    let in_the_appear = toast_queue.find_next_wakeup(
        compute_instant_after_milliseconds(start, 120),
        DEFAULT_TOAST_TIMING,
    );
    toast_queue.advance(
        compute_instant_after_milliseconds(start, 1000),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let in_the_stay = toast_queue.find_next_wakeup(
        compute_instant_after_milliseconds(start, 1000),
        DEFAULT_TOAST_TIMING,
    );
    toast_queue.queue_preview(&posted_entries[1]);
    let in_the_capped_stay = toast_queue.find_next_wakeup(
        compute_instant_after_milliseconds(start, 1000),
        DEFAULT_TOAST_TIMING,
    );

    assert_eq!(before_the_first_advance, Some(Duration::ZERO));
    assert_eq!(in_the_appear, Some(Duration::from_millis(30)));
    assert_eq!(in_the_stay, Some(Duration::from_millis(3000)));
    assert_eq!(in_the_capped_stay, Some(Duration::from_millis(500)));
    assert_eq!(
        NotificationToastQueue::default().find_next_wakeup(start, DEFAULT_TOAST_TIMING),
        None
    );
}

#[test]
fn the_next_wakeup_in_a_slide_stops_at_the_end_of_its_phase() {
    let posted_entries = post_own_entries(2);
    let mut toast_queue = build_previewing_queue(&posted_entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);

    let near_the_end_of_the_appear = toast_queue.find_next_wakeup(
        compute_instant_after_milliseconds(start, 480),
        DEFAULT_TOAST_TIMING,
    );

    assert_eq!(near_the_end_of_the_appear, Some(Duration::from_millis(20)));
}

/// A `Warning` entry of a new session queue with `text`.
fn post_session_entry(text: &str) -> NotificationEntry {
    let mut session_queue =
        NotificationQueue::from_owner(NotificationQueueOwner::Session(SessionId::new()));
    session_queue
        .post_core_entry(
            NotificationSeverity::Warning,
            text,
            NonZeroU32::MIN,
            SystemTime::UNIX_EPOCH,
        )
        .clone()
}

#[test]
fn replaced_entries_keep_one_preview_that_shows_the_replacing_entry() {
    let posted_entries = post_own_entries(4);
    let session_entry = post_session_entry("entry 1");
    let mut toast_queue = build_previewing_queue(&posted_entries);

    toast_queue.repoint_previews(
        &[
            posted_entries[1].notification_id,
            posted_entries[3].notification_id,
        ],
        &session_entry,
    );

    assert_eq!(
        clone_shown_preview(&toast_queue),
        build_shown_entry_preview(&posted_entries[0], 1000)
    );
    assert_eq!(
        toast_queue.waiting_entries,
        VecDeque::from([session_entry, posted_entries[2].clone()])
    );
}

#[test]
fn a_replaced_shown_entry_shows_the_replacing_entry_and_its_summary_member_goes() {
    let posted_entries = post_own_entries(5);
    let session_entry = post_session_entry("entry 0");
    let mut toast_queue = build_previewing_queue(&posted_entries);

    toast_queue.repoint_previews(
        &[
            posted_entries[0].notification_id,
            posted_entries[4].notification_id,
        ],
        &session_entry,
    );

    assert_eq!(
        clone_shown_preview(&toast_queue),
        build_shown_entry_preview(&session_entry, 1000)
    );
    assert_eq!(toast_queue.waiting_summary, None);
}

#[test]
fn a_replaced_summary_member_counts_the_replacing_entry() {
    let posted_entries = post_own_entries(6);
    let mut session_entry = post_session_entry("entry 4");
    session_entry.severity = NotificationSeverity::Error;
    let mut toast_queue = build_previewing_queue(&posted_entries);

    toast_queue.repoint_previews(&[posted_entries[4].notification_id], &session_entry);

    assert_eq!(
        toast_queue.waiting_summary,
        Some(vec![
            (session_entry.notification_id, NotificationSeverity::Error),
            (
                posted_entries[5].notification_id,
                posted_entries[5].severity
            ),
        ])
    );
}
