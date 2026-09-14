//! Tests for the client side of the router socket, against a stand-in router
//! serving a real socket in a temporary runtime directory.
//!
//! Every test that starts the stand-in finds it already listening, so the
//! exchange succeeds on its first attempt and no router is ever started.
//! Starting one is covered by the integration tests.

use super::*;
use koshi_ipc::router::ROUTER_PROTOCOL_VERSION;

use std::thread::JoinHandle;
use std::time::UNIX_EPOCH;

use koshi_core::discovery::SessionDiscovery;
use koshi_core::ids::{ClientId, SessionId};
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcErrorPayload};
use koshi_ipc::router::{
    compute_router_socket_address, resolve_router_endpoint_path, RouterHandshake, RouterResponse,
    SessionAddress,
};
use koshi_ipc::transport::Listener;
use koshi_test_support::fixtures::build_test_runtime_directory;

/// How the stand-in router answers the caller.
enum RouterScript {
    /// The endpoint file carries the router's own token, so the Hello opens
    /// the connection and the request behind it is answered with this result.
    AcceptAndAnswer(RouterResult),
    /// The same, and the Hello reports the build named here. An empty string
    /// is what a router that predates the build field answers.
    AcceptAndAnswerAs(String, RouterResult),
    /// The endpoint file carries a token the router does not hold, so the
    /// Hello is refused and the request behind it is refused too.
    RefuseHello,
}

/// Serve one connection as a router would: bind the router's address, write
/// the endpoint file advertising it, then accept one caller and answer the
/// Hello and the request pipelined behind it per `router_script`.
///
/// The bind and the endpoint file are both done before this returns, so a
/// caller that runs next finds the stand-in ready.
fn spawn_fake_router(runtime_directory: &Path, router_script: RouterScript) -> JoinHandle<()> {
    let router_connection_token = ConnectionToken::generate();
    let advertised_connection_token = match router_script {
        RouterScript::AcceptAndAnswer(_) | RouterScript::AcceptAndAnswerAs(..) => {
            router_connection_token.clone()
        }
        RouterScript::RefuseHello => ConnectionToken::generate(),
    };
    let reported_version = match &router_script {
        RouterScript::AcceptAndAnswerAs(reported_version, _) => reported_version.clone(),
        RouterScript::AcceptAndAnswer(_) | RouterScript::RefuseHello => "9.9.9".to_string(),
    };
    let router_socket_address = compute_router_socket_address(runtime_directory);
    let router_listener = Listener::bind(&router_socket_address).expect("bind the stand-in router");
    EndpointFile {
        socket_address: router_socket_address,
        connection_token: advertised_connection_token,
        process_id: std::process::id(),
    }
    .write_to_path(&resolve_router_endpoint_path(runtime_directory))
    .expect("write the router endpoint file");

    std::thread::spawn(move || {
        let mut router_connection = router_listener.accept().expect("accept the caller");
        let mut router_handshake = RouterHandshake::from_connection_token(router_connection_token);
        let hello_request: RouterRequest = router_connection.recv().expect("read the hello");
        let router_request: RouterRequest = router_connection.recv().expect("read the request");

        let hello_result = match router_handshake.validate_request_kind(&hello_request.request_kind)
        {
            Ok(()) => RouterResult::Hello {
                protocol_version: ROUTER_PROTOCOL_VERSION,
                build_version: reported_version,
            },
            Err(refusal) => RouterResult::Error(refusal),
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
            (Ok(()), RouterScript::RefuseHello) => {
                panic!("a refused hello leaves the handshake closed")
            }
            // A caller that stops at the refused Hello has already hung up.
            // This send's result is dropped.
            (Err(refusal), _) => {
                let _ = router_connection.send(&RouterResponse {
                    request_id: Some(router_request.request_id),
                    answer_result: RouterResult::Error(refusal),
                });
            }
        }
    })
}

