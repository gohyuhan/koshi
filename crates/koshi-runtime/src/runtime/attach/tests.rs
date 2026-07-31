//! Tests for the attach-structure builder: mapping a live session's tabs,
//! layout trees, focus history and pane registry into the structure a client
//! attaches with.

use std::time::SystemTime;

use koshi_core::geometry::SplitDirection;
use koshi_core::ids::{PaneId, SessionId, TabId};
use koshi_ipc::attach::{PaneStructure, TabStructure};
use koshi_layout::tree::{LayoutChild, LayoutNode, SplitNode};
use koshi_pane::pane::state::{PaneKind, PaneRecord};
use koshi_session::client::ClientRegistry;
use koshi_session::session::state::{Session, Tab};

use super::*;

/// An empty session named `name`, with no tabs and no panes.
fn session(name: &str) -> Session {
    Session::new(
        SessionId::new(),
        name.to_string(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    )
}

/// Register a terminal pane and hand back its id.
fn add_pane(session: &mut Session) -> PaneId {
    let pane_id = PaneId::new();
    session
        .panes
        .insert(PaneRecord::new(pane_id, SystemTime::UNIX_EPOCH))
        .expect("unique pane id");
    pane_id
}

#[test]
fn an_empty_session_carries_its_identity_and_no_tabs_or_panes() {
    let session = session("koshi-dev");

    let structure = session_structure(&session);

    assert_eq!(structure.id, session.id);
    assert_eq!(structure.name, "koshi-dev");
    assert_eq!(structure.tabs, Vec::<TabStructure>::new());
    assert_eq!(structure.panes, Vec::<PaneStructure>::new());
}

#[test]
fn a_single_pane_tab_carries_its_name_index_layout_and_focus() {
    let mut session = session("s");
    let pane_id = add_pane(&mut session);
    let tab_id = TabId::new();
    let mut tab = Tab::new(tab_id, "edit".to_string(), 0, pane_id);
    tab.record_focus_mru(pane_id);
    session.tabs.insert(tab_id, tab);

    let structure = session_structure(&session);

    assert_eq!(
        structure.tabs,
        vec![TabStructure {
            id: tab_id,
            name: "edit".to_string(),
            index: 0,
            layout: LayoutNode::Pane(pane_id),
            focus_mru: vec![pane_id],
        }]
    );
    assert_eq!(
        structure.panes,
        vec![PaneStructure {
            id: pane_id,
            kind: PaneKind::Terminal,
        }]
    );
}

#[test]
fn a_split_layout_travels_unsolved_with_its_weights_and_direction() {
    let mut session = session("s");
    let left = add_pane(&mut session);
    let right = add_pane(&mut session);
    let split = SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            LayoutChild::new(LayoutNode::Pane(left)),
            LayoutChild::new(LayoutNode::Pane(right)),
        ],
    );
    let tab_id = TabId::new();
    let mut tab = Tab::new(tab_id, "edit".to_string(), 0, left);
    tab.update_layout(LayoutNode::Split(split.clone()));
    session.tabs.insert(tab_id, tab);

    let structure = session_structure(&session);

    assert_eq!(structure.tabs.len(), 1);
    assert_eq!(structure.tabs[0].layout, LayoutNode::Split(split));
}

#[test]
fn a_stacked_tab_keeps_every_collapsed_flag_and_the_active_index() {
    let mut session = session("s");
    let first = add_pane(&mut session);
    let second = add_pane(&mut session);
    let third = add_pane(&mut session);
    let stack = SplitNode::stack(vec![first, second, third], 1);
    let tab_id = TabId::new();
    let mut tab = Tab::new(tab_id, "stacked".to_string(), 0, first);
    tab.update_layout(LayoutNode::Split(stack));
    session.tabs.insert(tab_id, tab);

    let structure = session_structure(&session);

    let LayoutNode::Split(carried) = &structure.tabs[0].layout else {
        panic!("the tab's layout is a split");
    };
    assert_eq!(carried.direction, SplitDirection::Stacked);
    assert_eq!(carried.active, 1);
    assert_eq!(
        carried
            .children
            .iter()
            .map(|child| child.collapsed)
            .collect::<Vec<bool>>(),
        vec![true, false, true]
    );
}

#[test]
fn tabs_come_out_in_display_order_not_map_order() {
    let mut session = session("s");
    let first_pane = add_pane(&mut session);
    let second_pane = add_pane(&mut session);
    // Insert the tab shown second before the tab shown first, so map order and
    // display order disagree.
    let second_id = TabId::new();
    session.tabs.insert(
        second_id,
        Tab::new(second_id, "logs".to_string(), 1, second_pane),
    );
    let first_id = TabId::new();
    session.tabs.insert(
        first_id,
        Tab::new(first_id, "edit".to_string(), 0, first_pane),
    );

    let structure = session_structure(&session);

    assert_eq!(
        structure
            .tabs
            .iter()
            .map(|tab| (tab.index, tab.name.as_str()))
            .collect::<Vec<(usize, &str)>>(),
        vec![(0, "edit"), (1, "logs")]
    );
}

