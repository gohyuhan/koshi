//! Tests for the session-server loop, driven headlessly: a fake PTY backend
//! stands in for real children, so the real inbox loop runs without spawning a
//! pane. The tests that rebuild a session bind a real control socket inside a
//! runtime directory created for the test. Printing the ready line needs a whole process,
//! so it is covered by the integration tests instead.
//!
//! The image swap is split the same way. What the session server decides — when
//! the loop ends into a swap, what a restart is refused for, which arguments the
//! new image is started with, what the carried state restores, and when the
//! router leaves a resuming session alone — is pinned here, over pseudoterminal
//! masters this test binary opens itself. Replacing the process image needs a
//! real install, so it is covered by the integration tests.

use super::*;

use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, CopyArgs, CopyTarget, GridPosition,
    Selection, SelectionKind, SetSelectionArgs, VisualCommand, WriteToPaneArgs,
};
use koshi_core::ids::{ClientId, CommandId};
use koshi_core::process::ExitStatus;
#[cfg(unix)]
use koshi_ipc::endpoint::EndpointFile;
use koshi_renderer::snapshot::Delivery;
use koshi_runtime::runtime::bus::EventFilter;
use koshi_runtime::runtime::event::AttachAccepted;
use koshi_test_support::fake_pty::FakePtyBackend;
use tempfile::TempDir;

/// A server built the way [`run_session_server`] builds it, on `fake_pty_backend` instead
/// of real children, plus a sender clone so a test can queue inbox events the
/// way the control socket does.
fn build_test_server(
    fake_pty_backend: Arc<FakePtyBackend>,
) -> (Server, mpsc::Sender<RuntimeEvent>) {
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend;
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let mut server = Server::from_runtime_parts(
        pty_backend,
        runtime_event_receiver,
        runtime_event_sender.clone(),
    );
    server.load_startup_config(None);
    (server, runtime_event_sender)
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
        executable_path: PathBuf::from("/opt/koshi/bin/koshi"),
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
) -> mpsc::Receiver<Result<(), String>> {
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
            envelope: CommandEnvelope::from_parts(
                CommandId::new(),
                CommandSource::from_key_binding(client_id),
                SystemTime::now(),
                command,
            ),
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
        event_filter: EventFilter::All,
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
    // The id and the name are picked outside this process and handed to it at
    // startup, so a session that generated either one itself would answer a
    // lookup under a name no caller asked for.
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (mut server, runtime_event_sender) = build_test_server(fake_pty_backend);
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
    // Ending a session is a command forwarded over its control socket, so the
    // loop must both apply it and stop on it — a loop that only applied it
    // would leave the process running with its panes killed.
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (mut server, runtime_event_sender) = build_test_server(fake_pty_backend);
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
            envelope: CommandEnvelope::from_parts(
                command_id,
                CommandSource::ExternalCli {
                    session_id: Some(session_id),
                    target_client_id: None,
                },
                SystemTime::now(),
                Command::Quit,
            ),
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
    // Nothing queues a quit here and the sender stays alive, so the only way
    // out is the loop's own no-panes check. A loop missing it would leave the
    // process alive on an empty session, blocked on an inbox nobody feeds.
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (mut server, runtime_event_sender) = build_test_server(fake_pty_backend);
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
    // This process paints nothing, so the loop pushing the frame is the only
    // way a client ever sees its session change: a loop that applied the child
    // output without pushing would leave the client on the picture it joined
    // on, with the shell's "hello" never drawn.
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (mut server, runtime_event_sender) = build_test_server(fake_pty_backend);
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
    // The push rides the render clock. An ungated one would build and queue a
    // frame on every pass, so a session nothing changed in — one woken only by
    // a discovery query — would keep filling its clients' queues.
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (mut server, _runtime_event_sender) = build_test_server(fake_pty_backend);
    seed_test_session(&mut server);
    let attached_client = attach_test_client(&mut server);
    // Spend the render the seeding and the attach made due, so the pass below
    // starts with nothing pending.
    assert!(server.poll_render(Instant::now()));

    _runtime_event_sender
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
    // The reply is written while the socket is still up and the swap runs after
    // the loop ends. A loop that answered the request and kept serving would
    // leave the caller told the session restarted while it never did.
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (mut server, runtime_event_sender) = build_test_server(fake_pty_backend);
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
    // The reply is the session's only chance to refuse: after it, the swap
    // runs. A path with nothing at it must not reach the swap, and the session
    // must still answer everything else afterwards.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let missing_executable_path = runtime_directory_fixture.path().join("koshi");
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (mut server, runtime_event_sender) = build_test_server(fake_pty_backend);
    seed_test_session(&mut server);
    // The first thing the installed check runs, on a path with nothing at it.
    let named_executable_path = missing_executable_path.clone();
    server.set_restart_check(Arc::new(move || is_binary_runnable(&named_executable_path)));

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
        format!(
            "the binary at {} could not be read: {unreadable_metadata_error}",
            missing_executable_path.display()
        )
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
    // built, and the first thing that check runs is `is_binary_runnable`. A
    // session wired to a check that never ran would tear itself down for a
    // binary that cannot be started.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let missing_executable_path = runtime_directory_fixture.path().join("koshi");
    let (mut server, runtime_event_sender) = build_test_server(Arc::new(FakePtyBackend::new()));
    seed_test_session(&mut server);
    let portable_pty_backend = Arc::new(PortablePtyBackend::new());

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
        Err(format!(
            "the binary at {} could not be read: {unreadable_metadata_error}",
            missing_executable_path.display()
        ))
    );
    assert!(!server.is_restart_requested());
    assert_eq!(serve_outcome, ServeOutcome::Ended);
}

