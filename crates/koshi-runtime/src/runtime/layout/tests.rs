//! Tests for building the layout dump from live session state: which tabs
//! are described, what each viewing client solves them to, what each client
//! draws for each floating pane, and what comes back when nothing can be
//! solved at all.

use std::num::NonZeroU16;
use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use crate::runtime::pty_inbox::InboxSink;
use koshi_core::geometry::{
    FloatingPaneDimension, FloatingPanePosition, FloatingPaneSize, PaneArea, Point, Rect, Size,
    DEFAULT_FLOATING_PANE_SIZE,
};
use koshi_core::ids::{ClientId, PaneId, SessionId};
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::StackHeader;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::state::PaneRecord;
use koshi_pty::backend::state::PtyBackend;
use koshi_session::client::{Client, ClientOrigin};
use koshi_session::session::state::Session;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::runtime::event::RuntimeEvent;

use super::*;
use crate::runtime::command::tests::{attach_client_record, build_horizontal_split, build_tab};
use crate::runtime::discovery::tests::build_empty_session;
use koshi_test_support::fixtures::build_test_uuid_with_suffix;

/// The terminal size every client below reports.
const TEST_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// The size a tab solves against for a client at [`TEST_VIEWPORT_SIZE`]: the terminal
/// minus the two chrome rows.
const TAB_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 22,
};

/// A bare runtime with stub services and no sessions, and the sender
/// that queues events on its inbox.
fn build_test_runtime() -> (Server, mpsc::Sender<RuntimeEvent>) {
    let (event_sender, event_receiver) = mpsc::channel();
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(event_sender.clone()),
    )));
    let server = Server::from_runtime_parts(pty_backend, event_receiver);
    (server, event_sender)
}

/// Add a tab named `tab_name` at bar position `tab_index`, showing `root_pane`.
fn add_session_tab(
    session: &mut Session,
    tab_id: TabId,
    tab_name: &str,
    tab_index: usize,
    root_pane: PaneId,
) {
    session.tabs.insert(
        tab_id,
        build_tab(tab_id, tab_name, tab_index, LayoutNode::Pane(root_pane)),
    );
}

/// Attach `client_id` viewing `tab_id` at `viewport_size` reporting `pane_area`,
/// focused on `focused_pane_id`, and zoomed on `zoomed_pane_id`, then solve
/// `tab_id` at its tab size ([`Session::update_tab_solve`]).
fn attach_test_client(
    session: &mut Session,
    client_id: ClientId,
    tab_id: TabId,
    viewport_size: Size,
    pane_area: Option<PaneArea>,
    focused_pane_id: Option<PaneId>,
    zoomed_pane_id: Option<PaneId>,
) {
    let mut attached_client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::UNIX_EPOCH,
        viewport_size,
        pane_area,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    if let Some(focused_pane_id) = focused_pane_id {
        attached_client.update_focused_pane(tab_id, focused_pane_id);
    }
    if let Some(zoomed_pane_id) = zoomed_pane_id {
        attached_client.zoom_pane(tab_id, zoomed_pane_id);
    }
    attach_client_record(session, attached_client);
}

/// A runtime holding exactly `session`.
fn build_test_runtime_with_session(session: Session) -> Server {
    let (mut server, _event_sender) = build_test_runtime();
    server.session_by_id.insert(session.session_id, session);
    server
}

#[test]
fn no_session_yields_no_layout() {
    let (server, _inbox_sender) = build_test_runtime();

    assert_eq!(server.build_session_layout(None), None);
}

#[test]
fn one_tab_one_client_reports_the_tree_the_solve_and_the_focus() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    attach_test_client(
        &mut session,
        client_id,
        tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(pane_id),
        None,
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    assert_eq!(
        session_layout,
        SessionLayout {
            session_id,
            session_name: "quiet-lake".to_string(),
            tabs: vec![TabLayout {
                tab_id,
                tab_name: "editor".to_string(),
                tab_index: 0,
                layout_tree: LayoutNode::Pane(pane_id),
                solved_tabs: vec![SolvedTab {
                    client_id,
                    viewport_size: TAB_VIEWPORT_SIZE,
                    layout_mode: LayoutMode::Tiled,
                    pane_rects: vec![SolvedPane {
                        pane_id,
                        outer_rect: Rect::from_size_at_origin(TAB_VIEWPORT_SIZE),
                    }],
                    suppressed_pane_ids: Vec::new(),
                    is_every_pane_suppressed: false,
                    stack_headers: Vec::new(),
                }],
            }],
            floating_panes: Vec::new(),
            clients: vec![ClientFocus {
                client_id,
                active_tab_id: tab_id,
                focused_pane_id: Some(pane_id),
            }],
        },
    );
}

