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

use koshi_core::discovery::SessionInfo;
use koshi_core::ids::{ClientId, SessionId};
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcErrorPayload};
use koshi_ipc::router::{router_socket_addr, RouterHandshake, RouterResponse, SessionAddress};
use koshi_ipc::transport::Listener;
use koshi_test_support::fixtures::test_runtime_dir;

/// How the stand-in router answers the caller.
enum Script {
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
/// Hello and the request pipelined behind it per `script`.
///
/// The bind and the endpoint file are both done before this returns, so a
/// caller that runs next finds the stand-in ready.
fn fake_router(runtime_dir: &Path, script: Script) -> JoinHandle<()> {
    let held = ConnectionToken::generate();
    let advertised = match script {
        Script::AcceptAndAnswer(_) | Script::AcceptAndAnswerAs(..) => held.clone(),
        Script::RefuseHello => ConnectionToken::generate(),
    };
    let reported_build = match &script {
        Script::AcceptAndAnswerAs(version, _) => version.clone(),
        Script::AcceptAndAnswer(_) | Script::RefuseHello => "9.9.9".to_string(),
    };
    let addr = router_socket_addr(runtime_dir);
    let listener = Listener::bind(&addr).expect("bind the stand-in router");
    EndpointFile {
        socket: addr,
        token: advertised,
        pid: std::process::id(),
    }
    .write(&router_endpoint_path(runtime_dir))
    .expect("write the router endpoint file");

    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the caller");
        let mut gate = RouterHandshake::new(held);
        let hello: RouterRequest = connection.recv().expect("read the hello");
        let request: RouterRequest = connection.recv().expect("read the request");

        let hello_answer = match gate.check(&hello.kind) {
            Ok(()) => RouterResult::Hello {
                protocol_version: ROUTER_PROTOCOL_VERSION,
                version: reported_build,
            },
            Err(refusal) => RouterResult::Error(refusal),
        };
        connection
            .send(&RouterResponse {
                request_id: Some(hello.request_id),
                result: hello_answer,
            })
            .expect("send the hello reply");

        match (gate.check(&request.kind), script) {
            (Ok(()), Script::AcceptAndAnswer(answer) | Script::AcceptAndAnswerAs(_, answer)) => {
                connection
                    .send(&RouterResponse {
                        request_id: Some(request.request_id),
                        result: answer,
                    })
                    .expect("send the request reply")
            }
            (Ok(()), Script::RefuseHello) => panic!("a refused hello leaves the gate closed"),
            // A caller that stops at the refused Hello has already hung up.
            // This send's result is dropped.
            (Err(refusal), _) => {
                let _ = connection.send(&RouterResponse {
                    request_id: Some(request.request_id),
                    result: RouterResult::Error(refusal),
                });
            }
        }
    })
}

#[test]
fn a_listing_comes_back_exactly_as_the_router_sent_it() {
    let runtime_dir = test_runtime_dir();
    let sent = vec![
        SessionInfo {
            id: SessionId::new(),
            name: "S-quiet-lake".to_string(),
            created_at: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            attached_clients: vec![ClientId::new()],
            pane_count: 3,
        },
        SessionInfo {
            id: SessionId::new(),
            name: "S-loud-river".to_string(),
            created_at: UNIX_EPOCH,
            attached_clients: Vec::new(),
            pane_count: 1,
        },
    ];
    let router = fake_router(
        runtime_dir.path(),
        Script::AcceptAndAnswer(RouterResult::Sessions(sent.clone())),
    );

    let answer = router_request(runtime_dir.path(), RouterRequestKind::ListSessions)
        .expect("the exchange succeeds");

    assert_eq!(answer, RouterResult::Sessions(sent));
    router.join().expect("the stand-in router exits");
}

#[test]
fn an_endpoint_file_carrying_the_wrong_token_reports_the_refusal() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router(runtime_dir.path(), Script::RefuseHello);

    let error = router_request(runtime_dir.path(), RouterRequestKind::ListSessions)
        .expect_err("the hello is refused");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    assert_eq!(detail, "the token presented does not match the router's");
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_restart_with_no_router_running_restarts_nothing() {
    let runtime_dir = test_runtime_dir();

    let restarted =
        restart_running_router(runtime_dir.path()).expect("an empty runtime directory answers");

    assert!(!restarted);
    assert!(!router_endpoint_path(runtime_dir.path()).exists());
}

