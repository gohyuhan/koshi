//! Tests for building the layout dump from live session state: which tabs
//! are described, what each viewing client solves them to, and what comes
//! back when nothing can be solved at all.

use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use koshi_core::geometry::{PaneArea, Point, Rect, Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, SessionId};
use koshi_layout::mode::LayoutMode;
use koshi_layout::size::SizeWeight;
use koshi_layout::solver::StackHeader;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pty::backend::state::PtyBackend;
use koshi_session::client::{Client, ClientOrigin, ClientRegistry};
use koshi_session::session::state::Session;
use koshi_test_support::fake_pty::FakePtyBackend;
use uuid::Uuid;

use crate::runtime::event::RuntimeEvent;

use super::*;

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

/// A bare runtime with stub services and no sessions. The sender is returned
/// so the inbox stays open.
fn build_test_runtime() -> (Server, mpsc::Sender<RuntimeEvent>) {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let (event_sender, event_receiver) = mpsc::channel();
    let server = Server::from_runtime_parts(pty_backend, event_receiver, event_sender.clone());
    (server, event_sender)
}

/// A fixed UUID ending in `suffix_byte`, so tab ids sort in a known order.
fn build_test_uuid_with_suffix(suffix_byte: u8) -> Uuid {
    Uuid::parse_str(&format!(
        "00000000-0000-0000-0000-0000000000{suffix_byte:02}"
    ))
    .expect("literal UUID parses")
}

/// A session named `quiet-lake` with no tabs and no clients.
fn build_empty_session(session_id: SessionId) -> Session {
    Session::from_identity_and_client_registry(
        session_id,
        "quiet-lake".to_string(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    )
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
        Tab::from_root_pane(tab_id, tab_name.to_string(), tab_index, root_pane),
    );
}

/// Attach `client_id` viewing `tab_id` at `viewport_size` reporting `pane_area`,
/// focused on `focused_pane_id`, and zoomed on `zoomed_pane_id`.
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
    session.attach_client(attached_client);
}

/// A runtime holding exactly `session`.
fn build_test_runtime_with_session(session: Session) -> (Server, mpsc::Sender<RuntimeEvent>) {
    let (mut server, event_sender) = build_test_runtime();
    server.session_by_id.insert(session.session_id, session);
    (server, event_sender)
}

/// A left-right split of `left_pane_id` and `right_pane_id`, each taking an equal share.
fn build_horizontal_split(left_pane_id: PaneId, right_pane_id: PaneId) -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(left_pane_id),
            LayoutNode::Pane(right_pane_id),
        ],
    ))
}

#[test]
fn no_session_yields_no_layout() {
    let (runtime, _tx) = build_test_runtime();

    assert_eq!(runtime.build_session_layout(None), None);
}

