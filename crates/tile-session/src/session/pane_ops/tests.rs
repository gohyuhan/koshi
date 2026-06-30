//! Tests for the NewPane state transaction.
//!
//! Each test builds a one-pane session (a tab, its leaf, and an attached
//! client) and runs [`new_pane`] against it, asserting the emitted events, the
//! post-split layout tree, the registered pane, and the client's focus — plus
//! the rejection paths (no space, unknown source) that must leave the session
//! untouched.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

use tile_core::event::{Event, LayoutChanged, PaneCreated, PaneFocused};
use tile_core::geometry::{Direction, Size, SplitDirection};
use tile_core::ids::{ClientId, PaneId, SessionId, TabId};
use tile_core::process::{ShellKind, SpawnSpec};
use tile_layout::tree::{LayoutChild, LayoutNode, SplitNode};
use tile_pane::pane::lifecycle::PaneLifecycle;
use tile_pane::pane::state::PaneRecord;

use super::{new_pane, NewPaneError, NewPaneSpec};
use crate::client::{Client, ClientRegistry};
use crate::session::state::{Session, Tab};

const VIEWPORT: Size = Size { cols: 80, rows: 24 };
const TINY: Size = Size { cols: 2, rows: 1 };

/// A session with one tab holding a single leaf `pane`, plus one attached
/// client of `viewport` viewing that tab with `pane` focused. Returns the
/// session and the ids needed to drive [`new_pane`].
fn session_one_pane(viewport: Size) -> (Session, TabId, PaneId, ClientId) {
    let tab_id = TabId::new();
    let pane = PaneId::new();
    let client_id = ClientId::new();

    let mut session = Session::new(SessionId::new(), "main".to_owned(), ClientRegistry::new());
    session
        .tabs
        .insert(tab_id, Tab::new(tab_id, "code".to_owned(), 0, pane));
    let _ = session
        .panes
        .insert(PaneRecord::new(pane, SystemTime::UNIX_EPOCH));

    let mut client = Client::new(
        client_id,
        session.id,
        SystemTime::UNIX_EPOCH,
        viewport,
        tab_id,
    );
    client.update_focused_pane(tab_id, pane);
    session.attach_client(client);

    (session, tab_id, pane, client_id)
}

/// The pane id minted by the op, read from its first event.
fn created_pane(events: &[Event]) -> PaneId {
    match events.first() {
        Some(Event::PaneCreated(created)) => created.pane_id,
        other => panic!("expected PaneCreated first, got {other:?}"),
    }
}

/// The two leaf ids of a tab whose layout is a single split, in child order.
fn split_panes(session: &Session, tab: TabId) -> (SplitDirection, Vec<PaneId>) {
    let LayoutNode::Split(split) = session.tabs.get(&tab).expect("tab").layout() else {
        panic!("expected a split layout");
    };
    let panes = split
        .children
        .iter()
        .map(|child| match child.node {
            LayoutNode::Pane(id) => id,
            _ => panic!("expected leaf children"),
        })
        .collect();
    (split.direction, panes)
}

#[test]
fn split_right_emits_three_events_and_focuses_the_new_pane() {
    let (mut session, tab, source, client) = session_one_pane(VIEWPORT);

    let events = new_pane(
        &mut session,
        source,
        tab,
        Direction::Right,
        Some(client),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    )
    .expect("fits the viewport");
    let new_id = created_pane(&events);

    assert_eq!(
        events,
        vec![
            Event::PaneCreated(PaneCreated {
                pane_id: new_id,
                tab_id: tab,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id: tab }),
            Event::PaneFocused(PaneFocused {
                pane_id: new_id,
                tab_id: tab,
            }),
        ]
    );

    // Tree is a horizontal split with the source first, the new pane after.
    assert_eq!(
        session.tabs.get(&tab).expect("tab").layout(),
        &LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutChild::new(LayoutNode::Pane(source)),
                LayoutChild::new(LayoutNode::Pane(new_id)),
            ],
        ))
    );

    // The new pane is registered, `Spawning`, and focused.
    assert_eq!(session.panes.len(), 2);
    assert_eq!(
        *session.panes.get(new_id).expect("record").lifecycle(),
        PaneLifecycle::Spawning,
    );
    assert_eq!(
        session
            .clients
            .get(client)
            .expect("client")
            .focused_pane(tab),
        Some(new_id),
    );
    assert_eq!(
        session.tabs.get(&tab).expect("tab").focus_mru().first(),
        Some(&new_id),
    );
}

