//! Tests for the self-update helpers:
//!
//! - version comparison, check scheduling, and release list parsing;
//! - archive URL construction, bounded downloads, checksum verification, and
//!   archive extraction;
//! - state serialization;
//! - the restart confirmation wait, the bounded call to a peer, the router
//!   restart, the walk that restarts every running session, a peer that
//!   refuses this build's protocol version, and the end of a router that
//!   refuses it;
//! - the update lock held while servers restart;
//! - the Homebrew upgrade;
//! - the version check of a new binary, the program file swap, its backup
//!   names, a swap whose copy or version check fails, the staged copy names,
//!   and the deletion of the staged copies that ended updates left;
//! - the scripts that write and rename the staged copy through `sudo`, and the
//!   install lines of `install.sh`: their copy, mode change, version check,
//!   rename, and cleanup;
//! - the install lock held while the staged copy replaces the program file and
//!   while backups are deleted, the lock calls that `install.ps1` makes on the
//!   same lock file, and the staged copy and version check of `install.ps1`;
//! - the runtime directories of koshi 0.1.0 and 0.2.0: which ones are walked,
//!   how many sessions and koshi 0.1.0 windows run from one, and the lines
//!   `list-sessions` prints about them;
//! - the binary file name, the update error, and the clock reading.

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
use koshi_test_support::fixtures::{
    build_test_runtime_directory, count_program_runs, spawn_previous_release_session,
    write_koshi_0_1_0_window_endpoint_file, write_printing_program, write_session_endpoint_file,
    KOSHI_0_2_0_HELLO_ANSWER_TEXT, KOSHI_0_2_0_RESTART_REFUSAL_TEXT, KOSHI_0_4_0_HELLO_ANSWER_TEXT,
    KOSHI_0_4_0_RESTARTING_ANSWER_TEXT, PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT,
};
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

    let Err(CliError::ProtocolVersionRefused {
        detail: refusal_detail,
    }) = router_restart
    else {
        panic!("expected ProtocolVersionRefused, got {router_restart:?}");
    };
    assert_eq!(refusal_detail, VERSION_REFUSAL_SENTENCE);
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

    assert_eq!(
        stop_incompatible_router(runtime_directory.path(), "3.3.3", VERSION_REFUSAL_SENTENCE),
        IncompatibleRouterStop::Stopped {
            router_process_id: router_child.id()
        }
    );

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

    assert_eq!(
        stop_incompatible_router(runtime_directory.path(), "3.3.3", VERSION_REFUSAL_SENTENCE),
        IncompatibleRouterStop::NotStopped
    );

    assert!(process_tree::is_process_running(&router_record));
    assert!(member_records.iter().all(process_tree::is_process_running));
    end_stand_in_session(router_child, &member_records);
}

/// Accept two callers on `listener`. The first one's Hello is answered with
/// `first_hello_answer_text`, as a router this build cannot talk to answers
/// it, and the request it writes after its Hello is read. The second one's
/// Hello is answered as a router on `3.3.3` answers it, as the router started
/// once the first one ended.
fn spawn_router_replaced_after_its_first_hello(
    listener: Listener,
    first_hello_answer_text: String,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut first_connection = listener.accept().expect("accept the first caller");
        let _hello_request: Box<serde_json::value::RawValue> =
            first_connection.recv().expect("read the hello");
        first_connection
            .send(
                &serde_json::value::RawValue::from_string(first_hello_answer_text)
                    .expect("the answer is JSON"),
            )
            .expect("send the hello answer");
        let _next_request: Result<Box<serde_json::value::RawValue>, _> = first_connection.recv();
        let mut second_connection = listener.accept().expect("accept the second caller");
        let hello_request: RouterRequest = second_connection.recv().expect("read the hello");
        send_router_response(
            &mut second_connection,
            hello_request.request_id,
            RouterResult::Hello {
                protocol_version: ROUTER_PROTOCOL_VERSION,
                build_version: "3.3.3".to_string(),
            },
        );
    })
}

/// Serve the router of `runtime_directory` from a stand-in koshi process that
/// answers its first Hello with `first_hello_answer_text`, run
/// [`restart_router_into_version`] for `3.3.3`, and check that the stand-in
/// process ended, that nothing under it did, and that a router on `3.3.3`
/// answered after it. Hands back what [`restart_router_into_version`] gave.
fn replace_stand_in_router(first_hello_answer_text: String) -> bool {
    let runtime_directory = build_test_runtime_directory();
    let (mut router_child, router_record, member_records) =
        start_stand_in_session(runtime_directory.path(), format_stand_in_file_name(true));
    let router_listener = Listener::bind(&compute_router_socket_address(runtime_directory.path()))
        .expect("bind the stand-in router");
    write_router_endpoint_naming_process(runtime_directory.path(), router_child.id());
    let router_thread =
        spawn_router_replaced_after_its_first_hello(router_listener, first_hello_answer_text);

    let has_router_restarted =
        restart_router_into_version(runtime_directory.path(), "3.3.3", RestartScope::EveryServer);

    router_thread
        .join()
        .expect("the stand-in served both connections");
    assert!(process_tree::wait_for_processes_to_end(
        std::slice::from_ref(&router_record),
        Duration::from_secs(5)
    ));
    router_child.wait().expect("the router process is reaped");
    assert!(member_records.iter().all(process_tree::is_process_running));
    process_tree::stop_processes(&member_records, Duration::ZERO);
    has_router_restarted
}

#[test]
fn restart_router_into_version_replaces_a_confirmed_router_that_refuses_this_builds_protocol_version(
) {
    let version_refusal_text = serde_json::to_string(&RouterResponse {
        request_id: Some(1),
        answer_result: RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: VERSION_REFUSAL_SENTENCE.to_string(),
        }),
    })
    .expect("the refusal serializes");

    assert!(replace_stand_in_router(version_refusal_text));
}

#[test]
fn restart_router_into_version_replaces_a_confirmed_router_of_koshi_0_4_0() {
    assert!(replace_stand_in_router(
        PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT.to_string()
    ));
}

#[test]
fn a_started_router_on_the_expected_version_confirms_the_replacement() {
    assert!(report_started_router(
        5000,
        "3.3.3",
        Ok("3.3.3".to_string())
    ));
}

#[test]
fn a_started_router_on_another_version_fails_the_replacement() {
    assert!(!report_started_router(
        5000,
        "3.3.3",
        Ok("3.3.2".to_string())
    ));
}

#[test]
fn a_router_that_did_not_start_fails_the_replacement() {
    assert!(!report_started_router(
        5000,
        "3.3.3",
        Err(CliError::IpcUnavailable {
            detail: "the router did not start".to_string(),
        })
    ));
}

#[test]
fn stop_incompatible_router_ends_nothing_without_a_router_endpoint_file() {
    let runtime_directory = build_test_runtime_directory();

    assert_eq!(
        stop_incompatible_router(runtime_directory.path(), "3.3.3", VERSION_REFUSAL_SENTENCE),
        IncompatibleRouterStop::NotStopped
    );
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

    assert_eq!(
        stop_incompatible_router(
            runtime_directory.path(),
            "9999.0.0",
            VERSION_REFUSAL_SENTENCE
        ),
        IncompatibleRouterStop::AlreadyOnVersion
    );

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

    assert_eq!(
        stop_incompatible_router(
            runtime_directory.path(),
            "9999.0.0",
            VERSION_REFUSAL_SENTENCE
        ),
        IncompatibleRouterStop::NotStopped
    );

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

    assert_eq!(
        stop_incompatible_router(
            runtime_directory.path(),
            "9999.0.0",
            VERSION_REFUSAL_SENTENCE
        ),
        IncompatibleRouterStop::Stopped {
            router_process_id: router_child.id()
        }
    );

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

/// The Hello answer of this build's envelope from a session on
/// `build_version`, as its JSON text.
fn format_current_session_hello_answer_text(build_version: &str) -> String {
    serde_json::to_string(&IpcResponse {
        request_id: Some(1),
        answer_result: IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            build_version: build_version.to_string(),
        },
    })
    .expect("the answer serializes")
}

/// The answer texts of the stand-in koshi 0.2.0 to 0.4.0 session that refuses
/// this build's exchange, then answers the exchange of its own envelope with
/// `previous_release_answer_texts`, then answers one connection per entry of
/// `later_answer_texts_by_connection`.
fn build_previous_release_session_script(
    previous_release_answer_texts: [&str; 2],
    later_answer_texts_by_connection: Vec<Vec<String>>,
) -> Vec<Vec<String>> {
    let mut answer_texts_by_connection = vec![
        vec![
            PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT.to_string(),
            PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT.to_string(),
        ],
        previous_release_answer_texts
            .into_iter()
            .map(str::to_string)
            .collect(),
    ];
    answer_texts_by_connection.extend(later_answer_texts_by_connection);
    answer_texts_by_connection
}

