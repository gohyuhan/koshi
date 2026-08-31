//! The recent-events ring: the last [`CAPACITY`](recent_events::CAPACITY)
//! events this process published, as `koshi debug events` prints them.
//!
//! [`record`](recent_events::record) runs once per committed event, beside
//! [`log_event`](event_log::log_event), and keeps the newest `CAPACITY`
//! records. Record `CAPACITY + 1` drops the oldest one.
//!
//! Each record holds only the event's name and the ids it named — see
//! [`koshi_core::recent_event`]. No payload content is stored for any event
//! class: an event carrying a typed character or a plugin failure message
//! leaves the character and the message behind.
//!
//! The ring is process-wide. Any thread may record into it or read it, and
//! every reader and writer recovers a poisoned lock.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

use koshi_core::event::Event;
use koshi_core::recent_event::{self, RecentEvent};

/// The most records the ring holds. Adding a record to a full ring drops the
/// oldest one.
pub const CAPACITY: usize = 1000;

/// The records, oldest first.
static RING: Mutex<VecDeque<RecentEvent>> = Mutex::new(VecDeque::new());

/// Lock the ring, recovering a poisoned lock.
fn ring() -> MutexGuard<'static, VecDeque<RecentEvent>> {
    RING.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Add `event` to the ring, stamped with the current wall-clock time, dropping
/// the oldest record when the ring is full.
///
/// Example: with [`CAPACITY`] records already held, recording
/// [`koshi_core::event::Event::Quit`] leaves the ring holding the newest
/// `CAPACITY - 1` of the old records plus the `Quit`.
pub fn record(event: &Event) {
    let mut records = ring();
    if records.len() == CAPACITY {
        records.pop_front();
    }
    records.push_back(recent_event::record(event, SystemTime::now()));
}

/// Every record the ring holds, oldest first.
#[must_use]
pub fn recent() -> Vec<RecentEvent> {
    ring().iter().cloned().collect()
}

/// Remove every record from the ring.
#[cfg(test)]
fn clear() {
    ring().clear();
}

#[cfg(test)]
mod tests;
