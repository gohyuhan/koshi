//! Tests for the session-server loop, driven by a fake PTY backend: the real
//! inbox loop runs without spawning a pane. The tests that rebuild a session
//! bind a real control socket inside a runtime directory created for the test.
//!
//! The image swap tests cover when the loop ends into a swap, what a restart is
//! refused for, which arguments the new image is started with, what the
//! carried state restores, over pseudoterminal masters this test binary opens
//! itself, and that a swap waits for the kill of a pane closed before it. The
//! ready line and the process image replacement are tested in
//! `crates/koshi/tests`.
//!
//! The tests of a start or a swap that fails run real `/bin/sh` panes, and
//! check that every pane is ended and its child reaped.

use super::*;

#[cfg(unix)]
use koshi_core::command::{ClosePaneArgs, NewPaneArgs, NewPanePlacement};
use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, CopyArgs, GridPosition, Selection,
    SelectionKind, SetSelectionArgs, VisualCommand, WriteToPaneArgs,
};
#[cfg(unix)]
use koshi_core::event::{Event, PaneCreated};
#[cfg(unix)]
use koshi_core::geometry::Direction;
use koshi_core::ids::{ClientId, CommandId};
use koshi_core::process::ExitStatus;
#[cfg(unix)]
use koshi_ipc::endpoint::EndpointFile;
use koshi_renderer::snapshot::Delivery;
use koshi_runtime::runtime::event::AttachAccepted;
#[cfg(unix)]
use koshi_runtime::runtime::event::SessionEnding;
use koshi_runtime::runtime::pty_inbox::InboxSink;
#[cfg(unix)]
use koshi_test_support::child_exit::wait_until_child_has_exited;
use koshi_test_support::fake_pty::FakePtyBackend;
#[cfg(unix)]
use koshi_test_support::fixtures::{
    BUSY_PROGRAM_RETRY_INTERVAL_DURATION, BUSY_PROGRAM_WAIT_DURATION,
};
use tempfile::TempDir;

/// A server built the way [`run_session_server`] builds it, on a fake backend,
/// plus that backend and a sender that queues events on the server's inbox.
/// The backend delivers each pane's output and exit into that inbox.
fn build_test_server() -> (Server, Arc<FakePtyBackend>, mpsc::Sender<RuntimeEvent>) {
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender.clone()),
    )));
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let mut server = Server::from_runtime_parts(pty_backend, runtime_event_receiver);
    server.load_startup_config(None);
    (server, fake_pty_backend, runtime_event_sender)
}

/// What [`run_session_server`] was started with, for the tests that read the
/// command line the swap builds from it.
fn build_test_session_start(
    runtime_directory: &Path,
    is_other_user_access_allowed: bool,
) -> SessionStart {
    SessionStart {
        runtime_directory: runtime_directory.to_path_buf(),
        session_id: SessionId::new(),
        session_name: "quiet-lake".to_string(),
        is_other_user_access_allowed,
        executable_watch: Arc::new(ExecutableWatch::new(
            PathBuf::from("/opt/koshi/bin/koshi"),
            "1.0.0",
        )),
        config_directory: None,
        supervisor_token: None,
        supervisor_process_id: None,
    }
}

/// The arguments of `process_command`, as plain strings in the order they are passed.
fn list_command_arguments(process_command: &std::process::Command) -> Vec<String> {
    process_command
        .get_args()
        .map(|command_argument| command_argument.to_string_lossy().into_owned())
        .collect()
}

/// Queue a restart request the way the control socket does, and hand back the
/// channel the dispatcher answers on.
fn request_session_restart(
    runtime_event_sender: &mpsc::Sender<RuntimeEvent>,
) -> mpsc::Receiver<Result<(), RestartRefusal>> {
    let (restart_response_sender, restart_response_receiver) = mpsc::channel();
    runtime_event_sender
        .send(RuntimeEvent::IpcRestart {
            response_sender: restart_response_sender,
        })
        .expect("the restart request is queued");
    restart_response_receiver
}

/// Queue a command the way a client's keybinding does over the control socket:
/// on a reply channel nobody reads, since that command is answered by the next
/// painted frame.
fn queue_command(
    runtime_event_sender: &mpsc::Sender<RuntimeEvent>,
    client_id: ClientId,
    command: Command,
) {
    runtime_event_sender
        .send(RuntimeEvent::Ipc {
            command_envelope: Box::new(CommandEnvelope::from_parts(
                CommandId::new(),
                CommandSource::from_key_binding(client_id),
                command,
            )),
            response_sender: mpsc::channel().0,
        })
        .expect("the command is queued");
}

/// Seed the one session this process serves, under a fresh session identifier.
fn seed_test_session(server: &mut Server) {
    server
        .bootstrap_session(
            SessionId::new(),
            "quiet-lake".to_string(),
            STARTING_VIEWPORT,
            SystemTime::now(),
            None,
        )
        .expect("the session is seeded");
}

/// Join the seeded session the way an attach over the control socket does, and
/// hand back what the dispatcher minted: the client's client identifier and its own queue.
fn attach_test_client(server: &mut Server) -> AttachAccepted {
    let (attach_response_sender, attach_response_receiver) = mpsc::channel();
    let _ = server.handle_runtime_event(RuntimeEvent::IpcAttach {
        resume_client_id: None,
        resume_token: None,
        viewport_size: STARTING_VIEWPORT,
        pane_area: None,
        cell_size: None,
        attached_at: SystemTime::now(),
        is_remote: false,
        response_sender: attach_response_sender,
    });
    attach_response_receiver
        .try_recv()
        .expect("the loop answered the attach request")
        .expect("a session is running")
}

/// The clients the seeded session still holds a record for, in id order.
fn list_attached_client_ids(server: &Server) -> Vec<ClientId> {
    let seeded_session = server
        .list_sessions()
        .values()
        .next()
        .expect("the session is seeded");
    let mut attached_client_ids: Vec<ClientId> = seeded_session
        .clients
        .list_attached_clients()
        .map(|client_record| client_record.get_client_id())
        .collect();
    attached_client_ids.sort();
    attached_client_ids
}

#[test]
fn the_session_answers_discovery_with_the_id_and_name_it_was_started_with() {
    let (mut server, _, runtime_event_sender) = build_test_server();
    let session_id = SessionId::new();
    server
        .bootstrap_session(
            session_id,
            "quiet-lake".to_string(),
            STARTING_VIEWPORT,
            SystemTime::now(),
            None,
        )
        .expect("the session is seeded");

    let (discovery_response_sender, discovery_response_receiver) = mpsc::channel();
    runtime_event_sender
        .send(RuntimeEvent::IpcDiscovery {
            response_sender: discovery_response_sender,
        })
        .expect("the discovery request is queued");
    runtime_event_sender
        .send(RuntimeEvent::Quit)
        .expect("the hangup is queued");

    run_session_serve_loop(&mut server);

    let discovery_overview = discovery_response_receiver
        .try_recv()
        .expect("the loop answered the discovery request")
        .expect("a session is running");
    assert_eq!(discovery_overview.session.session_id, session_id);
    assert_eq!(discovery_overview.session.session_name, "quiet-lake");
    assert_eq!(discovery_overview.session.pane_count, 1);
}

#[test]
fn a_quit_command_arriving_on_the_socket_ends_the_loop() {
    let (mut server, _, runtime_event_sender) = build_test_server();
    let session_id = SessionId::new();
    server
        .bootstrap_session(
            session_id,
            "quiet-lake".to_string(),
            STARTING_VIEWPORT,
            SystemTime::now(),
            None,
        )
        .expect("the session is seeded");

    let command_id = CommandId::new();
    let (command_response_sender, command_response_receiver) = mpsc::channel();
    runtime_event_sender
        .send(RuntimeEvent::Ipc {
            command_envelope: Box::new(CommandEnvelope::from_parts(
                command_id,
                CommandSource::ExternalCli {
                    session_id: Some(session_id),
                    target_client_id: None,
                },
                Command::Quit,
            )),
            response_sender: command_response_sender,
        })
        .expect("the quit command is queued");

    run_session_serve_loop(&mut server);

    assert_eq!(
        command_response_receiver
            .try_recv()
            .expect("the loop answered the command"),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );
    assert!(server.is_quit_requested());
}

#[test]
fn the_last_childs_exit_ends_the_loop_with_no_quit_asked_for() {
    // Nothing queues a quit, and the sender stays alive.
    let (mut server, _, runtime_event_sender) = build_test_server();
    seed_test_session(&mut server);
    let pane_id = *server
        .list_terminal_engines()
        .keys()
        .next()
        .expect("the root pane holds a terminal engine");

    runtime_event_sender
        .send(RuntimeEvent::ChildExit {
            pane_id,
            exit_status: ExitStatus::ExitCode(0),
        })
        .expect("the child's exit is queued");

    run_session_serve_loop(&mut server);

    assert!(!server.has_active_panes());
    assert!(!server.is_quit_requested());
}

#[test]
fn a_due_render_hands_the_attached_client_its_frame() {
    let (mut server, _, runtime_event_sender) = build_test_server();
    seed_test_session(&mut server);
    let attached_client = attach_test_client(&mut server);
    let pane_id = *server
        .list_terminal_engines()
        .keys()
        .next()
        .expect("the root pane holds a terminal engine");

    runtime_event_sender
        .send(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: b"hello".to_vec(),
        })
        .expect("the child output is queued");
    runtime_event_sender
        .send(RuntimeEvent::Quit)
        .expect("the hangup is queued");

    run_session_serve_loop(&mut server);

    let expected_snapshot = server
        .build_snapshot(attached_client.client_id)
        .expect("the attached client has a frame");
    assert_eq!(
        attached_client.deliveries.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Frame(Box::new(expected_snapshot))]
    );
}

#[test]
fn a_pass_with_no_render_due_pushes_no_frame() {
    let (mut server, _, runtime_event_sender) = build_test_server();
    seed_test_session(&mut server);
    let attached_client = attach_test_client(&mut server);
    // Spend the render the seeding and the attach made due. The pass below
    // starts with nothing pending.
    assert!(server.poll_render(Instant::now()));

    runtime_event_sender
        .send(RuntimeEvent::Quit)
        .expect("the hangup is queued");

    run_session_serve_loop(&mut server);

    assert_eq!(
        attached_client.deliveries.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
}

#[test]
fn an_accepted_restart_ends_the_loop_into_the_swap() {
    // The reply is written while the socket is still up; the swap runs after
    // the loop ends.
    let (mut server, _, runtime_event_sender) = build_test_server();
    seed_test_session(&mut server);
    server.set_restart_check(Arc::new(|| Ok(())));

    let restart_response_receiver = request_session_restart(&runtime_event_sender);

    let serve_outcome = run_session_serve_loop(&mut server);

    assert_eq!(
        restart_response_receiver
            .try_recv()
            .expect("the loop answered the request"),
        Ok(())
    );
    assert!(server.is_restart_requested());
    assert_eq!(serve_outcome, ServeOutcome::Restart);
}

#[test]
fn a_restart_naming_a_binary_that_cannot_be_read_is_refused_and_the_session_keeps_serving() {
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let missing_executable_path = runtime_directory_fixture.path().join("koshi");
    let (mut server, _, runtime_event_sender) = build_test_server();
    seed_test_session(&mut server);
    // The first thing the installed check runs, on a path with nothing at it.
    let named_executable_path = missing_executable_path.clone();
    server.set_restart_check(Arc::new(move || {
        is_binary_runnable(&named_executable_path)
            .map_err(|refusal_reason| RestartRefusal::UnfitProgramFile { refusal_reason })
    }));

    let restart_response_receiver = request_session_restart(&runtime_event_sender);
    let (discovery_response_sender, discovery_response_receiver) = mpsc::channel();
    runtime_event_sender
        .send(RuntimeEvent::IpcDiscovery {
            response_sender: discovery_response_sender,
        })
        .expect("the discovery request is queued");
    runtime_event_sender
        .send(RuntimeEvent::Quit)
        .expect("the hangup is queued");

    let serve_outcome = run_session_serve_loop(&mut server);

    let unreadable_metadata_error =
        std::fs::metadata(&missing_executable_path).expect_err("nothing is at that path");
    assert_eq!(
        restart_response_receiver
            .try_recv()
            .expect("the loop answered the request")
            .expect_err("a binary that is not there is refused"),
        RestartRefusal::UnfitProgramFile {
            refusal_reason: format!(
                "the binary at {} could not be read: {unreadable_metadata_error}",
                missing_executable_path.display()
            )
        }
    );
    assert!(!server.is_restart_requested());
    assert_eq!(serve_outcome, ServeOutcome::Ended);
    let discovery_overview = discovery_response_receiver
        .try_recv()
        .expect("the loop answered the discovery request")
        .expect("a session is running");
    assert_eq!(discovery_overview.session.pane_count, 1);
}

#[cfg(unix)]
#[test]
fn the_check_installed_on_the_server_refuses_a_restart_naming_a_binary_that_is_not_there() {
    // Every restart the socket answers runs the check `install_restart_check`
    // built; that check runs `is_binary_runnable` first.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let missing_executable_path = runtime_directory_fixture.path().join("koshi");
    let (mut server, _, runtime_event_sender) = build_test_server();
    seed_test_session(&mut server);
    let portable_pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender.clone()),
    )));

    install_restart_check(&mut server, &portable_pty_backend, &missing_executable_path);

    let restart_response_receiver = request_session_restart(&runtime_event_sender);
    runtime_event_sender
        .send(RuntimeEvent::Quit)
        .expect("the hangup is queued");

    let serve_outcome = run_session_serve_loop(&mut server);

    let unreadable_metadata_error =
        std::fs::metadata(&missing_executable_path).expect_err("nothing is at that path");
    assert_eq!(
        restart_response_receiver
            .try_recv()
            .expect("the loop answered the request"),
        Err(RestartRefusal::UnfitProgramFile {
            refusal_reason: format!(
                "the binary at {} could not be read: {unreadable_metadata_error}",
                missing_executable_path.display()
            )
        })
    );
    assert!(!server.is_restart_requested());
    assert_eq!(serve_outcome, ServeOutcome::Ended);
}

