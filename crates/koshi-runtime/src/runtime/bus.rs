//! Event fan-out: the bounded per-subscriber delivery hub.
//!
//! `EventBus::subscribe` registers a subscriber and hands back its id plus
//! the receiving end of that subscriber's own bounded queue.
//! `EventBus::publish` clones each event into every queue whose filter
//! matches, `EventBus::try_send_frame` puts the frame the session composed
//! for one subscriber's client on that subscriber's own queue,
//! `EventBus::try_send_answer` puts one round of mouse answers on it,
//! `EventBus::try_send_host_write` puts bytes aimed at that subscriber's own
//! terminal on it, and `EventBus::try_send_switch` puts the session that
//! subscriber's client moves to on it. Delivery
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
//! backlog it already had, then the snapshot, then live events again. An
//! answer, host write, or switch that does not fit marks the subscriber
//! desynced the same way; a frame that does not fit is dropped and the
//! subscriber stays live.
//!
//! [`Event::Quit`] and [`Event::Restarting`] are the exception: each is the
//! stream's last frame, so it reaches a desynced subscriber as well as a live
//! one. A client reading either one leaves this socket; after a restart it
//! joins the session's new socket. Publishing either one also raises the
//! [`EndingNotice`] this bus shares with every attached client's writing
//! thread, which is the path that last frame takes to a client whose queue is
//! full.
//!
//! A subscriber in another process works in the wire spellings from
//! `koshi-ipc` instead. The two conversions between them live here: the
//! [`From`] impl on [`EventFilter`] reads the filter an attaching client sent,
//! and [`wire_event`] turns one queue item into the frame that client is sent.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;

use koshi_core::event::{classify_event, Event, EventClass, SubscriberLagged};
use koshi_core::ids::{SessionId, SubscriberId};
use koshi_core::mouse::MouseAnswer;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::protocol::EventFilterSpec;
use koshi_renderer::snapshot::{Delivery, RenderSnapshot};

use crate::runtime::event::{EndingNotice, SessionEnding};
use crate::runtime::frame::wire_frame;

/// How many undelivered items one subscriber's queue holds. Anything put on a
/// full queue is dropped for that subscriber.
pub(crate) const SUBSCRIBER_QUEUE_CAPACITY: usize = 1024;

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
    fn is_event_allowed(self, _event: &Event) -> bool {
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
    /// snapshot lands, apart from the event that ends the stream.
    Desynced {
        /// How many deliveries the subscriber has missed: the one that paused
        /// it, plus every [`EventClass::Critical`] event published since.
        dropped_event_count: u64,
    },
}

/// What putting one delivery on a subscriber's queue did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueDeliveryStatus {
    /// The delivery is on the queue.
    Sent,
    /// The queue was full: the delivery is gone and the subscriber is now
    /// desynced, awaiting a snapshot.
    Dropped,
    /// Nothing was queued and nothing is owed — the subscriber is unknown,
    /// already paused, or its receiver was gone and it has been removed.
    Skipped,
}

/// One registered subscriber: its id, its filter, its delivery state, and the
/// sending end of its queue.
#[derive(Debug)]
struct Subscriber {
    /// Stable subscriber ID assigned at subscription, named in log lines about this
    /// subscriber.
    subscriber_id: SubscriberId,
    /// Which events this subscriber receives.
    filter: EventFilter,
    /// Whether this subscriber is receiving events or paused awaiting a
    /// snapshot.
    delivery_state: DeliveryState,
    /// Sending end of the subscriber's bounded queue; the receiver lives with
    /// the subscriber.
    delivery_sender: SyncSender<Delivery>,
}

/// Event fan-out hub: every published event is delivered to each live
/// subscriber whose filter matches, over that subscriber's own bounded queue.
#[derive(Debug, Default)]
pub(crate) struct EventBus {
    /// Live subscribers, in subscription order.
    subscribers: Vec<Subscriber>,
    /// Shared with every attached client's writing thread: raised by
    /// [`publish`](Self::publish) with the last frame it delivers.
    ending_notice: Arc<EndingNotice>,
}

