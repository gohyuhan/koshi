//! Tests for the router's request gate: which connections each intake admits,
//! where a request read off an admitted connection goes, and how long the
//! closes wait for the open connections.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use koshi_ipc::protocol::{IpcErrorCode, IpcErrorPayload};

use super::*;

/// The answer the stand-in dispatcher sends to every request it receives: a
/// list of no grants.
const DISPATCHER_ANSWER: RouterResult = RouterResult::Tokens(Vec::new());

/// How long the wait tests hold a connection open before its serving thread
/// would end: 100 milliseconds.
const HELD_CONNECTION_DURATION: Duration = Duration::from_millis(100);

/// The deadline the drain test gives its close: 30 seconds, far past the
/// moment its last connection ends.
const DISTANT_DRAIN_DURATION: Duration = Duration::from_secs(30);

/// The longest the drain test's close may take once its last connection
/// ends at 100 milliseconds: 10 seconds.
const DRAIN_WAKE_LIMIT_DURATION: Duration = Duration::from_secs(10);

/// Start a stand-in dispatcher that answers each request on `router_events`
/// with [`DISPATCHER_ANSWER`] until the channel closes.
fn spawn_answering_dispatcher(router_events: Receiver<RouterEvent>) {
    std::thread::spawn(move || {
        for router_event in router_events {
            let RouterEvent::Request {
                response_sender, ..
            } = router_event
            else {
                panic!("the gate hands over requests only");
            };
            let _ = response_sender.send(DISPATCHER_ANSWER);
        }
    });
}

/// The refusal a restarting gate answers every request but a `Restart` with.
fn build_restarting_refusal() -> RouterResult {
    RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::RequestFailed,
        message: ROUTER_RESTARTING_MESSAGE.to_string(),
    })
}

#[test]
fn an_open_gate_admits_a_connection_and_hands_its_request_to_the_dispatcher() {
    let router_request_gate = Arc::new(RouterRequestGate::default());
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    spawn_answering_dispatcher(router_events_receiver);

    let admitted_connection = router_request_gate
        .admit_connection()
        .expect("an open gate admits the connection");

    assert_eq!(
        admitted_connection.ask_dispatcher(&router_events_sender, RouterRequestKind::RemoteStatus),
        Some(DISPATCHER_ANSWER)
    );
}

#[test]
fn a_restarting_gate_admits_a_connection_and_refuses_its_request_without_the_dispatcher() {
    // The stand-in dispatcher answers every request it receives with
    // `DISPATCHER_ANSWER`. Neither answer below is `DISPATCHER_ANSWER`.
    let router_request_gate = Arc::new(RouterRequestGate::default());
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    spawn_answering_dispatcher(router_events_receiver);
    router_request_gate.start_refusing_requests();

    let admitted_connection = router_request_gate
        .admit_connection()
        .expect("a restarting gate admits the connection");

    assert_eq!(
        admitted_connection.ask_dispatcher(&router_events_sender, RouterRequestKind::RemoteStatus),
        Some(build_restarting_refusal())
    );
    assert_eq!(
        admitted_connection.ask_dispatcher(&router_events_sender, RouterRequestKind::Restart),
        Some(RouterResult::Restarting)
    );
}

#[test]
fn a_closed_gate_admits_no_connection_and_gives_an_admitted_one_no_answer() {
    let router_request_gate = Arc::new(RouterRequestGate::default());
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let admitted_connection = router_request_gate
        .admit_connection()
        .expect("an open gate admits the connection");

    router_request_gate.close_once_connections_end(Instant::now());

    assert!(router_request_gate.admit_connection().is_none());
    assert_eq!(
        admitted_connection.ask_dispatcher(&router_events_sender, RouterRequestKind::Restart),
        None
    );
    assert_eq!(
        router_events_receiver.try_recv().err(),
        Some(TryRecvError::Empty)
    );
}