#[test]
fn a_copy_applied_while_the_swap_runs_reaches_the_clients_own_terminal() {
    // The swap applies what the socket queued after the serve loop returned,
    // and hands the bytes it produced to the clients.
    let (mut server, _, runtime_event_sender) = build_test_server();
    seed_test_session(&mut server);
    let attached_client = attach_test_client(&mut server);
    let pane_id = *server
        .list_terminal_engines()
        .keys()
        .next()
        .expect("the root pane holds a terminal engine");
    server.handle_pty_output(pane_id, b"hello");
    queue_command(
        &runtime_event_sender,
        attached_client.client_id,
        Command::Visual(VisualCommand::SetSelection(SetSelectionArgs {
            pane_id,
            selection: Selection {
                selection_kind: SelectionKind::Character,
                anchor: GridPosition {
                    row_index: 0,
                    column_index: 0,
                },
                cursor: GridPosition {
                    row_index: 0,
                    column_index: 4,
                },
            },
        })),
    );
    queue_command(
        &runtime_event_sender,
        attached_client.client_id,
        Command::Visual(VisualCommand::Copy(CopyArgs {
            pane_id,
            should_trim_trailing_whitespace: true,
        })),
    );

    apply_queued_runtime_events(&mut server, DetachPolicy::Apply);

    // base64("hello") = aGVsbG8=
    let host_write_bytes: Vec<Vec<u8>> = attached_client
        .deliveries
        .try_iter()
        .filter_map(|delivery| match delivery {
            Delivery::HostWrite(written_bytes) => Some(written_bytes),
            _ => None,
        })
        .collect();
    assert_eq!(host_write_bytes, vec![b"\x1b]52;c;aGVsbG8=\x07".to_vec()]);
}

#[test]
fn a_quit_applied_while_the_swap_runs_ends_the_session_instead_of_serving_it_again() {
    // `koshi kill-session` arriving inside the swap window is applied by the
    // same pass, after the serve loop returned.
    let (mut server, _, runtime_event_sender) = build_test_server();
    let session_id = SessionId::new();
    server
        .bootstrap_session(
            session_id,
            "quiet-lake".to_string(),
            STARTING_VIEWPORT,
            SystemTime::now(),
            None,
        )
        .expect("the session is seeded");
    server.set_restart_check(Arc::new(|| Ok(())));
    let restart_response_receiver = request_session_restart(&runtime_event_sender);
    assert_eq!(run_session_serve_loop(&mut server), ServeOutcome::Restart);
    assert_eq!(
        restart_response_receiver
            .try_recv()
            .expect("the loop answered the request"),
        Ok(())
    );

    runtime_event_sender
        .send(RuntimeEvent::Ipc {
            command_envelope: Box::new(CommandEnvelope::from_parts(
                CommandId::new(),
                CommandSource::ExternalCli {
                    session_id: Some(session_id),
                    target_client_id: None,
                },
                Command::Quit,
            )),
            response_sender: mpsc::channel().0,
        })
        .expect("the quit command is queued");
    apply_queued_runtime_events(&mut server, DetachPolicy::Apply);
    assert!(
        server.is_quit_requested(),
        "the swap's inbox pass applies the quit"
    );

    let (discovery_response_sender, discovery_response_receiver) = mpsc::channel();
    runtime_event_sender
        .send(RuntimeEvent::IpcDiscovery {
            response_sender: discovery_response_sender,
        })
        .expect("the discovery request is queued");

    assert_eq!(run_session_serve_loop(&mut server), ServeOutcome::Ended);
    assert_eq!(
        discovery_response_receiver
            .try_recv()
            .expect_err("a session that was asked to quit answers nothing else"),
        mpsc::TryRecvError::Empty
    );
}

#[test]
#[cfg(unix)]
fn a_swap_the_session_abandons_takes_the_accepted_restart_back() {
    // Every abandon path hands the same server back to the serve loop.
    let (mut server, _, runtime_event_sender) = build_test_server();
    seed_test_session(&mut server);
    server.set_restart_check(Arc::new(|| Ok(())));
    let restart_response_receiver = request_session_restart(&runtime_event_sender);
    assert_eq!(run_session_serve_loop(&mut server), ServeOutcome::Restart);
    assert_eq!(
        restart_response_receiver
            .try_recv()
            .expect("the loop answered the request"),
        Ok(())
    );
    assert!(server.is_restart_requested());
    let portable_pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender.clone()),
    )));

    let restored_server = restore_session_serving(server, &portable_pty_backend);

    assert!(!restored_server.is_restart_requested());
    assert!(!restored_server.is_quit_requested());
}

#[test]
fn a_client_that_hung_up_before_the_swap_told_anyone_is_detached() {
    // The inbox passes before the announce run while every client is still
    // streaming.
    let (mut server, _, runtime_event_sender) = build_test_server();
    seed_test_session(&mut server);
    let staying_client = attach_test_client(&mut server);
    let leaving_client = attach_test_client(&mut server);
    runtime_event_sender
        .send(RuntimeEvent::ClientDetached {
            client_id: leaving_client.client_id,
            detached_at: SystemTime::now(),
            is_streamed: true,
        })
        .expect("the detach is queued");

    apply_queued_runtime_events(&mut server, DetachPolicy::Apply);

    assert_eq!(
        list_attached_client_ids(&server),
        vec![staying_client.client_id]
    );
}

#[test]
fn the_record_of_a_client_the_swap_told_survives_the_pass_after_the_announce() {
    // Both halves of a told client's connection queue a detach once it reads
    // the restart frame.
    let (mut server, _, runtime_event_sender) = build_test_server();
    seed_test_session(&mut server);
    let told_client = attach_test_client(&mut server);
    runtime_event_sender
        .send(RuntimeEvent::ClientDetached {
            client_id: told_client.client_id,
            detached_at: SystemTime::now(),
            is_streamed: true,
        })
        .expect("the detach is queued");

    apply_queued_runtime_events(&mut server, DetachPolicy::Skip);

    assert_eq!(
        list_attached_client_ids(&server),
        vec![told_client.client_id]
    );
}

#[test]
fn a_line_a_client_types_as_it_reads_the_restart_frame_reaches_its_pane() {
    // The client answers the frame on its socket after the announce returns.
    // `IpcServer::close_intake` puts what crossed in the inbox before this pass
    // runs.
    let (mut server, fake_pty_backend, runtime_event_sender) = build_test_server();
    seed_test_session(&mut server);
    let client_id = attach_test_client(&mut server).client_id;
    let pane_id = *server
        .list_terminal_engines()
        .keys()
        .next()
        .expect("the root pane holds a terminal engine");
    queue_command(
        &runtime_event_sender,
        client_id,
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(pane_id),
            pane_input_bytes: vec![b'\r'],
        }),
    );

    apply_queued_runtime_events(&mut server, DetachPolicy::Skip);

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("the pane is open"),
        vec![vec![b'\r']]
    );
}

#[test]
fn the_carried_state_reads_back_with_every_tab_pane_and_screen() {
    // The session server writes the state to a file, and the image that
    // replaces it reads that file back.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let resume_file_path = runtime_directory_fixture.path().join("session.resume");
    let (mut server, fake_pty_backend, _runtime_event_sender) = build_test_server();
    let session_id = SessionId::new();
    let client_id = server
        .bootstrap_local_named(
            session_id,
            "quiet-lake".to_string(),
            STARTING_VIEWPORT,
            SystemTime::UNIX_EPOCH,
        )
        .expect("the session is seeded");
    let open_second_tab_command_id = CommandId::new();
    let command_envelope = CommandEnvelope::from_parts(
        open_second_tab_command_id,
        CommandSource::from_key_binding(client_id),
        Command::NewTab(koshi_core::command::NewTabArgs {
            working_directory: None,
            client_id: Some(client_id),
        }),
    );
    match server.submit_command(command_envelope) {
        CommandResult::Ok { command_id, .. } => {
            assert_eq!(command_id, open_second_tab_command_id)
        }
        unexpected_result => panic!("the second tab must open, got {unexpected_result:?}"),
    }
    // Each pane prints output of its own.
    let pane_ids: Vec<PaneId> = server.list_terminal_engines().keys().copied().collect();
    assert_eq!(pane_ids.len(), 2, "two tabs means two panes");
    for (pane_index, pane_id) in pane_ids.iter().enumerate() {
        server.handle_pty_output(*pane_id, format!("pane {pane_index}").as_bytes());
        // Carrying the state out cancels the sequence each engine is still
        // decoding, which settles the grapheme cluster it holds too. Settling
        // it here makes the screen read below the one that is carried.
        server.handle_pty_output(*pane_id, &[0x18]);
    }
    let expected_tabs = server.list_sessions()[&session_id].tabs.clone();
    let expected_pane_records = server.list_sessions()[&session_id].panes.clone();
    let expected_screens: HashMap<PaneId, koshi_terminal::state::TerminalState> = server
        .list_terminal_engines()
        .iter()
        .map(|(pane_id, terminal_engine)| (*pane_id, terminal_engine.get_terminal_state().clone()))
        .collect();
    let carried_pty_panes: Vec<CarriedPtyPane> = pane_ids
        .iter()
        .enumerate()
        .map(|(pane_index, pane_id)| CarriedPtyPane {
            pane_id: *pane_id,
            #[cfg(unix)]
            terminal_fd: Some(30 + pane_index as i32),
            process_id: 4000 + pane_index as u32,
            pty_size: PtySize {
                column_count: 1,
                row_count: 1,
            },
            exit_status: None,
        })
        .collect();

    let (resume_header, resume_body) = server
        .carry_out(&carried_pty_panes)
        .expect("a session to carry");
    resume::write_resume_file(&resume_file_path, &resume_header, &resume_body)
        .expect("the carried state is written");
    let (read_resume_header, encoded_resume_body) =
        resume::read_resume_header(&resume_file_path).expect("the header reads back");
    let decoded_resume_body =
        resume::read_resume_body(read_resume_header.resume_format, &encoded_resume_body)
            .expect("the body reads back");
    let (_, resumed_event_receiver) = mpsc::channel();
    let resumed_server = Server::resume(
        Arc::clone(&fake_pty_backend) as Arc<dyn PtyBackend>,
        resumed_event_receiver,
        None,
        decoded_resume_body,
        build_carried_pty_sizes(&read_resume_header),
        HashMap::new(),
    );

    assert_eq!(
        read_resume_header, resume_header,
        "the header must read back unchanged"
    );
    assert_eq!(resumed_server.list_sessions().len(), 1);
    let resumed_session = &resumed_server.list_sessions()[&session_id];
    assert_eq!(resumed_session.session_name, "quiet-lake");
    assert_eq!(
        resumed_session.tabs, expected_tabs,
        "every tab and its layout tree"
    );
    assert_eq!(
        resumed_session.panes, expected_pane_records,
        "every pane record"
    );
    for (pane_id, expected_screen) in &expected_screens {
        assert_eq!(
            resumed_server
                .list_terminal_engines()
                .get(pane_id)
                .expect("the pane's engine came back")
                .get_terminal_state(),
            expected_screen,
            "the screen of pane {pane_id}"
        );
    }
    let carried_pty_size_by_pane_id = build_carried_pty_sizes(&read_resume_header);
    let mut named_pane_ids: Vec<PaneId> = carried_pty_size_by_pane_id.keys().copied().collect();
    named_pane_ids.sort();
    let mut held_pane_ids = pane_ids.clone();
    held_pane_ids.sort();
    assert_eq!(
        named_pane_ids, held_pane_ids,
        "every pane the session held names a size"
    );
    assert_eq!(
        carried_pty_size_by_pane_id,
        read_resume_header
            .carried_panes
            .iter()
            .map(|pane_record| {
                (
                    pane_record.pane_id,
                    PtySize {
                        row_count: pane_record.row_count,
                        column_count: pane_record.column_count,
                    },
                )
            })
            .collect::<HashMap<PaneId, PtySize>>(),
        "each pane names the size its own record holds"
    );
}

#[test]
#[cfg(unix)]
fn a_carried_descriptor_that_is_no_terminal_master_is_refused_and_left_open() {
    // The resume file carries plain numbers, and this image holds its own open
    // descriptors under numbers of the same shape.
    use std::os::fd::AsRawFd;

    let ordinary_file = std::fs::File::open("/dev/null").expect("open an ordinary file");
    let terminal_file_descriptor = ordinary_file.as_raw_fd();
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let pane_id = PaneId::new();
    let resume_header = ResumeHeader {
        resume_format: 1,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![koshi_runtime::resume::CarriedPane {
            pane_id,
            // Process id 0 names no pane child: ending the refused pane
            // signals nothing.
            process_id: 0,
            row_count: 24,
            column_count: 80,
            terminal_fd: Some(terminal_file_descriptor),
            terminal_name: None,
            exit_status: None,
        }],
    };
    let pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(mpsc::channel().0),
    )));

    let take_back_error = take_one_pane_back(&pty_backend, &resume_header.carried_panes[0])
        .expect_err("a descriptor that is no terminal master must be refused");
    let (_, pty_size_by_pane_id, _) = take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    )
    .expect("every path returns Ok");

    assert_eq!(
        take_back_error.to_string(),
        format!(
            "pane {pane_id} carried descriptor {terminal_file_descriptor}, which names no pseudoterminal master, \
             so it cannot be taken back"
        )
    );
    assert_eq!(
        pty_size_by_pane_id,
        HashMap::new(),
        "the refused pane does not come back"
    );
    assert!(
        unsafe { libc::fcntl(terminal_file_descriptor, libc::F_GETFD) } >= 0,
        "the refused descriptor must be left open"
    );
}

