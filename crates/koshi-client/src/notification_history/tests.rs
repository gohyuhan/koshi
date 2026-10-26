//! Tests for `NotificationHistory`: the viewer's own posts, the session's
//! queue and posts, stale and foreign input, the history bound, the unread
//! and dropped counts, clearing, the center's state and the toast previews
//! arrivals queue.

use std::time::{Duration, Instant, SystemTime};

use koshi_core::notification::{NotificationSource, MAX_QUEUED_NOTIFICATION_COUNT};

use super::*;

/// `seconds` after the Unix epoch.
fn at_seconds(seconds: u64) -> SystemTime {
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
        .post_core_entry(NotificationSeverity::Error, text, at_seconds(1))
        .clone();
    (session_queue.get_revision(), posted_entry)
}

/// The texts of `notification_history`, oldest arrival first, each with its
/// repeat count.
fn list_texts(notification_history: &NotificationHistory) -> Vec<(String, u32)> {
    notification_history
        .list_merged_entries()
        .map(|entry| (entry.text.clone(), entry.repeat_count))
        .collect()
}

#[test]
fn an_own_post_is_held_and_a_repeat_merges_into_it() {
    let client_id = ClientId::new();
    let mut notification_history = NotificationHistory::from_client_id(client_id);

    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        "core:resize-pane-left: below minimum size",
        at_seconds(1),
        true,
    );
    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        "core:resize-pane-left: below minimum size",
        at_seconds(2),
        true,
    );

    let merged_entries: Vec<&NotificationEntry> =
        notification_history.list_merged_entries().collect();
    assert_eq!(merged_entries.len(), 1);
    assert_eq!(merged_entries[0].repeat_count, 2);
    assert_eq!(merged_entries[0].created_at, at_seconds(2));
    assert_eq!(merged_entries[0].source, NotificationSource::Core);
    assert_eq!(
        merged_entries[0].id.owner,
        NotificationQueueOwner::Client(client_id)
    );
    assert_eq!(merged_entries[0].id.sequence, 0);
}

