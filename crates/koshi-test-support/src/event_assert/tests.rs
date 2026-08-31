//! Tests for the event-sequence assertion.

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
fn an_identical_sequence_passes() {
    let a = created();
    let b = focused();
    assert_events(&[a.clone(), b.clone()], &[a, b]);
}

#[test]
fn two_empty_sequences_pass() {
    assert_events(&[], &[]);
}

#[test]
fn mismatch_diff_points_at_the_divergent_index() {
    let a = created();
    let b = focused();
    let wrong = closed();
    let err = catch_unwind(|| assert_events(&[a.clone(), b], &[a, wrong]));
    let msg = message(err);
    assert!(msg.contains("event sequence mismatch"), "{msg}");
    assert!(msg.contains("[0] ok"), "{msg}");
    assert!(msg.contains("[1] MISMATCH"), "{msg}");
}

#[test]
fn a_short_actual_reports_the_missing_event() {
    let a = created();
    let b = focused();
    let err = catch_unwind(|| assert_events(std::slice::from_ref(&a), &[a.clone(), b]));
    let msg = message(err);
    assert!(msg.contains("[1] MISSING"), "{msg}");
    assert!(msg.contains("length: expected 2, actual 1"), "{msg}");
}

#[test]
fn a_long_actual_reports_the_extra_event() {
    let a = created();
    let b = focused();
    let err = catch_unwind(|| assert_events(&[a.clone(), b], std::slice::from_ref(&a)));
    let msg = message(err);
    assert!(msg.contains("event sequence mismatch"), "{msg}");
    assert!(msg.contains("[1] EXTRA"), "{msg}");
    assert!(msg.contains("length: expected 1, actual 2"), "{msg}");
}

#[test]
fn a_reordered_sequence_fails_at_the_first_swapped_index() {
    let a = created();
    let b = focused();
    let err = catch_unwind({
        let (a, b) = (a.clone(), b.clone());
        move || assert_events(&[a.clone(), b.clone()], &[b, a])
    });
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
fn an_empty_expected_against_a_full_actual_lists_every_event() {
    let a = created();
    let b = focused();
    let err = catch_unwind({
        let (a, b) = (a.clone(), b.clone());
        move || assert_events(&[a, b], &[])
    });
    assert_eq!(
        message(err),
        format!(
            "event sequence mismatch:\n\
             \x20 [0] EXTRA    actual   {a:?}\n\
             \x20 [1] EXTRA    actual   {b:?}\n\
             \x20 length: expected 0, actual 2"
        )
    );
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