#[test]
fn a_request_to_a_dispatcher_that_is_gone_gets_no_answer() {
    let router_request_gate = Arc::new(RouterRequestGate::default());
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    drop(router_events_receiver);
    let admitted_connection = router_request_gate
        .admit_connection()
        .expect("an open gate admits the connection");

    assert_eq!(
        admitted_connection.ask_dispatcher(&router_events_sender, RouterRequestKind::RemoteStatus),
        None
    );
}

#[test]
fn a_reopened_gate_admits_a_connection_and_hands_its_request_over_again() {
    let router_request_gate = Arc::new(RouterRequestGate::default());
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    spawn_answering_dispatcher(router_events_receiver);
    router_request_gate.start_refusing_requests();
    router_request_gate.close_once_connections_end(Instant::now());

    router_request_gate.reopen_intake();

    let admitted_connection = router_request_gate
        .admit_connection()
        .expect("a reopened gate admits the connection");
    assert_eq!(
        admitted_connection.ask_dispatcher(&router_events_sender, RouterRequestKind::RemoteStatus),
        Some(DISPATCHER_ANSWER)
    );
}

#[test]
fn the_idle_close_happens_only_once_no_connection_is_open() {
    let router_request_gate = Arc::new(RouterRequestGate::default());
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    spawn_answering_dispatcher(router_events_receiver);
    let admitted_connection = router_request_gate
        .admit_connection()
        .expect("an open gate admits the connection");

    assert!(!router_request_gate.close_if_no_connection_is_open());
    assert_eq!(
        admitted_connection.ask_dispatcher(&router_events_sender, RouterRequestKind::RemoteStatus),
        Some(DISPATCHER_ANSWER)
    );

    drop(admitted_connection);

    assert!(router_request_gate.close_if_no_connection_is_open());
    assert!(router_request_gate.admit_connection().is_none());
}

#[test]
fn the_drain_close_waits_for_every_open_connection_to_end() {
    let router_request_gate = Arc::new(RouterRequestGate::default());
    let first_connection = router_request_gate
        .admit_connection()
        .expect("an open gate admits the connection");
    let second_connection = router_request_gate
        .admit_connection()
        .expect("an open gate admits the connection");
    router_request_gate.start_refusing_requests();
    let drain_started_at = Instant::now();
    std::thread::spawn(move || {
        std::thread::sleep(HELD_CONNECTION_DURATION / 2);
        drop(first_connection);
        std::thread::sleep(HELD_CONNECTION_DURATION / 2);
        drop(second_connection);
    });

    let open_connection_count =
        router_request_gate.close_once_connections_end(drain_started_at + DISTANT_DRAIN_DURATION);

    let drain_duration = drain_started_at.elapsed();
    assert_eq!(open_connection_count, 0);
    assert!(drain_duration >= HELD_CONNECTION_DURATION);
    assert!(drain_duration < DRAIN_WAKE_LIMIT_DURATION);
    assert!(router_request_gate.admit_connection().is_none());
}

#[test]
fn the_drain_close_closes_at_its_deadline_with_a_connection_still_open() {
    let router_request_gate = Arc::new(RouterRequestGate::default());
    let held_connection = router_request_gate
        .admit_connection()
        .expect("an open gate admits the connection");
    let drain_started_at = Instant::now();

    let open_connection_count =
        router_request_gate.close_once_connections_end(drain_started_at + HELD_CONNECTION_DURATION);

    assert_eq!(open_connection_count, 1);
    assert!(drain_started_at.elapsed() >= HELD_CONNECTION_DURATION);
    assert!(router_request_gate.admit_connection().is_none());
    drop(held_connection);
}

#[test]
fn the_drain_close_with_no_connection_open_closes_at_once() {
    let router_request_gate = Arc::new(RouterRequestGate::default());

    let open_connection_count =
        router_request_gate.close_once_connections_end(Instant::now() + Duration::from_secs(10));

    assert_eq!(open_connection_count, 0);
    assert!(router_request_gate.admit_connection().is_none());
}
