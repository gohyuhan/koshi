//! Integration cover for attaching over a real control socket: what one
//! attach registers, what the reply carries and when it is written, what a
//! second attach sees, what an attach naming a field this build does not know
//! still sets, which lock command of a client the frame that shows its lock
//! names, what a client's key presses and resizes reach, what a client's
//! mouse rounds do to its panes and what the one answer each round is given
//! carries, what a detach leaves behind for the clients that stay and for the
//! panes, what a dropped connection leaves behind, and what a frame this build
//! cannot read costs the client that sent it.

mod common;

use std::sync::mpsc;

use common::{
    count_attached_clients, get_last_painted_frame, get_session_overview, open_session_connection,
    read_session_frames_to_detached, read_session_frames_to_mouse_answer,
    read_session_frames_until, read_session_frames_until_painted_frame, serve_test_session,
    submit_test_command, submit_test_command_for_client, wait_for_client_count,
    wait_for_mouse_tracking, ViewerConnection, TEST_SESSION_NAME, TEST_VIEWPORT_SIZE,
    TEST_WAIT_TIMEOUT_DURATION,
};

use koshi_core::command::{
    CloseTabArgs, Command, CommandEnvelope, CommandSource, DetachArgs, FocusPaneArgs, FocusTabArgs,
    FocusTarget, LockModeArgs, MoveFloatingPaneArgs, NewPaneArgs, NewPanePlacement, NewTabArgs,
    PanePlacementAnchor, PanePlacementTarget, TabTarget, TiledPlacementTarget,
};
use koshi_core::event::{Event, QuitCause};
use koshi_core::geometry::{Direction, Point, Rect, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::key::{BindingModifierFlags, Key, KeyChord};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseAnswer, MouseButton, MouseKind};
use koshi_core::process::{ExitStatus, PtySize};
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::frame::PaintedFrame;
use koshi_ipc::placement::{
    PanePlacementPreview, PanePlacementPreviewTarget, PanePlacementSnapshot, PanePlacementSource,
};
use koshi_ipc::protocol::{
    ConnectionToken, IpcRequest, IpcRequestKind, IpcResponse, IpcResult, WireMouseAction,
};
use koshi_ipc::transport::{Connection, ReceivedFrame};
use koshi_layout::mode::LayoutMode;
use koshi_layout::placement::{
    place_pane_within_tab, solve_placed_tab, PlacedTabLayout, PlacementTarget,
};
use koshi_layout::solver::PaneSizing;
use koshi_layout::tree::LayoutNode;
use koshi_runtime::runtime::painted_update::SessionFrame;
use koshi_session::client::{compute_default_pane_area_size, Client, ClientOrigin};
use koshi_test_support::fake_pty::FakePtyBackend;
use koshi_test_support::fixtures::{
    build_key_input_for_chord, read_prepared_and_send_commit, send_committed_request,
};

/// Attach on `ipc_connection` reporting [`TEST_VIEWPORT_SIZE`], and return what
/// the reply carried. The connection carries only the client's event stream
/// afterwards.
fn attach_test_client(
    ipc_connection: &mut Connection,
    request_id: u64,
) -> (
    ClientId,
    SessionId,
    AttachedSessionStructureSnapshot,
    Option<ConnectionToken>,
) {
    attach_test_client_with_viewport_size(ipc_connection, request_id, TEST_VIEWPORT_SIZE)
}

/// [`attach_test_client`] reporting `viewport_size` instead, for a test that
/// puts two differently sized clients on one tab.
fn attach_test_client_with_viewport_size(
    ipc_connection: &mut Connection,
    request_id: u64,
    viewport_size: Size,
) -> (
    ClientId,
    SessionId,
    AttachedSessionStructureSnapshot,
    Option<ConnectionToken>,
) {
    attach_test_client_with_resume_token(ipc_connection, request_id, viewport_size, None)
}

/// [`attach_test_client_with_viewport_size`] presenting `resume_token`, which
/// asks the session for the view the token's client left behind.
fn attach_test_client_with_resume_token(
    ipc_connection: &mut Connection,
    request_id: u64,
    viewport_size: Size,
    resume_token: Option<ConnectionToken>,
) -> (
    ClientId,
    SessionId,
    AttachedSessionStructureSnapshot,
    Option<ConnectionToken>,
) {
    send_committed_request(
        ipc_connection,
        IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Attach {
                viewport_size,
                resume_client_id: None,
                resume_token,
                pane_area: None,
                graphics_capabilities: koshi_ipc::protocol::GraphicsCapabilities::default(),
                cell_size: None,
                link_timeout_ms: None,
                max_frames_per_second: 60,
                placement_preview_target: None,
            },
        },
    );
    let attach_response: IpcResponse = ipc_connection.recv().expect("attach reply");
    assert_eq!(attach_response.request_id, Some(request_id));
    let IpcResult::Attached {
        client_id,
        session_id,
        session_structure,
        resume_token,
        ..
    } = attach_response.answer_result
    else {
        panic!(
            "expected an attach reply, got {:?}",
            attach_response.answer_result
        );
    };
    (client_id, session_id, session_structure, resume_token)
}

/// Add one tab to the running session over `ipc_connection`, and return its id.
fn create_test_tab(
    ipc_connection: &mut Connection,
    session_id: SessionId,
    request_id: u64,
) -> TabId {
    let command = Command::NewTab(NewTabArgs {
        working_directory: None,
        client_id: None,
    });
    submit_test_command(ipc_connection, session_id, command, request_id)
        .iter()
        .find_map(|emitted_event| match emitted_event {
            Event::TabCreated(tab_created) => Some(tab_created.tab_id),
            _ => None,
        })
        .expect("the new tab reports its id")
}

/// Split `source_pane_id` over `ipc_connection` on behalf of `client_id`, with
/// the new pane to the right, and return the new pane's id.
fn create_right_split_pane(
    ipc_connection: &mut Connection,
    session_id: SessionId,
    source_pane_id: PaneId,
    client_id: ClientId,
    request_id: u64,
) -> PaneId {
    let command = Command::NewPane(NewPaneArgs {
        placement: NewPanePlacement::Split {
            source_pane_id: Some(source_pane_id),
            tab_id: None,
            direction: Direction::Right,
        },
        working_directory: None,
        spawn_spec: None,
        client_id: Some(client_id),
    });
    submit_test_command(ipc_connection, session_id, command, request_id)
        .iter()
        .find_map(|emitted_event| match emitted_event {
            Event::PaneCreated(pane_created) => Some(pane_created.pane_id),
            _ => None,
        })
        .expect("the split reports the new pane")
}

/// Half the columns of [`TEST_VIEWPORT_SIZE`], the size the seeded session was
/// bootstrapped at.
const NARROW_VIEWPORT_SIZE: Size = Size {
    column_count: 40,
    row_count: 24,
};

/// One envelope asking `session_id` for a new tab, issued by the external CLI
/// under a fresh command id.
fn build_new_tab_envelope(session_id: SessionId) -> CommandEnvelope {
    CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_external_cli(Some(session_id), None),
        Command::NewTab(NewTabArgs {
            working_directory: None,
            client_id: None,
        }),
    )
}

#[test]
fn one_attach_registers_the_client_the_server_minted() {
    let (server, _fake_pty_backend, (session_id, client_id, session_structure)) =
        serve_test_session(
            "registers",
            |runtime_directory, session_id, _fake_pty_backend| {
                let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
                let (client_id, replied_session_id, session_structure, _) =
                    attach_test_client(&mut viewer_connection, 2);
                assert_eq!(replied_session_id, session_id);
                (
                    vec![viewer_connection],
                    (session_id, client_id, session_structure),
                )
            },
        );

    assert_eq!(session_structure.session_id, session_id);
    assert_eq!(session_structure.session_name, TEST_SESSION_NAME);
    assert_eq!(session_structure.tabs.len(), 1);
    assert_eq!(session_structure.tabs[0].tab_index, 0);
    assert_eq!(
        session_structure.tabs[0].layout.list_leaf_pane_ids().len(),
        1
    );

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.clients.count_clients(), 1);
    let client = session
        .clients
        .get_client_by_id(client_id)
        .expect("minted client");
    assert_eq!(client.get_client_id(), client_id);
    assert_eq!(client.get_session_id(), session_id);
    assert_eq!(client.get_origin(), ClientOrigin::Local);
    assert_eq!(client.get_color_index(), 0);
    assert_eq!(client.get_viewport_size(), TEST_VIEWPORT_SIZE);
    assert_eq!(client.get_active_tab_id(), session_structure.tabs[0].tab_id);
    let label_parts: Vec<&str> = client.get_label().split('-').collect();
    assert_eq!(
        label_parts.len(),
        3,
        "generated label, got {}",
        client.get_label()
    );
    assert_eq!(label_parts[0], "C");
}

