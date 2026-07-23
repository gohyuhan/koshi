//! Tests for choosing and ending a running session.

use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::SystemTime;

use koshi_core::command::CliExitCode;
use koshi_core::discovery::{SessionInfo, SessionOverview};
use koshi_core::event::Event;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::protocol::{ConnectionToken, IpcRequest, IpcRequestKind, IpcResponse, IpcResult};
use koshi_ipc::transport::{Connection, Listener};

use super::*;

fn overview(name: &str) -> SessionOverview {
    let session_id = SessionId::new();
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
        reply(&mut discovery, hello.request_id, IpcResult::Hello);
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
        reply(&mut kill, hello.request_id, IpcResult::Hello);
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
fn duplicate_names_are_ambiguous() {
    let error = select_kill_session(
        &census(vec![overview("quiet-lake"), overview("quiet-lake")]),
        Some("quiet-lake"),
    )
    .expect_err("two sessions share the name");

    assert!(matches!(
        error,
        CliError::CommandRejected {
            reason: RejectReason::TargetAmbiguous,
            ..
        }
    ));
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

    let result = kill_session_in(&runtime_dir, Some("quiet-lake")).expect("kill exchange succeeds");

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
