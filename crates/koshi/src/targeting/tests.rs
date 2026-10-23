//! Routing tests: which running session an invocation targets, and how the
//! `--session`/`--tab` flags resolve — count rules, explicit targets, and
//! every refusal, checked against hand-built session overviews.

use koshi_core::client::ClientOrigin;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::SystemTime;

use koshi_core::discovery::{
    ClientDiscovery, PaneDiscovery, PaneLifecycle, SessionDiscovery, SessionOverview, TabDiscovery,
};
use koshi_core::event::RejectReason;
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::protocol::{ConnectionToken, IpcRequest, IpcRequestKind, IpcResult};
use koshi_ipc::transport::Listener;
use uuid::uuid;

use super::*;
use crate::cli::CliCommand;
use koshi_test_support::fixtures::{build_accepted_hello_result, send_ipc_reply};

/// One session overview with the given name and one tab/pane/client per
/// listed id, wired to each other in order.
fn build_session_overview(
    session_name: &str,
    session_id: SessionId,
    tab_records: &[(TabId, &str)],
    pane_records: &[(PaneId, TabId)],
    client_ids: &[ClientId],
) -> SessionOverview {
    SessionOverview {
        session: SessionDiscovery {
            session_id,
            session_name: session_name.to_string(),
            created_at: SystemTime::UNIX_EPOCH,
            attached_client_ids: client_ids.to_vec(),
            pane_count: pane_records.len(),
        },
        tabs: tab_records
            .iter()
            .enumerate()
            .map(|(tab_index, (tab_id, tab_name))| TabDiscovery {
                tab_id: *tab_id,
                session_id,
                tab_name: (*tab_name).to_string(),
                tab_index,
                active_pane_id: None,
                pane_count: pane_records
                    .iter()
                    .filter(|(_, pane_tab_id)| pane_tab_id == tab_id)
                    .count(),
            })
            .collect(),
        panes: pane_records
            .iter()
            .map(|(pane_id, tab_id)| PaneDiscovery {
                pane_id: *pane_id,
                tab_id: Some(*tab_id),
                session_id,
                pane_title: None,
                working_directory: None,
                command_argv: None,
                lifecycle: PaneLifecycle::Running,
                focused_by_client_ids: Vec::new(),
            })
            .collect(),
        clients: client_ids
            .iter()
            .map(|client_id| ClientDiscovery {
                client_id: *client_id,
                session_id,
                attached_at: SystemTime::UNIX_EPOCH,
                viewport_size: Size {
                    column_count: 80,
                    row_count: 24,
                },
                active_tab_id: tab_records[0].0,
                focused_pane_id: None,
                lock_mode: LockMode::Normal,
                origin: Some(ClientOrigin::Local),
                pane_area: None,
            })
            .collect(),
    }
}

/// A census where every running session answered — the normal case, and the
/// only one in which a "nowhere" or count-rule answer is trustworthy.
fn build_complete_discovery<const N: usize>(session_overviews: [SessionOverview; N]) -> Discovered {
    Discovered {
        sessions: session_overviews.to_vec(),
        unasked_session_count: 0,
        unread_path_count: 0,
        owner_user_id_by_session_id: std::collections::HashMap::new(),
    }
}

/// A census missing `unasked_session_count` sessions: they are running and listening, but
/// none of them could be asked what they hold.
fn build_incomplete_discovery<const N: usize>(
    session_overviews: [SessionOverview; N],
    unasked_session_count: usize,
) -> Discovered {
    Discovered {
        sessions: session_overviews.to_vec(),
        unasked_session_count,
        unread_path_count: 0,
        owner_user_id_by_session_id: std::collections::HashMap::new(),
    }
}

/// The rejection reason inside a `CommandRejected`, or a panic naming what
/// came back instead.
fn read_rejection_reason(selection_error: &CliError) -> RejectReason {
    match selection_error {
        CliError::CommandRejected { reason, .. } => *reason,
        unexpected_error => panic!("expected a rejection, got {unexpected_error:?}"),
    }
}

/// The hint inside a `CommandRejected`, or a panic naming what came back
/// instead.
fn read_rejection_help(selection_error: &CliError) -> String {
    match selection_error {
        CliError::CommandRejected {
            help: Some(help), ..
        } => help.clone(),
        unexpected_error => {
            panic!("expected a rejection carrying a hint, got {unexpected_error:?}")
        }
    }
}

