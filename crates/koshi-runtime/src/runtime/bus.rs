//! Event fan-out: the bounded per-subscriber delivery hub.
//!
//! `EventBus::subscribe` registers a subscriber and hands back its id, the
//! receiving end of that subscriber's own bounded queue, and its
//! [`WaitingHostWrites`].
//! `EventBus::publish` clones each event into every subscriber's queue,
//! `EventBus::try_send_frame` puts the frame the session composed for one
//! subscriber's client on that subscriber's own queue,
//! `EventBus::try_send_mouse_answer` puts one round of mouse answers on it,
//! `EventBus::queue_host_write` adds bytes aimed at that subscriber's own
//! terminal to its waiting host writes, and `EventBus::try_send_switch` puts
//! the session that subscriber's client moves to on its queue. Delivery
//! never blocks the dispatcher: a subscriber whose receiver was dropped is
//! removed on the next publish.
//!
//! An event that does not fit a subscriber's full queue marks the subscriber
//! desynced: it receives nothing at all while it counts the events it misses,
//! until `EventBus::try_resync` puts a fresh
//! [`RenderSnapshot`] on its queue and returns it to live delivery. The
//! snapshot rides the same queue as events, so the subscriber reads the
//! backlog it already had, then the snapshot, then live events again. A mouse
//! answer, session switch, placement preview, placement refusal, or placement
//! command rejection that does not fit marks the subscriber desynced the same
//! way; a frame that does not fit is dropped and the subscriber stays live.
//!
//! Host-write bytes never wait for room on the queue. They wait in the
//! subscriber's [`WaitingHostWrites`], which the thread that writes the
//! subscriber's connection empties before each delivery it reads. A
//! [`Delivery::HostWrite`] on the queue only wakes that thread, and a full
//! queue drops the wake and pauses no one.
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
//! frame that client is sent, and [`build_wire_host_write_events`] turns the
//! waiting host-write bytes into the frames that client is sent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use koshi_core::event::{Event, SubscriberLagged};
use koshi_core::ids::{CommandId, SessionId, SubscriberId};
use koshi_core::mouse::MouseAnswer;
use koshi_ipc::event::{SessionEvent, MAX_HOST_WRITE_BYTE_COUNT};
use koshi_renderer::snapshot::{
    Delivery, PlacementSnapshot, PlacementSnapshotError, PlacementSnapshotErrorCode, RenderSnapshot,
};

use crate::runtime::clipboard::encode_osc52_copy;
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
/// its queue, and the host-write bytes that wait for its client's terminal.
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
    /// The host writes that wait for the terminal the subscriber's client runs
    /// in, shared with the thread that writes the client's connection.
    waiting_host_writes: Arc<WaitingHostWrites>,
}

/// The host writes that wait for the terminal one subscriber's client runs
/// in: whole `OSC 52` sequences, oldest first. The bus adds to them, and the
/// thread that writes the client's connection takes all of them before each
/// delivery it reads.
#[derive(Debug, Default)]
pub struct WaitingHostWrites {
    /// The host writes that wait, oldest first.
    host_writes: Mutex<Vec<HostWrite>>,
    /// Whether a [`Delivery::HostWrite`] that wakes the writing thread is on
    /// the subscriber's queue. The bus sets it when it queues the wake, and
    /// clears it when a full queue drops the wake. The writing thread clears it
    /// when it reads the wake, and when it ends, after its receiver is gone.
    is_wake_queued: AtomicBool,
}

/// One write for the terminal a client runs in: a whole `OSC 52` sequence,
/// and the clipboard targets it sets.
#[derive(Debug, Clone)]
pub(crate) struct HostWrite {
    /// The clipboard targets the sequence sets, in byte order: targets `pc`
    /// are kept as `cp`.
    sorted_clipboard_targets: Vec<u8>,
    /// The whole sequence.
    host_write_bytes: Vec<u8>,
}

