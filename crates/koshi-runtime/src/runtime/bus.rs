//! Event fan-out: the bounded per-subscriber delivery hub.
//!
//! `EventBus::subscribe` registers a subscriber and hands back its id, the
//! receiving end of that subscriber's own bounded queue, and its
//! [`WriterHandoff`].
//! `EventBus::publish` clones each pane placement, quit and restart event
//! into every subscriber's queue,
//! `EventBus::publish_notification_post` clones one post of the session's
//! notification queue into every subscriber's queue,
//! `EventBus::try_send_frame` puts the frame the session composed for one
//! subscriber's client on that subscriber's own queue, in the place of a frame
//! at the tail of the queue that the writing thread has not taken,
//! `EventBus::try_send_mouse_answer` puts one round of mouse answers on it,
//! `EventBus::try_send_client_action_request` puts one viewer-local action
//! the subscriber's client is asked to run on it,
//! `EventBus::queue_host_write` adds bytes aimed at that subscriber's own
//! terminal to its waiting host writes, and `EventBus::try_send_switch` puts
//! the session that subscriber's client moves to on its queue, and
//! `EventBus::send_command_rejection` puts the session's rejection of a
//! command from that subscriber's client on it. Delivery never blocks the
//! dispatcher: a subscriber whose receiver was dropped is removed on the next
//! publish.
//!
//! An event that does not fit a subscriber's full queue marks the subscriber
//! desynced: it receives nothing at all while it counts the events it misses,
//! until `EventBus::try_resync` puts a fresh
//! [`RenderSnapshot`] on its queue and returns it to live delivery. The
//! snapshot rides the same queue as events, so the subscriber reads the
//! backlog it already had, then the snapshot, then live events again. A
//! notification post reaches a desynced subscriber through the notification
//! queue its snapshot carries. A mouse answer, a viewer action request or a
//! session switch that does not fit marks the subscriber desynced the same
//! way, and is never replayed; a frame that does not
//! fit is dropped and the subscriber stays live.
//!
//! A command rejection that does not fit marks the subscriber desynced and is
//! held, not missed. A desynced subscriber holds every rejection of its
//! client's commands, each run of equal rejections in a row as one
//! [`HeldCommandRejection`], at most `MAX_HELD_COMMAND_REJECTION_COUNT` runs.
//! The snapshot that returns the subscriber to live delivery carries the held
//! runs. Example: while `pane-a`'s client is desynced, three
//! `core:resize-pane-left` rejections and one `core:resize-pane-right`
//! rejection reach its resync snapshot as two runs, ×3 and ×1.
//!
//! A frame or a resync snapshot that does not fit also marks the subscriber's
//! [`WriterHandoff`] owed a frame. The thread that writes the subscriber's
//! connection takes that mark after the next delivery it reads, and sends
//! [`RuntimeEvent::QueueRoomFreed`](crate::runtime::event::RuntimeEvent::QueueRoomFreed)
//! to the session, which offers that subscriber alone a newer frame, or its
//! resync snapshot when it is paused. Until the mark is taken, the session
//! builds that subscriber no frame and no resync snapshot.
//!
//! Host-write bytes never wait for room on the queue. They wait in the
//! subscriber's [`WriterHandoff`], which the thread that writes the
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

use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::TrySendError;
use std::sync::{Arc, Mutex};

use koshi_core::action::ActionReference;
use koshi_core::event::{Event, HeldCommandRejection, RejectReason, SubscriberLagged};
use koshi_core::ids::{CommandId, SessionId, SubscriberId};
use koshi_core::mouse::MouseAnswer;
use koshi_core::notification::{
    NotificationEntry, NotificationQueue, MAX_QUEUED_NOTIFICATION_COUNT,
};
use koshi_ipc::event::{SessionEvent, MAX_HOST_WRITE_BYTE_COUNT};
use koshi_renderer::snapshot::{Delivery, RenderSnapshot};

use crate::runtime::clipboard::encode_osc52_copy;
use crate::runtime::delivery_queue::{create_delivery_queue, DeliveryReceiver, DeliverySender};
use crate::runtime::event::{EndingNotice, SessionEnding};

/// How many undelivered items one subscriber's queue holds. Anything put on a
/// full queue is dropped for that subscriber, apart from a command rejection.
pub(crate) const SUBSCRIBER_QUEUE_CAPACITY: usize = 1024;