#[test]
fn one_tab_one_client_reports_the_tree_the_solve_and_the_focus() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    attach_test_client(
        &mut session,
        client,
        tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(pane_id),
        None,
    );
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    assert_eq!(
        layout,
        SessionLayout {
            session_id,
            session_name: "quiet-lake".to_string(),
            tabs: vec![TabLayout {
                tab_id,
                tab_name: "editor".to_string(),
                tab_index: 0,
                layout_tree: LayoutNode::Pane(pane_id),
                solved_tabs: vec![SolvedTab {
                    client_id: client,
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
            clients: vec![ClientFocus {
                client_id: client,
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
    let client = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    attach_test_client(
        &mut session,
        client,
        tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        None,
        None,
    );
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    assert_eq!(
        layout.clients,
        vec![ClientFocus {
            client_id: client,
            active_tab_id: tab_id,
            focused_pane_id: None,
        }],
    );
}

#[test]
fn a_session_with_no_tabs_and_no_clients_reports_only_its_own_name() {
    let session_id = SessionId::new();
    let (runtime, _tx) = build_test_runtime_with_session(build_empty_session(session_id));

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    assert_eq!(
        layout,
        SessionLayout {
            session_id,
            session_name: "quiet-lake".to_string(),
            tabs: Vec::new(),
            clients: Vec::new(),
        },
    );
}

#[test]
fn a_tab_whose_only_viewer_is_starving_lists_no_solved_layout() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    attach_test_client(
        &mut session,
        client,
        tab_id,
        TEST_VIEWPORT_SIZE,
        Some(PaneArea::Starving),
        Some(pane_id),
        None,
    );
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let described = layout
        .tabs
        .iter()
        .find(|tab_summary| tab_summary.tab_id == tab_id)
        .expect("the tab is still described");
    assert_eq!(described.layout_tree, LayoutNode::Pane(pane_id));
    assert_eq!(described.solved_tabs, Vec::new());
}

#[test]
fn a_reported_pane_area_is_the_size_the_tab_solves_against() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    // A reported area replaces the terminal-minus-chrome default outright: the
    // two chrome rows are not taken off it again.
    attach_test_client(
        &mut session,
        client,
        tab_id,
        TEST_VIEWPORT_SIZE,
        Some(PaneArea::Reported(Size {
            column_count: 40,
            row_count: 10,
        })),
        Some(pane_id),
        None,
    );
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let solved = &layout.tabs[0].solved_tabs[0];
    assert_eq!(
        solved.viewport_size,
        Size {
            column_count: 40,
            row_count: 10
        }
    );
    assert_eq!(
        solved.pane_rects,
        vec![SolvedPane {
            pane_id,
            outer_rect: Rect::from_size_at_origin(Size {
                column_count: 40,
                row_count: 10
            }),
        }],
    );
    assert_eq!(solved.suppressed_pane_ids, Vec::new());
    assert!(!solved.is_every_pane_suppressed);
}

#[test]
fn a_reported_pane_area_larger_than_the_terminal_is_clamped_to_it() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    attach_test_client(
        &mut session,
        client,
        tab_id,
        TEST_VIEWPORT_SIZE,
        Some(PaneArea::Reported(Size {
            column_count: 200,
            row_count: 100,
        })),
        Some(pane_id),
        None,
    );
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    // Clamped to the 80x24 terminal on each axis, chrome rows included.
    let solved = &layout.tabs[0].solved_tabs[0];
    assert_eq!(solved.viewport_size, TEST_VIEWPORT_SIZE);
    assert_eq!(
        solved.pane_rects,
        vec![SolvedPane {
            pane_id,
            outer_rect: Rect::from_size_at_origin(TEST_VIEWPORT_SIZE),
        }],
    );
    assert_eq!(solved.suppressed_pane_ids, Vec::new());
    assert!(!solved.is_every_pane_suppressed);
}

#[test]
fn a_starving_viewer_still_gets_a_solve_when_another_viewer_reports_a_size() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut ids = [ClientId::new(), ClientId::new()];
    ids.sort();
    let [starving, reporting] = ids;
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    attach_test_client(
        &mut session,
        starving,
        tab_id,
        TEST_VIEWPORT_SIZE,
        Some(PaneArea::Starving),
        Some(pane_id),
        None,
    );
    attach_test_client(
        &mut session,
        reporting,
        tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(pane_id),
        None,
    );
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    // Clients are listed in id order, and the ids were sorted above. The
    // starving client contributes no size to the tab_id, and is still solved
    // against the size the other viewer set.
    let solved = &layout.tabs[0].solved_tabs;
    assert_eq!(solved.len(), 2);
    assert_eq!(solved[0].client_id, starving);
    assert_eq!(solved[0].viewport_size, TAB_VIEWPORT_SIZE);
    assert_eq!(
        solved[0].pane_rects,
        vec![SolvedPane {
            pane_id,
            outer_rect: Rect::from_size_at_origin(TAB_VIEWPORT_SIZE),
        }],
    );
    assert_eq!(solved[1].client_id, reporting);
    assert_eq!(solved[1].viewport_size, TAB_VIEWPORT_SIZE);
    assert_eq!(
        solved[1].pane_rects,
        vec![SolvedPane {
            pane_id,
            outer_rect: Rect::from_size_at_origin(TAB_VIEWPORT_SIZE),
        }],
    );
}

#[test]
fn a_tab_no_client_views_carries_its_tree_and_no_solve() {
    let session_id = SessionId::new();
    let watched = TabId::new();
    let unwatched = TabId::new();
    let watched_pane = PaneId::new();
    let unwatched_pane = PaneId::new();
    let client = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, watched, "editor", 0, watched_pane);
    add_session_tab(&mut session, unwatched, "logs", 1, unwatched_pane);
    attach_test_client(
        &mut session,
        client,
        watched,
        TEST_VIEWPORT_SIZE,
        None,
        Some(watched_pane),
        None,
    );
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let described = layout
        .tabs
        .iter()
        .find(|tab_summary| tab_summary.tab_id == unwatched)
        .expect("the unwatched tab is still described");
    assert_eq!(described.layout_tree, LayoutNode::Pane(unwatched_pane));
    assert_eq!(described.solved_tabs, Vec::new());

    let viewed = layout
        .tabs
        .iter()
        .find(|tab_summary| tab_summary.tab_id == watched)
        .expect("the watched tab is described");
    assert_eq!(viewed.solved_tabs.len(), 1);
    assert_eq!(viewed.solved_tabs[0].client_id, client);
}

#[test]
fn a_client_viewing_another_tab_is_left_out_of_this_tab_solve() {
    // Two tabs, each with its own viewer. Every tab has a viewer, so no tab is
    // skipped for want of a size; each tab must still solve for its own client
    // alone.
    let session_id = SessionId::new();
    let editor = TabId::new();
    let logs = TabId::new();
    let editor_pane = PaneId::new();
    let logs_pane = PaneId::new();
    let on_editor = ClientId::new();
    let on_logs = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, editor, "editor", 0, editor_pane);
    add_session_tab(&mut session, logs, "logs", 1, logs_pane);
    attach_test_client(
        &mut session,
        on_editor,
        editor,
        TEST_VIEWPORT_SIZE,
        None,
        Some(editor_pane),
        None,
    );
    attach_test_client(
        &mut session,
        on_logs,
        logs,
        TEST_VIEWPORT_SIZE,
        None,
        Some(logs_pane),
        None,
    );
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let editor_solvers: Vec<ClientId> = layout
        .tabs
        .iter()
        .find(|tab_summary| tab_summary.tab_id == editor)
        .expect("the editor tab is described")
        .solved_tabs
        .iter()
        .map(|solved| solved.client_id)
        .collect();
    assert_eq!(editor_solvers, vec![on_editor]);

    let logs_solvers: Vec<ClientId> = layout
        .tabs
        .iter()
        .find(|tab_summary| tab_summary.tab_id == logs)
        .expect("the logs tab is described")
        .solved_tabs
        .iter()
        .map(|solved| solved.client_id)
        .collect();
    assert_eq!(logs_solvers, vec![on_logs]);
}

#[test]
fn tabs_come_back_in_tab_bar_order_not_in_id_order() {
    // The tab map is keyed by id, so the lower id is visited first; the tab
    // bar puts it second.
    let session_id = SessionId::new();
    let lower = TabId::from_uuid(build_test_uuid_with_suffix(1));
    let higher = TabId::from_uuid(build_test_uuid_with_suffix(2));
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, lower, "second", 1, PaneId::new());
    add_session_tab(&mut session, higher, "first", 0, PaneId::new());
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let order: Vec<(TabId, usize)> = layout
        .tabs
        .iter()
        .map(|tab_summary| (tab_summary.tab_id, tab_summary.tab_index))
        .collect();
    assert_eq!(order, vec![(higher, 0), (lower, 1)]);
}

