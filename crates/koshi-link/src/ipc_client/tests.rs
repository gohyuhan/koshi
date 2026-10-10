//! Tests for the CLI side of the control socket, against a scripted
//! stand-in session serving a real socket.

use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use koshi_core::command::{NewPaneArgs, NewPanePlacement, NewTabArgs, ToggleLockModeArgs};
use koshi_core::discovery::SessionDiscovery;
use koshi_core::geometry::Direction;
use koshi_core::ids::{PaneId, SessionId};
use koshi_ipc::endpoint::RESTART_WINDOW_DURATION;
use koshi_ipc::layout::TabLayout;
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcResponse, MIN_PROTOCOL_VERSION};
use koshi_ipc::transport::Listener;
use koshi_layout::tree::LayoutNode;
use koshi_test_support::fixtures::{
    build_test_runtime_directory, close_connection_after_peer_hangs_up,
    spawn_previous_release_session, spawn_session_listening_before_it_advertises,
    write_koshi_0_1_0_window_endpoint_file, write_session_endpoint_file,
    KOSHI_0_2_0_HELLO_ANSWER_TEXT, KOSHI_0_2_0_RESTART_REFUSAL_TEXT, KOSHI_0_4_0_HELLO_ANSWER_TEXT,
    KOSHI_0_4_0_RESTARTING_ANSWER_TEXT, PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT,
};

use super::*;
use koshi_ipc::protocol::{IpcErrorPayload, PROTOCOL_VERSION};

/// A `new-pane` request with nothing chosen: the focused pane splits rightward.
fn build_default_new_pane_args() -> NewPaneArgs {
    NewPaneArgs {
        placement: NewPanePlacement::Split {
            source_pane_id: None,
            tab_id: None,
            direction: Direction::Right,
        },
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
const VERSION_REFUSAL_SENTENCE: &str = "this server speaks an older protocol than the caller";

/// Serve one scripted connection per entry of `session_scripts`, in order, for
/// `session_id` at `runtime_directory`: write the endpoint file, accept one
/// caller, answer per the entry, and close the connection once the caller hung
/// up. Before each connection after the first, the session restarts on the
/// socket it already holds: it writes the endpoint file again under a new
/// token. The returned receiver carries the envelope each caller submitted, in
/// order.
fn spawn_fake_session(
    runtime_directory: &Path,
    session_id: SessionId,
    session_scripts: Vec<SessionScript>,
) -> (JoinHandle<()>, Receiver<CommandEnvelope>) {
    let session_socket_address =
        koshi_ipc::endpoint::compute_socket_address(runtime_directory, session_id);
    let session_listener = Listener::bind(&session_socket_address).expect("bind fake session");
    let endpoint_file_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    let advertise_session = move |session_connection_token: &ConnectionToken| {
        EndpointFile {
            socket_address: session_socket_address.clone(),
            connection_token: session_connection_token.clone(),
            process_id: std::process::id(),
        }
        .write_to_path(&endpoint_file_path)
        .expect("write endpoint file");
    };
    let mut session_connection_token = ConnectionToken::generate();
    advertise_session(&session_connection_token);

    let (submitted_envelope_sender, submitted_envelope_receiver) = mpsc::channel();
    let session_server_thread = std::thread::spawn(move || {
        for (connection_index, session_script) in session_scripts.into_iter().enumerate() {
            if connection_index > 0 {
                session_connection_token = ConnectionToken::generate();
                advertise_session(&session_connection_token);
            }
            let mut session_connection = session_listener.accept().expect("accept the CLI");
            answer_scripted_connection(
                &mut session_connection,
                &session_connection_token,
                session_script,
                &submitted_envelope_sender,
            );
            close_connection_after_peer_hangs_up(session_connection);
        }
    });
    (session_server_thread, submitted_envelope_receiver)
}

/// Read the Hello and the `SubmitCommand` behind it from `session_connection`,
/// check that the Hello presents `session_connection_token`, send the
/// submitted envelope down `submitted_envelope_sender`, and answer both
/// requests per `session_script`.
fn answer_scripted_connection(
    session_connection: &mut Connection,
    session_connection_token: &ConnectionToken,
    session_script: SessionScript,
    submitted_envelope_sender: &mpsc::Sender<CommandEnvelope>,
) {
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
        presented_token, session_connection_token,
        "the CLI presents the endpoint's token"
    );
    submitted_envelope_sender
        .send((**command_envelope).clone())
        .expect("report the envelope submitted");

    match session_script {
        SessionScript::AcceptAndApply => {
            send_ipc_result(
                session_connection,
                hello_request.request_id,
                IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    build_version: env!("CARGO_PKG_VERSION").to_string(),
                },
            );
            send_ipc_result(
                session_connection,
                submit_request.request_id,
                IpcResult::CommandResult(CommandResult::Ok {
                    command_id: command_envelope.command_id,
                    emitted_events: Vec::new(),
                }),
            );
        }
        SessionScript::RefuseHello => {
            send_ipc_result(
                session_connection,
                hello_request.request_id,
                IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::BadToken,
                    message: "the token presented does not match this Koshi's".to_string(),
                }),
            );
            send_ipc_result_best_effort(
                session_connection,
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
                session_connection,
                hello_request.request_id,
                IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::UnsupportedVersion,
                    message: VERSION_REFUSAL_SENTENCE.to_string(),
                }),
            );
            send_ipc_result_best_effort(
                session_connection,
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
                session_connection,
                hello_request.request_id,
                IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    build_version: env!("CARGO_PKG_VERSION").to_string(),
                },
            );
            send_ipc_result(
                session_connection,
                submit_request.request_id,
                IpcResult::CommandResult(CommandResult::Rejected {
                    command_id: command_envelope.command_id,
                    reason: koshi_core::event::RejectReason::Unauthorized,
                    help: Some("\u{1b}[2Jno client is attached\u{7f} to the session".to_string()),
                }),
            );
        }
    }
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
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let cli_context = build_in_session_context(session_id);
    let (session_server_thread, submitted_envelopes) = spawn_fake_session(
        runtime_directory.path(),
        session_id,
        vec![SessionScript::AcceptAndApply],
    );

    let command_result = submit_command_via_runtime_directory(
        runtime_directory.path(),
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
                runtime_directory.path(),
                session_id
            )),
        ),
    );
}

#[test]
fn a_rejected_command_comes_back_with_reason_and_help() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, _submitted_envelopes) = spawn_fake_session(
        runtime_directory.path(),
        session_id,
        vec![SessionScript::RejectCommand],
    );

    let command_result = submit_command_via_runtime_directory(
        runtime_directory.path(),
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
}

#[test]
fn a_missing_endpoint_file_reports_the_session_not_running() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();

    let command_error = submit_command_via_runtime_directory(
        runtime_directory.path(),
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
}

#[test]
fn an_endpoint_nothing_listens_behind_reports_the_session_not_running() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(
            runtime_directory.path(),
            session_id,
        ),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        session_id,
    ))
    .expect("write endpoint file");

    let command_error = submit_command_via_runtime_directory(
        runtime_directory.path(),
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
}

#[test]
fn an_endpoint_file_that_holds_no_endpoint_reports_ipc_unavailable() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let endpoint_file_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), session_id);
    std::fs::write(&endpoint_file_path, b"not an endpoint file")
        .expect("write the unreadable endpoint file");
    let endpoint_file_error = EndpointFile::load_from_path(&endpoint_file_path)
        .expect_err("the bytes hold no endpoint file");

    let endpoint_error = load_session_endpoint(runtime_directory.path(), None, session_id)
        .expect_err("the endpoint file cannot be read");

    let CliError::IpcUnavailable { detail } = endpoint_error else {
        panic!("expected IpcUnavailable, got {endpoint_error:?}");
    };
    assert_eq!(detail, endpoint_file_error.to_string());
}

#[test]
fn the_endpoint_file_of_a_closed_koshi_0_1_0_window_names_no_running_session() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    write_koshi_0_1_0_window_endpoint_file(runtime_directory.path(), session_id);

    let endpoint_error = load_session_endpoint(runtime_directory.path(), None, session_id)
        .expect_err("a closed window is no running session");

    let CliError::SessionNotFound { session_name } = endpoint_error else {
        panic!("expected SessionNotFound, got {endpoint_error:?}");
    };
    assert_eq!(session_name, session_id.to_string());
    assert!(is_koshi_0_1_0_window_closed(
        runtime_directory.path(),
        session_id
    ));
}

#[test]
fn the_endpoint_file_of_an_open_koshi_0_1_0_window_names_that_window() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let _window_listener = Listener::bind(&compute_socket_address(
        runtime_directory.path(),
        session_id,
    ))
    .expect("bind the stand-in window");
    let endpoint_file_path =
        write_koshi_0_1_0_window_endpoint_file(runtime_directory.path(), session_id);

    let endpoint_error = load_session_endpoint(runtime_directory.path(), None, session_id)
        .expect_err("an open window speaks no wire this build reads");

    let CliError::IpcUnavailable { detail } = endpoint_error else {
        panic!("expected IpcUnavailable, got {endpoint_error:?}");
    };
    assert_eq!(
        detail,
        format!(
            "endpoint file {} is unreadable: a koshi 0.1.0 window wrote it, and this koshi \
             cannot talk to that window; the window ends when its terminal closes",
            endpoint_file_path.display()
        )
    );
    assert!(!is_koshi_0_1_0_window_closed(
        runtime_directory.path(),
        session_id
    ));
}

#[test]
fn a_session_refusing_the_token_of_two_endpoint_files_in_a_row_reports_the_refusal() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, submitted_envelopes) = spawn_fake_session(
        runtime_directory.path(),
        session_id,
        vec![SessionScript::RefuseHello, SessionScript::RefuseHello],
    );

    let command_error = submit_command_via_runtime_directory(
        runtime_directory.path(),
        &build_in_session_context(session_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect_err("both hellos are refused");
    let CliError::ConnectionTokenRefused { detail } = command_error else {
        panic!("expected ConnectionTokenRefused, got {command_error:?}");
    };
    assert_eq!(detail, "the token presented does not match this Koshi's");
    assert_eq!(submitted_envelopes.try_iter().count(), 2);

    session_server_thread.join().expect("fake session exits");
}

#[test]
fn a_command_refused_for_its_token_is_applied_once_the_session_advertises_another() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, submitted_envelopes) = spawn_fake_session(
        runtime_directory.path(),
        session_id,
        vec![SessionScript::RefuseHello, SessionScript::AcceptAndApply],
    );

    let command_result = submit_external_command_via_runtime_directory(
        runtime_directory.path(),
        None,
        session_id,
        None,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect("the second hello presents the session's own token");

    session_server_thread.join().expect("fake session exits");
    let refused_envelope = submitted_envelopes
        .recv()
        .expect("the refusing connection read one command");
    let command_envelope = submitted_envelopes
        .recv()
        .expect("the accepting connection read one command");
    assert_eq!(refused_envelope.command, command_envelope.command);
    assert_eq!(
        command_result,
        CommandResult::Ok {
            command_id: command_envelope.command_id,
            emitted_events: Vec::new(),
        },
    );
}

