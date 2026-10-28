//! Tests for the loop-facing driver surface: render-wakeup timing, frame pace
//! per subscriber, the views a child exit marks, the frames each pass pushes,
//! a freed queue slot that is a wake only, the image animations each pass
//! advances, the live-pane check, the routing of an attached client's key
//! press and pasted text, the inbox events that are dropped or answered on
//! their reply channel, and the abrupt group-kill the panic path takes.

use std::collections::{BTreeMap, HashSet};
use std::sync::mpsc;
use std::time::SystemTime;

use crate::runtime::pty_inbox::InboxSink;
use koshi_core::action::ActionReference;
use koshi_core::command::{
    Command, CommandEnvelope, CommandSource, FocusPaneArgs, FocusTarget, MoveFloatingPaneArgs,
    ToggleLockModeArgs,
};
use koshi_core::event::RejectReason;
use koshi_core::geometry::{Direction, Point, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::key::{
    BindingModifierFlags, Key, KeyChord, KeyEventKind, KeyIdentity, KeyInput, KeyModifierFlags,
};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind};
use koshi_core::process::{PtySize, SpawnSpec};
use koshi_pty::backend::state::PtyBackend;
use koshi_terminal::engine::TerminalEngine;
use koshi_test_support::fake_pty::FakePtyBackend;
use koshi_test_support::fixtures::build_key_input_for_chord;

use crate::runtime::command::tests::{
    attach_second_client, build_floating_new_pane_args, create_floating_pane,
    create_session_with_attached_client,
};
use crate::runtime::delivery_queue::DeliveryReceiver;
use crate::runtime::event::RuntimeEvent;
use crate::runtime::render_schedule::tests::{
    mark_changed_views_then_compute_frame_wait, DEFAULT_FRAME_INTERVAL_DURATION,
};
use koshi_renderer::snapshot::{Delivery, RenderSnapshot};
use koshi_test_support::fixtures::build_new_pane_args;

use super::*;

const TEST_PTY_SIZE: PtySize = PtySize {
    column_count: 80,
    row_count: 24,
};

/// A runtime sharing one fake PTY backend. Returns the backend too: it records
/// the kills and writes the driver issues.
fn build_test_runtime_with_fake_pty_backend() -> (Server, Arc<FakePtyBackend>) {
    let (server, fake_pty_backend, _inbox_sender) = build_test_runtime_with_inbox_sender();
    (server, fake_pty_backend)
}

/// [`build_test_runtime_with_fake_pty_backend`], with a sender into the
/// runtime's inbox.
fn build_test_runtime_with_inbox_sender(
) -> (Server, Arc<FakePtyBackend>, mpsc::Sender<RuntimeEvent>) {
    let (event_sender, event_receiver) = mpsc::channel();
    let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(event_sender.clone()),
    )));
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let server = Server::from_runtime_parts(pty_backend, event_receiver, Arc::default());
    (server, fake_pty_backend, event_sender)
}

/// Starts `session_id` on `server` with one 80x24 local client at timestamp
/// `0`. Returns that client's id and the id of the session's one pane.
fn bootstrap_test_session(server: &mut Server, session_id: SessionId) -> (ClientId, PaneId) {
    let client_id = create_session_with_attached_client(
        server,
        session_id,
        Size {
            column_count: 80,
            row_count: 24,
        },
        SystemTime::UNIX_EPOCH,
    )
    .expect("bootstrap");
    let pane_id = *server.live_pane_ids.iter().next().expect("one pane");
    (client_id, pane_id)
}

/// Spawns a pane in the fake PTY backend and parks its handle in the runtime.
/// The pane is live in both: the backend records kills, and the runtime counts
/// it as active.
fn spawn_and_park_test_pane(
    server: &mut Server,
    fake_pty_backend: &FakePtyBackend,
    pane_id: PaneId,
) {
    fake_pty_backend
        .spawn_pane(
            pane_id,
            SpawnSpec::build_default_shell(None, BTreeMap::new()),
            TEST_PTY_SIZE,
        )
        .expect("spawn");
    server.park_pane_pty(pane_id, TEST_PTY_SIZE);
}

