//! Tests for the session-server loop, driven headlessly: a fake PTY backend
//! stands in for real children, so the real inbox loop runs without spawning a
//! process or binding a socket. Binding the socket and printing the ready line
//! need a whole process, so they are covered by the integration tests instead.

use super::*;

use koshi_core::command::{Command, CommandEnvelope, CommandResult, CommandSource};
use koshi_core::ids::CommandId;
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

#[test]
fn the_session_answers_discovery_with_the_id_and_name_it_was_started_with() {
    // The id and the name are picked outside this process and handed to it at
    // startup, so a session that generated either one itself would answer a
    // lookup under a name no caller asked for.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, tx) = test_server(fake);
    let session_id = SessionId::new();
    server
        .bootstrap_local_named(
            session_id,
            "quiet-lake".to_string(),
            STARTING_VIEWPORT,
            SystemTime::now(),
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
        .bootstrap_local_named(
            session_id,
            "quiet-lake".to_string(),
            STARTING_VIEWPORT,
            SystemTime::now(),
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
