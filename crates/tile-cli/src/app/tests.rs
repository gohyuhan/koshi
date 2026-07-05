//! Tests for the event loop and its handlers, driven headlessly: a fake PTY
//! backend stands in for real children and ratatui's `TestBackend` renders into
//! an in-memory buffer, so the real `run_loop`, `render`, and `handle_event`
//! run without a terminal. Only the crossterm terminal I/O and the input
//! thread's `event::read` — both TTY-bound — are out of reach here; key
//! decoding is covered separately in `keys::tests`.

use super::*;

use ratatui::backend::TestBackend;

use tile_core::ids::PaneId;
use tile_core::process::ExitStatus;
use tile_test_support::fake_pty::FakePtyBackend;

const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// A runtime driven by `fake`, plus a sender clone so a test can inject inbox
/// events the way the input thread and forwarders do.
fn test_runtime(fake: Arc<FakePtyBackend>) -> (Runtime, mpsc::Sender<RuntimeEvent>) {
    let backend: Arc<dyn PtyBackend> = fake;
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, rx) = mpsc::channel();
    let runtime = Runtime::new(
        backend,
        snapshot_provider,
        storage,
        rx,
        tx.clone(),
        TerminalCleanupGuard::new(),
    );
    (runtime, tx)
}

/// A bootstrapped runtime with its client id and sole pane id.
fn boot(fake: &Arc<FakePtyBackend>) -> (Runtime, mpsc::Sender<RuntimeEvent>, ClientId, PaneId) {
    let (mut runtime, tx) = test_runtime(fake.clone());
    let client_id = runtime
        .bootstrap_local(VIEWPORT, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake.spawned_panes()[0];
    (runtime, tx, client_id, pane_id)
}

/// The whole rendered screen flattened to a string, for substring assertions.
fn screen_text(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[test]
fn pty_output_event_renders_to_the_screen() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut runtime, _tx, client_id, pane_id) = boot(&fake);

    assert!(handle_event(
        &mut runtime,
        RuntimeEvent::PtyOutput {
            pane_id,
            bytes: b"hello".to_vec(),
        },
    )
    .is_continue());

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    render(&mut terminal, &runtime, client_id).expect("render");

    assert!(
        screen_text(&terminal).contains("hello"),
        "the shell's output should appear on the rendered screen"
    );
}

#[test]
fn outer_input_event_writes_to_the_focused_pane() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut runtime, _tx, client_id, pane_id) = boot(&fake);

    assert!(handle_event(
        &mut runtime,
        RuntimeEvent::OuterInput {
            client_id,
            bytes: b"ls\r".to_vec(),
        },
    )
    .is_continue());

    assert_eq!(
        fake.writes(pane_id).expect("writes"),
        vec![b"ls\r".to_vec()]
    );
}

#[test]
fn child_exit_event_removes_the_pane() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut runtime, _tx, _client_id, pane_id) = boot(&fake);
    assert!(runtime.has_active_panes());

    let flow = handle_event(
        &mut runtime,
        RuntimeEvent::ChildExit {
            pane_id,
            status: ExitStatus::ExitCode(0),
            exited_at: SystemTime::now(),
        },
    );

    assert!(flow.is_continue());
    assert!(!runtime.has_active_panes());
}

#[test]
fn quit_event_breaks_the_loop() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut runtime, _tx, _client_id, _pane_id) = boot(&fake);

    assert!(handle_event(&mut runtime, RuntimeEvent::Quit).is_break());
}

#[test]
fn run_loop_exits_when_the_shell_exits() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut runtime, _tx, client_id, pane_id) = boot(&fake);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

    // Model child death: the PTY reaches EOF, then the exit fires. The forwarder
    // relays the exit; the loop applies it, finds no pane left, and returns.
    fake.close_output(pane_id).expect("close output");
    fake.trigger_child_exit(pane_id, ExitStatus::ExitCode(0))
        .expect("exit");

    run_loop(&mut runtime, &mut terminal, client_id).expect("loop");

    assert!(!runtime.has_active_panes());
}

#[test]
fn run_loop_exits_on_a_quit_event() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut runtime, tx, client_id, _pane_id) = boot(&fake);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

    // The input thread sends Quit on Ctrl-Q; queue it. The shell stays alive, so
    // only the quit event ends the loop.
    tx.send(RuntimeEvent::Quit).expect("queue quit");

    run_loop(&mut runtime, &mut terminal, client_id).expect("loop");

    assert!(
        runtime.has_active_panes(),
        "the shell is still alive; the quit event ended the loop"
    );
}