#[test]
fn no_panes_are_active_before_any_pane_is_parked() {
    let (server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();

    assert!(!server.has_active_panes());
}

#[test]
fn a_parked_pane_makes_the_runtime_report_active() {
    let (mut server, fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let pane_id = PaneId::new();

    spawn_and_park_test_pane(&mut server, &fake_pty_backend, pane_id);

    assert!(server.has_active_panes());
}

#[test]
fn the_panic_teardown_group_kills_every_pane_as_a_tree() {
    let (mut server, fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
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
    let (mut server, fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());

    let control_flow = server.handle_runtime_event(RuntimeEvent::ClientKeyboard {
        client_id,
        key_inputs: vec![build_key_input_for_chord(KeyChord::from_parts(
            BindingModifierFlags::NONE,
            Key::Char('a'),
        ))],
    });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![vec![b'a']]
    );
}

/// A repeat of `a` writes `a` to the focused pane, as a press does.
#[test]
fn a_client_key_repeat_is_written_to_that_clients_focused_pane() {
    let (mut server, fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    let repeated_key_input = KeyInput {
        key_event_kind: KeyEventKind::Repeat,
        ..build_key_input_for_chord(KeyChord::from_parts(
            BindingModifierFlags::NONE,
            Key::Char('a'),
        ))
    };

    let control_flow = server.handle_runtime_event(RuntimeEvent::ClientKeyboard {
        client_id,
        key_inputs: vec![repeated_key_input],
    });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![vec![b'a']]
    );
}

/// A release of `a` writes nothing to a focused pane that set no keyboard flags.
#[test]
fn a_client_key_release_writes_nothing_to_that_clients_focused_pane() {
    let (mut server, fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    let released_key_input = KeyInput {
        key_event_kind: KeyEventKind::Release,
        ..build_key_input_for_chord(KeyChord::from_parts(
            BindingModifierFlags::NONE,
            Key::Char('a'),
        ))
    };

    let control_flow = server.handle_runtime_event(RuntimeEvent::ClientKeyboard {
        client_id,
        key_inputs: vec![released_key_input],
    });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

/// Left Shift reports as codepoint 57441. A focused pane that set no keyboard
/// flags receives nothing for it.
#[test]
fn a_client_key_that_no_chord_can_name_writes_nothing_to_that_clients_focused_pane() {
    let (mut server, fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
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
        key_inputs: vec![left_shift_key_input],
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
    let (mut server, fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());

    let control_flow = server.handle_runtime_event(RuntimeEvent::HostPaste {
        client_id,
        pasted_text: String::from("hello\nworld"),
    });

    // A fresh pane has bracketed paste off: the text reaches it unwrapped,
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
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();

    assert_eq!(
        server.handle_runtime_event(RuntimeEvent::Quit),
        ControlFlow::Break(())
    );
}

#[test]
fn a_key_no_attached_viewer_resolved_is_dropped_instead_of_written() {
    let (mut server, fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());

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
    let (mut server, fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());

    let control_flow = server.handle_runtime_event(RuntimeEvent::MouseInput {
        client_id,
        mouse_input: MouseInput {
            mouse_kind: MouseKind::Press(MouseButton::Left),
            position: Point { column: 10, row: 3 },
            modifier_flags: BindingModifierFlags::NONE,
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
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, _pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    let (response_sender, response_receiver) = mpsc::channel();
    drop(response_receiver);

    let control_flow = server.handle_runtime_event(RuntimeEvent::Ipc {
        command_envelope: Box::new(CommandEnvelope::from_parts(
            CommandId::new(),
            CommandSource::from_key_binding(client_id),
            Command::ToggleLockMode(ToggleLockModeArgs::default()),
        )),
        fired_action: None,
        response_sender,
    });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    let session_overview = server.build_overview().expect("one session is running");
    assert_eq!(session_overview.clients[0].client_id, client_id);
    assert_eq!(session_overview.clients[0].lock_mode, LockMode::Locked);
}

#[test]
fn a_discovery_request_is_answered_with_the_running_session() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let session_id = SessionId::new();
    bootstrap_test_session(&mut server, session_id);
    let (response_sender, response_receiver) = mpsc::channel();

    let control_flow = server.handle_runtime_event(RuntimeEvent::IpcDiscovery { response_sender });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    let session_overview = response_receiver
        .recv()
        .expect("the reply")
        .expect("one session is running");
    assert_eq!(session_overview.session.session_id, session_id);
}

#[test]
fn a_discovery_request_with_no_session_is_answered_none() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (response_sender, response_receiver) = mpsc::channel();

    let control_flow = server.handle_runtime_event(RuntimeEvent::IpcDiscovery { response_sender });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(response_receiver.recv().expect("the reply"), None);
}

#[test]
fn a_layout_request_is_answered_with_the_running_session() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let session_id = SessionId::new();
    bootstrap_test_session(&mut server, session_id);
    let running_tab_ids: Vec<TabId> = server
        .build_overview()
        .expect("one session is running")
        .tabs
        .iter()
        .map(|tab_discovery| tab_discovery.tab_id)
        .collect();
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
    let described_tab_ids: Vec<TabId> = session_layout
        .tabs
        .iter()
        .map(|tab_layout| tab_layout.tab_id)
        .collect();
    assert_eq!(described_tab_ids, running_tab_ids);
}

#[test]
fn a_restart_request_is_taken_answered_and_raises_the_flag() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (response_sender, response_receiver) = mpsc::channel();

    let control_flow = server.handle_runtime_event(RuntimeEvent::IpcRestart { response_sender });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    response_receiver.recv().expect("the reply");
    assert!(server.is_restart_requested);
}

/// Every frame on `delivery_receiver`, in queue order; any other delivery is
/// dropped.
fn take_queued_frames(delivery_receiver: &DeliveryReceiver) -> Vec<RenderSnapshot> {
    delivery_receiver
        .try_iter()
        .filter_map(|delivery| match delivery {
            Delivery::Frame(render_snapshot) => Some(*render_snapshot),
            _ => None,
        })
        .collect()
}

#[test]
fn with_no_subscriber_the_loop_sleeps_until_an_event() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let current_time = Instant::now();
    let _ = bootstrap_test_session(&mut server, SessionId::new());

    assert_eq!(server.compute_next_render_wakeup(current_time), None);
}

#[test]
fn a_new_subscriber_gets_its_first_frame_at_once_and_one_push_sends_it() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, _pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    let (delivery_receiver, _) = server.subscribe(client_id, 60);
    let current_time = Instant::now();

    assert_eq!(
        server.compute_next_render_wakeup(current_time),
        Some(Duration::ZERO)
    );
    server.push_due_frames(current_time);
    assert_eq!(take_queued_frames(&delivery_receiver).len(), 1);
    assert_eq!(server.compute_next_render_wakeup(current_time), None);
    server.push_due_frames(current_time);
    assert_eq!(take_queued_frames(&delivery_receiver).len(), 0);
}

#[test]
fn an_attach_paces_its_client_at_the_frame_rate_the_attach_names() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (_client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    let (attach_response_sender, attach_response_receiver) = mpsc::channel();
    let _ = server.handle_runtime_event(RuntimeEvent::IpcAttach {
        resume_client_id: None,
        resume_token: None,
        viewport_size: Size {
            column_count: 80,
            row_count: 24,
        },
        pane_area: None,
        cell_size: None,
        attached_at: SystemTime::UNIX_EPOCH,
        is_remote: false,
        max_frames_per_second: 120,
        response_sender: attach_response_sender,
        placement_preview_target: None,
    });
    let attach_accepted = attach_response_receiver
        .try_recv()
        .expect("the attach is answered")
        .expect("a session is running");
    let current_time = Instant::now();
    server.push_due_frames(current_time);
    assert_eq!(take_queued_frames(&attach_accepted.deliveries).len(), 1);

    server.handle_pty_output(pane_id, b"x");

    assert_eq!(
        mark_changed_views_then_compute_frame_wait(
            &mut server,
            attach_accepted.client_id,
            current_time
        ),
        Some(Duration::from_nanos(8_333_334))
    );
}

#[test]
fn after_each_pass_every_clients_last_frame_is_its_current_frame() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (first_client_id, animated_pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    server.insert_terminal_engine(animated_pane_id, build_two_frame_animated_terminal_engine());
    let second_client_id =
        attach_second_client(&mut server, first_client_id, Some(animated_pane_id));
    let third_client_id =
        attach_second_client(&mut server, first_client_id, Some(animated_pane_id));
    let client_ids = [first_client_id, second_client_id, third_client_id];
    let delivery_receivers: Vec<DeliveryReceiver> = client_ids
        .iter()
        .map(|&client_id| server.subscribe(client_id, 60).0)
        .collect();
    let mut last_frames: Vec<Option<RenderSnapshot>> = vec![None; client_ids.len()];
    let mut random_state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut pass_time = Instant::now();

    for step_index in 0..400_u32 {
        random_state ^= random_state << 13;
        random_state ^= random_state >> 7;
        random_state ^= random_state << 17;
        let client_id = client_ids[(random_state % 3) as usize];
        let mut pane_ids: Vec<PaneId> = server.list_terminal_engines().keys().copied().collect();
        pane_ids.sort();
        let pane_id = pane_ids[((random_state >> 8) % pane_ids.len() as u64) as usize];
        match (random_state >> 32) % 10 {
            0..=3 => server.handle_pty_output(
                pane_id,
                format!("line {step_index}\r\n\x1b]2;title {step_index}\x07").as_bytes(),
            ),
            4 if pane_ids.len() < 6 => open_new_tab_for_client(&mut server, client_id),
            4 | 5 => {
                let _ = server.submit_command(CommandEnvelope::from_parts(
                    CommandId::new(),
                    CommandSource::KeyBinding { client_id },
                    Command::FocusTab(koshi_core::command::FocusTabArgs {
                        focus_target: koshi_core::command::TabTarget::Next,
                        client_id: Some(client_id),
                    }),
                ));
            }
            6 => server.scroll_up(client_id, pane_id, 2),
            7 => server.scroll_down(client_id, pane_id, 1),
            8 => {
                let _ = server.handle_client_resize(
                    client_id,
                    Size {
                        column_count: 60 + (random_state >> 40) as u16 % 40,
                        row_count: 20 + (random_state >> 48) as u16 % 10,
                    },
                    None,
                    None,
                );
            }
            _ => {
                let _ = server.handle_runtime_event(RuntimeEvent::ClientKeyboard {
                    client_id,
                    key_inputs: vec![build_key_input_for_chord(KeyChord::from_parts(
                        BindingModifierFlags::NONE,
                        Key::Char('a'),
                    ))],
                });
            }
        }
        pass_time += DEFAULT_FRAME_INTERVAL_DURATION;
        server.push_due_frames(pass_time);

        for (client_index, &viewing_client_id) in client_ids.iter().enumerate() {
            if let Some(newest_frame) = take_queued_frames(&delivery_receivers[client_index]).pop()
            {
                last_frames[client_index] = Some(newest_frame);
            }
            assert_eq!(
                last_frames[client_index],
                server.build_snapshot(viewing_client_id),
                "step {step_index}: the last frame of client {client_index} is stale"
            );
            assert_eq!(
                mark_changed_views_then_compute_frame_wait(
                    &mut server,
                    viewing_client_id,
                    pass_time
                ),
                None,
                "step {step_index}: client {client_index} still owes a frame"
            );
        }
    }
}

#[test]
fn a_command_that_fails_validation_marks_no_view_and_a_lock_toggle_marks_the_view_of_its_client_alone(
) {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    let other_client_id = attach_second_client(&mut server, client_id, Some(pane_id));
    let (_delivery_receiver, _) = server.subscribe(client_id, 60);
    let (_other_delivery_receiver, _) = server.subscribe(other_client_id, 60);
    let current_time = Instant::now();
    server.push_due_frames(current_time);
    let rejected_command_id = CommandId::new();

    let rejected_command_result = server.submit_command(CommandEnvelope::from_parts(
        rejected_command_id,
        CommandSource::KeyBinding {
            client_id: ClientId::new(),
        },
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    ));

    assert_eq!(
        rejected_command_result,
        CommandResult::Rejected {
            command_id: rejected_command_id,
            reason: koshi_core::event::RejectReason::SourceClientStale,
            help: None,
        }
    );
    for viewing_client_id in [client_id, other_client_id] {
        assert_eq!(
            mark_changed_views_then_compute_frame_wait(
                &mut server,
                viewing_client_id,
                current_time
            ),
            None
        );
    }
    let _ = server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    ));
    for (viewing_client_id, expected_frame_wait) in [
        (client_id, Some(DEFAULT_FRAME_INTERVAL_DURATION)),
        (other_client_id, None),
    ] {
        assert_eq!(
            mark_changed_views_then_compute_frame_wait(
                &mut server,
                viewing_client_id,
                current_time
            ),
            expected_frame_wait
        );
    }
}