#[test]
fn a_client_that_has_focused_nothing_reports_no_focused_pane() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    attach_test_client(
        &mut session,
        client_id,
        tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        None,
        None,
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    assert_eq!(
        session_layout.clients,
        vec![ClientFocus {
            client_id,
            active_tab_id: tab_id,
            focused_pane_id: None,
        }],
    );
}

#[test]
fn a_session_with_no_tabs_and_no_clients_reports_only_its_own_name() {
    let session_id = SessionId::new();
    let server = build_test_runtime_with_session(build_empty_session(session_id));

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    assert_eq!(
        session_layout,
        SessionLayout {
            session_id,
            session_name: "quiet-lake".to_string(),
            tabs: Vec::new(),
            floating_panes: Vec::new(),
            clients: Vec::new(),
        },
    );
}

#[test]
fn a_tab_whose_only_viewer_is_starving_lists_no_solved_layout() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    attach_test_client(
        &mut session,
        client_id,
        tab_id,
        TEST_VIEWPORT_SIZE,
        Some(PaneArea::Starving),
        Some(pane_id),
        None,
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    let described_tab_layout = session_layout
        .tabs
        .iter()
        .find(|tab_summary| tab_summary.tab_id == tab_id)
        .expect("the tab is still described");
    assert_eq!(described_tab_layout.layout_tree, LayoutNode::Pane(pane_id));
    assert_eq!(described_tab_layout.solved_tabs, Vec::new());
}

#[test]
fn a_reported_pane_area_is_the_size_the_tab_solves_against() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    // A reported area replaces the terminal-minus-chrome default outright: the
    // two chrome rows are not taken off it again.
    attach_test_client(
        &mut session,
        client_id,
        tab_id,
        TEST_VIEWPORT_SIZE,
        Some(PaneArea::Reported(Size {
            column_count: 40,
            row_count: 10,
        })),
        Some(pane_id),
        None,
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    let solved_tab = &session_layout.tabs[0].solved_tabs[0];
    assert_eq!(
        solved_tab.viewport_size,
        Size {
            column_count: 40,
            row_count: 10
        }
    );
    assert_eq!(
        solved_tab.pane_rects,
        vec![SolvedPane {
            pane_id,
            outer_rect: Rect::from_size_at_origin(Size {
                column_count: 40,
                row_count: 10
            }),
        }],
    );
    assert_eq!(solved_tab.suppressed_pane_ids, Vec::new());
    assert!(!solved_tab.is_every_pane_suppressed);
}

#[test]
fn a_reported_pane_area_larger_than_the_terminal_is_clamped_to_it() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    attach_test_client(
        &mut session,
        client_id,
        tab_id,
        TEST_VIEWPORT_SIZE,
        Some(PaneArea::Reported(Size {
            column_count: 200,
            row_count: 100,
        })),
        Some(pane_id),
        None,
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    // Clamped to the 80x24 terminal on each axis, chrome rows included.
    let solved_tab = &session_layout.tabs[0].solved_tabs[0];
    assert_eq!(solved_tab.viewport_size, TEST_VIEWPORT_SIZE);
    assert_eq!(
        solved_tab.pane_rects,
        vec![SolvedPane {
            pane_id,
            outer_rect: Rect::from_size_at_origin(TEST_VIEWPORT_SIZE),
        }],
    );
    assert_eq!(solved_tab.suppressed_pane_ids, Vec::new());
    assert!(!solved_tab.is_every_pane_suppressed);
}

