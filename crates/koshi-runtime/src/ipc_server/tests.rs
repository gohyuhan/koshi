//! Tests for the control-socket server over real sockets: serving lifecycle,
//! handshake gating, fault containment per connection, and the reply path
//! from a stand-in dispatcher thread.

use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::SystemTime;

use koshi_core::command::{Command, CommandEnvelope, CommandResult, CommandSource};
use koshi_core::discovery::{SessionInfo, SessionOverview};
use koshi_core::ids::{CommandId, SessionId};

use super::*;

/// A fresh directory to stand in for the runtime dir, under a short base so
/// the Unix socket path stays inside the OS path-length cap.
/// [`IpcServer::start`] creates it private itself.
fn test_runtime_dir(tag: &str) -> PathBuf {
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    base.join(format!("koshi-serve-{}-{tag}", std::process::id()))
}

/// Remove a test's runtime dir once it is done with it.
fn cleanup(runtime_dir: &Path) {
    let _ = std::fs::remove_dir_all(runtime_dir);
}

/// A stand-in for the dispatcher thread: drains the inbox, answers every
/// submitted command with `Ok` echoing its id, and every discovery request
/// with `overview`. Exits when every inbox sender is gone.
fn spawn_dispatcher(
    inbox_rx: Receiver<RuntimeEvent>,
    overview: Option<SessionOverview>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while let Ok(event) = inbox_rx.recv() {
            match event {
                RuntimeEvent::Ipc { envelope, reply } => {
                    let _ = reply.send(CommandResult::Ok {
                        command_id: envelope.id,
                        emitted_events: Vec::new(),
                    });
                }
                RuntimeEvent::IpcDiscovery { reply } => {
                    let _ = reply.send(overview.clone());
                }
                _ => {}
            }
        }
    })
}

/// A served socket in a fresh runtime dir, with a stand-in dispatcher
/// answering `overview`, plus everything a test needs to talk and clean up.
fn serve(
    tag: &str,
    overview: Option<SessionOverview>,
) -> (IpcServer, SessionId, PathBuf, JoinHandle<()>) {
    let runtime_dir = test_runtime_dir(tag);
    let session = SessionId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher = spawn_dispatcher(inbox_rx, overview);
    let server = IpcServer::start(&runtime_dir, session, inbox_tx).expect("start serving");
    (server, session, runtime_dir, dispatcher)
}

/// A deterministic envelope for submissions.
fn envelope() -> CommandEnvelope {
    CommandEnvelope::new(
        CommandId::new(),
        CommandSource::Internal,
        SystemTime::UNIX_EPOCH,
        Command::ToggleLockMode,
    )
}

/// The Hello that matches the endpoint file at `runtime_dir` for `session`.
fn hello_for(runtime_dir: &Path, session: SessionId) -> IpcRequest {
    let endpoint = EndpointFile::read(&EndpointFile::path(runtime_dir, session))
        .expect("endpoint file readable");
    IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            protocol_version: koshi_ipc::protocol::PROTOCOL_VERSION,
            token: endpoint.token,
        },
    }
}

/// Connect to the socket the endpoint file at `runtime_dir` advertises.
fn connect_to(runtime_dir: &Path, session: SessionId) -> Connection {
    let endpoint = EndpointFile::read(&EndpointFile::path(runtime_dir, session))
        .expect("endpoint file readable");
    Connection::connect(&endpoint.socket).expect("connect")
}

/// A tiny overview to answer discovery with, distinguishable by its name.
fn overview_named(name: &str) -> SessionOverview {
    SessionOverview {
        session: SessionInfo {
            id: SessionId::new(),
            name: name.to_string(),
            created_at: SystemTime::UNIX_EPOCH,
            attached_clients: Vec::new(),
            pane_count: 0,
        },
        tabs: Vec::new(),
        panes: Vec::new(),
        clients: Vec::new(),
    }
}

