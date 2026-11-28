//! Tests for the delivery queue: deliveries come out in the order they went
//! in, a full queue drops the delivery, a frame takes the place of the
//! frame at the tail and of nothing else and keeps its frame number, both
//! ends close the queue, the newest paint report waits, a frame taken notice
//! answers once when the requested frame is taken, and a paced receive holds a frame at the head
//! until a frame may go, the sender goes or the session ends. A delivery put
//! on the queue, a paint report, the sender's drop and a wake each end a
//! paced wait before its deadline.

use std::sync::mpsc::RecvError;
use std::thread;
use std::time::Duration;

use koshi_core::event::SubscriberLagged;
use koshi_core::ids::{PaneId, SubscriberId, TabId};
use koshi_renderer::snapshot::RenderSnapshot;

use super::*;
use crate::runtime::bus::tests::{build_test_notification_queue, build_test_placement_delivery};
use crate::runtime::event::SessionEnding;
use crate::runtime::frame::tests::build_render_snapshot;

impl DeliveryReceiver {
    /// Wait for the delivery at the head and take it.
    ///
    /// # Errors
    /// [`RecvError`] once the queue is empty and the sender is gone.
    pub(crate) fn recv(&self) -> Result<Delivery, RecvError> {
        let mut queue_state = self.shared_queue.lock_queue_state();
        loop {
            if let Some(delivery) = queue_state.pop_delivery() {
                return Ok(delivery);
            }
            if queue_state.is_sender_closed {
                return Err(RecvError);
            }
            queue_state = self
                .shared_queue
                .queue_changed
                .wait(queue_state)
                .expect("delivery queue state");
        }
    }

    /// Take every delivery on the queue now, oldest first, and wait for none.
    pub(crate) fn try_iter(&self) -> impl Iterator<Item = Delivery> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
    }

    /// Take the paint report that waits, or `None` when none waits.
    pub(crate) fn take_painted_update_report(&self) -> Option<PaintedUpdateReport> {
        self.shared_queue
            .lock_queue_state()
            .painted_update_report
            .take()
    }
}

/// `base_render_snapshot` with its session named `session_name`.
fn build_named_frame(
    base_render_snapshot: &RenderSnapshot,
    session_name: &str,
) -> Box<RenderSnapshot> {
    let mut render_snapshot = base_render_snapshot.clone();
    render_snapshot.session_snapshot.session_name = session_name.to_string();
    Box::new(render_snapshot)
}

