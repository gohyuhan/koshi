//! Tests for the session-server loop, driven headlessly: a fake PTY backend
//! stands in for real children, so the real inbox loop runs without spawning a
//! process or binding a socket. Binding the socket and printing the ready line
//! need a whole process, so they are covered by the integration tests instead.

use super::*;

use koshi_core::command::{Command, CommandEnvelope, CommandResult, CommandSource};
use koshi_core::ids::CommandId;
use koshi_core::process::ExitStatus;
use koshi_renderer::snapshot::Delivery;
use koshi_runtime::runtime::bus::EventFilter;
use koshi_runtime::runtime::event::AttachAccepted;
use koshi_test_support::fake_pty::FakePtyBackend;

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
        viewport: STARTING_VIEWPORT,
        filter: EventFilter::All,
        attached_at: SystemTime::now(),
        reply: reply_tx,
    });
    reply_rx
        .try_recv()
        .expect("the loop answered the attach request")
        .expect("a session is running")
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