#[test]
fn a_starving_viewer_still_gets_a_solve_when_another_viewer_reports_a_size() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut client_ids = [ClientId::new(), ClientId::new()];
    client_ids.sort();
    let [starving_client_id, reporting_client_id] = client_ids;
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    attach_test_client(
        &mut session,
        starving_client_id,
        tab_id,
        TEST_VIEWPORT_SIZE,
        Some(PaneArea::Starving),
        Some(pane_id),
        None,
    );
    attach_test_client(
        &mut session,
        reporting_client_id,
        tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(pane_id),
        None,
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    // Clients are listed in id order, and the ids were sorted above. The
    // starving client contributes no size to the tab, and is still solved
    // against the size the other viewer set.
    let solved_tabs = &session_layout.tabs[0].solved_tabs;
    assert_eq!(solved_tabs.len(), 2);
    assert_eq!(solved_tabs[0].client_id, starving_client_id);
    assert_eq!(solved_tabs[0].viewport_size, TAB_VIEWPORT_SIZE);
    assert_eq!(
        solved_tabs[0].pane_rects,
        vec![SolvedPane {
            pane_id,
            outer_rect: Rect::from_size_at_origin(TAB_VIEWPORT_SIZE),
        }],
    );
    assert_eq!(solved_tabs[1].client_id, reporting_client_id);
    assert_eq!(solved_tabs[1].viewport_size, TAB_VIEWPORT_SIZE);
    assert_eq!(
        solved_tabs[1].pane_rects,
        vec![SolvedPane {
            pane_id,
            outer_rect: Rect::from_size_at_origin(TAB_VIEWPORT_SIZE),
        }],
    );
}

#[test]
fn a_tab_no_client_views_carries_its_tree_and_no_solve() {
    let session_id = SessionId::new();
    let watched_tab_id = TabId::new();
    let unwatched_tab_id = TabId::new();
    let watched_pane_id = PaneId::new();
    let unwatched_pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, watched_tab_id, "editor", 0, watched_pane_id);
    add_session_tab(&mut session, unwatched_tab_id, "logs", 1, unwatched_pane_id);
    attach_test_client(
        &mut session,
        client_id,
        watched_tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(watched_pane_id),
        None,
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    let described_tab_layout = session_layout
        .tabs
        .iter()
        .find(|tab_summary| tab_summary.tab_id == unwatched_tab_id)
        .expect("the unwatched tab is still described");
    assert_eq!(
        described_tab_layout.layout_tree,
        LayoutNode::Pane(unwatched_pane_id)
    );
    assert_eq!(described_tab_layout.solved_tabs, Vec::new());

    let watched_tab_layout = session_layout
        .tabs
        .iter()
        .find(|tab_summary| tab_summary.tab_id == watched_tab_id)
        .expect("the watched tab is described");
    assert_eq!(watched_tab_layout.solved_tabs.len(), 1);
    assert_eq!(watched_tab_layout.solved_tabs[0].client_id, client_id);
}

#[test]
fn a_client_viewing_another_tab_is_left_out_of_this_tab_solve() {
    // Two tabs, each with its own viewer. Every tab has a viewer, so no tab is
    // skipped for want of a size; each tab must still solve for its own client
    // alone.
    let session_id = SessionId::new();
    let editor_tab_id = TabId::new();
    let logs_tab_id = TabId::new();
    let editor_pane_id = PaneId::new();
    let logs_pane_id = PaneId::new();
    let editor_client_id = ClientId::new();
    let logs_client_id = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, editor_tab_id, "editor", 0, editor_pane_id);
    add_session_tab(&mut session, logs_tab_id, "logs", 1, logs_pane_id);
    attach_test_client(
        &mut session,
        editor_client_id,
        editor_tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(editor_pane_id),
        None,
    );
    attach_test_client(
        &mut session,
        logs_client_id,
        logs_tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(logs_pane_id),
        None,
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    let editor_solving_client_ids: Vec<ClientId> = session_layout
        .tabs
        .iter()
        .find(|tab_summary| tab_summary.tab_id == editor_tab_id)
        .expect("the editor tab is described")
        .solved_tabs
        .iter()
        .map(|solved_tab| solved_tab.client_id)
        .collect();
    assert_eq!(editor_solving_client_ids, vec![editor_client_id]);

    let logs_solving_client_ids: Vec<ClientId> = session_layout
        .tabs
        .iter()
        .find(|tab_summary| tab_summary.tab_id == logs_tab_id)
        .expect("the logs tab is described")
        .solved_tabs
        .iter()
        .map(|solved_tab| solved_tab.client_id)
        .collect();
    assert_eq!(logs_solving_client_ids, vec![logs_client_id]);
}