#[test]
fn an_own_repeat_moves_to_the_newest_place() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    notification_history.post_core_entry(NotificationSeverity::Warning, "a", at_seconds(1), true);
    post_to_session(&mut session_queue, "pane …a1b2 exited 101");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue");

    notification_history.post_core_entry(NotificationSeverity::Warning, "a", at_seconds(2), true);

    assert_eq!(
        list_texts(&notification_history),
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
        list_texts(&notification_history),
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
        list_texts(&notification_history),
        vec![("first".to_string(), 1)]
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
        list_texts(&notification_history),
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
    notification_history.post_core_entry(NotificationSeverity::Info, "own", at_seconds(1), true);

    let (revision, merged_entry) = post_to_session(&mut session_queue, "a");
    notification_history
        .take_session_post(revision, merged_entry, true)
        .expect("take the post");

    assert_eq!(
        list_texts(&notification_history),
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
        list_texts(&notification_history),
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
        list_texts(&notification_history),
        vec![("stale".to_string(), 1), ("kept".to_string(), 1)]
    );
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
        list_texts(&notification_history),
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
    notification_history.post_core_entry(NotificationSeverity::Error, "same", at_seconds(1), true);
    post_to_session(&mut session_queue, "same");

    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue");

    let sequences: Vec<u64> = notification_history
        .list_merged_entries()
        .map(|entry| entry.id.sequence)
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
    forged_entry.id.owner = NotificationQueueOwner::Client(ClientId::new());
    forged_queue_json["entries"][0] = serde_json::to_value(&forged_entry).expect("encode");
    let forged_queue: NotificationQueue =
        serde_json::from_value(forged_queue_json).expect("decode");

    let take_result = notification_history.take_session_queue(session_id, &forged_queue, true);

    assert_eq!(
        take_result,
        Err(NotificationTakeError::InvalidQueue(
            NotificationQueueError::ForeignEntry {
                notification_id: forged_entry.id
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
    let posted_id = posted_entry.id;

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
    let posted_id = posted_entry.id;

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

    for entry_index in 0..=MAX_MERGED_NOTIFICATION_COUNT {
        notification_history.post_core_entry(
            NotificationSeverity::Info,
            &format!("entry {entry_index}"),
            at_seconds(1),
            true,
        );
    }

    let texts = list_texts(&notification_history);
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
    for entry_index in 0..(MAX_MERGED_NOTIFICATION_COUNT - 1) {
        notification_history.post_core_entry(
            NotificationSeverity::Info,
            &format!("own {entry_index}"),
            at_seconds(1),
            true,
        );
    }

    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the resync queue");

    let texts = list_texts(&notification_history);
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
    for entry_index in 0..MAX_MERGED_NOTIFICATION_COUNT {
        notification_history.post_core_entry(
            NotificationSeverity::Info,
            &format!("own {entry_index}"),
            at_seconds(1),
            true,
        );
    }
    post_to_session(&mut session_queue, "repeated");

    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the resync queue");

    assert_eq!(
        list_texts(&notification_history).last(),
        Some(&("repeated".to_string(), 2))
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
    notification_history.post_core_entry(NotificationSeverity::Warning, "a", at_seconds(1), true);

    notification_history.set_client_id(second_client_id);
    notification_history.post_core_entry(NotificationSeverity::Warning, "a", at_seconds(2), true);

    let held_ids: Vec<(NotificationQueueOwner, u64)> = notification_history
        .list_merged_entries()
        .map(|entry| (entry.id.owner, entry.id.sequence))
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
    notification_history.post_core_entry(NotificationSeverity::Warning, "a", at_seconds(1), true);

    notification_history.set_client_id(client_id);
    notification_history.post_core_entry(NotificationSeverity::Warning, "a", at_seconds(2), true);

    assert_eq!(
        list_texts(&notification_history),
        vec![(String::from("a"), 2)]
    );
}

/// The config warning both a viewer and its session post when they read the
/// same `koshi.kdl`.
const SHARED_CONFIG_WARNING: &str = "/home/user/.config/koshi/koshi.kdl: unknown key frobnicate";

/// The owner, severity and text of each entry `notification_history` holds,
/// oldest arrival first.
fn list_notices(
    notification_history: &NotificationHistory,
) -> Vec<(NotificationQueueOwner, NotificationSeverity, String)> {
    notification_history
        .list_merged_entries()
        .map(|entry| (entry.id.owner, entry.severity, entry.text.clone()))
        .collect()
}

#[test]
fn a_local_session_queue_holding_the_viewers_warning_replaces_the_viewers_entry() {
    let client_id = ClientId::new();
    let mut notification_history = NotificationHistory::from_client_id(client_id);
    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        SHARED_CONFIG_WARNING,
        at_seconds(1),
        true,
    );
    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        "/home/user/.config/koshi/keybinding.kdl: unknown action core:frobnicate",
        at_seconds(1),
        true,
    );
    let (session_id, mut session_queue) = create_session_queue();
    session_queue.post_core_entry(
        NotificationSeverity::Warning,
        SHARED_CONFIG_WARNING,
        at_seconds(2),
    );

    notification_history.set_session_local(true);
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("the queue is taken");

    assert_eq!(
        list_notices(&notification_history),
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
        at_seconds(1),
    );
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("the queue is taken");

    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        SHARED_CONFIG_WARNING,
        at_seconds(2),
        true,
    );
    notification_history.post_core_entry(
        NotificationSeverity::Error,
        SHARED_CONFIG_WARNING,
        at_seconds(2),
        true,
    );

    assert_eq!(
        list_notices(&notification_history),
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
        at_seconds(1),
        true,
    );
    let posted_entry = session_queue
        .post_core_entry(
            NotificationSeverity::Warning,
            SHARED_CONFIG_WARNING,
            at_seconds(2),
        )
        .clone();

    notification_history
        .take_session_post(session_queue.get_revision(), posted_entry, true)
        .expect("the post is taken");

    assert_eq!(
        list_notices(&notification_history),
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
        at_seconds(1),
        true,
    );
    let (session_id, mut session_queue) = create_session_queue();
    session_queue.post_core_entry(
        NotificationSeverity::Warning,
        SHARED_CONFIG_WARNING,
        at_seconds(2),
    );

    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("the queue is taken");
    notification_history.post_core_entry(
        NotificationSeverity::Warning,
        SHARED_CONFIG_WARNING,
        at_seconds(3),
        true,
    );

    assert_eq!(
        list_notices(&notification_history),
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
fn find_newest_entry(notification_history: &NotificationHistory) -> NotificationEntry {
    notification_history
        .list_merged_entries()
        .last()
        .expect("the history holds an entry")
        .clone()
}

#[test]
fn entries_that_arrive_while_the_center_is_closed_count_as_unread() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    notification_history.post_core_entry(NotificationSeverity::Info, "a", at_seconds(1), true);
    notification_history.post_core_entry(NotificationSeverity::Info, "b", at_seconds(1), true);
    let unread_before_the_center = notification_history.get_unread_entry_count();

    notification_history.open_notification_center();
    let opened_center = notification_history.get_notification_center();
    notification_history.post_core_entry(NotificationSeverity::Info, "c", at_seconds(2), true);
    let unread_while_open = notification_history.get_unread_entry_count();
    notification_history.close_notification_center();
    notification_history.post_core_entry(NotificationSeverity::Info, "d", at_seconds(3), true);

    assert_eq!(unread_before_the_center, 2);
    assert_eq!(
        opened_center,
        Some(NotificationCenter {
            scroll_row_offset: 0,
            is_focused: true,
        })
    );
    assert_eq!(unread_while_open, 0);
    assert_eq!(notification_history.get_unread_entry_count(), 1);
    assert_eq!(notification_history.get_notification_center(), None);
}

#[test]
fn a_repeat_of_a_read_entry_counts_as_unread_again() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    notification_history.post_core_entry(NotificationSeverity::Info, "a", at_seconds(1), true);
    notification_history.post_core_entry(NotificationSeverity::Info, "b", at_seconds(1), true);
    notification_history.open_notification_center();
    notification_history.close_notification_center();

    notification_history.post_core_entry(NotificationSeverity::Info, "b", at_seconds(2), true);

    assert_eq!(notification_history.get_unread_entry_count(), 1);
    assert_eq!(
        list_texts(&notification_history),
        vec![("a".to_string(), 1), ("b".to_string(), 2)]
    );
}

#[test]
fn an_unread_entry_the_history_evicts_leaves_the_unread_count() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());

    for entry_index in 0..=MAX_MERGED_NOTIFICATION_COUNT {
        notification_history.post_core_entry(
            NotificationSeverity::Info,
            &format!("entry {entry_index}"),
            at_seconds(1),
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
    for entry_index in 0..(MAX_QUEUED_NOTIFICATION_COUNT + 40) {
        post_to_session(&mut session_queue, &format!("entry {entry_index}"));
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
            .map(|entry| entry.id.sequence),
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
        list_texts(&notification_history),
        vec![("received".to_string(), 1)]
    );
}

#[test]
fn clearing_removes_every_entry_and_count_and_closes_the_center() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let (session_id, mut session_queue) = create_session_queue();
    for entry_index in 0..(MAX_QUEUED_NOTIFICATION_COUNT + 1) {
        post_to_session(&mut session_queue, &format!("entry {entry_index}"));
    }
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the queue");
    notification_history.post_core_entry(NotificationSeverity::Info, "own", at_seconds(1), true);
    notification_history.open_notification_center();

    notification_history.clear_entries();

    let notification_view = notification_history.build_notification_view();
    assert_eq!(notification_history.list_merged_entries().next(), None);
    assert_eq!(notification_view.dropped_entry_count, 0);
    assert_eq!(notification_view.toast_content, None);
    assert_eq!(notification_history.get_unread_entry_count(), 0);
    assert_eq!(notification_history.get_notification_center(), None);
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

    notification_history.clear_entries();
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
        list_texts(&notification_history),
        vec![("pane …e5f6 exited 2".to_string(), 1)]
    );
}

