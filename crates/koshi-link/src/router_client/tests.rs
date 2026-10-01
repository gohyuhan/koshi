//! Tests for the client side of the router socket, against a stand-in router
//! serving a real socket in a temporary runtime directory.
//!
//! Every test that starts the stand-in finds it already listening, so the
//! exchange succeeds on its first attempt and no router is ever started.
//! Starting one is covered by the integration tests.

use super::*;
use koshi_ipc::router::ROUTER_PROTOCOL_VERSION;

use std::thread::JoinHandle;

use koshi_core::ids::SessionId;
use koshi_ipc::endpoint::ServerProgramFile;
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcErrorPayload};
use koshi_ipc::router::{
    compute_router_socket_address, resolve_router_endpoint_path, RouterHandshake, RouterResponse,
    SessionAddress, SessionSelector,
};
use koshi_ipc::transport::Listener;
use koshi_test_support::fixtures::{
    build_test_runtime_directory, close_connection_after_peer_hangs_up, hold_update_lock,
    write_router_endpoint_file,
};

/// How the stand-in router answers the caller.
enum RouterScript {
    /// The endpoint file carries the router's own token, so the Hello opens
    /// the connection and the request behind it is answered with this result.
    AcceptAndAnswer(RouterResult),
    /// The same, and the Hello reports the build named here.
    AcceptAndAnswerAs(String, RouterResult),
    /// The endpoint file carries a token the router does not hold, so the
    /// Hello is refused and the request behind it is refused too.
    RefuseHello,
    /// The Hello is refused with `UnsupportedVersion` and
    /// [`VERSION_REFUSAL_SENTENCE`], and the request behind it gets no answer.
    RefuseVersion,
}

/// Serve one connection per entry of `router_scripts`, in order, as a router
/// would: bind the router's address, write the endpoint file advertising it,
/// accept one caller, answer the Hello and the request pipelined behind it per
/// the entry, and close the connection once the caller hung up. Before each
/// connection after the first, the router restarts on the address it already
/// holds: it writes the endpoint file again under a new token. The thread
/// hands back the kind of the request behind the Hello on the last
/// connection.
///
/// The bind and the first endpoint file are both done before this returns.
fn spawn_fake_router(
    runtime_directory: &Path,
    router_scripts: Vec<RouterScript>,
) -> JoinHandle<RouterRequestKind> {
    let router_socket_address = compute_router_socket_address(runtime_directory);
    let router_listener = Listener::bind(&router_socket_address).expect("bind the stand-in router");
    let router_endpoint_path = resolve_router_endpoint_path(runtime_directory);
    let advertise_router = move |router_script: &RouterScript| {
        let router_connection_token = ConnectionToken::generate();
        let advertised_connection_token = match router_script {
            RouterScript::AcceptAndAnswer(_)
            | RouterScript::AcceptAndAnswerAs(..)
            | RouterScript::RefuseVersion => router_connection_token.clone(),
            RouterScript::RefuseHello => ConnectionToken::generate(),
        };
        EndpointFile {
            socket_address: router_socket_address.clone(),
            connection_token: advertised_connection_token,
            process_id: std::process::id(),
        }
        .write_to_path(&router_endpoint_path)
        .expect("write the router endpoint file");
        router_connection_token
    };
    let mut router_scripts = router_scripts.into_iter();
    let first_router_script = router_scripts
        .next()
        .expect("the stand-in router serves at least one connection");
    let first_router_connection_token = advertise_router(&first_router_script);

    std::thread::spawn(move || {
        let mut router_request_kind = answer_router_connection(
            &router_listener,
            first_router_connection_token,
            first_router_script,
        );
        for router_script in router_scripts {
            let router_connection_token = advertise_router(&router_script);
            router_request_kind =
                answer_router_connection(&router_listener, router_connection_token, router_script);
        }
        router_request_kind
    })
}