#[test]
fn a_refused_command_carries_the_sessions_sentence() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, asked_requests) = spawn_answering_session(
        runtime_directory.path(),
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::NotFound,
            message: "this session holds no pane by that id".to_string(),
        }),
    );

    let submit_error = submit_command_via_runtime_directory(
        runtime_directory.path(),
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
}

#[test]
fn a_command_answered_with_another_reply_kind_names_that_kind() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, _asked_requests) =
        spawn_answering_session(runtime_directory.path(), session_id, IpcResult::Restarting);

    let submit_error = submit_command_via_runtime_directory(
        runtime_directory.path(),
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
}

// --- Asking a session for its layout ----------------------------------------

/// A stand-in koshi serving one exchange at `runtime_directory`: write the endpoint
/// file, accept one caller, read the Hello and the request behind it, answer
/// the Hello, then answer that request with `ipc_result`, and close the
/// connection once the caller hung up. The returned receiver carries the
/// request the caller actually sent.
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
        close_connection_after_peer_hangs_up(session_connection);
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
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let expected_session_layout = build_session_layout_with_tab("workspace", session_id, tab_id);
    let (session_server_thread, request_kinds) = spawn_answering_session(
        runtime_directory.path(),
        session_id,
        IpcResult::Layout(expected_session_layout.clone()),
    );

    let session_layout = fetch_layout(runtime_directory.path(), None, session_id, Some(tab_id))
        .expect("the session answers");

    assert_eq!(session_layout, expected_session_layout);
    assert_eq!(
        request_kinds.recv().expect("the session read one request"),
        IpcRequestKind::Layout {
            tab_id: Some(tab_id)
        },
    );

    session_server_thread.join().expect("fake session exits");
}

#[test]
fn fetching_the_whole_layout_asks_for_no_tab() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, request_kinds) = spawn_answering_session(
        runtime_directory.path(),
        session_id,
        IpcResult::Layout(build_named_session_layout("workspace", session_id)),
    );

    let session_layout = fetch_layout(runtime_directory.path(), None, session_id, None)
        .expect("the session answers");

    assert_eq!(session_layout.session_name, "workspace");
    assert_eq!(
        request_kinds.recv().expect("the session read one request"),
        IpcRequestKind::Layout { tab_id: None },
    );

    session_server_thread.join().expect("fake session exits");
}

#[test]
fn fetching_recent_events_returns_them_in_the_order_the_session_sent() {
    let runtime_directory = build_test_runtime_directory();
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
        runtime_directory.path(),
        session_id,
        IpcResult::RecentEvents(expected_recent_events.clone()),
    );

    let recent_events = fetch_recent_events(runtime_directory.path(), None, session_id)
        .expect("the session answers");

    assert_eq!(recent_events, expected_recent_events);
    assert_eq!(
        request_kinds.recv().expect("the session read one request"),
        IpcRequestKind::RecentEvents,
    );

    session_server_thread.join().expect("fake session exits");
}

#[test]
fn a_recent_events_request_a_session_cannot_read_carries_its_own_message() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, _request_kinds) = spawn_answering_session(
        runtime_directory.path(),
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the bytes received are not a request this build can read".to_string(),
        }),
    );

    let recent_events_error = fetch_recent_events(runtime_directory.path(), None, session_id)
        .expect_err("the request is refused");

    assert_eq!(
        recent_events_error.to_string(),
        CliError::IpcUnavailable {
            detail: "the bytes received are not a request this build can read".to_string(),
        }
        .to_string(),
    );

    session_server_thread.join().expect("fake session exits");
}

#[test]
fn a_recent_events_refusal_with_a_bad_token_carries_the_sessions_own_message() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, _request_kinds) = spawn_answering_session(
        runtime_directory.path(),
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let recent_events_error = fetch_recent_events(runtime_directory.path(), None, session_id)
        .expect_err("the request is refused");

    assert_eq!(
        recent_events_error.to_string(),
        CliError::IpcUnavailable {
            detail: "the token presented does not match this Koshi's".to_string(),
        }
        .to_string(),
    );

    session_server_thread.join().expect("fake session exits");
}

#[test]
fn fetching_a_layout_with_no_endpoint_file_reports_the_session_not_running() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();

    let layout_error = fetch_layout(runtime_directory.path(), None, session_id, None)
        .expect_err("no endpoint file exists");

    assert_eq!(
        layout_error.to_string(),
        CliError::SessionNotFound {
            session_name: session_id.to_string(),
        }
        .to_string(),
    );
}

#[test]
fn a_layout_request_a_session_cannot_read_carries_its_own_message() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, _request_kinds) = spawn_answering_session(
        runtime_directory.path(),
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the bytes received are not a request this build can read".to_string(),
        }),
    );

    let layout_error = fetch_layout(runtime_directory.path(), None, session_id, None)
        .expect_err("the request is refused");

    assert_eq!(
        layout_error.to_string(),
        CliError::IpcUnavailable {
            detail: "the bytes received are not a request this build can read".to_string(),
        }
        .to_string(),
    );

    session_server_thread.join().expect("fake session exits");
}

#[test]
fn a_layout_refusal_with_a_bad_token_carries_the_sessions_own_message() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, _request_kinds) = spawn_answering_session(
        runtime_directory.path(),
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let layout_error = fetch_layout(runtime_directory.path(), None, session_id, None)
        .expect_err("the request is refused");

    assert_eq!(
        layout_error.to_string(),
        CliError::IpcUnavailable {
            detail: "the token presented does not match this Koshi's".to_string(),
        }
        .to_string(),
    );

    session_server_thread.join().expect("fake session exits");
}

#[test]
fn a_layout_for_a_tab_the_session_no_longer_holds_reports_the_tab_missing() {
    // The session answers with a layout that describes no tab.
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let (session_server_thread, _request_kinds) = spawn_answering_session(
        runtime_directory.path(),
        session_id,
        IpcResult::Layout(build_named_session_layout("workspace", session_id)),
    );

    let layout_error = fetch_layout(runtime_directory.path(), None, session_id, Some(tab_id))
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
}

#[test]
fn a_layout_request_answered_with_another_reply_kind_names_that_kind() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, _asked) = spawn_answering_session(
        runtime_directory.path(),
        session_id,
        IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            build_version: env!("CARGO_PKG_VERSION").to_string(),
        },
    );

    let layout_error = fetch_layout(runtime_directory.path(), None, session_id, None)
        .expect_err("a Hello does not answer a layout request");

    let CliError::IpcUnavailable { detail } = layout_error else {
        panic!("expected IpcUnavailable naming the reply kind, got {layout_error:?}");
    };
    assert_eq!(
        detail,
        "the session answered with an unexpected Hello reply"
    );

    session_server_thread.join().expect("fake session exits");
}

// --- Asking a session to describe itself ------------------------------------

#[test]
fn fetching_an_overview_returns_it_and_asks_for_a_discovery() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let answered_overview = build_named_session_overview("S-quiet-lake", session_id);
    let (session_server_thread, asked_requests) = spawn_answering_session(
        runtime_directory.path(),
        session_id,
        IpcResult::Overview(answered_overview.clone()),
    );

    let fetched_overview = fetch_session_overview_from_endpoint(
        &load_session_endpoint(runtime_directory.path(), None, session_id)
            .expect("the endpoint file"),
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
}

#[test]
fn a_refused_overview_carries_the_sessions_sentence() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, _asked) = spawn_answering_session(
        runtime_directory.path(),
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let overview_error = fetch_session_overview_from_endpoint(
        &load_session_endpoint(runtime_directory.path(), None, session_id)
            .expect("the endpoint file"),
        session_id,
        None,
    )
    .expect_err("the request is refused");

    let CliError::IpcUnavailable { detail } = overview_error else {
        panic!("expected IpcUnavailable, got {overview_error:?}");
    };
    assert_eq!(detail, "the token presented does not match this Koshi's");

    session_server_thread.join().expect("fake session exits");
}

#[test]
fn an_overview_request_answered_with_another_reply_kind_names_that_kind() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, _asked_requests) =
        spawn_answering_session(runtime_directory.path(), session_id, IpcResult::Restarting);

    let overview_error = fetch_session_overview_from_endpoint(
        &load_session_endpoint(runtime_directory.path(), None, session_id)
            .expect("the endpoint file"),
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
}

// --- Asking a session to restart --------------------------------------------

#[test]
fn a_restarting_reply_reports_the_session_restarting_and_asked_for_a_restart() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, asked_requests) =
        spawn_answering_session(runtime_directory.path(), session_id, IpcResult::Restarting);

    assert_eq!(
        restart_running_session(runtime_directory.path(), None, session_id)
            .expect("the exchange succeeds"),
        SessionRestart::Restarting
    );
    assert_eq!(
        asked_requests.recv().expect("the session read one request"),
        IpcRequestKind::Restart,
    );

    session_server_thread.join().expect("fake session exits");
}

#[test]
fn a_restart_refused_with_the_malformed_request_code_carries_the_sessions_sentence() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, _asked) = spawn_answering_session(
        runtime_directory.path(),
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the binary at /opt/koshi could not be read: Exec format error (os error 8)"
                .to_string(),
        }),
    );

    let restart_error = restart_running_session(runtime_directory.path(), None, session_id)
        .expect_err("the restart is refused");

    assert_eq!(
        restart_error.to_string(),
        "IPC unavailable: the binary at /opt/koshi could not be read: Exec format error (os error 8)"
    );

    session_server_thread.join().expect("fake session exits");
}

#[test]
fn a_restart_refused_with_the_request_failed_code_carries_the_sessions_sentence() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, _asked) = spawn_answering_session(
        runtime_directory.path(),
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::RequestFailed,
            message: "the binary at /opt/koshi could not be read: No such file or directory"
                .to_string(),
        }),
    );

    let restart_error = restart_running_session(runtime_directory.path(), None, session_id)
        .expect_err("the restart is refused");

    assert_eq!(
        restart_error.to_string(),
        "IPC unavailable: the binary at /opt/koshi could not be read: No such file or directory"
    );

    session_server_thread.join().expect("fake session exits");
}

