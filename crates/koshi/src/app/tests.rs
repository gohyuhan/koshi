//! Tests for the event loop and its handlers, driven headlessly: a fake PTY
//! backend stands in for real children and ratatui's `TestBackend` renders into
//! an in-memory buffer, so the real `run_loop`, `render`, and the server's
//! inbox routing run without a terminal. Only the crossterm terminal I/O and
//! the input thread's `event::read` — both TTY-bound — are out of reach here;
//! key decoding is covered separately in `keys::tests`.

use super::*;

use std::time::Duration;

use ratatui::backend::TestBackend;

use koshi_client::input::KeyOutcome;
use koshi_config::layer::{
    PartialColorPalette, PartialKoshiConfig, PartialTerminalConfig, PartialThemeConfig,
};
use koshi_config::types::RgbColor;
use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, ToggleLockModeArgs,
};
use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::geometry::Point;
use koshi_core::ids::{CommandId, PaneId, SessionId};
use koshi_core::key::{Key, KeyChord, ModFlags, NamedKey};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind, ScrollDirection};
use koshi_core::process::{ExitStatus, KillPolicy};
use koshi_renderer::snapshot::ViewerChrome;
use koshi_renderer::{hit_test, pane_local_cell, HitRegion};
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::config::LoadedConfig;

const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// A server driven by `fake`, plus a sender clone so a test can inject inbox
/// events the way the input thread and forwarders do.
fn test_server(fake: Arc<FakePtyBackend>) -> (Server, mpsc::Sender<RuntimeEvent>) {
    test_server_with(fake, None)
}

/// The same, on the `koshi.kdl` layer `app` stands for, built the way the
/// launch builds it — through [`session`].
fn test_server_with(
    fake: Arc<FakePtyBackend>,
    app: Option<PartialKoshiConfig>,
) -> (Server, mpsc::Sender<RuntimeEvent>) {
    let backend: Arc<dyn PtyBackend> = fake;
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, rx) = mpsc::channel();
    let server = session(backend, snapshot_provider, storage, rx, tx.clone(), app);
    (server, tx)
}

#[test]
fn the_launch_hands_the_session_the_app_config_file_it_read() {
    // `koshi.kdl`'s session-owned settings decide what every pane runs. A
    // launch that built the session without them would spawn the stock shell
    // over the one the user configured.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx) = test_server_with(
        fake.clone(),
        Some(PartialKoshiConfig {
            terminal: Some(PartialTerminalConfig {
                term: None,
                colorterm: None,
                default_shell: Some(Some("/bin/fish".to_owned())),
            }),
            ..PartialKoshiConfig::default()
        }),
    );

    server
        .bootstrap_local(SessionId::new(), VIEWPORT, SystemTime::now())
        .expect("bootstrap");

    let pane = fake.spawned_panes()[0];
    let spec = fake.spawn_spec(pane).expect("spawn spec");
    assert_eq!(spec.program, std::path::Path::new("/bin/fish"));
}

/// A client half for `client_id`, subscribed to `server`'s events, built the
/// way the launch builds it — through [`viewer`] — for tests that drive the
/// real `run_loop`.
fn test_client(server: &mut Server, client_id: ClientId) -> Client {
    test_client_with(server, client_id, LoadedConfig::default())
}

/// The same, on the config files `loaded` stands for.
fn test_client_with(server: &mut Server, client_id: ClientId, loaded: LoadedConfig) -> Client {
    let events = server.subscribe(EventFilter::All);
    viewer(
        client_id,
        VIEWPORT,
        events,
        TerminalCleanupGuard::new(),
        loaded,
    )
}