#[test]
fn tabs_come_back_in_tab_bar_order_not_in_id_order() {
    // The tab map is keyed by id, so the lower id is visited first; the tab
    // bar puts it second.
    let session_id = SessionId::new();
    let lower_tab_id = TabId::from_uuid(build_test_uuid_with_suffix(1));
    let higher_tab_id = TabId::from_uuid(build_test_uuid_with_suffix(2));
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, lower_tab_id, "second", 1, PaneId::new());
    add_session_tab(&mut session, higher_tab_id, "first", 0, PaneId::new());
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    let tab_order: Vec<(TabId, usize)> = session_layout
        .tabs
        .iter()
        .map(|tab_summary| (tab_summary.tab_id, tab_summary.tab_index))
        .collect();
    assert_eq!(tab_order, vec![(higher_tab_id, 0), (lower_tab_id, 1)]);
}

#[test]
fn two_tabs_at_the_same_bar_index_come_back_in_id_order() {
    // Sorting by bar position keeps the order the tab map handed over, which
    // is id order, so a shared index is broken by id.
    let session_id = SessionId::new();
    let lower_tab_id = TabId::from_uuid(build_test_uuid_with_suffix(1));
    let higher_tab_id = TabId::from_uuid(build_test_uuid_with_suffix(2));
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, lower_tab_id, "editor", 0, PaneId::new());
    add_session_tab(&mut session, higher_tab_id, "logs", 0, PaneId::new());
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    let tab_order: Vec<(TabId, usize)> = session_layout
        .tabs
        .iter()
        .map(|tab_summary| (tab_summary.tab_id, tab_summary.tab_index))
        .collect();
    assert_eq!(tab_order, vec![(lower_tab_id, 0), (higher_tab_id, 0)]);
}

#[test]
fn narrowing_to_one_tab_describes_that_tab_alone_and_still_names_every_client() {
    let session_id = SessionId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, first_tab_id, "editor", 0, first_pane_id);
    add_session_tab(&mut session, second_tab_id, "logs", 1, second_pane_id);
    attach_test_client(
        &mut session,
        client_id,
        first_tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(first_pane_id),
        None,
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(Some(second_tab_id))
        .expect("one session is running");

    assert_eq!(
        session_layout.tabs,
        vec![TabLayout {
            tab_id: second_tab_id,
            tab_name: "logs".to_string(),
            tab_index: 1,
            layout_tree: LayoutNode::Pane(second_pane_id),
            solved_tabs: Vec::new(),
        }],
    );
    assert_eq!(
        session_layout.clients,
        vec![ClientFocus {
            client_id,
            active_tab_id: first_tab_id,
            focused_pane_id: Some(first_pane_id),
        }],
    );
}

#[test]
fn narrowing_to_the_tab_its_own_viewer_watches_keeps_that_tabs_solve() {
    let session_id = SessionId::new();
    let editor_tab_id = TabId::new();
    let logs_tab_id = TabId::new();
    let editor_pane_id = PaneId::new();
    let logs_pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, editor_tab_id, "editor", 0, editor_pane_id);
    add_session_tab(&mut session, logs_tab_id, "logs", 1, logs_pane_id);
    attach_test_client(
        &mut session,
        client_id,
        editor_tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(editor_pane_id),
        None,
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(Some(editor_tab_id))
        .expect("one session is running");

    assert_eq!(
        session_layout.tabs,
        vec![TabLayout {
            tab_id: editor_tab_id,
            tab_name: "editor".to_string(),
            tab_index: 0,
            layout_tree: LayoutNode::Pane(editor_pane_id),
            solved_tabs: vec![SolvedTab {
                client_id,
                viewport_size: TAB_VIEWPORT_SIZE,
                layout_mode: LayoutMode::Tiled,
                pane_rects: vec![SolvedPane {
                    pane_id: editor_pane_id,
                    outer_rect: Rect::from_size_at_origin(TAB_VIEWPORT_SIZE),
                }],
                suppressed_pane_ids: Vec::new(),
                is_every_pane_suppressed: false,
                stack_headers: Vec::new(),
            }],
        }],
    );
}

#[test]
fn narrowing_to_a_tab_that_does_not_exist_describes_no_tab_at_all() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    attach_test_client(
        &mut session,
        client_id,
        tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(pane_id),
        None,
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(Some(TabId::new()))
        .expect("one session is running");

    assert_eq!(
        session_layout,
        SessionLayout {
            session_id,
            session_name: "quiet-lake".to_string(),
            tabs: Vec::new(),
            floating_panes: Vec::new(),
            clients: vec![ClientFocus {
                client_id,
                active_tab_id: tab_id,
                focused_pane_id: Some(pane_id),
            }],
        },
    );
}