#[test]
fn two_tabs_at_the_same_bar_index_come_back_in_id_order() {
    // Sorting by bar position keeps the order the tab map handed over, which
    // is id order, so a shared index is broken by id.
    let session_id = SessionId::new();
    let lower = TabId::from_uuid(build_test_uuid_with_suffix(1));
    let higher = TabId::from_uuid(build_test_uuid_with_suffix(2));
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, lower, "editor", 0, PaneId::new());
    add_session_tab(&mut session, higher, "logs", 0, PaneId::new());
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let order: Vec<(TabId, usize)> = layout
        .tabs
        .iter()
        .map(|tab_summary| (tab_summary.tab_id, tab_summary.tab_index))
        .collect();
    assert_eq!(order, vec![(lower, 0), (higher, 0)]);
}

#[test]
fn narrowing_to_one_tab_describes_that_tab_alone_and_still_names_every_client() {
    let session_id = SessionId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let client = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, first_tab_id, "editor", 0, first_pane_id);
    add_session_tab(&mut session, second_tab_id, "logs", 1, second_pane_id);
    attach_test_client(
        &mut session,
        client,
        first_tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(first_pane_id),
        None,
    );
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(Some(second_tab_id))
        .expect("one session is running");

    assert_eq!(
        layout.tabs,
        vec![TabLayout {
            tab_id: second_tab_id,
            tab_name: "logs".to_string(),
            tab_index: 1,
            layout_tree: LayoutNode::Pane(second_pane_id),
            solved_tabs: Vec::new(),
        }],
    );
    assert_eq!(
        layout.clients,
        vec![ClientFocus {
            client_id: client,
            active_tab_id: first_tab_id,
            focused_pane_id: Some(first_pane_id),
        }],
    );
}