#[test]
#[cfg(unix)]
fn carried_panes_in_conflict_are_ended_and_reaped_and_their_shared_terminal_closed_once() {
    use std::os::unix::process::CommandExt;

    let mut child_command = std::process::Command::new("/bin/sh");
    child_command
        .arg("-c")
        .arg("sleep 30")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // The child leads its own process group, as `portable-pty` places a pane's
    // child.
    unsafe {
        child_command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child_process_id = child_command.spawn().expect("the child starts").id();
    let terminal_master_file_descriptor =
        unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(
        terminal_master_file_descriptor >= 0,
        "the pseudoterminal master opens"
    );
    let carried_terminal_name = find_terminal_master_name(terminal_master_file_descriptor)
        .expect("the master names its terminal");
    // A second number on the same pseudoterminal, held to the end of the test,
    // keeps that terminal allocated to this test.
    let held_terminal_file_descriptor = unsafe { libc::dup(terminal_master_file_descriptor) };
    assert!(
        held_terminal_file_descriptor >= 0,
        "the master opens under a second number"
    );
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let build_carried_pane_record = |process_id| koshi_runtime::resume::CarriedPane {
        pane_id: PaneId::new(),
        process_id,
        row_count: 24,
        column_count: 80,
        terminal_fd: Some(terminal_master_file_descriptor),
        terminal_name: Some(carried_terminal_name.clone()),
        exit_status: None,
    };
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![
            build_carried_pane_record(child_process_id),
            build_carried_pane_record(0),
        ],
    };

    let (pty_backend, pty_size_by_pane_id, _) = take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    )
    .expect("every path returns Ok");

    wait_until_child_is_reaped(child_process_id);
    assert_eq!(
        pty_size_by_pane_id,
        HashMap::new(),
        "neither pane comes back"
    );
    assert_eq!(pty_backend.list_carried_panes(), Vec::new());
    assert_ne!(
        find_terminal_master_name(terminal_master_file_descriptor),
        Some(carried_terminal_name),
        "the shared terminal is closed"
    );
    assert_eq!(
        unsafe { libc::close(held_terminal_file_descriptor) },
        0,
        "the test still owns the second number it opened"
    );
}

#[test]
#[cfg(unix)]
fn a_swap_that_did_not_happen_leaves_every_terminal_closed_on_exec_again() {
    // The swap clears the close-on-exec flag on each pane's descriptor.
    use std::os::fd::AsRawFd;

    let terminal_file = std::fs::File::open("/dev/null").expect("open a descriptor");
    let terminal_file_descriptor = terminal_file.as_raw_fd();
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let resume_header = ResumeHeader {
        resume_format: 1,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![koshi_runtime::resume::CarriedPane {
            pane_id: PaneId::new(),
            process_id: std::process::id(),
            row_count: 24,
            column_count: 80,
            terminal_fd: Some(terminal_file_descriptor),
            terminal_name: None,
            exit_status: None,
        }],
    };

    keep_terminals_across_exec(&resume_header).expect("the flag is cleared");
    let cleared_descriptor_flags = unsafe { libc::fcntl(terminal_file_descriptor, libc::F_GETFD) };
    put_close_on_exec_back(&resume_header);
    let restored_descriptor_flags = unsafe { libc::fcntl(terminal_file_descriptor, libc::F_GETFD) };

    assert_eq!(cleared_descriptor_flags & libc::FD_CLOEXEC, 0);
    assert_eq!(
        restored_descriptor_flags & libc::FD_CLOEXEC,
        libc::FD_CLOEXEC
    );
}

#[test]
#[cfg(unix)]
fn a_terminal_this_process_does_not_hold_stops_the_flags_being_cleared_for_the_swap() {
    // Clearing the flags runs over the plain numbers the carried state holds,
    // before anything about the image changes. A number this process does not
    // hold comes back as a failure.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let unheld_resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![koshi_runtime::resume::CarriedPane {
            pane_id: PaneId::new(),
            process_id: NO_SUCH_PROCESS,
            row_count: 24,
            column_count: 80,
            terminal_fd: Some(NEVER_OPENED_TERMINAL_FILE_DESCRIPTOR),
            terminal_name: None,
            exit_status: None,
        }],
    };

    let resume_error = keep_terminals_across_exec(&unheld_resume_header)
        .expect_err("a descriptor this process does not hold has no flags to clear");

    assert_eq!(resume_error.raw_os_error(), Some(libc::EBADF));

    // A record naming no descriptor is passed over.
    let descriptorless_resume_header = ResumeHeader {
        carried_panes: vec![koshi_runtime::resume::CarriedPane {
            terminal_fd: None,
            ..unheld_resume_header.carried_panes[0].clone()
        }],
        ..unheld_resume_header.clone()
    };
    keep_terminals_across_exec(&descriptorless_resume_header)
        .expect("a record naming no descriptor leaves nothing to clear");
}

#[test]
fn the_resume_command_line_names_the_state_and_never_a_profile() {
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let resume_file_path = runtime_directory_fixture.path().join("session.resume");

    let command_arguments =
        list_command_arguments(&build_resume_command(&session_start, &resume_file_path));

    assert_eq!(
        command_arguments,
        vec![
            "serve-session".to_string(),
            session_start.session_id.to_string(),
            "quiet-lake".to_string(),
            "--runtime-dir".to_string(),
            runtime_directory_fixture.path().display().to_string(),
            "--resume".to_string(),
            resume_file_path.display().to_string(),
        ]
    );
}

#[test]
fn the_resume_command_line_keeps_the_reach_this_session_was_started_with() {
    // `--allow-other-users` is passed on to the resume run.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let mut session_start = build_test_session_start(runtime_directory_fixture.path(), true);
    session_start.supervisor_token = Some("a-secret".to_string());
    session_start.supervisor_process_id = Some(4821);
    let resume_file_path = runtime_directory_fixture.path().join("session.resume");

    let command_arguments =
        list_command_arguments(&build_resume_command(&session_start, &resume_file_path));

    assert_eq!(
        command_arguments,
        vec![
            "serve-session".to_string(),
            session_start.session_id.to_string(),
            "quiet-lake".to_string(),
            "--runtime-dir".to_string(),
            runtime_directory_fixture.path().display().to_string(),
            "--resume".to_string(),
            resume_file_path.display().to_string(),
            "--allow-other-users".to_string(),
            "--supervisor-token".to_string(),
            "a-secret".to_string(),
            "--supervisor-pid".to_string(),
            "4821".to_string(),
        ]
    );
}

#[test]
fn the_line_a_build_prints_names_the_formats_it_reads() {
    assert_eq!(
        parse_resume_support("{\"minimum_resume_format\":1,\"maximum_resume_format\":3}")
            .expect("the line reads"),
        ResumeSupport {
            minimum_resume_format: 1,
            maximum_resume_format: 3
        }
    );
    assert_eq!(
        serde_json::to_string(&ResumeSupport::from_current_build()).expect("the range encodes"),
        format!("{{\"minimum_resume_format\":{RESUME_FORMAT_MIN},\"maximum_resume_format\":{RESUME_FORMAT}}}")
    );
    assert_eq!(
        parse_resume_support("koshi 0.2.0").expect_err("a version line is not a range"),
        "does not say which resume formats it reads: expected value at line 1 column 1"
    );
}

#[test]
fn a_binary_reading_no_format_this_one_writes_is_refused_naming_both_ranges() {
    let executable_path = PathBuf::from("/opt/koshi/bin/koshi");

    let compatibility_error = reads_the_format_this_build_writes(
        ResumeSupport {
            minimum_resume_format: 7,
            maximum_resume_format: 9,
        },
        &executable_path,
    )
    .expect_err("a range the written format is outside of is refused");

    assert_eq!(
        compatibility_error,
        format!(
            "the binary at /opt/koshi/bin/koshi reads resume formats 7 to 9, and this one reads \
             {RESUME_FORMAT_MIN} to {RESUME_FORMAT} and writes {RESUME_FORMAT}"
        )
    );
    assert_eq!(
        reads_the_format_this_build_writes(
            ResumeSupport {
                minimum_resume_format: RESUME_FORMAT_MIN,
                maximum_resume_format: RESUME_FORMAT + 4
            },
            &executable_path
        ),
        Ok(())
    );

    // A binary that reads only formats 1 and 2 is refused, and the error
    // names both ranges.
    assert_eq!(
        reads_the_format_this_build_writes(
            ResumeSupport {
                minimum_resume_format: 1,
                maximum_resume_format: 2
            },
            &executable_path
        ),
        Err(format!(
            "the binary at /opt/koshi/bin/koshi reads resume formats 1 to 2, and this one reads \
             {RESUME_FORMAT_MIN} to {RESUME_FORMAT} and writes {RESUME_FORMAT}"
        ))
    );
}

/// A runnable stand-in for the program file: a script at `probe_binary_path`
/// that prints `resume_support_line` and exits, whatever it is asked. Unix
/// only: it runs through its `#!/bin/sh` line.
#[cfg(unix)]
fn write_probe_binary(probe_binary_path: &Path, resume_support_line: &str) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::write(
        probe_binary_path,
        format!("#!/bin/sh\nprintf '%s\\n' '{resume_support_line}'\n"),
    )
    .expect("the stand-in binary is written");
    std::fs::set_permissions(probe_binary_path, std::fs::Permissions::from_mode(0o755))
        .expect("the stand-in binary is runnable");
}

#[test]
fn a_binary_that_cannot_be_run_is_refused_naming_the_path_and_the_reason() {
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let executable_path = runtime_directory_fixture.path().join("koshi");
    let process_spawn_error = std::process::Command::new(&executable_path)
        .arg(RESUME_SUPPORT_SUBCOMMAND)
        .spawn()
        .expect_err("nothing is at that path");

    assert_eq!(
        read_resume_support(&executable_path),
        Err(format!(
            "the binary at {} could not be run: {process_spawn_error}",
            executable_path.display()
        ))
    );
}

#[cfg(unix)]
#[test]
fn a_binary_answering_a_range_this_one_writes_into_passes_the_whole_check() {
    // The three answers the session must hold before it tears itself down: the
    // binary runs, the panes cross, and the binary reads what this one writes.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let executable_path = runtime_directory_fixture.path().join("koshi");
    write_probe_binary(
        &executable_path,
        &format!(
            "{{\"minimum_resume_format\":{RESUME_FORMAT_MIN},\"maximum_resume_format\":{}}}",
            RESUME_FORMAT + 5
        ),
    );

    assert_eq!(
        read_resume_support(&executable_path),
        Ok(ResumeSupport {
            minimum_resume_format: RESUME_FORMAT_MIN,
            maximum_resume_format: RESUME_FORMAT + 5
        })
    );
    assert_eq!(is_binary_runnable(&executable_path), Ok(()));
    assert_eq!(
        reads_the_format_this_build_writes(
            ResumeSupport {
                minimum_resume_format: RESUME_FORMAT_MIN,
                maximum_resume_format: RESUME_FORMAT + 5
            },
            &executable_path
        ),
        Ok(())
    );
}

#[cfg(unix)]
#[test]
fn a_binary_that_prints_nothing_is_refused_rather_than_read_as_a_range() {
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let executable_path = runtime_directory_fixture.path().join("koshi");
    write_probe_binary(&executable_path, "");

    let compatibility_error =
        read_resume_support(&executable_path).expect_err("an empty answer is no answer");

    assert_eq!(
        compatibility_error,
        format!(
            "the binary at {} does not say which resume formats it reads: EOF while parsing a \
             value at line 1 column 0",
            executable_path.display()
        )
    );
}

#[cfg(unix)]
#[test]
fn a_binary_answering_a_range_that_misses_this_ones_is_refused_naming_both() {
    // Both directions of a miss are refused: a range wholly above what this
    // build writes, and one wholly below it.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let executable_path = runtime_directory_fixture.path().join("koshi");
    let unsupported_minimum_resume_format = RESUME_FORMAT + 1;
    write_probe_binary(
        &executable_path,
        &format!(
            "{{\"minimum_resume_format\":{unsupported_minimum_resume_format},\"maximum_resume_format\":{}}}",
            unsupported_minimum_resume_format + 2
        ),
    );

    let resume_support = read_resume_support(&executable_path).expect("the line reads as a range");
    assert_eq!(
        resume_support,
        ResumeSupport {
            minimum_resume_format: unsupported_minimum_resume_format,
            maximum_resume_format: unsupported_minimum_resume_format + 2
        }
    );
    assert_eq!(
        reads_the_format_this_build_writes(resume_support, &executable_path),
        Err(format!(
            "the binary at {} reads resume formats {unsupported_minimum_resume_format} to {}, and this one reads \
             {RESUME_FORMAT_MIN} to {RESUME_FORMAT} and writes {RESUME_FORMAT}",
            executable_path.display(),
            unsupported_minimum_resume_format + 2
        ))
    );
    assert_eq!(
        reads_the_format_this_build_writes(
            ResumeSupport {
                minimum_resume_format: 0,
                maximum_resume_format: 0
            },
            &executable_path
        ),
        Err(format!(
            "the binary at {} reads resume formats 0 to 0, and this one reads \
             {RESUME_FORMAT_MIN} to {RESUME_FORMAT} and writes {RESUME_FORMAT}",
            executable_path.display()
        ))
    );
}

