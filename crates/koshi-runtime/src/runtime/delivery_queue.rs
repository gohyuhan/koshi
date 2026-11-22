//! One attached client's delivery queue: the bounded queue between the
//! session's event bus and the thread that writes that client's connection
//! ([`create_delivery_queue`]).
//!
//! [`DeliverySender`] is the bus's end. It puts each delivery at the tail and
//! drops one when the queue holds its capacity.
//! [`DeliverySender::try_send_frame`] puts a frame in the place of the frame
//! at the tail when the writing thread has not taken that frame yet. The
//! queue then holds the newest frame and keeps the order of everything else.
//! Each frame or resync snapshot put at a place of its own gets the next frame
//! number, from 1. A frame that takes the place of the tail frame keeps the
//! tail frame's number.
//!
//! Example — the queue holds `Frame(a)` (frame 1), then `Event(e)`, then
//! `Frame(b)` (frame 2). A new `Frame(c)` takes the place of `Frame(b)` as
//! frame 2: the queue holds `Frame(a)`, `Event(e)`, `Frame(c)`. The client
//! reads `a`, then `e`, then `c`.
//!
//! [`DeliveryReceiver`] is the writing thread's end, and it takes deliveries
//! from the head, oldest first. [`DeliveryReceiver::recv_paced`] holds a
//! frame at the head while the writing thread may send no frame, and gives
//! back each paint report as it arrives. [`PaintReporter`] is the end of the
//! connection's reading thread: it records the last painted update the
//! client reported.
//!
//! [`DeliverySender::request_frame_taken_notice`] asks the writing thread to
//! say when it took the frame with a given frame number, and
//! [`DeliveryReceiver::take_frame_taken_notice`] is the writing thread's
//! check after each frame or resync snapshot it takes.
//!
//! Dropping the sender closes the queue: the receiver hands out every queued
//! delivery, then reports the queue closed. Dropping the receiver closes it
//! the other way: each send after that drops its delivery and reports the
//! queue closed.

use std::collections::VecDeque;
use std::sync::mpsc::{TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

use koshi_renderer::snapshot::Delivery;

use crate::runtime::event::EndingNotice;

/// Create a queue that holds at most `capacity` deliveries, and hand back its
/// two ends.
#[must_use]
pub fn create_delivery_queue(capacity: usize) -> (DeliverySender, DeliveryReceiver) {
    let shared_queue = Arc::new(SharedDeliveryQueue {
        queue_state: Mutex::new(DeliveryQueueState {
            deliveries: VecDeque::new(),
            capacity,
            is_sender_closed: false,
            is_receiver_closed: false,
            painted_update_report: None,
            queued_frame_count: 0,
            taken_frame_count: 0,
            frame_taken_notice_frame_number: None,
        }),
        queue_changed: Condvar::new(),
    });
    (
        DeliverySender {
            shared_queue: Arc::clone(&shared_queue),
        },
        DeliveryReceiver { shared_queue },
    )
}

/// The client's report that it painted the update numbered `update_sequence`,
/// and when its connection's reading thread read the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaintedUpdateReport {
    /// The last painted update the client applied before it painted.
    pub update_sequence: u64,
    /// When the reading thread read the report.
    pub reported_at: Instant,
}

/// What [`DeliveryReceiver::recv_paced`] gives back.
#[derive(Debug, PartialEq)]
pub enum PacedReceive {
    /// The delivery at the head, taken off the queue.
    Delivery(Delivery),
    /// The client reported a paint. The report is taken.
    PaintedUpdateReported(PaintedUpdateReport),
    /// The session is ending: the ending notice is raised.
    SessionEnding,
    /// The deadline passed first.
    TimedOut,
    /// The sender is gone and the queue is empty.
    Closed,
}

/// What both ends of one queue share.
#[derive(Debug)]
struct SharedDeliveryQueue {
    queue_state: Mutex<DeliveryQueueState>,
    /// Notified on each change a waiting receiver acts on: a delivery put on
    /// the queue, a paint report, the sender's drop, and a wake.
    queue_changed: Condvar,
}

