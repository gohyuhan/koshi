//! Tests for `NotificationHistory`: the viewer's own posts, the session's
//! queue and posts, stale and foreign input, the history bound, the unread
//! and dropped counts, clearing, the center's state, its selected card and
//! its slide, and the toast previews arrivals queue.

use std::time::{Duration, Instant, SystemTime};

use koshi_core::notification::{NotificationSource, MAX_QUEUED_NOTIFICATION_COUNT};

use super::*;

/// `seconds` after the Unix epoch.
fn compute_time_after_unix_epoch(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

/// A session id and an empty queue that session owns.
fn create_session_queue() -> (SessionId, NotificationQueue) {
    let session_id = SessionId::new();
    (
        session_id,
        NotificationQueue::from_owner(NotificationQueueOwner::Session(session_id)),
    )
}

/// Post `text` as an `Error` to `session_queue` and return what the session
/// sends for it: the revision and the entry.
fn post_to_session(session_queue: &mut NotificationQueue, text: &str) -> (u64, NotificationEntry) {
    let posted_entry = session_queue
        .post_core_entry(
            NotificationSeverity::Error,
            text,
            NonZeroU32::MIN,
            compute_time_after_unix_epoch(1),
        )
        .clone();
    (session_queue.get_revision(), posted_entry)
}

/// What the shown toast of `notification_history` shows, and the thousandths
/// of its width past its side edge, or `None` while no toast shows.
fn find_shown_toast_content(
    notification_history: &NotificationHistory,
) -> Option<(NotificationToastContent<'_>, u16)> {
    notification_history
        .find_shown_toast()
        .map(|(shown_toast, hidden_per_mille)| (shown_toast.toast_content, hidden_per_mille))
}

/// The texts of `notification_history`, oldest arrival first, each with its
/// repeat count.
fn list_merged_texts_with_repeat_counts(
    notification_history: &NotificationHistory,
) -> Vec<(String, u32)> {
    notification_history
        .list_merged_entries()
        .map(|merged_entry| (merged_entry.text.clone(), merged_entry.repeat_count))
        .collect()
}

#[test]
fn an_own_post_is_held_and_a_repeat_merges_into_it() {
    let client_id = ClientId::new();
    let mut notification_history = NotificationHistory::from_client_id(client_id);

    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        "core:resize-pane-left: below minimum size",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );
    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        "core:resize-pane-left: below minimum size",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(2),
        true,
    );

    let merged_entries: Vec<&NotificationEntry> =
        notification_history.list_merged_entries().collect();
    assert_eq!(merged_entries.len(), 1);
    assert_eq!(merged_entries[0].repeat_count, 2);
    assert_eq!(
        merged_entries[0].created_at,
        compute_time_after_unix_epoch(2)
    );
    assert_eq!(merged_entries[0].source, NotificationSource::Core);
    assert_eq!(
        merged_entries[0].notification_id.owner,
        NotificationQueueOwner::Client(client_id)
    );
    assert_eq!(merged_entries[0].notification_id.sequence, 0);
}

#[test]
fn an_own_post_of_several_repeats_merges_them_all_and_shows_one_toast() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        "core:resize-pane-left: below minimum size",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );

    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        "core:resize-pane-left: below minimum size",
        NonZeroU32::new(3).expect("3 is not zero"),
        compute_time_after_unix_epoch(2),
        true,
    );

    let newest_entry = get_newest_entry(&notification_history);
    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![("core:resize-pane-left: below minimum size".to_string(), 4)]
    );
    assert_eq!(newest_entry.created_at, compute_time_after_unix_epoch(2));
    assert_eq!(notification_history.get_unread_entry_count(), 1);
    assert_eq!(
        find_shown_toast_content(&notification_history),
        Some((NotificationToastContent::Entry(&newest_entry), 1000))
    );
}

#[test]
fn dropped_entries_add_to_the_dropped_count_and_none_changes_nothing() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let revision_before = notification_history.get_revision();

    notification_history.add_dropped_entries(0);
    assert_eq!(notification_history.get_revision(), revision_before);
    assert_eq!(
        notification_history
            .build_notification_view()
            .dropped_entry_count,
        0
    );

    notification_history.add_dropped_entries(2);
    notification_history.add_dropped_entries(u64::MAX);
    assert_ne!(notification_history.get_revision(), revision_before);
    assert_eq!(
        notification_history
            .build_notification_view()
            .dropped_entry_count,
        u64::MAX
    );
}

#[test]
fn an_own_repeat_moves_to_the_newest_place() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        "a",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );
    post_to_session(&mut session_queue, "pane …a1b2 exited 101");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue");

    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        "a",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(2),
        true,
    );

    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![
            ("pane …a1b2 exited 101".to_string(), 1),
            ("a".to_string(), 2)
        ]
    );
}

#[test]
fn a_session_queue_adds_its_entries_oldest_first() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    post_to_session(&mut session_queue, "first");
    post_to_session(&mut session_queue, "second");

    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue");

    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![("first".to_string(), 1), ("second".to_string(), 1)]
    );
}

#[test]
fn a_resync_of_entries_already_held_adds_nothing() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    post_to_session(&mut session_queue, "first");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue");

    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue again");

    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![("first".to_string(), 1)]
    );
}

#[test]
fn posts_at_the_last_sequence_saturate_the_dropped_count() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the attach queue");
    let (revision, mut last_entry) = post_to_session(&mut session_queue, "last");
    last_entry.notification_id.sequence = u64::MAX;

    notification_history
        .take_session_post(revision, last_entry.clone(), true)
        .expect("take the post at the last sequence");
    notification_history
        .take_session_post(revision + 1, last_entry, true)
        .expect("take the post at the last sequence again");

    assert_eq!(
        notification_history
            .build_notification_view()
            .dropped_entry_count,
        u64::MAX
    );
    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![("last".to_string(), 1)]
    );
}

#[test]
fn a_viewer_that_missed_posts_holds_every_entry_after_a_resync() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the attach queue");
    let (revision, first_entry) = post_to_session(&mut session_queue, "first");
    notification_history
        .take_session_post(revision, first_entry, true)
        .expect("take the first post");
    post_to_session(&mut session_queue, "second");
    post_to_session(&mut session_queue, "third");

    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the resync queue");

    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![
            ("first".to_string(), 1),
            ("second".to_string(), 1),
            ("third".to_string(), 1)
        ]
    );
}