#[test]
fn a_freed_queue_slot_is_a_wake_that_handles_nothing() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let current_time = Instant::now();

    let control_flow = server.handle_runtime_event(RuntimeEvent::QueueRoomFreed);

    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(server.compute_next_render_wakeup(current_time), None);
}

#[test]
fn a_grace_window_close_carrying_another_servers_id_detaches_nobody_and_one_carrying_this_servers_id_detaches_the_unclaimed_client(
) {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (earlier_server, _earlier_fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let session_id = SessionId::new();
    let (client_id, _pane_id) = bootstrap_test_session(&mut server, session_id);
    server.client_ids_awaiting_reconnect.insert(client_id);
    assert_ne!(
        earlier_server.get_grace_window_id(),
        server.get_grace_window_id()
    );

    let control_flow = server.handle_runtime_event(RuntimeEvent::DropUnclaimedClients {
        grace_window_id: earlier_server.get_grace_window_id(),
        unclaimed_client_deadline: SystemTime::UNIX_EPOCH,
    });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(
        server.client_ids_awaiting_reconnect,
        HashSet::from([client_id])
    );
    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 1);

    let control_flow = server.handle_runtime_event(RuntimeEvent::DropUnclaimedClients {
        grace_window_id: server.get_grace_window_id(),
        unclaimed_client_deadline: SystemTime::UNIX_EPOCH,
    });

    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(server.client_ids_awaiting_reconnect, HashSet::new());
    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 0);
}

#[test]
fn a_change_right_after_a_frame_waits_out_the_frame_interval() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    let (delivery_receiver, _) = server.subscribe(client_id, 60);
    let current_time = Instant::now();
    server.push_due_frames(current_time);
    assert_eq!(take_queued_frames(&delivery_receiver).len(), 1);

    server.handle_pty_output(pane_id, b"x");

    assert_eq!(
        server.compute_next_render_wakeup(current_time),
        Some(DEFAULT_FRAME_INTERVAL_DURATION)
    );
    server.push_due_frames(current_time);
    assert_eq!(take_queued_frames(&delivery_receiver).len(), 0);
    let next_frame_time = current_time + DEFAULT_FRAME_INTERVAL_DURATION;
    assert_eq!(
        server.compute_next_render_wakeup(next_frame_time),
        Some(Duration::ZERO)
    );
    server.push_due_frames(next_frame_time);
    assert_eq!(take_queued_frames(&delivery_receiver).len(), 1);
}