/// A private runtime directory of this test's own, emptied first so a
/// leftover endpoint file from an earlier run is never read.
fn build_test_runtime_directory(test_label: &str) -> PathBuf {
    #[cfg(unix)]
    let runtime_base_directory = PathBuf::from("/tmp");
    #[cfg(windows)]
    let runtime_base_directory = std::env::temp_dir();
    let runtime_directory =
        runtime_base_directory.join(format!("koshi-scope-{}-{test_label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime_directory);
    std::fs::create_dir_all(&runtime_directory).expect("create runtime dir");
    runtime_directory
}

/// A stand-in session: it advertises an endpoint file in `runtime_directory` and
/// answers exactly one discovery exchange with `session_overview`.
fn serve_discovery(runtime_directory: &Path, session_overview: SessionOverview) -> JoinHandle<()> {
    let session_id = session_overview.session.session_id;
    let socket_address = koshi_ipc::endpoint::compute_socket_address(runtime_directory, session_id);
    let connection_token = ConnectionToken::generate();
    let listener = Listener::bind(&socket_address).expect("stand-in session binds");
    EndpointFile {
        socket_address,
        connection_token: connection_token.clone(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("endpoint file written");

    std::thread::spawn(move || {
        let mut discovery_connection = listener.accept().expect("accept discovery");
        let hello_request: IpcRequest = discovery_connection.recv().expect("read discovery hello");
        let discovery_request: IpcRequest =
            discovery_connection.recv().expect("read discovery request");
        assert_eq!(
            hello_request.request_kind,
            IpcRequestKind::build_hello_request(connection_token)
        );
        assert_eq!(discovery_request.request_kind, IpcRequestKind::Discovery);
        send_ipc_reply(
            &mut discovery_connection,
            hello_request.request_id,
            build_accepted_hello_result(),
        );
        send_ipc_reply(
            &mut discovery_connection,
            discovery_request.request_id,
            IpcResult::Overview(session_overview),
        );
    })
}

#[test]
fn sole_running_session_is_the_default() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let discovered_sessions = build_complete_discovery([build_session_overview(
        "amber-fox",
        session_id,
        &[(tab_id, "one")],
        &[],
        &[],
    )]);
    let selected_session_overview =
        select_target_session(None, None, None, None, &discovered_sessions).expect("sole session");
    assert_eq!(selected_session_overview.session.session_id, session_id);
}

#[test]
fn no_running_session_reports_no_sessions() {
    let selection_error =
        select_target_session(None, None, None, None, &build_complete_discovery([]))
            .expect_err("nothing to target");
    assert!(matches!(selection_error, CliError::NoSessions));
}

#[test]
fn a_sole_answering_session_is_not_the_default_while_another_is_unasked() {
    // One session answered, one is running but could not be asked. The
    // selection picks neither and refuses.
    let discovered_sessions = build_incomplete_discovery(
        [build_session_overview(
            "amber-fox",
            SessionId::new(),
            &[],
            &[],
            &[],
        )],
        1,
    );
    let selection_error = select_target_session(None, None, None, None, &discovered_sessions)
        .expect_err("census incomplete");
    assert!(
        matches!(&selection_error, CliError::IpcUnavailable { detail }
            if detail == "cannot tell which session to target; name one with \
                          --session <name-or-id> (1 running session did not answer)"),
        "got {selection_error:?}"
    );
}

#[test]
fn no_answer_at_all_is_not_reported_as_no_sessions() {
    // The only running session could not be asked. The error names the
    // session that did not answer.
    let selection_error =
        select_target_session(None, None, None, None, &build_incomplete_discovery([], 1))
            .expect_err("census empty");
    assert!(
        matches!(&selection_error, CliError::IpcUnavailable { detail }
            if detail == "cannot tell which session to target; name one with \
                          --session <name-or-id> (1 running session did not answer)"),
        "got {selection_error:?}"
    );
}

#[test]
fn an_unasked_session_does_not_turn_an_explicit_target_into_not_found() {
    let discovered_sessions = build_incomplete_discovery(
        [build_session_overview(
            "amber-fox",
            SessionId::new(),
            &[],
            &[],
            &[],
        )],
        1,
    );
    let pane_id = PaneId::new();
    let selection_error =
        select_target_session(None, Some(pane_id), None, None, &discovered_sessions)
            .expect_err("the pane may be in the session that stayed silent");
    assert!(
        matches!(&selection_error, CliError::IpcUnavailable { detail }
        if *detail == format!(
            "pane {pane_id} is in none of the sessions that answered \
             (1 running session did not answer)"
        )),
        "got {selection_error:?}"
    );

    let session_reference = SessionReference::SessionName("blue-owl".to_string());
    let selection_error = select_target_session(
        Some(&session_reference),
        None,
        None,
        None,
        &discovered_sessions,
    )
    .expect_err("the name may belong to the session that stayed silent");
    assert!(
        matches!(&selection_error, CliError::IpcUnavailable { detail }
            if detail == "`blue-owl` is not among the sessions that answered \
                          (1 running session did not answer)"),
        "got {selection_error:?}"
    );
}

#[test]
fn a_session_name_with_one_match_is_refused_while_a_session_is_unasked() {
    // The unasked session may carry the same name, so "exactly one is named
    // amber-fox" cannot be claimed — the same refusal kill-session gives.
    let discovered_sessions = build_incomplete_discovery(
        [build_session_overview(
            "amber-fox",
            SessionId::new(),
            &[],
            &[],
            &[],
        )],
        1,
    );
    let session_reference = SessionReference::SessionName("amber-fox".to_string());
    let selection_error = select_target_session(
        Some(&session_reference),
        None,
        None,
        None,
        &discovered_sessions,
    )
    .expect_err("the unasked session may share the name");
    assert!(
        matches!(&selection_error, CliError::IpcUnavailable { detail }
            if detail
                == "cannot tell whether `amber-fox` is unique \
                    (1 running session did not answer)"),
        "got {selection_error:?}"
    );
}

#[test]
fn a_tab_name_with_one_match_is_refused_while_a_session_is_unasked() {
    // One answering session holds tab `logs`, and one session could not be
    // asked. The tab name is refused.
    let tab_id = TabId::new();
    let discovered_sessions = build_incomplete_discovery(
        [build_session_overview(
            "amber-fox",
            SessionId::new(),
            &[(tab_id, "logs")],
            &[],
            &[],
        )],
        1,
    );
    let selection_error = select_session_for_tab(
        &TabReference::TabName("logs".to_string()),
        &discovered_sessions,
    )
    .expect_err("the unasked session may hold a tab of that name");
    assert!(
        matches!(&selection_error, CliError::IpcUnavailable { detail }
            if detail
                == "cannot tell whether tab `logs` is unique \
                    (1 running session did not answer)"),
        "got {selection_error:?}"
    );
}

#[test]
fn two_answering_sessions_stay_ambiguous_even_with_one_unasked() {
    // Two sessions answered and one could not be asked: the selection is
    // ambiguous.
    let discovered_sessions = build_incomplete_discovery(
        [
            build_session_overview("amber-fox", SessionId::new(), &[], &[], &[]),
            build_session_overview("blue-owl", SessionId::new(), &[], &[], &[]),
        ],
        1,
    );
    let selection_error =
        select_target_session(None, None, None, None, &discovered_sessions).expect_err("ambiguous");
    assert_eq!(
        read_rejection_reason(&selection_error),
        RejectReason::TargetAmbiguous
    );
}

#[test]
fn two_running_sessions_demand_the_session_flag() {
    let discovered_sessions = build_complete_discovery([
        build_session_overview("amber-fox", SessionId::new(), &[], &[], &[]),
        build_session_overview("blue-owl", SessionId::new(), &[], &[], &[]),
    ]);
    let selection_error =
        select_target_session(None, None, None, None, &discovered_sessions).expect_err("ambiguous");
    assert_eq!(
        read_rejection_reason(&selection_error),
        RejectReason::TargetAmbiguous
    );
}

#[test]
fn session_name_matches_exactly_one() {
    let target_session_id = SessionId::new();
    let discovered_sessions = build_complete_discovery([
        build_session_overview("amber-fox", SessionId::new(), &[], &[], &[]),
        build_session_overview("blue-owl", target_session_id, &[], &[], &[]),
    ]);
    let session_reference = SessionReference::SessionName("blue-owl".to_string());
    let selected_session_overview = select_target_session(
        Some(&session_reference),
        None,
        None,
        None,
        &discovered_sessions,
    )
    .expect("unique name");
    assert_eq!(
        selected_session_overview.session.session_id,
        target_session_id
    );
}

#[test]
fn unknown_session_name_is_not_running() {
    let discovered_sessions = build_complete_discovery([build_session_overview(
        "amber-fox",
        SessionId::new(),
        &[],
        &[],
        &[],
    )]);
    let session_reference = SessionReference::SessionName("blue-owl".to_string());
    let selection_error = select_target_session(
        Some(&session_reference),
        None,
        None,
        None,
        &discovered_sessions,
    )
    .expect_err("no match");
    assert!(
        matches!(&selection_error, CliError::SessionNotFound { session_name } if session_name == "blue-owl"),
        "got {selection_error:?}"
    );
}

#[test]
fn duplicate_session_name_is_ambiguous() {
    let first_session_id = SessionId::from_uuid(uuid!("019bb2ba-0000-7000-8000-000000000001"));
    let second_session_id = SessionId::from_uuid(uuid!("019bb2ba-0000-7000-8000-000000000002"));
    let discovered_sessions = build_complete_discovery([
        build_session_overview("amber-fox", first_session_id, &[], &[], &[]),
        build_session_overview("amber-fox", second_session_id, &[], &[], &[]),
    ]);
    let session_reference = SessionReference::SessionName("amber-fox".to_string());
    let selection_error = select_target_session(
        Some(&session_reference),
        None,
        None,
        None,
        &discovered_sessions,
    )
    .expect_err("two match");
    assert_eq!(
        read_rejection_reason(&selection_error),
        RejectReason::TargetAmbiguous
    );
    assert_eq!(
        read_rejection_help(&selection_error),
        "several sessions are named `amber-fox`: \
         session-019bb2ba-0000-7000-8000-000000000001, \
         session-019bb2ba-0000-7000-8000-000000000002; use the session id"
    );
}

#[test]
fn session_id_not_advertised_is_not_running() {
    let discovered_sessions = build_complete_discovery([build_session_overview(
        "amber-fox",
        SessionId::new(),
        &[],
        &[],
        &[],
    )]);
    let missing_session_id = SessionId::new();
    let session_reference = SessionReference::SessionId(missing_session_id);
    let selection_error = select_target_session(
        Some(&session_reference),
        None,
        None,
        None,
        &discovered_sessions,
    )
    .expect_err("not running");
    assert!(
        matches!(&selection_error, CliError::SessionNotFound { session_name } if *session_name == missing_session_id.to_string()),
        "got {selection_error:?}"
    );
}

#[test]
fn explicit_pane_picks_its_owning_session() {
    let target_session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let discovered_sessions = build_complete_discovery([
        build_session_overview("amber-fox", SessionId::new(), &[], &[], &[]),
        build_session_overview(
            "blue-owl",
            target_session_id,
            &[(tab_id, "one")],
            &[(pane_id, tab_id)],
            &[],
        ),
    ]);
    let selected_session_overview =
        select_target_session(None, Some(pane_id), None, None, &discovered_sessions)
            .expect("owner found");
    assert_eq!(
        selected_session_overview.session.session_id,
        target_session_id
    );
}

#[test]
fn pane_in_no_session_is_not_found() {
    let discovered_sessions = build_complete_discovery([build_session_overview(
        "amber-fox",
        SessionId::new(),
        &[],
        &[],
        &[],
    )]);
    let selection_error =
        select_target_session(None, Some(PaneId::new()), None, None, &discovered_sessions)
            .expect_err("nowhere");
    assert_eq!(
        read_rejection_reason(&selection_error),
        RejectReason::TargetNotFound
    );
}

#[test]
fn explicit_session_with_a_pane_from_another_session_refuses() {
    let named_session_id = SessionId::new();
    let other_tab_id = TabId::new();
    let foreign_pane_id = PaneId::new();
    let discovered_sessions = build_complete_discovery([
        build_session_overview("amber-fox", named_session_id, &[], &[], &[]),
        build_session_overview(
            "blue-owl",
            SessionId::new(),
            &[(other_tab_id, "one")],
            &[(foreign_pane_id, other_tab_id)],
            &[],
        ),
    ]);
    let session_reference = SessionReference::SessionName("amber-fox".to_string());
    let selection_error = select_target_session(
        Some(&session_reference),
        Some(foreign_pane_id),
        None,
        None,
        &discovered_sessions,
    )
    .expect_err("mismatch never retargets");
    assert_eq!(
        read_rejection_reason(&selection_error),
        RejectReason::TargetNotFound
    );
}

#[test]
fn explicit_client_picks_its_session() {
    let target_session_id = SessionId::new();
    let tab_id = TabId::new();
    let client_id = ClientId::new();
    let discovered_sessions = build_complete_discovery([
        build_session_overview("amber-fox", SessionId::new(), &[], &[], &[]),
        build_session_overview(
            "blue-owl",
            target_session_id,
            &[(tab_id, "one")],
            &[],
            &[client_id],
        ),
    ]);
    let selected_session_overview =
        select_target_session(None, None, None, Some(client_id), &discovered_sessions)
            .expect("owner found");
    assert_eq!(
        selected_session_overview.session.session_id,
        target_session_id
    );
}

#[test]
fn detached_client_is_not_found_anywhere() {
    let tab_id = TabId::new();
    let discovered_sessions = build_complete_discovery([build_session_overview(
        "amber-fox",
        SessionId::new(),
        &[(tab_id, "one")],
        &[],
        &[],
    )]);
    let selection_error = select_target_session(
        None,
        None,
        None,
        Some(ClientId::new()),
        &discovered_sessions,
    )
    .expect_err("attached only");
    assert_eq!(
        read_rejection_reason(&selection_error),
        RejectReason::TargetNotFound
    );
}

#[test]
fn a_client_from_another_session_is_not_retargeted() {
    let named_session_id = SessionId::new();
    let other_tab_id = TabId::new();
    let foreign_client_id = ClientId::new();
    let discovered_sessions = build_complete_discovery([
        build_session_overview("amber-fox", named_session_id, &[], &[], &[]),
        build_session_overview(
            "blue-owl",
            SessionId::new(),
            &[(other_tab_id, "one")],
            &[],
            &[foreign_client_id],
        ),
    ]);
    let session_reference = SessionReference::SessionName("amber-fox".to_string());
    let selection_error = select_target_session(
        Some(&session_reference),
        None,
        None,
        Some(foreign_client_id),
        &discovered_sessions,
    )
    .expect_err("mismatch never retargets");
    assert_eq!(
        read_rejection_reason(&selection_error),
        RejectReason::TargetNotFound
    );
}

#[test]
fn a_new_tab_client_flag_reaches_the_session_lookup() {
    let client_session_id = SessionId::new();
    let client_session_tab_id = TabId::new();
    let client_id = ClientId::new();
    let discovered_sessions = build_complete_discovery([
        build_session_overview("amber-fox", SessionId::new(), &[], &[], &[]),
        build_session_overview(
            "blue-owl",
            client_session_id,
            &[(client_session_tab_id, "one")],
            &[],
            &[client_id],
        ),
    ]);
    let command = CliCommand::NewTab {
        session_reference: None,
        client_id: Some(client_id),
    };
    let (target_session_id, resolved_targets) =
        resolve_command_targets(&command, &discovered_sessions, None)
            .expect("the client names its session");
    assert_eq!(target_session_id, client_session_id);
    assert_eq!(
        resolved_targets,
        ResolvedTargets {
            session_id: Some(client_session_id),
            tab_id: None,
            pane_id: None,
        }
    );
}

#[test]
fn a_fullscreen_client_flag_reaches_the_session_lookup() {
    let client_session_id = SessionId::new();
    let client_session_tab_id = TabId::new();
    let client_id = ClientId::new();
    let discovered_sessions = build_complete_discovery([
        build_session_overview("amber-fox", SessionId::new(), &[], &[], &[]),
        build_session_overview(
            "blue-owl",
            client_session_id,
            &[(client_session_tab_id, "one")],
            &[],
            &[client_id],
        ),
    ]);
    let command = CliCommand::TogglePaneFullscreen {
        client_id: Some(client_id),
    };
    let (target_session_id, resolved_targets) =
        resolve_command_targets(&command, &discovered_sessions, None)
            .expect("the client names its session");
    assert_eq!(target_session_id, client_session_id);
    assert_eq!(
        resolved_targets,
        ResolvedTargets {
            session_id: Some(client_session_id),
            tab_id: None,
            pane_id: None,
        }
    );
}

#[test]
fn tab_id_picks_its_owning_session() {
    let target_session_id = SessionId::new();
    let tab_id = TabId::new();
    let discovered_sessions = build_complete_discovery([
        build_session_overview("amber-fox", SessionId::new(), &[], &[], &[]),
        build_session_overview("blue-owl", target_session_id, &[(tab_id, "one")], &[], &[]),
    ]);
    let tab_reference = TabReference::TabId(tab_id);
    let selected_session_overview =
        select_target_session(None, None, Some(&tab_reference), None, &discovered_sessions)
            .expect("owner found");
    assert_eq!(
        selected_session_overview.session.session_id,
        target_session_id
    );
}

#[test]
fn tab_name_owned_by_two_sessions_is_ambiguous() {
    let first_tab_id = TabId::from_uuid(uuid!("019bb2ba-0001-7000-8000-000000000001"));
    let second_tab_id = TabId::from_uuid(uuid!("019bb2ba-0001-7000-8000-000000000002"));
    let discovered_sessions = build_complete_discovery([
        build_session_overview(
            "amber-fox",
            SessionId::new(),
            &[(first_tab_id, "logs")],
            &[],
            &[],
        ),
        build_session_overview(
            "blue-owl",
            SessionId::new(),
            &[(second_tab_id, "logs")],
            &[],
            &[],
        ),
    ]);
    let tab_reference = TabReference::TabName("logs".to_string());
    let selection_error =
        select_target_session(None, None, Some(&tab_reference), None, &discovered_sessions)
            .expect_err("two owners");
    assert_eq!(
        read_rejection_reason(&selection_error),
        RejectReason::TargetAmbiguous
    );
    assert_eq!(
        read_rejection_help(&selection_error),
        "several tabs are named `logs`: \
         tab-019bb2ba-0001-7000-8000-000000000001 in session `amber-fox`, \
         tab-019bb2ba-0001-7000-8000-000000000002 in session `blue-owl`; \
         use the tab id or --session"
    );
}

#[test]
fn two_tabs_of_one_session_sharing_a_name_are_ambiguous() {
    // Both matches live in one session: that session is the unambiguous
    // owner, and resolving the tab inside it refuses with the ids —
    // `--session` is not offered, only the tab ids tell them apart.
    let session_id = SessionId::new();
    let first_tab_id = TabId::from_uuid(uuid!("019bb2ba-0002-7000-8000-000000000001"));
    let second_tab_id = TabId::from_uuid(uuid!("019bb2ba-0002-7000-8000-000000000002"));
    let discovered_sessions = build_complete_discovery([build_session_overview(
        "amber-fox",
        session_id,
        &[(first_tab_id, "logs"), (second_tab_id, "logs")],
        &[],
        &[],
    )]);
    let tab_reference = TabReference::TabName("logs".to_string());

    let selected_session_overview =
        select_target_session(None, None, Some(&tab_reference), None, &discovered_sessions)
            .expect("one owning session");
    assert_eq!(selected_session_overview.session.session_id, session_id);

    let tab_resolution_error = resolve_target_tab(selected_session_overview, &tab_reference)
        .expect_err("two tabs share the name");
    assert_eq!(
        read_rejection_reason(&tab_resolution_error),
        RejectReason::TargetAmbiguous
    );
    assert_eq!(
        read_rejection_help(&tab_resolution_error),
        "several tabs are named `logs` in session `amber-fox`: \
         tab-019bb2ba-0002-7000-8000-000000000001, \
         tab-019bb2ba-0002-7000-8000-000000000002; \
         use the tab id"
    );
}

#[test]
fn inspecting_a_duplicated_tab_name_in_one_session_lists_its_ids() {
    let first_tab_id = TabId::from_uuid(uuid!("019bb2ba-0004-7000-8000-000000000001"));
    let second_tab_id = TabId::from_uuid(uuid!("019bb2ba-0004-7000-8000-000000000002"));
    let discovered_sessions = build_complete_discovery([build_session_overview(
        "amber-fox",
        SessionId::new(),
        &[(first_tab_id, "logs"), (second_tab_id, "logs")],
        &[],
        &[],
    )]);

    let tab_resolution_error = resolve_tab_reference(
        &discovered_sessions,
        &TabReference::TabName("logs".to_string()),
    )
    .expect_err("two tabs share the name");
    assert_eq!(
        read_rejection_reason(&tab_resolution_error),
        RejectReason::TargetAmbiguous
    );
    assert_eq!(
        read_rejection_help(&tab_resolution_error),
        "several tabs are named `logs` in session `amber-fox`: \
         tab-019bb2ba-0004-7000-8000-000000000001, \
         tab-019bb2ba-0004-7000-8000-000000000002; \
         use the tab id"
    );
}

#[test]
fn duplicate_tabs_spanning_sessions_still_offer_the_session_flag() {
    // Two matches in one session plus one in another: the matches span
    // sessions, so `--session` can still narrow to the session with the
    // unique tab.
    let first_tab_id = TabId::from_uuid(uuid!("019bb2ba-0003-7000-8000-000000000001"));
    let second_tab_id = TabId::from_uuid(uuid!("019bb2ba-0003-7000-8000-000000000002"));
    let third_tab_id = TabId::from_uuid(uuid!("019bb2ba-0003-7000-8000-000000000003"));
    let discovered_sessions = build_complete_discovery([
        build_session_overview(
            "amber-fox",
            SessionId::new(),
            &[(first_tab_id, "logs"), (second_tab_id, "logs")],
            &[],
            &[],
        ),
        build_session_overview(
            "blue-owl",
            SessionId::new(),
            &[(third_tab_id, "logs")],
            &[],
            &[],
        ),
    ]);
    let tab_reference = TabReference::TabName("logs".to_string());
    let selection_error =
        select_target_session(None, None, Some(&tab_reference), None, &discovered_sessions)
            .expect_err("three tabs");
    assert_eq!(
        read_rejection_reason(&selection_error),
        RejectReason::TargetAmbiguous
    );
    assert_eq!(
        read_rejection_help(&selection_error),
        "several tabs are named `logs`: \
         tab-019bb2ba-0003-7000-8000-000000000001 in session `amber-fox`, \
         tab-019bb2ba-0003-7000-8000-000000000002 in session `amber-fox`, \
         tab-019bb2ba-0003-7000-8000-000000000003 in session `blue-owl`; \
         use the tab id or --session"
    );
}

#[test]
fn a_tab_name_no_session_holds_is_not_found() {
    let discovered_sessions = build_complete_discovery([build_session_overview(
        "amber-fox",
        SessionId::new(),
        &[(TabId::new(), "work")],
        &[],
        &[],
    )]);
    let tab_reference = TabReference::TabName("logs".to_string());
    let selection_error =
        select_target_session(None, None, Some(&tab_reference), None, &discovered_sessions)
            .expect_err("nowhere");
    assert_eq!(
        read_rejection_reason(&selection_error),
        RejectReason::TargetNotFound
    );
    assert_eq!(
        read_rejection_help(&selection_error),
        "no running session has tab named `logs`"
    );
}

#[test]
fn tab_name_resolves_within_the_session() {
    let session_id = SessionId::new();
    let logs_tab_id = TabId::new();
    let session_overview = build_session_overview(
        "amber-fox",
        session_id,
        &[(TabId::new(), "work"), (logs_tab_id, "logs")],
        &[],
        &[],
    );
    let resolved_tab_id = resolve_target_tab(
        &session_overview,
        &TabReference::TabName("logs".to_string()),
    )
    .expect("unique name");
    assert_eq!(resolved_tab_id, logs_tab_id);
}

#[test]
fn duplicate_tab_name_in_the_session_is_ambiguous() {
    let first_tab_id = TabId::from_uuid(uuid!("019bb2ba-0003-7000-8000-000000000001"));
    let second_tab_id = TabId::from_uuid(uuid!("019bb2ba-0003-7000-8000-000000000002"));
    let session_overview = build_session_overview(
        "amber-fox",
        SessionId::new(),
        &[(first_tab_id, "logs"), (second_tab_id, "logs")],
        &[],
        &[],
    );
    let tab_resolution_error = resolve_target_tab(
        &session_overview,
        &TabReference::TabName("logs".to_string()),
    )
    .expect_err("two match");
    assert_eq!(
        read_rejection_reason(&tab_resolution_error),
        RejectReason::TargetAmbiguous
    );
    assert_eq!(
        read_rejection_help(&tab_resolution_error),
        "several tabs are named `logs` in session `amber-fox`: \
         tab-019bb2ba-0003-7000-8000-000000000001, \
         tab-019bb2ba-0003-7000-8000-000000000002; use the tab id"
    );
}

#[test]
fn unknown_tab_name_in_the_session_is_not_found() {
    let session_overview = build_session_overview(
        "amber-fox",
        SessionId::new(),
        &[(TabId::new(), "work")],
        &[],
        &[],
    );
    let tab_resolution_error = resolve_target_tab(
        &session_overview,
        &TabReference::TabName("logs".to_string()),
    )
    .expect_err("no match");
    assert_eq!(
        read_rejection_reason(&tab_resolution_error),
        RejectReason::TargetNotFound
    );
    assert_eq!(
        read_rejection_help(&tab_resolution_error),
        "no tab named `logs` in session `amber-fox`"
    );
}

#[test]
fn tab_id_outside_the_session_is_not_found() {
    let session_overview = build_session_overview(
        "amber-fox",
        SessionId::new(),
        &[(TabId::new(), "work")],
        &[],
        &[],
    );
    let tab_resolution_error =
        resolve_target_tab(&session_overview, &TabReference::TabId(TabId::new()))
            .expect_err("foreign tab");
    assert_eq!(
        read_rejection_reason(&tab_resolution_error),
        RejectReason::TargetNotFound
    );
}

#[test]
fn in_session_command_with_no_flags_routes_home_without_probing() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let in_session_context = InSessionContext {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
    };
    let command = CliCommand::ClosePane {
        pane_id: None,
        should_force_close: false,
    };
    let command_route = resolve_command_route_in_runtime_directory(
        runtime_directory.path(),
        None,
        &command,
        Some(&in_session_context),
    )
    .expect("home route needs no probe");
    assert_eq!(command_route, Route::InSession(ResolvedTargets::default()));
}

#[test]
fn in_session_command_naming_its_own_session_id_under_another_user_does_not_route_home() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    #[cfg(unix)]
    let other_user_id = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(runtime_directory.path())
            .expect("read the runtime directory")
            .uid()
            + 1
    };
    #[cfg(windows)]
    let other_user_id = 1001;
    let in_session_context = InSessionContext {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
    };
    let command = CliCommand::CloseTab {
        tab_reference: None,
        session_reference: Some(SessionReference::OwnedSessionId(OwnedSessionId {
            owner_user_id: other_user_id,
            session_id: in_session_context.session_id,
        })),
        should_force_close: false,
    };

    let command_route = resolve_command_route_in_runtime_directory(
        runtime_directory.path(),
        None,
        &command,
        Some(&in_session_context),
    );

    let Err(CliError::SessionNotFound { session_name }) = command_route else {
        panic!("expected SessionNotFound, got {command_route:?}");
    };
    assert_eq!(
        session_name,
        format!("{other_user_id}/{}", in_session_context.session_id)
    );
}