#[cfg(unix)]
#[test]
fn an_image_swap_that_could_not_start_hands_back_its_reason_and_keeps_ignoring_sigpipe() {
    // Replacing the image resets `SIGPIPE` to its default in this process
    // before the call. A swap that did not start puts the ignore back.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let mut session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    session_start.executable_watch = Arc::new(ExecutableWatch::new(
        runtime_directory_fixture
            .path()
            .join("koshi-that-is-not-there"),
        "1.0.0",
    ));
    let resume_file_path = runtime_directory_fixture.path().join("session.resume");

    let restart_error = restart_session_by_exec(&session_start, &resume_file_path);

    assert_eq!(restart_error.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(restart_error.raw_os_error(), Some(libc::ENOENT));

    let mut held_sigpipe_action: libc::sigaction = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::sigaction(libc::SIGPIPE, std::ptr::null(), &mut held_sigpipe_action,) },
        0,
        "the signal's handler is read back"
    );
    assert_eq!(
        held_sigpipe_action.sa_sigaction,
        libc::SIG_IGN,
        "a swap that did not start must leave the broken-pipe signal ignored"
    );
}

#[cfg(unix)]
#[test]
fn a_pane_the_header_names_no_descriptor_for_refuses_to_be_taken_back() {
    // A Unix pane is taken back by its descriptor and nothing else.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let stranded_pane_id = PaneId::new();
    let carried_pane = koshi_runtime::resume::CarriedPane {
        pane_id: stranded_pane_id,
        process_id: 0,
        row_count: 24,
        column_count: 80,
        terminal_fd: None,
        terminal_name: None,
        exit_status: None,
    };
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![carried_pane.clone()],
    };
    let pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(mpsc::channel().0),
    )));

    let take_back_error = take_one_pane_back(&pty_backend, &carried_pane)
        .expect_err("a pane with no descriptor cannot be taken back");
    let (_, pty_size_by_pane_id, _) = take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    )
    .expect("every path returns Ok");

    assert_eq!(
        take_back_error.to_string(),
        format!(
            "pane {stranded_pane_id} carried no terminal descriptor, so it cannot be taken back"
        )
    );
    assert_eq!(pty_size_by_pane_id, HashMap::new());
}

#[cfg(unix)]
#[test]
fn the_children_of_panes_not_taken_back_are_ended_and_reaped() {
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let child_process_ids: Vec<u32> = (0..3)
        .map(|_| start_group_leading_child("sleep", &["100"]))
        .collect();
    let conflicting_pane_id = PaneId::new();
    let build_carried_pane =
        |pane_id: PaneId, process_id: u32| koshi_runtime::resume::CarriedPane {
            pane_id,
            process_id,
            row_count: 24,
            column_count: 80,
            terminal_fd: None,
            terminal_name: None,
            exit_status: None,
        };
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![
            build_carried_pane(PaneId::new(), child_process_ids[0]),
            build_carried_pane(conflicting_pane_id, child_process_ids[1]),
            build_carried_pane(conflicting_pane_id, child_process_ids[2]),
        ],
    };

    let (_, pty_size_by_pane_id, _) = take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    )
    .expect("every path returns Ok");

    assert_eq!(pty_size_by_pane_id, HashMap::new());
    for child_process_id in child_process_ids {
        wait_until_child_is_reaped(child_process_id);
    }
}

#[cfg(unix)]
#[test]
fn a_header_naming_no_pane_is_taken_back_as_a_session_holding_none() {
    // Taking back an empty header gives a working backend.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: Vec::new(),
    };

    let (pty_backend, pty_size_by_pane_id, _) = take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    )
    .expect("an empty header is taken back");

    assert_eq!(pty_size_by_pane_id.len(), 0, "no pane means no size");
    assert_eq!(
        pty_backend.list_carried_panes().len(),
        0,
        "and no pane to carry on"
    );
    assert_eq!(
        build_carried_pty_sizes(&resume_header).len(),
        0,
        "and no size to record"
    );
}

// On Windows the panes live in a helper process. Taking them back needs the
// secret of the link to that process.
#[cfg(windows)]
#[test]
fn a_resume_run_that_was_passed_no_link_secret_refuses_to_take_its_panes_back() {
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    assert_eq!(
        session_start.supervisor_token, None,
        "no secret was passed on"
    );
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: Vec::new(),
    };

    let resume_error = match take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    ) {
        Err(resume_error) => resume_error,
        Ok(_) => panic!("panes cannot be reached without the link secret"),
    };

    assert_eq!(
        resume_error.to_string(),
        "the secret of the link to the process holding the panes was not passed on, \
         so those panes cannot be reached"
    );
    assert_eq!(
        build_carried_pty_sizes(&resume_header).len(),
        0,
        "and no size to record"
    );
}

// The helper's link address carries its own process id. Taking the panes back
// needs that id.
#[cfg(windows)]
#[test]
fn a_resume_run_that_was_passed_no_helper_process_id_refuses_to_take_its_panes_back() {
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let mut session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    session_start.supervisor_token = Some("a-secret".to_string());
    assert_eq!(
        session_start.supervisor_process_id, None,
        "no process id was passed on"
    );
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: Vec::new(),
    };

    let resume_error = match take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    ) {
        Err(resume_error) => resume_error,
        Ok(_) => panic!("panes cannot be reached without the helper's process id"),
    };

    assert_eq!(
        resume_error.to_string(),
        "the process id of the process holding the panes was not passed on, \
         so those panes cannot be reached"
    );
}

/// A process id that is positive, fits an `i32`, and names no process on any
/// system: process ids are handed out from the low numbers up.
#[cfg(unix)]
const NO_SUCH_PROCESS: u32 = 2_147_483_646;

/// A descriptor number this process never opened, far above every descriptor
/// a test run holds.
#[cfg(unix)]
const NEVER_OPENED_TERMINAL_FILE_DESCRIPTOR: i32 = 1_000_000;

/// How long a test waits for a child of this process to be reaped.
#[cfg(unix)]
const CHILD_WAIT_DURATION: Duration = Duration::from_secs(10);

/// How long that wait pauses between two checks.
#[cfg(unix)]
const CHILD_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(10);

/// Start `program` with `arguments` as a child of this test process that leads
/// its own process group, as a pane's child does, and hand back its process id.
/// The child is never waited for here.
#[cfg(unix)]
fn start_group_leading_child(program: &str, arguments: &[&str]) -> u32 {
    use std::os::unix::process::CommandExt;

    std::process::Command::new(program)
        .args(arguments)
        .process_group(0)
        .spawn()
        .expect("start a child leading its own process group")
        .id()
}

/// Wait until `child_process_id` is reaped: `kill(child_process_id, 0)` answers
/// `ESRCH`, where a zombie still answers `0`.
///
/// # Panics
/// Panics when [`CHILD_WAIT_DURATION`] runs out first, and when the check
/// fails with an error other than `ESRCH`.
#[cfg(unix)]
fn wait_until_child_is_reaped(child_process_id: u32) {
    let child_process_id = libc::pid_t::try_from(child_process_id).expect("a child id fits a pid");
    let reap_deadline = Instant::now() + CHILD_WAIT_DURATION;
    while unsafe { libc::kill(child_process_id, 0) } == 0 {
        assert!(
            Instant::now() < reap_deadline,
            "child {child_process_id} was never reaped"
        );
        std::thread::sleep(CHILD_POLL_INTERVAL_DURATION);
    }
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH),
        "child {child_process_id} is gone, not merely unreachable"
    );
}

#[test]
fn a_child_that_ends_after_the_header_is_built_still_carries_its_real_status() {
    // The panes are read once to build the header and again just before it is
    // written. A child reaped in between gets its exit status on the second
    // read.
    let settled_pane_id = PaneId::new();
    let still_running_pane_id = PaneId::new();
    let already_known_pane_id = PaneId::new();
    let gone_from_backend_pane_id = PaneId::new();
    let closed_before_swap_pane_id = PaneId::new();

    // The refresh pairs records by pane id and never reads the process id.
    // Every record carries the same one.
    const PANE_CHILD_PROCESS_ID: u32 = 4821;

    let build_carried_pane_record = |pane_id, exit_status| koshi_runtime::resume::CarriedPane {
        pane_id,
        process_id: PANE_CHILD_PROCESS_ID,
        row_count: 24,
        column_count: 80,
        terminal_fd: None,
        terminal_name: None,
        exit_status,
    };
    let mut resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: SessionId::new(),
        session_name: "main".to_string(),
        carried_panes: vec![
            build_carried_pane_record(settled_pane_id, None),
            build_carried_pane_record(still_running_pane_id, None),
            build_carried_pane_record(already_known_pane_id, Some(ExitStatus::ExitCode(3))),
            build_carried_pane_record(gone_from_backend_pane_id, None),
        ],
    };

    let build_current_carried_pty_pane = |pane_id, exit_status| CarriedPtyPane {
        pane_id,
        #[cfg(unix)]
        terminal_fd: None,
        process_id: PANE_CHILD_PROCESS_ID,
        pty_size: PtySize {
            row_count: 24,
            column_count: 80,
        },
        exit_status,
    };
    refresh_carried_exits(
        &mut resume_header,
        &[
            build_current_carried_pty_pane(settled_pane_id, Some(ExitStatus::ExitCode(7))),
            build_current_carried_pty_pane(still_running_pane_id, None),
            build_current_carried_pty_pane(already_known_pane_id, Some(ExitStatus::ExitCode(9))),
            build_current_carried_pty_pane(
                closed_before_swap_pane_id,
                Some(ExitStatus::ExitCode(11)),
            ),
        ],
    );

    let get_exit_status = |pane_id| {
        resume_header
            .carried_panes
            .iter()
            .find(|pane_record| pane_record.pane_id == pane_id)
            .expect("the pane is in the header")
            .exit_status
    };
    assert_eq!(
        get_exit_status(settled_pane_id),
        Some(ExitStatus::ExitCode(7))
    );
    assert_eq!(get_exit_status(still_running_pane_id), None);
    // A status the header already carried is not written over.
    assert_eq!(
        get_exit_status(already_known_pane_id),
        Some(ExitStatus::ExitCode(3))
    );
    // The live read walks the panes the backend still holds. A header record
    // that read names no more is left as the header carried it.
    assert_eq!(get_exit_status(gone_from_backend_pane_id), None);
    // The header names which panes the next image takes back. A pane the live
    // read reports but the header never named is left out of it.
    assert_eq!(
        resume_header
            .carried_panes
            .iter()
            .map(|pane_record| pane_record.pane_id)
            .collect::<Vec<PaneId>>(),
        vec![
            settled_pane_id,
            still_running_pane_id,
            already_known_pane_id,
            gone_from_backend_pane_id,
        ]
    );
}

#[cfg(unix)]
#[test]
fn a_pane_naming_a_descriptor_this_process_does_not_hold_is_refused() {
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let pane_id = PaneId::new();
    let carried_pane = koshi_runtime::resume::CarriedPane {
        pane_id,
        process_id: NO_SUCH_PROCESS,
        row_count: 24,
        column_count: 80,
        terminal_fd: Some(NEVER_OPENED_TERMINAL_FILE_DESCRIPTOR),
        terminal_name: None,
        exit_status: None,
    };
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![carried_pane.clone()],
    };
    let pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(mpsc::channel().0),
    )));

    let take_back_error = take_one_pane_back(&pty_backend, &carried_pane)
        .expect_err("a descriptor this process does not hold cannot be taken back");
    let (_, pty_size_by_pane_id, _) = take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    )
    .expect("every path returns Ok");

    // What the number names is read before the descriptor is owned. The
    // refusal names the pane, and the number stays open.
    assert_eq!(
        take_back_error.to_string(),
        format!(
            "pane {pane_id} carried descriptor {NEVER_OPENED_TERMINAL_FILE_DESCRIPTOR}, which names no pseudoterminal \
             master, so it cannot be taken back"
        )
    );
    assert_eq!(pty_size_by_pane_id, HashMap::new());
}

#[cfg(unix)]
#[test]
fn ending_a_carried_child_signals_only_a_running_child_of_this_process() {
    // Process id 0 and every process id past `i32::MAX` are refused before any
    // signal is sent.
    assert_eq!(end_carried_child(0), None, "process id 0 is refused");
    assert_eq!(
        end_carried_child(u32::MAX),
        None,
        "process id 4294967295 is refused"
    );
    assert_eq!(
        end_carried_child(3_000_000_000),
        None,
        "process id 3000000000 is refused"
    );
    assert_eq!(
        end_carried_child(NO_SUCH_PROCESS),
        None,
        "a process id that names no child of this process is left alone"
    );

    let exited_child_process_id = start_group_leading_child("true", &[]);
    wait_until_child_has_exited(exited_child_process_id);
    assert_eq!(
        end_carried_child(exited_child_process_id),
        None,
        "a child that already exited is not signalled"
    );
    assert_eq!(
        unsafe {
            libc::kill(
                libc::pid_t::try_from(exited_child_process_id).expect("a child id fits a pid"),
                0,
            )
        },
        -1,
        "the check itself reaped the exited child"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );

    let running_child_process_id = start_group_leading_child("sleep", &["100"]);
    let ended_child_process_id =
        end_carried_child(running_child_process_id).expect("the running child is ended");
    assert_eq!(
        u32::try_from(ended_child_process_id),
        Ok(running_child_process_id)
    );
    let mut wait_status: libc::c_int = 0;
    assert_eq!(
        unsafe { libc::waitpid(ended_child_process_id, &mut wait_status, 0) },
        ended_child_process_id
    );
    assert_eq!(
        (libc::WIFSIGNALED(wait_status), libc::WTERMSIG(wait_status)),
        (true, libc::SIGKILL),
        "the running child's group was ended with SIGKILL"
    );
}