#[test]
fn the_frame_that_shows_a_viewers_lock_names_the_lock_command_it_sent() {
    let (_server, _fake_pty_backend, (lock_command_id, frames_client_snapshots)) =
        serve_test_session(
            "lock-answer",
            |runtime_directory, session_id, _fake_pty_backend| {
                let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
                let (client_id, _, _, _) = attach_test_client(&mut viewer_connection, 2);
                let lock_command_id = CommandId::new();
                viewer_connection
                    .send(&IpcRequest {
                        request_id: 3,
                        request_kind: IpcRequestKind::SubmitCommand(Box::new(
                            CommandEnvelope::from_parts(
                                lock_command_id,
                                CommandSource::from_key_binding(client_id),
                                Command::SetLockMode(LockModeArgs {
                                    is_locked: true,
                                    client_id: None,
                                }),
                            ),
                        )),
                    })
                    .expect("send the lock command");
                let (viewer_connection, session_frames) = read_session_frames_until_painted_frame(
                    viewer_connection,
                    |received_painted_frame| {
                        received_painted_frame
                            .painted_frame
                            .client_snapshot
                            .lock_mode
                            == LockMode::Locked
                    },
                );
                let frames_client_snapshots: Vec<_> = session_frames
                    .iter()
                    .filter_map(|session_frame| match session_frame {
                        SessionFrame::Painted(received_painted_frame) => {
                            Some(received_painted_frame.painted_frame.client_snapshot.clone())
                        }
                        SessionFrame::Event(_) | SessionFrame::ImageChunk(_) => None,
                    })
                    .collect();
                (
                    vec![viewer_connection.ipc_connection],
                    (lock_command_id, frames_client_snapshots),
                )
            },
        );

    let (locked_client_snapshot, earlier_client_snapshots) = frames_client_snapshots
        .split_last()
        .expect("the run ends with the locked frame");
    assert_eq!(
        locked_client_snapshot.last_applied_lock_command_id,
        Some(lock_command_id)
    );
    for earlier_client_snapshot in earlier_client_snapshots {
        assert_eq!(earlier_client_snapshot.lock_mode, LockMode::Normal);
        assert_eq!(earlier_client_snapshot.last_applied_lock_command_id, None);
    }
}

#[test]
fn nothing_in_the_request_can_raise_the_clients_authority() {
    let (server, _fake_pty_backend, session_id) = serve_test_session(
        "strict",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);

            // A well-framed attach naming one field this build does not know. The
            // field is ignored and the attach succeeds. Every fact about the client
            // it mints comes from the server, never from these bytes.
            viewer_connection
                .send(&serde_json::json!({
                    "request_id": 2,
                    "request_kind": {
                        "Prepare": {
                            "Attach": {
                                "viewport_size": { "column_count": 80, "row_count": 24 },
                                "max_frames_per_second": 60,
                                "tier": "admin"
                            }
                        }
                    }
                }))
                .expect("send an attach carrying an extra field");
            read_prepared_and_send_commit(&mut viewer_connection, 2);

            let attach_response: IpcResponse = viewer_connection.recv().expect("attach reply");
            assert_eq!(attach_response.request_id, Some(2));
            let IpcResult::Attached {
                session_id: joined_session_id,
                ..
            } = attach_response.answer_result
            else {
                panic!(
                    "the attach was answered with {:?}",
                    attach_response.answer_result
                );
            };
            assert_eq!(
                joined_session_id, session_id,
                "the attach joined the session it named"
            );
            (vec![viewer_connection], session_id)
        },
    );

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(
        session.clients.count_clients(),
        1,
        "the attach registered one client"
    );
    let client = session
        .clients
        .list_attached_clients()
        .next()
        .expect("the one attached client");
    assert_eq!(
        client.get_origin(),
        ClientOrigin::Local,
        "the origin comes from the connection, not the request"
    );
    assert_eq!(
        client.get_viewport_size(),
        Size {
            column_count: 80,
            row_count: 24
        },
        "the viewport is the one field of the attach the server does take"
    );
}

#[test]
fn the_structure_reply_is_written_before_the_first_event_frame() {
    let (server, _fake_pty_backend, (session_id, booted_tab_id, added_tab_id)) = serve_test_session(
        "reply-first",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);

            // Frame one on this connection decodes as a response: no event frame
            // was written ahead of the reply.
            let (_, _, session_structure, _) = attach_test_client(&mut viewer_connection, 2);
            assert_eq!(session_structure.tabs.len(), 1);

            // Everything after it is an event frame. Reading one blocks with no
            // deadline of its own and runs on a thread this one can give up
            // waiting on.
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let added_tab_id = create_test_tab(&mut caller_connection, session_id, 3);

            let (created_tab_id_sender, created_tab_id_receiver) = mpsc::channel();
            std::thread::spawn(move || loop {
                let received_frame = viewer_connection
                    .recv_message_or_binary::<SessionEvent>()
                    .expect("an event frame");
                if let ReceivedFrame::Message(SessionEvent::TabCreated { tab_id }) = received_frame
                {
                    let _ = created_tab_id_sender.send(tab_id);
                    return;
                }
            });
            assert_eq!(
                created_tab_id_receiver
                    .recv_timeout(TEST_WAIT_TIMEOUT_DURATION)
                    .expect("the new tab reaches the event stream"),
                added_tab_id,
            );
            (
                vec![caller_connection],
                (session_id, session_structure.tabs[0].tab_id, added_tab_id),
            )
        },
    );

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.tabs.len(), 2);
    assert!(session.tabs.contains_key(&booted_tab_id));
    assert!(session.tabs.contains_key(&added_tab_id));
}

#[test]
fn a_second_attach_mints_a_fresh_client_and_sees_the_tab_added_since_the_first() {
    let (server, _fake_pty_backend, (session_id, initial_client_id, additional_client_id)) =
        serve_test_session(
            "reattach",
            |runtime_directory, session_id, _fake_pty_backend| {
                let mut initial_connection =
                    open_session_connection(&runtime_directory, session_id);
                let (initial_client_id, _, initial_attach_snapshot, _) =
                    attach_test_client(&mut initial_connection, 2);
                assert_eq!(initial_attach_snapshot.tabs.len(), 1);
                let booted_tab_id = initial_attach_snapshot.tabs[0].tab_id;

                let mut caller_connection = open_session_connection(&runtime_directory, session_id);
                let added_tab_id = create_test_tab(&mut caller_connection, session_id, 3);

                let mut additional_connection =
                    open_session_connection(&runtime_directory, session_id);
                let (additional_client_id, _, additional_attach_snapshot, _) =
                    attach_test_client(&mut additional_connection, 4);
                assert_ne!(additional_client_id, initial_client_id);
                assert_eq!(
                    additional_attach_snapshot
                        .tabs
                        .iter()
                        .map(|tab| tab.tab_id)
                        .collect::<Vec<TabId>>(),
                    vec![booted_tab_id, added_tab_id],
                    "the second attach is built from live state, not a cached copy",
                );
                (
                    vec![initial_connection, additional_connection, caller_connection],
                    (session_id, initial_client_id, additional_client_id),
                )
            },
        );

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.clients.count_clients(), 2);
    let initial_client_record = session
        .clients
        .get_client_by_id(initial_client_id)
        .expect("initial client");
    let additional_client_record = session
        .clients
        .get_client_by_id(additional_client_id)
        .expect("additional client");
    assert_eq!(initial_client_record.get_color_index(), 0);
    assert_eq!(additional_client_record.get_color_index(), 1);
    assert_ne!(
        initial_client_record.get_label(),
        additional_client_record.get_label()
    );
}

/// Close `tab_id`, the session's last tab, over `ipc_connection`, killing its
/// panes outright. Panics unless the close quits the session.
fn close_last_tab(
    ipc_connection: &mut Connection,
    session_id: SessionId,
    tab_id: TabId,
    request_id: u64,
) {
    let command = Command::CloseTab(CloseTabArgs {
        tab_id: Some(tab_id),
        should_force_close: true,
        should_kill_process_tree: false,
    });
    let emitted_events = submit_test_command(ipc_connection, session_id, command, request_id);
    assert_eq!(
        emitted_events.last(),
        Some(&Event::Quit(QuitCause::LastTabClosed {
            tab_id,
            pane_exit: None,
        })),
        "closing the last tab quits the session",
    );
}

#[test]
fn the_event_stream_ends_with_the_quit_frame() {
    let (server, _fake_pty_backend, session_id) = serve_test_session(
        "quit-ends-stream",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
            let (_, _, session_structure, _) = attach_test_client(&mut viewer_connection, 2);
            let only_tab_id = session_structure.tabs[0].tab_id;

            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            assert_eq!(count_attached_clients(&mut caller_connection, 3), 1);
            close_last_tab(&mut caller_connection, session_id, only_tab_id, 4);

            // The quit frame is the last one the viewer's stream carries.
            let (viewer_connection, session_frames) =
                read_session_frames_until(viewer_connection, |session_frame| {
                    *session_frame == SessionFrame::Event(SessionEvent::Quit)
                });
            assert_eq!(
                session_frames.last(),
                Some(&SessionFrame::Event(SessionEvent::Quit))
            );

            // The quit frame ends the stream. The viewer closes its connection
            // once it reads that frame, and the record goes with it.
            drop(viewer_connection);
            wait_for_client_count(&mut caller_connection, 0, 5);
            (vec![caller_connection], session_id)
        },
    );

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.clients.count_clients(), 0);
}