/// The most runs of command rejections one desynced subscriber holds: 256,
/// the most entries a client's notification queue holds.
pub(crate) const MAX_HELD_COMMAND_REJECTION_COUNT: usize = MAX_QUEUED_NOTIFICATION_COUNT;

/// Whether a subscriber is receiving events, or paused awaiting a snapshot.
#[derive(Debug, PartialEq, Eq)]
enum DeliveryState {
    /// Events are delivered as they are published.
    Live,
    /// A delivery did not fit the queue; nothing is delivered until a
    /// snapshot lands, apart from the event that ends the stream.
    Desynced {
        /// How many deliveries the subscriber has missed: the one that paused
        /// it when it was not a command rejection, plus each delivery to every
        /// subscriber since: a pane placement commit, a notification post, or
        /// a last frame its full queue had no room for.
        dropped_event_count: u64,
        /// The rejections of the subscriber's client's commands since the
        /// pause, oldest first, each run of equal rejections in a row as one
        /// run. At most [`MAX_HELD_COMMAND_REJECTION_COUNT`] runs.
        held_command_rejections: VecDeque<HeldCommandRejection>,
        /// How many runs were dropped from the front of
        /// `held_command_rejections` for room.
        dropped_command_rejection_count: u64,
    },
}

impl DeliveryState {
    /// A paused state that missed `dropped_event_count` deliveries and holds
    /// no command rejection.
    fn from_dropped_event_count(dropped_event_count: u64) -> Self {
        DeliveryState::Desynced {
            dropped_event_count,
            held_command_rejections: VecDeque::new(),
            dropped_command_rejection_count: 0,
        }
    }
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
/// its queue, and its [`WriterHandoff`].
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
    delivery_sender: DeliverySender,
    /// What waits beside the queue for the thread that writes the client's
    /// connection, shared with that thread.
    writer_handoff: Arc<WriterHandoff>,
}

/// What the bus leaves beside one subscriber's queue for the thread that
/// writes that subscriber's client connection: the host writes that wait for
/// the terminal the client runs in, and whether a frame or resync snapshot
/// found the queue full.
///
/// The host writes are whole `OSC 52` sequences, oldest first. The bus adds to
/// them, and the writing thread takes all of them before each delivery it
/// reads. The bus marks a frame owed when a frame or resync snapshot finds the
/// queue full, and the writing thread takes the mark after each delivery it
/// reads ([`take_owed_frame_mark`](Self::take_owed_frame_mark)).
#[derive(Debug, Default)]
pub struct WriterHandoff {
    /// The host writes that wait, oldest first.
    host_writes: Mutex<Vec<HostWrite>>,
    /// Whether a [`Delivery::HostWrite`] that wakes the writing thread is on
    /// the subscriber's queue. The bus sets it when it queues the wake, and
    /// clears it when a full queue drops the wake. The writing thread clears it
    /// when it reads the wake, and when it ends, after its receiver is gone.
    is_wake_queued: AtomicBool,
    /// Whether a frame or resync snapshot found the queue full since the
    /// writing thread last took the mark. The bus sets it while it holds the
    /// lock across the send that found the queue full.
    is_frame_owed: Mutex<bool>,
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

impl WriterHandoff {
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

    /// Take the owed-frame mark: `true` when a frame or resync snapshot found
    /// the queue full since the last take, and the mark is cleared.
    ///
    /// The writing thread takes it after each delivery it reads. The bus sets
    /// the mark only on a full queue, which still holds a delivery the writing
    /// thread has not read.
    ///
    /// # Panics
    /// When a thread panicked while it held the mark.
    #[must_use]
    pub fn take_owed_frame_mark(&self) -> bool {
        std::mem::take(&mut *self.is_frame_owed.lock().expect("owed frame mark"))
    }

    /// Whether a frame or resync snapshot found the queue full since the
    /// writing thread last took the owed-frame mark. The mark stays.
    ///
    /// # Panics
    /// When a thread panicked while it held the mark.
    fn is_frame_owed(&self) -> bool {
        *self.is_frame_owed.lock().expect("owed frame mark")
    }

