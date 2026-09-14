//! Assert event sequences in tests.
//!
//! A runtime command produces an ordered burst of
//! [`koshi_core::event::Event`]s. [`event_assert::assert_events`] compares it
//! with an expected sequence and panics with an index-aligned diff when they
//! differ.

use koshi_core::event::Event;

/// Assert that `actual_events` and `expected_events` contain the same events in the same
/// order and with the same count.
///
/// # Panics
///
/// Panics when the slices differ in length or content. The message contains an
/// index-aligned diff with `ok`, `MISMATCH`, `MISSING`, or `EXTRA` rows and a
/// final line with both lengths.
pub fn assert_events(actual_events: &[Event], expected_events: &[Event]) {
    if actual_events != expected_events {
        panic!(
            "event sequence mismatch:\n{}",
            format_event_sequence_diff(expected_events, actual_events)
        );
    }
}

/// Build an index-aligned expected-versus-actual diff, with one line per
/// position and a final length line.
fn format_event_sequence_diff(expected_events: &[Event], actual_events: &[Event]) -> String {
    let mut diff = String::new();
    let event_count = expected_events.len().max(actual_events.len());
    for event_index in 0..event_count {
        match (
            expected_events.get(event_index),
            actual_events.get(event_index),
        ) {
            (Some(expected_event), Some(actual_event)) if expected_event == actual_event => {
                diff.push_str(&format!("  [{event_index}] ok       {expected_event:?}\n"));
            }
            (Some(expected_event), Some(actual_event)) => {
                diff.push_str(&format!(
                    "  [{event_index}] MISMATCH expected {expected_event:?}\n"
                ));
                diff.push_str(&format!("               actual   {actual_event:?}\n"));
            }
            (Some(expected_event), None) => {
                diff.push_str(&format!(
                    "  [{event_index}] MISSING  expected {expected_event:?}\n"
                ));
            }
            (None, Some(actual_event)) => {
                diff.push_str(&format!(
                    "  [{event_index}] EXTRA    actual   {actual_event:?}\n"
                ));
            }
            (None, None) => unreachable!("index is bounded by the longer slice"),
        }
    }
    diff.push_str(&format!(
        "  length: expected {}, actual {}",
        expected_events.len(),
        actual_events.len()
    ));
    diff
}

#[cfg(test)]
mod tests;