/// A pane's program exiting reaches every attached client while the session
/// keeps serving. A second pane is what keeps it serving: the session ends when
/// its last pane goes, and an ending session drops what is queued for a client.
#[test]
fn the_stream_carries_a_pane_exit_while_the_session_keeps_serving() {
    let (server, _fake_pty_backend, (session_id, exited_pane_id)) = serve_test_session(
        "pane-exit-on-stream",
        |runtime_directory, session_id, fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, session_structure, _) =
                attach_test_client(&mut viewer_connection, 2);
            let existing_pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let new_pane_id = create_right_split_pane(
                &mut caller_connection,
                session_id,
                existing_pane_id,
                client_id,
                3,
            );

            fake_pty_backend
                .trigger_child_exit(new_pane_id, ExitStatus::ExitCode(0))
                .expect("the second pane's program exits");

            let (viewer_connection, session_frames) =
                read_session_frames_until(viewer_connection, move |session_frame| {
                    matches!(
                        session_frame,
                        SessionFrame::Event(SessionEvent::PaneProcessExited { pane_id, .. })
                            if *pane_id == new_pane_id
                    )
                });
            assert_eq!(
                session_frames.last(),
                Some(&SessionFrame::Event(SessionEvent::PaneProcessExited {
                    pane_id: new_pane_id,
                    exit_code: Some(0),
                    signal: None,
                })),
            );
            (
                vec![viewer_connection.ipc_connection, caller_connection],
                (session_id, new_pane_id),
            )
        },
    );

    // The pane the program left is gone, and the one the client is viewing is
    // still there.
    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.panes.count_pane_records(), 1);
    assert!(session
        .panes
        .get_pane_record_by_id(exited_pane_id)
        .is_none());
}

/// The only pane's program exiting ends the session by itself, with no close
/// asked for: the stream ends with the quit frame, and the session is left with
/// no pane and no tab.
#[test]
fn the_event_stream_ends_with_the_quit_frame_when_the_only_program_exits() {
    let (server, _fake_pty_backend, session_id) = serve_test_session(
        "quit-on-program-exit",
        |runtime_directory, session_id, fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
            let (_, _, session_structure, _) = attach_test_client(&mut viewer_connection, 2);
            let only_pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

            fake_pty_backend
                .trigger_child_exit(only_pane_id, ExitStatus::ExitCode(0))
                .expect("the only pane's program exits");

            // The exit and the quit the session ends on are published in one pass,
            // and the raised ending drops whatever is still queued for a client.
            // The quit frame is the one frame this stream is promised.
            let (viewer_connection, session_frames) =
                read_session_frames_until(viewer_connection, |session_frame| {
                    *session_frame == SessionFrame::Event(SessionEvent::Quit)
                });
            assert_eq!(
                session_frames.last(),
                Some(&SessionFrame::Event(SessionEvent::Quit))
            );
            (vec![viewer_connection.ipc_connection], session_id)
        },
    );

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.panes.count_pane_records(), 0);
    assert_eq!(session.tabs.len(), 0);
}

#[test]
fn dropping_an_attached_connection_removes_its_client_record() {
    let (server, _fake_pty_backend, (session_id, client_id, tab_id, pane_id)) = serve_test_session(
        "disconnect",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, session_structure, _) =
                attach_test_client(&mut viewer_connection, 2);
            let tab_id = session_structure.tabs[0].tab_id;
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            assert_eq!(count_attached_clients(&mut caller_connection, 3), 1);

            drop(viewer_connection);

            // The record goes when the serving thread notices the connection
            // ended. Ask again until it is gone.
            let next_request_id = wait_for_client_count(&mut caller_connection, 0, 4);

            // Losing the viewer costs the session nothing else: its tab and
            // its pane are both still there.
            let overview_after_disconnect =
                get_session_overview(&mut caller_connection, next_request_id);
            assert_eq!(
                overview_after_disconnect
                    .tabs
                    .iter()
                    .map(|tab| tab.tab_id)
                    .collect::<Vec<TabId>>(),
                vec![tab_id],
            );
            assert_eq!(
                overview_after_disconnect
                    .panes
                    .iter()
                    .map(|pane| pane.pane_id)
                    .collect::<Vec<PaneId>>(),
                vec![pane_id],
            );
            (
                vec![caller_connection],
                (session_id, client_id, tab_id, pane_id),
            )
        },
    );

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.clients.count_clients(), 0);
    assert!(session.clients.get_client_by_id(client_id).is_none());
    assert!(session.tabs.contains_key(&tab_id));
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(pane_id)
            .map(|pane| pane.get_pane_id()),
        Some(pane_id)
    );
}

#[test]
fn a_frame_this_build_cannot_read_costs_one_request_not_the_stream() {
    let (server, _fake_pty_backend, (session_id, client_id, added_tab_id)) = serve_test_session(
        "unreadable-frame",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, session_structure, _) =
                attach_test_client(&mut viewer_connection, 2);
            assert_eq!(
                session_structure.tabs.len(),
                1,
                "the session starts with one tab"
            );
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            wait_for_client_count(&mut caller_connection, 1, 3);

            // A frame this build cannot read: `SubmitCommand` is a kind it has,
            // and the command inside names a variant no build has. The whole
            // frame fails to decode.
            let mut command_envelope_json =
                serde_json::to_value(build_new_tab_envelope(session_id))
                    .expect("the envelope encodes");
            command_envelope_json["command"] = serde_json::json!({ "CommandFromALaterKoshi": {} });
            viewer_connection
                .send(&serde_json::json!({
                    "request_id": 20,
                    "request_kind": { "SubmitCommand": command_envelope_json },
                }))
                .expect("send a frame this build cannot read");

            // The frame after it is served: this one adds a tab, and the tab
            // reaches this viewer's own stream. Every frame read here decodes as
            // a `SessionEvent`: nothing was written back for the frame above.
            viewer_connection
                .send(&IpcRequest {
                    request_id: 21,
                    request_kind: IpcRequestKind::SubmitCommand(Box::new(build_new_tab_envelope(
                        session_id,
                    ))),
                })
                .expect("send the next request");
            let (viewer_connection, session_frames) =
                read_session_frames_until(viewer_connection, |session_frame| {
                    matches!(
                        session_frame,
                        SessionFrame::Event(SessionEvent::TabCreated { .. })
                    )
                });
            let added_tab_id = session_frames
                .iter()
                .find_map(|session_frame| match session_frame {
                    SessionFrame::Event(SessionEvent::TabCreated { tab_id }) => Some(*tab_id),
                    SessionFrame::Event(_)
                    | SessionFrame::Painted(_)
                    | SessionFrame::ImageChunk(_) => None,
                })
                .expect("the new tab reaches the stream");

            assert_eq!(
                count_attached_clients(&mut caller_connection, 30),
                1,
                "the client that sent the unreadable frame is still attached"
            );
            (
                vec![viewer_connection.ipc_connection, caller_connection],
                (session_id, client_id, added_tab_id),
            )
        },
    );

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(
        session
            .clients
            .list_attached_clients()
            .map(Client::get_client_id)
            .collect::<Vec<_>>(),
        vec![client_id]
    );
    assert!(
        session.tabs.contains_key(&added_tab_id),
        "the request after the unreadable one applied"
    );
    assert_eq!(session.tabs.len(), 2, "the unreadable frame added no tab");
}

#[test]
fn detaching_one_client_leaves_every_other_stream_running() {
    let (
        server,
        _fake_pty_backend,
        (session_id, detached_client_id, remaining_client_id, added_tab_id),
    ) = serve_test_session(
        "detach-one",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut detaching_connection = open_session_connection(&runtime_directory, session_id);
            let (detached_client_id, _, _, _) = attach_test_client(&mut detaching_connection, 2);
            let mut remaining_connection = open_session_connection(&runtime_directory, session_id);
            let (remaining_client_id, _, _, _) = attach_test_client(&mut remaining_connection, 2);

            // The caller never attaches and has no event stream of its own.
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            assert_eq!(count_attached_clients(&mut caller_connection, 3), 2);

            let emitted_events = submit_test_command(
                &mut caller_connection,
                session_id,
                Command::Detach(DetachArgs {
                    client_id: Some(detached_client_id),
                }),
                4,
            );
            // Both clients report the same viewport. The tab the leaver held
            // keeps its size, and the detach emits nothing.
            assert_eq!(emitted_events, Vec::new());

            // The detached frame is the last one the departing viewer's stream carries.
            let (detached_connection, detached_session_frames) =
                read_session_frames_to_detached(detaching_connection);
            assert_eq!(
                detached_session_frames.last(),
                Some(&SessionFrame::Event(SessionEvent::Detached))
            );
            wait_for_client_count(&mut caller_connection, 1, 5);

            // The client that stayed is untouched: a tab added now still
            // reaches its stream.
            let added_tab_id = create_test_tab(&mut caller_connection, session_id, 100);
            let (tab_event_sender, tab_event_receiver) = mpsc::channel();
            std::thread::spawn(move || loop {
                let received_frame = remaining_connection
                    .recv_message_or_binary::<SessionEvent>()
                    .expect("an event frame");
                if let ReceivedFrame::Message(SessionEvent::TabCreated { tab_id }) = received_frame
                {
                    let _ = tab_event_sender.send((remaining_connection, tab_id));
                    return;
                }
            });
            let (remaining_connection_after_event, created_tab_id) = tab_event_receiver
                .recv_timeout(TEST_WAIT_TIMEOUT_DURATION)
                .expect("the new tab reaches the client that stayed");
            assert_eq!(created_tab_id, added_tab_id);

            (
                vec![
                    caller_connection,
                    detached_connection.ipc_connection,
                    remaining_connection_after_event,
                ],
                (
                    session_id,
                    detached_client_id,
                    remaining_client_id,
                    added_tab_id,
                ),
            )
        },
    );

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.clients.count_clients(), 1);
    assert!(session
        .clients
        .get_client_by_id(detached_client_id)
        .is_none());
    assert_eq!(
        session
            .clients
            .get_client_by_id(remaining_client_id)
            .map(|client| client.get_client_id()),
        Some(remaining_client_id)
    );
    assert!(session.tabs.contains_key(&added_tab_id));
}