    /// Put `delivery`, a frame or a resync snapshot, on `delivery_sender`
    /// while holding the owed-frame mark, and set the mark when the queue is
    /// full. A frame takes the place of a frame at the tail of the queue
    /// ([`DeliverySender::try_send_frame`]). Returns what the send returned,
    /// without the delivery.
    ///
    /// # Panics
    /// When a thread panicked while it held the mark.
    fn send_or_mark_frame_owed(
        &self,
        delivery_sender: &DeliverySender,
        delivery: Delivery,
    ) -> Result<(), TrySendError<()>> {
        let mut is_frame_owed = self.is_frame_owed.lock().expect("owed frame mark");
        let send_result = match delivery {
            Delivery::Frame(render_snapshot) => delivery_sender.try_send_frame(render_snapshot),
            delivery => delivery_sender
                .try_send(delivery)
                .map_err(|send_error| match send_error {
                    TrySendError::Full(_) => TrySendError::Full(()),
                    TrySendError::Disconnected(_) => TrySendError::Disconnected(()),
                }),
        };
        if let Err(TrySendError::Full(())) = send_result {
            *is_frame_owed = true;
        }
        send_result
    }
}

/// Fan-out hub: [`publish`](Self::publish) delivers each pane placement
/// commit, quit and restart to every live subscriber, over that subscriber's
/// own bounded queue. Every other event reaches no subscriber.
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