#[test]
fn a_copy_applied_while_the_swap_runs_reaches_the_clients_own_terminal() {
    // The swap applies what the socket queued after the serve loop returned, so
    // it is the last thing that can hand those bytes over. A pass that only
    // applied the copy would carry nothing across and destroy the escape with
    // the image: the system clipboard would keep its old contents, and the
    // client is told nothing either way.
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (mut server, runtime_event_sender) = build_test_server(fake_pty_backend);
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
            clipboard_target: CopyTarget::Osc52,
            should_trim_trailing_whitespace: true,
        })),
    );

    apply_queued_runtime_events(&mut server, DetachPolicy::Apply);

    // base64("hello") = aGVsbG8=
    let host_write_bytes: Vec<Vec<u8>> = attached_client
        .deliveries
        .try_iter()
        .filter_map(|delivery| match delivery {
            Delivery::HostWrite(bytes) => Some(bytes),
            _ => None,
        })
        .collect();
    assert_eq!(host_write_bytes, vec![b"\x1b]52;c;aGVsbG8=\x07".to_vec()]);
}

#[test]
fn a_quit_applied_while_the_swap_runs_ends_the_session_instead_of_serving_it_again() {
    // `koshi kill-session` arriving inside the swap window is applied there, by
    // the same pass, after the serve loop returned. A loop that went back to
    // waiting on the inbox would keep serving the session the user asked to
    // end, and the swap would carry it into the new image alive.
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (mut server, runtime_event_sender) = build_test_server(fake_pty_backend);
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
            envelope: CommandEnvelope::from_parts(
                CommandId::new(),
                CommandSource::ExternalCli {
                    session_id: Some(session_id),
                    target_client_id: None,
                },
                SystemTime::now(),
                Command::Quit,
            ),
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
    // Every abandon path hands the same server back to the serve loop. A server
    // that kept the accepted restart would leave that loop and run the swap that
    // just failed again, on every pass.
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (mut server, runtime_event_sender) = build_test_server(fake_pty_backend);
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
    let portable_pty_backend = Arc::new(PortablePtyBackend::new());

    let restored_server = restore_session_serving(server, &portable_pty_backend);

    assert!(!restored_server.is_restart_requested());
    assert!(!restored_server.is_quit_requested());
}

#[test]
fn a_client_that_hung_up_before_the_swap_told_anyone_is_detached() {
    // The inbox passes before the announce run while every client is still
    // streaming, so a detach they drain is a client that closed its terminal. A
    // pass that dropped it would leave that record attached on every abandon
    // path: the tab would stay clamped to the size of a terminal that is gone,
    // and `auto-close-session` would never see the session empty.
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (mut server, runtime_event_sender) = build_test_server(fake_pty_backend);
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
    // Both halves of a told client's connection queue a detach once it reads the
    // restart frame. Applying those would carry a session holding no client into
    // the new image, and every client would come back a stranger, with fresh
    // focus, zoom, scroll offset and selection.
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (mut server, runtime_event_sender) = build_test_server(fake_pty_backend);
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
    // The frame reaches the client over its socket and the client answers on
    // that same socket, so the line is still crossing the wire when the announce
    // returns. `IpcServer::close_intake` puts what crossed in the inbox before
    // this pass runs; a pass that dropped it would destroy the line with the
    // image, and the child would sit waiting for input the user has already
    // typed.
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (mut server, runtime_event_sender) = build_test_server(Arc::clone(&fake_pty_backend));
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
            input_bytes: vec![b'\r'],
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
    // The session server writes the state to a file and the image that replaces
    // it reads that file back. A round trip that lost a tab, a pane record or a
    // pane's screen would come back as a session the user does not recognise.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let resume_file_path = runtime_directory_fixture.path().join("session.resume");
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (mut server, _runtime_event_sender) = build_test_server(Arc::clone(&fake_pty_backend));
    let session_id = SessionId::new();
    let client_id = server
        .bootstrap_local_named(
            session_id,
            "quiet-lake".to_string(),
            STARTING_VIEWPORT,
            SystemTime::UNIX_EPOCH,
        )
        .expect("the session is seeded");
    let open_second_tab = CommandId::new();
    let envelope = CommandEnvelope::from_parts(
        open_second_tab,
        CommandSource::from_key_binding(client_id),
        SystemTime::UNIX_EPOCH,
        Command::NewTab(koshi_core::command::NewTabArgs {
            working_directory: None,
            client_id: Some(client_id),
        }),
    );
    match server.submit_command(envelope) {
        CommandResult::Ok { command_id, .. } => assert_eq!(command_id, open_second_tab),
        rejected => panic!("the second tab must open, got {rejected:?}"),
    }
    // Distinct output per pane, so a screen that came back under the wrong pane
    // is caught.
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
    let pty_handle_by_pane_id: HashMap<PaneId, PtyHandle> = read_resume_header
        .carried_panes
        .iter()
        .map(|pane_record| {
            (
                pane_record.pane_id,
                PtyHandle::from_detached_pane_id(pane_record.pane_id),
            )
        })
        .collect();
    let (resumed_event_sender, resumed_event_receiver) = mpsc::channel();
    let resumed_server = Server::resume(
        Arc::clone(&fake_pty_backend) as Arc<dyn PtyBackend>,
        resumed_event_receiver,
        resumed_event_sender,
        decoded_resume_body,
        pty_handle_by_pane_id,
        build_carried_pty_sizes(&read_resume_header),
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
    // descriptors under numbers of the same shape. Taking one of those back
    // would drive an ordinary file as a pane's terminal and close it when the
    // pane ends.
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
            // No pane child to end: a process id of zero names none, so the
            // cleanup this failure runs signals nothing.
            process_id: 0,
            row_count: 24,
            column_count: 80,
            terminal_fd: Some(terminal_file_descriptor),
            terminal_name: None,
            exit_status: None,
        }],
    };

    let resume_error = take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    )
    .err()
    .expect("a descriptor that is no terminal master must be refused");

    assert_eq!(
        resume_error.to_string(),
        format!(
            "pane {pane_id} carried descriptor {terminal_file_descriptor}, which names no pseudoterminal master, \
             so it cannot be taken back"
        )
    );
    assert!(
        unsafe { libc::fcntl(terminal_file_descriptor, libc::F_GETFD) } >= 0,
        "the refused descriptor must be left open"
    );
}

