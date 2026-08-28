//! Tests for the session-server loop, driven headlessly: a fake PTY backend
//! stands in for real children, so the real inbox loop runs without spawning a
//! process or binding a socket. Binding the socket and printing the ready line
//! need a whole process, so they are covered by the integration tests instead.
//!
//! The image swap is split the same way. What the session server decides — when
//! the loop ends into a swap, what a restart is refused for, which arguments the
//! new image is started with, what the carried state restores, and when the
//! router leaves a resuming session alone — is decided here. Taking a pane back
//! from a descriptor and replacing the process image need real children and real
//! processes, so they are covered by the integration tests.

use super::*;

use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, CopyArgs, CopyTarget, GridPos,
    Selection, SelectionKind, SetSelectionArgs, VisualCommand, WriteToPaneArgs,
};
use koshi_core::ids::{ClientId, CommandId};
use koshi_core::process::ExitStatus;
use koshi_renderer::snapshot::Delivery;
use koshi_runtime::placeholder::{SnapshotProvider, Storage};
use koshi_runtime::runtime::bus::EventFilter;
use koshi_runtime::runtime::event::AttachAccepted;
use koshi_test_support::fake_pty::FakePtyBackend;
use tempfile::TempDir;

/// A server built the way [`run_session_server`] builds it, on `fake` instead
/// of real children, plus a sender clone so a test can queue inbox events the
/// way the control socket does.
fn test_server(fake: Arc<FakePtyBackend>) -> (Server, mpsc::Sender<RuntimeEvent>) {
    let backend: Arc<dyn PtyBackend> = fake;
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, rx) = mpsc::channel();
    let mut server = Server::new(backend, snapshot_provider, storage, rx, tx.clone());
    server.load_startup_config(None);
    (server, tx)
}

/// What [`run_session_server`] was started with, for the tests that read the
/// command line the swap builds from it.
fn test_start(runtime_dir: &Path, allow_other_users: bool) -> SessionStart {
    SessionStart {
        runtime_dir: runtime_dir.to_path_buf(),
        session_id: SessionId::new(),
        session_name: "quiet-lake".to_string(),
        allow_other_users,
        exe: PathBuf::from("/opt/koshi/bin/koshi"),
        supervisor_token: None,
        supervisor_pid: None,
    }
}