#[test]
fn a_session_post_merged_into_a_held_entry_replaces_it_at_the_newest_place() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    post_to_session(&mut session_queue, "a");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue");
    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "own",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );

    let (revision, merged_entry) = post_to_session(&mut session_queue, "a");
    notification_history
        .take_session_post(revision, merged_entry, true)
        .expect("take the post");

    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![("own".to_string(), 1), ("a".to_string(), 2)]
    );
}

#[test]
fn a_queue_older_than_the_one_taken_is_left_untaken() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    post_to_session(&mut session_queue, "first");
    let older_queue = session_queue.clone();
    post_to_session(&mut session_queue, "first");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the newer queue");

    notification_history
        .take_session_queue(session_id, &older_queue, true)
        .expect("leave the older queue");

    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![("first".to_string(), 2)]
    );
}

#[test]
fn a_post_not_above_the_revision_taken_is_dropped() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    let (revision, stale_entry) = post_to_session(&mut session_queue, "stale");
    post_to_session(&mut session_queue, "kept");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue");
    let mut changed_entry = stale_entry;
    changed_entry.repeat_count = 9;

    notification_history
        .take_session_post(revision, changed_entry, true)
        .expect("drop the stale post");

    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![("stale".to_string(), 1), ("kept".to_string(), 1)]
    );
}

#[test]
fn a_post_above_the_revision_taken_that_carries_an_older_entry_changes_no_entry() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    let (_, older_entry) = post_to_session(&mut session_queue, "older");
    let (revision, _) = post_to_session(&mut session_queue, "newer");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue");

    notification_history
        .take_session_post(revision + 1, older_entry, true)
        .expect("take the post");

    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![("older".to_string(), 1), ("newer".to_string(), 1)]
    );
    assert_eq!(notification_history.get_unread_entry_count(), 2);
}

#[test]
fn a_queue_of_a_new_epoch_keeps_every_entry_held() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    post_to_session(&mut session_queue, "before the restart");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the first epoch");
    let mut restarted_queue =
        NotificationQueue::from_owner(NotificationQueueOwner::Session(session_id));
    post_to_session(&mut restarted_queue, "after the restart");

    notification_history
        .take_session_queue(session_id, &restarted_queue, true)
        .expect("take the new epoch");

    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![
            ("before the restart".to_string(), 1),
            ("after the restart".to_string(), 1)
        ]
    );
    let (revision, posted_entry) = post_to_session(&mut restarted_queue, "posted in the new epoch");
    notification_history
        .take_session_post(revision, posted_entry, true)
        .expect("take a post of the new epoch");
    assert_eq!(notification_history.list_merged_entries().count(), 3);
}

#[test]
fn an_entry_of_the_own_queue_and_one_of_the_session_queue_never_collide() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    notification_history.post_core_entry(
        NotificationSeverity::Error,
        "same",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );
    post_to_session(&mut session_queue, "same");

    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue");

    let sequences: Vec<u64> = notification_history
        .list_merged_entries()
        .map(|merged_entry| merged_entry.notification_id.sequence)
        .collect();
    assert_eq!(sequences, vec![0, 0]);
}

#[test]
fn a_queue_of_another_session_is_refused() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (_, mut other_queue) = create_session_queue();
    post_to_session(&mut other_queue, "elsewhere");

    let take_result = notification_history.take_session_queue(SessionId::new(), &other_queue, true);

    assert_eq!(
        take_result,
        Err(NotificationTakeError::ForeignQueue {
            owner: other_queue.get_owner()
        })
    );
    assert_eq!(notification_history.list_merged_entries().count(), 0);
}

#[test]
fn a_queue_that_fails_its_own_check_is_refused() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    let (_, posted_entry) = post_to_session(&mut session_queue, "a");
    let mut forged_queue_json = serde_json::to_value(&session_queue).expect("encode");
    let mut forged_entry = posted_entry;
    forged_entry.notification_id.owner = NotificationQueueOwner::Client(ClientId::new());
    forged_queue_json["queued_entries"][0] = serde_json::to_value(&forged_entry).expect("encode");
    let forged_queue: NotificationQueue =
        serde_json::from_value(forged_queue_json).expect("decode");

    let take_result = notification_history.take_session_queue(session_id, &forged_queue, true);

    assert_eq!(
        take_result,
        Err(NotificationTakeError::InvalidQueue(
            NotificationQueueError::ForeignEntry {
                notification_id: forged_entry.notification_id
            }
        ))
    );
    assert_eq!(notification_history.list_merged_entries().count(), 0);
}

#[test]
fn a_post_before_any_queue_is_refused() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (_, mut session_queue) = create_session_queue();
    let (revision, posted_entry) = post_to_session(&mut session_queue, "a");
    let posted_id = posted_entry.notification_id;

    let take_result = notification_history.take_session_post(revision, posted_entry, true);

    assert_eq!(
        take_result,
        Err(NotificationTakeError::PostWithoutQueue {
            notification_id: posted_id
        })
    );
}

#[test]
fn a_post_of_another_epoch_is_refused() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, session_queue) = create_session_queue();
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue");
    let mut other_epoch_queue =
        NotificationQueue::from_owner(NotificationQueueOwner::Session(session_id));
    let (revision, posted_entry) = post_to_session(&mut other_epoch_queue, "a");
    let posted_id = posted_entry.notification_id;

    let take_result = notification_history.take_session_post(revision, posted_entry, true);

    assert_eq!(
        take_result,
        Err(NotificationTakeError::ForeignPost {
            notification_id: posted_id
        })
    );
    assert_eq!(notification_history.list_merged_entries().count(), 0);
}

#[test]
fn a_full_history_evicts_its_oldest_entry_and_counts_it() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());

    for posted_entry_index in 0..=MAX_MERGED_NOTIFICATION_COUNT {
        notification_history.post_core_entry(
            NotificationSeverity::Info,
            &format!("entry {posted_entry_index}"),
            NonZeroU32::MIN,
            compute_time_after_unix_epoch(1),
            true,
        );
    }

    let texts = list_merged_texts_with_repeat_counts(&notification_history);
    assert_eq!(texts.len(), 256);
    assert_eq!(texts[0].0, "entry 1");
    assert_eq!(
        notification_history
            .build_notification_view()
            .dropped_entry_count,
        1
    );
}

