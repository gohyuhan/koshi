//! Tests for `NotificationToastQueue`: the normal and backlog phases, the
//! cap a new successor sets and the deadline its removal restores, flood
//! control, repeats, the close control, eviction, the pause and the wakeups.

use std::time::SystemTime;

use koshi_core::ids::{ClientId, SessionId};
use koshi_core::notification::{NotificationQueue, NotificationQueueOwner};

use super::*;

/// The phase lengths of the default settings: a 3 s stay, with motion.
const DEFAULT_TOAST_TIMING: ToastTiming = ToastTiming {
    stay_duration: Duration::from_secs(3),
    should_reduce_motion: false,
};

/// `entry_count` `Warning` entries of one viewer's own queue, oldest first,
/// with the texts `entry 0`, `entry 1`, ….
fn post_entries(entry_count: usize) -> Vec<NotificationEntry> {
    let mut client_queue =
        NotificationQueue::from_owner(NotificationQueueOwner::Client(ClientId::new()));
    (0..entry_count)
        .map(|entry_index| {
            client_queue
                .post_core_entry(
                    NotificationSeverity::Warning,
                    &format!("entry {entry_index}"),
                    SystemTime::UNIX_EPOCH,
                )
                .clone()
        })
        .collect()
}

/// `millisecond_count` milliseconds after `start`.
fn after_milliseconds(start: Instant, millisecond_count: u64) -> Instant {
    start + Duration::from_millis(millisecond_count)
}

/// The shown preview of `toast_queue` and the thousandths of its width past
/// its side edge.
fn find_shown(toast_queue: &NotificationToastQueue) -> Option<(ToastPreview, u16)> {
    toast_queue
        .find_shown_preview()
        .map(|(toast_preview, hidden_per_mille)| (toast_preview.clone(), hidden_per_mille))
}

/// `entry` shown with `hidden_per_mille` thousandths of its width past its
/// side edge.
fn shown_entry(entry: &NotificationEntry, hidden_per_mille: u16) -> Option<(ToastPreview, u16)> {
    Some((ToastPreview::Entry(entry.clone()), hidden_per_mille))
}

/// A queue that previews `entries` in order.
fn queue_entries(entries: &[NotificationEntry]) -> NotificationToastQueue {
    let mut toast_queue = NotificationToastQueue::default();
    for entry in entries {
        toast_queue.queue_entry(entry);
    }
    toast_queue
}

#[test]
fn one_toast_runs_one_second_of_appear_three_of_stay_and_one_of_withdraw() {
    let entries = post_entries(1);
    let mut toast_queue = queue_entries(&entries);
    let start = Instant::now();

    let mut slide_positions = Vec::new();
    for millisecond_count in [0, 500, 1000, 3999, 4000, 4500] {
        assert!(!toast_queue.advance(
            after_milliseconds(start, millisecond_count),
            DEFAULT_TOAST_TIMING,
            false
        ));
        slide_positions.push(find_shown(&toast_queue));
    }
    let is_preview_changed =
        toast_queue.advance(after_milliseconds(start, 5000), DEFAULT_TOAST_TIMING, false);

    assert_eq!(
        slide_positions,
        vec![
            shown_entry(&entries[0], 1000),
            shown_entry(&entries[0], 500),
            shown_entry(&entries[0], 0),
            shown_entry(&entries[0], 0),
            shown_entry(&entries[0], 0),
            shown_entry(&entries[0], 500),
        ]
    );
    assert!(is_preview_changed);
    assert_eq!(find_shown(&toast_queue), None);
}