#[test]
fn a_restarting_reply_reports_the_router_restarted() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router(
        runtime_dir.path(),
        Script::AcceptAndAnswer(RouterResult::Restarting),
    );

    let restarted = restart_running_router(runtime_dir.path()).expect("the exchange succeeds");

    assert!(restarted);
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_reply_that_answers_no_restart_is_reported_as_unexpected() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router(
        runtime_dir.path(),
        Script::AcceptAndAnswer(RouterResult::Sessions(Vec::new())),
    );

    let error =
        restart_running_router(runtime_dir.path()).expect_err("the reply answers no restart");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    assert_eq!(
        detail,
        "the router answered with an unexpected Sessions reply"
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_refused_restart_reports_the_reason_the_router_gave() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router(
        runtime_dir.path(),
        Script::AcceptAndAnswer(RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: "this build has no request kind named Restart".to_string(),
        })),
    );

    let error = restart_running_router(runtime_dir.path()).expect_err("the restart is refused");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    // The stand-in router reports build 9.9.9. The reason it gave is followed
    // by the two builds and what ends the mismatch.
    assert_eq!(
        detail,
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
    let runtime_dir = test_runtime_dir();
    let router = fake_router(
        runtime_dir.path(),
        Script::AcceptAndAnswerAs(
            String::new(),
            RouterResult::Error(IpcErrorPayload {
                code: IpcErrorCode::UnsupportedKind,
                message: "this build has no request kind named Restart".to_string(),
            }),
        ),
    );

    let error = restart_running_router(runtime_dir.path()).expect_err("the restart is refused");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    assert_eq!(
        detail,
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
    let runtime_dir = test_runtime_dir();
    let router = fake_router(
        runtime_dir.path(),
        Script::AcceptAndAnswerAs(
            env!("CARGO_PKG_VERSION").to_string(),
            RouterResult::Error(IpcErrorPayload {
                code: IpcErrorCode::UnsupportedKind,
                message: "this build has no request kind named Restart".to_string(),
            }),
        ),
    );

    let error = restart_running_router(runtime_dir.path()).expect_err("the restart is refused");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    assert_eq!(detail, "this build has no request kind named Restart");
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_refusal_that_is_not_an_unknown_kind_is_left_as_the_router_wrote_it() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router(
        runtime_dir.path(),
        Script::AcceptAndAnswer(RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the session name is not one this router knows".to_string(),
        })),
    );

    let error = restart_running_router(runtime_dir.path()).expect_err("the restart is refused");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    // The stand-in router reports build 9.9.9, and this refusal still reads as
    // the router wrote it: only an unknown request kind names the two builds.
    assert_eq!(detail, "the session name is not one this router knows");
    router.join().expect("the stand-in router exits");
}

// --- Counting the connections from another machine --------------------------

/// A remote-status answer reporting `remote_connections`, with the rest of the
/// answer fixed so only the count varies between tests.
fn remote_status(remote_connections: Option<usize>) -> RouterResult {
    RouterResult::RemoteStatus {
        address: Some("0.0.0.0:7654".to_string()),
        enabled: true,
        listening: true,
        fingerprint: Some("aa".repeat(32)),
        remote_connections,
    }
}

#[test]
fn the_count_of_connections_from_another_machine_comes_back_as_the_router_sent_it() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router(
        runtime_dir.path(),
        Script::AcceptAndAnswer(remote_status(Some(3))),
    );

    assert_eq!(
        running_router_remote_connections(runtime_dir.path()),
        RemoteConnections::Answered(Some(3))
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_holding_no_such_connection_answers_a_count_of_zero() {
    // A count of zero and a build reporting no count at all are different
    // answers: one says none are held, the other says nothing.
    let runtime_dir = test_runtime_dir();
    let router = fake_router(
        runtime_dir.path(),
        Script::AcceptAndAnswer(remote_status(Some(0))),
    );

    assert_eq!(
        running_router_remote_connections(runtime_dir.path()),
        RemoteConnections::Answered(Some(0))
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_whose_build_reports_no_count_answers_no_count() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router(
        runtime_dir.path(),
        Script::AcceptAndAnswer(remote_status(None)),
    );

    assert_eq!(
        running_router_remote_connections(runtime_dir.path()),
        RemoteConnections::Answered(None)
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn no_running_router_reports_nothing_running_rather_than_a_count() {
    let runtime_dir = test_runtime_dir();

    assert_eq!(
        running_router_remote_connections(runtime_dir.path()),
        RemoteConnections::NotRunning
    );
    assert!(!router_endpoint_path(runtime_dir.path()).exists());
}

#[test]
fn a_router_with_no_such_request_kind_reads_as_an_older_build() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router(
        runtime_dir.path(),
        Script::AcceptAndAnswer(RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: "this build has no request kind named RemoteStatus".to_string(),
        })),
    );

    assert_eq!(
        running_router_remote_connections(runtime_dir.path()),
        RemoteConnections::OlderBuild
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn any_other_refusal_of_the_count_carries_the_sentence_the_router_gave() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router(
        runtime_dir.path(),
        Script::AcceptAndAnswer(RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the bytes received are not a request this build can read".to_string(),
        })),
    );

    assert_eq!(
        running_router_remote_connections(runtime_dir.path()),
        RemoteConnections::NoAnswer {
            detail: "the bytes received are not a request this build can read".to_string(),
        }
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_reply_that_answers_no_count_is_reported_as_unexpected() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router(
        runtime_dir.path(),
        Script::AcceptAndAnswer(RouterResult::Sessions(Vec::new())),
    );

    assert_eq!(
        running_router_remote_connections(runtime_dir.path()),
        RemoteConnections::NoAnswer {
            detail: "IPC unavailable: the router answered with an unexpected Sessions reply"
                .to_string(),
        }
    );
    router.join().expect("the stand-in router exits");
}

/// The Hello answer a stand-in router sends: this build's control-plane
/// version, and `version` as the build it reports.
fn hello_answer(version: &str) -> RouterResult {
    RouterResult::Hello {
        protocol_version: ROUTER_PROTOCOL_VERSION,
        version: version.to_string(),
    }
}

/// Serve one Hello-only connection as a router would: bind the router's
/// address, write the endpoint file advertising it, accept one caller, and
/// answer its Hello with `answer`. A Hello carrying the wrong token is
/// answered with the handshake's own refusal instead. The thread ends when the
/// caller hangs up.
fn fake_router_hello_only(runtime_dir: &Path, answer: RouterResult) -> JoinHandle<()> {
    let held = ConnectionToken::generate();
    let addr = router_socket_addr(runtime_dir);
    let listener = Listener::bind(&addr).expect("bind the stand-in router");
    EndpointFile {
        socket: addr,
        token: held.clone(),
        pid: std::process::id(),
    }
    .write(&router_endpoint_path(runtime_dir))
    .expect("write the router endpoint file");

    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the caller");
        let mut gate = RouterHandshake::new(held);
        let hello: RouterRequest = connection.recv().expect("read the hello");
        let result = match gate.check(&hello.kind) {
            Ok(()) => answer,
            Err(refusal) => RouterResult::Error(refusal),
        };
        connection
            .send(&RouterResponse {
                request_id: Some(hello.request_id),
                result,
            })
            .expect("send the hello reply");
    })
}