#[test]
fn the_launch_hands_the_viewer_the_config_files_it_read() {
    // The viewer's settings, colors, and keymap all come from the files the
    // launch read. A launch that built the viewer without them would paint the
    // stock palette over the user's theme and answer the stock keys.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, _pane) = boot(&fake);

    let client = test_client_with(
        &mut server,
        client_id,
        LoadedConfig {
            app: None,
            theme: Some(PartialThemeConfig {
                name: Some("ocean".to_owned()),
                colors: Some(PartialColorPalette {
                    border_focused: Some(RgbColor::new(0xff, 0, 0)),
                    ..PartialColorPalette::default()
                }),
            }),
            keybindings: None,
        },
    );

    assert_eq!(client.config().theme.name, "ocean");
    assert_eq!(
        client.theme().border_focused,
        ratatui::style::Color::Rgb(0xff, 0, 0)
    );
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
            if hit_test(snapshot.layout(ViewerChrome::default()), point)
                == (HitRegion::PaneContent { pane_id })
            {
                return point;
            }
        }
    }
    panic!("pane content cell");
}

#[test]
fn the_painted_hint_bar_follows_the_clients_mouse_select_state() {
    // The hint bar is painted from the viewer's own keymap, but which label the
    // mouse-select entry wears depends on session state the frame carries. A
    // frame that dropped that link would keep offering "Mouse Select" while
    // selection was already on.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, _pane_id) = boot(&fake);
    let mut client = test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");

    render(
        &mut terminal,
        &server,
        &mut client,
        &mut String::new(),
        &mut None,
        &mut None,
    )
    .expect("render");
    assert!(screen_text(&terminal).contains("Mouse Select"));

    server.submit_command(CommandEnvelope::new(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        SystemTime::now(),
        Command::ToggleMouseSelect,
    ));
    render(
        &mut terminal,
        &server,
        &mut client,
        &mut String::new(),
        &mut None,
        &mut None,
    )
    .expect("render");

    let painted = screen_text(&terminal);
    assert!(painted.contains("Mouse Unselect"), "{painted}");
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

    let mut client = test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    render(
        &mut terminal,
        &server,
        &mut client,
        &mut String::new(),
        &mut None,
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
    let mut client = test_client(&mut server, client_id);

    // Typing `ls` + Enter through the loop's own routing: the viewer resolves
    // each press, binds none of them, and the session writes each as it is
    // made.
    for key in [Key::Char('l'), Key::Char('s'), Key::Named(NamedKey::Enter)] {
        assert!(apply_event(
            &mut server,
            &mut client,
            RuntimeEvent::KeyInput {
                client_id,
                chord: KeyChord::new(ModFlags::NONE, key),
            },
            None,
        )
        .is_continue());
    }

    assert_eq!(
        fake.writes(pane_id).expect("writes"),
        vec![b"l".to_vec(), b"s".to_vec(), b"\r".to_vec()]
    );
}

#[test]
fn a_key_for_another_client_is_not_resolved_by_this_viewer() {
    // The loop routes only its own viewer's keys. One addressed to a client
    // this process does not drive falls through to the session, which has no
    // keymap and drops it rather than typing it at someone else's pane.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, pane_id) = boot(&fake);
    let mut client = test_client(&mut server, client_id);

    assert!(apply_event(
        &mut server,
        &mut client,
        RuntimeEvent::KeyInput {
            client_id: ClientId::new(),
            chord: KeyChord::new(ModFlags::NONE, Key::Char('x')),
        },
        None,
    )
    .is_continue());

    assert_eq!(fake.writes(pane_id).expect("writes"), Vec::<Vec<u8>>::new());
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

/// How far the client's view of the pane is scrolled back from live output,
/// read off the frame the renderer draws.
fn view_offset(server: &Server, client_id: ClientId, pane_id: PaneId) -> usize {
    server
        .build_snapshot(client_id)
        .expect("snapshot")
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .expect("the pane")
        .grid_view
        .as_ref()
        .expect("a terminal pane")
        .view_offset
}

/// Fill the pane's scrollback so a wheel up has room to move.
fn feed_scrollback(server: &mut Server, pane_id: PaneId, lines: usize) {
    for _ in 0..lines {
        assert!(server
            .handle_runtime_event(RuntimeEvent::PtyOutput {
                pane_id,
                bytes: b"x\r\n".to_vec(),
            })
            .is_continue());
    }
}

#[test]
fn a_wheel_event_is_answered_by_the_viewer_and_run_by_the_session() {
    // The wheel never reaches the session as a raw mouse event. The loop hands
    // it to the viewer with the frame it painted, and runs what comes back — so
    // a loop that stopped asking the viewer would scroll nothing.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, pane_id) = boot(&fake);
    let mut client = test_client(&mut server, client_id);
    feed_scrollback(&mut server, pane_id, 40);
    let at = content_point(&server, client_id, pane_id);
    let frame = MouseFrame::from(server.build_snapshot(client_id).expect("snapshot"));

    assert!(apply_event(
        &mut server,
        &mut client,
        RuntimeEvent::MouseInput {
            client_id,
            mouse: MouseInput {
                kind: MouseKind::Scroll(ScrollDirection::Up),
                at,
                mods: ModFlags::NONE,
            },
        },
        Some(&frame),
    )
    .is_continue());

    // scroll_lines defaults to 3, so one notch moves the view three lines.
    assert_eq!(view_offset(&server, client_id, pane_id), 3);
}

#[test]
fn a_wheel_event_before_the_first_paint_is_dropped() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, pane_id) = boot(&fake);
    let mut client = test_client(&mut server, client_id);
    feed_scrollback(&mut server, pane_id, 40);
    let at = content_point(&server, client_id, pane_id);

    assert!(apply_event(
        &mut server,
        &mut client,
        RuntimeEvent::MouseInput {
            client_id,
            mouse: MouseInput {
                kind: MouseKind::Scroll(ScrollDirection::Up),
                at,
                mods: ModFlags::NONE,
            },
        },
        None,
    )
    .is_continue());

    assert_eq!(
        view_offset(&server, client_id, pane_id),
        0,
        "with no painted frame there is nothing to hit-test the tick against"
    );
}