#[test]
fn a_session_of_koshi_0_4_0_restarted_in_its_own_envelope_confirms_on_the_installed_version() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let session_thread = spawn_previous_release_session(
        runtime_directory.path(),
        session_id,
        "k7QxSecret",
        build_previous_release_session_script(
            [
                KOSHI_0_4_0_HELLO_ANSWER_TEXT,
                KOSHI_0_4_0_RESTARTING_ANSWER_TEXT,
            ],
            vec![vec![format_current_session_hello_answer_text("3.3.3")]],
        ),
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
fn a_session_of_koshi_0_4_0_whose_restart_did_not_start_still_reports_0_4_0_after_the_wait() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let session_thread = spawn_previous_release_session(
        runtime_directory.path(),
        session_id,
        "k7QxSecret",
        build_previous_release_session_script(
            [
                KOSHI_0_4_0_HELLO_ANSWER_TEXT,
                KOSHI_0_4_0_RESTARTING_ANSWER_TEXT,
            ],
            vec![
                vec![PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT.to_string()],
                vec![KOSHI_0_4_0_HELLO_ANSWER_TEXT.to_string()],
            ],
        ),
    );

    let session_outcomes = restart_advertised_sessions(
        runtime_directory.path(),
        "3.3.3",
        RestartScope::EveryServer,
        Duration::from_secs(1),
    )
    .expect("read the runtime directory");

    assert_eq!(
        session_outcomes,
        vec![(
            session_id,
            SessionOutcome::StillOnVersion("0.4.0".to_string())
        )]
    );
    session_thread
        .join()
        .expect("the stand-in served its connections");
}

#[test]
fn a_session_of_koshi_0_2_0_is_reported_without_a_restart_request() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let session_thread = spawn_previous_release_session(
        runtime_directory.path(),
        session_id,
        "k7QxSecret",
        build_previous_release_session_script(
            [
                KOSHI_0_2_0_HELLO_ANSWER_TEXT,
                KOSHI_0_2_0_RESTART_REFUSAL_TEXT,
            ],
            Vec::new(),
        ),
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
        vec![(session_id, SessionOutcome::WithoutRestartRequest)]
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

#[test]
fn a_new_binary_that_prints_the_expected_version_passes_the_version_check() {
    let program_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let run_log_path = program_directory.path().join("new-binary-runs");
    let new_binary_path = write_printing_program(
        program_directory.path(),
        "new-binary",
        &run_log_path,
        "koshi 9.9.9",
    );

    assert_eq!(validate_release_binary(&new_binary_path, "v9.9.9"), Ok(()));
    assert_eq!(count_program_runs(&run_log_path), 1);
}

#[test]
fn a_new_binary_that_prints_another_version_fails_the_version_check() {
    let program_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let new_binary_path = write_printing_program(
        program_directory.path(),
        "new-binary",
        &program_directory.path().join("new-binary-runs"),
        "koshi 9.9.8",
    );

    assert_eq!(
        validate_release_binary(&new_binary_path, "v9.9.9"),
        Err("the new koshi prints version 9.9.8, not 9.9.9".to_string())
    );
}

#[test]
fn a_new_binary_that_prints_no_version_fails_the_version_check_with_its_line() {
    let program_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let new_binary_path = write_printing_program(
        program_directory.path(),
        "new-binary",
        &program_directory.path().join("new-binary-runs"),
        "unknown option --version",
    );

    assert_eq!(
        validate_release_binary(&new_binary_path, "v9.9.9"),
        Err(format!(
            "the new koshi 9.9.9 does not run on this system: the binary at {} printed \
             \"unknown option --version\" for --version",
            new_binary_path.display()
        ))
    );
}

#[cfg(unix)]
#[test]
fn a_swap_whose_new_binary_prints_another_version_keeps_the_program_file_and_leaves_no_copy() {
    let program_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let program_path = program_directory.path().join("koshi");
    let run_log_path = program_directory.path().join("new-binary-runs");
    let new_binary_path = write_printing_program(
        program_directory.path(),
        "new-binary",
        &run_log_path,
        "koshi 9.9.8",
    );
    fs::write(&program_path, b"old-binary").expect("write the old executable");

    let swap_result = swap_executable(&new_binary_path, &program_path, "v9.9.9");

    assert_eq!(
        swap_result,
        Err("the new koshi prints version 9.9.8, not 9.9.9".to_string())
    );
    assert_eq!(
        fs::read(&program_path).expect("read the program file"),
        b"old-binary"
    );
    assert_eq!(count_program_runs(&run_log_path), 1);
    assert_eq!(
        list_sorted_entry_names(program_directory.path()),
        ["koshi", "new-binary", "new-binary-runs"]
    );
}

#[cfg(windows)]
#[test]
fn a_windows_swap_whose_staged_copy_cannot_run_keeps_the_program_file_and_takes_no_install_lock() {
    let program_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let program_path = program_directory.path().join("koshi.exe");
    fs::write(&program_path, b"old-binary").expect("write the old executable");
    let new_binary_path = program_directory.path().join("new-binary");
    fs::write(&new_binary_path, b"new-binary").expect("write the replacement bytes");
    fs::write(
        program_directory
            .path()
            .join(format!("koshi-staged-{FREE_PROCESS_ID}.exe")),
        b"stray-binary",
    )
    .expect("write the stray staged copy");
    let probe_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("probe directory");
    let probe_path = probe_directory.path().join("probe.exe");
    fs::write(&probe_path, b"new-binary").expect("write the probe");
    let spawn_error = std::process::Command::new(&probe_path)
        .arg("--version")
        .spawn()
        .expect_err("bytes that are no program do not start");
    let staged_binary_path = fs::canonicalize(&program_path)
        .expect("canonicalize the program file")
        .with_file_name(format!("koshi-staged-{}.exe", std::process::id()));

    let swap_result = swap_executable(&new_binary_path, &program_path, "v9.9.9");

    assert_eq!(
        swap_result,
        Err(format!(
            "the new koshi 9.9.9 does not run on this system: the binary at {} could not be run: \
             {spawn_error}",
            staged_binary_path.display()
        ))
    );
    assert_eq!(
        fs::read(&program_path).expect("read the program file"),
        b"old-binary"
    );
    assert_eq!(
        list_sorted_entry_names(program_directory.path()),
        ["koshi.exe", "new-binary"]
    );
}

#[test]
fn replacing_the_program_file_with_the_staged_copy_leaves_its_bytes_and_no_backup() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let executable_path = test_directory.path().join("koshi.exe");
    let staged_binary_path = test_directory.path().join("koshi-staged-5000.exe");
    fs::write(&executable_path, b"old-binary").expect("write the old executable");
    fs::write(&staged_binary_path, b"new-binary").expect("write the staged copy");

    let replace_result =
        replace_program_file_with_staged_copy(&staged_binary_path, &executable_path);

    assert_eq!(replace_result, Ok(()));
    assert_eq!(
        fs::read(&executable_path).expect("read the replaced executable"),
        b"new-binary"
    );
    assert_eq!(
        list_sorted_entry_names(test_directory.path()),
        ["koshi.exe", "koshi.lock"]
    );
}

#[cfg(windows)]
#[test]
fn a_windows_replacement_takes_the_next_backup_name_while_a_process_runs_from_the_first() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let executable_path = test_directory.path().join("koshi.exe");
    let first_backup_path = test_directory.path().join("koshi.old");
    let (stand_in_child, _, member_records) =
        start_stand_in_session(test_directory.path(), "koshi.exe");
    fs::rename(&executable_path, &first_backup_path)
        .expect("rename the running executable to the first backup name");
    fs::write(&executable_path, b"installed-binary").expect("write the installed executable");
    let staged_binary_path = test_directory.path().join("koshi-staged-5000.exe");
    fs::write(&staged_binary_path, b"new-binary").expect("write the staged copy");

    let replace_result =
        replace_program_file_with_staged_copy(&staged_binary_path, &executable_path);
    let is_first_backup_left = first_backup_path.exists();
    end_stand_in_session(stand_in_child, &member_records);

    assert_eq!(replace_result, Ok(()));
    assert_eq!(
        fs::read(&executable_path).expect("read the replaced executable"),
        b"new-binary"
    );
    assert!(is_first_backup_left);
    assert!(!test_directory.path().join("koshi.1.old").exists());
}