#[test]
fn narrowing_to_the_tab_its_own_viewer_watches_keeps_that_tabs_solve() {
    let session_id = SessionId::new();
    let editor = TabId::new();
    let logs = TabId::new();
    let editor_pane = PaneId::new();
    let logs_pane = PaneId::new();
    let client = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, editor, "editor", 0, editor_pane);
    add_session_tab(&mut session, logs, "logs", 1, logs_pane);
    attach_test_client(
        &mut session,
        client,
        editor,
        TEST_VIEWPORT_SIZE,
        None,
        Some(editor_pane),
        None,
    );
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(Some(editor))
        .expect("one session is running");

    assert_eq!(
        layout.tabs,
        vec![TabLayout {
            tab_id: editor,
            tab_name: "editor".to_string(),
            tab_index: 0,
            layout_tree: LayoutNode::Pane(editor_pane),
            solved_tabs: vec![SolvedTab {
                client_id: client,
                viewport_size: TAB_VIEWPORT_SIZE,
                layout_mode: LayoutMode::Tiled,
                pane_rects: vec![SolvedPane {
                    pane_id: editor_pane,
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
    let client = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    attach_test_client(
        &mut session,
        client,
        tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(pane_id),
        None,
    );
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(Some(TabId::new()))
        .expect("one session is running");

    assert_eq!(
        layout,
        SessionLayout {
            session_id,
            session_name: "quiet-lake".to_string(),
            tabs: Vec::new(),
            clients: vec![ClientFocus {
                client_id: client,
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
    let client = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, left_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("the tab was just added")
        .update_layout(build_horizontal_split(left_pane_id, right_pane_id));
    attach_test_client(
        &mut session,
        client,
        tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(right_pane_id),
        Some(right_pane_id),
    );
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let solved = &layout.tabs[0].solved_tabs[0];
    assert_eq!(
        solved.layout_mode,
        LayoutMode::Fullscreen {
            focused_pane_id: right_pane_id
        }
    );
    assert_eq!(
        solved.pane_rects,
        vec![
            SolvedPane {
                pane_id: left_pane_id,
                outer_rect: Rect::empty_at_origin(),
            },
            SolvedPane {
                pane_id: right_pane_id,
                outer_rect: Rect::from_size_at_origin(TAB_VIEWPORT_SIZE),
            },
        ],
    );
    assert_eq!(solved.suppressed_pane_ids, Vec::new());
    assert!(!solved.is_every_pane_suppressed);
    assert_eq!(solved.stack_headers, Vec::new());
}

#[test]
fn two_clients_on_one_tab_each_get_their_own_solve_of_the_same_tree() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let mut ids = [ClientId::new(), ClientId::new()];
    ids.sort();
    let [tiled_client_id, zoomed_client_id] = ids;
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, left_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("the tab was just added")
        .update_layout(build_horizontal_split(left_pane_id, right_pane_id));
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
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    // Clients are listed in id order, and the ids were sorted above.
    let solved = &layout.tabs[0].solved_tabs;
    assert_eq!(solved.len(), 2);
    assert_eq!(solved[0].client_id, tiled_client_id);
    assert_eq!(solved[0].layout_mode, LayoutMode::Tiled);
    assert_eq!(
        solved[0].pane_rects,
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
    assert_eq!(solved[1].client_id, zoomed_client_id);
    assert_eq!(
        solved[1].layout_mode,
        LayoutMode::Fullscreen {
            focused_pane_id: left_pane_id
        }
    );
    assert_eq!(
        solved[1].pane_rects,
        vec![
            SolvedPane {
                pane_id: left_pane_id,
                outer_rect: Rect::from_size_at_origin(TAB_VIEWPORT_SIZE),
            },
            SolvedPane {
                pane_id: right_pane_id,
                outer_rect: Rect::empty_at_origin(),
            },
        ],
    );
}

#[test]
fn two_clients_of_different_sizes_on_one_tab_both_solve_against_the_smaller() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut ids = [ClientId::new(), ClientId::new()];
    ids.sort();
    let [small_client_id, big_client_id] = ids;
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
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    // Clients are listed in id order, and the ids were sorted above. The tab
    // solves once, for the smallest viewer on each axis, so the 120x40 client
    // gets 80x22 too.
    let solved = &layout.tabs[0].solved_tabs;
    assert_eq!(solved.len(), 2);
    assert_eq!(solved[0].client_id, small_client_id);
    assert_eq!(solved[0].viewport_size, TAB_VIEWPORT_SIZE);
    assert_eq!(
        solved[0].pane_rects,
        vec![SolvedPane {
            pane_id,
            outer_rect: Rect::from_size_at_origin(TAB_VIEWPORT_SIZE),
        }],
    );
    assert_eq!(solved[1].client_id, big_client_id);
    assert_eq!(solved[1].viewport_size, TAB_VIEWPORT_SIZE);
    assert_eq!(
        solved[1].pane_rects,
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
    let client = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, shown_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("the tab was just added")
        .update_layout(LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![shown_pane_id, collapsed_pane_id],
            0,
        )));
    attach_test_client(
        &mut session,
        client,
        tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(shown_pane_id),
        None,
    );
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let solved = &layout.tabs[0].solved_tabs[0];
    // The header takes the last row; the active member keeps the other 21.
    assert_eq!(
        solved.pane_rects,
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
        solved.stack_headers,
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
    assert_eq!(solved.suppressed_pane_ids, Vec::new());
    assert!(!solved.is_every_pane_suppressed);
}

#[test]
fn a_stack_whose_active_member_is_flagged_collapsed_still_expands_that_member() {
    // `active` decides which member expands; the per-child `collapsed` flag
    // does not feed the solve.
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let client = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, first_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("the tab was just added")
        .update_layout(LayoutNode::Split(SplitNode {
            direction: SplitDirection::Stacked,
            children: vec![
                LayoutNode::Pane(first_pane_id),
                LayoutNode::Pane(second_pane_id),
            ],
            weights: vec![SizeWeight::default(), SizeWeight::default()],
            active_child_index: 0,
        }));
    attach_test_client(
        &mut session,
        client,
        tab_id,
        TEST_VIEWPORT_SIZE,
        None,
        Some(first_pane_id),
        None,
    );
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let solved = &layout.tabs[0].solved_tabs[0];
    assert_eq!(
        solved.pane_rects,
        vec![
            SolvedPane {
                pane_id: first_pane_id,
                outer_rect: Rect::from_size_at_origin(Size {
                    column_count: 80,
                    row_count: 21
                }),
            },
            SolvedPane {
                pane_id: second_pane_id,
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
        solved.stack_headers,
        vec![StackHeader {
            pane_id: second_pane_id,
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
}

#[test]
fn a_terminal_too_small_for_one_pane_suppresses_every_pane() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, pane_id);
    // Three columns leaves no room for a bordered pane_id, which needs four.
    attach_test_client(
        &mut session,
        client,
        tab_id,
        Size {
            column_count: 3,
            row_count: 5,
        },
        None,
        Some(pane_id),
        None,
    );
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let solved = &layout.tabs[0].solved_tabs[0];
    assert_eq!(
        solved.viewport_size,
        Size {
            column_count: 3,
            row_count: 3
        }
    );
    assert_eq!(
        solved.pane_rects,
        vec![SolvedPane {
            pane_id,
            outer_rect: Rect::empty_at_origin(),
        }],
    );
    assert_eq!(solved.suppressed_pane_ids, vec![pane_id]);
    assert!(solved.is_every_pane_suppressed);
}

#[test]
fn a_pane_that_no_longer_fits_beside_its_neighbour_is_the_only_one_suppressed() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let client = ClientId::new();
    let mut session = build_empty_session(session_id);
    add_session_tab(&mut session, tab_id, "editor", 0, left_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("the tab was just added")
        .update_layout(build_horizontal_split(left_pane_id, right_pane_id));
    // Six columns hold one bordered pane of four, never two.
    attach_test_client(
        &mut session,
        client,
        tab_id,
        Size {
            column_count: 6,
            row_count: 6,
        },
        None,
        Some(left_pane_id),
        None,
    );
    let (runtime, _tx) = build_test_runtime_with_session(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let solved = &layout.tabs[0].solved_tabs[0];
    assert_eq!(
        solved.pane_rects,
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
                outer_rect: Rect::empty_at_origin(),
            },
        ],
    );
    assert_eq!(solved.suppressed_pane_ids, vec![right_pane_id]);
    assert!(!solved.is_every_pane_suppressed);
}