/// Accept one caller on `router_listener`, answer the Hello and the request
/// pipelined behind it per `router_script`, with `router_connection_token` as
/// the router's own token, and close the connection once the caller hung up.
/// Hands back the kind of the request behind the Hello.
fn answer_router_connection(
    router_listener: &Listener,
    router_connection_token: ConnectionToken,
    router_script: RouterScript,
) -> RouterRequestKind {
    let reported_version = match &router_script {
        RouterScript::AcceptAndAnswerAs(reported_version, _) => reported_version.clone(),
        RouterScript::AcceptAndAnswer(_)
        | RouterScript::RefuseHello
        | RouterScript::RefuseVersion => "9.9.9".to_string(),
    };
    let mut router_connection = router_listener.accept().expect("accept the caller");
    let mut router_handshake = RouterHandshake::from_connection_token(router_connection_token);
    let hello_request: RouterRequest = router_connection.recv().expect("read the hello");
    let router_request: RouterRequest = router_connection.recv().expect("read the request");

    let hello_result = match &router_script {
        RouterScript::RefuseVersion => RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: VERSION_REFUSAL_SENTENCE.to_string(),
        }),
        RouterScript::AcceptAndAnswer(_)
        | RouterScript::AcceptAndAnswerAs(..)
        | RouterScript::RefuseHello => {
            match router_handshake.validate_request_kind(&hello_request.request_kind) {
                Ok(()) => RouterResult::Hello {
                    protocol_version: ROUTER_PROTOCOL_VERSION,
                    build_version: reported_version,
                },
                Err(refusal) => RouterResult::Error(refusal),
            }
        }
    };
    router_connection
        .send(&RouterResponse {
            request_id: Some(hello_request.request_id),
            answer_result: hello_result,
        })
        .expect("send the hello reply");

    match (
        router_handshake.validate_request_kind(&router_request.request_kind),
        router_script,
    ) {
        (
            Ok(()),
            RouterScript::AcceptAndAnswer(router_result)
            | RouterScript::AcceptAndAnswerAs(_, router_result),
        ) => router_connection
            .send(&RouterResponse {
                request_id: Some(router_request.request_id),
                answer_result: router_result,
            })
            .expect("send the request reply"),
        (Ok(()), RouterScript::RefuseHello | RouterScript::RefuseVersion) => {
            panic!("a refused hello leaves the handshake closed")
        }
        (Err(_), RouterScript::RefuseVersion) => {}
        // A caller that stops at the refused Hello has already hung up.
        // This send's result is dropped.
        (Err(refusal), _) => {
            let _ = router_connection.send(&RouterResponse {
                request_id: Some(router_request.request_id),
                answer_result: RouterResult::Error(refusal),
            });
        }
    }
    close_connection_after_peer_hangs_up(router_connection);
    router_request.request_kind
}

#[test]
fn an_answer_comes_back_exactly_as_the_router_sent_it() {
    let runtime_directory = build_test_runtime_directory();
    let sent_session_address = SessionAddress {
        session_id: SessionId::new(),
        session_name: "S-quiet-lake".to_string(),
        socket_address: "/nowhere.sock".to_string(),
        process_id: 4321,
    };
    let router = spawn_fake_router(
        runtime_directory.path(),
        vec![RouterScript::AcceptAndAnswer(RouterResult::Found(
            sent_session_address.clone(),
        ))],
    );

    let router_result = submit_router_request(
        runtime_directory.path(),
        RouterRequestKind::AttachLookup {
            session_selector: SessionSelector::SessionName("S-quiet-lake".to_string()),
        },
    )
    .expect("the exchange succeeds");

    assert_eq!(router_result, RouterResult::Found(sent_session_address));
    router.join().expect("the stand-in router exits");
}