#[test]
fn replacing_the_program_file_waits_while_another_install_holds_the_install_lock() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let executable_path = test_directory.path().join("koshi.exe");
    let staged_binary_path = test_directory.path().join("koshi-staged-5000.exe");
    fs::write(&executable_path, b"old-binary").expect("write the old executable");
    fs::write(&staged_binary_path, b"new-binary").expect("write the staged copy");
    let holding_lock_file =
        take_install_lock(&executable_path).expect("another install holds the install lock");
    let (replace_sender, replace_receiver) = mpsc::channel();
    let replace_thread = std::thread::spawn({
        let executable_path = executable_path.clone();
        let staged_binary_path = staged_binary_path.clone();
        move || {
            let _ = replace_sender.send(replace_program_file_with_staged_copy(
                &staged_binary_path,
                &executable_path,
            ));
        }
    });

    let answer_while_held = replace_receiver.recv_timeout(Duration::from_millis(200));
    let executable_bytes_while_held = fs::read(&executable_path).expect("read the old executable");
    drop(holding_lock_file);
    let answer_once_free = replace_receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the replacement ends once the install lock is free");
    replace_thread.join().expect("the replacement thread ends");

    assert_eq!(answer_while_held, Err(mpsc::RecvTimeoutError::Timeout));
    assert_eq!(executable_bytes_while_held, b"old-binary");
    assert_eq!(answer_once_free, Ok(()));
    assert_eq!(
        fs::read(&executable_path).expect("read the replaced executable"),
        b"new-binary"
    );
}

#[test]
fn replacing_the_program_file_with_a_missing_staged_copy_renames_the_backup_back() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let executable_path = test_directory.path().join("koshi.exe");
    let missing_staged_binary_path = test_directory.path().join("koshi-staged-5000.exe");
    fs::write(&executable_path, b"old-binary").expect("write the old executable");
    let rename_error = fs::rename(
        &missing_staged_binary_path,
        test_directory.path().join("probe"),
    )
    .expect_err("a missing file does not rename");

    let replace_result =
        replace_program_file_with_staged_copy(&missing_staged_binary_path, &executable_path);

    assert_eq!(replace_result, Err(rename_error.to_string()));
    assert_eq!(
        fs::read(&executable_path).expect("read the restored executable"),
        b"old-binary"
    );
    assert_eq!(
        list_sorted_entry_names(test_directory.path()),
        ["koshi.exe", "koshi.lock"]
    );
}

#[test]
fn replacing_the_program_file_changes_nothing_when_the_install_lock_cannot_be_taken() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let executable_path = test_directory.path().join("koshi.exe");
    let staged_binary_path = test_directory.path().join("koshi-staged-5000.exe");
    let install_lock_path = test_directory.path().join("koshi.lock");
    fs::write(&executable_path, b"old-binary").expect("write the old executable");
    fs::write(&staged_binary_path, b"new-binary").expect("write the staged copy");
    fs::create_dir(&install_lock_path).expect("a directory at koshi.lock");
    let open_error =
        open_lock_file(&install_lock_path).expect_err("a directory does not open as the lock file");

    let replace_result =
        replace_program_file_with_staged_copy(&staged_binary_path, &executable_path);

    assert_eq!(
        replace_result,
        Err(format!(
            "the install lock {} could not be taken: {open_error}",
            install_lock_path.display()
        ))
    );
    assert_eq!(
        fs::read(&executable_path).expect("read the program file"),
        b"old-binary"
    );
    assert_eq!(
        fs::read(&staged_binary_path).expect("read the staged copy"),
        b"new-binary"
    );
}

#[test]
fn the_first_backup_name_is_the_program_file_with_the_old_extension() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("backup directory");

    assert_eq!(
        prepare_backup_executable_path(&test_directory.path().join("koshi.exe"))
            .expect("a backup path"),
        test_directory.path().join("koshi.old")
    );
}

#[test]
fn a_backup_file_that_can_be_removed_is_removed_and_its_name_taken() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("backup directory");
    let first_backup_path = test_directory.path().join("koshi.old");
    fs::write(&first_backup_path, b"old-binary").expect("write the first backup");

    assert_eq!(
        prepare_backup_executable_path(&test_directory.path().join("koshi.exe"))
            .expect("a backup path"),
        first_backup_path
    );
    assert!(!first_backup_path.exists());
}

#[test]
fn a_backup_name_whose_entry_cannot_be_removed_passes_to_the_next_number() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("backup directory");
    fs::create_dir(test_directory.path().join("koshi.old")).expect("a directory at koshi.old");
    fs::create_dir(test_directory.path().join("koshi.1.old")).expect("a directory at koshi.1.old");

    assert_eq!(
        prepare_backup_executable_path(&test_directory.path().join("koshi.exe"))
            .expect("a backup path"),
        test_directory.path().join("koshi.2.old")
    );
    assert!(test_directory.path().join("koshi.old").is_dir());
    assert!(test_directory.path().join("koshi.1.old").is_dir());
}

#[test]
fn the_backup_list_names_only_the_backups_of_the_program_file() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("backup directory");
    for entry_name in [
        "koshi.exe",
        "koshi.old",
        "koshi.1.old",
        "koshi.12.old",
        "koshi.x.old",
        "koshi..old",
        "koshi.old.txt",
        "notes.old",
        "koshi-staged-5000.exe",
    ] {
        fs::write(test_directory.path().join(entry_name), b"").expect("write a directory entry");
    }

    let mut backup_executable_paths =
        list_backup_executable_paths(&test_directory.path().join("koshi.exe"));
    backup_executable_paths.sort();

    assert_eq!(
        backup_executable_paths,
        vec![
            test_directory.path().join("koshi.1.old"),
            test_directory.path().join("koshi.12.old"),
            test_directory.path().join("koshi.old"),
        ]
    );
}

/// The names of the entries of `directory`, sorted.
fn list_sorted_entry_names(directory: &Path) -> Vec<String> {
    let mut entry_names: Vec<String> = fs::read_dir(directory)
        .expect("read the test directory")
        .map(|directory_entry| {
            directory_entry
                .expect("read a directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    entry_names.sort();
    entry_names
}

#[test]
fn the_install_lock_of_koshi_exe_is_koshi_lock_beside_it_and_is_held_until_its_file_is_dropped() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let install_lock_file = take_install_lock(&test_directory.path().join("koshi.exe"))
        .expect("the install lock is taken");
    let other_lock_file = open_lock_file(&test_directory.path().join("koshi.lock"))
        .expect("open koshi.lock beside koshi.exe");

    assert!(matches!(
        other_lock_file.try_lock(),
        Err(fs::TryLockError::WouldBlock)
    ));
    drop(install_lock_file);
    assert!(matches!(other_lock_file.try_lock(), Ok(())));
}

#[test]
fn taking_the_install_lock_waits_until_its_holder_drops_it() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let executable_path = test_directory.path().join("koshi.exe");
    let holding_lock_file =
        take_install_lock(&executable_path).expect("the first holder takes the install lock");
    let (lock_sender, lock_receiver) = mpsc::channel();
    let waiting_thread = std::thread::spawn({
        let executable_path = executable_path.clone();
        move || {
            let _ = lock_sender.send(take_install_lock(&executable_path));
        }
    });

    let answer_while_held = lock_receiver.recv_timeout(Duration::from_millis(200));
    drop(holding_lock_file);
    let answer_once_free = lock_receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the waiting thread takes the install lock once it is free");
    waiting_thread.join().expect("the waiting thread ends");

    assert!(matches!(
        answer_while_held,
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert_eq!(answer_once_free.map(|_| ()), Ok(()));
}

#[test]
fn stale_backups_stay_while_the_install_lock_is_held_and_are_deleted_once_it_is_free() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let executable_path = test_directory.path().join("koshi.exe");
    for entry_name in ["koshi.exe", "koshi.old", "koshi.2.old", "notes.old"] {
        fs::write(test_directory.path().join(entry_name), b"").expect("write a directory entry");
    }
    let holding_lock_file =
        take_install_lock(&executable_path).expect("another install holds the install lock");

    delete_stale_backups_beside(&executable_path);
    let entry_names_while_held = list_sorted_entry_names(test_directory.path());
    drop(holding_lock_file);
    delete_stale_backups_beside(&executable_path);

    assert_eq!(
        entry_names_while_held,
        vec![
            "koshi.2.old",
            "koshi.exe",
            "koshi.lock",
            "koshi.old",
            "notes.old"
        ]
    );
    assert_eq!(
        list_sorted_entry_names(test_directory.path()),
        vec!["koshi.exe", "koshi.lock", "notes.old"]
    );
}

#[test]
fn deleting_stale_backups_when_there_is_none_creates_no_install_lock_file() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let executable_path = test_directory.path().join("koshi.exe");
    fs::write(&executable_path, b"").expect("write the program file");

    delete_stale_backups_beside(&executable_path);

    assert_eq!(
        list_sorted_entry_names(test_directory.path()),
        vec!["koshi.exe"]
    );
}

#[test]
fn taking_the_install_lock_fails_with_the_lock_path_when_the_lock_file_cannot_be_opened() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let install_lock_path = test_directory.path().join("koshi.lock");
    fs::create_dir(&install_lock_path).expect("a directory at koshi.lock");
    let open_error =
        open_lock_file(&install_lock_path).expect_err("a directory does not open as the lock file");

    assert_eq!(
        take_install_lock(&test_directory.path().join("koshi.exe")).map(|_| ()),
        Err(format!(
            "the install lock {} could not be taken: {open_error}",
            install_lock_path.display()
        ))
    );
}

