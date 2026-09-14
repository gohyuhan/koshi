//! Tests for the attach-structure builder: mapping a live session's tabs,
//! layout trees, focus history and pane registry into the structure a client
//! attaches with.

use std::time::SystemTime;

use koshi_core::geometry::SplitDirection;
use koshi_core::ids::{PaneId, SessionId, TabId};
use koshi_ipc::attach::{PaneStructure, TabStructure};
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::state::{PaneKind, PaneRecord};
use koshi_session::client::ClientRegistry;
use koshi_session::session::state::{Session, Tab};

use super::*;

/// An empty session named `session_name`, with no tabs and no panes.
fn build_test_session(session_name: &str) -> Session {
    Session::from_identity_and_client_registry(
        SessionId::new(),
        session_name.to_string(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    )
}

/// Register a terminal pane and hand back its id.
fn register_test_pane(session: &mut Session) -> PaneId {
    let pane_id = PaneId::new();
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(
            pane_id,
            SystemTime::UNIX_EPOCH,
        ))
        .expect("unique pane id");
    pane_id
}

#[test]
fn an_empty_session_carries_its_identity_and_no_tabs_or_panes() {
    let session = build_test_session("koshi-dev");

    let session_structure = build_session_structure_snapshot(&session);

    assert_eq!(session_structure.session_id, session.session_id);
    assert_eq!(session_structure.session_name, "koshi-dev");
    assert_eq!(session_structure.tabs, Vec::<TabStructure>::new());
    assert_eq!(session_structure.panes, Vec::<PaneStructure>::new());
}

#[test]
fn a_single_pane_tab_carries_its_name_index_layout_and_focus() {
    let mut session = build_test_session("session");
    let pane_id = register_test_pane(&mut session);
    let tab_id = TabId::new();
    let mut tab = Tab::from_root_pane(tab_id, "edit".to_string(), 0, pane_id);
    tab.record_focus_mru(pane_id);
    session.tabs.insert(tab_id, tab);

    let session_structure = build_session_structure_snapshot(&session);

    assert_eq!(
        session_structure.tabs,
        vec![TabStructure {
            tab_id,
            tab_name: "edit".to_string(),
            tab_index: 0,
            layout: LayoutNode::Pane(pane_id),
            focus_mru: vec![pane_id],
        }]
    );
    assert_eq!(
        session_structure.panes,
        vec![PaneStructure {
            pane_id,
            pane_kind: PaneKind::Terminal,
        }]
    );
}

#[test]
fn a_split_layout_travels_unsolved_with_its_weights_and_direction() {
    let mut session = build_test_session("session");
    let left_pane_id = register_test_pane(&mut session);
    let right_pane_id = register_test_pane(&mut session);
    let split_node = SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            LayoutNode::Pane(left_pane_id),
            LayoutNode::Pane(right_pane_id),
        ],
    );
    let tab_id = TabId::new();
    let mut tab = Tab::from_root_pane(tab_id, "edit".to_string(), 0, left_pane_id);
    tab.update_layout(LayoutNode::Split(split_node.clone()));
    session.tabs.insert(tab_id, tab);

    let session_structure = build_session_structure_snapshot(&session);

    assert_eq!(session_structure.tabs.len(), 1);
    assert_eq!(
        session_structure.tabs[0].layout,
        LayoutNode::Split(split_node)
    );
}

#[test]
fn a_stacked_tab_keeps_every_collapsed_flag_and_the_active_index() {
    let mut session = build_test_session("session");
    let first_pane_id = register_test_pane(&mut session);
    let second_pane_id = register_test_pane(&mut session);
    let third_pane_id = register_test_pane(&mut session);
    let stack =
        SplitNode::from_stacked_pane_ids(vec![first_pane_id, second_pane_id, third_pane_id], 1);
    let tab_id = TabId::new();
    let mut tab = Tab::from_root_pane(tab_id, "stacked".to_string(), 0, first_pane_id);
    tab.update_layout(LayoutNode::Split(stack));
    session.tabs.insert(tab_id, tab);

    let session_structure = build_session_structure_snapshot(&session);

    let LayoutNode::Split(split_node) = &session_structure.tabs[0].layout else {
        panic!("the tab's layout is a split");
    };
    assert_eq!(split_node.direction, SplitDirection::Stacked);
    assert_eq!(split_node.active_child_index, 1);
    assert_eq!(
        split_node
            .children
            .iter()
            .enumerate()
            .map(|(child_index, _)| split_node.is_child_collapsed(child_index))
            .collect::<Vec<bool>>(),
        vec![true, false, true]
    );
}

