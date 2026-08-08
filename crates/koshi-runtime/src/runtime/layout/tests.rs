//! Tests for building the layout dump from live session state: which tabs
//! are described, what each viewing client solves them to, and what comes
//! back when nothing can be solved at all.

use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use koshi_core::geometry::{Point, Rect, Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, SessionId};
use koshi_layout::mode::LayoutMode;
use koshi_layout::size::SizeWeight;
use koshi_layout::solver::StackHeader;
use koshi_layout::tree::{LayoutChild, LayoutNode, SplitNode};
use koshi_pty::backend::state::PtyBackend;
use koshi_session::client::{Client, ClientOrigin, ClientRegistry};
use koshi_session::session::state::Session;
use koshi_test_support::fake_pty::FakePtyBackend;
use uuid::Uuid;

use crate::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use crate::runtime::event::RuntimeEvent;

use super::*;

/// The terminal size every client below reports.
const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// The size a tab solves against for a client at [`VIEWPORT`]: the terminal
/// minus the two chrome rows.
const TAB_VIEWPORT: Size = Size { cols: 80, rows: 22 };

/// A bare runtime with stub services and no sessions. The sender is returned
/// so the inbox stays open.
fn new_runtime() -> (Server, mpsc::Sender<RuntimeEvent>) {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, inbox_rx) = mpsc::channel();
    let runtime = Server::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        tx.clone(),
    );
    (runtime, tx)
}

/// A fixed UUID ending in `tail`, so tab ids sort in a known order.
fn uuid_ending(tail: u8) -> Uuid {
    Uuid::parse_str(&format!("00000000-0000-0000-0000-0000000000{tail:02}"))
        .expect("literal UUID parses")
}

/// A session named `quiet-lake` with no tabs and no clients.
fn empty_session(session_id: SessionId) -> Session {
    Session::new(
        session_id,
        "quiet-lake".to_string(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    )
}

/// Add a tab named `name` at bar position `index`, showing `root_pane`.
fn add_tab(session: &mut Session, tab_id: TabId, name: &str, index: usize, root_pane: PaneId) {
    session
        .tabs
        .insert(tab_id, Tab::new(tab_id, name.to_string(), index, root_pane));
}

/// Attach `client_id` viewing `tab` at `viewport`, focused on `focused`, and
/// zoomed on `zoomed`.
fn attach(
    session: &mut Session,
    client_id: ClientId,
    tab: TabId,
    viewport: Size,
    focused: Option<PaneId>,
    zoomed: Option<PaneId>,
) {
    let mut client = Client::new(
        client_id,
        session.id,
        SystemTime::UNIX_EPOCH,
        viewport,
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    if let Some(pane) = focused {
        client.update_focused_pane(tab, pane);
    }
    if let Some(pane) = zoomed {
        client.zoom_pane(tab, pane);
    }
    session.attach_client(client);
}

/// A runtime holding exactly `session`.
fn runtime_with(session: Session) -> (Server, mpsc::Sender<RuntimeEvent>) {
    let (mut runtime, tx) = new_runtime();
    runtime.sessions.insert(session.id, session);
    (runtime, tx)
}

/// A left-right split of `left` and `right`, each taking an equal share.
fn side_by_side(left: PaneId, right: PaneId) -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutChild::new(LayoutNode::Pane(left)),
            LayoutChild::new(LayoutNode::Pane(right)),
        ],
    ))
}

#[test]
fn no_session_yields_no_layout() {
    let (runtime, _tx) = new_runtime();

    assert_eq!(runtime.build_session_layout(None), None);
}

