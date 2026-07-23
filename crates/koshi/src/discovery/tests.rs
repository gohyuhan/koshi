//! Tests for the discovery answers: row building across sessions, `inspect`
//! lookups, and the endpoint sweep that removes what a session left behind.

use std::path::PathBuf;
use std::time::SystemTime;

use koshi_core::event::RejectReason;
use koshi_core::geometry::Size;
use koshi_core::lock::LockMode;
use koshi_ipc::protocol::{ConnectionToken, IpcRequest, IpcResponse, IpcResult};
use koshi_ipc::transport::{Connection, Listener};

use super::*;

/// A fresh directory to stand in for the runtime dir, under a short base so
/// the Unix socket path stays inside the OS path-length cap.
fn test_runtime_dir(tag: &str) -> PathBuf {
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    let dir = base.join(format!("koshi-discovery-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create runtime dir");
    dir
}

/// Advertise `session` at `runtime_dir` with `socket` as its address.
fn advertise(runtime_dir: &Path, session: SessionId, socket: String) -> PathBuf {
    let path = EndpointFile::path(runtime_dir, session);
    EndpointFile {
        socket,
        token: ConnectionToken::generate(),
    }
    .write(&path)
    .expect("endpoint file written");
    path
}

/// A stand-in koshi serving one discovery exchange for `overview` over a
/// real socket at `runtime_dir`: answer the Hello, then hand back the
/// overview.
fn serve_overview(runtime_dir: &Path, overview: SessionOverview) -> std::thread::JoinHandle<()> {
    let session = overview.session.id;
    let socket = koshi_ipc::endpoint::socket_addr(runtime_dir, session);
    let listener = Listener::bind(&socket).expect("stand-in session binds");
    advertise(runtime_dir, session, socket);

    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the CLI");
        let hello: IpcRequest = connection.recv().expect("read hello");
        let query: IpcRequest = connection.recv().expect("read discovery request");
        reply(&mut connection, hello.request_id, IpcResult::Hello);
        reply(
            &mut connection,
            query.request_id,
            IpcResult::Overview(overview),
        );
    })
}

/// Answer `request_id` with `result` on `connection`.
fn reply(connection: &mut Connection, request_id: u64, result: IpcResult) {
    connection
        .send(&IpcResponse {
            request_id: Some(request_id),
            result,
        })
        .expect("send scripted reply");
}

/// A session overview with `tabs` (name, panes-per-tab), one client, and
/// pane titles derived from their position, so every row is identifiable.
fn overview(name: &str, tabs: &[(&str, usize)]) -> SessionOverview {
    let session_id = SessionId::new();
    let mut tab_infos = Vec::new();
    let mut pane_infos = Vec::new();
    for (index, (tab_name, panes)) in tabs.iter().enumerate() {
        let tab_id = TabId::new();
        tab_infos.push(TabInfo {
            id: tab_id,
            session_id,
            name: (*tab_name).to_string(),
            index,
            active_pane: None,
            pane_count: *panes,
        });
        for pane in 0..*panes {
            pane_infos.push(PaneInfo {
                id: PaneId::new(),
                tab_id,
                session_id,
                title: Some(format!("{tab_name}-{pane}")),
                cwd: None,
                command: None,
                state: koshi_core::discovery::PaneState::Running,
                focused_by_clients: Vec::new(),
            });
        }
    }
    SessionOverview {
        session: SessionInfo {
            id: session_id,
            name: name.to_string(),
            created_at: SystemTime::UNIX_EPOCH,
            attached_clients: Vec::new(),
            pane_count: pane_infos.len(),
        },
        tabs: tab_infos,
        panes: pane_infos,
        clients: vec![ClientInfo {
            id: ClientId::new(),
            session_id,
            attached_at: SystemTime::UNIX_EPOCH,
            viewport_size: Size { cols: 80, rows: 24 },
            active_tab: TabId::new(),
            focused_pane: None,
            lock_state: LockMode::Normal,
        }],
    }
}

#[test]
fn session_rows_are_one_row_per_session() {
    let overviews = vec![overview("quiet-lake", &[]), overview("amber-fox", &[])];
    let rows = session_rows(&overviews);
    assert_eq!(
        rows,
        vec![
            SessionRow {
                id: overviews[0].session.id,
                name: "quiet-lake".to_string(),
            },
            SessionRow {
                id: overviews[1].session.id,
                name: "amber-fox".to_string(),
            },
        ]
    );
}

