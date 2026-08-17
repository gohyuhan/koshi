//! What an attached client reads when its session moves it to another one.
//!
//! A real `koshi` binary runs as the router, the router starts two real
//! session servers, and the test joins the first the way the attached client
//! does — Hello then Attach on one connection. It then submits
//! `SwitchSession` naming the second session on that same connection and reads
//! the event stream until a frame ends it.
//!
//! What this covers is the wire and the frame: the command crossing a real
//! socket, and the exact frame the session server writes back. The command
//! arrives here from an attached client, which needs no pane to exist; the
//! source a `koshi attach <session>` typed inside a pane really sends is
//! covered by the dispatcher's own tests, which can build a live pane to send
//! it from.
//!
//! Every test serves its own temporary runtime directory, so the routers here
//! never meet the one a developer is running. The directory sits under a short
//! base because a Unix socket path has an operating-system length cap that a
//! deep temporary path would break.
//!
//! Reading a frame blocks forever, so the walk to the ending runs on a thread
//! this one can stop waiting on: a stream that never ends fails the test
//! instead of hanging it.
//!
//! Every process a test starts is held in a guard that ends it when the test
//! drops it, so a failed assertion leaves nothing running.

use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use koshi_core::command::{Command, CommandEnvelope, CommandSource, SwitchSessionArgs};
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, CommandId, SessionId};
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::error::IpcError;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::protocol::{
    EventFilterSpec, IpcRequest, IpcRequestKind, IpcResponse, IpcResult, MIN_PROTOCOL_VERSION,
    PROTOCOL_VERSION,
};
use koshi_ipc::router::{
    router_endpoint_path, RouterRequest, RouterRequestKind, RouterResponse, RouterResult,
    SessionAddress, MIN_ROUTER_PROTOCOL_VERSION, ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::transport::Connection;
use tempfile::TempDir;

mod common;

use common::end_process;

/// How long a poll waits for something a started process has to do before the
/// test calls it a failure.
const WAIT: Duration = Duration::from_secs(20);

/// How long a poll pauses between attempts.
const POLL: Duration = Duration::from_millis(100);

/// The terminal size the attaching client in this test reports.
const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// A fresh directory to serve, under a short base so the Unix socket path
/// stays inside the operating system's path-length cap. Removed when the test
/// drops it.
fn test_runtime_dir() -> TempDir {
    #[cfg(unix)]
    let base = std::path::PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    TempDir::new_in(base).expect("a temporary runtime directory")
}

/// A router the test started. Dropping it ends that router.
struct RunningRouter(Child);

impl Drop for RunningRouter {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The session servers a test made a router start. Dropping it ends them, so a
/// test that kills its router leaves no session server behind.
struct RunningSessions(Vec<u32>);

impl Drop for RunningSessions {
    fn drop(&mut self) {
        for pid in &self.0 {
            end_process(*pid);
        }
    }
}

/// Start the `koshi` binary as the router serving `runtime_dir`.
fn start_router(runtime_dir: &Path) -> RunningRouter {
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_koshi"))
        .arg("serve-router")
        .arg("--runtime-dir")
        .arg(runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the koshi binary starts");
    RunningRouter(child)
}

/// Open a connection to the router serving `runtime_dir`, with its handshake
/// already done, retrying until one answers.
fn router_connect(runtime_dir: &Path) -> Connection {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(connection) = try_router_connect(runtime_dir) {
            return connection;
        }
        assert!(
            Instant::now() < deadline,
            "no router answered in {}",
            runtime_dir.display()
        );
        std::thread::sleep(POLL);
    }
}

/// One attempt at opening a router connection: read the endpoint file,
/// connect, and send the Hello that opens the connection.
///
/// `None` means no router answered yet; the next attempt reads the file again.
fn try_router_connect(runtime_dir: &Path) -> Option<Connection> {
    let endpoint = EndpointFile::read(&router_endpoint_path(runtime_dir)).ok()?;
    let mut connection = Connection::connect(&endpoint.socket).ok()?;
    let hello = RouterRequest {
        request_id: 1,
        kind: RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            token: endpoint.token,
        },
    };
    connection.send(&hello).ok()?;
    let reply: RouterResponse = connection.recv().ok()?;
    match reply.result {
        RouterResult::Hello { .. } => Some(connection),
        RouterResult::Error(_) => None,
        other => panic!("the Hello was answered with {other:?}"),
    }
}

/// Ask the router for a new session and hand back where it listens.
fn create_session(connection: &mut Connection, request_id: u64) -> SessionAddress {
    let request = RouterRequest {
        request_id,
        kind: RouterRequestKind::CreateSession {
            profile: None,
            cwd: None,
            allow_other_users: None,
        },
    };
    connection
        .send(&request)
        .expect("the router reads the request");
    let reply: RouterResponse = connection.recv().expect("the router answers the request");
    assert_eq!(reply.request_id, Some(request_id));
    match reply.result {
        RouterResult::Created(address) => address,
        other => panic!("creating a session was answered with {other:?}"),
    }
}

/// Open a connection to the session server, with its handshake already done,
/// retrying until the server answers.
fn open(runtime_dir: &Path, session_id: SessionId) -> Connection {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(connection) = try_open(runtime_dir, session_id) {
            return connection;
        }
        assert!(
            Instant::now() < deadline,
            "no session server answered for {session_id}"
        );
        std::thread::sleep(POLL);
    }
}