#[test]
fn a_restart_refused_with_a_bad_token_carries_the_sessions_sentence() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, _asked) = spawn_answering_session(
        runtime_directory.path(),
        session_id,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let restart_error = restart_running_session(runtime_directory.path(), None, session_id)
        .expect_err("the restart is refused");

    assert_eq!(
        restart_error.to_string(),
        "IPC unavailable: the token presented does not match this Koshi's"
    );

    session_server_thread.join().expect("fake session exits");
}

#[test]
fn a_restart_answered_with_another_reply_kind_names_that_kind() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, _request_kinds) = spawn_answering_session(
        runtime_directory.path(),
        session_id,
        IpcResult::Overview(build_named_session_overview("workspace", session_id)),
    );

    let restart_error = restart_running_session(runtime_directory.path(), None, session_id)
        .expect_err("an Overview does not answer a restart");

    let CliError::IpcUnavailable { detail } = restart_error else {
        panic!("expected IpcUnavailable naming the reply kind, got {restart_error:?}");
    };
    assert_eq!(
        detail,
        "the session answered with an unexpected Overview reply",
    );

    session_server_thread.join().expect("fake session exits");
}

#[test]
fn asking_a_session_with_no_endpoint_file_to_restart_restarts_nothing() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();

    assert_eq!(
        restart_running_session(runtime_directory.path(), None, session_id)
            .expect("a missing session is not an error"),
        SessionRestart::NotRunning
    );
}

#[test]
fn asking_a_session_nothing_listens_behind_to_restart_restarts_nothing() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(
            runtime_directory.path(),
            session_id,
        ),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        session_id,
    ))
    .expect("write endpoint file");

    assert_eq!(
        restart_running_session(runtime_directory.path(), None, session_id)
            .expect("a dead socket is not an error"),
        SessionRestart::NotRunning
    );
}

// --- Reading a running session's build --------------------------------------

#[test]
fn a_running_session_reports_the_build_its_hello_named() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, session_requests) = spawn_settled_session(
        runtime_directory.path(),
        session_id,
        PROTOCOL_VERSION,
        HelloTiming::AtOnce,
    );

    assert_eq!(
        find_running_session_version(runtime_directory.path(), None, session_id, None)
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
}

#[test]
fn a_session_refusing_its_old_token_reports_the_build_once_it_advertises_its_own() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let session_server_thread = spawn_session_listening_before_it_advertises(
        runtime_directory.path(),
        session_id,
        OLD_CONNECTION_TOKEN,
        NEW_CONNECTION_TOKEN,
        |request_kind| {
            panic!(
                "reading the build sends no {} after the Hello",
                request_kind.get_request_kind_name()
            )
        },
    );

    let session_version =
        find_running_session_version(runtime_directory.path(), None, session_id, None)
            .expect("the second Hello is answered");

    assert_eq!(session_version, Some("9.9.9".to_string()));
    session_server_thread.join().expect("fake session exits");
}

#[test]
fn a_session_with_no_endpoint_file_reports_no_build() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();

    assert_eq!(
        find_running_session_version(runtime_directory.path(), None, session_id, None)
            .expect("a missing session is not an error"),
        None
    );
}

#[test]
fn a_session_nothing_listens_behind_reports_no_build() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(
            runtime_directory.path(),
            session_id,
        ),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        session_id,
    ))
    .expect("write endpoint file");

    assert_eq!(
        find_running_session_version(runtime_directory.path(), None, session_id, None)
            .expect("a dead socket is not an error"),
        None
    );
}

#[test]
fn a_version_ask_whose_answer_deadline_has_passed_connects_to_nothing() {
    // Nothing listens at `socket_address`.
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let socket_address =
        koshi_ipc::endpoint::compute_socket_address(runtime_directory.path(), session_id);

    let version_answer = find_foreign_session_version(session_id, &socket_address, Instant::now());

    let Err(CliError::SessionAnswerTimedOut) = version_answer else {
        panic!("expected SessionAnswerTimedOut, got {version_answer:?}");
    };
}

#[cfg(windows)]
#[test]
fn a_connect_to_a_busy_pipe_ends_at_the_answer_deadline() {
    // The pipe's one instance holds a caller the listener never accepts. The
    // OS can end the connect's wait a moment before `answer_deadline`, and the
    // failure is then `IpcUnavailable`.
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let socket_address =
        koshi_ipc::endpoint::compute_socket_address(runtime_directory.path(), session_id);
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
}

// --- Which file names a session, and under which suffix ---------------------

#[test]
fn an_endpoint_file_and_a_resume_file_are_told_apart_by_their_suffix() {
    // Both names start `session-<uuid>`. The suffix tells the session that
    // advertises a socket from the one that left a resume file.
    let runtime_directory = build_test_runtime_directory();
    let advertised_session_id = SessionId::new();
    let resumable_session_id = SessionId::new();
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(
            runtime_directory.path(),
            advertised_session_id,
        ),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        advertised_session_id,
    ))
    .expect("write endpoint file");
    std::fs::write(
        runtime_directory
            .path()
            .join(format!("{resumable_session_id}{RESUME_SUFFIX}")),
        b"{}",
    )
    .expect("write resume file");

    assert_eq!(
        list_advertised_sessions(runtime_directory.path()),
        Ok(vec![advertised_session_id])
    );
    assert_eq!(
        list_sessions_with_resume_files(runtime_directory.path()),
        Ok(vec![resumable_session_id])
    );
}

#[test]
fn a_file_that_names_no_session_is_passed_over() {
    let runtime_directory = build_test_runtime_directory();
    let advertised_session_id = SessionId::new();
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(
            runtime_directory.path(),
            advertised_session_id,
        ),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        advertised_session_id,
    ))
    .expect("write endpoint file");
    std::fs::write(
        runtime_directory.path().join("session-not-a-uuid.json"),
        b"{}",
    )
    .expect("bad uuid");
    std::fs::write(runtime_directory.path().join("router.json"), b"{}").expect("no session prefix");
    std::fs::write(
        runtime_directory
            .path()
            .join(advertised_session_id.to_string()),
        b"{}",
    )
    .expect("no suffix");

    assert_eq!(
        list_advertised_sessions(runtime_directory.path()),
        Ok(vec![advertised_session_id])
    );
    assert_eq!(
        list_sessions_with_resume_files(runtime_directory.path()),
        Ok(Vec::new())
    );
}

#[test]
fn a_runtime_directory_that_does_not_exist_names_no_session() {
    let runtime_directory = build_test_runtime_directory();
    let absent_runtime_directory = runtime_directory.path().join("absent");

    assert_eq!(
        list_advertised_sessions(&absent_runtime_directory),
        Ok(Vec::new())
    );
    assert_eq!(
        list_sessions_with_resume_files(&absent_runtime_directory),
        Ok(Vec::new())
    );
    assert_eq!(list_own_sessions(&absent_runtime_directory), Ok(Vec::new()));
}

#[cfg(unix)]
#[test]
fn a_runtime_directory_that_cannot_be_read_is_refused_naming_it() {
    use std::os::unix::fs::PermissionsExt;

    let runtime_directory = build_test_runtime_directory();
    write_own_endpoint_file(runtime_directory.path(), SessionId::new());
    std::fs::set_permissions(
        runtime_directory.path(),
        std::fs::Permissions::from_mode(0o000),
    )
    .expect("make the runtime directory unreadable");
    let Err(read_error) = std::fs::read_dir(runtime_directory.path()) else {
        eprintln!(
            "skipped `a_runtime_directory_that_cannot_be_read_is_refused_naming_it`: \
             this user reads through a mode-000 directory"
        );
        let _ = std::fs::set_permissions(
            runtime_directory.path(),
            std::fs::Permissions::from_mode(0o700),
        );
        return;
    };

    let advertised_sessions = list_advertised_sessions(runtime_directory.path());
    let resumable_sessions = list_sessions_with_resume_files(runtime_directory.path());
    let own_sessions = list_own_sessions(runtime_directory.path());

    let _ = std::fs::set_permissions(
        runtime_directory.path(),
        std::fs::Permissions::from_mode(0o700),
    );
    let unread_runtime_directory =
        UnreadPath::from_read_error(runtime_directory.path(), &read_error);
    assert_eq!(advertised_sessions, Err(unread_runtime_directory.clone()));
    assert_eq!(resumable_sessions, Err(unread_runtime_directory.clone()));
    assert_eq!(own_sessions, Err(unread_runtime_directory));
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
/// presents, answer the discovery request with `session_overview`, and close
/// the connection once the caller hung up. No endpoint file is written, since
/// that user's own runtime directory is theirs alone. The returned receiver
/// carries the token the caller presented.
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
        close_connection_after_peer_hangs_up(session_connection);
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

    let runtime_directory = build_test_runtime_directory();
    let shared_sessions_base_directory = build_test_runtime_directory();
    let own_user_id = std::fs::metadata(runtime_directory.path())
        .expect("read the runtime directory")
        .uid();
    let other_user_directory = shared_sessions_base_directory
        .path()
        .join((own_user_id + 1).to_string());
    std::fs::create_dir_all(&other_user_directory).expect("create the other user's directory");
    let session_id = SessionId::new();
    let socket_path = other_user_directory.join(format!("{session_id}.sock"));
    plant_session_socket(&socket_path);

    let endpoint = load_session_endpoint(
        runtime_directory.path(),
        Some(shared_sessions_base_directory.path()),
        session_id,
    )
    .expect("the shared directory advertises the session");
    let lookup_without_shared_directory =
        load_session_endpoint(runtime_directory.path(), None, session_id);

    assert_eq!(endpoint.socket_address, socket_path.display().to_string());
    assert_eq!(endpoint.connection_token.expose_secret(), "");
    assert_eq!(endpoint.process_id, 0);
    let Err(CliError::SessionNotFound { session_name }) = lookup_without_shared_directory else {
        panic!("expected SessionNotFound, got {lookup_without_shared_directory:?}");
    };
    assert_eq!(session_name, session_id.to_string());
}

