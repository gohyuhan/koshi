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

const TEST_PTY_SIZE: PtySize = PtySize {
    column_count: 80,
    row_count: 24,
};

/// A runtime sharing one fake PTY backend, returned alongside it so a test can
/// assert on the kills shutdown issues. The sender keeps the inbox open.
fn build_test_server_with_fake_pty_backend(
) -> (Server, Arc<FakePtyBackend>, mpsc::Sender<RuntimeEvent>) {
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let (event_sender, inbox_rx) = mpsc::channel();
    let runtime = Server::from_runtime_parts(pty_backend, inbox_rx, event_sender.clone());
    (runtime, fake_pty_backend, event_sender)
}

/// Spawn a pane in the fake PTY backend and park its handle in the runtime, so the
/// pane is live in both — the backend can record kills and shutdown reaches it.
fn spawn_test_pane_and_park(
    runtime: &mut Server,
    fake_pty_backend: &FakePtyBackend,
    pane_id: PaneId,
) {
    let pty_handle = fake_pty_backend
        .spawn_pane(
            pane_id,
            SpawnSpec::default_shell(None, BTreeMap::new()),
            TEST_PTY_SIZE,
        )
        .expect("spawn");
    runtime.park_pane_pty(pane_id, pty_handle, TEST_PTY_SIZE);
}

/// A fresh directory to stand in for the runtime directory, under a short base so the
/// Unix socket path stays inside the OS path-length cap.
/// [`IpcServer::start`] creates it private itself.
fn build_test_server_directory(directory_tag: &str) -> PathBuf {
    #[cfg(unix)]
    let base_path = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base_path = std::env::temp_dir();
    base_path.join(format!("koshi-quit-{}-{directory_tag}", std::process::id()))
}

#[test]
fn explicit_quit_group_kills_every_pane_immediately_as_a_tree() {
    let (mut runtime, fake_pty_backend, _event_sender) = build_test_server_with_fake_pty_backend();
    let pane_id = PaneId::new();
    spawn_test_pane_and_park(&mut runtime, &fake_pty_backend, pane_id);
    runtime.should_shutdown_immediately = true;

    runtime.shutdown();

    assert!(runtime.is_draining());
    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(pane_id)
            .expect("pane"),
        vec![KillPolicy::Tree]
    );
}

#[test]
fn a_natural_ending_group_kills_every_pane_gracefully_with_the_configured_timeout() {
    let (mut runtime, fake_pty_backend, _event_sender) = build_test_server_with_fake_pty_backend();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    spawn_test_pane_and_park(&mut runtime, &fake_pty_backend, first_pane_id);
    spawn_test_pane_and_park(&mut runtime, &fake_pty_backend, second_pane_id);

    runtime.shutdown();

    assert!(runtime.is_draining());
    let graceful = KillPolicy::GracefulTree {
        timeout_duration: GRACEFUL_TIMEOUT_DURATION,
    };
    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(first_pane_id)
            .expect("first pane"),
        vec![graceful]
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(second_pane_id)
            .expect("second pane"),
        vec![graceful]
    );
}

#[test]
fn shutdown_with_no_parked_panes_enters_draining_and_kills_nothing() {
    let (mut runtime, fake_pty_backend, _event_sender) = build_test_server_with_fake_pty_backend();
    // Spawn a pane in the backend but never park it, so it is not a live pane
    // the runtime tracks; shutdown must not reach it.
    let unparked_pane_id = PaneId::new();
    fake_pty_backend
        .spawn_pane(
            unparked_pane_id,
            SpawnSpec::default_shell(None, BTreeMap::new()),
            TEST_PTY_SIZE,
        )
        .expect("spawn");

    runtime.shutdown();

    assert!(runtime.is_draining());
    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(unparked_pane_id)
            .expect("pane"),
        Vec::new()
    );
}

#[test]
fn calling_shutdown_again_kills_the_pane_group_once() {
    let (mut runtime, fake_pty_backend, _event_sender) = build_test_server_with_fake_pty_backend();
    let pane_id = PaneId::new();
    spawn_test_pane_and_park(&mut runtime, &fake_pty_backend, pane_id);
    runtime.should_shutdown_immediately = true;

    runtime.shutdown();
    runtime.shutdown();

    // The first shutdown closes the pane in the backend. The second one's kill
    // for it answers `PtyError::UnknownPane` and signals nothing.
    assert!(runtime.is_draining());
    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(pane_id)
            .expect("pane"),
        vec![KillPolicy::Tree]
    );
}

#[test]
fn a_pane_the_backend_cannot_kill_leaves_every_other_pane_killed() {
    let (mut runtime, fake_pty_backend, _event_sender) = build_test_server_with_fake_pty_backend();
    let live_pane_id = PaneId::new();
    let unknown_pane_id = PaneId::new();
    spawn_test_pane_and_park(&mut runtime, &fake_pty_backend, live_pane_id);
    // A handle parked for a pane the backend never spawned: its kill answers
    // `PtyError::UnknownPane`, the kill the graceful stage drops.
    runtime.park_pane_pty(
        unknown_pane_id,
        PtyHandle::from_detached_pane_id(unknown_pane_id),
        TEST_PTY_SIZE,
    );

    runtime.shutdown();

    assert!(runtime.is_draining());
    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(live_pane_id)
            .expect("live pane"),
        vec![KillPolicy::GracefulTree {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION,
        }]
    );
    assert_eq!(
        fake_pty_backend.list_pane_kill_policies(unknown_pane_id),
        Err(PtyError::UnknownPane {
            pane_id: unknown_pane_id
        })
    );
}

#[test]
fn shutdown_stops_the_attached_control_socket_and_removes_its_endpoint_file() {
    let (mut runtime, _fake_pty_backend, event_sender) = build_test_server_with_fake_pty_backend();
    let session = SessionId::new();
    let runtime_directory = build_test_server_directory("socket");
    let ipc_server =
        IpcServer::start(&runtime_directory, session, event_sender.clone(), None).expect("serving");
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session);
    assert!(endpoint_path.exists(), "the session is advertised");
    runtime.attach_ipc_server(ipc_server);

    runtime.shutdown();

    assert!(runtime.is_draining());
    assert!(runtime.ipc_server().is_none());
    assert!(!endpoint_path.exists());
    let _ = std::fs::remove_dir_all(&runtime_directory);
}