/// Subscribe two clients of one session and send both their first frames at
/// one instant, hand `build_input_event(first client id, session id)` to the
/// runtime, then feed one byte of output to the pane both clients show.
/// Returns the frame waits of the first client and of the second client at
/// that instant.
fn list_frame_waits_after_input_event(
    build_input_event: impl FnOnce(ClientId, SessionId) -> RuntimeEvent,
) -> (Option<Duration>, Option<Duration>) {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let session_id = SessionId::new();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, session_id);
    let other_client_id = attach_second_client(&mut server, client_id, Some(pane_id));
    let (_delivery_receiver, _) = server.subscribe(client_id, 60);
    let (_other_delivery_receiver, _) = server.subscribe(other_client_id, 60);
    let current_time = Instant::now();
    server.push_due_frames(current_time);

    let _ = server.handle_runtime_event(build_input_event(client_id, session_id));
    server.handle_pty_output(pane_id, b"a");

    (
        mark_changed_views_then_compute_frame_wait(&mut server, client_id, current_time),
        mark_changed_views_then_compute_frame_wait(&mut server, other_client_id, current_time),
    )
}

/// An IPC event from `command_source` that toggles the lock mode of
/// `client_id`. Its reply channel is closed.
fn build_toggle_lock_mode_event(
    command_source: CommandSource,
    client_id: ClientId,
) -> RuntimeEvent {
    let (response_sender, _response_receiver) = mpsc::channel();
    RuntimeEvent::Ipc {
        command_envelope: Box::new(CommandEnvelope::from_parts(
            CommandId::new(),
            command_source,
            Command::ToggleLockMode(ToggleLockModeArgs {
                client_id: Some(client_id),
            }),
        )),
        fired_action: None,
        response_sender,
    }
}

#[test]
fn a_clients_own_input_lets_its_next_frame_go_at_once_and_a_command_from_outside_the_session_does_not(
) {
    let own_input_waits = (Some(Duration::ZERO), Some(DEFAULT_FRAME_INTERVAL_DURATION));

    let key_waits =
        list_frame_waits_after_input_event(|client_id, _| RuntimeEvent::ClientKeyboard {
            client_id,
            key_inputs: vec![build_key_input_for_chord(KeyChord::from_parts(
                BindingModifierFlags::NONE,
                Key::Char('a'),
            ))],
        });
    assert_eq!(key_waits, own_input_waits);
    let paste_waits = list_frame_waits_after_input_event(|client_id, _| RuntimeEvent::HostPaste {
        client_id,
        pasted_text: String::from("a"),
    });
    assert_eq!(paste_waits, own_input_waits);
    let mouse_round_waits =
        list_frame_waits_after_input_event(|client_id, _| RuntimeEvent::ClientMouse {
            client_id,
            request_id: 1,
            mouse_actions: Vec::new(),
        });
    assert_eq!(mouse_round_waits, own_input_waits);
    let keybinding_command_waits = list_frame_waits_after_input_event(|client_id, _| {
        build_toggle_lock_mode_event(CommandSource::KeyBinding { client_id }, client_id)
    });
    assert_eq!(keybinding_command_waits, own_input_waits);
    let mouse_command_waits = list_frame_waits_after_input_event(|client_id, _| {
        build_toggle_lock_mode_event(CommandSource::Mouse { client_id }, client_id)
    });
    assert_eq!(mouse_command_waits, own_input_waits);

    let outside_command_waits = list_frame_waits_after_input_event(|client_id, session_id| {
        build_toggle_lock_mode_event(
            CommandSource::ExternalCli {
                session_id: Some(session_id),
                target_client_id: None,
            },
            client_id,
        )
    });
    assert_eq!(
        outside_command_waits,
        (
            Some(DEFAULT_FRAME_INTERVAL_DURATION),
            Some(DEFAULT_FRAME_INTERVAL_DURATION)
        )
    );
}

#[test]
fn the_exit_of_a_tiled_pane_and_of_a_floating_pane_each_mark_every_view() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, _root_pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    let CommandResult::Ok { emitted_events, .. } =
        server.submit_command(CommandEnvelope::from_parts(
            CommandId::new(),
            CommandSource::KeyBinding { client_id },
            Command::NewPane(build_new_pane_args()),
        ))
    else {
        panic!("the split is handled");
    };
    let tiled_pane_id = emitted_events
        .iter()
        .find_map(|emitted_event| match emitted_event {
            koshi_core::event::Event::PaneCreated(pane_created) => Some(pane_created.pane_id),
            _ => None,
        })
        .expect("the split creates a pane");
    let floating_pane_id = create_floating_pane(
        &mut server,
        CommandSource::KeyBinding { client_id },
        build_floating_new_pane_args(None, None, false),
    );
    let (_delivery_receiver, _) = server.subscribe(client_id, 60);
    let first_exit_time = Instant::now();
    server.push_due_frames(first_exit_time);

    let _ = server.handle_child_exit(tiled_pane_id, koshi_core::process::ExitStatus::ExitCode(0));
    assert_eq!(
        mark_changed_views_then_compute_frame_wait(&mut server, client_id, first_exit_time),
        Some(DEFAULT_FRAME_INTERVAL_DURATION)
    );

    let second_exit_time = first_exit_time + DEFAULT_FRAME_INTERVAL_DURATION;
    server.push_due_frames(second_exit_time);
    assert_eq!(
        mark_changed_views_then_compute_frame_wait(&mut server, client_id, second_exit_time),
        None
    );
    let _ = server.handle_child_exit(
        floating_pane_id,
        koshi_core::process::ExitStatus::ExitCode(0),
    );
    assert_eq!(
        mark_changed_views_then_compute_frame_wait(&mut server, client_id, second_exit_time),
        Some(DEFAULT_FRAME_INTERVAL_DURATION)
    );
}

/// An 80x24 engine that shows one kitty animation in one cell and loops it
/// forever: frame 1 is red for `first_gap_ms`, frame 2 is blue for
/// `second_gap_ms`, and each gap is at least `1`. The animation runs, and its
/// clock starts at the next advance.
fn build_two_frame_animated_terminal_engine_with_gaps(
    first_gap_ms: u32,
    second_gap_ms: u32,
) -> TerminalEngine {
    let mut terminal_engine = TerminalEngine::from_pty_size(TEST_PTY_SIZE);
    let second_frame_command =
        format!("\x1b_Ga=f,i=7,f=32,s=1,v=1,z={second_gap_ms},q=2;AAD//w==\x1b\\");
    let first_frame_command = format!("\x1b_Ga=a,i=7,r=1,z={first_gap_ms},q=2\x1b\\");
    let kitty_commands: [&[u8]; 4] = [
        b"\x1b_Ga=T,i=7,f=32,s=1,v=1,c=1,r=1,C=1,q=2;/wAA/w==\x1b\\",
        second_frame_command.as_bytes(),
        first_frame_command.as_bytes(),
        b"\x1b_Ga=a,i=7,s=3,v=1,q=2\x1b\\",
    ];
    for kitty_command in kitty_commands {
        assert_eq!(terminal_engine.process_pty_output(kitty_command), b"");
    }
    terminal_engine
}