#[test]
fn a_waiting_preview_runs_each_phase_of_the_shown_toast_for_half_a_second() {
    let entries = post_entries(2);
    let mut toast_queue = queue_entries(&entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);

    toast_queue.advance(after_milliseconds(start, 250), DEFAULT_TOAST_TIMING, false);
    let first_at_250 = find_shown(&toast_queue);
    let is_preview_changed =
        toast_queue.advance(after_milliseconds(start, 1500), DEFAULT_TOAST_TIMING, false);
    let second_at_1500 = find_shown(&toast_queue);
    toast_queue.advance(after_milliseconds(start, 2000), DEFAULT_TOAST_TIMING, false);

    assert_eq!(first_at_250, shown_entry(&entries[0], 500));
    assert!(is_preview_changed);
    assert_eq!(second_at_1500, shown_entry(&entries[1], 1000));
    assert_eq!(find_shown(&toast_queue), shown_entry(&entries[1], 500));
}

#[test]
fn a_new_successor_caps_the_shown_phase_at_half_a_second_from_its_start() {
    let entries = post_entries(2);
    let mut toast_queue = queue_entries(&entries[..1]);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(after_milliseconds(start, 200), DEFAULT_TOAST_TIMING, false);
    let before_the_successor = find_shown(&toast_queue);

    toast_queue.queue_entry(&entries[1]);
    toast_queue.advance(after_milliseconds(start, 300), DEFAULT_TOAST_TIMING, false);
    let after_the_successor = find_shown(&toast_queue);
    toast_queue.advance(after_milliseconds(start, 500), DEFAULT_TOAST_TIMING, false);

    assert_eq!(before_the_successor, shown_entry(&entries[0], 800));
    assert_eq!(after_the_successor, shown_entry(&entries[0], 400));
    assert_eq!(find_shown(&toast_queue), shown_entry(&entries[0], 0));
}

#[test]
fn a_successor_past_the_capped_deadline_ends_the_shown_phase_at_the_next_advance() {
    let entries = post_entries(2);
    let mut toast_queue = queue_entries(&entries[..1]);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(after_milliseconds(start, 800), DEFAULT_TOAST_TIMING, false);
    let before_the_successor = find_shown(&toast_queue);

    toast_queue.queue_entry(&entries[1]);
    toast_queue.advance(after_milliseconds(start, 800), DEFAULT_TOAST_TIMING, false);
    let in_the_capped_stay = find_shown(&toast_queue);
    toast_queue.advance(after_milliseconds(start, 1250), DEFAULT_TOAST_TIMING, false);

    assert_eq!(before_the_successor, shown_entry(&entries[0], 200));
    assert_eq!(in_the_capped_stay, shown_entry(&entries[0], 0));
    assert_eq!(find_shown(&toast_queue), shown_entry(&entries[0], 500));
}