#[test]
fn an_entry_accepted_after_a_clear_stays_whatever_its_time() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    notification_history.post_core_entry(NotificationSeverity::Info, "before", at_seconds(5), true);

    notification_history.clear_entries();
    notification_history.post_core_entry(NotificationSeverity::Info, "after", at_seconds(5), true);
    notification_history.post_core_entry(NotificationSeverity::Info, "after", at_seconds(5), true);

    assert_eq!(
        list_texts(&notification_history),
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
    notification_history.clear_entries();
    let mut restarted_queue =
        NotificationQueue::from_owner(NotificationQueueOwner::Session(session_id));
    post_to_session(&mut restarted_queue, "second epoch 0");
    post_to_session(&mut restarted_queue, "second epoch 1");

    notification_history
        .take_session_queue(session_id, &restarted_queue, true)
        .expect("take the new epoch");
    let (revision, old_epoch_entry) = post_to_session(&mut session_queue, "first epoch late");
    let old_epoch_entry_id = old_epoch_entry.id;
    let old_epoch_post_result =
        notification_history.take_session_post(revision, old_epoch_entry, true);

    assert_eq!(
        list_texts(&notification_history),
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
    assert_eq!(notification_history.find_shown_toast(), None);

    post_to_session(&mut session_queue, "after the attach");
    notification_history
        .take_session_queue(session_id, &session_queue, true)
        .expect("take the resync queue");

    let newest_entry = find_newest_entry(&notification_history);
    assert_eq!(
        notification_history.find_shown_toast(),
        Some((NotificationToastContent::Entry(&newest_entry), 1000))
    );
    assert_eq!(newest_entry.text, "after the attach");
}

#[test]
fn an_arrival_previews_nothing_while_toasts_are_off() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());

    notification_history.post_core_entry(NotificationSeverity::Error, "a", at_seconds(1), false);

    assert_eq!(notification_history.find_shown_toast(), None);
    assert_eq!(notification_history.get_unread_entry_count(), 1);
}

