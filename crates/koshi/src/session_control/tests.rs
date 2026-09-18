//! Tests for creating, choosing and ending a running session.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::SystemTime;

use koshi_core::command::CliExitCode;
use koshi_core::discovery::{SessionDiscovery, SessionOverview};
use koshi_core::event::{Event, QuitCause, RejectReason};
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::protocol::{
    ConnectionToken, IpcErrorCode, IpcErrorPayload, IpcRequest, IpcRequestKind, IpcResponse,
    IpcResult, PROTOCOL_VERSION,
};
use koshi_ipc::router::{
    compute_router_socket_address, resolve_router_endpoint_path, RouterHandshake, RouterRequest,
    RouterResponse, SessionAddress, ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::transport::{Connection, Listener};
use uuid::Uuid;

use super::*;
use koshi_ipc::router::{RouterRequestKind, RouterResult};

/// The answer an accepted session Hello earns.
fn hello_accepted() -> IpcResult {
    IpcResult::Hello {
        protocol_version: PROTOCOL_VERSION,
        build_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// The answer an accepted router Hello earns.
fn router_hello_accepted() -> RouterResult {
    RouterResult::Hello {
        protocol_version: ROUTER_PROTOCOL_VERSION,
        build_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn build_session_overview(session_name: &str) -> SessionOverview {
    build_named_session_overview(SessionId::new(), session_name)
}

fn build_named_session_overview(session_id: SessionId, session_name: &str) -> SessionOverview {
    SessionOverview {
        session: SessionDiscovery {
            session_id,
            session_name: session_name.to_string(),
            created_at: SystemTime::UNIX_EPOCH,
            attached_client_ids: Vec::new(),
            pane_count: 0,
        },
        tabs: Vec::new(),
        panes: Vec::new(),
        clients: Vec::new(),
    }
}

fn build_complete_discovery(session_overviews: Vec<SessionOverview>) -> Discovered {
    Discovered {
        sessions: session_overviews,
        unasked_session_count: 0,
    }
}

fn build_incomplete_discovery(session_overviews: Vec<SessionOverview>) -> Discovered {
    Discovered {
        sessions: session_overviews,
        unasked_session_count: 1,
    }
}

fn build_test_runtime_directory(test_label: &str) -> PathBuf {
    #[cfg(unix)]
    let runtime_base_directory = PathBuf::from("/tmp");
    #[cfg(windows)]
    let runtime_base_directory = std::env::temp_dir();
    let runtime_directory =
        runtime_base_directory.join(format!("koshi-kill-{}-{test_label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime_directory);
    std::fs::create_dir_all(&runtime_directory).expect("create runtime dir");
    runtime_directory
}

fn send_ipc_reply(connection: &mut Connection, request_id: u64, ipc_result: IpcResult) {
    connection
        .send(&IpcResponse {
            request_id: Some(request_id),
            answer_result: ipc_result,
        })
        .expect("send scripted reply");
}

fn serve_kill_session(
    runtime_directory: &Path,
    session_overview: SessionOverview,
) -> JoinHandle<()> {
    let session_id = session_overview.session.session_id;
    let socket_address = koshi_ipc::endpoint::compute_socket_address(runtime_directory, session_id);
    let connection_token = ConnectionToken::generate();
    let listener = Listener::bind(&socket_address).expect("stand-in session binds");
    EndpointFile {
        socket_address,
        connection_token: connection_token.clone(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("endpoint file written");

    std::thread::spawn(move || {
        let mut discovery_connection = listener.accept().expect("accept discovery");
        let hello_request: IpcRequest = discovery_connection.recv().expect("read discovery hello");
        let discovery_request: IpcRequest =
            discovery_connection.recv().expect("read discovery request");
        assert!(matches!(
            &hello_request.request_kind,
            IpcRequestKind::Hello {
                connection_token: presented_connection_token,
                ..
            } if presented_connection_token == &connection_token
        ));
        assert!(matches!(
            discovery_request.request_kind,
            IpcRequestKind::Discovery
        ));
        send_ipc_reply(
            &mut discovery_connection,
            hello_request.request_id,
            hello_accepted(),
        );
        send_ipc_reply(
            &mut discovery_connection,
            discovery_request.request_id,
            IpcResult::Overview(session_overview),
        );
        drop(discovery_connection);

        let mut kill_connection = listener.accept().expect("accept kill command");
        let kill_hello_request: IpcRequest = kill_connection.recv().expect("read kill hello");
        let kill_command_request: IpcRequest = kill_connection.recv().expect("read kill request");
        let IpcRequestKind::SubmitCommand(command_envelope) = kill_command_request.request_kind
        else {
            panic!("expected a submitted command");
        };
        assert_eq!(command_envelope.command, Command::Quit);
        send_ipc_reply(
            &mut kill_connection,
            kill_hello_request.request_id,
            hello_accepted(),
        );
        send_ipc_reply(
            &mut kill_connection,
            kill_command_request.request_id,
            IpcResult::CommandResult(CommandResult::Ok {
                command_id: command_envelope.command_id,
                emitted_events: vec![Event::Quit(QuitCause::Requested)],
            }),
        );
    })
}

/// A stand-in session that scripts the kill exchange alone. A discovery
/// request on the first connection fails the scripted thread, so joining it
/// proves the caller asked no session to describe itself.
fn serve_kill_session_without_discovery(
    runtime_directory: &Path,
    session_id: SessionId,
) -> JoinHandle<()> {
    let socket_address = koshi_ipc::endpoint::compute_socket_address(runtime_directory, session_id);
    let connection_token = ConnectionToken::generate();
    let listener = Listener::bind(&socket_address).expect("stand-in session binds");
    EndpointFile {
        socket_address,
        connection_token: connection_token.clone(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("endpoint file written");

    std::thread::spawn(move || {
        let mut kill_connection = listener.accept().expect("accept kill command");
        let kill_hello_request: IpcRequest = kill_connection.recv().expect("read kill hello");
        let kill_command_request: IpcRequest = kill_connection.recv().expect("read kill request");
        assert!(matches!(
            &kill_hello_request.request_kind,
            IpcRequestKind::Hello {
                connection_token: presented_connection_token,
                ..
            } if presented_connection_token == &connection_token
        ));
        let IpcRequestKind::SubmitCommand(command_envelope) = kill_command_request.request_kind
        else {
            panic!("expected a submitted command as the first request");
        };
        assert_eq!(command_envelope.command, Command::Quit);
        send_ipc_reply(
            &mut kill_connection,
            kill_hello_request.request_id,
            hello_accepted(),
        );
        send_ipc_reply(
            &mut kill_connection,
            kill_command_request.request_id,
            IpcResult::CommandResult(CommandResult::Ok {
                command_id: command_envelope.command_id,
                emitted_events: vec![Event::Quit(QuitCause::Requested)],
            }),
        );
    })
}

/// What a stand-in router saw on the one connection it served.
#[derive(Default)]
struct RouterLog {
    /// Whether the Hello presented a connection token the gate accepted.
    is_hello_accepted: bool,
    /// The request pipelined behind the Hello.
    router_request_kind: Option<RouterRequestKind>,
}

/// The create request a caller is expected to put on the wire for `profile_name` and
/// `is_other_user_access_allowed`.
fn expected_create_session_request(
    profile_name: Option<&str>,
    is_other_user_access_allowed: Option<bool>,
) -> RouterRequestKind {
    RouterRequestKind::CreateSession {
        profile: profile_name.map(str::to_string),
        working_directory: Some(
            std::env::current_dir().expect("this test process has a directory"),
        ),
        is_other_user_access_allowed,
    }
}

/// A stand-in router that accepts one caller's Hello and answers the request
/// pipelined behind it with `router_result`. What it saw goes in the returned log for
/// the test to assert on.
///
/// The bind and the endpoint file are both done before this returns, so a
/// caller that runs next finds the stand-in ready and never starts a router of
/// its own.
///
/// It records before it replies, so a caller that has its answer is a caller
/// whose request is already in the log. That ordering is what lets a test read
/// the log without joining the thread.
///
/// Both replies go out whatever the Hello and the request turn out to be: a
/// stand-in that stops early strands the caller on a reply that never comes.
fn serve_router_request(
    runtime_directory: &Path,
    router_result: RouterResult,
) -> Arc<Mutex<RouterLog>> {
    let connection_token = ConnectionToken::generate();
    let socket_address = compute_router_socket_address(runtime_directory);
    let listener = Listener::bind(&socket_address).expect("stand-in router binds");
    EndpointFile {
        socket_address,
        connection_token: connection_token.clone(),
        process_id: std::process::id(),
    }
    .write_to_path(&resolve_router_endpoint_path(runtime_directory))
    .expect("endpoint file written");

    let router_log = Arc::new(Mutex::new(RouterLog::default()));
    let shared_router_log = Arc::clone(&router_log);
    std::thread::spawn(move || {
        let Ok(mut router_connection) = listener.accept() else {
            return;
        };
        let mut router_handshake = RouterHandshake::from_connection_token(connection_token);
        let Ok(hello_request) = router_connection.recv::<RouterRequest>() else {
            return;
        };
        let Ok(router_request) = router_connection.recv::<RouterRequest>() else {
            return;
        };

        {
            let mut router_log = shared_router_log
                .lock()
                .expect("the log outlives every panic");
            router_log.is_hello_accepted = router_handshake
                .validate_request_kind(&hello_request.request_kind)
                .is_ok();
            router_log.router_request_kind = Some(router_request.request_kind);
        }

        let _ = router_connection.send(&RouterResponse {
            request_id: Some(hello_request.request_id),
            answer_result: router_hello_accepted(),
        });
        let _ = router_connection.send(&RouterResponse {
            request_id: Some(router_request.request_id),
            answer_result: router_result,
        });
    });
    router_log
}

/// The Hello and the request a stand-in router saw, once its caller has been
/// answered.
fn read_router_log(router_log: &Arc<Mutex<RouterLog>>) -> (bool, Option<RouterRequestKind>) {
    let stored_router_log = router_log.lock().expect("the log outlives every panic");
    (
        stored_router_log.is_hello_accepted,
        stored_router_log.router_request_kind.clone(),
    )
}

#[test]
fn a_created_answer_hands_back_the_new_session_id() {
    let runtime_directory = build_test_runtime_directory("headless-created");
    let session_id = SessionId::from_uuid(Uuid::from_u128(7));
    let router_log = serve_router_request(
        &runtime_directory,
        RouterResult::Created(SessionAddress {
            session_id,
            session_name: "quiet-lake".to_string(),
            socket_address: "unused".to_string(),
            process_id: std::process::id(),
        }),
    );

    let created_session_id =
        request_new_session(&runtime_directory, None, None).expect("the router created a session");

    assert_eq!(created_session_id, session_id);
    let (is_hello_accepted, router_request_kind) = read_router_log(&router_log);
    assert!(is_hello_accepted, "the hello opens the gate");
    assert_eq!(
        router_request_kind,
        Some(expected_create_session_request(None, None))
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

/// `request_headless_session` forwards to `request_new_session`, so this one request
/// is what either entry point puts on the wire.
#[test]
fn the_headless_wrapper_and_the_plain_create_ask_the_router_the_same_thing() {
    let runtime_directory = build_test_runtime_directory("create-with-profile");
    let session_id = SessionId::from_uuid(Uuid::from_u128(11));
    let router_log = serve_router_request(
        &runtime_directory,
        RouterResult::Created(SessionAddress {
            session_id,
            session_name: "amber-fox".to_string(),
            socket_address: "unused".to_string(),
            process_id: std::process::id(),
        }),
    );

    let created_session_id = request_headless_session(&runtime_directory, Some("work"), None)
        .expect("the router created a session");

    assert_eq!(created_session_id, session_id);
    let (is_hello_accepted, router_request_kind) = read_router_log(&router_log);
    assert!(is_hello_accepted, "the hello opens the gate");
    assert_eq!(
        router_request_kind,
        Some(expected_create_session_request(Some("work"), None))
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

/// The wire is the only place `--allow-other-users` can travel, so a create
/// that does not carry it leaves the new session as private as any other.
#[test]
fn a_headless_create_forcing_the_other_users_on_carries_that_answer_to_the_router() {
    let runtime_directory = build_test_runtime_directory("create-other-users");
    let session_id = SessionId::from_uuid(Uuid::from_u128(12));
    let router_log = serve_router_request(
        &runtime_directory,
        RouterResult::Created(SessionAddress {
            session_id,
            session_name: "amber-fox".to_string(),
            socket_address: "unused".to_string(),
            process_id: std::process::id(),
        }),
    );

    let created_session_id = request_headless_session(&runtime_directory, None, Some(true))
        .expect("the router created a session");

    assert_eq!(created_session_id, session_id);
    let (is_hello_accepted, router_request_kind) = read_router_log(&router_log);
    assert!(is_hello_accepted, "the hello opens the gate");
    assert_eq!(
        router_request_kind,
        Some(expected_create_session_request(None, Some(true)))
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_refused_create_reports_the_routers_own_message() {
    let runtime_directory = build_test_runtime_directory("headless-refused");
    let router_log = serve_router_request(
        &runtime_directory,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the session server did not start".to_string(),
        }),
    );

    let create_error =
        request_new_session(&runtime_directory, None, None).expect_err("the router refused");

    let CliError::IpcUnavailable { detail } = create_error else {
        panic!("expected IpcUnavailable, got {create_error:?}");
    };
    assert_eq!(detail, "the session server did not start");
    let (is_hello_accepted, router_request_kind) = read_router_log(&router_log);
    assert!(is_hello_accepted, "the hello opens the gate");
    assert_eq!(
        router_request_kind,
        Some(expected_create_session_request(None, None))
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn an_answer_to_another_request_names_what_came_back() {
    let runtime_directory = build_test_runtime_directory("headless-wrong-answer");
    let router_log = serve_router_request(&runtime_directory, router_hello_accepted());

    let create_error =
        request_new_session(&runtime_directory, None, None).expect_err("the answer fits no create");

    let CliError::IpcUnavailable { detail } = create_error else {
        panic!("expected IpcUnavailable, got {create_error:?}");
    };
    assert_eq!(detail, "the router answered with an unexpected Hello reply");
    let (is_hello_accepted, router_request_kind) = read_router_log(&router_log);
    assert!(is_hello_accepted, "the hello opens the gate");
    assert_eq!(
        router_request_kind,
        Some(expected_create_session_request(None, None))
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_name_selects_its_session() {
    let quiet_session_overview = build_session_overview("quiet-lake");
    let quiet_session_id = quiet_session_overview.session.session_id;
    let discovered_sessions = build_complete_discovery(vec![
        build_session_overview("amber-fox"),
        quiet_session_overview,
    ]);

    assert_eq!(
        select_session_to_kill(&discovered_sessions, Some("quiet-lake")).expect("name matches"),
        quiet_session_id
    );
}

#[test]
fn no_name_selects_the_only_running_session() {
    let quiet_session_overview = build_session_overview("quiet-lake");
    let quiet_session_id = quiet_session_overview.session.session_id;

    assert_eq!(
        select_session_to_kill(
            &build_complete_discovery(vec![quiet_session_overview]),
            None,
        )
        .expect("sole session"),
        quiet_session_id
    );
}

#[test]
fn an_unknown_name_uses_the_session_not_found_exit_code() {
    let selection_error = select_session_to_kill(
        &build_complete_discovery(vec![build_session_overview("quiet-lake")]),
        Some("missing"),
    )
    .expect_err("name is absent");

    assert!(matches!(
        &selection_error,
        CliError::SessionNotFound { session_name } if session_name == "missing"
    ));
    assert_eq!(
        CliExitCode::from(&selection_error),
        CliExitCode::SessionNotFound
    );
}

#[test]
fn no_running_session_uses_the_session_not_found_exit_code() {
    let selection_error = select_session_to_kill(&build_complete_discovery(Vec::new()), None)
        .expect_err("nothing to kill");

    assert!(matches!(selection_error, CliError::NoSessions));
    assert_eq!(
        CliExitCode::from(&selection_error),
        CliExitCode::SessionNotFound
    );
}

#[test]
fn duplicate_names_list_every_session_id() {
    let first_session_id = SessionId::from_uuid(Uuid::from_u128(1));
    let second_session_id = SessionId::from_uuid(Uuid::from_u128(2));
    let selection_error = select_session_to_kill(
        &build_complete_discovery(vec![
            build_named_session_overview(first_session_id, "quiet-lake"),
            build_named_session_overview(second_session_id, "quiet-lake"),
        ]),
        Some("quiet-lake"),
    )
    .expect_err("two sessions share the name");

    let CliError::CommandRejected { reason, help } = selection_error else {
        panic!("expected a rejected command");
    };
    assert_eq!(reason, RejectReason::TargetAmbiguous);
    assert_eq!(
        help,
        Some(format!(
            "several sessions are named `quiet-lake`: {first_session_id}, {second_session_id}; use the session id"
        ))
    );
}

#[test]
fn several_sessions_need_a_name() {
    let selection_error = select_session_to_kill(
        &build_complete_discovery(vec![
            build_session_overview("quiet-lake"),
            build_session_overview("amber-fox"),
        ]),
        None,
    )
    .expect_err("several sessions need a name");

    let CliError::CommandRejected { reason, help } = selection_error else {
        panic!("expected a rejected command");
    };
    assert_eq!(reason, RejectReason::TargetAmbiguous);
    assert_eq!(
        help,
        Some("several sessions are running; name one: koshi kill-session <name>".to_string())
    );
}

#[test]
fn an_incomplete_census_cannot_prove_a_name_is_unique() {
    let selection_error = select_session_to_kill(
        &build_incomplete_discovery(vec![build_session_overview("quiet-lake")]),
        Some("quiet-lake"),
    )
    .expect_err("another session may share the name");

    let CliError::IpcUnavailable { detail } = selection_error else {
        panic!("expected IpcUnavailable, got {selection_error:?}");
    };
    assert_eq!(
        detail,
        "cannot tell whether `quiet-lake` is unique (1 running session did not answer)"
    );
}

#[test]
fn an_incomplete_census_cannot_apply_the_count_rule() {
    let selection_error = select_session_to_kill(
        &build_incomplete_discovery(vec![build_session_overview("quiet-lake")]),
        None,
    )
    .expect_err("another session may be running");

    let CliError::IpcUnavailable { detail } = selection_error else {
        panic!("expected IpcUnavailable, got {selection_error:?}");
    };
    assert_eq!(
        detail,
        "cannot tell which session to kill; name one: koshi kill-session <name> \
         (1 running session did not answer)"
    );
}

#[test]
fn kill_by_name_submits_quit_to_that_session() {
    let runtime_directory = build_test_runtime_directory("named");
    let quiet_session_overview = build_session_overview("quiet-lake");
    let kill_server_thread = serve_kill_session(&runtime_directory, quiet_session_overview);

    let command_result = kill_session_in_runtime_directory(
        &runtime_directory,
        Some(&SessionReference::SessionName("quiet-lake".to_string())),
    )
    .expect("kill exchange succeeds");

    assert!(matches!(
        command_result,
        CommandResult::Ok {
            emitted_events,
            ..
        } if emitted_events == vec![Event::Quit(QuitCause::Requested)]
    ));
    kill_server_thread.join().expect("stand-in session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn kill_without_a_name_submits_quit_to_the_only_session() {
    let runtime_directory = build_test_runtime_directory("sole");
    let quiet_session_overview = build_session_overview("quiet-lake");
    let kill_server_thread = serve_kill_session(&runtime_directory, quiet_session_overview);

    let command_result = kill_session_in_runtime_directory(&runtime_directory, None)
        .expect("kill exchange succeeds");

    assert!(matches!(
        command_result,
        CommandResult::Ok {
            emitted_events,
            ..
        } if emitted_events == vec![Event::Quit(QuitCause::Requested)]
    ));
    kill_server_thread.join().expect("stand-in session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn kill_by_session_id_submits_quit_without_discovery() {
    let runtime_directory = build_test_runtime_directory("by-session-id");
    let session_id = SessionId::new();
    let kill_server_thread = serve_kill_session_without_discovery(&runtime_directory, session_id);

    let command_result = kill_session_in_runtime_directory(
        &runtime_directory,
        Some(&SessionReference::SessionId(session_id)),
    )
    .expect("kill exchange succeeds");

    assert!(matches!(
        command_result,
        CommandResult::Ok {
            emitted_events,
            ..
        } if emitted_events == vec![Event::Quit(QuitCause::Requested)]
    ));
    kill_server_thread
        .join()
        .expect("stand-in session saw no discovery");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn kill_by_unknown_session_id_is_session_not_found() {
    let runtime_directory = build_test_runtime_directory("unknown-session-id");
    let session_id = SessionId::new();

    let kill_error = kill_session_in_runtime_directory(
        &runtime_directory,
        Some(&SessionReference::SessionId(session_id)),
    )
    .expect_err("nothing advertises that id");

    assert!(matches!(
        &kill_error,
        CliError::SessionNotFound { session_name }
            if session_name.as_str() == session_id.to_string()
    ));
    assert_eq!(CliExitCode::from(&kill_error), CliExitCode::SessionNotFound);
    let _ = std::fs::remove_dir_all(&runtime_directory);
}
