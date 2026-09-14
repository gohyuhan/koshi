//! Tests for the discovery answers: row building across sessions, `inspect`
//! lookups, and the endpoint sweep that removes what a session left behind.

use koshi_core::client::ClientOrigin;
use std::path::PathBuf;
use std::time::SystemTime;

use koshi_core::discovery::{
    ClientDiscovery, PaneDiscovery, PaneLifecycle, SessionDiscovery, SessionOverview, TabDiscovery,
};
use koshi_core::event::RejectReason;
use koshi_core::geometry::Size;
use koshi_core::lock::LockMode;
use koshi_ipc::protocol::{ConnectionToken, IpcRequest, IpcResponse, IpcResult};
use koshi_ipc::transport::{Connection, Listener};

use super::*;

/// A fresh directory to stand in for the runtime directory, under a short base so
/// the Unix socket path stays inside the OS path-length cap.
fn build_test_runtime_directory(test_tag: &str) -> PathBuf {
    #[cfg(unix)]
    let base_directory = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base_directory = std::env::temp_dir();
    let runtime_directory =
        base_directory.join(format!("koshi-discovery-{}-{test_tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime_directory);
    std::fs::create_dir_all(&runtime_directory).expect("create runtime directory");
    runtime_directory
}

/// A complete discovery where every running session answered.
fn build_complete_discovery(session_overviews: Vec<SessionOverview>) -> Discovered {
    Discovered {
        sessions: session_overviews,
        unasked_session_count: 0,
    }
}

/// A partial discovery missing `unasked` sessions: running and listening, but unable to
/// say what they hold.
fn build_partial_discovery(
    session_overviews: Vec<SessionOverview>,
    unasked_session_count: usize,
) -> Discovered {
    Discovered {
        sessions: session_overviews,
        unasked_session_count,
    }
}

/// Advertise `session_id` at `runtime_directory` with `socket_address` as its address.
fn write_endpoint_file(
    runtime_directory: &Path,
    session_id: SessionId,
    socket_address: String,
) -> PathBuf {
    let endpoint_file_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    EndpointFile {
        socket_address,
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&endpoint_file_path)
    .expect("endpoint file written");
    endpoint_file_path
}

/// A stand-in koshi serving one discovery exchange for `session_overview` over a
/// real socket at `runtime_directory`: answer the Hello, then hand back the
/// `session_overview`.
fn spawn_overview_server(
    runtime_directory: &Path,
    session_overview: SessionOverview,
) -> std::thread::JoinHandle<()> {
    let session_id = session_overview.session.session_id;
    let session_socket_address =
        koshi_ipc::endpoint::compute_socket_address(runtime_directory, session_id);
    let session_listener = Listener::bind(&session_socket_address).expect("stand-in session binds");
    write_endpoint_file(runtime_directory, session_id, session_socket_address);

    std::thread::spawn(move || {
        let mut session_connection = session_listener.accept().expect("accept the CLI");
        let hello_request: IpcRequest = session_connection.recv().expect("read hello");
        let discovery_request: IpcRequest =
            session_connection.recv().expect("read discovery request");
        send_ipc_response(
            &mut session_connection,
            hello_request.request_id,
            IpcResult::Hello {
                protocol_version: koshi_ipc::protocol::PROTOCOL_VERSION,
                build_version: env!("CARGO_PKG_VERSION").to_string(),
            },
        );
        send_ipc_response(
            &mut session_connection,
            discovery_request.request_id,
            IpcResult::Overview(session_overview),
        );
    })
}

/// Answer `request_id` with `ipc_result` on `connection`.
fn send_ipc_response(connection: &mut Connection, request_id: u64, ipc_result: IpcResult) {
    connection
        .send(&IpcResponse {
            request_id: Some(request_id),
            answer_result: ipc_result,
        })
        .expect("send scripted reply");
}

/// A session overview with `tab_descriptions` (name, panes-per-tab), one client, and
/// pane titles derived from their position, so every row is identifiable.
fn build_session_overview(
    session_name: &str,
    tab_descriptions: &[(&str, usize)],
) -> SessionOverview {
    let session_id = SessionId::new();
    let mut tab_discoveries = Vec::new();
    let mut pane_discoveries = Vec::new();
    for (tab_index, (tab_name, pane_count)) in tab_descriptions.iter().enumerate() {
        let tab_id = TabId::new();
        tab_discoveries.push(TabDiscovery {
            tab_id,
            session_id,
            tab_name: (*tab_name).to_string(),
            tab_index,
            active_pane_id: None,
            pane_count: *pane_count,
        });
        for pane_index in 0..*pane_count {
            pane_discoveries.push(PaneDiscovery {
                pane_id: PaneId::new(),
                tab_id,
                session_id,
                pane_title: Some(format!("{tab_name}-{pane_index}")),
                working_directory: None,
                command_argv: None,
                lifecycle: PaneLifecycle::Running,
                focused_by_client_ids: Vec::new(),
            });
        }
    }
    SessionOverview {
        session: SessionDiscovery {
            session_id,
            session_name: session_name.to_string(),
            created_at: SystemTime::UNIX_EPOCH,
            attached_client_ids: Vec::new(),
            pane_count: pane_discoveries.len(),
        },
        tabs: tab_discoveries,
        panes: pane_discoveries,
        clients: vec![ClientDiscovery {
            client_id: ClientId::new(),
            session_id,
            attached_at: SystemTime::UNIX_EPOCH,
            viewport_size: Size {
                column_count: 80,
                row_count: 24,
            },
            active_tab_id: TabId::new(),
            focused_pane_id: None,
            lock_mode: LockMode::Normal,
            origin: Some(ClientOrigin::Local),
            pane_area: None,
        }],
    }
}

#[test]
fn session_rows_are_one_row_per_session() {
    let overviews = vec![
        build_session_overview("quiet-lake", &[]),
        build_session_overview("amber-fox", &[]),
    ];
    let session_rows = build_session_rows(&overviews);
    assert_eq!(
        session_rows,
        vec![
            SessionRow {
                session_id: overviews[0].session.session_id,
                session_name: "quiet-lake".to_string(),
                server_name_or_address: None,
            },
            SessionRow {
                session_id: overviews[1].session.session_id,
                session_name: "amber-fox".to_string(),
                server_name_or_address: None,
            },
        ]
    );
}

#[test]
fn tab_rows_span_every_session_in_bar_order() {
    let overviews = vec![
        build_session_overview("quiet-lake", &[("editor", 1), ("logs", 1)]),
        build_session_overview("amber-fox", &[("shell", 1)]),
    ];
    let tab_rows = build_tab_rows(&overviews);
    assert_eq!(
        tab_rows,
        vec![
            TabRow {
                tab_id: overviews[0].tabs[0].tab_id,
                tab_name: "editor".to_string(),
                session_id: overviews[0].session.session_id,
                session_name: "quiet-lake".to_string(),
            },
            TabRow {
                tab_id: overviews[0].tabs[1].tab_id,
                tab_name: "logs".to_string(),
                session_id: overviews[0].session.session_id,
                session_name: "quiet-lake".to_string(),
            },
            TabRow {
                tab_id: overviews[1].tabs[0].tab_id,
                tab_name: "shell".to_string(),
                session_id: overviews[1].session.session_id,
                session_name: "amber-fox".to_string(),
            },
        ]
    );
}

#[test]
fn pane_rows_carry_the_tab_and_session_they_belong_to() {
    let overviews = vec![
        build_session_overview("quiet-lake", &[("editor", 2)]),
        build_session_overview("amber-fox", &[("shell", 1)]),
    ];
    let pane_rows = build_pane_rows(&overviews);
    assert_eq!(
        pane_rows,
        vec![
            PaneRow {
                pane_id: overviews[0].panes[0].pane_id,
                pane_name: Some("editor-0".to_string()),
                tab_id: overviews[0].tabs[0].tab_id,
                tab_name: "editor".to_string(),
                session_id: overviews[0].session.session_id,
                session_name: "quiet-lake".to_string(),
            },
            PaneRow {
                pane_id: overviews[0].panes[1].pane_id,
                pane_name: Some("editor-1".to_string()),
                tab_id: overviews[0].tabs[0].tab_id,
                tab_name: "editor".to_string(),
                session_id: overviews[0].session.session_id,
                session_name: "quiet-lake".to_string(),
            },
            PaneRow {
                pane_id: overviews[1].panes[0].pane_id,
                pane_name: Some("shell-0".to_string()),
                tab_id: overviews[1].tabs[0].tab_id,
                tab_name: "shell".to_string(),
                session_id: overviews[1].session.session_id,
                session_name: "amber-fox".to_string(),
            },
        ]
    );
}

#[test]
fn a_pane_whose_tab_is_not_listed_produces_no_row() {
    let mut overviews = vec![build_session_overview("quiet-lake", &[("editor", 1)])];
    overviews[0].tabs.clear();
    assert_eq!(build_pane_rows(&overviews), Vec::new());
}

#[test]
fn client_rows_name_the_session_they_are_attached_to() {
    let overviews = vec![build_session_overview("quiet-lake", &[("editor", 1)])];
    assert_eq!(
        build_client_rows(&overviews),
        vec![ClientRow {
            client_id: overviews[0].clients[0].client_id,
            session_id: overviews[0].session.session_id,
            session_name: "quiet-lake".to_string(),
        }]
    );
}

#[test]
fn client_rows_are_one_row_per_attached_client_across_sessions() {
    let mut overviews = vec![
        build_session_overview("quiet-lake", &[("editor", 1)]),
        build_session_overview("amber-fox", &[("shell", 1)]),
    ];
    let second_client = ClientDiscovery {
        client_id: ClientId::new(),
        ..overviews[0].clients[0].clone()
    };
    overviews[0].clients.push(second_client.clone());

    assert_eq!(
        build_client_rows(&overviews),
        vec![
            ClientRow {
                client_id: overviews[0].clients[0].client_id,
                session_id: overviews[0].session.session_id,
                session_name: "quiet-lake".to_string(),
            },
            ClientRow {
                client_id: second_client.client_id,
                session_id: overviews[0].session.session_id,
                session_name: "quiet-lake".to_string(),
            },
            ClientRow {
                client_id: overviews[1].clients[0].client_id,
                session_id: overviews[1].session.session_id,
                session_name: "amber-fox".to_string(),
            },
        ]
    );
}

#[test]
fn a_session_with_no_client_attached_contributes_no_client_row() {
    let mut overviews = vec![build_session_overview("quiet-lake", &[("editor", 1)])];
    overviews[0].clients.clear();
    assert_eq!(build_client_rows(&overviews), Vec::new());
}

#[test]
fn every_listing_over_no_sessions_is_empty() {
    let empty_session_overviews: [SessionOverview; 0] = [];
    assert_eq!(build_session_rows(&empty_session_overviews), Vec::new());
    assert_eq!(build_tab_rows(&empty_session_overviews), Vec::new());
    assert_eq!(build_pane_rows(&empty_session_overviews), Vec::new());
    assert_eq!(build_client_rows(&empty_session_overviews), Vec::new());
}

#[test]
fn a_tab_holding_no_panes_is_listed_and_contributes_no_pane_row() {
    let overviews = vec![build_session_overview("quiet-lake", &[("empty", 0)])];
    assert_eq!(
        build_tab_rows(&overviews),
        vec![TabRow {
            tab_id: overviews[0].tabs[0].tab_id,
            tab_name: "empty".to_string(),
            session_id: overviews[0].session.session_id,
            session_name: "quiet-lake".to_string(),
        }]
    );
    assert_eq!(build_pane_rows(&overviews), Vec::new());
}

#[test]
fn a_pane_whose_child_set_no_title_yields_a_row_with_no_name() {
    let mut overviews = vec![build_session_overview("quiet-lake", &[("editor", 1)])];
    overviews[0].panes[0].pane_title = None;
    assert_eq!(
        build_pane_rows(&overviews),
        vec![PaneRow {
            pane_id: overviews[0].panes[0].pane_id,
            pane_name: None,
            tab_id: overviews[0].tabs[0].tab_id,
            tab_name: "editor".to_string(),
            session_id: overviews[0].session.session_id,
            session_name: "quiet-lake".to_string(),
        }]
    );
}

#[test]
fn sorting_a_census_orders_by_name_then_id() {
    let zulu = build_session_overview("zulu", &[]);
    let first_alpha = build_session_overview("alpha", &[]);
    let second_alpha = build_session_overview("alpha", &[]);
    let mut alpha_ids = [
        first_alpha.session.session_id,
        second_alpha.session.session_id,
    ];
    alpha_ids.sort();

    let mut discovery = build_complete_discovery(vec![zulu.clone(), first_alpha, second_alpha]);
    discovery.sort_sessions();

    assert_eq!(
        discovery
            .sessions
            .iter()
            .map(|session_overview| session_overview.session.session_id)
            .collect::<Vec<_>>(),
        vec![alpha_ids[0], alpha_ids[1], zulu.session.session_id]
    );
}

#[test]
fn inspect_finds_an_entity_in_the_second_session() {
    let discovery = build_complete_discovery(vec![
        build_session_overview("quiet-lake", &[("editor", 1)]),
        build_session_overview("amber-fox", &[("shell", 1)]),
    ]);
    let second_session_overview = &discovery.sessions[1];
    assert_eq!(
        find_pane(&discovery, second_session_overview.panes[0].pane_id,).expect("pane found"),
        second_session_overview.panes[0]
    );
    assert_eq!(
        find_tab(&discovery, second_session_overview.tabs[0].tab_id,).expect("tab found"),
        second_session_overview.tabs[0]
    );
    assert_eq!(
        find_client(&discovery, second_session_overview.clients[0].client_id,)
            .expect("client found"),
        second_session_overview.clients[0]
    );
}

#[test]
fn inspecting_an_unknown_pane_reports_the_target_as_not_found() {
    let discovery =
        build_complete_discovery(vec![build_session_overview("quiet-lake", &[("editor", 1)])]);
    let missing_pane_id = PaneId::new();
    let discovery_error = find_pane(&discovery, missing_pane_id).expect_err("no such pane");
    match discovery_error {
        CliError::CommandRejected { reason, help } => {
            assert_eq!(reason, RejectReason::TargetNotFound);
            assert_eq!(
                help,
                Some(format!("no running session has pane {missing_pane_id}"))
            );
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn inspecting_an_unknown_tab_or_client_reports_the_target_as_not_found() {
    let discovery =
        build_complete_discovery(vec![build_session_overview("quiet-lake", &[("editor", 1)])]);

    let tab_id = TabId::new();
    match find_tab(&discovery, tab_id).expect_err("no such tab") {
        CliError::CommandRejected { reason, help } => {
            assert_eq!(reason, RejectReason::TargetNotFound);
            assert_eq!(help, Some(format!("no running session has tab {tab_id}")));
        }
        other => panic!("unexpected error: {other}"),
    }

    let client_id = ClientId::new();
    match find_client(&discovery, client_id).expect_err("no such client") {
        CliError::CommandRejected { reason, help } => {
            assert_eq!(reason, RejectReason::TargetNotFound);
            assert_eq!(
                help,
                Some(format!("no running session has client {client_id}"))
            );
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn two_sessions_unasked_are_counted_in_the_plural() {
    let discovery = build_partial_discovery(
        vec![build_session_overview("quiet-lake", &[("editor", 1)])],
        2,
    );
    let tab_id = TabId::new();

    match find_tab(&discovery, tab_id).expect_err("the discovery is incomplete") {
        CliError::IpcUnavailable { detail } => assert_eq!(
            detail,
            format!(
                "tab {tab_id} is in none of the sessions that answered \
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
    let discovery = build_partial_discovery(
        vec![build_session_overview("quiet-lake", &[("editor", 1)])],
        1,
    );
    let missing_pane_id = PaneId::new();
    let discovery_error =
        find_pane(&discovery, missing_pane_id).expect_err("the discovery is incomplete");
    match discovery_error {
        CliError::IpcUnavailable { detail } => assert_eq!(
            detail,
            format!(
                "pane {missing_pane_id} is in none of the sessions that answered \
                 (1 running session did not answer)"
            )
        ),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn a_session_no_answering_session_matched_is_reported_as_not_running() {
    let discovery =
        build_complete_discovery(vec![build_session_overview("quiet-lake", &[("editor", 1)])]);

    match discovery.build_missing_session_error("amber-fox") {
        CliError::SessionNotFound { session_name } => assert_eq!(session_name, "amber-fox"),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn a_session_missed_while_one_went_unasked_reports_the_gap_not_a_miss() {
    // The name may well belong to the session that stayed silent, so "not
    // running" would be a guess.
    let discovery = build_partial_discovery(
        vec![build_session_overview("quiet-lake", &[("editor", 1)])],
        1,
    );

    match discovery.build_missing_session_error("amber-fox") {
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
    let only_session_overview = build_session_overview("quiet-lake", &[("editor", 1)]);

    let discovery = Discovered::from_overview(only_session_overview.clone());

    assert_eq!(discovery.unasked_session_count, 0);
    assert!(discovery.is_complete());
    assert_eq!(discovery.sessions, vec![only_session_overview]);
}

#[test]
fn a_complete_listing_reports_no_gap() {
    assert!(
        build_complete_discovery(vec![build_session_overview("quiet-lake", &[("editor", 1)])])
            .incomplete_listing()
            .is_none()
    );
}

#[test]
fn a_listing_missing_a_session_reports_the_gap() {
    // The rows still print; the exit code is what says they are not all of
    // them, since a script reads stdout and the exit code, not stderr.
    let discovery = build_partial_discovery(
        vec![build_session_overview("quiet-lake", &[("editor", 1)])],
        2,
    );
    match discovery
        .incomplete_listing()
        .expect("the discovery is incomplete")
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
    // `build_unanswered_error` is public and states the count it is given; only exactly 1
    // reads as singular.
    match build_complete_discovery(Vec::new()).build_unanswered_error("nothing was asked") {
        CliError::IpcUnavailable { detail } => assert_eq!(
            detail,
            "nothing was asked (0 running sessions did not answer)"
        ),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn fetching_all_from_an_empty_runtime_directory_answers_no_sessions() {
    let runtime_directory = build_test_runtime_directory("empty");
    let discovery = fetch_all_session_overviews(&runtime_directory);
    assert_eq!(discovery.sessions, Vec::new());
    assert_eq!(discovery.unasked_session_count, 0);
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn an_endpoint_nobody_listens_behind_is_swept() {
    let runtime_directory = build_test_runtime_directory("stale");
    let session_id = SessionId::new();
    let socket_address =
        koshi_ipc::endpoint::compute_socket_address(&runtime_directory, session_id);
    let endpoint_path = write_endpoint_file(&runtime_directory, session_id, socket_address.clone());

    let discovery = fetch_all_session_overviews(&runtime_directory);
    assert_eq!(discovery.sessions, Vec::new());
    assert_eq!(
        discovery.unasked_session_count, 0,
        "a session that is gone is not unasked"
    );
    assert!(
        !endpoint_path.exists(),
        "the endpoint file of a session that is gone is removed"
    );
    #[cfg(unix)]
    assert!(
        !Path::new(&socket_address).exists(),
        "the socket file of a session that is gone is removed"
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_listening_endpoint_survives_a_failed_exchange() {
    // A session that accepts the connection and then hangs up: something IS
    // serving there, so the endpoint stays even though the exchange gets no
    // answer and the session contributes no rows.
    let runtime_directory = build_test_runtime_directory("live-but-mute");
    let session_id = SessionId::new();
    let socket_address =
        koshi_ipc::endpoint::compute_socket_address(&runtime_directory, session_id);
    let listener = Listener::bind(&socket_address).expect("listener binds");
    let endpoint_path = write_endpoint_file(&runtime_directory, session_id, socket_address.clone());
    let serving = std::thread::spawn(move || {
        // Accepting and dropping closes the connection mid-exchange.
        let _ = listener.accept();
    });

    let discovery = fetch_all_session_overviews(&runtime_directory);
    assert_eq!(discovery.sessions, Vec::new());
    assert_eq!(
        discovery.unasked_session_count, 1,
        "a session that is listening is unasked"
    );
    serving
        .join()
        .expect("the stand-in session thread finishes");
    assert!(
        endpoint_path.exists(),
        "an endpoint something listens behind is kept"
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_live_session_is_listed_while_a_stale_endpoint_beside_it_is_swept() {
    // One sweep over a directory holding both kinds of endpoint: the answer
    // carries the live session, and the endpoint of the one that is gone goes.
    let runtime_directory = build_test_runtime_directory("live-and-stale");
    let quiet_session_overview = build_session_overview("quiet-lake", &[("editor", 1)]);
    let quiet_session_id = quiet_session_overview.session.session_id;
    let serving = spawn_overview_server(&runtime_directory, quiet_session_overview);
    let gone_session_id = SessionId::new();
    let stale_path = write_endpoint_file(
        &runtime_directory,
        gone_session_id,
        koshi_ipc::endpoint::compute_socket_address(&runtime_directory, gone_session_id),
    );

    let discovery = fetch_all_session_overviews(&runtime_directory);
    serving.join().expect("the stand-in session finishes");

    assert_eq!(
        discovery.unasked_session_count, 0,
        "a session that is gone is not unasked"
    );
    assert_eq!(
        discovery
            .sessions
            .iter()
            .map(|session_overview| session_overview.session.session_id)
            .collect::<Vec<_>>(),
        vec![quiet_session_id]
    );
    assert!(
        !stale_path.exists(),
        "the endpoint file of the session that is gone is removed"
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn two_running_sessions_merge_into_one_listing() {
    // The acceptance bar: two koshi processes, each serving its own socket,
    // and one `list-panes` showing both sessions' panes.
    let runtime_directory = build_test_runtime_directory("two-sessions");
    let quiet_session_overview = build_session_overview("quiet-lake", &[("editor", 2)]);
    let amber_session_overview = build_session_overview("amber-fox", &[("shell", 1)]);
    let quiet_session_id = quiet_session_overview.session.session_id;
    let amber_session_id = amber_session_overview.session.session_id;
    let first_server = spawn_overview_server(&runtime_directory, quiet_session_overview);
    let second_server = spawn_overview_server(&runtime_directory, amber_session_overview);

    let discovery = fetch_all_session_overviews(&runtime_directory);
    first_server
        .join()
        .expect("the first stand-in session finishes");
    second_server
        .join()
        .expect("the second stand-in session finishes");
    assert!(discovery.is_complete(), "both sessions answered");

    // Sorted by session name, so `amber-fox` comes before `quiet-lake`
    // whatever order the runtime directory listed the endpoint files in.
    assert_eq!(
        build_session_rows(&discovery.sessions),
        vec![
            SessionRow {
                session_id: amber_session_id,
                session_name: "amber-fox".to_string(),
                server_name_or_address: None,
            },
            SessionRow {
                session_id: quiet_session_id,
                session_name: "quiet-lake".to_string(),
                server_name_or_address: None,
            },
        ]
    );
    let pane_rows = build_pane_rows(&discovery.sessions);
    assert_eq!(
        pane_rows
            .iter()
            .map(|pane| (pane.session_id, pane.pane_name.clone()))
            .collect::<Vec<_>>(),
        vec![
            (amber_session_id, Some("shell-0".to_string())),
            (quiet_session_id, Some("editor-0".to_string())),
            (quiet_session_id, Some("editor-1".to_string())),
        ]
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn one_session_can_be_fetched_on_its_own() {
    let runtime_directory = build_test_runtime_directory("one-session");
    let quiet_session_overview = build_session_overview("quiet-lake", &[("editor", 1)]);
    let quiet_session_id = quiet_session_overview.session.session_id;
    let serving = spawn_overview_server(&runtime_directory, quiet_session_overview);

    let fetched_session_overview =
        fetch_session_overview(&runtime_directory, quiet_session_id).expect("the session answers");
    serving.join().expect("the stand-in session finishes");
    assert_eq!(
        fetched_session_overview.session.session_id,
        quiet_session_id
    );
    assert_eq!(fetched_session_overview.session.session_name, "quiet-lake");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn fetching_one_live_session_that_cannot_answer_is_not_reported_as_gone() {
    // The endpoint accepts and hangs up: the session IS running, so the
    // failure must stay a transport failure rather than "not running".
    let runtime_directory = build_test_runtime_directory("one-live-but-mute");
    let session_id = SessionId::new();
    let socket_address =
        koshi_ipc::endpoint::compute_socket_address(&runtime_directory, session_id);
    let listener = Listener::bind(&socket_address).expect("listener binds");
    let endpoint_path = write_endpoint_file(&runtime_directory, session_id, socket_address);
    let serving = std::thread::spawn(move || {
        let _ = listener.accept();
    });

    let discovery_error = fetch_session_overview(&runtime_directory, session_id)
        .expect_err("the exchange cannot finish");
    serving
        .join()
        .expect("the stand-in session thread finishes");
    let CliError::IpcUnavailable { detail } = discovery_error else {
        panic!("unexpected error: {discovery_error}");
    };
    assert_eq!(detail, "ipc peer disconnected");
    assert!(
        endpoint_path.exists(),
        "an endpoint something listens behind is kept"
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn fetching_one_session_that_is_gone_reports_it_as_not_running() {
    let runtime_directory = build_test_runtime_directory("one-missing");
    let session_id = SessionId::new();
    let discovery_error =
        fetch_session_overview(&runtime_directory, session_id).expect_err("nothing advertises it");
    match discovery_error {
        CliError::SessionNotFound { session_name } => {
            assert_eq!(session_name, session_id.to_string());
        }
        other => panic!("unexpected error: {other}"),
    }
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

// --- Hiding pane command arguments ------------------------------------------

#[test]
fn redacting_pane_commands_keeps_each_program_and_hides_its_arguments() {
    let mut overviews = vec![build_session_overview("quiet-lake", &[("editor", 1)])];
    overviews[0].panes[0].command_argv = Some(vec![
        "mysql".to_string(),
        "-pHUNTER2".to_string(),
        "--host=db.internal".to_string(),
    ]);

    redact_pane_commands(&mut overviews);

    assert_eq!(
        overviews[0].panes[0].command_argv,
        Some(vec![
            "mysql".to_string(),
            "***".to_string(),
            "***".to_string(),
        ]),
    );
}

#[test]
fn redacting_pane_commands_leaves_a_pane_with_no_command_absent() {
    let mut overviews = vec![build_session_overview("quiet-lake", &[("editor", 1)])];
    assert_eq!(
        overviews[0].panes[0].command_argv, None,
        "the fixture has none"
    );

    redact_pane_commands(&mut overviews);

    assert_eq!(overviews[0].panes[0].command_argv, None);
}

#[test]
fn redacting_pane_commands_reaches_every_pane_of_every_session() {
    let mut overviews = vec![
        build_session_overview("quiet-lake", &[("editor", 2)]),
        build_session_overview("amber-fox", &[("shell", 1)]),
    ];
    overviews[0].panes[0].command_argv = Some(vec!["vim".to_string(), "secret.txt".to_string()]);
    overviews[0].panes[1].command_argv =
        Some(vec!["psql".to_string(), "postgres://u:p@db".to_string()]);
    overviews[1].panes[0].command_argv = Some(vec!["ssh".to_string(), "root@10.0.0.1".to_string()]);

    redact_pane_commands(&mut overviews);

    assert_eq!(
        overviews[0].panes[0].command_argv,
        Some(vec!["vim".to_string(), "***".to_string()]),
    );
    assert_eq!(
        overviews[0].panes[1].command_argv,
        Some(vec!["psql".to_string(), "***".to_string()]),
    );
    assert_eq!(
        overviews[1].panes[0].command_argv,
        Some(vec!["ssh".to_string(), "***".to_string()]),
    );
}

#[test]
fn redacting_pane_commands_changes_nothing_but_the_command() {
    let mut overviews = vec![build_session_overview("quiet-lake", &[("editor", 1)])];
    overviews[0].panes[0].working_directory = Some(PathBuf::from("/home/user"));
    overviews[0].panes[0].command_argv = Some(vec!["vim".to_string(), "secret.txt".to_string()]);
    let original_overview = overviews[0].clone();

    redact_pane_commands(&mut overviews);

    let redacted_overview = &overviews[0];
    assert_eq!(redacted_overview.session, original_overview.session);
    assert_eq!(redacted_overview.tabs, original_overview.tabs);
    assert_eq!(redacted_overview.clients, original_overview.clients);
    assert_eq!(
        redacted_overview.panes[0].pane_id,
        original_overview.panes[0].pane_id
    );
    assert_eq!(
        redacted_overview.panes[0].tab_id,
        original_overview.panes[0].tab_id
    );
    assert_eq!(
        redacted_overview.panes[0].session_id,
        original_overview.panes[0].session_id
    );
    assert_eq!(
        redacted_overview.panes[0].pane_title,
        original_overview.panes[0].pane_title
    );
    assert_eq!(
        redacted_overview.panes[0].working_directory,
        Some(PathBuf::from("/home/user"))
    );
    assert_eq!(
        redacted_overview.panes[0].lifecycle,
        original_overview.panes[0].lifecycle
    );
    assert_eq!(
        redacted_overview.panes[0].focused_by_client_ids,
        original_overview.panes[0].focused_by_client_ids,
    );
}

#[test]
fn redacting_a_command_with_no_arguments_leaves_it_as_it_is() {
    let mut overviews = vec![build_session_overview("quiet-lake", &[("editor", 2)])];
    overviews[0].panes[0].command_argv = Some(vec!["htop".to_string()]);
    overviews[0].panes[1].command_argv = Some(Vec::new());

    redact_pane_commands(&mut overviews);

    assert_eq!(
        overviews[0].panes[0].command_argv,
        Some(vec!["htop".to_string()])
    );
    assert_eq!(overviews[0].panes[1].command_argv, Some(Vec::new()));
}

#[test]
fn redacting_pane_commands_across_no_sessions_is_a_noop() {
    let mut overviews: Vec<SessionOverview> = Vec::new();

    redact_pane_commands(&mut overviews);

    assert!(overviews.is_empty());
}

#[test]
fn display_rows_filter_names_while_the_overview_keeps_them_raw() {
    // A name is what targeting matches on, so the session_overview keeps exactly what
    // the peer sent. Only the rows built for printing are filtered.
    let mut unfiltered_session_overview =
        build_session_overview("web\u{7f}srv", &[("ta\u{202e}b", 1)]);
    unfiltered_session_overview.panes[0].pane_title = Some("ti\u{7f}tle".to_string());

    assert_eq!(
        unfiltered_session_overview.session.session_name,
        "web\u{7f}srv"
    );
    assert_eq!(unfiltered_session_overview.tabs[0].tab_name, "ta\u{202e}b");

    let sessions = build_session_rows(std::slice::from_ref(&unfiltered_session_overview));
    assert_eq!(sessions[0].session_name, "websrv");

    let tabs = build_tab_rows(std::slice::from_ref(&unfiltered_session_overview));
    assert_eq!(tabs[0].tab_name, "tab");
    assert_eq!(tabs[0].session_name, "websrv");

    let pane_rows = build_pane_rows(std::slice::from_ref(&unfiltered_session_overview));
    assert_eq!(pane_rows[0].pane_name.as_deref(), Some("title"));
    assert_eq!(pane_rows[0].tab_name, "tab");
    assert_eq!(pane_rows[0].session_name, "websrv");

    let clients = build_client_rows(std::slice::from_ref(&unfiltered_session_overview));
    assert_eq!(clients[0].session_name, "websrv");
}

#[test]
fn a_display_row_name_is_bounded() {
    let unfiltered_session_overview = build_session_overview(&"a".repeat(100_000), &[("t", 1)]);
    assert_eq!(
        unfiltered_session_overview.session.session_name.len(),
        100_000,
        "the session overview keeps it whole"
    );
    assert_eq!(
        build_session_rows(std::slice::from_ref(&unfiltered_session_overview))[0]
            .session_name
            .len(),
        koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT
    );
    assert_eq!(
        build_client_rows(std::slice::from_ref(&unfiltered_session_overview))[0]
            .session_name
            .len(),
        koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT
    );
}

#[test]
fn a_session_row_filters_its_name_however_it_is_built() {
    // Four sites build a `SessionRow`, and only the constructor filters, so a
    // name reaches a listing or a picker filtered whichever site made it.
    let session_id = SessionId::new();
    let session_row = SessionRow::from_session(
        session_id,
        "web\u{7f}s\u{202e}rv",
        Some("host-1".to_string()),
    );
    assert_eq!(session_row.session_name, "websrv");
    assert_eq!(
        session_row.session_id, session_id,
        "the session id is carried, never altered"
    );
    assert_eq!(
        session_row.server_name_or_address.as_deref(),
        Some("host-1"),
        "the server is carried, never altered"
    );

    let long_session_row = SessionRow::from_session(session_id, &"a".repeat(100_000), None);
    assert_eq!(
        long_session_row.session_name.len(),
        koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT
    );
}

#[test]
fn a_session_row_name_of_nothing_but_filtered_characters_is_empty() {
    let session_id = SessionId::new();
    assert_eq!(
        SessionRow::from_session(session_id, "", None).session_name,
        ""
    );
    assert_eq!(
        SessionRow::from_session(session_id, "\u{7f}\u{202e}\u{200e}", None).session_name,
        ""
    );
}

#[test]
fn filter_reported_text_filters_every_string_the_answering_session_chose() {
    let mut answered_session_overview =
        build_session_overview("quiet\u{1b}lake", &[("edit\u{7f}or", 1)]);
    answered_session_overview.panes[0].pane_title = Some("ssh \u{202e}gpj.exe".to_string());
    answered_session_overview.panes[0].working_directory = Some(PathBuf::from("/tmp/a\u{1b}[2Jb"));
    answered_session_overview.panes[0].command_argv = Some(vec![
        "sh".to_string(),
        "-c".to_string(),
        "\u{1b}]0;pwned\u{7}".to_string(),
    ]);
    let pane_ids: Vec<PaneId> = answered_session_overview
        .panes
        .iter()
        .map(|pane| pane.pane_id)
        .collect();

    filter_session_overview_text(&mut answered_session_overview);

    assert_eq!(answered_session_overview.session.session_name, "quietlake");
    assert_eq!(answered_session_overview.tabs[0].tab_name, "editor");
    assert_eq!(
        answered_session_overview.panes[0].pane_title.as_deref(),
        Some("ssh gpj.exe")
    );
    assert_eq!(
        answered_session_overview.panes[0].working_directory,
        Some(PathBuf::from("/tmp/a[2Jb"))
    );
    assert_eq!(
        answered_session_overview.panes[0].command_argv.as_deref(),
        Some(["sh".to_string(), "-c".to_string(), "]0;pwned".to_string()].as_slice())
    );
    assert_eq!(
        answered_session_overview
            .panes
            .iter()
            .map(|pane| pane.pane_id)
            .collect::<Vec<_>>(),
        pane_ids,
        "ids are carried, never altered"
    );
}

#[test]
fn a_session_that_answers_with_escapes_is_filtered_before_the_caller_sees_it() {
    // The answering session is another process, so what it sends is not
    // trusted. `inspect` and `debug dump-state` render these fields straight,
    // and a filtered session_overview is what reaches them.
    let runtime_directory = build_test_runtime_directory("hostile-answer");
    let mut hostile_session_overview = build_session_overview("quiet-lake", &[("editor", 1)]);
    let session_id = hostile_session_overview.session.session_id;
    hostile_session_overview.session.session_name = "quiet\u{1b}[2Jlake".to_string();
    hostile_session_overview.tabs[0].tab_name = "edi\u{7f}tor".to_string();
    hostile_session_overview.panes[0].command_argv = Some(vec!["\u{1b}]0;pwned\u{7}".to_string()]);
    hostile_session_overview.panes[0].working_directory = Some(PathBuf::from("/tmp/\u{1b}[2J"));
    let serving = spawn_overview_server(&runtime_directory, hostile_session_overview);

    let fetched_session_overview =
        fetch_session_overview(&runtime_directory, session_id).expect("the session answers");
    serving.join().expect("the stand-in session finishes");

    assert_eq!(
        fetched_session_overview.session.session_name,
        "quiet[2Jlake"
    );
    assert_eq!(fetched_session_overview.tabs[0].tab_name, "editor");
    assert_eq!(
        fetched_session_overview.panes[0].command_argv.as_deref(),
        Some(["]0;pwned".to_string()].as_slice())
    );
    assert_eq!(
        fetched_session_overview.panes[0].working_directory,
        Some(PathBuf::from("/tmp/[2J"))
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
}
