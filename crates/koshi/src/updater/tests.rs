//! Tests for the self-update helpers: version comparison, check scheduling,
//! archive URL construction, bounded downloads, checksum verification, state
//! serialization, the restart confirmation wait, the bounded call to a peer,
//! the router restart, the walk that restarts every running session, a peer
//! that refuses this build's protocol version, the end of a router that
//! refuses it, and the update lock held while servers restart.

use super::*;

use std::io::Write;
use std::sync::mpsc;
use std::thread::JoinHandle;

use koshi_ipc::endpoint::{compute_socket_address, is_update_restarting_servers, EndpointFile};
use koshi_ipc::protocol::{
    ConnectionToken, IpcErrorCode, IpcErrorPayload, IpcRequest, IpcRequestKind, IpcResponse,
    IpcResult, PROTOCOL_VERSION,
};
use koshi_ipc::router::{
    compute_router_socket_address, resolve_router_endpoint_path, RouterHandshake, RouterRequest,
    RouterRequestKind, RouterResponse, RouterResult, ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::transport::{Connection, Listener};
use koshi_test_support::fixtures::build_test_runtime_directory;
#[cfg(unix)]
use koshi_test_support::fixtures::{
    BUSY_PROGRAM_RETRY_INTERVAL_DURATION, BUSY_PROGRAM_WAIT_DURATION,
};

use crate::session_end::tests::{
    end_stand_in_session, format_stand_in_file_name, start_stand_in_session,
};
use serde::de::DeserializeOwned;

/// Bind the router's address in `runtime_directory` and write the endpoint
/// file advertising it, as a router would. Hands back the listener and the
/// token that file carries.
fn bind_advertised_router(runtime_directory: &Path) -> (Listener, ConnectionToken) {
    let connection_token = ConnectionToken::generate();
    let socket_address = compute_router_socket_address(runtime_directory);
    let listener = Listener::bind(&socket_address).expect("bind the stand-in router");
    EndpointFile {
        socket_address,
        connection_token: connection_token.clone(),
        process_id: std::process::id(),
    }
    .write_to_path(&resolve_router_endpoint_path(runtime_directory))
    .expect("write the router endpoint file");
    (listener, connection_token)
}

/// Read one Hello on `connection` and answer it as a router on
/// `reported_build_version` would: a Hello reply when the Hello presents
/// `connection_token`, and the handshake's refusal otherwise.
fn answer_router_hello(
    connection: &mut Connection,
    connection_token: &ConnectionToken,
    reported_build_version: &str,
) {
    let mut router_handshake = RouterHandshake::from_connection_token(connection_token.clone());
    let hello_request: RouterRequest = connection.recv().expect("read the hello");
    let response_result = match router_handshake.validate_request_kind(&hello_request.request_kind)
    {
        Ok(()) => RouterResult::Hello {
            protocol_version: ROUTER_PROTOCOL_VERSION,
            build_version: reported_build_version.to_string(),
        },
        Err(error_response) => RouterResult::Error(error_response),
    };
    send_router_response(connection, hello_request.request_id, response_result);
}

/// Answer `request_id` with `response_result` on `connection`, as a router.
fn send_router_response(
    connection: &mut Connection,
    request_id: u64,
    response_result: RouterResult,
) {
    connection
        .send(&RouterResponse {
            request_id: Some(request_id),
            answer_result: response_result,
        })
        .expect("send the scripted reply");
}

/// Serve one Hello-only connection as a router would: bind the router's
/// address, write the endpoint file advertising it, accept one caller, and
/// answer its Hello with `reported_build_version`.
fn spawn_fake_router_reporting(
    runtime_directory: &Path,
    reported_build_version: &str,
) -> JoinHandle<()> {
    let (listener, connection_token) = bind_advertised_router(runtime_directory);
    let reported_build_version = reported_build_version.to_string();
    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the caller");
        answer_router_hello(&mut connection, &connection_token, &reported_build_version);
    })
}

/// Serve a router that restarts: bind and advertise the router's address,
/// answer the first caller's Hello with `previous_build_version` and its
/// Restart with [`RouterResult::Restarting`], then answer the second caller's
/// Hello with `restarted_build_version`.
fn spawn_fake_router_restarting(
    runtime_directory: &Path,
    previous_build_version: &str,
    restarted_build_version: &str,
) -> JoinHandle<()> {
    let (listener, connection_token) = bind_advertised_router(runtime_directory);
    let previous_build_version = previous_build_version.to_string();
    let restarted_build_version = restarted_build_version.to_string();
    std::thread::spawn(move || {
        let mut restart_connection = listener.accept().expect("accept the restart caller");
        answer_router_hello(
            &mut restart_connection,
            &connection_token,
            &previous_build_version,
        );
        let restart_request: RouterRequest = restart_connection.recv().expect("read the restart");
        assert_eq!(
            restart_request.request_kind,
            RouterRequestKind::Restart,
            "expected a Restart after the Hello"
        );
        send_router_response(
            &mut restart_connection,
            restart_request.request_id,
            RouterResult::Restarting,
        );
        let mut probe_connection = listener.accept().expect("accept the version probe");
        answer_router_hello(
            &mut probe_connection,
            &connection_token,
            &restarted_build_version,
        );
    })
}

/// Run `guarded_call` on its own thread and hand back what it returns. The
/// test fails once 10 seconds pass with no answer.
fn run_before_test_deadline<GuardedAnswer: Send + 'static>(
    guarded_call: impl FnOnce() -> GuardedAnswer + Send + 'static,
) -> GuardedAnswer {
    let (guarded_answer_sender, guarded_answer_receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = guarded_answer_sender.send(guarded_call());
    });
    guarded_answer_receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the call ends once its wait runs out")
}

/// Accept one caller on `listener`, read its Hello and its Restart as
/// `PeerRequest`, and answer neither. The connection stays open until
/// `release_receiver` hears from the test or its sender drops.
fn spawn_peer_that_never_answers<PeerRequest: DeserializeOwned + 'static>(
    listener: Listener,
    release_receiver: mpsc::Receiver<()>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the caller");
        let _hello_request: PeerRequest = connection.recv().expect("read the hello");
        let _restart_request: PeerRequest = connection.recv().expect("read the restart");
        let _ = release_receiver.recv();
    })
}

#[test]
fn a_router_reporting_the_installed_version_confirms_the_restart() {
    let runtime_directory = build_test_runtime_directory();
    let router_thread = spawn_fake_router_reporting(runtime_directory.path(), "3.3.3");

    let version_probe_outcome = wait_for_version("3.3.3", Duration::from_secs(5), || {
        probe_router_version(runtime_directory.path())
    });

    assert_eq!(version_probe_outcome, VersionProbeOutcome::Installed);
    router_thread
        .join()
        .expect("the stand-in served its connection");
}

#[test]
fn a_router_still_on_another_version_is_reported_after_the_wait() {
    let runtime_directory = build_test_runtime_directory();
    let router_thread = spawn_fake_router_reporting(runtime_directory.path(), "1.0.0");

    let version_probe_outcome = wait_for_version("2.0.0", Duration::from_millis(250), || {
        probe_router_version(runtime_directory.path())
    });

    assert_eq!(
        version_probe_outcome,
        VersionProbeOutcome::OtherVersion("1.0.0".to_string())
    );
    router_thread
        .join()
        .expect("the stand-in served its connection");
}

#[test]
fn no_router_answering_reports_no_version_after_the_wait() {
    let runtime_directory = build_test_runtime_directory();
    assert_eq!(
        wait_for_version("2.0.0", Duration::from_millis(50), || probe_router_version(
            runtime_directory.path()
        )),
        VersionProbeOutcome::Silent
    );
}

#[test]
fn a_peer_call_that_answers_inside_its_bound_hands_back_its_answer() {
    assert_eq!(
        run_peer_call_within(Duration::from_secs(5), || 7_u32),
        Some(7)
    );
}

#[test]
fn a_peer_call_that_outlasts_its_bound_gives_no_answer() {
    let (release_sender, release_receiver) = mpsc::channel::<()>();

    let peer_answer = run_peer_call_within(Duration::from_millis(100), move || {
        let _ = release_receiver.recv_timeout(Duration::from_secs(10));
        7_u32
    });

    assert_eq!(peer_answer, None);
    drop(release_sender);
}