#[test]
fn a_session_entry_the_history_evicted_does_not_come_back_with_a_resync() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    post_to_session(&mut session_queue, "session entry");
    post_to_session(&mut session_queue, "newest session entry");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue");
    for posted_entry_index in 0..(MAX_MERGED_NOTIFICATION_COUNT - 1) {
        notification_history.post_core_entry(
            NotificationSeverity::Info,
            &format!("own {posted_entry_index}"),
            NonZeroU32::MIN,
            compute_time_after_unix_epoch(1),
            true,
        );
    }

    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the resync queue");

    let texts = list_merged_texts_with_repeat_counts(&notification_history);
    assert_eq!(texts.len(), 256);
    assert_eq!(texts[0].0, "newest session entry");
    assert!(!texts.iter().any(|(text, _)| text == "session entry"));
    assert_eq!(
        notification_history
            .build_notification_view()
            .dropped_entry_count,
        1
    );
}

#[test]
fn a_repeat_of_an_evicted_entry_that_a_resync_carries_comes_back() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    post_to_session(&mut session_queue, "repeated");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue");
    for posted_entry_index in 0..MAX_MERGED_NOTIFICATION_COUNT {
        notification_history.post_core_entry(
            NotificationSeverity::Info,
            &format!("own {posted_entry_index}"),
            NonZeroU32::MIN,
            compute_time_after_unix_epoch(1),
            true,
        );
    }
    post_to_session(&mut session_queue, "repeated");

    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the resync queue");

    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history).last(),
        Some(&("repeated".to_string(), 2))
    );
}

#[test]
fn a_repeat_of_a_held_entry_that_a_resync_carries_moves_to_the_newest_place_as_unread() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    post_to_session(&mut session_queue, "repeated");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the attach queue");
    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "own",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );
    notification_history.open_notification_center(true);
    notification_history.close_notification_center(true);
    post_to_session(&mut session_queue, "repeated");

    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the resync queue");

    let newest_entry = get_newest_entry(&notification_history);
    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![("own".to_string(), 1), ("repeated".to_string(), 2)]
    );
    assert_eq!(notification_history.get_unread_entry_count(), 1);
    assert_eq!(
        find_shown_toast_content(&notification_history),
        Some((NotificationToastContent::Entry(&newest_entry), 1000))
    );
}

#[test]
fn a_repeat_of_a_held_entry_in_a_full_history_evicts_nothing() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    post_to_session(&mut session_queue, "repeated");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the attach queue");
    for own_entry_index in 0..(MAX_MERGED_NOTIFICATION_COUNT - 1) {
        notification_history.post_core_entry(
            NotificationSeverity::Info,
            &format!("own {own_entry_index}"),
            NonZeroU32::MIN,
            compute_time_after_unix_epoch(1),
            true,
        );
    }
    post_to_session(&mut session_queue, "repeated");

    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the resync queue");

    let texts = list_merged_texts_with_repeat_counts(&notification_history);
    assert_eq!(texts.len(), MAX_MERGED_NOTIFICATION_COUNT);
    assert_eq!(texts[0], ("own 0".to_string(), 1));
    assert_eq!(texts.last(), Some(&("repeated".to_string(), 2)));
    assert_eq!(
        notification_history
            .build_notification_view()
            .dropped_entry_count,
        0
    );
}

#[test]
fn a_repeat_of_a_held_entry_that_a_resync_carries_while_the_center_is_open_moves_to_the_top_as_read(
) {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    post_to_session(&mut session_queue, "repeated");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the attach queue");
    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "own",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );
    notification_history.open_notification_center(true);
    post_to_session(&mut session_queue, "repeated");

    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the resync queue");

    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![("own".to_string(), 1), ("repeated".to_string(), 2)]
    );
    assert_eq!(notification_history.get_unread_entry_count(), 0);
    assert_eq!(find_shown_toast_content(&notification_history), None);
    assert_eq!(
        notification_history.get_notification_center(),
        Some(NotificationCenter {
            scroll_row_offset: 0,
            selected_notification_id: Some(get_entry_id_by_text(&notification_history, "own")),
            hidden_per_mille: 0,
        })
    );
}

#[test]
fn a_received_text_loses_refused_characters_and_is_cut_to_its_bound() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue");
    let (revision, mut posted_entry) = post_to_session(&mut session_queue, "a");
    posted_entry.text = format!("\u{1b}]52;c;{}", "a".repeat(5000));

    notification_history
        .take_session_post(revision, posted_entry, true)
        .expect("take the post");

    let held_text = &notification_history
        .list_merged_entries()
        .next()
        .expect("one entry")
        .text;
    assert_eq!(
        held_text,
        &format!("]52;c;{}", "a".repeat(MAX_NOTIFICATION_TEXT_BYTE_COUNT - 6))
    );
}

#[test]
fn a_new_client_id_starts_a_new_own_queue_and_keeps_every_entry_held() {
    let first_client_id = ClientId::new();
    let second_client_id = ClientId::new();
    let mut notification_history = NotificationHistory::from_client_id(first_client_id);
    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        "a",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );

    notification_history.set_client_id(second_client_id);
    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        "a",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(2),
        true,
    );

    let held_ids: Vec<(NotificationQueueOwner, u64)> = notification_history
        .list_merged_entries()
        .map(|merged_entry| {
            (
                merged_entry.notification_id.owner,
                merged_entry.notification_id.sequence,
            )
        })
        .collect();
    assert_eq!(
        held_ids,
        vec![
            (NotificationQueueOwner::Client(first_client_id), 0),
            (NotificationQueueOwner::Client(second_client_id), 0),
        ]
    );
}

#[test]
fn the_same_client_id_keeps_the_own_queue() {
    let client_id = ClientId::new();
    let mut notification_history = NotificationHistory::from_client_id(client_id);
    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        "a",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );

    notification_history.set_client_id(client_id);
    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        "a",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(2),
        true,
    );

    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![(String::from("a"), 2)]
    );
}

/// The config warning both a viewer and its session post when they read the
/// same `koshi.kdl`.
const SHARED_CONFIG_WARNING: &str = "/home/user/.config/koshi/koshi.kdl: unknown key frobnicate";

/// The owner, severity and text of each entry `notification_history` holds,
/// oldest arrival first.
fn list_merged_owners_severities_and_texts(
    notification_history: &NotificationHistory,
) -> Vec<(NotificationQueueOwner, NotificationSeverity, String)> {
    notification_history
        .list_merged_entries()
        .map(|merged_entry| {
            (
                merged_entry.notification_id.owner,
                merged_entry.severity,
                merged_entry.text.clone(),
            )
        })
        .collect()
}