#[test]
fn deliveries_come_out_in_the_order_they_went_in_and_a_full_queue_drops_one() {
    let (delivery_sender, delivery_receiver) = create_delivery_queue(2);
    let (first_tab_id, second_tab_id, third_tab_id) = (TabId::new(), TabId::new(), TabId::new());

    delivery_sender
        .try_send(build_test_placement_delivery(first_tab_id))
        .expect("the first delivery fits");
    delivery_sender
        .try_send(build_test_placement_delivery(second_tab_id))
        .expect("the second delivery fits");

    assert_eq!(
        delivery_sender.try_send(build_test_placement_delivery(third_tab_id)),
        Err(TrySendError::Full(build_test_placement_delivery(
            third_tab_id
        )))
    );
    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<Delivery>>(),
        vec![
            build_test_placement_delivery(first_tab_id),
            build_test_placement_delivery(second_tab_id)
        ]
    );
    assert_eq!(delivery_receiver.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn a_frame_takes_the_place_of_the_frame_at_the_tail_and_of_nothing_else() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let (delivery_sender, delivery_receiver) = create_delivery_queue(3);
    let tab_id = TabId::new();
    let lag_report = SubscriberLagged {
        subscriber_id: SubscriberId::new(),
        dropped_event_count: 4,
    };

    assert_eq!(
        delivery_sender.try_send_frame(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "a"
        ))),
        Ok(1)
    );
    delivery_sender
        .try_send(build_test_placement_delivery(tab_id))
        .expect("the event fits");
    assert_eq!(
        delivery_sender.try_send_frame(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "b"
        ))),
        Ok(2)
    );
    assert_eq!(
        delivery_sender.try_send_frame(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "c"
        ))),
        Ok(2),
        "frame c takes the place of frame b in a full queue"
    );
    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<Delivery>>(),
        vec![
            Delivery::Frame(build_named_frame(&base_render_snapshot, "a")),
            build_test_placement_delivery(tab_id),
            Delivery::Frame(build_named_frame(&base_render_snapshot, "c")),
        ]
    );

    delivery_sender
        .try_send(Delivery::Snapshot {
            held_panes: Vec::new(),
            render_snapshot: build_named_frame(&base_render_snapshot, "resync"),
            notification_queue: build_test_notification_queue(),
            lag_report,
            held_command_rejections: Vec::new(),
            dropped_command_rejection_count: 0,
        })
        .expect("the resync snapshot fits");
    assert_eq!(
        delivery_sender.try_send_frame(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "d"
        ))),
        Ok(4),
        "the resync snapshot took frame number 3 and keeps its place"
    );
    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<Delivery>>(),
        vec![
            Delivery::Snapshot {
                held_panes: Vec::new(),
                render_snapshot: build_named_frame(&base_render_snapshot, "resync"),
                notification_queue: build_test_notification_queue(),
                lag_report,
                held_command_rejections: Vec::new(),
                dropped_command_rejection_count: 0,
            },
            Delivery::Frame(build_named_frame(&base_render_snapshot, "d")),
        ]
    );
}

/// The queue holds frame `a` (frame 1), an event, then frame `b` (frame 2).
/// The sender asks to hear of the take of frame 2, then of frame 1 in its
/// place. Taking `a` answers yes once, while `b` still waits. Taking the
/// event and `b` answers no. A request for a frame the receiver took returns
/// the taken frame count, and the receiver hears nothing.
#[test]
fn a_frame_taken_notice_answers_once_when_the_requested_frame_is_taken_while_a_newer_frame_waits() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let (delivery_sender, delivery_receiver) = create_delivery_queue(4);
    let tab_id = TabId::new();
    assert_eq!(
        delivery_sender.try_send_frame(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "a"
        ))),
        Ok(1)
    );
    delivery_sender
        .try_send(build_test_placement_delivery(tab_id))
        .expect("the event fits");
    assert_eq!(
        delivery_sender.try_send_frame(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "b"
        ))),
        Ok(2)
    );

    assert_eq!(delivery_sender.request_frame_taken_notice(2), None);
    assert_eq!(delivery_sender.request_frame_taken_notice(1), None);
    let mut notice_answers = Vec::new();
    while let Ok(delivery) = delivery_receiver.try_recv() {
        notice_answers.push((delivery, delivery_receiver.take_frame_taken_notice()));
    }
    assert_eq!(
        notice_answers,
        vec![
            (
                Delivery::Frame(build_named_frame(&base_render_snapshot, "a")),
                true
            ),
            (build_test_placement_delivery(tab_id), false),
            (
                Delivery::Frame(build_named_frame(&base_render_snapshot, "b")),
                false
            ),
        ]
    );

    assert_eq!(delivery_sender.request_frame_taken_notice(2), Some(2));
    assert!(!delivery_receiver.take_frame_taken_notice());
}

