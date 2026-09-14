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
    resolve_router_endpoint_path, RouterRequest, RouterRequestKind, RouterResponse, RouterResult,
    SessionAddress, MIN_ROUTER_PROTOCOL_VERSION, ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::transport::Connection;

mod common;

use common::terminate_process;
use koshi_test_support::fixtures::build_test_runtime_directory;

/// How long a poll waits for something a started process has to do before the
/// test calls it a failure.
const WAIT_DURATION: Duration = Duration::from_secs(20);

/// How long a poll pauses between attempts.
const SESSION_SWITCH_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(100);

/// The terminal size the attaching client in this test reports.
const ATTACH_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// A router the test started. Dropping it ends that router.
struct RunningRouter {
    child_process: Child,
}

impl Drop for RunningRouter {
    fn drop(&mut self) {
        let _ = self.child_process.kill();
        let _ = self.child_process.wait();
    }
}

/// The session servers a test made a router start. Dropping it ends them, so a
/// test that kills its router leaves no session server behind.
struct RunningSessions {
    session_server_process_ids: Vec<u32>,
}

impl Drop for RunningSessions {
    fn drop(&mut self) {
        for process_id in &self.session_server_process_ids {
            terminate_process(*process_id);
        }
    }
}

/// Start the `koshi` binary as the router serving `runtime_directory`.
fn start_router_process(runtime_directory: &Path) -> RunningRouter {
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_koshi"))
        .arg("serve-router")
        .arg("--runtime-dir")
        .arg(runtime_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the koshi binary starts");
    RunningRouter {
        child_process: child,
    }
}

/// Open a connection to the router serving `runtime_directory`, with its handshake
/// already done, retrying until one answers.
fn connect_to_router(runtime_directory: &Path) -> Connection {
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Some(connection) = try_connect_to_router(runtime_directory) {
            return connection;
        }
        assert!(
            Instant::now() < deadline,
            "no router answered in {}",
            runtime_directory.display()
        );
        std::thread::sleep(SESSION_SWITCH_POLL_INTERVAL_DURATION);
    }
}

/// One attempt at opening a router connection: read the endpoint file,
/// connect, and send the Hello that opens the connection.
///
/// `None` means no router answered yet; the next attempt reads the file again.
fn try_connect_to_router(runtime_directory: &Path) -> Option<Connection> {
    let endpoint =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory)).ok()?;
    let mut connection = Connection::connect(&endpoint.socket_address).ok()?;
    let hello = RouterRequest {
        request_id: 1,
        request_kind: RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            connection_token: endpoint.connection_token,
        },
    };
    connection.send(&hello).ok()?;
    let router_response: RouterResponse = connection.recv().ok()?;
    match router_response.answer_result {
        RouterResult::Hello { .. } => Some(connection),
        RouterResult::Error(_) => None,
        unexpected_result => panic!("the Hello was answered with {unexpected_result:?}"),
    }
}

/// Ask the router for a new session and hand back where it listens.
fn build_session(connection: &mut Connection, request_id: u64) -> SessionAddress {
    let request = RouterRequest {
        request_id,
        request_kind: RouterRequestKind::CreateSession {
            profile: None,
            working_directory: None,
            is_other_user_access_allowed: None,
        },
    };
    connection
        .send(&request)
        .expect("the router reads the request");
    let router_response: RouterResponse =
        connection.recv().expect("the router answers the request");
    assert_eq!(router_response.request_id, Some(request_id));
    match router_response.answer_result {
        RouterResult::Created(address) => address,
        unexpected_result => {
            panic!("creating a session was answered with {unexpected_result:?}")
        }
    }
}

/// Open a connection to the session server, with its handshake already done,
/// retrying until the server answers.
fn open_session_connection(runtime_directory: &Path, session_id: SessionId) -> Connection {
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Some(connection) = try_open_session_connection(runtime_directory, session_id) {
            return connection;
        }
        assert!(
            Instant::now() < deadline,
            "no session server answered for {session_id}"
        );
        std::thread::sleep(SESSION_SWITCH_POLL_INTERVAL_DURATION);
    }
}

