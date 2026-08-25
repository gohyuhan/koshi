//! Routing tests: which running session an invocation targets, and how the
//! `--session`/`--tab` flags resolve — count rules, explicit targets, and
//! every refusal, checked against hand-built session overviews.

use koshi_core::client::ClientOrigin;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::SystemTime;

use koshi_core::discovery::{
    ClientInfo, PaneInfo, PaneState, SessionInfo, SessionOverview, TabInfo,
};
use koshi_core::event::RejectReason;
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::protocol::{ConnectionToken, IpcRequest, IpcRequestKind, IpcResponse, IpcResult};
use koshi_ipc::transport::{Connection, Listener};
use uuid::uuid;

use super::*;
use crate::cli::CliCommand;

/// One session overview with the given name and one tab/pane/client per
/// listed id, wired to each other in order.
fn overview(
    name: &str,
    session: SessionId,
    tabs: &[(TabId, &str)],
    panes: &[(PaneId, TabId)],
    clients: &[ClientId],
) -> SessionOverview {
    SessionOverview {
        session: SessionInfo {
            id: session,
            name: name.to_string(),
            created_at: SystemTime::UNIX_EPOCH,
            attached_clients: clients.to_vec(),
            pane_count: panes.len(),
        },
        tabs: tabs
            .iter()
            .enumerate()
            .map(|(index, (id, tab_name))| TabInfo {
                id: *id,
                session_id: session,
                name: (*tab_name).to_string(),
                index,
                active_pane: None,
                pane_count: panes.iter().filter(|(_, tab)| tab == id).count(),
            })
            .collect(),
        panes: panes
            .iter()
            .map(|(id, tab_id)| PaneInfo {
                id: *id,
                tab_id: *tab_id,
                session_id: session,
                title: None,
                cwd: None,
                command: None,
                state: PaneState::Running,
                focused_by_clients: Vec::new(),
            })
            .collect(),
        clients: clients
            .iter()
            .map(|id| ClientInfo {
                id: *id,
                session_id: session,
                attached_at: SystemTime::UNIX_EPOCH,
                viewport_size: Size { cols: 80, rows: 24 },
                active_tab: tabs[0].0,
                focused_pane: None,
                lock_state: LockMode::Normal,
                origin: Some(ClientOrigin::Local),
            })
            .collect(),
    }
}

/// A census where every running session answered — the normal case, and the
/// only one in which a "nowhere" or count-rule answer is trustworthy.
fn census<const N: usize>(overviews: [SessionOverview; N]) -> Discovered {
    Discovered {
        sessions: overviews.to_vec(),
        unasked: 0,
    }
}

/// A census missing `unasked` sessions: they are running and listening, but
/// none of them could be asked what they hold.
fn partial<const N: usize>(overviews: [SessionOverview; N], unasked: usize) -> Discovered {
    Discovered {
        sessions: overviews.to_vec(),
        unasked,
    }
}

/// The rejection reason inside a `CommandRejected`, or a panic naming what
/// came back instead.
fn rejection_reason(error: &CliError) -> RejectReason {
    match error {
        CliError::CommandRejected { reason, .. } => *reason,
        other => panic!("expected a rejection, got {other:?}"),
    }
}

/// The hint inside a `CommandRejected`, or a panic naming what came back
/// instead.
fn rejection_help(error: &CliError) -> String {
    match error {
        CliError::CommandRejected {
            help: Some(help), ..
        } => help.clone(),
        other => panic!("expected a rejection carrying a hint, got {other:?}"),
    }
}

