//! Tests for getting a pane_id's child output into the inbox by either route:
//! [`InboxSink`], which the backend calls from the pane_id's own reader thread,
//! and the forwarder thread, which relays child output in order and then the
//! exit once output reaches end of file. Parking a pane_id picks the route and
//! records the pty_handle, size, and a terminal engine either way.

use std::collections::BTreeMap;
use std::sync::mpsc::TryRecvError;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use koshi_core::process::SpawnSpec;
use koshi_pty::backend::state::PtyBackend;
use koshi_test_support::fake_pty::FakePtyBackend;

use super::*;

const TEST_PTY_SIZE: PtySize = PtySize {
    column_count: 80,
    row_count: 24,
};

/// The cutoff for a value that must arrive from the forwarder thread. A test
/// fails once it elapses.
const FORWARDER_TEST_DEADLINE_DURATION: Duration = Duration::from_secs(5);

/// Receive one inbox event within the deadline and assert it is exactly
/// `PtyOutput` for a pane id carrying `expected_output_bytes`.
fn assert_pty_output(
    event_receiver: &mpsc::Receiver<RuntimeEvent>,
    pane_id: PaneId,
    expected_output_bytes: &[u8],
) {
    match event_receiver.recv_timeout(FORWARDER_TEST_DEADLINE_DURATION) {
        Ok(RuntimeEvent::PtyOutput {
            pane_id: reported_pane_id,
            output_bytes: received_output_bytes,
        }) => {
            assert_eq!(reported_pane_id, pane_id);
            assert_eq!(received_output_bytes, expected_output_bytes);
        }
        unexpected_event => panic!("expected PtyOutput, got {unexpected_event:?}"),
    }
}

/// A runtime sharing one fake PTY backend, returned alongside it so a test can push
/// output and exit through the backend. The sender keeps the inbox open.
fn build_test_server_with_fake_pty_backend(
) -> (Server, Arc<FakePtyBackend>, mpsc::Sender<RuntimeEvent>) {
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let (event_sender, inbox_rx) = mpsc::channel();
    let runtime = Server::from_runtime_parts(pty_backend, inbox_rx, event_sender.clone());
    (runtime, fake_pty_backend, event_sender)
}

/// Spawn a pane in the fake PTY backend, returning the PTY handle the runtime would
/// park.
fn spawn_test_pane(fake_pty_backend: &FakePtyBackend, pane_id: PaneId) -> PtyHandle {
    fake_pty_backend
        .spawn_pane(
            pane_id,
            SpawnSpec::default_shell(None, BTreeMap::new()),
            TEST_PTY_SIZE,
        )
        .expect("spawn")
}

#[test]
fn parking_a_pane_records_its_handle_size_and_a_terminal_engine() {
    let (mut runtime, fake_pty_backend, _event_sender) = build_test_server_with_fake_pty_backend();
    let pane_id = PaneId::new();
    let pty_handle = spawn_test_pane(&fake_pty_backend, pane_id);

    runtime.park_pane_pty(pane_id, pty_handle, TEST_PTY_SIZE);

    assert_eq!(
        runtime.pty_handle_by_pane_id[&pane_id].get_pane_id(),
        pane_id
    );
    assert_eq!(
        runtime.pty_size_by_pane_id.get(&pane_id),
        Some(&TEST_PTY_SIZE)
    );
    assert_eq!(
        runtime.terminal_engine_by_pane_id[&pane_id]
            .get_terminal_state()
            .get_active_grid()
            .get_grid_dimensions(),
        (TEST_PTY_SIZE.row_count, TEST_PTY_SIZE.column_count)
    );
}

#[test]
fn parking_records_the_size_it_is_given_not_the_spawn_size() {
    let (mut runtime, fake_pty_backend, _event_sender) = build_test_server_with_fake_pty_backend();
    let pane_id = PaneId::new();
    let pty_handle = spawn_test_pane(&fake_pty_backend, pane_id);
    let parked_pty_size = PtySize {
        column_count: 100,
        row_count: 40,
    };

    runtime.park_pane_pty(pane_id, pty_handle, parked_pty_size);

    assert_eq!(
        runtime.pty_size_by_pane_id.get(&pane_id),
        Some(&parked_pty_size)
    );
    assert_eq!(
        runtime.terminal_engine_by_pane_id[&pane_id]
            .get_terminal_state()
            .get_active_grid()
            .get_grid_dimensions(),
        (parked_pty_size.row_count, parked_pty_size.column_count)
    );
}

#[test]
fn parking_the_same_pane_again_replaces_its_size_and_engine() {
    let (mut runtime, fake_pty_backend, _event_sender) = build_test_server_with_fake_pty_backend();
    let pane_id = PaneId::new();
    let pty_handle = spawn_test_pane(&fake_pty_backend, pane_id);
    runtime.park_pane_pty(pane_id, pty_handle, TEST_PTY_SIZE);

    let larger_pty_size = PtySize {
        column_count: 120,
        row_count: 50,
    };
    runtime.park_pane_pty(
        pane_id,
        PtyHandle::from_detached_pane_id(pane_id),
        larger_pty_size,
    );

    assert_eq!(runtime.pty_handle_by_pane_id.len(), 1);
    assert_eq!(
        runtime.pty_size_by_pane_id.get(&pane_id),
        Some(&larger_pty_size)
    );
    assert_eq!(
        runtime.terminal_engine_by_pane_id[&pane_id]
            .get_terminal_state()
            .get_active_grid()
            .get_grid_dimensions(),
        (larger_pty_size.row_count, larger_pty_size.column_count)
    );
}