#[cfg(unix)]
#[test]
fn in_session_command_naming_its_own_session_id_in_another_users_folder_routes_there() {
    use std::os::unix::fs::MetadataExt;

    // Every folder a test makes belongs to the user running it. With no
    // runtime directory, that user's id names another user.
    let shared_sessions_base_directory = build_test_runtime_directory("owned-same-id");
    let runtime_directory = shared_sessions_base_directory.join("missing-runtime");
    let folder_owner_user_id = std::fs::metadata(&shared_sessions_base_directory)
        .expect("read the shared directory")
        .uid();
    let user_directory =
        shared_sessions_base_directory.join((folder_owner_user_id + 1).to_string());
    std::fs::create_dir(&user_directory).expect("create a user's folder");
    let in_session_context = InSessionContext {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
    };
    let listener = Listener::bind(&koshi_ipc::endpoint::compute_socket_address(
        &user_directory,
        in_session_context.session_id,
    ))
    .expect("the other user's session binds");
    let session_overview =
        build_session_overview("amber-fox", in_session_context.session_id, &[], &[], &[]);
    let discovery_server_thread = std::thread::spawn(move || {
        let mut discovery_connection = listener.accept().expect("accept discovery");
        let hello_request: IpcRequest = discovery_connection.recv().expect("read discovery hello");
        let discovery_request: IpcRequest =
            discovery_connection.recv().expect("read discovery request");
        let IpcRequestKind::Hello {
            connection_token: presented_connection_token,
            ..
        } = &hello_request.request_kind
        else {
            panic!("expected a Hello first");
        };
        assert_eq!(presented_connection_token.expose_secret(), "");
        send_ipc_reply(
            &mut discovery_connection,
            hello_request.request_id,
            build_accepted_hello_result(),
        );
        send_ipc_reply(
            &mut discovery_connection,
            discovery_request.request_id,
            IpcResult::Overview(session_overview),
        );
    });
    let command = CliCommand::CloseTab {
        tab_reference: None,
        session_reference: Some(SessionReference::OwnedSessionId(OwnedSessionId {
            owner_user_id: folder_owner_user_id,
            session_id: in_session_context.session_id,
        })),
        should_force_close: false,
    };

    let command_route = resolve_command_route_in_runtime_directory(
        &runtime_directory,
        Some(&shared_sessions_base_directory),
        &command,
        Some(&in_session_context),
    )
    .expect("the other user's session answers");

    discovery_server_thread
        .join()
        .expect("stand-in session exits");
    assert_eq!(
        command_route,
        Route::External {
            session_id: in_session_context.session_id,
            owner_user_id: Some(folder_owner_user_id),
            targets: ResolvedTargets {
                session_id: Some(in_session_context.session_id),
                tab_id: None,
                pane_id: None,
            },
        }
    );
    let _ = std::fs::remove_dir_all(&shared_sessions_base_directory);
}