/// The sender asks to hear of the take of frame 1 while frame `a` waits, and
/// frame `b` takes its place as frame 1. Taking `b` answers yes. A resync
/// snapshot then takes frame number 2 and frame `c` frame number 3, and a
/// request for frame 3 answers yes once `c` is taken. A request for frame 2
/// then returns 3, the count of every frame taken. A request after the
/// receiver is gone returns `None`.
#[test]
fn a_frame_taken_notice_follows_the_frame_that_took_the_requested_frames_place_and_a_gone_receiver_gives_no_count(
) {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let (delivery_sender, delivery_receiver) = create_delivery_queue(2);
    let lag_report = SubscriberLagged {
        subscriber_id: SubscriberId::new(),
        dropped_event_count: 1,
    };
    let build_resync_snapshot = || Delivery::Snapshot {
        held_panes: Vec::new(),
        render_snapshot: build_named_frame(&base_render_snapshot, "resync"),
        notification_queue: build_test_notification_queue(),
        lag_report,
        held_command_rejections: Vec::new(),
        dropped_command_rejection_count: 0,
    };
    assert_eq!(
        delivery_sender.try_send_frame(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "a"
        ))),
        Ok(1)
    );
    assert_eq!(delivery_sender.request_frame_taken_notice(1), None);
    assert_eq!(
        delivery_sender.try_send_frame(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "b"
        ))),
        Ok(1)
    );

    assert!(!delivery_receiver.take_frame_taken_notice());
    assert_eq!(
        delivery_receiver.try_recv(),
        Ok(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "b"
        )))
    );
    assert!(delivery_receiver.take_frame_taken_notice());

    assert_eq!(
        delivery_sender.try_send_frame(build_resync_snapshot()),
        Ok(2)
    );
    assert_eq!(
        delivery_sender.try_send_frame(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "c"
        ))),
        Ok(3)
    );
    assert_eq!(delivery_sender.request_frame_taken_notice(3), None);
    assert_eq!(delivery_receiver.try_recv(), Ok(build_resync_snapshot()));
    assert!(!delivery_receiver.take_frame_taken_notice());
    assert_eq!(
        delivery_receiver.try_recv(),
        Ok(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "c"
        )))
    );
    assert!(delivery_receiver.take_frame_taken_notice());
    assert_eq!(delivery_sender.request_frame_taken_notice(2), Some(3));

    assert_eq!(
        delivery_sender.try_send_frame(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "d"
        ))),
        Ok(4)
    );
    drop(delivery_receiver);
    assert_eq!(delivery_sender.request_frame_taken_notice(4), None);
}

/// The sender asks to hear of the take of frame 1, and the receiver takes
/// that frame before its check. A second request for frame 1 returns the
/// taken frame count 1 and ends the first request: the receiver's check
/// answers no.
#[test]
fn a_request_for_a_taken_frame_ends_the_request_that_waited_for_it() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let (delivery_sender, delivery_receiver) = create_delivery_queue(2);
    assert_eq!(
        delivery_sender.try_send_frame(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "a"
        ))),
        Ok(1)
    );
    assert_eq!(delivery_sender.request_frame_taken_notice(1), None);
    assert_eq!(
        delivery_receiver.try_recv(),
        Ok(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "a"
        )))
    );

    assert_eq!(delivery_sender.request_frame_taken_notice(1), Some(1));
    assert!(!delivery_receiver.take_frame_taken_notice());
}

#[test]
fn a_frame_taken_by_the_receiver_is_not_replaced() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let (delivery_sender, delivery_receiver) = create_delivery_queue(2);

    delivery_sender
        .try_send_frame(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "a",
        )))
        .expect("frame a fits");
    assert_eq!(
        delivery_receiver.try_recv(),
        Ok(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "a"
        )))
    );
    delivery_sender
        .try_send_frame(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "b",
        )))
        .expect("frame b fits");

    assert_eq!(
        delivery_receiver.try_recv(),
        Ok(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "b"
        )))
    );
}

#[test]
fn a_dropped_sender_closes_the_queue_after_every_queued_delivery() {
    let (delivery_sender, delivery_receiver) = create_delivery_queue(2);
    let tab_id = TabId::new();
    delivery_sender
        .try_send(build_test_placement_delivery(tab_id))
        .expect("the event fits");

    drop(delivery_sender);

    assert_eq!(
        delivery_receiver.recv(),
        Ok(build_test_placement_delivery(tab_id))
    );
    assert_eq!(delivery_receiver.recv(), Err(RecvError));
    assert_eq!(
        delivery_receiver.try_recv(),
        Err(TryRecvError::Disconnected)
    );
}