    /// Register a subscriber and hand back its id, the receiving end of its
    /// queue, and its [`WriterHandoff`], with no host write waiting and no
    /// frame owed. The subscriber starts live.
    /// Dropping the receiver ends the subscription; the bus removes it on the
    /// next publish or delivery to it.
    pub(crate) fn subscribe(&mut self) -> (SubscriberId, DeliveryReceiver, Arc<WriterHandoff>) {
        let (delivery_sender, delivery_receiver) = create_delivery_queue(SUBSCRIBER_QUEUE_CAPACITY);
        let subscriber_id = SubscriberId::new();
        let writer_handoff = Arc::new(WriterHandoff::default());
        self.subscribers.push(Subscriber {
            subscriber_id,
            delivery_state: DeliveryState::Live,
            delivery_sender,
            writer_handoff: Arc::clone(&writer_handoff),
        });
        (subscriber_id, delivery_receiver, writer_handoff)
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
    ///
    /// Only [`Event::PanePlacementCommitted`], [`Event::Quit`] and
    /// [`Event::Restarting`] reach a subscriber, as
    /// [`Delivery::PanePlacementCommitted`], [`Delivery::Quit`] and
    /// [`Delivery::Restarting`]. Every other event reaches no subscriber,
    /// counts as missed by no desynced subscriber, and returns an empty list.
    /// The match names every [`Event`] variant and takes no wildcard arm.
    #[deny(
        clippy::wildcard_enum_match_arm,
        clippy::match_wildcard_for_single_variants
    )]
    pub(crate) fn publish(&mut self, event: &Event) -> Vec<SubscriberId> {
        let (delivery, session_ending) = match event {
            Event::PanePlacementCommitted(pane_placement_committed) => (
                Delivery::PanePlacementCommitted(pane_placement_committed.clone()),
                None,
            ),
            Event::Quit(_) => (Delivery::Quit, Some(SessionEnding::Quit)),
            Event::Restarting => (Delivery::Restarting, Some(SessionEnding::Restarting)),
            Event::PaneCreated(_)
            | Event::PaneProcessExited(_)
            | Event::PaneClosing(_)
            | Event::PaneRemoved(_)
            | Event::PaneFocused(_)
            | Event::LayoutChanged(_)
            | Event::TabCreated(_)
            | Event::TabClosed(_)
            | Event::TabFocused(_)
            | Event::TabMoved(_)
            | Event::PtyResized(_)
            | Event::TerminalTooSmallEntered(_)
            | Event::ConfigReloaded(_)
            | Event::InputModeChanged(_)
            | Event::MouseSelectChanged(_)
            | Event::PaneCommandStarted(_)
            | Event::PaneCommandFinished(_)
            | Event::SelectionChanged(_)
            | Event::FloatingPaneMoved(_)
            | Event::PanePinChanged(_)
            | Event::PaneMinimizedChanged(_) => return Vec::new(),
        };
        // The stream's last frame, delivered whatever state the subscriber is
        // in: the client reading it leaves this socket. The notice carries it
        // to a client whose queue has no room left for it.
        if let Some(session_ending) = session_ending {
            self.ending_notice.raise_session_ending(session_ending);
        }
        self.deliver_to_every_subscriber(
            || delivery.clone(),
            session_ending.is_some(),
            event.get_event_name(),
        )
    }

    /// Deliver one post of the session's notification queue to every live
    /// subscriber: `posted_entry` as the queue holds it after the post, with the
    /// queue's `revision` after it. A desynced subscriber
    /// counts the post as missed, and a live one whose queue is full misses it
    /// and becomes desynced, as [`publish`](Self::publish) states for an
    /// event. Returns the subscribers removed for a receiver that is gone.
    pub(crate) fn publish_notification_post(
        &mut self,
        revision: u64,
        posted_entry: &NotificationEntry,
    ) -> Vec<SubscriberId> {
        self.deliver_to_every_subscriber(
            || Delivery::NotificationPosted {
                revision,
                posted_entry: Box::new(posted_entry.clone()),
            },
            false,
            "NotificationPosted",
        )
    }

    /// Put the delivery `build_delivery` builds on every live subscriber's
    /// queue, and on every desynced one when `is_stream_ending`. A desynced
    /// subscriber otherwise counts it as missed. A live subscriber whose queue
    /// is full misses it and becomes desynced; a desynced one whose queue is
    /// full misses it and counts it. Each drop logs `delivery_name`. Returns
    /// the subscribers removed for a receiver that is gone.
    fn deliver_to_every_subscriber(
        &mut self,
        build_delivery: impl Fn() -> Delivery,
        is_stream_ending: bool,
        delivery_name: &'static str,
    ) -> Vec<SubscriberId> {
        let mut removed_subscriber_ids = Vec::new();
        self.subscribers.retain_mut(|subscriber| {
            if let DeliveryState::Desynced {
                dropped_event_count,
                ..
            } = &mut subscriber.delivery_state
            {
                if !is_stream_ending {
                    *dropped_event_count += 1;
                    return true;
                }
            }
            match subscriber.delivery_sender.try_send(build_delivery()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    // The writing thread reads the raised notice when it
                    // wakes, also while a frame waits at its queue's head.
                    if is_stream_ending {
                        subscriber.delivery_sender.wake_receiver();
                    }
                    match &mut subscriber.delivery_state {
                        DeliveryState::Desynced {
                            dropped_event_count,
                            ..
                        } => {
                            *dropped_event_count += 1;
                            tracing::warn!(
                                subscriber_id = %subscriber.subscriber_id,
                                event = delivery_name,
                                "last frame dropped; subscriber queue full"
                            );
                        }
                        delivery_state @ DeliveryState::Live => {
                            *delivery_state = DeliveryState::from_dropped_event_count(1);
                            tracing::warn!(
                                subscriber_id = %subscriber.subscriber_id,
                                event = delivery_name,
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

    /// Whether a frame or resync snapshot found `subscriber_id`'s queue full
    /// since its writing thread last took the owed-frame mark
    /// ([`WriterHandoff::take_owed_frame_mark`]). `false` when `subscriber_id`
    /// is not registered.
    pub(crate) fn is_frame_owed(&self, subscriber_id: SubscriberId) -> bool {
        self.find_subscriber_index(subscriber_id)
            .is_some_and(|subscriber_index| {
                self.subscribers[subscriber_index]
                    .writer_handoff
                    .is_frame_owed()
            })
    }

    /// Whether `subscriber_id` is live and not owed a frame
    /// ([`is_frame_owed`](Self::is_frame_owed)). `false` when `subscriber_id`
    /// is not registered.
    pub(crate) fn can_take_frame(&self, subscriber_id: SubscriberId) -> bool {
        self.find_subscriber_index(subscriber_id)
            .is_some_and(|subscriber_index| {
                let subscriber = &self.subscribers[subscriber_index];
                subscriber.delivery_state == DeliveryState::Live
                    && !subscriber.writer_handoff.is_frame_owed()
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

    /// Put `snapshot` and `notification_queue`, the session's notification
    /// queue, on desynced subscriber `subscriber_id`'s queue and return it to
    /// live delivery, reporting how many deliveries it missed and carrying the
    /// command rejections it held.
    ///
    /// Returns `true` once the snapshot is queued. Returns `false` when `subscriber_id` is
    /// unknown, when it is already live, or when its queue is full. A full
    /// queue marks the subscriber's [`WriterHandoff`] owed a frame. A
    /// subscriber whose receiver is gone is removed.
    pub(crate) fn try_resync(
        &mut self,
        subscriber_id: SubscriberId,
        render_snapshot: Box<RenderSnapshot>,
        notification_queue: Box<NotificationQueue>,
    ) -> bool {
        let Some(subscriber_index) = self.find_subscriber_index(subscriber_id) else {
            return false;
        };
        let subscriber = &mut self.subscribers[subscriber_index];
        let DeliveryState::Desynced {
            dropped_event_count,
            held_command_rejections,
            dropped_command_rejection_count,
        } = &subscriber.delivery_state
        else {
            return false;
        };
        let dropped_event_count = *dropped_event_count;
        let lag_report = SubscriberLagged {
            subscriber_id,
            dropped_event_count,
        };
        let held_command_rejections: Vec<HeldCommandRejection> =
            held_command_rejections.iter().cloned().collect();
        let held_command_rejection_count = held_command_rejections.len();
        let dropped_command_rejection_count = *dropped_command_rejection_count;
        match subscriber.writer_handoff.send_or_mark_frame_owed(
            &subscriber.delivery_sender,
            Delivery::Snapshot {
                render_snapshot,
                lag_report,
                notification_queue,
                held_command_rejections,
                dropped_command_rejection_count,
            },
        ) {
            Ok(()) => {
                subscriber.delivery_state = DeliveryState::Live;
                tracing::info!(
                    subscriber_id = %subscriber_id,
                    dropped_event_count,
                    held_command_rejection_count,
                    dropped_command_rejection_count,
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
    /// draws. A frame at the tail of the queue that the writing thread has not
    /// taken is replaced by `snapshot`: the queue holds `Frame(a)`, `Event(e)`,
    /// `Frame(b)`, and a new `Frame(c)` leaves `Frame(a)`, `Event(e)`,
    /// `Frame(c)`.
    ///
    /// Returns `true` once the frame is queued. Returns `false` when `subscriber_id` is
    /// unknown, when it is desynced — a paused subscriber takes its resync
    /// snapshot first — or when its queue is full. A full queue marks the
    /// subscriber's [`WriterHandoff`] owed a frame. A subscriber whose receiver
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
        match subscriber.writer_handoff.send_or_mark_frame_owed(
            &subscriber.delivery_sender,
            Delivery::Frame(render_snapshot),
        ) {
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
                subscriber.delivery_state = DeliveryState::from_dropped_event_count(1);
                QueueDeliveryStatus::Dropped
            }
            Err(TrySendError::Disconnected(_)) => {
                self.subscribers.remove(subscriber_index);
                QueueDeliveryStatus::Skipped
            }
        }
    }

    /// Put `mouse_answers`, the answer to mouse round `request_id`, on a live
    /// subscriber's queue.
    ///
    /// Returns `true` when queued. A full queue marks the subscriber desynced,
    /// and a missing, paused or disconnected subscriber returns `false`.
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

    /// Put the request numbered `client_action_request_id` to run
    /// `action_reference` on a live subscriber's queue.
    ///
    /// Returns `true` when queued. A full queue marks the subscriber desynced,
    /// and a missing, paused or disconnected subscriber returns `false`. A
    /// request that returns `false` never reaches the client.
    pub(crate) fn try_send_client_action_request(
        &mut self,
        subscriber_id: SubscriberId,
        client_action_request_id: u64,
        action_reference: ActionReference,
    ) -> bool {
        match self.try_send_delivery(
            subscriber_id,
            Delivery::ClientActionRequested {
                client_action_request_id,
                action_reference,
            },
        ) {
            QueueDeliveryStatus::Sent => true,
            QueueDeliveryStatus::Dropped => {
                tracing::warn!(
                    subscriber_id = %subscriber_id,
                    client_action_request_id,
                    "viewer action request dropped; subscriber desynced, awaiting snapshot"
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
                .writer_handoff
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
            .writer_handoff
            .is_wake_queued
            .swap(true, Ordering::SeqCst)
        {
            return true;
        }
        match subscriber.delivery_sender.try_send(Delivery::HostWrite) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                subscriber.writer_handoff.clear_queued_wake();
                true
            }
            Err(TrySendError::Disconnected(_)) => {
                self.subscribers.remove(subscriber_index);
                false
            }
        }
    }

    /// Put the session live subscriber `subscriber_id`'s client moves to on that
    /// subscriber's queue: `session_id`, and `owner_user_id`, the user id whose
    /// folder of the shared directory advertises it, or `None` for a lookup by
    /// id alone. The thread that writes the client's connection
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
        owner_user_id: Option<u32>,
    ) -> bool {
        match self.try_send_delivery(
            subscriber_id,
            Delivery::SwitchTo {
                session_id,
                owner_user_id,
            },
        ) {
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

    /// Deliver the session's rejection of command `command_id` to subscriber
    /// `subscriber_id`: `reject_reason`, the hint `help`, and `fired_action`,
    /// the action the client's key fired for the command or `None`.
    ///
    /// A live subscriber takes it on its queue. A live subscriber whose queue
    /// is full becomes desynced, holding the rejection and missing no
    /// delivery. A desynced subscriber holds it
    /// ([`hold_command_rejection`]). The snapshot that returns the subscriber
    /// to live delivery carries what it holds ([`try_resync`](Self::try_resync)).
    /// An unknown subscriber takes nothing, and one whose receiver is gone is
    /// removed.
    pub(crate) fn send_command_rejection(
        &mut self,
        subscriber_id: SubscriberId,
        command_id: CommandId,
        fired_action: Option<ActionReference>,
        reject_reason: RejectReason,
        help: Option<String>,
    ) {
        let Some(subscriber_index) = self.find_subscriber_index(subscriber_id) else {
            return;
        };
        let subscriber = &mut self.subscribers[subscriber_index];
        if let DeliveryState::Desynced {
            held_command_rejections,
            dropped_command_rejection_count,
            ..
        } = &mut subscriber.delivery_state
        {
            hold_command_rejection(
                held_command_rejections,
                dropped_command_rejection_count,
                fired_action,
                reject_reason,
                help,
            );
            return;
        }
        let rejection_delivery = Delivery::CommandRejected {
            command_id,
            fired_action,
            reject_reason,
            help,
        };
        match subscriber.delivery_sender.try_send(rejection_delivery) {
            Ok(()) => {}
            Err(TrySendError::Full(unsent_delivery)) => {
                let mut held_command_rejections = VecDeque::new();
                if let Delivery::CommandRejected {
                    fired_action,
                    reject_reason,
                    help,
                    ..
                } = unsent_delivery
                {
                    held_command_rejections.push_back(HeldCommandRejection {
                        fired_action,
                        reject_reason,
                        help,
                        repeat_count: NonZeroU32::MIN,
                    });
                }
                subscriber.delivery_state = DeliveryState::Desynced {
                    dropped_event_count: 0,
                    held_command_rejections,
                    dropped_command_rejection_count: 0,
                };
                tracing::warn!(
                    subscriber_id = %subscriber_id,
                    %command_id,
                    "command rejection held; subscriber desynced, awaiting snapshot"
                );
            }
            Err(TrySendError::Disconnected(_)) => {
                self.subscribers.remove(subscriber_index);
            }
        }
    }
}

/// Add one command rejection behind `held_command_rejections`, the runs a
/// desynced subscriber holds: `fired_action`, `reject_reason` and `help`.
///
/// A rejection equal to the newest run in all three adds 1 to that run's
/// `repeat_count`, which saturates at [`u32::MAX`]. Any other rejection
/// starts a new run with `repeat_count` 1. A new run that finds
/// [`MAX_HELD_COMMAND_REJECTION_COUNT`] runs held first drops the oldest run
/// and adds 1 to `dropped_command_rejection_count`.
///
/// Example: holding `core:resize-pane-left` with
/// [`RejectReason::MinimumSize`] twice, then `core:resize-pane-right` with the
/// same reason, gives the runs `core:resize-pane-left` ×2 and
/// `core:resize-pane-right` ×1.
fn hold_command_rejection(
    held_command_rejections: &mut VecDeque<HeldCommandRejection>,
    dropped_command_rejection_count: &mut u64,
    fired_action: Option<ActionReference>,
    reject_reason: RejectReason,
    help: Option<String>,
) {
    if let Some(newest_run) = held_command_rejections.back_mut() {
        if newest_run.fired_action == fired_action
            && newest_run.reject_reason == reject_reason
            && newest_run.help == help
        {
            newest_run.repeat_count = newest_run.repeat_count.saturating_add(1);
            return;
        }
    }
    if held_command_rejections.len() == MAX_HELD_COMMAND_REJECTION_COUNT {
        held_command_rejections.pop_front();
        *dropped_command_rejection_count += 1;
    }
    held_command_rejections.push_back(HeldCommandRejection {
        fired_action,
        reject_reason,
        help,
        repeat_count: NonZeroU32::MIN,
    });
}

/// The frame an attached client is sent for one item off its queue, or `None`
/// when no [`SessionEvent`] spells that item.
///
/// A [`Delivery::Frame`] gives `None`: the connection writer sends it as a
/// painted update ([`crate::runtime::painted_update`]), and a client that can
/// paint images receives new RGBA records after it.
///
/// A [`Delivery::Snapshot`] becomes [`SessionEvent::Resync`] carrying the count
/// of missed deliveries, the session's notification queue, the held command
/// rejections, and the count of held runs dropped for room. The connection
/// writer sends the snapshot's frame right after it, as a painted update.
///
/// A [`Delivery::NotificationPosted`] becomes
/// [`SessionEvent::NotificationPosted`] carrying the same revision and posted
/// entry.
///
/// A [`Delivery::MouseAnswer`] becomes [`SessionEvent::MouseAnswer`] carrying
/// the id of the round it answers and that round's answers.
///
/// A [`Delivery::HostWrite`] gives `None`: it only wakes the writing thread,
/// and the waiting bytes go out as the frames [`build_wire_host_write_events`]
/// lists.
///
/// A [`Delivery::SwitchTo`] becomes [`SessionEvent::SwitchTo`] carrying the id
/// of the session the client attaches to next and the user id whose shared
/// folder advertises it.
///
/// A [`Delivery::CommandRejected`] becomes [`SessionEvent::CommandRejected`]
/// carrying the same command id, fired action, reason and hint. A
/// [`Delivery::ClientActionRequested`] becomes
/// [`SessionEvent::ClientActionRequested`] carrying the same number and
/// action.
///
/// A [`Delivery::PanePlacementCommitted`] becomes
/// [`SessionEvent::PanePlacementCommitted`] carrying the same command id,
/// source pane, source and destination tabs, and placement target. A
/// [`Delivery::Quit`] becomes [`SessionEvent::Quit`], and a
/// [`Delivery::Restarting`] becomes [`SessionEvent::Restarting`].
///
/// The match names every [`Delivery`] variant and takes no wildcard arm.
#[must_use]
#[deny(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]
pub fn build_wire_event(delivery: &Delivery) -> Option<SessionEvent> {
    match delivery {
        Delivery::PanePlacementCommitted(pane_placement_committed) => {
            Some(SessionEvent::PanePlacementCommitted {
                command_id: pane_placement_committed.command_id,
                source_pane_id: pane_placement_committed.source_pane_id,
                source_tab_id: pane_placement_committed.source_tab_id,
                destination_tab_id: pane_placement_committed.destination_tab_id,
                placement_target: pane_placement_committed.placement_target.clone(),
            })
        }
        Delivery::Quit => Some(SessionEvent::Quit),
        Delivery::Restarting => Some(SessionEvent::Restarting),
        Delivery::Frame(_) => None,
        Delivery::Snapshot {
            lag_report,
            notification_queue,
            held_command_rejections,
            dropped_command_rejection_count,
            ..
        } => Some(SessionEvent::Resync {
            dropped_event_count: lag_report.dropped_event_count,
            notification_queue: NotificationQueue::clone(notification_queue),
            held_command_rejections: held_command_rejections.clone(),
            dropped_command_rejection_count: *dropped_command_rejection_count,
        }),
        Delivery::NotificationPosted {
            revision,
            posted_entry,
        } => Some(SessionEvent::NotificationPosted {
            revision: *revision,
            posted_entry: NotificationEntry::clone(posted_entry),
        }),
        Delivery::MouseAnswer {
            request_id,
            mouse_answers,
        } => Some(SessionEvent::MouseAnswer {
            request_id: *request_id,
            mouse_answers: mouse_answers.clone(),
        }),
        Delivery::HostWrite => None,
        Delivery::SwitchTo {
            session_id,
            owner_user_id,
        } => Some(SessionEvent::SwitchTo {
            session_id: *session_id,
            owner_user_id: *owner_user_id,
        }),
        Delivery::CommandRejected {
            command_id,
            fired_action,
            reject_reason,
            help,
        } => Some(SessionEvent::CommandRejected {
            command_id: *command_id,
            fired_action: fired_action.clone(),
            reject_reason: *reject_reason,
            help: help.clone(),
        }),
        Delivery::ClientActionRequested {
            client_action_request_id,
            action_reference,
        } => Some(SessionEvent::ClientActionRequested {
            client_action_request_id: *client_action_request_id,
            action_reference: action_reference.clone(),
        }),
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
pub(crate) mod tests;