#[test]
#[cfg(unix)]
fn carried_panes_that_cannot_be_read_are_ended_and_their_terminals_closed() {
    // The header stays readable when the body does not, and it is all that is
    // left to clean up with. A path that skipped it would leave every pane's
    // child running with nobody reading its terminal.
    use std::os::unix::process::{CommandExt, ExitStatusExt};

    let mut child_command = std::process::Command::new("/bin/sh");
    child_command
        .arg("-c")
        .arg("sleep 30")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // A pane's child leads its own process group, which is what the whole
    // group being ended means; `portable-pty` puts it there with the same call.
    unsafe {
        child_command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child_process = child_command.spawn().expect("the child starts");
    // A real pseudoterminal master stands in for the pane's terminal, since
    // only a master is closed. The terminal it is paired with is what says the
    // descriptor is gone afterwards, whatever number this process hands out
    // next.
    let terminal_master_file_descriptor =
        unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(
        terminal_master_file_descriptor >= 0,
        "the pseudoterminal master opens"
    );
    let carried_terminal_name = find_terminal_master_name(terminal_master_file_descriptor)
        .expect("the master names its terminal");
    // A second number on the same pseudoterminal, held to the end of the test.
    // The terminal stays allocated while it is open, so no other test can be
    // handed this terminal again under the number released below.
    let held_terminal_file_descriptor = unsafe { libc::dup(terminal_master_file_descriptor) };
    assert!(
        held_terminal_file_descriptor >= 0,
        "the master opens under a second number"
    );
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let resume_header = ResumeHeader {
        resume_format: 1,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![koshi_runtime::resume::CarriedPane {
            pane_id: PaneId::new(),
            process_id: child_process.id(),
            row_count: 24,
            column_count: 80,
            terminal_fd: Some(terminal_master_file_descriptor),
            terminal_name: Some(carried_terminal_name.clone()),
            exit_status: None,
        }],
    };

    release_carried_panes(
        &resume_header,
        &session_start,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
    );

    let child_exit_status = child_process.wait().expect("the child is reaped");
    assert_eq!(child_exit_status.signal(), Some(libc::SIGKILL));
    assert_ne!(
        find_terminal_master_name(terminal_master_file_descriptor),
        Some(carried_terminal_name),
        "the terminal must be closed"
    );
    assert_eq!(
        unsafe { libc::close(held_terminal_file_descriptor) },
        0,
        "the test still owns the second number it opened"
    );
}

#[cfg(unix)]
#[test]
fn ending_the_panes_after_a_failure_leaves_a_number_an_earlier_pane_holds_open() {
    // A header naming one descriptor for two panes, with the duplicate after
    // `untouched`: the backend holds the number for the first pane, so closing
    // it here would close it twice.
    let shared_terminal_file_descriptor =
        unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(
        shared_terminal_file_descriptor >= 0,
        "the pseudoterminal master opens"
    );
    let shared_terminal_name = find_terminal_master_name(shared_terminal_file_descriptor)
        .expect("the master names its terminal");
    let build_carried_pane_record = |terminal_file_descriptor| koshi_runtime::resume::CarriedPane {
        pane_id: PaneId::new(),
        process_id: 0,
        row_count: 24,
        column_count: 80,
        terminal_fd: Some(terminal_file_descriptor),
        terminal_name: None,
        exit_status: None,
    };
    let resume_header = build_resume_header(vec![
        build_carried_pane_record(shared_terminal_file_descriptor),
        build_carried_pane_record(shared_terminal_file_descriptor),
    ]);

    end_panes_after_failure(&resume_header, 1);

    assert_eq!(
        find_terminal_master_name(shared_terminal_file_descriptor),
        Some(shared_terminal_name),
        "the number the first pane was taken back on stays open"
    );
    assert_eq!(
        unsafe { libc::close(shared_terminal_file_descriptor) },
        0,
        "the test still owns the master left open"
    );
}

#[cfg(unix)]
#[test]
fn ending_the_panes_after_a_failure_closes_each_number_once() {
    // Two untouched panes naming one descriptor: the number is closed once,
    // and the second pane's turn is a no-op rather than a second close.
    let shared_terminal_file_descriptor =
        unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(
        shared_terminal_file_descriptor >= 0,
        "the pseudoterminal master opens"
    );
    let shared_terminal_name = find_terminal_master_name(shared_terminal_file_descriptor)
        .expect("the master names its terminal");
    // A second number on the same terminal, held to the end of the test, so no
    // other test can be handed this terminal under the number closed below.
    let held_terminal_file_descriptor = unsafe { libc::dup(shared_terminal_file_descriptor) };
    assert!(
        held_terminal_file_descriptor >= 0,
        "the master opens under a second number"
    );
    let build_carried_pane_record = |terminal_file_descriptor| koshi_runtime::resume::CarriedPane {
        pane_id: PaneId::new(),
        process_id: 0,
        row_count: 24,
        column_count: 80,
        terminal_fd: Some(terminal_file_descriptor),
        terminal_name: None,
        exit_status: None,
    };
    let resume_header = build_resume_header(vec![
        build_carried_pane_record(shared_terminal_file_descriptor),
        build_carried_pane_record(shared_terminal_file_descriptor),
    ]);

    end_panes_after_failure(&resume_header, 0);

    assert_ne!(
        find_terminal_master_name(shared_terminal_file_descriptor),
        Some(shared_terminal_name),
        "the number nobody took over is closed"
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
    // The flag is cleared so the descriptor crosses the swap. A swap that never
    // ran and left it cleared would hand the next pane's child a hold on this
    // pane's terminal.
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
    // Clearing the flags runs over plain numbers the carried state holds,
    // before anything about the image changes. A number this process does not
    // hold comes back as a failure, which is what leaves the session serving
    // here instead of replacing its image.
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

    // A record naming no descriptor is passed over, so a header holding one
    // never stops the swap.
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
    // The profile opened this session's tabs and panes once. A resume run that
    // ran it again would come up with the profile's panes beside the carried
    // ones.
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
    // `--allow-other-users` is the only input to the socket's reach, so a
    // resume run without it would rebind the socket where the other users of
    // this machine can no longer see it.
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
    // The one line is the whole answer, so a build that printed something else
    // must be refused rather than read as a range that happens to parse.
    assert_eq!(
        parse_resume_support("{\"min\":1,\"max\":3}").expect("the line reads"),
        ResumeSupport {
            minimum_resume_format: 1,
            maximum_resume_format: 3
        }
    );
    assert_eq!(
        serde_json::to_string(&ResumeSupport::from_current_build()).expect("the range encodes"),
        format!("{{\"min\":{RESUME_FORMAT_MIN},\"max\":{RESUME_FORMAT}}}")
    );
    assert_eq!(
        parse_resume_support("koshi 0.2.0").expect_err("a version line is not a range"),
        "does not say which resume formats it reads: expected value at line 1 column 1"
    );
}

#[test]
fn a_binary_reading_no_format_this_one_writes_is_refused_naming_both_ranges() {
    // This is the only check that cannot be made again after the swap: the
    // install already replaced the old binary on disk, so an image that cannot
    // read the carried state cannot be put back.
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

    // The real case behind the refusal: a koshi that reads formats 1 through
    // 2 is an older build, and this one writes a body with format 3.
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

/// A runnable stand-in for the newly installed binary: a script at `path` that
/// prints `line` and exits, whatever it is asked. Unix only — it leans on the
/// shebang line, and Windows names a runnable file by its extension instead.
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
    // The probe runs the binary, so a download that arrived broken or built for
    // another machine is caught here rather than after the swap has started.
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
            "{{\"min\":{RESUME_FORMAT_MIN},\"max\":{}}}",
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
    // A binary that answers with an empty line said nothing at all. Reading it
    // as a range would let the swap start into an image that cannot take the
    // carried state back.
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
            "{{\"min\":{unsupported_minimum_resume_format},\"max\":{}}}",
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
    // before the call, so a swap that did not start has to put the ignore back.
    // Without it the next write to a client that hung up would end the session.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let mut session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    session_start.executable_path = runtime_directory_fixture
        .path()
        .join("koshi-that-is-not-there");
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
    // A Unix pane is taken back by its descriptor and nothing else, so a record
    // carrying none names a pane this image cannot drive.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let stranded = PaneId::new();
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![koshi_runtime::resume::CarriedPane {
            pane_id: stranded,
            process_id: std::process::id(),
            row_count: 24,
            column_count: 80,
            terminal_fd: None,
            terminal_name: None,
            exit_status: None,
        }],
    };

    let resume_error = match take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    ) {
        Err(resume_error) => resume_error,
        Ok(_) => panic!("a pane with no descriptor cannot be taken back"),
    };

    assert_eq!(
        resume_error.to_string(),
        format!("pane {stranded} carried no terminal descriptor, so it cannot be taken back")
    );
}

