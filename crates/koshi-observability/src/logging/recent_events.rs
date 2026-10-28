//! The recent-events ring: the last
//! [`MAX_RECENT_EVENT_COUNT`](recent_events::MAX_RECENT_EVENT_COUNT) events one session
//! server published, as `koshi debug events` prints them.
//!
//! [`RecentEventRing::record_event`](recent_events::RecentEventRing::record_event) runs once
//! per committed event, beside [`log_event`](event_log::log_event), and keeps the newest
//! `MAX_RECENT_EVENT_COUNT` records. Record `MAX_RECENT_EVENT_COUNT + 1` drops the oldest one.
//!
//! Each record holds only the event's name and the ids it named — see
//! [`koshi_core::recent_event`]. No payload content is stored for any event
//! class: an event carrying a typed character or a submitted line leaves the
//! character and the line behind.
//!
//! A session server process holds one ring. Its dispatcher records into it, and
//! the threads that serve its control socket read it. Every reader and writer
//! recovers a poisoned lock.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

use koshi_core::event::Event;
use koshi_core::recent_event::{self, RecentEvent};

/// The most records the ring holds. Adding a record to a full ring drops the
/// oldest one.
pub const MAX_RECENT_EVENT_COUNT: usize = 1000;

/// The newest [`MAX_RECENT_EVENT_COUNT`] records of the events one session
/// server published, shared by the thread that records them and the threads
/// that read them.
#[derive(Debug, Default)]
pub struct RecentEventRing {
    /// The records, oldest first.
    recent_event_records: Mutex<VecDeque<RecentEvent>>,
}

impl RecentEventRing {
    /// Add `runtime_event` to the ring, stamped with the current wall-clock
    /// time, dropping the oldest record when the ring is full.
    ///
    /// Example: with [`MAX_RECENT_EVENT_COUNT`] records already held, recording
    /// [`koshi_core::event::Event::Quit`] leaves the ring holding the newest
    /// `MAX_RECENT_EVENT_COUNT - 1` of the old records plus the `Quit`.
    pub fn record_event(&self, runtime_event: &Event) {
        let mut recent_event_records = self.lock_recent_event_records();
        if recent_event_records.len() == MAX_RECENT_EVENT_COUNT {
            recent_event_records.pop_front();
        }
        recent_event_records
            .push_back(recent_event::record_event(runtime_event, SystemTime::now()));
    }

    /// Every record the ring holds, oldest first.
    #[must_use]
    pub fn list_recent_events(&self) -> Vec<RecentEvent> {
        self.lock_recent_event_records().iter().cloned().collect()
    }

    /// Lock the records, recovering a poisoned lock.
    fn lock_recent_event_records(&self) -> MutexGuard<'_, VecDeque<RecentEvent>> {
        self.recent_event_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests;