#[test]
fn removing_every_successor_restores_the_normal_deadline_of_the_unfinished_phase() {
    let entries = post_entries(2);
    let mut toast_queue = queue_entries(&entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(after_milliseconds(start, 300), DEFAULT_TOAST_TIMING, false);
    let with_the_successor = find_shown(&toast_queue);

    toast_queue.remove_entry(entries[1].id);
    toast_queue.advance(after_milliseconds(start, 300), DEFAULT_TOAST_TIMING, false);

    assert_eq!(with_the_successor, shown_entry(&entries[0], 400));
    assert_eq!(find_shown(&toast_queue), shown_entry(&entries[0], 700));
}

#[test]
fn removing_every_successor_never_rewinds_a_finished_phase() {
    let entries = post_entries(2);
    let mut toast_queue = queue_entries(&entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(after_milliseconds(start, 600), DEFAULT_TOAST_TIMING, false);

    toast_queue.remove_entry(entries[1].id);
    let mut slide_positions = Vec::new();
    for millisecond_count in [700, 3499, 3500, 4000] {
        toast_queue.advance(
            after_milliseconds(start, millisecond_count),
            DEFAULT_TOAST_TIMING,
            false,
        );
        slide_positions.push(find_shown(&toast_queue));
    }

    assert_eq!(
        slide_positions,
        vec![
            shown_entry(&entries[0], 0),
            shown_entry(&entries[0], 0),
            shown_entry(&entries[0], 0),
            shown_entry(&entries[0], 500),
        ]
    );
}

#[test]
fn a_toast_duration_of_five_seconds_holds_the_toast_five_seconds() {
    let entries = post_entries(1);
    let mut toast_queue = queue_entries(&entries);
    let toast_timing = ToastTiming {
        stay_duration: Duration::from_secs(5),
        should_reduce_motion: false,
    };
    let start = Instant::now();
    toast_queue.advance(start, toast_timing, false);

    toast_queue.advance(after_milliseconds(start, 5999), toast_timing, false);
    let before_the_withdraw = find_shown(&toast_queue);
    toast_queue.advance(after_milliseconds(start, 6500), toast_timing, false);
    let in_the_withdraw = find_shown(&toast_queue);
    toast_queue.advance(after_milliseconds(start, 7000), toast_timing, false);

    assert_eq!(before_the_withdraw, shown_entry(&entries[0], 0));
    assert_eq!(in_the_withdraw, shown_entry(&entries[0], 500));
    assert_eq!(find_shown(&toast_queue), None);
}

#[test]
fn reduced_motion_puts_the_toast_in_place_in_one_frame_and_removes_it_in_one() {
    let entries = post_entries(1);
    let mut toast_queue = queue_entries(&entries);
    let toast_timing = ToastTiming {
        should_reduce_motion: true,
        ..DEFAULT_TOAST_TIMING
    };
    let start = Instant::now();

    toast_queue.advance(start, toast_timing, false);
    let first_frame = find_shown(&toast_queue);
    toast_queue.advance(after_milliseconds(start, 2999), toast_timing, false);
    let last_stay_frame = find_shown(&toast_queue);
    let is_preview_changed =
        toast_queue.advance(after_milliseconds(start, 3000), toast_timing, false);

    assert_eq!(first_frame, shown_entry(&entries[0], 0));
    assert_eq!(last_stay_frame, shown_entry(&entries[0], 0));
    assert!(is_preview_changed);
    assert_eq!(find_shown(&toast_queue), None);
}

#[test]
fn forty_entries_give_four_previews_then_one_summary_that_counts_thirty_six() {
    let entries = post_entries(40);
    let mut toast_queue = queue_entries(&entries);
    let start = Instant::now();

    let mut shown_previews = Vec::new();
    for millisecond_count in [0, 1500, 3000, 4500, 6000, 9999] {
        toast_queue.advance(
            after_milliseconds(start, millisecond_count),
            DEFAULT_TOAST_TIMING,
            false,
        );
        shown_previews.push(find_shown(&toast_queue));
    }
    toast_queue.advance(
        after_milliseconds(start, 11_000),
        DEFAULT_TOAST_TIMING,
        false,
    );

    let summary_members: Vec<(NotificationId, NotificationSeverity)> = entries[4..]
        .iter()
        .map(|entry| (entry.id, entry.severity))
        .collect();
    assert_eq!(summary_members.len(), 36);
    assert_eq!(
        shown_previews,
        vec![
            shown_entry(&entries[0], 1000),
            shown_entry(&entries[1], 1000),
            shown_entry(&entries[2], 1000),
            shown_entry(&entries[3], 1000),
            Some((ToastPreview::Summary(summary_members.clone()), 1000)),
            Some((ToastPreview::Summary(summary_members), 0)),
        ]
    );
    assert_eq!(find_shown(&toast_queue), None);
}

#[test]
fn a_repeat_updates_its_shown_and_its_waiting_preview_and_queues_nothing() {
    let entries = post_entries(2);
    let mut toast_queue = queue_entries(&entries);
    let mut repeated_first_entry = entries[0].clone();
    repeated_first_entry.repeat_count = 2;
    let mut repeated_second_entry = entries[1].clone();
    repeated_second_entry.repeat_count = 2;
    let start = Instant::now();

    toast_queue.queue_entry(&repeated_first_entry);
    toast_queue.queue_entry(&repeated_second_entry);
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    let first_preview = find_shown(&toast_queue);
    toast_queue.advance(after_milliseconds(start, 1500), DEFAULT_TOAST_TIMING, false);
    let second_preview = find_shown(&toast_queue);
    toast_queue.advance(after_milliseconds(start, 6500), DEFAULT_TOAST_TIMING, false);

    assert_eq!(first_preview, shown_entry(&repeated_first_entry, 1000));
    assert_eq!(second_preview, shown_entry(&repeated_second_entry, 1000));
    assert_eq!(find_shown(&toast_queue), None);
}

#[test]
fn a_repeat_counted_by_a_summary_counts_once() {
    let entries = post_entries(5);
    let mut toast_queue = queue_entries(&entries);
    let mut repeated_last_entry = entries[4].clone();
    repeated_last_entry.repeat_count = 2;

    toast_queue.queue_entry(&repeated_last_entry);

    assert_eq!(
        toast_queue.waiting_summary,
        Some(vec![(entries[4].id, entries[4].severity)])
    );
}

#[test]
fn a_repeat_after_its_toast_ended_shows_a_new_preview() {
    let entries = post_entries(1);
    let mut toast_queue = queue_entries(&entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(after_milliseconds(start, 5000), DEFAULT_TOAST_TIMING, false);
    let mut repeated_entry = entries[0].clone();
    repeated_entry.repeat_count = 2;

    toast_queue.queue_entry(&repeated_entry);

    assert_eq!(find_shown(&toast_queue), shown_entry(&repeated_entry, 1000));
}

#[test]
fn the_close_control_withdraws_the_toast_from_where_it_stands() {
    let entries = post_entries(1);
    let mut toast_queue = queue_entries(&entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(after_milliseconds(start, 300), DEFAULT_TOAST_TIMING, false);

    toast_queue.request_withdraw();
    let wakeup_after_the_press =
        toast_queue.find_next_wakeup(after_milliseconds(start, 300), DEFAULT_TOAST_TIMING);
    toast_queue.advance(after_milliseconds(start, 300), DEFAULT_TOAST_TIMING, false);
    let at_the_press = find_shown(&toast_queue);
    toast_queue.advance(after_milliseconds(start, 800), DEFAULT_TOAST_TIMING, false);
    let halfway_out = find_shown(&toast_queue);
    let is_preview_changed =
        toast_queue.advance(after_milliseconds(start, 1300), DEFAULT_TOAST_TIMING, false);

    assert_eq!(wakeup_after_the_press, Some(Duration::ZERO));
    assert_eq!(at_the_press, shown_entry(&entries[0], 700));
    assert_eq!(halfway_out, shown_entry(&entries[0], 850));
    assert!(is_preview_changed);
    assert_eq!(find_shown(&toast_queue), None);
}

#[test]
fn the_close_control_on_a_toast_not_yet_drawn_ends_it_at_once() {
    let entries = post_entries(2);
    let mut toast_queue = queue_entries(&entries);
    let start = Instant::now();

    toast_queue.request_withdraw();
    let is_preview_changed = toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);

    assert!(is_preview_changed);
    assert_eq!(find_shown(&toast_queue), shown_entry(&entries[1], 1000));
}

#[test]
fn the_close_control_during_a_withdraw_changes_nothing() {
    let entries = post_entries(1);
    let mut toast_queue = queue_entries(&entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(after_milliseconds(start, 4500), DEFAULT_TOAST_TIMING, false);

    toast_queue.request_withdraw();
    toast_queue.advance(after_milliseconds(start, 4750), DEFAULT_TOAST_TIMING, false);

    assert_eq!(find_shown(&toast_queue), shown_entry(&entries[0], 750));
}

#[test]
fn an_evicted_shown_entry_withdraws_in_half_a_second() {
    let entries = post_entries(1);
    let mut toast_queue = queue_entries(&entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(after_milliseconds(start, 2000), DEFAULT_TOAST_TIMING, false);

    toast_queue.remove_entry(entries[0].id);
    toast_queue.advance(after_milliseconds(start, 2000), DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(after_milliseconds(start, 2250), DEFAULT_TOAST_TIMING, false);
    let halfway_out = find_shown(&toast_queue);
    let is_preview_changed =
        toast_queue.advance(after_milliseconds(start, 2500), DEFAULT_TOAST_TIMING, false);

    assert_eq!(halfway_out, shown_entry(&entries[0], 500));
    assert!(is_preview_changed);
    assert_eq!(find_shown(&toast_queue), None);
}

#[test]
fn an_evicted_waiting_entry_loses_its_preview() {
    let entries = post_entries(3);
    let mut toast_queue = queue_entries(&entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);

    toast_queue.remove_entry(entries[1].id);
    toast_queue.advance(after_milliseconds(start, 1500), DEFAULT_TOAST_TIMING, false);
    let next_preview = find_shown(&toast_queue);
    toast_queue.advance(after_milliseconds(start, 2000), DEFAULT_TOAST_TIMING, false);

    assert_eq!(next_preview, shown_entry(&entries[2], 1000));
    assert_eq!(find_shown(&toast_queue), shown_entry(&entries[2], 500));
}

#[test]
fn an_evicted_summary_member_leaves_the_summary_and_an_empty_summary_goes() {
    let entries = post_entries(5);
    let mut toast_queue = queue_entries(&entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);

    toast_queue.remove_entry(entries[4].id);
    for millisecond_count in [1500, 3000, 4500, 6000] {
        toast_queue.advance(
            after_milliseconds(start, millisecond_count),
            DEFAULT_TOAST_TIMING,
            false,
        );
    }

    assert_eq!(toast_queue.waiting_summary, None);
    assert_eq!(find_shown(&toast_queue), shown_entry(&entries[3], 0));
}

#[test]
fn an_evicted_last_member_of_the_shown_summary_ends_it_at_once() {
    let entries = post_entries(5);
    let mut toast_queue = queue_entries(&entries);
    let start = Instant::now();
    for millisecond_count in [0, 1500, 3000, 4500, 6000] {
        toast_queue.advance(
            after_milliseconds(start, millisecond_count),
            DEFAULT_TOAST_TIMING,
            false,
        );
    }
    let shown_summary = find_shown(&toast_queue);

    toast_queue.remove_entry(entries[4].id);

    assert_eq!(
        shown_summary,
        Some((
            ToastPreview::Summary(vec![(entries[4].id, entries[4].severity)]),
            1000
        ))
    );
    assert_eq!(find_shown(&toast_queue), None);
}

#[test]
fn clearing_the_previews_drops_the_shown_and_every_waiting_preview() {
    let entries = post_entries(5);
    let mut toast_queue = queue_entries(&entries);

    toast_queue.clear_previews();

    assert_eq!(toast_queue, NotificationToastQueue::default());
}

#[test]
fn a_paused_clock_holds_the_toast_and_resumes_where_it_stopped() {
    let entries = post_entries(1);
    let mut toast_queue = queue_entries(&entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(after_milliseconds(start, 300), DEFAULT_TOAST_TIMING, false);

    let is_preview_changed =
        toast_queue.advance(after_milliseconds(start, 400), DEFAULT_TOAST_TIMING, true);
    let while_paused = find_shown(&toast_queue);
    let wakeup_while_paused =
        toast_queue.find_next_wakeup(after_milliseconds(start, 400), DEFAULT_TOAST_TIMING);
    toast_queue.advance(
        after_milliseconds(start, 10_000),
        DEFAULT_TOAST_TIMING,
        true,
    );
    toast_queue.advance(
        after_milliseconds(start, 10_400),
        DEFAULT_TOAST_TIMING,
        false,
    );

    assert!(!is_preview_changed);
    assert_eq!(while_paused, shown_entry(&entries[0], 700));
    assert_eq!(wakeup_while_paused, None);
    assert_eq!(find_shown(&toast_queue), shown_entry(&entries[0], 600));
}

#[test]
fn a_toast_that_arrives_while_the_clock_is_paused_starts_when_it_runs_again() {
    let entries = post_entries(1);
    let mut toast_queue = NotificationToastQueue::default();
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, true);

    toast_queue.queue_entry(&entries[0]);
    toast_queue.advance(after_milliseconds(start, 5000), DEFAULT_TOAST_TIMING, false);
    toast_queue.advance(after_milliseconds(start, 5500), DEFAULT_TOAST_TIMING, false);

    assert_eq!(find_shown(&toast_queue), shown_entry(&entries[0], 500));
}

#[test]
fn the_next_wakeup_is_the_next_slide_step_or_the_end_of_the_stay() {
    let entries = post_entries(2);
    let mut toast_queue = queue_entries(&entries[..1]);
    let start = Instant::now();

    let before_the_first_advance = toast_queue.find_next_wakeup(start, DEFAULT_TOAST_TIMING);
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);
    let in_the_appear =
        toast_queue.find_next_wakeup(after_milliseconds(start, 120), DEFAULT_TOAST_TIMING);
    toast_queue.advance(after_milliseconds(start, 1000), DEFAULT_TOAST_TIMING, false);
    let in_the_stay =
        toast_queue.find_next_wakeup(after_milliseconds(start, 1000), DEFAULT_TOAST_TIMING);
    toast_queue.queue_entry(&entries[1]);
    let in_the_capped_stay =
        toast_queue.find_next_wakeup(after_milliseconds(start, 1000), DEFAULT_TOAST_TIMING);

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
    let entries = post_entries(2);
    let mut toast_queue = queue_entries(&entries);
    let start = Instant::now();
    toast_queue.advance(start, DEFAULT_TOAST_TIMING, false);

    let near_the_end_of_the_appear =
        toast_queue.find_next_wakeup(after_milliseconds(start, 480), DEFAULT_TOAST_TIMING);

    assert_eq!(near_the_end_of_the_appear, Some(Duration::from_millis(20)));
}

/// A `Warning` entry of a new session queue with `text`.
fn post_session_entry(text: &str) -> NotificationEntry {
    let mut session_queue =
        NotificationQueue::from_owner(NotificationQueueOwner::Session(SessionId::new()));
    session_queue
        .post_core_entry(NotificationSeverity::Warning, text, SystemTime::UNIX_EPOCH)
        .clone()
}

#[test]
fn replaced_entries_keep_one_preview_that_shows_the_replacing_entry() {
    let entries = post_entries(4);
    let session_entry = post_session_entry("entry 1");
    let mut toast_queue = queue_entries(&entries);

    toast_queue.replace_entries(&[entries[1].id, entries[3].id], &session_entry);

    assert_eq!(find_shown(&toast_queue), shown_entry(&entries[0], 1000));
    assert_eq!(
        toast_queue.waiting_entries,
        VecDeque::from([session_entry, entries[2].clone()])
    );
}

#[test]
fn a_replaced_shown_entry_shows_the_replacing_entry_and_its_summary_member_goes() {
    let entries = post_entries(5);
    let session_entry = post_session_entry("entry 0");
    let mut toast_queue = queue_entries(&entries);

    toast_queue.replace_entries(&[entries[0].id, entries[4].id], &session_entry);

    assert_eq!(find_shown(&toast_queue), shown_entry(&session_entry, 1000));
    assert_eq!(toast_queue.waiting_summary, None);
}

#[test]
fn a_replaced_summary_member_counts_the_replacing_entry() {
    let entries = post_entries(6);
    let mut session_entry = post_session_entry("entry 4");
    session_entry.severity = NotificationSeverity::Error;
    let mut toast_queue = queue_entries(&entries);

    toast_queue.replace_entries(&[entries[4].id], &session_entry);

    assert_eq!(
        toast_queue.waiting_summary,
        Some(vec![
            (session_entry.id, NotificationSeverity::Error),
            (entries[5].id, entries[5].severity),
        ])
    );
}