/// [`build_two_frame_animated_terminal_engine_with_gaps`] with a red frame of
/// 100 ms and a blue frame of 300 ms.
fn build_two_frame_animated_terminal_engine() -> TerminalEngine {
    build_two_frame_animated_terminal_engine_with_gaps(100, 300)
}

/// The pixels that the first image placement of the pane `pane_id` shows.
fn get_shown_rgba_bytes(server: &Server, pane_id: PaneId) -> Vec<u8> {
    server.terminal_engine_by_pane_id[&pane_id]
        .get_terminal_state()
        .list_image_placements()[0]
        .get_image_record()
        .image
        .rgba_bytes
        .clone()
}

/// The red frame's pixels.
const RED_RGBA_BYTES: [u8; 4] = [255, 0, 0, 255];

/// The blue frame's pixels.
const BLUE_RGBA_BYTES: [u8; 4] = [0, 0, 255, 255];

#[test]
fn every_animated_pane_advances_when_another_pane_changes_frame() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    for pane_id in [first_pane_id, second_pane_id] {
        server.insert_terminal_engine(pane_id, build_two_frame_animated_terminal_engine());
    }
    let start_time = Instant::now();
    let frame_change_time = start_time + Duration::from_millis(100);
    server.advance_due_pane_renders(start_time);

    server.advance_due_pane_renders(frame_change_time);

    for pane_id in [first_pane_id, second_pane_id] {
        assert_eq!(get_shown_rgba_bytes(&server, pane_id), BLUE_RGBA_BYTES);
        assert_eq!(
            server.terminal_engine_by_pane_id[&pane_id]
                .get_next_image_animation_delay(frame_change_time),
            Some(Duration::from_millis(300))
        );
    }
}

#[test]
fn an_animated_pane_marks_the_view_that_shows_it_only_when_its_frame_changes() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    server.insert_terminal_engine(pane_id, build_two_frame_animated_terminal_engine());
    let (_delivery_receiver, _) = server.subscribe(client_id, 60);
    let start_time = Instant::now();
    server.push_due_frames(start_time);

    server.advance_due_pane_renders(start_time + Duration::from_millis(50));
    assert_eq!(
        mark_changed_views_then_compute_frame_wait(&mut server, client_id, start_time),
        None
    );
    server.advance_due_pane_renders(start_time + Duration::from_millis(100));
    assert_eq!(
        mark_changed_views_then_compute_frame_wait(&mut server, client_id, start_time),
        Some(DEFAULT_FRAME_INTERVAL_DURATION)
    );

    assert_eq!(get_shown_rgba_bytes(&server, pane_id), BLUE_RGBA_BYTES);
}

#[test]
fn an_animation_that_starts_after_an_idle_stretch_shows_its_first_frame_for_its_whole_gap() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    let (delivery_receiver, _) = server.subscribe(client_id, 60);
    let idle_time = Instant::now();
    server.push_due_frames(idle_time);
    assert_eq!(take_queued_frames(&delivery_receiver).len(), 1);
    server.insert_terminal_engine(pane_id, build_two_frame_animated_terminal_engine());
    // One whole second of grid steps after the frame at `idle_time`.
    let first_poll_time = idle_time + DEFAULT_FRAME_INTERVAL_DURATION * 60;

    assert_eq!(
        server.compute_next_render_wakeup(first_poll_time),
        Some(Duration::ZERO)
    );
    server.push_due_frames(first_poll_time);

    assert_eq!(get_shown_rgba_bytes(&server, pane_id), RED_RGBA_BYTES);
    // The new engine marks the view that shows the pane, so the frame at
    // `first_poll_time` shows the red frame.
    let first_animation_frames = take_queued_frames(&delivery_receiver);
    assert_eq!(first_animation_frames.len(), 1);
    assert_eq!(
        get_framed_rgba_bytes(&first_animation_frames[0]),
        RED_RGBA_BYTES
    );
    // The red frame shows for its 100 ms, rounded up to the frame grid.
    assert_eq!(
        server.compute_next_render_wakeup(first_poll_time),
        Some(DEFAULT_FRAME_INTERVAL_DURATION * 6)
    );
    let frame_change_time = first_poll_time + DEFAULT_FRAME_INTERVAL_DURATION * 6;
    server.push_due_frames(frame_change_time);
    assert_eq!(get_shown_rgba_bytes(&server, pane_id), BLUE_RGBA_BYTES);
    let changed_frames = take_queued_frames(&delivery_receiver);
    assert_eq!(changed_frames.len(), 1);
    assert_eq!(get_framed_rgba_bytes(&changed_frames[0]), BLUE_RGBA_BYTES);
    assert_eq!(
        server.compute_next_render_wakeup(frame_change_time),
        Some(DEFAULT_FRAME_INTERVAL_DURATION * 18)
    );
}

#[test]
fn an_overdue_animation_frame_wakes_the_loop_at_the_first_grid_time_after_now() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    server.insert_terminal_engine(pane_id, build_two_frame_animated_terminal_engine());
    let (delivery_receiver, _) = server.subscribe(client_id, 60);
    let start_time = Instant::now();
    // The pass sends the first frame and starts the animation clock: the red
    // frame ends at `start_time + 100 ms`.
    server.push_due_frames(start_time);
    assert_eq!(take_queued_frames(&delivery_receiver).len(), 1);

    // At 110 ms the red frame's end has passed. The first grid time after
    // 110 ms is the seventh, 116666669 ns after `start_time`.
    let late_time = start_time + Duration::from_millis(110);
    assert_eq!(
        server.compute_next_render_wakeup(late_time),
        Some(Duration::from_nanos(6_666_669))
    );
}

#[test]
fn a_detached_session_sets_no_wakeup_for_its_animation_and_an_attach_sees_the_frame_the_elapsed_time_selects(
) {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    server.insert_terminal_engine(pane_id, build_two_frame_animated_terminal_engine());
    let start_time = Instant::now();
    // The pass that read the animation starts its clock.
    server.push_due_frames(start_time);

    for elapsed_milliseconds in (0..10_000).step_by(50) {
        assert_eq!(
            server.compute_next_render_wakeup(
                start_time + Duration::from_millis(elapsed_milliseconds)
            ),
            None,
            "{elapsed_milliseconds} ms"
        );
    }
    // 10150 ms is 25 loops of 400 ms and 150 ms into the next one: blue.
    let attach_time = start_time + Duration::from_millis(10_150);
    let (delivery_receiver, _) = server.subscribe(client_id, 60);
    server.push_due_frames(attach_time);

    assert_eq!(get_shown_rgba_bytes(&server, pane_id), BLUE_RGBA_BYTES);
    let queued_frames = take_queued_frames(&delivery_receiver);
    assert_eq!(queued_frames.len(), 1);
    assert_eq!(get_framed_rgba_bytes(&queued_frames[0]), BLUE_RGBA_BYTES);
}