#[test]
fn a_local_session_queue_holding_the_viewers_warning_replaces_the_viewers_entry() {
    let client_id = ClientId::new();
    let mut notification_history = NotificationHistory::from_client_id(client_id);
    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        SHARED_CONFIG_WARNING,
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );
    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        "/home/user/.config/koshi/keybinding.kdl: unknown action core:frobnicate",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );
    let (session_id, mut session_queue) = create_session_queue();
    session_queue.post_core_entry(
        NotificationSeverity::Warning,
        SHARED_CONFIG_WARNING,
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(2),
    );

    notification_history.set_session_local(true);
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("the queue is taken");

    assert_eq!(
        list_merged_owners_severities_and_texts(&notification_history),
        vec![
            (
                NotificationQueueOwner::Client(client_id),
                NotificationSeverity::Warning,
                "/home/user/.config/koshi/keybinding.kdl: unknown action core:frobnicate"
                    .to_owned()
            ),
            (
                NotificationQueueOwner::Session(session_id),
                NotificationSeverity::Warning,
                SHARED_CONFIG_WARNING.to_owned()
            ),
        ]
    );
}

#[test]
fn a_local_viewer_posts_nothing_the_session_already_posted() {
    let client_id = ClientId::new();
    let mut notification_history = NotificationHistory::from_client_id(client_id);
    notification_history.set_session_local(true);
    let (session_id, mut session_queue) = create_session_queue();
    session_queue.post_core_entry(
        NotificationSeverity::Warning,
        SHARED_CONFIG_WARNING,
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
    );
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("the queue is taken");

    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        SHARED_CONFIG_WARNING,
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(2),
        true,
    );
    notification_history.post_core_entry(
        NotificationSeverity::Error,
        SHARED_CONFIG_WARNING,
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(2),
        true,
    );

    assert_eq!(
        list_merged_owners_severities_and_texts(&notification_history),
        vec![
            (
                NotificationQueueOwner::Session(session_id),
                NotificationSeverity::Warning,
                SHARED_CONFIG_WARNING.to_owned()
            ),
            (
                NotificationQueueOwner::Client(client_id),
                NotificationSeverity::Error,
                SHARED_CONFIG_WARNING.to_owned()
            ),
        ]
    );
}

#[test]
fn a_local_session_post_of_the_viewers_warning_replaces_the_viewers_entry() {
    let client_id = ClientId::new();
    let mut notification_history = NotificationHistory::from_client_id(client_id);
    notification_history.set_session_local(true);
    let (session_id, mut session_queue) = create_session_queue();
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("the empty queue is taken");
    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        SHARED_CONFIG_WARNING,
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );
    let posted_entry = session_queue
        .post_core_entry(
            NotificationSeverity::Warning,
            SHARED_CONFIG_WARNING,
            NonZeroU32::MIN,
            compute_time_after_unix_epoch(2),
        )
        .clone();

    notification_history
        .take_session_post(session_queue.get_revision(), posted_entry, true)
        .expect("the post is taken");

    assert_eq!(
        list_merged_owners_severities_and_texts(&notification_history),
        vec![(
            NotificationQueueOwner::Session(session_id),
            NotificationSeverity::Warning,
            SHARED_CONFIG_WARNING.to_owned()
        )]
    );
}

#[test]
fn a_remote_session_and_its_viewer_each_keep_their_own_warning() {
    let client_id = ClientId::new();
    let mut notification_history = NotificationHistory::from_client_id(client_id);
    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        SHARED_CONFIG_WARNING,
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );
    let (session_id, mut session_queue) = create_session_queue();
    session_queue.post_core_entry(
        NotificationSeverity::Warning,
        SHARED_CONFIG_WARNING,
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(2),
    );

    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("the queue is taken");
    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        SHARED_CONFIG_WARNING,
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(3),
        true,
    );

    assert_eq!(
        list_merged_owners_severities_and_texts(&notification_history),
        vec![
            (
                NotificationQueueOwner::Session(session_id),
                NotificationSeverity::Warning,
                SHARED_CONFIG_WARNING.to_owned()
            ),
            (
                NotificationQueueOwner::Client(client_id),
                NotificationSeverity::Warning,
                SHARED_CONFIG_WARNING.to_owned()
            ),
        ]
    );
}

/// The phase lengths of the default settings: a 3 s stay, with motion.
const DEFAULT_TOAST_TIMING: ToastTiming = ToastTiming {
    stay_duration: Duration::from_secs(3),
    should_reduce_motion: false,
};

/// The newest entry `notification_history` holds.
fn get_newest_entry(notification_history: &NotificationHistory) -> NotificationEntry {
    notification_history
        .list_merged_entries()
        .last()
        .expect("the history holds an entry")
        .clone()
}

/// The id of the entry of `notification_history` whose text is `text`.
fn get_entry_id_by_text(notification_history: &NotificationHistory, text: &str) -> NotificationId {
    notification_history
        .list_merged_entries()
        .find(|merged_entry| merged_entry.text == text)
        .expect("the history holds the entry")
        .notification_id
}

/// The texts of `notification_history`'s entries, oldest arrival first.
fn list_merged_texts(notification_history: &NotificationHistory) -> Vec<String> {
    notification_history
        .list_merged_entries()
        .map(|merged_entry| merged_entry.text.clone())
        .collect()
}

/// The text of the entry the center of `notification_history` selects, or
/// `None` while it selects none or is closed.
fn find_selected_text(notification_history: &NotificationHistory) -> Option<String> {
    let selected_notification_id = notification_history
        .get_notification_center()?
        .selected_notification_id?;
    notification_history
        .list_merged_entries()
        .find(|merged_entry| merged_entry.notification_id == selected_notification_id)
        .map(|merged_entry| merged_entry.text.clone())
}

/// A history that holds the `Info` entries of `texts`, posted oldest first.
fn build_history_with_texts(texts: &[&str]) -> NotificationHistory {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    for text in texts {
        notification_history.post_core_entry(
            NotificationSeverity::Info,
            text,
            NonZeroU32::MIN,
            compute_time_after_unix_epoch(1),
            true,
        );
    }
    notification_history
}

