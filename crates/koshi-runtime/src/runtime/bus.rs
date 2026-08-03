//! Event fan-out: the bounded per-subscriber delivery hub.
//!
//! `EventBus::subscribe` registers a subscriber and hands back its id plus
//! the receiving end of that subscriber's own bounded queue.
//! `EventBus::publish` clones each event into every queue whose filter
//! matches, `EventBus::try_send_frame` puts the frame the session composed
//! for one subscriber's client on that subscriber's own queue,
//! `EventBus::try_send_answer` puts one round of mouse answers on it, and
//! `EventBus::try_send_host_write` puts bytes aimed at that subscriber's own
//! terminal on it. Delivery
//! never blocks the dispatcher: a subscriber whose receiver was dropped is
//! removed on the next publish, and an event that does not fit a subscriber's
//! full queue is handled by its class.
//!
//! A dropped [`EventClass::Lossy`] event is logged and forgotten. A dropped
//! [`EventClass::Critical`] event marks the subscriber desynced: it receives
//! nothing at all — critical and lossy alike — while it counts the critical
//! events it misses, until `EventBus::try_resync` puts a fresh
//! [`RenderSnapshot`] on its queue and returns it to live delivery. The
//! snapshot rides the same queue as events, so the subscriber reads the
//! backlog it already had, then the snapshot, then live events again.
//!
//! A subscriber in another process works in the wire spellings from
//! `koshi-ipc` instead. The two conversions between them live here: the
//! [`From`] impl on [`EventFilter`] reads the filter an attaching client sent,
//! and [`wire_event`] turns one queue item into the frame that client is sent.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};

use koshi_core::event::{classify, Event, EventClass, SubscriberLagged};
use koshi_core::ids::SubscriberId;
use koshi_core::mouse::MouseAnswer;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::protocol::EventFilterSpec;
use koshi_renderer::snapshot::{Delivery, RenderSnapshot};

use crate::runtime::frame::wire_frame;

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

impl From<EventFilterSpec> for EventFilter {
    /// The filter an attaching client asked for, in the form the bus works in.
    fn from(spec: EventFilterSpec) -> Self {
        match spec {
            EventFilterSpec::All => EventFilter::All,
        }
    }
}

/// Whether a subscriber is receiving events, or paused awaiting a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryState {
    /// Matching events are delivered as they are published.
    Live,
    /// A critical event did not fit the queue; nothing is delivered until a
    /// snapshot lands.
    Desynced {
        /// How many critical events the subscriber has missed, counting the one
        /// that caused the pause.
        dropped: u64,
    },
}

/// One registered subscriber: its id, its filter, its delivery state, and the
/// sending end of its queue.
#[derive(Debug)]
struct Subscriber {
    /// Stable id assigned at subscription, named in log lines about this
    /// subscriber.
    id: SubscriberId,
    /// Which events this subscriber receives.
    filter: EventFilter,
    /// Whether this subscriber is receiving events or paused awaiting a
    /// snapshot.
    state: DeliveryState,
    /// Sending end of the subscriber's bounded queue; the receiver lives with
    /// the subscriber.
    tx: SyncSender<Delivery>,
}

/// Event fan-out hub: every published event is delivered to each live
/// subscriber whose filter matches, over that subscriber's own bounded queue.
#[derive(Debug, Default)]
pub(crate) struct EventBus {
    /// Live subscribers, in subscription order.
    subscribers: Vec<Subscriber>,
}

impl EventBus {
    /// A bus with no subscribers.
    #[must_use]
    pub(crate) fn new() -> Self {
        EventBus {
            subscribers: Vec::new(),
        }
    }

