//! Tests for the deterministic event-sequence recorder.

use super::*;
use koshi_core::event::{TabClosed, TabCreated, TabFocused};
use koshi_core::ids::{ClientId, TabId};
use std::panic::catch_unwind;

fn created() -> Event {
    Event::TabCreated(TabCreated {
        tab_id: TabId::new(),
    })
}

fn focused() -> Event {
    Event::TabFocused(TabFocused {
        client_id: ClientId::new(),
        tab_id: TabId::new(),
        prior_tab: TabId::new(),
    })
}

fn closed() -> Event {
    Event::TabClosed(TabClosed {
        tab_id: TabId::new(),
    })
}

/// Extract the string panic message from a caught panic.
fn message(result: std::thread::Result<()>) -> String {
    let payload = result.expect_err("expected a panic");
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .expect("panic payload should be a string")
}

#[test]
fn push_and_take_preserve_order() {
    let a = created();
    let b = focused();
    let mut rec = RecordedEvents::new();
    rec.push(a.clone());
    rec.push(b.clone());
    assert_eq!(rec.take(), vec![a, b]);
    assert!(rec.is_empty());
}

#[test]
fn drain_from_pulls_until_none() {
    let events = vec![created(), focused(), closed()];
    let mut iter = events.clone().into_iter();
    let mut rec = RecordedEvents::new();
    rec.drain_from(|| iter.next());
    assert_eq!(rec.len(), 3);
    assert_eq!(rec.take(), events);
}

#[test]
fn assert_exact_matches_full_sequence() {
    let a = created();
    let b = focused();
    let mut rec = RecordedEvents::new();
    rec.push(a.clone());
    rec.push(b.clone());
    rec.assert_exact(&[a, b]);
    rec.assert_no_more();
}

#[test]
fn assert_prefix_consumes_only_the_prefix() {
    let a = created();
    let b = focused();
    let c = closed();
    let mut rec = RecordedEvents::new();
    rec.push(a.clone());
    rec.push(b.clone());
    rec.push(c.clone());
    rec.assert_prefix(&[a, b]);
    rec.assert_prefix(&[c]);
    rec.assert_no_more();
}

#[test]
fn assert_no_more_fails_with_trailing_events() {
    let a = created();
    let mut rec = RecordedEvents::new();
    rec.push(a);
    let err = catch_unwind(std::panic::AssertUnwindSafe(|| rec.assert_no_more()));
    let msg = message(err);
    assert!(msg.contains("expected no more events"), "{msg}");
    assert!(msg.contains("EXTRA"), "{msg}");
}

#[test]
fn mismatch_diff_points_at_the_divergent_index() {
    let a = created();
    let b = focused();
    let wrong = closed();
    let mut rec = RecordedEvents::new();
    rec.push(a.clone());
    rec.push(b.clone());
    let err = catch_unwind(std::panic::AssertUnwindSafe(|| {
        rec.assert_exact(&[a, wrong]);
    }));
    let msg = message(err);
    assert!(msg.contains("event sequence mismatch"), "{msg}");
    assert!(msg.contains("[0] ok"), "{msg}");
    assert!(msg.contains("[1] MISMATCH"), "{msg}");
}

#[test]
fn length_mismatch_reports_missing_event() {
    let a = created();
    let b = focused();
    let mut rec = RecordedEvents::new();
    rec.push(a.clone());
    let err = catch_unwind(std::panic::AssertUnwindSafe(|| {
        rec.assert_exact(&[a, b]);
    }));
    let msg = message(err);
    assert!(msg.contains("[1] MISSING"), "{msg}");
    assert!(msg.contains("length: expected 2, actual 1"), "{msg}");
}

#[test]
fn prefix_longer_than_recorded_fails() {
    let a = created();
    let b = focused();
    let mut rec = RecordedEvents::new();
    rec.push(a.clone());
    let err = catch_unwind(std::panic::AssertUnwindSafe(|| {
        rec.assert_prefix(&[a, b]);
    }));
    let msg = message(err);
    assert!(msg.contains("event prefix mismatch"), "{msg}");
    assert!(msg.contains("[1] MISSING"), "{msg}");
}

