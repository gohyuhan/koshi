//! Tests for `NotificationQueue`: ids, repeats, eviction, text bounds and the
//! check of a queue another process sent.

use std::time::{Duration, SystemTime};

use super::*;

/// A session queue for a new session id.
fn create_session_queue() -> NotificationQueue {
    NotificationQueue::from_owner(NotificationQueueOwner::Session(SessionId::new()))
}

/// `seconds` after the Unix epoch.
fn at_seconds(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

#[test]
fn a_new_queue_is_empty_at_revision_zero_in_its_own_epoch() {
    let owner = NotificationQueueOwner::Client(ClientId::new());
    let first_queue = NotificationQueue::from_owner(owner);
    let second_queue = NotificationQueue::from_owner(owner);

    assert_eq!(first_queue.get_owner(), owner);
    assert_eq!(first_queue.get_revision(), 0);
    assert_eq!(first_queue.list_entries().count(), 0);
    assert_ne!(first_queue.get_epoch(), second_queue.get_epoch());
}

#[test]
fn a_post_takes_the_next_sequence_under_the_queue_owner_and_epoch() {
    let mut notification_queue = create_session_queue();

    let first_entry = notification_queue
        .post_core_entry(
            NotificationSeverity::Error,
            "pane …a1b2 killed by signal 9",
            at_seconds(10),
        )
        .clone();
    let second_entry = notification_queue
        .post_core_entry(
            NotificationSeverity::Info,
            "pane …c3d4 exited 101",
            at_seconds(11),
        )
        .clone();

    assert_eq!(
        first_entry,
        NotificationEntry {
            id: NotificationId {
                owner: notification_queue.get_owner(),
                epoch: notification_queue.get_epoch(),
                sequence: 0,
            },
            source: NotificationSource::Core,
            severity: NotificationSeverity::Error,
            text: "pane …a1b2 killed by signal 9".to_string(),
            created_at: at_seconds(10),
            repeat_count: 1,
        }
    );
    assert_eq!(second_entry.id.sequence, 1);
    assert_eq!(notification_queue.get_revision(), 2);
    assert_eq!(
        notification_queue
            .list_entries()
            .cloned()
            .collect::<Vec<_>>(),
        vec![first_entry, second_entry]
    );
}

#[test]
fn a_post_equal_to_the_newest_entry_merges_into_it() {
    let mut notification_queue = create_session_queue();
    notification_queue.post_core_entry(
        NotificationSeverity::Warning,
        "below minimum size",
        at_seconds(1),
    );

    let merged_entry = notification_queue
        .post_core_entry(
            NotificationSeverity::Warning,
            "below minimum size",
            at_seconds(2),
        )
        .clone();

    assert_eq!(merged_entry.id.sequence, 0);
    assert_eq!(merged_entry.repeat_count, 2);
    assert_eq!(merged_entry.created_at, at_seconds(2));
    assert_eq!(notification_queue.get_revision(), 2);
    assert_eq!(notification_queue.list_entries().count(), 1);
}

#[test]
fn an_entry_between_two_equal_posts_keeps_all_three() {
    let mut notification_queue = create_session_queue();

    for text in ["a", "b", "a"] {
        notification_queue.post_core_entry(NotificationSeverity::Info, text, at_seconds(1));
    }

    let entries: Vec<(u64, &str, u32)> = notification_queue
        .list_entries()
        .map(|entry| (entry.id.sequence, entry.text.as_str(), entry.repeat_count))
        .collect();
    assert_eq!(entries, vec![(0, "a", 1), (1, "b", 1), (2, "a", 1)]);
}

#[test]
fn the_same_text_at_another_severity_is_a_new_entry() {
    let mut notification_queue = create_session_queue();

    notification_queue.post_core_entry(NotificationSeverity::Info, "a", at_seconds(1));
    notification_queue.post_core_entry(NotificationSeverity::Error, "a", at_seconds(1));

    assert_eq!(notification_queue.list_entries().count(), 2);
}

#[test]
fn a_full_queue_evicts_its_oldest_entry_and_keeps_its_sequences_without_a_gap() {
    let mut notification_queue = create_session_queue();

    for entry_index in 0..=MAX_QUEUED_NOTIFICATION_COUNT {
        notification_queue.post_core_entry(
            NotificationSeverity::Info,
            &format!("entry {entry_index}"),
            at_seconds(1),
        );
    }

    let sequences: Vec<u64> = notification_queue
        .list_entries()
        .map(|entry| entry.id.sequence)
        .collect();
    assert_eq!(sequences, (1..=256).collect::<Vec<u64>>());
    assert_eq!(notification_queue.get_revision(), 257);
}

#[test]
fn a_post_removes_refused_characters_and_cuts_the_text_to_its_bound() {
    let mut notification_queue = create_session_queue();

    let escaped_entry = notification_queue
        .post_core_entry(
            NotificationSeverity::Warning,
            "file\u{1b}[2J.kdl",
            at_seconds(1),
        )
        .clone();
    let long_entry = notification_queue
        .post_core_entry(
            NotificationSeverity::Warning,
            &"a".repeat(5000),
            at_seconds(1),
        )
        .clone();

    assert_eq!(escaped_entry.text, "file[2J.kdl");
    assert_eq!(
        long_entry.text,
        "a".repeat(MAX_NOTIFICATION_TEXT_BYTE_COUNT)
    );
}

#[test]
fn a_repeat_is_compared_after_the_text_is_sanitized() {
    let mut notification_queue = create_session_queue();

    notification_queue.post_core_entry(NotificationSeverity::Info, "a\u{7f}", at_seconds(1));
    let merged_entry =
        notification_queue.post_core_entry(NotificationSeverity::Info, "a", at_seconds(2));

    assert_eq!(merged_entry.repeat_count, 2);
}

#[test]
fn a_repeat_count_saturates() {
    let mut notification_queue = create_session_queue();
    notification_queue.post_core_entry(NotificationSeverity::Info, "a", at_seconds(1));
    notification_queue.entries[0].repeat_count = u32::MAX;

    let merged_entry =
        notification_queue.post_core_entry(NotificationSeverity::Info, "a", at_seconds(2));

    assert_eq!(merged_entry.repeat_count, u32::MAX);
}

#[test]
fn a_queue_survives_a_serde_round_trip() {
    let mut notification_queue = create_session_queue();
    notification_queue.post_core_entry(NotificationSeverity::Error, "a", at_seconds(1));

    let encoded_queue = serde_json::to_string(&notification_queue).expect("encode");
    let decoded_queue: NotificationQueue = serde_json::from_str(&encoded_queue).expect("decode");

    assert_eq!(decoded_queue, notification_queue);
}

#[test]
fn validating_a_queue_accepts_its_own_entries() {
    let mut notification_queue = create_session_queue();
    notification_queue.post_core_entry(NotificationSeverity::Error, "a", at_seconds(1));

    assert_eq!(notification_queue.validate_entries(), Ok(()));
}

#[test]
fn validating_a_queue_refuses_an_entry_of_another_queue() {
    let mut notification_queue = create_session_queue();
    let client_owner = NotificationQueueOwner::Client(ClientId::new());
    notification_queue.post_core_entry(NotificationSeverity::Error, "a", at_seconds(1));
    notification_queue.entries[0].id.owner = client_owner;
    let foreign_id = notification_queue.entries[0].id;

    assert_eq!(
        notification_queue.validate_entries(),
        Err(NotificationQueueError::ForeignEntry {
            notification_id: foreign_id
        })
    );

    notification_queue.entries[0].id.owner = notification_queue.get_owner();
    notification_queue.entries[0].id.epoch = NotificationEpoch::new();
    let other_epoch_id = notification_queue.entries[0].id;
    assert_eq!(
        notification_queue.validate_entries(),
        Err(NotificationQueueError::ForeignEntry {
            notification_id: other_epoch_id
        })
    );
}

#[test]
fn validating_a_queue_refuses_more_entries_than_the_bound() {
    let mut notification_queue = create_session_queue();
    notification_queue.post_core_entry(NotificationSeverity::Info, "a", at_seconds(1));
    let template_entry = notification_queue.entries[0].clone();
    notification_queue.entries = std::iter::repeat_n(template_entry, 257).collect();

    assert_eq!(
        notification_queue.validate_entries(),
        Err(NotificationQueueError::TooManyEntries { entry_count: 257 })
    );
}

#[test]
fn severities_order_from_the_quietest_to_the_loudest() {
    let mut severities = [
        NotificationSeverity::Error,
        NotificationSeverity::Info,
        NotificationSeverity::Warning,
    ];
    severities.sort();

    assert_eq!(
        severities,
        [
            NotificationSeverity::Info,
            NotificationSeverity::Warning,
            NotificationSeverity::Error,
        ]
    );
}