#[test]
fn a_non_wheel_mouse_event_still_reaches_the_session() {
    // Only the wheel was taken off the session. A press still routes there, and
    // over an unfocused pane it moves focus.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, pane_id) = boot(&fake);
    let mut client = test_client(&mut server, client_id);
    let at = content_point(&server, client_id, pane_id);
    assert!(server
        .build_snapshot(client_id)
        .expect("snapshot")
        .panes
        .iter()
        .all(|pane| pane.selection.is_none()));

    assert!(apply_event(
        &mut server,
        &mut client,
        RuntimeEvent::MouseInput {
            client_id,
            mouse: MouseInput {
                kind: MouseKind::Press(MouseButton::Left),
                at,
                mods: ModFlags::NONE,
            },
        },
        None,
    )
    .is_continue());

    assert_eq!(
        server
            .build_snapshot(client_id)
            .expect("snapshot")
            .client
            .focused_pane,
        Some(pane_id),
        "the press reached the session's own routing"
    );
}

#[test]
fn render_leaves_the_frame_it_painted_for_the_viewer() {
    // The viewer answers a wheel tick from the last painted frame, so the paint
    // must hand it back to the loop — as a `MouseFrame`, which carries no grid.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, _pane_id) = boot(&fake);
    let mut client = test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut last_frame = None;

    render(
        &mut terminal,
        &server,
        &mut client,
        &mut String::new(),
        &mut None,
        &mut last_frame,
    )
    .expect("render");

    let painted = server.build_snapshot(client_id).expect("snapshot");
    assert_eq!(last_frame, Some(MouseFrame::from(painted)));
}