#[test]
fn assert_prefix_with_empty_expected_leaves_events_untouched() {
    let a = created();
    let b = focused();
    let mut rec = RecordedEvents::new();
    rec.push(a.clone());
    rec.push(b.clone());
    rec.assert_prefix(&[]);
    assert_eq!(rec.len(), 2);
    rec.assert_exact(&[a, b]);
}

#[test]
fn assert_prefix_content_mismatch_reports_mismatch_not_missing() {
    let a = created();
    let b = focused();
    let wrong = closed();
    let mut rec = RecordedEvents::new();
    rec.push(a.clone());
    rec.push(b);
    let err = catch_unwind(std::panic::AssertUnwindSafe(|| {
        rec.assert_prefix(&[a, wrong]);
    }));
    let msg = message(err);
    assert!(msg.contains("event prefix mismatch"), "{msg}");
    assert!(msg.contains("[0] ok"), "{msg}");
    assert!(msg.contains("[1] MISMATCH"), "{msg}");
    assert!(!msg.contains("MISSING"), "{msg}");
}

#[test]
fn assert_exact_with_fewer_expected_than_actual_reports_extra() {
    let a = created();
    let b = focused();
    let mut rec = RecordedEvents::new();
    rec.push(a.clone());
    rec.push(b);
    let err = catch_unwind(std::panic::AssertUnwindSafe(|| {
        rec.assert_exact(&[a]);
    }));
    let msg = message(err);
    assert!(msg.contains("event sequence mismatch"), "{msg}");
    assert!(msg.contains("[1] EXTRA"), "{msg}");
    assert!(msg.contains("length: expected 1, actual 2"), "{msg}");
}

#[test]
fn failed_assert_prefix_does_not_consume_events() {
    let a = created();
    let b = focused();
    let wrong = closed();
    let mut rec = RecordedEvents::new();
    rec.push(a.clone());
    rec.push(b.clone());
    let _ = catch_unwind(std::panic::AssertUnwindSafe(|| {
        rec.assert_prefix(&[wrong]);
    }));
    // The failed assertion must not have drained anything: the original
    // sequence is still there for a subsequent correct assertion.
    rec.assert_exact(&[a, b]);
}

#[test]
fn failed_assert_exact_does_not_clear_recorder() {
    let a = created();
    let wrong = closed();
    let mut rec = RecordedEvents::new();
    rec.push(a.clone());
    let _ = catch_unwind(std::panic::AssertUnwindSafe(|| {
        rec.assert_exact(&[wrong]);
    }));
    assert_eq!(rec.len(), 1);
    rec.assert_exact(&[a]);
}

#[test]
fn assert_exact_empty_expected_on_empty_recorder_succeeds() {
    let mut rec = RecordedEvents::new();
    rec.assert_exact(&[]);
    rec.assert_no_more();
}

#[test]
fn format_diff_renders_ok_mismatch_and_missing_rows_exactly() {
    let a = created();
    let b = focused();
    let c = closed();
    let wrong = closed();
    let rendered = format_diff(
        &[a.clone(), b.clone(), c.clone()],
        &[a.clone(), wrong.clone()],
    );
    assert_eq!(
        rendered,
        format!(
            "  [0] ok       {a:?}\n\
             \x20 [1] MISMATCH expected {b:?}\n\
             \x20              actual   {wrong:?}\n\
             \x20 [2] MISSING  expected {c:?}\n\
             \x20 length: expected 3, actual 2"
        )
    );
}

#[test]
fn format_diff_renders_extra_rows_exactly() {
    let a = created();
    let b = focused();
    let rendered = format_diff(std::slice::from_ref(&a), &[a.clone(), b.clone()]);
    assert_eq!(
        rendered,
        format!(
            "  [0] ok       {a:?}\n\
             \x20 [1] EXTRA    actual   {b:?}\n\
             \x20 length: expected 1, actual 2"
        )
    );
}

