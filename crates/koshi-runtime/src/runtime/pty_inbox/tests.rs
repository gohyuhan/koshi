//! Tests for [`InboxSink`], which queues a pane's child output and exit on the
//! inbox, and for parking a pane, which marks it live and records its size and
//! a terminal engine.

use std::collections::{BTreeMap, HashSet};
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

/// The cutoff for an inbox event a test waits on. A test fails once it
/// elapses.
const INBOX_EVENT_DEADLINE_DURATION: Duration = Duration::from_secs(5);

/// Receive one inbox event within the deadline and assert it is exactly
/// `PtyOutput` for a pane id carrying `expected_output_bytes`.
fn assert_pty_output(
    event_receiver: &mpsc::Receiver<RuntimeEvent>,
    pane_id: PaneId,
    expected_output_bytes: &[u8],
) {
    match event_receiver.recv_timeout(INBOX_EVENT_DEADLINE_DURATION) {
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

/// A runtime sharing one fake PTY backend, returned alongside it so a test can
/// spawn panes on the backend.
fn build_test_server_with_fake_pty_backend() -> (Server, Arc<FakePtyBackend>) {
    let (event_sender, inbox_receiver) = mpsc::channel();
    let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(event_sender),
    )));
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let runtime = Server::from_runtime_parts(pty_backend, inbox_receiver);
    (runtime, fake_pty_backend)
}

/// Spawn a pane under `pane_id` in the fake PTY backend.
fn spawn_test_pane(fake_pty_backend: &FakePtyBackend, pane_id: PaneId) {
    fake_pty_backend
        .spawn_pane(
            pane_id,
            SpawnSpec::build_default_shell(None, BTreeMap::new()),
            TEST_PTY_SIZE,
        )
        .expect("spawn");
}

#[test]
fn parking_a_pane_marks_it_live_and_records_its_size_and_a_terminal_engine() {
    let (mut runtime, fake_pty_backend) = build_test_server_with_fake_pty_backend();
    let pane_id = PaneId::new();
    spawn_test_pane(&fake_pty_backend, pane_id);

    runtime.park_pane_pty(pane_id, TEST_PTY_SIZE);

    assert_eq!(runtime.live_pane_ids, HashSet::from([pane_id]));
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
    let (mut runtime, fake_pty_backend) = build_test_server_with_fake_pty_backend();
    let pane_id = PaneId::new();
    spawn_test_pane(&fake_pty_backend, pane_id);
    let parked_pty_size = PtySize {
        column_count: 100,
        row_count: 40,
    };

    runtime.park_pane_pty(pane_id, parked_pty_size);

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
    let (mut runtime, fake_pty_backend) = build_test_server_with_fake_pty_backend();
    let pane_id = PaneId::new();
    spawn_test_pane(&fake_pty_backend, pane_id);
    runtime.park_pane_pty(pane_id, TEST_PTY_SIZE);

    let larger_pty_size = PtySize {
        column_count: 120,
        row_count: 50,
    };
    runtime.park_pane_pty(pane_id, larger_pty_size);

    assert_eq!(runtime.live_pane_ids.len(), 1);
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

    match event_receiver.recv_timeout(INBOX_EVENT_DEADLINE_DURATION) {
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
    // The reader thread stops reading a pane the moment `accept_output_bytes`
    // answers `false`. A runtime that has gone away answers exactly that.
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let event_sink = InboxSink::from_event_sender(event_sender);
    let pane_id = PaneId::new();
    drop(event_receiver);

    assert!(!event_sink.accept_output_bytes(pane_id, b"nobody home".to_vec()));
    // The exit half has no way to report a closed inbox. It does not panic.
    event_sink.accept_exit_status(pane_id, ExitStatus::ExitCode(0));
}