/// The queue's contents and the state of both ends.
#[derive(Debug)]
struct DeliveryQueueState {
    /// The queued deliveries, oldest first.
    deliveries: VecDeque<Delivery>,
    /// How many deliveries the queue holds at most.
    capacity: usize,
    /// Whether the sender is gone.
    is_sender_closed: bool,
    /// Whether the receiver is gone.
    is_receiver_closed: bool,
    /// The newest paint report the receiver has not taken.
    painted_update_report: Option<PaintedUpdateReport>,
    /// How many frames and resync snapshots were put on the queue at a place
    /// of their own: the frame number of the newest one.
    queued_frame_count: u64,
    /// How many frames and resync snapshots the receiver took.
    taken_frame_count: u64,
    /// The frame number of the frame whose take the sender asked to hear about
    /// ([`DeliverySender::request_frame_taken_notice`]).
    frame_taken_notice_frame_number: Option<u64>,
}

impl DeliveryQueueState {
    /// Put `delivery` at the tail, and count it when it is a frame or a resync
    /// snapshot.
    fn push_delivery(&mut self, delivery: Delivery) {
        if is_frame_delivery(&delivery) {
            self.queued_frame_count += 1;
        }
        self.deliveries.push_back(delivery);
    }

    /// Take the delivery at the head, and count it when it is a frame or a
    /// resync snapshot.
    fn pop_delivery(&mut self) -> Option<Delivery> {
        let delivery = self.deliveries.pop_front()?;
        if is_frame_delivery(&delivery) {
            self.taken_frame_count += 1;
        }
        Some(delivery)
    }
}

impl SharedDeliveryQueue {
    /// Lock the queue state.
    ///
    /// # Panics
    /// When a thread panicked while it held the lock.
    fn lock_queue_state(&self) -> MutexGuard<'_, DeliveryQueueState> {
        self.queue_state.lock().expect("delivery queue state")
    }
}

/// Whether `delivery` carries a frame that the client paints: a frame, or a
/// resync snapshot.
fn is_frame_delivery(delivery: &Delivery) -> bool {
    matches!(delivery, Delivery::Frame(_) | Delivery::Snapshot { .. })
}

/// The bus's end of a delivery queue. Dropping it closes the queue.
#[derive(Debug)]
pub struct DeliverySender {
    shared_queue: Arc<SharedDeliveryQueue>,
}

impl DeliverySender {
    /// Put `delivery` at the tail of the queue.
    ///
    /// # Errors
    /// [`TrySendError::Full`] when the queue holds its capacity, and
    /// [`TrySendError::Disconnected`] when the receiver is gone. Each error
    /// hands back `delivery`.
    // The error is as large as a `Delivery`.
    #[allow(clippy::result_large_err)]
    pub fn try_send(&self, delivery: Delivery) -> Result<(), TrySendError<Delivery>> {
        let mut queue_state = self.shared_queue.lock_queue_state();
        if queue_state.is_receiver_closed {
            return Err(TrySendError::Disconnected(delivery));
        }
        if queue_state.deliveries.len() >= queue_state.capacity {
            return Err(TrySendError::Full(delivery));
        }
        queue_state.push_delivery(delivery);
        self.shared_queue.queue_changed.notify_all();
        Ok(())
    }

