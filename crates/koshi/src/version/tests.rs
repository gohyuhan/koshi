//! Tests for gathering build versions, against stand-in servers serving real
//! sockets in a temporary runtime directory.
//!
//! A stand-in answers the Hello and nothing else, which is the whole exchange
//! a version probe makes.

use super::*;

use std::thread::JoinHandle;

use koshi_ipc::endpoint::{compute_socket_address, EndpointFile};
use koshi_ipc::protocol::{ConnectionToken, IpcRequest, IpcResponse, IpcResult, PROTOCOL_VERSION};
use koshi_ipc::router::{
    compute_router_socket_address, resolve_router_endpoint_path, RouterRequest, RouterResponse,
    RouterResult, ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::transport::Listener;
use koshi_test_support::fixtures::build_test_runtime_directory;

/// Serve one router Hello at `runtime_directory`, answering with `build_version_text` as the
/// build. Binds and writes the endpoint file before returning, so a probe
/// running next finds the stand-in ready.
fn spawn_fake_router(runtime_directory: &Path, build_version_text: &str) -> JoinHandle<()> {
    let connection_token = ConnectionToken::generate();
    let socket_address = compute_router_socket_address(runtime_directory);
    let listener = Listener::bind(&socket_address).expect("bind the stand-in router");
    EndpointFile {
        socket_address,
        connection_token,
        process_id: std::process::id(),
    }
    .write_to_path(&resolve_router_endpoint_path(runtime_directory))
    .expect("write the router endpoint file");

    let build_version_text = build_version_text.to_string();
    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the probe");
        let hello_request: RouterRequest = connection.recv().expect("read the hello");
        connection
            .send(&RouterResponse {
                request_id: Some(hello_request.request_id),
                answer_result: RouterResult::Hello {
                    protocol_version: ROUTER_PROTOCOL_VERSION,
                    build_version: build_version_text,
                },
            })
            .expect("send the hello reply");
    })
}

/// Serve one session Hello for `session_id` at `runtime_directory`, answering with
/// `build_version_text` as the build.
fn spawn_fake_session(
    runtime_directory: &Path,
    session_id: SessionId,
    build_version_text: &str,
) -> JoinHandle<()> {
    let socket_address = compute_socket_address(runtime_directory, session_id);
    let connection_token = ConnectionToken::generate();
    let listener = Listener::bind(&socket_address).expect("bind the stand-in session");
    EndpointFile {
        socket_address,
        connection_token,
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("write the session endpoint file");

    let build_version_text = build_version_text.to_string();
    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the probe");
        let hello: IpcRequest = connection.recv().expect("read the hello");
        connection
            .send(&IpcResponse {
                request_id: Some(hello.request_id),
                answer_result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    build_version: build_version_text,
                },
            })
            .expect("send the hello reply");
    })
}