#[cfg(unix)]
#[test]
fn a_header_naming_no_pane_is_taken_back_as_a_session_holding_none() {
    // A swap runs whatever the session holds, including nothing. Taking back an
    // empty header must give a working backend rather than fail.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: Vec::new(),
    };

    let (pty_backend, pty_handles) = take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    )
    .expect("an empty header is taken back");

    assert_eq!(pty_handles.len(), 0, "no pane means no handle");
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

// On Windows the panes live in a helper process, so taking them back means
// reaching that process; the secret of the link to it is the one thing the new
// image cannot do without.
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

// The helper's link address carries its own process id, so a resume run
// without that id cannot name the process holding the panes either.
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

#[test]
fn a_session_with_a_fresh_resume_file_is_left_alone_and_a_stale_one_is_not() {
    // During a swap the session's socket is unbound, so every way the router
    // notices a session is gone notices this one too. Without the guard a
    // `koshi list-sessions` running at that moment deletes the endpoint file
    // the resuming session is about to rewrite.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_id = SessionId::new();
    assert!(
        !is_replacing_its_image(runtime_directory_fixture.path(), session_id),
        "a session with no resume file is not replacing its image"
    );

    let resume_file_path = resolve_resume_file_path(runtime_directory_fixture.path(), session_id);
    std::fs::write(&resume_file_path, b"{}").expect("the resume file is written");
    assert!(
        is_replacing_its_image(runtime_directory_fixture.path(), session_id),
        "a resume file written just now means a swap is in flight"
    );

    let stale_resume_file = std::fs::File::options()
        .write(true)
        .open(&resume_file_path)
        .expect("the resume file opens for writing");
    stale_resume_file
        .set_modified(SystemTime::now() - RESTART_WINDOW_DURATION - Duration::from_secs(1))
        .expect("the resume file is aged");
    assert!(
        !is_replacing_its_image(runtime_directory_fixture.path(), session_id),
        "a resume file older than the window means the swap died"
    );
}

/// A process id that is positive, fits an `i32`, and names no process on any
/// system: process ids are handed out from the low numbers up.
#[cfg(unix)]
const NO_SUCH_PROCESS: u32 = 2_147_483_646;

/// A descriptor number this process never opened. It sits far above every
/// descriptor a test run holds, so nothing takes it while the tests run.
#[cfg(unix)]
const NEVER_OPENED_TERMINAL_FILE_DESCRIPTOR: i32 = 1_000_000;

/// How long a test keeps trying to run a file the operating system reports as
/// held open for writing.
#[cfg(unix)]
const BUSY_WAIT_DURATION: Duration = Duration::from_secs(20);

/// How long a test pauses between those attempts.
#[cfg(unix)]
const BUSY_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(20);

