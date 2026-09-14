//! Integration smoke for the one-pane interactive slice: genesis, PTY output
//! forwarding, typed input, child-exit forwarding, and shutdown kill — driven
//! through a fake PTY backend, exercising the public `Server` surface the
//! binary's loop uses.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use koshi_client::input::KeyOutcome;
use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::geometry::Size;
use koshi_core::ids::SessionId;
use koshi_core::key::{Key, KeyChord, ModFlags, NamedKey};
use koshi_core::process::{ExitStatus, KillPolicy};
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_pty::backend::state::PtyBackend;
use koshi_runtime::runtime::bus::EventFilter;
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::server::Server;
use koshi_test_support::fake_pty::FakePtyBackend;

const TEST_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// A server driven by `fake_backend`, holding its own inbox receiver and a sender
/// clone for the pane forwarders.
fn build_server_with_backend(fake_backend: Arc<FakePtyBackend>) -> Server {
    let pty_backend: Arc<dyn PtyBackend> = fake_backend;
    let (event_tx, event_rx) = mpsc::channel();
    Server::from_runtime_parts(pty_backend, event_rx, event_tx)
}

/// Receive the first inbox event `accepts_event` accepts, dropping the ones before it.
/// Panics once 2 seconds have passed with no accepted event.
fn receive_matching_runtime_event(
    server: &Server,
    mut accepts_event: impl FnMut(&RuntimeEvent) -> bool,
) -> RuntimeEvent {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("event did not arrive in time");
        let runtime_event = server
            .inbox_rx()
            .recv_timeout(remaining)
            .expect("event did not arrive in time");
        if accepts_event(&runtime_event) {
            return runtime_event;
        }
    }
}

#[test]
fn bootstrap_opens_one_shell_and_marks_a_frame_due() {
    let fake_backend = Arc::new(FakePtyBackend::new());
    let mut server = build_server_with_backend(fake_backend.clone());

    let client_id = server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");

    assert_eq!(server.list_sessions().len(), 1);
    let pane_ids = fake_backend.list_spawned_pane_ids();
    assert_eq!(pane_ids.len(), 1);
    assert!(server.list_terminal_engines().contains_key(&pane_ids[0]));
    assert!(server.has_active_panes());

    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    assert_eq!(render_snapshot.pane_snapshots.len(), 1);

    // Genesis invalidated layout, so the first frame is due immediately.
    assert!(server.poll_render(Instant::now()));
}

#[test]
fn pty_output_is_forwarded_into_the_inbox() {
    let fake_backend = Arc::new(FakePtyBackend::new());
    let mut server = build_server_with_backend(fake_backend.clone());
    server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake_backend.list_spawned_pane_ids()[0];

    fake_backend
        .push_output(pane_id, b"hi".to_vec())
        .expect("push");

    let runtime_event = receive_matching_runtime_event(&server, |runtime_event| {
        matches!(runtime_event, RuntimeEvent::PtyOutput { .. })
    });
    match runtime_event {
        RuntimeEvent::PtyOutput {
            pane_id: received,
            output_bytes,
        } => {
            assert_eq!(received, pane_id);
            assert_eq!(output_bytes, b"hi");
        }
        unexpected_event => panic!("expected PtyOutput, got {unexpected_event:?}"),
    }
}

#[test]
fn pty_output_received_through_the_inbox_reaches_the_client_snapshot() {
    let fake_backend = Arc::new(FakePtyBackend::new());
    let mut server = build_server_with_backend(fake_backend.clone());
    let client_id = server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake_backend.list_spawned_pane_ids()[0];

    // Full slice: fake child writes bytes -> forwarder thread relays them
    // through the real inbox channel -> the dispatcher applies them to the
    // pane's terminal engine -> a client-facing snapshot shows the result.
    fake_backend
        .push_output(pane_id, b"hi".to_vec())
        .expect("push");
    let runtime_event = receive_matching_runtime_event(&server, |runtime_event| {
        matches!(runtime_event, RuntimeEvent::PtyOutput { .. })
    });
    let RuntimeEvent::PtyOutput {
        pane_id: got_pane,
        output_bytes,
    } = runtime_event
    else {
        unreachable!("matched above")
    };
    server.handle_pty_output(got_pane, &output_bytes);

    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    let pane_snapshot = render_snapshot
        .pane_snapshots
        .iter()
        .find(|pane_snapshot| pane_snapshot.pane_id == pane_id)
        .expect("pane in snapshot");
    let terminal_grid = &pane_snapshot
        .terminal_grid_view
        .as_ref()
        .expect("terminal grid view")
        .grid;
    assert_eq!(
        terminal_grid
            .get_cell(0, 0)
            .map(|cell| cell.get_character()),
        Some('h')
    );
    assert_eq!(
        terminal_grid
            .get_cell(0, 1)
            .map(|cell| cell.get_character()),
        Some('i')
    );
}

