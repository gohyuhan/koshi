//! Tests for choosing a running session, ending it, and reading the target
//! of `koshi detach`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::SystemTime;

use koshi_core::command::CliExitCode;
use koshi_core::discovery::{SessionDiscovery, SessionOverview};
use koshi_core::event::{Event, QuitCause, RejectReason};
use koshi_core::ids::OwnedSessionId;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::protocol::{ConnectionToken, IpcRequest, IpcRequestKind, IpcResult};
use koshi_ipc::transport::{Connection, Listener};
use uuid::Uuid;

use super::*;
use koshi_test_support::fixtures::{build_accepted_hello_result, send_ipc_reply};

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
        unread_path_count: 0,
        owner_user_id_by_session_id: HashMap::new(),
    }
}

fn build_incomplete_discovery(session_overviews: Vec<SessionOverview>) -> Discovered {
    Discovered {
        sessions: session_overviews,
        unasked_session_count: 1,
        unread_path_count: 0,
        owner_user_id_by_session_id: HashMap::new(),
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

/// Answer the Hello numbered `hello_request_id` on `kill_connection`, answer
/// `kill_prepare_request` with `Prepared`, read the `Commit` that follows,
/// and answer it with a quit for [`QuitCause::Requested`].
///
/// # Panics
/// When `kill_prepare_request` is not a `Prepare` of a submitted
/// [`Command::Quit`], or the next request is not a `Commit`.
fn answer_prepared_quit(
    kill_connection: &mut Connection,
    hello_request_id: u64,
    kill_prepare_request: IpcRequest,
) {
    let IpcRequestKind::Prepare(prepared_request_kind) = kill_prepare_request.request_kind else {
        panic!("expected a Prepare after the Hello");
    };
    let IpcRequestKind::SubmitCommand {
        command_envelope, ..
    } = *prepared_request_kind
    else {
        panic!("expected a prepared submitted command");
    };
    assert_eq!(command_envelope.command, Command::Quit);
    send_ipc_reply(
        kill_connection,
        hello_request_id,
        build_accepted_hello_result(),
    );
    send_ipc_reply(
        kill_connection,
        kill_prepare_request.request_id,
        IpcResult::Prepared,
    );
    let kill_commit_request: IpcRequest = kill_connection.recv().expect("read kill commit");
    assert_eq!(kill_commit_request.request_kind, IpcRequestKind::Commit);
    send_ipc_reply(
        kill_connection,
        kill_commit_request.request_id,
        IpcResult::CommandResult(CommandResult::Ok {
            command_id: command_envelope.command_id,
            emitted_events: vec![Event::Quit(QuitCause::Requested)],
        }),
    );
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
        assert_eq!(
            hello_request.request_kind,
            IpcRequestKind::build_hello_request(connection_token.clone())
        );
        assert_eq!(discovery_request.request_kind, IpcRequestKind::Discovery);
        send_ipc_reply(
            &mut discovery_connection,
            hello_request.request_id,
            build_accepted_hello_result(),
        );
        send_ipc_reply(
            &mut discovery_connection,
            discovery_request.request_id,
            IpcResult::Overview(session_overview),
        );
        drop(discovery_connection);

        let mut kill_connection = listener.accept().expect("accept kill command");
        let kill_hello_request: IpcRequest = kill_connection.recv().expect("read kill hello");
        let kill_prepare_request: IpcRequest = kill_connection.recv().expect("read kill prepare");
        answer_prepared_quit(
            &mut kill_connection,
            kill_hello_request.request_id,
            kill_prepare_request,
        );
    })
}

/// A stand-in session for `session_id` that serves one connection: a Hello,
/// then a `Prepare` of a submitted [`Command::Quit`] and its `Commit`,
/// answered with [`QuitCause::Requested`]. Any other first request after the Hello, such as
/// a discovery request, panics the thread, and joining it raises that panic.
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
        let kill_prepare_request: IpcRequest = kill_connection.recv().expect("read kill prepare");
        assert_eq!(
            kill_hello_request.request_kind,
            IpcRequestKind::build_hello_request(connection_token.clone())
        );
        answer_prepared_quit(
            &mut kill_connection,
            kill_hello_request.request_id,
            kill_prepare_request,
        );
    })
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
        resolve_discovered_session_target(&discovered_sessions, Some("quiet-lake"))
            .expect("name matches"),
        (quiet_session_id, None)
    );
}