#[test]
fn in_session_tab_id_routes_home_and_rides_into_the_command() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let in_session_context = InSessionContext {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
    };
    let tab_id = TabId::new();
    let command = CliCommand::CloseTab {
        tab_reference: Some(TabReference::TabId(tab_id)),
        session_reference: None,
        should_force_close: false,
    };
    // An id needs no lookup: the route resolves nothing and `build_action_command`
    // carries the id into the command directly.
    let command_route = resolve_command_route_in_runtime_directory(
        runtime_directory.path(),
        None,
        &command,
        Some(&in_session_context),
    )
    .expect("id needs no lookup");
    let Route::InSession(resolved_targets) = command_route else {
        panic!("expected the home route, got {command_route:?}");
    };
    assert_eq!(resolved_targets, ResolvedTargets::default());
    let (_, mapped_command) = command
        .build_action_command(&resolved_targets, koshi_core::geometry::Direction::Right)
        .expect("close-tab is an action");
    assert_eq!(
        mapped_command,
        koshi_core::command::Command::CloseTab(koshi_core::command::CloseTabArgs {
            tab_id: Some(tab_id),
            should_force_close: false,
            should_kill_process_tree: false,
        })
    );
}

#[test]
fn in_session_move_tab_by_id_routes_home_and_rides_into_the_command() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let in_session_context = InSessionContext {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
    };
    let tab_id = TabId::new();
    let command = CliCommand::MoveTab {
        tab_index: 2,
        tab_reference: Some(TabReference::TabId(tab_id)),
    };
    // The runtime directory under test holds no session, so answering at all
    // proves no session was probed.
    let command_route = resolve_command_route_in_runtime_directory(
        runtime_directory.path(),
        None,
        &command,
        Some(&in_session_context),
    )
    .expect("id needs no lookup");
    let Route::InSession(resolved_targets) = command_route else {
        panic!("expected the home route, got {command_route:?}");
    };
    assert_eq!(resolved_targets, ResolvedTargets::default());
    let (_, mapped_command) = command
        .build_action_command(&resolved_targets, koshi_core::geometry::Direction::Right)
        .expect("move-tab is an action");
    assert_eq!(
        mapped_command,
        koshi_core::command::Command::MoveTab(koshi_core::command::MoveTabArgs {
            tab_id: Some(tab_id),
            target_tab_index: 2,
        })
    );
}

