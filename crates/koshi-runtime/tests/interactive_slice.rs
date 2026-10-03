//! Integration tests for one interactive pane: genesis, PTY output forwarding,
//! typed input, child-exit forwarding, and the shutdown kill. They drive the
//! public `Server` surface the binary's loop uses, over a fake PTY backend.

mod common;

use std::time::{Duration, Instant, SystemTime};

use common::{build_server_with_fake_pty_backend, TEST_VIEWPORT_SIZE};
use koshi_client::input::KeyOutcome;
use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::ids::SessionId;
use koshi_core::key::{BindingModifierFlags, Key, KeyChord, NamedKey};
use koshi_core::process::{ExitStatus, KillPolicy};
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::server::Server;
use koshi_test_support::fixtures::build_key_input_for_chord;

/// Receive the first inbox event `accepts_event` accepts, dropping the ones before it.
/// Panics once 2 seconds have passed with no accepted event.
fn receive_matching_runtime_event(
    server: &Server,
    mut accepts_event: impl FnMut(&RuntimeEvent) -> bool,
) -> RuntimeEvent {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining_wait_duration = deadline
            .checked_duration_since(Instant::now())
            .expect("event did not arrive in time");
        let runtime_event = server
            .get_inbox_receiver()
            .recv_timeout(remaining_wait_duration)
            .expect("event did not arrive in time");
        if accepts_event(&runtime_event) {
            return runtime_event;
        }
    }
}

#[test]
fn bootstrap_opens_one_shell_and_marks_a_frame_due() {
    let (mut server, fake_pty_backend, _) = build_server_with_fake_pty_backend();

    let client_id = server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");

    assert_eq!(server.list_sessions().len(), 1);
    let pane_ids = fake_pty_backend.list_spawned_pane_ids();
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
    let (mut server, fake_pty_backend, _) = build_server_with_fake_pty_backend();
    server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake_pty_backend.list_spawned_pane_ids()[0];

    fake_pty_backend
        .push_output(pane_id, b"hi".to_vec())
        .expect("push");

    let runtime_event = receive_matching_runtime_event(&server, |runtime_event| {
        matches!(runtime_event, RuntimeEvent::PtyOutput { .. })
    });
    match runtime_event {
        RuntimeEvent::PtyOutput {
            pane_id: received_pane_id,
            output_bytes,
        } => {
            assert_eq!(received_pane_id, pane_id);
            assert_eq!(output_bytes, b"hi");
        }
        unexpected_event => panic!("expected PtyOutput, got {unexpected_event:?}"),
    }
}

#[test]
fn pty_output_received_through_the_inbox_reaches_the_client_snapshot() {
    let (mut server, fake_pty_backend, _) = build_server_with_fake_pty_backend();
    let client_id = server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake_pty_backend.list_spawned_pane_ids()[0];

    // End to end: fake child writes bytes -> the backend's sink queues them on
    // the real inbox channel -> the dispatcher applies them to the pane's
    // terminal engine -> a client-facing snapshot shows the result.
    fake_pty_backend
        .push_output(pane_id, b"hi".to_vec())
        .expect("push");
    let runtime_event = receive_matching_runtime_event(&server, |runtime_event| {
        matches!(runtime_event, RuntimeEvent::PtyOutput { .. })
    });
    let RuntimeEvent::PtyOutput {
        pane_id: output_pane_id,
        output_bytes,
    } = runtime_event
    else {
        unreachable!("matched above")
    };
    server.handle_pty_output(output_pane_id, &output_bytes);

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
    let (mut server, fake_pty_backend, _) = build_server_with_fake_pty_backend();
    let client_id = server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake_pty_backend.list_spawned_pane_ids()[0];

    // `ls` + Enter, key by key. The viewer resolves each one, binds none of
    // them, and hands the press to the session, which writes it to the focused
    // pane as it is made.
    let mut viewer = koshi_client::Client::from_client_id_and_viewport_size(
        client_id,
        TEST_VIEWPORT_SIZE,
        server.subscribe(client_id),
        TerminalCleanupGuard::new(),
    );
    for key in [Key::Char('l'), Key::Char('s'), Key::Named(NamedKey::Enter)] {
        let key_chord = KeyChord::from_parts(BindingModifierFlags::NONE, key);
        match viewer.resolve_key(key_chord, Instant::now()) {
            KeyOutcome::PassThrough(passthrough_key_chord) => {
                server
                    .handle_key_input(client_id, &build_key_input_for_chord(passthrough_key_chord));
            }
            unexpected_key_outcome => panic!(
                "`{key_chord}` binds nothing and passes through; got {unexpected_key_outcome:?}"
            ),
        }
    }

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![b"l".to_vec(), b"s".to_vec(), b"\r".to_vec()]
    );
}

