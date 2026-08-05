//! Tests for the control-socket server over real sockets: serving lifecycle,
//! handshake gating, fault containment per connection, the reply path from a
//! stand-in dispatcher thread, and what an attached connection's reading half
//! carries.

use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::SystemTime;

use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, ToggleLockModeArgs,
};
use koshi_core::discovery::{SessionInfo, SessionOverview};
use koshi_core::geometry::Size;
use koshi_core::ids::{CommandId, PaneId, SessionId};
use koshi_core::key::{Key, KeyChord, ModFlags};
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::protocol::{EventFilterSpec, WireMouseAction};

use crate::runtime::event::AttachAccepted;

use super::*;

/// The terminal size every attaching client in these tests reports.
const VIEWPORT: Size = Size { cols: 80, rows: 24 };

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

/// The structure a stand-in attach answers with: the session, named, with
/// nothing in it.
fn attached_structure(session_id: SessionId) -> AttachedSessionStructureSnapshot {
    AttachedSessionStructureSnapshot {
        id: session_id,
        name: "attachable".to_string(),
        tabs: Vec::new(),
        panes: Vec::new(),
    }
}

/// A stand-in dispatcher that accepts attaches: it answers every attach as
/// `client_id`, holds the queue it hands out open so the writing thread stays
/// blocked, and closes those queues on a detach the way the real dispatcher
/// does. Every other event it drains is forwarded to the returned receiver, so
/// a test reads exactly what an attached connection sent. Exits when every
/// inbox sender is gone.
fn spawn_attaching_dispatcher(
    inbox_rx: Receiver<RuntimeEvent>,
    client_id: ClientId,
    session_id: SessionId,
) -> (JoinHandle<()>, Receiver<RuntimeEvent>) {
    let (seen_tx, seen_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut queues = Vec::new();
        while let Ok(event) = inbox_rx.recv() {
            match event {
                RuntimeEvent::IpcAttach { reply, .. } => {
                    let (events_tx, events_rx) = mpsc::channel();
                    queues.push(events_tx);
                    let _ = reply.send(Some(AttachAccepted {
                        client_id,
                        session_id,
                        structure: attached_structure(session_id),
                        events: events_rx,
                    }));
                }
                detached @ RuntimeEvent::ClientDetached { .. } => {
                    queues.clear();
                    if seen_tx.send(detached).is_err() {
                        break;
                    }
                }
                other => {
                    if seen_tx.send(other).is_err() {
                        break;
                    }
                }
            }
        }
    });
    (handle, seen_rx)
}

/// A served socket whose stand-in dispatcher accepts an attach as `client_id`,
/// plus the events that attached connection sends the dispatcher.
fn serve_attachable(
    tag: &str,
    client_id: ClientId,
) -> (
    IpcServer,
    SessionId,
    PathBuf,
    JoinHandle<()>,
    Receiver<RuntimeEvent>,
) {
    let runtime_dir = test_runtime_dir(tag);
    let session = SessionId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let (dispatcher, seen) = spawn_attaching_dispatcher(inbox_rx, client_id, session);
    let server = IpcServer::start(&runtime_dir, session, inbox_tx).expect("start serving");
    (server, session, runtime_dir, dispatcher, seen)
}