#[test]
fn in_session_focus_tab_by_id_routes_home_and_rides_into_the_command() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let in_session_context = InSessionContext {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
    };
    let tab_id = TabId::new();
    let command = CliCommand::FocusTab {
        tab_index: None,
        tab_reference: Some(TabReference::TabId(tab_id)),
        client_id: None,
    };
    let command_route = resolve_command_route_in_runtime_directory(
        runtime_directory.path(),
        None,
        &command,
        Some(&in_session_context),
    )
    .expect("id needs no lookup");
    let Route::InSession(resolved_targets) = command_route else {
        panic!("expected the home route, got {command_route:?}");
    };
    assert_eq!(resolved_targets, ResolvedTargets::default());
    let (_, mapped_command) = command
        .build_action_command(&resolved_targets, koshi_core::geometry::Direction::Right)
        .expect("focus-tab is an action");
    assert_eq!(
        mapped_command,
        koshi_core::command::Command::FocusTab(koshi_core::command::FocusTabArgs {
            focus_target: koshi_core::command::TabTarget::Id(tab_id),
            client_id: None,
        })
    );
}

#[test]
fn a_named_client_leaves_the_home_route() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let in_session_context = InSessionContext {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
    };
    let bare_command = CliCommand::TogglePaneFullscreen { client_id: None };
    let home_route = resolve_command_route_in_runtime_directory(
        runtime_directory.path(),
        None,
        &bare_command,
        Some(&in_session_context),
    )
    .expect("no flag needs no lookup");
    assert_eq!(home_route, Route::InSession(ResolvedTargets::default()));

    // The named client leaves home and is looked up across the running
    // sessions, of which the empty runtime directory has none.
    let named_client_id = ClientId::new();
    let client_named_command = CliCommand::TogglePaneFullscreen {
        client_id: Some(named_client_id),
    };
    let client_route = resolve_command_route_in_runtime_directory(
        runtime_directory.path(),
        None,
        &client_named_command,
        Some(&in_session_context),
    );
    let Err(CliError::CommandRejected { reason, help }) = client_route else {
        panic!("expected the client lookup to be refused, got {client_route:?}");
    };
    assert_eq!(reason, RejectReason::TargetNotFound);
    assert_eq!(
        help,
        Some(format!("no running session has client {named_client_id}"))
    );
}

