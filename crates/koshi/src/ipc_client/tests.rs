//! Tests for the CLI side of the control socket, against a scripted
//! stand-in session serving a real socket.

use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;

use koshi_core::command::{NewPaneArgs, NewTabArgs, ToggleLockModeArgs};
use koshi_core::geometry::Direction;
use koshi_core::ids::{PaneId, SessionId};
use koshi_ipc::layout::TabLayout;
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode};
use koshi_ipc::transport::Listener;
use koshi_layout::tree::LayoutNode;

use super::*;

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
        socket: None,
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
                send(&mut connection, hello.request_id, IpcResult::Hello);
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
                send(&mut connection, hello.request_id, IpcResult::Hello);
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
        send(&mut connection, hello.request_id, IpcResult::Hello);
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
    let (server, _asked) = fake_layout_session(&runtime_dir, session, IpcResult::Hello);

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

#[test]
fn an_unexpected_layout_answer_is_named_layout() {
    let error = unexpected_reply(&IpcResult::Layout(layout_named(
        "workspace",
        SessionId::new(),
    )));

    assert!(
        matches!(
            &error,
            CliError::IpcUnavailable { detail }
                if detail == "the session answered with an unexpected Layout reply"
        ),
        "expected IpcUnavailable naming Layout, got {error:?}",
    );
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
