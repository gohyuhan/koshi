//! Tests for the event loop and its handlers, driven headlessly: a fake PTY
//! backend stands in for real children and ratatui's `TestBackend` renders into
//! an in-memory buffer, so the real `run_loop`, `render`, and the server's
//! inbox routing run without a terminal. Only the crossterm terminal I/O and
//! the input thread's `event::read` — both TTY-bound — are out of reach here;
//! key decoding is covered separately in `keys::tests`.

use super::*;

use std::time::Duration;

use ratatui::backend::TestBackend;

use koshi_core::command::{Command, CommandEnvelope, CommandSource};
use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::geometry::Point;
use koshi_core::ids::{CommandId, PaneId, SessionId};
use koshi_core::key::{Key, KeyChord, ModFlags, NamedKey};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind};
use koshi_core::process::{ExitStatus, KillPolicy};
use koshi_renderer::{hit_test, HitRegion};
use koshi_test_support::fake_pty::FakePtyBackend;

const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// A server driven by `fake`, plus a sender clone so a test can inject inbox
/// events the way the input thread and forwarders do.
fn test_server(fake: Arc<FakePtyBackend>) -> (Server, mpsc::Sender<RuntimeEvent>) {
    let backend: Arc<dyn PtyBackend> = fake;
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, rx) = mpsc::channel();
    let server = Server::new(
        backend,
        snapshot_provider,
        storage,
        rx,
        tx.clone(),
        Direction::Right,
    );
    (server, tx)
}

/// A client half for `client_id`, subscribed to `server`'s events, for tests
/// that drive the real `run_loop`.
fn test_client(server: &mut Server, client_id: ClientId) -> Client {
    let events = server.subscribe(EventFilter::All);
    Client::new(client_id, VIEWPORT, events, TerminalCleanupGuard::new())
}

/// A bootstrapped server with its client id and sole pane id.
fn boot(fake: &Arc<FakePtyBackend>) -> (Server, mpsc::Sender<RuntimeEvent>, ClientId, PaneId) {
    let (mut server, tx) = test_server(fake.clone());
    let client_id = server
        .bootstrap_local(SessionId::new(), VIEWPORT, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake.spawned_panes()[0];
    (server, tx, client_id, pane_id)
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

/// The first screen cell belonging to the sole pane's terminal content.
fn content_point(server: &Server, client_id: ClientId, pane_id: PaneId) -> Point {
    let snapshot = server.build_snapshot(client_id).expect("snapshot");
    for y in 0..snapshot.client.viewport.rows {
        for x in 0..snapshot.client.viewport.cols {
            let point = Point { x, y };
            if hit_test(&snapshot, point) == (HitRegion::PaneContent { pane_id }) {
                return point;
            }
        }
    }
    panic!("pane content cell");
}

#[test]
fn pty_output_event_renders_to_the_screen() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, pane_id) = boot(&fake);

    assert!(server
        .handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            bytes: b"hello".to_vec(),
        },)
        .is_continue());

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    render(
        &mut terminal,
        &server,
        client_id,
        &mut String::new(),
        &mut None,
    )
    .expect("render");

    assert!(
        screen_text(&terminal).contains("hello"),
        "the shell's output should appear on the rendered screen"
    );
}