#[test]
fn in_session_new_tab_with_a_client_stays_home() {
    let runtime_directory = koshi_test_support::fixtures::build_test_runtime_directory();
    let in_session_context = InSessionContext {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
    };
    let client_id = ClientId::new();
    let command = CliCommand::NewTab {
        session_reference: None,
        client_id: Some(client_id),
    };
    let command_route = resolve_command_route_in_runtime_directory(
        runtime_directory.path(),
        None,
        &command,
        Some(&in_session_context),
    )
    .expect("a client needs no lookup");
    let Route::InSession(resolved_targets) = command_route else {
        panic!("expected the home route, got {command_route:?}");
    };
    assert_eq!(resolved_targets, ResolvedTargets::default());
    let (_, mapped_command) = command
        .build_action_command(&resolved_targets, koshi_core::geometry::Direction::Right)
        .expect("new-tab is an action");
    assert_eq!(
        mapped_command,
        koshi_core::command::Command::NewTab(koshi_core::command::NewTabArgs {
            working_directory: None,
            client_id: Some(client_id),
        })
    );
}

#[test]
fn a_tab_id_resolves_without_any_census() {
    let tab_id = TabId::new();
    let resolved_tab_id =
        resolve_tab_reference(&build_complete_discovery([]), &TabReference::TabId(tab_id))
            .expect("an id is its own answer");
    assert_eq!(resolved_tab_id, tab_id);
}

#[test]
fn a_tab_name_resolves_across_the_sessions_in_scope() {
    let logs_tab_id = TabId::new();
    let discovered_sessions = build_complete_discovery([
        build_session_overview(
            "amber-fox",
            SessionId::new(),
            &[(TabId::new(), "work")],
            &[],
            &[],
        ),
        build_session_overview(
            "blue-owl",
            SessionId::new(),
            &[(logs_tab_id, "logs")],
            &[],
            &[],
        ),
    ]);
    let resolved_tab_id = resolve_tab_reference(
        &discovered_sessions,
        &TabReference::TabName("logs".to_string()),
    )
    .expect("one session holds it");
    assert_eq!(resolved_tab_id, logs_tab_id);
}

