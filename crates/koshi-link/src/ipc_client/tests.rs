//! Tests for the CLI side of the control socket, against a scripted
//! stand-in session serving a real socket.

use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::UNIX_EPOCH;

use koshi_core::command::{NewPaneArgs, NewTabArgs, ToggleLockModeArgs};
use koshi_core::discovery::SessionInfo;
use koshi_core::geometry::Direction;
use koshi_core::ids::{PaneId, SessionId};
use koshi_ipc::layout::TabLayout;
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcResponse};
use koshi_ipc::transport::Listener;
use koshi_layout::tree::LayoutNode;

use super::*;
use koshi_ipc::protocol::{IpcErrorPayload, PROTOCOL_VERSION};

/// A fresh directory to stand in for the runtime dir, under a short base so
/// the Unix socket path stays inside the OS path-length cap.
fn test_runtime_dir(tag: &str) -> PathBuf {
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    let dir = base.join(format!("koshi-cli-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create runtime dir");
    dir
}

/// A `new-pane` request with nothing chosen: the focused pane splits rightward.
fn new_pane_args() -> NewPaneArgs {
    NewPaneArgs {
        source: None,
        tab: None,
        direction: Direction::Right,
        stacked: false,
        cwd: None,
        command: None,
        client: None,
    }
}

/// The in-session identity a test CLI presents.
fn context(session_id: SessionId) -> InSessionContext {
    InSessionContext {
        session_id,
        client_id: None,
        pane_id: PaneId::new(),
    }
}

/// How the scripted session answers the submitted command.
enum Script {
    /// Answer the Hello, then answer the command with `Ok`.
    AcceptAndApply,
    /// Refuse the Hello with `BadToken` (and the pipelined command with
    /// `HelloRequired`, as a real gate would).
    RefuseHello,
    /// Answer the Hello, then reject the command.
    RejectCommand,
}

/// Serve one scripted connection for `session` at `runtime_dir`: write the
/// endpoint file, accept one caller, and answer per `script`.
fn fake_session(runtime_dir: &Path, session: SessionId, script: Script) -> JoinHandle<()> {
    let addr = koshi_ipc::endpoint::socket_addr(runtime_dir, session);
    let token = ConnectionToken::generate();
    let listener = Listener::bind(&addr).expect("bind fake session");
    EndpointFile {
        socket: addr,
        token: token.clone(),
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(runtime_dir, session))
    .expect("write endpoint file");

    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the CLI");
        let hello: IpcRequest = connection.recv().expect("read hello");
        let submit: IpcRequest = connection.recv().expect("read submit");
        let IpcRequestKind::SubmitCommand(envelope) = &submit.kind else {
            panic!("expected a SubmitCommand after the Hello");
        };
        let IpcRequestKind::Hello {
            token: presented, ..
        } = &hello.kind
        else {
            panic!("expected a Hello first");
        };
        assert_eq!(presented, &token, "the CLI presents the endpoint's token");

        match script {
            Script::AcceptAndApply => {
                send(
                    &mut connection,
                    hello.request_id,
                    IpcResult::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        version: env!("CARGO_PKG_VERSION").to_string(),
                    },
                );
                send(
                    &mut connection,
                    submit.request_id,
                    IpcResult::CommandResult(CommandResult::Ok {
                        command_id: envelope.id,
                        emitted_events: Vec::new(),
                    }),
                );
            }
            Script::RefuseHello => {
                send(
                    &mut connection,
                    hello.request_id,
                    IpcResult::Error(IpcErrorPayload {
                        code: IpcErrorCode::BadToken,
                        message: "the token presented does not match this Koshi's".to_string(),
                    }),
                );
                send_best_effort(
                    &mut connection,
                    submit.request_id,
                    IpcResult::Error(IpcErrorPayload {
                        code: IpcErrorCode::HelloRequired,
                        message: "SubmitCommand arrived before a Hello opened the connection"
                            .to_string(),
                    }),
                );
            }
            Script::RejectCommand => {
                send(
                    &mut connection,
                    hello.request_id,
                    IpcResult::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        version: env!("CARGO_PKG_VERSION").to_string(),
                    },
                );
                send(
                    &mut connection,
                    submit.request_id,
                    IpcResult::CommandResult(CommandResult::Rejected {
                        command_id: envelope.id,
                        reason: koshi_core::event::RejectReason::Unauthorized,
                        help: Some("no client is attached to the session".to_string()),
                    }),
                );
            }
        }
    })
}

/// Answer `request_id` with `result` on `connection`, requiring it to arrive.
fn send(connection: &mut Connection, request_id: u64, result: IpcResult) {
    connection
        .send(&IpcResponse {
            request_id: Some(request_id),
            result,
        })
        .expect("send scripted reply");
}

/// Answer `request_id` with `result`, allowing the client to have hung up.
///
/// A refused hello is the CLI's cue to stop, so it closes the connection
/// without reading the rest of the script. Whether a later reply still lands
/// depends on whether the connection's buffer takes it before that close
/// arrives — a coin flip the CLI's behaviour does not depend on, and one that
/// tore this test down at random on Windows, where connections close faster.
/// Only for replies nothing is left to read.
fn send_best_effort(connection: &mut Connection, request_id: u64, result: IpcResult) {
    let _ = connection.send(&IpcResponse {
        request_id: Some(request_id),
        result,
    });
}