#[test]
fn an_endpoint_file_carrying_the_wrong_token_reports_the_refusal() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(runtime_directory.path(), vec![RouterScript::RefuseHello]);

    let router_request_error =
        submit_router_request(runtime_directory.path(), RouterRequestKind::RemoteStatus)
            .expect_err("the hello is refused");

    let CliError::IpcUnavailable {
        detail: error_detail,
    } = router_request_error
    else {
        panic!("expected IpcUnavailable, got {router_request_error:?}");
    };
    assert_eq!(
        error_detail,
        "the token presented does not match the router's"
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_restart_with_no_router_running_restarts_nothing() {
    let runtime_directory = build_test_runtime_directory();

    let restarted = restart_running_router(runtime_directory.path())
        .expect("an empty runtime directory answers");

    assert!(!restarted);
    assert!(!resolve_router_endpoint_path(runtime_directory.path()).exists());
}

#[test]
fn a_restarting_reply_reports_the_router_restarted() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        vec![RouterScript::AcceptAndAnswer(RouterResult::Restarting)],
    );

    let restarted =
        restart_running_router(runtime_directory.path()).expect("the exchange succeeds");

    assert!(restarted);
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_reply_that_answers_no_restart_is_reported_as_unexpected() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        vec![RouterScript::AcceptAndAnswer(RouterResult::Tokens(
            Vec::new(),
        ))],
    );

    let router_restart_error =
        restart_running_router(runtime_directory.path()).expect_err("the reply answers no restart");

    let CliError::IpcUnavailable {
        detail: error_detail,
    } = router_restart_error
    else {
        panic!("expected IpcUnavailable, got {router_restart_error:?}");
    };
    assert_eq!(
        error_detail,
        "the router answered with an unexpected Tokens reply"
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_refused_restart_reports_the_reason_the_router_gave() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        vec![RouterScript::AcceptAndAnswer(RouterResult::Error(
            IpcErrorPayload {
                code: IpcErrorCode::UnsupportedKind,
                message: "this build has no request kind named Restart".to_string(),
            },
        ))],
    );

    let router_restart_error =
        restart_running_router(runtime_directory.path()).expect_err("the restart is refused");

    let CliError::IpcUnavailable {
        detail: error_detail,
    } = router_restart_error
    else {
        panic!("expected IpcUnavailable, got {router_restart_error:?}");
    };
    // The stand-in router reports build 9.9.9. The reason it gave is followed
    // by the two builds and what ends the mismatch.
    assert_eq!(
        error_detail,
        format!(
            "this build has no request kind named Restart — the running router is koshi 9.9.9 \
             and this command is koshi {}; run: koshi restart-servers",
            env!("CARGO_PKG_VERSION")
        )
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_on_this_build_has_its_unknown_kind_refusal_left_alone() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        vec![RouterScript::AcceptAndAnswerAs(
            env!("CARGO_PKG_VERSION").to_string(),
            RouterResult::Error(IpcErrorPayload {
                code: IpcErrorCode::UnsupportedKind,
                message: "this build has no request kind named Restart".to_string(),
            }),
        )],
    );

    let router_restart_error =
        restart_running_router(runtime_directory.path()).expect_err("the restart is refused");

    let CliError::IpcUnavailable {
        detail: error_detail,
    } = router_restart_error
    else {
        panic!("expected IpcUnavailable, got {router_restart_error:?}");
    };
    assert_eq!(error_detail, "this build has no request kind named Restart");
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_refusal_that_is_not_an_unknown_kind_is_left_as_the_router_wrote_it() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        vec![RouterScript::AcceptAndAnswer(RouterResult::Error(
            IpcErrorPayload {
                code: IpcErrorCode::RequestFailed,
                message: "the session name is not one this router knows".to_string(),
            },
        ))],
    );

    let router_restart_error =
        restart_running_router(runtime_directory.path()).expect_err("the restart is refused");

    let CliError::IpcUnavailable {
        detail: error_detail,
    } = router_restart_error
    else {
        panic!("expected IpcUnavailable, got {router_restart_error:?}");
    };
    // The stand-in router reports build 9.9.9, and this refusal still reads as
    // the router wrote it: only an unknown request kind names the two builds.
    assert_eq!(
        error_detail,
        "the session name is not one this router knows"
    );
    router.join().expect("the stand-in router exits");
}

// --- Counting the connections from another machine --------------------------