#[test]
fn a_zoomed_client_reports_fullscreen_and_gives_the_whole_tab_to_one_pane() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, left_pane_id);
    let _ = session.update_tab_layout(tab_id, build_horizontal_split(left_pane_id, right_pane_id));
    attach_test_client(
        &mut session,
        client_id,
        tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(right_pane_id),
        Some(right_pane_id),
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    let solved_tab = &session_layout.tabs[0].solved_tabs[0];
    assert_eq!(
        solved_tab.layout_mode,
        LayoutMode::Fullscreen {
            focused_pane_id: right_pane_id
        }
    );
    assert_eq!(
        solved_tab.pane_rects,
        vec![
            SolvedPane {
                pane_id: left_pane_id,
                outer_rect: Rect::build_empty_at_origin(),
            },
            SolvedPane {
                pane_id: right_pane_id,
                outer_rect: Rect::from_size_at_origin(TAB_VIEWPORT_SIZE),
            },
        ],
    );
    assert_eq!(solved_tab.suppressed_pane_ids, Vec::new());
    assert!(!solved_tab.is_every_pane_suppressed);
    assert_eq!(solved_tab.stack_headers, Vec::new());
}

#[test]
fn two_clients_on_one_tab_each_get_their_own_solve_of_the_same_tree() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let mut client_ids = [ClientId::new(), ClientId::new()];
    client_ids.sort();
    let [tiled_client_id, zoomed_client_id] = client_ids;
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, left_pane_id);
    let _ = session.update_tab_layout(tab_id, build_horizontal_split(left_pane_id, right_pane_id));
    attach_test_client(
        &mut session,
        tiled_client_id,
        tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(left_pane_id),
        None,
    );
    attach_test_client(
        &mut session,
        zoomed_client_id,
        tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(left_pane_id),
        Some(left_pane_id),
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    // Clients are listed in id order, and the ids were sorted above.
    let solved_tabs = &session_layout.tabs[0].solved_tabs;
    assert_eq!(solved_tabs.len(), 2);
    assert_eq!(solved_tabs[0].client_id, tiled_client_id);
    assert_eq!(solved_tabs[0].layout_mode, LayoutMode::Tiled);
    assert_eq!(
        solved_tabs[0].pane_rects,
        vec![
            SolvedPane {
                pane_id: left_pane_id,
                outer_rect: Rect::from_size_at_origin(Size {
                    column_count: 40,
                    row_count: 22
                }),
            },
            SolvedPane {
                pane_id: right_pane_id,
                outer_rect: Rect::from_origin_and_size(
                    Point { column: 40, row: 0 },
                    Size {
                        column_count: 40,
                        row_count: 22
                    },
                ),
            },
        ],
    );
    assert_eq!(solved_tabs[1].client_id, zoomed_client_id);
    assert_eq!(
        solved_tabs[1].layout_mode,
        LayoutMode::Fullscreen {
            focused_pane_id: left_pane_id
        }
    );
    assert_eq!(
        solved_tabs[1].pane_rects,
        vec![
            SolvedPane {
                pane_id: left_pane_id,
                outer_rect: Rect::from_size_at_origin(TAB_VIEWPORT_SIZE),
            },
            SolvedPane {
                pane_id: right_pane_id,
                outer_rect: Rect::build_empty_at_origin(),
            },
        ],
    );
}

#[test]
fn two_clients_of_different_sizes_on_one_tab_both_solve_against_the_smaller() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut client_ids = [ClientId::new(), ClientId::new()];
    client_ids.sort();
    let [small_client_id, big_client_id] = client_ids;
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    attach_test_client(
        &mut session,
        small_client_id,
        tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(pane_id),
        None,
    );
    attach_test_client(
        &mut session,
        big_client_id,
        tab_id,
        Size {
            column_count: 120,
            row_count: 40,
        },
        None,
        Some(pane_id),
        None,
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    // Clients are listed in id order, and the ids were sorted above. The tab
    // solves once, for the smallest viewer on each axis, so the 120x40 client
    // gets 80x22 too.
    let solved_tabs = &session_layout.tabs[0].solved_tabs;
    assert_eq!(solved_tabs.len(), 2);
    assert_eq!(solved_tabs[0].client_id, small_client_id);
    assert_eq!(solved_tabs[0].viewport_size, TAB_VIEWPORT_SIZE);
    assert_eq!(
        solved_tabs[0].pane_rects,
        vec![SolvedPane {
            pane_id,
            outer_rect: Rect::from_size_at_origin(TAB_VIEWPORT_SIZE),
        }],
    );
    assert_eq!(solved_tabs[1].client_id, big_client_id);
    assert_eq!(solved_tabs[1].viewport_size, TAB_VIEWPORT_SIZE);
    assert_eq!(
        solved_tabs[1].pane_rects,
        vec![SolvedPane {
            pane_id,
            outer_rect: Rect::from_size_at_origin(TAB_VIEWPORT_SIZE),
        }],
    );
}