#[test]
fn every_tab_is_carried_not_only_the_first() {
    let mut session = session("s");
    let first_pane = add_pane(&mut session);
    let second_pane = add_pane(&mut session);
    let first_id = TabId::new();
    let second_id = TabId::new();
    session.tabs.insert(
        first_id,
        Tab::new(first_id, "edit".to_string(), 0, first_pane),
    );
    session.tabs.insert(
        second_id,
        Tab::new(second_id, "logs".to_string(), 1, second_pane),
    );

    let structure = session_structure(&session);

    assert_eq!(structure.tabs.len(), 2);
    assert_eq!(structure.tabs[1].layout, LayoutNode::Pane(second_pane));
}

/// How many panes the ordering tests register.
///
/// `PaneRegistry` stores records in a `HashMap`, so a builder that dropped the
/// sort would emit hash order. Hash order happens to be ascending for one
/// arrangement out of every factorial of this count, so twelve panes leaves
/// about one chance in 479 million of such a builder passing.
const PANE_ORDER_SAMPLE: usize = 12;

#[test]
fn every_registered_pane_is_carried_ordered_by_id() {
    let mut session = session("s");
    let mut registered: Vec<PaneId> = (0..PANE_ORDER_SAMPLE)
        .map(|_| add_pane(&mut session))
        .collect();
    registered.sort();

    let structure = session_structure(&session);

    assert_eq!(
        structure.panes,
        registered
            .iter()
            .map(|&id| PaneStructure {
                id,
                kind: PaneKind::Terminal,
            })
            .collect::<Vec<PaneStructure>>()
    );
}

#[test]
fn the_pane_list_is_strictly_ascending_by_id() {
    let mut session = session("s");
    for _ in 0..PANE_ORDER_SAMPLE {
        add_pane(&mut session);
    }

    let structure = session_structure(&session);

    let out_of_order: Vec<(PaneId, PaneId)> = structure
        .panes
        .windows(2)
        .filter(|pair| pair[0].id >= pair[1].id)
        .map(|pair| (pair[0].id, pair[1].id))
        .collect();
    assert_eq!(
        out_of_order,
        Vec::new(),
        "pane list is not strictly ascending: {:?}",
        structure
            .panes
            .iter()
            .map(|pane| pane.id)
            .collect::<Vec<PaneId>>()
    );
    assert_eq!(structure.panes.len(), PANE_ORDER_SAMPLE);
}

#[test]
fn the_tab_list_is_strictly_ascending_by_display_index() {
    let mut session = session("s");
    // Insert tabs in reverse display order, so map order and bar order disagree.
    for index in (0..6).rev() {
        let pane_id = add_pane(&mut session);
        let tab_id = TabId::new();
        session.tabs.insert(
            tab_id,
            Tab::new(tab_id, format!("t{index}"), index, pane_id),
        );
    }

    let structure = session_structure(&session);

    let out_of_order: Vec<(usize, usize)> = structure
        .tabs
        .windows(2)
        .filter(|pair| pair[0].index >= pair[1].index)
        .map(|pair| (pair[0].index, pair[1].index))
        .collect();
    assert_eq!(
        out_of_order,
        Vec::new(),
        "tab list is not strictly ascending: {:?}",
        structure
            .tabs
            .iter()
            .map(|tab| tab.index)
            .collect::<Vec<usize>>()
    );
    assert_eq!(structure.tabs.len(), 6);
}

#[test]
fn a_tab_nothing_has_focused_carries_an_empty_focus_history() {
    let mut session = session("s");
    let pane_id = add_pane(&mut session);
    let tab_id = TabId::new();
    // `Tab::new` records no focus, which is the state of a freshly created tab
    // and of a tab whose last focused pane was closed with no client attached.
    session
        .tabs
        .insert(tab_id, Tab::new(tab_id, "edit".to_string(), 0, pane_id));

    let structure = session_structure(&session);

    assert_eq!(structure.tabs[0].focus_mru, Vec::<PaneId>::new());
}

#[test]
fn a_plugin_pane_reports_its_plugin_id() {
    use koshi_core::ids::PluginId;

    let mut session = session("s");
    let plugin_id = PluginId::new();
    let pane_id = PaneId::new();
    session
        .panes
        .insert(PaneRecord::new_with_kind(
            pane_id,
            PaneKind::Plugin { plugin_id },
            SystemTime::UNIX_EPOCH,
        ))
        .expect("unique pane id");

    let structure = session_structure(&session);

    assert_eq!(
        structure.panes,
        vec![PaneStructure {
            id: pane_id,
            kind: PaneKind::Plugin { plugin_id },
        }]
    );
}

#[test]
fn focus_history_is_carried_most_recent_first() {
    let mut session = session("s");
    let first = add_pane(&mut session);
    let second = add_pane(&mut session);
    let tab_id = TabId::new();
    let mut tab = Tab::new(tab_id, "edit".to_string(), 0, first);
    tab.record_focus_mru(first);
    tab.record_focus_mru(second);
    session.tabs.insert(tab_id, tab);

    let structure = session_structure(&session);

    assert_eq!(structure.tabs[0].focus_mru, vec![second, first]);
}
