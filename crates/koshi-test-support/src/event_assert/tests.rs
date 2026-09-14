//! Tests for the event-sequence assertion.

use super::*;
use koshi_core::event::{TabClosed, TabCreated, TabFocused};
use koshi_core::ids::{ClientId, TabId};
use std::panic::catch_unwind;

fn build_tab_created_event() -> Event {
    Event::TabCreated(TabCreated {
        tab_id: TabId::new(),
    })
}

fn build_tab_focused_event() -> Event {
    Event::TabFocused(TabFocused {
        client_id: ClientId::new(),
        tab_id: TabId::new(),
        previous_tab_id: TabId::new(),
    })
}

fn build_tab_closed_event() -> Event {
    Event::TabClosed(TabClosed {
        tab_id: TabId::new(),
    })
}

/// Extract the string panic message from a caught panic.
fn extract_panic_message(panic_result: std::thread::Result<()>) -> String {
    let panic_payload = panic_result.expect_err("expected a panic");
    panic_payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            panic_payload
                .downcast_ref::<&str>()
                .map(|panic_message| (*panic_message).to_owned())
        })
        .expect("panic payload should be a string")
}

#[test]
fn an_identical_sequence_passes() {
    let created_event = build_tab_created_event();
    let focused_event = build_tab_focused_event();
    assert_events(
        &[created_event.clone(), focused_event.clone()],
        &[created_event, focused_event],
    );
}

#[test]
fn two_empty_sequences_pass() {
    assert_events(&[], &[]);
}

#[test]
fn mismatch_diff_points_at_the_divergent_index() {
    let created_event = build_tab_created_event();
    let focused_event = build_tab_focused_event();
    let unexpected_event = build_tab_closed_event();
    let error = catch_unwind(|| {
        assert_events(
            &[created_event.clone(), focused_event.clone()],
            &[created_event.clone(), unexpected_event.clone()],
        )
    });

    assert_eq!(
        extract_panic_message(error),
        format!(
            "event sequence mismatch:\n\
             \x20 [0] ok       {created_event:?}\n\
             \x20 [1] MISMATCH expected {unexpected_event:?}\n\
             \x20              actual   {focused_event:?}\n\
             \x20 length: expected 2, actual 2"
        )
    );
}

#[test]
fn a_short_actual_reports_the_missing_event() {
    let created_event = build_tab_created_event();
    let focused_event = build_tab_focused_event();
    let error = catch_unwind(|| {
        assert_events(
            std::slice::from_ref(&created_event),
            &[created_event.clone(), focused_event.clone()],
        )
    });

    assert_eq!(
        extract_panic_message(error),
        format!(
            "event sequence mismatch:\n\
             \x20 [0] ok       {created_event:?}\n\
             \x20 [1] MISSING  expected {focused_event:?}\n\
             \x20 length: expected 2, actual 1"
        )
    );
}

#[test]
fn a_long_actual_reports_the_extra_event() {
    let created_event = build_tab_created_event();
    let focused_event = build_tab_focused_event();
    let error = catch_unwind(|| {
        assert_events(
            &[created_event.clone(), focused_event.clone()],
            std::slice::from_ref(&created_event),
        )
    });

    assert_eq!(
        extract_panic_message(error),
        format!(
            "event sequence mismatch:\n\
             \x20 [0] ok       {created_event:?}\n\
             \x20 [1] EXTRA    actual   {focused_event:?}\n\
             \x20 length: expected 1, actual 2"
        )
    );
}

#[test]
fn a_reordered_sequence_fails_at_the_first_swapped_index() {
    let created_event = build_tab_created_event();
    let focused_event = build_tab_focused_event();
    let error = catch_unwind({
        let (expected_created_event, expected_focused_event) =
            (created_event.clone(), focused_event.clone());
        move || {
            assert_events(
                &[
                    expected_created_event.clone(),
                    expected_focused_event.clone(),
                ],
                &[expected_focused_event, expected_created_event],
            )
        }
    });
    assert_eq!(
        extract_panic_message(error),
        format!(
            "event sequence mismatch:\n\
             \x20 [0] MISMATCH expected {focused_event:?}\n\
             \x20              actual   {created_event:?}\n\
             \x20 [1] MISMATCH expected {created_event:?}\n\
             \x20              actual   {focused_event:?}\n\
             \x20 length: expected 2, actual 2"
        )
    );
}

#[test]
fn an_empty_expected_against_a_full_actual_lists_every_event() {
    let created_event = build_tab_created_event();
    let focused_event = build_tab_focused_event();
    let error = catch_unwind({
        let (created_event, focused_event) = (created_event.clone(), focused_event.clone());
        move || assert_events(&[created_event, focused_event], &[])
    });
    assert_eq!(
        extract_panic_message(error),
        format!(
            "event sequence mismatch:\n\
             \x20 [0] EXTRA    actual   {created_event:?}\n\
             \x20 [1] EXTRA    actual   {focused_event:?}\n\
             \x20 length: expected 0, actual 2"
        )
    );
}

#[test]
fn format_event_sequence_diff_renders_ok_mismatch_and_missing_rows_exactly() {
    let created_event = build_tab_created_event();
    let focused_event = build_tab_focused_event();
    let closed_event = build_tab_closed_event();
    let unexpected_event = build_tab_closed_event();
    let formatted_diff = format_event_sequence_diff(
        &[
            created_event.clone(),
            focused_event.clone(),
            closed_event.clone(),
        ],
        &[created_event.clone(), unexpected_event.clone()],
    );
    assert_eq!(
        formatted_diff,
        format!(
            "  [0] ok       {created_event:?}\n\
             \x20 [1] MISMATCH expected {focused_event:?}\n\
             \x20              actual   {unexpected_event:?}\n\
             \x20 [2] MISSING  expected {closed_event:?}\n\
             \x20 length: expected 3, actual 2"
        )
    );
}

#[test]
fn format_event_sequence_diff_renders_extra_rows_exactly() {
    let created_event = build_tab_created_event();
    let focused_event = build_tab_focused_event();
    let formatted_diff = format_event_sequence_diff(
        std::slice::from_ref(&created_event),
        &[created_event.clone(), focused_event.clone()],
    );
    assert_eq!(
        formatted_diff,
        format!(
            "  [0] ok       {created_event:?}\n\
             \x20 [1] EXTRA    actual   {focused_event:?}\n\
             \x20 length: expected 1, actual 2"
        )
    );
}

#[test]
fn format_event_sequence_diff_of_two_empty_sequences_is_only_the_length_line() {
    assert_eq!(
        format_event_sequence_diff(&[], &[]),
        "  length: expected 0, actual 0"
    );
}