#[test]
fn a_collapsed_stack_member_reports_its_header_strip() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let shown_pane_id = PaneId::new();
    let collapsed_pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, shown_pane_id);
    let _ = session.update_tab_layout(
        tab_id,
        LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![shown_pane_id, collapsed_pane_id],
            0,
        )),
    );
    attach_test_client(
        &mut session,
        client_id,
        tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(shown_pane_id),
        None,
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    let solved_tab = &session_layout.tabs[0].solved_tabs[0];
    // The header takes the last row; the active member keeps the other 21.
    assert_eq!(
        solved_tab.pane_rects,
        vec![
            SolvedPane {
                pane_id: shown_pane_id,
                outer_rect: Rect::from_size_at_origin(Size {
                    column_count: 80,
                    row_count: 21
                }),
            },
            SolvedPane {
                pane_id: collapsed_pane_id,
                outer_rect: Rect::from_origin_and_size(
                    Point { column: 0, row: 21 },
                    Size {
                        column_count: 80,
                        row_count: 1
                    },
                ),
            },
        ],
    );
    assert_eq!(
        solved_tab.stack_headers,
        vec![StackHeader {
            pane_id: collapsed_pane_id,
            header_rect: Rect::from_origin_and_size(
                Point { column: 0, row: 21 },
                Size {
                    column_count: 80,
                    row_count: 1
                },
            ),
            member_index: 1,
            member_count: 2,
        }],
    );
    assert_eq!(solved_tab.suppressed_pane_ids, Vec::new());
    assert!(!solved_tab.is_every_pane_suppressed);
}

#[test]
fn a_terminal_too_small_for_one_pane_suppresses_every_pane() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    // Three columns leave no room for a bordered pane, which needs four.
    attach_test_client(
        &mut session,
        client_id,
        tab_id,
        Size {
            column_count: 3,
            row_count: 5,
        },
        None,
        Some(pane_id),
        None,
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    let solved_tab = &session_layout.tabs[0].solved_tabs[0];
    assert_eq!(
        solved_tab.viewport_size,
        Size {
            column_count: 3,
            row_count: 3
        }
    );
    assert_eq!(
        solved_tab.pane_rects,
        vec![SolvedPane {
            pane_id,
            outer_rect: Rect::build_empty_at_origin(),
        }],
    );
    assert_eq!(solved_tab.suppressed_pane_ids, vec![pane_id]);
    assert!(solved_tab.is_every_pane_suppressed);
}

#[test]
fn a_pane_that_no_longer_fits_beside_its_neighbor_is_the_only_one_suppressed() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, left_pane_id);
    let _ = session.update_tab_layout(tab_id, build_horizontal_split(left_pane_id, right_pane_id));
    // Six columns hold one bordered pane of four, never two.
    attach_test_client(
        &mut session,
        client_id,
        tab_id,
        Size {
            column_count: 6,
            row_count: 6,
        },
        None,
        Some(left_pane_id),
        None,
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    let solved_tab = &session_layout.tabs[0].solved_tabs[0];
    assert_eq!(
        solved_tab.pane_rects,
        vec![
            SolvedPane {
                pane_id: left_pane_id,
                outer_rect: Rect::from_size_at_origin(Size {
                    column_count: 6,
                    row_count: 4
                }),
            },
            SolvedPane {
                pane_id: right_pane_id,
                outer_rect: Rect::build_empty_at_origin(),
            },
        ],
    );
    assert_eq!(solved_tab.suppressed_pane_ids, vec![right_pane_id]);
    assert!(!solved_tab.is_every_pane_suppressed);
}

/// The client that draws both floating panes of [`build_floating_pane_session`].
fn build_drawing_client_id() -> ClientId {
    ClientId::from_uuid(build_test_uuid_with_suffix(11))
}

/// The client that moved and then minimized the `60%` floating pane.
fn build_minimizing_client_id() -> ClientId {
    ClientId::from_uuid(build_test_uuid_with_suffix(12))
}