impl EventBus {
    /// A bus with no subscribers, over a session that is still serving.
    #[must_use]
    pub(crate) fn new() -> Self {
        EventBus {
            subscribers: Vec::new(),
            ending_notice: Arc::new(EndingNotice::default()),
        }
    }

    /// Borrow what this bus and every attached client's writing thread share
    /// about the session's last frame.
    pub(crate) fn ending_notice(&self) -> &Arc<EndingNotice> {
        &self.ending_notice
    }

    /// Register a subscriber for the events `filter` selects and hand back its
    /// id plus the receiving end of its queue. The subscriber starts live.
    /// Dropping the receiver ends the subscription; the bus removes it on the
    /// next publish or delivery to it.
    pub(crate) fn subscribe(&mut self, filter: EventFilter) -> (SubscriberId, Receiver<Delivery>) {
        let (delivery_sender, delivery_receiver) = sync_channel(SUBSCRIBER_QUEUE_CAPACITY);
        let subscriber_id = SubscriberId::new();
        self.subscribers.push(Subscriber {
            subscriber_id,
            filter,
            delivery_state: DeliveryState::Live,
            delivery_sender,
        });
        (subscriber_id, delivery_receiver)
    }

    /// Deliver `event` to every live subscriber whose filter matches it, and
    /// to every desynced one when `event` ends the stream. An event that ends
    /// the stream raises the [`EndingNotice`] first, so a subscriber whose
    /// queue has no room for it is told over the notice instead.
    ///
    /// A desynced subscriber receives nothing else, and counts what it misses
    /// when that is [`EventClass::Critical`]. A live subscriber whose queue is
    /// full misses a [`EventClass::Lossy`] event (logged as a warning) and
    /// becomes desynced on a [`EventClass::Critical`] one. A desynced
    /// subscriber whose queue is full misses the last frame too, and counts it.
    ///
    /// A subscriber whose receiver is gone is removed, and its id returned so
    /// the caller can drop whatever it keeps alongside the subscription. The
    /// returned list is empty on every publish that removes nobody.
    pub(crate) fn publish(&mut self, event: &Event) -> Vec<SubscriberId> {
        let event_class = classify_event(event);
        // The stream's last frame, delivered whatever state the subscriber is
        // in: the client reading it leaves this socket. The notice carries it
        // to a client whose queue has no room left for it.
        let session_ending = match event {
            Event::Quit(_) => Some(SessionEnding::Quit),
            Event::Restarting => Some(SessionEnding::Restarting),
            _ => None,
        };
        if let Some(session_ending) = session_ending {
            self.ending_notice.raise_session_ending(session_ending);
        }
        let ends_the_stream = session_ending.is_some();
        let mut removed_subscriber_ids = Vec::new();
        self.subscribers.retain_mut(|subscriber| {
            if !subscriber.filter.is_event_allowed(event) {
                return true;
            }
            if let DeliveryState::Desynced {
                dropped_event_count,
            } = &mut subscriber.delivery_state
            {
                if !ends_the_stream {
                    if event_class == EventClass::Critical {
                        *dropped_event_count += 1;
                    }
                    return true;
                }
            }
            match subscriber
                .delivery_sender
                .try_send(Delivery::Event(event.clone()))
            {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    match (&mut subscriber.delivery_state, event_class) {
                        (
                            DeliveryState::Desynced {
                                dropped_event_count,
                            },
                            _,
                        ) => {
                            *dropped_event_count += 1;
                            tracing::warn!(
                                subscriber = %subscriber.subscriber_id,
                                event = event.get_event_name(),
                                "last frame dropped; subscriber queue full"
                            );
                        }
                        (DeliveryState::Live, EventClass::Lossy) => tracing::warn!(
                            subscriber = %subscriber.subscriber_id,
                            event = event.get_event_name(),
                            "event dropped; subscriber queue full"
                        ),
                        (delivery_state @ DeliveryState::Live, EventClass::Critical) => {
                            *delivery_state = DeliveryState::Desynced {
                                dropped_event_count: 1,
                            };
                            tracing::warn!(
                                subscriber = %subscriber.subscriber_id,
                                event = event.get_event_name(),
                                "critical event dropped; subscriber desynced, awaiting snapshot"
                            );
                        }
                    }
                    true
                }
                Err(TrySendError::Disconnected(_)) => {
                    removed_subscriber_ids.push(subscriber.subscriber_id);
                    false
                }
            }
        });
        removed_subscriber_ids
    }

    /// Whether any subscriber is desynced and awaiting a snapshot.
    pub(crate) fn has_desynced_subscribers(&self) -> bool {
        self.subscribers
            .iter()
            .any(|subscriber| subscriber.delivery_state != DeliveryState::Live)
    }

    /// The ids of every subscriber desynced and awaiting a snapshot, in
    /// subscription order.
    pub(crate) fn list_desynced_subscriber_ids(&self) -> Vec<SubscriberId> {
        self.subscribers
            .iter()
            .filter(|subscriber| subscriber.delivery_state != DeliveryState::Live)
            .map(|subscriber| subscriber.subscriber_id)
            .collect()
    }

    /// Whether `subscriber_id` is still registered.
    pub(crate) fn has_subscriber(&self, subscriber_id: SubscriberId) -> bool {
        self.find_subscriber_index(subscriber_id).is_some()
    }

    /// Where `subscriber_id` sits in the subscription-ordered list, or `None` when `subscriber_id` is
    /// not registered.
    fn find_subscriber_index(&self, subscriber_id: SubscriberId) -> Option<usize> {
        self.subscribers
            .iter()
            .position(|subscriber| subscriber.subscriber_id == subscriber_id)
    }

    /// Drop `subscriber_id`'s subscription. Does nothing when `subscriber_id` is not registered.
    pub(crate) fn unsubscribe(&mut self, subscriber_id: SubscriberId) {
        self.subscribers
            .retain(|subscriber| subscriber.subscriber_id != subscriber_id);
    }

    /// Put `snapshot` on desynced subscriber `subscriber_id`'s queue and return it to live
    /// delivery, reporting how many deliveries it missed.
    ///
    /// Returns `true` once the snapshot is queued. Returns `false` when `subscriber_id` is
    /// unknown, when it is already live, or when its queue is still full — the
    /// caller retries a full queue on a subsequent pass. A subscriber whose receiver
    /// is gone is removed.
    pub(crate) fn try_resync(
        &mut self,
        subscriber_id: SubscriberId,
        render_snapshot: Box<RenderSnapshot>,
    ) -> bool {
        let Some(subscriber_index) = self.find_subscriber_index(subscriber_id) else {
            return false;
        };
        let subscriber = &mut self.subscribers[subscriber_index];
        let DeliveryState::Desynced {
            dropped_event_count,
        } = subscriber.delivery_state
        else {
            return false;
        };
        let lag_report = SubscriberLagged {
            subscriber_id,
            dropped_event_count,
            event_class: EventClass::Critical,
        };
        match subscriber.delivery_sender.try_send(Delivery::Snapshot {
            render_snapshot,
            lag_report,
        }) {
            Ok(()) => {
                subscriber.delivery_state = DeliveryState::Live;
                tracing::info!(
                    subscriber = %subscriber_id,
                    dropped_event_count,
                    "snapshot queued; subscriber resynced"
                );
                true
            }
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => {
                self.subscribers.remove(subscriber_index);
                false
            }
        }
    }

    /// Put `snapshot` on live subscriber `subscriber_id`'s queue as the frame its client
    /// draws.
    ///
    /// Returns `true` once the frame is queued. Returns `false` when `subscriber_id` is
    /// unknown, when it is desynced — a paused subscriber takes its resync
    /// snapshot first — or when its queue is full. A subscriber whose receiver
    /// is gone is removed.
    pub(crate) fn try_send_frame(
        &mut self,
        subscriber_id: SubscriberId,
        render_snapshot: Box<RenderSnapshot>,
    ) -> bool {
        let Some(subscriber_index) = self.find_subscriber_index(subscriber_id) else {
            return false;
        };
        let subscriber = &mut self.subscribers[subscriber_index];
        if subscriber.delivery_state != DeliveryState::Live {
            return false;
        }
        match subscriber
            .delivery_sender
            .try_send(Delivery::Frame(render_snapshot))
        {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => {
                self.subscribers.remove(subscriber_index);
                false
            }
        }
    }

    /// Put `delivery` on live subscriber `subscriber_id`'s queue, and report what that did.
    ///
    /// A full queue drops the delivery and marks the subscriber desynced, so it
    /// is handed a fresh [`RenderSnapshot`] to resume from. A subscriber that is
    /// unknown or already paused takes nothing, and one whose receiver is gone
    /// is removed.
    fn try_send_delivery(
        &mut self,
        subscriber_id: SubscriberId,
        delivery: Delivery,
    ) -> QueueDeliveryStatus {
        let Some(subscriber_index) = self.find_subscriber_index(subscriber_id) else {
            return QueueDeliveryStatus::Skipped;
        };
        let subscriber = &mut self.subscribers[subscriber_index];
        if subscriber.delivery_state != DeliveryState::Live {
            return QueueDeliveryStatus::Skipped;
        }
        match subscriber.delivery_sender.try_send(delivery) {
            Ok(()) => QueueDeliveryStatus::Sent,
            Err(TrySendError::Full(_)) => {
                subscriber.delivery_state = DeliveryState::Desynced {
                    dropped_event_count: 1,
                };
                QueueDeliveryStatus::Dropped
            }
            Err(TrySendError::Disconnected(_)) => {
                self.subscribers.remove(subscriber_index);
                QueueDeliveryStatus::Skipped
            }
        }
    }

    /// Put the `mouse_answers` to mouse round `request_id` on live subscriber
    /// `subscriber_id`'s
    /// queue.
    ///
    /// Returns `true` once the answers are queued, `false` otherwise
    /// ([`Self::try_send_delivery`]). A lost answer leaves the viewer's drag
    /// anchor where it was.
    pub(crate) fn try_send_answer(
        &mut self,
        subscriber_id: SubscriberId,
        request_id: u64,
        mouse_answers: Vec<MouseAnswer>,
    ) -> bool {
        match self.try_send_delivery(
            subscriber_id,
            Delivery::MouseAnswer {
                request_id,
                mouse_answers,
            },
        ) {
            QueueDeliveryStatus::Sent => true,
            QueueDeliveryStatus::Dropped => {
                tracing::warn!(
                    subscriber = %subscriber_id,
                    request_id,
                    "mouse answer dropped; subscriber desynced, awaiting snapshot"
                );
                false
            }
            QueueDeliveryStatus::Skipped => false,
        }
    }

    /// Put `host_write_bytes` for the terminal live subscriber `subscriber_id`'s client runs in on
    /// that subscriber's queue.
    ///
    /// Returns `true` once the bytes are queued, `false` otherwise
    /// ([`Self::try_send_delivery`]). Dropped bytes leave a clipboard copy
    /// unwritten.
    pub(crate) fn try_send_host_write(
        &mut self,
        subscriber_id: SubscriberId,
        host_write_bytes: Vec<u8>,
    ) -> bool {
        match self.try_send_delivery(subscriber_id, Delivery::HostWrite(host_write_bytes)) {
            QueueDeliveryStatus::Sent => true,
            QueueDeliveryStatus::Dropped => {
                tracing::warn!(
                    subscriber = %subscriber_id,
                    "host write dropped; subscriber desynced, awaiting snapshot"
                );
                false
            }
            QueueDeliveryStatus::Skipped => false,
        }
    }

    /// Put the session live subscriber `subscriber_id`'s client moves to on that
    /// subscriber's queue.
    ///
    /// Returns `true` once the switch is queued, `false` otherwise
    /// ([`Self::try_send_delivery`]). A dropped switch leaves the client in this
    /// session.
    pub(crate) fn try_send_switch(
        &mut self,
        subscriber_id: SubscriberId,
        session_id: SessionId,
    ) -> bool {
        match self.try_send_delivery(subscriber_id, Delivery::SwitchTo(session_id)) {
            QueueDeliveryStatus::Sent => true,
            QueueDeliveryStatus::Dropped => {
                tracing::warn!(
                    subscriber = %subscriber_id,
                    session = %session_id,
                    "session switch dropped; subscriber desynced, awaiting snapshot"
                );
                false
            }
            QueueDeliveryStatus::Skipped => false,
        }
    }

    /// How many subscribers are registered. Counts subscribers whose receiver
    /// is already gone but whose removal awaits the next publish or delivery.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }
}

