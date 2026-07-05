//! Integration smoke for the one-pane interactive slice: genesis, PTY output
//! forwarding, outer input, child-exit forwarding, and shutdown kill — driven
//! through a fake PTY backend, exercising the public `Runtime` surface the
//! binary's loop uses.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tile_core::geometry::Size;
use tile_core::process::{ExitStatus, KillPolicy};
use tile_observability::cleanup::TerminalCleanupGuard;
use tile_pty::backend::state::PtyBackend;
use tile_runtime::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use tile_runtime::runtime::event::RuntimeEvent;
use tile_runtime::runtime::state::Runtime;
use tile_test_support::fake_pty::FakePtyBackend;

const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// A runtime driven by `fake`, holding its own inbox with a forwarder-facing
/// sender clone — the shape the binary constructs.
fn runtime_with(fake: Arc<FakePtyBackend>) -> Runtime {
    let backend: Arc<dyn PtyBackend> = fake;
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, rx) = mpsc::channel();
    Runtime::new(
        backend,
        snapshot_provider,
        storage,
        rx,
        tx,
        TerminalCleanupGuard::new(),
    )
}

/// Receive the next event of the wanted shape, ignoring any earlier ones, or
/// panic on timeout. The single relay forwards a pane's output before its exit,
/// so a `ChildExit` is preceded by any trailing output.
fn recv_matching(rt: &Runtime, mut want: impl FnMut(&RuntimeEvent) -> bool) -> RuntimeEvent {
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
    let mut rt = runtime_with(fake.clone());

    let client_id = rt
        .bootstrap_local(VIEWPORT, SystemTime::now())
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
    let mut rt = runtime_with(fake.clone());
    rt.bootstrap_local(VIEWPORT, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake.spawned_panes()[0];

    fake.push_output(pane_id, b"hi".to_vec()).expect("push");

    let event = recv_matching(&rt, |e| matches!(e, RuntimeEvent::PtyOutput { .. }));
    assert_eq!(
        event,
        RuntimeEvent::PtyOutput {
            pane_id,
            bytes: b"hi".to_vec(),
        }
    );
}

#[test]
fn outer_input_writes_to_the_focused_pane() {
    let fake = Arc::new(FakePtyBackend::new());
    let mut rt = runtime_with(fake.clone());
    let client_id = rt
        .bootstrap_local(VIEWPORT, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake.spawned_panes()[0];

    rt.handle_outer_input(client_id, b"ls\r");

    assert_eq!(
        fake.writes(pane_id).expect("writes"),
        vec![b"ls\r".to_vec()]
    );
}

#[test]
fn child_exit_is_forwarded_and_ends_the_last_pane() {
    let fake = Arc::new(FakePtyBackend::new());
    let mut rt = runtime_with(fake.clone());
    rt.bootstrap_local(VIEWPORT, SystemTime::now())
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
    let mut rt = runtime_with(fake.clone());
    rt.bootstrap_local(VIEWPORT, SystemTime::now())
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
    assert_eq!(
        first,
        RuntimeEvent::PtyOutput {
            pane_id,
            bytes: b"bye".to_vec(),
        }
    );
    let second = rt
        .inbox_rx()
        .recv_timeout(Duration::from_secs(2))
        .expect("second event");
    assert!(matches!(second, RuntimeEvent::ChildExit { .. }));
}

#[test]
fn kill_all_panes_force_kills_the_shell() {
    let fake = Arc::new(FakePtyBackend::new());
    let mut rt = runtime_with(fake.clone());
    rt.bootstrap_local(VIEWPORT, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake.spawned_panes()[0];

    rt.kill_all_panes();

    assert_eq!(fake.kills(pane_id).expect("kills"), vec![KillPolicy::Force]);
}
