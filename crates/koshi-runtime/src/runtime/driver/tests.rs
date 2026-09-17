//! Tests for the loop-facing driver surface: render-wakeup timing and
//! poll delegation to the scheduler, the live- pane_id check, the routing of an
//! attached client's key press and pasted text, the inbox events that are
//! dropped or answered on their reply channel, and the abrupt group-kill the
//! panic path takes.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::SystemTime;

use koshi_core::command::{Command, CommandEnvelope, CommandSource, ToggleLockModeArgs};
use koshi_core::geometry::{Point, Size};
use koshi_core::ids::{CommandId, PaneId, SessionId};
use koshi_core::key::{
    Key, KeyChord, KeyEventKind, KeyIdentity, KeyInput, KeyModifierFlags, ModFlags,
};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind};
use koshi_core::process::{PtySize, SpawnSpec};
use koshi_pty::backend::state::PtyBackend;
use koshi_test_support::fake_pty::FakePtyBackend;
use koshi_test_support::fixtures::build_key_input_for_chord;

use crate::runtime::event::RuntimeEvent;
use crate::runtime::render_schedule::FRAME_INTERVAL_DURATION;

use super::*;

const TEST_PTY_SIZE: PtySize = PtySize {
    column_count: 80,
    row_count: 24,
};

/// A runtime sharing one fake PTY backend, returned alongside it so a test can
/// assert on the kills the driver issues. The sender keeps the inbox open.
fn build_test_runtime_with_fake_pty_backend(
) -> (Server, Arc<FakePtyBackend>, mpsc::Sender<RuntimeEvent>) {
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let (event_sender, event_receiver) = mpsc::channel();
    let server = Server::from_runtime_parts(pty_backend, event_receiver, event_sender.clone());
    (server, fake_pty_backend, event_sender)
}

/// Spawn a pane in the fake PTY backend and park its handle in the runtime, so the
/// pane is live in both — the backend can record kills and the runtime counts
/// it as active.
fn spawn_and_park_test_pane(
    server: &mut Server,
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
    server.park_pane_pty(pane_id, pty_handle, TEST_PTY_SIZE);
}

#[test]
fn no_panes_are_active_before_any_pane_is_parked() {
    let (server, _fake_pty_backend, _event_sender) = build_test_runtime_with_fake_pty_backend();

    assert!(!server.has_active_panes());
}

#[test]
fn a_parked_pane_makes_the_runtime_report_active() {
    let (mut server, fake_pty_backend, _event_sender) = build_test_runtime_with_fake_pty_backend();
    let pane_id = PaneId::new();

    spawn_and_park_test_pane(&mut server, &fake_pty_backend, pane_id);

    assert!(server.has_active_panes());
}

#[test]
fn the_panic_teardown_group_kills_every_pane_as_a_tree() {
    let (mut server, fake_pty_backend, _event_sender) = build_test_runtime_with_fake_pty_backend();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    spawn_and_park_test_pane(&mut server, &fake_pty_backend, first_pane_id);
    spawn_and_park_test_pane(&mut server, &fake_pty_backend, second_pane_id);

    server.kill_all_panes();

    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(first_pane_id)
            .expect("first pane"),
        vec![KillPolicy::Tree]
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(second_pane_id)
            .expect("second pane"),
        vec![KillPolicy::Tree]
    );
}

#[test]
fn a_client_key_press_is_written_to_that_clients_focused_pane() {
    let (mut server, fake_pty_backend, _event_sender) = build_test_runtime_with_fake_pty_backend();
    let client_id = server
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let pane_id = *server
        .pty_handle_by_pane_id
        .keys()
        .next()
        .expect("one pane");

    let control_flow = server.handle_runtime_event(RuntimeEvent::ClientKeyboard {
        client_id,
        key_input: build_key_input_for_chord(KeyChord::from_parts(ModFlags::NONE, Key::Char('a'))),
    });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![vec![b'a']]
    );
}

/// A repeat carries a chord the same way a press does, so the pane reads the
/// key again.
#[test]
fn a_client_key_repeat_is_written_to_that_clients_focused_pane() {
    let (mut server, fake_pty_backend, _event_sender) = build_test_runtime_with_fake_pty_backend();
    let client_id = server
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let pane_id = *server
        .pty_handle_by_pane_id
        .keys()
        .next()
        .expect("one pane");
    let repeated_key_input = KeyInput {
        key_event_kind: KeyEventKind::Repeat,
        ..build_key_input_for_chord(KeyChord::from_parts(ModFlags::NONE, Key::Char('a')))
    };

    let control_flow = server.handle_runtime_event(RuntimeEvent::ClientKeyboard {
        client_id,
        key_input: repeated_key_input,
    });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![vec![b'a']]
    );
}