#[test]
fn painting_another_tab_throws_the_strip_peek_away() {
    // Painting is how the viewer learns which tab it is on, so a tab switch by
    // any route reaches it. The peek must be thrown away, not just left
    // unapplied: coming back to the tab it was made on has to start from that
    // tab, or the strip can be scrolled past the tab the user is looking at.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, _pane_id) = boot(&fake);
    let mut client = test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut paint = |server: &Server, client: &mut Client| {
        render(
            &mut terminal,
            server,
            client,
            &mut String::new(),
            &mut None,
            &mut None,
        )
        .expect("render");
    };

    let first_tab = server
        .build_snapshot(client_id)
        .expect("snapshot")
        .client
        .active_tab;
    // A wheel over the tab strip's row peeks this viewer's strip one tab along.
    let frame = MouseFrame::from(server.build_snapshot(client_id).expect("snapshot"));
    client.handle_mouse(
        MouseInput {
            kind: MouseKind::Scroll(ScrollDirection::Down),
            at: Point { x: 40, y: 0 },
            mods: ModFlags::NONE,
        },
        &frame,
        Instant::now(),
    );
    assert_eq!(client.chrome(first_tab).tabline_offset, Some(1), "peeked");

    // A new tab, which the session switches this client to.
    server.submit_command(CommandEnvelope::new(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        SystemTime::now(),
        Command::NewTab(koshi_core::command::NewTabArgs::default()),
    ));
    paint(&server, &mut client);

    // Back to the tab the peek was made on.
    server.submit_command(CommandEnvelope::new(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        SystemTime::now(),
        Command::FocusTab(koshi_core::command::FocusTabArgs {
            target: koshi_core::command::TabTarget::Id(first_tab),
            client: Some(client_id),
        }),
    ));
    paint(&server, &mut client);

    assert_eq!(
        client.chrome(first_tab).tabline_offset,
        None,
        "the peek did not come back with the tab it was made on"
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

    // The explicit quit chord travels the binding path: the viewer resolves
    // `<C-q>` to `core:quit`, which flags zero-grace shutdown, the loop stops
    // on the quit request, and teardown group-kills at once.
    let mut client = test_client(&mut server, client_id);
    let quit = KeyChord::new(ModFlags::CTRL, Key::Char('q'));
    match client.resolve_key(quit, Instant::now()) {
        KeyOutcome::Fire(bound) => server.handle_bound_action(client_id, bound),
        other => panic!("`<C-q>` fires core:quit; got {other:?}"),
    }
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

/// Whether this client has a highlight in `pane_id`, read the way the renderer
/// reads it.
fn has_highlight(server: &Server, client_id: ClientId, pane_id: PaneId) -> bool {
    server
        .build_snapshot(client_id)
        .expect("snapshot")
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .expect("the pane is in the frame")
        .has_selection
}

#[test]
fn a_host_paste_ends_this_viewers_selection_gesture() {
    // The paste key's text belongs to the program in the pane, exactly like a
    // key that falls through, so the highlight gesture over it is over. Without
    // that the pointer's next move would put the highlight the paste just
    // cleared straight back up.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, pane_id) = boot(&fake);
    let mut client = test_client(&mut server, client_id);
    server.handle_pty_output(pane_id, b"hello world");

    let from = content_point(&server, client_id, pane_id);
    let frame = MouseFrame::from(server.build_snapshot(client_id).expect("snapshot"));
    let mouse = |kind, at| RuntimeEvent::MouseInput {
        client_id,
        mouse: MouseInput {
            kind,
            at,
            mods: ModFlags::NONE,
        },
    };

    // Press and drag: a highlight is up and the gesture is still under way.
    let run = |server: &mut Server, client: &mut Client, event| {
        assert!(apply_event(server, client, event, Some(&frame)).is_continue());
    };
    run(
        &mut server,
        &mut client,
        mouse(MouseKind::Press(MouseButton::Left), from),
    );
    let to = Point {
        x: from.x + 4,
        ..from
    };
    run(
        &mut server,
        &mut client,
        mouse(MouseKind::Drag(MouseButton::Left), to),
    );
    assert!(has_highlight(&server, client_id, pane_id), "highlighted");

    // The user hits their terminal's paste key. The text reaches the pane's
    // child, so the session drops the highlight.
    run(
        &mut server,
        &mut client,
        RuntimeEvent::HostPaste {
            client_id,
            text: "pasted".to_string(),
        },
    );
    assert!(!has_highlight(&server, client_id, pane_id));

    // Moving the pointer on asks for nothing: the gesture ended with the paste.
    let further = Point {
        x: from.x + 6,
        ..from
    };
    run(
        &mut server,
        &mut client,
        mouse(MouseKind::Drag(MouseButton::Left), further),
    );
    assert!(
        !has_highlight(&server, client_id, pane_id),
        "no highlight was asked for again"
    );
}

#[test]
fn render_for_a_client_without_a_snapshot_draws_nothing() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, _client_id, _pane_id) = boot(&fake);
    let mut unknown = test_client(&mut server, ClientId::new());
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

    // An unknown client resolves to no snapshot, so render early-returns and
    // leaves the screen blank.
    render(
        &mut terminal,
        &server,
        &mut unknown,
        &mut String::new(),
        &mut None,
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
    let mut client = test_client(&mut server, client_id);
    let mut last_cursor = None;
    render(
        &mut terminal,
        &server,
        &mut client,
        &mut String::new(),
        &mut last_cursor,
        &mut None,
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
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );

    let (reply_tx, reply_rx) = mpsc::channel();
    assert!(server
        .handle_runtime_event(RuntimeEvent::Ipc {
            envelope,
            reply: reply_tx,
        })
        .is_continue());
    assert!(
        matches!(
            reply_rx.try_recv().expect("the dispatcher replies"),
            CommandResult::Ok { .. }
        ),
        "the toggle-lock command's result must ride back on the reply channel"
    );

    assert_eq!(
        server.build_snapshot(client_id).unwrap().client.lock_mode,
        LockMode::Locked,
        "the toggle-lock command dispatched by the Ipc event must take effect"
    );
}

#[test]
fn a_press_event_reaches_the_viewer_and_its_answer_reaches_the_pane() {
    // Every mouse event, not only the wheel, is the viewer's to answer: the
    // loop hands it the frame it painted and runs what came back. Here the
    // program in the focused pane asked for the mouse, so the press is written
    // to it as a report.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, pane_id) = boot(&fake);
    let mut client = test_client(&mut server, client_id);
    // Normal tracking with SGR encoding.
    server.handle_pty_output(pane_id, b"\x1b[?1000h\x1b[?1006h");

    let at = content_point(&server, client_id, pane_id);
    let frame = MouseFrame::from(server.build_snapshot(client_id).expect("snapshot"));
    let (col, row) = pane_local_cell(frame.layout(ViewerChrome::default()), pane_id, at)
        .expect("a pane-local cell");

    assert!(apply_event(
        &mut server,
        &mut client,
        RuntimeEvent::MouseInput {
            client_id,
            mouse: MouseInput {
                kind: MouseKind::Press(MouseButton::Left),
                at,
                mods: ModFlags::NONE,
            },
        },
        Some(&frame),
    )
    .is_continue());

    assert_eq!(
        fake.writes(pane_id).expect("writes"),
        vec![format!("\x1b[<0;{col};{row}M").into_bytes()],
        "the press the viewer answered was written to the pane"
    );
}

#[test]
fn a_mouse_event_for_another_client_is_not_answered_by_this_viewer() {
    // The loop routes only its own viewer's mouse. One addressed to a client
    // this process does not drive falls through to the session, which has no
    // frame to answer it from and drops it.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx, client_id, pane_id) = boot(&fake);
    let mut client = test_client(&mut server, client_id);
    server.handle_pty_output(pane_id, b"\x1b[?1000h\x1b[?1006h");

    let at = content_point(&server, client_id, pane_id);
    let frame = MouseFrame::from(server.build_snapshot(client_id).expect("snapshot"));

    assert!(apply_event(
        &mut server,
        &mut client,
        RuntimeEvent::MouseInput {
            client_id: ClientId::new(),
            mouse: MouseInput {
                kind: MouseKind::Press(MouseButton::Left),
                at,
                mods: ModFlags::NONE,
            },
        },
        Some(&frame),
    )
    .is_continue());

    assert_eq!(fake.writes(pane_id).expect("writes"), Vec::<Vec<u8>>::new());
}