#[test]
fn each_pane_cursor_style_maps_to_the_crossterm_command_that_re_emits_it() {
    // koshi copies the focused pane's DECSCUSR style out to the terminal it is
    // itself running in, and crossterm writes these commands as the very same
    // DECSCUSR sequences. So each pair must map to the command whose bytes are
    // the sequence that produced it — `CSI 5 SP q` in, `CSI 5 SP q` out.
    // Nothing else in the suite would catch a swapped arm: a `Bar` sent as
    // `BlinkingUnderScore` renders vim's insert cursor as an underline while
    // every test still passes.
    let shaped = |shape, blink| CursorStyle::Shaped { shape, blink };
    let cases = [
        // A pane that asked for nothing hands the cursor back to the user.
        (CursorStyle::UserDefault, SetCursorStyle::DefaultUserShape),
        (
            shaped(CursorShape::Block, true),
            SetCursorStyle::BlinkingBlock,
        ),
        (
            shaped(CursorShape::Block, false),
            SetCursorStyle::SteadyBlock,
        ),
        (
            shaped(CursorShape::Underline, true),
            SetCursorStyle::BlinkingUnderScore,
        ),
        (
            shaped(CursorShape::Underline, false),
            SetCursorStyle::SteadyUnderScore,
        ),
        (shaped(CursorShape::Bar, true), SetCursorStyle::BlinkingBar),
        (shaped(CursorShape::Bar, false), SetCursorStyle::SteadyBar),
    ];
    for (style, expected) in cases {
        assert_eq!(set_cursor_style(style), expected, "{style:?}");
    }
}

#[test]
fn key_input_events_write_to_the_focused_pane() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, pane_id) = boot(&fake);

    // Typing `ls` + Enter: three unbound presses, each written as it is made.
    for key in [Key::Char('l'), Key::Char('s'), Key::Named(NamedKey::Enter)] {
        assert!(server
            .handle_runtime_event(RuntimeEvent::KeyInput {
                client_id,
                chord: KeyChord::new(ModFlags::NONE, key),
            },)
            .is_continue());
    }

    assert_eq!(
        fake.writes(pane_id).expect("writes"),
        vec![b"l".to_vec(), b"s".to_vec(), b"\r".to_vec()]
    );
}

#[test]
fn child_exit_event_removes_the_pane() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, _client_id, pane_id) = boot(&fake);
    assert!(server.has_active_panes());

    let flow = server.handle_runtime_event(RuntimeEvent::ChildExit {
        pane_id,
        status: ExitStatus::ExitCode(0),
        exited_at: SystemTime::now(),
    });

    assert!(flow.is_continue());
    assert!(!server.has_active_panes());
}

#[test]
fn quit_event_breaks_the_loop() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, _client_id, _pane_id) = boot(&fake);

    assert!(server.handle_runtime_event(RuntimeEvent::Quit).is_break());
}

#[test]
fn hangup_quit_keeps_the_graceful_teardown() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, _client_id, pane_id) = boot(&fake);

    // A terminal hangup delivers `RuntimeEvent::Quit`; the following teardown
    // must give children the graceful window — the immediate group-kill is
    // reserved for the explicit `core:quit` command.
    assert!(server.handle_runtime_event(RuntimeEvent::Quit).is_break());
    let outcome: thread::Result<Result<(), <TestBackend as Backend>::Error>> = Ok(Ok(()));
    teardown(&mut server, outcome).expect("teardown");

    assert_eq!(
        fake.kills(pane_id).expect("kills"),
        vec![KillPolicy::GracefulTree {
            timeout: GRACEFUL_TIMEOUT_DURATION,
        }],
    );
}

#[test]
fn run_loop_exits_when_the_shell_exits() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, pane_id) = boot(&fake);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

    // Model child death: the PTY reaches EOF, then the exit fires. The forwarder
    // relays the exit; the loop applies it, finds no pane left, and returns.
    fake.close_output(pane_id).expect("close output");
    fake.trigger_child_exit(pane_id, ExitStatus::ExitCode(0))
        .expect("exit");

    let mut client = test_client(&mut server, client_id);
    run_loop(&mut server, &mut client, &mut terminal).expect("loop");

    assert!(!server.has_active_panes());
}