/// A remote-status answer reporting `remote_connection_count`, with
/// `0.0.0.0:7654`, remote access on and listening, and a fixed fingerprint.
fn build_remote_status_result(remote_connection_count: usize) -> RouterResult {
    RouterResult::RemoteStatus {
        remote_listen_address: Some(std::net::SocketAddr::from(([0, 0, 0, 0], 7654))),
        is_remote_access_enabled: true,
        is_listening: true,
        certificate_fingerprint: Some("aa".repeat(32)),
        remote_connection_count,
    }
}

#[test]
fn the_count_of_connections_from_another_machine_comes_back_as_the_router_sent_it() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        vec![RouterScript::AcceptAndAnswer(build_remote_status_result(3))],
    );

    assert_eq!(
        query_running_router_remote_connections(runtime_directory.path()),
        RemoteConnections::Answered(3)
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_holding_no_such_connection_answers_a_count_of_zero() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        vec![RouterScript::AcceptAndAnswer(build_remote_status_result(0))],
    );

    assert_eq!(
        query_running_router_remote_connections(runtime_directory.path()),
        RemoteConnections::Answered(0)
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn no_running_router_reports_nothing_running_rather_than_a_count() {
    let runtime_directory = build_test_runtime_directory();

    assert_eq!(
        query_running_router_remote_connections(runtime_directory.path()),
        RemoteConnections::NotRunning
    );
    assert!(!resolve_router_endpoint_path(runtime_directory.path()).exists());
}

#[test]
fn a_router_with_no_such_request_kind_reads_as_an_older_build() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        vec![RouterScript::AcceptAndAnswer(RouterResult::Error(
            IpcErrorPayload {
                code: IpcErrorCode::UnsupportedKind,
                message: "this build has no request kind named RemoteStatus".to_string(),
            },
        ))],
    );

    assert_eq!(
        query_running_router_remote_connections(runtime_directory.path()),
        RemoteConnections::OlderBuild
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn any_other_refusal_of_the_count_carries_the_sentence_the_router_gave() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        vec![RouterScript::AcceptAndAnswer(RouterResult::Error(
            IpcErrorPayload {
                code: IpcErrorCode::MalformedRequest,
                message: "the bytes received are not a request this build can read".to_string(),
            },
        ))],
    );

    assert_eq!(
        query_running_router_remote_connections(runtime_directory.path()),
        RemoteConnections::NoAnswer {
            error_detail: "the bytes received are not a request this build can read".to_string(),
            router_process_id: Some(std::process::id()),
        }
    );
    router.join().expect("the stand-in router exits");
}