#[test]
fn detach_all_takes_every_client_and_leaves_the_session_whole() {
    let (
        server,
        _fake_pty_backend,
        (session_id, detached_first_client_id, detached_second_client_id, tab_id, pane_id),
    ) = serve_test_session(
        "detach-all",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut first_attached_connection =
                open_session_connection(&runtime_directory, session_id);
            let (detached_first_client_id, _, session_structure, _) =
                attach_test_client(&mut first_attached_connection, 2);
            let mut second_attached_connection =
                open_session_connection(&runtime_directory, session_id);
            let (detached_second_client_id, _, _, _) =
                attach_test_client(&mut second_attached_connection, 2);

            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            assert_eq!(count_attached_clients(&mut caller_connection, 3), 2);

            let emitted_events =
                submit_test_command(&mut caller_connection, session_id, Command::DetachAll, 4);
            assert_eq!(emitted_events, Vec::new());

            // Every attached client's stream ends with the same detached frame.
            let (detached_first_connection, first_detached_session_frames) =
                read_session_frames_to_detached(first_attached_connection);
            let (detached_second_connection, second_detached_session_frames) =
                read_session_frames_to_detached(second_attached_connection);
            assert_eq!(
                first_detached_session_frames.last(),
                Some(&SessionFrame::Event(SessionEvent::Detached))
            );
            assert_eq!(
                second_detached_session_frames.last(),
                Some(&SessionFrame::Event(SessionEvent::Detached))
            );
            wait_for_client_count(&mut caller_connection, 0, 5);

            // The session with nobody watching still holds its tab and pane.
            let overview_after_detach = get_session_overview(&mut caller_connection, 100);
            assert_eq!(overview_after_detach.session.session_id, session_id);
            assert_eq!(
                overview_after_detach
                    .tabs
                    .iter()
                    .map(|tab| tab.tab_id)
                    .collect::<Vec<TabId>>(),
                vec![session_structure.tabs[0].tab_id],
            );
            assert_eq!(
                overview_after_detach
                    .panes
                    .iter()
                    .map(|pane| pane.pane_id)
                    .collect::<Vec<PaneId>>(),
                vec![session_structure.tabs[0].layout.list_leaf_pane_ids()[0]],
            );

            (
                vec![
                    caller_connection,
                    detached_first_connection.ipc_connection,
                    detached_second_connection.ipc_connection,
                ],
                (
                    session_id,
                    detached_first_client_id,
                    detached_second_client_id,
                    session_structure.tabs[0].tab_id,
                    session_structure.tabs[0].layout.list_leaf_pane_ids()[0],
                ),
            )
        },
    );

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.clients.count_clients(), 0);
    assert!(session
        .clients
        .get_client_by_id(detached_first_client_id)
        .is_none());
    assert!(session
        .clients
        .get_client_by_id(detached_second_client_id)
        .is_none());
    assert!(session.tabs.contains_key(&tab_id));
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(pane_id)
            .map(|pane| pane.get_pane_id()),
        Some(pane_id)
    );
}

#[test]
fn detaching_the_smaller_client_grows_the_tabs_pty_back() {
    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "detach-reflow",
        |runtime_directory, session_id, _fake_pty_backend| {
            // The narrow client attaches first and holds the tab down: the
            // effective tab size is the smallest viewport of every client viewing
            // it. The pane's PTY shrinks.
            let mut narrow_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (narrow_client_id, _, session_structure, _) = attach_test_client_with_viewport_size(
                &mut narrow_client_connection,
                2,
                NARROW_VIEWPORT_SIZE,
            );
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

            let mut wide_client_connection =
                open_session_connection(&runtime_directory, session_id);
            attach_test_client_with_viewport_size(
                &mut wide_client_connection,
                2,
                TEST_VIEWPORT_SIZE,
            );

            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            assert_eq!(count_attached_clients(&mut caller_connection, 3), 2);

            // The narrow client leaves and only the full-width viewer is left. The
            // tab grows back and the pane's PTY reflows with it.
            submit_test_command(
                &mut caller_connection,
                session_id,
                Command::Detach(DetachArgs {
                    client_id: Some(narrow_client_id),
                }),
                4,
            );
            let (narrow_client_connection, session_frames) =
                read_session_frames_to_detached(narrow_client_connection);
            assert_eq!(
                session_frames.last(),
                Some(&SessionFrame::Event(SessionEvent::Detached))
            );
            wait_for_client_count(&mut caller_connection, 1, 5);

            (
                vec![
                    caller_connection,
                    narrow_client_connection.ipc_connection,
                    wide_client_connection,
                ],
                pane_id,
            )
        },
    );

    // Three resizes, in this order: the size the seeded session gave the pane,
    // down by the difference in viewport width when the narrow client joined,
    // and back to the first size when it left. The rows never changed.
    let pty_sizes = fake_pty_backend
        .list_pane_sizes(pane_id)
        .expect("the pane was spawned");
    let initial_pty_size = pty_sizes[0];
    assert_eq!(
        pty_sizes,
        vec![
            initial_pty_size,
            PtySize {
                column_count: initial_pty_size.column_count
                    - (TEST_VIEWPORT_SIZE.column_count - NARROW_VIEWPORT_SIZE.column_count),
                row_count: initial_pty_size.row_count,
            },
            initial_pty_size,
        ],
    );
}

#[test]
fn dropping_a_smaller_client_connection_grows_the_tabs_pty_back() {
    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "drop-reflow",
        |runtime_directory, session_id, _fake_pty_backend| {
            // The narrow client attaches first and holds the tab down: the
            // effective tab size is the smallest viewport of every client viewing
            // it. The pane's PTY shrinks.
            let mut narrow_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (_, _, session_structure, _) = attach_test_client_with_viewport_size(
                &mut narrow_client_connection,
                2,
                NARROW_VIEWPORT_SIZE,
            );
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

            let mut wide_client_connection =
                open_session_connection(&runtime_directory, session_id);
            attach_test_client_with_viewport_size(
                &mut wide_client_connection,
                3,
                TEST_VIEWPORT_SIZE,
            );

            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            assert_eq!(count_attached_clients(&mut caller_connection, 4), 2);

            // Drop the narrow client's connection without sending Command::Detach.
            // The record goes when the serving thread notices the connection ended,
            // and the tab's PTY reflows immediately to the full width.
            drop(narrow_client_connection);

            wait_for_client_count(&mut caller_connection, 1, 5);

            (vec![caller_connection, wide_client_connection], pane_id)
        },
    );

    // Three resizes, in this order: the size the seeded session gave the pane,
    // down by the difference in viewport width when the narrow client joined,
    // and back to the first size when the narrow client's connection dropped.
    let pty_sizes = fake_pty_backend
        .list_pane_sizes(pane_id)
        .expect("the pane was spawned");
    let initial_pty_size = pty_sizes[0];
    assert_eq!(
        pty_sizes,
        vec![
            initial_pty_size,
            PtySize {
                column_count: initial_pty_size.column_count
                    - (TEST_VIEWPORT_SIZE.column_count - NARROW_VIEWPORT_SIZE.column_count),
                row_count: initial_pty_size.row_count,
            },
            initial_pty_size,
        ],
        "connection drop triggers immediate PTY reconciliation"
    );
}