#[test]
fn teardown_runs_the_staged_shutdown_on_a_normal_exit() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, _client_id, pane_id) = boot(&fake);

    // The loop returned normally: teardown runs the staged shutdown.
    let outcome: thread::Result<Result<(), <TestBackend as Backend>::Error>> = Ok(Ok(()));
    teardown(&mut server, outcome).expect("teardown");

    assert!(
        server.is_draining(),
        "a normal exit runs the staged shutdown"
    );
    assert_eq!(
        fake.kills(pane_id).expect("kills"),
        vec![KillPolicy::GracefulTree {
            timeout: GRACEFUL_TIMEOUT_DURATION,
        }],
    );
}

#[test]
fn teardown_propagates_a_loop_error_after_the_staged_shutdown() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, _client_id, pane_id) = boot(&fake);

    // The loop returned its own I/O error (the crossterm backend's error
    // type): teardown still runs the staged shutdown, then hands the error
    // back for `run` to propagate.
    let outcome: thread::Result<Result<(), io::Error>> = Ok(Err(io::Error::other("draw failed")));
    let err = teardown(&mut server, outcome).expect_err("the loop error propagates");

    assert_eq!(err.to_string(), "draw failed");
    assert!(
        server.is_draining(),
        "a loop error still runs the staged shutdown"
    );
    assert_eq!(
        fake.kills(pane_id).expect("kills"),
        vec![KillPolicy::GracefulTree {
            timeout: GRACEFUL_TIMEOUT_DURATION,
        }],
    );
}

#[test]
fn teardown_group_kills_and_reraises_on_a_panic() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, _client_id, pane_id) = boot(&fake);

    // The loop panicked: teardown takes the abrupt path — immediate group-kill,
    // no staged shutdown, and the panic re-raised.
    let outcome: thread::Result<Result<(), <TestBackend as Backend>::Error>> =
        Err(Box::new("boom"));
    let reraised = catch_unwind(AssertUnwindSafe(|| teardown(&mut server, outcome)));

    assert!(reraised.is_err(), "the original panic is re-raised");
    assert!(
        !server.is_draining(),
        "the panic path skips the staged shutdown"
    );
    assert_eq!(
        fake.kills(pane_id).expect("kills"),
        vec![KillPolicy::Tree],
        "the panic path immediately group-kills",
    );
}

#[test]
fn run_loop_exits_on_a_quit_event() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx, client_id, _pane_id) = boot(&fake);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

    // The input thread sends Quit on terminal hangup; queue it. The shell stays
    // alive, so only the quit event ends the loop.
    tx.send(RuntimeEvent::Quit).expect("queue quit");

    let mut client = test_client(&mut server, client_id);
    run_loop(&mut server, &mut client, &mut terminal).expect("loop");

    assert!(
        server.has_active_panes(),
        "the shell is still alive; the quit event ended the loop"
    );
}

#[test]
fn selection_release_flushes_its_clipboard_write_before_queued_quit() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx, client_id, pane_id) = boot(&fake);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let start = content_point(&server, client_id, pane_id);
    let end = Point {
        x: start.x + 1,
        y: start.y,
    };
    let mouse = |kind, at| RuntimeEvent::MouseInput {
        client_id,
        mouse: MouseInput {
            kind,
            at,
            mods: ModFlags::NONE,
        },
    };

    tx.send(RuntimeEvent::PtyOutput {
        pane_id,
        bytes: b"hi".to_vec(),
    })
    .expect("queue output");
    tx.send(mouse(MouseKind::Press(MouseButton::Left), start))
        .expect("queue press");
    tx.send(mouse(MouseKind::Drag(MouseButton::Left), end))
        .expect("queue drag");
    tx.send(mouse(MouseKind::Release(MouseButton::Left), end))
        .expect("queue release");
    tx.send(RuntimeEvent::Quit).expect("queue quit");

    let mut client = test_client(&mut server, client_id);
    run_loop(&mut server, &mut client, &mut terminal).expect("loop");

    assert_eq!(server.take_host_writes(client_id), None);
}