#[cfg(unix)]
#[test]
fn the_shared_listing_holds_other_users_sockets_and_not_this_users() {
    use std::os::unix::fs::MetadataExt;

    let runtime_directory = build_test_runtime_directory();
    let shared_sessions_base_directory = build_test_runtime_directory();
    let own_user_id = std::fs::metadata(runtime_directory.path())
        .expect("read the runtime directory")
        .uid();
    let other_user_directory_name = (own_user_id + 1).to_string();
    let own_session_id = SessionId::new();
    let foreign_session_id = SessionId::new();
    std::fs::create_dir_all(
        shared_sessions_base_directory
            .path()
            .join(own_user_id.to_string()),
    )
    .expect("create this user's directory");
    std::fs::create_dir_all(
        shared_sessions_base_directory
            .path()
            .join(&other_user_directory_name),
    )
    .expect("create the other user's directory");
    plant_session_socket(
        &shared_sessions_base_directory
            .path()
            .join(own_user_id.to_string())
            .join(format!("{own_session_id}.sock")),
    );
    plant_session_socket(
        &shared_sessions_base_directory
            .path()
            .join(&other_user_directory_name)
            .join(format!("{foreign_session_id}.sock")),
    );

    assert_eq!(
        list_foreign_sessions(
            shared_sessions_base_directory.path(),
            runtime_directory.path()
        ),
        ForeignSessionListing {
            foreign_sessions: vec![(
                foreign_session_id,
                shared_sessions_base_directory
                    .path()
                    .join(&other_user_directory_name)
                    .join(format!("{foreign_session_id}.sock"))
                    .display()
                    .to_string(),
            )],
            ..ForeignSessionListing::default()
        },
    );
}

#[cfg(unix)]
#[test]
fn a_foreign_socket_reusing_a_local_session_id_is_left_out() {
    // A socket in another user's folder named after an id this user's endpoint
    // file advertises is not listed.
    use std::os::unix::fs::MetadataExt;

    let runtime_directory = build_test_runtime_directory();
    let shared_sessions_base_directory = build_test_runtime_directory();
    let own_user_id = std::fs::metadata(runtime_directory.path())
        .expect("read the runtime directory")
        .uid();
    let own_session_id = SessionId::new();
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(
            runtime_directory.path(),
            own_session_id,
        ),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        own_session_id,
    ))
    .expect("advertise this user's session");
    let foreign_user_directory = shared_sessions_base_directory
        .path()
        .join((own_user_id + 1).to_string());
    std::fs::create_dir_all(&foreign_user_directory).expect("create the other user's directory");
    plant_session_socket(&foreign_user_directory.join(format!("{own_session_id}.sock")));

    assert_eq!(
        list_foreign_sessions(
            shared_sessions_base_directory.path(),
            runtime_directory.path()
        ),
        ForeignSessionListing::default()
    );
}

#[cfg(windows)]
#[test]
fn the_shared_listing_holds_the_markers_this_user_does_not_advertise() {
    // A marker whose id this user's endpoint file advertises is not listed.
    let runtime_directory = build_test_runtime_directory();
    let shared_sessions_base_directory = build_test_runtime_directory();
    let own_session_id = SessionId::new();
    let foreign_session_id = SessionId::new();
    std::fs::write(
        shared_sessions_base_directory
            .path()
            .join(own_session_id.to_string()),
        b"",
    )
    .expect("plant this user's marker");
    std::fs::write(
        shared_sessions_base_directory
            .path()
            .join(foreign_session_id.to_string()),
        b"",
    )
    .expect("plant the other user's marker");
    EndpointFile {
        socket_address: koshi_ipc::endpoint::compute_socket_address(
            runtime_directory.path(),
            own_session_id,
        ),
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        own_session_id,
    ))
    .expect("write this user's endpoint file");

    assert_eq!(
        list_foreign_sessions(
            shared_sessions_base_directory.path(),
            runtime_directory.path(),
        ),
        ForeignSessionListing {
            foreign_sessions: vec![(foreign_session_id, format!("koshi-{foreign_session_id}"),)],
            ..ForeignSessionListing::default()
        },
    );
}

#[test]
fn a_shared_directory_that_does_not_exist_holds_no_session() {
    let runtime_directory = build_test_runtime_directory();

    assert_eq!(
        list_foreign_sessions(
            &runtime_directory.path().join("absent"),
            runtime_directory.path()
        ),
        ForeignSessionListing::default(),
    );
}

#[cfg(unix)]
#[test]
fn an_absent_runtime_directory_skips_no_shared_subdirectory() {
    // With no runtime directory, the folder named after this user's id is
    // listed like any other.
    use std::os::unix::fs::MetadataExt;

    let shared_sessions_base_directory = build_test_runtime_directory();
    let own_user_id = std::fs::metadata(shared_sessions_base_directory.path())
        .expect("read the shared directory")
        .uid();
    let foreign_session_id = SessionId::new();
    let foreign_user_directory = shared_sessions_base_directory
        .path()
        .join(own_user_id.to_string());
    std::fs::create_dir_all(&foreign_user_directory).expect("create a user's directory");
    plant_session_socket(&foreign_user_directory.join(format!("{foreign_session_id}.sock")));

    assert_eq!(
        list_foreign_sessions(
            shared_sessions_base_directory.path(),
            &shared_sessions_base_directory
                .path()
                .join("no-runtime-dir-here"),
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
}

#[cfg(unix)]
#[test]
fn a_runtime_directory_that_cannot_be_read_lists_no_foreign_session_and_names_it() {
    // The runtime directory sits under a mode-000 folder: the listing keeps
    // the runtime directory as unread, and a lookup by id fails naming the
    // endpoint file it could not look up.
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let shared_sessions_base_directory = build_test_runtime_directory();
    let own_user_id = std::fs::metadata(shared_sessions_base_directory.path())
        .expect("read the shared directory")
        .uid();
    if own_user_id == 0 {
        eprintln!(
            "skipped `a_runtime_directory_that_cannot_be_read_lists_no_foreign_session_and_names_it`: \
             root reads through a mode-000 directory"
        );
        return;
    }
    let foreign_session_id = SessionId::new();
    let foreign_user_directory = shared_sessions_base_directory
        .path()
        .join((own_user_id + 1).to_string());
    std::fs::create_dir_all(&foreign_user_directory).expect("create the other user's directory");
    plant_session_socket(&foreign_user_directory.join(format!("{foreign_session_id}.sock")));

    let unreadable_parent_directory = build_test_runtime_directory();
    let runtime_directory = unreadable_parent_directory.path().join("runtime");
    std::fs::create_dir_all(&runtime_directory).expect("create the runtime directory");
    std::fs::set_permissions(
        unreadable_parent_directory.path(),
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
        list_foreign_sessions(shared_sessions_base_directory.path(), &runtime_directory);
    let lookup_error = find_foreign_session_address(
        shared_sessions_base_directory.path(),
        &runtime_directory,
        foreign_session_id,
    );

    let _ = std::fs::set_permissions(
        unreadable_parent_directory.path(),
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
}

#[cfg(unix)]
#[test]
fn entries_another_user_planted_that_name_no_session_are_passed_over() {
    // Only a `session-<uuid>.sock` inside a folder named like a user id is a
    // session.
    use std::os::unix::fs::MetadataExt;

    let runtime_directory = build_test_runtime_directory();
    let shared_sessions_base_directory = build_test_runtime_directory();
    let own_user_id = std::fs::metadata(runtime_directory.path())
        .expect("read the runtime directory")
        .uid();
    let foreign_user_directory = shared_sessions_base_directory
        .path()
        .join((own_user_id + 1).to_string());
    std::fs::create_dir_all(&foreign_user_directory).expect("create the other user's directory");
    let foreign_session_id = SessionId::new();
    plant_session_socket(&foreign_user_directory.join(format!("{foreign_session_id}.sock")));
    plant_session_socket(&foreign_user_directory.join("session-not-a-uuid.sock"));
    plant_session_socket(&foreign_user_directory.join(foreign_session_id.to_string()));
    plant_session_socket(&foreign_user_directory.join("README.sock"));
    std::fs::create_dir_all(foreign_user_directory.join("nested")).expect("plant a subdirectory");
    std::fs::write(
        shared_sessions_base_directory.path().join("loose-file"),
        b"",
    )
    .expect("plant a file beside the user directory");

    assert_eq!(
        list_foreign_sessions(
            shared_sessions_base_directory.path(),
            runtime_directory.path(),
        ),
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
}

#[cfg(unix)]
#[test]
fn a_plain_file_named_like_a_session_socket_is_passed_over() {
    use std::os::unix::fs::MetadataExt;

    let runtime_directory = build_test_runtime_directory();
    let shared_sessions_base_directory = build_test_runtime_directory();
    let own_user_id = std::fs::metadata(runtime_directory.path())
        .expect("read the runtime directory")
        .uid();
    let foreign_user_directory = shared_sessions_base_directory
        .path()
        .join((own_user_id + 1).to_string());
    std::fs::create_dir_all(&foreign_user_directory).expect("create the other user's directory");
    let plain_file_session_id = SessionId::new();
    std::fs::write(
        foreign_user_directory.join(format!("{plain_file_session_id}.sock")),
        b"",
    )
    .expect("plant a plain file");

    assert_eq!(
        list_foreign_sessions(
            shared_sessions_base_directory.path(),
            runtime_directory.path()
        ),
        ForeignSessionListing::default()
    );
}

#[cfg(windows)]
#[test]
fn markers_another_user_planted_that_name_no_session_are_passed_over() {
    // Only an entry named `session-<uuid>` is a session.
    let runtime_directory = build_test_runtime_directory();
    let shared_sessions_base_directory = build_test_runtime_directory();
    let foreign_session_id = SessionId::new();
    std::fs::write(
        shared_sessions_base_directory
            .path()
            .join(foreign_session_id.to_string()),
        b"",
    )
    .expect("plant their marker");
    std::fs::write(
        shared_sessions_base_directory
            .path()
            .join("session-not-a-uuid"),
        b"",
    )
    .expect("plant a bad uuid");
    std::fs::write(shared_sessions_base_directory.path().join("README"), b"")
        .expect("plant a name with no prefix");
    std::fs::create_dir_all(shared_sessions_base_directory.path().join("nested"))
        .expect("plant a subdirectory");

    assert_eq!(
        list_foreign_sessions(
            shared_sessions_base_directory.path(),
            runtime_directory.path()
        ),
        ForeignSessionListing {
            foreign_sessions: vec![(foreign_session_id, format!("koshi-{foreign_session_id}"),)],
            ..ForeignSessionListing::default()
        },
    );
}

/// A scan of the shared directory that has read `read_entry_count` entries
/// and counted nothing else.
fn build_shared_directory_scan(read_entry_count: usize) -> SharedDirectoryScan {
    SharedDirectoryScan {
        read_entry_count,
        ..SharedDirectoryScan::default()
    }
}

/// The unread path that a scan of `shared_sessions_base_directory` reports for
/// the entry past [`MAX_SHARED_DIRECTORY_ENTRY_COUNT`]: `it holds more than
/// 65536 entries`.
fn build_entry_limit_unread_path(shared_sessions_base_directory: &Path) -> UnreadPath {
    UnreadPath {
        looked_up_path: shared_sessions_base_directory.to_path_buf(),
        read_error_text: "it holds more than 65536 entries".to_string(),
    }
}

/// Plant one user folder in `shared_sessions_base_directory`, named after the
/// user id one past the id of this user, and the socket of a new session in
/// that folder. Hands back that session and the address a listing reports for
/// it. The shared directory then holds 1 entry, and the folder holds 1.
#[cfg(unix)]
fn plant_one_foreign_session_socket(shared_sessions_base_directory: &Path) -> (SessionId, String) {
    let other_user_directory = shared_sessions_base_directory
        .join((load_own_user_id(shared_sessions_base_directory) + 1).to_string());
    std::fs::create_dir(&other_user_directory).expect("create the other user's folder");
    let session_id = SessionId::new();
    plant_session_socket(&other_user_directory.join(format!("{session_id}.sock")));
    let socket_address =
        koshi_ipc::endpoint::compute_socket_address(&other_user_directory, session_id);
    (session_id, socket_address)
}

/// The user id that owns `shared_sessions_base_directory`: the id of this
/// user.
#[cfg(unix)]
fn load_own_user_id(shared_sessions_base_directory: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt;

    std::fs::metadata(shared_sessions_base_directory)
        .expect("read the shared directory")
        .uid()
}

/// The unread path that a scan of `shared_sessions_base_directory` reports for
/// the user folder past [`MAX_SHARED_USER_DIRECTORY_COUNT`]: `it holds more
/// than 256 user folders`.
#[cfg(unix)]
fn build_folder_limit_unread_path(shared_sessions_base_directory: &Path) -> UnreadPath {
    UnreadPath {
        looked_up_path: shared_sessions_base_directory.to_path_buf(),
        read_error_text: "it holds more than 256 user folders".to_string(),
    }
}

/// Plant the marker of a new session in `shared_sessions_base_directory`, and
/// hand back that session. The shared directory then holds 1 entry.
#[cfg(windows)]
fn plant_one_foreign_session_marker(shared_sessions_base_directory: &Path) -> SessionId {
    let foreign_session_id = SessionId::new();
    std::fs::write(
        shared_sessions_base_directory.join(foreign_session_id.to_string()),
        b"",
    )
    .expect("plant their marker");
    foreign_session_id
}

#[test]
fn a_scan_reads_the_65536th_entry_and_stops_at_the_next() {
    let shared_sessions_base_directory = Path::new("/tmp/koshi");
    let mut shared_directory_scan =
        build_shared_directory_scan(MAX_SHARED_DIRECTORY_ENTRY_COUNT - 1);

    assert_eq!(
        shared_directory_scan.count_read_entry(shared_sessions_base_directory),
        Ok(())
    );
    let entry_limit_unread_path = build_entry_limit_unread_path(shared_sessions_base_directory);
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
fn a_listing_that_reaches_the_entry_limit_at_a_session_socket_lists_that_session() {
    // The scan holds 65,534 entries. The user folder is entry 65,535, and the
    // socket in it is entry 65,536.
    let shared_directory_guard = build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let missing_runtime_directory = shared_sessions_base_directory.join("missing-runtime");
    let (session_id, socket_address) =
        plant_one_foreign_session_socket(shared_sessions_base_directory);

    let foreign_session_listing = list_foreign_sessions_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        build_shared_directory_scan(MAX_SHARED_DIRECTORY_ENTRY_COUNT - 2),
    );

    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            foreign_sessions: vec![(session_id, socket_address)],
            ..ForeignSessionListing::default()
        }
    );
}

#[cfg(unix)]
#[test]
fn a_listing_that_meets_the_entry_past_the_limit_in_a_user_folder_keeps_the_shared_directory_unread(
) {
    // The scan holds 65,535 entries. The user folder is entry 65,536, and the
    // socket in it is the entry past the limit.
    let shared_directory_guard = build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let missing_runtime_directory = shared_sessions_base_directory.join("missing-runtime");
    plant_one_foreign_session_socket(shared_sessions_base_directory);

    let foreign_session_listing = list_foreign_sessions_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        build_shared_directory_scan(MAX_SHARED_DIRECTORY_ENTRY_COUNT - 1),
    );

    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            unread_path: Some(build_entry_limit_unread_path(
                shared_sessions_base_directory
            )),
            ..ForeignSessionListing::default()
        }
    );
}

