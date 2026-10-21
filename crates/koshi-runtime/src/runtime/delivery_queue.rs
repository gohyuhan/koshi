//! One attached client's delivery queue: the bounded queue between the
//! session's event bus and the thread that writes that client's connection
//! ([`create_delivery_queue`]).
//!
//! [`DeliverySender`] is the bus's end. It puts each delivery at the tail and
//! drops one when the queue holds its capacity.
//! [`DeliverySender::try_send_frame`] puts a frame in the place of the frame
//! at the tail when the writing thread has not taken that frame yet. The
//! queue then holds the newest frame and keeps the order of everything else.
//!
//! Example — the queue holds `Frame(a)`, then `Event(e)`, then `Frame(b)`. A
//! new `Frame(c)` takes the place of `Frame(b)`: the queue holds `Frame(a)`,
//! `Event(e)`, `Frame(c)`. The client reads `a`, then `e`, then `c`.
//!
//! [`DeliveryReceiver`] is the writing thread's end, and it takes deliveries
//! from the head, oldest first. [`DeliveryReceiver::recv_paced`] holds a
//! frame at the head while the writing thread may send no frame, and gives
//! back each paint report as it arrives. [`PaintReporter`] is the end of the
//! connection's reading thread: it records the last painted update the
//! client reported.
//!
//! Dropping the sender closes the queue: the receiver hands out every queued
//! delivery, then reports the queue closed. Dropping the receiver closes it
//! the other way: each send after that drops its delivery and reports the
//! queue closed.

use std::collections::VecDeque;
use std::sync::mpsc::{TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

use koshi_renderer::snapshot::{Delivery, RenderSnapshot};

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
/// the placement preview it holds, and when its connection's reading thread
/// read the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaintedUpdateReport {
    /// The last painted update the client applied before it painted.
    pub update_sequence: u64,
    /// The request whose placement preview the client holds, or `None` when
    /// it holds no preview.
    pub shown_placement_request_id: Option<u64>,
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
    /// [`TrySendError::Disconnected`] when the receiver is gone. `delivery` is
    /// dropped then.
    pub fn try_send(&self, delivery: Delivery) -> Result<(), TrySendError<()>> {
        let mut queue_state = self.shared_queue.lock_queue_state();
        if queue_state.is_receiver_closed {
            return Err(TrySendError::Disconnected(()));
        }
        if queue_state.deliveries.len() >= queue_state.capacity {
            return Err(TrySendError::Full(()));
        }
        queue_state.deliveries.push_back(delivery);
        self.shared_queue.queue_changed.notify_all();
        Ok(())
    }

    /// Put the frame `render_snapshot` in the place of the frame at the tail
    /// of the queue, or at the tail when the tail holds no frame. A resync
    /// snapshot at the tail keeps its place.
    ///
    /// # Errors
    /// The errors of [`try_send`](Self::try_send). A frame that replaces the
    /// tail frame always fits.
    pub fn try_send_frame(
        &self,
        render_snapshot: Box<RenderSnapshot>,
    ) -> Result<(), TrySendError<()>> {
        let mut queue_state = self.shared_queue.lock_queue_state();
        if queue_state.is_receiver_closed {
            return Err(TrySendError::Disconnected(()));
        }
        if let Some(Delivery::Frame(tail_render_snapshot)) = queue_state.deliveries.back_mut() {
            *tail_render_snapshot = render_snapshot;
            return Ok(());
        }
        if queue_state.deliveries.len() >= queue_state.capacity {
            return Err(TrySendError::Full(()));
        }
        queue_state
            .deliveries
            .push_back(Delivery::Frame(render_snapshot));
        self.shared_queue.queue_changed.notify_all();
        Ok(())
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
        match queue_state.deliveries.pop_front() {
            Some(delivery) => Ok(delivery),
            None if queue_state.is_sender_closed => Err(TryRecvError::Disconnected),
            None => Err(TryRecvError::Empty),
        }
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
                if let Some(delivery) = queue_state.deliveries.pop_front() {
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