#[test]
fn an_attached_client_types_into_its_pane_and_resizes_the_tab_it_views() {
    // Smaller than [`TEST_VIEWPORT_SIZE`] on both axes. This one client's report
    // is the smallest of every client viewing the tab, and the tab follows it.
    const RESIZED_VIEWPORT_SIZE: Size = Size {
        column_count: 60,
        row_count: 20,
    };

    // `<C-a>` reaches the pane as the ASCII SOH byte.
    const TYPED_KEY_CHORD: KeyChord =
        KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('a'));
    const TYPED_KEY_BYTES: &[u8] = &[0x01];

    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "types-and-resizes",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
            let (client_id, replied_session_id, session_structure, _) =
                attach_test_client(&mut viewer_connection, 2);
            assert_eq!(replied_session_id, session_id);
            let tab_id = session_structure.tabs[0].tab_id;
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

            // The attach records no focused pane. The pane this client types into
            // is named over a second connection, which never attaches.
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            submit_test_command(
                &mut caller_connection,
                session_id,
                Command::FocusPane(FocusPaneArgs {
                    focus_target: FocusTarget::Pane(pane_id),
                    client_id: Some(client_id),
                }),
                3,
            );

            // The first frame the session composes for this client, drawn at the
            // size the attach reported.
            let (mut viewer_connection, session_frames) =
                read_session_frames_until(viewer_connection, |session_frame| {
                    matches!(session_frame, SessionFrame::Painted(_))
                });
            let first_painted_frame = &get_last_painted_frame(&session_frames).painted_frame;
            assert_eq!(first_painted_frame.client_snapshot.client_id, client_id);
            assert_eq!(first_painted_frame.session_snapshot.session_id, session_id);
            assert_eq!(
                first_painted_frame.client_snapshot.viewport_size,
                TEST_VIEWPORT_SIZE
            );
            assert_eq!(first_painted_frame.client_snapshot.active_tab_id, tab_id);
            assert_eq!(
                first_painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .tab_id,
                tab_id
            );
            assert_eq!(
                first_painted_frame.client_snapshot.focused_pane_id,
                Some(pane_id)
            );

            // Both requests travel up this one connection, and the dispatcher
            // reads them in this order: the press has reached the pane by the time
            // the resized frame is composed.
            viewer_connection
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: IpcRequestKind::Keyboard {
                        key_inputs: vec![build_key_input_for_chord(TYPED_KEY_CHORD)],
                    },
                })
                .expect("send key press");
            viewer_connection
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 5,
                    request_kind: IpcRequestKind::Resize {
                        viewport_size: RESIZED_VIEWPORT_SIZE,
                        pane_area: None,
                        cell_size: None,
                    },
                })
                .expect("send resize");

            let (viewer_connection, session_frames) = read_session_frames_until_painted_frame(
                viewer_connection,
                |received_painted_frame| {
                    received_painted_frame
                        .painted_frame
                        .client_snapshot
                        .viewport_size
                        == RESIZED_VIEWPORT_SIZE
                },
            );
            // Nothing between the two frames drew at a third size.
            for earlier_session_frame in &session_frames[..session_frames.len() - 1] {
                if let SessionFrame::Painted(earlier_received_painted_frame) = earlier_session_frame
                {
                    assert_eq!(
                        earlier_received_painted_frame
                            .painted_frame
                            .client_snapshot
                            .viewport_size,
                        TEST_VIEWPORT_SIZE
                    );
                }
            }
            let resized_painted_frame = &get_last_painted_frame(&session_frames).painted_frame;
            assert_eq!(resized_painted_frame.client_snapshot.client_id, client_id);
            assert_eq!(
                resized_painted_frame.client_snapshot.viewport_size,
                RESIZED_VIEWPORT_SIZE
            );
            assert_eq!(
                resized_painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .tab_size,
                compute_default_pane_area_size(RESIZED_VIEWPORT_SIZE),
            );

            // The stream still ends with the detached frame once the client is detached.
            submit_test_command(
                &mut caller_connection,
                session_id,
                Command::Detach(DetachArgs {
                    client_id: Some(client_id),
                }),
                6,
            );
            let (viewer_connection, session_frames) =
                read_session_frames_to_detached(viewer_connection);
            assert_eq!(
                session_frames.last(),
                Some(&SessionFrame::Event(SessionEvent::Detached))
            );
            assert_eq!(
                session_frames
                    .iter()
                    .filter(|session_frame| **session_frame
                        == SessionFrame::Event(SessionEvent::Detached))
                    .count(),
                1,
            );

            (
                vec![caller_connection, viewer_connection.ipc_connection],
                pane_id,
            )
        },
    );

    // The press is the only thing written to the pane, and it arrived encoded.
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("the pane was spawned"),
        vec![TYPED_KEY_BYTES.to_vec()],
    );
}

#[test]
fn an_accepted_pane_swap_delivers_its_frame_without_a_resize() {
    let (_server, _fake_pty_backend, ()) = serve_test_session(
        "pane-swap-frame",
        |runtime_directory, session_id, fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, attached_session_structure, _) =
                attach_test_client(&mut viewer_connection, 2);
            let target_pane_id = attached_session_structure.tabs[0]
                .layout
                .list_leaf_pane_ids()[0];
            let mut observer_connection = open_session_connection(&runtime_directory, session_id);
            let (observer_client_id, _, _, _) = attach_test_client(&mut observer_connection, 2);
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let source_pane_id = create_right_split_pane(
                &mut caller_connection,
                session_id,
                target_pane_id,
                client_id,
                3,
            );

            let (viewer_connection, initial_session_frames) =
                read_session_frames_until_two_panes_show(viewer_connection, client_id);
            let (observer_connection, observer_initial_session_frames) =
                read_session_frames_until_two_panes_show(observer_connection, observer_client_id);
            let initial_frame = &get_last_painted_frame(&initial_session_frames).painted_frame;
            let active_tab_id = initial_frame.client_snapshot.active_tab_id;
            let source_rect_before = find_pane_outer_rect(initial_frame, source_pane_id);
            let target_rect_before = find_pane_outer_rect(initial_frame, target_pane_id);
            let target_pane_resize_history = fake_pty_backend
                .list_pane_sizes(target_pane_id)
                .expect("the target PTY was spawned");
            let source_pane_resize_history = fake_pty_backend
                .list_pane_sizes(source_pane_id)
                .expect("the source PTY was spawned");
            let (mut viewer_connection, placement_snapshot) =
                read_placement_preview(viewer_connection, source_pane_id, active_tab_id);
            let placement_target = TiledPlacementTarget::Swap { target_pane_id };
            let expected_placed_tab_layout =
                build_same_tab_placed_tab_layout(&placement_snapshot, &placement_target);
            let placement_command_id = CommandId::new();

            viewer_connection
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: IpcRequestKind::ConfirmPanePlacement {
                        command_id: placement_command_id,
                        source_pane_id,
                        placement_target: placement_target.clone(),
                        expected_placed_tab_layout: expected_placed_tab_layout.clone(),
                    },
                })
                .expect("send the confirmed pane swap");

            let (viewer_connection, committed_session_frames) =
                read_session_frames_until_painted_frame(
                    viewer_connection,
                    move |received_painted_frame| {
                        received_painted_frame
                            .painted_frame
                            .client_snapshot
                            .last_applied_placement_command_id
                            == Some(placement_command_id)
                    },
                );
            let (observer_connection, observer_committed_session_frames) =
                read_session_frames_until_painted_frame(
                    observer_connection,
                    move |received_painted_frame| {
                        let painted_frame = &received_painted_frame.painted_frame;
                        painted_frame.client_snapshot.client_id == observer_client_id
                            && find_pane_outer_rect(painted_frame, source_pane_id)
                                == target_rect_before
                    },
                );
            let committed_frame = &get_last_painted_frame(&committed_session_frames).painted_frame;
            // Each read above ends at its committed frame and returns every
            // frame that arrived before it.
            let expected_commit_frame = SessionFrame::Event(SessionEvent::PanePlacementCommitted {
                command_id: placement_command_id,
                source_pane_id,
                source_tab_id: Some(active_tab_id),
                destination_tab_id: Some(active_tab_id),
                placement_target: PanePlacementTarget::Tiled(placement_target),
            });
            for committed_client_frames in [
                &committed_session_frames,
                &observer_committed_session_frames,
            ] {
                assert!(
                    committed_client_frames.contains(&expected_commit_frame),
                    "each viewer receives the exact placement commit before the committed frame"
                );
            }
            assert_eq!(
                get_last_painted_frame(&observer_initial_session_frames)
                    .painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .pane_slots,
                initial_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .pane_slots,
                "the observer starts from the same committed layout"
            );
            assert_eq!(
                committed_frame.client_snapshot.viewport_size,
                TEST_VIEWPORT_SIZE
            );
            assert_eq!(
                list_pane_outer_rects(committed_frame),
                expected_placed_tab_layout.layout_solve.pane_rects,
                "the committed tab is the tab the preview showed"
            );
            assert_eq!(
                find_pane_outer_rect(committed_frame, source_pane_id),
                target_rect_before
            );
            assert_eq!(
                find_pane_outer_rect(committed_frame, target_pane_id),
                source_rect_before
            );

            assert_eq!(
                fake_pty_backend
                    .list_pane_sizes(target_pane_id)
                    .expect("the target PTY remains available"),
                target_pane_resize_history,
                "the swap does not resize the target PTY"
            );
            assert_eq!(
                fake_pty_backend
                    .list_pane_sizes(source_pane_id)
                    .expect("the source PTY remains available"),
                source_pane_resize_history,
                "the swap does not resize the source PTY"
            );

            (
                vec![
                    caller_connection,
                    viewer_connection.ipc_connection,
                    observer_connection.ipc_connection,
                ],
                (),
            )
        },
    );
}

