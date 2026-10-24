//! Buffered reads from one host-terminal event source.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use koshi_input::host::Event;

use super::platform::{PlatformEventSource, PlatformWaker};

/// A platform source that can wait for one parsed event.
pub(super) trait EventSource {
    /// Return one event, or `None` when `timeout_duration` expires. An event
    /// parsed from bytes an earlier call read comes back before the terminal
    /// is read again.
    fn try_read_event(&mut self, timeout_duration: Option<Duration>) -> io::Result<Option<Event>>;

    /// Remove and return the next event parsed from bytes this source already
    /// read, without reading the terminal. `None` once every event parsed
    /// from those bytes is returned.
    fn remove_next_parsed_event(&mut self) -> Option<Event>;
}

/// The owned event reader for one attached terminal.
#[derive(Debug)]
pub(super) struct InputReader<Source = PlatformEventSource> {
    event_source: Source,
    buffered_terminal_events: VecDeque<Event>,
}

impl InputReader<PlatformEventSource> {
    /// Build a handle that interrupts this reader's platform wait.
    pub(super) fn create_waker(&self) -> PlatformWaker {
        self.event_source.create_waker()
    }
}

impl<Source: EventSource> InputReader<Source> {
    /// A reader over `event_source` with nothing buffered.
    pub(super) fn from_event_source(event_source: Source) -> Self {
        Self {
            event_source,
            buffered_terminal_events: VecDeque::with_capacity(32),
        }
    }

    /// Wait until an event accepted by `event_filter` is buffered. Every event
    /// read on the way stays buffered in arrival order. Returns `true` once
    /// such an event is buffered, and `false` when `timeout_duration` passes
    /// first.
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

    /// Remove and return the events of the next read of the terminal, oldest
    /// first, with no time limit: the next event, then every event parsed
    /// from the bytes that read returned.
    ///
    /// An event that [`Self::wait_for_event`] buffered comes back alone, the
    /// oldest first. Typing `é-` that the terminal sends in one read returns
    /// the keys `é` and `-` in one list.
    pub(super) fn read_events_of_next_terminal_read(&mut self) -> io::Result<Vec<Event>> {
        if let Some(buffered_terminal_event) = self.buffered_terminal_events.pop_front() {
            return Ok(vec![buffered_terminal_event]);
        }
        let first_terminal_event = loop {
            if let Some(terminal_event) = self.event_source.try_read_event(None)? {
                break terminal_event;
            }
        };
        let mut terminal_events = vec![first_terminal_event];
        while let Some(parsed_terminal_event) = self.event_source.remove_next_parsed_event() {
            terminal_events.push(parsed_terminal_event);
        }
        Ok(terminal_events)
    }

    /// Remove and return the first event accepted by `event_filter`, from the
    /// buffered events first, and then from the event source, with no time
    /// limit. Every rejected event read from the source stays buffered.
    pub(super) fn read_matching_event(
        &mut self,
        mut event_filter: impl FnMut(&Event) -> bool,
    ) -> io::Result<Event> {
        loop {
            if let Some(buffered_terminal_event) = self
                .buffered_terminal_events
                .iter()
                .position(&mut event_filter)
                .and_then(|buffered_terminal_event_index| {
                    self.buffered_terminal_events
                        .remove(buffered_terminal_event_index)
                })
            {
                return Ok(buffered_terminal_event);
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