#[test]
fn one_tab_one_client_reports_the_tree_the_solve_and_the_focus() {
    let session_id = SessionId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let client = ClientId::new();
    let mut session = empty_session(session_id);
    add_tab(&mut session, tab, "editor", 0, pane);
    attach(&mut session, client, tab, VIEWPORT, Some(pane), None);
    let (runtime, _tx) = runtime_with(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    assert_eq!(
        layout,
        SessionLayout {
            id: session_id,
            name: "quiet-lake".to_string(),
            tabs: vec![TabLayout {
                id: tab,
                name: "editor".to_string(),
                index: 0,
                tree: LayoutNode::Pane(pane),
                solved: vec![SolvedTab {
                    client,
                    viewport: TAB_VIEWPORT,
                    mode: LayoutMode::Tiled,
                    panes: vec![SolvedPane {
                        id: pane,
                        rect: Rect::new(Point { x: 0, y: 0 }, TAB_VIEWPORT),
                    }],
                    suppressed: Vec::new(),
                    all_suppressed: false,
                    stack_headers: Vec::new(),
                }],
            }],
            clients: vec![ClientFocus {
                id: client,
                active_tab: tab,
                focused_pane: Some(pane),
            }],
        },
    );
}

#[test]
fn a_client_that_has_focused_nothing_reports_no_focused_pane() {
    let session_id = SessionId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let client = ClientId::new();
    let mut session = empty_session(session_id);
    add_tab(&mut session, tab, "editor", 0, pane);
    attach(&mut session, client, tab, VIEWPORT, None, None);
    let (runtime, _tx) = runtime_with(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    assert_eq!(
        layout.clients,
        vec![ClientFocus {
            id: client,
            active_tab: tab,
            focused_pane: None,
        }],
    );
}

#[test]
fn a_session_with_no_tabs_and_no_clients_reports_only_its_own_name() {
    let session_id = SessionId::new();
    let (runtime, _tx) = runtime_with(empty_session(session_id));

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    assert_eq!(
        layout,
        SessionLayout {
            id: session_id,
            name: "quiet-lake".to_string(),
            tabs: Vec::new(),
            clients: Vec::new(),
        },
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
    let mut session = empty_session(session_id);
    add_tab(&mut session, watched, "editor", 0, watched_pane);
    add_tab(&mut session, unwatched, "logs", 1, unwatched_pane);
    attach(
        &mut session,
        client,
        watched,
        VIEWPORT,
        Some(watched_pane),
        None,
    );
    let (runtime, _tx) = runtime_with(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let described = layout
        .tabs
        .iter()
        .find(|entry| entry.id == unwatched)
        .expect("the unwatched tab is still described");
    assert_eq!(described.tree, LayoutNode::Pane(unwatched_pane));
    assert_eq!(described.solved, Vec::new());

    let viewed = layout
        .tabs
        .iter()
        .find(|entry| entry.id == watched)
        .expect("the watched tab is described");
    assert_eq!(viewed.solved.len(), 1);
    assert_eq!(viewed.solved[0].client, client);
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
    let mut session = empty_session(session_id);
    add_tab(&mut session, editor, "editor", 0, editor_pane);
    add_tab(&mut session, logs, "logs", 1, logs_pane);
    attach(
        &mut session,
        on_editor,
        editor,
        VIEWPORT,
        Some(editor_pane),
        None,
    );
    attach(&mut session, on_logs, logs, VIEWPORT, Some(logs_pane), None);
    let (runtime, _tx) = runtime_with(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let editor_solvers: Vec<ClientId> = layout
        .tabs
        .iter()
        .find(|entry| entry.id == editor)
        .expect("the editor tab is described")
        .solved
        .iter()
        .map(|solved| solved.client)
        .collect();
    assert_eq!(editor_solvers, vec![on_editor]);

    let logs_solvers: Vec<ClientId> = layout
        .tabs
        .iter()
        .find(|entry| entry.id == logs)
        .expect("the logs tab is described")
        .solved
        .iter()
        .map(|solved| solved.client)
        .collect();
    assert_eq!(logs_solvers, vec![on_logs]);
}

#[test]
fn tabs_come_back_in_tab_bar_order_not_in_id_order() {
    // The tab map is keyed by id, so the lower id is visited first; the tab
    // bar puts it second.
    let session_id = SessionId::new();
    let lower = TabId::from_uuid(uuid_ending(1));
    let higher = TabId::from_uuid(uuid_ending(2));
    let mut session = empty_session(session_id);
    add_tab(&mut session, lower, "second", 1, PaneId::new());
    add_tab(&mut session, higher, "first", 0, PaneId::new());
    let (runtime, _tx) = runtime_with(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let order: Vec<(TabId, usize)> = layout
        .tabs
        .iter()
        .map(|entry| (entry.id, entry.index))
        .collect();
    assert_eq!(order, vec![(higher, 0), (lower, 1)]);
}

#[test]
fn narrowing_to_one_tab_describes_that_tab_alone_and_still_names_every_client() {
    let session_id = SessionId::new();
    let first = TabId::new();
    let second = TabId::new();
    let first_pane = PaneId::new();
    let second_pane = PaneId::new();
    let client = ClientId::new();
    let mut session = empty_session(session_id);
    add_tab(&mut session, first, "editor", 0, first_pane);
    add_tab(&mut session, second, "logs", 1, second_pane);
    attach(
        &mut session,
        client,
        first,
        VIEWPORT,
        Some(first_pane),
        None,
    );
    let (runtime, _tx) = runtime_with(session);

    let layout = runtime
        .build_session_layout(Some(second))
        .expect("one session is running");

    assert_eq!(layout.tabs.len(), 1);
    assert_eq!(layout.tabs[0].id, second);
    assert_eq!(layout.tabs[0].name, "logs");
    assert_eq!(layout.tabs[0].index, 1);
    assert_eq!(
        layout.clients,
        vec![ClientFocus {
            id: client,
            active_tab: first,
            focused_pane: Some(first_pane),
        }],
    );
}

#[test]
fn narrowing_to_a_tab_that_does_not_exist_describes_no_tab_at_all() {
    let session_id = SessionId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let client = ClientId::new();
    let mut session = empty_session(session_id);
    add_tab(&mut session, tab, "editor", 0, pane);
    attach(&mut session, client, tab, VIEWPORT, Some(pane), None);
    let (runtime, _tx) = runtime_with(session);

    let layout = runtime
        .build_session_layout(Some(TabId::new()))
        .expect("one session is running");

    assert_eq!(
        layout,
        SessionLayout {
            id: session_id,
            name: "quiet-lake".to_string(),
            tabs: Vec::new(),
            clients: vec![ClientFocus {
                id: client,
                active_tab: tab,
                focused_pane: Some(pane),
            }],
        },
    );
}

#[test]
fn a_zoomed_client_reports_fullscreen_and_gives_the_whole_tab_to_one_pane() {
    let session_id = SessionId::new();
    let tab = TabId::new();
    let left = PaneId::new();
    let right = PaneId::new();
    let client = ClientId::new();
    let mut session = empty_session(session_id);
    add_tab(&mut session, tab, "editor", 0, left);
    session
        .tabs
        .get_mut(&tab)
        .expect("the tab was just added")
        .update_layout(side_by_side(left, right));
    attach(
        &mut session,
        client,
        tab,
        VIEWPORT,
        Some(right),
        Some(right),
    );
    let (runtime, _tx) = runtime_with(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let solved = &layout.tabs[0].solved[0];
    assert_eq!(solved.mode, LayoutMode::Fullscreen { focused: right });
    assert_eq!(
        solved.panes,
        vec![
            SolvedPane {
                id: left,
                rect: Rect::zero(),
            },
            SolvedPane {
                id: right,
                rect: Rect::new(Point { x: 0, y: 0 }, TAB_VIEWPORT),
            },
        ],
    );
    assert_eq!(solved.suppressed, Vec::new());
    assert!(!solved.all_suppressed);
    assert_eq!(solved.stack_headers, Vec::new());
}

#[test]
fn two_clients_on_one_tab_each_get_their_own_solve_of_the_same_tree() {
    let session_id = SessionId::new();
    let tab = TabId::new();
    let left = PaneId::new();
    let right = PaneId::new();
    let mut ids = [ClientId::new(), ClientId::new()];
    ids.sort();
    let [tiled, zoomed] = ids;
    let mut session = empty_session(session_id);
    add_tab(&mut session, tab, "editor", 0, left);
    session
        .tabs
        .get_mut(&tab)
        .expect("the tab was just added")
        .update_layout(side_by_side(left, right));
    attach(&mut session, tiled, tab, VIEWPORT, Some(left), None);
    attach(&mut session, zoomed, tab, VIEWPORT, Some(left), Some(left));
    let (runtime, _tx) = runtime_with(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    // Clients are listed in id order, and the ids were sorted above.
    let solved = &layout.tabs[0].solved;
    assert_eq!(solved.len(), 2);
    assert_eq!(solved[0].client, tiled);
    assert_eq!(solved[0].mode, LayoutMode::Tiled);
    assert_eq!(
        solved[0].panes,
        vec![
            SolvedPane {
                id: left,
                rect: Rect::new(Point { x: 0, y: 0 }, Size { cols: 40, rows: 22 }),
            },
            SolvedPane {
                id: right,
                rect: Rect::new(Point { x: 40, y: 0 }, Size { cols: 40, rows: 22 }),
            },
        ],
    );
    assert_eq!(solved[1].client, zoomed);
    assert_eq!(solved[1].mode, LayoutMode::Fullscreen { focused: left });
    assert_eq!(
        solved[1].panes,
        vec![
            SolvedPane {
                id: left,
                rect: Rect::new(Point { x: 0, y: 0 }, TAB_VIEWPORT),
            },
            SolvedPane {
                id: right,
                rect: Rect::zero(),
            },
        ],
    );
}

#[test]
fn two_clients_of_different_sizes_on_one_tab_both_solve_against_the_smaller() {
    let session_id = SessionId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut ids = [ClientId::new(), ClientId::new()];
    ids.sort();
    let [small, big] = ids;
    let mut session = empty_session(session_id);
    add_tab(&mut session, tab, "editor", 0, pane);
    attach(&mut session, small, tab, VIEWPORT, Some(pane), None);
    attach(
        &mut session,
        big,
        tab,
        Size {
            cols: 120,
            rows: 40,
        },
        Some(pane),
        None,
    );
    let (runtime, _tx) = runtime_with(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    // Clients are listed in id order, and the ids were sorted above. The tab
    // solves once, for the smallest viewer on each axis, so the 120x40 client
    // gets 80x22 too.
    let solved = &layout.tabs[0].solved;
    assert_eq!(solved.len(), 2);
    assert_eq!(solved[0].client, small);
    assert_eq!(solved[0].viewport, TAB_VIEWPORT);
    assert_eq!(
        solved[0].panes,
        vec![SolvedPane {
            id: pane,
            rect: Rect::new(Point { x: 0, y: 0 }, TAB_VIEWPORT),
        }],
    );
    assert_eq!(solved[1].client, big);
    assert_eq!(solved[1].viewport, TAB_VIEWPORT);
    assert_eq!(
        solved[1].panes,
        vec![SolvedPane {
            id: pane,
            rect: Rect::new(Point { x: 0, y: 0 }, TAB_VIEWPORT),
        }],
    );
}

#[test]
fn a_collapsed_stack_member_reports_its_header_strip() {
    let session_id = SessionId::new();
    let tab = TabId::new();
    let shown = PaneId::new();
    let collapsed = PaneId::new();
    let client = ClientId::new();
    let mut session = empty_session(session_id);
    add_tab(&mut session, tab, "editor", 0, shown);
    session
        .tabs
        .get_mut(&tab)
        .expect("the tab was just added")
        .update_layout(LayoutNode::Split(SplitNode::stack(
            vec![shown, collapsed],
            0,
        )));
    attach(&mut session, client, tab, VIEWPORT, Some(shown), None);
    let (runtime, _tx) = runtime_with(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let solved = &layout.tabs[0].solved[0];
    // The header takes the last row; the active member keeps the other 21.
    assert_eq!(
        solved.panes,
        vec![
            SolvedPane {
                id: shown,
                rect: Rect::new(Point { x: 0, y: 0 }, Size { cols: 80, rows: 21 }),
            },
            SolvedPane {
                id: collapsed,
                rect: Rect::new(Point { x: 0, y: 21 }, Size { cols: 80, rows: 1 }),
            },
        ],
    );
    assert_eq!(
        solved.stack_headers,
        vec![StackHeader {
            pane: collapsed,
            rect: Rect::new(Point { x: 0, y: 21 }, Size { cols: 80, rows: 1 }),
            position: 1,
            total: 2,
        }],
    );
    assert_eq!(solved.suppressed, Vec::new());
    assert!(!solved.all_suppressed);
}

#[test]
fn a_stack_whose_active_member_is_flagged_collapsed_still_expands_that_member() {
    // `active` decides which member expands; the per-child `collapsed` flag
    // does not feed the solve.
    let session_id = SessionId::new();
    let tab = TabId::new();
    let first = PaneId::new();
    let second = PaneId::new();
    let client = ClientId::new();
    let mut session = empty_session(session_id);
    add_tab(&mut session, tab, "editor", 0, first);
    session
        .tabs
        .get_mut(&tab)
        .expect("the tab was just added")
        .update_layout(LayoutNode::Split(SplitNode {
            direction: SplitDirection::Stacked,
            children: vec![
                LayoutChild {
                    node: LayoutNode::Pane(first),
                    collapsed: true,
                },
                LayoutChild {
                    node: LayoutNode::Pane(second),
                    collapsed: false,
                },
            ],
            weights: vec![SizeWeight::default(), SizeWeight::default()],
            active: 0,
        }));
    attach(&mut session, client, tab, VIEWPORT, Some(first), None);
    let (runtime, _tx) = runtime_with(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let solved = &layout.tabs[0].solved[0];
    assert_eq!(
        solved.panes,
        vec![
            SolvedPane {
                id: first,
                rect: Rect::new(Point { x: 0, y: 0 }, Size { cols: 80, rows: 21 }),
            },
            SolvedPane {
                id: second,
                rect: Rect::new(Point { x: 0, y: 21 }, Size { cols: 80, rows: 1 }),
            },
        ],
    );
    assert_eq!(
        solved.stack_headers,
        vec![StackHeader {
            pane: second,
            rect: Rect::new(Point { x: 0, y: 21 }, Size { cols: 80, rows: 1 }),
            position: 1,
            total: 2,
        }],
    );
}

#[test]
fn a_terminal_too_small_for_one_pane_suppresses_every_pane() {
    let session_id = SessionId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let client = ClientId::new();
    let mut session = empty_session(session_id);
    add_tab(&mut session, tab, "editor", 0, pane);
    // Three columns leaves no room for a bordered pane, which needs four.
    attach(
        &mut session,
        client,
        tab,
        Size { cols: 3, rows: 5 },
        Some(pane),
        None,
    );
    let (runtime, _tx) = runtime_with(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let solved = &layout.tabs[0].solved[0];
    assert_eq!(solved.viewport, Size { cols: 3, rows: 3 });
    assert_eq!(
        solved.panes,
        vec![SolvedPane {
            id: pane,
            rect: Rect::zero(),
        }],
    );
    assert_eq!(solved.suppressed, vec![pane]);
    assert!(solved.all_suppressed);
}

#[test]
fn a_pane_that_no_longer_fits_beside_its_neighbour_is_the_only_one_suppressed() {
    let session_id = SessionId::new();
    let tab = TabId::new();
    let left = PaneId::new();
    let right = PaneId::new();
    let client = ClientId::new();
    let mut session = empty_session(session_id);
    add_tab(&mut session, tab, "editor", 0, left);
    session
        .tabs
        .get_mut(&tab)
        .expect("the tab was just added")
        .update_layout(side_by_side(left, right));
    // Six columns hold one bordered pane of four, never two.
    attach(
        &mut session,
        client,
        tab,
        Size { cols: 6, rows: 6 },
        Some(left),
        None,
    );
    let (runtime, _tx) = runtime_with(session);

    let layout = runtime
        .build_session_layout(None)
        .expect("one session is running");

    let solved = &layout.tabs[0].solved[0];
    assert_eq!(
        solved.panes,
        vec![
            SolvedPane {
                id: left,
                rect: Rect::new(Point { x: 0, y: 0 }, Size { cols: 6, rows: 4 }),
            },
            SolvedPane {
                id: right,
                rect: Rect::zero(),
            },
        ],
    );
    assert_eq!(solved.suppressed, vec![right]);
    assert!(!solved.all_suppressed);
}