#[test]
fn opening_the_center_drops_every_preview_and_arrivals_while_it_is_open_preview_nothing() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    notification_history.post_core_entry(NotificationSeverity::Info, "a", at_seconds(1), true);
    notification_history.post_core_entry(NotificationSeverity::Info, "b", at_seconds(1), true);

    notification_history.open_notification_center();
    assert_eq!(notification_history.find_shown_toast(), None);
    notification_history.post_core_entry(NotificationSeverity::Info, "c", at_seconds(2), true);
    notification_history.close_notification_center();
    assert_eq!(notification_history.find_shown_toast(), None);
    notification_history.post_core_entry(NotificationSeverity::Info, "d", at_seconds(3), true);

    let newest_entry = find_newest_entry(&notification_history);
    assert_eq!(
        notification_history.find_shown_toast(),
        Some((NotificationToastContent::Entry(&newest_entry), 1000))
    );
    assert_eq!(notification_history.list_merged_entries().count(), 4);
}

#[test]
fn forty_posts_end_in_a_summary_of_thirty_six_at_the_loudest_severity() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    for entry_index in 0..40 {
        let severity = if entry_index == 20 {
            NotificationSeverity::Error
        } else {
            NotificationSeverity::Warning
        };
        notification_history.post_core_entry(
            severity,
            &format!("pane {entry_index} exited 1"),
            at_seconds(1),
            true,
        );
    }
    let start = Instant::now();

    for millisecond_count in [0, 1500, 3000, 4500, 6000] {
        notification_history.advance_notification_toast(
            start + Duration::from_millis(millisecond_count),
            DEFAULT_TOAST_TIMING,
            false,
        );
    }

    assert_eq!(
        notification_history.find_shown_toast(),
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
    for entry_index in 0..=MAX_MERGED_NOTIFICATION_COUNT {
        notification_history.post_core_entry(
            NotificationSeverity::Info,
            &format!("entry {entry_index}"),
            at_seconds(1),
            true,
        );
    }

    notification_history.advance_notification_toast(Instant::now(), DEFAULT_TOAST_TIMING, false);

    let second_entry = notification_history
        .list_merged_entries()
        .next()
        .expect("the history holds an entry")
        .clone();
    assert_eq!(second_entry.text, "entry 1");
    assert_eq!(
        notification_history.find_shown_toast(),
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
        at_seconds(1),
        true,
    );
    let session_entry = session_queue
        .post_core_entry(
            NotificationSeverity::Warning,
            SHARED_CONFIG_WARNING,
            at_seconds(2),
        )
        .clone();

    notification_history
        .take_session_post(session_queue.get_revision(), session_entry.clone(), true)
        .expect("take the post");

    assert_eq!(
        notification_history.find_shown_toast(),
        Some((NotificationToastContent::Entry(&session_entry), 1000))
    );
}

#[test]
fn each_change_to_the_center_counts_one_revision_and_no_change_counts_none() {
    let mut notification_history = NotificationHistory::from_client_id(ClientId::new());
    let mut revisions = vec![notification_history.get_revision()];

    notification_history.open_notification_center();
    revisions.push(notification_history.get_revision());
    notification_history.open_notification_center();
    revisions.push(notification_history.get_revision());
    notification_history.set_notification_center_focus(false);
    revisions.push(notification_history.get_revision());
    notification_history.set_notification_center_focus(false);
    revisions.push(notification_history.get_revision());
    notification_history.set_notification_center_scroll_row_offset(3);
    revisions.push(notification_history.get_revision());
    notification_history.set_notification_center_scroll_row_offset(3);
    revisions.push(notification_history.get_revision());
    notification_history.close_notification_center();
    revisions.push(notification_history.get_revision());
    notification_history.close_notification_center();
    revisions.push(notification_history.get_revision());

    assert_eq!(revisions, vec![0, 1, 1, 2, 2, 3, 3, 4, 4]);
}
