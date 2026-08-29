//! The recent-events ring: the last [`CAPACITY`](recent_events::CAPACITY)
//! events this process published, as `koshi debug events` prints them.
//!
//! [`record`](recent_events::record) runs once per committed event, beside
//! [`log_event`](event_log::log_event), and keeps the newest `CAPACITY`
//! records. Record `CAPACITY + 1` drops the oldest one.
//!
//! Each record holds only the event's name and the ids it named — see
//! [`koshi_core::recent_event`]. No payload content is stored for any event
//! class, so an event carrying a typed character or a plugin failure message
//! leaves the character and the message behind.
//!
//! The ring is process-wide. Any thread may record into it or read it, and
//! every reader and writer recovers a poisoned lock.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::SystemTime;

use koshi_core::event::Event;
use koshi_core::recent_event::{self, RecentEvent};

/// How many records the ring holds. The oldest is dropped to make room for a
/// newer one.
pub const CAPACITY: usize = 1000;

/// The records, oldest first.
static RING: Mutex<VecDeque<RecentEvent>> = Mutex::new(VecDeque::new());

/// Add `event` to the ring, stamped with the current wall-clock time, dropping
/// the oldest record when the ring is full.
///
/// Example: with [`CAPACITY`] records already held, recording
/// [`koshi_core::event::Event::Quit`] leaves the ring holding the newest
/// `CAPACITY - 1` of the old records plus the `Quit`.
pub fn record(event: &Event) {
    let mut ring = RING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if ring.len() == CAPACITY {
        ring.pop_front();
    }
    ring.push_back(recent_event::record(event, SystemTime::now()));
}

/// Every record the ring holds, oldest first.
#[must_use]
pub fn recent() -> Vec<RecentEvent> {
    RING.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .cloned()
        .collect()
}

/// Empty the ring, leaving no records.
#[cfg(test)]
fn clear() {
    RING.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

#[cfg(test)]
mod tests;