#[test]
fn format_diff_of_two_empty_sequences_is_only_the_length_line() {
    assert_eq!(format_diff(&[], &[]), "  length: expected 0, actual 0");
}

#[test]
fn assert_no_more_panic_message_lists_every_trailing_event() {
    let a = created();
    let b = focused();
    let mut rec = RecordedEvents::new();
    rec.push(a.clone());
    rec.push(b.clone());
    let err = catch_unwind(std::panic::AssertUnwindSafe(|| rec.assert_no_more()));
    assert_eq!(
        message(err),
        format!(
            "expected no more events, but 2 remain:\n\
             \x20 [0] EXTRA    actual   {a:?}\n\
             \x20 [1] EXTRA    actual   {b:?}\n\
             \x20 length: expected 0, actual 2"
        )
    );
}

#[test]
fn assert_no_more_on_an_empty_recorder_succeeds() {
    RecordedEvents::new().assert_no_more();
}

#[test]
fn drain_from_a_puller_that_is_empty_at_once_records_nothing() {
    let mut rec = RecordedEvents::new();
    rec.drain_from(|| None);
    assert_eq!(rec.len(), 0);
    assert!(rec.is_empty());
    assert_eq!(rec.take(), Vec::<Event>::new());
}

#[test]
fn drain_from_appends_after_events_already_recorded() {
    let a = created();
    let b = focused();
    let mut rec = RecordedEvents::new();
    rec.push(a.clone());
    let mut pulled = vec![b.clone()].into_iter();
    rec.drain_from(|| pulled.next());
    assert_eq!(rec.take(), vec![a, b]);
}

#[test]
fn take_leaves_the_recorder_reusable() {
    let a = created();
    let b = focused();
    let mut rec = RecordedEvents::new();
    rec.push(a.clone());
    assert_eq!(rec.take(), vec![a]);
    rec.push(b.clone());
    assert_eq!(rec.len(), 1);
    rec.assert_exact(&[b]);
}

#[test]
fn assert_prefix_of_the_whole_sequence_leaves_nothing() {
    let a = created();
    let b = focused();
    let mut rec = RecordedEvents::new();
    rec.push(a.clone());
    rec.push(b.clone());
    rec.assert_prefix(&[a, b]);
    assert_eq!(rec.len(), 0);
    rec.assert_no_more();
}

#[test]
fn assert_exact_against_a_reordered_sequence_fails_at_the_first_swapped_index() {
    let a = created();
    let b = focused();
    let mut rec = RecordedEvents::new();
    rec.push(a.clone());
    rec.push(b.clone());
    let err = catch_unwind(std::panic::AssertUnwindSafe(|| {
        rec.assert_exact(&[b.clone(), a.clone()]);
    }));
    assert_eq!(
        message(err),
        format!(
            "event sequence mismatch:\n\
             \x20 [0] MISMATCH expected {b:?}\n\
             \x20              actual   {a:?}\n\
             \x20 [1] MISMATCH expected {a:?}\n\
             \x20              actual   {b:?}\n\
             \x20 length: expected 2, actual 2"
        )
    );
}

#[test]
fn a_cloned_recorder_is_independent_of_the_original() {
    let a = created();
    let mut rec = RecordedEvents::new();
    rec.push(a.clone());
    let mut copy = rec.clone();
    copy.assert_exact(std::slice::from_ref(&a));
    assert_eq!(rec.len(), 1);
    rec.assert_exact(&[a]);
}

#[test]
fn a_prefix_mismatch_diff_leaves_out_the_events_past_the_prefix() {
    // Trailing events are what `assert_prefix` leaves for the next assertion,
    // so naming them EXTRA points the reader at a count problem it never had.
    let mut recorder = RecordedEvents::new();
    recorder.push(created());
    recorder.push(focused());
    recorder.push(closed());

    let err = catch_unwind(std::panic::AssertUnwindSafe(|| {
        recorder.assert_prefix(&[created(), closed()]);
    }));

    let msg = message(err);
    assert!(!msg.contains("EXTRA"), "{msg}");
    assert!(msg.contains("length: expected 2, actual 2"), "{msg}");
}
