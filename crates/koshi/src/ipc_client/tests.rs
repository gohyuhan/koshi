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

#[test]
fn a_version_inside_the_range_this_build_asked_for_is_accepted() {
    for version in MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION {
        assert!(
            settled_version(version).is_ok(),
            "version {version} is inside this build's range"
        );
    }
}

/// The session picks from the range the Hello named, so anything outside it
/// answers a Hello this build did not send. Reading the rest of the connection
/// at a version neither side agreed on is the failure this prevents.
#[test]
fn a_version_above_the_range_this_build_asked_for_stops_the_exchange() {
    let above = PROTOCOL_VERSION + 1;
    let error = settled_version(above).expect_err("a version above the range is refused");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    assert_eq!(
        detail,
        format!(
            "the session settled on protocol version {above}, which is outside the \
             {MIN_PROTOCOL_VERSION} to {PROTOCOL_VERSION} this koshi asked for"
        )
    );
}

#[test]
fn a_version_below_the_range_this_build_asked_for_stops_the_exchange() {
    let below = MIN_PROTOCOL_VERSION.saturating_sub(1);
    let error = settled_version(below).expect_err("a version below the range is refused");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    assert!(
        detail.contains(&format!("settled on protocol version {below}")),
        "the message names the version the session picked: {detail}"
    );
}
