//! The recent-events ring: the last [`MAX_RECENT_EVENT_COUNT`](recent_events::MAX_RECENT_EVENT_COUNT)
//! events this process published, as `koshi debug events` prints them.
//!
//! [`record_event`](recent_events::record_event) runs once per committed event, beside
//! [`log_event`](event_log::log_event), and keeps the newest `MAX_RECENT_EVENT_COUNT`
//! records. Record `MAX_RECENT_EVENT_COUNT + 1` drops the oldest one.
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
pub const MAX_RECENT_EVENT_COUNT: usize = 1000;

/// The records, oldest first.
static RECENT_EVENT_RING: Mutex<VecDeque<RecentEvent>> = Mutex::new(VecDeque::new());

/// Lock the ring, recovering a poisoned lock.
fn lock_recent_event_ring() -> MutexGuard<'static, VecDeque<RecentEvent>> {
    RECENT_EVENT_RING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Add `event` to the ring, stamped with the current wall-clock time, dropping
/// the oldest record when the ring is full.
///
/// Example: with [`MAX_RECENT_EVENT_COUNT`] records already held, recording
/// [`koshi_core::event::Event::Quit`] leaves the ring holding the newest
/// `MAX_RECENT_EVENT_COUNT - 1` of the old records plus the `Quit`.
pub fn record_event(runtime_event: &Event) {
    let mut recent_event_records = lock_recent_event_ring();
    if recent_event_records.len() == MAX_RECENT_EVENT_COUNT {
        recent_event_records.pop_front();
    }
    recent_event_records.push_back(recent_event::record_event(runtime_event, SystemTime::now()));
}

/// Every record the ring holds, oldest first.
#[must_use]
pub fn list_recent_events() -> Vec<RecentEvent> {
    lock_recent_event_ring().iter().cloned().collect()
}

/// Remove every record from the ring.
#[cfg(test)]
fn clear_recent_events() {
    lock_recent_event_ring().clear();
}

#[cfg(test)]
mod tests;