#[test]
fn tab_rows_span_every_session_in_bar_order() {
    let overviews = vec![
        overview("quiet-lake", &[("editor", 1), ("logs", 1)]),
        overview("amber-fox", &[("shell", 1)]),
    ];
    let rows = tab_rows(&overviews);
    assert_eq!(
        rows,
        vec![
            TabRow {
                id: overviews[0].tabs[0].id,
                name: "editor".to_string(),
                session: overviews[0].session.id,
                session_name: "quiet-lake".to_string(),
            },
            TabRow {
                id: overviews[0].tabs[1].id,
                name: "logs".to_string(),
                session: overviews[0].session.id,
                session_name: "quiet-lake".to_string(),
            },
            TabRow {
                id: overviews[1].tabs[0].id,
                name: "shell".to_string(),
                session: overviews[1].session.id,
                session_name: "amber-fox".to_string(),
            },
        ]
    );
}

#[test]
fn pane_rows_carry_the_tab_and_session_they_belong_to() {
    let overviews = vec![
        overview("quiet-lake", &[("editor", 2)]),
        overview("amber-fox", &[("shell", 1)]),
    ];
    let rows = pane_rows(&overviews);
    assert_eq!(
        rows,
        vec![
            PaneRow {
                id: overviews[0].panes[0].id,
                name: Some("editor-0".to_string()),
                tab: overviews[0].tabs[0].id,
                tab_name: "editor".to_string(),
                session: overviews[0].session.id,
                session_name: "quiet-lake".to_string(),
            },
            PaneRow {
                id: overviews[0].panes[1].id,
                name: Some("editor-1".to_string()),
                tab: overviews[0].tabs[0].id,
                tab_name: "editor".to_string(),
                session: overviews[0].session.id,
                session_name: "quiet-lake".to_string(),
            },
            PaneRow {
                id: overviews[1].panes[0].id,
                name: Some("shell-0".to_string()),
                tab: overviews[1].tabs[0].id,
                tab_name: "shell".to_string(),
                session: overviews[1].session.id,
                session_name: "amber-fox".to_string(),
            },
        ]
    );
}

#[test]
fn a_pane_whose_tab_is_not_listed_produces_no_row() {
    let mut overviews = vec![overview("quiet-lake", &[("editor", 1)])];
    overviews[0].tabs.clear();
    assert_eq!(pane_rows(&overviews), Vec::new());
}

#[test]
fn client_rows_name_the_session_they_are_attached_to() {
    let overviews = vec![overview("quiet-lake", &[("editor", 1)])];
    assert_eq!(
        client_rows(&overviews),
        vec![ClientRow {
            id: overviews[0].clients[0].id,
            session: overviews[0].session.id,
            session_name: "quiet-lake".to_string(),
        }]
    );
}

#[test]
fn inspect_finds_an_entity_in_the_second_session() {
    let overviews = vec![
        overview("quiet-lake", &[("editor", 1)]),
        overview("amber-fox", &[("shell", 1)]),
    ];
    let pane = &overviews[1].panes[0];
    assert_eq!(find_pane(&overviews, pane.id).expect("pane found"), *pane);
    assert_eq!(
        find_tab(&overviews, overviews[1].tabs[0].id).expect("tab found"),
        overviews[1].tabs[0]
    );
    assert_eq!(
        find_client(&overviews, overviews[1].clients[0].id).expect("client found"),
        overviews[1].clients[0]
    );
    assert_eq!(
        find_session(&overviews, overviews[1].session.id).expect("session found"),
        overviews[1].session
    );
}

