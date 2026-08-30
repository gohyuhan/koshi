//! Deterministic event-sequence recorder for command-transaction tests.
//!
//! A command applied to the runtime produces an ordered burst of [`koshi_core::event::Event`]s.
//! [`event_queue::RecordedEvents`] is an in-memory log of that burst with
//! consuming assertions ([`assert_prefix`](event_queue::RecordedEvents::assert_prefix),
//! [`assert_exact`](event_queue::RecordedEvents::assert_exact),
//! [`assert_no_more`](event_queue::RecordedEvents::assert_no_more)). Each one
//! prints an index-aligned diff when the sequence does not match.
//!
//! ## Pulling events from a channel
//!
//! [`drain_from`](event_queue::RecordedEvents::drain_from) takes any
//! `FnMut() -> Option<koshi_core::event::Event>` puller and appends each
//! event it returns until it returns `None`. A test passes
//! `|| rx.try_recv().ok()` for a `std::sync::mpsc` receiver.

use koshi_core::event::Event;

/// An ordered, consuming log of recorded [`Event`]s.
///
/// [`push`](Self::push) appends one event in emission order.
/// [`drain_from`](Self::drain_from) appends many. The assertion methods consume
/// matched events from the front. [`assert_no_more`](Self::assert_no_more)
/// checks that nothing is left.
#[derive(Debug, Clone, Default)]
pub struct RecordedEvents {
    inner: Vec<Event>,
}

impl RecordedEvents {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a single event in emission order.
    pub fn push(&mut self, event: Event) {
        self.inner.push(event);
    }

    /// Pull events until `next` returns `None`, appending each in order.
    ///
    /// For a `std::sync::mpsc` receiver pass `|| rx.try_recv().ok()`.
    pub fn drain_from(&mut self, mut next: impl FnMut() -> Option<Event>) {
        while let Some(event) = next() {
            self.inner.push(event);
        }
    }

    /// How many events remain unconsumed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether no events remain unconsumed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Remove and return all remaining events, leaving the recorder empty.
    pub fn take(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.inner)
    }

    /// Assert the remaining events begin with `expected`, then consume them.
    ///
    /// Leaves any trailing events for further assertions.
    ///
    /// # Panics
    ///
    /// If the next `expected.len()` events are not exactly `expected`, or if
    /// fewer than `expected.len()` events remain. The panic message holds an
    /// index-aligned diff of `expected` against the first `expected.len()`
    /// events; events past them are not in it. A failed assertion consumes
    /// nothing.
    pub fn assert_prefix(&mut self, expected: &[Event]) {
        if self.inner.len() < expected.len() || self.inner[..expected.len()] != *expected {
            let compared = self.inner.len().min(expected.len());
            panic!(
                "event prefix mismatch:\n{}",
                format_diff(expected, &self.inner[..compared])
            );
        }
        self.inner.drain(..expected.len());
    }

    /// Assert the remaining events are *exactly* `expected`, then consume them.
    ///
    /// Passes in the same cases as [`assert_prefix`](Self::assert_prefix)
    /// followed by [`assert_no_more`](Self::assert_no_more).
    ///
    /// # Panics
    ///
    /// If the remaining events differ from `expected` in length or content. The
    /// panic message holds one index-aligned diff covering both. A failed
    /// assertion consumes nothing.
    pub fn assert_exact(&mut self, expected: &[Event]) {
        if self.inner != *expected {
            panic!(
                "event sequence mismatch:\n{}",
                format_diff(expected, &self.inner)
            );
        }
        self.inner.clear();
    }

    /// Assert no events remain unconsumed.
    ///
    /// # Panics
    ///
    /// If any events remain, listing the unexpected trailing events.
    pub fn assert_no_more(&self) {
        if !self.inner.is_empty() {
            panic!(
                "expected no more events, but {} remain:\n{}",
                self.inner.len(),
                format_diff(&[], &self.inner)
            );
        }
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
