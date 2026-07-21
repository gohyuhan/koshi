//! Tests for per-pane PTY forwarding: parking records the handle, size, and a
//! terminal engine, and the forwarder thread relays child output in order and
//! then the exit once output reaches end of file.

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
    assert_eq!(
        rx.recv_timeout(DEADLINE),
        Ok(RuntimeEvent::PtyOutput {
            pane_id: pane,
            bytes: b"first".to_vec(),
        })
    );
    assert_eq!(
        rx.recv_timeout(DEADLINE),
        Ok(RuntimeEvent::PtyOutput {
            pane_id: pane,
            bytes: b"second".to_vec(),
        })
    );
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
    assert_eq!(
        rx.recv_timeout(DEADLINE),
        Ok(RuntimeEvent::PtyOutput {
            pane_id: pane,
            bytes: b"out".to_vec(),
        })
    );
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
    assert_eq!(
        rx.recv_timeout(DEADLINE),
        Ok(RuntimeEvent::PtyOutput {
            pane_id: pane,
            bytes: b"tail".to_vec(),
        })
    );

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
    assert_eq!(rt.inbox_rx().try_recv(), Err(TryRecvError::Empty));
}
