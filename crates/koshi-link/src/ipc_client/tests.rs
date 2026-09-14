//! Tests for the CLI side of the control socket, against a scripted
//! stand-in session serving a real socket.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::UNIX_EPOCH;

use koshi_core::command::{NewPaneArgs, NewTabArgs, RunCommandPaneArgs, ToggleLockModeArgs};
use koshi_core::discovery::SessionDiscovery;
use koshi_core::geometry::Direction;
use koshi_core::ids::{PaneId, SessionId};
use koshi_core::process::SpawnSpec;
use koshi_ipc::layout::TabLayout;
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcResponse};
use koshi_ipc::transport::Listener;
use koshi_layout::tree::LayoutNode;

use super::*;
use koshi_ipc::protocol::{IpcErrorPayload, PROTOCOL_VERSION};

/// A fresh directory to stand in for the runtime directory, under a short base so
/// the Unix socket path stays inside the OS path-length cap.
fn build_test_runtime_directory(tag: &str) -> PathBuf {
    #[cfg(unix)]
    let runtime_base_directory = PathBuf::from("/tmp");
    #[cfg(windows)]
    let runtime_base_directory = std::env::temp_dir();
    let runtime_directory =
        runtime_base_directory.join(format!("koshi-cli-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&runtime_directory).expect("create runtime directory");
    runtime_directory
}

/// A `new-pane` request with nothing chosen: the focused pane splits rightward.
fn build_default_new_pane_args() -> NewPaneArgs {
    NewPaneArgs {
        source_pane_id: None,
        tab_id: None,
        direction: Direction::Right,
        should_stack: false,
        working_directory: None,
        spawn_spec: None,
        client_id: None,
    }
}

/// The in-session identity a test CLI presents.
fn build_in_session_context(session_id: SessionId) -> InSessionContext {
    InSessionContext {
        session_id,
        client_id: None,
        pane_id: PaneId::new(),
    }
}

/// How the scripted session answers the submitted command.
enum SessionScript {
    /// Answer the Hello, then answer the command with `Ok`.
    AcceptAndApply,
    /// Refuse the Hello with `BadToken` (and the pipelined command with
    /// `HelloRequired`, as a real gate would).
    RefuseHello,
    /// Answer the Hello, then reject the command.
    RejectCommand,
}

/// Serve one scripted connection for `session_id` at `runtime_directory`: write the
/// endpoint file, accept one caller, and answer per `session_script`. The returned
/// receiver carries the envelope the caller submitted.
fn spawn_fake_session(
    runtime_directory: &Path,
    session_id: SessionId,
    session_script: SessionScript,
) -> (JoinHandle<()>, Receiver<CommandEnvelope>) {
    let session_socket_address =
        koshi_ipc::endpoint::compute_socket_address(runtime_directory, session_id);
    let session_connection_token = ConnectionToken::generate();
    let session_listener = Listener::bind(&session_socket_address).expect("bind fake session");
    EndpointFile {
        socket_address: session_socket_address,
        connection_token: session_connection_token.clone(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("write endpoint file");

    let (submitted_envelope_sender, submitted_envelope_receiver) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut session_connection = session_listener.accept().expect("accept the CLI");
        let hello_request: IpcRequest = session_connection.recv().expect("read hello");
        let submit_request: IpcRequest = session_connection.recv().expect("read submit");
        let IpcRequestKind::SubmitCommand(command_envelope) = &submit_request.request_kind else {
            panic!("expected a SubmitCommand after the Hello");
        };
        let IpcRequestKind::Hello {
            connection_token: presented_token,
            ..
        } = &hello_request.request_kind
        else {
            panic!("expected a Hello first");
        };
        assert_eq!(
            presented_token, &session_connection_token,
            "the CLI presents the endpoint's token"
        );
        submitted_envelope_sender
            .send((**command_envelope).clone())
            .expect("report the envelope submitted");

        match session_script {
            SessionScript::AcceptAndApply => {
                send_ipc_result(
                    &mut session_connection,
                    hello_request.request_id,
                    IpcResult::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        build_version: env!("CARGO_PKG_VERSION").to_string(),
                    },
                );
                send_ipc_result(
                    &mut session_connection,
                    submit_request.request_id,
                    IpcResult::CommandResult(CommandResult::Ok {
                        command_id: command_envelope.command_id,
                        emitted_events: Vec::new(),
                    }),
                );
            }
            SessionScript::RefuseHello => {
                send_ipc_result(
                    &mut session_connection,
                    hello_request.request_id,
                    IpcResult::Error(IpcErrorPayload {
                        code: IpcErrorCode::BadToken,
                        message: "the token presented does not match this Koshi's".to_string(),
                    }),
                );
                send_ipc_result_best_effort(
                    &mut session_connection,
                    submit_request.request_id,
                    IpcResult::Error(IpcErrorPayload {
                        code: IpcErrorCode::HelloRequired,
                        message: "SubmitCommand arrived before a Hello opened the connection"
                            .to_string(),
                    }),
                );
            }
            SessionScript::RejectCommand => {
                send_ipc_result(
                    &mut session_connection,
                    hello_request.request_id,
                    IpcResult::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        build_version: env!("CARGO_PKG_VERSION").to_string(),
                    },
                );
                send_ipc_result(
                    &mut session_connection,
                    submit_request.request_id,
                    IpcResult::CommandResult(CommandResult::Rejected {
                        command_id: command_envelope.command_id,
                        reason: koshi_core::event::RejectReason::Unauthorized,
                        help: Some(
                            "\u{1b}[2Jno client is attached\u{7f} to the session".to_string(),
                        ),
                    }),
                );
            }
        }
    });
    (handle, submitted_envelope_receiver)
}

/// Answer `request_id` with `ipc_result` on `connection`, requiring it to arrive.
fn send_ipc_result(connection: &mut Connection, request_id: u64, ipc_result: IpcResult) {
    connection
        .send(&IpcResponse {
            request_id: Some(request_id),
            answer_result: ipc_result,
        })
        .expect("send scripted reply");
}

/// Answer `request_id` with `ipc_result`, ignoring a send that fails.
///
/// The CLI closes the connection once the Hello is refused, without reading
/// the rest of the script, and whether a later reply still lands in the
/// connection's buffer before that close arrives varies. Only for replies
/// nothing is left to read.
fn send_ipc_result_best_effort(
    connection: &mut Connection,
    request_id: u64,
    ipc_result: IpcResult,
) {
    let _ = connection.send(&IpcResponse {
        request_id: Some(request_id),
        answer_result: ipc_result,
    });
}