#[test]
fn a_submitted_command_comes_back_applied() {
    let runtime_dir = test_runtime_dir("apply");
    let session = SessionId::new();
    let server = fake_session(&runtime_dir, session, Script::AcceptAndApply);

    let result = submit_via_runtime_dir(
        &runtime_dir,
        &context(session),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect("the exchange succeeds");
    assert!(matches!(result, CommandResult::Ok { .. }));

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_rejected_command_comes_back_with_reason_and_help() {
    let runtime_dir = test_runtime_dir("reject");
    let session = SessionId::new();
    let server = fake_session(&runtime_dir, session, Script::RejectCommand);

    let result = submit_via_runtime_dir(
        &runtime_dir,
        &context(session),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect("the exchange succeeds even when the command is rejected");
    let CommandResult::Rejected { reason, help, .. } = result else {
        panic!("expected the rejection to ride back, got {result:?}");
    };
    assert_eq!(reason, koshi_core::event::RejectReason::Unauthorized);
    assert_eq!(
        help.as_deref(),
        Some("no client is attached to the session"),
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_missing_endpoint_file_reports_the_session_not_running() {
    let runtime_dir = test_runtime_dir("no-endpoint");
    let session = SessionId::new();

    let error = submit_via_runtime_dir(
        &runtime_dir,
        &context(session),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect_err("no endpoint file exists");
    assert!(
        matches!(&error, CliError::SessionNotFound { session: named } if *named == session.to_string()),
        "expected SessionNotFound, got {error:?}",
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn an_endpoint_nothing_listens_behind_reports_the_session_not_running() {
    let runtime_dir = test_runtime_dir("dead-socket");
    let session = SessionId::new();
    EndpointFile {
        socket: koshi_ipc::endpoint::socket_addr(&runtime_dir, session),
        token: ConnectionToken::generate(),
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(&runtime_dir, session))
    .expect("write endpoint file");

    let error = submit_via_runtime_dir(
        &runtime_dir,
        &context(session),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect_err("nothing listens behind the endpoint");
    assert!(
        matches!(&error, CliError::SessionNotFound { session: named } if *named == session.to_string()),
        "expected SessionNotFound, got {error:?}",
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_refused_hello_reports_ipc_unavailable() {
    let runtime_dir = test_runtime_dir("refused");
    let session = SessionId::new();
    let server = fake_session(&runtime_dir, session, Script::RefuseHello);

    let error = submit_via_runtime_dir(
        &runtime_dir,
        &context(session),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect_err("the hello is refused");
    assert!(
        matches!(
            &error,
            CliError::IpcUnavailable { detail } if detail == "the token presented does not match this Koshi's"
        ),
        "expected IpcUnavailable with the refusal message, got {error:?}",
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

// --- Asking a session for its layout ----------------------------------------

/// A stand-in koshi serving one layout exchange at `runtime_dir`: write the
/// endpoint file, accept one caller, answer the Hello, then answer the layout
/// request with `answer`. The returned receiver carries the request the caller
/// actually sent.
fn fake_layout_session(
    runtime_dir: &Path,
    session: SessionId,
    answer: IpcResult,
) -> (JoinHandle<()>, Receiver<IpcRequestKind>) {
    let addr = koshi_ipc::endpoint::socket_addr(runtime_dir, session);
    let listener = Listener::bind(&addr).expect("bind fake session");
    EndpointFile {
        socket: addr,
        token: ConnectionToken::generate(),
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(runtime_dir, session))
    .expect("write endpoint file");

    let (asked_tx, asked_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the CLI");
        let hello: IpcRequest = connection.recv().expect("read hello");
        let query: IpcRequest = connection.recv().expect("read layout request");
        asked_tx.send(query.kind).expect("report what was asked");
        send(
            &mut connection,
            hello.request_id,
            IpcResult::Hello {
                protocol_version: PROTOCOL_VERSION,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        );
        send(&mut connection, query.request_id, answer);
    });
    (handle, asked_rx)
}

/// A layout of one empty session, named so a reply is identifiable.
fn layout_named(name: &str, session: SessionId) -> SessionLayout {
    SessionLayout {
        id: session,
        name: name.to_string(),
        tabs: Vec::new(),
        clients: Vec::new(),
    }
}

/// A layout of one session holding exactly `tab`, which no client views.
fn layout_holding(name: &str, session: SessionId, tab: TabId) -> SessionLayout {
    SessionLayout {
        tabs: vec![TabLayout {
            id: tab,
            name: "editor".to_string(),
            index: 0,
            tree: LayoutNode::Pane(PaneId::new()),
            solved: Vec::new(),
        }],
        ..layout_named(name, session)
    }
}

#[test]
fn fetching_a_layout_returns_it_and_asks_for_the_tab_named() {
    let runtime_dir = test_runtime_dir("layout-one-tab");
    let session = SessionId::new();
    let tab = TabId::new();
    let answer = layout_holding("workspace", session, tab);
    let (server, asked) =
        fake_layout_session(&runtime_dir, session, IpcResult::Layout(answer.clone()));

    let layout = fetch_layout(&runtime_dir, session, Some(tab)).expect("the session answers");

    assert_eq!(layout, answer);
    assert_eq!(
        asked.recv().expect("the session read one request"),
        IpcRequestKind::Layout { tab: Some(tab) },
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn fetching_the_whole_layout_asks_for_no_tab() {
    let runtime_dir = test_runtime_dir("layout-every-tab");
    let session = SessionId::new();
    let (server, asked) = fake_layout_session(
        &runtime_dir,
        session,
        IpcResult::Layout(layout_named("workspace", session)),
    );

    let layout = fetch_layout(&runtime_dir, session, None).expect("the session answers");

    assert_eq!(layout.name, "workspace");
    assert_eq!(
        asked.recv().expect("the session read one request"),
        IpcRequestKind::Layout { tab: None },
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn fetching_recent_events_returns_them_in_the_order_the_session_sent() {
    let runtime_dir = test_runtime_dir("events-round-trip");
    let session = SessionId::new();
    let tab = TabId::new();
    let answer = vec![
        koshi_core::recent_event::record(
            &koshi_core::event::Event::TabCreated(koshi_core::event::TabCreated { tab_id: tab }),
            SystemTime::UNIX_EPOCH,
        ),
        koshi_core::recent_event::record(&koshi_core::event::Event::Quit, SystemTime::UNIX_EPOCH),
    ];
    let (server, asked) = fake_layout_session(
        &runtime_dir,
        session,
        IpcResult::RecentEvents(answer.clone()),
    );

    let events = fetch_recent_events(&runtime_dir, session).expect("the session answers");

    assert_eq!(events, answer);
    assert_eq!(
        asked.recv().expect("the session read one request"),
        IpcRequestKind::RecentEvents,
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_recent_events_request_a_session_has_no_name_for_reports_it_as_too_old() {
    let runtime_dir = test_runtime_dir("events-too-old");
    let session = SessionId::new();
    let (server, _asked) = fake_layout_session(
        &runtime_dir,
        session,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: "this session has no request kind named RecentEvents".to_string(),
        }),
    );

    let error = fetch_recent_events(&runtime_dir, session).expect_err("the request is refused");

    assert_eq!(
        error.to_string(),
        CliError::IpcUnavailable {
            detail: "this session was started by an older koshi that keeps no recent-events \
                     buffer; restart the session to use `debug events`"
                .to_string(),
        }
        .to_string(),
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_recent_events_refusal_that_is_not_about_reading_carries_its_own_message() {
    let runtime_dir = test_runtime_dir("events-refused");
    let session = SessionId::new();
    let (server, _asked) = fake_layout_session(
        &runtime_dir,
        session,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let error = fetch_recent_events(&runtime_dir, session).expect_err("the request is refused");

    assert_eq!(
        error.to_string(),
        CliError::IpcUnavailable {
            detail: "the token presented does not match this Koshi's".to_string(),
        }
        .to_string(),
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn fetching_a_layout_with_no_endpoint_file_reports_the_session_not_running() {
    let runtime_dir = test_runtime_dir("layout-no-endpoint");
    let session = SessionId::new();

    let error = fetch_layout(&runtime_dir, session, None).expect_err("no endpoint file exists");

    assert_eq!(
        error.to_string(),
        CliError::SessionNotFound {
            session: session.to_string(),
        }
        .to_string(),
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_layout_request_a_session_cannot_read_reports_the_session_as_too_old() {
    let runtime_dir = test_runtime_dir("layout-too-old");
    let session = SessionId::new();
    let (server, _asked) = fake_layout_session(
        &runtime_dir,
        session,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the bytes received are not a request this build can read".to_string(),
        }),
    );

    let error = fetch_layout(&runtime_dir, session, None).expect_err("the request is refused");

    assert_eq!(
        error.to_string(),
        CliError::IpcUnavailable {
            detail: "this session was started by an older koshi that cannot report its \
                     layout; restart the session to use `debug dump-layout`, or run \
                     `koshi debug dump-state`, which this session does answer"
                .to_string(),
        }
        .to_string(),
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_layout_refusal_that_is_not_about_reading_carries_its_own_message() {
    let runtime_dir = test_runtime_dir("layout-refused");
    let session = SessionId::new();
    let (server, _asked) = fake_layout_session(
        &runtime_dir,
        session,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let error = fetch_layout(&runtime_dir, session, None).expect_err("the request is refused");

    assert_eq!(
        error.to_string(),
        CliError::IpcUnavailable {
            detail: "the token presented does not match this Koshi's".to_string(),
        }
        .to_string(),
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_layout_for_a_tab_the_session_no_longer_holds_reports_the_tab_missing() {
    // The tab was resolved from a discovery sweep, then closed before the
    // session answered, so the answer describes no tab at all.
    let runtime_dir = test_runtime_dir("layout-tab-gone");
    let session = SessionId::new();
    let tab = TabId::new();
    let (server, _asked) = fake_layout_session(
        &runtime_dir,
        session,
        IpcResult::Layout(layout_named("workspace", session)),
    );

    let error =
        fetch_layout(&runtime_dir, session, Some(tab)).expect_err("the tab is no longer there");

    assert_eq!(
        error.to_string(),
        CliError::CommandRejected {
            reason: RejectReason::TargetNotFound,
            help: Some(format!("no running session has tab {tab}")),
        }
        .to_string(),
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_layout_request_answered_with_another_reply_kind_names_that_kind() {
    let runtime_dir = test_runtime_dir("layout-wrong-kind");
    let session = SessionId::new();
    let (server, _asked) = fake_layout_session(
        &runtime_dir,
        session,
        IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    );

    let error = fetch_layout(&runtime_dir, session, None)
        .expect_err("a Hello does not answer a layout request");

    assert!(
        matches!(
            &error,
            CliError::IpcUnavailable { detail }
                if detail == "the session answered with an unexpected Hello reply"
        ),
        "expected IpcUnavailable naming the reply kind, got {error:?}",
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

// --- Asking a session to restart --------------------------------------------

#[test]
fn a_restarting_reply_reports_the_session_restarting_and_asked_for_a_restart() {
    let runtime_dir = test_runtime_dir("restart-ok");
    let session = SessionId::new();
    let (server, asked) = fake_layout_session(&runtime_dir, session, IpcResult::Restarting);

    assert_eq!(
        restart_running_session(&runtime_dir, session).expect("the exchange succeeds"),
        SessionRestart::Restarting
    );
    assert_eq!(
        asked.recv().expect("the session read one request"),
        IpcRequestKind::Restart,
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_session_whose_build_has_no_restart_request_reads_as_too_old() {
    let runtime_dir = test_runtime_dir("restart-too-old");
    let session = SessionId::new();
    let (server, _asked) = fake_layout_session(
        &runtime_dir,
        session,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: "this build has no request kind named Restart".to_string(),
        }),
    );

    assert_eq!(
        restart_running_session(&runtime_dir, session).expect("a refusal by name is not an error"),
        SessionRestart::TooOld
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_refused_restart_that_is_not_an_unknown_kind_carries_the_sessions_sentence() {
    let runtime_dir = test_runtime_dir("restart-refused");
    let session = SessionId::new();
    let (server, _asked) = fake_layout_session(
        &runtime_dir,
        session,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let error = restart_running_session(&runtime_dir, session).expect_err("the restart is refused");

    assert_eq!(
        error.to_string(),
        "IPC unavailable: the token presented does not match this Koshi's"
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn asking_a_session_with_no_endpoint_file_to_restart_restarts_nothing() {
    let runtime_dir = test_runtime_dir("restart-no-endpoint");
    let session = SessionId::new();

    assert_eq!(
        restart_running_session(&runtime_dir, session).expect("a missing session is not an error"),
        SessionRestart::NotRunning
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn asking_a_session_nothing_listens_behind_to_restart_restarts_nothing() {
    let runtime_dir = test_runtime_dir("restart-dead-socket");
    let session = SessionId::new();
    EndpointFile {
        socket: koshi_ipc::endpoint::socket_addr(&runtime_dir, session),
        token: ConnectionToken::generate(),
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(&runtime_dir, session))
    .expect("write endpoint file");

    assert_eq!(
        restart_running_session(&runtime_dir, session).expect("a dead socket is not an error"),
        SessionRestart::NotRunning
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);
}

// --- Reading a running session's build --------------------------------------

#[test]
fn a_session_with_no_endpoint_file_reports_no_build() {
    let runtime_dir = test_runtime_dir("version-no-endpoint");
    let session = SessionId::new();

    assert_eq!(
        running_session_version(&runtime_dir, session).expect("a missing session is not an error"),
        None
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_session_nothing_listens_behind_reports_no_build() {
    let runtime_dir = test_runtime_dir("version-dead-socket");
    let session = SessionId::new();
    EndpointFile {
        socket: koshi_ipc::endpoint::socket_addr(&runtime_dir, session),
        token: ConnectionToken::generate(),
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(&runtime_dir, session))
    .expect("write endpoint file");

    assert_eq!(
        running_session_version(&runtime_dir, session).expect("a dead socket is not an error"),
        None
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);
}

// --- Which file names a session, and under which suffix ---------------------

#[test]
fn an_endpoint_file_and_a_resume_file_are_told_apart_by_their_suffix() {
    // Both names start `session-<uuid>`, so only the suffix separates the
    // session that advertises a socket from the one that left a resume file.
    let runtime_dir = test_runtime_dir("suffixes");
    let advertised = SessionId::new();
    let resumable = SessionId::new();
    EndpointFile {
        socket: koshi_ipc::endpoint::socket_addr(&runtime_dir, advertised),
        token: ConnectionToken::generate(),
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(&runtime_dir, advertised))
    .expect("write endpoint file");
    std::fs::write(
        runtime_dir.join(format!("{resumable}{RESUME_SUFFIX}")),
        b"{}",
    )
    .expect("write resume file");

    assert_eq!(advertised_sessions(&runtime_dir), vec![advertised]);
    assert_eq!(sessions_with_resume_files(&runtime_dir), vec![resumable]);

    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_file_that_names_no_session_is_passed_over() {
    let runtime_dir = test_runtime_dir("suffixes-junk");
    let advertised = SessionId::new();
    EndpointFile {
        socket: koshi_ipc::endpoint::socket_addr(&runtime_dir, advertised),
        token: ConnectionToken::generate(),
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(&runtime_dir, advertised))
    .expect("write endpoint file");
    std::fs::write(runtime_dir.join("session-not-a-uuid.json"), b"{}").expect("bad uuid");
    std::fs::write(runtime_dir.join("router.json"), b"{}").expect("no session prefix");
    std::fs::write(runtime_dir.join(advertised.to_string()), b"{}").expect("no suffix");

    assert_eq!(advertised_sessions(&runtime_dir), vec![advertised]);
    assert_eq!(
        sessions_with_resume_files(&runtime_dir),
        Vec::<SessionId>::new()
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_runtime_directory_that_cannot_be_read_names_no_session() {
    let runtime_dir = test_runtime_dir("suffixes-absent");
    let absent = runtime_dir.join("absent");

    assert_eq!(advertised_sessions(&absent), Vec::<SessionId>::new());
    assert_eq!(sessions_with_resume_files(&absent), Vec::<SessionId>::new());

    let _ = std::fs::remove_dir_all(&runtime_dir);
}

// --- Sessions other local users started -------------------------------------

/// A session describing itself as `name` and holding nothing.
fn overview_named(name: &str, session: SessionId) -> SessionOverview {
    SessionOverview {
        session: SessionInfo {
            id: session,
            name: name.to_string(),
            created_at: UNIX_EPOCH,
            attached_clients: Vec::new(),
            pane_count: 0,
        },
        tabs: Vec::new(),
        panes: Vec::new(),
        clients: Vec::new(),
    }
}

/// A stand-in koshi another local user started, serving one discovery
/// exchange at `addr`: accept one caller, answer the Hello whatever it
/// presents, and answer the discovery request with `answer`. No endpoint file
/// is written, since that user's own runtime directory is theirs alone. The
/// returned receiver carries the token the caller presented.
fn fake_foreign_session(
    addr: &str,
    answer: SessionOverview,
) -> (JoinHandle<()>, Receiver<ConnectionToken>) {
    let listener = Listener::bind(addr).expect("bind the other user's session");
    let (presented_tx, presented_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the CLI");
        let hello: IpcRequest = connection.recv().expect("read hello");
        let query: IpcRequest = connection.recv().expect("read discovery request");
        let IpcRequestKind::Hello { token, .. } = hello.kind else {
            panic!("expected a Hello first");
        };
        presented_tx
            .send(token)
            .expect("report the token presented");
        send(
            &mut connection,
            hello.request_id,
            IpcResult::Hello {
                protocol_version: PROTOCOL_VERSION,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        );
        send(
            &mut connection,
            query.request_id,
            IpcResult::Overview(answer),
        );
    });
    (handle, presented_rx)
}

#[cfg(unix)]
#[test]
fn the_shared_listing_holds_other_users_sockets_and_not_this_users() {
    use std::os::unix::fs::MetadataExt;

    let runtime_dir = test_runtime_dir("shared-unix");
    let shared = test_runtime_dir("shared-unix-base");
    let own = std::fs::metadata(&runtime_dir)
        .expect("read the runtime directory")
        .uid();
    let theirs_dir = (own + 1).to_string();
    let mine = SessionId::new();
    let theirs = SessionId::new();
    std::fs::create_dir_all(shared.join(own.to_string())).expect("create this user's directory");
    std::fs::create_dir_all(shared.join(&theirs_dir)).expect("create the other user's directory");
    std::fs::write(
        shared.join(own.to_string()).join(format!("{mine}.sock")),
        b"",
    )
    .expect("plant this user's socket");
    std::fs::write(shared.join(&theirs_dir).join(format!("{theirs}.sock")), b"")
        .expect("plant the other user's socket");

    assert_eq!(
        foreign_sessions(&shared, &runtime_dir),
        vec![(
            theirs,
            shared
                .join(&theirs_dir)
                .join(format!("{theirs}.sock"))
                .display()
                .to_string(),
        )],
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);
    let _ = std::fs::remove_dir_all(&shared);
}

#[cfg(unix)]
#[test]
fn a_foreign_socket_reusing_a_local_session_id_is_left_out() {
    // Another local user may name a socket after an id this user already
    // runs; the walk keeps the local session, never the planted one.
    use std::os::unix::fs::MetadataExt;

    let runtime_dir = test_runtime_dir("shared-collide");
    let shared = test_runtime_dir("shared-collide-base");
    let own = std::fs::metadata(&runtime_dir)
        .expect("read the runtime directory")
        .uid();
    let mine = SessionId::new();
    EndpointFile {
        socket: koshi_ipc::endpoint::socket_addr(&runtime_dir, mine),
        token: ConnectionToken::generate(),
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(&runtime_dir, mine))
    .expect("advertise this user's session");
    let theirs_dir = shared.join((own + 1).to_string());
    std::fs::create_dir_all(&theirs_dir).expect("create the other user's directory");
    std::fs::write(theirs_dir.join(format!("{mine}.sock")), b"")
        .expect("plant a socket reusing this user's id");

    assert_eq!(foreign_sessions(&shared, &runtime_dir), Vec::new());

    let _ = std::fs::remove_dir_all(&runtime_dir);
    let _ = std::fs::remove_dir_all(&shared);
}

#[cfg(windows)]
#[test]
fn the_shared_listing_holds_the_markers_this_user_does_not_advertise() {
    // A marker names no user, so the endpoint files in this user's runtime
    // directory are the only record of which sessions are this user's own.
    let runtime_dir = test_runtime_dir("shared-windows");
    let shared = test_runtime_dir("shared-windows-base");
    let mine = SessionId::new();
    let theirs = SessionId::new();
    std::fs::write(shared.join(mine.to_string()), b"").expect("plant this user's marker");
    std::fs::write(shared.join(theirs.to_string()), b"").expect("plant the other user's marker");
    EndpointFile {
        socket: koshi_ipc::endpoint::socket_addr(&runtime_dir, mine),
        token: ConnectionToken::generate(),
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(&runtime_dir, mine))
    .expect("write this user's endpoint file");

    assert_eq!(
        foreign_sessions(&shared, &runtime_dir),
        vec![(theirs, format!("koshi-{theirs}"))],
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);
    let _ = std::fs::remove_dir_all(&shared);
}

#[test]
fn a_shared_directory_that_cannot_be_read_holds_no_session() {
    let runtime_dir = test_runtime_dir("shared-unreadable");

    assert_eq!(
        foreign_sessions(&runtime_dir.join("absent"), &runtime_dir),
        Vec::new(),
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[cfg(unix)]
#[test]
fn an_absent_runtime_dir_skips_no_shared_subdirectory() {
    // A user with no runtime directory holds no session, so every
    // subdirectory of the shared directory — this user's uid included —
    // yields its rows.
    use std::os::unix::fs::MetadataExt;

    let shared = test_runtime_dir("shared-absent-runtime-base");
    let own = std::fs::metadata(&shared)
        .expect("read the shared directory")
        .uid();
    let theirs = SessionId::new();
    let user_dir = shared.join(own.to_string());
    std::fs::create_dir_all(&user_dir).expect("create a user's directory");
    std::fs::write(user_dir.join(format!("{theirs}.sock")), b"").expect("plant a socket");

    assert_eq!(
        foreign_sessions(&shared, &shared.join("no-runtime-dir-here")),
        vec![(
            theirs,
            koshi_ipc::endpoint::shared_socket_addr(&user_dir, theirs)
        )],
    );

    let _ = std::fs::remove_dir_all(&shared);
}

#[cfg(unix)]
#[test]
fn a_runtime_dir_with_an_unreadable_owner_yields_no_foreign_session() {
    // The owner of a runtime directory that cannot be read leaves this
    // user's subdirectory unknown, so the walk yields nothing.
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let shared = test_runtime_dir("shared-owner-unreadable-base");
    let own = std::fs::metadata(&shared)
        .expect("read the shared directory")
        .uid();
    if own == 0 {
        eprintln!(
            "skipped `a_runtime_dir_with_an_unreadable_owner_yields_no_foreign_session`: \
             root reads through a mode-000 directory"
        );
        let _ = std::fs::remove_dir_all(&shared);
        return;
    }
    let theirs = SessionId::new();
    let user_dir = shared.join((own + 1).to_string());
    std::fs::create_dir_all(&user_dir).expect("create the other user's directory");
    std::fs::write(user_dir.join(format!("{theirs}.sock")), b"").expect("plant their socket");

    let parent = test_runtime_dir("shared-owner-unreadable-parent");
    let runtime_dir = parent.join("runtime");
    std::fs::create_dir_all(&runtime_dir).expect("create the runtime directory");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000))
        .expect("make the parent unsearchable");

    assert_eq!(foreign_sessions(&shared, &runtime_dir), Vec::new());

    let _ = std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700));
    let _ = std::fs::remove_dir_all(&parent);
    let _ = std::fs::remove_dir_all(&shared);
}

#[cfg(unix)]
#[test]
fn entries_another_user_planted_that_name_no_session_are_passed_over() {
    // Every local user may create an entry in the shared directory, so the
    // walk meets whatever any of them names. Only a `session-<uuid>.sock`
    // inside a user's own subdirectory is a session.
    use std::os::unix::fs::MetadataExt;

    let runtime_dir = test_runtime_dir("shared-planted");
    let shared = test_runtime_dir("shared-planted-base");
    let own = std::fs::metadata(&runtime_dir)
        .expect("read the runtime directory")
        .uid();
    let theirs_dir = shared.join((own + 1).to_string());
    std::fs::create_dir_all(&theirs_dir).expect("create the other user's directory");
    let theirs = SessionId::new();
    std::fs::write(theirs_dir.join(format!("{theirs}.sock")), b"").expect("plant their socket");
    std::fs::write(theirs_dir.join("session-not-a-uuid.sock"), b"").expect("plant a bad uuid");
    std::fs::write(theirs_dir.join(theirs.to_string()), b"").expect("plant a name with no suffix");
    std::fs::write(theirs_dir.join("README.sock"), b"").expect("plant a name with no prefix");
    std::fs::create_dir_all(theirs_dir.join("nested")).expect("plant a subdirectory");
    std::fs::write(shared.join("loose-file"), b"").expect("plant a file beside the user directory");

    assert_eq!(
        foreign_sessions(&shared, &runtime_dir),
        vec![(
            theirs,
            theirs_dir
                .join(format!("{theirs}.sock"))
                .display()
                .to_string(),
        )],
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);
    let _ = std::fs::remove_dir_all(&shared);
}

#[cfg(windows)]
#[test]
fn markers_another_user_planted_that_name_no_session_are_passed_over() {
    // Every local user may create an entry in the shared directory, so the
    // walk meets whatever any of them names. Only a `session-<uuid>` is a
    // session.
    let runtime_dir = test_runtime_dir("shared-planted-windows");
    let shared = test_runtime_dir("shared-planted-windows-base");
    let theirs = SessionId::new();
    std::fs::write(shared.join(theirs.to_string()), b"").expect("plant their marker");
    std::fs::write(shared.join("session-not-a-uuid"), b"").expect("plant a bad uuid");
    std::fs::write(shared.join("README"), b"").expect("plant a name with no prefix");
    std::fs::create_dir_all(shared.join("nested")).expect("plant a subdirectory");

    assert_eq!(
        foreign_sessions(&shared, &runtime_dir),
        vec![(theirs, format!("koshi-{theirs}"))],
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);
    let _ = std::fs::remove_dir_all(&shared);
}

#[test]
fn a_session_another_user_started_is_asked_with_an_empty_token() {
    // That user's endpoint file is unreadable here, so the session is asked
    // over the address the shared directory named and admits by position.
    let runtime_dir = test_runtime_dir("shared-empty-token");
    let session = SessionId::new();
    let addr = koshi_ipc::endpoint::shared_socket_addr(&runtime_dir, session);
    let answer = overview_named("S-quiet-lake", session);
    let (server, presented) = fake_foreign_session(&addr, answer.clone());

    let overview = fetch_foreign_overview(session, &addr).expect("the session answers");

    assert_eq!(overview, answer);
    assert_eq!(
        presented.recv().expect("the session read one Hello"),
        ConnectionToken::new(""),
    );

    server.join().expect("the other user's session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_shared_advert_nothing_listens_behind_reports_the_session_not_running() {
    // A crashed session leaves its socket or its marker behind; the listing
    // must read that as gone, not as a session that could not answer.
    let runtime_dir = test_runtime_dir("shared-dead");
    let session = SessionId::new();
    let addr = koshi_ipc::endpoint::shared_socket_addr(&runtime_dir, session);

    let error = fetch_foreign_overview(session, &addr).expect_err("nothing listens at the address");

    assert_eq!(
        error.to_string(),
        CliError::SessionNotFound {
            session: session.to_string(),
        }
        .to_string(),
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);
}

// --- Send-time working-directory capture ------------------------------------

#[test]
fn a_pane_creating_command_gets_this_process_directory_at_send_time() {
    let captured = capture_cwd(Command::NewPane(new_pane_args()));
    let Command::NewPane(args) = captured else {
        panic!("the variant must not change");
    };
    assert_eq!(args.cwd, std::env::current_dir().ok());

    let captured = capture_cwd(Command::NewTab(NewTabArgs::default()));
    let Command::NewTab(args) = captured else {
        panic!("the variant must not change");
    };
    assert_eq!(args.cwd, std::env::current_dir().ok());
}

#[test]
fn an_explicit_directory_survives_the_capture() {
    let command = Command::NewPane(NewPaneArgs {
        cwd: Some(PathBuf::from("/explicit")),
        ..new_pane_args()
    });
    let Command::NewPane(args) = capture_cwd(command) else {
        panic!("the variant must not change");
    };
    assert_eq!(args.cwd, Some(PathBuf::from("/explicit")));
}

#[test]
fn a_command_without_a_directory_field_is_untouched() {
    assert_eq!(capture_cwd(Command::Quit), Command::Quit);
    assert_eq!(
        capture_cwd(Command::ToggleLockMode(ToggleLockModeArgs::default())),
        Command::ToggleLockMode(ToggleLockModeArgs::default())
    );
}

// --- Sending a target client only to a session that reads it ----------------

/// When a stand-in session answers the Hello.
#[derive(Clone, Copy)]
enum HelloTiming {
    /// Answer the Hello as soon as it arrives, before reading anything else.
    AtOnce,
    /// Read the request after the Hello first, then answer both in order.
    AfterTheNextRequest,
}

/// A stand-in session at `runtime_dir` that settles on `protocol_version`.
///
/// Answers the Hello per `timing`, then answers a `SubmitCommand` with
/// [`CommandResult::Ok`]. Every request it reads goes down the returned
/// receiver, in arrival order. An at-once answer below the target client
/// protocol reports the Hello and exits without waiting for a request.
fn fake_settled_session(
    runtime_dir: &Path,
    session: SessionId,
    protocol_version: u32,
    timing: HelloTiming,
) -> (JoinHandle<()>, Receiver<IpcRequest>) {
    let addr = koshi_ipc::endpoint::socket_addr(runtime_dir, session);
    let listener = Listener::bind(&addr).expect("bind fake session");
    EndpointFile {
        socket: addr,
        token: ConnectionToken::generate(),
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(runtime_dir, session))
    .expect("write endpoint file");

    let (read_tx, read_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the CLI");
        let hello: IpcRequest = connection.recv().expect("read hello");
        let hello_answer = IpcResult::Hello {
            protocol_version,
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        if matches!(timing, HelloTiming::AtOnce) {
            send(&mut connection, hello.request_id, hello_answer.clone());
        }
        if matches!(timing, HelloTiming::AtOnce)
            && protocol_version < crate::talk::TARGET_CLIENT_PROTOCOL
        {
            read_tx.send(hello).expect("report the Hello");
            return;
        }
        let next: Option<IpcRequest> = connection.recv().ok();
        if matches!(timing, HelloTiming::AfterTheNextRequest) {
            send(&mut connection, hello.request_id, hello_answer);
        }
        read_tx.send(hello).expect("report the hello");
        if let Some(request) = next {
            if let IpcRequestKind::SubmitCommand(envelope) = &request.kind {
                send(
                    &mut connection,
                    request.request_id,
                    IpcResult::CommandResult(CommandResult::Ok {
                        command_id: envelope.id,
                        emitted_events: Vec::new(),
                    }),
                );
            }
            read_tx.send(request).expect("report the request read");
        }
    });
    (handle, read_rx)
}

#[test]
fn a_named_client_refuses_a_session_that_speaks_two() {
    let runtime_dir = test_runtime_dir("client-protocol-two");
    let session = SessionId::new();
    let client = ClientId::new();
    let (server, read) = fake_settled_session(&runtime_dir, session, 2, HelloTiming::AtOnce);

    let error = submit_external_via_runtime_dir(
        &runtime_dir,
        session,
        Some(client),
        Command::TogglePaneFullscreen,
    )
    .expect_err("a session speaking 2 ignores the target client");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    assert_eq!(
        detail,
        "this session speaks protocol 2; --client needs a session started by koshi 0.4.0 or \
         later"
    );

    server.join().expect("fake session exits");
    assert_eq!(
        read.recv().expect("the session read the Hello").kind.name(),
        "Hello",
    );
    assert!(
        read.recv().is_err(),
        "the refusal stand-in reported only the Hello",
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_named_client_reaches_a_session_that_speaks_three() {
    let runtime_dir = test_runtime_dir("client-protocol-three");
    let session = SessionId::new();
    let client = ClientId::new();
    let (server, read) = fake_settled_session(&runtime_dir, session, 3, HelloTiming::AtOnce);

    let result = submit_external_via_runtime_dir(
        &runtime_dir,
        session,
        Some(client),
        Command::TogglePaneFullscreen,
    )
    .expect("a session speaking 3 reads the target client");
    assert!(matches!(result, CommandResult::Ok { .. }));

    server.join().expect("fake session exits");
    assert_eq!(
        read.recv().expect("the session read the Hello").kind.name(),
        "Hello",
    );
    let submitted = read.recv().expect("the session read the SubmitCommand");
    let IpcRequestKind::SubmitCommand(envelope) = submitted.kind else {
        panic!("expected a SubmitCommand after the Hello, got {submitted:?}");
    };
    assert_eq!(
        envelope.source,
        CommandSource::external_cli(Some(session), Some(client)),
    );
    assert_eq!(envelope.command, Command::TogglePaneFullscreen);

    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn no_named_client_still_costs_one_round_trip() {
    let runtime_dir = test_runtime_dir("client-none-pipelined");
    let session = SessionId::new();
    let (server, read) =
        fake_settled_session(&runtime_dir, session, 2, HelloTiming::AfterTheNextRequest);

    let result =
        submit_external_via_runtime_dir(&runtime_dir, session, None, Command::TogglePaneFullscreen)
            .expect("a session speaking 2 answers a command naming no client");
    assert!(matches!(result, CommandResult::Ok { .. }));

    server.join().expect("fake session exits");
    assert_eq!(
        read.recv().expect("the session read the Hello").kind.name(),
        "Hello",
    );
    assert_eq!(
        read.recv()
            .expect("the session read the SubmitCommand")
            .kind
            .name(),
        "SubmitCommand",
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);
}