/// The client that reported no room to draw a pane.
fn build_starving_client_id() -> ClientId {
    ClientId::from_uuid(build_test_uuid_with_suffix(13))
}

/// The floating pane that asks for `60%` by `60%`.
fn build_percent_floating_pane_id() -> PaneId {
    PaneId::from_uuid(build_test_uuid_with_suffix(21))
}

/// The floating pane that asks for `20` by `6` cells.
fn build_cells_floating_pane_id() -> PaneId {
    PaneId::from_uuid(build_test_uuid_with_suffix(22))
}

/// A floating pane size of `column_count` by `row_count` cells.
fn build_cells_floating_pane_size(column_count: u16, row_count: u16) -> FloatingPaneSize {
    FloatingPaneSize {
        width: FloatingPaneDimension::Cells(NonZeroU16::new(column_count).expect("not zero")),
        height: FloatingPaneDimension::Cells(NonZeroU16::new(row_count).expect("not zero")),
    }
}

/// A session with one tab `editor` holding one tiled pane, three attached
/// clients viewing it, and two floating panes:
/// - the `60%` pane holds `48x13`, the `20` by `6` pane holds `20x6`;
/// - the drawing client (`80x24`) shows the `60%` pane at the default
///   placement, then the `20x6` pane, which it pinned at `(70, 2)`;
/// - the minimizing client (`80x24`) moved the `60%` pane to `(2, 1)`, then
///   minimized it, and never showed the `20x6` pane;
/// - the starving client reported no room to draw a pane, and shows the `60%`
///   pane.
///
/// Returns the session and its tab.
fn build_floating_pane_session() -> (Session, TabId) {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let tiled_pane_id = PaneId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, tiled_pane_id);
    for (client_id, pane_area) in [
        (build_drawing_client_id(), None),
        (build_minimizing_client_id(), None),
        (build_starving_client_id(), Some(PaneArea::Starving)),
    ] {
        attach_test_client(
            &mut session,
            client_id,
            tab_id,
            TEST_VIEWPORT_SIZE,
            pane_area,
            Some(tiled_pane_id),
            None,
        );
    }
    for (floating_pane_id, desired_size, solved_size) in [
        (
            build_percent_floating_pane_id(),
            DEFAULT_FLOATING_PANE_SIZE,
            Size {
                column_count: 48,
                row_count: 13,
            },
        ),
        (
            build_cells_floating_pane_id(),
            build_cells_floating_pane_size(20, 6),
            Size {
                column_count: 20,
                row_count: 6,
            },
        ),
    ] {
        session
            .panes
            .register_pane_record(PaneRecord::from_terminal_pane(floating_pane_id))
            .expect("a fresh pane id");
        session
            .add_floating_member(
                FloatingMember {
                    pane_id: floating_pane_id,
                    desired_size,
                    solved_size,
                },
                &[],
            )
            .expect("the set has room");
    }

    let drawing_client = session
        .clients
        .get_client_mut_by_id(build_drawing_client_id())
        .expect("the drawing client is attached");
    assert!(drawing_client.focus_floating_pane(build_percent_floating_pane_id()));
    assert!(drawing_client.focus_floating_pane(build_cells_floating_pane_id()));
    drawing_client.pin_floating_pane(build_cells_floating_pane_id(), Point { column: 70, row: 2 });

    let minimizing_client = session
        .clients
        .get_client_mut_by_id(build_minimizing_client_id())
        .expect("the minimizing client is attached");
    assert!(minimizing_client.focus_floating_pane(build_percent_floating_pane_id()));
    assert!(minimizing_client.set_floating_pane_position(
        build_percent_floating_pane_id(),
        Point { column: 2, row: 1 }
    ));
    assert!(minimizing_client.minimize_floating_pane(build_percent_floating_pane_id()));

    let starving_client = session
        .clients
        .get_client_mut_by_id(build_starving_client_id())
        .expect("the starving client is attached");
    assert!(starving_client.focus_floating_pane(build_percent_floating_pane_id()));

    (session, tab_id)
}