#[test]
fn entries_that_arrive_while_the_center_is_closed_count_as_unread() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "a",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );
    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "b",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );
    let unread_before_the_center = notification_history.get_unread_entry_count();

    notification_history.open_notification_center(true);
    let opened_center = notification_history.get_notification_center();
    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "c",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(2),
        true,
    );
    let unread_while_open = notification_history.get_unread_entry_count();
    notification_history.close_notification_center(true);
    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "d",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(3),
        true,
    );

    assert_eq!(unread_before_the_center, 2);
    assert_eq!(
        opened_center,
        Some(NotificationCenter {
            scroll_row_offset: 0,
            selected_notification_id: Some(get_entry_id_by_text(&notification_history, "b")),
            hidden_per_mille: 0,
        })
    );
    assert_eq!(unread_while_open, 0);
    assert_eq!(notification_history.get_unread_entry_count(), 1);
    assert_eq!(notification_history.get_notification_center(), None);
}

#[test]
fn a_repeat_of_a_read_entry_counts_as_unread_again() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "a",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );
    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "b",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );
    notification_history.open_notification_center(true);
    notification_history.close_notification_center(true);

    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "b",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(2),
        true,
    );

    assert_eq!(notification_history.get_unread_entry_count(), 1);
    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![("a".to_string(), 1), ("b".to_string(), 2)]
    );
}

#[test]
fn an_unread_entry_the_history_evicts_leaves_the_unread_count() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());

    for posted_entry_index in 0..=MAX_MERGED_NOTIFICATION_COUNT {
        notification_history.post_core_entry(
            NotificationSeverity::Info,
            &format!("entry {posted_entry_index}"),
            NonZeroU32::MIN,
            compute_time_after_unix_epoch(1),
            true,
        );
    }

    assert_eq!(
        notification_history.get_unread_entry_count(),
        MAX_MERGED_NOTIFICATION_COUNT
    );
    assert_eq!(
        notification_history
            .build_notification_view()
            .dropped_entry_count,
        1
    );
}

#[test]
fn a_first_queue_that_starts_past_sequence_zero_counts_the_entries_before_it_as_dropped() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    for posted_entry_index in 0..(MAX_QUEUED_NOTIFICATION_COUNT + 40) {
        post_to_session(&mut session_queue, &format!("entry {posted_entry_index}"));
    }

    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue");

    assert_eq!(
        notification_history
            .build_notification_view()
            .dropped_entry_count,
        40
    );
    assert_eq!(
        notification_history
            .list_merged_entries()
            .next()
            .map(|merged_entry| merged_entry.notification_id.sequence),
        Some(40)
    );
}

#[test]
fn a_post_that_skips_sequences_counts_them_as_dropped() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the empty queue");
    post_to_session(&mut session_queue, "missed 0");
    post_to_session(&mut session_queue, "missed 1");
    let (revision, posted_entry) = post_to_session(&mut session_queue, "received");

    notification_history
        .take_session_post(revision, posted_entry, true)
        .expect("take the post");

    assert_eq!(
        notification_history
            .build_notification_view()
            .dropped_entry_count,
        2
    );
    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![("received".to_string(), 1)]
    );
}

#[test]
fn clearing_removes_every_entry_and_count_and_keeps_the_open_center_at_its_top_with_no_selection() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    for posted_entry_index in 0..(MAX_QUEUED_NOTIFICATION_COUNT + 1) {
        post_to_session(&mut session_queue, &format!("entry {posted_entry_index}"));
    }
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue");
    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "own",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );
    notification_history.open_notification_center(true);
    notification_history.set_notification_center_scroll_row_offset(4);

    notification_history.clear_merged_entries();

    let notification_view = notification_history.build_notification_view();
    assert_eq!(notification_history.list_merged_entries().next(), None);
    assert_eq!(notification_view.dropped_entry_count, 0);
    assert!(notification_view.shown_toast.is_none());
    assert_eq!(notification_history.get_unread_entry_count(), 0);
    assert!(notification_history.is_notification_center_open());
    assert_eq!(
        notification_history.get_notification_center(),
        Some(NotificationCenter {
            scroll_row_offset: 0,
            selected_notification_id: None,
            hidden_per_mille: 0,
        })
    );
}

#[test]
fn a_resync_after_a_clear_brings_back_no_cleared_entry() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    post_to_session(&mut session_queue, "pane …a1b2 killed by signal 9");
    let older_queue = session_queue.clone();
    post_to_session(&mut session_queue, "pane …c3d4 exited 101");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue");

    notification_history.clear_merged_entries();
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the resync queue");
    notification_history
        .take_session_queue(session_id, &older_queue, true)
        .expect("leave the older queue");
    let (revision, posted_entry) = post_to_session(&mut session_queue, "pane …e5f6 exited 2");
    notification_history
        .take_session_post(revision, posted_entry, true)
        .expect("take the post");

    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![("pane …e5f6 exited 2".to_string(), 1)]
    );
}

#[test]
fn an_entry_accepted_after_a_clear_stays_whatever_its_time() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "before",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(5),
        true,
    );

    notification_history.clear_merged_entries();
    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "after",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(5),
        true,
    );
    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "after",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(5),
        true,
    );

    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![("after".to_string(), 2)]
    );
}

#[test]
fn a_queue_of_a_new_epoch_after_a_clear_adds_every_entry_of_that_epoch() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    post_to_session(&mut session_queue, "first epoch");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the first epoch");
    notification_history.clear_merged_entries();
    let mut restarted_queue =
        NotificationQueue::from_owner(NotificationQueueOwner::Session(session_id));
    post_to_session(&mut restarted_queue, "second epoch 0");
    post_to_session(&mut restarted_queue, "second epoch 1");

    notification_history
        .take_session_queue(session_id, &restarted_queue, true)
        .expect("take the new epoch");
    let (revision, old_epoch_entry) = post_to_session(&mut session_queue, "first epoch late");
    let old_epoch_entry_id = old_epoch_entry.notification_id;
    let old_epoch_post_result =
        notification_history.take_session_post(revision, old_epoch_entry, true);

    assert_eq!(
        list_merged_texts_with_repeat_counts(&notification_history),
        vec![
            ("second epoch 0".to_string(), 1),
            ("second epoch 1".to_string(), 1)
        ]
    );
    assert_eq!(
        old_epoch_post_result,
        Err(NotificationTakeError::ForeignPost {
            notification_id: old_epoch_entry_id
        })
    );
}