#[cfg(unix)]
#[test]
fn a_lookup_counts_the_shared_directory_alone_and_finds_a_session_at_the_entry_limit() {
    // The scan holds 65,535 entries, and the user folder is entry 65,536. The
    // lookup reads no listing of that folder, and counts no entry for its
    // socket.
    let shared_directory_guard = build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let missing_runtime_directory = shared_sessions_base_directory.join("missing-runtime");
    let (session_id, socket_address) =
        plant_one_foreign_session_socket(shared_sessions_base_directory);

    let found_socket_address = find_foreign_session_address_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        session_id,
        build_shared_directory_scan(MAX_SHARED_DIRECTORY_ENTRY_COUNT - 1),
    )
    .expect("the lookup at the limit reads");

    assert_eq!(found_socket_address, Some(socket_address));
}

#[cfg(unix)]
#[test]
fn a_lookup_past_the_entry_limit_keeps_the_shared_directory_unread() {
    // The scan holds 65,536 entries, and the user folder is the entry past the
    // limit.
    let shared_directory_guard = build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let missing_runtime_directory = shared_sessions_base_directory.join("missing-runtime");
    let (session_id, _) = plant_one_foreign_session_socket(shared_sessions_base_directory);

    let lookup_error = find_foreign_session_address_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        session_id,
        build_shared_directory_scan(MAX_SHARED_DIRECTORY_ENTRY_COUNT),
    )
    .expect_err("the lookup stops past the limit");

    let ForeignSessionLookupError::PathUnreadable { unread_path } = lookup_error else {
        panic!("expected PathUnreadable, got {lookup_error:?}");
    };
    assert_eq!(
        unread_path,
        build_entry_limit_unread_path(shared_sessions_base_directory)
    );
}

#[cfg(windows)]
#[test]
fn a_marker_listing_that_reaches_the_entry_limit_at_a_marker_lists_that_session() {
    // The scan holds 65,535 entries, and the marker is entry 65,536.
    let shared_directory_guard = build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let missing_runtime_directory = shared_sessions_base_directory.join("missing-runtime");
    let foreign_session_id = plant_one_foreign_session_marker(shared_sessions_base_directory);

    let foreign_session_listing = list_foreign_sessions_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        build_shared_directory_scan(MAX_SHARED_DIRECTORY_ENTRY_COUNT - 1),
    );

    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            foreign_sessions: vec![(foreign_session_id, format!("koshi-{foreign_session_id}"))],
            ..ForeignSessionListing::default()
        }
    );
}

#[cfg(windows)]
#[test]
fn a_marker_listing_that_meets_a_marker_past_the_entry_limit_keeps_the_shared_directory_unread() {
    // The scan holds 65,536 entries, and the marker is the entry past the
    // limit.
    let shared_directory_guard = build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let missing_runtime_directory = shared_sessions_base_directory.join("missing-runtime");
    plant_one_foreign_session_marker(shared_sessions_base_directory);

    let foreign_session_listing = list_foreign_sessions_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        build_shared_directory_scan(MAX_SHARED_DIRECTORY_ENTRY_COUNT),
    );

    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            unread_path: Some(build_entry_limit_unread_path(
                shared_sessions_base_directory
            )),
            ..ForeignSessionListing::default()
        }
    );
}

#[cfg(unix)]
#[test]
fn a_scan_finds_the_256th_user_folder_and_stops_at_the_next() {
    let shared_sessions_base_directory = Path::new("/tmp/koshi");
    let mut shared_directory_scan = SharedDirectoryScan {
        found_user_directory_count: MAX_SHARED_USER_DIRECTORY_COUNT - 1,
        ..build_shared_directory_scan(0)
    };

    assert_eq!(
        shared_directory_scan.count_found_user_directory(shared_sessions_base_directory),
        Ok(())
    );
    let folder_limit_unread_path = build_folder_limit_unread_path(shared_sessions_base_directory);
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
fn a_scan_that_finds_the_256th_user_folder_lists_and_looks_up_its_session() {
    // The scan has found 255 user folders.
    let shared_directory_guard = build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let missing_runtime_directory = shared_sessions_base_directory.join("missing-runtime");
    let (session_id, socket_address) =
        plant_one_foreign_session_socket(shared_sessions_base_directory);

    let foreign_session_listing = list_foreign_sessions_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        SharedDirectoryScan {
            found_user_directory_count: MAX_SHARED_USER_DIRECTORY_COUNT - 1,
            ..build_shared_directory_scan(0)
        },
    );
    let found_socket_address = find_foreign_session_address_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        session_id,
        SharedDirectoryScan {
            found_user_directory_count: MAX_SHARED_USER_DIRECTORY_COUNT - 1,
            ..build_shared_directory_scan(0)
        },
    )
    .expect("the lookup at the limit reads");

    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            foreign_sessions: vec![(session_id, socket_address.clone())],
            ..ForeignSessionListing::default()
        }
    );
    assert_eq!(found_socket_address, Some(socket_address));
}

#[cfg(unix)]
#[test]
fn a_scan_that_finds_a_user_folder_past_the_limit_keeps_the_shared_directory_unread() {
    // The scan has found 256 user folders.
    let shared_directory_guard = build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let missing_runtime_directory = shared_sessions_base_directory.join("missing-runtime");
    let (session_id, _) = plant_one_foreign_session_socket(shared_sessions_base_directory);

    let foreign_session_listing = list_foreign_sessions_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        SharedDirectoryScan {
            found_user_directory_count: MAX_SHARED_USER_DIRECTORY_COUNT,
            ..build_shared_directory_scan(0)
        },
    );
    let lookup_error = find_foreign_session_address_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        session_id,
        SharedDirectoryScan {
            found_user_directory_count: MAX_SHARED_USER_DIRECTORY_COUNT,
            ..build_shared_directory_scan(0)
        },
    )
    .expect_err("the lookup stops past the limit");

    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            unread_path: Some(build_folder_limit_unread_path(
                shared_sessions_base_directory
            )),
            ..ForeignSessionListing::default()
        }
    );
    let ForeignSessionLookupError::PathUnreadable { unread_path } = lookup_error else {
        panic!("expected PathUnreadable, got {lookup_error:?}");
    };
    assert_eq!(
        unread_path,
        build_folder_limit_unread_path(shared_sessions_base_directory)
    );
}