/// Open a new tab for `client_id` and show it to that client.
fn open_new_tab_for_client(server: &mut Server, client_id: ClientId) {
    let _ = server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        Command::NewTab(koshi_core::command::NewTabArgs {
            working_directory: None,
            client_id: Some(client_id),
        }),
    ));
}

/// Show the first tab of its session to `client_id`.
fn focus_first_tab_for_client(server: &mut Server, client_id: ClientId) {
    let _ = server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        Command::FocusTab(koshi_core::command::FocusTabArgs {
            focus_target: koshi_core::command::TabTarget::Index(0),
            client_id: Some(client_id),
        }),
    ));
}

/// The pixels of the first image placement of the first pane in `frame`.
fn get_framed_rgba_bytes(frame: &RenderSnapshot) -> Vec<u8> {
    frame.pane_snapshots[0].image_placement_snapshots[0]
        .get_image_record()
        .expect("the placement carries its record")
        .image
        .rgba_bytes
        .clone()
}

#[test]
fn a_three_loop_animation_that_ends_while_hidden_shows_its_last_frame_when_its_tab_is_shown() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    let mut terminal_engine = build_two_frame_animated_terminal_engine();
    // `v=4` plays the 400 ms loop three times.
    assert_eq!(
        terminal_engine.process_pty_output(b"\x1b_Ga=a,i=7,v=4,q=2\x1b\\"),
        b""
    );
    server.insert_terminal_engine(pane_id, terminal_engine);
    let (delivery_receiver, _) = server.subscribe(client_id, 60);
    open_new_tab_for_client(&mut server, client_id);
    assert!(!server.is_pane_shown_to_client(client_id, pane_id));
    let start_time = Instant::now();
    // The pass starts the animation clock and sends the new tab's frame.
    server.push_due_frames(start_time);
    assert_eq!(take_queued_frames(&delivery_receiver).len(), 1);
    assert_eq!(server.compute_next_render_wakeup(start_time), None);

    let show_time = start_time + Duration::from_millis(1300);
    focus_first_tab_for_client(&mut server, client_id);
    server.push_due_frames(show_time);

    let queued_frames = take_queued_frames(&delivery_receiver);
    assert_eq!(queued_frames.len(), 1);
    assert_eq!(get_framed_rgba_bytes(&queued_frames[0]), BLUE_RGBA_BYTES);
    assert_eq!(
        server.terminal_engine_by_pane_id[&pane_id].get_next_image_animation_delay(show_time),
        None
    );
    assert_eq!(server.compute_next_render_wakeup(show_time), None);
}

#[test]
fn a_client_that_switches_to_the_tab_of_a_running_animation_gets_its_frames_from_then_on() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    server.insert_terminal_engine(pane_id, build_two_frame_animated_terminal_engine());
    let (delivery_receiver, _) = server.subscribe(client_id, 60);
    open_new_tab_for_client(&mut server, client_id);
    let start_time = Instant::now();
    server.push_due_frames(start_time);
    assert_eq!(take_queued_frames(&delivery_receiver).len(), 1);
    assert_eq!(server.compute_next_render_wakeup(start_time), None);

    let show_time = start_time + Duration::from_millis(50);
    focus_first_tab_for_client(&mut server, client_id);
    server.push_due_frames(show_time);
    let shown_frames = take_queued_frames(&delivery_receiver);
    assert_eq!(shown_frames.len(), 1);
    assert_eq!(get_framed_rgba_bytes(&shown_frames[0]), RED_RGBA_BYTES);
    // The red frame ends 100 ms after `start_time`: 50 ms after `show_time`,
    // rounded up to the grid that the frame at `show_time` started.
    let frame_change_wakeup = Duration::from_nanos(50_000_001);
    assert_eq!(
        server.compute_next_render_wakeup(show_time),
        Some(frame_change_wakeup)
    );
    let frame_change_time = show_time + frame_change_wakeup;
    server.push_due_frames(frame_change_time);

    let changed_frames = take_queued_frames(&delivery_receiver);
    assert_eq!(changed_frames.len(), 1);
    assert_eq!(get_framed_rgba_bytes(&changed_frames[0]), BLUE_RGBA_BYTES);
}

#[test]
fn a_two_frame_animation_of_one_ms_frames_wakes_a_session_at_60_frames_a_second_at_most_60_times_a_second(
) {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    server.insert_terminal_engine(
        pane_id,
        build_two_frame_animated_terminal_engine_with_gaps(1, 1),
    );
    let (delivery_receiver, _) = server.subscribe(client_id, 60);
    let start_time = Instant::now();
    let end_time = start_time + Duration::from_secs(1);

    let mut wake_count = 0;
    let mut frame_count = 0;
    let mut current_time = start_time;
    while current_time < end_time {
        server.push_due_frames(current_time);
        wake_count += 1;
        frame_count += take_queued_frames(&delivery_receiver).len();
        let Some(wakeup_duration) = server.compute_next_render_wakeup(current_time) else {
            break;
        };
        assert_ne!(
            wakeup_duration,
            Duration::ZERO,
            "the loop spins at {current_time:?}"
        );
        current_time += wakeup_duration;
    }

    // Every wake is on the client's frame grid: 0 ms, 16666667 ns, and so on
    // to 983333353 ns.
    assert_eq!(wake_count, 60);
    // The grid samples a 2 ms loop every 16666667 ns, 666667 ns further into
    // the loop each time: the shown frame changes at grid steps 2, 3, 5, 6 and
    // on to 59, two of every three steps, and the first frame goes at 0 ms.
    assert_eq!(frame_count, 40);
}