/// One attempt at opening a connection: read the endpoint file, connect, and
/// send the Hello that opens the connection.
///
/// `None` means the session server has yet to bind its socket and advertise
/// the token the Hello presents; the next attempt reads the file again.
fn try_open_session_connection(
    runtime_directory: &Path,
    session_id: SessionId,
) -> Option<Connection> {
    let endpoint = EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .ok()?;
    let mut connection = Connection::connect(&endpoint.socket_address).ok()?;
    let hello = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            connection_token: endpoint.connection_token,
            is_remote: false,
        },
    };
    connection.send(&hello).ok()?;
    let ipc_response: IpcResponse = connection.recv().ok()?;
    match ipc_response.answer_result {
        IpcResult::Hello { .. } => Some(connection),
        unexpected_result => panic!("the Hello was answered with {unexpected_result:?}"),
    }
}

/// Attach on `connection` the way the attached client does, and hand back the
/// client the server minted. The connection carries only that client's event
/// stream and that client's own input afterwards.
fn attach_test_client(connection: &mut Connection, session_id: SessionId) -> ClientId {
    let request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Attach {
            viewport: ATTACH_VIEWPORT_SIZE,
            event_filter: EventFilterSpec::All,
            resume_client_id: None,
            resume_token: None,
            pane_area: None,
            graphics_capabilities: koshi_ipc::protocol::GraphicsCapabilities::default(),
            cell_size: None,
        },
    };
    connection
        .send(&request)
        .expect("the server reads the attach");
    let ipc_response: IpcResponse = connection.recv().expect("the server answers the attach");
    assert_eq!(ipc_response.request_id, Some(2));
    let IpcResult::Attached {
        client_id,
        session_id: joined,
        ..
    } = ipc_response.answer_result
    else {
        panic!(
            "expected an attach reply, got {:?}",
            ipc_response.answer_result
        );
    };
    assert_eq!(joined, session_id);
    client_id
}

/// Send `command` up the attached client's own connection, attributed to
/// `client_id`. The streaming half writes no reply, so the answer is whatever
/// the session puts on the event stream.
fn submit_session_command(connection: &mut Connection, client_id: ClientId, command: Command) {
    let envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        SystemTime::now(),
        command,
    );
    let request = IpcRequest {
        request_id: 3,
        request_kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
    };
    connection
        .send(&request)
        .expect("the server reads the command");
}

/// Read `connection`'s event stream the way the attached client reads it — a
/// frame that says nothing about the ending is passed over — and hand back the
/// frame or the read failure that ended it. Fails the test once [`WAIT_DURATION`] has
/// passed with no ending.
fn read_session_ending(mut connection: Connection) -> Result<SessionEvent, IpcError> {
    let (ending_tx, ending_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let ending = loop {
            match connection.recv::<SessionEvent>() {
                Ok(SessionEvent::Detached) => break Ok(SessionEvent::Detached),
                Ok(SessionEvent::Quit) => break Ok(SessionEvent::Quit),
                Ok(SessionEvent::SwitchTo { session_id }) => {
                    break Ok(SessionEvent::SwitchTo { session_id })
                }
                Ok(_) => {}
                Err(receive_error) => break Err(receive_error),
            }
        };
        let _ = ending_tx.send(ending);
    });
    ending_rx
        .recv_timeout(WAIT_DURATION)
        .expect("the event stream ends")
}

#[test]
fn a_switch_ends_the_stream_with_the_session_to_join_next() {
    let runtime_directory = build_test_runtime_directory();
    let _router = start_router_process(runtime_directory.path());
    let mut router = connect_to_router(runtime_directory.path());

    let source_session = build_session(&mut router, 2);
    let target_session = build_session(&mut router, 3);
    let _sessions = RunningSessions {
        session_server_process_ids: vec![source_session.process_id, target_session.process_id],
    };

    let mut source_connection =
        open_session_connection(runtime_directory.path(), source_session.session_id);
    let client_id = attach_test_client(&mut source_connection, source_session.session_id);

    submit_session_command(
        &mut source_connection,
        client_id,
        Command::SwitchSession(SwitchSessionArgs {
            client_id: None,
            session_id: target_session.session_id,
        }),
    );

    assert_eq!(
        read_session_ending(source_connection).expect("the stream ends with a frame"),
        SessionEvent::SwitchTo {
            session_id: target_session.session_id
        }
    );
}