#[test]
fn stale_backups_stay_when_the_install_lock_file_cannot_be_opened() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let executable_path = test_directory.path().join("koshi.exe");
    for entry_name in ["koshi.exe", "koshi.old"] {
        fs::write(test_directory.path().join(entry_name), b"").expect("write a directory entry");
    }
    fs::create_dir(test_directory.path().join("koshi.lock")).expect("a directory at koshi.lock");

    delete_stale_backups_beside(&executable_path);

    assert_eq!(
        list_sorted_entry_names(test_directory.path()),
        vec!["koshi.exe", "koshi.lock", "koshi.old"]
    );
}

/// The line of `install.ps1` that opens `koshi.lock`, whose path
/// `$install_lock_path` holds.
const INSTALL_SCRIPT_LOCK_FILE_OPEN_LINE: &str = "$install_lock = [System.IO.File]::Open($install_lock_path, [System.IO.FileMode]::OpenOrCreate, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::ReadWrite)";

/// The line of `install.ps1` that tries the install lock once.
const INSTALL_SCRIPT_LOCK_CALL_LINE: &str = "$install_lock.Lock(0, 1)";

/// The `HResult` that `install.ps1` reads as another process holding the
/// install lock: `0x80070021`, `ERROR_LOCK_VIOLATION`.
const INSTALL_SCRIPT_LOCK_VIOLATION_HRESULT: &str = "-2147024863";

#[test]
fn install_ps1_holds_each_lock_call_that_the_windows_lock_test_runs_once() {
    let install_script = include_str!("../../../../install.ps1");
    let lock_violation_check = format!("HResult -ne {INSTALL_SCRIPT_LOCK_VIOLATION_HRESULT})");

    for install_script_line in [
        r#"$install_lock_path = Join-Path $installation_directory "koshi.lock""#,
        INSTALL_SCRIPT_LOCK_FILE_OPEN_LINE,
        INSTALL_SCRIPT_LOCK_CALL_LINE,
        lock_violation_check.as_str(),
    ] {
        assert_eq!(
            install_script.matches(install_script_line).count(),
            1,
            "{install_script_line}"
        );
    }
}

#[cfg(windows)]
#[test]
fn the_install_ps1_lock_call_fails_with_lock_violation_while_the_install_lock_is_held() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let executable_path = test_directory.path().join("koshi.exe");
    let _install_lock_file =
        take_install_lock(&executable_path).expect("the install lock is taken");
    let escaped_install_lock_path = compute_install_lock_path(&executable_path)
        .display()
        .to_string()
        .replace('\'', "''");

    let powershell_output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!(
            "$ErrorActionPreference = 'Stop'; \
             $install_lock_path = '{escaped_install_lock_path}'; \
             {INSTALL_SCRIPT_LOCK_FILE_OPEN_LINE}; \
             try {{ {INSTALL_SCRIPT_LOCK_CALL_LINE}; 'locked' }} \
             catch [System.IO.IOException] {{ $_.Exception.GetBaseException().HResult }} \
             finally {{ $install_lock.Dispose() }}"
        ))
        .output()
        .expect("run Windows PowerShell");

    assert_eq!(
        String::from_utf8_lossy(&powershell_output.stdout).trim(),
        INSTALL_SCRIPT_LOCK_VIOLATION_HRESULT,
        "Windows PowerShell wrote on standard error: {}",
        String::from_utf8_lossy(&powershell_output.stderr)
    );
}

/// The line of `install.ps1` that names the staged copy of the new
/// `koshi.exe`: `koshi-staged-<process id>.exe` beside the installed one.
const INSTALL_SCRIPT_STAGED_COPY_LINE: &str =
    r#"$staged_binary_path = Join-Path $installation_directory "koshi-staged-$PID.exe""#;

/// The lines of `install.ps1` that move the new `koshi.exe`, whose `FileInfo`
/// `$binary_file` holds, to `$staged_binary_path`, run it there with
/// `--version`, and stop the install with an error when its first output line
/// is not `koshi $release_version_number`. The `if` block closes on the line
/// after the last one.
const INSTALL_SCRIPT_VERSION_CHECK_LINES: [&str; 4] = [
    "Move-Item -Force $binary_file.FullName $staged_binary_path",
    "$new_version_line = & $staged_binary_path --version | Select-Object -First 1",
    r#"if ("$new_version_line" -ne "koshi $release_version_number") {"#,
    r#"Write-Error "The new koshi printed '$new_version_line' for --version, not 'koshi $release_version_number'""#,
];

/// The lines of `install.ps1` that move the staged copy into place under the
/// install lock, and that delete a staged copy that is left when the install
/// stops.
const INSTALL_SCRIPT_STAGED_COPY_INSTALL_LINES: [&str; 2] = [
    "Move-Item $staged_binary_path $binary_path",
    "Remove-Item $staged_binary_path -ErrorAction SilentlyContinue",
];

#[test]
fn install_ps1_runs_the_staged_copy_before_the_install_lock_and_moves_it_in_after() {
    let install_script = include_str!("../../../../install.ps1");
    let lock_open_position = install_script
        .find(INSTALL_SCRIPT_LOCK_FILE_OPEN_LINE)
        .expect("install.ps1 opens the install lock");

    for staged_copy_line in
        std::iter::once(INSTALL_SCRIPT_STAGED_COPY_LINE).chain(INSTALL_SCRIPT_VERSION_CHECK_LINES)
    {
        assert_eq!(
            install_script.matches(staged_copy_line).count(),
            1,
            "{staged_copy_line}"
        );
        let line_position = install_script
            .find(staged_copy_line)
            .expect("install.ps1 holds the staged copy line");
        assert!(line_position < lock_open_position, "{staged_copy_line}");
    }
    for staged_copy_line in INSTALL_SCRIPT_STAGED_COPY_INSTALL_LINES {
        assert_eq!(
            install_script.matches(staged_copy_line).count(),
            1,
            "{staged_copy_line}"
        );
        let line_position = install_script
            .find(staged_copy_line)
            .expect("install.ps1 holds the staged copy line");
        assert!(line_position > lock_open_position, "{staged_copy_line}");
    }
}

/// Runs [`INSTALL_SCRIPT_VERSION_CHECK_LINES`] in Windows PowerShell under
/// `$ErrorActionPreference = 'Stop'`, with `$release_version_number` set to
/// `9.9.9`, `$binary_file` naming a `koshi.cmd` that prints `printed_line`, and
/// `$staged_binary_path` naming `koshi-staged-5000.cmd` beside it, which runs
/// as the batch file it holds. Hands back what the run prints on standard
/// output, trimmed: `installed` once the check passes, or the message of the
/// error that stops it.
#[cfg(windows)]
fn run_install_ps1_version_check(printed_line: &str) -> String {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("test directory");
    let new_binary_path = write_printing_program(
        test_directory.path(),
        "koshi",
        &test_directory.path().join("koshi-runs"),
        printed_line,
    );
    let escaped_new_binary_path = new_binary_path.display().to_string().replace('\'', "''");
    let escaped_staged_binary_path = test_directory
        .path()
        .join("koshi-staged-5000.cmd")
        .display()
        .to_string()
        .replace('\'', "''");
    let [move_line, version_line, compare_line, error_line] = INSTALL_SCRIPT_VERSION_CHECK_LINES;
    let check_script_path = test_directory.path().join("version-check.ps1");
    fs::write(
        &check_script_path,
        format!(
            "$ErrorActionPreference = 'Stop'\n\
             $release_version_number = '9.9.9'\n\
             $binary_file = Get-Item -LiteralPath '{escaped_new_binary_path}'\n\
             $staged_binary_path = '{escaped_staged_binary_path}'\n\
             try {{\n\
             {move_line}\n\
             {version_line}\n\
             {compare_line}\n\
             {error_line}\n\
             }}\n\
             'installed'\n\
             }} catch {{\n\
             $_.Exception.Message\n\
             }}\n"
        ),
    )
    .expect("write the version check script");

    let powershell_output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&check_script_path)
        .output()
        .expect("run Windows PowerShell");
    String::from_utf8_lossy(&powershell_output.stdout)
        .trim()
        .to_string()
}