#[test]
fn a_submitted_command_comes_back_applied() {
    let runtime_directory = build_test_runtime_directory("apply");
    let session_id = SessionId::new();
    let cli_context = build_in_session_context(session_id);
    let (session_server_thread, submitted_envelopes) = spawn_fake_session(
        &runtime_directory,
        session_id,
        SessionScript::AcceptAndApply,
    );

    let command_result = submit_command_via_runtime_directory(
        &runtime_directory,
        &cli_context,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect("the exchange succeeds");

    session_server_thread.join().expect("fake session exits");
    let command_envelope = submitted_envelopes
        .recv()
        .expect("the session read one command");
    assert_eq!(
        command_result,
        CommandResult::Ok {
            command_id: command_envelope.command_id,
            emitted_events: Vec::new(),
        },
    );
    assert_eq!(
        command_envelope.command,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert_eq!(
        command_envelope.command_source,
        CommandSource::from_in_session_cli(
            session_id,
            None,
            cli_context.pane_id,
            PathBuf::from(koshi_ipc::endpoint::compute_socket_address(
                &runtime_directory,
                session_id
            )),
        ),
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_rejected_command_comes_back_with_reason_and_help() {
    let runtime_directory = build_test_runtime_directory("reject");
    let session_id = SessionId::new();
    let (session_server_thread, _submitted_envelopes) =
        spawn_fake_session(&runtime_directory, session_id, SessionScript::RejectCommand);

    let command_result = submit_command_via_runtime_directory(
        &runtime_directory,
        &build_in_session_context(session_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect("the exchange succeeds even when the command is rejected");
    let CommandResult::Rejected { reason, help, .. } = command_result else {
        panic!("expected the rejection to ride back, got {command_result:?}");
    };
    assert_eq!(reason, koshi_core::event::RejectReason::Unauthorized);
    // The session that answered writes the hint, and a session reached through
    // the shared directory belongs to another user, so the hint is filtered.
    assert_eq!(
        help.as_deref(),
        Some("[2Jno client is attached to the session"),
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_missing_endpoint_file_reports_the_session_not_running() {
    let runtime_directory = build_test_runtime_directory("no-endpoint");
    let session_id = SessionId::new();

    let command_error = submit_command_via_runtime_directory(
        &runtime_directory,
        &build_in_session_context(session_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect_err("no endpoint file exists");
    let CliError::SessionNotFound {
        session_name: found_session_name,
    } = command_error
    else {
        panic!("expected SessionNotFound, got {command_error:?}");
    };
    assert_eq!(found_session_name, session_id.to_string());

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn an_endpoint_nothing_listens_behind_reports_the_session_not_running() {
    let runtime_directory = build_test_runtime_directory("dead-socket");
    let session_id = SessionId::new();
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(&runtime_directory, session_id),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        &runtime_directory,
        session_id,
    ))
    .expect("write endpoint file");

    let command_error = submit_command_via_runtime_directory(
        &runtime_directory,
        &build_in_session_context(session_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect_err("nothing listens behind the endpoint");
    let CliError::SessionNotFound {
        session_name: found_session_name,
    } = command_error
    else {
        panic!("expected SessionNotFound, got {command_error:?}");
    };
    assert_eq!(found_session_name, session_id.to_string());

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn an_endpoint_file_that_holds_no_endpoint_reports_ipc_unavailable() {
    let runtime_directory = build_test_runtime_directory("endpoint-unreadable");
    let session_id = SessionId::new();
    let endpoint_file_path =
        EndpointFile::resolve_endpoint_file_path(&runtime_directory, session_id);
    std::fs::write(&endpoint_file_path, b"not an endpoint file")
        .expect("write the unreadable endpoint file");
    let endpoint_file_error = EndpointFile::load_from_path(&endpoint_file_path)
        .expect_err("the bytes hold no endpoint file");

    let endpoint_error = load_session_endpoint(&runtime_directory, session_id)
        .expect_err("the endpoint file cannot be read");

    let CliError::IpcUnavailable { detail } = endpoint_error else {
        panic!("expected IpcUnavailable, got {endpoint_error:?}");
    };
    assert_eq!(detail, endpoint_file_error.to_string());

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_refused_hello_reports_ipc_unavailable() {
    let runtime_directory = build_test_runtime_directory("refused");
    let session_id = SessionId::new();
    let (session_server_thread, _submitted_envelopes) =
        spawn_fake_session(&runtime_directory, session_id, SessionScript::RefuseHello);

    let command_error = submit_command_via_runtime_directory(
        &runtime_directory,
        &build_in_session_context(session_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect_err("the hello is refused");
    let CliError::IpcUnavailable { detail } = command_error else {
        panic!("expected IpcUnavailable, got {command_error:?}");
    };
    assert_eq!(detail, "the token presented does not match this Koshi's");

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_refused_command_carries_the_sessions_sentence() {
    let runtime_directory = build_test_runtime_directory("submit-refused");
    let session_id = SessionId::new();
    let (server, asked) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::NotFound,
            message: "this session holds no pane by that id".to_string(),
        }),
    );

    let error = submit_command_via_runtime_directory(
        &runtime_directory,
        &build_in_session_context(session_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect_err("the session refuses the command");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    assert_eq!(detail, "this session holds no pane by that id");
    assert_eq!(
        asked
            .recv()
            .expect("the session read one request")
            .get_request_kind_name(),
        "SubmitCommand",
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_command_answered_with_another_reply_kind_names_that_kind() {
    let runtime_directory = build_test_runtime_directory("submit-wrong-kind");
    let session_id = SessionId::new();
    let (server, _asked) =
        spawn_answering_session(&runtime_directory, session_id, IpcResult::Restarting);

    let error = submit_command_via_runtime_directory(
        &runtime_directory,
        &build_in_session_context(session_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect_err("a Restarting does not answer a submitted command");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable naming the reply kind, got {error:?}");
    };
    assert_eq!(
        detail,
        "the session answered with an unexpected Restarting reply",
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

// --- Asking a session for its layout ----------------------------------------

/// A stand-in koshi serving one exchange at `runtime_directory`: write the endpoint
/// file, accept one caller, read the Hello and the request behind it, answer
/// the Hello, then answer that request with `ipc_result`. The returned receiver
/// carries the request the caller actually sent.
fn spawn_answering_session(
    runtime_directory: &Path,
    session_id: SessionId,
    ipc_result: IpcResult,
) -> (JoinHandle<()>, Receiver<IpcRequestKind>) {
    let session_socket_address =
        koshi_ipc::endpoint::compute_socket_address(runtime_directory, session_id);
    let session_listener = Listener::bind(&session_socket_address).expect("bind fake session");
    EndpointFile {
        socket_address: session_socket_address,
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("write endpoint file");

    let (request_kind_sender, request_kind_receiver) = mpsc::channel();
    let session_server_thread = std::thread::spawn(move || {
        let mut session_connection = session_listener.accept().expect("accept the CLI");
        let hello_request: IpcRequest = session_connection.recv().expect("read hello");
        let query_request: IpcRequest = session_connection
            .recv()
            .expect("read the request behind the Hello");
        request_kind_sender
            .send(query_request.request_kind)
            .expect("report what was asked");
        send_ipc_result(
            &mut session_connection,
            hello_request.request_id,
            IpcResult::Hello {
                protocol_version: PROTOCOL_VERSION,
                build_version: env!("CARGO_PKG_VERSION").to_string(),
            },
        );
        send_ipc_result(
            &mut session_connection,
            query_request.request_id,
            ipc_result,
        );
    });
    (session_server_thread, request_kind_receiver)
}

/// A layout of one empty session, named so a reply is identifiable.
fn build_named_session_layout(session_name: &str, session_id: SessionId) -> SessionLayout {
    SessionLayout {
        session_id,
        session_name: session_name.to_string(),
        tabs: Vec::new(),
        clients: Vec::new(),
    }
}

/// A layout of one session holding exactly `tab`, which no client views.
fn build_session_layout_with_tab(
    session_name: &str,
    session_id: SessionId,
    tab_id: TabId,
) -> SessionLayout {
    SessionLayout {
        tabs: vec![TabLayout {
            tab_id,
            tab_name: "editor".to_string(),
            tab_index: 0,
            layout_tree: LayoutNode::Pane(PaneId::new()),
            solved_tabs: Vec::new(),
        }],
        ..build_named_session_layout(session_name, session_id)
    }
}

#[test]
fn fetching_a_layout_returns_it_and_asks_for_the_tab_named() {
    let runtime_directory = build_test_runtime_directory("layout-one-tab");
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let expected_session_layout = build_session_layout_with_tab("workspace", session_id, tab_id);
    let (session_server_thread, request_kinds) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Layout(expected_session_layout.clone()),
    );

    let session_layout =
        fetch_layout(&runtime_directory, session_id, Some(tab_id)).expect("the session answers");

    assert_eq!(session_layout, expected_session_layout);
    assert_eq!(
        request_kinds.recv().expect("the session read one request"),
        IpcRequestKind::Layout {
            tab_id: Some(tab_id)
        },
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn fetching_the_whole_layout_asks_for_no_tab() {
    let runtime_directory = build_test_runtime_directory("layout-every-tab");
    let session_id = SessionId::new();
    let (session_server_thread, request_kinds) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Layout(build_named_session_layout("workspace", session_id)),
    );

    let session_layout =
        fetch_layout(&runtime_directory, session_id, None).expect("the session answers");

    assert_eq!(session_layout.session_name, "workspace");
    assert_eq!(
        request_kinds.recv().expect("the session read one request"),
        IpcRequestKind::Layout { tab_id: None },
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn fetching_recent_events_returns_them_in_the_order_the_session_sent() {
    let runtime_directory = build_test_runtime_directory("events-round-trip");
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let expected_recent_events = vec![
        koshi_core::recent_event::record_event(
            &koshi_core::event::Event::TabCreated(koshi_core::event::TabCreated { tab_id }),
            SystemTime::UNIX_EPOCH,
        ),
        koshi_core::recent_event::record_event(
            &koshi_core::event::Event::Quit,
            SystemTime::UNIX_EPOCH,
        ),
    ];
    let (session_server_thread, request_kinds) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::RecentEvents(expected_recent_events.clone()),
    );

    let recent_events =
        fetch_recent_events(&runtime_directory, session_id).expect("the session answers");

    assert_eq!(recent_events, expected_recent_events);
    assert_eq!(
        request_kinds.recv().expect("the session read one request"),
        IpcRequestKind::RecentEvents,
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_recent_events_request_a_session_has_no_name_for_reports_it_as_too_old() {
    let runtime_directory = build_test_runtime_directory("events-too-old");
    let session_id = SessionId::new();
    let (session_server_thread, _request_kinds) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: "this session has no request kind named RecentEvents".to_string(),
        }),
    );

    let recent_events_error =
        fetch_recent_events(&runtime_directory, session_id).expect_err("the request is refused");

    assert_eq!(
        recent_events_error.to_string(),
        CliError::IpcUnavailable {
            detail: "this session was started by an older koshi that keeps no recent-events \
                     buffer; restart the session to use `debug events`"
                .to_string(),
        }
        .to_string(),
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_recent_events_refusal_that_is_not_about_reading_carries_its_own_message() {
    let runtime_directory = build_test_runtime_directory("events-refused");
    let session_id = SessionId::new();
    let (session_server_thread, _request_kinds) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let recent_events_error =
        fetch_recent_events(&runtime_directory, session_id).expect_err("the request is refused");

    assert_eq!(
        recent_events_error.to_string(),
        CliError::IpcUnavailable {
            detail: "the token presented does not match this Koshi's".to_string(),
        }
        .to_string(),
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn fetching_a_layout_with_no_endpoint_file_reports_the_session_not_running() {
    let runtime_directory = build_test_runtime_directory("layout-no-endpoint");
    let session_id = SessionId::new();

    let error =
        fetch_layout(&runtime_directory, session_id, None).expect_err("no endpoint file exists");

    assert_eq!(
        error.to_string(),
        CliError::SessionNotFound {
            session_name: session_id.to_string(),
        }
        .to_string(),
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_layout_request_a_session_cannot_read_reports_the_session_as_too_old() {
    let runtime_directory = build_test_runtime_directory("layout-too-old");
    let session_id = SessionId::new();
    let (session_server_thread, _request_kinds) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the bytes received are not a request this build can read".to_string(),
        }),
    );

    let error =
        fetch_layout(&runtime_directory, session_id, None).expect_err("the request is refused");

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

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_layout_refusal_that_is_not_about_reading_carries_its_own_message() {
    let runtime_directory = build_test_runtime_directory("layout-refused");
    let session_id = SessionId::new();
    let (session_server_thread, _request_kinds) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let error =
        fetch_layout(&runtime_directory, session_id, None).expect_err("the request is refused");

    assert_eq!(
        error.to_string(),
        CliError::IpcUnavailable {
            detail: "the token presented does not match this Koshi's".to_string(),
        }
        .to_string(),
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_layout_for_a_tab_the_session_no_longer_holds_reports_the_tab_missing() {
    // The tab was resolved from a discovery sweep, then closed before the
    // session answered, so the response describes no tab at all.
    let runtime_directory = build_test_runtime_directory("layout-tab-gone");
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let (session_server_thread, _request_kinds) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Layout(build_named_session_layout("workspace", session_id)),
    );

    let error = fetch_layout(&runtime_directory, session_id, Some(tab_id))
        .expect_err("the tab is no longer there");

    assert_eq!(
        error.to_string(),
        CliError::CommandRejected {
            reason: RejectReason::TargetNotFound,
            help: Some(format!("no running session has tab {tab_id}")),
        }
        .to_string(),
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_layout_request_answered_with_another_reply_kind_names_that_kind() {
    let runtime_directory = build_test_runtime_directory("layout-wrong-kind");
    let session_id = SessionId::new();
    let (session_server_thread, _asked) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            build_version: env!("CARGO_PKG_VERSION").to_string(),
        },
    );

    let error = fetch_layout(&runtime_directory, session_id, None)
        .expect_err("a Hello does not answer a layout request");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable naming the reply kind, got {error:?}");
    };
    assert_eq!(
        detail,
        "the session answered with an unexpected Hello reply"
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

// --- Asking a session to describe itself ------------------------------------

#[test]
fn fetching_an_overview_returns_it_and_asks_for_a_discovery() {
    let runtime_directory = build_test_runtime_directory("overview-round-trip");
    let session_id = SessionId::new();
    let response = build_named_session_overview("S-quiet-lake", session_id);
    let (server, asked) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Overview(response.clone()),
    );

    let overview =
        fetch_session_overview(&runtime_directory, session_id).expect("the session answers");

    assert_eq!(overview, response);
    assert_eq!(
        asked.recv().expect("the session read one request"),
        IpcRequestKind::Discovery,
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_refused_overview_carries_the_sessions_sentence() {
    let runtime_directory = build_test_runtime_directory("overview-refused");
    let session_id = SessionId::new();
    let (session_server_thread, _asked) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let error =
        fetch_session_overview(&runtime_directory, session_id).expect_err("the request is refused");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    assert_eq!(detail, "the token presented does not match this Koshi's");

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn an_overview_request_answered_with_another_reply_kind_names_that_kind() {
    let runtime_directory = build_test_runtime_directory("overview-wrong-kind");
    let session_id = SessionId::new();
    let (server, _asked) =
        spawn_answering_session(&runtime_directory, session_id, IpcResult::Restarting);

    let error = fetch_session_overview(&runtime_directory, session_id)
        .expect_err("a Restarting does not describe a session");

    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable naming the reply kind, got {error:?}");
    };
    assert_eq!(
        detail,
        "the session answered with an unexpected Restarting reply",
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

// --- Asking a session to restart --------------------------------------------

#[test]
fn a_restarting_reply_reports_the_session_restarting_and_asked_for_a_restart() {
    let runtime_directory = build_test_runtime_directory("restart-ok");
    let session_id = SessionId::new();
    let (server, asked) =
        spawn_answering_session(&runtime_directory, session_id, IpcResult::Restarting);

    assert_eq!(
        restart_running_session(&runtime_directory, session_id).expect("the exchange succeeds"),
        SessionRestart::Restarting
    );
    assert_eq!(
        asked.recv().expect("the session read one request"),
        IpcRequestKind::Restart,
    );

    server.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_session_whose_build_has_no_restart_request_reads_as_too_old() {
    let runtime_directory = build_test_runtime_directory("restart-too-old");
    let session_id = SessionId::new();
    let (session_server_thread, _asked) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: "this build has no request kind named Restart".to_string(),
        }),
    );

    assert_eq!(
        restart_running_session(&runtime_directory, session_id)
            .expect("a refusal by name is not an error"),
        SessionRestart::TooOld
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_session_that_cannot_read_the_restart_request_reads_as_too_old() {
    // A session older than the tolerant wire cannot decode the request at all,
    // which says the same thing as naming the kind it lacks.
    let runtime_directory = build_test_runtime_directory("restart-unreadable");
    let session_id = SessionId::new();
    let (session_server_thread, _asked) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the bytes received are not a request this build can read".to_string(),
        }),
    );

    assert_eq!(
        restart_running_session(&runtime_directory, session_id)
            .expect("a session that cannot read the request is not an error"),
        SessionRestart::TooOld
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_refused_restart_that_is_not_an_unknown_kind_carries_the_sessions_sentence() {
    let runtime_directory = build_test_runtime_directory("restart-refused");
    let session_id = SessionId::new();
    let (session_server_thread, _asked) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let restart_error = restart_running_session(&runtime_directory, session_id)
        .expect_err("the restart is refused");

    assert_eq!(
        restart_error.to_string(),
        "IPC unavailable: the token presented does not match this Koshi's"
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_restart_answered_with_another_reply_kind_names_that_kind() {
    let runtime_directory = build_test_runtime_directory("restart-wrong-kind");
    let session_id = SessionId::new();
    let (session_server_thread, _request_kinds) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Overview(build_named_session_overview("workspace", session_id)),
    );

    let restart_error = restart_running_session(&runtime_directory, session_id)
        .expect_err("an Overview does not answer a restart");

    let CliError::IpcUnavailable { detail } = restart_error else {
        panic!("expected IpcUnavailable naming the reply kind, got {restart_error:?}");
    };
    assert_eq!(
        detail,
        "the session answered with an unexpected Overview reply",
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn asking_a_session_with_no_endpoint_file_to_restart_restarts_nothing() {
    let runtime_directory = build_test_runtime_directory("restart-no-endpoint");
    let session_id = SessionId::new();

    assert_eq!(
        restart_running_session(&runtime_directory, session_id)
            .expect("a missing session is not an error"),
        SessionRestart::NotRunning
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn asking_a_session_nothing_listens_behind_to_restart_restarts_nothing() {
    let runtime_directory = build_test_runtime_directory("restart-dead-socket");
    let session_id = SessionId::new();
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(&runtime_directory, session_id),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        &runtime_directory,
        session_id,
    ))
    .expect("write endpoint file");

    assert_eq!(
        restart_running_session(&runtime_directory, session_id)
            .expect("a dead socket is not an error"),
        SessionRestart::NotRunning
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

// --- Reading a running session's build --------------------------------------

#[test]
fn a_running_session_reports_the_build_its_hello_named() {
    let runtime_directory = build_test_runtime_directory("version-hello");
    let session_id = SessionId::new();
    let (session_server_thread, session_requests) = spawn_settled_session(
        &runtime_directory,
        session_id,
        PROTOCOL_VERSION,
        HelloTiming::AtOnce,
    );

    assert_eq!(
        get_running_session_version(&runtime_directory, session_id)
            .expect("the session answers its Hello"),
        Some(env!("CARGO_PKG_VERSION").to_string()),
    );

    session_server_thread.join().expect("fake session exits");
    assert_eq!(
        session_requests
            .recv()
            .expect("the session read the Hello")
            .request_kind
            .get_request_kind_name(),
        "Hello",
    );
    assert_eq!(
        session_requests.recv(),
        Err(mpsc::RecvError),
        "reading the build sends nothing besides the Hello",
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_session_with_no_endpoint_file_reports_no_build() {
    let runtime_directory = build_test_runtime_directory("version-no-endpoint");
    let session_id = SessionId::new();

    assert_eq!(
        get_running_session_version(&runtime_directory, session_id)
            .expect("a missing session is not an error"),
        None
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_session_nothing_listens_behind_reports_no_build() {
    let runtime_directory = build_test_runtime_directory("version-dead-socket");
    let session_id = SessionId::new();
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(&runtime_directory, session_id),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        &runtime_directory,
        session_id,
    ))
    .expect("write endpoint file");

    assert_eq!(
        get_running_session_version(&runtime_directory, session_id)
            .expect("a dead socket is not an error"),
        None
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

// --- Which file names a session, and under which suffix ---------------------

#[test]
fn an_endpoint_file_and_a_resume_file_are_told_apart_by_their_suffix() {
    // Both names start `session-<uuid>`, so only the suffix separates the
    // session that advertises a socket from the one that left a resume file.
    let runtime_directory = build_test_runtime_directory("suffixes");
    let advertised = SessionId::new();
    let resumable = SessionId::new();
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(&runtime_directory, advertised),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        &runtime_directory,
        advertised,
    ))
    .expect("write endpoint file");
    std::fs::write(
        runtime_directory.join(format!("{resumable}{RESUME_SUFFIX}")),
        b"{}",
    )
    .expect("write resume file");

    assert_eq!(
        list_advertised_sessions(&runtime_directory),
        vec![advertised]
    );
    assert_eq!(
        list_sessions_with_resume_files(&runtime_directory),
        vec![resumable]
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_file_that_names_no_session_is_passed_over() {
    let runtime_directory = build_test_runtime_directory("suffixes-junk");
    let advertised = SessionId::new();
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(&runtime_directory, advertised),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        &runtime_directory,
        advertised,
    ))
    .expect("write endpoint file");
    std::fs::write(runtime_directory.join("session-not-a-uuid.json"), b"{}").expect("bad uuid");
    std::fs::write(runtime_directory.join("router.json"), b"{}").expect("no session prefix");
    std::fs::write(runtime_directory.join(advertised.to_string()), b"{}").expect("no suffix");

    assert_eq!(
        list_advertised_sessions(&runtime_directory),
        vec![advertised]
    );
    assert_eq!(
        list_sessions_with_resume_files(&runtime_directory),
        Vec::<SessionId>::new()
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_runtime_directory_that_cannot_be_read_names_no_session() {
    let runtime_directory = build_test_runtime_directory("suffixes-absent");
    let absent = runtime_directory.join("absent");

    assert_eq!(list_advertised_sessions(&absent), Vec::<SessionId>::new());
    assert_eq!(
        list_sessions_with_resume_files(&absent),
        Vec::<SessionId>::new()
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

// --- Sessions other local users started -------------------------------------

/// A session describing itself as `session_name` and holding nothing.
fn build_named_session_overview(session_name: &str, session_id: SessionId) -> SessionOverview {
    SessionOverview {
        session: SessionDiscovery {
            session_id,
            session_name: session_name.to_string(),
            created_at: UNIX_EPOCH,
            attached_client_ids: Vec::new(),
            pane_count: 0,
        },
        tabs: Vec::new(),
        panes: Vec::new(),
        clients: Vec::new(),
    }
}

/// A stand-in koshi another local user started, serving one discovery
/// exchange at `socket_address`: accept one caller, answer the Hello whatever it
/// presents, and answer the discovery request with `session_overview`. No endpoint file
/// is written, since that user's own runtime directory is theirs alone. The
/// returned receiver carries the token the caller presented.
fn spawn_foreign_session(
    socket_address: &str,
    session_overview: SessionOverview,
) -> (JoinHandle<()>, Receiver<ConnectionToken>) {
    let session_listener = Listener::bind(socket_address).expect("bind the other user's session");
    let (presented_token_sender, presented_token_receiver) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut session_connection = session_listener.accept().expect("accept the CLI");
        let hello_request: IpcRequest = session_connection.recv().expect("read hello");
        let discovery_request: IpcRequest =
            session_connection.recv().expect("read discovery request");
        let IpcRequestKind::Hello {
            connection_token: presented_token,
            ..
        } = hello_request.request_kind
        else {
            panic!("expected a Hello first");
        };
        presented_token_sender
            .send(presented_token)
            .expect("report the token presented");
        send_ipc_result(
            &mut session_connection,
            hello_request.request_id,
            IpcResult::Hello {
                protocol_version: PROTOCOL_VERSION,
                build_version: env!("CARGO_PKG_VERSION").to_string(),
            },
        );
        send_ipc_result(
            &mut session_connection,
            discovery_request.request_id,
            IpcResult::Overview(session_overview),
        );
    });
    (handle, presented_token_receiver)
}

#[cfg(unix)]
#[test]
fn the_shared_listing_holds_other_users_sockets_and_not_this_users() {
    use std::os::unix::fs::MetadataExt;

    let runtime_directory = build_test_runtime_directory("shared-unix");
    let shared = build_test_runtime_directory("shared-unix-base");
    let own = std::fs::metadata(&runtime_directory)
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
        list_foreign_sessions(&shared, &runtime_directory),
        vec![(
            theirs,
            shared
                .join(&theirs_dir)
                .join(format!("{theirs}.sock"))
                .display()
                .to_string(),
        )],
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared);
}

#[cfg(unix)]
#[test]
fn a_foreign_socket_reusing_a_local_session_id_is_left_out() {
    // Another local user may name a socket after an id this user already
    // runs; the walk keeps the local session, never the planted one.
    use std::os::unix::fs::MetadataExt;

    let runtime_directory = build_test_runtime_directory("shared-collide");
    let shared = build_test_runtime_directory("shared-collide-base");
    let own = std::fs::metadata(&runtime_directory)
        .expect("read the runtime directory")
        .uid();
    let mine = SessionId::new();
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(&runtime_directory, mine),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        &runtime_directory,
        mine,
    ))
    .expect("advertise this user's session");
    let theirs_dir = shared.join((own + 1).to_string());
    std::fs::create_dir_all(&theirs_dir).expect("create the other user's directory");
    std::fs::write(theirs_dir.join(format!("{mine}.sock")), b"")
        .expect("plant a socket reusing this user's id");

    assert_eq!(
        list_foreign_sessions(&shared, &runtime_directory),
        Vec::new()
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared);
}

#[cfg(windows)]
#[test]
fn the_shared_listing_holds_the_markers_this_user_does_not_advertise() {
    // A marker names no user, so the endpoint files in this user's runtime
    // directory are the only record of which sessions are this user's own.
    let runtime_directory = build_test_runtime_directory("shared-windows");
    let shared_sessions_base_directory = build_test_runtime_directory("shared-windows-base");
    let own_session_id = SessionId::new();
    let foreign_session_id = SessionId::new();
    std::fs::write(
        shared_sessions_base_directory.join(own_session_id.to_string()),
        b"",
    )
    .expect("plant this user's marker");
    std::fs::write(
        shared_sessions_base_directory.join(foreign_session_id.to_string()),
        b"",
    )
    .expect("plant the other user's marker");
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(
            &runtime_directory,
            own_session_id,
        ),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        &runtime_directory,
        own_session_id,
    ))
    .expect("write this user's endpoint file");

    assert_eq!(
        list_foreign_sessions(&shared_sessions_base_directory, &runtime_directory,),
        vec![(foreign_session_id, format!("koshi-{foreign_session_id}"),)],
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[test]
fn a_shared_directory_that_cannot_be_read_holds_no_session() {
    let runtime_directory = build_test_runtime_directory("shared-unreadable");

    assert_eq!(
        list_foreign_sessions(&runtime_directory.join("absent"), &runtime_directory),
        Vec::new(),
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[cfg(unix)]
#[test]
fn an_absent_runtime_directory_skips_no_shared_subdirectory() {
    // A user with no runtime directory holds no session, so every
    // subdirectory of the shared directory — this user's uid included —
    // yields its rows.
    use std::os::unix::fs::MetadataExt;

    let shared_sessions_base_directory = build_test_runtime_directory("shared-absent-runtime-base");
    let own_user_id = std::fs::metadata(&shared_sessions_base_directory)
        .expect("read the shared directory")
        .uid();
    let foreign_session_id = SessionId::new();
    let foreign_user_directory = shared_sessions_base_directory.join(own_user_id.to_string());
    std::fs::create_dir_all(&foreign_user_directory).expect("create a user's directory");
    std::fs::write(
        foreign_user_directory.join(format!("{foreign_session_id}.sock")),
        b"",
    )
    .expect("plant a socket");

    assert_eq!(
        list_foreign_sessions(
            &shared_sessions_base_directory,
            &shared_sessions_base_directory.join("no-runtime-dir-here"),
        ),
        vec![(
            foreign_session_id,
            koshi_ipc::endpoint::compute_shared_socket_address(
                &foreign_user_directory,
                foreign_session_id,
            )
        )],
    );

    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[cfg(unix)]
#[test]
fn a_runtime_directory_with_an_unreadable_owner_yields_no_foreign_session() {
    // The owner of a runtime directory that cannot be read leaves this
    // user's subdirectory unknown, so the walk yields nothing.
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let shared_sessions_base_directory =
        build_test_runtime_directory("shared-owner-unreadable-base");
    let own_user_id = std::fs::metadata(&shared_sessions_base_directory)
        .expect("read the shared directory")
        .uid();
    if own_user_id == 0 {
        eprintln!(
            "skipped `a_runtime_directory_with_an_unreadable_owner_yields_no_foreign_session`: \
             root reads through a mode-000 directory"
        );
        let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
        return;
    }
    let foreign_session_id = SessionId::new();
    let foreign_user_directory = shared_sessions_base_directory.join((own_user_id + 1).to_string());
    std::fs::create_dir_all(&foreign_user_directory).expect("create the other user's directory");
    std::fs::write(
        foreign_user_directory.join(format!("{foreign_session_id}.sock")),
        b"",
    )
    .expect("plant their socket");

    let parent = build_test_runtime_directory("shared-owner-unreadable-parent");
    let runtime_directory = parent.join("runtime");
    std::fs::create_dir_all(&runtime_directory).expect("create the runtime directory");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000))
        .expect("make the parent unsearchable");

    assert_eq!(
        list_foreign_sessions(&shared_sessions_base_directory, &runtime_directory,),
        Vec::new()
    );

    let _ = std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700));
    let _ = std::fs::remove_dir_all(&parent);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[cfg(unix)]
#[test]
fn entries_another_user_planted_that_name_no_session_are_passed_over() {
    // Every local user may create an entry in the shared directory, so the
    // walk meets whatever any of them names. Only a `session-<uuid>.sock`
    // inside a user's own subdirectory is a session.
    use std::os::unix::fs::MetadataExt;

    let runtime_directory = build_test_runtime_directory("shared-planted");
    let shared_sessions_base_directory = build_test_runtime_directory("shared-planted-base");
    let own_user_id = std::fs::metadata(&runtime_directory)
        .expect("read the runtime directory")
        .uid();
    let foreign_user_directory = shared_sessions_base_directory.join((own_user_id + 1).to_string());
    std::fs::create_dir_all(&foreign_user_directory).expect("create the other user's directory");
    let foreign_session_id = SessionId::new();
    std::fs::write(
        foreign_user_directory.join(format!("{foreign_session_id}.sock")),
        b"",
    )
    .expect("plant their socket");
    std::fs::write(foreign_user_directory.join("session-not-a-uuid.sock"), b"")
        .expect("plant a bad uuid");
    std::fs::write(
        foreign_user_directory.join(foreign_session_id.to_string()),
        b"",
    )
    .expect("plant a name with no suffix");
    std::fs::write(foreign_user_directory.join("README.sock"), b"")
        .expect("plant a name with no prefix");
    std::fs::create_dir_all(foreign_user_directory.join("nested")).expect("plant a subdirectory");
    std::fs::write(shared_sessions_base_directory.join("loose-file"), b"")
        .expect("plant a file beside the user directory");

    assert_eq!(
        list_foreign_sessions(&shared_sessions_base_directory, &runtime_directory,),
        vec![(
            foreign_session_id,
            foreign_user_directory
                .join(format!("{foreign_session_id}.sock"))
                .display()
                .to_string(),
        )],
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[cfg(windows)]
#[test]
fn markers_another_user_planted_that_name_no_session_are_passed_over() {
    // Every local user may create an entry in the shared directory, so the
    // walk meets whatever any of them names. Only a `session-<uuid>` is a
    // session.
    let runtime_directory = build_test_runtime_directory("shared-planted-windows");
    let shared_sessions_base_directory =
        build_test_runtime_directory("shared-planted-windows-base");
    let foreign_session_id = SessionId::new();
    std::fs::write(
        shared_sessions_base_directory.join(foreign_session_id.to_string()),
        b"",
    )
    .expect("plant their marker");
    std::fs::write(
        shared_sessions_base_directory.join("session-not-a-uuid"),
        b"",
    )
    .expect("plant a bad uuid");
    std::fs::write(shared_sessions_base_directory.join("README"), b"")
        .expect("plant a name with no prefix");
    std::fs::create_dir_all(shared_sessions_base_directory.join("nested"))
        .expect("plant a subdirectory");

    assert_eq!(
        list_foreign_sessions(&shared_sessions_base_directory, &runtime_directory),
        vec![(foreign_session_id, format!("koshi-{foreign_session_id}"),)],
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[test]
fn a_session_another_user_started_is_asked_with_an_empty_token() {
    // That user's endpoint file is unreadable here, so the session is asked
    // over the address the shared directory named and admits by position.
    let runtime_directory = build_test_runtime_directory("shared-empty-token");
    let session_id = SessionId::new();
    let socket_address =
        koshi_ipc::endpoint::compute_shared_socket_address(&runtime_directory, session_id);
    let expected_session_overview = build_named_session_overview("S-quiet-lake", session_id);
    let (foreign_session_thread, presented_connection_tokens) =
        spawn_foreign_session(&socket_address, expected_session_overview.clone());

    let session_overview =
        fetch_foreign_session_overview(session_id, &socket_address).expect("the session answers");

    assert_eq!(session_overview, expected_session_overview);
    assert_eq!(
        presented_connection_tokens
            .recv()
            .expect("the session read one Hello"),
        ConnectionToken::from_secret(""),
    );

    foreign_session_thread
        .join()
        .expect("the other user's session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_shared_advert_nothing_listens_behind_reports_the_session_not_running() {
    // A crashed session leaves its socket or its marker behind; the listing
    // must read that as gone, not as a session that could not answer.
    let runtime_directory = build_test_runtime_directory("shared-dead");
    let session_id = SessionId::new();
    let socket_address =
        koshi_ipc::endpoint::compute_shared_socket_address(&runtime_directory, session_id);

    let session_lookup_error = fetch_foreign_session_overview(session_id, &socket_address)
        .expect_err("nothing listens at the address");

    assert_eq!(
        session_lookup_error.to_string(),
        CliError::SessionNotFound {
            session_name: session_id.to_string(),
        }
        .to_string(),
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

// --- Send-time working-directory capture ------------------------------------

#[test]
fn a_pane_creating_command_gets_this_process_directory_at_send_time() {
    let captured_command =
        capture_current_working_directory(Command::NewPane(build_default_new_pane_args()));
    let Command::NewPane(command_args) = captured_command else {
        panic!("the variant must not change");
    };
    assert_eq!(command_args.working_directory, std::env::current_dir().ok());

    let captured_command =
        capture_current_working_directory(Command::NewTab(NewTabArgs::default()));
    let Command::NewTab(command_args) = captured_command else {
        panic!("the variant must not change");
    };
    assert_eq!(command_args.working_directory, std::env::current_dir().ok());

    let captured_command =
        capture_current_working_directory(Command::RunCommandPane(RunCommandPaneArgs {
            spawn_spec: SpawnSpec::default_shell(None, BTreeMap::new()),
            working_directory: None,
            source_pane_id: None,
            tab_id: None,
            direction: Direction::Right,
            should_stack: false,
            client_id: None,
        }));
    let Command::RunCommandPane(command_args) = captured_command else {
        panic!("the variant must not change");
    };
    assert_eq!(command_args.working_directory, std::env::current_dir().ok());
}

#[test]
fn an_explicit_directory_survives_the_capture() {
    let command = Command::NewPane(NewPaneArgs {
        working_directory: Some(PathBuf::from("/explicit")),
        ..build_default_new_pane_args()
    });
    let Command::NewPane(command_args) = capture_current_working_directory(command) else {
        panic!("the variant must not change");
    };
    assert_eq!(
        command_args.working_directory,
        Some(PathBuf::from("/explicit"))
    );
}

#[test]
fn a_command_without_a_directory_field_is_untouched() {
    assert_eq!(
        capture_current_working_directory(Command::Quit),
        Command::Quit
    );
    assert_eq!(
        capture_current_working_directory(Command::ToggleLockMode(ToggleLockModeArgs::default())),
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

/// A stand-in session at `runtime_directory` that settles on `protocol_version`.
///
/// Answers the Hello per `timing`, then answers a `SubmitCommand` with
/// [`CommandResult::Ok`]. Every request it reads goes down the returned
/// receiver, in arrival order.
fn spawn_settled_session(
    runtime_directory: &Path,
    session_id: SessionId,
    protocol_version: u32,
    timing: HelloTiming,
) -> (JoinHandle<()>, Receiver<IpcRequest>) {
    let session_socket_address =
        koshi_ipc::endpoint::compute_socket_address(runtime_directory, session_id);
    let session_listener = Listener::bind(&session_socket_address).expect("bind fake session");
    EndpointFile {
        socket_address: session_socket_address,
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("write endpoint file");

    let (request_sender, request_receiver) = mpsc::channel();
    let session_thread = std::thread::spawn(move || {
        let mut session_connection = session_listener.accept().expect("accept the CLI");
        let hello_request: IpcRequest = session_connection.recv().expect("read hello");
        let hello_result = IpcResult::Hello {
            protocol_version,
            build_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        if matches!(timing, HelloTiming::AtOnce) {
            send_ipc_result(
                &mut session_connection,
                hello_request.request_id,
                hello_result.clone(),
            );
        }
        let next_request: Option<IpcRequest> = session_connection.recv().ok();
        if matches!(timing, HelloTiming::AfterTheNextRequest) {
            send_ipc_result(
                &mut session_connection,
                hello_request.request_id,
                hello_result,
            );
        }
        request_sender
            .send(hello_request)
            .expect("report the hello");
        if let Some(next_request) = next_request {
            if let IpcRequestKind::SubmitCommand(command_envelope) = &next_request.request_kind {
                send_ipc_result(
                    &mut session_connection,
                    next_request.request_id,
                    IpcResult::CommandResult(CommandResult::Ok {
                        command_id: command_envelope.command_id,
                        emitted_events: Vec::new(),
                    }),
                );
            }
            request_sender
                .send(next_request)
                .expect("report the request read");
        }
    });
    (session_thread, request_receiver)
}

#[test]
fn a_session_speaking_two_is_refused_before_the_command_is_written() {
    let runtime_directory = build_test_runtime_directory("client-protocol-two");
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let (session_thread, received_requests) =
        spawn_settled_session(&runtime_directory, session_id, 2, HelloTiming::AtOnce);

    let command_error = submit_external_command_via_runtime_directory(
        &runtime_directory,
        session_id,
        Some(client_id),
        Command::TogglePaneFullscreen,
    )
    .expect_err("a session speaking 2 is below this build's floor of 3");

    let CliError::IpcUnavailable { detail } = command_error else {
        panic!("expected IpcUnavailable, got {command_error:?}");
    };
    assert_eq!(
        detail,
        "the session settled on protocol version 2, which is outside the 3 to 3 this koshi \
         asked for"
    );

    session_thread.join().expect("fake session exits");
    assert_eq!(
        received_requests
            .recv()
            .expect("the session read the Hello")
            .request_kind
            .get_request_kind_name(),
        "Hello",
    );
    assert_eq!(
        received_requests.recv(),
        Err(mpsc::RecvError),
        "no command reached the session",
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_named_client_reaches_a_session_that_speaks_three() {
    let runtime_directory = build_test_runtime_directory("client-protocol-three");
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let (session_thread, received_requests) =
        spawn_settled_session(&runtime_directory, session_id, 3, HelloTiming::AtOnce);

    let command_result = submit_external_command_via_runtime_directory(
        &runtime_directory,
        session_id,
        Some(client_id),
        Command::TogglePaneFullscreen,
    )
    .expect("a session speaking 3 reads the target client");

    session_thread.join().expect("fake session exits");
    assert_eq!(
        received_requests
            .recv()
            .expect("the session read the Hello")
            .request_kind
            .get_request_kind_name(),
        "Hello",
    );
    let submitted_request = received_requests
        .recv()
        .expect("the session read the SubmitCommand");
    let IpcRequestKind::SubmitCommand(command_envelope) = submitted_request.request_kind else {
        panic!("expected a SubmitCommand after the Hello, got {submitted_request:?}");
    };
    assert_eq!(
        command_result,
        CommandResult::Ok {
            command_id: command_envelope.command_id,
            emitted_events: Vec::new(),
        },
    );
    assert_eq!(
        command_envelope.command_source,
        CommandSource::from_external_cli(Some(session_id), Some(client_id)),
    );
    assert_eq!(command_envelope.command, Command::TogglePaneFullscreen);

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn no_named_client_still_costs_one_round_trip() {
    let runtime_directory = build_test_runtime_directory("client-none-pipelined");
    let session_id = SessionId::new();
    let (session_thread, received_requests) = spawn_settled_session(
        &runtime_directory,
        session_id,
        PROTOCOL_VERSION,
        HelloTiming::AfterTheNextRequest,
    );

    let command_result = submit_external_command_via_runtime_directory(
        &runtime_directory,
        session_id,
        None,
        Command::TogglePaneFullscreen,
    )
    .expect("a session this build speaks to answers a command naming no client");

    session_thread.join().expect("fake session exits");
    assert_eq!(
        received_requests
            .recv()
            .expect("the session read the Hello")
            .request_kind
            .get_request_kind_name(),
        "Hello",
    );
    let submitted_request = received_requests
        .recv()
        .expect("the session read the SubmitCommand");
    let IpcRequestKind::SubmitCommand(command_envelope) = submitted_request.request_kind else {
        panic!("expected a SubmitCommand after the Hello, got {submitted_request:?}");
    };
    assert_eq!(
        command_result,
        CommandResult::Ok {
            command_id: command_envelope.command_id,
            emitted_events: Vec::new(),
        },
    );
    assert_eq!(
        command_envelope.command_source,
        CommandSource::from_external_cli(Some(session_id), None),
    );
    assert_eq!(command_envelope.command, Command::TogglePaneFullscreen);

    let _ = std::fs::remove_dir_all(&runtime_directory);
}