impl HostWrite {
    /// The `OSC 52` sequence that [`encode_osc52_copy`] writes for
    /// `clipboard_targets` and `copied_bytes`, with the targets it sets.
    ///
    /// Example: `pc` and `hello` give the sequence `\x1b]52;pc;aGVsbG8=\x07`,
    /// which sets the targets `cp`.
    #[must_use]
    pub(crate) fn from_clipboard_copy(clipboard_targets: &str, copied_bytes: &[u8]) -> HostWrite {
        let mut sorted_clipboard_targets = clipboard_targets.as_bytes().to_vec();
        sorted_clipboard_targets.sort_unstable();
        HostWrite {
            sorted_clipboard_targets,
            host_write_bytes: encode_osc52_copy(clipboard_targets, copied_bytes),
        }
    }
}

impl WaitingHostWrites {
    /// Take the bytes of every host write that waits, oldest first, and leave
    /// none. Empty when none waits.
    ///
    /// # Panics
    /// When a thread panicked while it held the host writes.
    #[must_use]
    pub fn take_host_write_bytes(&self) -> Vec<u8> {
        let mut waiting_host_writes =
            std::mem::take(&mut *self.host_writes.lock().expect("waiting host writes")).into_iter();
        let Some(oldest_host_write) = waiting_host_writes.next() else {
            return Vec::new();
        };
        let mut host_write_bytes = oldest_host_write.host_write_bytes;
        for waiting_host_write in waiting_host_writes {
            host_write_bytes.extend_from_slice(&waiting_host_write.host_write_bytes);
        }
        host_write_bytes
    }

    /// Record that no [`Delivery::HostWrite`] waits on the subscriber's queue:
    /// the next bytes the bus adds queue a new wake. The writing thread calls
    /// it when it reads the wake, and when it ends, after its receiver is
    /// gone. The bus calls it when a full queue drops the wake.
    pub fn clear_queued_wake(&self) {
        self.is_wake_queued.store(false, Ordering::SeqCst);
    }
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

    /// Register a subscriber for every published event and hand back its id,
    /// the receiving end of its queue, and its [`WaitingHostWrites`], with no
    /// host write waiting. The subscriber starts live. Dropping the receiver
    /// ends the subscription; the bus removes it on the next publish or
    /// delivery to it.
    pub(crate) fn subscribe(
        &mut self,
    ) -> (SubscriberId, Receiver<Delivery>, Arc<WaitingHostWrites>) {
        let (delivery_sender, delivery_receiver) = sync_channel(SUBSCRIBER_QUEUE_CAPACITY);
        let subscriber_id = SubscriberId::new();
        let waiting_host_writes = Arc::new(WaitingHostWrites::default());
        self.subscribers.push(Subscriber {
            subscriber_id,
            delivery_state: DeliveryState::Live,
            delivery_sender,
            waiting_host_writes: Arc::clone(&waiting_host_writes),
        });
        (subscriber_id, delivery_receiver, waiting_host_writes)
    }

    /// Deliver `event` to every live subscriber, and to every desynced one
    /// when `event` ends the stream. An event that ends the stream first
    /// raises the [`EndingNotice`], so a subscriber whose queue has no room
    /// for the event is told over the notice instead.
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