/// The arguments of `command`, as plain strings in the order they are passed.
fn arguments(command: &std::process::Command) -> Vec<String> {
    command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

/// Queue a restart request the way the control socket does, and hand back the
/// channel the dispatcher answers on.
fn ask_to_restart(tx: &mpsc::Sender<RuntimeEvent>) -> mpsc::Receiver<Result<(), String>> {
    let (reply_tx, reply_rx) = mpsc::channel();
    tx.send(RuntimeEvent::IpcRestart { reply: reply_tx })
        .expect("the restart request is queued");
    reply_rx
}

/// Queue a command the way a client's keybinding does over the control socket:
/// on a reply channel nobody reads, since that command is answered by the next
/// painted frame.
fn queue_command(tx: &mpsc::Sender<RuntimeEvent>, client_id: ClientId, command: Command) {
    tx.send(RuntimeEvent::Ipc {
        envelope: CommandEnvelope::new(
            CommandId::new(),
            CommandSource::key_binding(client_id),
            SystemTime::now(),
            command,
        ),
        reply: mpsc::channel().0,
    })
    .expect("the command is queued");
}

/// Seed the one session this process serves, under a fresh id.
fn seed(server: &mut Server) {
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
/// hand back what the dispatcher minted: the client's id and its own queue.
fn attach(server: &mut Server) -> AttachAccepted {
    let (reply_tx, reply_rx) = mpsc::channel();
    let _ = server.handle_runtime_event(RuntimeEvent::IpcAttach {
        resume: None,
        resume_token: None,
        viewport: STARTING_VIEWPORT,
        pane_area: None,
        filter: EventFilter::All,
        attached_at: SystemTime::now(),
        remote: false,
        reply: reply_tx,
    });
    reply_rx
        .try_recv()
        .expect("the loop answered the attach request")
        .expect("a session is running")
}

/// The clients the seeded session still holds a record for, in id order.
fn attached_client_ids(server: &Server) -> Vec<ClientId> {
    let session = server
        .sessions()
        .values()
        .next()
        .expect("the session is seeded");
    let mut ids: Vec<ClientId> = session
        .clients
        .list_attached()
        .map(|client| client.id())
        .collect();
    ids.sort();
    ids
}

#[test]
fn the_session_answers_discovery_with_the_id_and_name_it_was_started_with() {
    // The id and the name are picked outside this process and handed to it at
    // startup, so a session that generated either one itself would answer a
    // lookup under a name no caller asked for.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx) = test_server(fake);
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

    let (reply_tx, reply_rx) = mpsc::channel();
    tx.send(RuntimeEvent::IpcDiscovery { reply: reply_tx })
        .expect("the discovery request is queued");
    tx.send(RuntimeEvent::Quit).expect("the hangup is queued");

    serve(&mut server);

    let overview = reply_rx
        .try_recv()
        .expect("the loop answered the discovery request")
        .expect("a session is running");
    assert_eq!(overview.session.id, session_id);
    assert_eq!(overview.session.name, "quiet-lake");
    assert_eq!(overview.session.pane_count, 1);
}

#[test]
fn a_quit_command_arriving_on_the_socket_ends_the_loop() {
    // Ending a session is a command forwarded over its control socket, so the
    // loop must both apply it and stop on it — a loop that only applied it
    // would leave the process running with its panes killed.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx) = test_server(fake);
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
    let (reply_tx, reply_rx) = mpsc::channel();
    tx.send(RuntimeEvent::Ipc {
        envelope: CommandEnvelope::new(
            command_id,
            CommandSource::ExternalCli {
                session_id: Some(session_id),
                target_client: None,
            },
            SystemTime::now(),
            Command::Quit,
        ),
        reply: reply_tx,
    })
    .expect("the quit command is queued");

    serve(&mut server);

    assert_eq!(
        reply_rx.try_recv().expect("the loop answered the command"),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );
    assert!(server.quit_requested());
}

#[test]
fn the_last_childs_exit_ends_the_loop_with_no_quit_asked_for() {
    // Nothing queues a quit here and the sender stays alive, so the only way
    // out is the loop's own no-panes check. A loop missing it would leave the
    // process alive on an empty session, blocked on an inbox nobody feeds.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx) = test_server(fake);
    seed(&mut server);
    let pane_id = *server
        .terminal_engines()
        .keys()
        .next()
        .expect("the root pane holds a terminal engine");

    tx.send(RuntimeEvent::ChildExit {
        pane_id,
        status: ExitStatus::ExitCode(0),
        exited_at: SystemTime::now(),
    })
    .expect("the child's exit is queued");

    serve(&mut server);

    assert!(!server.has_active_panes());
    assert!(!server.quit_requested());
}

#[test]
fn a_due_render_hands_the_attached_client_its_frame() {
    // This process paints nothing, so the loop pushing the frame is the only
    // way a client ever sees its session change: a loop that applied the child
    // output without pushing would leave the client on the picture it joined
    // on, with the shell's "hello" never drawn.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx) = test_server(fake);
    seed(&mut server);
    let accepted = attach(&mut server);
    let pane_id = *server
        .terminal_engines()
        .keys()
        .next()
        .expect("the root pane holds a terminal engine");

    tx.send(RuntimeEvent::PtyOutput {
        pane_id,
        bytes: b"hello".to_vec(),
    })
    .expect("the child output is queued");
    tx.send(RuntimeEvent::Quit).expect("the hangup is queued");

    serve(&mut server);

    let expected = server
        .build_snapshot(accepted.client_id)
        .expect("the attached client has a frame");
    assert_eq!(
        accepted.events.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Frame(Box::new(expected))]
    );
}

#[test]
fn a_pass_with_no_render_due_pushes_no_frame() {
    // The push rides the render clock. An ungated one would build and queue a
    // frame on every pass, so a session nothing changed in — one woken only by
    // a discovery query — would keep filling its clients' queues.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx) = test_server(fake);
    seed(&mut server);
    let accepted = attach(&mut server);
    // Spend the render the seeding and the attach made due, so the pass below
    // starts with nothing pending.
    assert!(server.poll_render(Instant::now()));

    tx.send(RuntimeEvent::Quit).expect("the hangup is queued");

    serve(&mut server);

    assert_eq!(
        accepted.events.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
}

#[test]
fn an_accepted_restart_ends_the_loop_into_the_swap() {
    // The reply is written while the socket is still up and the swap runs after
    // the loop ends. A loop that answered the request and kept serving would
    // leave the caller told the session restarted while it never did.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx) = test_server(fake);
    seed(&mut server);
    server.set_restart_check(Arc::new(|| Ok(())));

    let reply = ask_to_restart(&tx);

    let outcome = serve(&mut server);

    assert_eq!(
        reply.try_recv().expect("the loop answered the request"),
        Ok(())
    );
    assert!(server.restart_requested());
    assert_eq!(outcome, ServeOutcome::Restart);
}

#[test]
fn a_restart_naming_a_binary_that_cannot_be_read_is_refused_and_the_session_keeps_serving() {
    // The reply is the session's only chance to refuse: after it, the swap
    // runs. A path with nothing at it must not reach the swap, and the session
    // must still answer everything else afterwards.
    let dir = TempDir::new().expect("create temp dir");
    let missing = dir.path().join("koshi");
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx) = test_server(fake);
    seed(&mut server);
    // The first thing the installed check runs, on a path with nothing at it.
    let named = missing.clone();
    server.set_restart_check(Arc::new(move || binary_is_runnable(&named)));

    let reply = ask_to_restart(&tx);
    let (discovery_tx, discovery_rx) = mpsc::channel();
    tx.send(RuntimeEvent::IpcDiscovery {
        reply: discovery_tx,
    })
    .expect("the discovery request is queued");
    tx.send(RuntimeEvent::Quit).expect("the hangup is queued");

    let outcome = serve(&mut server);

    let refusal = reply
        .try_recv()
        .expect("the loop answered the request")
        .expect_err("a binary that is not there is refused");
    assert!(
        refusal.contains(&missing.display().to_string()),
        "the refusal must name the path, got {refusal}"
    );
    assert!(!server.restart_requested());
    assert_eq!(outcome, ServeOutcome::Ended);
    let overview = discovery_rx
        .try_recv()
        .expect("the loop answered the discovery request")
        .expect("a session is running");
    assert_eq!(overview.session.pane_count, 1);
}

#[test]
fn a_copy_applied_while_the_swap_runs_reaches_the_clients_own_terminal() {
    // The swap applies what the socket queued after the serve loop returned, so
    // it is the last thing that can hand those bytes over. A pass that only
    // applied the copy would carry nothing across and destroy the escape with
    // the image: the system clipboard would keep its old contents, and the
    // client is told nothing either way.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx) = test_server(fake);
    seed(&mut server);
    let accepted = attach(&mut server);
    let pane_id = *server
        .terminal_engines()
        .keys()
        .next()
        .expect("the root pane holds a terminal engine");
    server.handle_pty_output(pane_id, b"hello");
    queue_command(
        &tx,
        accepted.client_id,
        Command::Visual(VisualCommand::SetSelection(SetSelectionArgs {
            pane: pane_id,
            selection: Selection {
                kind: SelectionKind::Character,
                anchor: GridPos { row: 0, col: 0 },
                cursor: GridPos { row: 0, col: 4 },
            },
        })),
    );
    queue_command(
        &tx,
        accepted.client_id,
        Command::Visual(VisualCommand::Copy(CopyArgs {
            pane: pane_id,
            target: CopyTarget::Osc52,
            trim_trailing_whitespace: true,
        })),
    );

    apply_queued(&mut server, Detaches::Apply);

    // base64("hello") = aGVsbG8=
    let written: Vec<Vec<u8>> = accepted
        .events
        .try_iter()
        .filter_map(|delivery| match delivery {
            Delivery::HostWrite(bytes) => Some(bytes),
            _ => None,
        })
        .collect();
    assert_eq!(written, vec![b"\x1b]52;c;aGVsbG8=\x07".to_vec()]);
}

#[test]
fn a_quit_applied_while_the_swap_runs_ends_the_session_instead_of_serving_it_again() {
    // `koshi kill-session` arriving inside the swap window is applied there, by
    // the same pass, after the serve loop returned. A loop that went back to
    // waiting on the inbox would keep serving the session the user asked to
    // end, and the swap would carry it into the new image alive.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx) = test_server(fake);
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
    let reply = ask_to_restart(&tx);
    assert_eq!(serve(&mut server), ServeOutcome::Restart);
    assert_eq!(
        reply.try_recv().expect("the loop answered the request"),
        Ok(())
    );

    tx.send(RuntimeEvent::Ipc {
        envelope: CommandEnvelope::new(
            CommandId::new(),
            CommandSource::ExternalCli {
                session_id: Some(session_id),
                target_client: None,
            },
            SystemTime::now(),
            Command::Quit,
        ),
        reply: mpsc::channel().0,
    })
    .expect("the quit command is queued");
    apply_queued(&mut server, Detaches::Apply);
    assert!(
        server.quit_requested(),
        "the swap's inbox pass applies the quit"
    );

    let (discovery_tx, discovery_rx) = mpsc::channel();
    tx.send(RuntimeEvent::IpcDiscovery {
        reply: discovery_tx,
    })
    .expect("the discovery request is queued");

    assert_eq!(serve(&mut server), ServeOutcome::Ended);
    assert!(
        matches!(discovery_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "a session that was asked to quit answers nothing else"
    );
}

#[test]
#[cfg(unix)]
fn a_swap_the_session_abandons_takes_the_accepted_restart_back() {
    // Every abandon path hands the same server back to the serve loop. A server
    // that kept the accepted restart would leave that loop and run the swap that
    // just failed again, on every pass.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx) = test_server(fake);
    seed(&mut server);
    server.set_restart_check(Arc::new(|| Ok(())));
    let reply = ask_to_restart(&tx);
    assert_eq!(serve(&mut server), ServeOutcome::Restart);
    assert_eq!(
        reply.try_recv().expect("the loop answered the request"),
        Ok(())
    );
    assert!(server.restart_requested());
    let panes = Arc::new(PortablePtyBackend::new());

    let kept = keep_serving(server, &panes);

    assert!(!kept.restart_requested());
    assert!(!kept.quit_requested());
}

#[test]
fn a_client_that_hung_up_before_the_swap_told_anyone_is_detached() {
    // The inbox passes before the announce run while every client is still
    // streaming, so a detach they drain is a client that closed its terminal. A
    // pass that dropped it would leave that record attached on every abandon
    // path: the tab would stay clamped to the size of a terminal that is gone,
    // and `auto-close-session` would never see the session empty.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx) = test_server(fake);
    seed(&mut server);
    let staying = attach(&mut server);
    let leaving = attach(&mut server);
    tx.send(RuntimeEvent::ClientDetached {
        client_id: leaving.client_id,
        detached_at: SystemTime::now(),
        streamed: true,
    })
    .expect("the detach is queued");

    apply_queued(&mut server, Detaches::Apply);

    assert_eq!(attached_client_ids(&server), vec![staying.client_id]);
}

#[test]
fn the_record_of_a_client_the_swap_told_survives_the_pass_after_the_announce() {
    // Both halves of a told client's connection queue a detach once it reads the
    // restart frame. Applying those would carry a session holding no client into
    // the new image, and every client would come back a stranger, with fresh
    // focus, zoom, scroll offset and selection.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx) = test_server(fake);
    seed(&mut server);
    let told = attach(&mut server);
    tx.send(RuntimeEvent::ClientDetached {
        client_id: told.client_id,
        detached_at: SystemTime::now(),
        streamed: true,
    })
    .expect("the detach is queued");

    apply_queued(&mut server, Detaches::Skip);

    assert_eq!(attached_client_ids(&server), vec![told.client_id]);
}

#[test]
fn a_line_a_client_types_as_it_reads_the_restart_frame_reaches_its_pane() {
    // The frame reaches the client over its socket and the client answers on
    // that same socket, so the line is still crossing the wire when the announce
    // returns. `IpcServer::close_intake` puts what crossed in the inbox before
    // this pass runs; a pass that dropped it would destroy the line with the
    // image, and the child would sit waiting for input the user has already
    // typed.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx) = test_server(Arc::clone(&fake));
    seed(&mut server);
    let client_id = attach(&mut server).client_id;
    let pane_id = *server
        .terminal_engines()
        .keys()
        .next()
        .expect("the root pane holds a terminal engine");
    queue_command(
        &tx,
        client_id,
        Command::WriteToPane(WriteToPaneArgs {
            pane: Some(pane_id),
            data: vec![b'\r'],
        }),
    );

    apply_queued(&mut server, Detaches::Skip);

    assert_eq!(
        fake.writes(pane_id).expect("the pane is open"),
        vec![vec![b'\r']]
    );
}

#[test]
fn the_carried_state_reads_back_with_every_tab_pane_and_screen() {
    // The session server writes the state to a file and the image that replaces
    // it reads that file back. A round trip that lost a tab, a pane record or a
    // pane's screen would come back as a session the user does not recognise.
    let dir = TempDir::new().expect("create temp dir");
    let resume_file = dir.path().join("session.resume");
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, _tx) = test_server(Arc::clone(&fake));
    let session_id = SessionId::new();
    let client_id = server
        .bootstrap_local_named(
            session_id,
            "quiet-lake".to_string(),
            STARTING_VIEWPORT,
            SystemTime::UNIX_EPOCH,
        )
        .expect("the session is seeded");
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::key_binding(client_id),
        SystemTime::UNIX_EPOCH,
        Command::NewTab(koshi_core::command::NewTabArgs {
            cwd: None,
            client: Some(client_id),
        }),
    );
    assert!(
        matches!(server.submit_command(envelope), CommandResult::Ok { .. }),
        "the second tab must open"
    );
    // Distinct output per pane, so a screen that came back under the wrong pane
    // is caught.
    let panes: Vec<PaneId> = server.terminal_engines().keys().copied().collect();
    assert_eq!(panes.len(), 2, "two tabs means two panes");
    for (index, pane) in panes.iter().enumerate() {
        server.handle_pty_output(*pane, format!("pane {index}").as_bytes());
        // Carrying the state out cancels the sequence each engine is still
        // decoding, which settles the grapheme cluster it holds too. Settling
        // it here makes the screen read below the one that is carried.
        server.handle_pty_output(*pane, &[0x18]);
    }
    let expected_tabs = server.sessions()[&session_id].tabs.clone();
    let expected_records = server.sessions()[&session_id].panes.clone();
    let expected_screens: HashMap<PaneId, koshi_terminal::state::TerminalState> = server
        .terminal_engines()
        .iter()
        .map(|(pane, engine)| (*pane, engine.state().clone()))
        .collect();
    let carried: Vec<koshi_pty::portable::CarriedPtyPane> = panes
        .iter()
        .enumerate()
        .map(|(index, pane_id)| koshi_pty::portable::CarriedPtyPane {
            pane_id: *pane_id,
            #[cfg(unix)]
            terminal_fd: Some(30 + index as i32),
            pid: 4000 + index as u32,
            size: PtySize { cols: 1, rows: 1 },
            exit: None,
        })
        .collect();

    let (header, body) = server.carry_out(session_id, "quiet-lake".to_string(), &carried);
    resume::write(&resume_file, &header, &body).expect("the carried state is written");
    let (read_back, raw_body) = resume::read_header(&resume_file).expect("the header reads back");
    let read_body = resume::read_body(read_back.format, &raw_body).expect("the body reads back");
    let handles: HashMap<PaneId, PtyHandle> = read_back
        .panes
        .iter()
        .map(|pane| (pane.pane_id, PtyHandle::detached(pane.pane_id)))
        .collect();
    let (resumed_tx, resumed_rx) = mpsc::channel();
    let resumed = Server::resume(
        Arc::clone(&fake) as Arc<dyn PtyBackend>,
        resumed_rx,
        resumed_tx,
        read_body,
        handles,
        carried_sizes(&read_back),
    );

    assert_eq!(read_back, header, "the header must read back unchanged");
    assert_eq!(resumed.sessions().len(), 1);
    let session = &resumed.sessions()[&session_id];
    assert_eq!(session.name, "quiet-lake");
    assert_eq!(session.tabs, expected_tabs, "every tab and its layout tree");
    assert_eq!(session.panes, expected_records, "every pane record");
    for (pane, screen) in &expected_screens {
        assert_eq!(
            resumed
                .terminal_engines()
                .get(pane)
                .expect("the pane's engine came back")
                .state(),
            screen,
            "the screen of pane {pane}"
        );
    }
    assert_eq!(
        carried_sizes(&read_back).len(),
        2,
        "each carried pane names a size"
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
    let raw = ordinary_file.as_raw_fd();
    let dir = TempDir::new().expect("create temp dir");
    let start = test_start(dir.path(), false);
    let pane_id = PaneId::new();
    let header = ResumeHeader {
        format: 1,
        session_id: start.session_id,
        session_name: start.session_name.clone(),
        panes: vec![koshi_runtime::resume::CarriedPane {
            pane_id,
            // No pane child to end: a process id of zero names none, so the
            // cleanup this failure runs signals nothing.
            pid: 0,
            rows: 24,
            cols: 80,
            terminal_fd: Some(raw),
            terminal_name: None,
            exit: None,
        }],
    };

    let refused = take_panes_back(&header, Arc::new(InboxSink::new(mpsc::channel().0)), &start)
        .err()
        .expect("a descriptor that is no terminal master must be refused");

    assert_eq!(
        refused.to_string(),
        format!(
            "pane {pane_id} carried descriptor {raw}, which names no pseudoterminal master, \
             so it cannot be taken back"
        )
    );
    assert!(
        unsafe { libc::fcntl(raw, libc::F_GETFD) } >= 0,
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

    let mut command = std::process::Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("sleep 30")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // A pane's child leads its own process group, which is what the whole
    // group being ended means; `portable-pty` puts it there with the same call.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().expect("the child starts");
    // A real pseudoterminal master stands in for the pane's terminal, since
    // only a master is closed. The terminal it is paired with is what says the
    // descriptor is gone afterwards, whatever number this process hands out
    // next.
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(master >= 0, "the pseudoterminal master opens");
    let named = terminal_master_name(master).expect("the master names its terminal");
    let dir = TempDir::new().expect("create temp dir");
    let start = test_start(dir.path(), false);
    let header = ResumeHeader {
        format: 1,
        session_id: start.session_id,
        session_name: start.session_name.clone(),
        panes: vec![koshi_runtime::resume::CarriedPane {
            pane_id: PaneId::new(),
            pid: child.id(),
            rows: 24,
            cols: 80,
            terminal_fd: Some(master),
            terminal_name: Some(named.clone()),
            exit: None,
        }],
    };

    release_carried_panes(&header, &start, Arc::new(InboxSink::new(mpsc::channel().0)));

    let status = child.wait().expect("the child is reaped");
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert_ne!(
        terminal_master_name(master),
        Some(named),
        "the terminal must be closed"
    );
}

#[test]
#[cfg(unix)]
fn a_swap_that_did_not_happen_leaves_every_terminal_closed_on_exec_again() {
    // The flag is cleared so the descriptor crosses the swap. A swap that never
    // ran and left it cleared would hand the next pane's child a hold on this
    // pane's terminal.
    use std::os::fd::AsRawFd;

    let terminal = std::fs::File::open("/dev/null").expect("open a descriptor");
    let raw = terminal.as_raw_fd();
    let dir = TempDir::new().expect("create temp dir");
    let start = test_start(dir.path(), false);
    let header = ResumeHeader {
        format: 1,
        session_id: start.session_id,
        session_name: start.session_name.clone(),
        panes: vec![koshi_runtime::resume::CarriedPane {
            pane_id: PaneId::new(),
            pid: std::process::id(),
            rows: 24,
            cols: 80,
            terminal_fd: Some(raw),
            terminal_name: None,
            exit: None,
        }],
    };

    keep_terminals_across_exec(&header).expect("the flag is cleared");
    let cleared = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    put_close_on_exec_back(&header);
    let restored = unsafe { libc::fcntl(raw, libc::F_GETFD) };

    assert_eq!(cleared & libc::FD_CLOEXEC, 0);
    assert_eq!(restored & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
}

#[test]
fn the_resume_command_line_names_the_state_and_never_a_profile() {
    // The profile opened this session's tabs and panes once. A resume run that
    // ran it again would come up with the profile's panes beside the carried
    // ones.
    let dir = TempDir::new().expect("create temp dir");
    let start = test_start(dir.path(), false);
    let resume_file = dir.path().join("session.resume");

    let args = arguments(&resume_command(&start, &resume_file));

    assert_eq!(
        args,
        vec![
            "serve-session".to_string(),
            start.session_id.to_string(),
            "quiet-lake".to_string(),
            "--runtime-dir".to_string(),
            dir.path().display().to_string(),
            "--resume".to_string(),
            resume_file.display().to_string(),
        ]
    );
}

#[test]
fn the_resume_command_line_keeps_the_reach_this_session_was_started_with() {
    // `--allow-other-users` is the only input to the socket's reach, so a
    // resume run without it would rebind the socket where the other users of
    // this machine can no longer see it.
    let dir = TempDir::new().expect("create temp dir");
    let mut start = test_start(dir.path(), true);
    start.supervisor_token = Some("a-secret".to_string());
    start.supervisor_pid = Some(4821);
    let resume_file = dir.path().join("session.resume");

    let args = arguments(&resume_command(&start, &resume_file));

    assert_eq!(
        args,
        vec![
            "serve-session".to_string(),
            start.session_id.to_string(),
            "quiet-lake".to_string(),
            "--runtime-dir".to_string(),
            dir.path().display().to_string(),
            "--resume".to_string(),
            resume_file.display().to_string(),
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
        ResumeSupport { min: 1, max: 3 }
    );
    assert_eq!(
        serde_json::to_string(&ResumeSupport::of_this_build()).expect("the range encodes"),
        format!("{{\"min\":{RESUME_FORMAT_MIN},\"max\":{RESUME_FORMAT}}}")
    );
    let refusal = parse_resume_support("koshi 0.2.0").expect_err("a version line is not a range");
    assert!(
        refusal.contains("does not say which resume formats it reads"),
        "the refusal must say what was missing, got {refusal}"
    );
}

#[test]
fn a_binary_reading_no_format_this_one_writes_is_refused_naming_both_ranges() {
    // This is the only check that cannot be made again after the swap: the
    // install already replaced the old binary on disk, so an image that cannot
    // read the carried state cannot be put back.
    let exe = PathBuf::from("/opt/koshi/bin/koshi");

    let refusal = reads_the_format_this_build_writes(ResumeSupport { min: 7, max: 9 }, &exe)
        .expect_err("a range the written format is outside of is refused");

    assert_eq!(
        refusal,
        format!(
            "the binary at /opt/koshi/bin/koshi reads resume formats 7 to 9, and this one reads \
             {RESUME_FORMAT_MIN} to {RESUME_FORMAT} and writes {RESUME_FORMAT}"
        )
    );
    assert_eq!(
        reads_the_format_this_build_writes(
            ResumeSupport {
                min: RESUME_FORMAT_MIN,
                max: RESUME_FORMAT + 4
            },
            &exe
        ),
        Ok(())
    );

    // The real case behind the refusal: a koshi that reads formats 1 through
    // 2 is an older build, and this one writes a body with format 3.
    assert_eq!(
        reads_the_format_this_build_writes(ResumeSupport { min: 1, max: 2 }, &exe),
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
fn write_probe_binary(path: &Path, line: &str) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::write(path, format!("#!/bin/sh\nprintf '%s\\n' '{line}'\n"))
        .expect("the stand-in binary is written");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("the stand-in binary is runnable");
}

#[test]
fn a_binary_that_cannot_be_run_is_refused_naming_the_path_and_the_reason() {
    // The probe runs the binary, so a download that arrived broken or built for
    // another machine is caught here rather than after the swap has started.
    let dir = TempDir::new().expect("create temp dir");
    let exe = dir.path().join("koshi");
    let refused = std::process::Command::new(&exe)
        .arg(RESUME_SUPPORT_SUBCOMMAND)
        .spawn()
        .expect_err("nothing is at that path");

    assert_eq!(
        resume_support(&exe),
        Err(format!(
            "the binary at {} could not be run: {refused}",
            exe.display()
        ))
    );
}

#[cfg(unix)]
#[test]
fn a_binary_answering_a_range_this_one_writes_into_passes_the_whole_check() {
    // The three answers the session must hold before it tears itself down: the
    // binary runs, the panes cross, and the binary reads what this one writes.
    let dir = TempDir::new().expect("create temp dir");
    let exe = dir.path().join("koshi");
    write_probe_binary(
        &exe,
        &format!(
            "{{\"min\":{RESUME_FORMAT_MIN},\"max\":{}}}",
            RESUME_FORMAT + 5
        ),
    );

    assert_eq!(
        resume_support(&exe),
        Ok(ResumeSupport {
            min: RESUME_FORMAT_MIN,
            max: RESUME_FORMAT + 5
        })
    );
    assert_eq!(binary_is_runnable(&exe), Ok(()));
    assert_eq!(
        reads_the_format_this_build_writes(
            ResumeSupport {
                min: RESUME_FORMAT_MIN,
                max: RESUME_FORMAT + 5
            },
            &exe
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
    let dir = TempDir::new().expect("create temp dir");
    let exe = dir.path().join("koshi");
    write_probe_binary(&exe, "");

    let refused = resume_support(&exe).expect_err("an empty answer is no answer");

    assert_eq!(
        refused,
        format!(
            "the binary at {} does not say which resume formats it reads: EOF while parsing a \
             value at line 1 column 0",
            exe.display()
        )
    );
}

#[cfg(unix)]
#[test]
fn a_binary_answering_a_range_that_misses_this_ones_is_refused_naming_both() {
    // Both directions of a miss are refused: a range wholly above what this
    // build writes, and one wholly below it.
    let dir = TempDir::new().expect("create temp dir");
    let exe = dir.path().join("koshi");
    let above = RESUME_FORMAT + 1;
    write_probe_binary(&exe, &format!("{{\"min\":{above},\"max\":{}}}", above + 2));

    let support = resume_support(&exe).expect("the line reads as a range");
    assert_eq!(
        support,
        ResumeSupport {
            min: above,
            max: above + 2
        }
    );
    assert_eq!(
        reads_the_format_this_build_writes(support, &exe),
        Err(format!(
            "the binary at {} reads resume formats {above} to {}, and this one reads \
             {RESUME_FORMAT_MIN} to {RESUME_FORMAT} and writes {RESUME_FORMAT}",
            exe.display(),
            above + 2
        ))
    );
    assert_eq!(
        reads_the_format_this_build_writes(ResumeSupport { min: 0, max: 0 }, &exe),
        Err(format!(
            "the binary at {} reads resume formats 0 to 0, and this one reads \
             {RESUME_FORMAT_MIN} to {RESUME_FORMAT} and writes {RESUME_FORMAT}",
            exe.display()
        ))
    );
}

#[cfg(unix)]
#[test]
fn an_image_swap_that_could_not_start_hands_back_its_reason_and_keeps_ignoring_sigpipe() {
    // Replacing the image resets `SIGPIPE` to its default in this process
    // before the call, so a swap that did not start has to put the ignore back.
    // Without it the next write to a client that hung up would end the session.
    let dir = TempDir::new().expect("create temp dir");
    let mut start = test_start(dir.path(), false);
    start.exe = dir.path().join("koshi-that-is-not-there");
    let resume_file = dir.path().join("session.resume");

    let refused = restart_by_exec(&start, &resume_file);

    assert_eq!(refused.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(refused.raw_os_error(), Some(libc::ENOENT));

    let mut held: libc::sigaction = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::sigaction(libc::SIGPIPE, std::ptr::null(), &mut held) },
        0,
        "the signal's handler is read back"
    );
    assert_eq!(
        held.sa_sigaction,
        libc::SIG_IGN,
        "a swap that did not start must leave the broken-pipe signal ignored"
    );
}

#[cfg(unix)]
#[test]
fn a_pane_the_header_names_no_descriptor_for_refuses_to_be_taken_back() {
    // A Unix pane is taken back by its descriptor and nothing else, so a record
    // carrying none names a pane this image cannot drive.
    let dir = TempDir::new().expect("create temp dir");
    let start = test_start(dir.path(), false);
    let stranded = PaneId::new();
    let header = ResumeHeader {
        format: RESUME_FORMAT,
        session_id: start.session_id,
        session_name: start.session_name.clone(),
        panes: vec![koshi_runtime::resume::CarriedPane {
            pane_id: stranded,
            pid: std::process::id(),
            rows: 24,
            cols: 80,
            terminal_fd: None,
            terminal_name: None,
            exit: None,
        }],
    };

    let refused =
        match take_panes_back(&header, Arc::new(InboxSink::new(mpsc::channel().0)), &start) {
            Err(refused) => refused,
            Ok(_) => panic!("a pane with no descriptor cannot be taken back"),
        };

    assert_eq!(
        refused.to_string(),
        format!("pane {stranded} carried no terminal descriptor, so it cannot be taken back")
    );
}

#[cfg(unix)]
#[test]
fn a_header_naming_no_pane_is_taken_back_as_a_session_holding_none() {
    // A swap runs whatever the session holds, including nothing. Taking back an
    // empty header must give a working backend rather than fail.
    let dir = TempDir::new().expect("create temp dir");
    let start = test_start(dir.path(), false);
    let header = ResumeHeader {
        format: RESUME_FORMAT,
        session_id: start.session_id,
        session_name: start.session_name.clone(),
        panes: Vec::new(),
    };

    let (panes, handles) =
        take_panes_back(&header, Arc::new(InboxSink::new(mpsc::channel().0)), &start)
            .expect("an empty header is taken back");

    assert_eq!(handles.len(), 0, "no pane means no handle");
    assert_eq!(panes.carried_panes().len(), 0, "and no pane to carry on");
    assert_eq!(carried_sizes(&header).len(), 0, "and no size to record");
}

// On Windows the panes live in a helper process, so taking them back means
// reaching that process; the secret of the link to it is the one thing the new
// image cannot do without.
#[cfg(windows)]
#[test]
fn a_resume_run_that_was_passed_no_link_secret_refuses_to_take_its_panes_back() {
    let dir = TempDir::new().expect("create temp dir");
    let start = test_start(dir.path(), false);
    assert_eq!(start.supervisor_token, None, "no secret was passed on");
    let header = ResumeHeader {
        format: RESUME_FORMAT,
        session_id: start.session_id,
        session_name: start.session_name.clone(),
        panes: Vec::new(),
    };

    let refused =
        match take_panes_back(&header, Arc::new(InboxSink::new(mpsc::channel().0)), &start) {
            Err(refused) => refused,
            Ok(_) => panic!("panes cannot be reached without the link secret"),
        };

    assert_eq!(
        refused.to_string(),
        "the secret of the link to the process holding the panes was not passed on, \
         so those panes cannot be reached"
    );
    assert_eq!(carried_sizes(&header).len(), 0, "and no size to record");
}

// The helper's link address carries its own process id, so a resume run
// without that id cannot name the process holding the panes either.
#[cfg(windows)]
#[test]
fn a_resume_run_that_was_passed_no_helper_process_id_refuses_to_take_its_panes_back() {
    let dir = TempDir::new().expect("create temp dir");
    let mut start = test_start(dir.path(), false);
    start.supervisor_token = Some("a-secret".to_string());
    assert_eq!(start.supervisor_pid, None, "no process id was passed on");
    let header = ResumeHeader {
        format: RESUME_FORMAT,
        session_id: start.session_id,
        session_name: start.session_name.clone(),
        panes: Vec::new(),
    };

    let refused =
        match take_panes_back(&header, Arc::new(InboxSink::new(mpsc::channel().0)), &start) {
            Err(refused) => refused,
            Ok(_) => panic!("panes cannot be reached without the helper's process id"),
        };

    assert_eq!(
        refused.to_string(),
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
    let dir = TempDir::new().expect("create temp dir");
    let session = SessionId::new();
    assert!(
        !is_replacing_its_image(dir.path(), session),
        "a session with no resume file is not replacing its image"
    );

    let resume_file = resume_path(dir.path(), session);
    std::fs::write(&resume_file, b"{}").expect("the resume file is written");
    assert!(
        is_replacing_its_image(dir.path(), session),
        "a resume file written just now means a swap is in flight"
    );

    let stale = std::fs::File::options()
        .write(true)
        .open(&resume_file)
        .expect("the resume file opens for writing");
    stale
        .set_modified(SystemTime::now() - RESTART_WINDOW - Duration::from_secs(1))
        .expect("the resume file is aged");
    assert!(
        !is_replacing_its_image(dir.path(), session),
        "a resume file older than the window means the swap died"
    );
}

/// A process id that is positive, fits an `i32`, and names no process on any
/// system: process ids are handed out from the low numbers up.
#[cfg(unix)]
const NO_SUCH_PROCESS: u32 = 2_147_483_646;

/// How long a test keeps trying to run a file the operating system reports as
/// held open for writing.
#[cfg(unix)]
const BUSY_WAIT: Duration = Duration::from_secs(20);

/// How long a test pauses between those attempts.
#[cfg(unix)]
const BUSY_POLL: Duration = Duration::from_millis(20);

#[test]
fn a_child_that_ends_after_the_header_is_built_still_carries_its_real_status() {
    // The panes are read once to build the header and again just before it is
    // written. A child reaped in between is known only to this image, so its
    // status has to reach the header on that second read or the next image
    // waits on a process id nobody can answer for.
    let settled = PaneId::new();
    let still_running = PaneId::new();
    let already_known = PaneId::new();
    let closed_before_the_swap = PaneId::new();

    // The refresh pairs records by pane id, so the process id below is never
    // read. Every record carries the same one.
    const PANE_CHILD: u32 = 4821;

    let carried = |pane_id, exit| koshi_runtime::resume::CarriedPane {
        pane_id,
        pid: PANE_CHILD,
        rows: 24,
        cols: 80,
        terminal_fd: None,
        terminal_name: None,
        exit,
    };
    let mut header = ResumeHeader {
        format: RESUME_FORMAT,
        session_id: SessionId::new(),
        session_name: "main".to_string(),
        panes: vec![
            carried(settled, None),
            carried(still_running, None),
            carried(already_known, Some(ExitStatus::ExitCode(3))),
        ],
    };

    let now = |pane_id, exit| koshi_pty::portable::CarriedPtyPane {
        pane_id,
        #[cfg(unix)]
        terminal_fd: None,
        pid: PANE_CHILD,
        size: PtySize { rows: 24, cols: 80 },
        exit,
    };
    refresh_carried_exits(
        &mut header,
        &[
            now(settled, Some(ExitStatus::ExitCode(7))),
            now(still_running, None),
            now(already_known, Some(ExitStatus::ExitCode(9))),
            now(closed_before_the_swap, Some(ExitStatus::ExitCode(11))),
        ],
    );

    let exit_of = |pane_id| {
        header
            .panes
            .iter()
            .find(|pane| pane.pane_id == pane_id)
            .expect("the pane is in the header")
            .exit
    };
    assert_eq!(exit_of(settled), Some(ExitStatus::ExitCode(7)));
    assert_eq!(exit_of(still_running), None);
    // A status the header already carried is the one this image reaped first,
    // so the later read never writes over it.
    assert_eq!(exit_of(already_known), Some(ExitStatus::ExitCode(3)));
    // The header names which panes the next image takes back. A pane the live
    // read reports but the header never named is left out of it.
    assert_eq!(
        header
            .panes
            .iter()
            .map(|pane| pane.pane_id)
            .collect::<Vec<PaneId>>(),
        vec![settled, still_running, already_known]
    );
}

#[cfg(unix)]
#[test]
fn a_pane_naming_a_descriptor_this_process_does_not_hold_is_refused() {
    // A number this process never opened. It sits far above every descriptor a
    // test run holds, so nothing can take it while the test runs.
    const NEVER_OPENED: i32 = 1_000_000;

    let dir = TempDir::new().expect("create temp dir");
    let start = test_start(dir.path(), false);

    let pane_id = PaneId::new();
    let header = ResumeHeader {
        format: RESUME_FORMAT,
        session_id: start.session_id,
        session_name: start.session_name.clone(),
        panes: vec![koshi_runtime::resume::CarriedPane {
            pane_id,
            pid: NO_SUCH_PROCESS,
            rows: 24,
            cols: 80,
            terminal_fd: Some(NEVER_OPENED),
            terminal_name: None,
            exit: None,
        }],
    };

    let refused =
        match take_panes_back(&header, Arc::new(InboxSink::new(mpsc::channel().0)), &start) {
            Err(refused) => refused,
            Ok(_) => panic!("a descriptor this process does not hold cannot be taken back"),
        };

    // What the number names is read before the descriptor is owned, so the
    // refusal names the pane rather than this process closing a number it never
    // opened.
    assert_eq!(
        refused.to_string(),
        format!(
            "pane {pane_id} carried descriptor {NEVER_OPENED}, which names no pseudoterminal \
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

/// A fresh directory to stand in for the runtime dir, under a short base so the
/// Unix socket path bound inside it stays within the operating system's
/// path-length cap. Removed when the test drops it.
#[cfg(unix)]
fn short_runtime_dir() -> TempDir {
    tempfile::Builder::new()
        .prefix("k")
        .tempdir_in(PathBuf::from("/tmp"))
        .expect("a temporary runtime directory")
}

#[cfg(unix)]
#[test]
fn a_rebuild_that_cannot_bind_its_socket_leaves_no_resume_file_behind() {
    // The swap wrote the file and then could neither start a new image nor put
    // this one back in this one. Nothing reads that file again, so it must not
    // stay on the disk.
    let dir = short_runtime_dir();
    let start = test_start(dir.path(), false);
    let (server, inbox_tx) = test_server(Arc::new(FakePtyBackend::new()));
    let panes = Arc::new(PortablePtyBackend::with_sink(Arc::new(InboxSink::new(
        inbox_tx.clone(),
    ))));

    // The address the rebuild must bind, already held, so the rebuild's own
    // bind is refused.
    let other_users =
        koshi_link::config::other_users_policy(koshi_link::config::load_app_layer().as_ref(), None);
    let _holding_the_address = IpcServer::start(
        &start.runtime_dir,
        start.session_id,
        inbox_tx.clone(),
        other_users,
    )
    .expect("the address binds once");

    // The file the swap wrote before it withdrew the socket.
    let resume_file = resume_path(&start.runtime_dir, start.session_id);
    std::fs::write(&resume_file, b"{}").expect("the resume file is written");

    let header = ResumeHeader {
        format: RESUME_FORMAT,
        session_id: start.session_id,
        session_name: start.session_name.clone(),
        panes: Vec::new(),
    };
    let body = ResumeBody {
        sessions: HashMap::new(),
        engines: HashMap::new(),
        undecoded: HashMap::new(),
        quit: None,
    };

    let rebuilt = resume_readers_and_rebuild(server, &panes, &header, body, &start, &inbox_tx);
    assert!(
        rebuilt.is_err(),
        "an address another server already holds cannot be bound"
    );

    assert!(
        !resume_file.exists(),
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

    let dir = TempDir::new().expect("create temp dir");
    let log = dir.path().join("pane.log");
    std::fs::write(&log, b"koshi").expect("the ordinary file is written");
    let mut ordinary_file = std::fs::File::open(&log).expect("open an ordinary file");
    let raw = ordinary_file.as_raw_fd();
    let start = test_start(dir.path(), false);
    let header = ResumeHeader {
        format: RESUME_FORMAT,
        session_id: start.session_id,
        session_name: start.session_name.clone(),
        panes: vec![koshi_runtime::resume::CarriedPane {
            pane_id: PaneId::new(),
            // A process id of zero names no pane child, so neither path
            // signals anything.
            pid: 0,
            rows: 24,
            cols: 80,
            terminal_fd: Some(raw),
            terminal_name: None,
            exit: None,
        }],
    };

    release_carried_panes(&header, &start, Arc::new(InboxSink::new(mpsc::channel().0)));
    assert!(
        unsafe { libc::fcntl(raw, libc::F_GETFD) } >= 0,
        "releasing the carried panes must leave the ordinary file open"
    );

    end_panes_after_failure(&header, 0);
    assert!(
        unsafe { libc::fcntl(raw, libc::F_GETFD) } >= 0,
        "ending the panes after a failure must leave the ordinary file open"
    );

    // The number still reaches the same file, so neither path closed it and
    // handed the number to something else.
    let mut read = String::new();
    ordinary_file
        .read_to_string(&mut read)
        .expect("the ordinary file still reads");
    assert_eq!(read, "koshi");
}

#[cfg(unix)]
#[test]
fn a_pane_whose_terminal_is_not_the_one_the_header_recorded_is_refused() {
    // A number can name a live pseudoterminal master that belongs to another
    // pane, which the kind check alone accepts. The recorded name is what tells
    // this pane's own master from any other.
    let panes = Arc::new(PortablePtyBackend::with_sink(Arc::new(InboxSink::new(
        mpsc::channel().0,
    ))));

    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(master >= 0, "the pseudoterminal master opens");
    let named = terminal_master_name(master).expect("the master names its terminal");
    let pane_id = PaneId::new();
    let carried = koshi_runtime::resume::CarriedPane {
        pane_id,
        pid: NO_SUCH_PROCESS,
        rows: 24,
        cols: 80,
        terminal_fd: Some(master),
        terminal_name: Some("/dev/koshi-another-pane".to_string()),
        exit: None,
    };

    let refused = take_one_pane_back(&panes, &carried)
        .expect_err("a descriptor whose terminal is not the recorded one must be refused");

    assert_eq!(
        refused.to_string(),
        format!(
            "pane {pane_id} carried descriptor {master} as the master of \
             /dev/koshi-another-pane, which is now the master of {named}, so it cannot be \
             taken back"
        )
    );
    assert_eq!(
        terminal_master_name(master),
        Some(named),
        "the refused descriptor must be left open"
    );
    assert_eq!(
        unsafe { libc::close(master) },
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
    let panes = Arc::new(PortablePtyBackend::with_sink(Arc::new(InboxSink::new(
        mpsc::channel().0,
    ))));

    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(master >= 0, "the pseudoterminal master opens");
    let named = terminal_master_name(master).expect("the master names its terminal");
    let pane_id = PaneId::new();
    let carried = koshi_runtime::resume::CarriedPane {
        pane_id,
        pid: NO_SUCH_PROCESS,
        rows: 24,
        cols: 80,
        terminal_fd: Some(master),
        terminal_name: Some(named),
        exit: None,
    };

    let handle = take_one_pane_back(&panes, &carried).expect("the pane is taken back");

    assert_eq!(handle.pane_id(), pane_id);
    // The backend owns the descriptor from here, and closing the pane closes
    // it.
    panes
        .kill(pane_id, KillPolicy::Tree)
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
    let dir = TempDir::new().expect("create temp dir");
    let start = test_start(dir.path(), false);

    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(master >= 0, "the pseudoterminal master opens");
    let named = terminal_master_name(master).expect("the master names its terminal");
    let first = PaneId::new();
    let second = PaneId::new();
    let record = |pane_id| koshi_runtime::resume::CarriedPane {
        pane_id,
        pid: NO_SUCH_PROCESS,
        rows: 24,
        cols: 80,
        terminal_fd: Some(master),
        terminal_name: Some(named.clone()),
        exit: None,
    };
    let header = ResumeHeader {
        format: RESUME_FORMAT,
        session_id: start.session_id,
        session_name: start.session_name.clone(),
        panes: vec![record(first), record(second)],
    };

    let taken = take_panes_back(&header, Arc::new(InboxSink::new(mpsc::channel().0)), &start);

    let read_back = match &taken {
        Err(refused) => refused.to_string(),
        Ok((panes, _handles)) => {
            let owners: Vec<Option<i32>> = panes
                .carried_panes()
                .iter()
                .map(|pane| pane.terminal_fd)
                .collect();
            format!(
                "descriptor {master} is owned by {} panes: {owners:?}",
                owners.len()
            )
        }
    };
    // Dropping what came back closes the descriptor once per owner, and the
    // second close reaches whatever number this test binary opened in between,
    // which ends the whole binary. What came back is leaked instead, so the
    // assertion below is what this test reports.
    std::mem::forget(taken);

    assert_eq!(
        read_back,
        format!(
            "pane {second} carried descriptor {master}, which pane {first} was already taken back \
             on, so it cannot be taken back"
        )
    );
}

/// One carried record for `pane_id`, naming no terminal descriptor. Enough for
/// the checks that read the header alone.
fn carried_record(pane_id: PaneId) -> koshi_runtime::resume::CarriedPane {
    koshi_runtime::resume::CarriedPane {
        pane_id,
        pid: 0,
        rows: 24,
        cols: 80,
        terminal_fd: None,
        terminal_name: None,
        exit: None,
    }
}

/// A header naming `panes`, in that order.
fn header_naming(panes: Vec<koshi_runtime::resume::CarriedPane>) -> ResumeHeader {
    ResumeHeader {
        format: RESUME_FORMAT,
        session_id: SessionId::new(),
        session_name: "S-quiet-lake".to_string(),
        panes,
    }
}

#[test]
fn a_header_naming_one_pane_twice_is_refused_before_any_pane_is_touched() {
    // Every platform runs this check as the first step of taking the panes
    // back, and each platform takes them back its own way, so the check itself
    // is pinned here rather than through either of them.
    let first = PaneId::new();
    let repeated = PaneId::new();
    let last = PaneId::new();

    header_names_each_pane_once(&header_naming(vec![
        carried_record(first),
        carried_record(repeated),
        carried_record(last),
    ]))
    .expect("three panes named once each are taken back");

    let refused = header_names_each_pane_once(&header_naming(vec![
        carried_record(first),
        carried_record(repeated),
        carried_record(repeated),
        carried_record(first),
    ]))
    .expect_err("a header naming a pane twice is refused");

    assert_eq!(
        refused.to_string(),
        format!("pane {repeated} is named twice by the carried state, so it cannot be taken back"),
        "the refusal names the first pane that repeats, not the last"
    );
}

#[test]
fn a_header_naming_no_pane_at_all_names_each_of_them_once() {
    // A session whose last pane closed as the swap was written carries no pane,
    // and the check must let that header through rather than read as empty
    // meaning wrong.
    header_names_each_pane_once(&header_naming(Vec::new()))
        .expect("a header naming no pane is taken back");
}

#[cfg(unix)]
#[test]
fn two_carried_records_naming_one_pane_are_refused_rather_than_taken_back_over_each_other() {
    // A header naming one pane twice would have that pane taken back twice: the
    // second take-back replaces the first pane's entry, so the first pane's
    // reader, writer and watcher are left running with no entry to close them.
    let dir = TempDir::new().expect("create temp dir");
    let start = test_start(dir.path(), false);

    let first_master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    let second_master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(first_master >= 0, "the first pseudoterminal master opens");
    assert!(second_master >= 0, "the second pseudoterminal master opens");
    let pane_id = PaneId::new();
    let record = |terminal_fd| koshi_runtime::resume::CarriedPane {
        pane_id,
        pid: NO_SUCH_PROCESS,
        rows: 24,
        cols: 80,
        terminal_fd: Some(terminal_fd),
        terminal_name: terminal_master_name(terminal_fd),
        exit: None,
    };
    let header = ResumeHeader {
        format: RESUME_FORMAT,
        session_id: start.session_id,
        session_name: start.session_name.clone(),
        panes: vec![record(first_master), record(second_master)],
    };

    let taken = take_panes_back(&header, Arc::new(InboxSink::new(mpsc::channel().0)), &start);

    let refused = match taken {
        Err(refused) => refused,
        Ok((panes, handles)) => panic!(
            "pane {pane_id} was taken back twice; the backend holds {} pane(s) and the caller {} \
             handle(s)",
            panes.carried_panes().len(),
            handles.len()
        ),
    };
    assert_eq!(
        refused.to_string(),
        format!("pane {pane_id} is named twice by the carried state, so it cannot be taken back")
    );
}

#[cfg(unix)]
#[test]
fn a_binary_printing_its_answer_with_no_newline_after_it_is_still_read() {
    // The reader takes the first line the binary prints, and a build that
    // writes its answer and exits without a newline has still answered. The
    // stream ending is what closes the line here, not a newline character.
    use std::os::unix::fs::PermissionsExt as _;

    let dir = TempDir::new().expect("create temp dir");
    let exe = dir.path().join("koshi");
    std::fs::write(
        &exe,
        format!("#!/bin/sh\nprintf '{{\"min\":{RESUME_FORMAT_MIN},\"max\":{RESUME_FORMAT}}}'\n"),
    )
    .expect("the stand-in binary is written");
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))
        .expect("the stand-in binary is runnable");

    assert_eq!(
        resume_support(&exe),
        Ok(ResumeSupport {
            min: RESUME_FORMAT_MIN,
            max: RESUME_FORMAT
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

    let dir = TempDir::new().expect("create temp dir");
    let exe = dir.path().join("koshi");
    std::fs::write(&exe, "#!/bin/sh\nexec sleep 60\n").expect("the stand-in binary is written");
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))
        .expect("the stand-in binary is runnable");

    // Linux refuses to run a file any process holds open for writing, and
    // answers `ETXTBSY`. A sibling test forking between the write above and its
    // own `exec` carries this file's write handle in that window, so the run is
    // retried until it starts. Every attempt that gets that far spends the whole
    // wait, so the timing claim below still holds.
    let deadline = Instant::now() + BUSY_WAIT;
    let (refused, waited) = loop {
        let started = Instant::now();
        let refused = resume_support(&exe);
        let waited = started.elapsed();
        let busy = refused
            .as_ref()
            .err()
            .is_some_and(|refusal| refusal.contains("could not be run"));
        if !busy {
            break (refused, waited);
        }
        assert!(
            Instant::now() < deadline,
            "the stand-in binary never became runnable: {refused:?}"
        );
        std::thread::sleep(BUSY_POLL);
    };

    assert_eq!(
        refused,
        Err(format!(
            "the binary at {} did not say which resume formats it reads within {} seconds",
            exe.display(),
            RESUME_SUPPORT_WAIT.as_secs()
        ))
    );
    // The whole wait ran out, so the refusal came from the binary saying
    // nothing rather than from the reader ending early.
    assert!(
        waited >= RESUME_SUPPORT_WAIT,
        "the refusal must come after the whole wait, and it came after {waited:?}"
    );
}

#[test]
fn a_binary_naming_a_lowest_format_above_its_highest_is_refused() {
    // A pair of numbers that names no format at all. The range is empty, so
    // nothing this build writes is inside it and the swap is refused.
    let exe = Path::new("/opt/koshi/bin/koshi");

    assert_eq!(
        reads_the_format_this_build_writes(ResumeSupport { min: 7, max: 2 }, exe),
        Err(format!(
            "the binary at {} reads resume formats 7 to 2, and this one reads {RESUME_FORMAT_MIN} \
             to {RESUME_FORMAT} and writes {RESUME_FORMAT}",
            exe.display()
        ))
    );
}

#[test]
fn a_restart_accepted_in_the_pass_that_loses_the_last_pane_ends_the_session() {
    // The swap has nothing to carry once the last pane's child is gone, and
    // there is no session left to come back to. The loop's no-panes check runs
    // before the restart check, so the session ends here.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx) = test_server(fake);
    seed(&mut server);
    server.set_restart_check(Arc::new(|| Ok(())));
    let pane_id = *server
        .terminal_engines()
        .keys()
        .next()
        .expect("the root pane holds a terminal engine");

    let reply = ask_to_restart(&tx);
    tx.send(RuntimeEvent::ChildExit {
        pane_id,
        status: ExitStatus::ExitCode(0),
        exited_at: SystemTime::now(),
    })
    .expect("the child's exit is queued");

    let outcome = serve(&mut server);

    assert_eq!(
        reply.try_recv().expect("the loop answered the request"),
        Ok(())
    );
    assert!(server.restart_requested());
    assert!(!server.has_active_panes());
    assert_eq!(
        outcome,
        ServeOutcome::Ended,
        "a session with no pane left ends rather than replacing its image"
    );
}

#[test]
fn a_quit_arriving_with_a_restart_in_one_pass_ends_the_session_instead_of_swapping() {
    // Both requests reach the inbox before the loop reads either. The quit is
    // the one that decides, so the session is not torn down into a swap the
    // user asked to end.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx) = test_server(fake);
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

    let reply = ask_to_restart(&tx);
    tx.send(RuntimeEvent::Ipc {
        envelope: CommandEnvelope::new(
            CommandId::new(),
            CommandSource::ExternalCli {
                session_id: Some(session_id),
                target_client: None,
            },
            SystemTime::now(),
            Command::Quit,
        ),
        reply: mpsc::channel().0,
    })
    .expect("the quit command is queued");

    let outcome = serve(&mut server);

    assert_eq!(
        reply.try_recv().expect("the loop answered the request"),
        Ok(())
    );
    assert!(server.quit_requested());
    assert!(server.restart_requested());
    assert_eq!(outcome, ServeOutcome::Ended);
}

#[test]
fn two_restart_requests_in_one_pass_are_both_answered_and_the_loop_swaps_once() {
    // Two `koshi update` runs can reach one session before its loop reads
    // either. Each caller is answered, and the loop leaves for exactly one
    // swap.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx) = test_server(fake);
    seed(&mut server);
    server.set_restart_check(Arc::new(|| Ok(())));

    let first = ask_to_restart(&tx);
    let second = ask_to_restart(&tx);

    let outcome = serve(&mut server);

    assert_eq!(
        first.try_recv().expect("the loop answered the first"),
        Ok(())
    );
    assert_eq!(
        second.try_recv().expect("the loop answered the second"),
        Ok(())
    );
    assert_eq!(outcome, ServeOutcome::Restart);
    assert!(server.restart_requested());
    assert!(!server.quit_requested());
}

#[test]
fn a_resume_file_stamped_ahead_of_this_machines_clock_reads_as_a_swap_in_flight() {
    // A runtime directory can sit on a filesystem whose clock runs ahead, and a
    // stamp in the future gives no age at all. The session is left alone, so a
    // clock this process cannot trust never costs a live session its endpoint
    // file.
    let dir = TempDir::new().expect("create temp dir");
    let session = SessionId::new();
    let resume_file = resume_path(dir.path(), session);
    std::fs::write(&resume_file, b"{}").expect("the resume file is written");
    let ahead = std::fs::File::options()
        .write(true)
        .open(&resume_file)
        .expect("the resume file opens for writing");
    ahead
        .set_modified(SystemTime::now() + RESTART_WINDOW * 10)
        .expect("the resume file is stamped ahead");

    assert!(
        is_replacing_its_image(dir.path(), session),
        "a stamp this machine's clock has not reached yet reads as fresh"
    );
}

#[test]
fn a_resume_file_exactly_as_old_as_the_window_reads_as_a_swap_that_died() {
    // The window is the boundary the router decides on, so the two sides of it
    // are pinned: a moment younger is a swap in flight, the window itself is a
    // swap that died.
    let dir = TempDir::new().expect("create temp dir");
    let session = SessionId::new();
    let resume_file = resume_path(dir.path(), session);
    std::fs::write(&resume_file, b"{}").expect("the resume file is written");
    let aged = std::fs::File::options()
        .write(true)
        .open(&resume_file)
        .expect("the resume file opens for writing");

    aged.set_modified(SystemTime::now() - RESTART_WINDOW + Duration::from_secs(2))
        .expect("the resume file is aged to just inside the window");
    assert!(
        is_replacing_its_image(dir.path(), session),
        "a resume file younger than the window means a swap is in flight"
    );

    aged.set_modified(SystemTime::now() - RESTART_WINDOW)
        .expect("the resume file is aged to the window itself");
    assert!(
        !is_replacing_its_image(dir.path(), session),
        "a resume file as old as the window means the swap died"
    );
}

#[test]
fn a_resume_file_whose_header_does_not_read_is_refused_and_taken_off_the_disk() {
    // A swap that died part way through its write leaves bytes no build reads.
    // Nothing can be taken back from them and nothing can be ended, and the
    // file holds every pane's screen and scrollback, so it goes.
    let dir = TempDir::new().expect("create temp dir");
    let mut start = test_start(dir.path(), false);
    let resume_file = resume_path(dir.path(), start.session_id);
    std::fs::write(&resume_file, b"{\"header\":{\"format\":1}")
        .expect("the half-written resume file is placed");
    let (inbox_tx, inbox_rx) = mpsc::channel();

    let refused = match resume_from_file(
        &resume_file,
        &mut start,
        None,
        Arc::new(InboxSink::new(inbox_tx.clone())),
        inbox_rx,
        &inbox_tx,
    ) {
        Err(refused) => refused,
        Ok(_) => panic!("a header that does not read cannot start a session"),
    };

    assert_eq!(
        refused.to_string(),
        format!(
            "corrupt stored state: resume state at {} is unreadable: \
             missing field `session_id` at line 1 column 22",
            resume_file.display()
        )
    );
    assert!(
        !resume_file.exists(),
        "the file goes, because no later build reads it either"
    );
}