#[test]
fn tabs_come_out_in_display_order_not_map_order() {
    let mut session = build_test_session("session");
    let first_pane_id = register_test_pane(&mut session);
    let second_pane_id = register_test_pane(&mut session);
    // Insert the tab shown second before the tab shown first, so map order and
    // display order disagree.
    let second_tab_id = TabId::new();
    session.tabs.insert(
        second_tab_id,
        Tab::from_root_pane(second_tab_id, "logs".to_string(), 1, second_pane_id),
    );
    let first_tab_id = TabId::new();
    session.tabs.insert(
        first_tab_id,
        Tab::from_root_pane(first_tab_id, "edit".to_string(), 0, first_pane_id),
    );

    let session_structure = build_session_structure_snapshot(&session);

    assert_eq!(
        session_structure
            .tabs
            .iter()
            .map(|tab_structure| (tab_structure.tab_index, tab_structure.tab_name.as_str()))
            .collect::<Vec<(usize, &str)>>(),
        vec![(0, "edit"), (1, "logs")]
    );
}

#[test]
fn every_tab_is_carried_not_only_the_first() {
    let mut session = build_test_session("session");
    let first_pane_id = register_test_pane(&mut session);
    let second_pane_id = register_test_pane(&mut session);
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    session.tabs.insert(
        first_tab_id,
        Tab::from_root_pane(first_tab_id, "edit".to_string(), 0, first_pane_id),
    );
    session.tabs.insert(
        second_tab_id,
        Tab::from_root_pane(second_tab_id, "logs".to_string(), 1, second_pane_id),
    );

    let session_structure = build_session_structure_snapshot(&session);

    assert_eq!(session_structure.tabs.len(), 2);
    assert_eq!(
        session_structure.tabs[1].layout,
        LayoutNode::Pane(second_pane_id)
    );
}

/// How many panes the ordering tests register. Twelve ids, minted in ascending
/// order and registered in that order, prove the snapshot carries every one of
/// them in id order.
const PANE_ORDER_SAMPLE_COUNT: usize = 12;

#[test]
fn every_registered_pane_is_carried_ordered_by_id() {
    let mut session = build_test_session("session");
    let mut registered_pane_ids: Vec<PaneId> = (0..PANE_ORDER_SAMPLE_COUNT)
        .map(|_| register_test_pane(&mut session))
        .collect();
    registered_pane_ids.sort();

    let session_structure = build_session_structure_snapshot(&session);

    assert_eq!(
        session_structure.panes,
        registered_pane_ids
            .iter()
            .map(|&pane_id| PaneStructure {
                pane_id,
                pane_kind: PaneKind::Terminal,
            })
            .collect::<Vec<PaneStructure>>()
    );
}

#[test]
fn the_pane_list_is_strictly_ascending_by_id() {
    let mut session = build_test_session("session");
    for _ in 0..PANE_ORDER_SAMPLE_COUNT {
        register_test_pane(&mut session);
    }

    let session_structure = build_session_structure_snapshot(&session);

    let out_of_order_pane_pairs: Vec<(PaneId, PaneId)> = session_structure
        .panes
        .windows(2)
        .filter(|pane_id_pair| pane_id_pair[0].pane_id >= pane_id_pair[1].pane_id)
        .map(|pane_id_pair| (pane_id_pair[0].pane_id, pane_id_pair[1].pane_id))
        .collect();
    assert_eq!(
        out_of_order_pane_pairs,
        Vec::new(),
        "pane list is not strictly ascending: {:?}",
        session_structure
            .panes
            .iter()
            .map(|pane_structure| pane_structure.pane_id)
            .collect::<Vec<PaneId>>()
    );
    assert_eq!(session_structure.panes.len(), PANE_ORDER_SAMPLE_COUNT);
}

#[test]
fn the_tab_list_is_strictly_ascending_by_display_index() {
    let mut session = build_test_session("session");
    // Insert tabs in reverse display order, so map order and bar order disagree.
    for tab_index in (0..6).rev() {
        let pane_id = register_test_pane(&mut session);
        let tab_id = TabId::new();
        session.tabs.insert(
            tab_id,
            Tab::from_root_pane(tab_id, format!("t{tab_index}"), tab_index, pane_id),
        );
    }

    let session_structure = build_session_structure_snapshot(&session);

    let out_of_order_tab_index_pairs: Vec<(usize, usize)> = session_structure
        .tabs
        .windows(2)
        .filter(|tab_index_pair| tab_index_pair[0].tab_index >= tab_index_pair[1].tab_index)
        .map(|tab_index_pair| (tab_index_pair[0].tab_index, tab_index_pair[1].tab_index))
        .collect();
    assert_eq!(
        out_of_order_tab_index_pairs,
        Vec::new(),
        "tab list is not strictly ascending: {:?}",
        session_structure
            .tabs
            .iter()
            .map(|tab_structure| tab_structure.tab_index)
            .collect::<Vec<usize>>()
    );
    assert_eq!(session_structure.tabs.len(), 6);
}