#[test]
fn inspecting_an_unknown_pane_reports_the_target_as_not_found() {
    let overviews = vec![overview("quiet-lake", &[("editor", 1)])];
    let missing = PaneId::new();
    let error = find_pane(&overviews, missing).expect_err("no such pane");
    match error {
        CliError::CommandRejected { reason, help } => {
            assert_eq!(reason, RejectReason::TargetNotFound);
            assert_eq!(help, Some(format!("no running session has pane {missing}")));
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn fetching_all_from_an_empty_runtime_dir_answers_no_sessions() {
    let dir = test_runtime_dir("empty");
    assert!(fetch_all(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_endpoint_nobody_listens_behind_is_swept() {
    let dir = test_runtime_dir("stale");
    let session_id = SessionId::new();
    let socket = koshi_ipc::endpoint::socket_addr(&dir, session_id);
    let endpoint_path = advertise(&dir, session_id, socket.clone());

    assert!(fetch_all(&dir).is_empty());
    assert!(
        !endpoint_path.exists(),
        "the endpoint file of a session that is gone is removed"
    );
    #[cfg(unix)]
    assert!(
        !Path::new(&socket).exists(),
        "the socket file of a session that is gone is removed"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_listening_endpoint_survives_a_failed_exchange() {
    // A session that accepts the connection and then hangs up: something IS
    // serving there, so the endpoint stays even though the exchange gets no
    // answer and the session contributes no rows.
    let dir = test_runtime_dir("live-but-mute");
    let session_id = SessionId::new();
    let socket = koshi_ipc::endpoint::socket_addr(&dir, session_id);
    let listener = Listener::bind(&socket).expect("listener binds");
    let endpoint_path = advertise(&dir, session_id, socket.clone());
    let serving = std::thread::spawn(move || {
        // Accepting and dropping closes the connection mid-exchange.
        let _ = listener.accept();
    });

    assert!(fetch_all(&dir).is_empty());
    serving
        .join()
        .expect("the stand-in session thread finishes");
    assert!(
        endpoint_path.exists(),
        "an endpoint something listens behind is kept"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn two_running_sessions_merge_into_one_listing() {
    // The acceptance bar: two koshi processes, each serving its own socket,
    // and one `list-panes` showing both sessions' panes.
    let dir = test_runtime_dir("two-sessions");
    let quiet = overview("quiet-lake", &[("editor", 2)]);
    let amber = overview("amber-fox", &[("shell", 1)]);
    let quiet_id = quiet.session.id;
    let amber_id = amber.session.id;
    let first = serve_overview(&dir, quiet);
    let second = serve_overview(&dir, amber);

    let overviews = fetch_all(&dir);
    first.join().expect("the first stand-in session finishes");
    second.join().expect("the second stand-in session finishes");

    // Sorted by session name, so `amber-fox` comes before `quiet-lake`
    // whatever order the runtime directory listed the endpoint files in.
    assert_eq!(
        session_rows(&overviews),
        vec![
            SessionRow {
                id: amber_id,
                name: "amber-fox".to_string(),
            },
            SessionRow {
                id: quiet_id,
                name: "quiet-lake".to_string(),
            },
        ]
    );
    let panes = pane_rows(&overviews);
    assert_eq!(
        panes
            .iter()
            .map(|pane| (pane.session, pane.name.clone()))
            .collect::<Vec<_>>(),
        vec![
            (amber_id, Some("shell-0".to_string())),
            (quiet_id, Some("editor-0".to_string())),
            (quiet_id, Some("editor-1".to_string())),
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn one_session_can_be_fetched_on_its_own() {
    let dir = test_runtime_dir("one-session");
    let quiet = overview("quiet-lake", &[("editor", 1)]);
    let quiet_id = quiet.session.id;
    let serving = serve_overview(&dir, quiet);

    let fetched = fetch_one(&dir, quiet_id).expect("the session answers");
    serving.join().expect("the stand-in session finishes");
    assert_eq!(fetched.session.id, quiet_id);
    assert_eq!(fetched.session.name, "quiet-lake");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetching_one_live_session_that_cannot_answer_is_not_reported_as_gone() {
    // The endpoint accepts and hangs up: the session IS running, so the
    // failure must stay a transport failure rather than "not running".
    let dir = test_runtime_dir("one-live-but-mute");
    let session_id = SessionId::new();
    let socket = koshi_ipc::endpoint::socket_addr(&dir, session_id);
    let listener = Listener::bind(&socket).expect("listener binds");
    let endpoint_path = advertise(&dir, session_id, socket);
    let serving = std::thread::spawn(move || {
        let _ = listener.accept();
    });

    let error = fetch_one(&dir, session_id).expect_err("the exchange cannot finish");
    serving
        .join()
        .expect("the stand-in session thread finishes");
    assert!(
        matches!(error, CliError::IpcUnavailable { .. }),
        "unexpected error: {error}"
    );
    assert!(
        endpoint_path.exists(),
        "an endpoint something listens behind is kept"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetching_one_session_that_is_gone_reports_it_as_not_running() {
    let dir = test_runtime_dir("one-missing");
    let session_id = SessionId::new();
    let error = fetch_one(&dir, session_id).expect_err("nothing advertises it");
    match error {
        CliError::SessionNotFound { session } => {
            assert_eq!(session, session_id.to_string());
        }
        other => panic!("unexpected error: {other}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