#[cfg(windows)]
#[test]
fn the_install_ps1_version_check_passes_a_koshi_that_prints_the_release_version() {
    assert_eq!(run_install_ps1_version_check("koshi 9.9.9"), "installed");
}

#[cfg(windows)]
#[test]
fn the_install_ps1_version_check_stops_the_install_for_a_koshi_that_prints_another_version() {
    assert_eq!(
        run_install_ps1_version_check("koshi 9.9.8"),
        "The new koshi printed 'koshi 9.9.8' for --version, not 'koshi 9.9.9'"
    );
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

#[test]
fn update_state_written_by_koshi_0_4_0_keeps_its_last_check_time() {
    assert_eq!(
        parse_update_state(r#"{"last_check":1700000000}"#).last_check_unix_seconds,
        Some(1_700_000_000)
    );
}

#[test]
fn update_state_in_the_current_shape_reads_as_written() {
    assert_eq!(
        parse_update_state(r#"{"last_check_unix_seconds":1700000000}"#).last_check_unix_seconds,
        Some(1_700_000_000)
    );
}

#[test]
fn update_state_that_parses_as_neither_shape_reads_as_never_checked() {
    assert_eq!(parse_update_state("not json").last_check_unix_seconds, None);
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
        CliError::Update {
            detail: error_detail,
        } => assert_eq!(error_detail, "boom"),
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
    let build_releases = |release_tags: &[&str]| -> Vec<Release> {
        release_tags
            .iter()
            .map(|release_tag| Release {
                tag_name: (*release_tag).to_string(),
            })
            .collect()
    };

    assert_eq!(
        find_highest_release_version(build_releases(&["v0.3.0-rc.2", "v0.3.0-rc.10", "v0.2.0",]))
            .unwrap(),
        "v0.3.0-rc.10"
    );
    // List order plays no part: the highest wins from the front too.
    assert_eq!(
        find_highest_release_version(build_releases(&["v0.4.0", "v0.3.0"])).unwrap(),
        "v0.4.0"
    );
    // A tag that is not a version is skipped, not an error.
    assert_eq!(
        find_highest_release_version(build_releases(&["nightly", "v0.1.0"])).unwrap(),
        "v0.1.0"
    );
    assert_eq!(
        find_highest_release_version(Vec::new()).unwrap_err(),
        "no releases found"
    );
    // A list where no tag is a version reads the same as an empty one.
    assert_eq!(
        find_highest_release_version(build_releases(&["nightly", "edge"])).unwrap_err(),
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
    let new_binary_path = write_printing_program(
        program_directory.path(),
        "new-binary",
        &program_directory.path().join("new-binary-runs"),
        "koshi 9.9.9",
    );
    let new_binary_bytes = fs::read(&new_binary_path).expect("read the replacement executable");

    swap_executable(&new_binary_path, &link_path, "v9.9.9").expect("replace the executable");

    assert_eq!(
        fs::read_link(&link_path).expect("the link is still a link"),
        program_path
    );
    assert_eq!(
        fs::read(&program_path).expect("read the replaced file"),
        new_binary_bytes
    );
}

#[cfg(unix)]
#[test]
fn a_swap_whose_copy_fails_removes_the_staged_copy_and_keeps_the_program_file() {
    let (program_directory, program_path, _other_program_path) = build_program_files();
    fs::write(&program_path, b"old-binary").expect("write the old executable");
    let new_binary_path = program_directory.path().join("new-binary");
    fs::write(&new_binary_path, b"new-binary").expect("write the replacement executable");
    let staged_binary_path = program_directory
        .path()
        .join(format!("koshi.koshi-update-{}", std::process::id()));
    std::os::unix::fs::symlink(
        program_directory.path().join("missing-directory/koshi"),
        &staged_binary_path,
    )
    .expect("link the staged copy path into a missing directory");

    let swap_result = swap_executable(&new_binary_path, &program_path, "v9.9.9");

    assert_eq!(
        swap_result,
        Err("No such file or directory (os error 2)".to_string())
    );
    assert_eq!(
        fs::symlink_metadata(&staged_binary_path)
            .err()
            .map(|metadata_error| metadata_error.kind()),
        Some(io::ErrorKind::NotFound)
    );
    assert_eq!(
        fs::read(&program_path).expect("read the program file"),
        b"old-binary"
    );
    assert_eq!(
        fs::read(&new_binary_path).expect("read the replacement executable"),
        b"new-binary"
    );
}

/// A process id above every id that Linux, macOS, and Windows give a process:
/// `2147483647`.
const FREE_PROCESS_ID: u32 = 2_147_483_647;

#[test]
fn a_staged_copy_name_gives_back_the_process_id_it_was_formatted_with() {
    let staged_copy_name = StagedCopyName::from_program_path(Path::new("/opt/koshi/koshi"));
    let expected_file_name = if cfg!(windows) {
        "koshi-staged-5000.exe"
    } else {
        "koshi.koshi-update-5000"
    };

    assert_eq!(staged_copy_name.format_file_name(5000), expected_file_name);
    assert_eq!(
        staged_copy_name.parse_process_id(expected_file_name),
        Some(5000)
    );
}

#[test]
fn a_name_that_is_not_a_staged_copy_name_gives_no_process_id() {
    let staged_copy_name = StagedCopyName::from_program_path(Path::new("/opt/koshi/koshi"));
    let other_entry_names = if cfg!(windows) {
        [
            "koshi-staged-.exe",
            "koshi-staged-+5.exe",
            "koshi-staged-5x.exe",
            "koshi-staged-4294967296.exe",
            "koshi-staged-5000.exe.old",
            "koshi-staged-5000",
            "koshi.old",
        ]
    } else {
        [
            "koshi.koshi-update-",
            "koshi.koshi-update-+5",
            "koshi.koshi-update-5x",
            "koshi.koshi-update-4294967296",
            "koshi.koshi-update-5000.old",
            "koshi-dev.koshi-update-5000",
            "koshi.old",
        ]
    };

    for other_entry_name in other_entry_names {
        assert_eq!(
            staged_copy_name.parse_process_id(other_entry_name),
            None,
            "{other_entry_name}"
        );
    }
}

#[test]
fn staged_copies_of_ended_updates_are_deleted_and_the_copy_of_a_running_process_stays() {
    let program_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let program_path = program_directory.path().join(get_binary_file_name());
    let staged_copy_name = StagedCopyName::from_program_path(&program_path);
    let running_staged_copy_file_name = staged_copy_name.format_file_name(std::process::id());
    for entry_name in [
        get_binary_file_name().to_string(),
        staged_copy_name.format_file_name(FREE_PROCESS_ID),
        running_staged_copy_file_name.clone(),
        "notes.txt".to_string(),
    ] {
        let entry_path = program_directory.path().join(entry_name);
        fs::write(entry_path, b"").expect("write a directory entry");
    }

    delete_staged_copies_of_ended_updates(&program_path);

    let mut expected_entry_names = vec![
        get_binary_file_name().to_string(),
        running_staged_copy_file_name,
        "notes.txt".to_string(),
    ];
    expected_entry_names.sort();
    assert_eq!(
        list_sorted_entry_names(program_directory.path()),
        expected_entry_names
    );
}

#[cfg(windows)]
#[test]
fn a_copy_that_koshi_0_5_0_staged_for_an_ended_update_is_deleted_and_the_copy_of_a_running_process_stays(
) {
    let program_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let program_path = program_directory.path().join("koshi.exe");
    let running_staged_copy_file_name = format!("koshi-update-{}.exe", std::process::id());
    for entry_name in [
        "koshi.exe".to_string(),
        format!("koshi-update-{FREE_PROCESS_ID}.exe"),
        running_staged_copy_file_name.clone(),
    ] {
        fs::write(program_directory.path().join(entry_name), b"").expect("write a directory entry");
    }

    delete_staged_copies_of_ended_updates(&program_path);

    let mut expected_entry_names = vec!["koshi.exe".to_string(), running_staged_copy_file_name];
    expected_entry_names.sort();
    assert_eq!(
        list_sorted_entry_names(program_directory.path()),
        expected_entry_names
    );
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_with_the_staged_copy_name_of_an_ended_update_is_deleted_and_its_target_stays() {
    let (program_directory, program_path, other_program_path) = build_program_files();
    fs::write(&other_program_path, b"other-binary").expect("write the link target");
    let staged_copy_name = StagedCopyName::from_program_path(&program_path);
    let staged_link_path = program_directory
        .path()
        .join(staged_copy_name.format_file_name(FREE_PROCESS_ID));
    std::os::unix::fs::symlink(&other_program_path, &staged_link_path)
        .expect("link the staged copy name to the other program file");

    delete_staged_copies_of_ended_updates(&program_path);

    assert_eq!(
        fs::symlink_metadata(&staged_link_path)
            .err()
            .map(|metadata_error| metadata_error.kind()),
        Some(io::ErrorKind::NotFound)
    );
    assert_eq!(
        fs::read(&other_program_path).expect("read the link target"),
        b"other-binary"
    );
}

#[cfg(unix)]
#[test]
fn a_swap_deletes_the_staged_copy_that_an_ended_update_left() {
    let program_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("program directory");
    let program_path = program_directory.path().join("koshi");
    let new_binary_path = write_printing_program(
        program_directory.path(),
        "new-binary",
        &program_directory.path().join("new-binary-runs"),
        "koshi 9.9.9",
    );
    let new_binary_bytes = fs::read(&new_binary_path).expect("read the replacement executable");
    let staged_copy_name = StagedCopyName::from_program_path(&program_path);
    let stray_staged_binary_path = program_directory
        .path()
        .join(staged_copy_name.format_file_name(FREE_PROCESS_ID));
    fs::write(&program_path, b"old-binary").expect("write the old executable");
    fs::write(&stray_staged_binary_path, b"stray-binary").expect("write the stray staged copy");

    let swap_result = swap_executable(&new_binary_path, &program_path, "v9.9.9");

    assert_eq!(swap_result, Ok(()));
    assert_eq!(
        fs::read(&program_path).expect("read the replaced executable"),
        new_binary_bytes
    );
    assert_eq!(
        fs::symlink_metadata(&stray_staged_binary_path)
            .err()
            .map(|metadata_error| metadata_error.kind()),
        Some(io::ErrorKind::NotFound)
    );
}

#[cfg(unix)]
#[test]
fn the_staged_copy_write_script_copies_the_release_with_mode_755_and_deletes_ended_copies() {
    use std::os::unix::fs::PermissionsExt;

    let (program_directory, program_path, _other_program_path) = build_program_files();
    fs::write(&program_path, b"old-binary").expect("write the old executable");
    let new_binary_path = program_directory.path().join("new-binary");
    fs::write(&new_binary_path, b"new-binary").expect("write the replacement executable");
    let staged_copy_name = StagedCopyName::from_program_path(&program_path);
    let staged_copy_file_name = staged_copy_name.format_file_name(std::process::id());
    let staged_binary_path = program_directory.path().join(&staged_copy_file_name);
    let ended_staged_copy_path = program_directory
        .path()
        .join(staged_copy_name.format_file_name(FREE_PROCESS_ID));
    fs::write(&ended_staged_copy_path, b"stray-binary").expect("write the stray staged copy");

    let script_status = std::process::Command::new("sh")
        .args(list_staged_copy_write_arguments(
            &new_binary_path,
            &staged_binary_path,
            &[ended_staged_copy_path],
        ))
        .status()
        .expect("run the staged copy write script");

    assert_eq!(script_status.code(), Some(0));
    assert_eq!(
        fs::read(&staged_binary_path).expect("read the staged copy"),
        b"new-binary"
    );
    let permission_mode = fs::metadata(&staged_binary_path)
        .expect("read the staged copy's metadata")
        .permissions()
        .mode();
    assert_eq!(permission_mode & 0o777, 0o755);
    assert_eq!(
        fs::read(&program_path).expect("read the program file"),
        b"old-binary"
    );
    assert_eq!(
        list_sorted_entry_names(program_directory.path()),
        [
            "koshi",
            staged_copy_file_name.as_str(),
            "new-binary",
            "other-koshi"
        ]
    );
}

#[cfg(unix)]
#[test]
fn the_staged_copy_write_script_whose_copy_fails_removes_the_copy_path() {
    let (program_directory, program_path, _other_program_path) = build_program_files();
    let new_binary_path = program_directory.path().join("new-binary");
    fs::write(&new_binary_path, b"new-binary").expect("write the replacement executable");
    let staged_binary_path = program_directory.path().join(
        StagedCopyName::from_program_path(&program_path).format_file_name(std::process::id()),
    );
    std::os::unix::fs::symlink(
        program_directory.path().join("missing-directory/koshi"),
        &staged_binary_path,
    )
    .expect("link the staged copy path into a missing directory");

    let script_status = std::process::Command::new("sh")
        .args(list_staged_copy_write_arguments(
            &new_binary_path,
            &staged_binary_path,
            &[],
        ))
        .stderr(std::process::Stdio::null())
        .status()
        .expect("run the staged copy write script");

    assert_eq!(script_status.code(), Some(1));
    assert_eq!(
        list_sorted_entry_names(program_directory.path()),
        ["koshi", "new-binary", "other-koshi"]
    );
}

#[cfg(unix)]
#[test]
fn the_staged_copy_rename_script_renames_the_copy_over_the_program_file() {
    let (program_directory, program_path, _other_program_path) = build_program_files();
    fs::write(&program_path, b"old-binary").expect("write the old executable");
    let staged_binary_path = program_directory.path().join(
        StagedCopyName::from_program_path(&program_path).format_file_name(std::process::id()),
    );
    fs::write(&staged_binary_path, b"new-binary").expect("write the staged copy");

    let script_status = std::process::Command::new("sh")
        .args(list_staged_copy_rename_arguments(
            &staged_binary_path,
            &program_path,
        ))
        .status()
        .expect("run the staged copy rename script");

    assert_eq!(script_status.code(), Some(0));
    assert_eq!(
        fs::read(&program_path).expect("read the replaced executable"),
        b"new-binary"
    );
    assert_eq!(
        list_sorted_entry_names(program_directory.path()),
        ["koshi", "other-koshi"]
    );
}

#[cfg(unix)]
#[test]
fn the_staged_copy_rename_script_whose_rename_fails_removes_the_copy() {
    let (program_directory, program_path, _other_program_path) = build_program_files();
    let staged_binary_path = program_directory.path().join(
        StagedCopyName::from_program_path(&program_path).format_file_name(std::process::id()),
    );
    fs::write(&staged_binary_path, b"new-binary").expect("write the staged copy");
    let missing_program_path = program_directory.path().join("missing-directory/koshi");

    let script_status = std::process::Command::new("sh")
        .args(list_staged_copy_rename_arguments(
            &staged_binary_path,
            &missing_program_path,
        ))
        .stderr(std::process::Stdio::null())
        .status()
        .expect("run the staged copy rename script");

    assert_eq!(script_status.code(), Some(1));
    assert_eq!(
        list_sorted_entry_names(program_directory.path()),
        ["koshi", "other-koshi"]
    );
}

/// The line of `install.sh` that names the staged copy of the binary.
const INSTALL_SH_STAGED_COPY_LINE: &str =
    r#"staged_binary_path="${installation_directory}/koshi.koshi-update-$$""#;

/// The line of `install.sh` that ends the script with status `1` on `HUP`,
/// `INT` and `TERM`, which runs its `EXIT` trap.
const INSTALL_SH_SIGNAL_TRAP_LINE: &str = "trap 'exit 1' HUP INT TERM";

/// The `EXIT` trap of `install.sh` for an installation directory this user
/// may write: it deletes the staging directory and the staged copy.
const INSTALL_SH_CLEANUP_TRAP_LINE: &str =
    r#"trap 'rm -rf "${staging_directory}"; rm -f "${staged_binary_path}"' EXIT"#;

/// The lines of `install.sh` that define `validate_staged_binary`: it runs the
/// staged copy with `--version`, and a first line on standard output other
/// than `koshi <release_version_number>` logs an error and ends the script
/// with status `1`. What the copy writes on standard error passes through.
const INSTALL_SH_VALIDATE_FUNCTION_LINES: [&str; 7] = [
    "validate_staged_binary() {",
    r#"staged_version_line="$("${staged_binary_path}" --version | head -n 1)""#,
    r#"if [ "${staged_version_line}" != "koshi ${release_version_number}" ]; then"#,
    r#"log_error "The new koshi printed '${staged_version_line}' for --version, not 'koshi ${release_version_number}'""#,
    "exit 1",
    "fi",
    "}",
];

/// The lines of `install.sh` that copy the binary beside the installed one,
/// set mode `755` on the copy, check the copy, and rename the copy over the
/// installed one, for an installation directory this user may write.
const INSTALL_SH_INSTALL_LINES: [&str; 4] = [
    r#"cp "${binary_path}" "${staged_binary_path}""#,
    r#"chmod 755 "${staged_binary_path}""#,
    "validate_staged_binary",
    r#"mv -f "${staged_binary_path}" "${installed_binary_path}""#,
];

/// The lines of [`INSTALL_SH_INSTALL_LINES`] for an installation directory
/// that only root may write: the copy, the mode change, and the rename run
/// through `sudo`, and the check runs as this user.
const INSTALL_SH_SUDO_INSTALL_LINES: [&str; 4] = [
    r#"sudo cp "${binary_path}" "${staged_binary_path}""#,
    r#"sudo chmod 755 "${staged_binary_path}""#,
    "validate_staged_binary",
    r#"sudo mv -f "${staged_binary_path}" "${installed_binary_path}""#,
];

/// A `bash` program that runs the install lines of `install.sh` in their
/// order, under `set -e`, with the release version number `9.9.9` and a
/// `log_error` that prints its message on standard error. `$1` is the
/// installation directory, `$2` the staging directory that holds `koshi`, and
/// `$3` the installed binary path. After the copy, its mode change, and its
/// check, the program prints the staged copy file name and runs
/// `step_before_rename`, then renames the copy.
#[cfg(unix)]
fn build_install_sh_program(step_before_rename: &str) -> String {
    let [copy_line, mode_line, validate_line, rename_line] = INSTALL_SH_INSTALL_LINES;
    let mut program_lines = vec![
        "set -e",
        r#"release_version_number="9.9.9""#,
        r#"installation_directory="$1""#,
        r#"staging_directory="$2""#,
        r#"binary_path="${staging_directory}/koshi""#,
        r#"installed_binary_path="$3""#,
        r#"log_error() { echo "$1" >&2; }"#,
        INSTALL_SH_SIGNAL_TRAP_LINE,
        INSTALL_SH_STAGED_COPY_LINE,
    ];
    program_lines.extend(INSTALL_SH_VALIDATE_FUNCTION_LINES);
    program_lines.extend([
        INSTALL_SH_CLEANUP_TRAP_LINE,
        copy_line,
        mode_line,
        validate_line,
        r#"echo "${staged_binary_path##*/}""#,
        step_before_rename,
        rename_line,
    ]);
    program_lines.join("\n")
}

/// An installation directory holding `koshi` with the bytes `old-binary`, and
/// a staging directory holding a `koshi` program that prints `printed_line`,
/// as [`write_printing_program`] writes it.
#[cfg(unix)]
fn build_install_sh_directories(printed_line: &str) -> (tempfile::TempDir, tempfile::TempDir) {
    let installation_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("an installation directory");
    let staging_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("a staging directory");
    fs::write(installation_directory.path().join("koshi"), b"old-binary")
        .expect("write the installed binary");
    write_printing_program(
        staging_directory.path(),
        "koshi",
        &staging_directory.path().join("koshi-runs"),
        printed_line,
    );
    (installation_directory, staging_directory)
}

#[test]
fn install_sh_holds_each_install_line_that_the_unix_install_tests_run() {
    let install_script_lines: Vec<&str> = include_str!("../../../../install.sh")
        .lines()
        .map(str::trim)
        .collect();

    for pinned_line in [
        INSTALL_SH_STAGED_COPY_LINE,
        INSTALL_SH_SIGNAL_TRAP_LINE,
        INSTALL_SH_CLEANUP_TRAP_LINE,
    ] {
        assert_eq!(
            install_script_lines
                .iter()
                .filter(|install_script_line| **install_script_line == pinned_line)
                .count(),
            1,
            "{pinned_line}"
        );
    }
    for pinned_lines in [
        INSTALL_SH_VALIDATE_FUNCTION_LINES.as_slice(),
        INSTALL_SH_INSTALL_LINES.as_slice(),
        INSTALL_SH_SUDO_INSTALL_LINES.as_slice(),
    ] {
        assert_eq!(
            install_script_lines
                .windows(pinned_lines.len())
                .filter(|install_script_window| *install_script_window == pinned_lines)
                .count(),
            1,
            "{pinned_lines:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn the_install_sh_lines_rename_a_copy_with_the_staged_copy_name_over_the_installed_binary() {
    use std::os::unix::fs::PermissionsExt;

    let (installation_directory, staging_directory) = build_install_sh_directories("koshi 9.9.9");
    let installed_binary_path = installation_directory.path().join("koshi");
    let new_binary_bytes =
        fs::read(staging_directory.path().join("koshi")).expect("read the extracted binary");

    let install_process = std::process::Command::new("bash")
        .arg("-c")
        .arg(build_install_sh_program(""))
        .arg("bash")
        .arg(installation_directory.path())
        .arg(staging_directory.path())
        .arg(&installed_binary_path)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("start the install lines");
    let install_process_id = install_process.id();
    let install_output = install_process
        .wait_with_output()
        .expect("wait for the install lines");

    assert_eq!(install_output.status.code(), Some(0));
    let staged_copy_file_name = StagedCopyName::from_program_path(&installed_binary_path)
        .format_file_name(install_process_id);
    assert_eq!(
        String::from_utf8(install_output.stdout).expect("a UTF-8 staged copy file name"),
        format!("{staged_copy_file_name}\n")
    );
    assert_eq!(
        fs::read(&installed_binary_path).expect("read the installed binary"),
        new_binary_bytes
    );
    let permission_mode = fs::metadata(&installed_binary_path)
        .expect("read the installed binary's metadata")
        .permissions()
        .mode();
    assert_eq!(permission_mode & 0o777, 0o755);
    assert_eq!(
        list_sorted_entry_names(installation_directory.path()),
        ["koshi"]
    );
    assert_eq!(
        fs::symlink_metadata(staging_directory.path())
            .err()
            .map(|metadata_error| metadata_error.kind()),
        Some(io::ErrorKind::NotFound)
    );
}

#[cfg(unix)]
#[test]
fn install_sh_lines_install_a_copy_that_writes_a_warning_on_standard_error_before_its_version() {
    let (installation_directory, staging_directory) = build_install_sh_directories("koshi 9.9.9");
    let installed_binary_path = installation_directory.path().join("koshi");
    let extracted_binary_path = staging_directory.path().join("koshi");
    let loader_warning_line =
        "ERROR: ld.so: object '/missing.so' from LD_PRELOAD cannot be preloaded: ignored.";
    fs::write(
        &extracted_binary_path,
        format!("#!/bin/sh\necho \"{loader_warning_line}\" >&2\necho 'koshi 9.9.9'\n"),
    )
    .expect("write a binary that warns on standard error before its version");
    let new_binary_bytes = fs::read(&extracted_binary_path).expect("read the extracted binary");

    let install_output = std::process::Command::new("bash")
        .arg("-c")
        .arg(build_install_sh_program(""))
        .arg("bash")
        .arg(installation_directory.path())
        .arg(staging_directory.path())
        .arg(&installed_binary_path)
        .output()
        .expect("run the install lines");

    assert_eq!(install_output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&install_output.stderr),
        format!("{loader_warning_line}\n")
    );
    assert_eq!(
        fs::read(&installed_binary_path).expect("read the installed binary"),
        new_binary_bytes
    );
}

#[cfg(unix)]
#[test]
fn install_sh_lines_whose_rename_fails_remove_the_copy_and_the_staging_directory() {
    let (installation_directory, staging_directory) = build_install_sh_directories("koshi 9.9.9");
    let missing_installed_binary_path = installation_directory
        .path()
        .join("missing-directory/koshi");

    let install_status = std::process::Command::new("bash")
        .arg("-c")
        .arg(build_install_sh_program(""))
        .arg("bash")
        .arg(installation_directory.path())
        .arg(staging_directory.path())
        .arg(&missing_installed_binary_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("run the install lines");

    assert_eq!(install_status.code(), Some(1));
    assert_eq!(
        list_sorted_entry_names(installation_directory.path()),
        ["koshi"]
    );
    assert_eq!(
        fs::read(installation_directory.path().join("koshi")).expect("read the installed binary"),
        b"old-binary"
    );
    assert_eq!(
        fs::symlink_metadata(staging_directory.path())
            .err()
            .map(|metadata_error| metadata_error.kind()),
        Some(io::ErrorKind::NotFound)
    );
}

#[cfg(unix)]
#[test]
fn install_sh_lines_stopped_by_ctrl_c_remove_the_copy_and_keep_the_installed_binary() {
    use std::io::BufRead as _;
    use std::os::unix::process::CommandExt as _;

    let (installation_directory, staging_directory) = build_install_sh_directories("koshi 9.9.9");
    let installed_binary_path = installation_directory.path().join("koshi");

    let mut install_process = std::process::Command::new("bash")
        .arg("-c")
        .arg(build_install_sh_program(
            "sh -c 'echo running; exec sleep 30'",
        ))
        .arg("bash")
        .arg(installation_directory.path())
        .arg(staging_directory.path())
        .arg(&installed_binary_path)
        .stdout(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("start the install lines");
    let install_output_pipe = install_process
        .stdout
        .take()
        .expect("the install lines output");
    let mut install_output_reader = std::io::BufReader::new(install_output_pipe);
    let mut staged_copy_line = String::new();
    install_output_reader
        .read_line(&mut staged_copy_line)
        .expect("read the staged copy file name");
    let mut running_line = String::new();
    install_output_reader
        .read_line(&mut running_line)
        .expect("read the line of the step before the rename");
    let install_process_group_id =
        libc::pid_t::try_from(install_process.id()).expect("a process group id that fits pid_t");
    // SAFETY: `kill` takes a process group id and a signal number, and reads
    // no memory of this process.
    let kill_answer = unsafe { libc::kill(-install_process_group_id, libc::SIGINT) };
    let install_status = install_process.wait().expect("wait for the install lines");

    let staged_copy_file_name = StagedCopyName::from_program_path(&installed_binary_path)
        .format_file_name(install_process.id());
    assert_eq!(staged_copy_line, format!("{staged_copy_file_name}\n"));
    assert_eq!(running_line, "running\n");
    assert_eq!(kill_answer, 0);
    assert_eq!(install_status.code(), Some(1));
    assert_eq!(
        fs::read(&installed_binary_path).expect("read the installed binary"),
        b"old-binary"
    );
    assert_eq!(
        list_sorted_entry_names(installation_directory.path()),
        ["koshi"]
    );
    assert_eq!(
        fs::symlink_metadata(staging_directory.path())
            .err()
            .map(|metadata_error| metadata_error.kind()),
        Some(io::ErrorKind::NotFound)
    );
}

#[cfg(unix)]
#[test]
fn install_sh_lines_whose_copy_prints_another_version_remove_the_copy_and_keep_the_installed_binary(
) {
    let (installation_directory, staging_directory) = build_install_sh_directories("koshi 9.9.8");
    let installed_binary_path = installation_directory.path().join("koshi");

    let install_output = std::process::Command::new("bash")
        .arg("-c")
        .arg(build_install_sh_program(""))
        .arg("bash")
        .arg(installation_directory.path())
        .arg(staging_directory.path())
        .arg(&installed_binary_path)
        .output()
        .expect("run the install lines");

    assert_eq!(install_output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(install_output.stderr).expect("a UTF-8 error message"),
        "The new koshi printed 'koshi 9.9.8' for --version, not 'koshi 9.9.9'\n"
    );
    assert_eq!(install_output.stdout, b"");
    assert_eq!(
        fs::read(&installed_binary_path).expect("read the installed binary"),
        b"old-binary"
    );
    assert_eq!(
        list_sorted_entry_names(installation_directory.path()),
        ["koshi"]
    );
    assert_eq!(
        fs::symlink_metadata(staging_directory.path())
            .err()
            .map(|metadata_error| metadata_error.kind()),
        Some(io::ErrorKind::NotFound)
    );
}

#[test]
fn the_runtime_directory_itself_is_left_out_of_the_other_runtime_directories() {
    let runtime_directory = build_test_runtime_directory();
    let other_directory = build_test_runtime_directory();

    assert_eq!(
        list_other_runtime_directories(
            vec![
                runtime_directory.path().to_path_buf(),
                other_directory.path().to_path_buf(),
            ],
            runtime_directory.path(),
        ),
        vec![other_directory.path().to_path_buf()]
    );
}

#[test]
fn a_runtime_directory_that_does_not_exist_is_compared_as_it_is() {
    let runtime_directory = build_test_runtime_directory();
    let missing_directory = runtime_directory.path().join("missing");

    assert_eq!(
        list_other_runtime_directories(
            vec![missing_directory.clone()],
            &runtime_directory.path().join("also-missing"),
        ),
        vec![missing_directory]
    );
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_to_the_runtime_directory_is_left_out_of_the_other_runtime_directories() {
    let runtime_directory = build_test_runtime_directory();
    let link_directory = build_test_runtime_directory();
    let link_path = link_directory.path().join("runtime-link");
    std::os::unix::fs::symlink(runtime_directory.path(), &link_path)
        .expect("link the runtime directory");

    assert_eq!(
        list_other_runtime_directories(vec![link_path], runtime_directory.path()),
        Vec::<PathBuf>::new()
    );
}

#[test]
fn a_runtime_directory_of_koshi_0_2_0_that_does_not_exist_counts_nothing() {
    let runtime_directory = build_test_runtime_directory();

    assert_eq!(
        count_previous_release_servers(&runtime_directory.path().join("missing")),
        PreviousReleaseServerCount {
            session_count: 0,
            open_window_count: 0,
        }
    );
}

#[test]
fn an_endpoint_file_naming_a_process_that_is_not_a_koshi_server_counts_nothing() {
    let runtime_directory = build_test_runtime_directory();
    write_session_endpoint_file(
        runtime_directory.path(),
        SessionId::new(),
        "k7QxSecret",
        std::process::id(),
    );

    assert_eq!(
        count_previous_release_servers(runtime_directory.path()),
        PreviousReleaseServerCount {
            session_count: 0,
            open_window_count: 0,
        }
    );
}

#[test]
fn the_endpoint_file_of_a_closed_koshi_0_1_0_window_counts_nothing() {
    let runtime_directory = build_test_runtime_directory();
    write_koshi_0_1_0_window_endpoint_file(runtime_directory.path(), SessionId::new());

    assert_eq!(
        count_previous_release_servers(runtime_directory.path()),
        PreviousReleaseServerCount {
            session_count: 0,
            open_window_count: 0,
        }
    );
}

#[test]
fn the_endpoint_file_of_an_open_koshi_0_1_0_window_counts_one_open_window() {
    let runtime_directory = build_test_runtime_directory();
    let window_session_id = SessionId::new();
    let _window_listener = Listener::bind(&compute_socket_address(
        runtime_directory.path(),
        window_session_id,
    ))
    .expect("bind the stand-in window");
    write_koshi_0_1_0_window_endpoint_file(runtime_directory.path(), window_session_id);

    assert_eq!(
        count_previous_release_servers(runtime_directory.path()),
        PreviousReleaseServerCount {
            session_count: 0,
            open_window_count: 1,
        }
    );
}

#[test]
fn no_session_running_from_a_runtime_directory_of_koshi_0_2_0_prints_no_line() {
    assert_eq!(
        format_previous_release_session_note(Path::new("/home/user/.local/share/koshi/run"), 0),
        None
    );
}

#[test]
fn one_session_running_from_a_runtime_directory_of_koshi_0_2_0_is_named_with_restart_servers() {
    assert_eq!(
        format_previous_release_session_note(Path::new("/home/user/.local/share/koshi/run"), 1),
        Some(
            "1 session that an older koshi started runs from /home/user/.local/share/koshi/run, \
             which this koshi does not list; run koshi restart-servers to move it or end it"
                .to_string()
        )
    );
}

#[test]
fn several_sessions_running_from_a_runtime_directory_of_koshi_0_2_0_are_counted_in_one_line() {
    assert_eq!(
        format_previous_release_session_note(Path::new("/home/user/.local/share/koshi/run"), 2),
        Some(
            "2 sessions that an older koshi started run from /home/user/.local/share/koshi/run, \
             which this koshi does not list; run koshi restart-servers to move them or end them"
                .to_string()
        )
    );
}

#[test]
fn no_koshi_0_1_0_window_running_from_a_runtime_directory_prints_no_line() {
    assert_eq!(
        format_koshi_0_1_0_window_note(Path::new("/home/user/.local/share/koshi/run"), 0),
        None
    );
}

#[test]
fn one_koshi_0_1_0_window_running_from_a_runtime_directory_is_named_with_how_it_ends() {
    assert_eq!(
        format_koshi_0_1_0_window_note(Path::new("/home/user/.local/share/koshi/run"), 1),
        Some(
            "1 koshi 0.1.0 window runs from /home/user/.local/share/koshi/run; this koshi cannot \
             talk to it, and it ends when its terminal closes"
                .to_string()
        )
    );
}

#[test]
fn several_koshi_0_1_0_windows_running_from_a_runtime_directory_are_counted_in_one_line() {
    assert_eq!(
        format_koshi_0_1_0_window_note(Path::new("/home/user/.local/share/koshi/run"), 2),
        Some(
            "2 koshi 0.1.0 windows run from /home/user/.local/share/koshi/run; this koshi cannot \
             talk to them, and each one ends when its terminal closes"
                .to_string()
        )
    );
}