/// A refusal goes straight to the terminal, so the characters a terminal acts
/// on are removed before any caller reports it.
#[test]
fn a_refusal_loses_what_a_terminal_would_act_on() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        vec![RouterScript::AcceptAndAnswer(RouterResult::Error(
            IpcErrorPayload {
                code: IpcErrorCode::MalformedRequest,
                message: "\u{1b}[2Jthe bytes\u{7f} are not a request".to_string(),
            },
        ))],
    );

    assert_eq!(
        query_running_router_remote_connections(runtime_directory.path()),
        RemoteConnections::NoAnswer {
            error_detail: "[2Jthe bytes are not a request".to_string(),
            router_process_id: Some(std::process::id()),
        }
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_reply_that_answers_no_count_is_reported_as_unexpected() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        vec![RouterScript::AcceptAndAnswer(RouterResult::Tokens(
            Vec::new(),
        ))],
    );

    assert_eq!(
        query_running_router_remote_connections(runtime_directory.path()),
        RemoteConnections::NoAnswer {
            error_detail: "IPC unavailable: the router answered with an unexpected Tokens reply"
                .to_string(),
            router_process_id: Some(std::process::id()),
        }
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_that_hangs_up_and_leaves_no_endpoint_file_reports_no_process_id() {
    let runtime_directory = build_test_runtime_directory();
    let router_socket_address = compute_router_socket_address(runtime_directory.path());
    let router_listener = Listener::bind(&router_socket_address).expect("bind the stand-in router");
    let router_endpoint_path = resolve_router_endpoint_path(runtime_directory.path());
    EndpointFile {
        socket_address: router_socket_address,
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&router_endpoint_path)
    .expect("write the router endpoint file");
    let router_thread = std::thread::spawn(move || {
        let mut router_connection = router_listener.accept().expect("accept the caller");
        let _hello_request: RouterRequest = router_connection.recv().expect("read the hello");
        let _router_request: RouterRequest = router_connection.recv().expect("read the request");
        std::fs::remove_file(&router_endpoint_path).expect("remove the router endpoint file");
    });

    let remote_connections = query_running_router_remote_connections(runtime_directory.path());

    assert_eq!(
        remote_connections,
        RemoteConnections::NoAnswer {
            error_detail: build_ipc_unavailable_error(IpcError::Disconnected).to_string(),
            router_process_id: None,
        }
    );
    router_thread.join().expect("the stand-in router exits");
}

/// The Hello answer a stand-in router sends: this build's control-plane
/// version, and `reported_version` as the build it reports.
fn build_hello_result(reported_version: &str) -> RouterResult {
    RouterResult::Hello {
        protocol_version: ROUTER_PROTOCOL_VERSION,
        build_version: reported_version.to_string(),
    }
}

/// Serve one Hello-only connection as a router would: bind the router's
/// address, write the endpoint file advertising it, accept one caller, and
/// answer its Hello with `hello_result`. A Hello carrying the wrong token is
/// answered with the handshake's own refusal instead. The thread ends when the
/// caller hangs up.
fn spawn_fake_router_for_hello(
    runtime_directory: &Path,
    hello_result: RouterResult,
) -> JoinHandle<()> {
    let router_connection_token = ConnectionToken::generate();
    let router_socket_address = compute_router_socket_address(runtime_directory);
    let router_listener = Listener::bind(&router_socket_address).expect("bind the stand-in router");
    EndpointFile {
        socket_address: router_socket_address,
        connection_token: router_connection_token.clone(),
        process_id: std::process::id(),
    }
    .write_to_path(&resolve_router_endpoint_path(runtime_directory))
    .expect("write the router endpoint file");

    std::thread::spawn(move || {
        let mut router_connection = router_listener.accept().expect("accept the caller");
        let mut router_handshake = RouterHandshake::from_connection_token(router_connection_token);
        let hello_request: RouterRequest = router_connection.recv().expect("read the hello");
        let hello_answer_result =
            match router_handshake.validate_request_kind(&hello_request.request_kind) {
                Ok(()) => hello_result,
                Err(refusal) => RouterResult::Error(refusal),
            };
        router_connection
            .send(&RouterResponse {
                request_id: Some(hello_request.request_id),
                answer_result: hello_answer_result,
            })
            .expect("send the hello reply");
        close_connection_after_peer_hangs_up(router_connection);
    })
}

#[test]
fn the_running_routers_version_is_read_from_its_hello() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router_for_hello(runtime_directory.path(), build_hello_result("9.9.9"));

    let router_version =
        find_running_router_version(runtime_directory.path()).expect("the exchange succeeds");

    assert_eq!(router_version, Some("9.9.9".to_string()));
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_hello_with_an_empty_build_version_reports_an_empty_version() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router_for_hello(runtime_directory.path(), build_hello_result(""));

    let router_version =
        find_running_router_version(runtime_directory.path()).expect("the exchange succeeds");

    assert_eq!(router_version, Some(String::new()));
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_settling_outside_the_control_plane_range_stops_the_exchange() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router_for_hello(
        runtime_directory.path(),
        RouterResult::Hello {
            protocol_version: 4,
            build_version: "9.9.9".to_string(),
        },
    );

    let router_version_error =
        find_running_router_version(runtime_directory.path()).expect_err("4 is outside the 3 to 3");

    let CliError::IpcUnavailable {
        detail: error_detail,
    } = router_version_error
    else {
        panic!("expected IpcUnavailable, got {router_version_error:?}");
    };
    assert_eq!(
        error_detail,
        "the router settled on control-plane protocol version 4, which is outside the 3 to 3 \
         this koshi asked for"
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_refusing_the_hello_reports_the_sentence_it_sent() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router_for_hello(
        runtime_directory.path(),
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match the router's".to_string(),
        }),
    );

    let router_version_error = find_running_router_version(runtime_directory.path())
        .expect_err("a refused hello opens nothing");

    let CliError::IpcUnavailable {
        detail: error_detail,
    } = router_version_error
    else {
        panic!("expected IpcUnavailable, got {router_version_error:?}");
    };
    assert_eq!(
        error_detail,
        "the token presented does not match the router's"
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_answering_no_hello_at_all_reports_the_reply_that_arrived() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router_for_hello(runtime_directory.path(), RouterResult::Restarting);

    let router_version_error = find_running_router_version(runtime_directory.path())
        .expect_err("a Restarting is not a Hello");

    let CliError::IpcUnavailable {
        detail: error_detail,
    } = router_version_error
    else {
        panic!("expected IpcUnavailable, got {router_version_error:?}");
    };
    assert_eq!(
        error_detail,
        "the router answered with an unexpected Restarting reply"
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn no_running_router_yields_no_version() {
    let runtime_directory = build_test_runtime_directory();
    assert_eq!(
        find_running_router_version(runtime_directory.path())
            .expect("a missing router is not an error"),
        None
    );
}

// --- Making a session -------------------------------------------------------

/// The create request [`request_new_session`] sends for `profile_name` and
/// `is_other_user_access_allowed`, from this test process's directory.
fn build_expected_create_session_request(
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

#[test]
fn a_create_sends_this_directory_and_hands_back_the_id_the_router_made() {
    let runtime_directory = build_test_runtime_directory();
    let created_session_id = SessionId::new();
    let router = spawn_fake_router(
        runtime_directory.path(),
        vec![RouterScript::AcceptAndAnswer(RouterResult::Created(
            SessionAddress {
                session_id: created_session_id,
                session_name: "S-quiet-lake".to_string(),
                socket_address: "/nowhere.sock".to_string(),
                process_id: 4321,
            },
        ))],
    );

    let returned_session_id =
        request_new_session(runtime_directory.path(), None, None).expect("the create succeeds");

    assert_eq!(returned_session_id, created_session_id);
    assert_eq!(
        router.join().expect("the stand-in router exits"),
        build_expected_create_session_request(None, None)
    );
}

#[test]
fn a_refused_create_sends_its_profile_and_other_users_answer_and_reports_the_reason() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        vec![RouterScript::AcceptAndAnswer(RouterResult::Error(
            IpcErrorPayload {
                code: IpcErrorCode::RequestFailed,
                message: "the profile named is not one this koshi.kdl declares".to_string(),
            },
        ))],
    );

    let session_creation_error =
        request_new_session(runtime_directory.path(), Some("desk"), Some(true))
            .expect_err("the create is refused");

    let CliError::IpcUnavailable {
        detail: error_detail,
    } = session_creation_error
    else {
        panic!("expected IpcUnavailable, got {session_creation_error:?}");
    };
    assert_eq!(
        error_detail,
        "the profile named is not one this koshi.kdl declares"
    );
    assert_eq!(
        router.join().expect("the stand-in router exits"),
        build_expected_create_session_request(Some("desk"), Some(true))
    );
}

#[test]
fn a_reply_that_creates_nothing_names_what_the_router_answered() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        vec![RouterScript::AcceptAndAnswer(RouterResult::Tokens(
            Vec::new(),
        ))],
    );

    let session_creation_error = request_new_session(runtime_directory.path(), None, None)
        .expect_err("the reply creates no session");

    let CliError::IpcUnavailable {
        detail: error_detail,
    } = session_creation_error
    else {
        panic!("expected IpcUnavailable, got {session_creation_error:?}");
    };
    assert_eq!(
        error_detail,
        "the router answered with an unexpected Tokens reply"
    );
    assert_eq!(
        router.join().expect("the stand-in router exits"),
        build_expected_create_session_request(None, None)
    );
}