#[test]
fn the_first_queue_of_a_session_previews_nothing_and_a_later_queue_previews_its_new_entries() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    post_to_session(&mut session_queue, "before the attach");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the attach queue");
    assert_eq!(find_shown_toast_content(&notification_history), None);

    post_to_session(&mut session_queue, "after the attach");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the resync queue");

    let newest_entry = get_newest_entry(&notification_history);
    assert_eq!(
        find_shown_toast_content(&notification_history),
        Some((NotificationToastContent::Entry(&newest_entry), 1000))
    );
    assert_eq!(newest_entry.text, "after the attach");
}

#[test]
fn an_arrival_previews_nothing_while_toasts_are_off() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());

    notification_history.post_core_entry(
        NotificationSeverity::Error,
        "a",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        false,
    );

    assert_eq!(find_shown_toast_content(&notification_history), None);
    assert_eq!(notification_history.get_unread_entry_count(), 1);
}

#[test]
fn opening_the_center_drops_every_preview_and_arrivals_while_it_is_open_preview_nothing() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "a",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );
    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "b",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );

    notification_history.open_notification_center(true);
    assert_eq!(find_shown_toast_content(&notification_history), None);
    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "c",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(2),
        true,
    );
    notification_history.close_notification_center(true);
    assert_eq!(find_shown_toast_content(&notification_history), None);
    notification_history.post_core_entry(
        NotificationSeverity::Info,
        "d",
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(3),
        true,
    );

    let newest_entry = get_newest_entry(&notification_history);
    assert_eq!(
        find_shown_toast_content(&notification_history),
        Some((NotificationToastContent::Entry(&newest_entry), 1000))
    );
    assert_eq!(notification_history.list_merged_entries().count(), 4);
}

#[test]
fn forty_posts_end_in_a_summary_of_thirty_six_at_the_loudest_severity() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    for posted_entry_index in 0..40 {
        let severity = if posted_entry_index == 20 {
            NotificationSeverity::Error
        } else {
            NotificationSeverity::Warning
        };
        notification_history.post_core_entry(
            severity,
            &format!("pane {posted_entry_index} exited 1"),
            NonZeroU32::MIN,
            compute_time_after_unix_epoch(1),
            true,
        );
    }
    let start = Instant::now();

    for millisecond_count in [0, 1500, 3000, 4500, 6000] {
        notification_history.advance_notification_clocks(
            start + Duration::from_millis(millisecond_count),
            DEFAULT_TOAST_TIMING,
            false,
        );
    }

    assert_eq!(
        find_shown_toast_content(&notification_history),
        Some((
            NotificationToastContent::Summary {
                notification_count: 36,
                severity: NotificationSeverity::Error,
            },
            1000
        ))
    );
    assert_eq!(notification_history.list_merged_entries().count(), 40);
}

#[test]
fn an_evicted_entry_loses_its_preview() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    for posted_entry_index in 0..=MAX_MERGED_NOTIFICATION_COUNT {
        notification_history.post_core_entry(
            NotificationSeverity::Info,
            &format!("entry {posted_entry_index}"),
            NonZeroU32::MIN,
            compute_time_after_unix_epoch(1),
            true,
        );
    }

    notification_history.advance_notification_clocks(Instant::now(), DEFAULT_TOAST_TIMING, false);

    let second_entry = notification_history
        .list_merged_entries()
        .next()
        .expect("the history holds an entry")
        .clone();
    assert_eq!(second_entry.text, "entry 1");
    assert_eq!(
        find_shown_toast_content(&notification_history),
        Some((NotificationToastContent::Entry(&second_entry), 1000))
    );
}

#[test]
fn a_local_session_entry_takes_over_the_preview_of_the_viewers_own_entry() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    notification_history.set_session_local(true);
    let (session_id, mut session_queue) = create_session_queue();
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the empty queue");
    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        SHARED_CONFIG_WARNING,
        NonZeroU32::MIN,
        compute_time_after_unix_epoch(1),
        true,
    );
    let session_entry = session_queue
        .post_core_entry(
            NotificationSeverity::Warning,
            SHARED_CONFIG_WARNING,
            NonZeroU32::MIN,
            compute_time_after_unix_epoch(2),
        )
        .clone();

    notification_history
        .take_session_post(session_queue.get_revision(), session_entry.clone(), true)
        .expect("take the post");

    assert_eq!(
        find_shown_toast_content(&notification_history),
        Some((NotificationToastContent::Entry(&session_entry), 1000))
    );
}

#[test]
fn each_change_to_the_center_counts_one_revision_and_no_change_counts_none() {
    let mut notification_history = build_history_with_texts(&["a", "b"]);
    let start_revision = notification_history.get_revision();
    let mut revision_steps = vec![0];
    let mut record_revision_step = |notification_history: &NotificationHistory| {
        revision_steps.push(notification_history.get_revision() - start_revision);
    };

    notification_history.open_notification_center(true);
    record_revision_step(&notification_history);
    notification_history.open_notification_center(true);
    record_revision_step(&notification_history);
    notification_history.select_adjacent_entry(false);
    record_revision_step(&notification_history);
    notification_history.select_adjacent_entry(false);
    record_revision_step(&notification_history);
    notification_history.set_notification_center_scroll_row_offset(3);
    record_revision_step(&notification_history);
    notification_history.set_notification_center_scroll_row_offset(3);
    record_revision_step(&notification_history);
    notification_history.close_notification_center(true);
    record_revision_step(&notification_history);
    notification_history.close_notification_center(true);
    record_revision_step(&notification_history);

    assert_eq!(revision_steps, vec![0, 1, 1, 2, 2, 3, 3, 4, 4]);
}

#[test]
fn opening_the_center_selects_the_newest_entry_and_an_empty_center_selects_none() {
    let mut notification_history = build_history_with_texts(&["a", "b", "c"]);
    let mut empty_history = build_history_with_texts(&[]);

    notification_history.open_notification_center(true);
    empty_history.open_notification_center(true);

    assert_eq!(
        find_selected_text(&notification_history),
        Some("c".to_string())
    );
    assert_eq!(
        empty_history.get_notification_center(),
        Some(NotificationCenter {
            scroll_row_offset: 0,
            selected_notification_id: None,
            hidden_per_mille: 0,
        })
    );
}

