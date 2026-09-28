//! Buffered reads from one host-terminal event source.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use koshi_input::host::Event;

use super::platform::{PlatformEventSource, PlatformWaker};

/// A platform source that can wait for one parsed event.
pub(super) trait EventSource {
    /// Return one event, or `None` when `timeout_duration` expires.
    fn try_read_event(&mut self, timeout_duration: Option<Duration>) -> io::Result<Option<Event>>;
}

/// The owned event reader for one attached terminal.
#[derive(Debug)]
pub(super) struct InputReader<Source = PlatformEventSource> {
    event_source: Source,
    buffered_terminal_events: VecDeque<Event>,
}

impl InputReader<PlatformEventSource> {
    /// Build a reader around one platform event source.
    pub(super) fn from_event_source(event_source: PlatformEventSource) -> Self {
        Self {
            event_source,
            buffered_terminal_events: VecDeque::with_capacity(32),
        }
    }

    /// Build a handle that interrupts this reader's platform wait.
    pub(super) fn create_waker(&self) -> PlatformWaker {
        self.event_source.create_waker()
    }
}

impl<Source: EventSource> InputReader<Source> {
    /// Wait until an event accepted by `event_filter` is buffered.
    pub(super) fn wait_for_event(
        &mut self,
        timeout_duration: Option<Duration>,
        mut event_filter: impl FnMut(&Event) -> bool,
    ) -> io::Result<bool> {
        if self.buffered_terminal_events.iter().any(&mut event_filter) {
            return Ok(true);
        }

        let wait_deadline_instant =
            timeout_duration.map(|wait_duration| Instant::now() + wait_duration);
        loop {
            let remaining_timeout_duration = wait_deadline_instant.map(|wait_deadline_instant| {
                wait_deadline_instant.saturating_duration_since(Instant::now())
            });
            match self
                .event_source
                .try_read_event(remaining_timeout_duration)?
            {
                Some(terminal_event) => {
                    let is_terminal_event_accepted = event_filter(&terminal_event);
                    self.buffered_terminal_events.push_back(terminal_event);
                    if is_terminal_event_accepted {
                        return Ok(true);
                    }
                }
                None => return Ok(false),
            }
            if wait_deadline_instant
                .is_some_and(|wait_deadline_instant| Instant::now() >= wait_deadline_instant)
            {
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
            if let Some(buffered_terminal_event_index) = self
                .buffered_terminal_events
                .iter()
                .position(&mut event_filter)
            {
                return self
                    .buffered_terminal_events
                    .remove(buffered_terminal_event_index)
                    .ok_or_else(|| io::Error::other("buffered terminal event disappeared"));
            }
            if let Some(terminal_event) = self.event_source.try_read_event(None)? {
                if event_filter(&terminal_event) {
                    return Ok(terminal_event);
                }
                self.buffered_terminal_events.push_back(terminal_event);
            }
        }
    }
}

#[cfg(test)]
mod tests;
