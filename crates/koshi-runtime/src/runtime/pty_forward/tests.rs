//! Tests for getting a pane's child output into the inbox by either route:
//! [`InboxSink`], which the backend calls from the pane's own reader thread,
//! and the forwarder thread, which relays child output in order and then the
//! exit once output reaches end of file. Parking a pane picks the route and
//! records the handle, size, and a terminal engine either way.

use std::collections::BTreeMap;
use std::sync::mpsc::TryRecvError;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use koshi_core::geometry::Direction;
use koshi_core::process::SpawnSpec;
use koshi_pty::backend::state::PtyBackend;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};

use super::*;

const PANE_SIZE: PtySize = PtySize { cols: 80, rows: 24 };

/// A generous deadline for a value that must arrive from the forwarder thread.
/// It is a failure cutoff, not a synchronization delay — the value is expected
/// well before it elapses.
const DEADLINE: Duration = Duration::from_secs(5);

/// Receive one inbox event within the deadline and assert it is exactly
/// `PtyOutput` for `pane` carrying `bytes`.
fn expect_pty_output(rx: &mpsc::Receiver<RuntimeEvent>, pane: PaneId, bytes: &[u8]) {
    match rx.recv_timeout(DEADLINE) {
        Ok(RuntimeEvent::PtyOutput {
            pane_id,
            bytes: received,
        }) => {
            assert_eq!(pane_id, pane);
            assert_eq!(received, bytes);
        }
        other => panic!("expected PtyOutput, got {other:?}"),
    }
}

/// A runtime sharing one fake backend, returned alongside it so a test can push
/// output and exit through the backend. The sender keeps the inbox open.
fn new_runtime_with_fake() -> (Server, Arc<FakePtyBackend>, mpsc::Sender<RuntimeEvent>) {
    let fake = Arc::new(FakePtyBackend::new());
    let pty_backend: Arc<dyn PtyBackend> = fake.clone();
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, inbox_rx) = mpsc::channel();
    let runtime = Server::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        tx.clone(),
        Direction::Right,
    );
    (runtime, fake, tx)
}

/// Spawn a pane in the fake backend, returning the handle the runtime would
/// park.
fn spawn_handle(fake: &FakePtyBackend, pane: PaneId) -> PtyHandle {
    fake.spawn(
        pane,
        SpawnSpec::default_shell(None, BTreeMap::new()),
        PANE_SIZE,
    )
    .expect("spawn")
}

#[test]
fn parking_a_pane_records_its_handle_size_and_a_terminal_engine() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let pane = PaneId::new();
    let handle = spawn_handle(&fake, pane);

    rt.park_pane_pty(pane, handle, PANE_SIZE);

    assert!(rt.pty_handles.contains_key(&pane));
    assert_eq!(rt.pty_sizes.get(&pane), Some(&PANE_SIZE));
    assert!(rt.terminal_engines.contains_key(&pane));
}

#[test]
fn child_output_chunks_reach_the_inbox_in_the_order_written() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let pane = PaneId::new();
    let handle = spawn_handle(&fake, pane);
    rt.park_pane_pty(pane, handle, PANE_SIZE);

    fake.push_output(pane, b"first".to_vec()).expect("push");
    fake.push_output(pane, b"second".to_vec()).expect("push");

    let rx = rt.inbox_rx();
    expect_pty_output(rx, pane, b"first");
    expect_pty_output(rx, pane, b"second");
}

#[test]
fn the_child_exit_is_forwarded_after_all_output_drains() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let pane = PaneId::new();
    let handle = spawn_handle(&fake, pane);
    rt.park_pane_pty(pane, handle, PANE_SIZE);

    fake.push_output(pane, b"out".to_vec()).expect("push");
    fake.close_output(pane).expect("close");
    let before = SystemTime::now();
    fake.trigger_child_exit(pane, ExitStatus::ExitCode(0))
        .expect("exit");

    let rx = rt.inbox_rx();
    expect_pty_output(rx, pane, b"out");
    match rx.recv_timeout(DEADLINE) {
        Ok(RuntimeEvent::ChildExit {
            pane_id,
            status,
            exited_at,
        }) => {
            assert_eq!(pane_id, pane);
            assert_eq!(status, ExitStatus::ExitCode(0));
            assert!(exited_at >= before);
        }
        other => panic!("expected ChildExit, got {other:?}"),
    }
}