#[test]
fn child_output_chunks_reach_the_inbox_in_the_order_written() {
    let (mut runtime, fake_pty_backend, _event_sender) = build_test_server_with_fake_pty_backend();
    let pane_id = PaneId::new();
    let pty_handle = spawn_test_pane(&fake_pty_backend, pane_id);
    runtime.park_pane_pty(pane_id, pty_handle, TEST_PTY_SIZE);

    fake_pty_backend
        .push_output(pane_id, b"first".to_vec())
        .expect("push");
    fake_pty_backend
        .push_output(pane_id, b"second".to_vec())
        .expect("push");

    let event_receiver = runtime.inbox_rx();
    assert_pty_output(event_receiver, pane_id, b"first");
    assert_pty_output(event_receiver, pane_id, b"second");
}

#[test]
fn the_child_exit_is_forwarded_after_all_output_drains() {
    let (mut runtime, fake_pty_backend, _event_sender) = build_test_server_with_fake_pty_backend();
    let pane_id = PaneId::new();
    let pty_handle = spawn_test_pane(&fake_pty_backend, pane_id);
    runtime.park_pane_pty(pane_id, pty_handle, TEST_PTY_SIZE);

    fake_pty_backend
        .push_output(pane_id, b"out".to_vec())
        .expect("push");
    fake_pty_backend.close_output(pane_id).expect("close");
    fake_pty_backend
        .trigger_child_exit(pane_id, ExitStatus::ExitCode(0))
        .expect("exit");

    let event_receiver = runtime.inbox_rx();
    assert_pty_output(event_receiver, pane_id, b"out");
    match event_receiver.recv_timeout(FORWARDER_TEST_DEADLINE_DURATION) {
        Ok(RuntimeEvent::ChildExit {
            pane_id: reported_pane_id,
            exit_status,
        }) => {
            assert_eq!(reported_pane_id, pane_id);
            assert_eq!(exit_status, ExitStatus::ExitCode(0));
        }
        unexpected_event => panic!("expected ChildExit, got {unexpected_event:?}"),
    }
}

#[test]
fn the_exit_waits_until_output_reaches_end_of_file() {
    let (mut runtime, fake_pty_backend, _event_sender) = build_test_server_with_fake_pty_backend();
    let pane_id = PaneId::new();
    let pty_handle = spawn_test_pane(&fake_pty_backend, pane_id);
    runtime.park_pane_pty(pane_id, pty_handle, TEST_PTY_SIZE);

    // The exit fires while output is still open: the forwarder must deliver the
    // output first and hold the exit back until the channel closes.
    fake_pty_backend
        .push_output(pane_id, b"tail".to_vec())
        .expect("push");
    fake_pty_backend
        .trigger_child_exit(pane_id, ExitStatus::ExitCode(7))
        .expect("exit");

    let event_receiver = runtime.inbox_rx();
    assert_pty_output(event_receiver, pane_id, b"tail");

    fake_pty_backend.close_output(pane_id).expect("close");
    match event_receiver.recv_timeout(FORWARDER_TEST_DEADLINE_DURATION) {
        Ok(RuntimeEvent::ChildExit {
            pane_id: reported_pane_id,
            exit_status,
            ..
        }) => {
            assert_eq!(reported_pane_id, pane_id);
            assert_eq!(exit_status, ExitStatus::ExitCode(7));
        }
        unexpected_event => panic!("expected ChildExit, got {unexpected_event:?}"),
    }
}

#[test]
fn an_exit_with_no_output_is_forwarded_once_the_channel_closes() {
    let (mut runtime, fake_pty_backend, _event_sender) = build_test_server_with_fake_pty_backend();
    let pane_id = PaneId::new();
    let pty_handle = spawn_test_pane(&fake_pty_backend, pane_id);
    runtime.park_pane_pty(pane_id, pty_handle, TEST_PTY_SIZE);

    fake_pty_backend.close_output(pane_id).expect("close");
    fake_pty_backend
        .trigger_child_exit(pane_id, ExitStatus::Signaled(9))
        .expect("exit");

    match runtime
        .inbox_rx()
        .recv_timeout(FORWARDER_TEST_DEADLINE_DURATION)
    {
        Ok(RuntimeEvent::ChildExit {
            pane_id: reported_pane_id,
            exit_status,
            ..
        }) => {
            assert_eq!(reported_pane_id, pane_id);
            assert_eq!(exit_status, ExitStatus::Signaled(9));
        }
        unexpected_event => panic!("expected ChildExit, got {unexpected_event:?}"),
    }
}

