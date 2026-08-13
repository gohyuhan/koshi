//! Tests for creating, choosing and ending a running session.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::SystemTime;

use koshi_core::command::CliExitCode;
use koshi_core::discovery::{SessionInfo, SessionOverview};
use koshi_core::event::Event;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::protocol::{
    ConnectionToken, IpcErrorCode, IpcErrorPayload, IpcRequest, IpcRequestKind, IpcResponse,
    IpcResult, PROTOCOL_VERSION,
};
use koshi_ipc::router::{
    router_endpoint_path, router_socket_addr, RouterHandshake, RouterRequest, RouterResponse,
    SessionAddress, ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::transport::{Connection, Listener};
use uuid::Uuid;

use super::*;

/// The answer an accepted session Hello earns.
fn hello_accepted() -> IpcResult {
    IpcResult::Hello {
        protocol_version: PROTOCOL_VERSION,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// The answer an accepted router Hello earns.
fn router_hello_accepted() -> RouterResult {
    RouterResult::Hello {
        protocol_version: ROUTER_PROTOCOL_VERSION,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn overview(name: &str) -> SessionOverview {
    named(SessionId::new(), name)
}

fn named(session_id: SessionId, name: &str) -> SessionOverview {
    SessionOverview {
        session: SessionInfo {
            id: session_id,
            name: name.to_string(),
            created_at: SystemTime::UNIX_EPOCH,
            attached_clients: Vec::new(),
            pane_count: 0,
        },
        tabs: Vec::new(),
        panes: Vec::new(),
        clients: Vec::new(),
    }
}

fn census(sessions: Vec<SessionOverview>) -> Discovered {
    Discovered {
        sessions,
        unasked: 0,
    }
}

fn partial(sessions: Vec<SessionOverview>) -> Discovered {
    Discovered {
        sessions,
        unasked: 1,
    }
}

fn test_runtime_dir(tag: &str) -> PathBuf {
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    let dir = base.join(format!("koshi-kill-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create runtime dir");
    dir
}

fn reply(connection: &mut Connection, request_id: u64, result: IpcResult) {
    connection
        .send(&IpcResponse {
            request_id: Some(request_id),
            result,
        })
        .expect("send scripted reply");
}

fn serve_kill(runtime_dir: &Path, overview: SessionOverview) -> JoinHandle<()> {
    let session_id = overview.session.id;
    let socket = koshi_ipc::endpoint::socket_addr(runtime_dir, session_id);
    let token = ConnectionToken::generate();
    let listener = Listener::bind(&socket).expect("stand-in session binds");
    EndpointFile {
        socket,
        token: token.clone(),
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(runtime_dir, session_id))
    .expect("endpoint file written");

    std::thread::spawn(move || {
        let mut discovery = listener.accept().expect("accept discovery");
        let hello: IpcRequest = discovery.recv().expect("read discovery hello");
        let request: IpcRequest = discovery.recv().expect("read discovery request");
        assert!(matches!(
            &hello.kind,
            IpcRequestKind::Hello {
                token: presented,
                ..
            } if presented == &token
        ));
        assert!(matches!(request.kind, IpcRequestKind::Discovery));
        reply(&mut discovery, hello.request_id, hello_accepted());
        reply(
            &mut discovery,
            request.request_id,
            IpcResult::Overview(overview),
        );
        drop(discovery);

        let mut kill = listener.accept().expect("accept kill command");
        let hello: IpcRequest = kill.recv().expect("read kill hello");
        let request: IpcRequest = kill.recv().expect("read kill request");
        let IpcRequestKind::SubmitCommand(envelope) = request.kind else {
            panic!("expected a submitted command");
        };
        assert_eq!(envelope.command, Command::Quit);
        reply(&mut kill, hello.request_id, hello_accepted());
        reply(
            &mut kill,
            request.request_id,
            IpcResult::CommandResult(CommandResult::Ok {
                command_id: envelope.id,
                emitted_events: vec![Event::Quit],
            }),
        );
    })
}

/// A stand-in session that scripts the kill exchange alone. A discovery
/// request on the first connection fails the scripted thread, so joining it
/// proves the caller asked no session to describe itself.
fn serve_kill_only(runtime_dir: &Path, session_id: SessionId) -> JoinHandle<()> {
    let socket = koshi_ipc::endpoint::socket_addr(runtime_dir, session_id);
    let token = ConnectionToken::generate();
    let listener = Listener::bind(&socket).expect("stand-in session binds");
    EndpointFile {
        socket,
        token: token.clone(),
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(runtime_dir, session_id))
    .expect("endpoint file written");

    std::thread::spawn(move || {
        let mut kill = listener.accept().expect("accept kill command");
        let hello: IpcRequest = kill.recv().expect("read kill hello");
        let request: IpcRequest = kill.recv().expect("read kill request");
        assert!(matches!(
            &hello.kind,
            IpcRequestKind::Hello {
                token: presented,
                ..
            } if presented == &token
        ));
        let IpcRequestKind::SubmitCommand(envelope) = request.kind else {
            panic!("expected a submitted command as the first request");
        };
        assert_eq!(envelope.command, Command::Quit);
        reply(&mut kill, hello.request_id, hello_accepted());
        reply(
            &mut kill,
            request.request_id,
            IpcResult::CommandResult(CommandResult::Ok {
                command_id: envelope.id,
                emitted_events: vec![Event::Quit],
            }),
        );
    })
}

/// What a stand-in router saw on the one connection it served.
#[derive(Default)]
struct RouterLog {
    /// Whether the Hello presented a token the gate accepted.
    hello_ok: bool,
    /// The request pipelined behind the Hello.
    request: Option<RouterRequestKind>,
}

/// The create a caller is expected to put on the wire for `profile` and
/// `allow_other_users`.
fn expected_create(profile: Option<&str>, allow_other_users: Option<bool>) -> RouterRequestKind {
    RouterRequestKind::CreateSession {
        profile: profile.map(str::to_string),
        cwd: Some(std::env::current_dir().expect("this test process has a directory")),
        allow_other_users,
    }
}

/// A stand-in router that accepts one caller's Hello and answers the request
/// pipelined behind it with `answer`. What it saw goes in the returned log for
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
fn serve_router(runtime_dir: &Path, answer: RouterResult) -> Arc<Mutex<RouterLog>> {
    let token = ConnectionToken::generate();
    let addr = router_socket_addr(runtime_dir);
    let listener = Listener::bind(&addr).expect("stand-in router binds");
    EndpointFile {
        socket: addr,
        token: token.clone(),
        pid: std::process::id(),
    }
    .write(&router_endpoint_path(runtime_dir))
    .expect("endpoint file written");

    let log = Arc::new(Mutex::new(RouterLog::default()));
    let recorded = Arc::clone(&log);
    std::thread::spawn(move || {
        let Ok(mut connection) = listener.accept() else {
            return;
        };
        let mut gate = RouterHandshake::new(token);
        let Ok(hello) = connection.recv::<RouterRequest>() else {
            return;
        };
        let Ok(request) = connection.recv::<RouterRequest>() else {
            return;
        };

        {
            let mut seen = recorded.lock().expect("the log outlives every panic");
            seen.hello_ok = gate.check(&hello.kind).is_ok();
            seen.request = Some(request.kind);
        }

        let _ = connection.send(&RouterResponse {
            request_id: Some(hello.request_id),
            result: router_hello_accepted(),
        });
        let _ = connection.send(&RouterResponse {
            request_id: Some(request.request_id),
            result: answer,
        });
    });
    log
}

/// The Hello and the request a stand-in router saw, once its caller has been
/// answered.
fn saw(log: &Arc<Mutex<RouterLog>>) -> (bool, Option<RouterRequestKind>) {
    let seen = log.lock().expect("the log outlives every panic");
    (seen.hello_ok, seen.request.clone())
}

#[test]
fn a_created_answer_hands_back_the_new_session_id() {
    let runtime_dir = test_runtime_dir("headless-created");
    let session_id = SessionId::from_uuid(Uuid::from_u128(7));
    let router = serve_router(
        &runtime_dir,
        RouterResult::Created(SessionAddress {
            id: session_id,
            name: "quiet-lake".to_string(),
            socket: "unused".to_string(),
            pid: std::process::id(),
        }),
    );

    let created =
        request_new_session(&runtime_dir, None, None).expect("the router created a session");

    assert_eq!(created, session_id);
    let (hello_ok, request) = saw(&router);
    assert!(hello_ok, "the hello opens the gate");
    assert_eq!(request, Some(expected_create(None, None)));
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

/// `request_headless_session` forwards to `request_new_session`, so this one request
/// is what either entry point puts on the wire.
#[test]
fn the_headless_wrapper_and_the_plain_create_ask_the_router_the_same_thing() {
    let runtime_dir = test_runtime_dir("create-with-profile");
    let session_id = SessionId::from_uuid(Uuid::from_u128(11));
    let router = serve_router(
        &runtime_dir,
        RouterResult::Created(SessionAddress {
            id: session_id,
            name: "amber-fox".to_string(),
            socket: "unused".to_string(),
            pid: std::process::id(),
        }),
    );

    let created = request_headless_session(&runtime_dir, Some("work"), None)
        .expect("the router created a session");

    assert_eq!(created, session_id);
    let (hello_ok, request) = saw(&router);
    assert!(hello_ok, "the hello opens the gate");
    assert_eq!(request, Some(expected_create(Some("work"), None)));
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

/// The wire is the only place `--allow-other-users` can travel, so a create
/// that does not carry it leaves the new session as private as any other.
#[test]
fn a_headless_create_forcing_the_other_users_on_carries_that_answer_to_the_router() {
    let runtime_dir = test_runtime_dir("create-other-users");
    let session_id = SessionId::from_uuid(Uuid::from_u128(12));
    let router = serve_router(
        &runtime_dir,
        RouterResult::Created(SessionAddress {
            id: session_id,
            name: "amber-fox".to_string(),
            socket: "unused".to_string(),
            pid: std::process::id(),
        }),
    );

    let created = request_headless_session(&runtime_dir, None, Some(true))
        .expect("the router created a session");

    assert_eq!(created, session_id);
    let (hello_ok, request) = saw(&router);
    assert!(hello_ok, "the hello opens the gate");
    assert_eq!(request, Some(expected_create(None, Some(true))));
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_refused_create_reports_the_routers_own_message() {
    let runtime_dir = test_runtime_dir("headless-refused");
    let router = serve_router(
        &runtime_dir,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the session server did not start".to_string(),
        }),
    );

    let error = request_new_session(&runtime_dir, None, None).expect_err("the router refused");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    assert_eq!(detail, "the session server did not start");
    let (hello_ok, request) = saw(&router);
    assert!(hello_ok, "the hello opens the gate");
    assert_eq!(request, Some(expected_create(None, None)));
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn an_answer_to_another_request_names_what_came_back() {
    let runtime_dir = test_runtime_dir("headless-wrong-answer");
    let router = serve_router(&runtime_dir, router_hello_accepted());

    let error =
        request_new_session(&runtime_dir, None, None).expect_err("the answer fits no create");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    assert_eq!(detail, "the router answered a create session with Hello");
    let (hello_ok, request) = saw(&router);
    assert!(hello_ok, "the hello opens the gate");
    assert_eq!(request, Some(expected_create(None, None)));
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_name_selects_its_session() {
    let quiet = overview("quiet-lake");
    let quiet_id = quiet.session.id;
    let found = census(vec![overview("amber-fox"), quiet]);

    assert_eq!(
        select_kill_session(&found, Some("quiet-lake")).expect("name matches"),
        quiet_id
    );
}

#[test]
fn no_name_selects_the_only_running_session() {
    let quiet = overview("quiet-lake");
    let quiet_id = quiet.session.id;

    assert_eq!(
        select_kill_session(&census(vec![quiet]), None).expect("sole session"),
        quiet_id
    );
}

#[test]
fn an_unknown_name_uses_the_session_not_found_exit_code() {
    let error = select_kill_session(&census(vec![overview("quiet-lake")]), Some("missing"))
        .expect_err("name is absent");

    assert!(matches!(
        &error,
        CliError::SessionNotFound { session } if session == "missing"
    ));
    assert_eq!(CliExitCode::from(&error), CliExitCode::SessionNotFound);
}

#[test]
fn no_running_session_uses_the_session_not_found_exit_code() {
    let error = select_kill_session(&census(Vec::new()), None).expect_err("nothing to kill");

    assert!(matches!(error, CliError::NoSessions));
    assert_eq!(CliExitCode::from(&error), CliExitCode::SessionNotFound);
}

#[test]
fn duplicate_names_list_every_session_id() {
    let first = SessionId::from_uuid(Uuid::from_u128(1));
    let second = SessionId::from_uuid(Uuid::from_u128(2));
    let error = select_kill_session(
        &census(vec![
            named(first, "quiet-lake"),
            named(second, "quiet-lake"),
        ]),
        Some("quiet-lake"),
    )
    .expect_err("two sessions share the name");

    let CliError::CommandRejected { reason, help } = error else {
        panic!("expected a rejected command");
    };
    assert_eq!(reason, RejectReason::TargetAmbiguous);
    assert_eq!(
        help,
        Some(format!(
            "several sessions are named `quiet-lake`: {first}, {second}; use the session id"
        ))
    );
}

#[test]
fn several_sessions_need_a_name() {
    let error = select_kill_session(
        &census(vec![overview("quiet-lake"), overview("amber-fox")]),
        None,
    )
    .expect_err("several sessions need a name");

    assert!(matches!(
        error,
        CliError::CommandRejected {
            reason: RejectReason::TargetAmbiguous,
            ..
        }
    ));
}

#[test]
fn an_incomplete_census_cannot_prove_a_name_is_unique() {
    let error = select_kill_session(&partial(vec![overview("quiet-lake")]), Some("quiet-lake"))
        .expect_err("another session may share the name");

    assert!(matches!(error, CliError::IpcUnavailable { .. }));
}

#[test]
fn an_incomplete_census_cannot_apply_the_count_rule() {
    let error = select_kill_session(&partial(vec![overview("quiet-lake")]), None)
        .expect_err("another session may be running");

    assert!(matches!(error, CliError::IpcUnavailable { .. }));
}

#[test]
fn kill_by_name_submits_quit_to_that_session() {
    let runtime_dir = test_runtime_dir("named");
    let quiet = overview("quiet-lake");
    let server = serve_kill(&runtime_dir, quiet);

    let result = kill_session_in(
        &runtime_dir,
        Some(&SessionRef::Name("quiet-lake".to_string())),
    )
    .expect("kill exchange succeeds");

    assert!(matches!(
        result,
        CommandResult::Ok {
            emitted_events,
            ..
        } if emitted_events == vec![Event::Quit]
    ));
    server.join().expect("stand-in session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn kill_without_a_name_submits_quit_to_the_only_session() {
    let runtime_dir = test_runtime_dir("sole");
    let quiet = overview("quiet-lake");
    let server = serve_kill(&runtime_dir, quiet);

    let result = kill_session_in(&runtime_dir, None).expect("kill exchange succeeds");

    assert!(matches!(
        result,
        CommandResult::Ok {
            emitted_events,
            ..
        } if emitted_events == vec![Event::Quit]
    ));
    server.join().expect("stand-in session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn kill_by_id_submits_quit_without_discovery() {
    let runtime_dir = test_runtime_dir("by-id");
    let session_id = SessionId::new();
    let server = serve_kill_only(&runtime_dir, session_id);

    let result = kill_session_in(&runtime_dir, Some(&SessionRef::Id(session_id)))
        .expect("kill exchange succeeds");

    assert!(matches!(
        result,
        CommandResult::Ok {
            emitted_events,
            ..
        } if emitted_events == vec![Event::Quit]
    ));
    server.join().expect("stand-in session saw no discovery");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn kill_by_unknown_id_is_session_not_found() {
    let runtime_dir = test_runtime_dir("unknown-id");
    let session_id = SessionId::new();

    let error = kill_session_in(&runtime_dir, Some(&SessionRef::Id(session_id)))
        .expect_err("nothing advertises that id");

    assert!(matches!(
        &error,
        CliError::SessionNotFound { session } if session == &session_id.to_string()
    ));
    assert_eq!(CliExitCode::from(&error), CliExitCode::SessionNotFound);
    let _ = std::fs::remove_dir_all(&runtime_dir);
}
