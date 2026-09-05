//! Buffered reads from one host-terminal event source.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use koshi_input::host::Event;

use super::platform::{PlatformEventSource, PlatformWaker};

/// A platform source that can wait for one parsed event.
pub(super) trait EventSource {
    /// Return one event, or `None` when `timeout` expires.
    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<Event>>;
}

/// The owned event reader for one attached terminal.
#[derive(Debug)]
pub(super) struct InputReader<S = PlatformEventSource> {
    source: S,
    buffered: VecDeque<Event>,
}

impl InputReader<PlatformEventSource> {
    /// Build a reader around one platform source.
    pub(super) fn new(source: PlatformEventSource) -> Self {
        Self {
            source,
            buffered: VecDeque::with_capacity(32),
        }
    }

    /// Build a handle that interrupts this reader's platform wait.
    pub(super) fn waker(&self) -> PlatformWaker {
        self.source.waker()
    }
}

impl<S: EventSource> InputReader<S> {
    /// Wait until an event accepted by `filter` is buffered.
    pub(super) fn poll(
        &mut self,
        timeout: Option<Duration>,
        mut filter: impl FnMut(&Event) -> bool,
    ) -> io::Result<bool> {
        if self.buffered.iter().any(&mut filter) {
            return Ok(true);
        }

        let deadline = timeout.map(|duration| Instant::now() + duration);
        loop {
            let remaining = deadline.map(|end| end.saturating_duration_since(Instant::now()));
            match self.source.try_read(remaining)? {
                Some(event) => {
                    let accepted = filter(&event);
                    self.buffered.push_back(event);
                    if accepted {
                        return Ok(true);
                    }
                }
                None => return Ok(false),
            }
            if deadline.is_some_and(|end| Instant::now() >= end) {
                return Ok(false);
            }
        }
    }

    /// Return the first event accepted by `filter`.
    pub(super) fn read(&mut self, mut filter: impl FnMut(&Event) -> bool) -> io::Result<Event> {
        loop {
            if let Some(index) = self.buffered.iter().position(&mut filter) {
                return self
                    .buffered
                    .remove(index)
                    .ok_or_else(|| io::Error::other("buffered terminal event disappeared"));
            }
            if let Some(event) = self.source.try_read(None)? {
                if filter(&event) {
                    return Ok(event);
                }
                self.buffered.push_back(event);
            }
        }
    }
}

#[cfg(test)]
mod tests;