/// The frame an attached client is sent for one item off its queue, or `None`
/// when no [`SessionEvent`] spells that item.
///
/// A [`Delivery::Frame`] becomes [`SessionEvent::Painted`] carrying the text
/// cells and image placements in koshi-ipc's wire spellings. A client that can
/// paint images receives new RGBA records from its connection writer after
/// this conversion.
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
///
/// A [`Delivery::SwitchTo`] becomes [`SessionEvent::SwitchTo`] carrying the id
/// of the session the client attaches to next.
///
/// Every [`Event`] variant with no [`SessionEvent`] spelling is named here and
/// returns `None`. The match takes no wildcard arm, so a new variant is a
/// compile error until this function says what the wire does with it.
#[must_use]
#[deny(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]
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
                signal: payload.signal,
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
                previous_pane_id: payload.previous_pane_id,
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
                previous_tab_id: payload.previous_tab_id,
            }),
            Event::TabMoved(payload) => Some(SessionEvent::TabMoved {
                tab_id: payload.tab_id,
                previous_tab_index: payload.previous_tab_index,
                new_tab_index: payload.new_tab_index,
            }),
            Event::Quit(_) => Some(SessionEvent::Quit),
            Event::Restarting => Some(SessionEvent::Restarting),

            // PTY size and content damage: the client redraws from the next
            // `Painted` frame.
            Event::PtyResized(_) | Event::PaneOutputUpdated(_) => None,
            // Visibility: the `Painted` frame already shows which panes have
            // area.
            Event::PaneSuppressed(_)
            | Event::PaneResumed(_)
            | Event::TerminalTooSmallEntered(_)
            | Event::TerminalTooSmallExited(_) => None,
            Event::ConfigReloaded(_) => None,
            // Per-client input state: the client holds its own mode, its own
            // mouse-select mode, and the binding it matched.
            Event::InputModeChanged(_)
            | Event::MouseSelectChanged(_)
            | Event::KeybindingMatched(_) => None,
            // Typed input: these payloads carry pane text.
            Event::PaneTyped(_) | Event::PaneEnterPressed(_) => None,
            // Mouse input: the client produced it and sends it the other way.
            Event::MousePressed(_)
            | Event::MouseReleased(_)
            | Event::MouseDragged(_)
            | Event::MouseScrolled(_)
            | Event::PaneMouseForwarded(_)
            | Event::PluginMouseInput(_) => None,
            // A shell's OSC 133 prompt reports.
            Event::PaneCommandStarted(_) | Event::PaneCommandFinished(_) => None,
            // Drop counters and rejections: a subscriber that misses a
            // critical event is told through `SessionEvent::Resync`.
            Event::PaneScrollbackTruncated(_)
            | Event::SubscriberLagged(_)
            | Event::CommandRejected(_) => None,
            // Selection and copy: both are client-local.
            Event::SelectionChanged(_) | Event::Copied(_) => None,
            Event::Plugin(_) => None,
        },
        Delivery::Frame(render_snapshot) => Some(SessionEvent::Painted {
            frame: Box::new(wire_frame(render_snapshot)),
        }),
        Delivery::Snapshot { lag_report, .. } => Some(SessionEvent::Resync {
            dropped_event_count: lag_report.dropped_event_count,
        }),
        Delivery::MouseAnswer {
            request_id,
            mouse_answers,
        } => Some(SessionEvent::MouseAnswer {
            request_id: *request_id,
            mouse_answers: mouse_answers.clone(),
        }),
        Delivery::HostWrite(host_write_bytes) => Some(SessionEvent::HostWrite {
            host_output_bytes: host_write_bytes.clone(),
        }),
        Delivery::SwitchTo(session_id) => Some(SessionEvent::SwitchTo {
            session_id: *session_id,
        }),
    }
}

#[cfg(test)]
mod tests;
