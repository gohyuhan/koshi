//! Session-state commit for checked cross-tab pane placement.
//!
//! The layout crate builds the proposed trees without touching live state.
//! This module installs those trees, repairs per-client references, and removes
//! an empty source tab without removing the transferred pane record.

use std::collections::HashSet;

use koshi_core::event::{Event, LayoutChanged, PaneFocused, TabFocused};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_layout::placement::CrossTabPlacement;
use thiserror::Error;

use crate::session::state::{Session, Tab};
use crate::session::tab_ops::close_empty_tab_after_transfer;

/// A checked placement could not be installed because its tabs or source pane
/// no longer match the prepared trees.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlacementCommitError {
    /// The source and destination tabs are the same tab.
    #[error("source and destination tabs must differ")]
    SameTab,
    /// The source tab is absent.
    #[error("source tab is not present")]
    SourceTabNotFound,
    /// The destination tab is absent.
    #[error("destination tab is not present")]
    DestinationTabNotFound,
    /// The source pane is absent from the source tab's current tree.
    #[error("source pane is not present in the source tab")]
    SourcePaneNotFound,
    /// The prepared destination tree is already owned by the source tree.
    #[error("prepared placement contains a pane in both trees")]
    PaneOwnershipConflict,
    /// The client that should follow the placed pane is absent.
    #[error("acting client is not attached")]
    ActingClientNotFound,
}

