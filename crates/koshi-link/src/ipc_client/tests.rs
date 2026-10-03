//! Tests for the CLI side of the control socket, against a scripted
//! stand-in session serving a real socket.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use koshi_core::command::{NewPaneArgs, NewTabArgs, RunCommandPaneArgs, ToggleLockModeArgs};
use koshi_core::discovery::SessionDiscovery;
use koshi_core::geometry::Direction;
use koshi_core::ids::{PaneId, SessionId};
use koshi_core::process::SpawnSpec;
use koshi_ipc::endpoint::RESTART_WINDOW_DURATION;
use koshi_ipc::layout::TabLayout;
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcResponse};
use koshi_ipc::transport::Listener;
use koshi_layout::tree::LayoutNode;
use koshi_test_support::fixtures::write_session_endpoint_file;

use super::*;
use koshi_ipc::protocol::{IpcErrorPayload, PROTOCOL_VERSION};

/// The path of a fresh runtime directory under a short base: `/tmp` on Unix,
/// the temporary directory on Windows.
fn build_test_runtime_directory(test_case_name: &str) -> PathBuf {
    #[cfg(unix)]
    let runtime_base_directory = PathBuf::from("/tmp");
    #[cfg(windows)]
    let runtime_base_directory = std::env::temp_dir();
    let runtime_directory =
        runtime_base_directory.join(format!("koshi-cli-{}-{test_case_name}", std::process::id()));
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
    /// Refuse the Hello with `UnsupportedVersion` and
    /// [`VERSION_REFUSAL_SENTENCE`] (and the pipelined command with
    /// `HelloRequired`).
    RefuseVersion,
}