/// A fresh runtime directory under `/tmp`. Removed when the test drops it.
#[cfg(unix)]
fn build_short_runtime_directory() -> TempDir {
    tempfile::Builder::new()
        .prefix("k")
        .tempdir_in(PathBuf::from("/tmp"))
        .expect("a fresh runtime directory")
}

/// The state a swap carried out of a session holding no pane: a header naming
/// none, under the identity `session_start` was started with, and a body holding no
/// session, no pane state and no quit.
#[cfg(unix)]
fn build_empty_carried_state(session_start: &SessionStart) -> (ResumeHeader, ResumeBody) {
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: Vec::new(),
    };
    let resume_body = ResumeBody {
        session_by_id: HashMap::new(),
        carried_pane_state_by_pane_id: HashMap::new(),
        carried_quit: None,
    };
    (resume_header, resume_body)
}

/// The process id of the child of the one pane `pty_backend` holds.
///
/// # Panics
/// Panics when `pty_backend` holds no pane, or more than one.
#[cfg(unix)]
fn get_only_pane_child_process_id(pty_backend: &PortablePtyBackend) -> u32 {
    let carried_pty_panes = pty_backend.list_carried_panes();
    assert_eq!(carried_pty_panes.len(), 1, "the backend holds one pane");
    carried_pty_panes[0].process_id
}

/// The text of the error an endpoint file write gives when a directory stands
/// at `endpoint_file_path`.
#[cfg(unix)]
fn format_endpoint_directory_error(endpoint_file_path: &Path) -> String {
    format!(
        "endpoint file {endpoint_file} could not be written: storage io error: replace \
         {endpoint_file}: {is_a_directory_error}",
        endpoint_file = endpoint_file_path.display(),
        is_a_directory_error = std::io::Error::from_raw_os_error(libc::EISDIR),
    )
}

#[cfg(unix)]
#[test]
fn a_rebuild_that_cannot_bind_its_socket_leaves_no_resume_file_behind() {
    // The swap wrote the file and then could neither start a new image nor put
    // the session back in this one.
    let runtime_directory_fixture = build_short_runtime_directory();
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let (server, _, runtime_event_sender) = build_test_server();
    let portable_pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender.clone()),
    )));

    // The address the rebuild binds is already held: the rebuild's own bind is
    // refused.
    let held_session_socket =
        bind_session_socket(&session_start, &runtime_event_sender).expect("the address binds once");

    // The file the swap wrote before it withdrew the socket.
    let resume_file_path =
        resolve_resume_file_path(&session_start.runtime_directory, session_start.session_id);
    std::fs::write(&resume_file_path, b"{}").expect("the resume file is written");

    let (resume_header, resume_body) = build_empty_carried_state(&session_start);

    let resume_error = match resume_readers_and_rebuild(
        server,
        &portable_pty_backend,
        &resume_header,
        resume_body,
        &session_start,
        &runtime_event_sender,
    ) {
        Err(resume_error) => resume_error,
        Ok(_) => panic!("an address another server already holds cannot be bound"),
    };

    assert_eq!(
        resume_error.to_string(),
        format!(
            "another process is already listening at {}",
            held_session_socket.get_socket_address()
        )
    );
    assert!(
        !resume_file_path.exists(),
        "the resume file must not be left on the disk"
    );
}

#[cfg(unix)]
#[test]
fn a_rebuild_binds_the_address_again_and_takes_the_resume_file_away() {
    // The caller withdrew the socket for an image that then did not start. The
    // rebuild binds the socket again, writes the endpoint file, and deletes the
    // resume file.
    let runtime_directory_fixture = build_short_runtime_directory();
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let (server, _, runtime_event_sender) = build_test_server();
    let portable_pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender.clone()),
    )));

    // The file the swap wrote before it withdrew the socket.
    let resume_file_path =
        resolve_resume_file_path(&session_start.runtime_directory, session_start.session_id);
    std::fs::write(&resume_file_path, b"{}").expect("the resume file is written");

    let (resume_header, resume_body) = build_empty_carried_state(&session_start);

    let (rebuilt_server, session_socket) = resume_readers_and_rebuild(
        server,
        &portable_pty_backend,
        &resume_header,
        resume_body,
        &session_start,
        &runtime_event_sender,
    )
    .expect("nothing holds the address, so the rebuild binds it");

    let advertised_endpoint =
        EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
            &session_start.runtime_directory,
            session_start.session_id,
        ))
        .expect("the rebound socket is advertised");
    assert_eq!(
        advertised_endpoint.socket_address,
        session_socket.get_socket_address()
    );
    assert_eq!(advertised_endpoint.process_id, std::process::id());
    assert_eq!(
        rebuilt_server.list_sessions().len(),
        0,
        "a body naming no session comes back holding none"
    );
    assert!(
        !resume_file_path.exists(),
        "the resume file must not be left on the disk"
    );
}

#[cfg(unix)]
#[test]
fn a_session_that_keeps_its_socket_serves_the_same_address_under_a_fresh_token() {
    // The rebuild binds the same address under a fresh connection token.
    let runtime_directory_fixture = build_short_runtime_directory();
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let (server, _, runtime_event_sender) = build_test_server();
    let portable_pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender.clone()),
    )));
    let session_socket =
        bind_session_socket(&session_start, &runtime_event_sender).expect("the address binds");
    let endpoint_file_path = EndpointFile::resolve_endpoint_file_path(
        &session_start.runtime_directory,
        session_start.session_id,
    );
    let initial_endpoint_file =
        EndpointFile::load_from_path(&endpoint_file_path).expect("the socket is advertised");

    let resume_file_path =
        resolve_resume_file_path(&session_start.runtime_directory, session_start.session_id);
    std::fs::write(&resume_file_path, b"{}").expect("the resume file is written");

    let (resume_header, resume_body) = build_empty_carried_state(&session_start);

    let (_rebuilt_server, kept_session_socket) = resume_readers_and_keep_socket(
        server,
        session_socket,
        &portable_pty_backend,
        &resume_header,
        resume_body,
        &session_start,
        &runtime_event_sender,
    )
    .expect("the fresh token is advertised");

    let refreshed_endpoint_file =
        EndpointFile::load_from_path(&endpoint_file_path).expect("the socket is still advertised");
    assert_eq!(
        kept_session_socket.get_socket_address(),
        initial_endpoint_file.socket_address,
        "the address does not change"
    );
    assert_eq!(
        refreshed_endpoint_file.socket_address,
        initial_endpoint_file.socket_address
    );
    assert_ne!(
        refreshed_endpoint_file.connection_token, initial_endpoint_file.connection_token,
        "a client that was told watches for a fresh token"
    );
    assert!(
        !resume_file_path.exists(),
        "the resume file must not be left on the disk"
    );
}

#[cfg(unix)]
#[test]
fn a_swap_whose_new_image_never_starts_hands_the_session_back_on_a_rebound_socket() {
    // The whole swap, run over a path with nothing at it: starting the new
    // image is the step that fails. The session comes back in this process with
    // the tabs and panes it had, serving on a socket, and the resume file is
    // gone from the disk.
    let runtime_directory_fixture = build_short_runtime_directory();
    let mut session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    session_start.executable_watch = Arc::new(ExecutableWatch::new(
        runtime_directory_fixture
            .path()
            .join("koshi-that-is-not-there"),
        "1.0.0",
    ));
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let portable_pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender.clone()),
    )));
    let pty_backend: Arc<dyn PtyBackend> = portable_pty_backend.clone();
    let mut server = Server::from_runtime_parts(pty_backend, runtime_event_receiver);
    let shell_config = koshi_config::app_config::parse_app_config(
        Path::new("koshi.kdl"),
        "version 2\nterminal {\n    default-shell \"/bin/sh\"\n}\n",
    )
    .expect("the config parses");
    server.load_startup_config(Some(shell_config.layer));
    seed_test_session(&mut server);
    let session_id = *server
        .list_sessions()
        .keys()
        .next()
        .expect("the session is seeded");
    let expected_tabs = server.list_sessions()[&session_id].tabs.clone();
    let session_socket =
        bind_session_socket(&session_start, &runtime_event_sender).expect("the address binds");
    let resume_file_path =
        resolve_resume_file_path(&session_start.runtime_directory, session_start.session_id);

    let (mut rebuilt_server, rebound_session_socket) = swap_session_image(
        server,
        session_socket,
        &portable_pty_backend,
        &session_start,
        &runtime_event_sender,
    )
    .expect("a swap that could not start puts the session back")
    .expect("the session runs in this process, not another one");

    let rebuilt_tabs = rebuilt_server.list_sessions()[&session_id].tabs.clone();
    rebuilt_server.kill_all_panes();
    assert_eq!(rebuilt_tabs, expected_tabs, "every tab comes back");
    let advertised_endpoint =
        EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
            &session_start.runtime_directory,
            session_start.session_id,
        ))
        .expect("the rebound socket is advertised");
    assert_eq!(
        advertised_endpoint.socket_address,
        rebound_session_socket.get_socket_address()
    );
    assert!(
        !resume_file_path.exists(),
        "the resume file must not be left on the disk"
    );
}

#[cfg(unix)]
#[test]
fn a_swap_starts_no_new_image_before_the_kill_of_a_pane_closed_earlier_ends() {
    // The new image is a path with nothing at it: the session comes back in
    // this process once the held kill has ended.
    let runtime_directory_fixture = build_short_runtime_directory();
    let mut session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    session_start.executable_watch = Arc::new(ExecutableWatch::new(
        runtime_directory_fixture
            .path()
            .join("koshi-that-is-not-there"),
        "1.0.0",
    ));
    assert_swap_waits_for_the_held_kill_of_a_closed_pane(session_start);
}

#[cfg(unix)]
#[test]
fn a_swap_whose_state_cannot_be_written_waits_for_the_kill_of_a_pane_closed_earlier() {
    // A directory stands at the resume file path: the session keeps its socket
    // and comes back in this process once the held kill has ended.
    let runtime_directory_fixture = build_short_runtime_directory();
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    std::fs::create_dir(resolve_resume_file_path(
        &session_start.runtime_directory,
        session_start.session_id,
    ))
    .expect("a directory stands at the resume file path");
    assert_swap_waits_for_the_held_kill_of_a_closed_pane(session_start);
}

/// Close a pane whose kill the fake backend holds at a barrier, then run
/// [`swap_session_image`] with `session_start` on a thread. Checks that the
/// swap comes back in this process only after the held kill ends, and that the
/// closed pane's child was killed once with [`KillPolicy::Force`].
#[cfg(unix)]
fn assert_swap_waits_for_the_held_kill_of_a_closed_pane(session_start: SessionStart) {
    let (mut server, fake_pty_backend, runtime_event_sender) = build_test_server();
    let kill_barrier = Arc::new(std::sync::Barrier::new(2));
    fake_pty_backend.hold_kills_at(kill_barrier.clone());
    seed_test_session(&mut server);
    let client_id = attach_test_client(&mut server).client_id;
    let CommandResult::Ok { emitted_events, .. } =
        server.submit_command(CommandEnvelope::from_parts(
            CommandId::new(),
            CommandSource::from_key_binding(client_id),
            Command::NewPane(NewPaneArgs {
                placement: NewPanePlacement::Split {
                    source_pane_id: None,
                    tab_id: None,
                    direction: Direction::Right,
                },
                working_directory: None,
                spawn_spec: None,
                client_id: None,
            }),
        ))
    else {
        panic!("the split is accepted");
    };
    let closed_pane_id = emitted_events
        .iter()
        .find_map(|emitted_event| match emitted_event {
            Event::PaneCreated(PaneCreated { pane_id, .. }) => Some(*pane_id),
            _ => None,
        })
        .expect("the split creates a pane");
    let _ = server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(closed_pane_id),
            should_force_close: true,
            should_kill_process_tree: false,
        }),
    ));
    let session_socket =
        bind_session_socket(&session_start, &runtime_event_sender).expect("the address binds");
    let pty_owner = Arc::new(PortablePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender.clone()),
    )));

    let (kill_record_sender, kill_record_receiver) = mpsc::channel();
    let swap_fake_pty_backend = fake_pty_backend.clone();
    let swap_thread = std::thread::spawn(move || {
        let (mut rebuilt_server, _rebound_session_socket) = swap_session_image(
            server,
            session_socket,
            &pty_owner,
            &session_start,
            &runtime_event_sender,
        )
        .expect("a swap that could not start puts the session back")
        .expect("the session runs in this process, not another one");
        kill_record_sender
            .send(swap_fake_pty_backend.list_pane_kill_policies(closed_pane_id))
            .expect("the test waits for the swap");
        rebuilt_server.kill_all_panes();
    });

    assert_eq!(
        kill_record_receiver.recv_timeout(Duration::from_millis(200)),
        Err(mpsc::RecvTimeoutError::Timeout),
        "the swap waits for the held kill"
    );
    kill_barrier.wait();
    assert_eq!(
        kill_record_receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the swap ends once the kill ends"),
        Ok(vec![KillPolicy::Force])
    );
    swap_thread.join().expect("the swap thread ends");
}