/// One attempt at opening a connection: read the endpoint file, connect, and
/// send the Hello that opens the connection.
///
/// `None` means the session server has yet to bind its socket and advertise
/// the token the Hello presents; the next attempt reads the file again.
fn try_open(runtime_dir: &Path, session_id: SessionId) -> Option<Connection> {
    let endpoint = EndpointFile::read(&EndpointFile::path(runtime_dir, session_id)).ok()?;
    let mut connection = Connection::connect(&endpoint.socket).ok()?;
    let hello = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            token: endpoint.token,
            remote: false,
        },
    };
    connection.send(&hello).ok()?;
    let reply: IpcResponse = connection.recv().ok()?;
    match reply.result {
        IpcResult::Hello { .. } => Some(connection),
        other => panic!("the Hello was answered with {other:?}"),
    }
}

/// Attach on `connection` the way the attached client does, and hand back the
/// client the server minted. The connection carries only that client's event
/// stream and that client's own input afterwards.
fn attach(connection: &mut Connection, session_id: SessionId) -> ClientId {
    let request = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::Attach {
            viewport: VIEWPORT,
            filter: EventFilterSpec::All,
            resume: None,
            resume_token: None,
        },
    };
    connection
        .send(&request)
        .expect("the server reads the attach");
    let reply: IpcResponse = connection.recv().expect("the server answers the attach");
    assert_eq!(reply.request_id, Some(2));
    let IpcResult::Attached {
        client_id,
        session_id: joined,
        ..
    } = reply.result
    else {
        panic!("expected an attach reply, got {:?}", reply.result);
    };
    assert_eq!(joined, session_id);
    client_id
}

/// Send `command` up the attached client's own connection, attributed to
/// `client_id`. The streaming half writes no reply, so the answer is whatever
/// the session puts on the event stream.
fn submit(connection: &mut Connection, client_id: ClientId, command: Command) {
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::key_binding(client_id),
        SystemTime::now(),
        command,
    );
    let request = IpcRequest {
        request_id: 3,
        kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
    };
    connection
        .send(&request)
        .expect("the server reads the command");
}

/// Read `connection`'s event stream the way the attached client reads it — a
/// frame that says nothing about the ending is passed over — and hand back the
/// frame or the read failure that ended it. Fails the test once [`WAIT`] has
/// passed with no ending.
fn stream_ending(mut connection: Connection) -> Result<SessionEvent, IpcError> {
    let (ended_tx, ended_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let ending = loop {
            match connection.recv::<SessionEvent>() {
                Ok(SessionEvent::Detached) => break Ok(SessionEvent::Detached),
                Ok(SessionEvent::Quit) => break Ok(SessionEvent::Quit),
                Ok(SessionEvent::SwitchTo { session_id }) => {
                    break Ok(SessionEvent::SwitchTo { session_id })
                }
                Ok(_) => {}
                Err(error) => break Err(error),
            }
        };
        let _ = ended_tx.send(ending);
    });
    ended_rx.recv_timeout(WAIT).expect("the event stream ends")
}

#[test]
fn a_switch_ends_the_stream_with_the_session_to_join_next() {
    let dir = test_runtime_dir();
    let _router = start_router(dir.path());
    let mut router = router_connect(dir.path());

    let first = create_session(&mut router, 2);
    let second = create_session(&mut router, 3);
    let _sessions = RunningSessions(vec![first.pid, second.pid]);

    let mut viewer = open(dir.path(), first.id);
    let client_id = attach(&mut viewer, first.id);

    submit(
        &mut viewer,
        client_id,
        Command::SwitchSession(SwitchSessionArgs {
            client: None,
            session: second.id,
        }),
    );

    assert_eq!(
        stream_ending(viewer).expect("the stream ends with a frame"),
        SessionEvent::SwitchTo {
            session_id: second.id
        }
    );
}