/// The token a test's caller connected under, which the wait watches for a
/// change.
const OLD_CONNECTION_TOKEN: &str = "the token this caller connected under";

/// The token the image replacing the server mints when it binds again.
const NEW_CONNECTION_TOKEN: &str = "the token the new image minted";

#[test]
fn wait_for_router_restart_ends_on_a_router_advertising_another_token() {
    let runtime_directory = build_test_runtime_directory();
    write_router_endpoint_file(runtime_directory.path(), NEW_CONNECTION_TOKEN);

    assert!(wait_for_router_restart(
        runtime_directory.path(),
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        Instant::now(),
    ));
}

#[test]
fn wait_for_router_restart_ends_on_a_router_file_this_build_cannot_read() {
    let runtime_directory = build_test_runtime_directory();
    let router_endpoint_path = resolve_router_endpoint_path(runtime_directory.path());
    std::fs::write(&router_endpoint_path, b"{\"router_socket\": 7}")
        .expect("write a router file of another build");

    assert!(wait_for_router_restart(
        runtime_directory.path(),
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        Instant::now(),
    ));
}

#[test]
fn wait_for_router_restart_gives_up_at_the_deadline_on_the_same_token_or_no_file() {
    let runtime_directory = build_test_runtime_directory();

    assert!(!wait_for_router_restart(
        runtime_directory.path(),
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        Instant::now(),
    ));
    write_router_endpoint_file(runtime_directory.path(), OLD_CONNECTION_TOKEN);
    assert!(!wait_for_router_restart(
        runtime_directory.path(),
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        Instant::now(),
    ));
}