#[test]
fn a_pane_swap_confirmed_before_another_viewers_new_pane_is_rejected_to_its_viewer() {
    let (_server, _fake_pty_backend, ()) = serve_test_session(
        "pane-swap-stale",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, attached_session_structure, _) =
                attach_test_client(&mut viewer_connection, 2);
            let target_pane_id = attached_session_structure.tabs[0]
                .layout
                .list_leaf_pane_ids()[0];
            let mut other_viewer_connection =
                open_session_connection(&runtime_directory, session_id);
            let (other_client_id, _, _, _) = attach_test_client(&mut other_viewer_connection, 2);
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let source_pane_id = create_right_split_pane(
                &mut caller_connection,
                session_id,
                target_pane_id,
                client_id,
                3,
            );
            let (viewer_connection, initial_session_frames) =
                read_session_frames_until_two_panes_show(viewer_connection, client_id);
            let active_tab_id = get_last_painted_frame(&initial_session_frames)
                .painted_frame
                .client_snapshot
                .active_tab_id;
            let (viewer_connection, placement_snapshot) =
                read_placement_preview(viewer_connection, source_pane_id, active_tab_id);
            let placement_target = TiledPlacementTarget::Swap { target_pane_id };
            let earlier_placed_tab_layout =
                build_same_tab_placed_tab_layout(&placement_snapshot, &placement_target);

            create_right_split_pane(
                &mut caller_connection,
                session_id,
                target_pane_id,
                other_client_id,
                4,
            );
            let (mut viewer_connection, _) = read_session_frames_until_painted_frame(
                viewer_connection,
                move |received_painted_frame| {
                    received_painted_frame
                        .painted_frame
                        .session_snapshot
                        .active_tab_snapshot
                        .pane_slots
                        .len()
                        == 3
                },
            );
            let placement_command_id = CommandId::new();
            viewer_connection
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 5,
                    request_kind: IpcRequestKind::ConfirmPanePlacement {
                        command_id: placement_command_id,
                        source_pane_id,
                        placement_target,
                        expected_placed_tab_layout: earlier_placed_tab_layout,
                    },
                })
                .expect("send the pane swap built on the earlier layout");

            let expected_rejection = SessionEvent::PlacementCommandRejected {
                command_id: placement_command_id,
            };
            let (viewer_connection, answer_events) =
                read_session_frames_until(viewer_connection, move |session_frame| {
                    *session_frame == SessionFrame::Event(expected_rejection.clone())
                });
            assert!(
                !answer_events.iter().any(|session_frame| matches!(
                    session_frame,
                    SessionFrame::Event(SessionEvent::PanePlacementCommitted { command_id, .. })
                        if *command_id == placement_command_id
                )),
                "the session commits nothing for the stale swap"
            );

            (
                vec![
                    caller_connection,
                    viewer_connection.ipc_connection,
                    other_viewer_connection,
                ],
                (),
            )
        },
    );
}

#[test]
fn a_refused_floating_pane_move_from_a_key_binding_is_rejected_to_its_viewer() {
    let (_server, _fake_pty_backend, ()) = serve_test_session(
        "floating-move-refused",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, attached_session_structure, _) =
                attach_test_client(&mut viewer_connection, 2);
            let tiled_pane_id = attached_session_structure.tabs[0]
                .layout
                .list_leaf_pane_ids()[0];
            let move_command_id = CommandId::new();
            viewer_connection
                .send(&IpcRequest {
                    request_id: 3,
                    request_kind: IpcRequestKind::SubmitCommand(Box::new(
                        CommandEnvelope::from_parts(
                            move_command_id,
                            CommandSource::from_key_binding(client_id),
                            Command::MoveFloatingPane(MoveFloatingPaneArgs {
                                pane_id: tiled_pane_id,
                                top_left_cell: Point { column: 2, row: 3 },
                            }),
                        ),
                    )),
                })
                .expect("send the floating pane move of a tiled pane");

            let expected_rejection = SessionEvent::PlacementCommandRejected {
                command_id: move_command_id,
            };
            let (viewer_connection, _) =
                read_session_frames_until(viewer_connection, move |session_frame| {
                    *session_frame == SessionFrame::Event(expected_rejection.clone())
                });

            (vec![viewer_connection.ipc_connection], ())
        },
    );
}

#[test]
fn an_accepted_pane_insertion_delivers_its_frame_without_a_resize() {
    let (_server, _fake_pty_backend, ()) = serve_test_session(
        "place-frame",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, attached_session_structure, _) =
                attach_test_client(&mut viewer_connection, 2);
            let target_pane_id = attached_session_structure.tabs[0]
                .layout
                .list_leaf_pane_ids()[0];
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let source_pane_id = create_right_split_pane(
                &mut caller_connection,
                session_id,
                target_pane_id,
                client_id,
                3,
            );

            let (viewer_connection, initial_session_frames) =
                read_session_frames_until_two_panes_show(viewer_connection, client_id);
            let initial_frame = &get_last_painted_frame(&initial_session_frames).painted_frame;
            let active_tab_id = initial_frame.session_snapshot.active_tab_snapshot.tab_id;
            let target_rect_before = find_pane_outer_rect(initial_frame, target_pane_id);
            let (mut viewer_connection, placement_snapshot) =
                read_placement_preview(viewer_connection, source_pane_id, active_tab_id);
            let placement_target = TiledPlacementTarget::Split {
                destination_tab_id: active_tab_id,
                anchor: PanePlacementAnchor::Pane(target_pane_id),
                direction: Direction::Down,
            };
            let expected_placed_tab_layout =
                build_same_tab_placed_tab_layout(&placement_snapshot, &placement_target);
            let placement_command_id = CommandId::new();

            viewer_connection
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: IpcRequestKind::ConfirmPanePlacement {
                        command_id: placement_command_id,
                        source_pane_id,
                        placement_target,
                        expected_placed_tab_layout: expected_placed_tab_layout.clone(),
                    },
                })
                .expect("send the confirmed pane insertion");

            let (viewer_connection, committed_session_frames) =
                read_session_frames_until_painted_frame(
                    viewer_connection,
                    move |received_painted_frame| {
                        received_painted_frame
                            .painted_frame
                            .client_snapshot
                            .last_applied_placement_command_id
                            == Some(placement_command_id)
                    },
                );
            let committed_frame = &get_last_painted_frame(&committed_session_frames).painted_frame;
            let source_rect_after = find_pane_outer_rect(committed_frame, source_pane_id);
            let target_rect_after = find_pane_outer_rect(committed_frame, target_pane_id);
            assert_eq!(
                committed_frame.client_snapshot.viewport_size,
                TEST_VIEWPORT_SIZE
            );
            assert_eq!(
                list_pane_outer_rects(committed_frame),
                expected_placed_tab_layout.layout_solve.pane_rects,
                "the committed tab is the tab the preview showed"
            );
            let target_row_count_after = target_rect_before.size.row_count / 2;
            let source_row_count_after = target_rect_before.size.row_count - target_row_count_after;
            assert_eq!(target_rect_after.origin, target_rect_before.origin);
            assert_eq!(
                target_rect_after.size,
                Size {
                    column_count: TEST_VIEWPORT_SIZE.column_count,
                    row_count: target_row_count_after,
                }
            );
            assert_eq!(
                source_rect_after.origin,
                Point {
                    column: target_rect_before.origin.column,
                    row: target_rect_before.origin.row + target_row_count_after,
                }
            );
            assert_eq!(
                source_rect_after.size,
                Size {
                    column_count: TEST_VIEWPORT_SIZE.column_count,
                    row_count: source_row_count_after,
                }
            );

            (
                vec![caller_connection, viewer_connection.ipc_connection],
                (),
            )
        },
    );
}

/// The outer rect of `pane_id`'s slot in the active tab of `painted_frame`.
///
/// # Panics
/// When the active tab has no slot for `pane_id`.
fn find_pane_outer_rect(painted_frame: &PaintedFrame, pane_id: PaneId) -> Rect {
    painted_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter()
        .find(|pane_slot| pane_slot.pane_id == pane_id)
        .expect("the active tab has a slot for the pane")
        .outer_rect
}

/// Each slot of the active tab of `painted_frame` as `(pane_id, outer_rect)`,
/// in slot order.
fn list_pane_outer_rects(painted_frame: &PaintedFrame) -> Vec<(PaneId, Rect)> {
    painted_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter()
        .map(|pane_slot| (pane_slot.pane_id, pane_slot.outer_rect))
        .collect()
}

/// Name `source_pane_id` and `destination_tab_id` as the pane placement
/// preview of `viewer_connection`'s client, and read its stream until a
/// painted update carries the preview.
///
/// Returns the connection and the preview's layouts.
///
/// # Panics
/// When the preview the session sends is unavailable.
fn read_placement_preview(
    mut viewer_connection: ViewerConnection,
    source_pane_id: PaneId,
    destination_tab_id: TabId,
) -> (ViewerConnection, PanePlacementSnapshot) {
    viewer_connection
        .ipc_connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::SetPanePlacementPreview {
                placement_target: Some(PanePlacementPreviewTarget {
                    source_pane_id,
                    destination_tab_id,
                }),
            },
        })
        .expect("send the previewed placement target");
    let (viewer_connection, session_frames) =
        read_session_frames_until_painted_frame(viewer_connection, |received_painted_frame| {
            received_painted_frame.placement_preview.is_some()
        });
    let placement_preview = get_last_painted_frame(&session_frames)
        .placement_preview
        .clone()
        .expect("the update carries the preview");
    let PanePlacementPreview::Available(placement_snapshot) = placement_preview.as_ref() else {
        panic!("the preview is available, got {placement_preview:?}");
    };
    (
        viewer_connection,
        PanePlacementSnapshot::clone(placement_snapshot),
    )
}

/// The source pane's own tab of `placement_snapshot` once
/// `placement_target` places the pane in it, solved for the previewing client
/// at the tab size the session checks the placement at
/// ([`solve_placed_tab`]).
///
/// # Panics
/// When the source pane floats, the session gave the tab no size, or the
/// placement does not fit the tab.
fn build_same_tab_placed_tab_layout(
    placement_snapshot: &PanePlacementSnapshot,
    placement_target: &TiledPlacementTarget,
) -> PlacedTabLayout {
    let PanePlacementSource::Tiled(source_tab_snapshot) = &placement_snapshot.source else {
        panic!("the source pane is tiled");
    };
    let pane_sizing = PaneSizing::from(placement_snapshot.pane_sizing);
    let tab_size = placement_snapshot
        .insertion_tab_sizes
        .destination_tab_size
        .expect("the session sizes the tab");
    let placed_layout_tree = place_pane_within_tab(
        &source_tab_snapshot.layout_tree,
        placement_snapshot.source_pane_id,
        &PlacementTarget::from_tiled_placement_target(placement_target),
        Rect::from_size_at_origin(tab_size),
        pane_sizing,
    )
    .expect("the placement fits the tab");
    PlacedTabLayout {
        tab_id: placement_snapshot.destination_tab_id,
        tab_size,
        layout_solve: solve_placed_tab(
            &placed_layout_tree,
            placement_snapshot.source_pane_id,
            &source_tab_snapshot.focus_history,
            tab_size,
            pane_sizing,
        )
        .layout_solve,
    }
}