#[test]
fn a_dropped_receiver_refuses_each_delivery_sent_after_the_drop() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let (delivery_sender, delivery_receiver) = create_delivery_queue(2);
    let tab_id = TabId::new();

    drop(delivery_receiver);

    assert_eq!(
        delivery_sender.try_send(build_test_placement_delivery(tab_id)),
        Err(TrySendError::Disconnected(build_test_placement_delivery(
            tab_id
        )))
    );
    assert_eq!(
        delivery_sender.try_send_frame(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "a"
        ))),
        Err(TrySendError::Disconnected(()))
    );
}

#[test]
fn the_highest_paint_report_waits_until_it_is_taken() {
    let (_delivery_sender, delivery_receiver) = create_delivery_queue(2);
    let paint_reporter = delivery_receiver.create_paint_reporter();
    let first_reported_at = Instant::now();
    let second_reported_at = first_reported_at + Duration::from_millis(5);

    let build_report = |update_sequence, reported_at| PaintedUpdateReport {
        update_sequence,
        reported_at,
    };

    paint_reporter.report_painted_update(build_report(7, first_reported_at));
    paint_reporter.report_painted_update(build_report(5, second_reported_at));

    assert_eq!(
        delivery_receiver.take_painted_update_report(),
        Some(build_report(7, first_reported_at))
    );
    assert_eq!(delivery_receiver.take_painted_update_report(), None);

    paint_reporter.report_painted_update(build_report(8, first_reported_at));
    paint_reporter.report_painted_update(build_report(9, second_reported_at));
    assert_eq!(
        delivery_receiver.take_painted_update_report(),
        Some(build_report(9, second_reported_at))
    );
}

#[test]
fn a_paced_receive_holds_a_frame_and_everything_behind_it_until_a_frame_may_go() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let (delivery_sender, delivery_receiver) = create_delivery_queue(4);
    let ending_notice = EndingNotice::default();
    let tab_id = TabId::new();
    delivery_sender
        .try_send(build_test_placement_delivery(tab_id))
        .expect("the event fits");
    delivery_sender
        .try_send_frame(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "a",
        )))
        .expect("the frame fits");
    delivery_sender
        .try_send(build_test_placement_delivery(tab_id))
        .expect("the event behind the frame fits");

    assert_eq!(
        delivery_receiver.recv_paced(None, false, &ending_notice),
        PacedReceive::Delivery(build_test_placement_delivery(tab_id)),
        "an event at the head goes while no frame may go"
    );
    assert_eq!(
        delivery_receiver.recv_paced(Some(Instant::now()), false, &ending_notice),
        PacedReceive::TimedOut,
        "the frame at the head holds the event behind it"
    );

    let paint_reporter = delivery_receiver.create_paint_reporter();
    let reported_at = Instant::now();
    let reporting_thread = thread::spawn(move || {
        paint_reporter.report_painted_update(PaintedUpdateReport {
            update_sequence: 1,
            reported_at,
        });
    });
    let report_deadline = Instant::now() + Duration::from_secs(5);
    assert_eq!(
        delivery_receiver.recv_paced(Some(report_deadline), false, &ending_notice),
        PacedReceive::PaintedUpdateReported(PaintedUpdateReport {
            update_sequence: 1,
            reported_at,
        })
    );
    assert!(
        Instant::now() < report_deadline,
        "the report wakes the wait before its deadline"
    );
    reporting_thread
        .join()
        .expect("the reporting thread finishes");

    assert_eq!(
        delivery_receiver.recv_paced(None, true, &ending_notice),
        PacedReceive::Delivery(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "a"
        )))
    );
    assert_eq!(
        delivery_receiver.recv_paced(None, false, &ending_notice),
        PacedReceive::Delivery(build_test_placement_delivery(tab_id))
    );
}