    /// Put `frame_delivery`, a frame or a resync snapshot, on the queue, and
    /// return its frame number. A frame takes the place of the frame at the
    /// tail of the queue, and keeps that frame's number. A frame goes to the
    /// tail when the tail holds no frame, and a resync snapshot always goes to
    /// the tail. Each delivery that goes to the tail gets the next frame
    /// number.
    ///
    /// Example: on an empty queue, `Frame(a)` returns 1. A `Frame(b)` before
    /// the writing thread takes `Frame(a)` takes its place and returns 1. A
    /// resync snapshot after that returns 2.
    ///
    /// # Errors
    /// [`TrySendError::Full`] when the queue holds its capacity, and
    /// [`TrySendError::Disconnected`] when the receiver is gone.
    /// `frame_delivery` is dropped then. A frame that replaces the tail frame
    /// always fits.
    pub fn try_send_frame(&self, frame_delivery: Delivery) -> Result<u64, TrySendError<()>> {
        let mut queue_state = self.shared_queue.lock_queue_state();
        if queue_state.is_receiver_closed {
            return Err(TrySendError::Disconnected(()));
        }
        let frame_delivery = match (frame_delivery, queue_state.deliveries.back_mut()) {
            (Delivery::Frame(render_snapshot), Some(Delivery::Frame(tail_render_snapshot))) => {
                *tail_render_snapshot = render_snapshot;
                return Ok(queue_state.queued_frame_count);
            }
            (frame_delivery, _) => frame_delivery,
        };
        if queue_state.deliveries.len() >= queue_state.capacity {
            return Err(TrySendError::Full(()));
        }
        queue_state.push_delivery(frame_delivery);
        self.shared_queue.queue_changed.notify_all();
        Ok(queue_state.queued_frame_count)
    }

    /// Ask to hear when the writing thread takes the frame numbered
    /// `frame_number` ([`DeliverySender::try_send_frame`]).
    ///
    /// When the writing thread took that frame already, this returns how many
    /// frames and resync snapshots it took, and the queue keeps no request.
    /// Otherwise it returns `None`, and the queue keeps the request in the
    /// place of any earlier one: the writing thread's first
    /// [`DeliveryReceiver::take_frame_taken_notice`] after it took that frame
    /// answers `true`. A request after the receiver is gone is never answered.
    ///
    /// Example: the queue held frame 1, then `Event(e)`, then frame 2, and the
    /// writing thread took frame 1. A request for frame 1 returns `Some(1)`. A
    /// request for frame 2 returns `None`, and the writing thread's check
    /// answers `true` once it takes frame 2.
    pub fn request_frame_taken_notice(&self, frame_number: u64) -> Option<u64> {
        let mut queue_state = self.shared_queue.lock_queue_state();
        if queue_state.taken_frame_count >= frame_number {
            queue_state.frame_taken_notice_frame_number = None;
            return Some(queue_state.taken_frame_count);
        }
        queue_state.frame_taken_notice_frame_number = Some(frame_number);
        None
    }

    /// Wake the receiver with nothing put on the queue: a receiver waiting in
    /// [`DeliveryReceiver::recv_paced`] reads the ending notice again.
    pub fn wake_receiver(&self) {
        let _queue_state = self.shared_queue.lock_queue_state();
        self.shared_queue.queue_changed.notify_all();
    }
}

impl Drop for DeliverySender {
    fn drop(&mut self) {
        let mut queue_state = self.shared_queue.lock_queue_state();
        queue_state.is_sender_closed = true;
        self.shared_queue.queue_changed.notify_all();
    }
}

/// The writing thread's end of a delivery queue. Dropping it closes the queue
/// and drops every delivery still on it.
#[derive(Debug)]
pub struct DeliveryReceiver {
    shared_queue: Arc<SharedDeliveryQueue>,
}

impl DeliveryReceiver {
    /// Take the delivery at the head.
    ///
    /// # Errors
    /// [`TryRecvError::Empty`] when the queue is empty and the sender is
    /// there, and [`TryRecvError::Disconnected`] when it is empty and the
    /// sender is gone.
    pub fn try_recv(&self) -> Result<Delivery, TryRecvError> {
        let mut queue_state = self.shared_queue.lock_queue_state();
        match queue_state.pop_delivery() {
            Some(delivery) => Ok(delivery),
            None if queue_state.is_sender_closed => Err(TryRecvError::Disconnected),
            None => Err(TryRecvError::Empty),
        }
    }

    /// Whether the receiver took the frame whose take the sender asked to hear
    /// about ([`DeliverySender::request_frame_taken_notice`]). A `true` answer
    /// ends the request: the next check answers `false` until the sender asks
    /// again.
    pub fn take_frame_taken_notice(&self) -> bool {
        let mut queue_state = self.shared_queue.lock_queue_state();
        let taken_frame_count = queue_state.taken_frame_count;
        let is_requested_frame_taken = queue_state
            .frame_taken_notice_frame_number
            .is_some_and(|frame_number| frame_number <= taken_frame_count);
        if is_requested_frame_taken {
            queue_state.frame_taken_notice_frame_number = None;
        }
        is_requested_frame_taken
    }