#[test]
fn asking_the_router_to_restart_confirms_it_once_its_hello_reports_the_installed_version() {
    let runtime_directory = build_test_runtime_directory();
    let router_thread = spawn_fake_router_restarting(runtime_directory.path(), "2.0.0", "3.3.3");

    let router_outcome =
        restart_advertised_router(runtime_directory.path(), "3.3.3", Duration::from_secs(5))
            .expect("the router takes the restart");

    assert_eq!(router_outcome, Some(VersionProbeOutcome::Installed));
    router_thread
        .join()
        .expect("the stand-in served its connections");
}

#[test]
fn asking_no_running_router_to_restart_restarts_nothing() {
    let runtime_directory = build_test_runtime_directory();

    let router_outcome =
        restart_advertised_router(runtime_directory.path(), "3.3.3", Duration::from_secs(5))
            .expect("no running router is no failure");

    assert_eq!(router_outcome, None);
}

#[test]
fn a_router_that_never_answers_its_restart_request_is_reported_silent_after_the_wait() {
    let runtime_directory = build_test_runtime_directory();
    let (listener, _connection_token) = bind_advertised_router(runtime_directory.path());
    let (release_sender, release_receiver) = mpsc::channel();
    let router_thread = spawn_peer_that_never_answers::<RouterRequest>(listener, release_receiver);

    let router_runtime_directory = runtime_directory.path().to_path_buf();
    let router_outcome = run_before_test_deadline(move || {
        restart_advertised_router(
            &router_runtime_directory,
            "3.3.3",
            Duration::from_millis(300),
        )
    })
    .expect("the router takes the connection");

    assert_eq!(router_outcome, Some(VersionProbeOutcome::Silent));
    drop(release_sender);
    router_thread
        .join()
        .expect("the stand-in held its connection");
}

/// The sentence a stand-in peer refuses this build's protocol version with.
const VERSION_REFUSAL_SENTENCE: &str =
    "this peer speaks protocol 3 to 4; the caller asked for 5 to 6";

/// Accept one caller on `listener`, refuse its Hello for this build's
/// protocol version with [`VERSION_REFUSAL_SENTENCE`], as a router would, then
/// read the one request a caller writes right after its Hello, when it wrote
/// one.
fn spawn_router_refusing_this_builds_protocol_version(listener: Listener) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the caller");
        let hello_request: RouterRequest = connection.recv().expect("read the hello");
        send_router_response(
            &mut connection,
            hello_request.request_id,
            RouterResult::Error(IpcErrorPayload {
                code: IpcErrorCode::UnsupportedVersion,
                message: VERSION_REFUSAL_SENTENCE.to_string(),
            }),
        );
        let _next_request: Result<RouterRequest, _> = connection.recv();
    })
}

/// Write the program file of the router in `runtime_directory`, naming
/// `process_id`, `build_version`, and `program_path`, as a router does once it
/// binds. Hands back what was written.
fn write_router_program_file(
    runtime_directory: &Path,
    process_id: u32,
    build_version: &str,
    program_path: &Path,
) -> ServerProgramFile {
    let router_program_file = ServerProgramFile {
        process_id,
        build_version: build_version.to_string(),
        program_path: program_path.display().to_string(),
    };
    router_program_file
        .write_to_path(&resolve_router_program_file_path(runtime_directory))
        .expect("write the router program file");
    router_program_file
}

#[test]
fn a_router_refusing_this_builds_protocol_version_gives_protocol_version_refused() {
    let runtime_directory = build_test_runtime_directory();
    let (listener, _connection_token) = bind_advertised_router(runtime_directory.path());
    let router_thread = spawn_router_refusing_this_builds_protocol_version(listener);

    let router_restart =
        restart_advertised_router(runtime_directory.path(), "3.3.3", Duration::from_secs(5));

    let Err(CliError::ProtocolVersionRefused { detail }) = router_restart else {
        panic!("expected ProtocolVersionRefused, got {router_restart:?}");
    };
    assert_eq!(detail, VERSION_REFUSAL_SENTENCE);
    router_thread
        .join()
        .expect("the stand-in served its connection");
}

/// Write the router endpoint file of `runtime_directory`, naming
/// `process_id` and a router socket nothing listens on.
fn write_router_endpoint_naming_process(runtime_directory: &Path, process_id: u32) {
    EndpointFile {
        socket_address: compute_router_socket_address(runtime_directory),
        connection_token: ConnectionToken::generate(),
        process_id,
    }
    .write_to_path(&resolve_router_endpoint_path(runtime_directory))
    .expect("write the router endpoint file");
}

#[test]
fn stop_incompatible_router_ends_the_confirmed_router_process_and_nothing_under_it() {
    let runtime_directory = build_test_runtime_directory();
    let (mut router_child, router_record, member_records) =
        start_stand_in_session(runtime_directory.path(), format_stand_in_file_name(true));
    write_router_endpoint_naming_process(runtime_directory.path(), router_child.id());

    assert!(stop_incompatible_router(
        runtime_directory.path(),
        "3.3.3",
        VERSION_REFUSAL_SENTENCE
    ));

    assert!(process_tree::wait_for_processes_to_end(
        std::slice::from_ref(&router_record),
        Duration::from_secs(5)
    ));
    router_child.wait().expect("the router process is reaped");
    assert!(member_records.iter().all(process_tree::is_process_running));
    process_tree::stop_processes(&member_records, Duration::ZERO);
}

#[test]
fn stop_incompatible_router_leaves_an_unconfirmed_process_running() {
    let runtime_directory = build_test_runtime_directory();
    let (router_child, router_record, member_records) =
        start_stand_in_session(runtime_directory.path(), format_stand_in_file_name(false));
    write_router_endpoint_naming_process(runtime_directory.path(), router_child.id());

    assert!(!stop_incompatible_router(
        runtime_directory.path(),
        "3.3.3",
        VERSION_REFUSAL_SENTENCE
    ));

    assert!(process_tree::is_process_running(&router_record));
    assert!(member_records.iter().all(process_tree::is_process_running));
    end_stand_in_session(router_child, &member_records);
}

#[test]
fn restart_router_into_version_ends_a_confirmed_router_that_refuses_this_builds_protocol_version() {
    // The test serves the router's socket and refuses the Hello for its
    // protocol version. The endpoint file names a stand-in koshi process.
    let runtime_directory = build_test_runtime_directory();
    let (mut router_child, router_record, member_records) =
        start_stand_in_session(runtime_directory.path(), format_stand_in_file_name(true));
    let router_socket_address = compute_router_socket_address(runtime_directory.path());
    let router_listener = Listener::bind(&router_socket_address).expect("bind the stand-in router");
    write_router_endpoint_naming_process(runtime_directory.path(), router_child.id());
    let refusing_router_thread =
        spawn_router_refusing_this_builds_protocol_version(router_listener);

    let has_router_restarted =
        restart_router_into_version(runtime_directory.path(), "3.3.3", RestartScope::EveryServer);

    let _ = Connection::connect(&router_socket_address);
    refusing_router_thread
        .join()
        .expect("the stand-in served its connection");
    assert!(has_router_restarted);
    assert!(process_tree::wait_for_processes_to_end(
        std::slice::from_ref(&router_record),
        Duration::from_secs(5)
    ));
    router_child.wait().expect("the router process is reaped");
    assert!(member_records.iter().all(process_tree::is_process_running));
    process_tree::stop_processes(&member_records, Duration::ZERO);
}

#[test]
fn stop_incompatible_router_ends_nothing_without_a_router_endpoint_file() {
    let runtime_directory = build_test_runtime_directory();

    assert!(!stop_incompatible_router(
        runtime_directory.path(),
        "3.3.3",
        VERSION_REFUSAL_SENTENCE
    ));
}

#[test]
fn stop_incompatible_router_leaves_a_router_on_the_newer_installed_version_running() {
    let runtime_directory = build_test_runtime_directory();
    let (router_child, router_record, member_records) =
        start_stand_in_session(runtime_directory.path(), format_stand_in_file_name(true));
    write_router_endpoint_naming_process(runtime_directory.path(), router_child.id());
    write_router_program_file(
        runtime_directory.path(),
        router_child.id(),
        "9999.0.0",
        Path::new("/usr/local/bin/koshi"),
    );

    assert!(stop_incompatible_router(
        runtime_directory.path(),
        "9999.0.0",
        VERSION_REFUSAL_SENTENCE
    ));

    assert!(process_tree::is_process_running(&router_record));
    end_stand_in_session(router_child, &member_records);
}