#[test]
fn a_listing_comes_back_exactly_as_the_router_sent_it() {
    let runtime_directory = build_test_runtime_directory();
    let sent_discoveries = vec![
        SessionDiscovery {
            session_id: SessionId::new(),
            session_name: "S-quiet-lake".to_string(),
            created_at: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            attached_client_ids: vec![ClientId::new()],
            pane_count: 3,
        },
        SessionDiscovery {
            session_id: SessionId::new(),
            session_name: "S-loud-river".to_string(),
            created_at: UNIX_EPOCH,
            attached_client_ids: Vec::new(),
            pane_count: 1,
        },
    ];
    let router = spawn_fake_router(
        runtime_directory.path(),
        RouterScript::AcceptAndAnswer(RouterResult::Sessions(sent_discoveries.clone())),
    );

    let router_result =
        submit_router_request(runtime_directory.path(), RouterRequestKind::ListSessions)
            .expect("the exchange succeeds");

    assert_eq!(router_result, RouterResult::Sessions(sent_discoveries));
    router.join().expect("the stand-in router exits");
}

#[test]
fn an_endpoint_file_carrying_the_wrong_token_reports_the_refusal() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(runtime_directory.path(), RouterScript::RefuseHello);

    let router_request_error =
        submit_router_request(runtime_directory.path(), RouterRequestKind::ListSessions)
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
        RouterScript::AcceptAndAnswer(RouterResult::Restarting),
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
        RouterScript::AcceptAndAnswer(RouterResult::Sessions(Vec::new())),
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
        "the router answered with an unexpected Sessions reply"
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_refused_restart_reports_the_reason_the_router_gave() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        RouterScript::AcceptAndAnswer(RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: "this build has no request kind named Restart".to_string(),
        })),
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
             and this command is koshi {}; the router serves its own build until it restarts, \
             which it does once no session is left running",
            env!("CARGO_PKG_VERSION")
        )
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_that_reports_no_build_is_named_as_an_older_koshi() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        RouterScript::AcceptAndAnswerAs(
            String::new(),
            RouterResult::Error(IpcErrorPayload {
                code: IpcErrorCode::UnsupportedKind,
                message: "this build has no request kind named Restart".to_string(),
            }),
        ),
    );

    let router_restart_error =
        restart_running_router(runtime_directory.path()).expect_err("the restart is refused");

    let CliError::IpcUnavailable {
        detail: error_detail,
    } = router_restart_error
    else {
        panic!("expected IpcUnavailable, got {router_restart_error:?}");
    };
    assert_eq!(
        error_detail,
        format!(
            "this build has no request kind named Restart — the running router is an older koshi \
             that does not report its build and this command is koshi {}; the router serves its \
             own build until it restarts, which it does once no session is left running",
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
        RouterScript::AcceptAndAnswerAs(
            env!("CARGO_PKG_VERSION").to_string(),
            RouterResult::Error(IpcErrorPayload {
                code: IpcErrorCode::UnsupportedKind,
                message: "this build has no request kind named Restart".to_string(),
            }),
        ),
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
        RouterScript::AcceptAndAnswer(RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the session name is not one this router knows".to_string(),
        })),
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

/// A remote-status answer reporting `remote_connections`, with the rest of the
/// answer fixed so only the count varies between tests.
fn build_remote_status_result(remote_connections: Option<usize>) -> RouterResult {
    RouterResult::RemoteStatus {
        remote_listen_address: Some("0.0.0.0:7654".to_string()),
        is_remote_access_enabled: true,
        is_listening: true,
        certificate_fingerprint: Some("aa".repeat(32)),
        remote_connection_count: remote_connections,
    }
}

