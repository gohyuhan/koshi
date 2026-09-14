//! Buffered reads from one host-terminal event source.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use koshi_input::host::Event;

use super::platform::{PlatformEventSource, PlatformWaker};

/// A platform source that can wait for one parsed event.
pub(super) trait EventSource {
    /// Return one event, or `None` when `timeout` expires.
    fn try_read_event(&mut self, timeout: Option<Duration>) -> io::Result<Option<Event>>;
}

/// The owned event reader for one attached terminal.
#[derive(Debug)]
pub(super) struct InputReader<S = PlatformEventSource> {
    event_source: S,
    buffered_events: VecDeque<Event>,
}

impl InputReader<PlatformEventSource> {
    /// Build a reader around one platform event source.
    pub(super) fn from_event_source(event_source: PlatformEventSource) -> Self {
        Self {
            event_source,
            buffered_events: VecDeque::with_capacity(32),
        }
    }

    /// Build a handle that interrupts this reader's platform wait.
    pub(super) fn create_waker(&self) -> PlatformWaker {
        self.event_source.create_waker()
    }
}

impl<S: EventSource> InputReader<S> {
    #[cfg(test)]
    pub(super) fn from_event_source_for_tests(event_source: S) -> Self {
        Self {
            event_source,
            buffered_events: VecDeque::with_capacity(32),
        }
    }

    /// Wait until an event accepted by `event_filter` is buffered.
    pub(super) fn wait_for_event(
        &mut self,
        timeout: Option<Duration>,
        mut event_filter: impl FnMut(&Event) -> bool,
    ) -> io::Result<bool> {
        if self.buffered_events.iter().any(&mut event_filter) {
            return Ok(true);
        }

        let deadline_instant = timeout.map(|timeout_duration| Instant::now() + timeout_duration);
        loop {
            let remaining_timeout =
                deadline_instant.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            match self.event_source.try_read_event(remaining_timeout)? {
                Some(event) => {
                    let is_accepted = event_filter(&event);
                    self.buffered_events.push_back(event);
                    if is_accepted {
                        return Ok(true);
                    }
                }
                None => return Ok(false),
            }
            if deadline_instant.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(false);
            }
        }
    }

    /// Return the first event accepted by `event_filter`.
    pub(super) fn read_matching_event(
        &mut self,
        mut event_filter: impl FnMut(&Event) -> bool,
    ) -> io::Result<Event> {
        loop {
            if let Some(buffered_event_index) =
                self.buffered_events.iter().position(&mut event_filter)
            {
                return self
                    .buffered_events
                    .remove(buffered_event_index)
                    .ok_or_else(|| io::Error::other("buffered terminal event disappeared"));
            }
            if let Some(event) = self.event_source.try_read_event(None)? {
                if event_filter(&event) {
                    return Ok(event);
                }
                self.buffered_events.push_back(event);
            }
        }
    }
}

#[cfg(test)]
mod tests;