#[test]
fn stop_incompatible_router_leaves_a_router_on_another_newer_version_running() {
    let runtime_directory = build_test_runtime_directory();
    let (router_child, router_record, member_records) =
        start_stand_in_session(runtime_directory.path(), format_stand_in_file_name(true));
    write_router_endpoint_naming_process(runtime_directory.path(), router_child.id());
    write_router_program_file(
        runtime_directory.path(),
        router_child.id(),
        "9998.0.0",
        Path::new("/usr/local/bin/koshi"),
    );

    assert!(!stop_incompatible_router(
        runtime_directory.path(),
        "9999.0.0",
        VERSION_REFUSAL_SENTENCE
    ));

    assert!(process_tree::is_process_running(&router_record));
    end_stand_in_session(router_child, &member_records);
}

#[test]
fn stop_incompatible_router_ends_a_router_whose_program_file_names_an_older_version() {
    let runtime_directory = build_test_runtime_directory();
    let (mut router_child, router_record, member_records) =
        start_stand_in_session(runtime_directory.path(), format_stand_in_file_name(true));
    write_router_endpoint_naming_process(runtime_directory.path(), router_child.id());
    write_router_program_file(
        runtime_directory.path(),
        router_child.id(),
        "0.0.1",
        Path::new("/usr/local/bin/koshi"),
    );

    assert!(stop_incompatible_router(
        runtime_directory.path(),
        "9999.0.0",
        VERSION_REFUSAL_SENTENCE
    ));

    assert!(process_tree::wait_for_processes_to_end(
        std::slice::from_ref(&router_record),
        Duration::from_secs(5)
    ));
    router_child.wait().expect("the router process is reaped");
    process_tree::stop_processes(&member_records, Duration::ZERO);
}

#[test]
fn a_release_update_leaves_a_router_already_on_the_installed_version_unasked() {
    // The stand-in router accepts nothing: a Restart request would wait out
    // the whole confirmation wait, past the test deadline.
    let runtime_directory = build_test_runtime_directory();
    let (_listener, _connection_token) = bind_advertised_router(runtime_directory.path());
    write_router_program_file(
        runtime_directory.path(),
        std::process::id(),
        "3.3.3",
        Path::new("/usr/local/bin/koshi"),
    );

    let router_runtime_directory = runtime_directory.path().to_path_buf();
    let has_router_restarted = run_before_test_deadline(move || {
        restart_router_into_version(
            &router_runtime_directory,
            "3.3.3",
            RestartScope::ServersNotOnVersion {
                program_path: Path::new("/usr/local/bin/koshi"),
            },
        )
    });

    assert!(has_router_restarted);
}

#[test]
fn a_router_refusing_this_builds_protocol_version_reports_the_version_its_program_file_holds() {
    let runtime_directory = build_test_runtime_directory();
    let (listener, _connection_token) = bind_advertised_router(runtime_directory.path());
    write_router_program_file(
        runtime_directory.path(),
        std::process::id(),
        "3.3.3",
        Path::new("/usr/local/bin/koshi"),
    );
    let router_thread = spawn_router_refusing_this_builds_protocol_version(listener);

    assert_eq!(
        probe_router_version(runtime_directory.path()),
        Some("3.3.3".to_string())
    );
    router_thread
        .join()
        .expect("the stand-in served its connection");
}

// --- restarting every running session ---

/// What a stand-in session answers with.
struct SessionRestartScript {
    /// The answer to the Restart request.
    restart_result: IpcResult,
    /// The build version every Hello answer of this session carries.
    reported_build_version: String,
}

/// Bind `session_id`'s address in `runtime_directory` and write the endpoint
/// file advertising it, as a session would. Hands back the listener and the
/// token that file carries.
fn bind_advertised_session(
    runtime_directory: &Path,
    session_id: SessionId,
) -> (Listener, ConnectionToken) {
    let connection_token = ConnectionToken::generate();
    let socket_address = compute_socket_address(runtime_directory, session_id);
    let listener = Listener::bind(&socket_address).expect("bind the stand-in session");
    EndpointFile {
        socket_address,
        connection_token: connection_token.clone(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("write the session endpoint file");
    (listener, connection_token)
}

/// Serve `connection_count` callers as a session would: bind the session's address,
/// write the endpoint file advertising it, then answer that many callers.
///
/// The first caller writes a Hello and a Restart back to back and is answered
/// per `session_script`. Every caller after the first writes a Hello alone and is
/// answered with the script's build version. A caller arriving once `connection_count`
/// are served finds nothing listening, which is what a session that is
/// replacing its own image looks like.
fn spawn_fake_session(
    runtime_directory: &Path,
    session_id: SessionId,
    session_script: SessionRestartScript,
    connection_count: usize,
) -> JoinHandle<()> {
    let (listener, connection_token) = bind_advertised_session(runtime_directory, session_id);

    std::thread::spawn(move || {
        let SessionRestartScript {
            restart_result,
            reported_build_version,
        } = session_script;
        for connection_index in 0..connection_count {
            let mut connection = listener.accept().expect("accept the caller");
            let hello_request: IpcRequest = connection.recv().expect("read the hello");
            let IpcRequestKind::Hello {
                connection_token: presented_connection_token,
                ..
            } = &hello_request.request_kind
            else {
                panic!("expected a Hello first");
            };
            assert_eq!(
                presented_connection_token, &connection_token,
                "the caller presents the endpoint file's token"
            );

            if connection_index == 0 {
                let restart_request: IpcRequest = connection.recv().expect("read the restart");
                assert_eq!(
                    restart_request.request_kind,
                    IpcRequestKind::Restart,
                    "expected a Restart after the Hello"
                );
                send_ipc_response(
                    &mut connection,
                    hello_request.request_id,
                    IpcResult::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        build_version: reported_build_version.clone(),
                    },
                );
                send_ipc_response(
                    &mut connection,
                    restart_request.request_id,
                    restart_result.clone(),
                );
            } else {
                send_ipc_response(
                    &mut connection,
                    hello_request.request_id,
                    IpcResult::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        build_version: reported_build_version.clone(),
                    },
                );
            }
        }
    })
}

/// Answer `request_id` with `response_result` on `connection`.
fn send_ipc_response(connection: &mut Connection, request_id: u64, response_result: IpcResult) {
    connection
        .send(&IpcResponse {
            request_id: Some(request_id),
            answer_result: response_result,
        })
        .expect("send the scripted reply");
}

/// Accept one caller on `listener`, refuse its Hello for this build's
/// protocol version with [`VERSION_REFUSAL_SENTENCE`], as a session would,
/// then read the one request a caller writes right after its Hello, when it
/// wrote one.
fn spawn_session_refusing_this_builds_protocol_version(listener: Listener) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the caller");
        let hello_request: IpcRequest = connection.recv().expect("read the hello");
        send_ipc_response(
            &mut connection,
            hello_request.request_id,
            IpcResult::Error(IpcErrorPayload {
                code: IpcErrorCode::UnsupportedVersion,
                message: VERSION_REFUSAL_SENTENCE.to_string(),
            }),
        );
        let _next_request: Result<IpcRequest, _> = connection.recv();
    })
}

/// Write the program file of `session_id` in `runtime_directory`, naming this
/// test process, `build_version`, and `program_path`, as a session does once
/// it binds. Hands back what was written.
fn write_session_program_file(
    runtime_directory: &Path,
    session_id: SessionId,
    build_version: &str,
    program_path: &Path,
) -> ServerProgramFile {
    let session_program_file = ServerProgramFile {
        process_id: std::process::id(),
        build_version: build_version.to_string(),
        program_path: program_path.display().to_string(),
    };
    session_program_file
        .write_to_path(&ServerProgramFile::resolve_session_program_file_path(
            runtime_directory,
            session_id,
        ))
        .expect("write the session program file");
    session_program_file
}

/// A test directory holding two empty files, `koshi` and `other-koshi`, and
/// the path of each.
fn build_program_files() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let program_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("a program directory");
    let program_path = program_directory.path().join("koshi");
    let other_program_path = program_directory.path().join("other-koshi");
    fs::write(&program_path, b"").expect("write the program file");
    fs::write(&other_program_path, b"").expect("write the other program file");
    (program_directory, program_path, other_program_path)
}

