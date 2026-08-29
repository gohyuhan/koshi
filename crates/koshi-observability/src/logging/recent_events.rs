//! The recent-events ring: the last [`CAPACITY`](recent_events::CAPACITY)
//! events this process published or observed, as `koshi debug events` prints
//! them.
//!
//! [`record`](recent_events::record) runs once per committed event, beside
//! [`log_event`](event_log::log_event), and keeps the newest `CAPACITY` records.
//! An attached client uses
//! [`record_client_event`](crate::logging::recent_events::record_client_event)
//! for the content-free session frames it observes, so its crash report names
//! the session activity it received. Record `CAPACITY + 1` drops the oldest one.
//!
//! Each record holds only the event or session-frame name and the ids it names
//! — see [`koshi_core::recent_event`]. No payload content is stored for any event
//! class, so an event carrying a typed character or a plugin failure message
//! leaves the character and the message behind.
//!
//! The ring is process-wide. Any thread may record into it or read it, and
//! every reader and writer recovers a poisoned lock.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::{Mutex, TryLockError};
use std::time::SystemTime;

use koshi_core::event::Event;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::recent_event::{self, RecentEvent};

/// How many records the ring holds. The oldest is dropped to make room for a
/// newer one.
pub const CAPACITY: usize = 1000;

/// The records, oldest first.
static RING: Mutex<VecDeque<RecentEvent>> = Mutex::new(VecDeque::new());

#[cfg(test)]
static TEST_SERIAL: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn lock_for_test() -> std::sync::MutexGuard<'static, ()> {
    TEST_SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Add `event` to the ring, stamped with the current wall-clock time, dropping
/// the oldest record when the ring is full.
///
/// Example: with [`CAPACITY`] records already held, recording
/// [`koshi_core::event::Event::Quit`] leaves the ring holding the newest
/// `CAPACITY - 1` of the old records plus the `Quit`.
pub fn record(event: &Event) {
    push(recent_event::record(event, SystemTime::now()));
}

/// Add a content-free session event that this client received from its server.
///
/// The client supplies the session id because the wire event names only the
/// ids carried by its payload. The name and optional ids come from the
/// exhaustive session-frame projection in `koshi-client`.
pub fn record_client_event(
    session_id: SessionId,
    name: &'static str,
    client: Option<ClientId>,
    tab: Option<TabId>,
    pane: Option<PaneId>,
) {
    push(RecentEvent {
        at: SystemTime::now(),
        name: Cow::Borrowed(name),
        session: Some(session_id),
        client,
        tab,
        pane,
        plugin: None,
        command: None,
        subscriber: None,
    });
}

fn push(event: RecentEvent) {
    let mut ring = RING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if ring.len() == CAPACITY {
        ring.pop_front();
    }
    ring.push_back(event);
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

/// Read the ring without waiting for an emitter that currently holds it.
///
/// `None` means that the ring is locked now. A poisoned lock is recovered,
/// because a panic report can still use records that another thread left.
#[must_use]
pub fn try_recent() -> Option<Vec<RecentEvent>> {
    match RING.try_lock() {
        Ok(ring) => Some(ring.iter().cloned().collect()),
        Err(TryLockError::Poisoned(poisoned)) => {
            Some(poisoned.into_inner().iter().cloned().collect())
        }
        Err(TryLockError::WouldBlock) => None,
    }
}

#[cfg(test)]
pub(crate) fn with_lock_for_test<T>(work: impl FnOnce() -> T) -> T {
    let _guard = RING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    work()
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