#[test]
fn child_exit_is_forwarded_and_ends_the_last_pane() {
    let (mut server, fake_pty_backend, _) = build_server_with_fake_pty_backend();
    server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake_pty_backend.list_spawned_pane_ids()[0];

    fake_pty_backend
        .trigger_child_exit(pane_id, ExitStatus::ExitCode(0))
        .expect("exit");

    let runtime_event = receive_matching_runtime_event(&server, |runtime_event| {
        matches!(runtime_event, RuntimeEvent::ChildExit { .. })
    });
    let RuntimeEvent::ChildExit {
        pane_id: exited_pane_id,
        exit_status,
    } = runtime_event
    else {
        unreachable!("matched above")
    };
    assert_eq!(exited_pane_id, pane_id);
    assert_eq!(exit_status, ExitStatus::ExitCode(0));

    // Applying the exit removes the only pane, and the server has no active pane.
    let _ = server.handle_child_exit(exited_pane_id, exit_status);
    assert!(!server.has_active_panes());
}

#[test]
fn trailing_output_is_forwarded_before_the_exit() {
    let (mut server, fake_pty_backend, _) = build_server_with_fake_pty_backend();
    server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake_pty_backend.list_spawned_pane_ids()[0];

    // The child writes, then exits: the output reaches the inbox before the exit.
    fake_pty_backend
        .push_output(pane_id, b"bye".to_vec())
        .expect("push");
    fake_pty_backend
        .trigger_child_exit(pane_id, ExitStatus::ExitCode(3))
        .expect("exit");

    let first_runtime_event = server
        .get_inbox_receiver()
        .recv_timeout(Duration::from_secs(2))
        .expect("first event");
    match first_runtime_event {
        RuntimeEvent::PtyOutput {
            pane_id: received_pane_id,
            output_bytes,
        } => {
            assert_eq!(received_pane_id, pane_id);
            assert_eq!(output_bytes, b"bye");
        }
        unexpected_event => panic!("expected PtyOutput, got {unexpected_event:?}"),
    }
    let second_runtime_event = server
        .get_inbox_receiver()
        .recv_timeout(Duration::from_secs(2))
        .expect("second event");
    match second_runtime_event {
        RuntimeEvent::ChildExit {
            pane_id: exited_pane_id,
            exit_status,
        } => {
            assert_eq!(exited_pane_id, pane_id);
            assert_eq!(exit_status, ExitStatus::ExitCode(3));
        }
        unexpected_event => panic!("expected ChildExit, got {unexpected_event:?}"),
    }
}

#[test]
fn kill_all_panes_group_kills_the_shell() {
    let (mut server, fake_pty_backend, _) = build_server_with_fake_pty_backend();
    server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake_pty_backend.list_spawned_pane_ids()[0];

    // The panic-path teardown group-kills every pane's child, reaping its
    // descendants.
    server.kill_all_panes();

    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(pane_id)
            .expect("kills"),
        vec![KillPolicy::Tree]
    );
}

#[test]
fn shutdown_graceful_group_kills_each_pane() {
    let (mut server, fake_pty_backend, _) = build_server_with_fake_pty_backend();
    server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake_pty_backend.list_spawned_pane_ids()[0];

    server.shutdown();

    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(pane_id)
            .expect("kills"),
        vec![KillPolicy::GracefulTree {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION,
        }],
        "each pane's child is graceful-then-group-killed on shutdown",
    );
}

#[test]
fn shutdown_with_no_panes_returns_without_hanging() {
    let (mut server, _, _) = build_server_with_fake_pty_backend();
    // No bootstrap: the server holds no pane. Shutdown still returns.
    server.shutdown();

    assert!(!server.has_active_panes());
}