#[test]
fn one_expiring_pane_among_a_thousand_idle_panes_is_the_only_one_the_index_holds() {
    // Every pane is parked. Only the last pane opens a synchronized-output
    // group. The index holds that pane alone, a pass reads no idle engine, and
    // the pass at the deadline releases the group.
    let (mut server, fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let pane_ids: Vec<PaneId> = (0..1001).map(|_| PaneId::new()).collect();
    for pane_id in &pane_ids {
        spawn_and_park_test_pane(&mut server, &fake_pty_backend, *pane_id);
    }
    let expiring_pane_id = pane_ids[1000];
    server.handle_pty_output(expiring_pane_id, b"\x1b[?2026hX");
    let render_deadline = server
        .pane_synchronized_output_deadlines
        .find_earliest_render_deadline()
        .expect("the open group holds a deadline");

    assert_eq!(
        server
            .pane_synchronized_output_deadlines
            .take_due_pane_ids(render_deadline + Duration::from_secs(3600)),
        vec![expiring_pane_id]
    );
    server.refresh_pane_render_deadlines(expiring_pane_id, render_deadline);
    assert_eq!(
        server
            .pane_synchronized_output_deadlines
            .find_earliest_render_deadline(),
        Some(render_deadline)
    );
    assert_eq!(
        server.compute_next_render_wakeup(render_deadline),
        Some(Duration::ZERO)
    );
    server.advance_due_pane_renders(render_deadline);

    assert_eq!(
        server.terminal_engine_by_pane_id[&expiring_pane_id]
            .get_terminal_state()
            .get_active_grid()
            .get_cell(0, 0)
            .expect("cell in bounds")
            .get_character(),
        'X'
    );
    assert_eq!(
        server
            .pane_synchronized_output_deadlines
            .find_earliest_render_deadline(),
        None
    );
    assert_eq!(server.compute_next_render_wakeup(render_deadline), None);
}

#[test]
fn a_released_pane_leaves_both_render_deadline_indexes() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (_client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    server.insert_terminal_engine(pane_id, build_two_frame_animated_terminal_engine());
    server.handle_pty_output(pane_id, b"\x1b[?2026h");
    for pane_render_deadlines in [
        &server.pane_synchronized_output_deadlines,
        &server.pane_animation_deadlines,
    ] {
        assert_eq!(
            pane_render_deadlines
                .list_render_deadlines()
                .map(|(_, deadline_pane_id)| deadline_pane_id)
                .collect::<Vec<PaneId>>(),
            vec![pane_id]
        );
    }

    let _ = server.handle_child_exit(pane_id, koshi_core::process::ExitStatus::ExitCode(0));

    assert_eq!(
        server
            .pane_synchronized_output_deadlines
            .find_earliest_render_deadline(),
        None
    );
    assert_eq!(
        server
            .pane_animation_deadlines
            .find_earliest_render_deadline(),
        None
    );
}

/// A resize of `client_id`'s terminal to `column_count` × `row_count`, with no
/// pane area and no cell size.
fn build_resize_event(client_id: ClientId, column_count: u16, row_count: u16) -> RuntimeEvent {
    RuntimeEvent::Resize {
        client_id,
        viewport_size: Size {
            column_count,
            row_count,
        },
        pane_area: None,
        cell_size: None,
    }
}

/// Each live pane's PTY size, by pane id.
fn list_pty_sizes(server: &Server) -> BTreeMap<PaneId, PtySize> {
    server
        .live_pane_ids
        .iter()
        .map(|pane_id| (*pane_id, server.pty_size_by_pane_id[pane_id]))
        .collect()
}

#[test]
fn a_batch_of_thirty_resizes_from_one_client_resizes_the_pane_once_at_the_last_size() {
    let (mut server, fake_pty_backend, inbox_sender) = build_test_runtime_with_inbox_sender();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    let pane_sizes_before = fake_pty_backend
        .list_pane_sizes(pane_id)
        .expect("the pane is spawned");
    for row_count in 31..=60 {
        inbox_sender
            .send(build_resize_event(client_id, 100, row_count))
            .expect("the inbox is open");
    }

    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);

    let last_pty_size = server.pty_size_by_pane_id[&pane_id];
    let mut expected_pane_sizes = pane_sizes_before;
    expected_pane_sizes.push(last_pty_size);
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(pane_id)
            .expect("the pane is spawned"),
        expected_pane_sizes
    );
    let terminal_grid_size = server.terminal_engine_by_pane_id[&pane_id]
        .get_terminal_state()
        .get_active_grid()
        .get_grid_dimensions();
    assert_eq!(
        terminal_grid_size,
        (last_pty_size.row_count, last_pty_size.column_count)
    );
    let session = server.get_session_for_client(client_id).expect("session");
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_viewport_size(),
        Size {
            column_count: 100,
            row_count: 60,
        }
    );
}

#[test]
fn a_resize_then_another_event_then_a_resize_applies_both_resizes_in_order() {
    let (mut server, fake_pty_backend, inbox_sender) = build_test_runtime_with_inbox_sender();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    let mut expected_pane_sizes = fake_pty_backend
        .list_pane_sizes(pane_id)
        .expect("the pane is spawned");
    inbox_sender
        .send(build_resize_event(client_id, 90, 30))
        .expect("the inbox is open");
    let (layout_sender, layout_receiver) = mpsc::channel();
    inbox_sender
        .send(RuntimeEvent::IpcLayout {
            tab_id: None,
            response_sender: layout_sender,
        })
        .expect("the inbox is open");
    inbox_sender
        .send(build_resize_event(client_id, 100, 35))
        .expect("the inbox is open");

    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);

    let layout_between_resizes = layout_receiver
        .recv()
        .expect("the reply")
        .expect("one session is running");
    assert_eq!(
        layout_between_resizes.tabs[0].solved_tabs[0].viewport_size,
        Size {
            column_count: 90,
            row_count: 28,
        }
    );
    let resized_sizes = fake_pty_backend
        .list_pane_sizes(pane_id)
        .expect("the pane is spawned");
    let mut first_resize_server = build_test_runtime_with_fake_pty_backend().0;
    let (first_resize_client_id, first_resize_pane_id) =
        bootstrap_test_session(&mut first_resize_server, SessionId::new());
    first_resize_server.handle_client_resize(
        first_resize_client_id,
        Size {
            column_count: 90,
            row_count: 30,
        },
        None,
        None,
    );
    expected_pane_sizes.push(first_resize_server.pty_size_by_pane_id[&first_resize_pane_id]);
    expected_pane_sizes.push(server.pty_size_by_pane_id[&pane_id]);
    assert_eq!(resized_sizes, expected_pane_sizes);
}

#[test]
fn the_resize_runs_of_two_clients_in_one_batch_each_apply_their_last_size() {
    let (mut server, fake_pty_backend, inbox_sender) = build_test_runtime_with_inbox_sender();
    let (first_client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    let second_client_id = attach_second_client(&mut server, first_client_id, Some(pane_id));
    let mut expected_pane_sizes = fake_pty_backend
        .list_pane_sizes(pane_id)
        .expect("the pane is spawned");
    for (client_id, column_count) in [
        (first_client_id, 70),
        (first_client_id, 60),
        (second_client_id, 70),
        (second_client_id, 50),
    ] {
        inbox_sender
            .send(build_resize_event(client_id, column_count, 24))
            .expect("the inbox is open");
    }

    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);

    let mut reference_server = build_test_runtime_with_fake_pty_backend().0;
    let (reference_first_client_id, reference_pane_id) =
        bootstrap_test_session(&mut reference_server, SessionId::new());
    attach_second_client(
        &mut reference_server,
        reference_first_client_id,
        Some(reference_pane_id),
    );
    reference_server.handle_client_resize(
        reference_first_client_id,
        Size {
            column_count: 60,
            row_count: 24,
        },
        None,
        None,
    );
    expected_pane_sizes.push(reference_server.pty_size_by_pane_id[&reference_pane_id]);
    expected_pane_sizes.push(server.pty_size_by_pane_id[&pane_id]);
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(pane_id)
            .expect("the pane is spawned"),
        expected_pane_sizes
    );
}