#[cfg(unix)]
#[test]
fn a_swap_that_can_write_neither_its_state_nor_a_fresh_token_ends_every_pane() {
    // A directory stands at the resume file path and at the endpoint file
    // path. The resume file cannot be written, the session keeps its socket,
    // and the endpoint file cannot advertise the fresh token. The swap returns
    // that failure, and the pane's child is ended and reaped.
    let runtime_directory_fixture = build_short_runtime_directory();
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let portable_pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender.clone()),
    )));
    let pty_backend: Arc<dyn PtyBackend> = portable_pty_backend.clone();
    let mut server = Server::from_runtime_parts(pty_backend, runtime_event_receiver);
    server.load_startup_config(Some(build_plain_shell_config()));
    seed_test_session(&mut server);
    let pane_child_process_id = get_only_pane_child_process_id(&portable_pty_backend);
    let session_socket =
        bind_session_socket(&session_start, &runtime_event_sender).expect("the address binds");
    let endpoint_file_path = EndpointFile::resolve_endpoint_file_path(
        &session_start.runtime_directory,
        session_start.session_id,
    );
    std::fs::remove_file(&endpoint_file_path).expect("the endpoint file is removed");
    std::fs::create_dir(&endpoint_file_path).expect("a directory stands at the endpoint file path");
    std::fs::create_dir(resolve_resume_file_path(
        &session_start.runtime_directory,
        session_start.session_id,
    ))
    .expect("a directory stands at the resume file path");

    let swap_error = match swap_session_image(
        server,
        session_socket,
        &portable_pty_backend,
        &session_start,
        &runtime_event_sender,
    ) {
        Err(swap_error) => swap_error,
        Ok(_) => panic!("a session that cannot advertise its fresh token is not put back"),
    };

    assert_eq!(
        swap_error.to_string(),
        format_endpoint_directory_error(&endpoint_file_path)
    );
    assert_eq!(
        portable_pty_backend.list_carried_panes(),
        Vec::new(),
        "every pane leaves the backend"
    );
    wait_until_child_is_reaped(pane_child_process_id);
}

#[cfg(unix)]
#[test]
fn a_swap_that_can_neither_start_nor_bind_its_socket_again_ends_every_pane() {
    // The new image is a path with nothing at it, and a directory stands at
    // the endpoint file path. The swap withdraws the socket, cannot start the
    // new image, and cannot advertise the socket it binds again. The swap
    // returns that failure, and the pane's child is ended and reaped.
    let runtime_directory_fixture = build_short_runtime_directory();
    let mut session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    session_start.executable_watch = Arc::new(ExecutableWatch::new(
        runtime_directory_fixture
            .path()
            .join("koshi-that-is-not-there"),
        "1.0.0",
    ));
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let portable_pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender.clone()),
    )));
    let pty_backend: Arc<dyn PtyBackend> = portable_pty_backend.clone();
    let mut server = Server::from_runtime_parts(pty_backend, runtime_event_receiver);
    server.load_startup_config(Some(build_plain_shell_config()));
    seed_test_session(&mut server);
    let pane_child_process_id = get_only_pane_child_process_id(&portable_pty_backend);
    let session_socket =
        bind_session_socket(&session_start, &runtime_event_sender).expect("the address binds");
    let endpoint_file_path = EndpointFile::resolve_endpoint_file_path(
        &session_start.runtime_directory,
        session_start.session_id,
    );
    std::fs::remove_file(&endpoint_file_path).expect("the endpoint file is removed");
    std::fs::create_dir(&endpoint_file_path).expect("a directory stands at the endpoint file path");

    let swap_error = match swap_session_image(
        server,
        session_socket,
        &portable_pty_backend,
        &session_start,
        &runtime_event_sender,
    ) {
        Err(swap_error) => swap_error,
        Ok(_) => panic!("a session that cannot advertise its rebound socket is not put back"),
    };

    assert_eq!(
        swap_error.to_string(),
        format_endpoint_directory_error(&endpoint_file_path)
    );
    assert_eq!(
        portable_pty_backend.list_carried_panes(),
        Vec::new(),
        "every pane leaves the backend"
    );
    wait_until_child_is_reaped(pane_child_process_id);
}

#[cfg(unix)]
#[test]
fn a_first_run_that_cannot_bind_its_socket_ends_every_pane() {
    // The first run seeds one shell. A directory stands at the endpoint file
    // path, and the endpoint file cannot be written. The start returns that
    // failure, and the shell is ended and reaped.
    let runtime_directory_fixture = build_short_runtime_directory();
    let mut session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let (_session_server, pty_owner) = seed_initial_session(
        &mut session_start,
        None,
        Some(build_plain_shell_config()),
        Arc::new(InboxSink::from_event_sender(runtime_event_sender.clone())),
        runtime_event_receiver,
    )
    .expect("the first run seeds one shell");
    let pane_child_process_id = get_only_pane_child_process_id(&pty_owner);
    let endpoint_file_path = EndpointFile::resolve_endpoint_file_path(
        &session_start.runtime_directory,
        session_start.session_id,
    );
    std::fs::create_dir(&endpoint_file_path).expect("a directory stands at the endpoint file path");

    let start_error = match start_serving_session(
        &pty_owner,
        &session_start,
        false,
        &runtime_event_sender,
        &mut std::io::sink(),
    ) {
        Err(start_error) => start_error,
        Ok(_) => panic!("a socket whose endpoint file cannot be written does not serve"),
    };

    assert_eq!(
        start_error.to_string(),
        format_endpoint_directory_error(&endpoint_file_path)
    );
    assert_eq!(
        pty_owner.list_carried_panes(),
        Vec::new(),
        "every pane leaves the backend"
    );
    wait_until_child_is_reaped(pane_child_process_id);
}

#[cfg(unix)]
#[test]
fn a_resume_run_with_a_fresh_shell_that_cannot_bind_its_socket_ends_every_pane() {
    // A resume run whose carried state does not read seeds one fresh shell. A
    // directory stands at the endpoint file path, and the endpoint file cannot
    // be written. The start returns that failure, and the shell is ended and
    // reaped.
    let runtime_directory_fixture = build_short_runtime_directory();
    let mut session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let (_session_server, pty_owner) = seed_session_after_failed_restore(
        &mut session_start,
        Some(build_plain_shell_config()),
        Arc::new(InboxSink::from_event_sender(runtime_event_sender.clone())),
        runtime_event_receiver,
    )
    .expect("the resume run seeds one fresh shell");
    let pane_child_process_id = get_only_pane_child_process_id(&pty_owner);
    let endpoint_file_path = EndpointFile::resolve_endpoint_file_path(
        &session_start.runtime_directory,
        session_start.session_id,
    );
    std::fs::create_dir(&endpoint_file_path).expect("a directory stands at the endpoint file path");

    let start_error = match start_serving_session(
        &pty_owner,
        &session_start,
        true,
        &runtime_event_sender,
        &mut std::io::sink(),
    ) {
        Err(start_error) => start_error,
        Ok(_) => panic!("a socket whose endpoint file cannot be written does not serve"),
    };

    assert_eq!(
        start_error.to_string(),
        format_endpoint_directory_error(&endpoint_file_path)
    );
    assert_eq!(
        pty_owner.list_carried_panes(),
        Vec::new(),
        "every pane leaves the backend"
    );
    wait_until_child_is_reaped(pane_child_process_id);
}

#[cfg(unix)]
#[test]
fn a_resume_run_with_a_carried_pane_that_cannot_bind_its_socket_ends_every_pane() {
    // A resume run takes back one pane: a pseudoterminal master and a running
    // `/bin/sh` child that leads its own process group. A directory stands at
    // the endpoint file path, and the endpoint file cannot be written. The
    // start returns that failure, and the carried child is ended and reaped.
    let runtime_directory_fixture = build_short_runtime_directory();
    let mut session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let terminal_master_file_descriptor =
        unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(
        terminal_master_file_descriptor >= 0,
        "the pseudoterminal master opens"
    );
    let carried_terminal_name = find_terminal_master_name(terminal_master_file_descriptor)
        .expect("the master names its terminal");
    let carried_child_process_id = start_group_leading_child("/bin/sh", &["-c", "sleep 30"]);
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![koshi_runtime::resume::CarriedPane {
            pane_id: PaneId::new(),
            process_id: carried_child_process_id,
            row_count: 24,
            column_count: 80,
            terminal_fd: Some(terminal_master_file_descriptor),
            terminal_name: Some(carried_terminal_name),
            exit_status: None,
        }],
    };
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let (_session_server, pty_owner) = build_from_carried_state(
        &resume_header,
        Err(StorageError::Corrupt {
            detail: "the carried body is not JSON".to_string(),
        }),
        &mut session_start,
        Some(build_plain_shell_config()),
        Arc::new(InboxSink::from_event_sender(runtime_event_sender.clone())),
        runtime_event_receiver,
        &runtime_event_sender,
    )
    .expect("the carried pane is taken back");
    assert_eq!(
        get_only_pane_child_process_id(&pty_owner),
        carried_child_process_id
    );
    let endpoint_file_path = EndpointFile::resolve_endpoint_file_path(
        &session_start.runtime_directory,
        session_start.session_id,
    );
    std::fs::create_dir(&endpoint_file_path).expect("a directory stands at the endpoint file path");

    let start_error = match start_serving_session(
        &pty_owner,
        &session_start,
        true,
        &runtime_event_sender,
        &mut std::io::sink(),
    ) {
        Err(start_error) => start_error,
        Ok(_) => panic!("a socket whose endpoint file cannot be written does not serve"),
    };

    assert_eq!(
        start_error.to_string(),
        format_endpoint_directory_error(&endpoint_file_path)
    );
    assert_eq!(
        pty_owner.list_carried_panes(),
        Vec::new(),
        "every pane leaves the backend"
    );
    wait_until_child_is_reaped(carried_child_process_id);
}

#[cfg(unix)]
#[test]
fn a_first_run_that_cannot_print_its_ready_line_ends_every_pane() {
    // The first run seeds one shell and binds its control socket. The ready
    // line goes to a file opened for reading only, and the write fails. The
    // start returns that failure, and the shell is ended and reaped.
    let runtime_directory_fixture = build_short_runtime_directory();
    let mut session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let (_session_server, pty_owner) = seed_initial_session(
        &mut session_start,
        None,
        Some(build_plain_shell_config()),
        Arc::new(InboxSink::from_event_sender(runtime_event_sender.clone())),
        runtime_event_receiver,
    )
    .expect("the first run seeds one shell");
    let pane_child_process_id = get_only_pane_child_process_id(&pty_owner);
    let mut read_only_output =
        std::fs::File::open("/dev/null").expect("the null device opens for reading");

    let start_error = match start_serving_session(
        &pty_owner,
        &session_start,
        false,
        &runtime_event_sender,
        &mut read_only_output,
    ) {
        Err(start_error) => start_error,
        Ok(_) => panic!("a first run whose ready line cannot be written does not start"),
    };

    assert_eq!(
        start_error.to_string(),
        std::io::Error::from_raw_os_error(libc::EBADF).to_string()
    );
    assert_eq!(
        pty_owner.list_carried_panes(),
        Vec::new(),
        "every pane leaves the backend"
    );
    wait_until_child_is_reaped(pane_child_process_id);
}

#[cfg(unix)]
#[test]
fn carried_panes_in_conflict_leave_a_descriptor_that_is_no_terminal_master_open() {
    use std::io::Read;
    use std::os::fd::AsRawFd;

    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let log_file_path = runtime_directory_fixture.path().join("pane.log");
    std::fs::write(&log_file_path, b"koshi").expect("the ordinary file is written");
    let mut ordinary_file = std::fs::File::open(&log_file_path).expect("open an ordinary file");
    let log_file_descriptor = ordinary_file.as_raw_fd();
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let build_carried_pane_record = || koshi_runtime::resume::CarriedPane {
        pane_id: PaneId::new(),
        // Process id 0 names no pane child: ending these panes signals
        // nothing.
        process_id: 0,
        row_count: 24,
        column_count: 80,
        terminal_fd: Some(log_file_descriptor),
        terminal_name: None,
        exit_status: None,
    };
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![build_carried_pane_record(), build_carried_pane_record()],
    };

    let (_, pty_size_by_pane_id, _) = take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    )
    .expect("every path returns Ok");

    assert_eq!(
        pty_size_by_pane_id,
        HashMap::new(),
        "neither pane comes back"
    );
    assert!(
        unsafe { libc::fcntl(log_file_descriptor, libc::F_GETFD) } >= 0,
        "the ordinary file stays open"
    );
    let mut log_file_contents = String::new();
    ordinary_file
        .read_to_string(&mut log_file_contents)
        .expect("the ordinary file still reads");
    assert_eq!(log_file_contents, "koshi");
}