#[test]
fn a_name_selects_another_users_session_with_its_owner() {
    let quiet_session_overview = build_session_overview("quiet-lake");
    let quiet_session_id = quiet_session_overview.session.session_id;
    let discovered_sessions = Discovered {
        owner_user_id_by_session_id: HashMap::from([(quiet_session_id, 1001)]),
        ..build_complete_discovery(vec![
            build_session_overview("amber-fox"),
            quiet_session_overview,
        ])
    };

    assert_eq!(
        resolve_discovered_session_target(&discovered_sessions, Some("quiet-lake"))
            .expect("name matches"),
        (quiet_session_id, Some(1001))
    );
}

#[test]
fn no_name_selects_the_only_running_session() {
    let quiet_session_overview = build_session_overview("quiet-lake");
    let quiet_session_id = quiet_session_overview.session.session_id;

    assert_eq!(
        resolve_discovered_session_target(
            &build_complete_discovery(vec![quiet_session_overview]),
            None,
        )
        .expect("sole session"),
        (quiet_session_id, None)
    );
}

#[test]
fn an_unknown_name_uses_the_session_not_found_exit_code() {
    let session_resolution_error = resolve_discovered_session_target(
        &build_complete_discovery(vec![build_session_overview("quiet-lake")]),
        Some("missing"),
    )
    .expect_err("name is absent");

    assert_eq!(
        CliExitCode::from(&session_resolution_error),
        CliExitCode::SessionNotFound
    );
    let CliError::SessionNotFound { session_name } = session_resolution_error else {
        panic!("expected SessionNotFound, got {session_resolution_error:?}");
    };
    assert_eq!(session_name, "missing");
}

#[test]
fn no_running_session_uses_the_session_not_found_exit_code() {
    let session_resolution_error =
        resolve_discovered_session_target(&build_complete_discovery(Vec::new()), None)
            .expect_err("nothing to kill");

    assert_eq!(
        CliExitCode::from(&session_resolution_error),
        CliExitCode::SessionNotFound
    );
    let CliError::NoSessions = session_resolution_error else {
        panic!("expected NoSessions, got {session_resolution_error:?}");
    };
}