#[test]
fn typed_keys_write_to_the_focused_pane() {
    let fake_backend = Arc::new(FakePtyBackend::new());
    let mut server = build_server_with_backend(fake_backend.clone());
    let client_id = server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake_backend.list_spawned_pane_ids()[0];

    // `ls` + Enter, key by key. The viewer resolves each one, binds none of
    // them, and hands the press to the session, which writes it to the focused
    // pane as it is made.
    let mut viewer = koshi_client::Client::from_client_id_and_viewport(
        client_id,
        TEST_VIEWPORT_SIZE,
        server.subscribe(client_id, EventFilter::All),
        TerminalCleanupGuard::new(),
    );
    for key in [Key::Char('l'), Key::Char('s'), Key::Named(NamedKey::Enter)] {
        let chord = KeyChord::from_parts(ModFlags::NONE, key);
        match viewer.resolve_key(chord, Instant::now()) {
            KeyOutcome::PassThrough(chord) => server.handle_key_press(client_id, chord),
            unexpected_key_outcome => panic!(
                "`{chord}` binds nothing, so it passes through; got {unexpected_key_outcome:?}"
            ),
        }
    }

    assert_eq!(
        fake_backend.list_pane_write_bytes(pane_id).expect("writes"),
        vec![b"l".to_vec(), b"s".to_vec(), b"\r".to_vec()]
    );
}

#[test]
fn child_exit_is_forwarded_and_ends_the_last_pane() {
    let fake_backend = Arc::new(FakePtyBackend::new());
    let mut server = build_server_with_backend(fake_backend.clone());
    server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake_backend.list_spawned_pane_ids()[0];

    // Model child death: PTY EOF, then the exit. The forwarder relays the exit
    // only after output is drained.
    fake_backend.close_output(pane_id).expect("close output");
    fake_backend
        .trigger_child_exit(pane_id, ExitStatus::ExitCode(0))
        .expect("exit");

    let runtime_event = receive_matching_runtime_event(&server, |runtime_event| {
        matches!(runtime_event, RuntimeEvent::ChildExit { .. })
    });
    let RuntimeEvent::ChildExit {
        pane_id: exited,
        exit_status,
    } = runtime_event
    else {
        unreachable!("matched above")
    };
    assert_eq!(exited, pane_id);
    assert_eq!(exit_status, ExitStatus::ExitCode(0));

    // Applying the exit removes the only pane, so the loop's exit condition trips.
    let _ = server.handle_child_exit(exited, exit_status);
    assert!(!server.has_active_panes());
}

#[test]
fn trailing_output_is_forwarded_before_the_exit() {
    let fake_backend = Arc::new(FakePtyBackend::new());
    let mut server = build_server_with_backend(fake_backend.clone());
    server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake_backend.list_spawned_pane_ids()[0];

    // The child writes, its PTY reaches end of file, then it exits. The single
    // relay delivers the output before the exit.
    fake_backend
        .push_output(pane_id, b"bye".to_vec())
        .expect("push");
    fake_backend.close_output(pane_id).expect("close output");
    fake_backend
        .trigger_child_exit(pane_id, ExitStatus::ExitCode(3))
        .expect("exit");

    let first_event = server
        .inbox_rx()
        .recv_timeout(Duration::from_secs(2))
        .expect("first event");
    match first_event {
        RuntimeEvent::PtyOutput {
            pane_id: received,
            output_bytes,
        } => {
            assert_eq!(received, pane_id);
            assert_eq!(output_bytes, b"bye");
        }
        unexpected_event => panic!("expected PtyOutput, got {unexpected_event:?}"),
    }
    let second_event = server
        .inbox_rx()
        .recv_timeout(Duration::from_secs(2))
        .expect("second event");
    match second_event {
        RuntimeEvent::ChildExit {
            pane_id: exited,
            exit_status,
        } => {
            assert_eq!(exited, pane_id);
            assert_eq!(exit_status, ExitStatus::ExitCode(3));
        }
        unexpected_event => panic!("expected ChildExit, got {unexpected_event:?}"),
    }
}

#[test]
fn kill_all_panes_group_kills_the_shell() {
    let fake_backend = Arc::new(FakePtyBackend::new());
    let mut server = build_server_with_backend(fake_backend.clone());
    server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake_backend.list_spawned_pane_ids()[0];

    // The panic-path teardown group-kills every pane's child, reaping its
    // descendants.
    server.kill_all_panes();

    assert_eq!(
        fake_backend
            .list_pane_kill_policies(pane_id)
            .expect("kills"),
        vec![KillPolicy::Tree]
    );
}

#[test]
fn shutdown_drains_and_graceful_group_kills_each_pane() {
    let fake_backend = Arc::new(FakePtyBackend::new());
    let mut server = build_server_with_backend(fake_backend.clone());
    server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake_backend.list_spawned_pane_ids()[0];

    server.shutdown();

    assert!(server.is_draining(), "stage 1 must enter draining mode");
    assert_eq!(
        fake_backend
            .list_pane_kill_policies(pane_id)
            .expect("kills"),
        vec![KillPolicy::GracefulTree {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION,
        }],
        "each pane's child is graceful-then-group-killed on shutdown",
    );
}

#[test]
fn shutdown_with_no_panes_drains_without_hanging() {
    let fake_backend = Arc::new(FakePtyBackend::new());
    let mut server = build_server_with_backend(fake_backend);
    // No bootstrap: no panes are parked. Shutdown must still drain and return.
    server.shutdown();

    assert!(server.is_draining());
    assert!(!server.has_active_panes());
}