#[test]
fn the_exit_waits_until_output_reaches_end_of_file() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let pane = PaneId::new();
    let handle = spawn_handle(&fake, pane);
    rt.park_pane_pty(pane, handle, PANE_SIZE);

    // The exit fires while output is still open: the forwarder must deliver the
    // output first and hold the exit back until the channel closes.
    fake.push_output(pane, b"tail".to_vec()).expect("push");
    fake.trigger_child_exit(pane, ExitStatus::ExitCode(7))
        .expect("exit");

    let rx = rt.inbox_rx();
    expect_pty_output(rx, pane, b"tail");

    fake.close_output(pane).expect("close");
    match rx.recv_timeout(DEADLINE) {
        Ok(RuntimeEvent::ChildExit {
            pane_id, status, ..
        }) => {
            assert_eq!(pane_id, pane);
            assert_eq!(status, ExitStatus::ExitCode(7));
        }
        other => panic!("expected ChildExit, got {other:?}"),
    }
}

#[test]
fn an_exit_with_no_output_is_forwarded_once_the_channel_closes() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let pane = PaneId::new();
    let handle = spawn_handle(&fake, pane);
    rt.park_pane_pty(pane, handle, PANE_SIZE);

    fake.close_output(pane).expect("close");
    fake.trigger_child_exit(pane, ExitStatus::Signaled(9))
        .expect("exit");

    match rt.inbox_rx().recv_timeout(DEADLINE) {
        Ok(RuntimeEvent::ChildExit {
            pane_id, status, ..
        }) => {
            assert_eq!(pane_id, pane);
            assert_eq!(status, ExitStatus::Signaled(9));
        }
        other => panic!("expected ChildExit, got {other:?}"),
    }
}

#[test]
fn parking_a_drained_handle_records_the_pane_but_spawns_no_forwarder() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let pane = PaneId::new();
    let mut handle = spawn_handle(&fake, pane);
    // Take the receivers before parking: park then finds none and spawns no
    // forwarder thread, but still records the pane's bookkeeping.
    let _receivers = handle
        .take_receivers()
        .expect("first take yields receivers");

    rt.park_pane_pty(pane, handle, PANE_SIZE);

    assert!(rt.pty_handles.contains_key(&pane));
    assert_eq!(rt.pty_sizes.get(&pane), Some(&PANE_SIZE));
    assert!(rt.terminal_engines.contains_key(&pane));

    // With no forwarder consuming the backend's output, nothing reaches the
    // inbox.
    fake.push_output(pane, b"ignored".to_vec()).expect("push");
    assert!(matches!(rt.inbox_rx().try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn a_sink_queues_child_output_on_the_inbox_as_it_arrives() {
    let (tx, rx) = mpsc::channel::<RuntimeEvent>();
    let sink = InboxSink::new(tx);
    let pane = PaneId::new();

    assert!(sink.output(pane, b"first".to_vec()));
    assert!(sink.output(pane, b"second".to_vec()));

    expect_pty_output(&rx, pane, b"first");
    expect_pty_output(&rx, pane, b"second");
}

#[test]
fn a_sink_tags_each_chunk_with_the_pane_it_came_from() {
    let (tx, rx) = mpsc::channel::<RuntimeEvent>();
    let sink = InboxSink::new(tx);
    let left = PaneId::new();
    let right = PaneId::new();

    assert!(sink.output(left, b"L".to_vec()));
    assert!(sink.output(right, b"R".to_vec()));

    expect_pty_output(&rx, left, b"L");
    expect_pty_output(&rx, right, b"R");
}

#[test]
fn a_sink_stamps_the_childs_exit_and_queues_it() {
    let (tx, rx) = mpsc::channel::<RuntimeEvent>();
    let sink = InboxSink::new(tx);
    let pane = PaneId::new();

    let before = SystemTime::now();
    sink.exit(pane, ExitStatus::Signaled(9));

    match rx.recv_timeout(DEADLINE) {
        Ok(RuntimeEvent::ChildExit {
            pane_id,
            status,
            exited_at,
        }) => {
            assert_eq!(pane_id, pane);
            assert_eq!(status, ExitStatus::Signaled(9));
            assert!(exited_at >= before);
        }
        other => panic!("expected ChildExit, got {other:?}"),
    }
}

#[test]
fn a_sink_reports_a_closed_inbox_so_the_reader_can_stop() {
    // The reader thread stops reading a pane the moment `output` answers
    // `false`, so a runtime that has gone away must produce exactly that —
    // otherwise a child outliving the runtime keeps its reader thread alive.
    let (tx, rx) = mpsc::channel::<RuntimeEvent>();
    let sink = InboxSink::new(tx);
    let pane = PaneId::new();
    drop(rx);

    assert!(!sink.output(pane, b"nobody home".to_vec()));
    // The exit half has no way to report the same, so it must simply not
    // panic: it runs on the teardown path of every pane.
    sink.exit(pane, ExitStatus::ExitCode(0));
}