/// [`read_session_frames_until_painted_frame`] stopping at the first frame
/// drawn for `client_id` whose active tab shows two panes.
fn read_session_frames_until_two_panes_show(
    viewer_connection: impl Into<ViewerConnection>,
    client_id: ClientId,
) -> (ViewerConnection, Vec<SessionFrame<SessionEvent>>) {
    read_session_frames_until_painted_frame(viewer_connection, move |received_painted_frame| {
        let painted_frame = &received_painted_frame.painted_frame;
        painted_frame.client_snapshot.client_id == client_id
            && painted_frame
                .session_snapshot
                .active_tab_snapshot
                .pane_slots
                .len()
                == 2
    })
}

/// Every mouse-round answer in `session_frames`, in the order they arrived: the
/// round each one answers, and what that answer carried.
fn list_mouse_answers(
    session_frames: &[SessionFrame<SessionEvent>],
) -> Vec<(u64, Vec<MouseAnswer>)> {
    session_frames
        .iter()
        .filter_map(|session_frame| match session_frame {
            SessionFrame::Event(SessionEvent::MouseAnswer {
                request_id,
                mouse_answers,
            }) => Some((*request_id, mouse_answers.clone())),
            SessionFrame::Event(_) | SessionFrame::Painted(_) | SessionFrame::ImageChunk(_) => None,
        })
        .collect()
}

/// Print enough lines in `pane_id` to push at least `retained_line_count` of
/// them into its scrollback, and read `viewer_connection`'s stream until a
/// painted frame holds them.
///
/// Returns the line that frame shows on the pane's top row. A scroll up from
/// here counts back from that line.
fn fill_pane_scrollback(
    fake_pty_backend: &FakePtyBackend,
    pane_id: PaneId,
    retained_line_count: usize,
    viewer_connection: impl Into<ViewerConnection>,
) -> (ViewerConnection, u64) {
    let printed_line_count = retained_line_count + usize::from(TEST_VIEWPORT_SIZE.row_count);
    fake_pty_backend
        .push_output(pane_id, b"x\r\n".repeat(printed_line_count))
        .expect("the pane was spawned");
    let (viewer_connection, session_frames) =
        read_session_frames_until_painted_frame(viewer_connection, move |received_painted_frame| {
            received_painted_frame.pane_views.iter().any(|pane_view| {
                pane_view.pane_id == pane_id && pane_view.retained_line_count >= retained_line_count
            })
        });
    let top_row_index = get_last_painted_frame(&session_frames)
        .pane_views
        .iter()
        .find(|pane_view| pane_view.pane_id == pane_id)
        .expect("the pane this client views")
        .view_top_row_index;
    (viewer_connection, top_row_index)
}

/// The forward of one left press with nothing held, at the pane's top-left
/// content cell `(0, 0)`.
fn build_left_mouse_press_forward(pane_id: PaneId) -> WireMouseAction {
    WireMouseAction::Forward {
        pane_id,
        mouse_kind: MouseKind::Press(MouseButton::Left),
        modifier_flags: BindingModifierFlags::NONE,
        pane_local_cell: Point { column: 0, row: 0 },
    }
}

/// The report the program in the pane reads for
/// [`build_left_mouse_press_forward`]: a left press at the pane's own column
/// 1, row 1, in the SGR form the pane asked for.
const MOUSE_REPORT_BYTES: &[u8] = b"\x1b[<0;1;1M";

#[test]
fn an_attached_client_forwards_a_mouse_press_into_its_pane() {
    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "mouse-forward",
        |runtime_directory, session_id, fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
            let (_, replied_session_id, session_structure, _) =
                attach_test_client(&mut viewer_connection, 2);
            assert_eq!(replied_session_id, session_id);
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

            // The program in the pane asks for mouse reports. Until it has, a
            // forwarded event is written nowhere.
            let mut viewer_connection =
                wait_for_mouse_tracking(&fake_pty_backend, pane_id, viewer_connection);
            viewer_connection
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: IpcRequestKind::Mouse(vec![build_left_mouse_press_forward(
                        pane_id,
                    )]),
                })
                .expect("send mouse round");

            // The answer arrives after the round ran and its write happened.
            let (viewer_connection, _) = read_session_frames_to_mouse_answer(viewer_connection, 4);

            (vec![viewer_connection.ipc_connection], pane_id)
        },
    );

    // The report is the only thing written to the pane, and it arrived encoded.
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("the pane was spawned"),
        vec![MOUSE_REPORT_BYTES.to_vec()],
    );
}

#[test]
fn the_answer_to_a_round_that_reports_nothing_still_reaches_the_viewer() {
    // The viewer sends no further mouse round until the round in flight is
    // answered.
    let (_server, _fake_pty_backend, ()) = serve_test_session(
        "mouse-answer",
        |runtime_directory, session_id, fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
            let (_, _, session_structure, _) = attach_test_client(&mut viewer_connection, 2);
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

            let mut viewer_connection =
                wait_for_mouse_tracking(&fake_pty_backend, pane_id, viewer_connection);
            viewer_connection
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: IpcRequestKind::Mouse(vec![build_left_mouse_press_forward(
                        pane_id,
                    )]),
                })
                .expect("send mouse round");

            // A forward has nothing to report, and the round is answered anyway.
            let (viewer_connection, session_frames) =
                read_session_frames_to_mouse_answer(viewer_connection, 4);
            assert_eq!(list_mouse_answers(&session_frames), vec![(4, Vec::new())]);

            (vec![viewer_connection.ipc_connection], ())
        },
    );
}

#[test]
fn a_scroll_round_answers_with_the_pane_and_the_line_its_view_landed_on() {
    // Lines of history to print, and how far up the round scrolls.
    const RETAINED_SCROLLBACK_LINE_COUNT: usize = 40;
    const SCROLL_LINE_COUNT: usize = 5;

    let (_server, _fake_pty_backend, ()) = serve_test_session(
        "mouse-scroll",
        |runtime_directory, session_id, fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
            let (_, _, session_structure, _) = attach_test_client(&mut viewer_connection, 2);
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

            let (mut viewer_connection, top_row_index) = fill_pane_scrollback(
                &fake_pty_backend,
                pane_id,
                RETAINED_SCROLLBACK_LINE_COUNT,
                viewer_connection,
            );
            viewer_connection
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                        pane_id,
                        is_scrolling_up: true,
                        scroll_line_count: SCROLL_LINE_COUNT,
                    }]),
                })
                .expect("send mouse round");

            // The view landed five lines above the line it was showing.
            let (viewer_connection, session_frames) =
                read_session_frames_to_mouse_answer(viewer_connection, 4);
            assert_eq!(
                list_mouse_answers(&session_frames),
                vec![(
                    4,
                    vec![MouseAnswer::Scrolled {
                        pane_id,
                        top_row_number: Some(top_row_index - SCROLL_LINE_COUNT as u64),
                    }],
                )],
            );

            (vec![viewer_connection.ipc_connection], ())
        },
    );
}

#[test]
fn a_border_move_round_answers_with_the_cells_it_applied() {
    // The request asks for far more cells than the neighbor pane can give. The
    // neighbor holds 40 of the tab's 80 columns. A pane's box keeps a 2-column
    // content minimum inside a 1-cell border, which is 4 columns, and the border
    // move takes the 36 columns above that minimum.
    const REQUESTED_CELL_COUNT: u16 = 200;
    const APPLIED_CELL_COUNT: u16 = 36;

    let (_server, _fake_pty_backend, ()) = serve_test_session(
        "mouse-border",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, session_structure, _) =
                attach_test_client(&mut viewer_connection, 2);
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

            // The neighbor whose room the border move takes, split off the
            // client's own pane over a second connection, which never attaches.
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            create_right_split_pane(&mut caller_connection, session_id, pane_id, client_id, 3);

            viewer_connection
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Resize {
                        pane_id,
                        border_side: Direction::Right,
                        resize_step: 1,
                        requested_cell_count: REQUESTED_CELL_COUNT,
                    }]),
                })
                .expect("send mouse round");

            let (viewer_connection, session_frames) =
                read_session_frames_to_mouse_answer(viewer_connection, 4);
            assert_eq!(
                list_mouse_answers(&session_frames),
                vec![(
                    4,
                    vec![MouseAnswer::Resized {
                        pane_id,
                        border_side: Direction::Right,
                        resize_step: 1,
                        applied_cell_count: APPLIED_CELL_COUNT,
                    }]
                )],
            );

            (
                vec![caller_connection, viewer_connection.ipc_connection],
                (),
            )
        },
    );
}