#[test]
fn a_session_reporting_the_installed_version_confirms_its_restart() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let session_thread = spawn_fake_session(
        runtime_directory.path(),
        session_id,
        SessionRestartScript {
            restart_result: IpcResult::Restarting,
            reported_build_version: "3.3.3".to_string(),
        },
        2,
    );

    let session_outcomes = restart_advertised_sessions(
        runtime_directory.path(),
        "3.3.3",
        RestartScope::EveryServer,
        Duration::from_secs(5),
    )
    .expect("read the runtime directory");

    assert_eq!(
        session_outcomes,
        vec![(session_id, SessionOutcome::Confirmed)]
    );
    session_thread
        .join()
        .expect("the stand-in served its connections");
}

#[test]
fn a_session_still_on_the_old_version_is_reported_after_the_wait() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let session_thread = spawn_fake_session(
        runtime_directory.path(),
        session_id,
        SessionRestartScript {
            restart_result: IpcResult::Restarting,
            reported_build_version: "1.0.0".to_string(),
        },
        2,
    );

    let session_outcomes = restart_advertised_sessions(
        runtime_directory.path(),
        "2.0.0",
        RestartScope::EveryServer,
        Duration::from_millis(250),
    )
    .expect("read the runtime directory");

    assert_eq!(
        session_outcomes,
        vec![(
            session_id,
            SessionOutcome::StillOnVersion("1.0.0".to_string())
        )]
    );
    session_thread
        .join()
        .expect("the stand-in served its connections");
}

#[test]
fn a_session_refusing_with_the_malformed_request_code_is_reported_failed_with_its_sentence() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let session_thread = spawn_fake_session(
        runtime_directory.path(),
        session_id,
        SessionRestartScript {
            restart_result: IpcResult::Error(IpcErrorPayload {
                code: IpcErrorCode::MalformedRequest,
                message: "the binary at /opt/koshi could not be read: Exec format error (os \
                          error 8)"
                    .to_string(),
            }),
            reported_build_version: "1.0.0".to_string(),
        },
        1,
    );

    let session_outcomes = restart_advertised_sessions(
        runtime_directory.path(),
        "3.3.3",
        RestartScope::EveryServer,
        Duration::from_secs(5),
    )
    .expect("read the runtime directory");

    assert_eq!(
        session_outcomes,
        vec![(
            session_id,
            SessionOutcome::Failed(
                "IPC unavailable: the binary at /opt/koshi could not be read: Exec format error \
                 (os error 8)"
                    .to_string()
            )
        )]
    );
    session_thread
        .join()
        .expect("the stand-in served its connection");
}

#[test]
fn a_session_refusing_this_builds_protocol_version_is_reported_incompatible_with_its_sentence() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (listener, _connection_token) =
        bind_advertised_session(runtime_directory.path(), session_id);
    let session_thread = spawn_session_refusing_this_builds_protocol_version(listener);

    let session_outcomes = restart_advertised_sessions(
        runtime_directory.path(),
        "3.3.3",
        RestartScope::EveryServer,
        Duration::from_secs(5),
    )
    .expect("read the runtime directory");

    assert_eq!(
        session_outcomes,
        vec![(
            session_id,
            SessionOutcome::Incompatible(format!(
                "{VERSION_REFUSAL_SENTENCE}; it runs a koshi older than {APP_VERSION} that cannot \
                 restart into it; end it with: koshi kill-session {session_id}"
            ))
        )]
    );
    session_thread
        .join()
        .expect("the stand-in served its connection");
}

#[test]
fn one_session_refusing_still_leaves_every_other_session_asked() {
    let runtime_directory = build_test_runtime_directory();
    let confirmed_session_id_one = SessionId::new();
    let refusing_session_id = SessionId::new();
    let confirmed_session_id_two = SessionId::new();
    let fake_session_threads = vec![
        spawn_fake_session(
            runtime_directory.path(),
            confirmed_session_id_one,
            SessionRestartScript {
                restart_result: IpcResult::Restarting,
                reported_build_version: "3.3.3".to_string(),
            },
            2,
        ),
        spawn_fake_session(
            runtime_directory.path(),
            refusing_session_id,
            SessionRestartScript {
                restart_result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::RequestFailed,
                    message: "a pane is mid-write".to_string(),
                }),
                reported_build_version: "3.3.3".to_string(),
            },
            1,
        ),
        spawn_fake_session(
            runtime_directory.path(),
            confirmed_session_id_two,
            SessionRestartScript {
                restart_result: IpcResult::Restarting,
                reported_build_version: "3.3.3".to_string(),
            },
            2,
        ),
    ];

    let mut session_outcomes = restart_advertised_sessions(
        runtime_directory.path(),
        "3.3.3",
        RestartScope::EveryServer,
        Duration::from_secs(5),
    )
    .expect("read the runtime directory");
    session_outcomes.sort_by_key(|(session_id, _)| session_id.to_string());
    let mut expected_session_outcomes = vec![
        (confirmed_session_id_one, SessionOutcome::Confirmed),
        (
            refusing_session_id,
            SessionOutcome::Failed("IPC unavailable: a pane is mid-write".to_string()),
        ),
        (confirmed_session_id_two, SessionOutcome::Confirmed),
    ];
    expected_session_outcomes.sort_by_key(|(session_id, _)| session_id.to_string());

    assert_eq!(session_outcomes, expected_session_outcomes);
    for session_thread in fake_session_threads {
        session_thread
            .join()
            .expect("the stand-in served its connections");
    }
}

#[test]
fn no_running_session_leaves_the_router_confirmation_unchanged() {
    let runtime_directory = build_test_runtime_directory();
    let router_thread = spawn_fake_router_reporting(runtime_directory.path(), "3.3.3");

    let session_outcomes = restart_advertised_sessions(
        runtime_directory.path(),
        "3.3.3",
        RestartScope::EveryServer,
        Duration::from_secs(5),
    )
    .expect("read the runtime directory");

    assert_eq!(session_outcomes, Vec::new());
    assert_eq!(
        wait_for_version("3.3.3", Duration::from_secs(5), || probe_router_version(
            runtime_directory.path()
        )),
        VersionProbeOutcome::Installed
    );
    router_thread
        .join()
        .expect("the stand-in served its connection");
}

#[test]
fn a_session_that_never_answers_its_restart_request_is_reported_unconfirmed_after_the_wait() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (listener, _connection_token) =
        bind_advertised_session(runtime_directory.path(), session_id);
    let (release_sender, release_receiver) = mpsc::channel();
    let session_thread = spawn_peer_that_never_answers::<IpcRequest>(listener, release_receiver);

    let walk_runtime_directory = runtime_directory.path().to_path_buf();
    let session_outcomes = run_before_test_deadline(move || {
        restart_advertised_sessions(
            &walk_runtime_directory,
            "3.3.3",
            RestartScope::EveryServer,
            Duration::from_millis(300),
        )
    })
    .expect("read the runtime directory");

    assert_eq!(
        session_outcomes,
        vec![(session_id, SessionOutcome::Unconfirmed)]
    );
    drop(release_sender);
    session_thread
        .join()
        .expect("the stand-in held its connection");
}

#[test]
fn a_release_update_leaves_a_session_already_on_the_installed_version_unasked() {
    // The stand-in session accepts nothing: a Restart request would end as
    // unconfirmed once the wait runs out.
    let runtime_directory = build_test_runtime_directory();
    let (_program_directory, program_path, _other_program_path) = build_program_files();
    let session_id = SessionId::new();
    let (_listener, _connection_token) =
        bind_advertised_session(runtime_directory.path(), session_id);
    write_session_program_file(runtime_directory.path(), session_id, "3.3.3", &program_path);

    let session_outcomes = restart_advertised_sessions(
        runtime_directory.path(),
        "3.3.3",
        RestartScope::ServersNotOnVersion {
            program_path: &program_path,
        },
        Duration::from_millis(300),
    )
    .expect("read the runtime directory");

    assert_eq!(
        session_outcomes,
        vec![(session_id, SessionOutcome::AlreadyOnVersion)]
    );
}

#[test]
fn a_release_update_leaves_a_session_on_another_program_file_unasked() {
    let runtime_directory = build_test_runtime_directory();
    let (_program_directory, program_path, other_program_path) = build_program_files();
    let session_id = SessionId::new();
    let (_listener, _connection_token) =
        bind_advertised_session(runtime_directory.path(), session_id);
    let session_program_file = write_session_program_file(
        runtime_directory.path(),
        session_id,
        "2.0.0",
        &other_program_path,
    );

    let session_outcomes = restart_advertised_sessions(
        runtime_directory.path(),
        "3.3.3",
        RestartScope::ServersNotOnVersion {
            program_path: &program_path,
        },
        Duration::from_millis(300),
    )
    .expect("read the runtime directory");

    assert_eq!(
        session_outcomes,
        vec![(
            session_id,
            SessionOutcome::OnOtherProgramFile(session_program_file)
        )]
    );
}