#[cfg(unix)]
#[test]
fn a_listing_checks_the_256th_session_of_one_owner() {
    // The scan has checked 255 sessions of the owner of the user folder.
    let shared_directory_guard = build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let missing_runtime_directory = shared_sessions_base_directory.join("missing-runtime");
    let (session_id, socket_address) =
        plant_one_foreign_session_socket(shared_sessions_base_directory);

    let foreign_session_listing = list_foreign_sessions_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        SharedDirectoryScan {
            checked_session_count_by_owner: HashMap::from([(
                load_own_user_id(shared_sessions_base_directory),
                MAX_SHARED_SESSION_COUNT_PER_OWNER - 1,
            )]),
            ..build_shared_directory_scan(0)
        },
    );

    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            foreign_sessions: vec![(session_id, socket_address)],
            ..ForeignSessionListing::default()
        }
    );
}

#[cfg(unix)]
#[test]
fn a_listing_counts_a_session_past_256_of_one_owner_across_their_folders() {
    // The scan has checked 256 sessions in other folders of the owner of the
    // user folder.
    let shared_directory_guard = build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let missing_runtime_directory = shared_sessions_base_directory.join("missing-runtime");
    plant_one_foreign_session_socket(shared_sessions_base_directory);

    let foreign_session_listing = list_foreign_sessions_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        SharedDirectoryScan {
            checked_session_count_by_owner: HashMap::from([(
                load_own_user_id(shared_sessions_base_directory),
                MAX_SHARED_SESSION_COUNT_PER_OWNER,
            )]),
            ..build_shared_directory_scan(0)
        },
    );

    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            unlisted_session_count: 1,
            ..ForeignSessionListing::default()
        }
    );
}

#[cfg(unix)]
#[test]
fn a_lookup_by_id_finds_a_session_the_listing_counts_past_its_cap() {
    // Both scans have checked 256 sessions of the owner of the user folder.
    let shared_directory_guard = build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let missing_runtime_directory = shared_sessions_base_directory.join("missing-runtime");
    let (session_id, socket_address) =
        plant_one_foreign_session_socket(shared_sessions_base_directory);
    let own_user_id = load_own_user_id(shared_sessions_base_directory);

    let foreign_session_listing = list_foreign_sessions_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        SharedDirectoryScan {
            checked_session_count_by_owner: HashMap::from([(
                own_user_id,
                MAX_SHARED_SESSION_COUNT_PER_OWNER,
            )]),
            ..build_shared_directory_scan(0)
        },
    );
    let found_socket_address = find_foreign_session_address_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        session_id,
        SharedDirectoryScan {
            checked_session_count_by_owner: HashMap::from([(
                own_user_id,
                MAX_SHARED_SESSION_COUNT_PER_OWNER,
            )]),
            ..build_shared_directory_scan(0)
        },
    )
    .expect("the shared directory is read");

    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            unlisted_session_count: 1,
            ..ForeignSessionListing::default()
        }
    );
    assert_eq!(found_socket_address, Some(socket_address));
}

#[cfg(unix)]
#[test]
fn an_entry_beside_the_user_folders_that_names_no_user_counts_as_no_user_folder() {
    // The scan has found 255 user folders. The shared directory holds a file
    // named `junk` beside the user folder.
    let shared_directory_guard = build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let missing_runtime_directory = shared_sessions_base_directory.join("missing-runtime");
    let (session_id, socket_address) =
        plant_one_foreign_session_socket(shared_sessions_base_directory);
    std::fs::write(shared_sessions_base_directory.join("junk"), b"")
        .expect("plant a file beside the user folder");

    let foreign_session_listing = list_foreign_sessions_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        SharedDirectoryScan {
            found_user_directory_count: MAX_SHARED_USER_DIRECTORY_COUNT - 1,
            ..build_shared_directory_scan(0)
        },
    );
    let found_socket_address = find_foreign_session_address_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        session_id,
        SharedDirectoryScan {
            found_user_directory_count: MAX_SHARED_USER_DIRECTORY_COUNT - 1,
            ..build_shared_directory_scan(0)
        },
    )
    .expect("the shared directory is read");

    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            foreign_sessions: vec![(session_id, socket_address.clone())],
            ..ForeignSessionListing::default()
        }
    );
    assert_eq!(found_socket_address, Some(socket_address));
}

#[cfg(unix)]
#[test]
fn an_entry_in_a_user_folder_that_names_no_session_counts_as_no_session() {
    // The scan has checked 255 sessions of the owner of the user folder. The
    // folder holds a file named `junk` beside the session socket.
    let shared_directory_guard = build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let missing_runtime_directory = shared_sessions_base_directory.join("missing-runtime");
    let (session_id, socket_address) =
        plant_one_foreign_session_socket(shared_sessions_base_directory);
    let own_user_id = load_own_user_id(shared_sessions_base_directory);
    std::fs::write(
        shared_sessions_base_directory
            .join((own_user_id + 1).to_string())
            .join("junk"),
        b"",
    )
    .expect("plant a file inside the user folder");

    let foreign_session_listing = list_foreign_sessions_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        SharedDirectoryScan {
            checked_session_count_by_owner: HashMap::from([(
                own_user_id,
                MAX_SHARED_SESSION_COUNT_PER_OWNER - 1,
            )]),
            ..build_shared_directory_scan(0)
        },
    );

    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            foreign_sessions: vec![(session_id, socket_address)],
            ..ForeignSessionListing::default()
        }
    );
}

#[cfg(windows)]
#[test]
fn a_marker_listing_lists_the_256th_marker() {
    // The scan has listed 255 markers.
    let shared_directory_guard = build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let missing_runtime_directory = shared_sessions_base_directory.join("missing-runtime");
    let foreign_session_id = plant_one_foreign_session_marker(shared_sessions_base_directory);

    let foreign_session_listing = list_foreign_sessions_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        SharedDirectoryScan {
            checked_marker_count: MAX_SHARED_MARKER_COUNT - 1,
            ..build_shared_directory_scan(0)
        },
    );

    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            foreign_sessions: vec![(foreign_session_id, format!("koshi-{foreign_session_id}"))],
            ..ForeignSessionListing::default()
        }
    );
}

#[cfg(windows)]
#[test]
fn a_marker_listing_counts_a_marker_past_256() {
    // The scan has listed 256 markers.
    let shared_directory_guard = build_test_runtime_directory();
    let shared_sessions_base_directory = shared_directory_guard.path();
    let missing_runtime_directory = shared_sessions_base_directory.join("missing-runtime");
    plant_one_foreign_session_marker(shared_sessions_base_directory);

    let foreign_session_listing = list_foreign_sessions_with_scan(
        shared_sessions_base_directory,
        &missing_runtime_directory,
        SharedDirectoryScan {
            checked_marker_count: MAX_SHARED_MARKER_COUNT,
            ..build_shared_directory_scan(0)
        },
    );

    assert_eq!(
        foreign_session_listing,
        ForeignSessionListing {
            unlisted_session_count: 1,
            ..ForeignSessionListing::default()
        }
    );
}

#[test]
fn a_session_another_user_started_is_asked_with_an_empty_token() {
    // The session is asked over the address the shared directory names, with
    // an empty token.
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let socket_address =
        koshi_ipc::endpoint::compute_socket_address(runtime_directory.path(), session_id);
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
}

#[test]
fn a_shared_advert_nothing_listens_behind_reports_the_session_not_running() {
    // A socket or marker with nothing listening behind it reads as a session
    // that is not running, not as one that could not answer.
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let socket_address =
        koshi_ipc::endpoint::compute_socket_address(runtime_directory.path(), session_id);

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
}

#[test]
fn an_overview_ask_whose_answer_deadline_has_passed_connects_to_nothing() {
    // Nothing listens at `socket_address`.
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let socket_address =
        koshi_ipc::endpoint::compute_socket_address(runtime_directory.path(), session_id);

    let overview_answer =
        fetch_foreign_session_overview(session_id, &socket_address, Instant::now());

    let Err(CliError::SessionAnswerTimedOut) = overview_answer else {
        panic!("expected SessionAnswerTimedOut, got {overview_answer:?}");
    };
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
/// [`CommandResult::Ok`], and closes the connection once the caller hung up.
/// Every request it reads goes down the returned receiver, in arrival order.
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
        close_connection_after_peer_hangs_up(session_connection);
    });
    (session_thread, request_receiver)
}

/// The Hello and the command go out back to back: a session that settles on a
/// version this build does not speak reads both. The caller gets the version
/// refusal and never a command result. A session that shares no version
/// answers every request after the failed Hello with `HelloRequired`, and does
/// not act on the command it read.
#[test]
fn a_session_speaking_below_this_builds_floor_refuses_the_caller_and_answers_no_command() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let refused_protocol_version = MIN_PROTOCOL_VERSION - 1;
    let (session_thread, received_requests) = spawn_settled_session(
        runtime_directory.path(),
        session_id,
        refused_protocol_version,
        HelloTiming::AtOnce,
    );

    let command_error = submit_external_command_via_runtime_directory(
        runtime_directory.path(),
        None,
        session_id,
        Some(client_id),
        Command::TogglePaneFullscreen,
    )
    .expect_err("a session below this build's floor is refused");

    let CliError::IpcUnavailable { detail } = command_error else {
        panic!("expected IpcUnavailable, got {command_error:?}");
    };
    assert_eq!(
        detail,
        format!(
            "the session settled on protocol version {refused_protocol_version}, which is \
             outside the {MIN_PROTOCOL_VERSION} to {PROTOCOL_VERSION} this koshi asked for"
        )
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
}

/// A command naming a target client costs one round trip, the same as every
/// other command: the session reads the command before it answers the Hello,
/// and still answers both in order.
#[test]
fn a_named_client_command_reaches_a_session_that_answers_the_hello_last() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let (session_thread, received_requests) = spawn_settled_session(
        runtime_directory.path(),
        session_id,
        5,
        HelloTiming::AfterTheNextRequest,
    );

    let command_result = submit_external_command_via_runtime_directory(
        runtime_directory.path(),
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
}

#[test]
fn a_named_client_reaches_a_session_that_speaks_this_builds_protocol() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let (session_thread, received_requests) = spawn_settled_session(
        runtime_directory.path(),
        session_id,
        PROTOCOL_VERSION,
        HelloTiming::AtOnce,
    );

    let command_result = submit_external_command_via_runtime_directory(
        runtime_directory.path(),
        None,
        session_id,
        Some(client_id),
        Command::TogglePaneFullscreen,
    )
    .expect("a session speaking this build's protocol reads the target client");

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
}

