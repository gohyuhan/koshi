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