/// Legacy pane delivery writes a chord, and a release has none, so the pane
/// reads nothing. The enhanced encoding that gives a release its own bytes is
/// not integrated yet.
#[test]
fn a_client_key_release_writes_nothing_to_that_clients_focused_pane() {
    let (mut server, fake_pty_backend, _event_sender) = build_test_runtime_with_fake_pty_backend();
    let client_id = server
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let pane_id = *server
        .pty_handle_by_pane_id
        .keys()
        .next()
        .expect("one pane");
    let released_key_input = KeyInput {
        key_event_kind: KeyEventKind::Release,
        ..build_key_input_for_chord(KeyChord::from_parts(ModFlags::NONE, Key::Char('a')))
    };

    let control_flow = server.handle_runtime_event(RuntimeEvent::ClientKeyboard {
        client_id,
        key_input: released_key_input,
    });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

/// Left Shift reports as codepoint 57441, which no chord can hold, so legacy
/// pane delivery writes nothing for it.
#[test]
fn a_client_key_that_no_chord_can_name_writes_nothing_to_that_clients_focused_pane() {
    let (mut server, fake_pty_backend, _event_sender) = build_test_runtime_with_fake_pty_backend();
    let client_id = server
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let pane_id = *server
        .pty_handle_by_pane_id
        .keys()
        .next()
        .expect("one pane");
    let left_shift_key_input = KeyInput {
        key: KeyIdentity::Codepoint(57441),
        key_event_kind: KeyEventKind::Press,
        shifted_key: None,
        base_layout_key: None,
        associated_text: String::new(),
        modifier_flags: KeyModifierFlags::SHIFT,
    };

    let control_flow = server.handle_runtime_event(RuntimeEvent::ClientKeyboard {
        client_id,
        key_input: left_shift_key_input,
    });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn a_host_paste_is_written_to_that_clients_focused_pane() {
    let (mut server, fake_pty_backend, _event_sender) = build_test_runtime_with_fake_pty_backend();
    let client_id = server
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let pane_id = *server
        .pty_handle_by_pane_id
        .keys()
        .next()
        .expect("one pane");

    let control_flow = server.handle_runtime_event(RuntimeEvent::HostPaste {
        client_id,
        pasted_text: String::from("hello\nworld"),
    });

    // A fresh pane has bracketed paste off, so the text reaches it unwrapped,
    // with the line break as the byte the Enter key sends.
    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![b"hello\rworld".to_vec()]
    );
}

#[test]
fn a_terminal_hangup_breaks_the_loop() {
    let (mut server, _fake_pty_backend, _event_sender) = build_test_runtime_with_fake_pty_backend();

    assert_eq!(
        server.handle_runtime_event(RuntimeEvent::Quit),
        ControlFlow::Break(())
    );
}

#[test]
fn a_timer_tick_continues_the_loop_and_schedules_no_render() {
    let (mut server, _fake_pty_backend, _event_sender) = build_test_runtime_with_fake_pty_backend();
    let current_time = Instant::now();

    let control_flow = server.handle_runtime_event(RuntimeEvent::Timer);

    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(server.next_render_wakeup(current_time), None);
}

#[test]
fn a_key_no_attached_viewer_resolved_is_dropped_instead_of_written() {
    let (mut server, fake_pty_backend, _event_sender) = build_test_runtime_with_fake_pty_backend();
    let client_id = server
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let pane_id = *server
        .pty_handle_by_pane_id
        .keys()
        .next()
        .expect("one pane");

    let control_flow = server.handle_runtime_event(RuntimeEvent::KeyInput {
        client_id,
        key_input: KeyInput {
            key: KeyIdentity::Key(Key::Char('a')),
            key_event_kind: KeyEventKind::Press,
            shifted_key: None,
            base_layout_key: None,
            associated_text: String::new(),
            modifier_flags: KeyModifierFlags::NONE,
        },
    });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn a_mouse_event_no_attached_viewer_answered_is_dropped_instead_of_written() {
    let (mut server, fake_pty_backend, _event_sender) = build_test_runtime_with_fake_pty_backend();
    let client_id = server
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let pane_id = *server
        .pty_handle_by_pane_id
        .keys()
        .next()
        .expect("one pane");

    let control_flow = server.handle_runtime_event(RuntimeEvent::MouseInput {
        client_id,
        mouse_input: MouseInput {
            mouse_kind: MouseKind::Press(MouseButton::Left),
            position: Point { column: 10, row: 3 },
            modifier_flags: ModFlags::NONE,
        },
    });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn an_ipc_command_still_applies_when_its_reply_channel_is_gone() {
    let (mut server, _fake_pty_backend, _event_sender) = build_test_runtime_with_fake_pty_backend();
    let client_id = server
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let (response_sender, response_receiver) = mpsc::channel();
    drop(response_receiver);

    let control_flow = server.handle_runtime_event(RuntimeEvent::Ipc {
        envelope: CommandEnvelope::from_parts(
            CommandId::new(),
            CommandSource::from_key_binding(client_id),
            SystemTime::UNIX_EPOCH,
            Command::ToggleLockMode(ToggleLockModeArgs::default()),
        ),
        response_sender,
    });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    let overview = server.build_overview().expect("one session is running");
    assert_eq!(overview.clients[0].client_id, client_id);
    assert_eq!(overview.clients[0].lock_mode, LockMode::Locked);
}

#[test]
fn a_discovery_request_is_answered_with_the_running_session() {
    let (mut server, _fake_pty_backend, _event_sender) = build_test_runtime_with_fake_pty_backend();
    let session_id = SessionId::new();
    server
        .bootstrap_local(
            session_id,
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let (response_sender, response_receiver) = mpsc::channel();

    let control_flow = server.handle_runtime_event(RuntimeEvent::IpcDiscovery { response_sender });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    let overview = response_receiver
        .recv()
        .expect("the reply")
        .expect("one session is running");
    assert_eq!(overview.session.session_id, session_id);
}

#[test]
fn a_discovery_request_with_no_session_is_answered_none() {
    let (mut server, _fake_pty_backend, _event_sender) = build_test_runtime_with_fake_pty_backend();
    let (response_sender, response_receiver) = mpsc::channel();

    let control_flow = server.handle_runtime_event(RuntimeEvent::IpcDiscovery { response_sender });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(response_receiver.recv().expect("the reply"), None);
}

#[test]
fn a_layout_request_is_answered_with_the_running_session() {
    let (mut server, _fake_pty_backend, _event_sender) = build_test_runtime_with_fake_pty_backend();
    let session_id = SessionId::new();
    server
        .bootstrap_local(
            session_id,
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let (response_sender, response_receiver) = mpsc::channel();

    let control_flow = server.handle_runtime_event(RuntimeEvent::IpcLayout {
        tab_id: None,
        response_sender,
    });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    let session_layout = response_receiver
        .recv()
        .expect("the reply")
        .expect("one session is running");
    assert_eq!(session_layout.session_id, session_id);
    assert_eq!(session_layout.tabs.len(), 1);
}

#[test]
fn a_restart_request_with_no_installed_check_is_refused_and_changes_nothing() {
    let (mut server, _fake_pty_backend, _event_sender) = build_test_runtime_with_fake_pty_backend();
    let (response_sender, response_receiver) = mpsc::channel();

    let control_flow = server.handle_runtime_event(RuntimeEvent::IpcRestart { response_sender });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(
        response_receiver.recv().expect("the reply"),
        Err("this koshi cannot replace its own image, so it cannot restart".to_string())
    );
    assert!(!server.is_restart_requested);
}

#[test]
fn nothing_is_pending_so_the_loop_sleeps_and_no_render_is_due() {
    let (mut runtime, _fake_pty_backend, _event_sender) =
        build_test_runtime_with_fake_pty_backend();
    let now = Instant::now();

    assert_eq!(runtime.next_render_wakeup(now), None);
    assert!(!runtime.poll_render(now));
}

#[test]
fn a_pending_invalidation_is_due_at_once_then_clears_after_one_render() {
    let (mut runtime, _fake_pty_backend, _event_sender) =
        build_test_runtime_with_fake_pty_backend();
    runtime.render_scheduler.invalidate();
    let now = Instant::now();

    assert_eq!(runtime.next_render_wakeup(now), Some(Duration::ZERO));
    assert!(runtime.poll_render(now));
    assert!(!runtime.poll_render(now));
    assert_eq!(runtime.next_render_wakeup(now), None);
}

#[test]
fn an_invalidation_right_after_a_render_waits_out_the_frame_cadence() {
    let (mut runtime, _fake_pty_backend, _event_sender) =
        build_test_runtime_with_fake_pty_backend();
    let now = Instant::now();
    runtime.render_scheduler.invalidate();
    assert!(runtime.poll_render(now));

    runtime.render_scheduler.invalidate();

    assert_eq!(
        runtime.next_render_wakeup(now),
        Some(FRAME_INTERVAL_DURATION)
    );
    assert!(!runtime.poll_render(now));
    let due = now + FRAME_INTERVAL_DURATION;
    assert_eq!(runtime.next_render_wakeup(due), Some(Duration::ZERO));
    assert!(runtime.poll_render(due));
}