#[test]
fn an_arrival_while_an_open_center_selects_nothing_is_selected_and_otherwise_keeps_the_selection() {
    let mut empty_history = build_history_with_texts(&[]);
    empty_history.open_notification_center(true);
    let mut notification_history = build_history_with_texts(&["a", "b"]);
    notification_history.open_notification_center(true);
    notification_history.select_adjacent_entry(false);

    for history in [&mut empty_history, &mut notification_history] {
        history.post_core_entry(
            NotificationSeverity::Info,
            "new",
            NonZeroU32::MIN,
            compute_time_after_unix_epoch(2),
            true,
        );
    }

    assert_eq!(find_selected_text(&empty_history), Some("new".to_string()));
    assert_eq!(
        find_selected_text(&notification_history),
        Some("a".to_string())
    );
}

#[test]
fn adjacent_selection_steps_one_entry_and_stops_at_the_newest_and_the_oldest() {
    let mut notification_history = build_history_with_texts(&["a", "b", "c"]);
    let closed_step = notification_history.select_adjacent_entry(false);
    notification_history.open_notification_center(true);

    let newer_than_newest = notification_history.select_adjacent_entry(true);
    let first_older = notification_history.select_adjacent_entry(false);
    let second_older = notification_history.select_adjacent_entry(false);
    let older_than_oldest = notification_history.select_adjacent_entry(false);
    let newer = notification_history.select_adjacent_entry(true);

    assert_eq!(closed_step, None);
    assert_eq!(newer_than_newest, None);
    assert_eq!(
        first_older,
        Some(get_entry_id_by_text(&notification_history, "b"))
    );
    assert_eq!(
        second_older,
        Some(get_entry_id_by_text(&notification_history, "a"))
    );
    assert_eq!(older_than_oldest, None);
    assert_eq!(
        newer,
        Some(get_entry_id_by_text(&notification_history, "b"))
    );
    assert_eq!(
        find_selected_text(&notification_history),
        Some("b".to_string())
    );
}

#[test]
fn dismissing_the_selected_entry_selects_the_next_older_else_the_next_newer_else_none() {
    let mut notification_history = build_history_with_texts(&["a", "b", "c"]);
    notification_history.open_notification_center(true);
    notification_history.select_adjacent_entry(false);

    notification_history.dismiss_merged_entry(get_entry_id_by_text(&notification_history, "b"));
    let after_dismissing_b = (
        list_merged_texts(&notification_history),
        find_selected_text(&notification_history),
    );
    notification_history.dismiss_merged_entry(get_entry_id_by_text(&notification_history, "a"));
    let after_dismissing_a = (
        list_merged_texts(&notification_history),
        find_selected_text(&notification_history),
    );
    notification_history.dismiss_merged_entry(get_entry_id_by_text(&notification_history, "c"));

    assert_eq!(
        after_dismissing_b,
        (
            vec!["a".to_string(), "c".to_string()],
            Some("a".to_string())
        )
    );
    assert_eq!(
        after_dismissing_a,
        (vec!["c".to_string()], Some("c".to_string()))
    );
    assert_eq!(
        list_merged_texts(&notification_history),
        Vec::<String>::new()
    );
    assert_eq!(
        notification_history
            .get_notification_center()
            .map(|notification_center| notification_center.selected_notification_id),
        Some(None)
    );
}

#[test]
fn dismissing_an_entry_that_is_not_selected_keeps_the_selection() {
    let mut notification_history = build_history_with_texts(&["a", "b"]);
    notification_history.open_notification_center(true);

    notification_history.dismiss_merged_entry(get_entry_id_by_text(&notification_history, "a"));

    assert_eq!(
        list_merged_texts(&notification_history),
        vec!["b".to_string()]
    );
    assert_eq!(
        find_selected_text(&notification_history),
        Some("b".to_string())
    );
}

#[test]
fn selecting_an_entry_needs_an_open_center_and_a_held_entry() {
    let mut notification_history = build_history_with_texts(&["a", "b"]);
    let a_notification_id = get_entry_id_by_text(&notification_history, "a");
    notification_history.select_entry(a_notification_id);
    let while_closed = notification_history.get_notification_center();
    notification_history.open_notification_center(false);
    notification_history.close_notification_center(false);
    notification_history.select_entry(a_notification_id);
    let while_sliding_out = find_selected_text(&notification_history);
    notification_history.open_notification_center(true);

    notification_history.select_entry(a_notification_id);
    let while_open = find_selected_text(&notification_history);
    notification_history.select_entry(
        build_history_with_texts(&["x"])
            .list_merged_entries()
            .next()
            .expect("the history holds an entry")
            .notification_id,
    );

    assert_eq!(while_closed, None);
    assert_eq!(while_sliding_out, Some("b".to_string()));
    assert_eq!(while_open, Some("a".to_string()));
    assert_eq!(
        find_selected_text(&notification_history),
        Some("a".to_string())
    );
}

/// The thousandths of the center of `notification_history` past the pane
/// area's edge, or `None` while it is closed.
fn find_center_hidden_per_mille(notification_history: &NotificationHistory) -> Option<u16> {
    notification_history
        .get_notification_center()
        .map(|notification_center| notification_center.hidden_per_mille)
}

#[test]
fn an_opened_center_slides_in_over_160_ms_from_its_first_advance() {
    let mut notification_history = build_history_with_texts(&["a"]);
    let start = Instant::now();

    notification_history.open_notification_center(false);
    let before_the_first_advance = find_center_hidden_per_mille(&notification_history);
    let mut hidden_per_mille_steps = Vec::new();
    for millisecond_count in [0, 80, 160, 400] {
        notification_history.advance_notification_clocks(
            start + Duration::from_millis(millisecond_count),
            DEFAULT_TOAST_TIMING,
            false,
        );
        hidden_per_mille_steps.push(find_center_hidden_per_mille(&notification_history));
    }

    assert!(notification_history.is_notification_center_open());
    assert_eq!(before_the_first_advance, Some(1000));
    assert_eq!(
        hidden_per_mille_steps,
        [Some(1000), Some(125), Some(0), Some(0)]
    );
}

