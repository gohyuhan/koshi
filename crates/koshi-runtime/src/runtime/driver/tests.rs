//! Tests for the loop-facing driver surface: render-wakeup timing and
//! poll delegation to the scheduler, the live-pane check, the routing of an
//! attached client's key press and pasted text, the inbox events that are
//! dropped or answered on their reply channel, and the abrupt group-kill the
//! panic path takes.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::SystemTime;

use koshi_core::command::{Command, CommandEnvelope, CommandSource, ToggleLockModeArgs};
use koshi_core::geometry::{Point, Size};
use koshi_core::ids::{CommandId, PaneId, SessionId};
use koshi_core::key::{Key, KeyChord, ModFlags};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind};
use koshi_core::process::{PtySize, SpawnSpec};
use koshi_pty::backend::state::PtyBackend;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::runtime::event::RuntimeEvent;
use crate::runtime::render_schedule::FRAME_INTERVAL;

use super::*;

const PANE_SIZE: PtySize = PtySize { cols: 80, rows: 24 };

/// A runtime sharing one fake backend, returned alongside it so a test can
/// assert on the kills the driver issues. The sender keeps the inbox open.
fn new_runtime_with_fake() -> (Server, Arc<FakePtyBackend>, mpsc::Sender<RuntimeEvent>) {
    let fake = Arc::new(FakePtyBackend::new());
    let pty_backend: Arc<dyn PtyBackend> = fake.clone();
    let (tx, inbox_rx) = mpsc::channel();
    let runtime = Server::new(pty_backend, inbox_rx, tx.clone());
    (runtime, fake, tx)
}

/// Spawn a pane in the fake backend and park its handle in the runtime, so the
/// pane is live in both — the backend can record kills and the runtime counts
/// it as active.
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

#[test]
fn no_panes_are_active_before_any_pane_is_parked() {
    let (rt, _fake, _tx) = new_runtime_with_fake();

    assert!(!rt.has_active_panes());
}

#[test]
fn a_parked_pane_makes_the_runtime_report_active() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let pane = PaneId::new();

    spawn_and_park(&mut rt, &fake, pane);

    assert!(rt.has_active_panes());
}

#[test]
fn the_panic_teardown_group_kills_every_pane_as_a_tree() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let first = PaneId::new();
    let second = PaneId::new();
    spawn_and_park(&mut rt, &fake, first);
    spawn_and_park(&mut rt, &fake, second);

    rt.kill_all_panes();

    assert_eq!(
        fake.kills(first).expect("first pane"),
        vec![KillPolicy::Tree]
    );
    assert_eq!(
        fake.kills(second).expect("second pane"),
        vec![KillPolicy::Tree]
    );
}

#[test]
fn a_client_key_press_is_written_to_that_clients_focused_pane() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client = rt
        .bootstrap_local(
            SessionId::new(),
            Size { cols: 80, rows: 24 },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let pane = *rt.pty_handles.keys().next().expect("one pane");

    let flow = rt.handle_runtime_event(RuntimeEvent::ClientKeyPress {
        client_id: client,
        chord: KeyChord::new(ModFlags::NONE, Key::Char('a')),
    });

    assert_eq!(flow, ControlFlow::Continue(()));
    assert_eq!(fake.writes(pane).expect("writes"), vec![vec![b'a']]);
}

#[test]
fn a_host_paste_is_written_to_that_clients_focused_pane() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client = rt
        .bootstrap_local(
            SessionId::new(),
            Size { cols: 80, rows: 24 },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let pane = *rt.pty_handles.keys().next().expect("one pane");

    let flow = rt.handle_runtime_event(RuntimeEvent::HostPaste {
        client_id: client,
        text: String::from("hello\nworld"),
    });

    // A fresh pane has bracketed paste off, so the text reaches it unwrapped,
    // with the line break as the byte the Enter key sends.
    assert_eq!(flow, ControlFlow::Continue(()));
    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![b"hello\rworld".to_vec()]
    );
}

#[test]
fn a_terminal_hangup_breaks_the_loop() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();

    assert_eq!(
        rt.handle_runtime_event(RuntimeEvent::Quit),
        ControlFlow::Break(())
    );
}

#[test]
fn a_timer_tick_continues_the_loop_and_schedules_no_render() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let now = Instant::now();

    let flow = rt.handle_runtime_event(RuntimeEvent::Timer);

    assert_eq!(flow, ControlFlow::Continue(()));
    assert_eq!(rt.next_render_wakeup(now), None);
}