#[test]
fn resize_event_reflows_before_the_next_queued_quit() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx, client_id, pane_id) = boot(&fake);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
    tx.send(RuntimeEvent::Resize {
        client_id,
        size: Size {
            cols: 100,
            rows: 30,
        },
    })
    .expect("queue resize");
    tx.send(RuntimeEvent::Quit).expect("queue quit");

    let mut client = test_client(&mut server, client_id);
    run_loop(&mut server, &mut client, &mut terminal).expect("loop");

    assert_eq!(
        server.build_snapshot(client_id).unwrap().client.viewport,
        Size {
            cols: 100,
            rows: 30
        }
    );
    assert_eq!(
        *fake.resizes(pane_id).unwrap().last().unwrap(),
        koshi_core::process::PtySize { cols: 98, rows: 26 }
    );
}

#[test]
fn explicit_quit_teardown_group_kills_without_grace_delay() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, pane_id) = boot(&fake);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

    // The explicit quit chord travels the binding path: `core:quit` flags
    // zero-grace shutdown, the loop stops on the quit request, and teardown
    // group-kills at once.
    server.handle_key_input(
        client_id,
        KeyChord::new(ModFlags::CTRL, Key::Char('q')),
        Instant::now(),
    );
    let mut client = test_client(&mut server, client_id);
    run_loop(&mut server, &mut client, &mut terminal).expect("loop");
    let outcome: thread::Result<Result<(), <TestBackend as Backend>::Error>> = Ok(Ok(()));
    teardown(&mut server, outcome).expect("teardown");

    assert_eq!(fake.kills(pane_id).expect("kills"), vec![KillPolicy::Tree]);
}

// --- earliest: the wakeup-timeout picker ---

#[test]
fn earliest_of_two_present_durations_is_the_smaller_either_order() {
    let short = Duration::from_millis(5);
    let long = Duration::from_millis(50);
    assert_eq!(earliest(Some(short), Some(long)), Some(short));
    assert_eq!(earliest(Some(long), Some(short)), Some(short));
}

#[test]
fn earliest_of_two_equal_durations_returns_that_duration() {
    let same = Duration::from_millis(10);
    assert_eq!(earliest(Some(same), Some(same)), Some(same));
}

#[test]
fn earliest_falls_back_to_whichever_single_side_is_present() {
    let only = Duration::from_millis(7);
    assert_eq!(earliest(Some(only), None), Some(only));
    assert_eq!(earliest(None, Some(only)), Some(only));
}

#[test]
fn earliest_of_two_absent_durations_is_none() {
    assert_eq!(earliest(None, None), None);
}

// --- window_title: the outer-terminal title string ---

#[test]
fn window_title_with_no_focused_pane_is_just_the_session_name() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, _tx, client_id, _pane_id) = boot(&fake);
    let mut snapshot = server.build_snapshot(client_id).expect("snapshot");
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = None;

    assert_eq!(window_title(&snapshot), "quiet-lake");
}

#[test]
fn window_title_with_a_titled_focused_pane_joins_session_and_title() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, _tx, client_id, pane_id) = boot(&fake);
    let mut snapshot = server.build_snapshot(client_id).expect("snapshot");
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = Some(pane_id);
    snapshot.panes[0].id = pane_id;
    snapshot.panes[0].title = Some("htop".to_string());

    assert_eq!(window_title(&snapshot), "quiet-lake | htop");
}

#[test]
fn window_title_with_an_empty_pane_title_falls_back_to_the_session_name() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, _tx, client_id, pane_id) = boot(&fake);
    let mut snapshot = server.build_snapshot(client_id).expect("snapshot");
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = Some(pane_id);
    snapshot.panes[0].id = pane_id;
    snapshot.panes[0].title = Some(String::new());

    assert_eq!(window_title(&snapshot), "quiet-lake");
}

#[test]
fn window_title_with_a_focused_pane_absent_from_the_pane_list_falls_back() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, _tx, client_id, pane_id) = boot(&fake);
    let mut snapshot = server.build_snapshot(client_id).expect("snapshot");
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = Some(pane_id);
    // No `PaneSnapshot` carries `pane_id`, so the lookup in `window_title`
    // cannot find a title for it.
    snapshot.panes.clear();

    assert_eq!(window_title(&snapshot), "quiet-lake");
}