#[test]
fn a_tab_nothing_has_focused_carries_an_empty_focus_history() {
    let mut session = build_test_session("session");
    let pane_id = register_test_pane(&mut session);
    let tab_id = TabId::new();
    // `Tab::from_root_pane` records no focus, which is the state of a freshly created tab
    // and of a tab whose last focused pane was closed with no client attached.
    session.tabs.insert(
        tab_id,
        Tab::from_root_pane(tab_id, "edit".to_string(), 0, pane_id),
    );

    let session_structure = build_session_structure_snapshot(&session);

    assert_eq!(session_structure.tabs[0].focus_mru, Vec::<PaneId>::new());
}

#[test]
fn a_plugin_pane_reports_its_plugin_id() {
    use koshi_core::ids::PluginId;

    let mut session = build_test_session("session");
    let plugin_id = PluginId::new();
    let pane_id = PaneId::new();
    session
        .panes
        .register_pane_record(PaneRecord::from_pane_kind(
            pane_id,
            PaneKind::Plugin { plugin_id },
            SystemTime::UNIX_EPOCH,
        ))
        .expect("unique pane id");

    let session_structure = build_session_structure_snapshot(&session);

    assert_eq!(
        session_structure.panes,
        vec![PaneStructure {
            pane_id,
            pane_kind: PaneKind::Plugin { plugin_id },
        }]
    );
}

#[test]
fn a_pane_no_tab_layout_names_is_still_carried() {
    let mut session = build_test_session("session");
    let layout_pane_id = register_test_pane(&mut session);
    let unlisted_pane_id = register_test_pane(&mut session);
    let tab_id = TabId::new();
    session.tabs.insert(
        tab_id,
        Tab::from_root_pane(tab_id, "edit".to_string(), 0, layout_pane_id),
    );

    let session_structure = build_session_structure_snapshot(&session);

    let mut all_pane_ids = vec![layout_pane_id, unlisted_pane_id];
    all_pane_ids.sort();
    assert_eq!(
        session_structure
            .panes
            .iter()
            .map(|pane_structure| pane_structure.pane_id)
            .collect::<Vec<PaneId>>(),
        all_pane_ids
    );
    assert_eq!(
        session_structure.tabs[0].layout,
        LayoutNode::Pane(layout_pane_id)
    );
}

#[test]
fn focus_history_carries_an_id_the_pane_list_no_longer_holds() {
    let mut session = build_test_session("session");
    let retained_pane_id = register_test_pane(&mut session);
    let removed_pane_id = register_test_pane(&mut session);
    let tab_id = TabId::new();
    let mut tab = Tab::from_root_pane(tab_id, "edit".to_string(), 0, retained_pane_id);
    tab.record_focus_mru(retained_pane_id);
    tab.record_focus_mru(removed_pane_id);
    session.tabs.insert(tab_id, tab);
    session
        .panes
        .remove_pane_record(removed_pane_id)
        .expect("the pane was registered");

    let session_structure = build_session_structure_snapshot(&session);

    assert_eq!(
        session_structure.tabs[0].focus_mru,
        vec![removed_pane_id, retained_pane_id]
    );
    assert_eq!(
        session_structure.panes,
        vec![PaneStructure {
            pane_id: retained_pane_id,
            pane_kind: PaneKind::Terminal,
        }]
    );
}

#[test]
fn the_session_and_tab_names_are_carried_byte_for_byte() {
    let mut session = build_test_session("");
    let pane_id = register_test_pane(&mut session);
    let tab_id = TabId::new();
    session.tabs.insert(
        tab_id,
        Tab::from_root_pane(tab_id, "編集 \u{1f5c2}\u{200b}".to_string(), 0, pane_id),
    );

    let session_structure = build_session_structure_snapshot(&session);

    assert_eq!(session_structure.session_name, "");
    assert_eq!(session_structure.tabs[0].tab_name, "編集 \u{1f5c2}\u{200b}");
}

#[test]
fn focus_history_is_carried_most_recent_first() {
    let mut session = build_test_session("session");
    let first_pane_id = register_test_pane(&mut session);
    let second_pane_id = register_test_pane(&mut session);
    let tab_id = TabId::new();
    let mut tab = Tab::from_root_pane(tab_id, "edit".to_string(), 0, first_pane_id);
    tab.record_focus_mru(first_pane_id);
    tab.record_focus_mru(second_pane_id);
    session.tabs.insert(tab_id, tab);

    let session_structure = build_session_structure_snapshot(&session);

    assert_eq!(
        session_structure.tabs[0].focus_mru,
        vec![second_pane_id, first_pane_id]
    );
}