/// The floating panes of [`build_floating_pane_session`], as the layout dump
/// describes them. The `60%` pane centers at `(16, 4)` in the drawing client's
/// `80x22` pane area. The `20x6` pane pinned at `(70, 2)` moves left to
/// `(60, 2)` to lie inside it, and draws over the `60%` pane.
fn build_expected_floating_pane_layouts() -> Vec<FloatingPaneLayout> {
    vec![
        FloatingPaneLayout {
            pane_id: build_percent_floating_pane_id(),
            desired_size: DEFAULT_FLOATING_PANE_SIZE,
            solved_size: Size {
                column_count: 48,
                row_count: 13,
            },
            client_views: vec![
                FloatingPaneClientView {
                    client_id: build_drawing_client_id(),
                    position: FloatingPanePosition::Default,
                    visibility: FloatingPaneVisibility::Drawn {
                        top_left_cell: Point { column: 16, row: 4 },
                        layer_index: 0,
                    },
                },
                FloatingPaneClientView {
                    client_id: build_minimizing_client_id(),
                    position: FloatingPanePosition::Moved(Point { column: 2, row: 1 }),
                    visibility: FloatingPaneVisibility::Minimized,
                },
                FloatingPaneClientView {
                    client_id: build_starving_client_id(),
                    position: FloatingPanePosition::Default,
                    visibility: FloatingPaneVisibility::NoFloatingViewport,
                },
            ],
        },
        FloatingPaneLayout {
            pane_id: build_cells_floating_pane_id(),
            desired_size: build_cells_floating_pane_size(20, 6),
            solved_size: Size {
                column_count: 20,
                row_count: 6,
            },
            client_views: vec![
                FloatingPaneClientView {
                    client_id: build_drawing_client_id(),
                    position: FloatingPanePosition::Pinned(Point { column: 70, row: 2 }),
                    visibility: FloatingPaneVisibility::Drawn {
                        top_left_cell: Point { column: 60, row: 2 },
                        layer_index: 1,
                    },
                },
                FloatingPaneClientView {
                    client_id: build_minimizing_client_id(),
                    position: FloatingPanePosition::Default,
                    visibility: FloatingPaneVisibility::Minimized,
                },
                FloatingPaneClientView {
                    client_id: build_starving_client_id(),
                    position: FloatingPanePosition::Default,
                    visibility: FloatingPaneVisibility::Minimized,
                },
            ],
        },
    ]
}

#[test]
fn each_floating_pane_lists_what_every_attached_client_draws_for_it() {
    let (session, _) = build_floating_pane_session();
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    assert_eq!(
        session_layout.floating_panes,
        build_expected_floating_pane_layouts()
    );
}

#[test]
fn a_session_with_no_floating_pane_lists_none() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    attach_test_client(
        &mut session,
        ClientId::new(),
        tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(pane_id),
        None,
    );
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    assert_eq!(session_layout.floating_panes, Vec::new());
}

#[test]
fn narrowing_to_one_tab_or_to_a_missing_tab_still_lists_every_floating_pane() {
    let (session, tab_id) = build_floating_pane_session();
    let server = build_test_runtime_with_session(session);

    let narrowed_session_layout = server
        .build_session_layout(Some(tab_id))
        .expect("one session is running");
    let missing_tab_session_layout = server
        .build_session_layout(Some(TabId::new()))
        .expect("one session is running");

    assert_eq!(narrowed_session_layout.tabs.len(), 1);
    assert_eq!(narrowed_session_layout.tabs[0].tab_id, tab_id);
    assert_eq!(
        narrowed_session_layout.floating_panes,
        build_expected_floating_pane_layouts()
    );
    assert_eq!(missing_tab_session_layout.tabs, Vec::new());
    assert_eq!(
        missing_tab_session_layout.floating_panes,
        build_expected_floating_pane_layouts()
    );
}

#[test]
fn a_floating_pane_with_no_attached_client_lists_no_client_view() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, PaneId::new());
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(
            build_percent_floating_pane_id(),
        ))
        .expect("a fresh pane id");
    session
        .add_floating_member(
            FloatingMember {
                pane_id: build_percent_floating_pane_id(),
                desired_size: DEFAULT_FLOATING_PANE_SIZE,
                solved_size: Size {
                    column_count: 48,
                    row_count: 13,
                },
            },
            &[],
        )
        .expect("the set has room");
    let server = build_test_runtime_with_session(session);

    let session_layout = server
        .build_session_layout(None)
        .expect("one session is running");

    assert_eq!(
        session_layout.floating_panes,
        vec![FloatingPaneLayout {
            pane_id: build_percent_floating_pane_id(),
            desired_size: DEFAULT_FLOATING_PANE_SIZE,
            solved_size: Size {
                column_count: 48,
                row_count: 13,
            },
            client_views: Vec::new(),
        }]
    );
    assert_eq!(session_layout.clients, Vec::new());
}
