//! Tests for the discovery answers: row building across sessions, `inspect`
//! lookups, and the endpoint sweep that removes what a session left behind.

use koshi_core::client::ClientOrigin;
use std::path::PathBuf;
use std::time::SystemTime;

use koshi_core::discovery::SessionInfo;
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

/// A census where every running session answered.
fn census(overviews: Vec<SessionOverview>) -> Discovered {
    Discovered {
        sessions: overviews,
        unasked: 0,
    }
}

/// A census missing `unasked` sessions: running and listening, but unable to
/// say what they hold.
fn partial(overviews: Vec<SessionOverview>, unasked: usize) -> Discovered {
    Discovered {
        sessions: overviews,
        unasked,
    }
}

/// Advertise `session` at `runtime_dir` with `socket` as its address.
fn advertise(runtime_dir: &Path, session: SessionId, socket: String) -> PathBuf {
    let path = EndpointFile::path(runtime_dir, session);
    EndpointFile {
        socket,
        token: ConnectionToken::generate(),
        pid: std::process::id(),
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
        reply(
            &mut connection,
            hello.request_id,
            IpcResult::Hello {
                protocol_version: koshi_ipc::protocol::PROTOCOL_VERSION,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        );
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
            origin: Some(ClientOrigin::Local),
            pane_area: None,
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
                server: None,
            },
            SessionRow {
                id: overviews[1].session.id,
                name: "amber-fox".to_string(),
                server: None,
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
fn client_rows_are_one_row_per_attached_client_across_sessions() {
    let mut overviews = vec![
        overview("quiet-lake", &[("editor", 1)]),
        overview("amber-fox", &[("shell", 1)]),
    ];
    let second_client = ClientInfo {
        id: ClientId::new(),
        ..overviews[0].clients[0].clone()
    };
    overviews[0].clients.push(second_client.clone());

    assert_eq!(
        client_rows(&overviews),
        vec![
            ClientRow {
                id: overviews[0].clients[0].id,
                session: overviews[0].session.id,
                session_name: "quiet-lake".to_string(),
            },
            ClientRow {
                id: second_client.id,
                session: overviews[0].session.id,
                session_name: "quiet-lake".to_string(),
            },
            ClientRow {
                id: overviews[1].clients[0].id,
                session: overviews[1].session.id,
                session_name: "amber-fox".to_string(),
            },
        ]
    );
}

#[test]
fn a_session_with_no_client_attached_contributes_no_client_row() {
    let mut overviews = vec![overview("quiet-lake", &[("editor", 1)])];
    overviews[0].clients.clear();
    assert_eq!(client_rows(&overviews), Vec::new());
}

#[test]
fn every_listing_over_no_sessions_is_empty() {
    let none: [SessionOverview; 0] = [];
    assert_eq!(session_rows(&none), Vec::new());
    assert_eq!(tab_rows(&none), Vec::new());
    assert_eq!(pane_rows(&none), Vec::new());
    assert_eq!(client_rows(&none), Vec::new());
}

#[test]
fn a_tab_holding_no_panes_is_listed_and_contributes_no_pane_row() {
    let overviews = vec![overview("quiet-lake", &[("empty", 0)])];
    assert_eq!(
        tab_rows(&overviews),
        vec![TabRow {
            id: overviews[0].tabs[0].id,
            name: "empty".to_string(),
            session: overviews[0].session.id,
            session_name: "quiet-lake".to_string(),
        }]
    );
    assert_eq!(pane_rows(&overviews), Vec::new());
}

#[test]
fn a_pane_whose_child_set_no_title_yields_a_row_with_no_name() {
    let mut overviews = vec![overview("quiet-lake", &[("editor", 1)])];
    overviews[0].panes[0].title = None;
    assert_eq!(
        pane_rows(&overviews),
        vec![PaneRow {
            id: overviews[0].panes[0].id,
            name: None,
            tab: overviews[0].tabs[0].id,
            tab_name: "editor".to_string(),
            session: overviews[0].session.id,
            session_name: "quiet-lake".to_string(),
        }]
    );
}

#[test]
fn sorting_a_census_orders_by_name_then_id() {
    let zulu = overview("zulu", &[]);
    let first_alpha = overview("alpha", &[]);
    let second_alpha = overview("alpha", &[]);
    let mut alpha_ids = [first_alpha.session.id, second_alpha.session.id];
    alpha_ids.sort();

    let mut found = census(vec![zulu.clone(), first_alpha, second_alpha]);
    found.sort_sessions();

    assert_eq!(
        found
            .sessions
            .iter()
            .map(|overview| overview.session.id)
            .collect::<Vec<_>>(),
        vec![alpha_ids[0], alpha_ids[1], zulu.session.id]
    );
}

#[test]
fn inspect_finds_an_entity_in_the_second_session() {
    let found = census(vec![
        overview("quiet-lake", &[("editor", 1)]),
        overview("amber-fox", &[("shell", 1)]),
    ]);
    let second = &found.sessions[1];
    assert_eq!(
        find_pane(&found, second.panes[0].id).expect("pane found"),
        second.panes[0]
    );
    assert_eq!(
        find_tab(&found, second.tabs[0].id).expect("tab found"),
        second.tabs[0]
    );
    assert_eq!(
        find_client(&found, second.clients[0].id).expect("client found"),
        second.clients[0]
    );
}

#[test]
fn inspecting_an_unknown_pane_reports_the_target_as_not_found() {
    let found = census(vec![overview("quiet-lake", &[("editor", 1)])]);
    let missing = PaneId::new();
    let error = find_pane(&found, missing).expect_err("no such pane");
    match error {
        CliError::CommandRejected { reason, help } => {
            assert_eq!(reason, RejectReason::TargetNotFound);
            assert_eq!(help, Some(format!("no running session has pane {missing}")));
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn inspecting_an_unknown_tab_or_client_reports_the_target_as_not_found() {
    let found = census(vec![overview("quiet-lake", &[("editor", 1)])]);

    let tab = TabId::new();
    match find_tab(&found, tab).expect_err("no such tab") {
        CliError::CommandRejected { reason, help } => {
            assert_eq!(reason, RejectReason::TargetNotFound);
            assert_eq!(help, Some(format!("no running session has tab {tab}")));
        }
        other => panic!("unexpected error: {other}"),
    }

    let client = ClientId::new();
    match find_client(&found, client).expect_err("no such client") {
        CliError::CommandRejected { reason, help } => {
            assert_eq!(reason, RejectReason::TargetNotFound);
            assert_eq!(
                help,
                Some(format!("no running session has client {client}"))
            );
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn two_sessions_unasked_are_counted_in_the_plural() {
    let found = partial(vec![overview("quiet-lake", &[("editor", 1)])], 2);
    let tab = TabId::new();

    match find_tab(&found, tab).expect_err("the census is incomplete") {
        CliError::IpcUnavailable { detail } => assert_eq!(
            detail,
            format!(
                "tab {tab} is in none of the sessions that answered \
                 (2 running sessions did not answer)"
            )
        ),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn inspecting_with_a_session_unasked_reports_the_gap_not_a_miss() {
    // One session answered and one could not be asked: the pane may well be
    // in the session that stayed silent, so "not found" would be a guess.
    let found = partial(vec![overview("quiet-lake", &[("editor", 1)])], 1);
    let missing = PaneId::new();
    let error = find_pane(&found, missing).expect_err("the census is incomplete");
    match error {
        CliError::IpcUnavailable { detail } => assert_eq!(
            detail,
            format!(
                "pane {missing} is in none of the sessions that answered \
                 (1 running session did not answer)"
            )
        ),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn a_session_no_answering_session_matched_is_reported_as_not_running() {
    let found = census(vec![overview("quiet-lake", &[("editor", 1)])]);

    match found.no_such_session("amber-fox") {
        CliError::SessionNotFound { session } => assert_eq!(session, "amber-fox"),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn a_session_missed_while_one_went_unasked_reports_the_gap_not_a_miss() {
    // The name may well belong to the session that stayed silent, so "not
    // running" would be a guess.
    let found = partial(vec![overview("quiet-lake", &[("editor", 1)])], 1);

    match found.no_such_session("amber-fox") {
        CliError::IpcUnavailable { detail } => assert_eq!(
            detail,
            "`amber-fox` is not among the sessions that answered \
             (1 running session did not answer)"
        ),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn one_session_asked_directly_is_a_complete_census_of_itself() {
    let only = overview("quiet-lake", &[("editor", 1)]);

    let found = Discovered::of(only.clone());

    assert_eq!(found.unasked, 0);
    assert!(found.is_complete());
    assert_eq!(found.sessions, vec![only]);
}

#[test]
fn a_complete_listing_reports_no_gap() {
    assert!(census(vec![overview("quiet-lake", &[("editor", 1)])])
        .incomplete_listing()
        .is_none());
}

#[test]
fn a_listing_missing_a_session_reports_the_gap() {
    // The rows still print; the exit code is what says they are not all of
    // them, since a script reads stdout and the exit code, not stderr.
    let found = partial(vec![overview("quiet-lake", &[("editor", 1)])], 2);
    match found
        .incomplete_listing()
        .expect("the census is incomplete")
    {
        CliError::IpcUnavailable { detail } => assert_eq!(
            detail,
            "this listing is incomplete (2 running sessions did not answer)"
        ),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn an_unanswered_failure_over_a_complete_census_counts_zero_sessions() {
    // `unanswered` is public and states the count it is given; only exactly 1
    // reads as singular.
    match census(Vec::new()).unanswered("nothing was asked") {
        CliError::IpcUnavailable { detail } => assert_eq!(
            detail,
            "nothing was asked (0 running sessions did not answer)"
        ),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn fetching_all_from_an_empty_runtime_dir_answers_no_sessions() {
    let dir = test_runtime_dir("empty");
    let found = fetch_all(&dir);
    assert_eq!(found.sessions, Vec::new());
    assert_eq!(found.unasked, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_endpoint_nobody_listens_behind_is_swept() {
    let dir = test_runtime_dir("stale");
    let session_id = SessionId::new();
    let socket = koshi_ipc::endpoint::socket_addr(&dir, session_id);
    let endpoint_path = advertise(&dir, session_id, socket.clone());

    let found = fetch_all(&dir);
    assert_eq!(found.sessions, Vec::new());
    assert_eq!(found.unasked, 0, "a session that is gone is not unasked");
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

    let found = fetch_all(&dir);
    assert_eq!(found.sessions, Vec::new());
    assert_eq!(found.unasked, 1, "a session that is listening is unasked");
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
fn a_live_session_is_listed_while_a_stale_endpoint_beside_it_is_swept() {
    // One sweep over a directory holding both kinds of endpoint: the answer
    // carries the live session, and the endpoint of the one that is gone goes.
    let dir = test_runtime_dir("live-and-stale");
    let quiet = overview("quiet-lake", &[("editor", 1)]);
    let quiet_id = quiet.session.id;
    let serving = serve_overview(&dir, quiet);
    let gone_id = SessionId::new();
    let stale_path = advertise(
        &dir,
        gone_id,
        koshi_ipc::endpoint::socket_addr(&dir, gone_id),
    );

    let found = fetch_all(&dir);
    serving.join().expect("the stand-in session finishes");

    assert_eq!(found.unasked, 0, "a session that is gone is not unasked");
    assert_eq!(
        found
            .sessions
            .iter()
            .map(|overview| overview.session.id)
            .collect::<Vec<_>>(),
        vec![quiet_id]
    );
    assert!(
        !stale_path.exists(),
        "the endpoint file of the session that is gone is removed"
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

    let found = fetch_all(&dir);
    first.join().expect("the first stand-in session finishes");
    second.join().expect("the second stand-in session finishes");
    assert!(found.is_complete(), "both sessions answered");

    // Sorted by session name, so `amber-fox` comes before `quiet-lake`
    // whatever order the runtime directory listed the endpoint files in.
    assert_eq!(
        session_rows(&found.sessions),
        vec![
            SessionRow {
                id: amber_id,
                name: "amber-fox".to_string(),
                server: None,
            },
            SessionRow {
                id: quiet_id,
                name: "quiet-lake".to_string(),
                server: None,
            },
        ]
    );
    let panes = pane_rows(&found.sessions);
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

// --- Hiding pane command arguments ------------------------------------------

#[test]
fn redacting_pane_commands_keeps_each_program_and_hides_its_arguments() {
    let mut overviews = vec![overview("quiet-lake", &[("editor", 1)])];
    overviews[0].panes[0].command = Some(vec![
        "mysql".to_string(),
        "-pHUNTER2".to_string(),
        "--host=db.internal".to_string(),
    ]);

    redact_pane_commands(&mut overviews);

    assert_eq!(
        overviews[0].panes[0].command,
        Some(vec![
            "mysql".to_string(),
            "***".to_string(),
            "***".to_string(),
        ]),
    );
}

#[test]
fn redacting_pane_commands_leaves_a_pane_with_no_command_absent() {
    let mut overviews = vec![overview("quiet-lake", &[("editor", 1)])];
    assert_eq!(overviews[0].panes[0].command, None, "the fixture has none");

    redact_pane_commands(&mut overviews);

    assert_eq!(overviews[0].panes[0].command, None);
}

#[test]
fn redacting_pane_commands_reaches_every_pane_of_every_session() {
    let mut overviews = vec![
        overview("quiet-lake", &[("editor", 2)]),
        overview("amber-fox", &[("shell", 1)]),
    ];
    overviews[0].panes[0].command = Some(vec!["vim".to_string(), "secret.txt".to_string()]);
    overviews[0].panes[1].command = Some(vec!["psql".to_string(), "postgres://u:p@db".to_string()]);
    overviews[1].panes[0].command = Some(vec!["ssh".to_string(), "root@10.0.0.1".to_string()]);

    redact_pane_commands(&mut overviews);

    assert_eq!(
        overviews[0].panes[0].command,
        Some(vec!["vim".to_string(), "***".to_string()]),
    );
    assert_eq!(
        overviews[0].panes[1].command,
        Some(vec!["psql".to_string(), "***".to_string()]),
    );
    assert_eq!(
        overviews[1].panes[0].command,
        Some(vec!["ssh".to_string(), "***".to_string()]),
    );
}

#[test]
fn redacting_pane_commands_changes_nothing_but_the_command() {
    let mut overviews = vec![overview("quiet-lake", &[("editor", 1)])];
    overviews[0].panes[0].cwd = Some(PathBuf::from("/home/user"));
    overviews[0].panes[0].command = Some(vec!["vim".to_string(), "secret.txt".to_string()]);
    let before = overviews[0].clone();

    redact_pane_commands(&mut overviews);

    let after = &overviews[0];
    assert_eq!(after.session, before.session);
    assert_eq!(after.tabs, before.tabs);
    assert_eq!(after.clients, before.clients);
    assert_eq!(after.panes[0].id, before.panes[0].id);
    assert_eq!(after.panes[0].tab_id, before.panes[0].tab_id);
    assert_eq!(after.panes[0].session_id, before.panes[0].session_id);
    assert_eq!(after.panes[0].title, before.panes[0].title);
    assert_eq!(after.panes[0].cwd, Some(PathBuf::from("/home/user")));
    assert_eq!(after.panes[0].state, before.panes[0].state);
    assert_eq!(
        after.panes[0].focused_by_clients,
        before.panes[0].focused_by_clients,
    );
}

#[test]
fn redacting_a_command_with_no_arguments_leaves_it_as_it_is() {
    let mut overviews = vec![overview("quiet-lake", &[("editor", 2)])];
    overviews[0].panes[0].command = Some(vec!["htop".to_string()]);
    overviews[0].panes[1].command = Some(Vec::new());

    redact_pane_commands(&mut overviews);

    assert_eq!(
        overviews[0].panes[0].command,
        Some(vec!["htop".to_string()])
    );
    assert_eq!(overviews[0].panes[1].command, Some(Vec::new()));
}

#[test]
fn redacting_pane_commands_across_no_sessions_is_a_noop() {
    let mut overviews: Vec<SessionOverview> = Vec::new();

    redact_pane_commands(&mut overviews);

    assert!(overviews.is_empty());
}

#[test]
fn display_rows_filter_names_while_the_overview_keeps_them_raw() {
    // A name is what targeting matches on, so the overview keeps exactly what
    // the peer sent. Only the rows built for printing are filtered.
    let mut raw = overview("web\u{7f}srv", &[("ta\u{202e}b", 1)]);
    raw.panes[0].title = Some("ti\u{7f}tle".to_string());

    assert_eq!(raw.session.name, "web\u{7f}srv");
    assert_eq!(raw.tabs[0].name, "ta\u{202e}b");

    let sessions = session_rows(std::slice::from_ref(&raw));
    assert_eq!(sessions[0].name, "websrv");

    let tabs = tab_rows(std::slice::from_ref(&raw));
    assert_eq!(tabs[0].name, "tab");
    assert_eq!(tabs[0].session_name, "websrv");

    let panes = pane_rows(std::slice::from_ref(&raw));
    assert_eq!(panes[0].name.as_deref(), Some("title"));
    assert_eq!(panes[0].tab_name, "tab");
    assert_eq!(panes[0].session_name, "websrv");
}

#[test]
fn a_display_row_name_is_bounded() {
    let raw = overview(&"a".repeat(100_000), &[("t", 1)]);
    assert_eq!(
        raw.session.name.len(),
        100_000,
        "the overview keeps it whole"
    );
    assert_eq!(
        session_rows(std::slice::from_ref(&raw))[0].name.len(),
        koshi_core::text::MAX_REPORTED_TEXT_BYTES
    );
}

#[test]
fn a_session_row_filters_its_name_however_it_is_built() {
    // Four sites build a `SessionRow`, and only the constructor filters, so a
    // name reaches a listing or a picker filtered whichever site made it.
    let id = SessionId::new();
    let row = SessionRow::new(id, "web\u{7f}s\u{202e}rv", Some("host-1".to_string()));
    assert_eq!(row.name, "websrv");
    assert_eq!(row.id, id, "the id is carried, never altered");
    assert_eq!(
        row.server.as_deref(),
        Some("host-1"),
        "the server is carried, never altered"
    );

    let long = SessionRow::new(id, &"a".repeat(100_000), None);
    assert_eq!(long.name.len(), koshi_core::text::MAX_REPORTED_TEXT_BYTES);
}

#[test]
fn a_session_row_name_of_nothing_but_filtered_characters_is_empty() {
    let id = SessionId::new();
    assert_eq!(SessionRow::new(id, "", None).name, "");
    assert_eq!(SessionRow::new(id, "\u{7f}\u{202e}\u{200e}", None).name, "");
}
