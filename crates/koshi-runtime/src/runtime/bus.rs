//! Event fan-out: the bounded per-subscriber delivery hub.
//!
//! `EventBus::subscribe` registers a subscriber and hands back its id plus
//! the receiving end of that subscriber's own bounded queue.
//! `EventBus::publish` clones each event into every subscriber's queue, `EventBus::try_send_frame` puts the frame the session composed
//! for one subscriber's client on that subscriber's own queue,
//! `EventBus::try_send_mouse_answer` puts one round of mouse answers on it,
//! `EventBus::try_send_host_write` puts bytes aimed at that subscriber's own
//! terminal on it, and `EventBus::try_send_switch` puts the session that
//! subscriber's client moves to on it. Delivery
//! never blocks the dispatcher: a subscriber whose receiver was dropped is
//! removed on the next publish.
//!
//! An event that does not fit a subscriber's full queue marks the subscriber
//! desynced: it receives nothing at all while it counts the events it misses,
//! until `EventBus::try_resync` puts a fresh
//! [`RenderSnapshot`] on its queue and returns it to live delivery. The
//! snapshot rides the same queue as events, so the subscriber reads the
//! backlog it already had, then the snapshot, then live events again. A mouse
//! answer, host write, session switch, placement preview, placement refusal,
//! or placement command rejection that does not fit marks the subscriber
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
//! `koshi-ipc` instead. [`build_wire_event`] turns one queue item into the
//! frame that client is sent, and [`build_wire_host_write_events`] turns one
//! host write into the frames that client is sent.
//!
//! `EventBus::has_host_write_in_flight` says whether the last host write put
//! on a subscriber's queue is still on that queue or with the thread writing
//! it. That thread drops the delivery once it wrote the bytes.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Weak};

use koshi_core::event::{Event, SubscriberLagged};
use koshi_core::ids::{CommandId, SessionId, SubscriberId};
use koshi_core::mouse::MouseAnswer;
use koshi_ipc::event::{SessionEvent, MAX_HOST_WRITE_BYTE_COUNT};
use koshi_renderer::snapshot::{
    Delivery, PlacementSnapshot, PlacementSnapshotError, PlacementSnapshotErrorCode, RenderSnapshot,
};

use crate::runtime::event::{EndingNotice, SessionEnding};
use crate::runtime::frame::build_wire_frame;

/// How many undelivered items one subscriber's queue holds. Anything put on a
/// full queue is dropped for that subscriber.
pub(crate) const SUBSCRIBER_QUEUE_CAPACITY: usize = 1024;

