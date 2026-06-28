//! Deterministic event-sequence recorder for command-transaction tests.
//!
//! A command applied to the runtime produces an ordered burst of [`tile_core::event::Event`]s.
//! Tests want to assert that burst *exactly* — same events, same order, nothing
//! extra. [`event_queue::RecordedEvents`] is a tiny in-memory log with consuming assertions
//! ([`assert_prefix`](event_queue::RecordedEvents::assert_prefix),
//! [`assert_exact`](event_queue::RecordedEvents::assert_exact),
//! [`assert_no_more`](event_queue::RecordedEvents::assert_no_more)) that pretty-print an
//! index-aligned diff when the sequence does not match, so a failing test points
//! straight at the first divergence.
//!
//! ## Channel-agnostic drain helper
//!
//! The runtime's event bus delivers events over a bounded channel. The drain
//! helper ([`drain_from`](event_queue::RecordedEvents::drain_from)) is channel-agnostic: it
//! takes a `FnMut() -> Option<tile_core::event::Event>` puller, so tests compose it with any
//! bounded channel — `std::sync::mpsc` (`|| rx.try_recv().ok()`), crossbeam, or
//! a future type — without depending on the concrete channel crate here.

use tile_core::event::Event;

/// An ordered, consuming log of recorded [`Event`]s.
///
/// Events are appended in emission order via [`push`](Self::push) (or pulled in
/// bulk with [`drain_from`](Self::drain_from)). The assertion methods consume
/// matched events from the front, so a test reads as a sequence of expectations
/// that walk the burst start to finish, ending in
/// [`assert_no_more`](Self::assert_no_more).
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
    /// `next` is the channel-agnostic source described in the module docs; for a
    /// `std::sync::mpsc` receiver pass `|| rx.try_recv().ok()`.
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
    /// Leaves any trailing events for further assertions. Panics with an
    /// index-aligned diff if the prefix does not match (including when fewer
    /// events remain than `expected` requires).
    ///
    /// # Panics
    ///
    /// If the next `expected.len()` events are not exactly `expected`.
    pub fn assert_prefix(&mut self, expected: &[Event]) {
        let available = self.inner.len().min(expected.len());
        if expected.len() > self.inner.len() || self.inner[..available] != *expected {
            panic!(
                "event prefix mismatch:\n{}",
                format_diff(expected, &self.inner)
            );
        }
        self.inner.drain(..expected.len());
    }

    /// Assert the remaining events are *exactly* `expected`, then consume them.
    ///
    /// Equivalent to [`assert_prefix`](Self::assert_prefix) followed by
    /// [`assert_no_more`](Self::assert_no_more), but reports length and content
    /// divergence in one diff.
    ///
    /// # Panics
    ///
    /// If the remaining events differ from `expected` in length or content.
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
mod tests {
    use super::*;
    use std::panic::catch_unwind;
    use tile_core::event::{TabClosed, TabCreated, TabFocused};
    use tile_core::ids::TabId;

    fn created() -> Event {
        Event::TabCreated(TabCreated {
            tab_id: TabId::new(),
        })
    }

    fn focused() -> Event {
        Event::TabFocused(TabFocused {
            tab_id: TabId::new(),
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
}
