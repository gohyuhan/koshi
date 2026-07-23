//! Tests for the CLI side of the control socket, against a scripted
//! stand-in session serving a real socket.

use std::thread::JoinHandle;

use koshi_core::command::ToggleLockModeArgs;
use koshi_core::ids::{PaneId, SessionId};
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode};
use koshi_ipc::transport::Listener;

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
                send(
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

/// Answer `request_id` with `result` on `connection`.
fn send(connection: &mut Connection, request_id: u64, result: IpcResult) {
    connection
        .send(&IpcResponse {
            request_id: Some(request_id),
            result,
        })
        .expect("send scripted reply");
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
