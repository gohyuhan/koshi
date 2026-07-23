//! Routing tests: which running session an invocation targets, and how the
//! `--session`/`--tab` flags resolve — count rules, explicit targets, and
//! every refusal, checked against hand-built session overviews.

use std::time::SystemTime;

use koshi_core::discovery::{
    ClientInfo, PaneInfo, PaneState, SessionInfo, SessionOverview, TabInfo,
};
use koshi_core::event::RejectReason;
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;

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
            })
            .collect(),
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

#[test]
fn sole_running_session_is_the_default() {
    let session = SessionId::new();
    let tab = TabId::new();
    let overviews = [overview("amber-fox", session, &[(tab, "one")], &[], &[])];
    let picked = pick_session(None, None, None, None, &overviews).expect("sole session");
    assert_eq!(picked.session.id, session);
}

#[test]
fn no_running_session_reports_no_sessions() {
    let error = pick_session(None, None, None, None, &[]).expect_err("nothing to target");
    assert!(matches!(error, CliError::NoSessions));
}

#[test]
fn two_running_sessions_demand_the_session_flag() {
    let overviews = [
        overview("amber-fox", SessionId::new(), &[], &[], &[]),
        overview("blue-owl", SessionId::new(), &[], &[], &[]),
    ];
    let error = pick_session(None, None, None, None, &overviews).expect_err("ambiguous");
    assert_eq!(rejection_reason(&error), RejectReason::TargetAmbiguous);
}

#[test]
fn session_name_matches_exactly_one() {
    let target = SessionId::new();
    let overviews = [
        overview("amber-fox", SessionId::new(), &[], &[], &[]),
        overview("blue-owl", target, &[], &[], &[]),
    ];
    let name = SessionRef::Name("blue-owl".to_string());
    let picked = pick_session(Some(&name), None, None, None, &overviews).expect("unique name");
    assert_eq!(picked.session.id, target);
}

#[test]
fn unknown_session_name_is_not_running() {
    let overviews = [overview("amber-fox", SessionId::new(), &[], &[], &[])];
    let name = SessionRef::Name("blue-owl".to_string());
    let error = pick_session(Some(&name), None, None, None, &overviews).expect_err("no match");
    assert!(
        matches!(&error, CliError::SessionNotFound { session } if session == "blue-owl"),
        "got {error:?}"
    );
}

#[test]
fn duplicate_session_name_is_ambiguous() {
    let overviews = [
        overview("amber-fox", SessionId::new(), &[], &[], &[]),
        overview("amber-fox", SessionId::new(), &[], &[], &[]),
    ];
    let name = SessionRef::Name("amber-fox".to_string());
    let error = pick_session(Some(&name), None, None, None, &overviews).expect_err("two match");
    assert_eq!(rejection_reason(&error), RejectReason::TargetAmbiguous);
}

#[test]
fn session_id_not_advertised_is_not_running() {
    let overviews = [overview("amber-fox", SessionId::new(), &[], &[], &[])];
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
    let overviews = [
        overview("amber-fox", SessionId::new(), &[], &[], &[]),
        overview("blue-owl", target, &[(tab, "one")], &[(pane, tab)], &[]),
    ];
    let picked = pick_session(None, Some(pane), None, None, &overviews).expect("owner found");
    assert_eq!(picked.session.id, target);
}

#[test]
fn pane_in_no_session_is_not_found() {
    let overviews = [overview("amber-fox", SessionId::new(), &[], &[], &[])];
    let error =
        pick_session(None, Some(PaneId::new()), None, None, &overviews).expect_err("nowhere");
    assert_eq!(rejection_reason(&error), RejectReason::TargetNotFound);
}

#[test]
fn explicit_session_with_a_pane_from_another_session_refuses() {
    let named = SessionId::new();
    let other_tab = TabId::new();
    let foreign_pane = PaneId::new();
    let overviews = [
        overview("amber-fox", named, &[], &[], &[]),
        overview(
            "blue-owl",
            SessionId::new(),
            &[(other_tab, "one")],
            &[(foreign_pane, other_tab)],
            &[],
        ),
    ];
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
    let overviews = [
        overview("amber-fox", SessionId::new(), &[], &[], &[]),
        overview("blue-owl", target, &[(tab, "one")], &[], &[client]),
    ];
    let picked = pick_session(None, None, None, Some(client), &overviews).expect("owner found");
    assert_eq!(picked.session.id, target);
}

#[test]
fn detached_client_is_not_found_anywhere() {
    let tab = TabId::new();
    let overviews = [overview(
        "amber-fox",
        SessionId::new(),
        &[(tab, "one")],
        &[],
        &[],
    )];
    let error = pick_session(None, None, None, Some(ClientId::new()), &overviews)
        .expect_err("attached only");
    assert_eq!(rejection_reason(&error), RejectReason::TargetNotFound);
}

#[test]
fn tab_id_picks_its_owning_session() {
    let target = SessionId::new();
    let tab = TabId::new();
    let overviews = [
        overview("amber-fox", SessionId::new(), &[], &[], &[]),
        overview("blue-owl", target, &[(tab, "one")], &[], &[]),
    ];
    let tab_ref = TabRef::Id(tab);
    let picked = pick_session(None, None, Some(&tab_ref), None, &overviews).expect("owner found");
    assert_eq!(picked.session.id, target);
}

#[test]
fn tab_name_owned_by_two_sessions_is_ambiguous() {
    let overviews = [
        overview(
            "amber-fox",
            SessionId::new(),
            &[(TabId::new(), "logs")],
            &[],
            &[],
        ),
        overview(
            "blue-owl",
            SessionId::new(),
            &[(TabId::new(), "logs")],
            &[],
            &[],
        ),
    ];
    let tab_ref = TabRef::Name("logs".to_string());
    let error = pick_session(None, None, Some(&tab_ref), None, &overviews).expect_err("two owners");
    assert_eq!(rejection_reason(&error), RejectReason::TargetAmbiguous);
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
    let session = overview(
        "amber-fox",
        SessionId::new(),
        &[(TabId::new(), "logs"), (TabId::new(), "logs")],
        &[],
        &[],
    );
    let error = resolve_tab(&session, &TabRef::Name("logs".to_string())).expect_err("two match");
    assert_eq!(rejection_reason(&error), RejectReason::TargetAmbiguous);
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
        socket: None,
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
        socket: None,
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
    let (_, mapped) = command.to_action(&targets).expect("close-tab is an action");
    assert_eq!(
        mapped,
        koshi_core::command::Command::CloseTab(koshi_core::command::CloseTabArgs {
            tab: Some(tab),
            force: false,
            tree: false,
        })
    );
}