/// Open a connection, say hello, attach on it, and read both replies back.
/// The connection comes back carrying `client_id`'s stream.
fn attach_to(runtime_dir: &Path, session: SessionId, client_id: ClientId) -> Connection {
    let mut connection = connect_to(runtime_dir, session);
    connection
        .send(&hello_for(runtime_dir, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, IpcResult::Hello);

    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Attach {
                viewport: VIEWPORT,
                filter: EventFilterSpec::All,
            },
        })
        .expect("send attach");
    let attach_reply: IpcResponse = connection.recv().expect("attach reply");
    assert_eq!(attach_reply.request_id, Some(2));
    assert_eq!(
        attach_reply.result,
        IpcResult::Attached {
            client_id,
            session_id: session,
            structure: attached_structure(session),
        },
    );
    connection
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
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
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
fn a_restart_advertises_a_fresh_token_and_refuses_the_old_one() {
    let (server, session, runtime_dir, dispatcher) = serve("restart-token", None);
    let endpoint_path = EndpointFile::path(&runtime_dir, session);
    let first = EndpointFile::read(&endpoint_path).expect("endpoint file readable");
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");

    let (inbox_tx, inbox_rx) = mpsc::channel();
    let restarted_dispatcher = spawn_dispatcher(inbox_rx, None);
    let restarted = IpcServer::start(&runtime_dir, session, inbox_tx).expect("start serving again");
    let second = EndpointFile::read(&endpoint_path).expect("endpoint file readable");
    assert_ne!(
        second.token, first.token,
        "the restarted server advertises a new secret",
    );

    let mut old = connect_to(&runtime_dir, session);
    old.send(&IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            protocol_version: koshi_ipc::protocol::PROTOCOL_VERSION,
            token: first.token,
        },
    })
    .expect("send hello with the token from before the restart");
    let refusal: IpcResponse = old.recv().expect("reply");
    assert_eq!(
        refusal.result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let mut fresh = connect_to(&runtime_dir, session);
    fresh
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello with the new secret");
    let accepted: IpcResponse = fresh.recv().expect("hello reply");
    assert_eq!(accepted.result, IpcResult::Hello);

    drop(old);
    drop(fresh);
    restarted.shutdown();
    restarted_dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_detach_leaves_the_sessions_token_unchanged() {
    let client = ClientId::new();
    let (server, session, runtime_dir, dispatcher, seen) = serve_attachable("detach-token", client);
    let endpoint_path = EndpointFile::path(&runtime_dir, session);
    let before = EndpointFile::read(&endpoint_path).expect("endpoint file readable");

    let attached = attach_to(&runtime_dir, session, client);
    drop(attached);
    let RuntimeEvent::ClientDetached { client_id } = seen.recv().expect("detach event") else {
        panic!("expected ClientDetached");
    };
    assert_eq!(client_id, client);

    let after = EndpointFile::read(&endpoint_path).expect("endpoint file readable");
    assert_eq!(
        after.token, before.token,
        "the detached client's departure leaves the session's secret alone",
    );

    let mut connection = connect_to(&runtime_dir, session);
    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello with the secret from before the detach");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, IpcResult::Hello);

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
fn an_attached_connection_forwards_input_unanswered_and_detaches_on_any_other_request() {
    let client = ClientId::new();
    let (server, session, runtime_dir, dispatcher, seen) =
        serve_attachable("attached-input", client);
    let mut connection = attach_to(&runtime_dir, session, client);
    let pressed = KeyChord::new(ModFlags::CTRL, Key::Char('t'));
    let resized = Size {
        cols: 120,
        rows: 40,
    };
    let env = envelope();

    connection
        .send(&IpcRequest {
            request_id: 3,
            kind: IpcRequestKind::KeyPress { chord: pressed },
        })
        .expect("send key press");
    let RuntimeEvent::ClientKeyPress { client_id, chord } = seen.recv().expect("key press event")
    else {
        panic!("expected ClientKeyPress");
    };
    assert_eq!(client_id, client);
    assert_eq!(chord, pressed);

    connection
        .send(&IpcRequest {
            request_id: 4,
            kind: IpcRequestKind::Resize { viewport: resized },
        })
        .expect("send resize");
    let RuntimeEvent::Resize { client_id, size } = seen.recv().expect("resize event") else {
        panic!("expected Resize");
    };
    assert_eq!(client_id, client);
    assert_eq!(size, resized);

    connection
        .send(&IpcRequest {
            request_id: 5,
            kind: IpcRequestKind::SubmitCommand(Box::new(env.clone())),
        })
        .expect("send submit");
    let RuntimeEvent::Ipc { envelope, reply } = seen.recv().expect("submit event") else {
        panic!("expected Ipc");
    };
    assert_eq!(envelope, env);
    assert!(
        reply
            .send(CommandResult::Ok {
                command_id: env.id,
                emitted_events: Vec::new(),
            })
            .is_err(),
        "the reply channel's receiving end is already gone",
    );

    let round = vec![WireMouseAction::Scroll {
        pane: PaneId::new(),
        up: true,
        lines: 3,
    }];
    connection
        .send(&IpcRequest {
            request_id: 6,
            kind: IpcRequestKind::Mouse(round.clone()),
        })
        .expect("send mouse round");
    let RuntimeEvent::ClientMouse {
        client_id,
        request_id,
        actions,
    } = seen.recv().expect("mouse round event")
    else {
        panic!("expected ClientMouse");
    };
    assert_eq!(client_id, client);
    assert_eq!(request_id, 6, "the round's own id crosses with it");
    assert_eq!(actions, round);

    connection
        .send(&IpcRequest {
            request_id: 7,
            kind: IpcRequestKind::Paste {
                text: String::from("hello\nworld"),
            },
        })
        .expect("send paste");
    let RuntimeEvent::HostPaste { client_id, text } = seen.recv().expect("paste event") else {
        panic!("expected HostPaste");
    };
    assert_eq!(client_id, client);
    assert_eq!(text, "hello\nworld");

    // A kind the reading half does not forward ends it, which detaches.
    connection
        .send(&IpcRequest {
            request_id: 8,
            kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");
    let RuntimeEvent::ClientDetached { client_id } = seen.recv().expect("detach event") else {
        panic!("expected ClientDetached");
    };
    assert_eq!(client_id, client);

    // The goodbye is the first frame after the attach reply, so none of the
    // five requests above was answered with an `IpcResponse`.
    assert_eq!(
        connection.recv::<SessionEvent>().expect("goodbye frame"),
        SessionEvent::Detached,
    );
    assert!(
        matches!(
            connection.recv::<SessionEvent>(),
            Err(IpcError::Disconnected),
        ),
        "the stream ends after the goodbye",
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_mouse_round_before_an_attach_closes_the_connection() {
    // A round names no client until the connection carries one, so it belongs
    // on an attached connection only.
    let (server, session, runtime_dir, dispatcher) = serve("mouse-unattached", None);
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, IpcResult::Hello);

    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                pane: PaneId::new(),
                up: true,
                lines: 3,
            }]),
        })
        .expect("send mouse round");
    assert!(
        connection.recv::<IpcResponse>().is_err(),
        "no reply comes back, and the connection is closed",
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
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
    assert_eq!(endpoint.pid, std::process::id());
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

#[test]
fn dropping_the_server_without_shutdown_still_removes_both_files() {
    let (server, session, runtime_dir, dispatcher) = serve("drop-cleans", None);
    let endpoint_path = EndpointFile::path(&runtime_dir, session);
    let endpoint = EndpointFile::read(&endpoint_path).expect("endpoint file readable");

    drop(server);
    dispatcher.join().expect("dispatcher exits");

    assert!(!endpoint_path.exists(), "endpoint file gone after drop");
    assert!(
        matches!(
            Connection::connect(&endpoint.socket),
            Err(IpcError::NoListener { .. }),
        ),
        "nothing listens after drop",
    );
    cleanup(&runtime_dir);
}

#[cfg(unix)]
#[test]
fn shutdown_returns_and_removes_the_endpoint_even_when_the_wake_cannot_connect() {
    let (server, session, runtime_dir, dispatcher) = serve("wake-fails", None);
    let endpoint_path = EndpointFile::path(&runtime_dir, session);
    let endpoint = EndpointFile::read(&endpoint_path).expect("endpoint file readable");

    // Unlink the socket file out from under the listener: the wake connect
    // inside shutdown now fails, so shutdown must skip the join instead of
    // waiting forever on the still-blocked accept loop.
    std::fs::remove_file(&endpoint.socket).expect("unlink the live socket");

    server.shutdown();

    assert!(
        !endpoint_path.exists(),
        "endpoint file gone even though the accept loop could not be woken",
    );
    drop(dispatcher);
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