#[test]
fn a_release_update_asks_a_session_on_this_program_file_and_an_older_version() {
    let runtime_directory = build_test_runtime_directory();
    let (_program_directory, program_path, _other_program_path) = build_program_files();
    let session_id = SessionId::new();
    let session_thread = spawn_fake_session(
        runtime_directory.path(),
        session_id,
        SessionRestartScript {
            restart_result: IpcResult::Restarting,
            reported_build_version: "3.3.3".to_string(),
        },
        2,
    );
    write_session_program_file(runtime_directory.path(), session_id, "2.0.0", &program_path);

    let session_outcomes = restart_advertised_sessions(
        runtime_directory.path(),
        "3.3.3",
        RestartScope::ServersNotOnVersion {
            program_path: &program_path,
        },
        Duration::from_secs(5),
    )
    .expect("read the runtime directory");

    assert_eq!(
        session_outcomes,
        vec![(session_id, SessionOutcome::Confirmed)]
    );
    session_thread
        .join()
        .expect("the stand-in served its connections");
}

#[test]
fn restarting_every_server_asks_a_session_already_on_the_installed_version() {
    let runtime_directory = build_test_runtime_directory();
    let (_program_directory, _program_path, other_program_path) = build_program_files();
    let session_id = SessionId::new();
    let session_thread = spawn_fake_session(
        runtime_directory.path(),
        session_id,
        SessionRestartScript {
            restart_result: IpcResult::Restarting,
            reported_build_version: "3.3.3".to_string(),
        },
        2,
    );
    write_session_program_file(
        runtime_directory.path(),
        session_id,
        "3.3.3",
        &other_program_path,
    );

    let session_outcomes = restart_advertised_sessions(
        runtime_directory.path(),
        "3.3.3",
        RestartScope::EveryServer,
        Duration::from_secs(5),
    )
    .expect("read the runtime directory");

    assert_eq!(
        session_outcomes,
        vec![(session_id, SessionOutcome::Confirmed)]
    );
    session_thread
        .join()
        .expect("the stand-in served its connections");
}

#[test]
fn a_session_refusing_this_builds_protocol_version_on_the_newer_installed_version_is_already_on_it()
{
    let runtime_directory = build_test_runtime_directory();
    let (_program_directory, program_path, _other_program_path) = build_program_files();
    let session_id = SessionId::new();
    let (listener, _connection_token) =
        bind_advertised_session(runtime_directory.path(), session_id);
    write_session_program_file(
        runtime_directory.path(),
        session_id,
        "9999.0.0",
        &program_path,
    );
    let session_thread = spawn_session_refusing_this_builds_protocol_version(listener);

    let session_outcomes = restart_advertised_sessions(
        runtime_directory.path(),
        "9999.0.0",
        RestartScope::EveryServer,
        Duration::from_secs(5),
    )
    .expect("read the runtime directory");

    assert_eq!(
        session_outcomes,
        vec![(session_id, SessionOutcome::AlreadyOnVersion)]
    );
    run_before_test_deadline(move || session_thread.join())
        .expect("the session was asked, and the stand-in served its connection");
}

#[test]
fn a_session_refusing_this_builds_protocol_version_on_an_older_installed_version_is_incompatible() {
    let runtime_directory = build_test_runtime_directory();
    let (_program_directory, _program_path, other_program_path) = build_program_files();
    let session_id = SessionId::new();
    let (listener, _connection_token) =
        bind_advertised_session(runtime_directory.path(), session_id);
    write_session_program_file(
        runtime_directory.path(),
        session_id,
        "0.0.1",
        &other_program_path,
    );
    let session_thread = spawn_session_refusing_this_builds_protocol_version(listener);

    let session_outcomes = restart_advertised_sessions(
        runtime_directory.path(),
        "0.0.1",
        RestartScope::EveryServer,
        Duration::from_secs(5),
    )
    .expect("read the runtime directory");

    assert_eq!(
        session_outcomes,
        vec![(
            session_id,
            SessionOutcome::Incompatible(format!(
                "{VERSION_REFUSAL_SENTENCE}; it runs koshi 0.0.1 from {}, a program file this \
                 koshi does not replace; use that koshi for it, or end it with: koshi \
                 kill-session {session_id}",
                other_program_path.display()
            ))
        )]
    );
    session_thread
        .join()
        .expect("the stand-in served its connection");
}

#[test]
fn a_session_refusing_this_builds_protocol_version_reports_the_version_its_program_file_holds() {
    let runtime_directory = build_test_runtime_directory();
    let (_program_directory, program_path, _other_program_path) = build_program_files();
    let session_id = SessionId::new();
    let (listener, _connection_token) =
        bind_advertised_session(runtime_directory.path(), session_id);
    write_session_program_file(runtime_directory.path(), session_id, "3.3.3", &program_path);
    let session_thread = spawn_session_refusing_this_builds_protocol_version(listener);

    assert_eq!(
        probe_session_version(runtime_directory.path(), session_id),
        Some("3.3.3".to_string())
    );
    session_thread
        .join()
        .expect("the stand-in served its connection");
}

#[test]
fn strip_version_prefix_drops_a_leading_v_only() {
    assert_eq!(strip_version_prefix("v1.2.3"), "1.2.3");
    assert_eq!(strip_version_prefix("1.2.3"), "1.2.3");
    assert_eq!(strip_version_prefix("version"), "ersion");
}

#[test]
fn a_far_higher_release_tag_is_newer() {
    assert!(is_release_newer("v9999.0.0"));
    assert!(is_release_newer("9999.0.0"));
}

#[test]
fn a_zero_release_tag_is_not_newer() {
    assert!(!is_release_newer("v0.0.0"));
}

#[test]
fn the_current_build_is_not_newer_than_itself() {
    assert!(!is_release_newer(APP_VERSION));
}

#[test]
fn a_malformed_release_tag_is_not_newer() {
    assert!(!is_release_newer("not-a-version"));
    assert!(!is_release_newer("v"));
}

#[test]
fn a_first_ever_check_is_due() {
    let update_state = UpdateState::default();
    assert!(is_update_due(&update_state, 14));
}

#[test]
fn a_check_within_the_interval_is_not_due() {
    let update_state = UpdateState {
        last_check_unix_seconds: Some(get_current_unix_seconds()),
    };
    assert!(!is_update_due(&update_state, 14));
}

#[test]
fn a_check_older_than_the_interval_is_due() {
    let fifteen_days_ago_unix_seconds =
        get_current_unix_seconds().saturating_sub(15 * SECONDS_PER_DAY);
    let update_state = UpdateState {
        last_check_unix_seconds: Some(fifteen_days_ago_unix_seconds),
    };
    assert!(is_update_due(&update_state, 14));
}

#[test]
fn a_zero_interval_is_always_due() {
    let update_state = UpdateState {
        last_check_unix_seconds: Some(get_current_unix_seconds()),
    };
    assert!(is_update_due(&update_state, 0));
}

#[test]
fn compute_binary_url_names_this_platforms_release_archive() {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    let archive_file_name = Some("koshi-v0.2.0-darwin-arm64.tar.gz");
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    let archive_file_name = Some("koshi-v0.2.0-darwin-amd64.tar.gz");
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    let archive_file_name = Some("koshi-v0.2.0-linux-arm64.tar.gz");
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let archive_file_name = Some("koshi-v0.2.0-linux-amd64.tar.gz");
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    let archive_file_name = Some("koshi-v0.2.0-windows-arm64.zip");
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    let archive_file_name = Some("koshi-v0.2.0-windows-amd64.zip");
    #[cfg(not(all(
        any(target_os = "macos", target_os = "linux", target_os = "windows"),
        any(target_arch = "aarch64", target_arch = "x86_64")
    )))]
    let archive_file_name: Option<&str> = None;

    assert_eq!(
        compute_binary_url("v0.2.0"),
        archive_file_name.map(|archive_file_name| format!(
            "https://github.com/gohyuhan/koshi/releases/download/v0.2.0/{archive_file_name}"
        ))
    );
}