#[test]
fn a_session_id_scopes_to_that_session_alone() {
    let runtime_directory = build_test_runtime_directory("by-session-id");
    let target_session_overview =
        build_session_overview("amber-fox", SessionId::new(), &[], &[], &[]);
    let target_session_id = target_session_overview.session.session_id;
    let discovery_server_thread =
        serve_discovery(&runtime_directory, target_session_overview.clone());
    // A second advertised session whose endpoint file cannot be read. A sweep
    // of every session counts it unanswered; asking the named session alone
    // leaves `unasked_session_count` at `0`.
    std::fs::write(
        EndpointFile::resolve_endpoint_file_path(&runtime_directory, SessionId::new()),
        b"not an endpoint file",
    )
    .expect("second endpoint file written");

    let discovered_sessions = resolve_session_scope(
        &runtime_directory,
        None,
        Some(&SessionReference::SessionId(target_session_id)),
    )
    .expect("id scope answers");

    assert_eq!(discovered_sessions.sessions, vec![target_session_overview]);
    assert_eq!(discovered_sessions.unasked_session_count, 0);
    discovery_server_thread
        .join()
        .expect("stand-in session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

/// One remote session record a server offers, named `session_name` at `session_id`.
fn build_remote_session_row(session_id: SessionId, session_name: &str) -> RemoteSessionRow {
    RemoteSessionRow {
        session_id,
        session_name: session_name.to_string(),
    }
}

#[test]
fn an_explicit_session_id_asks_that_one_remote_session_and_no_other() {
    // An explicit id keeps one row out of three.
    let target_session_id = SessionId::new();
    let remote_session_rows = vec![
        build_remote_session_row(SessionId::new(), "S-first"),
        build_remote_session_row(target_session_id, "S-wanted"),
        build_remote_session_row(SessionId::new(), "S-third"),
    ];

    let probed_remote_session_rows = select_remote_rows_to_probe(
        Some(&SessionReference::SessionId(target_session_id)),
        remote_session_rows,
    );

    assert_eq!(
        probed_remote_session_rows,
        vec![build_remote_session_row(target_session_id, "S-wanted")],
        "one dial, not three"
    );
}

#[test]
fn a_session_name_asks_every_remote_session() {
    // A name keeps every row.
    let remote_session_rows = vec![
        build_remote_session_row(SessionId::new(), "S-first"),
        build_remote_session_row(SessionId::new(), "S-second"),
    ];

    let probed_remote_session_rows = select_remote_rows_to_probe(
        Some(&SessionReference::SessionName("S-first".to_string())),
        remote_session_rows.clone(),
    );

    assert_eq!(
        probed_remote_session_rows, remote_session_rows,
        "a name needs the whole picture"
    );
}

#[test]
fn no_session_flag_asks_every_remote_session() {
    // No flag keeps every row.
    let remote_session_rows = vec![
        build_remote_session_row(SessionId::new(), "S-first"),
        build_remote_session_row(SessionId::new(), "S-second"),
    ];

    assert_eq!(
        select_remote_rows_to_probe(None, remote_session_rows.clone()),
        remote_session_rows
    );
}

#[test]
fn an_explicit_id_no_remote_session_carries_asks_nothing() {
    // An id no row carries keeps no rows.
    let remote_session_rows = vec![build_remote_session_row(SessionId::new(), "S-first")];

    assert!(select_remote_rows_to_probe(
        Some(&SessionReference::SessionId(SessionId::new())),
        remote_session_rows
    )
    .is_empty());
}

/// The discovery row of floating pane `pane_id` in session `session_id`: no
/// tab, no title, no working directory, no command, focused by no client.
fn build_floating_pane_discovery(pane_id: PaneId, session_id: SessionId) -> PaneDiscovery {
    PaneDiscovery {
        pane_id,
        tab_id: None,
        session_id,
        pane_title: None,
        working_directory: None,
        command_argv: None,
        lifecycle: PaneLifecycle::Running,
        focused_by_client_ids: Vec::new(),
    }
}

/// A bare `dock-pane`: no `--pane`, `--tab`, `--stack-onto`, `--direction` or
/// `--client`.
fn build_bare_dock_pane_command() -> CliCommand {
    CliCommand::DockPane {
        pane_id: None,
        tab_reference: None,
        direction: None,
        stack_onto_pane_id: None,
        client_id: None,
    }
}

#[test]
fn a_bare_dock_pane_resolves_the_sole_tab_of_the_target_session() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let discovered_sessions = build_complete_discovery([build_session_overview(
        "amber-fox",
        session_id,
        &[(tab_id, "one")],
        &[(PaneId::new(), tab_id)],
        &[],
    )]);
    assert_eq!(
        resolve_command_targets(&build_bare_dock_pane_command(), &discovered_sessions, None)
            .expect("one tab resolves"),
        (
            session_id,
            ResolvedTargets {
                session_id: Some(session_id),
                tab_id: Some(tab_id),
                pane_id: None,
            }
        )
    );
}

#[test]
fn a_bare_dock_pane_with_two_tabs_and_no_client_to_read_names_the_tab_count() {
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    for client_ids in [Vec::new(), vec![ClientId::new(), ClientId::new()]] {
        let discovered_sessions = build_complete_discovery([build_session_overview(
            "amber-fox",
            SessionId::new(),
            &[(first_tab_id, "one"), (second_tab_id, "two")],
            &[
                (PaneId::new(), first_tab_id),
                (PaneId::new(), second_tab_id),
            ],
            &client_ids,
        )]);
        let selection_error =
            resolve_command_targets(&build_bare_dock_pane_command(), &discovered_sessions, None)
                .expect_err("two tabs with no client to read are ambiguous");
        assert_eq!(
            read_rejection_reason(&selection_error),
            RejectReason::TargetAmbiguous,
            "with {} clients",
            client_ids.len()
        );
        assert_eq!(
            read_rejection_help(&selection_error),
            "session `amber-fox` has 2 tabs; name one with --tab or --stack-onto",
            "with {} clients",
            client_ids.len()
        );
    }
}

#[test]
fn a_bare_dock_pane_docks_into_the_tab_the_only_attached_client_views() {
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let mut session_overview = build_session_overview(
        "amber-fox",
        SessionId::new(),
        &[(first_tab_id, "one"), (second_tab_id, "two")],
        &[(PaneId::new(), first_tab_id)],
        &[ClientId::new()],
    );
    session_overview.clients[0].active_tab_id = second_tab_id;
    assert_eq!(
        resolve_command_tab(&build_bare_dock_pane_command(), &session_overview, None)
            .expect("the client's tab resolves"),
        Some(second_tab_id)
    );
}

#[test]
fn a_bare_dock_pane_with_a_client_flag_docks_into_the_tab_that_client_views() {
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let tiled_pane_id = PaneId::new();
    let first_client_id = ClientId::new();
    let second_client_id = ClientId::new();
    let mut session_overview = build_session_overview(
        "amber-fox",
        SessionId::new(),
        &[(first_tab_id, "one"), (second_tab_id, "two")],
        &[(tiled_pane_id, first_tab_id)],
        &[first_client_id, second_client_id],
    );
    session_overview.clients[1].active_tab_id = second_tab_id;
    let command = CliCommand::DockPane {
        pane_id: None,
        tab_reference: None,
        direction: None,
        stack_onto_pane_id: None,
        client_id: Some(second_client_id),
    };
    // `--client` wins over the tiled pane the CLI runs in and over its own
    // client.
    let in_session_context = InSessionContext {
        session_id: session_overview.session.session_id,
        client_id: Some(first_client_id),
        pane_id: tiled_pane_id,
    };
    for in_session_context in [None, Some(&in_session_context)] {
        assert_eq!(
            resolve_command_tab(&command, &session_overview, in_session_context)
                .expect("the named client's tab resolves"),
            Some(second_tab_id),
            "with in-session context {in_session_context:?}"
        );
    }
}

#[test]
fn a_bare_dock_pane_typed_in_a_tiled_pane_docks_into_that_panes_tab() {
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let tiled_pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session_overview = build_session_overview(
        "amber-fox",
        SessionId::new(),
        &[(first_tab_id, "one"), (second_tab_id, "two")],
        &[(tiled_pane_id, first_tab_id)],
        &[client_id],
    );
    session_overview.clients[0].active_tab_id = second_tab_id;
    let in_session_context = InSessionContext {
        session_id: session_overview.session.session_id,
        client_id: Some(client_id),
        pane_id: tiled_pane_id,
    };
    assert_eq!(
        resolve_command_tab(
            &build_bare_dock_pane_command(),
            &session_overview,
            Some(&in_session_context)
        )
        .expect("the pane's tab resolves"),
        Some(first_tab_id)
    );
}

#[test]
fn a_bare_dock_pane_typed_in_a_floating_pane_docks_into_the_tab_its_client_views() {
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let floating_pane_id = PaneId::new();
    let first_client_id = ClientId::new();
    let second_client_id = ClientId::new();
    let mut session_overview = build_session_overview(
        "amber-fox",
        SessionId::new(),
        &[(first_tab_id, "one"), (second_tab_id, "two")],
        &[(PaneId::new(), first_tab_id)],
        &[first_client_id, second_client_id],
    );
    session_overview.clients[1].active_tab_id = second_tab_id;
    session_overview.panes.push(build_floating_pane_discovery(
        floating_pane_id,
        session_overview.session.session_id,
    ));
    let in_session_context = InSessionContext {
        session_id: session_overview.session.session_id,
        client_id: Some(second_client_id),
        pane_id: floating_pane_id,
    };
    assert_eq!(
        resolve_command_tab(
            &build_bare_dock_pane_command(),
            &session_overview,
            Some(&in_session_context)
        )
        .expect("the pane's client's tab resolves"),
        Some(second_tab_id)
    );

    // A client that is gone leaves two attached clients to choose from.
    let detached_client_context = InSessionContext {
        client_id: Some(ClientId::new()),
        ..in_session_context
    };
    let selection_error = resolve_command_tab(
        &build_bare_dock_pane_command(),
        &session_overview,
        Some(&detached_client_context),
    )
    .expect_err("two attached clients are ambiguous");
    assert_eq!(
        read_rejection_help(&selection_error),
        "session `amber-fox` has 2 tabs; name one with --tab or --stack-onto"
    );
}

#[test]
fn a_dock_pane_with_a_stack_target_resolves_no_tab() {
    let session_id = SessionId::new();
    let first_tab_id = TabId::new();
    let stack_pane_id = PaneId::new();
    let discovered_sessions = build_complete_discovery([build_session_overview(
        "amber-fox",
        session_id,
        &[(first_tab_id, "one"), (TabId::new(), "two")],
        &[(stack_pane_id, first_tab_id)],
        &[],
    )]);
    let command = CliCommand::DockPane {
        pane_id: None,
        tab_reference: None,
        direction: None,
        stack_onto_pane_id: Some(stack_pane_id),
        client_id: None,
    };
    assert_eq!(
        resolve_command_targets(&command, &discovered_sessions, None)
            .expect("stack target resolves"),
        (
            session_id,
            ResolvedTargets {
                session_id: Some(session_id),
                tab_id: None,
                pane_id: None,
            }
        )
    );
}

#[test]
fn an_in_session_bare_dock_pane_asks_its_own_session_for_its_panes_tab() {
    let runtime_directory = build_test_runtime_directory("dock-pane-tab");
    let in_session_context = InSessionContext {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
    };
    let tab_id = TabId::new();
    let discovery_server_thread = serve_discovery(
        &runtime_directory,
        build_session_overview(
            "amber-fox",
            in_session_context.session_id,
            &[(TabId::new(), "one"), (tab_id, "two")],
            &[(in_session_context.pane_id, tab_id)],
            &[],
        ),
    );
    let command_route = resolve_command_route_in_runtime_directory(
        &runtime_directory,
        None,
        &build_bare_dock_pane_command(),
        Some(&in_session_context),
    )
    .expect("the session answers");
    assert_eq!(
        command_route,
        Route::InSession(ResolvedTargets {
            session_id: None,
            tab_id: Some(tab_id),
            pane_id: None,
        })
    );
    discovery_server_thread
        .join()
        .expect("stand-in session exits");
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_client_flag_typed_in_a_pane_goes_external_with_that_pane_as_the_default_pane() {
    let runtime_directory = build_test_runtime_directory("client-flag-default-pane");
    let named_client_id = ClientId::new();
    let in_session_context = InSessionContext {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
    };
    let tab_id = TabId::new();
    let discovery_server_thread = serve_discovery(
        &runtime_directory,
        build_session_overview(
            "amber-fox",
            in_session_context.session_id,
            &[(tab_id, "one")],
            &[(in_session_context.pane_id, tab_id)],
            &[named_client_id],
        ),
    );
    let command = CliCommand::PinPane {
        pane_id: None,
        client_id: Some(named_client_id),
    };

    let command_route = resolve_command_route_in_runtime_directory(
        &runtime_directory,
        None,
        &command,
        Some(&in_session_context),
    )
    .expect("the session answers");

    discovery_server_thread
        .join()
        .expect("stand-in session exits");
    assert_eq!(
        command_route,
        Route::External {
            session_id: in_session_context.session_id,
            owner_user_id: None,
            targets: ResolvedTargets {
                session_id: Some(in_session_context.session_id),
                tab_id: None,
                pane_id: Some(in_session_context.pane_id),
            },
        }
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn a_client_flag_naming_another_sessions_client_carries_no_default_pane() {
    let runtime_directory = build_test_runtime_directory("client-flag-other-session");
    let named_client_id = ClientId::new();
    let in_session_context = InSessionContext {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
    };
    let (home_tab_id, other_tab_id) = (TabId::new(), TabId::new());
    let other_session_id = SessionId::new();
    let home_discovery_server_thread = serve_discovery(
        &runtime_directory,
        build_session_overview(
            "amber-fox",
            in_session_context.session_id,
            &[(home_tab_id, "one")],
            &[(in_session_context.pane_id, home_tab_id)],
            &[],
        ),
    );
    let other_discovery_server_thread = serve_discovery(
        &runtime_directory,
        build_session_overview(
            "blue-owl",
            other_session_id,
            &[(other_tab_id, "one")],
            &[(PaneId::new(), other_tab_id)],
            &[named_client_id],
        ),
    );
    let command = CliCommand::PinPane {
        pane_id: None,
        client_id: Some(named_client_id),
    };

    let command_route = resolve_command_route_in_runtime_directory(
        &runtime_directory,
        None,
        &command,
        Some(&in_session_context),
    )
    .expect("both sessions answer");

    for discovery_server_thread in [home_discovery_server_thread, other_discovery_server_thread] {
        discovery_server_thread
            .join()
            .expect("stand-in session exits");
    }
    assert_eq!(
        command_route,
        Route::External {
            session_id: other_session_id,
            owner_user_id: None,
            targets: ResolvedTargets {
                session_id: Some(other_session_id),
                tab_id: None,
                pane_id: None,
            },
        }
    );
    let _ = std::fs::remove_dir_all(&runtime_directory);
}

#[test]
fn an_explicit_floating_pane_picks_its_owning_session() {
    let target_session_id = SessionId::new();
    let floating_pane_id = PaneId::new();
    let mut target_session_overview =
        build_session_overview("blue-owl", target_session_id, &[], &[], &[]);
    target_session_overview
        .panes
        .push(build_floating_pane_discovery(
            floating_pane_id,
            target_session_id,
        ));
    let discovered_sessions = build_complete_discovery([
        build_session_overview("amber-fox", SessionId::new(), &[], &[], &[]),
        target_session_overview,
    ]);
    let selected_session_overview = select_target_session(
        None,
        Some(floating_pane_id),
        None,
        None,
        &discovered_sessions,
    )
    .expect("owner found");
    assert_eq!(
        selected_session_overview.session.session_id,
        target_session_id
    );
}