#[test]
fn wait_for_router_restart_outlasts_its_deadline_while_an_update_holds_the_lock() {
    // The update lock is held and the deadline has passed. The router comes
    // back under a new token 200 ms after the wait starts.
    let runtime_directory = build_test_runtime_directory();
    write_router_endpoint_file(runtime_directory.path(), OLD_CONNECTION_TOKEN);
    let update_lock_file = hold_update_lock(runtime_directory.path());
    let wait_started_at = Instant::now();
    let restarting_router_directory = runtime_directory.path().to_path_buf();
    let restarting_router = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        write_router_endpoint_file(&restarting_router_directory, NEW_CONNECTION_TOKEN);
    });

    let has_router_restarted = wait_for_router_restart(
        runtime_directory.path(),
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        Instant::now(),
    );
    let wait_duration = wait_started_at.elapsed();

    restarting_router
        .join()
        .expect("the router restart thread ends");
    drop(update_lock_file);
    assert!(has_router_restarted);
    assert!(wait_duration >= Duration::from_millis(200));
}

#[test]
fn wait_for_router_restart_gives_up_past_its_deadline_once_the_update_releases_the_lock() {
    // The deadline has passed and the router keeps its token. The update lock
    // is released 200 ms after the wait starts.
    let runtime_directory = build_test_runtime_directory();
    write_router_endpoint_file(runtime_directory.path(), OLD_CONNECTION_TOKEN);
    let update_lock_file = hold_update_lock(runtime_directory.path());
    let wait_started_at = Instant::now();
    let finishing_update = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        drop(update_lock_file);
    });

    let has_router_restarted = wait_for_router_restart(
        runtime_directory.path(),
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        Instant::now(),
    );
    let wait_duration = wait_started_at.elapsed();

    finishing_update.join().expect("the update thread ends");
    assert!(!has_router_restarted);
    assert!(wait_duration >= Duration::from_millis(200));
}

/// The sentence a stand-in router refuses this build's protocol version with.
const VERSION_REFUSAL_SENTENCE: &str =
    "this router speaks protocol 3 to 4; the caller asked for 5 to 6";