#[test]
fn a_standard_shasum_row_returns_the_archive_checksum() {
    let archive_file_name = "koshi-v0.5.0-linux-amd64.tar.gz";
    let checksums_text = format!(
        "0000000000000000000000000000000000000000000000000000000000000000  other-file\nBA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD  {archive_file_name}\n"
    );

    assert_eq!(
        find_release_checksum(&checksums_text, archive_file_name)
            .expect("the archive row has a valid checksum"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn a_missing_checksum_row_names_the_release_archive() {
    let archive_file_name = "koshi-v0.5.0-linux-amd64.tar.gz";
    let checksum_error = find_release_checksum(
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  other-file\n",
        archive_file_name,
    )
    .expect_err("an absent archive row must fail");

    assert_eq!(
        checksum_error,
        "checksums.txt has no row for release archive koshi-v0.5.0-linux-amd64.tar.gz"
    );
}

#[test]
fn duplicate_checksum_rows_name_the_release_archive() {
    let archive_file_name = "koshi-v0.5.0-linux-amd64.tar.gz";
    let checksums_text = format!(
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  {archive_file_name}\nba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  {archive_file_name}\n"
    );
    let checksum_error = find_release_checksum(&checksums_text, archive_file_name)
        .expect_err("duplicate archive rows must fail");

    assert_eq!(
        checksum_error,
        "checksums.txt has multiple rows for release archive koshi-v0.5.0-linux-amd64.tar.gz"
    );
}

#[test]
fn malformed_checksum_row_for_the_release_archive_is_rejected() {
    let archive_file_name = "koshi-v0.5.0-linux-amd64.tar.gz";
    let checksums_text = format!(
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  {archive_file_name} extra\n"
    );
    let checksum_error = find_release_checksum(&checksums_text, archive_file_name)
        .expect_err("an archive row with extra fields must fail");

    assert_eq!(
        checksum_error,
        "checksums.txt has a malformed row for release archive koshi-v0.5.0-linux-amd64.tar.gz"
    );
}

#[test]
fn invalid_checksum_for_the_release_archive_is_rejected() {
    let archive_file_name = "koshi-v0.5.0-linux-amd64.tar.gz";
    let checksums_text = format!("{}  {archive_file_name}\n", "z".repeat(64));
    let checksum_error = find_release_checksum(&checksums_text, archive_file_name)
        .expect_err("a non-hex checksum must fail");

    assert_eq!(
        checksum_error,
        "checksums.txt has an invalid SHA-256 checksum for release archive koshi-v0.5.0-linux-amd64.tar.gz"
    );
}

#[test]
fn a_stream_at_the_byte_limit_is_copied() {
    let mut release_file_reader = b"abc".as_slice();
    let mut copied_bytes = Vec::new();

    copy_stream_with_byte_limit(&mut release_file_reader, &mut copied_bytes, 3)
        .expect("a stream at the limit is accepted");

    assert_eq!(copied_bytes, b"abc");
}

#[test]
fn a_stream_over_the_byte_limit_is_rejected_without_copying_the_extra_byte() {
    let mut release_file_reader = b"abcd".as_slice();
    let mut copied_bytes = Vec::new();

    let copy_error = copy_stream_with_byte_limit(&mut release_file_reader, &mut copied_bytes, 3)
        .expect_err("a stream over the limit must fail");

    assert_eq!(copy_error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(copy_error.to_string(), "download response exceeds 3 bytes");
    assert_eq!(copied_bytes, b"abc");
}

fn write_release_file(release_file_bytes: &[u8]) -> TempPath {
    let mut release_file = Builder::new()
        .prefix("koshi-test-")
        .tempfile()
        .expect("release tempfile");
    release_file
        .as_file_mut()
        .write_all(release_file_bytes)
        .expect("write release file");
    release_file.into_temp_path()
}

#[test]
fn matching_release_archive_checksum_is_accepted() {
    let archive_path = write_release_file(b"abc");

    verify_release_archive(
        archive_path.as_ref(),
        "koshi-v0.5.0-linux-amd64.tar.gz",
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    )
    .expect("the matching checksum is accepted");
}

#[test]
fn mismatched_release_archive_checksum_names_both_digests() {
    let archive_path = write_release_file(b"abc");
    let expected_checksum = "0000000000000000000000000000000000000000000000000000000000000000";
    let extract_error = extract_verified_release_binary(
        archive_path.as_ref(),
        "koshi.tar.gz",
        "koshi-v0.5.0-linux-amd64.tar.gz",
        expected_checksum,
    )
    .expect_err("the changed checksum must fail before unpacking");

    assert_eq!(
        extract_error,
        "checksum mismatch for release archive koshi-v0.5.0-linux-amd64.tar.gz: expected 0000000000000000000000000000000000000000000000000000000000000000, computed ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn get_binary_file_name_is_platform_specific() {
    if cfg!(windows) {
        assert_eq!(get_binary_file_name(), "koshi.exe");
    } else {
        assert_eq!(get_binary_file_name(), "koshi");
    }
}

#[cfg(windows)]
#[test]
fn a_windows_swap_replaces_the_executable_and_cleans_the_backup() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("swap directory");
    let executable_path = test_directory.path().join("koshi.exe");
    let new_binary_path = test_directory.path().join("new-binary.exe");
    let staged_binary_path = test_directory
        .path()
        .join(format!("koshi-update-{}.exe", std::process::id()));
    let backup_executable_path = executable_path.with_extension("old");
    fs::write(&executable_path, b"old-binary").expect("write the old executable");
    fs::write(&new_binary_path, b"new-binary").expect("write the replacement executable");

    swap_executable(&new_binary_path, &executable_path).expect("replace the executable");

    assert_eq!(
        fs::read(&executable_path).expect("read the replacement executable"),
        b"new-binary"
    );
    assert!(!backup_executable_path.exists());
    assert!(!staged_binary_path.exists());
}

#[test]
fn update_state_defaults_when_deserialized_from_empty_object() {
    let update_state: UpdateState =
        serde_json::from_str("{}").expect("empty object is valid update state");
    assert_eq!(update_state.last_check_unix_seconds, None);
}

#[test]
fn update_state_survives_a_serialize_deserialize_round_trip() {
    let original_update_state = UpdateState {
        last_check_unix_seconds: Some(1_700_000_000),
    };
    let serialized_update_state =
        serde_json::to_string(&original_update_state).expect("serializable");
    let restored_update_state: UpdateState =
        serde_json::from_str(&serialized_update_state).expect("deserializable");
    assert_eq!(
        restored_update_state.last_check_unix_seconds,
        original_update_state.last_check_unix_seconds
    );
}

// --- release JSON parsing (no network: fixture strings only) ---

#[test]
fn a_release_object_deserializes_its_tag_name() {
    let release: Release = serde_json::from_str(r#"{"tag_name":"v0.2.0","name":"ignored"}"#)
        .expect("a release object with extra fields still parses");
    assert_eq!(release.tag_name, "v0.2.0");
}

#[test]
fn a_release_list_deserializes_every_tag_in_order() {
    let releases: Vec<Release> =
        serde_json::from_str(r#"[{"tag_name":"v0.2.0"},{"tag_name":"v0.1.0"}]"#)
            .expect("a release array parses");
    let release_tags: Vec<String> = releases
        .into_iter()
        .map(|release| release.tag_name)
        .collect();
    assert_eq!(
        release_tags,
        vec!["v0.2.0".to_string(), "v0.1.0".to_string()]
    );
}

// --- update error + current Unix time ---

#[test]
fn update_error_wraps_detail_in_cli_update_error() {
    match build_update_error("boom") {
        CliError::Update { detail } => assert_eq!(detail, "boom"),
        unexpected_error => panic!("expected CliError::Update, got {unexpected_error:?}"),
    }
}

#[test]
fn get_current_unix_seconds_is_after_the_year_2023() {
    // A whole-second Unix timestamp taken now is always past 2023-11-14.
    assert!(get_current_unix_seconds() > 1_700_000_000);
}

// --- archive extraction (local files, no network) ---

/// Writes a gzip-compressed tar to a temp file, one regular-file entry per
/// `(name, bytes)`.
fn write_tar_gz(archive_entries: &[(&str, &[u8])]) -> TempPath {
    let archive_file = Builder::new()
        .prefix("koshi-test-")
        .suffix(".tar.gz")
        .tempfile()
        .expect("archive tempfile");
    {
        let gzip_encoder =
            flate2::write::GzEncoder::new(archive_file.as_file(), flate2::Compression::default());
        let mut tar_archive = tar::Builder::new(gzip_encoder);
        for (archive_entry_name, archive_entry_bytes) in archive_entries {
            let mut archive_header = tar::Header::new_gnu();
            archive_header
                .set_path(archive_entry_name)
                .expect("archive entry path");
            archive_header.set_size(archive_entry_bytes.len() as u64);
            archive_header.set_mode(0o755);
            archive_header.set_cksum();
            tar_archive
                .append(&archive_header, *archive_entry_bytes)
                .expect("append archive entry");
        }
        tar_archive
            .into_inner()
            .expect("finish tar archive")
            .finish()
            .expect("finish gzip archive");
    }
    archive_file.into_temp_path()
}

/// Writes a zip archive to a temp file, one entry per `(name, bytes)`.
fn write_zip(archive_entries: &[(&str, &[u8])]) -> TempPath {
    let archive_file = Builder::new()
        .prefix("koshi-test-")
        .suffix(".zip")
        .tempfile()
        .expect("archive tempfile");
    {
        let mut zip_archive = zip::ZipWriter::new(archive_file.as_file());
        let zip_entry_options = zip::write::SimpleFileOptions::default();
        for (archive_entry_name, archive_entry_bytes) in archive_entries {
            zip_archive
                .start_file(*archive_entry_name, zip_entry_options)
                .expect("start archive entry");
            zip_archive
                .write_all(archive_entry_bytes)
                .expect("write archive entry");
        }
        zip_archive.finish().expect("finish zip archive");
    }
    archive_file.into_temp_path()
}

#[test]
fn extracting_a_tar_gz_returns_the_named_binary_bytes() {
    let release_archive_path = write_tar_gz(&[
        ("readme.txt", b"docs"),
        (get_binary_file_name(), b"binary-bytes"),
    ]);
    let extracted_binary_path =
        extract_release_binary(release_archive_path.as_ref(), "koshi.tar.gz")
            .expect("extract the binary");
    assert_eq!(
        fs::read(AsRef::<Path>::as_ref(&extracted_binary_path)).expect("read extracted binary"),
        b"binary-bytes"
    );
}

#[test]
fn extracting_a_tar_gz_without_the_binary_is_an_error() {
    let release_archive_path = write_tar_gz(&[("readme.txt", b"docs")]);
    assert_eq!(
        extract_release_binary(release_archive_path.as_ref(), "koshi.tar.gz")
            .expect_err("no binary present"),
        "binary not found in archive"
    );
}

#[test]
fn extracting_a_zip_returns_the_named_binary_bytes() {
    let release_archive_path = write_zip(&[
        ("readme.txt", b"docs"),
        (get_binary_file_name(), b"binary-bytes"),
    ]);
    let extracted_binary_path = extract_release_binary(release_archive_path.as_ref(), "koshi.zip")
        .expect("extract the binary");
    assert_eq!(
        fs::read(AsRef::<Path>::as_ref(&extracted_binary_path)).expect("read extracted binary"),
        b"binary-bytes"
    );
}

#[test]
fn extracting_a_zip_without_the_binary_is_an_error() {
    let release_archive_path = write_zip(&[("readme.txt", b"docs")]);
    assert_eq!(
        extract_release_binary(release_archive_path.as_ref(), "koshi.zip")
            .expect_err("no binary present"),
        "binary not found in archive"
    );
}

/// Writes a gzip-compressed tar to a temp file, one entry per
/// `(name, entry type, bytes)`.
fn write_tar_gz_of_kinds(archive_entries: &[(&str, tar::EntryType, &[u8])]) -> TempPath {
    let archive_file = Builder::new()
        .prefix("koshi-test-")
        .suffix(".tar.gz")
        .tempfile()
        .expect("archive tempfile");
    {
        let gzip_encoder =
            flate2::write::GzEncoder::new(archive_file.as_file(), flate2::Compression::default());
        let mut tar_archive = tar::Builder::new(gzip_encoder);
        for (archive_entry_name, archive_entry_type, archive_entry_bytes) in archive_entries {
            let mut archive_header = tar::Header::new_gnu();
            archive_header
                .set_path(archive_entry_name)
                .expect("archive entry path");
            archive_header.set_entry_type(*archive_entry_type);
            archive_header.set_size(archive_entry_bytes.len() as u64);
            archive_header.set_mode(0o755);
            archive_header.set_cksum();
            tar_archive
                .append(&archive_header, *archive_entry_bytes)
                .expect("append archive entry");
        }
        tar_archive
            .into_inner()
            .expect("finish tar archive")
            .finish()
            .expect("finish gzip archive");
    }
    archive_file.into_temp_path()
}

#[test]
fn a_directory_carrying_the_binary_name_is_passed_over_for_the_real_file() {
    let release_archive_path = write_tar_gz_of_kinds(&[
        (get_binary_file_name(), tar::EntryType::Directory, b""),
        (
            get_binary_file_name(),
            tar::EntryType::Regular,
            b"binary-bytes",
        ),
    ]);

    let extracted_binary_path =
        extract_release_binary(release_archive_path.as_ref(), "koshi.tar.gz")
            .expect("extract the binary");

    assert_eq!(
        fs::read(AsRef::<Path>::as_ref(&extracted_binary_path)).expect("read extracted binary"),
        b"binary-bytes"
    );
}

#[test]
fn a_symbolic_link_carrying_the_binary_name_is_passed_over_for_the_real_file() {
    let release_archive_path = write_tar_gz_of_kinds(&[
        (get_binary_file_name(), tar::EntryType::Symlink, b""),
        (
            get_binary_file_name(),
            tar::EntryType::Regular,
            b"binary-bytes",
        ),
    ]);

    let extracted_binary_path =
        extract_release_binary(release_archive_path.as_ref(), "koshi.tar.gz")
            .expect("extract the binary");

    assert_eq!(
        fs::read(AsRef::<Path>::as_ref(&extracted_binary_path)).expect("read extracted binary"),
        b"binary-bytes"
    );
}

#[test]
fn a_tar_gz_binary_under_a_top_level_directory_is_found_by_its_file_name() {
    let nested_archive_path = format!("koshi-v9.9.9-linux-amd64/{}", get_binary_file_name());
    let release_archive_path = write_tar_gz(&[(nested_archive_path.as_str(), b"nested-bytes")]);

    let extracted_binary_path =
        extract_release_binary(release_archive_path.as_ref(), "koshi.tar.gz")
            .expect("extract the binary");

    assert_eq!(
        fs::read(AsRef::<Path>::as_ref(&extracted_binary_path)).expect("read extracted binary"),
        b"nested-bytes"
    );
}

#[test]
fn a_zip_binary_under_a_top_level_directory_is_found_by_its_file_name() {
    let nested_archive_path = format!("koshi-v9.9.9-windows-amd64/{}", get_binary_file_name());
    let release_archive_path = write_zip(&[(nested_archive_path.as_str(), b"nested-bytes")]);

    let extracted_binary_path = extract_release_binary(release_archive_path.as_ref(), "koshi.zip")
        .expect("extract the binary");

    assert_eq!(
        fs::read(AsRef::<Path>::as_ref(&extracted_binary_path)).expect("read extracted binary"),
        b"nested-bytes"
    );
}

#[cfg(unix)]
#[test]
fn an_extracted_binary_is_left_runnable() {
    use std::os::unix::fs::PermissionsExt;

    let release_archive_path = write_tar_gz_of_kinds(&[(
        get_binary_file_name(),
        tar::EntryType::Regular,
        b"binary-bytes" as &[u8],
    )]);

    let extracted_binary_path =
        extract_release_binary(release_archive_path.as_ref(), "koshi.tar.gz")
            .expect("extract the binary");

    let permission_mode = fs::metadata(AsRef::<Path>::as_ref(&extracted_binary_path))
        .expect("read the extracted binary's metadata")
        .permissions()
        .mode();
    assert_eq!(permission_mode & 0o777, 0o755);
}

/// The pre-release picker takes the highest version by semver order, never
/// the newest by publish date: a re-published older-versioned tag loses to a
/// higher one wherever it sits in the list.
#[test]
fn highest_release_version_picks_semver_order_not_list_order() {
    let releases = |release_tags: &[&str]| -> Vec<Release> {
        release_tags
            .iter()
            .map(|release_tag| Release {
                tag_name: (*release_tag).to_string(),
            })
            .collect()
    };

    assert_eq!(
        find_highest_release_version(releases(&["v0.3.0-rc.2", "v0.3.0-rc.10", "v0.2.0",]))
            .unwrap(),
        "v0.3.0-rc.10"
    );
    // List order plays no part: the highest wins from the front too.
    assert_eq!(
        find_highest_release_version(releases(&["v0.4.0", "v0.3.0"])).unwrap(),
        "v0.4.0"
    );
    // A tag that is not a version is skipped, not an error.
    assert_eq!(
        find_highest_release_version(releases(&["nightly", "v0.1.0"])).unwrap(),
        "v0.1.0"
    );
    assert_eq!(
        find_highest_release_version(Vec::new()).unwrap_err(),
        "no releases found"
    );
    // A list where no tag is a version reads the same as an empty one.
    assert_eq!(
        find_highest_release_version(releases(&["nightly", "edge"])).unwrap_err(),
        "no releases found"
    );
}

#[cfg(unix)]
#[test]
fn a_runtime_directory_that_cannot_be_read_restarts_no_session_and_names_it() {
    use std::os::unix::fs::PermissionsExt;

    let runtime_directory = build_test_runtime_directory();
    std::fs::set_permissions(
        runtime_directory.path(),
        std::fs::Permissions::from_mode(0o300),
    )
    .expect("make the runtime directory unlistable");
    let Err(runtime_read_error) = std::fs::read_dir(runtime_directory.path()) else {
        eprintln!(
            "skipped `a_runtime_directory_that_cannot_be_read_restarts_no_session_and_names_it`: \
             this user lists a mode-300 directory"
        );
        let _ = std::fs::set_permissions(
            runtime_directory.path(),
            std::fs::Permissions::from_mode(0o700),
        );
        return;
    };

    let session_outcomes = restart_advertised_sessions(
        runtime_directory.path(),
        "3.3.3",
        RestartScope::EveryServer,
        Duration::from_secs(5),
    );

    let _ = std::fs::set_permissions(
        runtime_directory.path(),
        std::fs::Permissions::from_mode(0o700),
    );
    assert_eq!(
        session_outcomes,
        Err(UnreadPath::from_read_error(
            runtime_directory.path(),
            &runtime_read_error
        ))
    );
}

#[test]
fn the_update_lock_is_held_until_its_file_is_dropped() {
    let runtime_directory = build_test_runtime_directory();
    let update_lock_file =
        take_update_lock(runtime_directory.path()).expect("the update lock is taken");

    assert!(is_update_restarting_servers(runtime_directory.path()));
    drop(update_lock_file);
    assert!(!is_update_restarting_servers(runtime_directory.path()));
}

#[test]
fn a_runtime_directory_that_does_not_exist_takes_no_update_lock() {
    let runtime_directory = build_test_runtime_directory();

    assert!(take_update_lock(&runtime_directory.path().join("absent")).is_none());
}

#[cfg(unix)]
#[test]
fn the_update_lock_file_is_readable_by_its_owner_alone() {
    use std::os::unix::fs::PermissionsExt;

    let runtime_directory = build_test_runtime_directory();
    let _update_lock_file =
        take_update_lock(runtime_directory.path()).expect("the update lock is taken");

    assert_eq!(
        fs::metadata(resolve_update_lock_path(runtime_directory.path()))
            .expect("the update lock file exists")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn a_pinned_homebrew_formula_is_not_upgraded_and_names_the_formula_to_move_to() {
    assert_eq!(
        update_release_install(&ReleaseInstall::Homebrew {
            brew_path: PathBuf::from("/opt/homebrew/bin/brew"),
            formula_name: "koshi@0.5.0".to_string(),
        }),
        Err(
            "this koshi comes from the Homebrew formula koshi@0.5.0, which stays on its version; \
             move to the newest koshi with: brew install gohyuhan/koshi/koshi"
                .to_string()
        )
    );
}

#[test]
fn a_brew_that_cannot_be_started_names_its_path_and_the_upgrade_command() {
    let (program_directory, _program_path, _other_program_path) = build_program_files();
    let missing_brew_path = program_directory.path().join("missing-brew");
    let spawn_error = std::process::Command::new(&missing_brew_path)
        .status()
        .expect_err("a missing program does not start");

    assert_eq!(
        upgrade_through_homebrew(&missing_brew_path, HOMEBREW_FORMULA_NAME),
        Err(format!(
            "{} could not be run: {spawn_error}; upgrade with: brew upgrade gohyuhan/koshi/koshi",
            missing_brew_path.display()
        ))
    );
}

/// Write a shell script named `brew` into `script_directory` that writes each
/// argument it gets, one per line, to `arguments_path`, then exits with
/// `exit_code`. Hands back its path.
#[cfg(unix)]
fn write_stand_in_brew(script_directory: &Path, arguments_path: &Path, exit_code: u8) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;

    let brew_path = script_directory.join("brew");
    fs::write(
        &brew_path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit {exit_code}\n",
            arguments_path.display()
        ),
    )
    .expect("write the stand-in brew");
    fs::set_permissions(&brew_path, fs::Permissions::from_mode(0o755))
        .expect("make the stand-in brew runnable");
    brew_path
}

/// What [`update_release_install`] gives for a Homebrew install of the formula
/// `koshi` whose `brew` is at `brew_path`. A run whose error reads `could not
/// be run` is tried again every [`BUSY_PROGRAM_RETRY_INTERVAL_DURATION`] for
/// up to [`BUSY_PROGRAM_WAIT_DURATION`]; Linux refuses to run a file that any
/// process holds open for writing.
#[cfg(unix)]
fn run_stand_in_brew_upgrade(brew_path: &Path) -> Result<(), String> {
    let homebrew_install = ReleaseInstall::Homebrew {
        brew_path: brew_path.to_path_buf(),
        formula_name: HOMEBREW_FORMULA_NAME.to_string(),
    };
    let busy_deadline = Instant::now() + BUSY_PROGRAM_WAIT_DURATION;
    loop {
        let upgrade_result = update_release_install(&homebrew_install);
        let is_brew_busy = upgrade_result
            .as_ref()
            .is_err_and(|upgrade_error| upgrade_error.contains("could not be run"));
        if !is_brew_busy || Instant::now() >= busy_deadline {
            return upgrade_result;
        }
        std::thread::sleep(BUSY_PROGRAM_RETRY_INTERVAL_DURATION);
    }
}

#[cfg(unix)]
#[test]
fn a_homebrew_upgrade_runs_brew_upgrade_on_the_tapped_formula() {
    let (script_directory, _program_path, _other_program_path) = build_program_files();
    let arguments_path = script_directory.path().join("brew-arguments");
    let brew_path = write_stand_in_brew(script_directory.path(), &arguments_path, 0);

    assert_eq!(run_stand_in_brew_upgrade(&brew_path), Ok(()));
    assert_eq!(
        fs::read_to_string(&arguments_path).expect("the stand-in brew ran"),
        "upgrade\ngohyuhan/koshi/koshi\n"
    );
}

#[cfg(unix)]
#[test]
fn a_failing_homebrew_upgrade_names_its_exit_status() {
    let (script_directory, _program_path, _other_program_path) = build_program_files();
    let arguments_path = script_directory.path().join("brew-arguments");
    let brew_path = write_stand_in_brew(script_directory.path(), &arguments_path, 3);

    assert_eq!(
        run_stand_in_brew_upgrade(&brew_path),
        Err("brew upgrade gohyuhan/koshi/koshi failed: exit status: 3".to_string())
    );
}

#[cfg(unix)]
#[test]
fn a_swap_through_a_symbolic_link_replaces_the_file_it_names_and_keeps_the_link() {
    let (program_directory, program_path, _other_program_path) = build_program_files();
    let link_path = program_directory.path().join("koshi-link");
    std::os::unix::fs::symlink(&program_path, &link_path).expect("link the program file");
    let new_binary_path = program_directory.path().join("new-binary");
    fs::write(&new_binary_path, b"new-binary").expect("write the replacement executable");

    swap_executable(&new_binary_path, &link_path).expect("replace the executable");

    assert_eq!(
        fs::read_link(&link_path).expect("the link is still a link"),
        program_path
    );
    assert_eq!(
        fs::read(&program_path).expect("read the replaced file"),
        b"new-binary"
    );
}