/// Install a prepared cross-tab placement and return its ordered events.
///
/// `source_tree` is `None` only when the source pane was the source tab's sole
/// leaf. In that case the source tab is removed after the destination tree is
/// installed. The commit verifies unique ownership, exact pane-set conservation,
/// and that only the source pane leaves the source tree. The pane registry and
/// every per-pane view remain attached to the pane id.
pub fn commit_cross_tab_placement(
    session: &mut Session,
    source_tab_id: TabId,
    destination_tab_id: TabId,
    source_pane_id: PaneId,
    prepared_placement: CrossTabPlacement,
    acting_client_id: ClientId,
) -> Result<Vec<Event>, PlacementCommitError> {
    if source_tab_id == destination_tab_id {
        return Err(PlacementCommitError::SameTab);
    }
    let source_leaf_pane_ids_before = session
        .tabs
        .get(&source_tab_id)
        .ok_or(PlacementCommitError::SourceTabNotFound)?
        .get_layout_tree()
        .list_leaf_pane_ids();
    let destination_leaf_pane_ids_before = session
        .tabs
        .get(&destination_tab_id)
        .ok_or(PlacementCommitError::DestinationTabNotFound)?
        .get_layout_tree()
        .list_leaf_pane_ids();
    if !source_leaf_pane_ids_before.contains(&source_pane_id) {
        return Err(PlacementCommitError::SourcePaneNotFound);
    }

    let source_leaf_pane_ids_after = prepared_placement
        .source_tree
        .as_ref()
        .map(|layout_tree| layout_tree.list_leaf_pane_ids())
        .unwrap_or_default();
    let destination_leaf_pane_ids_after = prepared_placement.destination_tree.list_leaf_pane_ids();
    let source_pane_ids_after: HashSet<PaneId> =
        source_leaf_pane_ids_after.iter().copied().collect();
    let destination_pane_ids_after: HashSet<PaneId> =
        destination_leaf_pane_ids_after.iter().copied().collect();
    if source_pane_ids_after.len() != source_leaf_pane_ids_after.len()
        || destination_pane_ids_after.len() != destination_leaf_pane_ids_after.len()
    {
        return Err(PlacementCommitError::PaneOwnershipConflict);
    }
    if !destination_pane_ids_after.contains(&source_pane_id)
        || !source_pane_ids_after.is_disjoint(&destination_pane_ids_after)
    {
        return Err(PlacementCommitError::PaneOwnershipConflict);
    }
    if session.clients.get_client_by_id(acting_client_id).is_none() {
        return Err(PlacementCommitError::ActingClientNotFound);
    }
    let acting_previous_tab_id = session
        .clients
        .get_client_by_id(acting_client_id)
        .map(|client| client.get_active_tab())
        .ok_or(PlacementCommitError::ActingClientNotFound)?;
    let acting_previous_pane_id = session
        .clients
        .get_client_by_id(acting_client_id)
        .and_then(|client| client.get_focused_pane(destination_tab_id));

    let source_pane_ids_before: HashSet<PaneId> =
        source_leaf_pane_ids_before.iter().copied().collect();
    let destination_pane_ids_before: HashSet<PaneId> =
        destination_leaf_pane_ids_before.iter().copied().collect();
    if source_pane_ids_before.len() != source_leaf_pane_ids_before.len()
        || destination_pane_ids_before.len() != destination_leaf_pane_ids_before.len()
    {
        return Err(PlacementCommitError::PaneOwnershipConflict);
    }
    if !source_pane_ids_before.is_disjoint(&destination_pane_ids_before) {
        return Err(PlacementCommitError::PaneOwnershipConflict);
    }
    let source_moved_pane_ids: HashSet<PaneId> = source_pane_ids_before
        .difference(&source_pane_ids_after)
        .copied()
        .collect();
    let destination_moved_pane_ids: HashSet<PaneId> = destination_pane_ids_before
        .difference(&destination_pane_ids_after)
        .copied()
        .collect();
    if source_moved_pane_ids.len() != 1
        || !source_moved_pane_ids.contains(&source_pane_id)
        || destination_moved_pane_ids.len() > 1
    {
        return Err(PlacementCommitError::PaneOwnershipConflict);
    }
    let all_pane_ids_before: HashSet<PaneId> = source_pane_ids_before
        .union(&destination_pane_ids_before)
        .copied()
        .collect();
    let all_pane_ids_after: HashSet<PaneId> = source_pane_ids_after
        .union(&destination_pane_ids_after)
        .copied()
        .collect();
    if all_pane_ids_before != all_pane_ids_after {
        return Err(PlacementCommitError::PaneOwnershipConflict);
    }
    let source_tab_will_close = prepared_placement.source_tree.is_none();

    let affected_client_ids: Vec<ClientId> = session
        .clients
        .list_attached_clients()
        .filter(|client| {
            client.get_client_id() == acting_client_id
                || client.get_active_tab() == source_tab_id
                || client.get_active_tab() == destination_tab_id
                || client.get_focused_pane(source_tab_id).is_some()
                || client.get_focused_pane(destination_tab_id).is_some()
                || client.get_zoomed_pane(source_tab_id).is_some()
                || client.get_zoomed_pane(destination_tab_id).is_some()
        })
        .map(|client| client.get_client_id())
        .collect();

    // Destination installation is the first live tree write. All fallible
    // checks happened before this point.
    if let Some(destination_tab) = session.tabs.get_mut(&destination_tab_id) {
        destination_tab.update_layout(prepared_placement.destination_tree);
        for &pane_id in &destination_moved_pane_ids {
            destination_tab.remove_focus_mru(pane_id);
        }
        for &pane_id in &source_moved_pane_ids {
            destination_tab.record_focus_mru(pane_id);
        }
    }
    let mut events = vec![Event::LayoutChanged(LayoutChanged {
        tab_id: destination_tab_id,
    })];

    if let Some(source_tree) = prepared_placement.source_tree {
        if let Some(source_tab) = session.tabs.get_mut(&source_tab_id) {
            source_tab.update_layout(source_tree);
            for &pane_id in &source_moved_pane_ids {
                source_tab.remove_focus_mru(pane_id);
            }
            for &pane_id in &destination_moved_pane_ids {
                source_tab.record_focus_mru(pane_id);
            }
        }
        events.push(Event::LayoutChanged(LayoutChanged {
            tab_id: source_tab_id,
        }));
    }

    if let Some(acting_client) = session.clients.get_client_mut_by_id(acting_client_id) {
        acting_client.clear_zoom(destination_tab_id);
        acting_client.update_active_tab(destination_tab_id);
        acting_client.update_focused_pane(destination_tab_id, source_pane_id);
    }
    if acting_previous_tab_id != destination_tab_id {
        events.push(Event::TabFocused(TabFocused {
            client_id: acting_client_id,
            tab_id: destination_tab_id,
            previous_tab_id: acting_previous_tab_id,
        }));
    }
    if acting_previous_pane_id != Some(source_pane_id) {
        events.push(Event::PaneFocused(PaneFocused {
            client_id: acting_client_id,
            tab_id: destination_tab_id,
            pane_id: source_pane_id,
            previous_pane_id: acting_previous_pane_id,
        }));
    }
    if let Some(destination_tab) = session.tabs.get_mut(&destination_tab_id) {
        destination_tab.record_focus_mru(source_pane_id);
    }

    if !source_tab_will_close {
        repair_client_tab_focus(
            session,
            acting_client_id,
            source_tab_id,
            &source_pane_ids_after,
            &mut events,
        );
        clear_invalid_client_zoom(
            session,
            acting_client_id,
            source_tab_id,
            &source_pane_ids_after,
        );
    }

    for client_id in affected_client_ids {
        if client_id == acting_client_id {
            continue;
        }
        repair_client_tab_focus(
            session,
            client_id,
            destination_tab_id,
            &destination_pane_ids_after,
            &mut events,
        );
        if !source_tab_will_close {
            repair_client_tab_focus(
                session,
                client_id,
                source_tab_id,
                &source_pane_ids_after,
                &mut events,
            );
        }
        clear_invalid_client_zoom(
            session,
            client_id,
            destination_tab_id,
            &destination_pane_ids_after,
        );
        if !source_tab_will_close {
            clear_invalid_client_zoom(session, client_id, source_tab_id, &source_pane_ids_after);
        }
    }

    if source_tab_will_close {
        events.extend(close_empty_tab_after_transfer(session, source_tab_id));
    }

    Ok(events)
}