#[cfg(unix)]
#[test]
fn a_pane_that_cannot_be_taken_back_leaves_every_other_pane_taken_back() {
    use std::os::fd::AsRawFd;

    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let ordinary_file = std::fs::File::open("/dev/null").expect("open an ordinary file");
    let ordinary_file_descriptor = ordinary_file.as_raw_fd();
    let terminal_master_file_descriptor =
        unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(
        terminal_master_file_descriptor >= 0,
        "the pseudoterminal master opens"
    );
    let carried_terminal_name = find_terminal_master_name(terminal_master_file_descriptor)
        .expect("the master names its terminal");
    let refused_pane_id = PaneId::new();
    let taken_back_pane_id = PaneId::new();
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![
            koshi_runtime::resume::CarriedPane {
                pane_id: refused_pane_id,
                process_id: 0,
                row_count: 24,
                column_count: 80,
                terminal_fd: Some(ordinary_file_descriptor),
                terminal_name: None,
                exit_status: None,
            },
            koshi_runtime::resume::CarriedPane {
                pane_id: taken_back_pane_id,
                process_id: NO_SUCH_PROCESS,
                row_count: 30,
                column_count: 100,
                terminal_fd: Some(terminal_master_file_descriptor),
                terminal_name: Some(carried_terminal_name),
                exit_status: None,
            },
        ],
    };

    let (pty_backend, pty_size_by_pane_id, _) = take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    )
    .expect("every path returns Ok");

    assert_eq!(
        pty_size_by_pane_id,
        HashMap::from([(
            taken_back_pane_id,
            PtySize {
                row_count: 30,
                column_count: 100,
            },
        )])
    );
    assert_eq!(
        pty_backend
            .list_carried_panes()
            .iter()
            .map(|carried_pty_pane| carried_pty_pane.pane_id)
            .collect::<Vec<PaneId>>(),
        vec![taken_back_pane_id]
    );
    assert!(
        unsafe { libc::fcntl(ordinary_file_descriptor, libc::F_GETFD) } >= 0,
        "the refused pane's descriptor is left open"
    );
    // The backend owns the master from here, and closing the pane closes it.
    pty_backend
        .kill_pane(taken_back_pane_id, KillPolicy::Tree)
        .expect("the pane closes");
}

#[cfg(unix)]
#[test]
fn a_pane_whose_terminal_name_changed_is_refused_and_its_master_closes() {
    // The number names a live pseudoterminal master of another pane. The kind
    // check accepts it; the recorded name refuses it.
    let pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(mpsc::channel().0),
    )));

    let terminal_master_file_descriptor =
        unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(
        terminal_master_file_descriptor >= 0,
        "the pseudoterminal master opens"
    );
    let carried_terminal_name = find_terminal_master_name(terminal_master_file_descriptor)
        .expect("the master names its terminal");
    let pane_id = PaneId::new();
    let carried_pane = koshi_runtime::resume::CarriedPane {
        pane_id,
        process_id: NO_SUCH_PROCESS,
        row_count: 24,
        column_count: 80,
        terminal_fd: Some(terminal_master_file_descriptor),
        terminal_name: Some("/dev/koshi-another-pane".to_string()),
        exit_status: None,
    };

    let resume_error = take_one_pane_back(&pty_backend, &carried_pane)
        .expect_err("a descriptor whose terminal is not the recorded one must be refused");

    assert_eq!(
        resume_error.to_string(),
        format!(
            "pane {pane_id} carried descriptor {terminal_master_file_descriptor} as the master of \
             /dev/koshi-another-pane, which is now the master of {carried_terminal_name}, so it cannot be \
             taken back"
        )
    );
    assert_eq!(
        unsafe { libc::fcntl(terminal_master_file_descriptor, libc::F_GETFD) },
        -1,
        "the refused terminal master is closed"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EBADF)
    );
}

#[cfg(unix)]
#[test]
fn a_pane_whose_terminal_is_the_one_the_header_recorded_is_taken_back() {
    // The control for the refusal above: the same call over the same kind of
    // descriptor, with the name the header recorded still matching, must take
    // the pane back.
    let pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(mpsc::channel().0),
    )));

    let terminal_master_file_descriptor =
        unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(
        terminal_master_file_descriptor >= 0,
        "the pseudoterminal master opens"
    );
    let carried_terminal_name = find_terminal_master_name(terminal_master_file_descriptor)
        .expect("the master names its terminal");
    let pane_id = PaneId::new();
    let carried_pane = koshi_runtime::resume::CarriedPane {
        pane_id,
        process_id: NO_SUCH_PROCESS,
        row_count: 24,
        column_count: 80,
        terminal_fd: Some(terminal_master_file_descriptor),
        terminal_name: Some(carried_terminal_name),
        exit_status: None,
    };

    take_one_pane_back(&pty_backend, &carried_pane).expect("the pane is taken back");

    assert_eq!(
        pty_backend
            .list_carried_panes()
            .iter()
            .map(|carried_pty_pane| carried_pty_pane.pane_id)
            .collect::<Vec<PaneId>>(),
        vec![pane_id]
    );
    // The backend owns the descriptor from here, and closing the pane closes
    // it.
    pty_backend
        .kill_pane(pane_id, KillPolicy::Tree)
        .expect("the pane closes");
}

#[cfg(unix)]
#[test]
fn two_carried_panes_naming_one_descriptor_are_neither_taken_back_and_it_is_closed_once() {
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let terminal_master_file_descriptor =
        unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(
        terminal_master_file_descriptor >= 0,
        "the pseudoterminal master opens"
    );
    let carried_terminal_name = find_terminal_master_name(terminal_master_file_descriptor)
        .expect("the master names its terminal");
    // A second number on the same pseudoterminal, held to the end of the test,
    // keeps that terminal allocated to this test.
    let held_terminal_file_descriptor = unsafe { libc::dup(terminal_master_file_descriptor) };
    assert!(
        held_terminal_file_descriptor >= 0,
        "the master opens under a second number"
    );
    // Two process ids: the descriptor is the only thing the two panes share.
    let build_carried_pane_record = |process_id| koshi_runtime::resume::CarriedPane {
        pane_id: PaneId::new(),
        process_id,
        row_count: 24,
        column_count: 80,
        terminal_fd: Some(terminal_master_file_descriptor),
        terminal_name: Some(carried_terminal_name.clone()),
        exit_status: None,
    };
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![
            build_carried_pane_record(NO_SUCH_PROCESS),
            build_carried_pane_record(NO_SUCH_PROCESS - 1),
        ],
    };

    let (pty_backend, pty_size_by_pane_id, _) = take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    )
    .expect("every path returns Ok");

    assert_eq!(
        pty_size_by_pane_id,
        HashMap::new(),
        "neither pane comes back"
    );
    assert_eq!(pty_backend.list_carried_panes(), Vec::new());
    assert_ne!(
        find_terminal_master_name(terminal_master_file_descriptor),
        Some(carried_terminal_name),
        "the shared descriptor is closed"
    );
    assert_eq!(
        unsafe { libc::close(held_terminal_file_descriptor) },
        0,
        "the test still owns the second number it opened"
    );
}

/// One carried record for `pane_id`, naming no terminal descriptor. Enough for
/// the checks that read the header alone.
fn build_carried_pane_record(pane_id: PaneId) -> koshi_runtime::resume::CarriedPane {
    koshi_runtime::resume::CarriedPane {
        pane_id,
        process_id: 0,
        row_count: 24,
        column_count: 80,
        terminal_fd: None,
        terminal_name: None,
        exit_status: None,
    }
}

/// A header naming `carried_panes`, in that order.
fn build_resume_header(carried_panes: Vec<koshi_runtime::resume::CarriedPane>) -> ResumeHeader {
    ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: SessionId::new(),
        session_name: "S-quiet-lake".to_string(),
        carried_panes,
    }
}

#[test]
fn every_record_of_a_pane_the_header_names_twice_is_in_conflict() {
    let first_pane_id = PaneId::new();
    let repeated_pane_id = PaneId::new();
    let last_pane_id = PaneId::new();

    assert_eq!(
        find_conflicting_carried_pane_indexes(&build_resume_header(vec![
            build_carried_pane_record(first_pane_id),
            build_carried_pane_record(repeated_pane_id),
            build_carried_pane_record(last_pane_id),
        ])),
        HashSet::new(),
        "three panes named once each"
    );
    assert_eq!(
        find_conflicting_carried_pane_indexes(&build_resume_header(vec![
            build_carried_pane_record(first_pane_id),
            build_carried_pane_record(repeated_pane_id),
            build_carried_pane_record(last_pane_id),
            build_carried_pane_record(repeated_pane_id),
        ])),
        HashSet::from([1, 3])
    );
}

#[test]
fn a_header_naming_no_pane_has_no_conflict() {
    assert_eq!(
        find_conflicting_carried_pane_indexes(&build_resume_header(Vec::new())),
        HashSet::new()
    );
}

#[cfg(unix)]
#[test]
fn two_carried_records_naming_one_pane_are_neither_taken_back_and_both_terminals_close() {
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let first_terminal_master_file_descriptor =
        unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    let second_terminal_master_file_descriptor =
        unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(
        first_terminal_master_file_descriptor >= 0,
        "the first pseudoterminal master opens"
    );
    assert!(
        second_terminal_master_file_descriptor >= 0,
        "the second pseudoterminal master opens"
    );
    let first_terminal_name = find_terminal_master_name(first_terminal_master_file_descriptor)
        .expect("the first master names its terminal");
    let second_terminal_name = find_terminal_master_name(second_terminal_master_file_descriptor)
        .expect("the second master names its terminal");
    // A second number on each pseudoterminal, held to the end of the test,
    // keeps both terminals allocated to this test.
    let first_held_terminal_file_descriptor =
        unsafe { libc::dup(first_terminal_master_file_descriptor) };
    let second_held_terminal_file_descriptor =
        unsafe { libc::dup(second_terminal_master_file_descriptor) };
    assert!(first_held_terminal_file_descriptor >= 0);
    assert!(second_held_terminal_file_descriptor >= 0);
    let repeated_pane_id = PaneId::new();
    let build_carried_pane_with_terminal =
        |terminal_file_descriptor, process_id| koshi_runtime::resume::CarriedPane {
            pane_id: repeated_pane_id,
            process_id,
            row_count: 24,
            column_count: 80,
            terminal_fd: Some(terminal_file_descriptor),
            terminal_name: find_terminal_master_name(terminal_file_descriptor),
            exit_status: None,
        };
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![
            build_carried_pane_with_terminal(
                first_terminal_master_file_descriptor,
                NO_SUCH_PROCESS,
            ),
            build_carried_pane_with_terminal(
                second_terminal_master_file_descriptor,
                NO_SUCH_PROCESS - 1,
            ),
        ],
    };

    let (pty_backend, pty_size_by_pane_id, _) = take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    )
    .expect("every path returns Ok");

    assert_eq!(
        pty_size_by_pane_id,
        HashMap::new(),
        "the pane does not come back"
    );
    assert_eq!(pty_backend.list_carried_panes(), Vec::new());
    assert_ne!(
        find_terminal_master_name(first_terminal_master_file_descriptor),
        Some(first_terminal_name),
        "the first terminal is closed"
    );
    assert_ne!(
        find_terminal_master_name(second_terminal_master_file_descriptor),
        Some(second_terminal_name),
        "the second terminal is closed"
    );
    assert_eq!(
        unsafe { libc::close(first_held_terminal_file_descriptor) },
        0
    );
    assert_eq!(
        unsafe { libc::close(second_held_terminal_file_descriptor) },
        0
    );
}

#[cfg(unix)]
#[test]
fn a_binary_printing_its_answer_with_no_newline_after_it_is_still_read() {
    // The binary writes its answer and exits without a newline. The stream
    // ending closes the line.
    use std::os::unix::fs::PermissionsExt as _;

    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let executable_path = runtime_directory_fixture.path().join("koshi");
    std::fs::write(
        &executable_path,
        format!("#!/bin/sh\nprintf '{{\"minimum_resume_format\":{RESUME_FORMAT_MIN},\"maximum_resume_format\":{RESUME_FORMAT}}}'\n"),
    )
    .expect("the stand-in binary is written");
    std::fs::set_permissions(&executable_path, std::fs::Permissions::from_mode(0o755))
        .expect("the stand-in binary is runnable");

    assert_eq!(
        read_resume_support(&executable_path),
        Ok(ResumeSupport {
            minimum_resume_format: RESUME_FORMAT_MIN,
            maximum_resume_format: RESUME_FORMAT
        })
    );
}

#[cfg(unix)]
#[test]
fn a_binary_that_says_nothing_is_refused_once_the_wait_runs_out() {
    // A binary that starts, prints nothing, and keeps running. `exec` makes the
    // sleeping process the one this call spawned: ending it ends the sleep.
    use std::os::unix::fs::PermissionsExt as _;

    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let executable_path = runtime_directory_fixture.path().join("koshi");
    std::fs::write(&executable_path, "#!/bin/sh\nexec sleep 60\n")
        .expect("the stand-in binary is written");
    std::fs::set_permissions(&executable_path, std::fs::Permissions::from_mode(0o755))
        .expect("the stand-in binary is runnable");

    // A start refused with `ETXTBSY` (some process holds the file open for
    // writing) is tried again until it starts. Every attempt that starts spends
    // the whole wait.
    let probe_deadline = Instant::now() + BUSY_PROGRAM_WAIT_DURATION;
    let (resume_support_result, wait_duration) = loop {
        let attempt_started_at = Instant::now();
        let resume_support_result = read_resume_support(&executable_path);
        let attempt_duration = attempt_started_at.elapsed();
        let is_executable_busy = resume_support_result
            .as_ref()
            .err()
            .is_some_and(|resume_error| resume_error.contains("could not be run"));
        if !is_executable_busy {
            break (resume_support_result, attempt_duration);
        }
        assert!(
            Instant::now() < probe_deadline,
            "the stand-in binary never became runnable: {resume_support_result:?}"
        );
        std::thread::sleep(BUSY_PROGRAM_RETRY_INTERVAL_DURATION);
    };

    assert_eq!(
        resume_support_result,
        Err(format!(
            "the binary at {} did not say which resume formats it reads within {} seconds",
            executable_path.display(),
            RESUME_SUPPORT_WAIT_DURATION.as_secs()
        ))
    );
    // The whole wait ran out before the refusal.
    assert!(
        wait_duration >= RESUME_SUPPORT_WAIT_DURATION,
        "the refusal must come after the whole wait, and it came after {wait_duration:?}"
    );
}