/// Whether a subscriber is receiving events, or paused awaiting a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryState {
    /// Events are delivered as they are published.
    Live,
    /// An event did not fit the queue; nothing is delivered until a
    /// snapshot lands, apart from the event that ends the stream.
    Desynced {
        /// How many deliveries the subscriber has missed: the one that paused
        /// it, plus every event published since.
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

/// One registered subscriber: its id, its delivery state, the sending end of
/// its queue, and the host-write bytes that wait for that queue.
#[derive(Debug)]
struct Subscriber {
    /// Stable subscriber ID assigned at subscription, named in log lines about this
    /// subscriber.
    subscriber_id: SubscriberId,
    /// Whether this subscriber is receiving events or paused awaiting a
    /// snapshot.
    delivery_state: DeliveryState,
    /// Sending end of the subscriber's bounded queue; the receiver lives with
    /// the subscriber.
    delivery_sender: SyncSender<Delivery>,
    /// Bytes for the terminal the subscriber's client runs in that wait to go
    /// on the queue as one host write: whole escape sequences, in the order
    /// they were queued.
    pending_host_write_bytes: Vec<u8>,
    /// The marker of the last host write put on this subscriber's queue. The
    /// host write is in flight while the marker exists, and the marker exists
    /// while the delivery that holds it is on the queue or with the thread
    /// writing it.
    host_write_in_flight: Weak<()>,
}

/// Event fan-out hub: every published event is delivered to each live
/// subscriber, over that subscriber's own bounded queue.
#[derive(Debug)]
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
    pub(crate) fn get_ending_notice(&self) -> &Arc<EndingNotice> {
        &self.ending_notice
    }

    /// Register a subscriber for every published event and hand back its id
    /// plus the receiving end of its queue. The subscriber starts live.
    /// Dropping the receiver ends the subscription; the bus removes it on the
    /// next publish or delivery to it.
    pub(crate) fn subscribe(&mut self) -> (SubscriberId, Receiver<Delivery>) {
        let (delivery_sender, delivery_receiver) = sync_channel(SUBSCRIBER_QUEUE_CAPACITY);
        let subscriber_id = SubscriberId::new();
        self.subscribers.push(Subscriber {
            subscriber_id,
            delivery_state: DeliveryState::Live,
            delivery_sender,
            pending_host_write_bytes: Vec::new(),
            host_write_in_flight: Weak::new(),
        });
        (subscriber_id, delivery_receiver)
    }

    /// Deliver `event` to every live subscriber, and to every desynced one
    /// when `event` ends the stream. An event that ends the stream first puts
    /// each live subscriber's waiting host-write bytes on its queue, as
    /// [`flush_pending_host_write`](Self::flush_pending_host_write) states,
    /// then raises the [`EndingNotice`], so a subscriber whose queue has no
    /// room for the event is told over the notice instead.
    ///
    /// A desynced subscriber receives nothing else, and counts what it misses.
    /// A live subscriber whose queue is full misses the event and becomes
    /// desynced. A desynced subscriber whose queue is full misses the last
    /// frame too, and counts it.
    ///
    /// A subscriber whose receiver is gone is removed, and its id returned so
    /// the caller can drop whatever it keeps alongside the subscription. The
    /// returned list is empty on every publish that removes nobody.
    pub(crate) fn publish(&mut self, event: &Event) -> Vec<SubscriberId> {
        // The stream's last frame, delivered whatever state the subscriber is
        // in: the client reading it leaves this socket. The notice carries it
        // to a client whose queue has no room left for it.
        let session_ending = match event {
            Event::Quit(_) => Some(SessionEnding::Quit),
            Event::Restarting => Some(SessionEnding::Restarting),
            _ => None,
        };
        if let Some(session_ending) = session_ending {
            let subscriber_ids: Vec<SubscriberId> = self
                .subscribers
                .iter()
                .map(|subscriber| subscriber.subscriber_id)
                .collect();
            for subscriber_id in subscriber_ids {
                let _ = self.flush_pending_host_write(subscriber_id);
            }
            self.ending_notice.raise_session_ending(session_ending);
        }
        let is_stream_ending = session_ending.is_some();
        let mut removed_subscriber_ids = Vec::new();
        self.subscribers.retain_mut(|subscriber| {
            if let DeliveryState::Desynced {
                dropped_event_count,
            } = &mut subscriber.delivery_state
            {
                if !is_stream_ending {
                    *dropped_event_count += 1;
                    return true;
                }
            }
            match subscriber
                .delivery_sender
                .try_send(Delivery::Event(event.clone()))
            {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    match &mut subscriber.delivery_state {
                        DeliveryState::Desynced {
                            dropped_event_count,
                        } => {
                            *dropped_event_count += 1;
                            tracing::warn!(
                                subscriber_id = %subscriber.subscriber_id,
                                event = event.get_event_name(),
                                "last frame dropped; subscriber queue full"
                            );
                        }
                        delivery_state @ DeliveryState::Live => {
                            *delivery_state = DeliveryState::Desynced {
                                dropped_event_count: 1,
                            };
                            tracing::warn!(
                                subscriber_id = %subscriber.subscriber_id,
                                event = event.get_event_name(),
                                "event dropped; subscriber desynced, awaiting snapshot"
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

    /// Whether the last host write put on `subscriber_id`'s queue is still on
    /// that queue or with the thread writing it. `false` for a subscriber that
    /// is not registered.
    pub(crate) fn has_host_write_in_flight(&self, subscriber_id: SubscriberId) -> bool {
        self.find_subscriber_index(subscriber_id)
            .is_some_and(|subscriber_index| {
                self.subscribers[subscriber_index]
                    .host_write_in_flight
                    .strong_count()
                    > 0
            })
    }

    /// Where `subscriber_id` sits in the subscription-ordered list, or `None` when `subscriber_id`
    /// is not registered.
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
    /// delivery, reporting how many deliveries it missed. The host-write bytes
    /// that waited while it was paused then go on its queue after the
    /// snapshot, as [`send_pending_host_write`](Self::send_pending_host_write)
    /// states.
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
        };
        match subscriber.delivery_sender.try_send(Delivery::Snapshot {
            render_snapshot,
            lag_report,
        }) {
            Ok(()) => {
                subscriber.delivery_state = DeliveryState::Live;
                tracing::info!(
                    subscriber_id = %subscriber_id,
                    dropped_event_count,
                    "snapshot queued; subscriber resynced"
                );
                let _ = self.send_pending_host_write(subscriber_id);
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

    /// Put one placement preview on a live subscriber's queue.
    ///
    /// Returns `true` when queued. A full queue marks the subscriber
    /// desynchronized, and a missing or disconnected subscriber returns
    /// `false`.
    pub(crate) fn try_send_placement_snapshot(
        &mut self,
        subscriber_id: SubscriberId,
        request_id: u64,
        placement_snapshot: Box<PlacementSnapshot>,
    ) -> bool {
        match self.try_send_delivery(
            subscriber_id,
            Delivery::PanePlacementSnapshot {
                request_id,
                snapshot: placement_snapshot,
            },
        ) {
            QueueDeliveryStatus::Sent => true,
            QueueDeliveryStatus::Dropped => {
                tracing::warn!(
                    subscriber_id = %subscriber_id,
                    request_id,
                    "placement preview dropped; subscriber desynced, awaiting snapshot"
                );
                false
            }
            QueueDeliveryStatus::Skipped => false,
        }
    }

    /// Put a placement preview refusal on a live subscriber's queue.
    pub(crate) fn try_send_placement_refusal(
        &mut self,
        subscriber_id: SubscriberId,
        request_id: u64,
        placement_error: PlacementSnapshotError,
    ) -> bool {
        match self.try_send_delivery(
            subscriber_id,
            Delivery::PanePlacementRefused {
                request_id,
                refusal: placement_error,
            },
        ) {
            QueueDeliveryStatus::Sent => true,
            QueueDeliveryStatus::Dropped => {
                tracing::warn!(
                    subscriber_id = %subscriber_id,
                    request_id,
                    "placement refusal dropped; subscriber desynced, awaiting snapshot"
                );
                false
            }
            QueueDeliveryStatus::Skipped => false,
        }
    }

    pub(crate) fn try_send_mouse_answer(
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
                    subscriber_id = %subscriber_id,
                    request_id,
                    "mouse answer dropped; subscriber desynced, awaiting snapshot"
                );
                false
            }
            QueueDeliveryStatus::Skipped => false,
        }
    }

    /// Queue `host_write_bytes`, one whole escape sequence, for the terminal
    /// subscriber `subscriber_id`'s client runs in, behind the bytes already
    /// waiting, then send the waiting bytes as
    /// [`send_pending_host_write`](Self::send_pending_host_write) states.
    ///
    /// When bytes wait, and they and `host_write_bytes` together are more than
    /// [`MAX_HOST_WRITE_BYTE_COUNT`], every waiting byte is dropped first and a
    /// warning is logged: the newest sequence always waits. Example: 12 MiB
    /// waiting and a copy of 1 MiB leave the 1 MiB copy alone. A sequence
    /// longer than [`MAX_HOST_WRITE_BYTE_COUNT`] with no bytes waiting drops
    /// nothing and logs nothing.
    ///
    /// Returns `false` when `subscriber_id` is not registered, and drops
    /// `host_write_bytes`. Returns `false` when the send drops the bytes, as
    /// [`send_pending_host_write`](Self::send_pending_host_write) states.
    pub(crate) fn queue_host_write(
        &mut self,
        subscriber_id: SubscriberId,
        host_write_bytes: &[u8],
    ) -> bool {
        let Some(subscriber_index) = self.find_subscriber_index(subscriber_id) else {
            return false;
        };
        let pending_host_write_bytes =
            &mut self.subscribers[subscriber_index].pending_host_write_bytes;
        if !pending_host_write_bytes.is_empty()
            && pending_host_write_bytes.len() + host_write_bytes.len() > MAX_HOST_WRITE_BYTE_COUNT
        {
            tracing::warn!(
                subscriber_id = %subscriber_id,
                dropped_byte_count = pending_host_write_bytes.len(),
                "older host writes dropped; the client reads them slower than they arrive"
            );
            pending_host_write_bytes.clear();
        }
        pending_host_write_bytes.extend_from_slice(host_write_bytes);
        self.send_pending_host_write(subscriber_id)
    }

    /// Put the bytes waiting for subscriber `subscriber_id`'s client's terminal
    /// on its queue as one host write, unless the last host write put on that
    /// queue is still in flight
    /// ([`has_host_write_in_flight`](Self::has_host_write_in_flight)). The rest
    /// is as [`flush_pending_host_write`](Self::flush_pending_host_write)
    /// states.
    ///
    /// Returns `false` when the bytes were dropped, and `true` when they are
    /// on the queue, wait there, or no bytes wait.
    pub(crate) fn send_pending_host_write(&mut self, subscriber_id: SubscriberId) -> bool {
        if self.has_host_write_in_flight(subscriber_id) {
            return true;
        }
        self.flush_pending_host_write(subscriber_id)
    }

    /// Put the bytes waiting for subscriber `subscriber_id`'s client's terminal
    /// on its queue as one host write, whether or not an earlier host write is
    /// in flight. Sends nothing when no bytes wait, and when the subscriber is
    /// unknown or paused: a paused subscriber keeps its bytes waiting. Bytes
    /// that do not fit the queue are dropped as
    /// [`try_send_host_write`](Self::try_send_host_write) states.
    ///
    /// Returns `false` when the bytes were dropped: the queue was full, or its
    /// receiver is gone. Returns `true` in every other case.
    fn flush_pending_host_write(&mut self, subscriber_id: SubscriberId) -> bool {
        let Some(subscriber_index) = self.find_subscriber_index(subscriber_id) else {
            return true;
        };
        let subscriber = &mut self.subscribers[subscriber_index];
        if subscriber.delivery_state != DeliveryState::Live
            || subscriber.pending_host_write_bytes.is_empty()
        {
            return true;
        }
        let host_write_bytes = std::mem::take(&mut subscriber.pending_host_write_bytes);
        self.try_send_host_write(subscriber_id, host_write_bytes)
    }

    /// Put `host_write_bytes` for the terminal live subscriber `subscriber_id`'s client runs in on
    /// that subscriber's queue, with a fresh in-flight marker
    /// ([`has_host_write_in_flight`](Self::has_host_write_in_flight)).
    ///
    /// Returns `true` once the bytes are queued, `false` otherwise
    /// ([`Self::try_send_delivery`]). Dropped bytes leave a clipboard copy
    /// unwritten, and leave no host write in flight.
    fn try_send_host_write(
        &mut self,
        subscriber_id: SubscriberId,
        host_write_bytes: Vec<u8>,
    ) -> bool {
        let in_flight_marker = Arc::new(());
        if let Some(subscriber_index) = self.find_subscriber_index(subscriber_id) {
            self.subscribers[subscriber_index].host_write_in_flight =
                Arc::downgrade(&in_flight_marker);
        }
        match self.try_send_delivery(
            subscriber_id,
            Delivery::HostWrite {
                host_write_bytes,
                in_flight_marker,
            },
        ) {
            QueueDeliveryStatus::Sent => true,
            QueueDeliveryStatus::Dropped => {
                tracing::warn!(
                    subscriber_id = %subscriber_id,
                    "host write dropped; subscriber desynced, awaiting snapshot"
                );
                false
            }
            QueueDeliveryStatus::Skipped => false,
        }
    }

    /// Put the session live subscriber `subscriber_id`'s client moves to on that
    /// subscriber's queue, after the host-write bytes that wait for it, as
    /// [`flush_pending_host_write`](Self::flush_pending_host_write) states.
    ///
    /// Returns `true` once the switch is queued, `false` otherwise
    /// ([`Self::try_send_delivery`]). A dropped switch leaves the client in this
    /// session.
    pub(crate) fn try_send_switch(
        &mut self,
        subscriber_id: SubscriberId,
        session_id: SessionId,
    ) -> bool {
        let _ = self.flush_pending_host_write(subscriber_id);
        match self.try_send_delivery(subscriber_id, Delivery::SwitchTo(session_id)) {
            QueueDeliveryStatus::Sent => true,
            QueueDeliveryStatus::Dropped => {
                tracing::warn!(
                    subscriber_id = %subscriber_id,
                    session_id = %session_id,
                    "session switch dropped; subscriber desynced, awaiting snapshot"
                );
                false
            }
            QueueDeliveryStatus::Skipped => false,
        }
    }

    /// Put a rejected pane placement command on the live subscriber's queue.
    ///
    /// Returns `true` when the rejection is queued and `false` when the
    /// subscriber is missing, disconnected, or desynchronized.
    pub(crate) fn try_send_placement_command_rejection(
        &mut self,
        subscriber_id: SubscriberId,
        command_id: CommandId,
    ) -> bool {
        match self.try_send_delivery(
            subscriber_id,
            Delivery::PlacementCommandRejected(command_id),
        ) {
            QueueDeliveryStatus::Sent => true,
            QueueDeliveryStatus::Dropped => {
                tracing::warn!(
                    subscriber_id = %subscriber_id,
                    %command_id,
                    "placement command rejection dropped; subscriber desynced, awaiting snapshot"
                );
                false
            }
            QueueDeliveryStatus::Skipped => false,
        }
    }

    /// Put the answer to one ping on live subscriber `subscriber_id`'s queue.
    ///
    /// A full queue drops the answer and marks the subscriber desynced, as
    /// [`Self::try_send_delivery`] states. A subscriber that is unknown,
    /// desynced, or disconnected takes nothing.
    pub(crate) fn try_send_pong(&mut self, subscriber_id: SubscriberId) {
        let _ = self.try_send_delivery(subscriber_id, Delivery::Pong);
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
/// A [`Delivery::HostWrite`] gives `None`: its bytes go out as the frames
/// [`build_wire_host_write_events`] lists.
///
/// A [`Delivery::SwitchTo`] becomes [`SessionEvent::SwitchTo`] carrying the id
/// of the session the client attaches to next.
///
/// A [`Delivery::PlacementCommandRejected`] becomes
/// [`SessionEvent::PlacementCommandRejected`] carrying the rejected command id.
///
/// A [`Delivery::Pong`] becomes [`SessionEvent::Pong`].
///
/// Every [`Event`] variant with no [`SessionEvent`] spelling is named here and
/// returns `None`. The match takes no wildcard arm, so a new variant is a
/// compile error until this function says what the wire does with it.
#[must_use]
#[deny(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]
pub fn build_wire_event(delivery: &Delivery) -> Option<SessionEvent> {
    match delivery {
        Delivery::Event(event) => match event {
            Event::PaneCreated(pane_created) => Some(SessionEvent::PaneCreated {
                pane_id: pane_created.pane_id,
                tab_id: pane_created.tab_id,
            }),
            Event::PaneProcessExited(pane_process_exit) => Some(SessionEvent::PaneProcessExited {
                pane_id: pane_process_exit.pane_id,
                exit_code: pane_process_exit.exit_code,
                signal: pane_process_exit.signal,
            }),
            Event::PaneClosing(pane_closing) => Some(SessionEvent::PaneClosing {
                pane_id: pane_closing.pane_id,
            }),
            Event::PaneRemoved(pane_removed) => Some(SessionEvent::PaneRemoved {
                pane_id: pane_removed.pane_id,
                tab_id: pane_removed.tab_id,
            }),
            Event::PaneFocused(pane_focused) => Some(SessionEvent::PaneFocused {
                client_id: pane_focused.client_id,
                tab_id: pane_focused.tab_id,
                pane_id: pane_focused.pane_id,
                previous_pane_id: pane_focused.previous_pane_id,
            }),
            Event::LayoutChanged(layout_changed) => Some(SessionEvent::LayoutChanged {
                tab_id: layout_changed.tab_id,
            }),
            Event::PanePlacementCommitted(pane_placement_committed) => {
                Some(SessionEvent::PanePlacementCommitted {
                    command_id: pane_placement_committed.command_id,
                    source_pane_id: pane_placement_committed.source_pane_id,
                    source_tab_id: pane_placement_committed.source_tab_id,
                    destination_tab_id: pane_placement_committed.destination_tab_id,
                    placement_target: pane_placement_committed.placement_target.clone(),
                })
            }
            Event::TabCreated(tab_created) => Some(SessionEvent::TabCreated {
                tab_id: tab_created.tab_id,
            }),
            Event::TabClosed(tab_closed) => Some(SessionEvent::TabClosed {
                tab_id: tab_closed.tab_id,
            }),
            Event::TabFocused(tab_focused) => Some(SessionEvent::TabFocused {
                client_id: tab_focused.client_id,
                tab_id: tab_focused.tab_id,
                previous_tab_id: tab_focused.previous_tab_id,
            }),
            Event::TabMoved(tab_moved) => Some(SessionEvent::TabMoved {
                tab_id: tab_moved.tab_id,
                previous_tab_index: tab_moved.previous_tab_index,
                new_tab_index: tab_moved.new_tab_index,
            }),
            Event::Quit(_) => Some(SessionEvent::Quit),
            Event::Restarting => Some(SessionEvent::Restarting),

            // PTY size: the client redraws from the next `Painted` frame.
            Event::PtyResized(_) => None,
            // Visibility: the `Painted` frame already shows which panes have
            // area.
            Event::TerminalTooSmallEntered(_) => None,
            Event::ConfigReloaded(_) => None,
            // Per-client input state: the client holds its own mode and its
            // own mouse-select mode.
            Event::InputModeChanged(_) | Event::MouseSelectChanged(_) => None,
            // A shell's OSC 133 prompt reports.
            Event::PaneCommandStarted(_) | Event::PaneCommandFinished(_) => None,
            // A selection is client-local.
            Event::SelectionChanged(_) => None,
            // One client's own view of a floating pane: its position, its
            // pin, and its minimized state.
            Event::FloatingPaneMoved(_)
            | Event::PanePinChanged(_)
            | Event::PaneMinimizedChanged(_) => None,
        },
        Delivery::Frame(render_snapshot) => Some(SessionEvent::Painted {
            frame: Box::new(build_wire_frame(render_snapshot)),
        }),
        Delivery::PanePlacementSnapshot {
            request_id,
            snapshot,
        } => Some(SessionEvent::PanePlacementSnapshot {
            request_id: *request_id,
            snapshot: Box::new(crate::runtime::frame::build_wire_placement_snapshot(
                snapshot,
            )),
        }),
        Delivery::PanePlacementRefused {
            request_id,
            refusal,
        } => Some(SessionEvent::PanePlacementRefused {
            request_id: *request_id,
            refusal: koshi_ipc::protocol::IpcErrorPayload {
                code: match refusal.code {
                    PlacementSnapshotErrorCode::NotFound => {
                        koshi_ipc::protocol::IpcErrorCode::NotFound
                    }
                    PlacementSnapshotErrorCode::ResourceLimit => {
                        koshi_ipc::protocol::IpcErrorCode::ResourceLimit
                    }
                },
                message: refusal.message.clone(),
            },
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
        Delivery::HostWrite { .. } => None,
        Delivery::SwitchTo(session_id) => Some(SessionEvent::SwitchTo {
            session_id: *session_id,
        }),
        Delivery::PlacementCommandRejected(command_id) => {
            Some(SessionEvent::PlacementCommandRejected {
                command_id: *command_id,
            })
        }
        Delivery::Pong => Some(SessionEvent::Pong),
    }
}

/// The frames one host write of `host_write_bytes` goes out as, in order: one
/// [`SessionEvent::HostWritePart`] for each run of [`MAX_HOST_WRITE_BYTE_COUNT`]
/// bytes before the last run, then one [`SessionEvent::HostWrite`] carrying the
/// last run. Each frame carries a copy of its run.
///
/// Example: 30000000 bytes go out as two `HostWritePart` frames of 12582879
/// bytes, then one `HostWrite` frame of 4834242 bytes. No bytes go out as one
/// empty `HostWrite` frame.
pub fn build_wire_host_write_events(
    host_write_bytes: &[u8],
) -> impl Iterator<Item = SessionEvent> + '_ {
    let mut host_write_runs = host_write_bytes.chunks(MAX_HOST_WRITE_BYTE_COUNT);
    let last_host_write_run = host_write_runs.next_back().unwrap_or_default();
    host_write_runs
        .map(|host_write_run| SessionEvent::HostWritePart {
            host_write_bytes: host_write_run.to_vec(),
        })
        .chain(std::iter::once(SessionEvent::HostWrite {
            host_write_bytes: last_host_write_run.to_vec(),
        }))
}

#[cfg(test)]
mod tests;