    /// Add `host_write` behind the host writes that wait for the terminal
    /// subscriber `subscriber_id`'s client runs in, live or paused alike. Each
    /// waiting host write that sets the same clipboard targets, in any order,
    /// is removed first: `pc` and `cp` are the same targets, and `c` and `pc`
    /// are not. Then, when no wake is on its queue, put one
    /// [`Delivery::HostWrite`] there to wake the thread that writes the
    /// client's connection. A full queue drops that wake and changes no
    /// delivery state: the thread takes the waiting host writes before the
    /// next delivery it reads.
    ///
    /// When host writes that set other clipboard targets wait, and their bytes
    /// and the bytes of `host_write` together are more than
    /// [`MAX_HOST_WRITE_BYTE_COUNT`], `host_write` is refused: no waiting host
    /// write changes, no wake is queued, and a warning is logged. Example:
    /// while a 7 MiB sequence for `c` waits, a 7 MiB sequence for `p` is
    /// refused, and a 7 MiB sequence for `c` replaces the one that waits. A
    /// sequence longer than [`MAX_HOST_WRITE_BYTE_COUNT`] waits when no host
    /// write that sets other clipboard targets waits.
    ///
    /// Returns `true` once `host_write` waits. Returns `false`, and adds
    /// nothing, when `subscriber_id` is not registered or `host_write` is
    /// refused. Returns `false` when the wake finds the receiver gone: the
    /// subscriber is removed with every host write that waits for it. A
    /// subscriber with a wake already on its queue takes no new wake, and its
    /// receiver is not checked. The thread that writes the client's connection
    /// clears that wake when it ends, after its receiver is gone: the next host
    /// write finds the receiver gone.
    ///
    /// # Panics
    /// When a thread panicked while it held the waiting host writes.
    pub(crate) fn queue_host_write(
        &mut self,
        subscriber_id: SubscriberId,
        host_write: &HostWrite,
    ) -> bool {
        let Some(subscriber_index) = self.find_subscriber_index(subscriber_id) else {
            return false;
        };
        let subscriber = &self.subscribers[subscriber_index];
        {
            let mut waiting_host_writes = subscriber
                .waiting_host_writes
                .host_writes
                .lock()
                .expect("waiting host writes");
            let kept_byte_count: usize = waiting_host_writes
                .iter()
                .filter(|waiting_host_write| {
                    waiting_host_write.sorted_clipboard_targets
                        != host_write.sorted_clipboard_targets
                })
                .map(|waiting_host_write| waiting_host_write.host_write_bytes.len())
                .sum();
            if kept_byte_count > 0
                && kept_byte_count + host_write.host_write_bytes.len() > MAX_HOST_WRITE_BYTE_COUNT
            {
                tracing::warn!(
                    subscriber_id = %subscriber_id,
                    waiting_byte_count = kept_byte_count,
                    refused_byte_count = host_write.host_write_bytes.len(),
                    "host write refused; the client reads host writes slower than they arrive"
                );
                return false;
            }
            waiting_host_writes.retain(|waiting_host_write| {
                waiting_host_write.sorted_clipboard_targets != host_write.sorted_clipboard_targets
            });
            waiting_host_writes.push(host_write.clone());
        }
        if subscriber
            .waiting_host_writes
            .is_wake_queued
            .swap(true, Ordering::SeqCst)
        {
            return true;
        }
        match subscriber.delivery_sender.try_send(Delivery::HostWrite) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                subscriber.waiting_host_writes.clear_queued_wake();
                true
            }
            Err(TrySendError::Disconnected(_)) => {
                self.subscribers.remove(subscriber_index);
                false
            }
        }
    }

    /// Put the session live subscriber `subscriber_id`'s client moves to on that
    /// subscriber's queue. The thread that writes the client's connection
    /// writes the host-write bytes that waited before the switch was queued
    /// ahead of it.
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
/// A [`Delivery::HostWrite`] gives `None`: it only wakes the writing thread,
/// and the waiting bytes go out as the frames [`build_wire_host_write_events`]
/// lists.
///
/// A [`Delivery::SwitchTo`] becomes [`SessionEvent::SwitchTo`] carrying the id
/// of the session the client attaches to next.
///
/// A [`Delivery::PlacementCommandRejected`] becomes
/// [`SessionEvent::PlacementCommandRejected`] carrying the rejected command id.
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
        Delivery::HostWrite => None,
        Delivery::SwitchTo(session_id) => Some(SessionEvent::SwitchTo {
            session_id: *session_id,
        }),
        Delivery::PlacementCommandRejected(command_id) => {
            Some(SessionEvent::PlacementCommandRejected {
                command_id: *command_id,
            })
        }
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