#[test]
fn a_submitted_command_round_trips_with_the_dispatchers_result() {
    let (server, session, runtime_dir, dispatcher) = serve("roundtrip", None);
    let mut connection = connect_to(&runtime_dir, session);
    let env = envelope();

    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::SubmitCommand(Box::new(env.clone())),
        })
        .expect("send submit");

    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.request_id, Some(1));
    assert_eq!(hello_reply.result, IpcResult::Hello);

    let submit_reply: IpcResponse = connection.recv().expect("submit reply");
    assert_eq!(submit_reply.request_id, Some(2));
    assert_eq!(
        submit_reply.result,
        IpcResult::CommandResult(CommandResult::Ok {
            command_id: env.id,
            emitted_events: Vec::new(),
        }),
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_request_before_hello_is_refused_and_the_connection_keeps_serving() {
    let (server, session, runtime_dir, dispatcher) = serve("hello-first", None);
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&IpcRequest {
            request_id: 7,
            kind: IpcRequestKind::SubmitCommand(Box::new(envelope())),
        })
        .expect("send submit before hello");
    let refusal: IpcResponse = connection.recv().expect("refusal reply");
    assert_eq!(refusal.request_id, Some(7));
    assert_eq!(
        refusal.result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "SubmitCommand arrived before a Hello opened the connection".to_string(),
        }),
    );

    // The same connection still serves: a Hello opens it and a submit works.
    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, IpcResult::Hello);

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_wrong_token_is_refused_as_bad_token() {
    let (server, session, runtime_dir, dispatcher) = serve("bad-token", None);
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&IpcRequest {
            request_id: 1,
            kind: IpcRequestKind::Hello {
                protocol_version: koshi_ipc::protocol::PROTOCOL_VERSION,
                token: ConnectionToken::new("not-the-secret"),
            },
        })
        .expect("send hello");
    let reply: IpcResponse = connection.recv().expect("reply");
    assert_eq!(
        reply.result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_malformed_frame_is_answered_and_the_connection_keeps_serving() {
    let (server, session, runtime_dir, dispatcher) = serve("malformed", None);
    let mut connection = connect_to(&runtime_dir, session);

    // A well-framed message that is not an `IpcRequest` at all.
    connection.send(&"not a request").expect("send junk frame");
    let reply: IpcResponse = connection.recv().expect("refusal reply");
    assert_eq!(reply.request_id, None);
    assert_eq!(
        reply.result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the bytes received are not a request this build can read".to_string(),
        }),
    );

    // The stream is still aligned: the same connection opens and serves.
    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, IpcResult::Hello);

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn an_oversize_frame_closes_the_connection() {
    let (server, session, runtime_dir, dispatcher) = serve("oversize", None);
    let endpoint = EndpointFile::read(&EndpointFile::path(&runtime_dir, session))
        .expect("endpoint file readable");

    // A raw stream, so the length prefix can lie past the cap without a
    // payload behind it.
    let mut raw = raw_connect(&endpoint.socket);
    let oversize = (koshi_ipc::transport::MAX_FRAME_LEN + 1).to_be_bytes();
    std::io::Write::write_all(&mut raw, &oversize).expect("write oversize header");

    // The server closes: the next read finds the stream at end.
    let mut buf = [0u8; 1];
    let closed = match std::io::Read::read(&mut raw, &mut buf) {
        Ok(0) => true,
        Ok(_) => false,
        Err(_) => true,
    };
    assert!(
        closed,
        "the connection must be closed after an oversize frame"
    );

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

/// Open the control socket as a raw byte stream, bypassing the framed
/// [`Connection`], so a test can write a corrupt frame header.
#[cfg(unix)]
fn raw_connect(addr: &str) -> std::os::unix::net::UnixStream {
    std::os::unix::net::UnixStream::connect(addr).expect("raw connect")
}

/// Open the control socket as a raw byte stream, bypassing the framed
/// [`Connection`], so a test can write a corrupt frame header. The bare pipe
/// name is served at `\\.\pipe\<name>`.
#[cfg(windows)]
fn raw_connect(addr: &str) -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!(r"\\.\pipe\{addr}"))
        .expect("raw connect")
}

#[test]
fn discovery_answers_with_the_dispatchers_overview() {
    let (server, session, runtime_dir, dispatcher) =
        serve("discovery", Some(overview_named("workspace")));
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");

    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, IpcResult::Hello);
    let discovery_reply: IpcResponse = connection.recv().expect("discovery reply");
    let IpcResult::Overview(overview) = discovery_reply.result else {
        panic!("expected an overview, got {:?}", discovery_reply.result);
    };
    assert_eq!(overview.session.name, "workspace");

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn discovery_with_no_running_session_closes_the_connection() {
    let (server, session, runtime_dir, dispatcher) = serve("discovery-none", None);
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, IpcResult::Hello);

    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");
    assert!(
        connection.recv::<IpcResponse>().is_err(),
        "no reply comes back once no session is running",
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_gone_dispatcher_closes_the_connection_instead_of_answering() {
    let runtime_dir = test_runtime_dir("no-dispatcher");
    let session = SessionId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    drop(inbox_rx);
    let server = IpcServer::start(&runtime_dir, session, inbox_tx).expect("start serving");
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, IpcResult::Hello);

    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::SubmitCommand(Box::new(envelope())),
        })
        .expect("send submit");
    assert!(
        connection.recv::<IpcResponse>().is_err(),
        "no reply comes back once the dispatcher is gone",
    );

    drop(connection);
    server.shutdown();
    cleanup(&runtime_dir);
}

#[test]
fn the_endpoint_file_lives_while_serving_and_both_files_go_at_shutdown() {
    let (server, session, runtime_dir, dispatcher) = serve("lifecycle", None);
    let endpoint_path = EndpointFile::path(&runtime_dir, session);
    let endpoint = EndpointFile::read(&endpoint_path).expect("endpoint file readable");

    assert!(
        endpoint_path.exists(),
        "endpoint file present while serving"
    );
    #[cfg(unix)]
    assert!(
        Path::new(&endpoint.socket).exists(),
        "socket file present while serving",
    );

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");

    assert!(!endpoint_path.exists(), "endpoint file gone after shutdown");
    #[cfg(unix)]
    assert!(
        !Path::new(&endpoint.socket).exists(),
        "socket file gone after shutdown",
    );
    assert!(
        matches!(
            Connection::connect(&endpoint.socket),
            Err(IpcError::NoListener { .. }),
        ),
        "nothing listens after shutdown",
    );
    cleanup(&runtime_dir);
}

#[cfg(unix)]
#[test]
fn a_leftover_socket_file_is_reclaimed_at_start() {
    let runtime_dir = test_runtime_dir("reclaim");
    koshi_paths::ensure_private_dir(&runtime_dir).expect("create runtime dir");
    let session = SessionId::new();
    let addr = socket_addr(&runtime_dir, session);
    std::fs::write(&addr, b"").expect("plant a leftover file at the socket path");

    let (inbox_tx, _inbox_rx) = mpsc::channel();
    let server = IpcServer::start(&runtime_dir, session, inbox_tx)
        .expect("start reclaims the leftover and serves");

    server.shutdown();
    cleanup(&runtime_dir);
}

#[test]
fn a_second_start_on_the_same_session_is_refused_while_serving() {
    let (server, session, runtime_dir, dispatcher) = serve("busy", None);

    let (inbox_tx, _inbox_rx) = mpsc::channel();
    assert!(
        matches!(
            IpcServer::start(&runtime_dir, session, inbox_tx),
            Err(IpcError::SocketBusy { .. }),
        ),
        "the live listener must refuse a second bind",
    );

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}