#[test]
fn a_child_that_ends_after_the_header_is_built_still_carries_its_real_status() {
    // The panes are read once to build the header and again just before it is
    // written. A child reaped in between is known only to this image, so its
    // status has to reach the header on that second read or the next image
    // waits on a process id nobody can answer for.
    let settled_pane_id = PaneId::new();
    let still_running_pane_id = PaneId::new();
    let already_known_pane_id = PaneId::new();
    let gone_from_backend_pane_id = PaneId::new();
    let closed_before_swap_pane_id = PaneId::new();

    // The refresh pairs records by pane id, so the process id below is never
    // read. Every record carries the same one.
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
    // A status the header already carried is the one this image reaped first,
    // so the subsequent read never writes over it.
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
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![koshi_runtime::resume::CarriedPane {
            pane_id,
            process_id: NO_SUCH_PROCESS,
            row_count: 24,
            column_count: 80,
            terminal_fd: Some(NEVER_OPENED_TERMINAL_FILE_DESCRIPTOR),
            terminal_name: None,
            exit_status: None,
        }],
    };

    let resume_error = match take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    ) {
        Err(resume_error) => resume_error,
        Ok(_) => panic!("a descriptor this process does not hold cannot be taken back"),
    };

    // What the number names is read before the descriptor is owned, so the
    // refusal names the pane rather than this process closing a number it never
    // opened.
    assert_eq!(
        resume_error.to_string(),
        format!(
            "pane {pane_id} carried descriptor {NEVER_OPENED_TERMINAL_FILE_DESCRIPTOR}, which names no pseudoterminal \
             master, so it cannot be taken back"
        )
    );
}

#[cfg(unix)]
#[test]
fn a_carried_header_naming_no_real_pane_child_ends_nothing() {
    // `killpg` reads its argument signed: 0 names this process's own group, and
    // anything past `i32::MAX` becomes -1, which names every process this one
    // may signal. Both are refused before the signal is sent.
    assert!(
        !end_carried_child(0),
        "process id 0 names this process's own group"
    );
    assert!(
        !end_carried_child(u32::MAX),
        "a process id past i32::MAX becomes -1"
    );
    assert!(
        !end_carried_child(3_000_000_000),
        "and so does any other process id that does not fit"
    );
    assert!(
        end_carried_child(NO_SUCH_PROCESS),
        "a process id that could name a pane child is signalled"
    );
}

/// A fresh directory to stand in for the runtime directory, under a short base so the
/// Unix socket path bound inside it stays within the operating system's
/// path-length cap. Removed when the test drops it.
#[cfg(unix)]
fn build_short_runtime_directory() -> TempDir {
    tempfile::Builder::new()
        .prefix("k")
        .tempdir_in(PathBuf::from("/tmp"))
        .expect("a fresh runtime directory")
}

/// The state a swap carried out of a session holding no pane: a header naming
/// none, under the identity `session_start` was started with, and a body holding no
/// session, no engine, no undecoded bytes and no quit.
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
        terminal_state_by_pane_id: HashMap::new(),
        undecoded_bytes_by_pane_id: HashMap::new(),
        graphics_undecoded_bytes_by_pane_id: HashMap::new(),
        graphics_screen_continuation_by_pane_id: HashMap::new(),
        graphics_screen_wrapper_active_by_pane_id: HashMap::new(),
        graphics_tmux_continuation_by_pane_id: HashMap::new(),
        graphics_tmux_wrapper_active_by_pane_id: HashMap::new(),
        graphics_events_by_pane_id: HashMap::new(),
        graphics_transport_by_pane_id: HashMap::new(),
        synchronized_output_by_pane_id: HashMap::new(),
        carried_quit: None,
    };
    (resume_header, resume_body)
}

#[cfg(unix)]
#[test]
fn a_rebuild_that_cannot_bind_its_socket_leaves_no_resume_file_behind() {
    // The swap wrote the file and then could neither start a new image nor put
    // this one back in this one. Nothing reads that file again, so it must not
    // stay on the disk.
    let runtime_directory_fixture = build_short_runtime_directory();
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let (server, runtime_event_sender) = build_test_server(Arc::new(FakePtyBackend::new()));
    let portable_pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender.clone()),
    )));

    // The address the rebuild must bind, already held, so the rebuild's own
    // bind is refused.
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
    // The caller withdrew the socket for an image that then did not start, so
    // the rebuild is what puts the session back on the air. The endpoint file
    // is what a client reads to find it, and the resume file has done its work.
    let runtime_directory_fixture = build_short_runtime_directory();
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let (server, runtime_event_sender) = build_test_server(Arc::new(FakePtyBackend::new()));
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
    // Every client was told the session is restarting, and a fresh connection
    // token is what each of them watches for. The address does not change, so a
    // client that was told comes back to the socket it left.
    let runtime_directory_fixture = build_short_runtime_directory();
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let (server, runtime_event_sender) = build_test_server(Arc::new(FakePtyBackend::new()));
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
    // The whole swap, run over a path with nothing at it, so starting the new
    // image is the step that fails. The session must come back in this process
    // with the tabs and panes it had, serving on a socket a client can find,
    // and the carried state must be gone from the disk.
    let runtime_directory_fixture = build_short_runtime_directory();
    let mut session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    session_start.executable_path = runtime_directory_fixture
        .path()
        .join("koshi-that-is-not-there");
    let (mut server, runtime_event_sender) = build_test_server(Arc::new(FakePtyBackend::new()));
    seed_test_session(&mut server);
    let session_id = *server
        .list_sessions()
        .keys()
        .next()
        .expect("the session is seeded");
    let expected_tabs = server.list_sessions()[&session_id].tabs.clone();
    let portable_pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender.clone()),
    )));
    let session_socket =
        bind_session_socket(&session_start, &runtime_event_sender).expect("the address binds");
    let resume_file_path =
        resolve_resume_file_path(&session_start.runtime_directory, session_start.session_id);

    let (rebuilt_server, rebound_session_socket) = swap_session_image(
        server,
        session_socket,
        &portable_pty_backend,
        &session_start,
        &runtime_event_sender,
    )
    .expect("a swap that could not start puts the session back")
    .expect("the session runs in this process, not another one");

    assert_eq!(
        rebuilt_server.list_sessions()[&session_id].tabs,
        expected_tabs,
        "every tab comes back"
    );
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
fn releasing_carried_panes_leaves_a_descriptor_that_is_no_terminal_master_open() {
    // Both release paths take a plain number out of a header this process did
    // not write. Closing it unchecked would close a log file or a socket this
    // process holds for something else, under a number of the same shape.
    use std::io::Read;
    use std::os::fd::AsRawFd;

    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let log_file_path = runtime_directory_fixture.path().join("pane.log");
    std::fs::write(&log_file_path, b"koshi").expect("the ordinary file is written");
    let mut ordinary_file = std::fs::File::open(&log_file_path).expect("open an ordinary file");
    let log_file_descriptor = ordinary_file.as_raw_fd();
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![koshi_runtime::resume::CarriedPane {
            pane_id: PaneId::new(),
            // A process id of zero names no pane child, so neither path
            // signals anything.
            process_id: 0,
            row_count: 24,
            column_count: 80,
            terminal_fd: Some(log_file_descriptor),
            terminal_name: None,
            exit_status: None,
        }],
    };

    release_carried_panes(
        &resume_header,
        &session_start,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
    );
    assert!(
        unsafe { libc::fcntl(log_file_descriptor, libc::F_GETFD) } >= 0,
        "releasing the carried panes must leave the ordinary file open"
    );

    end_panes_after_failure(&resume_header, 0);
    assert!(
        unsafe { libc::fcntl(log_file_descriptor, libc::F_GETFD) } >= 0,
        "ending the panes after a failure must leave the ordinary file open"
    );

    // The number still reaches the same file, so neither path closed it and
    // handed the number to something else.
    let mut log_file_contents = String::new();
    ordinary_file
        .read_to_string(&mut log_file_contents)
        .expect("the ordinary file still reads");
    assert_eq!(log_file_contents, "koshi");
}

