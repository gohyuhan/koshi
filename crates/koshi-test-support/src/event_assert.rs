//! Event-sequence assertion for tests.
//!
//! A command applied to the runtime produces an ordered burst of
//! [`koshi_core::event::Event`]s.
//! [`event_assert::assert_events`] compares that burst against the expected
//! sequence and panics with an index-aligned diff when the two differ.

use koshi_core::event::Event;

/// Assert `actual` is exactly `expected`: the same events, in the same order,
/// and the same count.
///
/// # Panics
///
/// If `actual` and `expected` differ in length or content. The panic message
/// holds an index-aligned diff, one line per position, each row marked `ok`,
/// `MISMATCH`, `MISSING` or `EXTRA`, and a closing line with both lengths.
pub fn assert_events(actual: &[Event], expected: &[Event]) {
    if actual != expected {
        panic!(
            "event sequence mismatch:\n{}",
            format_diff(expected, actual)
        );
    }
}

/// Render an index-aligned `expected` vs `actual` diff, one line per position,
/// marking the rows that differ and any length mismatch.
fn format_diff(expected: &[Event], actual: &[Event]) -> String {
    let mut out = String::new();
    let rows = expected.len().max(actual.len());
    for i in 0..rows {
        match (expected.get(i), actual.get(i)) {
            (Some(e), Some(a)) if e == a => {
                out.push_str(&format!("  [{i}] ok       {e:?}\n"));
            }
            (Some(e), Some(a)) => {
                out.push_str(&format!("  [{i}] MISMATCH expected {e:?}\n"));
                out.push_str(&format!("               actual   {a:?}\n"));
            }
            (Some(e), None) => {
                out.push_str(&format!("  [{i}] MISSING  expected {e:?}\n"));
            }
            (None, Some(a)) => {
                out.push_str(&format!("  [{i}] EXTRA    actual   {a:?}\n"));
            }
            (None, None) => unreachable!("index is bounded by the longer slice"),
        }
    }
    out.push_str(&format!(
        "  length: expected {}, actual {}",
        expected.len(),
        actual.len()
    ));
    out
}

#[cfg(test)]
mod tests;
