//! Tests for the client side of the router socket, against a stand-in router
//! serving a real socket in a temporary runtime directory.
//!
//! Every test that starts the stand-in finds it already listening, so the
//! exchange succeeds on its first attempt and no router is ever started.
//! Starting one needs whole processes, so that is covered by the integration
//! tests instead.

use super::*;

use std::thread::JoinHandle;
use std::time::UNIX_EPOCH;

use koshi_core::discovery::SessionInfo;
use koshi_core::ids::{ClientId, SessionId};
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcErrorPayload};
use koshi_ipc::router::{router_socket_addr, RouterHandshake, RouterResponse};
use koshi_ipc::transport::Listener;
use tempfile::TempDir;

/// A fresh directory to stand in for the runtime dir, under a short base so
/// the Unix socket path stays inside the OS path-length cap. Removed when the
/// test drops it.
fn test_runtime_dir() -> TempDir {
    #[cfg(unix)]
    let base = std::path::PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    TempDir::new_in(base).expect("a temporary runtime directory")
}

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
            // A refused Hello is the caller's cue to stop, so whether it is
            // still reading when this reply lands is a race.
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
    // The stand-in router reports build 9.9.9, so the reason it gave is
    // followed by the two builds and what ends the mismatch.
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

/// Serve one Hello-only connection as a router would: bind the router's
/// address, write the endpoint file advertising it, accept one caller, and
/// answer its Hello with `version`. The thread ends when the caller hangs up.
fn fake_router_hello_only(runtime_dir: &Path, version: &str) -> JoinHandle<()> {
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

    let version = version.to_string();
    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the caller");
        let mut gate = RouterHandshake::new(held);
        let hello: RouterRequest = connection.recv().expect("read the hello");
        let result = match gate.check(&hello.kind) {
            Ok(()) => RouterResult::Hello {
                protocol_version: ROUTER_PROTOCOL_VERSION,
                version,
            },
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
    let router = fake_router_hello_only(runtime_dir.path(), "9.9.9");

    let version = running_router_version(runtime_dir.path()).expect("the exchange succeeds");

    assert_eq!(version, Some("9.9.9".to_string()));
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
