//! Integration smoke for the one-pane interactive slice: genesis, PTY output
//! forwarding, typed input, child-exit forwarding, and shutdown kill — driven
//! through a fake PTY backend, exercising the public `Server` surface the
//! binary's loop uses.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::SessionId;
use koshi_core::key::{Key, KeyChord, ModFlags, NamedKey};
use koshi_core::process::{ExitStatus, KillPolicy};
use koshi_pty::backend::state::PtyBackend;
use koshi_runtime::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::server::Server;
use koshi_test_support::fake_pty::FakePtyBackend;

const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// A server driven by `fake`, holding its own inbox with a forwarder-facing
/// sender clone — the shape the binary constructs.
fn server_with(fake: Arc<FakePtyBackend>) -> Server {
    let backend: Arc<dyn PtyBackend> = fake;
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, rx) = mpsc::channel();
    Server::new(
        backend,
        snapshot_provider,
        storage,
        rx,
        tx,
        Direction::Right,
    )
}

/// Receive the next event of the wanted shape, ignoring any earlier ones, or
/// panic on timeout. The single relay forwards a pane's output before its exit,
/// so a `ChildExit` is preceded by any trailing output.
fn recv_matching(rt: &Server, mut want: impl FnMut(&RuntimeEvent) -> bool) -> RuntimeEvent {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("event did not arrive in time");
        let event = rt
            .inbox_rx()
            .recv_timeout(remaining)
            .expect("event did not arrive in time");
        if want(&event) {
            return event;
        }
    }
}

#[test]
fn bootstrap_opens_one_shell_and_marks_a_frame_due() {
    let fake = Arc::new(FakePtyBackend::new());
    let mut rt = server_with(fake.clone());

    let client_id = rt
        .bootstrap_local(SessionId::new(), VIEWPORT, SystemTime::now())
        .expect("bootstrap");

    assert_eq!(rt.sessions().len(), 1);
    let panes = fake.spawned_panes();
    assert_eq!(panes.len(), 1);
    assert!(rt.terminal_engines().contains_key(&panes[0]));
    assert!(rt.has_active_panes());

    let snapshot = rt.build_snapshot(client_id).expect("snapshot");
    assert_eq!(snapshot.panes.len(), 1);

    // Genesis invalidated layout, so the first frame is due immediately.
    assert!(rt.poll_render(Instant::now()));
}