    /// Register a subscriber for the events `filter` selects and hand back its
    /// id plus the receiving end of its queue. The subscriber starts live.
    /// Dropping the receiver ends the subscription; the bus notices on the next
    /// publish.
    pub(crate) fn subscribe(&mut self, filter: EventFilter) -> (SubscriberId, Receiver<Delivery>) {
        let (tx, rx) = sync_channel(SUBSCRIBER_QUEUE_CAPACITY);
        let id = SubscriberId::new();
        self.subscribers.push(Subscriber {
            id,
            filter,
            state: DeliveryState::Live,
            tx,
        });
        (id, rx)
    }

    /// Deliver `event` to every live subscriber whose filter matches it.
    ///
    /// A desynced subscriber receives nothing, and counts the event when it is
    /// [`EventClass::Critical`]. A live subscriber whose queue is full misses a
    /// [`EventClass::Lossy`] event (logged as a warning) and becomes desynced on
    /// a [`EventClass::Critical`] one.
    ///
    /// A subscriber whose receiver is gone is removed, and its id returned so
    /// the caller can drop whatever it keeps alongside the subscription. The
    /// returned list is empty on every publish that removes nobody.
    pub(crate) fn publish(&mut self, event: &Event) -> Vec<SubscriberId> {
        let class = classify(event);
        let mut removed = Vec::new();
        self.subscribers.retain_mut(|subscriber| {
            if !subscriber.filter.matches(event) {
                return true;
            }
            if let DeliveryState::Desynced { dropped } = &mut subscriber.state {
                if class == EventClass::Critical {
                    *dropped += 1;
                }
                return true;
            }
            match subscriber.tx.try_send(Delivery::Event(event.clone())) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    match class {
                        EventClass::Lossy => tracing::warn!(
                            subscriber = %subscriber.id,
                            event = event.name(),
                            "event dropped; subscriber queue full"
                        ),
                        EventClass::Critical => {
                            subscriber.state = DeliveryState::Desynced { dropped: 1 };
                            tracing::warn!(
                                subscriber = %subscriber.id,
                                event = event.name(),
                                "critical event dropped; subscriber desynced, awaiting snapshot"
                            );
                        }
                    }
                    true
                }
                Err(TrySendError::Disconnected(_)) => {
                    removed.push(subscriber.id);
                    false
                }
            }
        });
        removed
    }

    /// Whether any subscriber is desynced and awaiting a snapshot.
    pub(crate) fn has_desynced(&self) -> bool {
        self.subscribers
            .iter()
            .any(|subscriber| subscriber.state != DeliveryState::Live)
    }

    /// The ids of every subscriber desynced and awaiting a snapshot, in
    /// subscription order.
    pub(crate) fn desynced(&self) -> Vec<SubscriberId> {
        self.subscribers
            .iter()
            .filter(|subscriber| subscriber.state != DeliveryState::Live)
            .map(|subscriber| subscriber.id)
            .collect()
    }

    /// Whether `id` is still registered.
    pub(crate) fn contains(&self, id: SubscriberId) -> bool {
        self.subscribers
            .iter()
            .any(|subscriber| subscriber.id == id)
    }

    /// Drop `id`'s subscription. Does nothing when `id` is not registered.
    pub(crate) fn unsubscribe(&mut self, id: SubscriberId) {
        self.subscribers.retain(|subscriber| subscriber.id != id);
    }

    /// Put `snapshot` on desynced subscriber `id`'s queue and return it to live
    /// delivery, reporting how many critical events it missed.
    ///
    /// Returns `true` once the snapshot is queued. Returns `false` when `id` is
    /// unknown, when it is already live, or when its queue is still full — the
    /// caller retries a full queue on a later pass. A subscriber whose receiver
    /// is gone is removed.
    pub(crate) fn try_resync(&mut self, id: SubscriberId, snapshot: Box<RenderSnapshot>) -> bool {
        let Some(index) = self
            .subscribers
            .iter()
            .position(|subscriber| subscriber.id == id)
        else {
            return false;
        };
        let subscriber = &mut self.subscribers[index];
        let DeliveryState::Desynced { dropped } = subscriber.state else {
            return false;
        };
        let lagged = SubscriberLagged {
            subscriber_id: id,
            dropped_count: dropped,
            event_class: EventClass::Critical,
        };
        match subscriber
            .tx
            .try_send(Delivery::Snapshot { snapshot, lagged })
        {
            Ok(()) => {
                subscriber.state = DeliveryState::Live;
                tracing::info!(
                    subscriber = %id,
                    dropped,
                    "snapshot queued; subscriber resynced"
                );
                true
            }
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => {
                self.subscribers.remove(index);
                false
            }
        }
    }

    /// Put `snapshot` on live subscriber `id`'s queue as the frame its client
    /// draws.
    ///
    /// Returns `true` once the frame is queued. Returns `false` when `id` is
    /// unknown, when it is desynced — a paused subscriber takes its resync
    /// snapshot first — or when its queue is full. A subscriber whose receiver
    /// is gone is removed.
    pub(crate) fn try_send_frame(
        &mut self,
        id: SubscriberId,
        snapshot: Box<RenderSnapshot>,
    ) -> bool {
        let Some(index) = self
            .subscribers
            .iter()
            .position(|subscriber| subscriber.id == id)
        else {
            return false;
        };
        let subscriber = &mut self.subscribers[index];
        if subscriber.state != DeliveryState::Live {
            return false;
        }
        match subscriber.tx.try_send(Delivery::Frame(snapshot)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => {
                self.subscribers.remove(index);
                false
            }
        }
    }

    /// Put the `answers` to mouse round `request_id` on live subscriber `id`'s
    /// queue.
    ///
    /// Returns `true` once the answers are queued. Returns `false` when `id` is
    /// unknown, when it is desynced — a paused subscriber takes its resync
    /// snapshot first — or when its queue is full. A full queue marks the
    /// subscriber desynced: a lost answer leaves the viewer's drag anchor where
    /// it was, and a desynced subscriber is handed a fresh [`RenderSnapshot`] to
    /// resume from.
    /// A subscriber whose receiver is gone is removed.
    pub(crate) fn try_send_answer(
        &mut self,
        id: SubscriberId,
        request_id: u64,
        answers: Vec<MouseAnswer>,
    ) -> bool {
        let Some(index) = self
            .subscribers
            .iter()
            .position(|subscriber| subscriber.id == id)
        else {
            return false;
        };
        let subscriber = &mut self.subscribers[index];
        if subscriber.state != DeliveryState::Live {
            return false;
        }
        match subscriber.tx.try_send(Delivery::MouseAnswer {
            request_id,
            answers,
        }) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                subscriber.state = DeliveryState::Desynced { dropped: 1 };
                tracing::warn!(
                    subscriber = %id,
                    request_id,
                    "mouse answer dropped; subscriber desynced, awaiting snapshot"
                );
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                self.subscribers.remove(index);
                false
            }
        }
    }

    /// Put `bytes` for the terminal live subscriber `id`'s client runs in on
    /// that subscriber's queue.
    ///
    /// Returns `true` once the bytes are queued. Returns `false` when `id` is
    /// unknown, when it is desynced — a paused subscriber takes its resync
    /// snapshot first — or when its queue is full. A full queue marks the
    /// subscriber desynced: dropped bytes leave a clipboard copy unwritten, and
    /// a desynced subscriber is handed a fresh [`RenderSnapshot`] to resume
    /// from.
    /// A subscriber whose receiver is gone is removed.
    pub(crate) fn try_send_host_write(&mut self, id: SubscriberId, bytes: Vec<u8>) -> bool {
        let Some(index) = self
            .subscribers
            .iter()
            .position(|subscriber| subscriber.id == id)
        else {
            return false;
        };
        let subscriber = &mut self.subscribers[index];
        if subscriber.state != DeliveryState::Live {
            return false;
        }
        match subscriber.tx.try_send(Delivery::HostWrite(bytes)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                subscriber.state = DeliveryState::Desynced { dropped: 1 };
                tracing::warn!(
                    subscriber = %id,
                    "host write dropped; subscriber desynced, awaiting snapshot"
                );
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                self.subscribers.remove(index);
                false
            }
        }
    }

    /// How many subscribers are registered. Counts subscribers whose receiver
    /// is already gone but whose removal awaits the next publish.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }
}