    /// The reporter that the connection's reading thread records paint
    /// reports with.
    #[must_use]
    pub fn create_paint_reporter(&self) -> PaintReporter {
        PaintReporter {
            shared_queue: Arc::clone(&self.shared_queue),
        }
    }

    /// Wait for the next thing the writing thread acts on, until `deadline`,
    /// or with no end when `deadline` is `None`. In this order:
    ///
    /// 1. a paint report that waits: it is taken;
    /// 2. a raised `ending_notice`;
    /// 3. the delivery at the head: a frame or a resync snapshot is taken only
    ///    while `is_frame_allowed` is `true` or the sender is gone, and every
    ///    other delivery is taken at once. A frame that waits holds every
    ///    delivery behind it;
    /// 4. an empty queue whose sender is gone;
    /// 5. the deadline.
    ///
    /// A delivery put on the queue, a paint report, the sender's drop and
    /// [`DeliverySender::wake_receiver`] end the wait, and the list is read
    /// again. A replaced frame does not end the wait.
    pub fn recv_paced(
        &self,
        deadline: Option<Instant>,
        is_frame_allowed: bool,
        ending_notice: &EndingNotice,
    ) -> PacedReceive {
        let mut queue_state = self.shared_queue.lock_queue_state();
        loop {
            if let Some(painted_update_report) = queue_state.painted_update_report.take() {
                return PacedReceive::PaintedUpdateReported(painted_update_report);
            }
            if ending_notice.get_session_ending().is_some() {
                return PacedReceive::SessionEnding;
            }
            let is_head_held = queue_state.deliveries.front().is_some_and(|delivery| {
                is_frame_delivery(delivery) && !is_frame_allowed && !queue_state.is_sender_closed
            });
            if !is_head_held {
                if let Some(delivery) = queue_state.pop_delivery() {
                    return PacedReceive::Delivery(delivery);
                }
                if queue_state.is_sender_closed {
                    return PacedReceive::Closed;
                }
            }
            queue_state = match deadline {
                Some(deadline) => {
                    let remaining_duration = deadline.saturating_duration_since(Instant::now());
                    if remaining_duration.is_zero() {
                        return PacedReceive::TimedOut;
                    }
                    self.shared_queue
                        .queue_changed
                        .wait_timeout(queue_state, remaining_duration)
                        .expect("delivery queue state")
                        .0
                }
                None => self
                    .shared_queue
                    .queue_changed
                    .wait(queue_state)
                    .expect("delivery queue state"),
            };
        }
    }
}

impl Drop for DeliveryReceiver {
    fn drop(&mut self) {
        let mut queue_state = self.shared_queue.lock_queue_state();
        queue_state.is_receiver_closed = true;
        queue_state.deliveries.clear();
    }
}

/// The reading thread's end of a delivery queue: it records the client's
/// paint reports for the writing thread.
#[derive(Debug, Clone)]
pub struct PaintReporter {
    shared_queue: Arc<SharedDeliveryQueue>,
}

impl PaintReporter {
    /// Record `painted_update_report`, and wake the writing thread. A report
    /// that waits keeps its place when it names a higher update.
    ///
    /// Example: reports of updates 5 and then 7 before the writing thread
    /// takes either leave the report of 7. Reports of 7 and then 5 leave the
    /// report of 7 too.
    pub fn report_painted_update(&self, painted_update_report: PaintedUpdateReport) {
        let mut queue_state = self.shared_queue.lock_queue_state();
        let is_newest_report = queue_state
            .painted_update_report
            .is_none_or(|waiting_report| {
                painted_update_report.update_sequence >= waiting_report.update_sequence
            });
        if is_newest_report {
            queue_state.painted_update_report = Some(painted_update_report);
        }
        self.shared_queue.queue_changed.notify_all();
    }
}

#[cfg(test)]
mod tests;