#[test]
fn a_key_no_attached_viewer_resolved_is_dropped_instead_of_written() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client = rt
        .bootstrap_local(
            SessionId::new(),
            Size { cols: 80, rows: 24 },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let pane = *rt.pty_handles.keys().next().expect("one pane");

    let flow = rt.handle_runtime_event(RuntimeEvent::KeyInput {
        client_id: client,
        chord: KeyChord::new(ModFlags::NONE, Key::Char('a')),
    });

    assert_eq!(flow, ControlFlow::Continue(()));
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn a_mouse_event_no_attached_viewer_answered_is_dropped_instead_of_written() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client = rt
        .bootstrap_local(
            SessionId::new(),
            Size { cols: 80, rows: 24 },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let pane = *rt.pty_handles.keys().next().expect("one pane");

    let flow = rt.handle_runtime_event(RuntimeEvent::MouseInput {
        client_id: client,
        mouse: MouseInput {
            kind: MouseKind::Press(MouseButton::Left),
            at: Point { x: 10, y: 3 },
            mods: ModFlags::NONE,
        },
    });

    assert_eq!(flow, ControlFlow::Continue(()));
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn an_ipc_command_still_applies_when_its_reply_channel_is_gone() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let client = rt
        .bootstrap_local(
            SessionId::new(),
            Size { cols: 80, rows: 24 },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let (reply, gone) = mpsc::channel();
    drop(gone);

    let flow = rt.handle_runtime_event(RuntimeEvent::Ipc {
        envelope: CommandEnvelope::new(
            CommandId::new(),
            CommandSource::key_binding(client),
            SystemTime::UNIX_EPOCH,
            Command::ToggleLockMode(ToggleLockModeArgs::default()),
        ),
        reply,
    });

    assert_eq!(flow, ControlFlow::Continue(()));
    let overview = rt.build_overview().expect("one session is running");
    assert_eq!(overview.clients[0].id, client);
    assert_eq!(overview.clients[0].lock_state, LockMode::Locked);
}

#[test]
fn a_discovery_request_is_answered_with_the_running_session() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let session = SessionId::new();
    rt.bootstrap_local(session, Size { cols: 80, rows: 24 }, SystemTime::UNIX_EPOCH)
        .expect("bootstrap");
    let (reply, answers) = mpsc::channel();

    let flow = rt.handle_runtime_event(RuntimeEvent::IpcDiscovery { reply });

    assert_eq!(flow, ControlFlow::Continue(()));
    let overview = answers
        .recv()
        .expect("the reply")
        .expect("one session is running");
    assert_eq!(overview.session.id, session);
}

#[test]
fn a_discovery_request_with_no_session_is_answered_none() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let (reply, answers) = mpsc::channel();

    let flow = rt.handle_runtime_event(RuntimeEvent::IpcDiscovery { reply });

    assert_eq!(flow, ControlFlow::Continue(()));
    assert_eq!(answers.recv().expect("the reply"), None);
}

#[test]
fn a_layout_request_is_answered_with_the_running_session() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let session = SessionId::new();
    rt.bootstrap_local(session, Size { cols: 80, rows: 24 }, SystemTime::UNIX_EPOCH)
        .expect("bootstrap");
    let (reply, answers) = mpsc::channel();

    let flow = rt.handle_runtime_event(RuntimeEvent::IpcLayout { tab: None, reply });

    assert_eq!(flow, ControlFlow::Continue(()));
    let layout = answers
        .recv()
        .expect("the reply")
        .expect("one session is running");
    assert_eq!(layout.id, session);
    assert_eq!(layout.tabs.len(), 1);
}

#[test]
fn a_restart_request_with_no_installed_check_is_refused_and_changes_nothing() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let (reply, answers) = mpsc::channel();

    let flow = rt.handle_runtime_event(RuntimeEvent::IpcRestart { reply });

    assert_eq!(flow, ControlFlow::Continue(()));
    assert_eq!(
        answers.recv().expect("the reply"),
        Err("this koshi cannot replace its own image, so it cannot restart".to_string())
    );
    assert!(!rt.restart_requested);
}

#[test]
fn nothing_is_pending_so_the_loop_sleeps_and_no_render_is_due() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let now = Instant::now();

    assert_eq!(rt.next_render_wakeup(now), None);
    assert!(!rt.poll_render(now));
}

#[test]
fn a_pending_invalidation_is_due_at_once_then_clears_after_one_render() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    rt.render_scheduler.invalidate();
    let now = Instant::now();

    assert_eq!(rt.next_render_wakeup(now), Some(Duration::ZERO));
    assert!(rt.poll_render(now));
    assert!(!rt.poll_render(now));
    assert_eq!(rt.next_render_wakeup(now), None);
}

#[test]
fn an_invalidation_right_after_a_render_waits_out_the_frame_cadence() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let now = Instant::now();
    rt.render_scheduler.invalidate();
    assert!(rt.poll_render(now));

    rt.render_scheduler.invalidate();

    assert_eq!(rt.next_render_wakeup(now), Some(FRAME_INTERVAL));
    assert!(!rt.poll_render(now));
    let due = now + FRAME_INTERVAL;
    assert_eq!(rt.next_render_wakeup(due), Some(Duration::ZERO));
    assert!(rt.poll_render(due));
}