fn repair_client_tab_focus(
    session: &mut Session,
    client_id: ClientId,
    tab_id: TabId,
    valid_pane_ids: &HashSet<PaneId>,
    events: &mut Vec<Event>,
) {
    let should_clear_zoom = session
        .clients
        .get_client_by_id(client_id)
        .and_then(|client| client.get_zoomed_pane(tab_id))
        .is_some_and(|pane_id| !valid_pane_ids.contains(&pane_id));
    let Some(previous_pane_id) = session
        .clients
        .get_client_by_id(client_id)
        .and_then(|client| client.get_focused_pane(tab_id))
    else {
        return;
    };
    if valid_pane_ids.contains(&previous_pane_id) {
        return;
    }
    let Some(tab) = session.tabs.get(&tab_id) else {
        return;
    };
    let next_pane_id = select_valid_focus_pane(tab, valid_pane_ids);
    let selected_pane_id = if let Some(client) = session.clients.get_client_mut_by_id(client_id) {
        match next_pane_id {
            Some(next_pane_id) => {
                client.update_focused_pane(tab_id, next_pane_id);
                events.push(Event::PaneFocused(PaneFocused {
                    client_id,
                    tab_id,
                    pane_id: next_pane_id,
                    previous_pane_id: Some(previous_pane_id),
                }));
                Some(next_pane_id)
            }
            None => {
                client.remove_focused_pane(tab_id);
                None
            }
        }
    } else {
        None
    };
    if let Some(selected_pane_id) = selected_pane_id {
        if let Some(tab) = session.tabs.get_mut(&tab_id) {
            tab.record_focus_mru(selected_pane_id);
        }
    }
    if should_clear_zoom {
        if let Some(client) = session.clients.get_client_mut_by_id(client_id) {
            client.clear_zoom(tab_id);
        }
    }
}

fn select_valid_focus_pane(tab: &Tab, valid_pane_ids: &HashSet<PaneId>) -> Option<PaneId> {
    tab.list_focus_mru()
        .iter()
        .copied()
        .chain(tab.get_layout_tree().list_leaf_pane_ids())
        .find(|pane_id| valid_pane_ids.contains(pane_id))
}

fn clear_invalid_client_zoom(
    session: &mut Session,
    client_id: ClientId,
    tab_id: TabId,
    valid_pane_ids: &HashSet<PaneId>,
) {
    let should_clear_zoom = session
        .clients
        .get_client_by_id(client_id)
        .and_then(|client| client.get_zoomed_pane(tab_id))
        .is_some_and(|pane_id| !valid_pane_ids.contains(&pane_id));
    if should_clear_zoom {
        if let Some(client) = session.clients.get_client_mut_by_id(client_id) {
            client.clear_zoom(tab_id);
        }
    }
}

#[cfg(test)]
mod tests;