// --- inbox routing: the events not covered above ---

#[test]
fn client_attached_event_registers_the_new_client_and_continues() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, _pane_id) = boot(&fake);
    let snapshot = server.build_snapshot(client_id).expect("snapshot");
    let session_id = snapshot.session.id;
    let active_tab = snapshot.session.active_tab.id;
    let new_client = ClientId::new();

    let flow = server.handle_runtime_event(RuntimeEvent::ClientAttached {
        session_id,
        client_id: new_client,
        viewport: VIEWPORT,
        active_tab,
        attached_at: SystemTime::now(),
    });

    assert!(flow.is_continue());
    assert!(
        server.build_snapshot(new_client).is_some(),
        "the newly attached client should now resolve to a snapshot"
    );
}

#[test]
fn client_detached_event_removes_the_client_and_continues() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, _pane_id) = boot(&fake);
    assert!(server.build_snapshot(client_id).is_some());

    let flow = server.handle_runtime_event(RuntimeEvent::ClientDetached { client_id });

    assert!(flow.is_continue());
    assert!(
        server.build_snapshot(client_id).is_none(),
        "the detached client should no longer resolve to a snapshot"
    );
}

#[test]
fn host_paste_event_writes_the_pasted_text_to_the_focused_pane() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, pane_id) = boot(&fake);

    // The default pane has bracketed paste off, so the raw text reaches it.
    assert!(server
        .handle_runtime_event(RuntimeEvent::HostPaste {
            client_id,
            text: "pasted".to_string(),
        },)
        .is_continue());

    assert_eq!(
        fake.writes(pane_id).expect("writes"),
        vec![b"pasted".to_vec()]
    );
}

#[test]
fn render_for_a_client_without_a_snapshot_draws_nothing() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, _tx, _client_id, _pane_id) = boot(&fake);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

    // An unknown client resolves to no snapshot, so render early-returns and
    // leaves the screen blank.
    render(
        &mut terminal,
        &server,
        ClientId::new(),
        &mut String::new(),
        &mut None,
    )
    .expect("render");

    assert_eq!(screen_text(&terminal).trim(), "");
}

#[test]
fn render_emits_a_changed_cursor_style_and_records_it() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, pane_id) = boot(&fake);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

    // The pane asks for a steady bar via DECSCUSR (`CSI 6 SP q`); the first
    // render sees it differ from the starting `None` and records the new style.
    assert!(server
        .handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            bytes: b"\x1b[6 q".to_vec(),
        },)
        .is_continue());
    let mut last_cursor = None;
    render(
        &mut terminal,
        &server,
        client_id,
        &mut String::new(),
        &mut last_cursor,
    )
    .expect("render");

    assert_eq!(
        last_cursor,
        Some(CursorStyle::Shaped {
            shape: CursorShape::Bar,
            blink: false,
        })
    );
}

#[test]
fn timer_event_never_breaks_the_loop() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, _client_id, _pane_id) = boot(&fake);

    assert!(server
        .handle_runtime_event(RuntimeEvent::Timer)
        .is_continue());
}

#[test]
fn ipc_event_dispatches_the_command_and_continues() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, _pane_id) = boot(&fake);
    assert_eq!(
        server.build_snapshot(client_id).unwrap().client.lock_mode,
        LockMode::Normal
    );

    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        SystemTime::now(),
        Command::ToggleLockMode,
    );

    assert!(server
        .handle_runtime_event(RuntimeEvent::Ipc(envelope))
        .is_continue());

    assert_eq!(
        server.build_snapshot(client_id).unwrap().client.lock_mode,
        LockMode::Locked,
        "the toggle-lock command dispatched by the Ipc event must take effect"
    );
}
