//! Tests for the staged quit teardown: draining is entered, the control socket
//! is stopped, an explicit quit group-kills immediately, a natural ending
//! group-kills gracefully, only parked panes are killed, and one pane's failed
//! kill leaves the rest killed.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc;

use koshi_core::ids::{PaneId, SessionId};
use koshi_core::process::{PtySize, SpawnSpec};
use koshi_ipc::endpoint::EndpointFile;
use koshi_pty::backend::state::{PtyBackend, PtyHandle};
use koshi_pty::error::PtyError;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::ipc_server::IpcServer;
use crate::runtime::event::RuntimeEvent;

use super::*;

const PANE_SIZE: PtySize = PtySize { cols: 80, rows: 24 };

/// A runtime sharing one fake backend, returned alongside it so a test can
/// assert on the kills shutdown issues. The sender keeps the inbox open.
fn new_runtime_with_fake() -> (Server, Arc<FakePtyBackend>, mpsc::Sender<RuntimeEvent>) {
    let fake = Arc::new(FakePtyBackend::new());
    let pty_backend: Arc<dyn PtyBackend> = fake.clone();
    let (tx, inbox_rx) = mpsc::channel();
    let runtime = Server::new(pty_backend, inbox_rx, tx.clone());
    (runtime, fake, tx)
}

/// Spawn a pane in the fake backend and park its handle in the runtime, so the
/// pane is live in both — the backend can record kills and shutdown reaches it.
fn spawn_and_park(rt: &mut Server, fake: &FakePtyBackend, pane: PaneId) {
    let handle = fake
        .spawn(
            pane,
            SpawnSpec::default_shell(None, BTreeMap::new()),
            PANE_SIZE,
        )
        .expect("spawn");
    rt.park_pane_pty(pane, handle, PANE_SIZE);
}

/// A fresh directory to stand in for the runtime dir, under a short base so the
/// Unix socket path stays inside the OS path-length cap.
/// [`IpcServer::start`] creates it private itself.
fn test_runtime_dir(tag: &str) -> PathBuf {
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    base.join(format!("koshi-quit-{}-{tag}", std::process::id()))
}

#[test]
fn explicit_quit_group_kills_every_pane_immediately_as_a_tree() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let pane = PaneId::new();
    spawn_and_park(&mut rt, &fake, pane);
    rt.immediate_shutdown = true;

    rt.shutdown();

    assert!(rt.is_draining());
    assert_eq!(fake.kills(pane).expect("pane"), vec![KillPolicy::Tree]);
}

#[test]
fn a_natural_ending_group_kills_every_pane_gracefully_with_the_configured_timeout() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let first = PaneId::new();
    let second = PaneId::new();
    spawn_and_park(&mut rt, &fake, first);
    spawn_and_park(&mut rt, &fake, second);

    rt.shutdown();

    assert!(rt.is_draining());
    let graceful = KillPolicy::GracefulTree {
        timeout: GRACEFUL_TIMEOUT_DURATION,
    };
    assert_eq!(fake.kills(first).expect("first pane"), vec![graceful]);
    assert_eq!(fake.kills(second).expect("second pane"), vec![graceful]);
}

#[test]
fn shutdown_with_no_parked_panes_enters_draining_and_kills_nothing() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    // Spawn a pane in the backend but never park it, so it is not a live pane
    // the runtime tracks; shutdown must not reach it.
    let unparked = PaneId::new();
    fake.spawn(
        unparked,
        SpawnSpec::default_shell(None, BTreeMap::new()),
        PANE_SIZE,
    )
    .expect("spawn");

    rt.shutdown();

    assert!(rt.is_draining());
    assert_eq!(fake.kills(unparked).expect("pane"), Vec::new());
}

#[test]
fn calling_shutdown_again_kills_the_pane_group_once() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let pane = PaneId::new();
    spawn_and_park(&mut rt, &fake, pane);
    rt.immediate_shutdown = true;

    rt.shutdown();
    rt.shutdown();

    // The first shutdown closes the pane in the backend. The second one's kill
    // for it answers `PtyError::UnknownPane` and signals nothing.
    assert!(rt.is_draining());
    assert_eq!(fake.kills(pane).expect("pane"), vec![KillPolicy::Tree]);
}

#[test]
fn a_pane_the_backend_cannot_kill_leaves_every_other_pane_killed() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let live = PaneId::new();
    let unknown = PaneId::new();
    spawn_and_park(&mut rt, &fake, live);
    // A handle parked for a pane the backend never spawned: its kill answers
    // `PtyError::UnknownPane`, the kill the graceful stage drops.
    rt.park_pane_pty(unknown, PtyHandle::detached(unknown), PANE_SIZE);

    rt.shutdown();

    assert!(rt.is_draining());
    assert_eq!(
        fake.kills(live).expect("live pane"),
        vec![KillPolicy::GracefulTree {
            timeout: GRACEFUL_TIMEOUT_DURATION,
        }]
    );
    assert_eq!(
        fake.kills(unknown),
        Err(PtyError::UnknownPane { pane: unknown })
    );
}

#[test]
fn shutdown_stops_the_attached_control_socket_and_removes_its_endpoint_file() {
    let (mut rt, _fake, tx) = new_runtime_with_fake();
    let session = SessionId::new();
    let runtime_dir = test_runtime_dir("socket");
    let ipc_server = IpcServer::start(&runtime_dir, session, tx.clone(), None).expect("serving");
    let endpoint_path = EndpointFile::path(&runtime_dir, session);
    assert!(endpoint_path.exists(), "the session is advertised");
    rt.attach_ipc_server(ipc_server);

    rt.shutdown();

    assert!(rt.is_draining());
    assert!(rt.ipc_server().is_none());
    assert!(!endpoint_path.exists());
    let _ = std::fs::remove_dir_all(&runtime_dir);
}