#[test]
fn the_count_of_connections_from_another_machine_comes_back_as_the_router_sent_it() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        RouterScript::AcceptAndAnswer(build_remote_status_result(Some(3))),
    );

    assert_eq!(
        query_running_router_remote_connections(runtime_directory.path()),
        RemoteConnections::Answered(Some(3))
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_holding_no_such_connection_answers_a_count_of_zero() {
    // A count of zero and a build reporting no count at all are different
    // answers: one says none are held, the other says nothing.
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        RouterScript::AcceptAndAnswer(build_remote_status_result(Some(0))),
    );

    assert_eq!(
        query_running_router_remote_connections(runtime_directory.path()),
        RemoteConnections::Answered(Some(0))
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_whose_build_reports_no_count_answers_no_count() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        RouterScript::AcceptAndAnswer(build_remote_status_result(None)),
    );

    assert_eq!(
        query_running_router_remote_connections(runtime_directory.path()),
        RemoteConnections::Answered(None)
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
        RouterScript::AcceptAndAnswer(RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: "this build has no request kind named RemoteStatus".to_string(),
        })),
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
        RouterScript::AcceptAndAnswer(RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the bytes received are not a request this build can read".to_string(),
        })),
    );

    assert_eq!(
        query_running_router_remote_connections(runtime_directory.path()),
        RemoteConnections::NoAnswer {
            error_detail: "the bytes received are not a request this build can read".to_string(),
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
        RouterScript::AcceptAndAnswer(RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "\u{1b}[2Jthe bytes\u{7f} are not a request".to_string(),
        })),
    );

    assert_eq!(
        query_running_router_remote_connections(runtime_directory.path()),
        RemoteConnections::NoAnswer {
            error_detail: "[2Jthe bytes are not a request".to_string(),
        }
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_reply_that_answers_no_count_is_reported_as_unexpected() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        RouterScript::AcceptAndAnswer(RouterResult::Sessions(Vec::new())),
    );

    assert_eq!(
        query_running_router_remote_connections(runtime_directory.path()),
        RemoteConnections::NoAnswer {
            error_detail: "IPC unavailable: the router answered with an unexpected Sessions reply"
                .to_string(),
        }
    );
    router.join().expect("the stand-in router exits");
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
    })
}

#[test]
fn the_running_routers_version_is_read_from_its_hello() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router_for_hello(runtime_directory.path(), build_hello_result("9.9.9"));

    let router_version =
        get_running_router_version(runtime_directory.path()).expect("the exchange succeeds");

    assert_eq!(router_version, Some("9.9.9".to_string()));
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_predating_the_build_field_reports_an_empty_version() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router_for_hello(runtime_directory.path(), build_hello_result(""));

    let router_version =
        get_running_router_version(runtime_directory.path()).expect("the exchange succeeds");

    assert_eq!(router_version, Some(String::new()));
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_settling_outside_the_control_plane_range_stops_the_exchange() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router_for_hello(
        runtime_directory.path(),
        RouterResult::Hello {
            protocol_version: 3,
            build_version: "9.9.9".to_string(),
        },
    );

    let router_version_error =
        get_running_router_version(runtime_directory.path()).expect_err("3 is outside the 1 to 2");

    let CliError::IpcUnavailable {
        detail: error_detail,
    } = router_version_error
    else {
        panic!("expected IpcUnavailable, got {router_version_error:?}");
    };
    assert_eq!(
        error_detail,
        "the router settled on control-plane protocol version 3, which is outside the 1 to 2 \
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

    let router_version_error = get_running_router_version(runtime_directory.path())
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

    let router_version_error = get_running_router_version(runtime_directory.path())
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
        get_running_router_version(runtime_directory.path())
            .expect("a missing router is not an error"),
        None
    );
}

// --- Making a session -------------------------------------------------------

#[test]
fn a_created_session_hands_back_the_id_the_router_made() {
    let runtime_directory = build_test_runtime_directory();
    let created_session_id = SessionId::new();
    let router = spawn_fake_router(
        runtime_directory.path(),
        RouterScript::AcceptAndAnswer(RouterResult::Created(SessionAddress {
            session_id: created_session_id,
            session_name: "S-quiet-lake".to_string(),
            socket_address: "/nowhere.sock".to_string(),
            process_id: 4321,
        })),
    );

    let returned_session_id =
        request_new_session(runtime_directory.path(), None, None).expect("the create succeeds");

    assert_eq!(returned_session_id, created_session_id);
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_refused_create_reports_the_reason_the_router_gave() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        RouterScript::AcceptAndAnswer(RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the profile named is not one this koshi.kdl declares".to_string(),
        })),
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
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_reply_that_creates_nothing_names_what_the_router_answered() {
    let runtime_directory = build_test_runtime_directory();
    let router = spawn_fake_router(
        runtime_directory.path(),
        RouterScript::AcceptAndAnswer(RouterResult::Sessions(Vec::new())),
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
        "the router answered with an unexpected Sessions reply"
    );
    router.join().expect("the stand-in router exits");
}