#[test]
fn direction_sets_axis_and_new_pane_position() {
    // (direction, split axis, index the new pane takes among the children)
    let cases = [
        (Direction::Right, SplitDirection::Horizontal, 1usize),
        (Direction::Left, SplitDirection::Horizontal, 0),
        (Direction::Down, SplitDirection::Vertical, 1),
        (Direction::Up, SplitDirection::Vertical, 0),
    ];

    for (direction, axis, new_index) in cases {
        let (mut session, tab, source, client) = session_one_pane(VIEWPORT);
        let events = new_pane(
            &mut session,
            source,
            tab,
            direction,
            Some(client),
            NewPaneSpec::default(),
            SystemTime::UNIX_EPOCH,
        )
        .expect("fits the viewport");
        let new_id = created_pane(&events);

        let (got_axis, panes) = split_panes(&session, tab);
        assert_eq!(got_axis, axis, "axis for {direction:?}");
        assert_eq!(
            panes[new_index], new_id,
            "new pane position for {direction:?}"
        );
        assert_eq!(
            panes[1 - new_index],
            source,
            "source position for {direction:?}",
        );
    }
}

#[test]
fn no_space_rejects_and_leaves_the_session_untouched() {
    let (mut session, tab, source, client) = session_one_pane(TINY);
    let before = session.tabs.get(&tab).expect("tab").layout().clone();

    let result = new_pane(
        &mut session,
        source,
        tab,
        Direction::Right,
        Some(client),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(result, Err(NewPaneError::WontFit));
    assert_eq!(session.tabs.get(&tab).expect("tab").layout(), &before);
    assert_eq!(session.panes.len(), 1);
    // Focus is unchanged — still the source pane, never moved to a new one.
    assert_eq!(
        session
            .clients
            .get(client)
            .expect("client")
            .focused_pane(tab),
        Some(source),
    );
}

#[test]
fn no_focus_client_creates_pane_without_focus_and_skips_preflight() {
    // A tiny viewport would fail the fit check, but without a focus client there
    // is no rect to judge against, so the split proceeds.
    let (mut session, tab, source, _client) = session_one_pane(TINY);

    let events = new_pane(
        &mut session,
        source,
        tab,
        Direction::Right,
        None,
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    )
    .expect("no preflight without a focus client");
    let new_id = created_pane(&events);

    assert_eq!(
        events,
        vec![
            Event::PaneCreated(PaneCreated {
                pane_id: new_id,
                tab_id: tab,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id: tab }),
        ]
    );
    assert_eq!(session.panes.len(), 2);
    assert!(session.tabs.get(&tab).expect("tab").focus_mru().is_empty());
}

#[test]
fn name_sets_the_new_pane_title() {
    let (mut session, tab, source, client) = session_one_pane(VIEWPORT);

    let events = new_pane(
        &mut session,
        source,
        tab,
        Direction::Right,
        Some(client),
        NewPaneSpec {
            name: Some("logs".to_owned()),
            ..NewPaneSpec::default()
        },
        SystemTime::UNIX_EPOCH,
    )
    .expect("fits the viewport");
    let new_id = created_pane(&events);

    assert_eq!(
        session.panes.get(new_id).expect("record").title.as_deref(),
        Some("logs"),
    );
}

#[test]
fn cwd_and_command_are_recorded_on_the_new_pane() {
    let (mut session, tab, source, client) = session_one_pane(VIEWPORT);
    let cwd = PathBuf::from("/work");
    let command = SpawnSpec {
        program: PathBuf::from("/usr/bin/htop"),
        args: Vec::new(),
        cwd: None,
        env: BTreeMap::new(),
        shell_kind: ShellKind::Other("htop".to_owned()),
    };

    let events = new_pane(
        &mut session,
        source,
        tab,
        Direction::Right,
        Some(client),
        NewPaneSpec {
            cwd: Some(cwd.clone()),
            command: Some(command.clone()),
            ..NewPaneSpec::default()
        },
        SystemTime::UNIX_EPOCH,
    )
    .expect("fits the viewport");
    let new_id = created_pane(&events);

    let record = session.panes.get(new_id).expect("record");
    assert_eq!(record.cwd.as_deref(), Some(cwd.as_path()));
    assert_eq!(record.command.as_ref(), Some(&command));
}

#[test]
fn unknown_source_pane_is_rejected() {
    let (mut session, tab, _source, client) = session_one_pane(VIEWPORT);
    let ghost = PaneId::new();

    let result = new_pane(
        &mut session,
        ghost,
        tab,
        Direction::Right,
        Some(client),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(result, Err(NewPaneError::SourceNotFound));
    assert_eq!(session.panes.len(), 1);
}

#[test]
fn stale_focus_client_creates_pane_without_claiming_focus() {
    let (mut session, tab, source, _client) = session_one_pane(VIEWPORT);
    let stale = ClientId::new(); // never attached to this session

    let events = new_pane(
        &mut session,
        source,
        tab,
        Direction::Right,
        Some(stale),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    )
    .expect("the split succeeds; a stale client simply focuses nothing");
    let new_id = created_pane(&events);

    // No PaneFocused — the named client is not attached, so nothing is focused.
    assert_eq!(
        events,
        vec![
            Event::PaneCreated(PaneCreated {
                pane_id: new_id,
                tab_id: tab,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id: tab }),
        ]
    );
    // The pane exists, but no focus MRU entry claims a focus that never happened.
    assert_eq!(session.panes.len(), 2);
    assert!(session.tabs.get(&tab).expect("tab").focus_mru().is_empty());
}