#[test]
fn parking_a_drained_handle_records_the_pane_but_spawns_no_forwarder() {
    let (mut runtime, fake_pty_backend, _event_sender) = build_test_server_with_fake_pty_backend();
    let pane_id = PaneId::new();
    let mut pty_handle = spawn_test_pane(&fake_pty_backend, pane_id);
    // Take the receivers before parking: park then finds none and spawns no
    // forwarder thread, but still records the pane_id's bookkeeping.
    let _pty_receivers = pty_handle
        .take_output_and_exit_receivers()
        .expect("first take yields receivers");

    runtime.park_pane_pty(pane_id, pty_handle, TEST_PTY_SIZE);

    assert_eq!(
        runtime.pty_handle_by_pane_id[&pane_id].get_pane_id(),
        pane_id
    );
    assert_eq!(
        runtime.pty_size_by_pane_id.get(&pane_id),
        Some(&TEST_PTY_SIZE)
    );
    assert_eq!(
        runtime.terminal_engine_by_pane_id[&pane_id]
            .get_terminal_state()
            .get_active_grid()
            .get_grid_dimensions(),
        (TEST_PTY_SIZE.row_count, TEST_PTY_SIZE.column_count)
    );

    // With no forwarder consuming the backend's output, nothing reaches the
    // inbox.
    fake_pty_backend
        .push_output(pane_id, b"ignored".to_vec())
        .expect("push");
    assert_eq!(
        runtime.inbox_rx().try_recv().unwrap_err(),
        TryRecvError::Empty
    );
}

#[test]
fn parking_a_detached_handle_records_the_pane_and_spawns_no_forwarder() {
    let (mut runtime, fake_pty_backend, _event_sender) = build_test_server_with_fake_pty_backend();
    let pane_id = PaneId::new();
    let _spawned_pty_handle = spawn_test_pane(&fake_pty_backend, pane_id);

    runtime.park_pane_pty(
        pane_id,
        PtyHandle::from_detached_pane_id(pane_id),
        TEST_PTY_SIZE,
    );

    assert_eq!(
        runtime.pty_handle_by_pane_id[&pane_id].get_pane_id(),
        pane_id
    );
    assert_eq!(
        runtime.pty_size_by_pane_id.get(&pane_id),
        Some(&TEST_PTY_SIZE)
    );
    fake_pty_backend
        .push_output(pane_id, b"ignored".to_vec())
        .expect("push");
    assert_eq!(
        runtime.inbox_rx().try_recv().unwrap_err(),
        TryRecvError::Empty
    );
}

#[test]
fn a_sink_queues_child_output_on_the_inbox_as_it_arrives() {
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let event_sink = InboxSink::from_event_sender(event_sender);
    let pane_id = PaneId::new();

    assert!(event_sink.accept_output_bytes(pane_id, b"first".to_vec()));
    assert!(event_sink.accept_output_bytes(pane_id, b"second".to_vec()));

    assert_pty_output(&event_receiver, pane_id, b"first");
    assert_pty_output(&event_receiver, pane_id, b"second");
}

#[test]
fn a_sink_queues_an_empty_chunk_unchanged() {
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let event_sink = InboxSink::from_event_sender(event_sender);
    let pane_id = PaneId::new();

    assert!(event_sink.accept_output_bytes(pane_id, Vec::new()));

    assert_pty_output(&event_receiver, pane_id, b"");
}

#[test]
fn a_sink_tags_each_chunk_with_the_pane_it_came_from() {
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let event_sink = InboxSink::from_event_sender(event_sender);
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();

    assert!(event_sink.accept_output_bytes(left_pane_id, b"L".to_vec()));
    assert!(event_sink.accept_output_bytes(right_pane_id, b"R".to_vec()));

    assert_pty_output(&event_receiver, left_pane_id, b"L");
    assert_pty_output(&event_receiver, right_pane_id, b"R");
}

#[test]
fn a_sink_queues_the_childs_exit_with_its_status() {
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let event_sink = InboxSink::from_event_sender(event_sender);
    let pane_id = PaneId::new();

    event_sink.accept_exit_status(pane_id, ExitStatus::Signaled(9));

    match event_receiver.recv_timeout(FORWARDER_TEST_DEADLINE_DURATION) {
        Ok(RuntimeEvent::ChildExit {
            pane_id: reported_pane_id,
            exit_status,
        }) => {
            assert_eq!(reported_pane_id, pane_id);
            assert_eq!(exit_status, ExitStatus::Signaled(9));
        }
        unexpected_event => panic!("expected ChildExit, got {unexpected_event:?}"),
    }
}

#[test]
fn a_sink_reports_a_closed_inbox_so_the_reader_can_stop() {
    // The reader thread stops reading a pane_id the moment `output` answers
    // `false`. A runtime that has gone away answers exactly that.
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let event_sink = InboxSink::from_event_sender(event_sender);
    let pane_id = PaneId::new();
    drop(event_receiver);

    assert!(!event_sink.accept_output_bytes(pane_id, b"nobody home".to_vec()));
    // The exit half has no way to report a closed inbox. It does not panic.
    event_sink.accept_exit_status(pane_id, ExitStatus::ExitCode(0));
}