#[test]
fn a_merged_resize_run_leaves_the_layout_that_every_size_in_turn_leaves() {
    let (mut server, _fake_pty_backend, inbox_sender) = build_test_runtime_with_inbox_sender();
    let session_id = SessionId::new();
    let (client_id, _pane_id) = bootstrap_test_session(&mut server, session_id);
    let new_pane_result = server.dispatch(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let CommandResult::Ok { .. } = new_pane_result else {
        panic!("expected the split pane to be created, got {new_pane_result:?}");
    };
    create_floating_pane(
        &mut server,
        CommandSource::from_key_binding(client_id),
        build_floating_new_pane_args(None, None, false),
    );
    let viewport_sizes: Vec<(u16, u16)> = (0..30)
        .map(|step_index| (60 + (step_index * 7) % 50, 15 + (step_index * 5) % 30))
        .collect();

    for &(column_count, row_count) in &viewport_sizes {
        inbox_sender
            .send(build_resize_event(client_id, column_count, row_count))
            .expect("the inbox is open");
        assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);
    }
    let layout_after_each_size = server.build_session_layout(None);
    let floating_sizes_after_each_size = list_floating_solved_sizes(&server, session_id);
    let pty_sizes_after_each_size = list_pty_sizes(&server);

    inbox_sender
        .send(build_resize_event(client_id, 80, 24))
        .expect("the inbox is open");
    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);
    for &(column_count, row_count) in &viewport_sizes {
        inbox_sender
            .send(build_resize_event(client_id, column_count, row_count))
            .expect("the inbox is open");
    }
    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);

    assert_eq!(server.build_session_layout(None), layout_after_each_size);
    assert_eq!(
        list_floating_solved_sizes(&server, session_id),
        floating_sizes_after_each_size
    );
    assert_eq!(list_pty_sizes(&server), pty_sizes_after_each_size);
}

/// Each floating pane of `session_id` and its solved size, in the order the
/// panes became floating.
fn list_floating_solved_sizes(server: &Server, session_id: SessionId) -> Vec<(PaneId, Size)> {
    server.session_by_id[&session_id]
        .floating_set
        .list_members()
        .iter()
        .map(|floating_member| (floating_member.pane_id, floating_member.solved_size))
        .collect()
}

/// An IPC event that runs `command` under `command_id` from `command_source`,
/// naming `fired_action`. Its reply channel is closed.
fn build_fired_command_event(
    command_id: CommandId,
    command_source: CommandSource,
    command: Command,
    fired_action: &str,
) -> RuntimeEvent {
    let (response_sender, _response_receiver) = mpsc::channel();
    RuntimeEvent::Ipc {
        command_envelope: Box::new(CommandEnvelope::from_parts(
            command_id,
            command_source,
            command,
        )),
        fired_action: Some(
            ActionReference::from_core_action_name(fired_action).expect("the action name is valid"),
        ),
        response_sender,
    }
}

#[test]
fn a_refused_key_binding_command_goes_back_to_the_pressing_client_alone_with_its_action_and_help() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    let other_client_id = attach_second_client(&mut server, client_id, Some(pane_id));
    let (delivery_receiver, _) = server.subscribe(client_id, 60);
    let (other_delivery_receiver, _) = server.subscribe(other_client_id, 60);
    let command_id = CommandId::new();

    let control_flow = server.handle_runtime_event(build_fired_command_event(
        command_id,
        CommandSource::from_key_binding(client_id),
        Command::MoveFloatingPane(MoveFloatingPaneArgs {
            pane_id,
            top_left_cell: Point { column: 2, row: 3 },
        }),
        "confirm-floating-pane-move",
    ));

    assert_eq!(control_flow, ControlFlow::Continue(()));
    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<Delivery>>(),
        vec![Delivery::CommandRejected {
            command_id,
            fired_action: Some(
                ActionReference::from_core_action_name("confirm-floating-pane-move")
                    .expect("the action name is valid"),
            ),
            reject_reason: RejectReason::InvalidState,
            help: Some(format!(
                "{pane_id} is tiled; only a floating pane moves this way"
            )),
        }]
    );
    assert_eq!(
        other_delivery_receiver
            .try_iter()
            .collect::<Vec<Delivery>>(),
        Vec::new()
    );
}

#[test]
fn a_key_that_finds_no_pane_in_its_direction_or_no_floating_pane_goes_back_as_nothing() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let (client_id, _pane_id) = bootstrap_test_session(&mut server, SessionId::new());
    let (delivery_receiver, _) = server.subscribe(client_id, 60);

    for (focus_target, fired_action) in [
        (FocusTarget::Direction(Direction::Left), "focus-pane-left"),
        (FocusTarget::NextFloatingPane, "focus-next-floating-pane"),
        (
            FocusTarget::PreviousFloatingPane,
            "focus-previous-floating-pane",
        ),
    ] {
        let _ = server.handle_runtime_event(build_fired_command_event(
            CommandId::new(),
            CommandSource::from_key_binding(client_id),
            Command::FocusPane(FocusPaneArgs {
                focus_target,
                client_id: None,
            }),
            fired_action,
        ));
    }

    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<Delivery>>(),
        Vec::new()
    );
}

#[test]
fn a_refused_command_from_the_command_line_goes_back_to_no_client() {
    let (mut server, _fake_pty_backend) = build_test_runtime_with_fake_pty_backend();
    let session_id = SessionId::new();
    let (client_id, pane_id) = bootstrap_test_session(&mut server, session_id);
    let (delivery_receiver, _) = server.subscribe(client_id, 60);

    let _ = server.handle_runtime_event(build_fired_command_event(
        CommandId::new(),
        CommandSource::from_external_cli(Some(session_id), Some(client_id)),
        Command::MoveFloatingPane(MoveFloatingPaneArgs {
            pane_id,
            top_left_cell: Point { column: 2, row: 3 },
        }),
        "confirm-floating-pane-move",
    ));

    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<Delivery>>(),
        Vec::new()
    );
}