#[test]
fn pty_output_is_forwarded_into_the_inbox() {
    let fake = Arc::new(FakePtyBackend::new());
    let mut rt = server_with(fake.clone());
    rt.bootstrap_local(SessionId::new(), VIEWPORT, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake.spawned_panes()[0];

    fake.push_output(pane_id, b"hi".to_vec()).expect("push");

    let event = recv_matching(&rt, |e| matches!(e, RuntimeEvent::PtyOutput { .. }));
    match event {
        RuntimeEvent::PtyOutput {
            pane_id: received,
            bytes,
        } => {
            assert_eq!(received, pane_id);
            assert_eq!(bytes, b"hi");
        }
        other => panic!("expected PtyOutput, got {other:?}"),
    }
}

#[test]
fn pty_output_received_through_the_inbox_reaches_the_client_snapshot() {
    let fake = Arc::new(FakePtyBackend::new());
    let mut rt = server_with(fake.clone());
    let client_id = rt
        .bootstrap_local(SessionId::new(), VIEWPORT, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake.spawned_panes()[0];

    // Full slice: fake child writes bytes -> forwarder thread relays them
    // through the real inbox channel -> the dispatcher applies them to the
    // pane's terminal engine -> a client-facing snapshot shows the result.
    fake.push_output(pane_id, b"hi".to_vec()).expect("push");
    let event = recv_matching(&rt, |e| matches!(e, RuntimeEvent::PtyOutput { .. }));
    let RuntimeEvent::PtyOutput {
        pane_id: got_pane,
        bytes,
    } = event
    else {
        unreachable!("matched above")
    };
    rt.handle_pty_output(got_pane, &bytes);

    let snapshot = rt.build_snapshot(client_id).expect("snapshot");
    let pane = snapshot
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .expect("pane in snapshot");
    let grid = &pane.grid_view.as_ref().expect("grid view").grid;
    assert_eq!(grid.cell(0, 0).map(|c| c.ch()), Some('h'));
    assert_eq!(grid.cell(0, 1).map(|c| c.ch()), Some('i'));
}

#[test]
fn typed_keys_write_to_the_focused_pane() {
    let fake = Arc::new(FakePtyBackend::new());
    let mut rt = server_with(fake.clone());
    let client_id = rt
        .bootstrap_local(SessionId::new(), VIEWPORT, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake.spawned_panes()[0];

    // `ls` + Enter, key by key: none is bound, so each falls through to the
    // focused pane and is written as it is pressed.
    for key in [Key::Char('l'), Key::Char('s'), Key::Named(NamedKey::Enter)] {
        rt.handle_key_input(
            client_id,
            KeyChord::new(ModFlags::NONE, key),
            Instant::now(),
        );
    }

    assert_eq!(
        fake.writes(pane_id).expect("writes"),
        vec![b"l".to_vec(), b"s".to_vec(), b"\r".to_vec()]
    );
}

#[test]
fn child_exit_is_forwarded_and_ends_the_last_pane() {
    let fake = Arc::new(FakePtyBackend::new());
    let mut rt = server_with(fake.clone());
    rt.bootstrap_local(SessionId::new(), VIEWPORT, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake.spawned_panes()[0];

    // Model child death: PTY EOF, then the exit. The forwarder relays the exit
    // only after output is drained.
    fake.close_output(pane_id).expect("close output");
    fake.trigger_child_exit(pane_id, ExitStatus::ExitCode(0))
        .expect("exit");

    let event = recv_matching(&rt, |e| matches!(e, RuntimeEvent::ChildExit { .. }));
    let RuntimeEvent::ChildExit {
        pane_id: exited,
        status,
        exited_at,
    } = event
    else {
        unreachable!("matched above")
    };
    assert_eq!(exited, pane_id);
    assert_eq!(status, ExitStatus::ExitCode(0));

    // Applying the exit removes the only pane, so the loop's exit condition trips.
    let _ = rt.handle_child_exit(exited, status, exited_at);
    assert!(!rt.has_active_panes());
}

#[test]
fn trailing_output_is_forwarded_before_the_exit() {
    let fake = Arc::new(FakePtyBackend::new());
    let mut rt = server_with(fake.clone());
    rt.bootstrap_local(SessionId::new(), VIEWPORT, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake.spawned_panes()[0];

    // The child writes, its PTY EOFs, then it exits. The single relay must
    // deliver the output before the exit — never the reverse, which would drop
    // the final output once the engine is torn down.
    fake.push_output(pane_id, b"bye".to_vec()).expect("push");
    fake.close_output(pane_id).expect("close output");
    fake.trigger_child_exit(pane_id, ExitStatus::ExitCode(0))
        .expect("exit");

    let first = rt
        .inbox_rx()
        .recv_timeout(Duration::from_secs(2))
        .expect("first event");
    match first {
        RuntimeEvent::PtyOutput {
            pane_id: received,
            bytes,
        } => {
            assert_eq!(received, pane_id);
            assert_eq!(bytes, b"bye");
        }
        other => panic!("expected PtyOutput, got {other:?}"),
    }
    let second = rt
        .inbox_rx()
        .recv_timeout(Duration::from_secs(2))
        .expect("second event");
    assert!(matches!(second, RuntimeEvent::ChildExit { .. }));
}

#[test]
fn kill_all_panes_group_kills_the_shell() {
    let fake = Arc::new(FakePtyBackend::new());
    let mut rt = server_with(fake.clone());
    rt.bootstrap_local(SessionId::new(), VIEWPORT, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake.spawned_panes()[0];

    // The panic-path teardown group-kills so no descendant is orphaned.
    rt.kill_all_panes();

    assert_eq!(fake.kills(pane_id).expect("kills"), vec![KillPolicy::Tree]);
}

#[test]
fn shutdown_drains_and_graceful_group_kills_each_pane() {
    let fake = Arc::new(FakePtyBackend::new());
    let mut rt = server_with(fake.clone());
    rt.bootstrap_local(SessionId::new(), VIEWPORT, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake.spawned_panes()[0];

    rt.shutdown();

    assert!(rt.is_draining(), "stage 1 must enter draining mode");
    assert_eq!(
        fake.kills(pane_id).expect("kills"),
        vec![KillPolicy::GracefulTree {
            timeout: GRACEFUL_TIMEOUT_DURATION,
        }],
        "each pane's child is graceful-then-group-killed on shutdown",
    );
}

#[test]
fn shutdown_with_no_panes_drains_without_hanging() {
    let fake = Arc::new(FakePtyBackend::new());
    let mut rt = server_with(fake);
    // No bootstrap: no panes are parked. Shutdown must still drain and return.
    rt.shutdown();

    assert!(rt.is_draining());
    assert!(!rt.has_active_panes());
}