#[test]
fn duplicate_names_list_every_session_id() {
    let first_session_id = SessionId::from_uuid(Uuid::from_u128(1));
    let second_session_id = SessionId::from_uuid(Uuid::from_u128(2));
    let session_resolution_error = resolve_discovered_session_target(
        &build_complete_discovery(vec![
            build_named_session_overview(first_session_id, "quiet-lake"),
            build_named_session_overview(second_session_id, "quiet-lake"),
        ]),
        Some("quiet-lake"),
    )
    .expect_err("two sessions share the name");

    let CliError::CommandRejected { reason, help } = session_resolution_error else {
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
    let session_resolution_error = resolve_discovered_session_target(
        &build_complete_discovery(vec![
            build_session_overview("quiet-lake"),
            build_session_overview("amber-fox"),
        ]),
        None,
    )
    .expect_err("several sessions need a name");

    let CliError::CommandRejected { reason, help } = session_resolution_error else {
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
    let session_resolution_error = resolve_discovered_session_target(
        &build_incomplete_discovery(vec![build_session_overview("quiet-lake")]),
        Some("quiet-lake"),
    )
    .expect_err("another session may share the name");

    let CliError::IpcUnavailable { detail } = session_resolution_error else {
        panic!("expected IpcUnavailable, got {session_resolution_error:?}");
    };
    assert_eq!(
        detail,
        "cannot tell whether `quiet-lake` is unique (1 running session did not answer)"
    );
}

#[test]
fn an_incomplete_census_cannot_apply_the_count_rule() {
    let session_resolution_error = resolve_discovered_session_target(
        &build_incomplete_discovery(vec![build_session_overview("quiet-lake")]),
        None,
    )
    .expect_err("another session may be running");

    let CliError::IpcUnavailable { detail } = session_resolution_error else {
        panic!("expected IpcUnavailable, got {session_resolution_error:?}");
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

    let session_ending = kill_session_in_runtime_directory(
        &runtime_directory,
        None,
        Some(&SessionReference::SessionName("quiet-lake".to_string())),
    )
    .expect("kill exchange succeeds");

    assert_eq!(
        session_ending,
        SessionEnding::Quit {
            stopped_process_count: 0
        }
    );
    kill_server_thread.join().expect("stand-in session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn kill_without_a_name_submits_quit_to_the_only_session() {
    let runtime_directory = build_test_runtime_directory("sole");
    let quiet_session_overview = build_session_overview("quiet-lake");
    let kill_server_thread = serve_kill_session(&runtime_directory, quiet_session_overview);

    let session_ending = kill_session_in_runtime_directory(&runtime_directory, None, None)
        .expect("kill exchange succeeds");

    assert_eq!(
        session_ending,
        SessionEnding::Quit {
            stopped_process_count: 0
        }
    );
    kill_server_thread.join().expect("stand-in session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn kill_by_session_id_submits_quit_without_discovery() {
    let runtime_directory = build_test_runtime_directory("by-session-id");
    let session_id = SessionId::new();
    let kill_server_thread = serve_kill_session_without_discovery(&runtime_directory, session_id);

    let session_ending = kill_session_in_runtime_directory(
        &runtime_directory,
        None,
        Some(&SessionReference::SessionId(session_id)),
    )
    .expect("kill exchange succeeds");

    assert_eq!(
        session_ending,
        SessionEnding::Quit {
            stopped_process_count: 0
        }
    );
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
        None,
        Some(&SessionReference::SessionId(session_id)),
    )
    .expect_err("nothing advertises that id");

    assert_eq!(CliExitCode::from(&kill_error), CliExitCode::SessionNotFound);
    let CliError::SessionNotFound { session_name } = kill_error else {
        panic!("expected SessionNotFound, got {kill_error:?}");
    };
    assert_eq!(session_name, session_id.to_string());
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn kill_by_owned_session_id_looks_in_that_users_folder_alone() {
    let runtime_directory = build_test_runtime_directory("owned-session-id");
    #[cfg(unix)]
    let other_user_id = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&runtime_directory)
            .expect("read the runtime directory")
            .uid()
            + 1
    };
    #[cfg(windows)]
    let other_user_id = 1001;
    let session_id = SessionId::new();
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(&runtime_directory, session_id),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        &runtime_directory,
        session_id,
    ))
    .expect("endpoint file written");

    let kill_error = kill_session_in_runtime_directory(
        &runtime_directory,
        None,
        Some(&SessionReference::OwnedSessionId(OwnedSessionId {
            owner_user_id: other_user_id,
            session_id,
        })),
    )
    .expect_err("no folder of that user advertises the session");

    assert_eq!(CliExitCode::from(&kill_error), CliExitCode::SessionNotFound);
    let CliError::SessionNotFound { session_name } = kill_error else {
        panic!("expected SessionNotFound, got {kill_error:?}");
    };
    assert_eq!(session_name, format!("{other_user_id}/{session_id}"));
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn detach_by_owned_session_id_goes_to_that_session_and_refuses_any_other_slash_form() {
    let runtime_directory = PathBuf::from("/home/user/missing-runtime");
    let session_id = SessionId::new();

    assert_eq!(
        resolve_detach_target(&runtime_directory, None, &format!("1001/{session_id}"))
            .expect("the owned id reads without a lookup"),
        (session_id, Some(1001), None)
    );
    let malformed_target_result = resolve_detach_target(&runtime_directory, None, "1001/work");
    let Err(CliError::InvalidArgs { detail }) = malformed_target_result else {
        panic!("expected InvalidArgs, got {malformed_target_result:?}");
    };
    assert_eq!(
        detail,
        "expected `<user id>/session-<uuid>`, such as `1001/session-<uuid>`"
    );
}