#[cfg(unix)]
#[test]
fn ending_the_panes_after_a_failure_closes_the_terminals_from_the_untouched_pane_on() {
    // `untouched` names the first pane this process never took over. Every pane
    // before it belongs to the backend, which closes its terminal as it is
    // dropped, so closing those here would close each of them twice.
    let taken_back_terminal_file_descriptor =
        unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    let never_taken_terminal_file_descriptor =
        unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(
        taken_back_terminal_file_descriptor >= 0,
        "the first pseudoterminal master opens"
    );
    assert!(
        never_taken_terminal_file_descriptor >= 0,
        "the second pseudoterminal master opens"
    );
    let taken_back_terminal_name = find_terminal_master_name(taken_back_terminal_file_descriptor)
        .expect("the first master names its terminal");
    let never_taken_terminal_name = find_terminal_master_name(never_taken_terminal_file_descriptor)
        .expect("the second master names its terminal");
    // A second number on the second pseudoterminal, held to the end of the
    // test. The terminal stays allocated while it is open, so no other test can
    // be handed this terminal again under the number closed below.
    let held_terminal_file_descriptor = unsafe { libc::dup(never_taken_terminal_file_descriptor) };
    assert!(
        held_terminal_file_descriptor >= 0,
        "the second master opens under a second number"
    );
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    // A process id of zero names no pane child, so nothing is signalled.
    let build_carried_pane_record = |terminal_file_descriptor| koshi_runtime::resume::CarriedPane {
        pane_id: PaneId::new(),
        process_id: 0,
        row_count: 24,
        column_count: 80,
        terminal_fd: Some(terminal_file_descriptor),
        terminal_name: None,
        exit_status: None,
    };
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: session_start.session_id,
        session_name: session_start.session_name.clone(),
        carried_panes: vec![
            build_carried_pane_record(taken_back_terminal_file_descriptor),
            build_carried_pane_record(never_taken_terminal_file_descriptor),
        ],
    };

    end_panes_after_failure(&resume_header, 1);

    assert_eq!(
        find_terminal_master_name(taken_back_terminal_file_descriptor),
        Some(taken_back_terminal_name),
        "the terminal of a pane the backend already holds stays open"
    );
    assert_ne!(
        find_terminal_master_name(never_taken_terminal_file_descriptor),
        Some(never_taken_terminal_name),
        "the terminal of a pane nobody took over is closed"
    );
    assert_eq!(
        unsafe { libc::close(taken_back_terminal_file_descriptor) },
        0,
        "the test still owns the master left open"
    );
    assert_eq!(
        unsafe { libc::close(held_terminal_file_descriptor) },
        0,
        "the test still owns the second number it opened"
    );
}

#[cfg(unix)]
#[test]
fn a_pane_whose_terminal_is_not_the_one_the_header_recorded_is_refused() {
    // A number can name a live pseudoterminal master that belongs to another
    // pane, which the kind check alone accepts. The recorded name is what tells
    // this pane's own master from any other.
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
        find_terminal_master_name(terminal_master_file_descriptor),
        Some(carried_terminal_name),
        "the refused descriptor must be left open"
    );
    assert_eq!(
        unsafe { libc::close(terminal_master_file_descriptor) },
        0,
        "the test still owns the master it opened"
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

    let pty_handle =
        take_one_pane_back(&pty_backend, &carried_pane).expect("the pane is taken back");

    assert_eq!(pty_handle.get_pane_id(), pane_id);
    // The backend owns the descriptor from here, and closing the pane closes
    // it.
    pty_backend
        .kill_pane(pane_id, KillPolicy::Tree)
        .expect("the pane closes");
}