#[test]
fn a_paced_receive_holds_a_resync_snapshot_as_it_holds_a_frame() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let (delivery_sender, delivery_receiver) = create_delivery_queue(2);
    let ending_notice = EndingNotice::default();
    let lag_report = SubscriberLagged {
        subscriber_id: SubscriberId::new(),
        dropped_event_count: 2,
    };
    delivery_sender
        .try_send(Delivery::Snapshot {
            held_panes: Vec::new(),
            render_snapshot: build_named_frame(&base_render_snapshot, "resync"),
            notification_queue: build_test_notification_queue(),
            lag_report,
            held_command_rejections: Vec::new(),
            dropped_command_rejection_count: 0,
        })
        .expect("the resync snapshot fits");

    assert_eq!(
        delivery_receiver.recv_paced(Some(Instant::now()), false, &ending_notice),
        PacedReceive::TimedOut
    );
    assert_eq!(
        delivery_receiver.recv_paced(None, true, &ending_notice),
        PacedReceive::Delivery(Delivery::Snapshot {
            held_panes: Vec::new(),
            render_snapshot: build_named_frame(&base_render_snapshot, "resync"),
            notification_queue: build_test_notification_queue(),
            lag_report,
            held_command_rejections: Vec::new(),
            dropped_command_rejection_count: 0,
        })
    );
}

#[test]
fn a_paced_receive_lets_a_held_frame_go_once_the_sender_is_gone() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let (delivery_sender, delivery_receiver) = create_delivery_queue(2);
    let ending_notice = EndingNotice::default();
    delivery_sender
        .try_send_frame(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "a",
        )))
        .expect("the frame fits");

    let wait_deadline = Instant::now() + Duration::from_secs(5);
    let closing_thread = thread::spawn(move || drop(delivery_sender));
    assert_eq!(
        delivery_receiver.recv_paced(Some(wait_deadline), false, &ending_notice),
        PacedReceive::Delivery(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "a"
        )))
    );
    assert!(
        Instant::now() < wait_deadline,
        "the sender's drop ends the wait before its deadline"
    );
    closing_thread.join().expect("the closing thread finishes");

    assert_eq!(
        delivery_receiver.recv_paced(None, false, &ending_notice),
        PacedReceive::Closed
    );
}

#[test]
fn a_paced_receive_on_an_empty_queue_takes_a_delivery_put_on_it_before_its_deadline() {
    let (delivery_sender, delivery_receiver) = create_delivery_queue(2);
    let ending_notice = EndingNotice::default();
    let tab_id = TabId::new();
    let wait_deadline = Instant::now() + Duration::from_secs(5);

    let sending_thread = thread::spawn(move || {
        delivery_sender
            .try_send(build_test_placement_delivery(tab_id))
            .expect("the event fits");
        delivery_sender
    });
    assert_eq!(
        delivery_receiver.recv_paced(Some(wait_deadline), false, &ending_notice),
        PacedReceive::Delivery(build_test_placement_delivery(tab_id))
    );
    assert!(
        Instant::now() < wait_deadline,
        "the delivery ends the wait before its deadline"
    );
    let _delivery_sender = sending_thread.join().expect("the sending thread finishes");
}

#[test]
fn a_paced_receive_reads_the_raised_ending_notice_when_the_sender_wakes_it() {
    let base_render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let (delivery_sender, delivery_receiver) = create_delivery_queue(2);
    let ending_notice = Arc::new(EndingNotice::default());
    delivery_sender
        .try_send_frame(Delivery::Frame(build_named_frame(
            &base_render_snapshot,
            "a",
        )))
        .expect("the frame fits");

    let raising_notice = Arc::clone(&ending_notice);
    let waking_thread = thread::spawn(move || {
        raising_notice.raise_session_ending(SessionEnding::Quit);
        delivery_sender.wake_receiver();
        delivery_sender
    });
    assert_eq!(
        delivery_receiver.recv_paced(None, false, &ending_notice),
        PacedReceive::SessionEnding
    );
    let _delivery_sender = waking_thread.join().expect("the waking thread finishes");
}