#[test]
fn a_closed_center_slides_out_takes_no_input_and_goes_when_the_slide_ends() {
    let mut notification_history = build_history_with_texts(&["a"]);
    notification_history.open_notification_center(true);
    let start = Instant::now();

    notification_history.close_notification_center(false);
    let is_open_while_sliding_out = notification_history.is_notification_center_open();
    notification_history.set_notification_center_scroll_row_offset(2);
    let select_step = notification_history.select_adjacent_entry(false);
    let mut hidden_per_mille_steps = Vec::new();
    for millisecond_count in [0, 80, 160] {
        notification_history.advance_notification_clocks(
            start + Duration::from_millis(millisecond_count),
            DEFAULT_TOAST_TIMING,
            false,
        );
        hidden_per_mille_steps.push(find_center_hidden_per_mille(&notification_history));
    }

    assert!(!is_open_while_sliding_out);
    assert_eq!(select_step, None);
    assert_eq!(hidden_per_mille_steps, [Some(0), Some(875), None]);
    assert_eq!(notification_history.get_notification_center(), None);
}

#[test]
fn a_center_reopened_while_it_slides_out_slides_in_from_where_it_stands() {
    let mut notification_history = build_history_with_texts(&["a"]);
    notification_history.open_notification_center(true);
    let start = Instant::now();
    notification_history.close_notification_center(false);
    notification_history.advance_notification_clocks(start, DEFAULT_TOAST_TIMING, false);
    notification_history.advance_notification_clocks(
        start + Duration::from_millis(80),
        DEFAULT_TOAST_TIMING,
        false,
    );

    notification_history.open_notification_center(false);
    let is_open_after_reopening = notification_history.is_notification_center_open();
    let reopen_start = start + Duration::from_millis(100);
    let mut hidden_per_mille_steps = Vec::new();
    for millisecond_count in [0, 80, 160] {
        notification_history.advance_notification_clocks(
            reopen_start + Duration::from_millis(millisecond_count),
            DEFAULT_TOAST_TIMING,
            false,
        );
        hidden_per_mille_steps.push(find_center_hidden_per_mille(&notification_history));
    }

    assert!(is_open_after_reopening);
    // 875 + (0 - 875) x 0.875 is 109.375.
    assert_eq!(hidden_per_mille_steps, [Some(875), Some(109), Some(0)]);
}

#[test]
fn with_reduced_motion_the_center_opens_in_place_and_closes_at_once() {
    let mut notification_history = build_history_with_texts(&["a"]);

    notification_history.open_notification_center(true);
    let opened_center = notification_history.get_notification_center();
    notification_history.close_notification_center(true);

    assert_eq!(
        opened_center.map(|notification_center| notification_center.hidden_per_mille),
        Some(0)
    );
    assert_eq!(notification_history.get_notification_center(), None);
}

#[test]
fn a_sliding_center_advanced_with_reduced_motion_ends_its_slide_at_once() {
    let mut opening_history = build_history_with_texts(&["a"]);
    opening_history.open_notification_center(false);
    let mut closing_history = build_history_with_texts(&["a"]);
    closing_history.open_notification_center(true);
    closing_history.close_notification_center(false);
    let reduced_motion_timing = ToastTiming {
        should_reduce_motion: true,
        ..DEFAULT_TOAST_TIMING
    };

    opening_history.advance_notification_clocks(Instant::now(), reduced_motion_timing, false);
    closing_history.advance_notification_clocks(Instant::now(), reduced_motion_timing, false);

    assert_eq!(find_center_hidden_per_mille(&opening_history), Some(0));
    assert_eq!(closing_history.get_notification_center(), None);
}

#[test]
fn removing_the_center_takes_it_away_at_once_open_or_sliding_out() {
    let mut open_history = build_history_with_texts(&["a"]);
    open_history.open_notification_center(true);
    let mut sliding_out_history = build_history_with_texts(&["a"]);
    sliding_out_history.open_notification_center(true);
    sliding_out_history.close_notification_center(false);

    open_history.remove_notification_center();
    sliding_out_history.remove_notification_center();

    assert_eq!(open_history.get_notification_center(), None);
    assert_eq!(sliding_out_history.get_notification_center(), None);
    assert_eq!(list_merged_texts(&open_history), vec!["a".to_string()]);
}

#[test]
fn a_sliding_center_wakes_at_once_before_its_first_advance_then_once_per_frame_until_its_end() {
    let mut notification_history = build_history_with_texts(&[]);
    let frame_interval = Duration::from_millis(16);
    let start = Instant::now();
    let find_wakeup = |notification_history: &NotificationHistory, millisecond_count: u64| {
        notification_history.find_next_notification_wakeup(
            start + Duration::from_millis(millisecond_count),
            DEFAULT_TOAST_TIMING,
            frame_interval,
        )
    };
    let closed_wakeup = find_wakeup(&notification_history, 0);

    notification_history.open_notification_center(false);
    let unstarted_wakeup = find_wakeup(&notification_history, 0);
    notification_history.advance_notification_clocks(start, DEFAULT_TOAST_TIMING, false);
    let early_wakeup = find_wakeup(&notification_history, 10);
    let late_wakeup = find_wakeup(&notification_history, 150);
    notification_history.advance_notification_clocks(
        start + Duration::from_millis(160),
        DEFAULT_TOAST_TIMING,
        false,
    );
    let in_place_wakeup = find_wakeup(&notification_history, 160);

    assert_eq!(closed_wakeup, None);
    assert_eq!(unstarted_wakeup, Some(Duration::ZERO));
    assert_eq!(early_wakeup, Some(frame_interval));
    assert_eq!(late_wakeup, Some(Duration::from_millis(10)));
    assert_eq!(in_place_wakeup, None);
}

#[test]
fn a_restored_center_state_puts_back_the_slide_the_scroll_and_the_selection() {
    let mut notification_history = build_history_with_texts(&["a", "b"]);
    notification_history.open_notification_center(false);
    notification_history.select_adjacent_entry(false);
    notification_history.set_notification_center_scroll_row_offset(2);
    let saved_center_state = notification_history.get_notification_center_state();
    let saved_center = notification_history.get_notification_center();
    let saved_revision = notification_history.get_revision();
    notification_history.close_notification_center(true);

    notification_history.set_notification_center_state(saved_center_state);
    let restored_revision = notification_history.get_revision();
    notification_history.set_notification_center_state(saved_center_state);

    assert_eq!(notification_history.get_notification_center(), saved_center);
    assert_eq!(
        notification_history.get_notification_center_state(),
        saved_center_state
    );
    assert_eq!(restored_revision, saved_revision + 2);
    assert_eq!(notification_history.get_revision(), restored_revision);
}
