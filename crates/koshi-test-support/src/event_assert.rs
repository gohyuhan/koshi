//! Assert event sequences in tests.
//!
//! A runtime command produces an ordered burst of
//! [`koshi_core::event::Event`]s. [`event_assert::assert_events`] compares it
//! with an expected sequence and panics with an index-aligned diff when they
//! differ.

use koshi_core::event::Event;

/// Assert that `actual` and `expected` contain the same events in the same
/// order and with the same count.
///
/// # Panics
///
/// Panics when the slices differ in length or content. The message contains an
/// index-aligned diff with `ok`, `MISMATCH`, `MISSING`, or `EXTRA` rows and a
/// final line with both lengths.
pub fn assert_events(actual: &[Event], expected: &[Event]) {
    if actual != expected {
        panic!(
            "event sequence mismatch:\n{}",
            format_diff(expected, actual)
        );
    }
}

/// Build an index-aligned `expected` versus `actual` diff, with one line per
/// position and a final length line.
fn format_diff(expected: &[Event], actual: &[Event]) -> String {
    let mut diff = String::new();
    let rows = expected.len().max(actual.len());
    for i in 0..rows {
        match (expected.get(i), actual.get(i)) {
            (Some(e), Some(a)) if e == a => {
                diff.push_str(&format!("  [{i}] ok       {e:?}\n"));
            }
            (Some(e), Some(a)) => {
                diff.push_str(&format!("  [{i}] MISMATCH expected {e:?}\n"));
                diff.push_str(&format!("               actual   {a:?}\n"));
            }
            (Some(e), None) => {
                diff.push_str(&format!("  [{i}] MISSING  expected {e:?}\n"));
            }
            (None, Some(a)) => {
                diff.push_str(&format!("  [{i}] EXTRA    actual   {a:?}\n"));
            }
            (None, None) => unreachable!("index is bounded by the longer slice"),
        }
    }
    diff.push_str(&format!(
        "  length: expected {}, actual {}",
        expected.len(),
        actual.len()
    ));
    diff
}

#[cfg(test)]
mod tests;