#[cfg(unix)]
#[test]
fn two_carried_panes_naming_one_descriptor_are_refused_rather_than_owned_twice() {
    // Every other thing a resume file can say about a descriptor is read before
    // the descriptor is owned. This one is not: both records name a real
    // pseudoterminal master under the name the header recorded, so both checks
    // pass and the same number is wrapped in an owned descriptor twice. The two
    // owners close it twice, and the second close reaches whatever number the
    // process opened in between.
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
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    // Two process ids, so the descriptor is the only thing the two panes
    // share: one id for two panes is refused before the descriptor is read.
    let build_carried_pane_record = |pane_id, process_id| koshi_runtime::resume::CarriedPane {
        pane_id,
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
            build_carried_pane_record(first_pane_id, NO_SUCH_PROCESS),
            build_carried_pane_record(second_pane_id, NO_SUCH_PROCESS - 1),
        ],
    };

    let take_panes_result = take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    );

    let take_panes_message = match &take_panes_result {
        Err(resume_error) => resume_error.to_string(),
        Ok((pty_backend, _pty_handles)) => {
            let owner_terminal_descriptors: Vec<Option<i32>> = pty_backend
                .list_carried_panes()
                .iter()
                .map(|pane_record| pane_record.terminal_fd)
                .collect();
            format!(
                "descriptor {terminal_master_file_descriptor} is owned by {} panes: \
                 {owner_terminal_descriptors:?}",
                owner_terminal_descriptors.len()
            )
        }
    };
    // Dropping what came back closes the descriptor once per owner, and the
    // second close reaches whatever number this test binary opened in between,
    // which ends the whole binary. What came back is leaked instead, so the
    // assertion below is what this test reports.
    std::mem::forget(take_panes_result);

    assert_eq!(
        take_panes_message,
        format!(
            "pane {second_pane_id} carried descriptor {terminal_master_file_descriptor}, which pane \
             {first_pane_id} was already taken back \
             on, so it cannot be taken back"
        )
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
fn a_header_naming_one_pane_twice_is_refused_before_any_pane_is_touched() {
    // Every platform runs this check as the first step of taking the panes
    // back, and each platform takes them back its own way, so the check itself
    // is pinned here rather than through either of them.
    let first_pane_id = PaneId::new();
    let repeated_pane_id = PaneId::new();
    let last_pane_id = PaneId::new();

    header_names_each_pane_once(&build_resume_header(vec![
        build_carried_pane_record(first_pane_id),
        build_carried_pane_record(repeated_pane_id),
        build_carried_pane_record(last_pane_id),
    ]))
    .expect("three panes named once each are taken back");

    let resume_error = header_names_each_pane_once(&build_resume_header(vec![
        build_carried_pane_record(first_pane_id),
        build_carried_pane_record(repeated_pane_id),
        build_carried_pane_record(repeated_pane_id),
        build_carried_pane_record(first_pane_id),
    ]))
    .expect_err("a header naming a pane twice is refused");

    assert_eq!(
        resume_error.to_string(),
        format!("pane {repeated_pane_id} is named twice by the carried state, so it cannot be taken back"),
        "the refusal names the first pane that repeats, not the last"
    );
}

#[test]
fn a_header_naming_no_pane_at_all_names_each_of_them_once() {
    // A session whose last pane closed as the swap was written carries no pane,
    // and the check must let that header through rather than read as empty
    // meaning wrong.
    header_names_each_pane_once(&build_resume_header(Vec::new()))
        .expect("a header naming no pane is taken back");
}

#[cfg(unix)]
#[test]
fn two_carried_records_naming_one_pane_are_refused_rather_than_taken_back_over_each_other() {
    // A header naming one pane twice would have that pane taken back twice: the
    // second take-back replaces the first pane's entry, so the first pane's
    // reader, writer and watcher are left running with no entry to close them.
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
    let repeated_pane_id = PaneId::new();
    let build_carried_pane_with_terminal =
        |terminal_file_descriptor| koshi_runtime::resume::CarriedPane {
            pane_id: repeated_pane_id,
            process_id: NO_SUCH_PROCESS,
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
            build_carried_pane_with_terminal(first_terminal_master_file_descriptor),
            build_carried_pane_with_terminal(second_terminal_master_file_descriptor),
        ],
    };

    let take_panes_result = take_panes_back(
        &resume_header,
        Arc::new(InboxSink::from_event_sender(mpsc::channel().0)),
        &session_start,
    );

    let resume_error = match take_panes_result {
        Err(resume_error) => resume_error,
        Ok((pty_backend, pty_handles)) => panic!(
            "pane {repeated_pane_id} was taken back twice; the backend holds {} pane(s) and the caller {} \
             handle(s)",
            pty_backend.list_carried_panes().len(),
            pty_handles.len()
        ),
    };
    assert_eq!(
        resume_error.to_string(),
        format!("pane {repeated_pane_id} is named twice by the carried state, so it cannot be taken back")
    );
}

#[cfg(unix)]
#[test]
fn a_binary_printing_its_answer_with_no_newline_after_it_is_still_read() {
    // The reader takes the first line the binary prints, and a build that
    // writes its answer and exits without a newline has still answered. The
    // stream ending is what closes the line here, not a newline character.
    use std::os::unix::fs::PermissionsExt as _;

    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let executable_path = runtime_directory_fixture.path().join("koshi");
    std::fs::write(
        &executable_path,
        format!("#!/bin/sh\nprintf '{{\"min\":{RESUME_FORMAT_MIN},\"max\":{RESUME_FORMAT}}}'\n"),
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
    // The one failure the wait exists for: a binary that starts, prints
    // nothing, and keeps running. `exec` makes the sleeping process the one
    // this call spawned, so ending it ends the sleep as well.
    use std::os::unix::fs::PermissionsExt as _;

    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let executable_path = runtime_directory_fixture.path().join("koshi");
    std::fs::write(&executable_path, "#!/bin/sh\nexec sleep 60\n")
        .expect("the stand-in binary is written");
    std::fs::set_permissions(&executable_path, std::fs::Permissions::from_mode(0o755))
        .expect("the stand-in binary is runnable");

    // Linux refuses to run a file any process holds open for writing, and
    // answers `ETXTBSY`. A sibling test forking between the write above and its
    // own `exec` carries this file's write handle in that window, so the run is
    // retried until it starts. Every attempt that gets that far spends the whole
    // wait, so the timing claim below still holds.
    let probe_deadline = Instant::now() + BUSY_WAIT_DURATION;
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
        std::thread::sleep(BUSY_POLL_INTERVAL_DURATION);
    };

    assert_eq!(
        resume_support_result,
        Err(format!(
            "the binary at {} did not say which resume formats it reads within {} seconds",
            executable_path.display(),
            RESUME_SUPPORT_WAIT_DURATION.as_secs()
        ))
    );
    // The whole wait ran out, so the refusal came from the binary saying
    // nothing rather than from the reader ending early.
    assert!(
        wait_duration >= RESUME_SUPPORT_WAIT_DURATION,
        "the refusal must come after the whole wait, and it came after {wait_duration:?}"
    );
}

