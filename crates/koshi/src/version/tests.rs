//! Tests for gathering build versions, against stand-in servers serving real
//! sockets in a temporary runtime directory.
//!
//! A stand-in answers the Hello and nothing else, which is the whole exchange
//! a version probe makes.

use super::*;

use std::thread::JoinHandle;

use koshi_ipc::endpoint::{socket_addr, EndpointFile};
use koshi_ipc::protocol::{ConnectionToken, IpcRequest, IpcResponse, IpcResult, PROTOCOL_VERSION};
use koshi_ipc::router::{
    router_endpoint_path, router_socket_addr, RouterRequest, RouterResponse, RouterResult,
    ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::transport::Listener;
use koshi_test_support::fixtures::test_runtime_dir;

/// Serve one router Hello at `runtime_dir`, answering with `named` as the
/// build. Binds and writes the endpoint file before returning, so a probe
/// running next finds the stand-in ready.
fn fake_router(runtime_dir: &Path, named: &str) -> JoinHandle<()> {
    let token = ConnectionToken::generate();
    let addr = router_socket_addr(runtime_dir);
    let listener = Listener::bind(&addr).expect("bind the stand-in router");
    EndpointFile {
        socket: addr,
        token,
        pid: std::process::id(),
    }
    .write(&router_endpoint_path(runtime_dir))
    .expect("write the router endpoint file");

    let named = named.to_string();
    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the probe");
        let hello: RouterRequest = connection.recv().expect("read the hello");
        connection
            .send(&RouterResponse {
                request_id: Some(hello.request_id),
                result: RouterResult::Hello {
                    protocol_version: ROUTER_PROTOCOL_VERSION,
                    version: named,
                },
            })
            .expect("send the hello reply");
    })
}

/// Serve one session Hello for `session` at `runtime_dir`, answering with
/// `named` as the build.
fn fake_session(runtime_dir: &Path, session: SessionId, named: &str) -> JoinHandle<()> {
    let addr = socket_addr(runtime_dir, session);
    let token = ConnectionToken::generate();
    let listener = Listener::bind(&addr).expect("bind the stand-in session");
    EndpointFile {
        socket: addr,
        token,
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(runtime_dir, session))
    .expect("write the session endpoint file");

    let named = named.to_string();
    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the probe");
        let hello: IpcRequest = connection.recv().expect("read the hello");
        connection
            .send(&IpcResponse {
                request_id: Some(hello.request_id),
                result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    version: named,
                },
            })
            .expect("send the hello reply");
    })
}

/// Serve one session connection for `session` at `runtime_dir` that closes
/// without answering, the way a server that is wedged or mid-shutdown does.
fn mute_session(runtime_dir: &Path, session: SessionId) -> JoinHandle<()> {
    let addr = socket_addr(runtime_dir, session);
    let token = ConnectionToken::generate();
    let listener = Listener::bind(&addr).expect("bind the mute session");
    EndpointFile {
        socket: addr,
        token,
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(runtime_dir, session))
    .expect("write the session endpoint file");

    std::thread::spawn(move || {
        let connection = listener.accept().expect("accept the probe");
        drop(connection);
    })
}

/// Advertise `session` at an address nothing listens on, the way a session
/// that died without cleaning up leaves its endpoint file behind.
fn stale_endpoint(runtime_dir: &Path, session: SessionId) {
    EndpointFile {
        socket: socket_addr(runtime_dir, session),
        token: ConnectionToken::generate(),
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(runtime_dir, session))
    .expect("write the session endpoint file");
}