/// The sentence a stand-in server refuses this build's protocol version with.
const VERSION_REFUSAL_SENTENCE: &str =
    "this server speaks protocol 3 to 4; the caller asked for 5 to 6";

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
    let session_server_thread = std::thread::spawn(move || {
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
            SessionScript::RefuseVersion => {
                send_ipc_result(
                    &mut session_connection,
                    hello_request.request_id,
                    IpcResult::Error(IpcErrorPayload {
                        code: IpcErrorCode::UnsupportedVersion,
                        message: VERSION_REFUSAL_SENTENCE.to_string(),
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
    (session_server_thread, submitted_envelope_receiver)
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
/// Used only for a reply the CLI may already have stopped reading: the CLI
/// closes the connection once the Hello is refused.
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
    // The hint the session wrote comes back filtered: its `ESC` byte is gone.
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

    let endpoint_error = load_session_endpoint(&runtime_directory, None, session_id)
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
    let (session_server_thread, asked_requests) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::NotFound,
            message: "this session holds no pane by that id".to_string(),
        }),
    );

    let submit_error = submit_command_via_runtime_directory(
        &runtime_directory,
        &build_in_session_context(session_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect_err("the session refuses the command");

    let CliError::IpcUnavailable { detail } = submit_error else {
        panic!("expected IpcUnavailable, got {submit_error:?}");
    };
    assert_eq!(detail, "this session holds no pane by that id");
    assert_eq!(
        asked_requests
            .recv()
            .expect("the session read one request")
            .get_request_kind_name(),
        "SubmitCommand",
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_command_answered_with_another_reply_kind_names_that_kind() {
    let runtime_directory = build_test_runtime_directory("submit-wrong-kind");
    let session_id = SessionId::new();
    let (session_server_thread, _asked_requests) =
        spawn_answering_session(&runtime_directory, session_id, IpcResult::Restarting);

    let submit_error = submit_command_via_runtime_directory(
        &runtime_directory,
        &build_in_session_context(session_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect_err("a Restarting does not answer a submitted command");

    let CliError::IpcUnavailable { detail } = submit_error else {
        panic!("expected IpcUnavailable naming the reply kind, got {submit_error:?}");
    };
    assert_eq!(
        detail,
        "the session answered with an unexpected Restarting reply",
    );

    session_server_thread.join().expect("fake session exits");
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

/// A layout of one empty session named `session_name`.
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

    let session_layout = fetch_layout(&runtime_directory, None, session_id, Some(tab_id))
        .expect("the session answers");

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
        fetch_layout(&runtime_directory, None, session_id, None).expect("the session answers");

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
            &koshi_core::event::Event::Quit(koshi_core::event::QuitCause::Requested),
            SystemTime::UNIX_EPOCH,
        ),
    ];
    let (session_server_thread, request_kinds) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::RecentEvents(expected_recent_events.clone()),
    );

    let recent_events =
        fetch_recent_events(&runtime_directory, None, session_id).expect("the session answers");

    assert_eq!(recent_events, expected_recent_events);
    assert_eq!(
        request_kinds.recv().expect("the session read one request"),
        IpcRequestKind::RecentEvents,
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_recent_events_request_a_session_cannot_read_carries_its_own_message() {
    let runtime_directory = build_test_runtime_directory("events-unreadable");
    let session_id = SessionId::new();
    let (session_server_thread, _request_kinds) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the bytes received are not a request this build can read".to_string(),
        }),
    );

    let recent_events_error = fetch_recent_events(&runtime_directory, None, session_id)
        .expect_err("the request is refused");

    assert_eq!(
        recent_events_error.to_string(),
        CliError::IpcUnavailable {
            detail: "the bytes received are not a request this build can read".to_string(),
        }
        .to_string(),
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_recent_events_refusal_with_a_bad_token_carries_the_sessions_own_message() {
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

    let recent_events_error = fetch_recent_events(&runtime_directory, None, session_id)
        .expect_err("the request is refused");

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

    let layout_error = fetch_layout(&runtime_directory, None, session_id, None)
        .expect_err("no endpoint file exists");

    assert_eq!(
        layout_error.to_string(),
        CliError::SessionNotFound {
            session_name: session_id.to_string(),
        }
        .to_string(),
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_layout_request_a_session_cannot_read_carries_its_own_message() {
    let runtime_directory = build_test_runtime_directory("layout-unreadable");
    let session_id = SessionId::new();
    let (session_server_thread, _request_kinds) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the bytes received are not a request this build can read".to_string(),
        }),
    );

    let layout_error = fetch_layout(&runtime_directory, None, session_id, None)
        .expect_err("the request is refused");

    assert_eq!(
        layout_error.to_string(),
        CliError::IpcUnavailable {
            detail: "the bytes received are not a request this build can read".to_string(),
        }
        .to_string(),
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_layout_refusal_with_a_bad_token_carries_the_sessions_own_message() {
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

    let layout_error = fetch_layout(&runtime_directory, None, session_id, None)
        .expect_err("the request is refused");

    assert_eq!(
        layout_error.to_string(),
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
    // The session answers with a layout that describes no tab.
    let runtime_directory = build_test_runtime_directory("layout-tab-gone");
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let (session_server_thread, _request_kinds) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Layout(build_named_session_layout("workspace", session_id)),
    );

    let layout_error = fetch_layout(&runtime_directory, None, session_id, Some(tab_id))
        .expect_err("the tab is no longer there");

    assert_eq!(
        layout_error.to_string(),
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

    let layout_error = fetch_layout(&runtime_directory, None, session_id, None)
        .expect_err("a Hello does not answer a layout request");

    let CliError::IpcUnavailable { detail } = layout_error else {
        panic!("expected IpcUnavailable naming the reply kind, got {layout_error:?}");
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
    let answered_overview = build_named_session_overview("S-quiet-lake", session_id);
    let (session_server_thread, asked_requests) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Overview(answered_overview.clone()),
    );

    let fetched_overview = fetch_session_overview_from_endpoint(
        &load_session_endpoint(&runtime_directory, None, session_id).expect("the endpoint file"),
        session_id,
        None,
    )
    .expect("the session answers");

    assert_eq!(fetched_overview, answered_overview);
    assert_eq!(
        asked_requests.recv().expect("the session read one request"),
        IpcRequestKind::Discovery,
    );

    session_server_thread.join().expect("fake session exits");
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

    let overview_error = fetch_session_overview_from_endpoint(
        &load_session_endpoint(&runtime_directory, None, session_id).expect("the endpoint file"),
        session_id,
        None,
    )
    .expect_err("the request is refused");

    let CliError::IpcUnavailable { detail } = overview_error else {
        panic!("expected IpcUnavailable, got {overview_error:?}");
    };
    assert_eq!(detail, "the token presented does not match this Koshi's");

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn an_overview_request_answered_with_another_reply_kind_names_that_kind() {
    let runtime_directory = build_test_runtime_directory("overview-wrong-kind");
    let session_id = SessionId::new();
    let (session_server_thread, _asked_requests) =
        spawn_answering_session(&runtime_directory, session_id, IpcResult::Restarting);

    let overview_error = fetch_session_overview_from_endpoint(
        &load_session_endpoint(&runtime_directory, None, session_id).expect("the endpoint file"),
        session_id,
        None,
    )
    .expect_err("a Restarting does not describe a session");

    let CliError::IpcUnavailable { detail } = overview_error else {
        panic!("expected IpcUnavailable naming the reply kind, got {overview_error:?}");
    };
    assert_eq!(
        detail,
        "the session answered with an unexpected Restarting reply",
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

// --- Asking a session to restart --------------------------------------------

#[test]
fn a_restarting_reply_reports_the_session_restarting_and_asked_for_a_restart() {
    let runtime_directory = build_test_runtime_directory("restart-ok");
    let session_id = SessionId::new();
    let (session_server_thread, asked_requests) =
        spawn_answering_session(&runtime_directory, session_id, IpcResult::Restarting);

    assert_eq!(
        restart_running_session(&runtime_directory, None, session_id)
            .expect("the exchange succeeds"),
        SessionRestart::Restarting
    );
    assert_eq!(
        asked_requests.recv().expect("the session read one request"),
        IpcRequestKind::Restart,
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_restart_refused_with_the_malformed_request_code_carries_the_sessions_sentence() {
    let runtime_directory = build_test_runtime_directory("restart-malformed-request");
    let session_id = SessionId::new();
    let (session_server_thread, _asked) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the binary at /opt/koshi could not be read: Exec format error (os error 8)"
                .to_string(),
        }),
    );

    let restart_error = restart_running_session(&runtime_directory, None, session_id)
        .expect_err("the restart is refused");

    assert_eq!(
        restart_error.to_string(),
        "IPC unavailable: the binary at /opt/koshi could not be read: Exec format error (os error 8)"
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_restart_refused_with_the_request_failed_code_carries_the_sessions_sentence() {
    let runtime_directory = build_test_runtime_directory("restart-request-failed");
    let session_id = SessionId::new();
    let (session_server_thread, _asked) = spawn_answering_session(
        &runtime_directory,
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::RequestFailed,
            message: "the binary at /opt/koshi could not be read: No such file or directory"
                .to_string(),
        }),
    );

    let restart_error = restart_running_session(&runtime_directory, None, session_id)
        .expect_err("the restart is refused");

    assert_eq!(
        restart_error.to_string(),
        "IPC unavailable: the binary at /opt/koshi could not be read: No such file or directory"
    );

    session_server_thread.join().expect("fake session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_restart_refused_with_a_bad_token_carries_the_sessions_sentence() {
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

    let restart_error = restart_running_session(&runtime_directory, None, session_id)
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

    let restart_error = restart_running_session(&runtime_directory, None, session_id)
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
        restart_running_session(&runtime_directory, None, session_id)
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
        restart_running_session(&runtime_directory, None, session_id)
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
        find_running_session_version(&runtime_directory, None, session_id, None)
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
        find_running_session_version(&runtime_directory, None, session_id, None)
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
        find_running_session_version(&runtime_directory, None, session_id, None)
            .expect("a dead socket is not an error"),
        None
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_version_ask_whose_answer_deadline_has_passed_connects_to_nothing() {
    // Nothing listens at `socket_address`.
    let runtime_directory = build_test_runtime_directory("version-past-deadline");
    let session_id = SessionId::new();
    let socket_address =
        koshi_ipc::endpoint::compute_socket_address(&runtime_directory, session_id);

    let version_answer = find_foreign_session_version(session_id, &socket_address, Instant::now());

    let Err(CliError::SessionAnswerTimedOut) = version_answer else {
        panic!("expected SessionAnswerTimedOut, got {version_answer:?}");
    };
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[cfg(windows)]
#[test]
fn a_connect_to_a_busy_pipe_ends_at_the_answer_deadline() {
    // The pipe's one instance holds a caller the listener never accepts. The
    // OS can end the connect's wait a moment before `answer_deadline`, and the
    // failure is then `IpcUnavailable`.
    let runtime_directory = build_test_runtime_directory("busy-pipe");
    let session_id = SessionId::new();
    let socket_address =
        koshi_ipc::endpoint::compute_socket_address(&runtime_directory, session_id);
    let _session_listener = Listener::bind(&socket_address).expect("bind the session's pipe");
    let _first_connection =
        Connection::connect(&socket_address).expect("the free pipe instance connects");
    let ask_started_at = Instant::now();

    let version_answer = find_foreign_session_version(
        session_id,
        &socket_address,
        ask_started_at + Duration::from_millis(300),
    );
    let ask_duration = ask_started_at.elapsed();

    match version_answer {
        Err(CliError::SessionAnswerTimedOut) | Err(CliError::IpcUnavailable { .. }) => {}
        unexpected_answer => panic!("expected a timed-out connect, got {unexpected_answer:?}"),
    }
    assert!(
        ask_duration >= Duration::from_millis(250),
        "the connect waits for the busy pipe, the ask took {ask_duration:?}"
    );
    assert!(
        ask_duration < CONNECT_WAIT_DURATION,
        "the connect ends at the answer deadline, the ask took {ask_duration:?}"
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

// --- Which file names a session, and under which suffix ---------------------

#[test]
fn an_endpoint_file_and_a_resume_file_are_told_apart_by_their_suffix() {
    // Both names start `session-<uuid>`. The suffix tells the session that
    // advertises a socket from the one that left a resume file.
    let runtime_directory = build_test_runtime_directory("suffixes");
    let advertised_session_id = SessionId::new();
    let resumable_session_id = SessionId::new();
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(
            &runtime_directory,
            advertised_session_id,
        ),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        &runtime_directory,
        advertised_session_id,
    ))
    .expect("write endpoint file");
    std::fs::write(
        runtime_directory.join(format!("{resumable_session_id}{RESUME_SUFFIX}")),
        b"{}",
    )
    .expect("write resume file");

    assert_eq!(
        list_advertised_sessions(&runtime_directory),
        Ok(vec![advertised_session_id])
    );
    assert_eq!(
        list_sessions_with_resume_files(&runtime_directory),
        Ok(vec![resumable_session_id])
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_file_that_names_no_session_is_passed_over() {
    let runtime_directory = build_test_runtime_directory("suffixes-junk");
    let advertised_session_id = SessionId::new();
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(
            &runtime_directory,
            advertised_session_id,
        ),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        &runtime_directory,
        advertised_session_id,
    ))
    .expect("write endpoint file");
    std::fs::write(runtime_directory.join("session-not-a-uuid.json"), b"{}").expect("bad uuid");
    std::fs::write(runtime_directory.join("router.json"), b"{}").expect("no session prefix");
    std::fs::write(
        runtime_directory.join(advertised_session_id.to_string()),
        b"{}",
    )
    .expect("no suffix");

    assert_eq!(
        list_advertised_sessions(&runtime_directory),
        Ok(vec![advertised_session_id])
    );
    assert_eq!(
        list_sessions_with_resume_files(&runtime_directory),
        Ok(Vec::new())
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_runtime_directory_that_does_not_exist_names_no_session() {
    let runtime_directory = build_test_runtime_directory("suffixes-absent");
    let absent_runtime_directory = runtime_directory.join("absent");

    assert_eq!(
        list_advertised_sessions(&absent_runtime_directory),
        Ok(Vec::new())
    );
    assert_eq!(
        list_sessions_with_resume_files(&absent_runtime_directory),
        Ok(Vec::new())
    );
    assert_eq!(list_own_sessions(&absent_runtime_directory), Ok(Vec::new()));

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[cfg(unix)]
#[test]
fn a_runtime_directory_that_cannot_be_read_is_refused_naming_it() {
    use std::os::unix::fs::PermissionsExt;

    let runtime_directory = build_test_runtime_directory("suffixes-unreadable");
    write_own_endpoint_file(&runtime_directory, SessionId::new());
    std::fs::set_permissions(&runtime_directory, std::fs::Permissions::from_mode(0o000))
        .expect("make the runtime directory unreadable");
    let Err(read_error) = std::fs::read_dir(&runtime_directory) else {
        eprintln!(
            "skipped `a_runtime_directory_that_cannot_be_read_is_refused_naming_it`: \
             this user reads through a mode-000 directory"
        );
        let _ =
            std::fs::set_permissions(&runtime_directory, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::remove_dir_all(&runtime_directory);
        return;
    };

    let advertised_sessions = list_advertised_sessions(&runtime_directory);
    let resumable_sessions = list_sessions_with_resume_files(&runtime_directory);
    let own_sessions = list_own_sessions(&runtime_directory);

    let _ = std::fs::set_permissions(&runtime_directory, std::fs::Permissions::from_mode(0o700));
    let unread_runtime_directory = UnreadPath::from_read_error(&runtime_directory, &read_error);
    assert_eq!(advertised_sessions, Err(unread_runtime_directory.clone()));
    assert_eq!(resumable_sessions, Err(unread_runtime_directory.clone()));
    assert_eq!(own_sessions, Err(unread_runtime_directory));
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
    let session_server_thread = std::thread::spawn(move || {
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
    (session_server_thread, presented_token_receiver)
}

/// Bind a Unix socket at `socket_path` and close it: the socket file stays,
/// with nothing listening behind it.
#[cfg(unix)]
fn plant_session_socket(socket_path: &Path) {
    drop(std::os::unix::net::UnixListener::bind(socket_path).expect("bind the planted socket"));
}

#[cfg(unix)]
#[test]
fn a_session_missing_here_is_found_in_the_shared_directory_given() {
    use std::os::unix::fs::MetadataExt;

    let runtime_directory = build_test_runtime_directory("shared-found");
    let shared_sessions_base_directory = build_test_runtime_directory("sf");
    let own_user_id = std::fs::metadata(&runtime_directory)
        .expect("read the runtime directory")
        .uid();
    let other_user_directory = shared_sessions_base_directory.join((own_user_id + 1).to_string());
    std::fs::create_dir_all(&other_user_directory).expect("create the other user's directory");
    let session_id = SessionId::new();
    let socket_path = other_user_directory.join(format!("{session_id}.sock"));
    plant_session_socket(&socket_path);

    let endpoint = load_session_endpoint(
        &runtime_directory,
        Some(&shared_sessions_base_directory),
        session_id,
    )
    .expect("the shared directory advertises the session");
    let lookup_without_shared_directory =
        load_session_endpoint(&runtime_directory, None, session_id);

    assert_eq!(endpoint.socket_address, socket_path.display().to_string());
    assert_eq!(endpoint.connection_token.expose_secret(), "");
    assert_eq!(endpoint.process_id, 0);
    let Err(CliError::SessionNotFound { session_name }) = lookup_without_shared_directory else {
        panic!("expected SessionNotFound, got {lookup_without_shared_directory:?}");
    };
    assert_eq!(session_name, session_id.to_string());
    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[cfg(unix)]
#[test]
fn the_shared_listing_holds_other_users_sockets_and_not_this_users() {
    use std::os::unix::fs::MetadataExt;

    let runtime_directory = build_test_runtime_directory("shared-unix");
    let shared_sessions_base_directory = build_test_runtime_directory("su");
    let own_user_id = std::fs::metadata(&runtime_directory)
        .expect("read the runtime directory")
        .uid();
    let other_user_directory_name = (own_user_id + 1).to_string();
    let own_session_id = SessionId::new();
    let foreign_session_id = SessionId::new();
    std::fs::create_dir_all(shared_sessions_base_directory.join(own_user_id.to_string()))
        .expect("create this user's directory");
    std::fs::create_dir_all(shared_sessions_base_directory.join(&other_user_directory_name))
        .expect("create the other user's directory");
    plant_session_socket(
        &shared_sessions_base_directory
            .join(own_user_id.to_string())
            .join(format!("{own_session_id}.sock")),
    );
    plant_session_socket(
        &shared_sessions_base_directory
            .join(&other_user_directory_name)
            .join(format!("{foreign_session_id}.sock")),
    );

    assert_eq!(
        list_foreign_sessions(&shared_sessions_base_directory, &runtime_directory),
        ForeignSessionListing {
            foreign_sessions: vec![(
                foreign_session_id,
                shared_sessions_base_directory
                    .join(&other_user_directory_name)
                    .join(format!("{foreign_session_id}.sock"))
                    .display()
                    .to_string(),
            )],
            ..ForeignSessionListing::default()
        },
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[cfg(unix)]
#[test]
fn a_foreign_socket_reusing_a_local_session_id_is_left_out() {
    // A socket in another user's folder named after an id this user's endpoint
    // file advertises is not listed.
    use std::os::unix::fs::MetadataExt;

    let runtime_directory = build_test_runtime_directory("shared-collide");
    let shared_sessions_base_directory = build_test_runtime_directory("sc");
    let own_user_id = std::fs::metadata(&runtime_directory)
        .expect("read the runtime directory")
        .uid();
    let own_session_id = SessionId::new();
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
    .expect("advertise this user's session");
    let foreign_user_directory = shared_sessions_base_directory.join((own_user_id + 1).to_string());
    std::fs::create_dir_all(&foreign_user_directory).expect("create the other user's directory");
    plant_session_socket(&foreign_user_directory.join(format!("{own_session_id}.sock")));

    assert_eq!(
        list_foreign_sessions(&shared_sessions_base_directory, &runtime_directory),
        ForeignSessionListing::default()
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[cfg(windows)]
#[test]
fn the_shared_listing_holds_the_markers_this_user_does_not_advertise() {
    // A marker whose id this user's endpoint file advertises is not listed.
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
        ForeignSessionListing {
            foreign_sessions: vec![(foreign_session_id, format!("koshi-{foreign_session_id}"),)],
            ..ForeignSessionListing::default()
        },
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[test]
fn a_shared_directory_that_does_not_exist_holds_no_session() {
    let runtime_directory = build_test_runtime_directory("shared-unreadable");

    assert_eq!(
        list_foreign_sessions(&runtime_directory.join("absent"), &runtime_directory),
        ForeignSessionListing::default(),
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[cfg(unix)]
#[test]
fn an_absent_runtime_directory_skips_no_shared_subdirectory() {
    // With no runtime directory, the folder named after this user's id is
    // listed like any other.
    use std::os::unix::fs::MetadataExt;

    let shared_sessions_base_directory = build_test_runtime_directory("sa");
    let own_user_id = std::fs::metadata(&shared_sessions_base_directory)
        .expect("read the shared directory")
        .uid();
    let foreign_session_id = SessionId::new();
    let foreign_user_directory = shared_sessions_base_directory.join(own_user_id.to_string());
    std::fs::create_dir_all(&foreign_user_directory).expect("create a user's directory");
    plant_session_socket(&foreign_user_directory.join(format!("{foreign_session_id}.sock")));

    assert_eq!(
        list_foreign_sessions(
            &shared_sessions_base_directory,
            &shared_sessions_base_directory.join("no-runtime-dir-here"),
        ),
        ForeignSessionListing {
            foreign_sessions: vec![(
                foreign_session_id,
                koshi_ipc::endpoint::compute_socket_address(
                    &foreign_user_directory,
                    foreign_session_id
                )
            )],
            ..ForeignSessionListing::default()
        },
    );

    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[cfg(unix)]
#[test]
fn a_runtime_directory_that_cannot_be_read_lists_no_foreign_session_and_names_it() {
    // The runtime directory sits under a mode-000 folder: the listing keeps
    // the runtime directory as unread, and a lookup by id fails naming the
    // endpoint file it could not look up.
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let shared_sessions_base_directory = build_test_runtime_directory("so");
    let own_user_id = std::fs::metadata(&shared_sessions_base_directory)
        .expect("read the shared directory")
        .uid();
    if own_user_id == 0 {
        eprintln!(
            "skipped `a_runtime_directory_that_cannot_be_read_lists_no_foreign_session_and_names_it`: \
             root reads through a mode-000 directory"
        );
        let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
        return;
    }
    let foreign_session_id = SessionId::new();
    let foreign_user_directory = shared_sessions_base_directory.join((own_user_id + 1).to_string());
    std::fs::create_dir_all(&foreign_user_directory).expect("create the other user's directory");
    plant_session_socket(&foreign_user_directory.join(format!("{foreign_session_id}.sock")));

    let unreadable_parent_directory =
        build_test_runtime_directory("shared-owner-unreadable-parent");
    let runtime_directory = unreadable_parent_directory.join("runtime");
    std::fs::create_dir_all(&runtime_directory).expect("create the runtime directory");
    std::fs::set_permissions(
        &unreadable_parent_directory,
        std::fs::Permissions::from_mode(0o000),
    )
    .expect("make the parent unsearchable");

    let runtime_read_error =
        std::fs::read_dir(&runtime_directory).expect_err("the parent is unsearchable");
    let endpoint_file_path =
        EndpointFile::resolve_endpoint_file_path(&runtime_directory, foreign_session_id);
    let endpoint_read_error =
        std::fs::symlink_metadata(&endpoint_file_path).expect_err("the parent is unsearchable");

    let foreign_session_listing =
        list_foreign_sessions(&shared_sessions_base_directory, &runtime_directory);
    let lookup_error = find_foreign_session_address(
        &shared_sessions_base_directory,
        &runtime_directory,
        foreign_session_id,
    );

    let _ = std::fs::set_permissions(
        &unreadable_parent_directory,
        std::fs::Permissions::from_mode(0o700),
    );
    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            unread_path: Some(UnreadPath::from_read_error(
                &runtime_directory,
                &runtime_read_error
            )),
            ..ForeignSessionListing::default()
        }
    );
    let Err(ForeignSessionLookupError::PathUnreadable { unread_path }) = lookup_error else {
        panic!("expected PathUnreadable, got {lookup_error:?}");
    };
    assert_eq!(
        unread_path,
        UnreadPath::from_read_error(&endpoint_file_path, &endpoint_read_error)
    );
    let _ = std::fs::remove_dir_all(&unreadable_parent_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[cfg(unix)]
#[test]
fn entries_another_user_planted_that_name_no_session_are_passed_over() {
    // Only a `session-<uuid>.sock` inside a folder named like a user id is a
    // session.
    use std::os::unix::fs::MetadataExt;

    let runtime_directory = build_test_runtime_directory("shared-planted");
    let shared_sessions_base_directory = build_test_runtime_directory("sp");
    let own_user_id = std::fs::metadata(&runtime_directory)
        .expect("read the runtime directory")
        .uid();
    let foreign_user_directory = shared_sessions_base_directory.join((own_user_id + 1).to_string());
    std::fs::create_dir_all(&foreign_user_directory).expect("create the other user's directory");
    let foreign_session_id = SessionId::new();
    plant_session_socket(&foreign_user_directory.join(format!("{foreign_session_id}.sock")));
    plant_session_socket(&foreign_user_directory.join("session-not-a-uuid.sock"));
    plant_session_socket(&foreign_user_directory.join(foreign_session_id.to_string()));
    plant_session_socket(&foreign_user_directory.join("README.sock"));
    std::fs::create_dir_all(foreign_user_directory.join("nested")).expect("plant a subdirectory");
    std::fs::write(shared_sessions_base_directory.join("loose-file"), b"")
        .expect("plant a file beside the user directory");

    assert_eq!(
        list_foreign_sessions(&shared_sessions_base_directory, &runtime_directory,),
        ForeignSessionListing {
            foreign_sessions: vec![(
                foreign_session_id,
                foreign_user_directory
                    .join(format!("{foreign_session_id}.sock"))
                    .display()
                    .to_string(),
            )],
            ..ForeignSessionListing::default()
        },
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[cfg(unix)]
#[test]
fn a_plain_file_named_like_a_session_socket_is_passed_over() {
    use std::os::unix::fs::MetadataExt;

    let runtime_directory = build_test_runtime_directory("shared-plain-file");
    let shared_sessions_base_directory = build_test_runtime_directory("pf");
    let own_user_id = std::fs::metadata(&runtime_directory)
        .expect("read the runtime directory")
        .uid();
    let foreign_user_directory = shared_sessions_base_directory.join((own_user_id + 1).to_string());
    std::fs::create_dir_all(&foreign_user_directory).expect("create the other user's directory");
    let plain_file_session_id = SessionId::new();
    std::fs::write(
        foreign_user_directory.join(format!("{plain_file_session_id}.sock")),
        b"",
    )
    .expect("plant a plain file");

    assert_eq!(
        list_foreign_sessions(&shared_sessions_base_directory, &runtime_directory),
        ForeignSessionListing::default()
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[cfg(unix)]
#[test]
fn no_more_than_256_sessions_of_one_owner_are_listed_across_their_folders() {
    // Both folders belong to the user running the test: 200 sockets in one
    // and 100 in the other are one owner's 300, 256 listed and 44 counted.
    use std::os::unix::fs::MetadataExt;

    let runtime_directory = build_test_runtime_directory("shared-owner-cap");
    let shared_sessions_base_directory = build_test_runtime_directory("oc");
    let own_user_id = std::fs::metadata(&runtime_directory)
        .expect("read the runtime directory")
        .uid();
    for (folder_offset, planted_socket_count) in [(1, 200), (2, 100)] {
        let user_directory =
            shared_sessions_base_directory.join((own_user_id + folder_offset).to_string());
        std::fs::create_dir_all(&user_directory).expect("create a user's directory");
        for _ in 0..planted_socket_count {
            plant_session_socket(&user_directory.join(format!("{}.sock", SessionId::new())));
        }
    }

    let foreign_session_listing =
        list_foreign_sessions(&shared_sessions_base_directory, &runtime_directory);

    assert_eq!(
        foreign_session_listing.foreign_sessions.len(),
        MAX_SHARED_SESSION_COUNT_PER_OWNER
    );
    assert_eq!(foreign_session_listing.unlisted_session_count, 44);
    assert_eq!(foreign_session_listing.duplicated_sessions, Vec::new());

    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[cfg(windows)]
#[test]
fn markers_another_user_planted_that_name_no_session_are_passed_over() {
    // Only an entry named `session-<uuid>` is a session.
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
        ForeignSessionListing {
            foreign_sessions: vec![(foreign_session_id, format!("koshi-{foreign_session_id}"),)],
            ..ForeignSessionListing::default()
        },
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[test]
fn a_scan_reads_65536_entries_and_stops_at_the_next() {
    let shared_sessions_base_directory = Path::new("/tmp/koshi");
    let mut shared_directory_scan = SharedDirectoryScan::default();

    for _ in 0..MAX_SHARED_DIRECTORY_ENTRY_COUNT {
        assert_eq!(
            shared_directory_scan.count_read_entry(shared_sessions_base_directory),
            Ok(())
        );
    }

    let entry_limit_unread_path = UnreadPath {
        looked_up_path: PathBuf::from("/tmp/koshi"),
        read_error_text: "it holds more than 65536 entries".to_string(),
    };
    assert_eq!(
        shared_directory_scan.count_read_entry(shared_sessions_base_directory),
        Err(entry_limit_unread_path.clone())
    );
    assert_eq!(
        shared_directory_scan.count_read_entry(shared_sessions_base_directory),
        Err(entry_limit_unread_path)
    );
}

#[cfg(unix)]
#[test]
fn a_scan_finds_256_user_folders_and_stops_at_the_next() {
    let shared_sessions_base_directory = Path::new("/tmp/koshi");
    let mut shared_directory_scan = SharedDirectoryScan::default();

    for _ in 0..MAX_SHARED_USER_DIRECTORY_COUNT {
        assert_eq!(
            shared_directory_scan.count_found_user_directory(shared_sessions_base_directory),
            Ok(())
        );
    }

    let folder_limit_unread_path = UnreadPath {
        looked_up_path: PathBuf::from("/tmp/koshi"),
        read_error_text: "it holds more than 256 user folders".to_string(),
    };
    assert_eq!(
        shared_directory_scan.count_found_user_directory(shared_sessions_base_directory),
        Err(folder_limit_unread_path.clone())
    );
    assert_eq!(
        shared_directory_scan.count_found_user_directory(shared_sessions_base_directory),
        Err(folder_limit_unread_path)
    );
}

#[cfg(unix)]
#[test]
fn a_shared_directory_with_one_user_folder_past_the_limit_is_kept_unread() {
    // One of the 256 user folders holds a session socket. The listing at the
    // limit holds that session.
    use std::os::unix::fs::MetadataExt;

    let runtime_directory_guard = koshi_test_support::fixtures::build_test_runtime_directory();
    let runtime_directory = runtime_directory_guard.path();
    let shared_directory_guard = koshi_test_support::fixtures::build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let own_user_id = std::fs::metadata(runtime_directory)
        .expect("read the runtime directory")
        .uid();
    for other_user_id in (own_user_id + 1..).take(MAX_SHARED_USER_DIRECTORY_COUNT) {
        std::fs::create_dir(shared_sessions_base_directory.join(other_user_id.to_string()))
            .expect("create a user folder");
    }
    let past_limit_user_id = (own_user_id + 1..)
        .nth(MAX_SHARED_USER_DIRECTORY_COUNT)
        .expect("a user id past the limit");
    let session_id = SessionId::new();
    let other_user_directory = shared_sessions_base_directory.join((own_user_id + 1).to_string());
    plant_session_socket(&other_user_directory.join(format!("{session_id}.sock")));
    let socket_address =
        koshi_ipc::endpoint::compute_socket_address(&other_user_directory, session_id);

    let listing_at_the_limit =
        list_foreign_sessions(shared_sessions_base_directory, runtime_directory);
    let lookup_at_the_limit = find_foreign_session_address(
        shared_sessions_base_directory,
        runtime_directory,
        session_id,
    );
    std::fs::create_dir(shared_sessions_base_directory.join(past_limit_user_id.to_string()))
        .expect("create the user folder past the limit");
    let listing_past_the_limit =
        list_foreign_sessions(shared_sessions_base_directory, runtime_directory);
    let lookup_past_the_limit = find_foreign_session_address(
        shared_sessions_base_directory,
        runtime_directory,
        session_id,
    );

    let folder_limit_unread_path = UnreadPath {
        looked_up_path: shared_sessions_base_directory.to_path_buf(),
        read_error_text: "it holds more than 256 user folders".to_string(),
    };
    assert_eq!(
        listing_at_the_limit,
        ForeignSessionListing {
            foreign_sessions: vec![(session_id, socket_address.clone())],
            ..ForeignSessionListing::default()
        }
    );
    assert_eq!(
        lookup_at_the_limit.expect("the lookup at the limit reads"),
        Some(socket_address.clone())
    );
    let listings_past_the_limit = [
        ForeignSessionListing {
            foreign_sessions: vec![(session_id, socket_address)],
            unread_path: Some(folder_limit_unread_path.clone()),
            ..ForeignSessionListing::default()
        },
        ForeignSessionListing {
            unread_path: Some(folder_limit_unread_path.clone()),
            ..ForeignSessionListing::default()
        },
    ];
    assert!(
        listings_past_the_limit.contains(&listing_past_the_limit),
        "the session is listed only when it was read before the stop, got {listing_past_the_limit:?}"
    );
    let Err(ForeignSessionLookupError::PathUnreadable { unread_path }) = lookup_past_the_limit
    else {
        panic!("expected PathUnreadable, got {lookup_past_the_limit:?}");
    };
    assert_eq!(unread_path, folder_limit_unread_path);
}

#[cfg(unix)]
#[test]
fn a_shared_directory_with_one_entry_past_the_limit_is_kept_unread() {
    // The shared directory holds 65,534 planted links and one user folder,
    // and that folder holds one session socket: 65,536 entries. Then one link
    // in that folder, then two more links beside it.
    use std::os::unix::fs::MetadataExt;

    let runtime_directory_guard = koshi_test_support::fixtures::build_test_runtime_directory();
    let runtime_directory = runtime_directory_guard.path();
    let shared_directory_guard = koshi_test_support::fixtures::build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let own_user_id = std::fs::metadata(runtime_directory)
        .expect("read the runtime directory")
        .uid();
    let other_user_directory = shared_sessions_base_directory.join((own_user_id + 1).to_string());
    std::fs::create_dir(&other_user_directory).expect("create the other user's folder");
    let session_id = SessionId::new();
    plant_session_socket(&other_user_directory.join(format!("{session_id}.sock")));
    let socket_address =
        koshi_ipc::endpoint::compute_socket_address(&other_user_directory, session_id);
    for entry_index in 2..MAX_SHARED_DIRECTORY_ENTRY_COUNT {
        std::os::unix::fs::symlink(
            "nowhere",
            shared_sessions_base_directory.join(format!("planted-{entry_index}")),
        )
        .expect("plant an entry");
    }

    let listing_at_the_limit =
        list_foreign_sessions(shared_sessions_base_directory, runtime_directory);
    let lookup_at_the_limit = find_foreign_session_address(
        shared_sessions_base_directory,
        runtime_directory,
        session_id,
    );
    std::os::unix::fs::symlink("nowhere", other_user_directory.join("planted"))
        .expect("plant an entry in the other user's folder");
    let listing_past_the_limit =
        list_foreign_sessions(shared_sessions_base_directory, runtime_directory);
    let lookup_with_the_folder_past_the_limit = find_foreign_session_address(
        shared_sessions_base_directory,
        runtime_directory,
        session_id,
    );
    for entry_index in 0..2 {
        std::os::unix::fs::symlink(
            "nowhere",
            shared_sessions_base_directory.join(format!("planted-{entry_index}")),
        )
        .expect("plant one more entry");
    }
    let lookup_past_the_limit = find_foreign_session_address(
        shared_sessions_base_directory,
        runtime_directory,
        session_id,
    );

    let entry_limit_unread_path = UnreadPath {
        looked_up_path: shared_sessions_base_directory.to_path_buf(),
        read_error_text: "it holds more than 65536 entries".to_string(),
    };
    assert_eq!(
        listing_at_the_limit,
        ForeignSessionListing {
            foreign_sessions: vec![(session_id, socket_address.clone())],
            ..ForeignSessionListing::default()
        }
    );
    assert_eq!(
        lookup_at_the_limit.expect("the lookup at the limit reads"),
        Some(socket_address.clone())
    );
    let listings_past_the_limit = [
        ForeignSessionListing {
            foreign_sessions: vec![(session_id, socket_address.clone())],
            unread_path: Some(entry_limit_unread_path.clone()),
            ..ForeignSessionListing::default()
        },
        ForeignSessionListing {
            unread_path: Some(entry_limit_unread_path.clone()),
            ..ForeignSessionListing::default()
        },
    ];
    assert!(
        listings_past_the_limit.contains(&listing_past_the_limit),
        "the session is listed only when it was read before the stop, got {listing_past_the_limit:?}"
    );
    assert_eq!(
        lookup_with_the_folder_past_the_limit.expect("the lookup reads no folder listing"),
        Some(socket_address)
    );
    let Err(ForeignSessionLookupError::PathUnreadable { unread_path }) = lookup_past_the_limit
    else {
        panic!("expected PathUnreadable, got {lookup_past_the_limit:?}");
    };
    assert_eq!(unread_path, entry_limit_unread_path);
}

#[cfg(unix)]
#[test]
fn a_user_folder_that_takes_the_scan_past_the_entry_limit_keeps_the_shared_directory_unread() {
    // The shared directory holds one user folder. That folder holds one
    // session socket and 65,534 planted links: 65,536 entries. Then one more
    // link in that folder.
    use std::os::unix::fs::MetadataExt;

    let runtime_directory_guard = koshi_test_support::fixtures::build_test_runtime_directory();
    let runtime_directory = runtime_directory_guard.path();
    let shared_directory_guard = koshi_test_support::fixtures::build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let own_user_id = std::fs::metadata(runtime_directory)
        .expect("read the runtime directory")
        .uid();
    let other_user_directory = shared_sessions_base_directory.join((own_user_id + 1).to_string());
    std::fs::create_dir(&other_user_directory).expect("create the other user's folder");
    let session_id = SessionId::new();
    plant_session_socket(&other_user_directory.join(format!("{session_id}.sock")));
    let socket_address =
        koshi_ipc::endpoint::compute_socket_address(&other_user_directory, session_id);
    for entry_index in 2..MAX_SHARED_DIRECTORY_ENTRY_COUNT {
        std::os::unix::fs::symlink(
            "nowhere",
            other_user_directory.join(format!("planted-{entry_index}")),
        )
        .expect("plant an entry");
    }

    let listing_at_the_limit =
        list_foreign_sessions(shared_sessions_base_directory, runtime_directory);
    std::os::unix::fs::symlink("nowhere", other_user_directory.join("planted-1"))
        .expect("plant one more entry");
    let listing_past_the_limit =
        list_foreign_sessions(shared_sessions_base_directory, runtime_directory);

    let entry_limit_unread_path = UnreadPath {
        looked_up_path: shared_sessions_base_directory.to_path_buf(),
        read_error_text: "it holds more than 65536 entries".to_string(),
    };
    assert_eq!(
        listing_at_the_limit,
        ForeignSessionListing {
            foreign_sessions: vec![(session_id, socket_address.clone())],
            ..ForeignSessionListing::default()
        }
    );
    let listings_past_the_limit = [
        ForeignSessionListing {
            foreign_sessions: vec![(session_id, socket_address)],
            unread_path: Some(entry_limit_unread_path.clone()),
            ..ForeignSessionListing::default()
        },
        ForeignSessionListing {
            unread_path: Some(entry_limit_unread_path.clone()),
            ..ForeignSessionListing::default()
        },
    ];
    assert!(
        listings_past_the_limit.contains(&listing_past_the_limit),
        "the session is listed only when it was read before the stop, got {listing_past_the_limit:?}"
    );
}

#[cfg(windows)]
#[test]
fn a_shared_directory_with_one_marker_entry_past_the_limit_is_kept_unread() {
    // One session marker and 65,535 planted files: 65,536 entries.
    let runtime_directory_guard = koshi_test_support::fixtures::build_test_runtime_directory();
    let runtime_directory = runtime_directory_guard.path();
    let shared_directory_guard = koshi_test_support::fixtures::build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let foreign_session_id = SessionId::new();
    std::fs::write(
        shared_sessions_base_directory.join(foreign_session_id.to_string()),
        b"",
    )
    .expect("plant their marker");
    for entry_index in 1..MAX_SHARED_DIRECTORY_ENTRY_COUNT {
        std::fs::write(
            shared_sessions_base_directory.join(format!("planted-{entry_index}")),
            b"",
        )
        .expect("plant an entry");
    }

    let listing_at_the_limit =
        list_foreign_sessions(shared_sessions_base_directory, runtime_directory);
    std::fs::write(shared_sessions_base_directory.join("planted-0"), b"")
        .expect("plant one more entry");
    let listing_past_the_limit =
        list_foreign_sessions(shared_sessions_base_directory, runtime_directory);

    assert_eq!(
        listing_at_the_limit,
        ForeignSessionListing {
            foreign_sessions: vec![(foreign_session_id, format!("koshi-{foreign_session_id}"))],
            ..ForeignSessionListing::default()
        }
    );
    let entry_limit_unread_path = UnreadPath {
        looked_up_path: shared_sessions_base_directory.to_path_buf(),
        read_error_text: "it holds more than 65536 entries".to_string(),
    };
    let listings_past_the_limit = [
        ForeignSessionListing {
            foreign_sessions: vec![(foreign_session_id, format!("koshi-{foreign_session_id}"))],
            unread_path: Some(entry_limit_unread_path.clone()),
            ..ForeignSessionListing::default()
        },
        ForeignSessionListing {
            unread_path: Some(entry_limit_unread_path.clone()),
            ..ForeignSessionListing::default()
        },
    ];
    assert!(
        listings_past_the_limit.contains(&listing_past_the_limit),
        "the session is listed only when it was read before the stop, got {listing_past_the_limit:?}"
    );
}

#[test]
fn a_session_another_user_started_is_asked_with_an_empty_token() {
    // The session is asked over the address the shared directory names, with
    // an empty token.
    let runtime_directory = build_test_runtime_directory("shared-empty-token");
    let session_id = SessionId::new();
    let socket_address =
        koshi_ipc::endpoint::compute_socket_address(&runtime_directory, session_id);
    let expected_session_overview = build_named_session_overview("S-quiet-lake", session_id);
    let (foreign_session_thread, presented_connection_tokens) =
        spawn_foreign_session(&socket_address, expected_session_overview.clone());

    let session_overview = fetch_foreign_session_overview(
        session_id,
        &socket_address,
        Instant::now() + SESSION_ANSWER_TIMEOUT_DURATION,
    )
    .expect("the session answers");

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
    // A socket or marker with nothing listening behind it reads as a session
    // that is not running, not as one that could not answer.
    let runtime_directory = build_test_runtime_directory("shared-dead");
    let session_id = SessionId::new();
    let socket_address =
        koshi_ipc::endpoint::compute_socket_address(&runtime_directory, session_id);

    let session_lookup_error = fetch_foreign_session_overview(
        session_id,
        &socket_address,
        Instant::now() + SESSION_ANSWER_TIMEOUT_DURATION,
    )
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

#[test]
fn an_overview_ask_whose_answer_deadline_has_passed_connects_to_nothing() {
    // Nothing listens at `socket_address`.
    let runtime_directory = build_test_runtime_directory("shared-past-deadline");
    let session_id = SessionId::new();
    let socket_address =
        koshi_ipc::endpoint::compute_socket_address(&runtime_directory, session_id);

    let overview_answer =
        fetch_foreign_session_overview(session_id, &socket_address, Instant::now());

    let Err(CliError::SessionAnswerTimedOut) = overview_answer else {
        panic!("expected SessionAnswerTimedOut, got {overview_answer:?}");
    };
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

// --- Applying the working directory at send time ---------------------------

#[test]
fn a_pane_creating_command_gets_this_process_directory_at_send_time() {
    let command_with_current_directory =
        apply_current_working_directory_to_command(Command::NewPane(build_default_new_pane_args()));
    let Command::NewPane(new_pane_arguments) = command_with_current_directory else {
        panic!("the variant must not change");
    };
    assert_eq!(
        new_pane_arguments.working_directory,
        std::env::current_dir().ok()
    );

    let command_with_current_directory =
        apply_current_working_directory_to_command(Command::NewTab(NewTabArgs::default()));
    let Command::NewTab(new_tab_arguments) = command_with_current_directory else {
        panic!("the variant must not change");
    };
    assert_eq!(
        new_tab_arguments.working_directory,
        std::env::current_dir().ok()
    );

    let command_with_current_directory =
        apply_current_working_directory_to_command(Command::RunCommandPane(RunCommandPaneArgs {
            spawn_spec: SpawnSpec::build_default_shell(None, BTreeMap::new()),
            working_directory: None,
            source_pane_id: None,
            tab_id: None,
            direction: Direction::Right,
            should_stack: false,
            client_id: None,
        }));
    let Command::RunCommandPane(run_command_pane_arguments) = command_with_current_directory else {
        panic!("the variant must not change");
    };
    assert_eq!(
        run_command_pane_arguments.working_directory,
        std::env::current_dir().ok()
    );
}

#[test]
fn an_explicit_working_directory_survives_command_preparation() {
    let command = Command::NewPane(NewPaneArgs {
        working_directory: Some(PathBuf::from("/explicit")),
        ..build_default_new_pane_args()
    });
    let command_with_current_directory = apply_current_working_directory_to_command(command);
    let Command::NewPane(new_pane_arguments) = command_with_current_directory else {
        panic!("the variant must not change");
    };
    assert_eq!(
        new_pane_arguments.working_directory,
        Some(PathBuf::from("/explicit"))
    );
}

#[test]
fn a_command_without_a_directory_field_is_untouched() {
    assert_eq!(
        apply_current_working_directory_to_command(Command::Quit),
        Command::Quit
    );
    assert_eq!(
        apply_current_working_directory_to_command(Command::ToggleLockMode(
            ToggleLockModeArgs::default(),
        )),
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
                // A caller that refused this session's version has already
                // closed the connection.
                send_ipc_result_best_effort(
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

/// The Hello and the command go out back to back: a session that settles on a
/// version this build does not speak reads both. The caller gets the version
/// refusal and never a command result. A session that shares no version
/// answers every request after the failed Hello with `HelloRequired`, and does
/// not act on the command it read.
#[test]
fn a_session_speaking_three_refuses_the_caller_and_answers_no_command() {
    let runtime_directory = build_test_runtime_directory("client-protocol-three-refused");
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let (session_thread, received_requests) =
        spawn_settled_session(&runtime_directory, session_id, 3, HelloTiming::AtOnce);

    let command_error = submit_external_command_via_runtime_directory(
        &runtime_directory,
        None,
        session_id,
        Some(client_id),
        Command::TogglePaneFullscreen,
    )
    .expect_err("a session speaking 3 is below this build's floor of 4");

    let CliError::IpcUnavailable { detail } = command_error else {
        panic!("expected IpcUnavailable, got {command_error:?}");
    };
    assert_eq!(
        detail,
        "the session settled on protocol version 3, which is outside the 4 to 4 this koshi \
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
        received_requests
            .recv()
            .expect("the session read the command behind the Hello")
            .request_kind
            .get_request_kind_name(),
        "SubmitCommand",
    );

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

/// A command naming a target client costs one round trip, the same as every
/// other command: the session reads the command before it answers the Hello,
/// and still answers both in order.
#[test]
fn a_named_client_command_reaches_a_session_that_answers_the_hello_last() {
    let runtime_directory = build_test_runtime_directory("client-hello-last");
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let (session_thread, received_requests) = spawn_settled_session(
        &runtime_directory,
        session_id,
        4,
        HelloTiming::AfterTheNextRequest,
    );

    let command_result = submit_external_command_via_runtime_directory(
        &runtime_directory,
        None,
        session_id,
        Some(client_id),
        Command::TogglePaneFullscreen,
    )
    .expect("the session answers the command");

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

    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_named_client_reaches_a_session_that_speaks_four() {
    let runtime_directory = build_test_runtime_directory("client-protocol-four");
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let (session_thread, received_requests) =
        spawn_settled_session(&runtime_directory, session_id, 4, HelloTiming::AtOnce);

    let command_result = submit_external_command_via_runtime_directory(
        &runtime_directory,
        None,
        session_id,
        Some(client_id),
        Command::TogglePaneFullscreen,
    )
    .expect("a session speaking 4 reads the target client");

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
        None,
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

/// Write an empty resume file for `session_id` in `runtime_directory`, stamped
/// `age_duration` old.
fn write_aged_resume_file(runtime_directory: &Path, session_id: SessionId, age_duration: Duration) {
    std::fs::File::create(resolve_resume_file_path(runtime_directory, session_id))
        .expect("write the resume file")
        .set_modified(SystemTime::now() - age_duration)
        .expect("age the resume file");
}

/// Write an endpoint file for `session_id` in `runtime_directory` naming the
/// socket address the id gives there.
fn write_own_endpoint_file(runtime_directory: &Path, session_id: SessionId) {
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(runtime_directory, session_id),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("write the endpoint file");
}

#[cfg(unix)]
#[test]
fn thousands_of_entries_that_name_no_session_hide_no_session() {
    // 2,000 files beside the user folders, and 2,000 more inside the folder
    // holding the one session.
    use std::os::unix::fs::MetadataExt;

    let runtime_directory = build_test_runtime_directory("shared-junk");
    let shared_sessions_base_directory = build_test_runtime_directory("sj");
    let own_user_id = std::fs::metadata(&runtime_directory)
        .expect("read the runtime directory")
        .uid();
    let foreign_user_directory = shared_sessions_base_directory.join((own_user_id + 1).to_string());
    std::fs::create_dir_all(&foreign_user_directory).expect("create the other user's directory");
    for junk_index in 0..2_000 {
        std::fs::write(
            shared_sessions_base_directory.join(format!("junk-{junk_index}")),
            b"",
        )
        .expect("plant a file beside the user folders");
        std::fs::write(
            foreign_user_directory.join(format!("junk-{junk_index}")),
            b"",
        )
        .expect("plant a file inside the user folder");
    }
    let foreign_session_id = SessionId::new();
    let foreign_socket_address =
        koshi_ipc::endpoint::compute_socket_address(&foreign_user_directory, foreign_session_id);
    plant_session_socket(Path::new(&foreign_socket_address));

    let foreign_session_listing =
        list_foreign_sessions(&shared_sessions_base_directory, &runtime_directory);
    let found_socket_address = find_foreign_session_address(
        &shared_sessions_base_directory,
        &runtime_directory,
        foreign_session_id,
    )
    .expect("the shared directory is read");

    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            foreign_sessions: vec![(foreign_session_id, foreign_socket_address.clone())],
            ..ForeignSessionListing::default()
        }
    );
    assert_eq!(found_socket_address, Some(foreign_socket_address));
    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[cfg(unix)]
#[test]
fn a_lookup_by_id_finds_a_session_the_listing_counts_past_its_cap() {
    // 257 sockets of one owner: the listing stops at 256, and the lookup of
    // each id reads that id's own path.
    use std::os::unix::fs::MetadataExt;

    let runtime_directory = build_test_runtime_directory("shared-past-cap");
    let shared_sessions_base_directory = build_test_runtime_directory("pc");
    let own_user_id = std::fs::metadata(&runtime_directory)
        .expect("read the runtime directory")
        .uid();
    let foreign_user_directory = shared_sessions_base_directory.join((own_user_id + 1).to_string());
    std::fs::create_dir_all(&foreign_user_directory).expect("create the other user's directory");
    let foreign_session_ids: Vec<SessionId> = (0..MAX_SHARED_SESSION_COUNT_PER_OWNER + 1)
        .map(|_| SessionId::new())
        .collect();
    for foreign_session_id in &foreign_session_ids {
        plant_session_socket(&foreign_user_directory.join(format!("{foreign_session_id}.sock")));
    }

    let foreign_session_listing =
        list_foreign_sessions(&shared_sessions_base_directory, &runtime_directory);
    let unlisted_session_ids: Vec<SessionId> = foreign_session_ids
        .iter()
        .copied()
        .filter(|foreign_session_id| {
            !foreign_session_listing
                .foreign_sessions
                .iter()
                .any(|(listed_session_id, _)| listed_session_id == foreign_session_id)
        })
        .collect();

    assert_eq!(foreign_session_listing.unlisted_session_count, 1);
    assert_eq!(unlisted_session_ids.len(), 1);
    assert_eq!(
        find_foreign_session_address(
            &shared_sessions_base_directory,
            &runtime_directory,
            unlisted_session_ids[0],
        )
        .expect("the shared directory is read"),
        Some(koshi_ipc::endpoint::compute_socket_address(
            &foreign_user_directory,
            unlisted_session_ids[0]
        ))
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[cfg(unix)]
#[test]
fn an_id_sockets_in_two_folders_advertise_is_reached_through_neither() {
    // Both folders belong to the user running the test.
    use std::os::unix::fs::MetadataExt;

    let runtime_directory = build_test_runtime_directory("shared-duplicate");
    let shared_sessions_base_directory = build_test_runtime_directory("sd");
    let own_user_id = std::fs::metadata(&runtime_directory)
        .expect("read the runtime directory")
        .uid();
    let duplicated_session_id = SessionId::new();
    for folder_offset in [1, 2] {
        let user_directory =
            shared_sessions_base_directory.join((own_user_id + folder_offset).to_string());
        std::fs::create_dir_all(&user_directory).expect("create a user's directory");
        plant_session_socket(&user_directory.join(format!("{duplicated_session_id}.sock")));
    }
    let expected_refusal_text = format!(
        "session {duplicated_session_id} is advertised 2 times in the shared directory, by user \
         id {own_user_id}; koshi reaches none of them"
    );

    let foreign_session_listing =
        list_foreign_sessions(&shared_sessions_base_directory, &runtime_directory);
    let lookup_result = find_foreign_session_address(
        &shared_sessions_base_directory,
        &runtime_directory,
        duplicated_session_id,
    );
    let endpoint_lookup_result = load_session_endpoint(
        &runtime_directory,
        Some(&shared_sessions_base_directory),
        duplicated_session_id,
    );

    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            duplicated_sessions: vec![DuplicatedForeignSession {
                session_id: duplicated_session_id,
                advertisement_count: 2,
                owner_user_ids: vec![own_user_id],
            }],
            ..ForeignSessionListing::default()
        }
    );
    let Err(ForeignSessionLookupError::AdvertisedMoreThanOnce { duplicated_session }) =
        lookup_result
    else {
        panic!("expected AdvertisedMoreThanOnce, got {lookup_result:?}");
    };
    assert_eq!(duplicated_session.to_string(), expected_refusal_text);
    let Err(CliError::IpcUnavailable { detail }) = endpoint_lookup_result else {
        panic!("expected IpcUnavailable, got {endpoint_lookup_result:?}");
    };
    assert_eq!(detail, expected_refusal_text);
    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[test]
fn a_duplicated_session_names_each_owner_once() {
    let session_id = SessionId::new();

    assert_eq!(
        DuplicatedForeignSession {
            session_id,
            advertisement_count: 3,
            owner_user_ids: vec![1001, 1002],
        }
        .to_string(),
        format!(
            "session {session_id} is advertised 3 times in the shared directory, by user ids \
             1001, 1002; koshi reaches none of them"
        )
    );
}

#[cfg(unix)]
#[test]
fn a_resume_file_of_this_users_keeps_a_foreign_socket_of_that_id_out() {
    use std::os::unix::fs::MetadataExt;

    let runtime_directory = build_test_runtime_directory("shared-own-resume");
    let shared_sessions_base_directory = build_test_runtime_directory("or");
    let own_user_id = std::fs::metadata(&runtime_directory)
        .expect("read the runtime directory")
        .uid();
    let foreign_user_directory = shared_sessions_base_directory.join((own_user_id + 1).to_string());
    std::fs::create_dir_all(&foreign_user_directory).expect("create the other user's directory");
    let dead_swap_session_id = SessionId::new();
    let restarting_session_id = SessionId::new();
    for own_session_id in [dead_swap_session_id, restarting_session_id] {
        plant_session_socket(&foreign_user_directory.join(format!("{own_session_id}.sock")));
    }
    write_aged_resume_file(
        &runtime_directory,
        dead_swap_session_id,
        RESTART_WINDOW_DURATION + Duration::from_secs(1),
    );
    write_aged_resume_file(&runtime_directory, restarting_session_id, Duration::ZERO);

    let foreign_session_listing =
        list_foreign_sessions(&shared_sessions_base_directory, &runtime_directory);
    let dead_swap_lookup = find_foreign_session_address(
        &shared_sessions_base_directory,
        &runtime_directory,
        dead_swap_session_id,
    )
    .expect("the shared directory is read");
    let dead_swap_endpoint_lookup = load_session_endpoint(
        &runtime_directory,
        Some(&shared_sessions_base_directory),
        dead_swap_session_id,
    );
    let restarting_endpoint_lookup = load_session_endpoint(
        &runtime_directory,
        Some(&shared_sessions_base_directory),
        restarting_session_id,
    );

    assert_eq!(foreign_session_listing, ForeignSessionListing::default());
    assert_eq!(dead_swap_lookup, None);
    let Err(CliError::SessionNotFound { session_name }) = dead_swap_endpoint_lookup else {
        panic!("expected SessionNotFound, got {dead_swap_endpoint_lookup:?}");
    };
    assert_eq!(session_name, dead_swap_session_id.to_string());
    let Err(CliError::IpcUnavailable { detail }) = restarting_endpoint_lookup else {
        panic!("expected IpcUnavailable, got {restarting_endpoint_lookup:?}");
    };
    assert_eq!(
        detail,
        format!("session {restarting_session_id} is restarting; ask again in a moment")
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[cfg(windows)]
#[test]
fn a_lookup_by_id_finds_the_marker_of_that_id_and_no_other() {
    let runtime_directory = build_test_runtime_directory("shared-marker-lookup");
    let shared_sessions_base_directory = build_test_runtime_directory("shared-marker-lookup-base");
    let foreign_session_id = SessionId::new();
    let own_session_id = SessionId::new();
    for marked_session_id in [foreign_session_id, own_session_id] {
        std::fs::write(
            shared_sessions_base_directory.join(marked_session_id.to_string()),
            b"",
        )
        .expect("plant a marker");
    }
    write_own_endpoint_file(&runtime_directory, own_session_id);

    let found_socket_addresses: Vec<Option<String>> =
        [foreign_session_id, own_session_id, SessionId::new()]
            .into_iter()
            .map(|session_id| {
                find_foreign_session_address(
                    &shared_sessions_base_directory,
                    &runtime_directory,
                    session_id,
                )
                .expect("the shared directory is read")
            })
            .collect();

    assert_eq!(
        found_socket_addresses,
        vec![Some(format!("koshi-{foreign_session_id}")), None, None]
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[test]
fn this_users_sessions_are_the_advertised_ones_and_the_ones_restarting() {
    let runtime_directory = build_test_runtime_directory("own-sessions");
    let mut advertised_session_ids = vec![SessionId::new(), SessionId::new()];
    for advertised_session_id in &advertised_session_ids {
        write_own_endpoint_file(&runtime_directory, *advertised_session_id);
    }
    write_aged_resume_file(
        &runtime_directory,
        advertised_session_ids[0],
        Duration::ZERO,
    );
    let restarting_session_id = SessionId::new();
    write_aged_resume_file(&runtime_directory, restarting_session_id, Duration::ZERO);
    write_aged_resume_file(
        &runtime_directory,
        SessionId::new(),
        RESTART_WINDOW_DURATION + Duration::from_secs(1),
    );

    let mut own_session_ids =
        list_own_sessions(&runtime_directory).expect("read the runtime directory");

    own_session_ids.sort();
    advertised_session_ids.push(restarting_session_id);
    advertised_session_ids.sort();
    assert_eq!(own_session_ids, advertised_session_ids);
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn every_session_is_answered_in_the_order_it_was_asked() {
    let session_asks: Vec<usize> = (0..40).collect();

    let session_answers = ask_sessions_at_once(
        &session_asks,
        Instant::now() + SESSION_ANSWER_TIMEOUT_DURATION,
        |session_ask| Ok(*session_ask * 10),
    );

    assert_eq!(
        session_answers
            .into_iter()
            .map(|session_answer| session_answer.expect("every ask answers"))
            .collect::<Vec<usize>>(),
        (0..40)
            .map(|ask_index| ask_index * 10)
            .collect::<Vec<usize>>()
    );
}

#[test]
fn a_session_still_unasked_at_the_deadline_is_not_asked_and_answers_that_it_timed_out() {
    let asked_session_count = std::sync::atomic::AtomicUsize::new(0);

    let session_answers = ask_sessions_at_once(&[1, 2, 3], Instant::now(), |_| {
        asked_session_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });

    assert_eq!(asked_session_count.load(Ordering::SeqCst), 0);
    assert_eq!(session_answers.len(), 3);
    for session_answer in session_answers {
        let Err(CliError::SessionAnswerTimedOut) = session_answer else {
            panic!("expected SessionAnswerTimedOut, got {session_answer:?}");
        };
    }
}

#[test]
fn sixteen_sessions_are_asked_at_the_same_time() {
    // Each ask waits until all 16 have started, for at most 5 seconds.
    let started_ask_count = std::sync::Mutex::new(0_usize);
    let ask_started = std::sync::Condvar::new();
    let session_asks: Vec<usize> = (0..MAX_SESSIONS_ASKED_AT_ONCE).collect();

    let session_answers = ask_sessions_at_once(
        &session_asks,
        Instant::now() + Duration::from_secs(10),
        |_| {
            let mut started_count = started_ask_count.lock().expect("the count is not poisoned");
            *started_count += 1;
            ask_started.notify_all();
            let (started_count, wait_result) = ask_started
                .wait_timeout_while(started_count, Duration::from_secs(5), |started_count| {
                    *started_count < MAX_SESSIONS_ASKED_AT_ONCE
                })
                .expect("the count is not poisoned");
            drop(started_count);
            Ok(wait_result.timed_out())
        },
    );

    assert_eq!(
        session_answers
            .into_iter()
            .map(|session_answer| session_answer.expect("every ask answers"))
            .collect::<Vec<bool>>(),
        vec![false; MAX_SESSIONS_ASKED_AT_ONCE]
    );
}

#[test]
fn a_panic_in_an_ask_on_a_helper_thread_reaches_the_caller() {
    // The calling thread's ask waits until a helper thread starts an ask, for
    // at most 5 seconds. Every ask on a helper thread panics.
    let has_helper_ask_started = std::sync::Mutex::new(false);
    let helper_ask_started = std::sync::Condvar::new();

    let ask_outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ask_sessions_at_once(
            &[0_usize, 1],
            Instant::now() + SESSION_ANSWER_TIMEOUT_DURATION,
            |_| {
                if std::thread::current().name() == Some("koshi-session-ask") {
                    *has_helper_ask_started
                        .lock()
                        .expect("the flag is not poisoned") = true;
                    helper_ask_started.notify_all();
                    panic!("an ask on a helper thread panicked");
                }
                let has_helper_ask_started = has_helper_ask_started
                    .lock()
                    .expect("the flag is not poisoned");
                let _ = helper_ask_started
                    .wait_timeout_while(
                        has_helper_ask_started,
                        Duration::from_secs(5),
                        |has_helper_ask_started| !*has_helper_ask_started,
                    )
                    .expect("the flag is not poisoned");
                Ok(())
            },
        )
    }));

    let panic_payload = ask_outcome.expect_err("the panic reaches the caller");
    assert_eq!(
        panic_payload.downcast_ref::<&str>(),
        Some(&"an ask on a helper thread panicked")
    );
}

#[test]
fn only_a_path_that_is_gone_not_a_directory_or_closed_to_this_user_advertises_nothing() {
    for unadvertised_error_kind in [
        std::io::ErrorKind::NotFound,
        std::io::ErrorKind::NotADirectory,
        std::io::ErrorKind::PermissionDenied,
    ] {
        assert!(
            is_unadvertised_path_error(&std::io::Error::from(unadvertised_error_kind)),
            "{unadvertised_error_kind:?} advertises nothing"
        );
    }
    for failed_read_error_kind in [
        std::io::ErrorKind::Other,
        std::io::ErrorKind::OutOfMemory,
        std::io::ErrorKind::Interrupted,
        std::io::ErrorKind::InvalidData,
    ] {
        assert!(
            !is_unadvertised_path_error(&std::io::Error::from(failed_read_error_kind)),
            "{failed_read_error_kind:?} is a failed read"
        );
    }
}

#[test]
fn the_first_failed_read_is_kept_and_a_path_that_advertises_nothing_is_not() {
    let mut foreign_session_listing = ForeignSessionListing::default();

    foreign_session_listing.record_failed_read(
        Path::new("/home/user/shared/1001"),
        &std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    );
    let unread_path_after_closed_folder = foreign_session_listing.unread_path.clone();
    foreign_session_listing.record_failed_read(
        Path::new("/home/user/shared/1002"),
        &std::io::Error::other("first failure"),
    );
    foreign_session_listing.record_failed_read(
        Path::new("/home/user/shared/1003"),
        &std::io::Error::other("second failure"),
    );

    assert_eq!(unread_path_after_closed_folder, None);
    let expected_unread_path = UnreadPath {
        looked_up_path: PathBuf::from("/home/user/shared/1002"),
        read_error_text: "first failure".to_string(),
    };
    assert_eq!(
        expected_unread_path.to_string(),
        "/home/user/shared/1002 could not be read: first failure"
    );
    assert_eq!(
        foreign_session_listing.unread_path,
        Some(expected_unread_path)
    );
}

#[cfg(unix)]
#[test]
fn a_folder_its_owner_closed_to_other_users_advertises_nothing() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let runtime_directory = build_test_runtime_directory("shared-closed");
    let shared_sessions_base_directory = build_test_runtime_directory("cf");
    let own_user_id = std::fs::metadata(&runtime_directory)
        .expect("read the runtime directory")
        .uid();
    let closed_user_directory = shared_sessions_base_directory.join((own_user_id + 1).to_string());
    let open_user_directory = shared_sessions_base_directory.join((own_user_id + 2).to_string());
    let closed_session_id = SessionId::new();
    let open_session_id = SessionId::new();
    for (user_directory, session_id) in [
        (&closed_user_directory, closed_session_id),
        (&open_user_directory, open_session_id),
    ] {
        std::fs::create_dir_all(user_directory).expect("create a user's directory");
        plant_session_socket(&user_directory.join(format!("{session_id}.sock")));
    }
    std::fs::set_permissions(
        &closed_user_directory,
        std::fs::Permissions::from_mode(0o000),
    )
    .expect("close the folder to every user");
    if std::fs::read_dir(&closed_user_directory).is_ok() {
        eprintln!(
            "skipped `a_folder_its_owner_closed_to_other_users_advertises_nothing`: \
             this user reads through a mode-000 directory"
        );
        let _ = std::fs::set_permissions(
            &closed_user_directory,
            std::fs::Permissions::from_mode(0o755),
        );
        let _ = std::fs::remove_dir_all(&runtime_directory);
        let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
        return;
    }

    let foreign_session_listing =
        list_foreign_sessions(&shared_sessions_base_directory, &runtime_directory);
    let closed_session_lookup = find_foreign_session_address(
        &shared_sessions_base_directory,
        &runtime_directory,
        closed_session_id,
    );
    let open_session_lookup = find_foreign_session_address(
        &shared_sessions_base_directory,
        &runtime_directory,
        open_session_id,
    );

    let _ = std::fs::set_permissions(
        &closed_user_directory,
        std::fs::Permissions::from_mode(0o755),
    );
    let open_socket_address =
        koshi_ipc::endpoint::compute_socket_address(&open_user_directory, open_session_id);
    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            foreign_sessions: vec![(open_session_id, open_socket_address.clone())],
            ..ForeignSessionListing::default()
        }
    );
    let Ok(None) = closed_session_lookup else {
        panic!("expected Ok(None), got {closed_session_lookup:?}");
    };
    let Ok(Some(found_socket_address)) = open_session_lookup else {
        panic!("expected the open folder's socket, got {open_session_lookup:?}");
    };
    assert_eq!(found_socket_address, open_socket_address);
    let _ = std::fs::remove_dir_all(&runtime_directory);
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[cfg(unix)]
#[test]
fn a_shared_directory_whose_read_fails_another_way_is_kept_and_refuses_a_lookup() {
    // A link to itself fails every read with `ELOOP`.
    let runtime_directory = build_test_runtime_directory("shared-loop");
    let looping_shared_directory = runtime_directory.join("looping");
    std::os::unix::fs::symlink("looping", &looping_shared_directory)
        .expect("link the shared directory to itself");
    let shared_read_error =
        std::fs::read_dir(&looping_shared_directory).expect_err("a link to itself cannot be read");
    let unread_shared_directory =
        UnreadPath::from_read_error(&looping_shared_directory, &shared_read_error);

    let foreign_session_listing =
        list_foreign_sessions(&looping_shared_directory, &runtime_directory);
    let lookup_error = find_foreign_session_address(
        &looping_shared_directory,
        &runtime_directory,
        SessionId::new(),
    );

    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            unread_path: Some(unread_shared_directory.clone()),
            ..ForeignSessionListing::default()
        }
    );
    let Err(ForeignSessionLookupError::PathUnreadable { unread_path }) = lookup_error else {
        panic!("expected PathUnreadable, got {lookup_error:?}");
    };
    assert_eq!(unread_path, unread_shared_directory);
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

/// The token a test's caller connected under, which the wait watches for a
/// change.
const OLD_CONNECTION_TOKEN: &str = "the token this caller connected under";

/// The token the image replacing the server mints when it binds again.
const NEW_CONNECTION_TOKEN: &str = "the token the new image minted";

#[test]
fn wait_for_new_session_endpoint_takes_the_endpoint_file_the_moment_it_names_another_token() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let session_id = SessionId::new();
    let advertised_endpoint = write_session_endpoint_file(
        runtime_directory.path(),
        session_id,
        NEW_CONNECTION_TOKEN,
        4321,
    );

    let wait_deadline = Instant::now() + Duration::from_secs(10);
    assert_eq!(
        wait_for_new_session_endpoint(
            runtime_directory.path(),
            session_id,
            &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
            wait_deadline,
        ),
        Some(advertised_endpoint)
    );
    assert!(
        Instant::now() < wait_deadline,
        "the wait returned before its deadline"
    );
}

#[test]
fn wait_for_new_session_endpoint_reads_the_endpoint_file_again_until_the_token_changes() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let session_id = SessionId::new();
    write_session_endpoint_file(
        runtime_directory.path(),
        session_id,
        OLD_CONNECTION_TOKEN,
        4321,
    );

    let runtime_directory_path = runtime_directory.path().to_path_buf();
    let endpoint_writer_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        write_session_endpoint_file(
            &runtime_directory_path,
            session_id,
            NEW_CONNECTION_TOKEN,
            4321,
        )
    });

    let wait_deadline = Instant::now() + Duration::from_secs(10);
    let new_endpoint = wait_for_new_session_endpoint(
        runtime_directory.path(),
        session_id,
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        wait_deadline,
    );
    let advertised_endpoint = endpoint_writer_thread
        .join()
        .expect("the writing thread finished");
    assert_eq!(new_endpoint, Some(advertised_endpoint));
    assert!(
        Instant::now() < wait_deadline,
        "the wait returned before its deadline"
    );
}

#[test]
fn wait_for_new_session_endpoint_ignores_an_endpoint_file_still_naming_the_old_token() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let session_id = SessionId::new();
    // A different process id under the same token. The wait compares the token
    // alone.
    write_session_endpoint_file(
        runtime_directory.path(),
        session_id,
        OLD_CONNECTION_TOKEN,
        9999,
    );

    assert_eq!(
        wait_for_new_session_endpoint(
            runtime_directory.path(),
            session_id,
            &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
            Instant::now(),
        ),
        None
    );
}

#[test]
fn wait_for_new_session_endpoint_ends_at_its_deadline_when_the_session_advertises_nothing() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    // No endpoint file exists.
    assert_eq!(
        wait_for_new_session_endpoint(
            runtime_directory.path(),
            SessionId::new(),
            &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
            Instant::now(),
        ),
        None
    );
}

#[test]
fn wait_for_new_session_endpoint_with_a_deadline_already_past_still_takes_a_session_that_is_back() {
    // The wait reads the endpoint file once before it checks the deadline. A
    // session that came back before a deadline already past is still joined.
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let session_id = SessionId::new();
    let advertised_endpoint = write_session_endpoint_file(
        runtime_directory.path(),
        session_id,
        NEW_CONNECTION_TOKEN,
        4321,
    );

    assert_eq!(
        wait_for_new_session_endpoint(
            runtime_directory.path(),
            session_id,
            &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
            Instant::now() - Duration::from_secs(1),
        ),
        Some(advertised_endpoint)
    );
}

#[test]
fn wait_for_new_session_endpoint_reads_past_an_endpoint_file_this_build_cannot_read() {
    // An endpoint file holding bytes no build reads is passed over, and the
    // wait keeps reading until its deadline.
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let session_id = SessionId::new();
    std::fs::write(
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), session_id),
        b"{",
    )
    .expect("write a half endpoint file");

    let wait_deadline = Instant::now() + Duration::from_millis(200);
    assert_eq!(
        wait_for_new_session_endpoint(
            runtime_directory.path(),
            session_id,
            &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
            wait_deadline,
        ),
        None
    );
    assert!(
        Instant::now() >= wait_deadline,
        "the wait sat out its whole window rather than giving up on the first read"
    );
}

#[test]
fn a_session_refusing_this_builds_protocol_version_is_asked_again_once_it_restarts() {
    let runtime_directory = build_test_runtime_directory("refused-version-restart");
    let session_id = SessionId::new();
    let (refusing_session_thread, _refused_envelopes) =
        spawn_fake_session(&runtime_directory, session_id, SessionScript::RefuseVersion);
    let restart_runtime_directory = runtime_directory.clone();
    let restarted_session_thread = std::thread::spawn(move || {
        refusing_session_thread
            .join()
            .expect("the refusing session exits");
        let (accepting_session_thread, submitted_envelopes) = spawn_fake_session(
            &restart_runtime_directory,
            session_id,
            SessionScript::AcceptAndApply,
        );
        accepting_session_thread
            .join()
            .expect("the restarted session exits");
        submitted_envelopes
            .recv()
            .expect("the restarted session read one command")
    });

    let command_result = submit_external_command_via_runtime_directory(
        &runtime_directory,
        None,
        session_id,
        None,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect("the restarted session answers");

    let command_envelope = restarted_session_thread
        .join()
        .expect("the restart thread exits");
    assert_eq!(
        command_result,
        CommandResult::Ok {
            command_id: command_envelope.command_id,
            emitted_events: Vec::new(),
        },
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn wait_for_refused_session_restart_gives_another_users_session_its_refusal_at_once() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let foreign_endpoint = build_foreign_session_endpoint("/home/user/shared.sock".to_string());
    let wait_started_at = Instant::now();

    let wait_result = wait_for_refused_session_restart(
        runtime_directory.path(),
        SessionId::new(),
        &foreign_endpoint,
        VERSION_REFUSAL_SENTENCE.to_string(),
        None,
    );

    let Err(CliError::ProtocolVersionRefused { detail }) = wait_result else {
        panic!("expected ProtocolVersionRefused, got {wait_result:?}");
    };
    assert_eq!(detail, VERSION_REFUSAL_SENTENCE);
    assert!(wait_started_at.elapsed() < Duration::from_secs(1));
}

#[test]
fn wait_for_refused_session_restart_names_kill_session_for_a_session_that_wrote_no_program_file() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let session_id = SessionId::new();
    let refused_endpoint = write_session_endpoint_file(
        runtime_directory.path(),
        session_id,
        OLD_CONNECTION_TOKEN,
        5000,
    );

    let wait_result = wait_for_refused_session_restart(
        runtime_directory.path(),
        session_id,
        &refused_endpoint,
        VERSION_REFUSAL_SENTENCE.to_string(),
        Some(Instant::now() + Duration::from_millis(200)),
    );

    let Err(CliError::ProtocolVersionRefused { detail }) = wait_result else {
        panic!("expected ProtocolVersionRefused, got {wait_result:?}");
    };
    assert_eq!(
        detail,
        format!(
            "{VERSION_REFUSAL_SENTENCE}; it runs a koshi older than {} that cannot restart into \
             it; end it with: koshi kill-session {session_id}",
            env!("CARGO_PKG_VERSION")
        )
    );
}

/// The endpoint file of `session_id` in `runtime_directory`, naming this
/// process, beside a program file this process wrote naming `build_version`
/// and `program_path`. Hands back the endpoint file.
fn write_recorded_session_files(
    runtime_directory: &Path,
    session_id: SessionId,
    build_version: &str,
    program_path: &Path,
) -> EndpointFile {
    let refused_endpoint = write_session_endpoint_file(
        runtime_directory,
        session_id,
        OLD_CONNECTION_TOKEN,
        std::process::id(),
    );
    ServerProgramFile {
        process_id: std::process::id(),
        build_version: build_version.to_string(),
        program_path: program_path.to_string_lossy().into_owned(),
    }
    .write_to_path(&ServerProgramFile::resolve_session_program_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("the program file is written");
    refused_endpoint
}

#[test]
fn wait_for_refused_session_restart_names_the_newer_koshi_a_session_runs_at_once() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let session_id = SessionId::new();
    let refused_endpoint = write_recorded_session_files(
        runtime_directory.path(),
        session_id,
        "999.0.0",
        Path::new("/opt/koshi/999.0.0/koshi"),
    );
    let wait_started_at = Instant::now();

    let wait_result = wait_for_refused_session_restart(
        runtime_directory.path(),
        session_id,
        &refused_endpoint,
        VERSION_REFUSAL_SENTENCE.to_string(),
        None,
    );

    let Err(CliError::ProtocolVersionRefused { detail }) = wait_result else {
        panic!("expected ProtocolVersionRefused, got {wait_result:?}");
    };
    assert_eq!(
        detail,
        format!(
            "{VERSION_REFUSAL_SENTENCE}; it runs koshi 999.0.0 from /opt/koshi/999.0.0/koshi, \
             which is newer than this koshi {}; use /opt/koshi/999.0.0/koshi for it",
            env!("CARGO_PKG_VERSION")
        )
    );
    assert!(wait_started_at.elapsed() < Duration::from_secs(1));
}

#[test]
fn wait_for_refused_session_restart_names_the_other_program_file_of_an_older_session_at_once() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let session_id = SessionId::new();
    let other_program_path = runtime_directory.path().join("other-koshi");
    std::fs::write(&other_program_path, b"").expect("the other program file is written");
    let refused_endpoint = write_recorded_session_files(
        runtime_directory.path(),
        session_id,
        "0.0.1",
        &other_program_path,
    );
    let wait_started_at = Instant::now();

    let wait_result = wait_for_refused_session_restart(
        runtime_directory.path(),
        session_id,
        &refused_endpoint,
        VERSION_REFUSAL_SENTENCE.to_string(),
        None,
    );

    let Err(CliError::ProtocolVersionRefused { detail }) = wait_result else {
        panic!("expected ProtocolVersionRefused, got {wait_result:?}");
    };
    assert_eq!(
        detail,
        format!(
            "{VERSION_REFUSAL_SENTENCE}; it runs koshi 0.0.1 from {}, a program file this koshi \
             does not replace; use that koshi for it, or end it with: koshi kill-session \
             {session_id}",
            other_program_path.display()
        )
    );
    assert!(wait_started_at.elapsed() < Duration::from_secs(1));
}

#[test]
fn wait_for_refused_session_restart_waits_for_an_older_session_on_this_program_file() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let session_id = SessionId::new();
    let refused_endpoint = write_recorded_session_files(
        runtime_directory.path(),
        session_id,
        "0.0.1",
        &std::env::current_exe().expect("the test binary has a path"),
    );
    let restart_runtime_directory = runtime_directory.path().to_path_buf();
    let restarting_session_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        write_session_endpoint_file(
            &restart_runtime_directory,
            session_id,
            NEW_CONNECTION_TOKEN,
            std::process::id(),
        )
    });

    let restarted_endpoint = wait_for_refused_session_restart(
        runtime_directory.path(),
        session_id,
        &refused_endpoint,
        VERSION_REFUSAL_SENTENCE.to_string(),
        None,
    )
    .expect("the session restarts within the wait");

    assert_eq!(
        restarted_endpoint,
        restarting_session_thread
            .join()
            .expect("the restarting session writes its endpoint file")
    );
}

#[test]
fn wait_for_refused_session_restart_says_an_older_session_on_this_program_file_tries_again() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let session_id = SessionId::new();
    let refused_endpoint = write_recorded_session_files(
        runtime_directory.path(),
        session_id,
        "0.0.1",
        &std::env::current_exe().expect("the test binary has a path"),
    );

    let wait_result = wait_for_refused_session_restart(
        runtime_directory.path(),
        session_id,
        &refused_endpoint,
        VERSION_REFUSAL_SENTENCE.to_string(),
        Some(Instant::now() + Duration::from_millis(200)),
    );

    let Err(CliError::ProtocolVersionRefused { detail }) = wait_result else {
        panic!("expected ProtocolVersionRefused, got {wait_result:?}");
    };
    assert_eq!(
        detail,
        format!(
            "{VERSION_REFUSAL_SENTENCE}; it runs koshi 0.0.1 and has not restarted into this \
             koshi {} yet; it tries again at each command from this koshi, and its log says \
             what stopped it",
            env!("CARGO_PKG_VERSION")
        )
    );
}

#[test]
fn wait_for_refused_session_restart_waits_through_a_restart_that_has_started() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let session_id = SessionId::new();
    let refused_endpoint = write_session_endpoint_file(
        runtime_directory.path(),
        session_id,
        OLD_CONNECTION_TOKEN,
        5000,
    );
    std::fs::remove_file(EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        session_id,
    ))
    .expect("the swap takes the endpoint file away");
    std::fs::write(
        resolve_resume_file_path(runtime_directory.path(), session_id),
        b"",
    )
    .expect("the swap writes its resume file");
    let restart_runtime_directory = runtime_directory.path().to_path_buf();
    let restarted_session_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        write_session_endpoint_file(
            &restart_runtime_directory,
            session_id,
            NEW_CONNECTION_TOKEN,
            5000,
        )
    });

    let wait_result = wait_for_refused_session_restart(
        runtime_directory.path(),
        session_id,
        &refused_endpoint,
        VERSION_REFUSAL_SENTENCE.to_string(),
        None,
    );

    let restarted_endpoint = restarted_session_thread
        .join()
        .expect("the restart thread exits");
    assert_eq!(
        wait_result.expect("the session restarted"),
        restarted_endpoint
    );
}