#[test]
fn a_binary_naming_a_lowest_format_above_its_highest_is_refused() {
    // A pair of numbers that names no format at all. The range is empty, so
    // nothing this build writes is inside it and the swap is refused.
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
    // The swap has nothing to carry once the last pane's child is gone, and
    // there is no session left to come back to. The loop's no-panes check runs
    // before the restart check, so the session ends here.
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (mut server, runtime_event_sender) = build_test_server(fake_pty_backend);
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
    // Both requests reach the inbox before the loop reads either. The quit is
    // the one that decides, so the session is not torn down into a swap the
    // user asked to end.
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (mut server, runtime_event_sender) = build_test_server(fake_pty_backend);
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
            envelope: CommandEnvelope::from_parts(
                CommandId::new(),
                CommandSource::ExternalCli {
                    session_id: Some(session_id),
                    target_client_id: None,
                },
                SystemTime::now(),
                Command::Quit,
            ),
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
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (mut server, runtime_event_sender) = build_test_server(fake_pty_backend);
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
fn a_resume_file_stamped_ahead_of_this_machines_clock_reads_as_a_swap_in_flight() {
    // A runtime directory can sit on a filesystem whose clock runs ahead, and a
    // stamp ahead of this process gives no age at all. The session is left alone, so a
    // clock this process cannot trust never costs a live session its endpoint
    // file.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_id = SessionId::new();
    let resume_file_path = resolve_resume_file_path(runtime_directory_fixture.path(), session_id);
    std::fs::write(&resume_file_path, b"{}").expect("the resume file is written");
    let ahead_resume_file = std::fs::File::options()
        .write(true)
        .open(&resume_file_path)
        .expect("the resume file opens for writing");
    ahead_resume_file
        .set_modified(SystemTime::now() + RESTART_WINDOW_DURATION * 10)
        .expect("the resume file is stamped ahead");

    assert!(
        is_replacing_its_image(runtime_directory_fixture.path(), session_id),
        "a stamp this machine's clock has not reached yet reads as fresh"
    );
}

#[test]
fn a_resume_file_exactly_as_old_as_the_window_reads_as_a_swap_that_died() {
    // The window is the boundary the router decides on, so the two sides of it
    // are pinned: a moment younger is a swap in flight, the window itself is a
    // swap that died.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_id = SessionId::new();
    let resume_file_path = resolve_resume_file_path(runtime_directory_fixture.path(), session_id);
    std::fs::write(&resume_file_path, b"{}").expect("the resume file is written");
    let aged_resume_file = std::fs::File::options()
        .write(true)
        .open(&resume_file_path)
        .expect("the resume file opens for writing");

    aged_resume_file
        .set_modified(SystemTime::now() - RESTART_WINDOW_DURATION + Duration::from_secs(2))
        .expect("the resume file is aged to just inside the window");
    assert!(
        is_replacing_its_image(runtime_directory_fixture.path(), session_id),
        "a resume file younger than the window means a swap is in flight"
    );

    aged_resume_file
        .set_modified(SystemTime::now() - RESTART_WINDOW_DURATION)
        .expect("the resume file is aged to the window itself");
    assert!(
        !is_replacing_its_image(runtime_directory_fixture.path(), session_id),
        "a resume file as old as the window means the swap died"
    );
}

#[test]
fn a_resume_file_whose_header_does_not_read_is_refused_and_taken_off_the_disk() {
    // A swap that died part way through its write leaves bytes no build reads.
    // Nothing can be taken back from them and nothing can be ended, and the
    // file holds every pane's screen and scrollback, so it goes.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let mut session_start = build_test_session_start(runtime_directory_fixture.path(), false);
    let resume_file_path =
        resolve_resume_file_path(runtime_directory_fixture.path(), session_start.session_id);
    std::fs::write(&resume_file_path, b"{\"header\":{\"format\":1}")
        .expect("the half-written resume file is placed");
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();

    let resume_error = match resume_from_file(
        &resume_file_path,
        &mut session_start,
        None,
        Arc::new(InboxSink::from_event_sender(runtime_event_sender.clone())),
        runtime_event_receiver,
        &runtime_event_sender,
    ) {
        Err(resume_error) => resume_error,
        Ok(_) => panic!("a header that does not read cannot start a session"),
    };

    assert_eq!(
        resume_error.to_string(),
        format!(
            "corrupt stored state: resume state at {} is unreadable: \
             missing field `session_id` at line 1 column 22",
            resume_file_path.display()
        )
    );
    assert!(
        !resume_file_path.exists(),
        "the file goes, because no subsequent build reads it either"
    );
}

#[test]
fn a_carried_state_naming_one_process_id_for_two_panes_is_refused() {
    // Each pane's watcher waits on that pane's child. Two watchers on one
    // process id leave the second finding nothing and reporting a live pane as
    // ended.
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut first_carried_pane = build_carried_pane_record(first_pane_id);
    first_carried_pane.process_id = 4821;
    let mut second_carried_pane = build_carried_pane_record(second_pane_id);
    second_carried_pane.process_id = 4821;

    let resume_error = header_names_each_pane_once(&build_resume_header(vec![
        first_carried_pane,
        second_carried_pane,
    ]))
    .expect_err("one process id cannot answer for two panes");

    assert_eq!(
        resume_error.to_string(),
        format!(
            "pane {second_pane_id} carries process id 4821, which pane {first_pane_id} already carries, \
             so it cannot be taken back"
        )
    );
}

#[test]
fn a_carried_state_naming_no_child_for_two_panes_is_taken_back() {
    // A process id of zero names no child, so it may repeat.
    let resume_header = build_resume_header(vec![
        build_carried_pane_record(PaneId::new()),
        build_carried_pane_record(PaneId::new()),
    ]);

    header_names_each_pane_once(&resume_header).expect("two panes with no child are taken back");
}