#[test]
fn a_router_refusing_this_builds_protocol_version_is_asked_again_once_it_restarts() {
    let runtime_directory = build_test_runtime_directory();
    let sent_session_address = SessionAddress {
        session_id: SessionId::new(),
        session_name: "S-quiet-lake".to_string(),
        socket_address: "/nowhere.sock".to_string(),
        process_id: 4321,
    };
    let router = spawn_fake_router(
        runtime_directory.path(),
        vec![
            RouterScript::RefuseVersion,
            RouterScript::AcceptAndAnswer(RouterResult::Found(sent_session_address.clone())),
        ],
    );

    let router_result = submit_router_request(
        runtime_directory.path(),
        RouterRequestKind::AttachLookup {
            session_selector: SessionSelector::SessionName("S-quiet-lake".to_string()),
        },
    )
    .expect("the restarted router answers");

    assert_eq!(router_result, RouterResult::Found(sent_session_address));
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_lookup_refused_while_the_router_restarts_is_asked_again_of_the_restarted_router() {
    let runtime_directory = build_test_runtime_directory();
    let sent_session_address = SessionAddress {
        session_id: SessionId::new(),
        session_name: "S-quiet-lake".to_string(),
        socket_address: "/nowhere.sock".to_string(),
        process_id: 5000,
    };
    let router = spawn_fake_router(
        runtime_directory.path(),
        vec![
            RouterScript::AcceptAndAnswer(RouterResult::Error(IpcErrorPayload {
                code: IpcErrorCode::RequestFailed,
                message: ROUTER_RESTARTING_MESSAGE.to_string(),
            })),
            RouterScript::AcceptAndAnswer(RouterResult::Found(sent_session_address.clone())),
        ],
    );

    let router_result = submit_router_request(
        runtime_directory.path(),
        RouterRequestKind::AttachLookup {
            session_selector: SessionSelector::SessionName("S-quiet-lake".to_string()),
        },
    )
    .expect("the restarted router answers");

    assert_eq!(router_result, RouterResult::Found(sent_session_address));
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_refusing_this_builds_protocol_version_that_wrote_no_program_file_names_restart_servers()
{
    let runtime_directory = build_test_runtime_directory();
    let refusing_router_thread =
        spawn_fake_router(runtime_directory.path(), vec![RouterScript::RefuseVersion]);

    let router_answer =
        submit_router_request(runtime_directory.path(), RouterRequestKind::RemoteStatus);

    let Err(CliError::ProtocolVersionRefused { detail }) = router_answer else {
        panic!("expected ProtocolVersionRefused, got {router_answer:?}");
    };
    assert_eq!(
        detail,
        format!(
            "{VERSION_REFUSAL_SENTENCE}; it runs a koshi older than {} that cannot restart into \
             it; run: koshi restart-servers",
            env!("CARGO_PKG_VERSION")
        )
    );
    refusing_router_thread
        .join()
        .expect("the refusing router exits");
}

#[test]
fn a_newer_router_refusing_this_builds_protocol_version_is_named_at_once() {
    let runtime_directory = build_test_runtime_directory();
    let refusing_router_thread =
        spawn_fake_router(runtime_directory.path(), vec![RouterScript::RefuseVersion]);
    ServerProgramFile {
        process_id: std::process::id(),
        build_version: "999.0.0".to_string(),
        program_path: "/opt/koshi/999.0.0/koshi".to_string(),
    }
    .write_to_path(&resolve_router_program_file_path(runtime_directory.path()))
    .expect("the program file is written");
    let request_started_at = Instant::now();

    let router_answer =
        submit_router_request(runtime_directory.path(), RouterRequestKind::RemoteStatus);

    let Err(CliError::ProtocolVersionRefused { detail }) = router_answer else {
        panic!("expected ProtocolVersionRefused, got {router_answer:?}");
    };
    assert_eq!(
        detail,
        format!(
            "{VERSION_REFUSAL_SENTENCE}; it runs koshi 999.0.0 from /opt/koshi/999.0.0/koshi, \
             which is newer than this koshi {}; use /opt/koshi/999.0.0/koshi for it",
            env!("CARGO_PKG_VERSION")
        )
    );
    assert!(request_started_at.elapsed() < Duration::from_secs(1));
    refusing_router_thread
        .join()
        .expect("the refusing router exits");
}