#[test]
fn the_running_routers_version_is_read_from_its_hello() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router_hello_only(runtime_dir.path(), hello_answer("9.9.9"));

    let version = running_router_version(runtime_dir.path()).expect("the exchange succeeds");

    assert_eq!(version, Some("9.9.9".to_string()));
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_predating_the_build_field_reports_an_empty_version() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router_hello_only(runtime_dir.path(), hello_answer(""));

    let version = running_router_version(runtime_dir.path()).expect("the exchange succeeds");

    assert_eq!(version, Some(String::new()));
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_settling_outside_the_control_plane_range_stops_the_exchange() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router_hello_only(
        runtime_dir.path(),
        RouterResult::Hello {
            protocol_version: 3,
            version: "9.9.9".to_string(),
        },
    );

    let error = running_router_version(runtime_dir.path()).expect_err("3 is outside the 1 to 2");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    assert_eq!(
        detail,
        "the router settled on control-plane protocol version 3, which is outside the 1 to 2 \
         this koshi asked for"
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_refusing_the_hello_reports_the_sentence_it_sent() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router_hello_only(
        runtime_dir.path(),
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match the router's".to_string(),
        }),
    );

    let error =
        running_router_version(runtime_dir.path()).expect_err("a refused hello opens nothing");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    assert_eq!(detail, "the token presented does not match the router's");
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_router_answering_no_hello_at_all_reports_the_reply_that_arrived() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router_hello_only(runtime_dir.path(), RouterResult::Restarting);

    let error =
        running_router_version(runtime_dir.path()).expect_err("a Restarting is not a Hello");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    assert_eq!(
        detail,
        "the router answered with an unexpected Restarting reply"
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn no_running_router_yields_no_version() {
    let runtime_dir = test_runtime_dir();
    assert_eq!(
        running_router_version(runtime_dir.path()).expect("a missing router is not an error"),
        None
    );
}

// --- Making a session -------------------------------------------------------

#[test]
fn a_created_session_hands_back_the_id_the_router_made() {
    let runtime_dir = test_runtime_dir();
    let made = SessionId::new();
    let router = fake_router(
        runtime_dir.path(),
        Script::AcceptAndAnswer(RouterResult::Created(SessionAddress {
            id: made,
            name: "S-quiet-lake".to_string(),
            socket: "/nowhere.sock".to_string(),
            pid: 4321,
        })),
    );

    let created = request_new_session(runtime_dir.path(), None, None).expect("the create succeeds");

    assert_eq!(created, made);
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_refused_create_reports_the_reason_the_router_gave() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router(
        runtime_dir.path(),
        Script::AcceptAndAnswer(RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the profile named is not one this koshi.kdl declares".to_string(),
        })),
    );

    let error = request_new_session(runtime_dir.path(), Some("desk"), Some(true))
        .expect_err("the create is refused");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    assert_eq!(
        detail,
        "the profile named is not one this koshi.kdl declares"
    );
    router.join().expect("the stand-in router exits");
}

#[test]
fn a_reply_that_creates_nothing_names_what_the_router_answered() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router(
        runtime_dir.path(),
        Script::AcceptAndAnswer(RouterResult::Sessions(Vec::new())),
    );

    let error = request_new_session(runtime_dir.path(), None, None)
        .expect_err("the reply creates no session");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    assert_eq!(detail, "the router answered a create session with Sessions");
    router.join().expect("the stand-in router exits");
}