#[test]
fn a_binary_naming_a_lowest_format_above_its_highest_is_refused() {
    // An empty range: nothing this build writes is inside it.
    let executable_path = Path::new("/opt/koshi/bin/koshi");

    assert_eq!(
        reads_the_format_this_build_writes(
            ResumeSupport {
                minimum_resume_format: 7,
                maximum_resume_format: 2
            },
            executable_path
        ),
        Err(format!(
            "the binary at {} reads resume formats 7 to 2, and this one reads {RESUME_FORMAT_MIN} \
             to {RESUME_FORMAT} and writes {RESUME_FORMAT}",
            executable_path.display()
        ))
    );
}

#[test]
fn a_restart_accepted_in_the_pass_that_loses_the_last_pane_ends_the_session() {
    // The loop's no-panes check runs before the restart check.
    let (mut server, _, runtime_event_sender) = build_test_server();
    seed_test_session(&mut server);
    server.set_restart_check(Arc::new(|| Ok(())));
    let pane_id = *server
        .list_terminal_engines()
        .keys()
        .next()
        .expect("the root pane holds a terminal engine");

    let restart_response_receiver = request_session_restart(&runtime_event_sender);
    runtime_event_sender
        .send(RuntimeEvent::ChildExit {
            pane_id,
            exit_status: ExitStatus::ExitCode(0),
        })
        .expect("the child's exit is queued");

    let serve_outcome = run_session_serve_loop(&mut server);

    assert_eq!(
        restart_response_receiver
            .try_recv()
            .expect("the loop answered the request"),
        Ok(())
    );
    assert!(server.is_restart_requested());
    assert!(!server.has_active_panes());
    assert_eq!(
        serve_outcome,
        ServeOutcome::Ended,
        "a session with no pane left ends rather than replacing its image"
    );
}

#[test]
fn a_quit_arriving_with_a_restart_in_one_pass_ends_the_session_instead_of_swapping() {
    // Both requests reach the inbox before the loop reads either. The quit
    // decides.
    let (mut server, _, runtime_event_sender) = build_test_server();
    let session_id = SessionId::new();
    server
        .bootstrap_session(
            session_id,
            "quiet-lake".to_string(),
            STARTING_VIEWPORT,
            SystemTime::now(),
            None,
        )
        .expect("the session is seeded");
    server.set_restart_check(Arc::new(|| Ok(())));

    let restart_response_receiver = request_session_restart(&runtime_event_sender);
    runtime_event_sender
        .send(RuntimeEvent::Ipc {
            command_envelope: Box::new(CommandEnvelope::from_parts(
                CommandId::new(),
                CommandSource::ExternalCli {
                    session_id: Some(session_id),
                    target_client_id: None,
                },
                Command::Quit,
            )),
            response_sender: mpsc::channel().0,
        })
        .expect("the quit command is queued");

    let serve_outcome = run_session_serve_loop(&mut server);

    assert_eq!(
        restart_response_receiver
            .try_recv()
            .expect("the loop answered the request"),
        Ok(())
    );
    assert!(server.is_quit_requested());
    assert!(server.is_restart_requested());
    assert_eq!(serve_outcome, ServeOutcome::Ended);
}

#[test]
fn two_restart_requests_in_one_pass_are_both_answered_and_the_loop_swaps_once() {
    // Two `koshi update` runs can reach one session before its loop reads
    // either. Each caller is answered, and the loop leaves for exactly one
    // swap.
    let (mut server, _, runtime_event_sender) = build_test_server();
    seed_test_session(&mut server);
    server.set_restart_check(Arc::new(|| Ok(())));

    let first_restart_response_receiver = request_session_restart(&runtime_event_sender);
    let second_restart_response_receiver = request_session_restart(&runtime_event_sender);

    let serve_outcome = run_session_serve_loop(&mut server);

    assert_eq!(
        first_restart_response_receiver
            .try_recv()
            .expect("the loop answered the first"),
        Ok(())
    );
    assert_eq!(
        second_restart_response_receiver
            .try_recv()
            .expect("the loop answered the second"),
        Ok(())
    );
    assert_eq!(serve_outcome, ServeOutcome::Restart);
    assert!(server.is_restart_requested());
    assert!(!server.is_quit_requested());
}

#[test]
fn every_pane_carrying_a_shared_process_id_is_in_conflict() {
    let mut first_carried_pane = build_carried_pane_record(PaneId::new());
    first_carried_pane.process_id = 4821;
    let mut second_carried_pane = build_carried_pane_record(PaneId::new());
    second_carried_pane.process_id = 4821;
    let mut third_carried_pane = build_carried_pane_record(PaneId::new());
    third_carried_pane.process_id = 5000;

    assert_eq!(
        find_conflicting_carried_pane_indexes(&build_resume_header(vec![
            first_carried_pane,
            second_carried_pane,
            third_carried_pane,
        ])),
        HashSet::from([0, 1])
    );
}

#[test]
fn every_pane_carrying_a_shared_descriptor_is_in_conflict() {
    let mut first_carried_pane = build_carried_pane_record(PaneId::new());
    first_carried_pane.terminal_fd = Some(7);
    let mut second_carried_pane = build_carried_pane_record(PaneId::new());
    second_carried_pane.terminal_fd = Some(9);
    let mut third_carried_pane = build_carried_pane_record(PaneId::new());
    third_carried_pane.terminal_fd = Some(7);

    assert_eq!(
        find_conflicting_carried_pane_indexes(&build_resume_header(vec![
            first_carried_pane,
            second_carried_pane,
            third_carried_pane,
        ])),
        HashSet::from([0, 2])
    );
}

#[test]
fn panes_carrying_process_id_zero_are_not_in_conflict() {
    assert_eq!(
        find_conflicting_carried_pane_indexes(&build_resume_header(vec![
            build_carried_pane_record(PaneId::new()),
            build_carried_pane_record(PaneId::new()),
        ])),
        HashSet::new()
    );
}

/// Every row of `pane_id`'s screen on `session_server` joined into one line,
/// with the trailing blank cells of the last row dropped. A line that wrapped
/// across rows reads back whole.
#[cfg(unix)]
fn get_joined_screen_text(session_server: &Server, pane_id: PaneId) -> String {
    let terminal_state = session_server.list_terminal_engines()[&pane_id].get_terminal_state();
    let (row_count, column_count) = terminal_state.get_active_grid().get_grid_dimensions();
    let mut screen_text = String::new();
    for row_index in 0..row_count {
        for column_index in 0..column_count {
            screen_text.push(
                terminal_state
                    .get_active_grid()
                    .get_cell(row_index, column_index)
                    .map_or(' ', |cell| cell.get_character()),
            );
        }
    }
    screen_text.trim_end().to_owned()
}

/// The `koshi.kdl` layer that makes `/bin/sh` the shell a fresh pane runs.
#[cfg(unix)]
fn build_plain_shell_config() -> PartialKoshiConfig {
    koshi_config::app_config::parse_app_config(
        Path::new("koshi.kdl"),
        "version 2\nterminal {\n    default-shell \"/bin/sh\"\n}\n",
    )
    .expect("the config parses")
    .layer
}

#[cfg(unix)]
#[test]
fn a_body_that_does_not_read_brings_each_carried_pane_back_in_a_tab_of_its_own() {
    let runtime_directory_fixture = build_short_runtime_directory();
    let mut session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let terminal_master_file_descriptor =
        unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(
        terminal_master_file_descriptor >= 0,
        "the pseudoterminal master opens"
    );
    let carried_terminal_name = find_terminal_master_name(terminal_master_file_descriptor)
        .expect("the master names its terminal");
    let carried_pane_id = PaneId::new();
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT + 1,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![koshi_runtime::resume::CarriedPane {
            pane_id: carried_pane_id,
            process_id: NO_SUCH_PROCESS,
            row_count: 24,
            column_count: 200,
            terminal_fd: Some(terminal_master_file_descriptor),
            terminal_name: Some(carried_terminal_name),
            exit_status: None,
        }],
    };
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();

    let (mut session_server, _pty_owner) = build_from_carried_state(
        &resume_header,
        Err(StorageError::Corrupt {
            detail: format!(
                "resume body format {} is outside the {RESUME_FORMAT_MIN} to {RESUME_FORMAT} range this build reads",
                RESUME_FORMAT + 1
            ),
        }),
        &mut session_start,
        Some(build_plain_shell_config()),
        Arc::new(InboxSink::from_event_sender(runtime_event_sender.clone())),
        runtime_event_receiver,
        &runtime_event_sender,
    )
    .expect("the session comes back");

    let restored_session = &session_server.list_sessions()[&session_start.session_id];
    let tab_pane_ids: Vec<Vec<PaneId>> = restored_session
        .tabs
        .values()
        .map(|tab| tab.get_layout_tree().list_leaf_pane_ids())
        .collect();
    let screen_text = get_joined_screen_text(&session_server, carried_pane_id);
    session_server.kill_all_panes();
    assert_eq!(tab_pane_ids, vec![vec![carried_pane_id]]);
    assert_eq!(
        screen_text,
        "[koshi] The session's layout could not be restored after the restart. Each pane now has its own tab, and the program in it is still running."
    );
}

#[cfg(unix)]
#[test]
fn a_carried_state_that_brings_no_pane_back_comes_back_as_one_fresh_shell_showing_the_notice() {
    let runtime_directory_fixture = build_short_runtime_directory();
    let mut session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let (resume_header, resume_body) = build_empty_carried_state(&session_start);
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();

    let (mut session_server, _pty_owner) = build_from_carried_state(
        &resume_header,
        Ok(resume_body),
        &mut session_start,
        Some(build_plain_shell_config()),
        Arc::new(InboxSink::from_event_sender(runtime_event_sender.clone())),
        runtime_event_receiver,
        &runtime_event_sender,
    )
    .expect("the session comes back");

    let restored_session = &session_server.list_sessions()[&session_start.session_id];
    let fresh_pane_ids: Vec<PaneId> = restored_session
        .panes
        .list_pane_records()
        .map(|pane_record| pane_record.get_pane_id())
        .collect();
    assert_eq!(fresh_pane_ids.len(), 1, "one fresh shell");
    assert_eq!(restored_session.session_name, session_start.session_name);
    let screen_text = get_joined_screen_text(&session_server, fresh_pane_ids[0]);
    session_server.handle_pty_output(fresh_pane_ids[0], b"\x1b[2J\x1b[H");
    let attached_client = attach_test_client(&mut session_server);
    assert!(
        session_server
            .build_snapshot(attached_client.client_id)
            .expect("the new shell has a frame")
            .is_recovery_notice_visible
    );
    assert_eq!(
        get_joined_screen_text(&session_server, fresh_pane_ids[0]),
        ""
    );
    session_server.kill_all_panes();
    assert_eq!(
        screen_text,
        "[koshi] The session could not be restored after the restart. This is a new shell; the previous panes are unavailable."
    );
}

#[cfg(unix)]
#[test]
fn a_last_carried_pane_exit_keeps_a_new_stream_live_and_a_carried_quit_ending() {
    for (carried_quit, expected_ending) in [
        (None, None),
        (
            Some(koshi_runtime::resume::CarriedQuit::Graceful),
            Some(SessionEnding::Quit),
        ),
    ] {
        let runtime_directory_fixture = build_short_runtime_directory();
        let mut session_start = build_test_session_start(runtime_directory_fixture.path(), false);
        let (mut previous_server, _, _) = build_test_server();
        previous_server
            .bootstrap_session(
                session_start.session_id,
                session_start.session_name.clone(),
                STARTING_VIEWPORT,
                SystemTime::now(),
                None,
            )
            .expect("the previous session has one pane");
        let previous_client_id = attach_test_client(&mut previous_server).client_id;
        let (_, mut resume_body) = previous_server
            .carry_out(&[])
            .expect("the previous session can be carried");
        resume_body.carried_quit = carried_quit;
        let (resume_header, _) = build_empty_carried_state(&session_start);
        let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();

        let (mut session_server, _pty_owner) = build_from_carried_state(
            &resume_header,
            Ok(resume_body),
            &mut session_start,
            Some(build_plain_shell_config()),
            Arc::new(InboxSink::from_event_sender(runtime_event_sender.clone())),
            runtime_event_receiver,
            &runtime_event_sender,
        )
        .expect("the session opens a new shell");

        let attached_client = attach_test_client(&mut session_server);
        assert_eq!(attached_client.session_id, session_start.session_id);
        assert_ne!(attached_client.client_id, previous_client_id);
        assert_eq!(
            attached_client.ending_notice.get_session_ending(),
            expected_ending
        );
        assert_eq!(session_server.is_quit_requested(), carried_quit.is_some());
        assert_eq!(session_server.is_awaiting_client(), carried_quit.is_some());
        assert_eq!(
            session_server.list_sessions()[&session_start.session_id]
                .panes
                .list_pane_records()
                .count(),
            1
        );
        session_server.kill_all_panes();
    }
}