/// Serve one session connection for `session_id` at `runtime_directory` that closes
/// without answering, the way a server that is wedged or mid-shutdown does.
fn spawn_unresponsive_session(runtime_directory: &Path, session_id: SessionId) -> JoinHandle<()> {
    let socket_address = compute_socket_address(runtime_directory, session_id);
    let connection_token = ConnectionToken::generate();
    let listener = Listener::bind(&socket_address).expect("bind the mute session");
    EndpointFile {
        socket_address,
        connection_token,
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("write the session endpoint file");

    std::thread::spawn(move || {
        let connection = listener.accept().expect("accept the probe");
        drop(connection);
    })
}

/// Advertise `session_id` at an address nothing listens on, the way a session
/// that died without cleaning up leaves its endpoint file behind.
fn write_stale_endpoint_file(runtime_directory: &Path, session_id: SessionId) {
    EndpointFile {
        socket_address: compute_socket_address(runtime_directory, session_id),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("write the session endpoint file");
}

#[test]
fn the_reported_build_is_the_one_this_program_was_compiled_at() {
    assert_eq!(
        ClientVersion::build_client_version(),
        ClientVersion {
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    );
}

#[test]
fn the_router_and_every_session_report_the_build_they_run() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let router = spawn_fake_router(runtime_directory.path(), "0.2.0");
    let session_thread = spawn_fake_session(runtime_directory.path(), session_id, "0.1.0");

    let server_version_rows =
        list_server_version_rows_in_runtime_directory(runtime_directory.path(), None)
            .expect("both servers answer");

    assert_eq!(
        server_version_rows,
        vec![
            ServerVersionRow {
                server_kind: ServerKind::Router,
                session_id: None,
                build: ServerBuild::Running {
                    version: "0.2.0".to_string(),
                },
            },
            ServerVersionRow {
                server_kind: ServerKind::Session,
                session_id: Some(session_id),
                build: ServerBuild::Running {
                    version: "0.1.0".to_string(),
                },
            },
        ]
    );
    router.join().expect("the stand-in router finishes");
    session_thread
        .join()
        .expect("the stand-in session finishes");
}

#[test]
fn a_machine_running_nothing_answers_with_the_router_alone() {
    let runtime_directory = build_test_runtime_directory();

    let server_version_rows =
        list_server_version_rows_in_runtime_directory(runtime_directory.path(), None)
            .expect("nothing running is an answer");

    assert_eq!(
        server_version_rows,
        vec![ServerVersionRow {
            server_kind: ServerKind::Router,
            session_id: None,
            build: ServerBuild::NotRunning,
        }]
    );
}

#[test]
fn a_server_that_names_no_build_is_told_apart_from_one_that_is_gone() {
    let runtime_directory = build_test_runtime_directory();
    let silent_session_id = SessionId::new();
    let gone_session_id = SessionId::new();
    let session_thread = spawn_fake_session(runtime_directory.path(), silent_session_id, "");
    write_stale_endpoint_file(runtime_directory.path(), gone_session_id);

    let server_version_rows =
        list_server_version_rows_in_runtime_directory(runtime_directory.path(), None)
            .expect("both sessions answer");

    let silent_server_version_row = server_version_rows
        .iter()
        .find(|server_version_row| server_version_row.session_id == Some(silent_session_id))
        .expect("the silent session has a version row");
    let gone_server_version_row = server_version_rows
        .iter()
        .find(|server_version_row| server_version_row.session_id == Some(gone_session_id))
        .expect("the gone session has a version row");
    assert_eq!(
        *silent_server_version_row,
        ServerVersionRow {
            server_kind: ServerKind::Session,
            session_id: Some(silent_session_id),
            build: ServerBuild::Unnamed,
        }
    );
    assert_eq!(
        *gone_server_version_row,
        ServerVersionRow {
            server_kind: ServerKind::Session,
            session_id: Some(gone_session_id),
            build: ServerBuild::NotRunning,
        }
    );
    session_thread
        .join()
        .expect("the stand-in session finishes");
}

#[test]
fn naming_one_session_leaves_out_the_router_and_the_other_sessions() {
    let runtime_directory = build_test_runtime_directory();
    let requested_session_id = SessionId::new();
    let other_session_id = SessionId::new();
    let session_thread =
        spawn_fake_session(runtime_directory.path(), requested_session_id, "0.2.0");
    write_stale_endpoint_file(runtime_directory.path(), other_session_id);

    let server_version_rows = list_server_version_rows_in_runtime_directory(
        runtime_directory.path(),
        Some(&SessionReference::SessionId(requested_session_id)),
    )
    .expect("the requested session answers");

    assert_eq!(
        server_version_rows,
        vec![ServerVersionRow {
            server_kind: ServerKind::Session,
            session_id: Some(requested_session_id),
            build: ServerBuild::Running {
                version: "0.2.0".to_string(),
            },
        }]
    );
    session_thread
        .join()
        .expect("the stand-in session finishes");
}

#[test]
fn naming_a_session_that_is_not_running_reports_it_as_not_running() {
    let runtime_directory = build_test_runtime_directory();
    let gone_session_id = SessionId::new();

    let server_version_rows = list_server_version_rows_in_runtime_directory(
        runtime_directory.path(),
        Some(&SessionReference::SessionId(gone_session_id)),
    )
    .expect("a session id that nothing answers is still an answer");

    assert_eq!(
        server_version_rows,
        vec![ServerVersionRow {
            server_kind: ServerKind::Session,
            session_id: Some(gone_session_id),
            build: ServerBuild::NotRunning,
        }]
    );
}

/// Two runs of `server-version` print the sessions in the same order, so the
/// server_version_rows are sorted by session id.
///
/// The endpoint files are written newest id first, so a listing that kept the
/// order the directory hands back comes out unsorted on any filesystem that
/// reports creation order. Six sessions make a directory order that happens to
/// be sorted a one-in-720 coincidence.
#[test]
fn the_session_rows_come_back_in_session_id_order() {
    let runtime_directory = build_test_runtime_directory();
    let mut session_ids: Vec<SessionId> = (0..6).map(|_| SessionId::new()).collect();
    session_ids.sort();
    for session_id in session_ids.iter().rev() {
        write_stale_endpoint_file(runtime_directory.path(), *session_id);
    }

    let server_version_rows =
        list_server_version_rows_in_runtime_directory(runtime_directory.path(), None)
            .expect("the sessions are listed");

    let listed_session_ids: Vec<SessionId> = server_version_rows
        .iter()
        .filter_map(|server_version_row| server_version_row.session_id)
        .collect();
    assert_eq!(listed_session_ids, session_ids);
}

#[test]
fn a_server_that_cannot_be_asked_leaves_the_other_rows_standing() {
    let runtime_directory = build_test_runtime_directory();
    let answering_session_id = SessionId::new();
    let unresponsive_session_id = SessionId::new();
    let router_thread = spawn_fake_router(runtime_directory.path(), "0.2.0");
    let session_thread =
        spawn_fake_session(runtime_directory.path(), answering_session_id, "0.2.0");
    let unresponsive_session =
        spawn_unresponsive_session(runtime_directory.path(), unresponsive_session_id);

    let server_version_rows =
        list_server_version_rows_in_runtime_directory(runtime_directory.path(), None)
            .expect("one server failing is still an answer");

    // The router and the answering session are both here, which is the whole
    // point: one wedged server used to take the entire answer with it.
    assert_eq!(
        server_version_rows
            .iter()
            .find(|server_version_row| server_version_row.server_kind == ServerKind::Router)
            .map(|server_version_row| &server_version_row.build),
        Some(&ServerBuild::Running {
            version: "0.2.0".to_string(),
        })
    );
    assert_eq!(
        server_version_rows
            .iter()
            .find(|server_version_row| server_version_row.session_id == Some(answering_session_id))
            .map(|server_version_row| &server_version_row.build),
        Some(&ServerBuild::Running {
            version: "0.2.0".to_string(),
        })
    );
    let unresponsive_server_version_row = server_version_rows
        .iter()
        .find(|server_version_row| server_version_row.session_id == Some(unresponsive_session_id))
        .expect("the unresponsive session has a version row");
    assert_eq!(
        unresponsive_server_version_row.build,
        ServerBuild::Unreachable {
            detail: "IPC unavailable: ipc peer disconnected".to_string(),
        }
    );

    router_thread.join().expect("the stand-in router finishes");
    session_thread
        .join()
        .expect("the stand-in session finishes");
    unresponsive_session
        .join()
        .expect("the unresponsive session finishes");
}

#[test]
fn every_server_answering_ends_the_command_with_no_failure() {
    let server_version_rows = vec![
        ServerVersionRow {
            server_kind: ServerKind::Router,
            session_id: None,
            build: ServerBuild::NotRunning,
        },
        ServerVersionRow {
            server_kind: ServerKind::Session,
            session_id: Some(SessionId::new()),
            build: ServerBuild::Unnamed,
        },
    ];

    assert!(
        build_unreachable_server_error(&server_version_rows).is_none(),
        "every server answered, so nothing is missing from this answer"
    );
}

#[test]
fn a_server_that_could_not_be_asked_fails_the_command_after_the_rows_print() {
    let server_version_rows = vec![
        ServerVersionRow {
            server_kind: ServerKind::Router,
            session_id: None,
            build: ServerBuild::Unreachable {
                detail: "the socket closed".to_string(),
            },
        },
        ServerVersionRow {
            server_kind: ServerKind::Session,
            session_id: Some(SessionId::new()),
            build: ServerBuild::Unreachable {
                detail: "the socket closed".to_string(),
            },
        },
    ];

    let Some(CliError::IpcUnavailable {
        detail: unreachable_detail,
    }) = build_unreachable_server_error(&server_version_rows)
    else {
        panic!("two unreachable servers must fail the command");
    };
    assert_eq!(
        unreachable_detail,
        "2 koshi servers did not answer, so this answer is incomplete"
    );

    let Some(CliError::IpcUnavailable {
        detail: unreachable_detail,
    }) = build_unreachable_server_error(&server_version_rows[..1])
    else {
        panic!("one unreachable server must fail the command");
    };
    assert_eq!(
        unreachable_detail,
        "1 koshi server did not answer, so this answer is incomplete"
    );
}