#[test]
fn one_round_runs_every_action_it_holds_and_is_answered_once() {
    // Lines of history to print, and how far up the round scrolls.
    const RETAINED_SCROLLBACK_LINE_COUNT: usize = 40;
    const SCROLL_LINE_COUNT: usize = 5;

    let (server, fake_pty_backend, (session_id, client_id, tab_id, pane_id)) = serve_test_session(
        "mouse-round",
        |runtime_directory, session_id, fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, session_structure, _) =
                attach_test_client(&mut viewer_connection, 2);
            let tab_id = session_structure.tabs[0].tab_id;
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

            // Splitting the pane this client attached on moves its focus to the
            // new pane. The pane the round asks for is then not the focused
            // one. The split is made over a second connection, which never
            // attaches.
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            create_right_split_pane(&mut caller_connection, session_id, pane_id, client_id, 3);

            let viewer_connection =
                wait_for_mouse_tracking(&fake_pty_backend, pane_id, viewer_connection);
            let (mut viewer_connection, top_row_index) = fill_pane_scrollback(
                &fake_pty_backend,
                pane_id,
                RETAINED_SCROLLBACK_LINE_COUNT,
                viewer_connection,
            );

            viewer_connection
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: IpcRequestKind::Mouse(vec![
                        WireMouseAction::Command(Box::new(Command::FocusPane(FocusPaneArgs {
                            focus_target: FocusTarget::Pane(pane_id),
                            client_id: Some(client_id),
                        }))),
                        WireMouseAction::Scroll {
                            pane_id,
                            is_scrolling_up: true,
                            scroll_line_count: SCROLL_LINE_COUNT,
                        },
                        build_left_mouse_press_forward(pane_id),
                    ]),
                })
                .expect("send mouse round");
            let (mut viewer_connection, mut session_frames) =
                read_session_frames_to_mouse_answer(viewer_connection, 4);

            // A second round, sent once the first was answered. Its own answer
            // is the frame after which no further answer to the first round can
            // still be in flight.
            viewer_connection
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 5,
                    request_kind: IpcRequestKind::Mouse(Vec::new()),
                })
                .expect("send empty mouse round");
            let (viewer_connection, second_round_session_frames) =
                read_session_frames_to_mouse_answer(viewer_connection, 5);
            session_frames.extend(second_round_session_frames);

            // One answer for the round, holding the scroll alone: the focus
            // command and the forward each report nothing.
            assert_eq!(
                list_mouse_answers(&session_frames),
                vec![
                    (
                        4,
                        vec![MouseAnswer::Scrolled {
                            pane_id,
                            top_row_number: Some(top_row_index - SCROLL_LINE_COUNT as u64),
                        }],
                    ),
                    (5, Vec::new()),
                ],
            );

            (
                vec![caller_connection, viewer_connection.ipc_connection],
                (session_id, client_id, tab_id, pane_id),
            )
        },
    );

    // The session applied the command the round carried: the focus the split
    // had moved away is back on the round's pane.
    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    let client = session
        .clients
        .get_client_by_id(client_id)
        .expect("the viewing client");
    assert_eq!(client.get_focused_pane_id(tab_id), Some(pane_id));

    // And the forward the round carried reached the pane, encoded, once.
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("the pane was spawned"),
        vec![MOUSE_REPORT_BYTES.to_vec()],
    );
}

#[test]
fn two_rounds_sent_back_to_back_are_answered_in_the_order_they_were_sent() {
    // Lines of history to print, then how far up each round scrolls.
    const RETAINED_SCROLLBACK_LINE_COUNT: usize = 40;
    const FIRST_SCROLL_LINE_COUNT: usize = 2;
    const SECOND_SCROLL_LINE_COUNT: usize = 3;

    let (_server, _fake_pty_backend, ()) = serve_test_session(
        "mouse-order",
        |runtime_directory, session_id, fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
            let (_, _, session_structure, _) = attach_test_client(&mut viewer_connection, 2);
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

            let (mut viewer_connection, top_row_index) = fill_pane_scrollback(
                &fake_pty_backend,
                pane_id,
                RETAINED_SCROLLBACK_LINE_COUNT,
                viewer_connection,
            );
            for (request_id, scroll_line_count) in
                [(4, FIRST_SCROLL_LINE_COUNT), (5, SECOND_SCROLL_LINE_COUNT)]
            {
                viewer_connection
                    .ipc_connection
                    .send(&IpcRequest {
                        request_id,
                        request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                            pane_id,
                            is_scrolling_up: true,
                            scroll_line_count,
                        }]),
                    })
                    .expect("send mouse round");
            }

            // Both rounds moved the same view, so the lines they answer with name
            // the order they ran in: two lines up, then three more.
            let (viewer_connection, session_frames) =
                read_session_frames_to_mouse_answer(viewer_connection, 5);
            assert_eq!(
                list_mouse_answers(&session_frames),
                vec![
                    (
                        4,
                        vec![MouseAnswer::Scrolled {
                            pane_id,
                            top_row_number: Some(top_row_index - FIRST_SCROLL_LINE_COUNT as u64),
                        }],
                    ),
                    (
                        5,
                        vec![MouseAnswer::Scrolled {
                            pane_id,
                            top_row_number: Some(
                                top_row_index
                                    - (FIRST_SCROLL_LINE_COUNT + SECOND_SCROLL_LINE_COUNT) as u64
                            ),
                        }],
                    ),
                ],
            );

            (vec![viewer_connection.ipc_connection], ())
        },
    );
}

#[test]
fn attaching_again_with_the_token_brings_back_the_tab_focus_zoom_and_scroll() {
    // Lines of history to print, and how far up the round scrolls.
    const RETAINED_SCROLLBACK_LINE_COUNT: usize = 40;
    const SCROLL_LINE_COUNT: usize = 5;

    let (
        server,
        _fake_pty_backend,
        (
            session_id,
            detached_client_id,
            resumed_client_id,
            booted_tab_id,
            added_tab_id,
            root_pane_id,
        ),
    ) = serve_test_session(
        "resume-view",
        |runtime_directory, session_id, fake_pty_backend| {
            let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
            let (detached_client_id, _, session_structure, resume_token) =
                attach_test_client(&mut viewer_connection, 2);
            let booted_tab_id = session_structure.tabs[0].tab_id;
            let root_pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];
            let resume_token = resume_token.expect("the attach minted a token");

            // Every command rides a connection that never attaches. The viewer's
            // own stream carries frames alone.
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);

            // Scroll the pane five lines up, zoom it, then switch tabs.
            let (mut viewer_connection, _) = fill_pane_scrollback(
                &fake_pty_backend,
                root_pane_id,
                RETAINED_SCROLLBACK_LINE_COUNT,
                viewer_connection,
            );
            viewer_connection
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                        pane_id: root_pane_id,
                        is_scrolling_up: true,
                        scroll_line_count: SCROLL_LINE_COUNT,
                    }]),
                })
                .expect("send mouse round");
            let (viewer_connection, _) = read_session_frames_to_mouse_answer(viewer_connection, 4);
            submit_test_command_for_client(
                &mut caller_connection,
                session_id,
                Some(detached_client_id),
                Command::TogglePaneFullscreen,
                5,
            );
            // Adding a tab moves the client that asked for it onto that tab.
            let added_tab_id = create_test_tab(&mut caller_connection, session_id, 6);
            submit_test_command_for_client(
                &mut caller_connection,
                session_id,
                Some(detached_client_id),
                Command::FocusTab(FocusTabArgs {
                    focus_target: TabTarget::Id(added_tab_id),
                    client_id: Some(detached_client_id),
                }),
                7,
            );

            // The link breaks. The session files the view under the token this
            // attach minted.
            drop(viewer_connection);
            wait_for_client_count(&mut caller_connection, 0, 8);

            let mut resumed_connection = open_session_connection(&runtime_directory, session_id);
            let (resumed_client_id, resumed_session_id, session_structure, _) =
                attach_test_client_with_resume_token(
                    &mut resumed_connection,
                    2,
                    TEST_VIEWPORT_SIZE,
                    Some(resume_token),
                );
            assert_eq!(resumed_session_id, session_id);
            assert_eq!(
                session_structure
                    .tabs
                    .iter()
                    .map(|tab| tab.tab_id)
                    .collect::<Vec<_>>(),
                vec![booted_tab_id, added_tab_id],
            );
            assert_eq!(
                session_structure.tabs[0].layout.list_leaf_pane_ids(),
                vec![root_pane_id]
            );
            let LayoutNode::Pane(additional_pane_id) = session_structure.tabs[1].layout else {
                panic!(
                    "the added tab holds one pane, got {:?}",
                    session_structure.tabs[1]
                );
            };
            assert_ne!(additional_pane_id, root_pane_id);

            (
                vec![caller_connection, resumed_connection],
                (
                    session_id,
                    detached_client_id,
                    resumed_client_id,
                    booted_tab_id,
                    added_tab_id,
                    root_pane_id,
                ),
            )
        },
    );

    assert_ne!(
        resumed_client_id, detached_client_id,
        "the token attaches as a fresh client"
    );
    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    let resumed_client = session
        .clients
        .get_client_by_id(resumed_client_id)
        .expect("the client the token attached");
    assert_eq!(resumed_client.get_active_tab_id(), added_tab_id);
    assert_eq!(
        resumed_client.get_focused_pane_id(booted_tab_id),
        Some(root_pane_id)
    );
    assert_eq!(
        resumed_client.get_layout_mode(booted_tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: root_pane_id
        }
    );
    assert_eq!(
        resumed_client.get_scroll_offset(root_pane_id),
        SCROLL_LINE_COUNT
    );
}