/// A private runtime directory of this test's own, emptied first so a
/// leftover endpoint file from an earlier run is never read.
fn test_runtime_dir(tag: &str) -> PathBuf {
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    let dir = base.join(format!("koshi-scope-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create runtime dir");
    dir
}

/// Send one scripted reply back over `connection`.
fn reply(connection: &mut Connection, request_id: u64, result: IpcResult) {
    connection
        .send(&IpcResponse {
            request_id: Some(request_id),
            result,
        })
        .expect("send scripted reply");
}

/// A stand-in session: it advertises an endpoint file in `runtime_dir` and
/// answers exactly one discovery exchange with `overview`.
fn serve_discovery(runtime_dir: &Path, overview: SessionOverview) -> JoinHandle<()> {
    let session_id = overview.session.id;
    let socket = koshi_ipc::endpoint::socket_addr(runtime_dir, session_id);
    let token = ConnectionToken::generate();
    let listener = Listener::bind(&socket).expect("stand-in session binds");
    EndpointFile {
        socket,
        token: token.clone(),
        pid: std::process::id(),
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
        reply(
            &mut discovery,
            hello.request_id,
            IpcResult::Hello {
                protocol_version: koshi_ipc::protocol::PROTOCOL_VERSION,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        );
        reply(
            &mut discovery,
            request.request_id,
            IpcResult::Overview(overview),
        );
    })
}

#[test]
fn sole_running_session_is_the_default() {
    let session = SessionId::new();
    let tab = TabId::new();
    let overviews = census([overview("amber-fox", session, &[(tab, "one")], &[], &[])]);
    let picked = pick_session(None, None, None, None, &overviews).expect("sole session");
    assert_eq!(picked.session.id, session);
}

#[test]
fn no_running_session_reports_no_sessions() {
    let error = pick_session(None, None, None, None, &census([])).expect_err("nothing to target");
    assert!(matches!(error, CliError::NoSessions));
}

#[test]
fn a_sole_answering_session_is_not_the_default_while_another_is_unasked() {
    // One session answered, one is running but could not be asked. Acting on
    // the one that answered would aim the command at a session the user may
    // not have meant, so it refuses instead.
    let overviews = partial([overview("amber-fox", SessionId::new(), &[], &[], &[])], 1);
    let error = pick_session(None, None, None, None, &overviews).expect_err("census incomplete");
    assert!(
        matches!(&error, CliError::IpcUnavailable { detail }
            if detail == "cannot tell which session to target; name one with \
                          --session <name-or-id> (1 running session did not answer)"),
        "got {error:?}"
    );
}

#[test]
fn no_answer_at_all_is_not_reported_as_no_sessions() {
    // The only running session could not be asked: "no koshi session is
    // running" would be false.
    let error = pick_session(None, None, None, None, &partial([], 1)).expect_err("census empty");
    assert!(
        matches!(error, CliError::IpcUnavailable { .. }),
        "got {error:?}"
    );
}

#[test]
fn an_unasked_session_does_not_turn_an_explicit_target_into_not_found() {
    let overviews = partial([overview("amber-fox", SessionId::new(), &[], &[], &[])], 1);
    let error = pick_session(None, Some(PaneId::new()), None, None, &overviews)
        .expect_err("the pane may be in the session that stayed silent");
    assert!(
        matches!(error, CliError::IpcUnavailable { .. }),
        "got {error:?}"
    );

    let name = SessionRef::Name("blue-owl".to_string());
    let error = pick_session(Some(&name), None, None, None, &overviews)
        .expect_err("the name may belong to the session that stayed silent");
    assert!(
        matches!(error, CliError::IpcUnavailable { .. }),
        "got {error:?}"
    );
}

#[test]
fn a_session_name_with_one_match_is_refused_while_a_session_is_unasked() {
    // The unasked session may carry the same name, so "exactly one is named
    // amber-fox" cannot be claimed — the same refusal kill-session gives.
    let overviews = partial([overview("amber-fox", SessionId::new(), &[], &[], &[])], 1);
    let name = SessionRef::Name("amber-fox".to_string());
    let error = pick_session(Some(&name), None, None, None, &overviews)
        .expect_err("the unasked session may share the name");
    assert!(
        matches!(&error, CliError::IpcUnavailable { detail }
            if detail
                == "cannot tell whether `amber-fox` is unique \
                    (1 running session did not answer)"),
        "got {error:?}"
    );
}

#[test]
fn a_tab_name_with_one_match_is_refused_while_a_session_is_unasked() {
    // The unasked session may hold a tab of the same name, so the sole match
    // is not provably the only one.
    let tab = TabId::new();
    let overviews = partial(
        [overview(
            "amber-fox",
            SessionId::new(),
            &[(tab, "logs")],
            &[],
            &[],
        )],
        1,
    );
    let error = pick_session_by_tab(&TabRef::Name("logs".to_string()), &overviews)
        .expect_err("the unasked session may hold a tab of that name");
    assert!(
        matches!(&error, CliError::IpcUnavailable { detail }
            if detail
                == "cannot tell whether tab `logs` is unique \
                    (1 running session did not answer)"),
        "got {error:?}"
    );
}

#[test]
fn two_answering_sessions_stay_ambiguous_even_with_one_unasked() {
    // Naming a session is the fix either way, so the actionable message wins.
    let overviews = partial(
        [
            overview("amber-fox", SessionId::new(), &[], &[], &[]),
            overview("blue-owl", SessionId::new(), &[], &[], &[]),
        ],
        1,
    );
    let error = pick_session(None, None, None, None, &overviews).expect_err("ambiguous");
    assert_eq!(rejection_reason(&error), RejectReason::TargetAmbiguous);
}

#[test]
fn two_running_sessions_demand_the_session_flag() {
    let overviews = census([
        overview("amber-fox", SessionId::new(), &[], &[], &[]),
        overview("blue-owl", SessionId::new(), &[], &[], &[]),
    ]);
    let error = pick_session(None, None, None, None, &overviews).expect_err("ambiguous");
    assert_eq!(rejection_reason(&error), RejectReason::TargetAmbiguous);
}

#[test]
fn session_name_matches_exactly_one() {
    let target = SessionId::new();
    let overviews = census([
        overview("amber-fox", SessionId::new(), &[], &[], &[]),
        overview("blue-owl", target, &[], &[], &[]),
    ]);
    let name = SessionRef::Name("blue-owl".to_string());
    let picked = pick_session(Some(&name), None, None, None, &overviews).expect("unique name");
    assert_eq!(picked.session.id, target);
}

#[test]
fn unknown_session_name_is_not_running() {
    let overviews = census([overview("amber-fox", SessionId::new(), &[], &[], &[])]);
    let name = SessionRef::Name("blue-owl".to_string());
    let error = pick_session(Some(&name), None, None, None, &overviews).expect_err("no match");
    assert!(
        matches!(&error, CliError::SessionNotFound { session } if session == "blue-owl"),
        "got {error:?}"
    );
}

#[test]
fn duplicate_session_name_is_ambiguous() {
    let first = SessionId::from_uuid(uuid!("019bb2ba-0000-7000-8000-000000000001"));
    let second = SessionId::from_uuid(uuid!("019bb2ba-0000-7000-8000-000000000002"));
    let overviews = census([
        overview("amber-fox", first, &[], &[], &[]),
        overview("amber-fox", second, &[], &[], &[]),
    ]);
    let name = SessionRef::Name("amber-fox".to_string());
    let error = pick_session(Some(&name), None, None, None, &overviews).expect_err("two match");
    assert_eq!(rejection_reason(&error), RejectReason::TargetAmbiguous);
    assert_eq!(
        rejection_help(&error),
        "several sessions are named `amber-fox`: \
         session-019bb2ba-0000-7000-8000-000000000001, \
         session-019bb2ba-0000-7000-8000-000000000002; use the session id"
    );
}

#[test]
fn session_id_not_advertised_is_not_running() {
    let overviews = census([overview("amber-fox", SessionId::new(), &[], &[], &[])]);
    let missing = SessionId::new();
    let id = SessionRef::Id(missing);
    let error = pick_session(Some(&id), None, None, None, &overviews).expect_err("not running");
    assert!(
        matches!(&error, CliError::SessionNotFound { session } if *session == missing.to_string()),
        "got {error:?}"
    );
}

#[test]
fn explicit_pane_picks_its_owning_session() {
    let target = SessionId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let overviews = census([
        overview("amber-fox", SessionId::new(), &[], &[], &[]),
        overview("blue-owl", target, &[(tab, "one")], &[(pane, tab)], &[]),
    ]);
    let picked = pick_session(None, Some(pane), None, None, &overviews).expect("owner found");
    assert_eq!(picked.session.id, target);
}

#[test]
fn pane_in_no_session_is_not_found() {
    let overviews = census([overview("amber-fox", SessionId::new(), &[], &[], &[])]);
    let error =
        pick_session(None, Some(PaneId::new()), None, None, &overviews).expect_err("nowhere");
    assert_eq!(rejection_reason(&error), RejectReason::TargetNotFound);
}

#[test]
fn explicit_session_with_a_pane_from_another_session_refuses() {
    let named = SessionId::new();
    let other_tab = TabId::new();
    let foreign_pane = PaneId::new();
    let overviews = census([
        overview("amber-fox", named, &[], &[], &[]),
        overview(
            "blue-owl",
            SessionId::new(),
            &[(other_tab, "one")],
            &[(foreign_pane, other_tab)],
            &[],
        ),
    ]);
    let name = SessionRef::Name("amber-fox".to_string());
    let error = pick_session(Some(&name), Some(foreign_pane), None, None, &overviews)
        .expect_err("mismatch never retargets");
    assert_eq!(rejection_reason(&error), RejectReason::TargetNotFound);
}

#[test]
fn explicit_client_picks_its_session() {
    let target = SessionId::new();
    let tab = TabId::new();
    let client = ClientId::new();
    let overviews = census([
        overview("amber-fox", SessionId::new(), &[], &[], &[]),
        overview("blue-owl", target, &[(tab, "one")], &[], &[client]),
    ]);
    let picked = pick_session(None, None, None, Some(client), &overviews).expect("owner found");
    assert_eq!(picked.session.id, target);
}

#[test]
fn detached_client_is_not_found_anywhere() {
    let tab = TabId::new();
    let overviews = census([overview(
        "amber-fox",
        SessionId::new(),
        &[(tab, "one")],
        &[],
        &[],
    )]);
    let error = pick_session(None, None, None, Some(ClientId::new()), &overviews)
        .expect_err("attached only");
    assert_eq!(rejection_reason(&error), RejectReason::TargetNotFound);
}

#[test]
fn a_client_from_another_session_is_not_retargeted() {
    let named = SessionId::new();
    let other_tab = TabId::new();
    let foreign_client = ClientId::new();
    let overviews = census([
        overview("amber-fox", named, &[], &[], &[]),
        overview(
            "blue-owl",
            SessionId::new(),
            &[(other_tab, "one")],
            &[],
            &[foreign_client],
        ),
    ]);
    let name = SessionRef::Name("amber-fox".to_string());
    let error = pick_session(Some(&name), None, None, Some(foreign_client), &overviews)
        .expect_err("mismatch never retargets");
    assert_eq!(rejection_reason(&error), RejectReason::TargetNotFound);
}

#[test]
fn a_new_tab_client_flag_reaches_the_session_lookup() {
    let holder = SessionId::new();
    let holder_tab = TabId::new();
    let client = ClientId::new();
    let overviews = census([
        overview("amber-fox", SessionId::new(), &[], &[], &[]),
        overview("blue-owl", holder, &[(holder_tab, "one")], &[], &[client]),
    ]);
    let command = CliCommand::NewTab {
        session: None,
        client: Some(client),
    };
    let (session, targets) =
        resolve_targets(&command, &overviews).expect("the client names its session");
    assert_eq!(session, holder);
    assert_eq!(
        targets,
        ResolvedTargets {
            session: Some(holder),
            tab: None,
        }
    );
}

#[test]
fn tab_id_picks_its_owning_session() {
    let target = SessionId::new();
    let tab = TabId::new();
    let overviews = census([
        overview("amber-fox", SessionId::new(), &[], &[], &[]),
        overview("blue-owl", target, &[(tab, "one")], &[], &[]),
    ]);
    let tab_ref = TabRef::Id(tab);
    let picked = pick_session(None, None, Some(&tab_ref), None, &overviews).expect("owner found");
    assert_eq!(picked.session.id, target);
}

#[test]
fn tab_name_owned_by_two_sessions_is_ambiguous() {
    let here = TabId::from_uuid(uuid!("019bb2ba-0001-7000-8000-000000000001"));
    let there = TabId::from_uuid(uuid!("019bb2ba-0001-7000-8000-000000000002"));
    let overviews = census([
        overview("amber-fox", SessionId::new(), &[(here, "logs")], &[], &[]),
        overview("blue-owl", SessionId::new(), &[(there, "logs")], &[], &[]),
    ]);
    let tab_ref = TabRef::Name("logs".to_string());
    let error = pick_session(None, None, Some(&tab_ref), None, &overviews).expect_err("two owners");
    assert_eq!(rejection_reason(&error), RejectReason::TargetAmbiguous);
    assert_eq!(
        rejection_help(&error),
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
    let session = SessionId::new();
    let first = TabId::from_uuid(uuid!("019bb2ba-0002-7000-8000-000000000001"));
    let second = TabId::from_uuid(uuid!("019bb2ba-0002-7000-8000-000000000002"));
    let overviews = census([overview(
        "amber-fox",
        session,
        &[(first, "logs"), (second, "logs")],
        &[],
        &[],
    )]);
    let tab_ref = TabRef::Name("logs".to_string());

    let picked =
        pick_session(None, None, Some(&tab_ref), None, &overviews).expect("one owning session");
    assert_eq!(picked.session.id, session);

    let error = resolve_tab(picked, &tab_ref).expect_err("two tabs share the name");
    assert_eq!(rejection_reason(&error), RejectReason::TargetAmbiguous);
    assert_eq!(
        rejection_help(&error),
        "several tabs are named `logs` in session `amber-fox`: \
         tab-019bb2ba-0002-7000-8000-000000000001, \
         tab-019bb2ba-0002-7000-8000-000000000002; \
         use the tab id"
    );
}

#[test]
fn inspecting_a_duplicated_tab_name_in_one_session_lists_its_ids() {
    let first = TabId::from_uuid(uuid!("019bb2ba-0004-7000-8000-000000000001"));
    let second = TabId::from_uuid(uuid!("019bb2ba-0004-7000-8000-000000000002"));
    let overviews = census([overview(
        "amber-fox",
        SessionId::new(),
        &[(first, "logs"), (second, "logs")],
        &[],
        &[],
    )]);

    let error = tab_by_ref(&overviews, &TabRef::Name("logs".to_string()))
        .expect_err("two tabs share the name");
    assert_eq!(rejection_reason(&error), RejectReason::TargetAmbiguous);
    assert_eq!(
        rejection_help(&error),
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
    let first = TabId::from_uuid(uuid!("019bb2ba-0003-7000-8000-000000000001"));
    let second = TabId::from_uuid(uuid!("019bb2ba-0003-7000-8000-000000000002"));
    let third = TabId::from_uuid(uuid!("019bb2ba-0003-7000-8000-000000000003"));
    let overviews = census([
        overview(
            "amber-fox",
            SessionId::new(),
            &[(first, "logs"), (second, "logs")],
            &[],
            &[],
        ),
        overview("blue-owl", SessionId::new(), &[(third, "logs")], &[], &[]),
    ]);
    let tab_ref = TabRef::Name("logs".to_string());
    let error = pick_session(None, None, Some(&tab_ref), None, &overviews).expect_err("three tabs");
    assert_eq!(rejection_reason(&error), RejectReason::TargetAmbiguous);
    assert_eq!(
        rejection_help(&error),
        "several tabs are named `logs`: \
         tab-019bb2ba-0003-7000-8000-000000000001 in session `amber-fox`, \
         tab-019bb2ba-0003-7000-8000-000000000002 in session `amber-fox`, \
         tab-019bb2ba-0003-7000-8000-000000000003 in session `blue-owl`; \
         use the tab id or --session"
    );
}

#[test]
fn a_tab_name_no_session_holds_is_not_found() {
    let overviews = census([overview(
        "amber-fox",
        SessionId::new(),
        &[(TabId::new(), "work")],
        &[],
        &[],
    )]);
    let tab_ref = TabRef::Name("logs".to_string());
    let error = pick_session(None, None, Some(&tab_ref), None, &overviews).expect_err("nowhere");
    assert_eq!(rejection_reason(&error), RejectReason::TargetNotFound);
    assert_eq!(
        rejection_help(&error),
        "no running session has tab named `logs`"
    );
}

#[test]
fn tab_name_resolves_within_the_session() {
    let session = SessionId::new();
    let logs = TabId::new();
    let overviews = overview(
        "amber-fox",
        session,
        &[(TabId::new(), "work"), (logs, "logs")],
        &[],
        &[],
    );
    let resolved = resolve_tab(&overviews, &TabRef::Name("logs".to_string())).expect("unique name");
    assert_eq!(resolved, logs);
}

#[test]
fn duplicate_tab_name_in_the_session_is_ambiguous() {
    let first = TabId::from_uuid(uuid!("019bb2ba-0003-7000-8000-000000000001"));
    let second = TabId::from_uuid(uuid!("019bb2ba-0003-7000-8000-000000000002"));
    let session = overview(
        "amber-fox",
        SessionId::new(),
        &[(first, "logs"), (second, "logs")],
        &[],
        &[],
    );
    let error = resolve_tab(&session, &TabRef::Name("logs".to_string())).expect_err("two match");
    assert_eq!(rejection_reason(&error), RejectReason::TargetAmbiguous);
    assert_eq!(
        rejection_help(&error),
        "several tabs are named `logs` in session `amber-fox`: \
         tab-019bb2ba-0003-7000-8000-000000000001, \
         tab-019bb2ba-0003-7000-8000-000000000002; use the tab id"
    );
}

#[test]
fn unknown_tab_name_in_the_session_is_not_found() {
    let session = overview(
        "amber-fox",
        SessionId::new(),
        &[(TabId::new(), "work")],
        &[],
        &[],
    );
    let error = resolve_tab(&session, &TabRef::Name("logs".to_string())).expect_err("no match");
    assert_eq!(rejection_reason(&error), RejectReason::TargetNotFound);
    assert_eq!(
        rejection_help(&error),
        "no tab named `logs` in session `amber-fox`"
    );
}

#[test]
fn tab_id_outside_the_session_is_not_found() {
    let session = overview(
        "amber-fox",
        SessionId::new(),
        &[(TabId::new(), "work")],
        &[],
        &[],
    );
    let error = resolve_tab(&session, &TabRef::Id(TabId::new())).expect_err("foreign tab");
    assert_eq!(rejection_reason(&error), RejectReason::TargetNotFound);
}

#[test]
fn in_session_command_with_no_flags_routes_home_without_probing() {
    let context = InSessionContext {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
    };
    let command = CliCommand::ClosePane {
        pane: None,
        force: false,
    };
    let route = route(&command, Some(&context)).expect("home route needs no probe");
    assert_eq!(route, Route::InSession(ResolvedTargets::default()));
}

#[test]
fn in_session_tab_id_routes_home_and_rides_into_the_command() {
    let context = InSessionContext {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
    };
    let tab = TabId::new();
    let command = CliCommand::CloseTab {
        tab: Some(TabRef::Id(tab)),
        session: None,
        force: false,
    };
    // An id needs no lookup: the route resolves nothing and `to_action`
    // carries the id into the command directly.
    let route = route(&command, Some(&context)).expect("id needs no lookup");
    let Route::InSession(targets) = route else {
        panic!("expected the home route, got {route:?}");
    };
    assert_eq!(targets, ResolvedTargets::default());
    let (_, mapped) = command
        .to_action(&targets, koshi_core::geometry::Direction::Right)
        .expect("close-tab is an action");
    assert_eq!(
        mapped,
        koshi_core::command::Command::CloseTab(koshi_core::command::CloseTabArgs {
            tab: Some(tab),
            force: false,
            tree: false,
        })
    );
}

#[test]
fn in_session_move_tab_by_id_routes_home_and_rides_into_the_command() {
    let context = InSessionContext {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
    };
    let tab = TabId::new();
    let command = CliCommand::MoveTab {
        index: 2,
        tab: Some(TabRef::Id(tab)),
    };
    // No runtime directory exists under test, so answering at all proves no
    // session was probed.
    let route = route(&command, Some(&context)).expect("id needs no lookup");
    let Route::InSession(targets) = route else {
        panic!("expected the home route, got {route:?}");
    };
    assert_eq!(targets, ResolvedTargets::default());
    let (_, mapped) = command
        .to_action(&targets, koshi_core::geometry::Direction::Right)
        .expect("move-tab is an action");
    assert_eq!(
        mapped,
        koshi_core::command::Command::MoveTab(koshi_core::command::MoveTabArgs {
            tab: Some(tab),
            index: 2,
        })
    );
}

#[test]
fn in_session_focus_tab_by_id_routes_home_and_rides_into_the_command() {
    let context = InSessionContext {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
    };
    let tab = TabId::new();
    let command = CliCommand::FocusTab {
        index: None,
        tab: Some(TabRef::Id(tab)),
        client: None,
    };
    let route = route(&command, Some(&context)).expect("id needs no lookup");
    let Route::InSession(targets) = route else {
        panic!("expected the home route, got {route:?}");
    };
    assert_eq!(targets, ResolvedTargets::default());
    let (_, mapped) = command
        .to_action(&targets, koshi_core::geometry::Direction::Right)
        .expect("focus-tab is an action");
    assert_eq!(
        mapped,
        koshi_core::command::Command::FocusTab(koshi_core::command::FocusTabArgs {
            target: koshi_core::command::TabTarget::Id(tab),
            client: None,
        })
    );
}

#[test]
fn in_session_new_tab_with_a_client_stays_home() {
    let context = InSessionContext {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
    };
    let client = ClientId::new();
    let command = CliCommand::NewTab {
        session: None,
        client: Some(client),
    };
    let route = route(&command, Some(&context)).expect("a client needs no lookup");
    let Route::InSession(targets) = route else {
        panic!("expected the home route, got {route:?}");
    };
    assert_eq!(targets, ResolvedTargets::default());
    let (_, mapped) = command
        .to_action(&targets, koshi_core::geometry::Direction::Right)
        .expect("new-tab is an action");
    assert_eq!(
        mapped,
        koshi_core::command::Command::NewTab(koshi_core::command::NewTabArgs {
            cwd: None,
            client: Some(client),
        })
    );
}

#[test]
fn a_tab_id_resolves_without_any_census() {
    let tab = TabId::new();
    let resolved = tab_by_ref(&census([]), &TabRef::Id(tab)).expect("an id is its own answer");
    assert_eq!(resolved, tab);
}

#[test]
fn a_tab_name_resolves_across_the_sessions_in_scope() {
    let logs = TabId::new();
    let overviews = census([
        overview(
            "amber-fox",
            SessionId::new(),
            &[(TabId::new(), "work")],
            &[],
            &[],
        ),
        overview("blue-owl", SessionId::new(), &[(logs, "logs")], &[], &[]),
    ]);
    let resolved =
        tab_by_ref(&overviews, &TabRef::Name("logs".to_string())).expect("one session holds it");
    assert_eq!(resolved, logs);
}

#[test]
fn a_session_id_scopes_to_that_session_alone() {
    let runtime_dir = test_runtime_dir("by-id");
    let target = overview("amber-fox", SessionId::new(), &[], &[], &[]);
    let target_id = target.session.id;
    let server = serve_discovery(&runtime_dir, target.clone());
    // A second advertised session whose endpoint file cannot be read: asking
    // every session would count it unasked, so `unasked: 0` here is proof
    // only the named session was asked.
    std::fs::write(
        EndpointFile::path(&runtime_dir, SessionId::new()),
        b"not an endpoint file",
    )
    .expect("second endpoint file written");

    let found =
        scope_sessions(&runtime_dir, Some(&SessionRef::Id(target_id))).expect("id scope answers");

    assert_eq!(found.sessions, vec![target]);
    assert_eq!(found.unasked, 0);
    server.join().expect("stand-in session exits");
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

/// One row a remote server offers, named `name` at `id`.
fn remote_row(id: SessionId, name: &str) -> RemoteSessionRow {
    RemoteSessionRow {
        id,
        name: name.to_string(),
    }
}

#[test]
fn an_explicit_session_id_asks_that_one_remote_session_and_no_other() {
    // An explicit id keeps one row out of three.
    let wanted = SessionId::new();
    let rows = vec![
        remote_row(SessionId::new(), "S-first"),
        remote_row(wanted, "S-wanted"),
        remote_row(SessionId::new(), "S-third"),
    ];

    let asked = rows_to_ask(Some(&SessionRef::Id(wanted)), rows);

    assert_eq!(asked.len(), 1, "one dial, not three");
    assert_eq!(asked[0].id, wanted);
}

#[test]
fn a_session_name_asks_every_remote_session() {
    // A name keeps every row.
    let rows = vec![
        remote_row(SessionId::new(), "S-first"),
        remote_row(SessionId::new(), "S-second"),
    ];

    let asked = rows_to_ask(Some(&SessionRef::Name("S-first".to_string())), rows.clone());

    assert_eq!(asked, rows, "a name needs the whole picture");
}

#[test]
fn no_session_flag_asks_every_remote_session() {
    // No flag keeps every row.
    let rows = vec![
        remote_row(SessionId::new(), "S-first"),
        remote_row(SessionId::new(), "S-second"),
    ];

    assert_eq!(rows_to_ask(None, rows.clone()), rows);
}

#[test]
fn an_explicit_id_no_remote_session_carries_asks_nothing() {
    // An id no row carries keeps no rows.
    let rows = vec![remote_row(SessionId::new(), "S-first")];

    assert!(rows_to_ask(Some(&SessionRef::Id(SessionId::new())), rows).is_empty());
}
