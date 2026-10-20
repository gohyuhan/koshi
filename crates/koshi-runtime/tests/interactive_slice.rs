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
use koshi_runtime::runtime::driver::DispatchPassOutcome;
use koshi_test_support::fixtures::build_key_input_for_chord;

#[test]
fn bootstrap_opens_one_shell_and_its_first_subscriber_gets_a_frame_at_once() {
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

    // A new subscriber is owed its first frame, which is due immediately.
    let (_delivery_receiver, _writer_handoff) = server.subscribe(client_id, 60);
    assert_eq!(
        server.compute_next_render_wakeup(Instant::now()),
        Some(Duration::ZERO)
    );
}

#[test]
fn pty_output_reaches_the_client_snapshot_in_one_dispatch_pass() {
    let (mut server, fake_pty_backend, _) = build_server_with_fake_pty_backend();
    let client_id = server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake_pty_backend.list_spawned_pane_ids()[0];

    // End to end: the fake child writes two chunks -> the backend's sink
    // queues both on the pane's output queue and sends a wake into the real
    // inbox -> one dispatch pass takes the wake and gives the pane its turn
    // -> a client-facing snapshot shows the result.
    fake_pty_backend
        .push_output(pane_id, b"h".to_vec())
        .expect("push");
    fake_pty_backend
        .push_output(pane_id, b"i".to_vec())
        .expect("push");
    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);

    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    let pane_snapshot = render_snapshot
        .find_pane_snapshot(pane_id)
        .expect("pane in snapshot");
    let terminal_grid = pane_snapshot
        .terminal_grid_view
        .as_ref()
        .expect("terminal grid view");
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

    // One pass takes the wake and the exit: the only pane is removed, and the
    // server has no active pane.
    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);
    assert!(!server.has_active_panes());
}

#[test]
fn trailing_output_is_parsed_before_the_exit() {
    let (mut server, fake_pty_backend, _) = build_server_with_fake_pty_backend();
    server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake_pty_backend.list_spawned_pane_ids()[0];

    // The child asks for its status (`CSI 5 n`), then exits. The engine reads
    // the question and writes `CSI 0 n` back before the exit removes it.
    fake_pty_backend
        .push_output(pane_id, b"\x1b[5n".to_vec())
        .expect("push");
    fake_pty_backend
        .trigger_child_exit(pane_id, ExitStatus::ExitCode(3))
        .expect("exit");

    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![b"\x1b[0n".to_vec()]
    );
    assert!(!server.has_active_panes());
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