#[test]
fn no_named_client_still_costs_one_round_trip() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_thread, received_requests) = spawn_settled_session(
        runtime_directory.path(),
        session_id,
        PROTOCOL_VERSION,
        HelloTiming::AfterTheNextRequest,
    );

    let command_result = submit_external_command_via_runtime_directory(
        runtime_directory.path(),
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
fn an_id_sockets_in_two_folders_advertise_is_reached_through_neither() {
    // Both folders belong to the user running the test.
    use std::os::unix::fs::MetadataExt;

    let shared_sessions_base_directory = build_test_runtime_directory();
    let runtime_directory = shared_sessions_base_directory
        .path()
        .join("missing-runtime");
    let own_user_id = std::fs::metadata(shared_sessions_base_directory.path())
        .expect("read the shared directory")
        .uid();
    let duplicated_session_id = SessionId::new();
    for folder_offset in [1, 2] {
        let user_directory = shared_sessions_base_directory
            .path()
            .join((own_user_id + folder_offset).to_string());
        std::fs::create_dir_all(&user_directory).expect("create a user's directory");
        plant_session_socket(&user_directory.join(format!("{duplicated_session_id}.sock")));
    }
    let expected_refusal_text = format!(
        "session {duplicated_session_id} is advertised 2 times in the shared directory, by user \
         id {own_user_id}; koshi reaches none of them"
    );

    let foreign_session_listing =
        list_foreign_sessions(shared_sessions_base_directory.path(), &runtime_directory);
    let lookup_result = find_foreign_session_address(
        shared_sessions_base_directory.path(),
        &runtime_directory,
        duplicated_session_id,
    );
    let endpoint_lookup_result = load_session_endpoint(
        &runtime_directory,
        Some(shared_sessions_base_directory.path()),
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

    let runtime_directory = build_test_runtime_directory();
    let shared_sessions_base_directory = build_test_runtime_directory();
    let own_user_id = std::fs::metadata(runtime_directory.path())
        .expect("read the runtime directory")
        .uid();
    let foreign_user_directory = shared_sessions_base_directory
        .path()
        .join((own_user_id + 1).to_string());
    std::fs::create_dir_all(&foreign_user_directory).expect("create the other user's directory");
    let dead_swap_session_id = SessionId::new();
    let restarting_session_id = SessionId::new();
    for own_session_id in [dead_swap_session_id, restarting_session_id] {
        plant_session_socket(&foreign_user_directory.join(format!("{own_session_id}.sock")));
    }
    write_aged_resume_file(
        runtime_directory.path(),
        dead_swap_session_id,
        RESTART_WINDOW_DURATION + Duration::from_secs(1),
    );
    write_aged_resume_file(
        runtime_directory.path(),
        restarting_session_id,
        Duration::ZERO,
    );

    let foreign_session_listing = list_foreign_sessions(
        shared_sessions_base_directory.path(),
        runtime_directory.path(),
    );
    let dead_swap_lookup = find_foreign_session_address(
        shared_sessions_base_directory.path(),
        runtime_directory.path(),
        dead_swap_session_id,
    )
    .expect("the shared directory is read");
    let dead_swap_endpoint_lookup = load_session_endpoint(
        runtime_directory.path(),
        Some(shared_sessions_base_directory.path()),
        dead_swap_session_id,
    );
    let restarting_endpoint_lookup = load_session_endpoint(
        runtime_directory.path(),
        Some(shared_sessions_base_directory.path()),
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
}

#[cfg(windows)]
#[test]
fn a_lookup_by_id_finds_the_marker_of_that_id_and_no_other() {
    let runtime_directory = build_test_runtime_directory();
    let shared_sessions_base_directory = build_test_runtime_directory();
    let foreign_session_id = SessionId::new();
    let own_session_id = SessionId::new();
    for marked_session_id in [foreign_session_id, own_session_id] {
        std::fs::write(
            shared_sessions_base_directory
                .path()
                .join(marked_session_id.to_string()),
            b"",
        )
        .expect("plant a marker");
    }
    write_own_endpoint_file(runtime_directory.path(), own_session_id);

    let found_socket_addresses: Vec<Option<String>> =
        [foreign_session_id, own_session_id, SessionId::new()]
            .into_iter()
            .map(|session_id| {
                find_foreign_session_address(
                    shared_sessions_base_directory.path(),
                    runtime_directory.path(),
                    session_id,
                )
                .expect("the shared directory is read")
            })
            .collect();

    assert_eq!(
        found_socket_addresses,
        vec![Some(format!("koshi-{foreign_session_id}")), None, None]
    );
}

#[test]
fn this_users_sessions_are_the_advertised_ones_and_the_ones_restarting() {
    let runtime_directory = build_test_runtime_directory();
    let mut advertised_session_ids = vec![SessionId::new(), SessionId::new()];
    for advertised_session_id in &advertised_session_ids {
        write_own_endpoint_file(runtime_directory.path(), *advertised_session_id);
    }
    write_aged_resume_file(
        runtime_directory.path(),
        advertised_session_ids[0],
        Duration::ZERO,
    );
    let restarting_session_id = SessionId::new();
    write_aged_resume_file(
        runtime_directory.path(),
        restarting_session_id,
        Duration::ZERO,
    );
    write_aged_resume_file(
        runtime_directory.path(),
        SessionId::new(),
        RESTART_WINDOW_DURATION + Duration::from_secs(1),
    );

    let mut own_session_ids =
        list_own_sessions(runtime_directory.path()).expect("read the runtime directory");

    own_session_ids.sort();
    advertised_session_ids.push(restarting_session_id);
    advertised_session_ids.sort();
    assert_eq!(own_session_ids, advertised_session_ids);
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

    let runtime_directory = build_test_runtime_directory();
    let shared_sessions_base_directory = build_test_runtime_directory();
    let own_user_id = std::fs::metadata(runtime_directory.path())
        .expect("read the runtime directory")
        .uid();
    let closed_user_directory = shared_sessions_base_directory
        .path()
        .join((own_user_id + 1).to_string());
    let open_user_directory = shared_sessions_base_directory
        .path()
        .join((own_user_id + 2).to_string());
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
        return;
    }

    let foreign_session_listing = list_foreign_sessions(
        shared_sessions_base_directory.path(),
        runtime_directory.path(),
    );
    let closed_session_lookup = find_foreign_session_address(
        shared_sessions_base_directory.path(),
        runtime_directory.path(),
        closed_session_id,
    );
    let open_session_lookup = find_foreign_session_address(
        shared_sessions_base_directory.path(),
        runtime_directory.path(),
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
}

#[cfg(unix)]
#[test]
fn a_shared_directory_whose_read_fails_another_way_is_kept_and_refuses_a_lookup() {
    // A link to itself fails every read with `ELOOP`.
    let runtime_directory = build_test_runtime_directory();
    let looping_shared_directory = runtime_directory.path().join("looping");
    std::os::unix::fs::symlink("looping", &looping_shared_directory)
        .expect("link the shared directory to itself");
    let shared_read_error =
        std::fs::read_dir(&looping_shared_directory).expect_err("a link to itself cannot be read");
    let unread_shared_directory =
        UnreadPath::from_read_error(&looping_shared_directory, &shared_read_error);

    let foreign_session_listing =
        list_foreign_sessions(&looping_shared_directory, runtime_directory.path());
    let lookup_error = find_foreign_session_address(
        &looping_shared_directory,
        runtime_directory.path(),
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
}

/// The token a test's caller connected under, which the wait watches for a
/// change.
const OLD_CONNECTION_TOKEN: &str = "the token this caller connected under";

/// The token the image replacing the server mints when it binds again.
const NEW_CONNECTION_TOKEN: &str = "the token the new image minted";

#[test]
fn wait_for_new_session_endpoint_takes_the_endpoint_file_the_moment_it_names_another_token() {
    let runtime_directory = build_test_runtime_directory();
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
    let runtime_directory = build_test_runtime_directory();
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
    let runtime_directory = build_test_runtime_directory();
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
    let runtime_directory = build_test_runtime_directory();
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
    let runtime_directory = build_test_runtime_directory();
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
    let runtime_directory = build_test_runtime_directory();
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
fn a_poll_pause_is_the_poll_interval_or_the_time_left_when_less_is_left() {
    assert_eq!(
        compute_poll_pause_duration(Instant::now() + Duration::from_secs(3600)),
        RESTART_POLL_INTERVAL_DURATION
    );
    assert_eq!(compute_poll_pause_duration(Instant::now()), Duration::ZERO);

    let wait_deadline = Instant::now() + Duration::from_millis(10);
    let poll_pause_duration = compute_poll_pause_duration(wait_deadline);
    let computed_at = Instant::now();
    assert!(poll_pause_duration <= Duration::from_millis(10));
    assert!(poll_pause_duration >= wait_deadline.saturating_duration_since(computed_at));
}

#[test]
fn a_session_of_koshi_0_4_0_names_restart_servers_without_waiting_for_a_restart() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let stand_in_thread = spawn_previous_release_session(
        runtime_directory.path(),
        session_id,
        "k7QxSecret",
        vec![vec![
            PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT.to_string(),
            PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT.to_string(),
        ]],
    );
    let submit_start = Instant::now();

    let submit_error = submit_external_command_via_runtime_directory(
        runtime_directory.path(),
        None,
        session_id,
        None,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect_err("a session of koshi 0.4.0 reads no frame this build writes");

    assert!(
        submit_start.elapsed() < REFUSED_SERVER_RESTART_START_WAIT_DURATION,
        "the command did not wait for a restart"
    );
    stand_in_thread
        .join()
        .expect("the stand-in served its connection");
    assert_eq!(
        submit_error.to_string(),
        "IPC unavailable: the server answered in the format of koshi 0.4.0 or older, which this \
         koshi cannot talk to; the user who started it runs: koshi restart-servers"
    );
    let CliError::PreviousReleaseServer { .. } = submit_error else {
        panic!("expected PreviousReleaseServer, got {submit_error:?}");
    };
}

/// The Hello and the Restart this build sends a session of koshi 0.2.0 to
/// 0.4.0 whose endpoint file carries the token `connection_secret`, as their
/// JSON text.
fn format_previous_release_request_texts(connection_secret: &str) -> Vec<String> {
    vec![
        format!(
            r#"{{"request_id":1,"kind":{{"Hello":{{"min_protocol_version":2,"max_protocol_version":3,"token":"{connection_secret}"}}}}}}"#
        ),
        r#"{"request_id":2,"kind":"Restart"}"#.to_string(),
    ]
}

/// Ask the stand-in session of koshi 0.2.0 to 0.4.0 that answers this build's
/// exchange with `malformed_request`, then the exchange of its own envelope
/// with `previous_release_answer_texts`, to restart. Hands back what
/// [`restart_running_session`] gave and the frames of the second connection.
fn restart_previous_release_stand_in(
    previous_release_answer_texts: Vec<&'static str>,
) -> (Result<SessionRestart, CliError>, Vec<String>) {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let stand_in_thread = spawn_previous_release_session(
        runtime_directory.path(),
        session_id,
        "k7QxSecret",
        vec![
            vec![
                PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT.to_string(),
                PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT.to_string(),
            ],
            previous_release_answer_texts
                .into_iter()
                .map(str::to_string)
                .collect(),
        ],
    );

    let session_restart = restart_running_session(runtime_directory.path(), None, session_id);

    let mut request_texts_by_connection = stand_in_thread
        .join()
        .expect("the stand-in served both connections");
    (session_restart, request_texts_by_connection.remove(1))
}

#[test]
fn a_session_of_koshi_0_4_0_is_asked_again_in_its_own_envelope_and_restarts() {
    let (session_restart, previous_release_request_texts) =
        restart_previous_release_stand_in(vec![
            KOSHI_0_4_0_HELLO_ANSWER_TEXT,
            KOSHI_0_4_0_RESTARTING_ANSWER_TEXT,
        ]);

    assert_eq!(
        session_restart.expect("the session restarts"),
        SessionRestart::Restarting
    );
    assert_eq!(
        previous_release_request_texts,
        format_previous_release_request_texts("k7QxSecret")
    );
}

#[test]
fn a_session_of_koshi_0_2_0_refusing_the_restart_has_no_restart_request() {
    let (session_restart, previous_release_request_texts) =
        restart_previous_release_stand_in(vec![
            KOSHI_0_2_0_HELLO_ANSWER_TEXT,
            KOSHI_0_2_0_RESTART_REFUSAL_TEXT,
        ]);

    assert_eq!(
        session_restart.expect("the session answers"),
        SessionRestart::WithoutRestartRequest
    );
    assert_eq!(
        previous_release_request_texts,
        format_previous_release_request_texts("k7QxSecret")
    );
}

#[test]
fn a_session_of_koshi_0_2_0_pr_1_refusing_the_hello_has_no_restart_request() {
    let (session_restart, previous_release_request_texts) =
        restart_previous_release_stand_in(vec![
            PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT,
            PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT,
        ]);

    assert_eq!(
        session_restart.expect("the session answers"),
        SessionRestart::WithoutRestartRequest
    );
    assert_eq!(
        previous_release_request_texts,
        format_previous_release_request_texts("k7QxSecret")
    );
}

#[test]
fn a_session_of_koshi_0_4_0_refusing_the_token_gives_the_sentence_it_sent() {
    let (session_restart, _) = restart_previous_release_stand_in(vec![
        r#"{"request_id":1,"result":{"Error":{"code":"bad_token","message":"the token presented does not match this Koshi's"}}}"#,
        r#"{"request_id":2,"result":{"Error":{"code":"hello_required","message":"Restart arrived before a Hello opened the connection"}}}"#,
    ]);

    let Err(CliError::IpcUnavailable { detail }) = session_restart else {
        panic!("expected IpcUnavailable, got {session_restart:?}");
    };
    assert_eq!(detail, "the token presented does not match this Koshi's");
}

#[test]
fn a_restart_answered_with_a_hello_names_the_unexpected_reply() {
    let (session_restart, _) = restart_previous_release_stand_in(vec![
        KOSHI_0_4_0_HELLO_ANSWER_TEXT,
        KOSHI_0_4_0_HELLO_ANSWER_TEXT,
    ]);

    let Err(CliError::IpcUnavailable { detail }) = session_restart else {
        panic!("expected IpcUnavailable, got {session_restart:?}");
    };
    assert_eq!(
        detail,
        "the session answered with an unexpected Hello reply"
    );
}

#[test]
fn a_hello_answered_with_restarting_names_the_unexpected_reply() {
    let (session_restart, _) = restart_previous_release_stand_in(vec![
        KOSHI_0_4_0_RESTARTING_ANSWER_TEXT,
        KOSHI_0_4_0_RESTARTING_ANSWER_TEXT,
    ]);

    let Err(CliError::IpcUnavailable { detail }) = session_restart else {
        panic!("expected IpcUnavailable, got {session_restart:?}");
    };
    assert_eq!(
        detail,
        "the session answered with an unexpected Restarting reply"
    );
}

#[test]
fn find_previous_release_session_version_reads_the_build_a_koshi_0_4_0_session_names() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let stand_in_thread = spawn_previous_release_session(
        runtime_directory.path(),
        session_id,
        "k7QxSecret",
        vec![vec![KOSHI_0_4_0_HELLO_ANSWER_TEXT.to_string()]],
    );

    let session_version =
        find_previous_release_session_version(runtime_directory.path(), session_id)
            .expect("the session answers its Hello");

    let request_texts_by_connection = stand_in_thread
        .join()
        .expect("the stand-in served its connection");
    assert_eq!(session_version, Some("0.4.0".to_string()));
    assert_eq!(
        request_texts_by_connection,
        vec![vec![
            format_previous_release_request_texts("k7QxSecret").remove(0)
        ]]
    );
}

#[test]
fn find_previous_release_session_version_finds_no_session_without_an_endpoint_file() {
    let runtime_directory = build_test_runtime_directory();

    assert_eq!(
        find_previous_release_session_version(runtime_directory.path(), SessionId::new())
            .expect("no session is no failure"),
        None
    );
}

#[test]
fn a_session_refusing_this_builds_protocol_version_is_asked_again_once_it_restarts() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let (session_server_thread, submitted_envelopes) = spawn_fake_session(
        runtime_directory.path(),
        session_id,
        vec![SessionScript::RefuseVersion, SessionScript::AcceptAndApply],
    );

    let command_result = submit_external_command_via_runtime_directory(
        runtime_directory.path(),
        None,
        session_id,
        None,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
    .expect("the restarted session answers");

    session_server_thread.join().expect("fake session exits");
    let refused_envelope = submitted_envelopes
        .recv()
        .expect("the refusing session read one command");
    let command_envelope = submitted_envelopes
        .recv()
        .expect("the restarted session read one command");
    assert_eq!(refused_envelope.command, command_envelope.command);
    assert_eq!(
        command_result,
        CommandResult::Ok {
            command_id: command_envelope.command_id,
            emitted_events: Vec::new(),
        },
    );
}

/// The refusal a session sends to a Hello whose connection token it does not
/// hold.
fn build_connection_token_refusal() -> CliError {
    CliError::ConnectionTokenRefused {
        detail: "the token presented does not match this Koshi's".to_string(),
    }
}

#[test]
fn a_token_refusal_from_another_users_session_comes_back_at_once() {
    let runtime_directory = build_test_runtime_directory();
    let foreign_endpoint = build_foreign_session_endpoint("/home/user/shared.sock".to_string());
    let mut exchange_count = 0;
    let wait_started_at = Instant::now();

    let (exchanged_endpoint, exchange_result) = run_session_exchange_with_token_wait(
        runtime_directory.path(),
        SessionId::new(),
        foreign_endpoint.clone(),
        None,
        |_| -> Result<(), CliError> {
            exchange_count += 1;
            Err(build_connection_token_refusal())
        },
    );

    let Err(CliError::ConnectionTokenRefused { detail }) = exchange_result else {
        panic!("expected ConnectionTokenRefused, got {exchange_result:?}");
    };
    assert_eq!(detail, "the token presented does not match this Koshi's");
    assert_eq!(exchanged_endpoint, foreign_endpoint);
    assert_eq!(exchange_count, 1);
    assert!(wait_started_at.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_token_refusal_whose_endpoint_file_keeps_its_token_comes_back_at_the_answer_deadline() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let refused_endpoint = write_session_endpoint_file(
        runtime_directory.path(),
        session_id,
        OLD_CONNECTION_TOKEN,
        5000,
    );
    let mut exchange_count = 0;
    let wait_started_at = Instant::now();

    let (exchanged_endpoint, exchange_result) = run_session_exchange_with_token_wait(
        runtime_directory.path(),
        session_id,
        refused_endpoint.clone(),
        Some(Instant::now()),
        |_| -> Result<(), CliError> {
            exchange_count += 1;
            Err(build_connection_token_refusal())
        },
    );

    let Err(CliError::ConnectionTokenRefused { detail }) = exchange_result else {
        panic!("expected ConnectionTokenRefused, got {exchange_result:?}");
    };
    assert_eq!(detail, "the token presented does not match this Koshi's");
    assert_eq!(exchanged_endpoint, refused_endpoint);
    assert_eq!(exchange_count, 1);
    assert!(wait_started_at.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_token_refusal_comes_back_without_a_second_exchange_once_the_answer_deadline_is_reached() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let refused_endpoint = write_session_endpoint_file(
        runtime_directory.path(),
        session_id,
        OLD_CONNECTION_TOKEN,
        5000,
    );
    write_session_endpoint_file(
        runtime_directory.path(),
        session_id,
        NEW_CONNECTION_TOKEN,
        5000,
    );
    let mut exchange_count = 0;

    let (exchanged_endpoint, exchange_result) = run_session_exchange_with_token_wait(
        runtime_directory.path(),
        session_id,
        refused_endpoint.clone(),
        Some(Instant::now()),
        |_| -> Result<(), CliError> {
            exchange_count += 1;
            Err(build_connection_token_refusal())
        },
    );

    let Err(CliError::ConnectionTokenRefused { detail }) = exchange_result else {
        panic!("expected ConnectionTokenRefused, got {exchange_result:?}");
    };
    assert_eq!(detail, "the token presented does not match this Koshi's");
    assert_eq!(exchanged_endpoint, refused_endpoint);
    assert_eq!(exchange_count, 1);
}

#[test]
fn wait_for_refused_session_restart_gives_another_users_session_its_refusal_at_once() {
    let runtime_directory = build_test_runtime_directory();
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
    let runtime_directory = build_test_runtime_directory();
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
    let runtime_directory = build_test_runtime_directory();
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
    let runtime_directory = build_test_runtime_directory();
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
    let runtime_directory = build_test_runtime_directory();
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
    let runtime_directory = build_test_runtime_directory();
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
    let runtime_directory = build_test_runtime_directory();
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