#[test]
fn the_reported_build_is_the_one_this_program_was_compiled_at() {
    assert_eq!(
        ClientVersion::of_this_build(),
        ClientVersion {
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    );
}

#[test]
fn the_router_and_every_session_report_the_build_they_run() {
    let runtime_dir = test_runtime_dir();
    let session = SessionId::new();
    let router = fake_router(runtime_dir.path(), "0.2.0");
    let server = fake_session(runtime_dir.path(), session, "0.1.0");

    let rows = server_version_rows_in(runtime_dir.path(), None).expect("both servers answer");

    assert_eq!(
        rows,
        vec![
            ServerVersionRow {
                kind: ServerKind::Router,
                session: None,
                build: ServerBuild::Running {
                    version: "0.2.0".to_string(),
                },
            },
            ServerVersionRow {
                kind: ServerKind::Session,
                session: Some(session),
                build: ServerBuild::Running {
                    version: "0.1.0".to_string(),
                },
            },
        ]
    );
    router.join().expect("the stand-in router finishes");
    server.join().expect("the stand-in session finishes");
}

#[test]
fn a_machine_running_nothing_answers_with_the_router_alone() {
    let runtime_dir = test_runtime_dir();

    let rows =
        server_version_rows_in(runtime_dir.path(), None).expect("nothing running is an answer");

    assert_eq!(
        rows,
        vec![ServerVersionRow {
            kind: ServerKind::Router,
            session: None,
            build: ServerBuild::NotRunning,
        }]
    );
}

#[test]
fn a_server_that_names_no_build_is_told_apart_from_one_that_is_gone() {
    let runtime_dir = test_runtime_dir();
    let silent = SessionId::new();
    let gone = SessionId::new();
    let server = fake_session(runtime_dir.path(), silent, "");
    stale_endpoint(runtime_dir.path(), gone);

    let rows = server_version_rows_in(runtime_dir.path(), None).expect("both sessions answer");

    let silent_row = rows
        .iter()
        .find(|row| row.session == Some(silent))
        .expect("the silent session has a row");
    let gone_row = rows
        .iter()
        .find(|row| row.session == Some(gone))
        .expect("the gone session has a row");
    assert_eq!(
        *silent_row,
        ServerVersionRow {
            kind: ServerKind::Session,
            session: Some(silent),
            build: ServerBuild::Unnamed,
        }
    );
    assert_eq!(
        *gone_row,
        ServerVersionRow {
            kind: ServerKind::Session,
            session: Some(gone),
            build: ServerBuild::NotRunning,
        }
    );
    server.join().expect("the stand-in session finishes");
}

#[test]
fn naming_one_session_leaves_out_the_router_and_the_other_sessions() {
    let runtime_dir = test_runtime_dir();
    let asked = SessionId::new();
    let other = SessionId::new();
    let server = fake_session(runtime_dir.path(), asked, "0.2.0");
    stale_endpoint(runtime_dir.path(), other);

    let rows = server_version_rows_in(runtime_dir.path(), Some(&SessionRef::Id(asked)))
        .expect("the named session answers");

    assert_eq!(
        rows,
        vec![ServerVersionRow {
            kind: ServerKind::Session,
            session: Some(asked),
            build: ServerBuild::Running {
                version: "0.2.0".to_string(),
            },
        }]
    );
    server.join().expect("the stand-in session finishes");
}

#[test]
fn naming_a_session_that_is_not_running_reports_it_as_not_running() {
    let runtime_dir = test_runtime_dir();
    let gone = SessionId::new();

    let rows = server_version_rows_in(runtime_dir.path(), Some(&SessionRef::Id(gone)))
        .expect("an id that nothing answers is still an answer");

    assert_eq!(
        rows,
        vec![ServerVersionRow {
            kind: ServerKind::Session,
            session: Some(gone),
            build: ServerBuild::NotRunning,
        }]
    );
}

/// Two runs of `server-version` print the sessions in the same order, so the
/// rows are sorted by session id.
///
/// The endpoint files are written newest id first, so a listing that kept the
/// order the directory hands back comes out unsorted on any filesystem that
/// reports creation order. Six sessions make a directory order that happens to
/// be sorted a one-in-720 coincidence.
#[test]
fn the_session_rows_come_back_in_session_id_order() {
    let runtime_dir = test_runtime_dir();
    let mut sessions: Vec<SessionId> = (0..6).map(|_| SessionId::new()).collect();
    sessions.sort();
    for session in sessions.iter().rev() {
        stale_endpoint(runtime_dir.path(), *session);
    }

    let rows = server_version_rows_in(runtime_dir.path(), None).expect("the sessions are listed");

    let listed: Vec<SessionId> = rows.iter().filter_map(|row| row.session).collect();
    assert_eq!(listed, sessions);
}

#[test]
fn a_server_that_cannot_be_asked_leaves_the_other_rows_standing() {
    let runtime_dir = test_runtime_dir();
    let answering = SessionId::new();
    let mute = SessionId::new();
    let router = fake_router(runtime_dir.path(), "0.2.0");
    let server = fake_session(runtime_dir.path(), answering, "0.2.0");
    let wedged = mute_session(runtime_dir.path(), mute);

    let rows = server_version_rows_in(runtime_dir.path(), None)
        .expect("one server failing is still an answer");

    // The router and the answering session are both here, which is the whole
    // point: one wedged server used to take the entire answer with it.
    assert_eq!(
        rows.iter()
            .find(|row| row.kind == ServerKind::Router)
            .map(|row| &row.build),
        Some(&ServerBuild::Running {
            version: "0.2.0".to_string(),
        })
    );
    assert_eq!(
        rows.iter()
            .find(|row| row.session == Some(answering))
            .map(|row| &row.build),
        Some(&ServerBuild::Running {
            version: "0.2.0".to_string(),
        })
    );
    let wedged_row = rows
        .iter()
        .find(|row| row.session == Some(mute))
        .expect("the wedged session has a row");
    assert_eq!(
        wedged_row.build,
        ServerBuild::Unreachable {
            detail: "IPC unavailable: ipc peer disconnected".to_string(),
        }
    );

    router.join().expect("the stand-in router finishes");
    server.join().expect("the stand-in session finishes");
    wedged.join().expect("the mute session finishes");
}

#[test]
fn every_server_answering_ends_the_command_with_no_failure() {
    let rows = vec![
        ServerVersionRow {
            kind: ServerKind::Router,
            session: None,
            build: ServerBuild::NotRunning,
        },
        ServerVersionRow {
            kind: ServerKind::Session,
            session: Some(SessionId::new()),
            build: ServerBuild::Unnamed,
        },
    ];

    assert!(
        unreachable_servers(&rows).is_none(),
        "every server answered, so nothing is missing from this answer"
    );
}

#[test]
fn a_server_that_could_not_be_asked_fails_the_command_after_the_rows_print() {
    let rows = vec![
        ServerVersionRow {
            kind: ServerKind::Router,
            session: None,
            build: ServerBuild::Unreachable {
                detail: "the socket closed".to_string(),
            },
        },
        ServerVersionRow {
            kind: ServerKind::Session,
            session: Some(SessionId::new()),
            build: ServerBuild::Unreachable {
                detail: "the socket closed".to_string(),
            },
        },
    ];

    let Some(CliError::IpcUnavailable { detail }) = unreachable_servers(&rows) else {
        panic!("two unreachable servers must fail the command");
    };
    assert_eq!(
        detail,
        "2 koshi servers did not answer, so this answer is incomplete"
    );

    let Some(CliError::IpcUnavailable { detail }) = unreachable_servers(&rows[..1]) else {
        panic!("one unreachable server must fail the command");
    };
    assert_eq!(
        detail,
        "1 koshi server did not answer, so this answer is incomplete"
    );
}