/// The frame an attached client is sent for one item off its queue, or `None`
/// when the item says nothing about the session's structure.
///
/// A [`Delivery::Frame`] becomes [`SessionEvent::Painted`] carrying the whole
/// picture in koshi-ipc's wire spellings.
///
/// A [`Delivery::Snapshot`] becomes [`SessionEvent::Resync`] carrying the count
/// of missed events. Its frame does not go on the wire: the client attaches
/// again, and the attach reply carries a fresh structure.
///
/// A [`Delivery::MouseAnswer`] becomes [`SessionEvent::MouseAnswer`] carrying
/// the id of the round it answers and that round's answers.
///
/// A [`Delivery::HostWrite`] becomes [`SessionEvent::HostWrite`] carrying the
/// bytes the client writes to its own terminal.
#[must_use]
pub fn wire_event(delivery: &Delivery) -> Option<SessionEvent> {
    match delivery {
        Delivery::Event(event) => match event {
            Event::PaneCreated(payload) => Some(SessionEvent::PaneCreated {
                pane_id: payload.pane_id,
                tab_id: payload.tab_id,
            }),
            Event::PaneProcessExited(payload) => Some(SessionEvent::PaneProcessExited {
                pane_id: payload.pane_id,
                exit_code: payload.exit_code,
            }),
            Event::PaneClosing(payload) => Some(SessionEvent::PaneClosing {
                pane_id: payload.pane_id,
            }),
            Event::PaneRemoved(payload) => Some(SessionEvent::PaneRemoved {
                pane_id: payload.pane_id,
                tab_id: payload.tab_id,
            }),
            Event::PaneFocused(payload) => Some(SessionEvent::PaneFocused {
                client_id: payload.client_id,
                tab_id: payload.tab_id,
                pane_id: payload.pane_id,
                prior_pane: payload.prior_pane,
            }),
            Event::LayoutChanged(payload) => Some(SessionEvent::LayoutChanged {
                tab_id: payload.tab_id,
            }),
            Event::TabCreated(payload) => Some(SessionEvent::TabCreated {
                tab_id: payload.tab_id,
            }),
            Event::TabClosed(payload) => Some(SessionEvent::TabClosed {
                tab_id: payload.tab_id,
            }),
            Event::TabFocused(payload) => Some(SessionEvent::TabFocused {
                client_id: payload.client_id,
                tab_id: payload.tab_id,
                prior_tab: payload.prior_tab,
            }),
            Event::TabMoved(payload) => Some(SessionEvent::TabMoved {
                tab_id: payload.tab_id,
                old_index: payload.old_index,
                new_index: payload.new_index,
            }),
            Event::Quit => Some(SessionEvent::Quit),
            // Pane content, PTY sizing, input, mouse, selection, plugin and
            // per-client view events carry no structure change.
            _ => None,
        },
        Delivery::Frame(snapshot) => Some(SessionEvent::Painted {
            frame: Box::new(wire_frame(snapshot)),
        }),
        Delivery::Snapshot { lagged, .. } => Some(SessionEvent::Resync {
            dropped_count: lagged.dropped_count,
        }),
        Delivery::MouseAnswer {
            request_id,
            answers,
        } => Some(SessionEvent::MouseAnswer {
            request_id: *request_id,
            answers: answers.clone(),
        }),
        Delivery::HostWrite(bytes) => Some(SessionEvent::HostWrite {
            bytes: bytes.clone(),
        }),
    }
}

#[cfg(test)]
mod tests;
