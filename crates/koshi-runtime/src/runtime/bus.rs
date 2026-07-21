//! Event fan-out: the bounded per-subscriber delivery hub.
//!
//! [`EventBus::subscribe`] registers a subscriber and hands back the receiving
//! end of that subscriber's own bounded queue. [`EventBus::publish`] clones
//! each event into every queue whose filter matches. Delivery never blocks the
//! dispatcher: an event that does not fit a subscriber's full queue is dropped
//! for that subscriber and logged, and a subscriber whose receiver was dropped
//! is removed on the next publish.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};

use koshi_core::event::Event;

/// How many undelivered events one subscriber's queue holds. An event
/// published while the queue is full is dropped for that subscriber.
const SUBSCRIBER_QUEUE_CAPACITY: usize = 1024;

/// Which published events a subscriber receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum EventFilter {
    /// Every event.
    #[default]
    All,
}

impl EventFilter {
    /// Whether `event` passes this filter.
    fn matches(self, _event: &Event) -> bool {
        match self {
            EventFilter::All => true,
        }
    }
}

/// One registered subscriber: its filter and the sending end of its queue.
#[derive(Debug)]
struct Subscriber {
    /// Which events this subscriber receives.
    filter: EventFilter,
    /// Sending end of the subscriber's bounded queue; the receiver lives with
    /// the subscriber.
    tx: SyncSender<Event>,
}

/// Event fan-out hub: every published event is delivered to each live
/// subscriber whose filter matches, over that subscriber's own bounded queue.
#[derive(Debug, Default)]
pub struct EventBus {
    /// Live subscribers, in subscription order.
    subscribers: Vec<Subscriber>,
}

impl EventBus {
    /// A bus with no subscribers.
    #[must_use]
    pub fn new() -> Self {
        EventBus {
            subscribers: Vec::new(),
        }
    }

    /// Register a subscriber for the events `filter` selects and hand back the
    /// receiving end of its queue. Dropping the receiver ends the
    /// subscription; the bus notices on the next publish.
    pub fn subscribe(&mut self, filter: EventFilter) -> Receiver<Event> {
        let (tx, rx) = sync_channel(SUBSCRIBER_QUEUE_CAPACITY);
        self.subscribers.push(Subscriber { filter, tx });
        rx
    }

    /// Deliver `event` to every subscriber whose filter matches it. A
    /// subscriber whose queue is full misses this event (logged as a warning);
    /// a subscriber whose receiver is gone is removed.
    pub fn publish(&mut self, event: &Event) {
        self.subscribers.retain(|subscriber| {
            if !subscriber.filter.matches(event) {
                return true;
            }
            match subscriber.tx.try_send(event.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    tracing::warn!("event dropped; subscriber queue full");
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
    }

    /// How many subscribers are registered. Counts subscribers whose receiver
    /// is already gone but whose removal awaits the next publish.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }
}

#[cfg(test)]
mod tests;
