//! Pane placement previews: the preview each client names, built from live
//! session state.
//!
//! A client names one placement target
//! ([`Server::set_pane_placement_preview`]), and each frame of that client
//! carries the target's preview
//! ([`RenderSnapshot::placement_preview`](koshi_renderer::snapshot::RenderSnapshot::placement_preview))
//! and the content of every pane the preview can show. A pass rebuilds a
//! preview only when a state it reads changed
//! ([`Server::update_changed_placement_previews`]). A rebuilt preview equal to
//! the held one keeps the held `Arc`, and the client's frame stays unmarked.
//!
//! Tabs `[A, B]`: Alice previews a split of her pane `vim` into `B`. Bob
//! scrolls a pane in `A`: Alice's preview stays as it is. Bob splits a pane in
//! `B`: Alice's preview is built again with the new layout of `B`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use koshi_core::geometry::{Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_ipc::placement::PanePlacementPreviewTarget;
use koshi_layout::mode::LayoutMode;
use koshi_layout::placement::SwapTabSizes;
use koshi_layout::solver::{solve_layout_with_sizing, PaneSizing};
use koshi_renderer::snapshot::{
    list_pane_slots, PlacementPreview, PlacementSnapshot, PlacementSource, PlacementTabSnapshot,
    TabSnapshot,
};
use koshi_session::client::Client;
use koshi_session::session::placement::compute_placement_tab_sizes;
use koshi_session::session::state::{Session, SessionViewChanges, Tab};

use crate::server::Server;

/// The pane placement preview one client names.
#[derive(Debug)]
pub(crate) struct ClientPlacementPreview {
    /// The session the preview was built in.
    session_id: SessionId,
    /// The pane and the destination tab the client previews.
    placement_target: PanePlacementPreviewTarget,
    /// The preview the client's frames carry.
    placement_preview: Arc<PlacementPreview>,
    /// The state the preview read of the acting client and of each client it
    /// depends on ([`PlacementClientInputs::is_read_by_placement`]), at the
    /// build.
    client_inputs_by_client_id: BTreeMap<ClientId, PlacementClientInputs>,
    /// The panes whose content the client's frames carry for the preview
    /// ([`PlacementSnapshot::list_drawable_pane_ids`]). Empty for an
    /// unavailable preview.
    preview_pane_ids: Vec<PaneId>,
}

impl ClientPlacementPreview {
    /// The preview the client's frames carry.
    pub(crate) fn get_placement_preview(&self) -> &Arc<PlacementPreview> {
        &self.placement_preview
    }

    /// The session the preview was built in.
    pub(crate) fn get_session_id(&self) -> SessionId {
        self.session_id
    }

    /// The panes whose content the client's frames carry for the preview:
    /// each pane the displayed tab draws now or after a placement the session
    /// accepts ([`PlacementSnapshot::list_drawable_pane_ids`]).
    pub(crate) fn list_preview_pane_ids(&self) -> &[PaneId] {
        &self.preview_pane_ids
    }

    /// Whether the preview of `acting_client_id` reads a state that
    /// `view_changes` lists for `session`, the session the preview was built
    /// in:
    /// - the tab bar or the pane sizing changed;
    /// - for an available preview, the layout of its source or destination
    ///   tab changed, or the floating panes changed while its source pane
    ///   floats;
    /// - for an unavailable preview, any tab layout or the floating panes
    ///   changed;
    /// - a changed client's state differs from the state the preview read
    ///   ([`Self::is_client_input_changed`]).
    fn is_stale(
        &self,
        acting_client_id: ClientId,
        session: &Session,
        view_changes: &SessionViewChanges,
    ) -> bool {
        if view_changes.is_tab_bar_changed || view_changes.is_pane_sizing_changed {
            return true;
        }
        let layout_changed_tab_ids = &view_changes.layout_changed_tab_ids;
        let is_read_layout_changed = layout_changed_tab_ids
            .contains(&self.placement_target.destination_tab_id)
            || match self.placement_preview.as_ref() {
                PlacementPreview::Available(placement_snapshot) => {
                    match placement_snapshot.find_source_tab_snapshot() {
                        Some(source_tab_snapshot) => layout_changed_tab_ids
                            .contains(&source_tab_snapshot.tab_snapshot.tab_id),
                        None => view_changes.is_floating_set_changed,
                    }
                }
                PlacementPreview::Unavailable { .. } => {
                    !layout_changed_tab_ids.is_empty() || view_changes.is_floating_set_changed
                }
            };
        is_read_layout_changed
            || view_changes
                .changed_client_ids
                .iter()
                .any(|&changed_client_id| {
                    self.is_client_input_changed(changed_client_id, acting_client_id, session)
                })
    }

    /// Whether the state of `changed_client_id` in `session` differs from the
    /// state the preview read of it:
    /// - a client the preview read that left the session, or whose
    ///   [`PlacementClientInputs`] differ. An unavailable preview reads the
    ///   acting client only;
    /// - for the acting client, also a focus history in the source or the
    ///   destination tab that differs from the preview's
    ///   ([`is_focus_history_changed`]);
    /// - for an available preview, a client the preview did not read that the
    ///   preview reads now ([`PlacementClientInputs::is_read_by_placement`]).
    fn is_client_input_changed(
        &self,
        changed_client_id: ClientId,
        acting_client_id: ClientId,
        session: &Session,
    ) -> bool {
        let changed_client = session.clients.get_client_by_id(changed_client_id);
        let changed_client_inputs = changed_client.map(PlacementClientInputs::from_client);
        let PlacementPreview::Available(placement_snapshot) = self.placement_preview.as_ref()
        else {
            return self
                .client_inputs_by_client_id
                .get(&changed_client_id)
                .is_some_and(|read_client_inputs| {
                    changed_client_inputs.as_ref() != Some(read_client_inputs)
                });
        };
        match (
            self.client_inputs_by_client_id.get(&changed_client_id),
            changed_client,
        ) {
            (Some(read_client_inputs), Some(changed_client)) => {
                changed_client_inputs.as_ref() != Some(read_client_inputs)
                    || (changed_client_id == acting_client_id
                        && is_focus_history_changed(placement_snapshot, session, changed_client))
            }
            (Some(_), None) => true,
            (None, Some(_)) => changed_client_inputs.is_some_and(|changed_client_inputs| {
                changed_client_inputs.is_read_by_placement(placement_snapshot)
            }),
            (None, None) => false,
        }
    }
}

/// The state of one client that a placement preview reads: the tab it views,
/// the cells its panes get, and the pane that takes its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlacementClientInputs {
    active_tab_id: TabId,
    pane_area: Option<Size>,
    active_focused_pane_id: Option<PaneId>,
}

impl PlacementClientInputs {
    /// The state of `client` that a placement preview reads.
    fn from_client(client: &Client) -> Self {
        PlacementClientInputs {
            active_tab_id: client.get_active_tab_id(),
            pane_area: client.get_pane_area(),
            active_focused_pane_id: client.get_active_focused_pane_id(),
        }
    }

    /// Whether the preview of `placement_snapshot` reads a client in this
    /// state: a client that views the source or the destination tab, or whose
    /// input is on the source pane. These are the clients
    /// [`compute_placement_tab_sizes`] sizes a tab by or moves.
    fn is_read_by_placement(&self, placement_snapshot: &PlacementSnapshot) -> bool {
        self.active_tab_id == placement_snapshot.destination_tab_id
            || self.active_focused_pane_id == Some(placement_snapshot.source_pane_id)
            || placement_snapshot
                .find_source_tab_snapshot()
                .is_some_and(|source_tab_snapshot| {
                    source_tab_snapshot.tab_snapshot.tab_id == self.active_tab_id
                })
    }
}

impl Server {
    /// Name `placement_target` as the pane placement preview of `client_id`,
    /// or end its preview with `None`, and mark the client's frame changed
    /// when its preview changed. A named target builds its preview at once
    /// ([`build_placement_preview`]). A target equal to the one the client
    /// names changes nothing. A client attached to no session holds no
    /// preview.
    pub(crate) fn set_pane_placement_preview(
        &mut self,
        client_id: ClientId,
        placement_target: Option<PanePlacementPreviewTarget>,
    ) {
        let Some(placement_target) = placement_target else {
            if self
                .placement_preview_by_client_id
                .remove(&client_id)
                .is_some()
            {
                self.mark_client_view_changed(client_id);
            }
            return;
        };
        let is_target_named = self
            .placement_preview_by_client_id
            .get(&client_id)
            .is_some_and(|client_placement_preview| {
                client_placement_preview.placement_target == placement_target
            });
        if is_target_named {
            return;
        }
        match self.build_client_placement_preview(client_id, placement_target) {
            Some(mut client_placement_preview) => {
                client_placement_preview.preview_pane_ids =
                    list_preview_pane_ids(&client_placement_preview.placement_preview);
                self.placement_preview_by_client_id
                    .insert(client_id, client_placement_preview);
            }
            None => {
                self.placement_preview_by_client_id.remove(&client_id);
            }
        }
        self.mark_client_view_changed(client_id);
    }

    /// Build again each preview that reads a state that
    /// `view_changes_by_session_id` lists ([`ClientPlacementPreview::is_stale`]),
    /// and the preview of each client whose session this server no longer
    /// holds. A rebuilt preview that differs from the held one replaces it
    /// and marks the client's frame changed. A rebuilt preview equal to the
    /// held one keeps the held `Arc` and marks nothing. A client attached to
    /// no session loses its preview, and its frame is marked changed.
    pub(crate) fn update_changed_placement_previews(
        &mut self,
        view_changes_by_session_id: &HashMap<SessionId, SessionViewChanges>,
    ) {
        let stale_placement_targets: Vec<(ClientId, PanePlacementPreviewTarget)> = self
            .placement_preview_by_client_id
            .iter()
            .filter(|&(&client_id, client_placement_preview)| {
                match self.session_by_id.get(&client_placement_preview.session_id) {
                    Some(session) => view_changes_by_session_id
                        .get(&client_placement_preview.session_id)
                        .is_some_and(|view_changes| {
                            client_placement_preview.is_stale(client_id, session, view_changes)
                        }),
                    None => true,
                }
            })
            .map(|(&client_id, client_placement_preview)| {
                (client_id, client_placement_preview.placement_target)
            })
            .collect();
        for (client_id, placement_target) in stale_placement_targets {
            let Some(mut rebuilt_placement_preview) =
                self.build_client_placement_preview(client_id, placement_target)
            else {
                self.placement_preview_by_client_id.remove(&client_id);
                self.mark_client_view_changed(client_id);
                continue;
            };
            let is_preview_changed = match self.placement_preview_by_client_id.get(&client_id) {
                Some(held_placement_preview)
                    if held_placement_preview.placement_preview
                        == rebuilt_placement_preview.placement_preview =>
                {
                    rebuilt_placement_preview.placement_preview =
                        Arc::clone(&held_placement_preview.placement_preview);
                    rebuilt_placement_preview
                        .preview_pane_ids
                        .clone_from(&held_placement_preview.preview_pane_ids);
                    false
                }
                _ => {
                    rebuilt_placement_preview.preview_pane_ids =
                        list_preview_pane_ids(&rebuilt_placement_preview.placement_preview);
                    true
                }
            };
            self.placement_preview_by_client_id
                .insert(client_id, rebuilt_placement_preview);
            if is_preview_changed {
                self.mark_client_view_changed(client_id);
            }
        }
    }

    /// The preview of `placement_target` for `client_id` in the session the
    /// client is attached to, with the state it read of each client it
    /// depends on and no preview pane, or `None` when the client is attached
    /// to no session.
    fn build_client_placement_preview(
        &self,
        client_id: ClientId,
        placement_target: PanePlacementPreviewTarget,
    ) -> Option<ClientPlacementPreview> {
        let session = self.get_session_for_client(client_id)?;
        let acting_client = session.clients.get_client_by_id(client_id)?;
        let placement_preview = build_placement_preview(session, acting_client, placement_target);
        let mut client_inputs_by_client_id = BTreeMap::new();
        for client in session.clients.list_attached_clients() {
            let client_inputs = PlacementClientInputs::from_client(client);
            let is_client_read = match &placement_preview {
                PlacementPreview::Available(placement_snapshot) => {
                    client_inputs.is_read_by_placement(placement_snapshot)
                }
                PlacementPreview::Unavailable { .. } => false,
            };
            if client.get_client_id() == client_id || is_client_read {
                client_inputs_by_client_id.insert(client.get_client_id(), client_inputs);
            }
        }
        Some(ClientPlacementPreview {
            session_id: session.session_id,
            placement_target,
            placement_preview: Arc::new(placement_preview),
            client_inputs_by_client_id,
            preview_pane_ids: Vec::new(),
        })
    }
}

/// The preview of `placement_target` as `acting_client` sees it in `session`.
/// It reads live state only; it changes no tab, client, pane, focus or
/// terminal size.
///
/// [`PlacementPreview::Unavailable`] when `session` holds the source pane in
/// no tab and among no floating panes, or holds no destination tab.
///
/// A tiled source pane carries its tab ([`PlacementSource::Tiled`]). A
/// floating source pane carries none ([`PlacementSource::Floating`]), and the
/// destination tab is always carried for it.
///
/// The tab sizes come from [`compute_placement_tab_sizes`], the function the
/// placement command checks and solves with: `insertion_tab_sizes` for a split
/// or a stack, and one `swap_tab_sizes` entry per destination pane when the
/// source pane is tiled in another tab. Each tab is solved at the size of
/// `insertion_tab_sizes`: the destination tab at its destination size, else,
/// when the source tab is the destination tab, at the acting client's pane
/// area, else at `0x0`; a source tab in another tab at its source size, else
/// at `0x0`. Each tab is solved with the session's pane sizing
/// ([`build_placement_tab_snapshot`]).
fn build_placement_preview(
    session: &Session,
    acting_client: &Client,
    placement_target: PanePlacementPreviewTarget,
) -> PlacementPreview {
    let PanePlacementPreviewTarget {
        source_pane_id,
        destination_tab_id,
    } = placement_target;
    let unavailable_preview = PlacementPreview::Unavailable {
        source_pane_id,
        destination_tab_id,
    };
    let source_tab = session.find_tab_by_pane_id(source_pane_id);
    if source_tab.is_none() && !session.floating_set.has_pane(source_pane_id) {
        return unavailable_preview;
    }
    let Some(destination_tab) = session.tabs.get(&destination_tab_id) else {
        return unavailable_preview;
    };
    let acting_client_id = acting_client.get_client_id();
    let pane_sizing = session.get_pane_sizing();
    let is_destination_source_tab =
        source_tab.is_some_and(|source_tab| source_tab.get_tab_id() == destination_tab_id);
    let insertion_tab_sizes = compute_placement_tab_sizes(
        session,
        source_pane_id,
        destination_tab_id,
        None,
        acting_client_id,
    );
    let swap_tab_sizes: Vec<SwapTabSizes> = if source_tab.is_some() && !is_destination_source_tab {
        destination_tab
            .get_layout_tree()
            .list_leaf_pane_ids()
            .into_iter()
            .map(|target_pane_id| SwapTabSizes {
                target_pane_id,
                tab_sizes: compute_placement_tab_sizes(
                    session,
                    source_pane_id,
                    destination_tab_id,
                    Some(target_pane_id),
                    acting_client_id,
                ),
            })
            .collect()
    } else {
        Vec::new()
    };
    let empty_tab_size = Size {
        column_count: 0,
        row_count: 0,
    };
    let destination_tab_size = match insertion_tab_sizes.destination_tab_size {
        Some(destination_tab_size) => destination_tab_size,
        None if is_destination_source_tab => {
            acting_client.get_pane_area().unwrap_or(empty_tab_size)
        }
        None => empty_tab_size,
    };
    let source = match source_tab {
        Some(source_tab) => {
            let source_tab_size = if is_destination_source_tab {
                destination_tab_size
            } else {
                insertion_tab_sizes
                    .source_tab_size
                    .unwrap_or(empty_tab_size)
            };
            PlacementSource::Tiled(build_placement_tab_snapshot(
                acting_client,
                source_tab,
                source_tab_size,
                pane_sizing,
            ))
        }
        None => PlacementSource::Floating,
    };
    let destination_tab_snapshot = (!is_destination_source_tab).then(|| {
        build_placement_tab_snapshot(
            acting_client,
            destination_tab,
            destination_tab_size,
            pane_sizing,
        )
    });
    PlacementPreview::Available(Box::new(PlacementSnapshot {
        source_pane_id,
        source,
        destination_tab_id,
        destination_tab_snapshot,
        pane_sizing,
        insertion_tab_sizes,
        swap_tab_sizes,
    }))
}

/// One tab of a preview: `tab`'s tree, `acting_client`'s focus history in the
/// tab ([`list_tab_focus_history`]), and the tree solved tiled over a
/// `tab_size` rectangle at the origin with `pane_sizing`, each stack showing
/// the member that history gives
/// ([`LayoutNode::build_shown_layout_tree`](koshi_layout::tree::LayoutNode::build_shown_layout_tree)).
/// A zoom `acting_client` holds in the tab is left out.
fn build_placement_tab_snapshot(
    acting_client: &Client,
    tab: &Tab,
    tab_size: Size,
    pane_sizing: PaneSizing,
) -> PlacementTabSnapshot {
    let layout_tree = tab.get_layout_tree();
    let focus_history = list_tab_focus_history(acting_client, tab);
    let shown_layout_tree = layout_tree.build_shown_layout_tree(&focus_history);
    let layout_solve = solve_layout_with_sizing(
        shown_layout_tree.as_ref().unwrap_or(layout_tree),
        Rect::from_size_at_origin(tab_size),
        pane_sizing,
    );
    PlacementTabSnapshot {
        layout_tree: layout_tree.clone(),
        focus_history,
        tab_snapshot: TabSnapshot {
            tab_id: tab.get_tab_id(),
            tab_name: tab.get_tab_name().to_owned(),
            pane_slots: list_pane_slots(&layout_solve),
            tab_size,
            stack_headers: layout_solve.stack_headers,
            layout_mode: LayoutMode::Tiled,
            is_every_pane_suppressed: layout_solve.is_every_pane_suppressed,
            gap_cell_count: pane_sizing.gap_cell_count,
        },
    }
}

/// `client`'s focus history in `tab`, newest first, with only the panes of
/// `tab`'s tree.
fn list_tab_focus_history(client: &Client, tab: &Tab) -> Vec<PaneId> {
    let leaf_pane_ids: HashSet<PaneId> = tab
        .get_layout_tree()
        .list_leaf_pane_ids()
        .into_iter()
        .collect();
    client
        .list_focus_history(tab.get_tab_id())
        .iter()
        .copied()
        .filter(|pane_id| leaf_pane_ids.contains(pane_id))
        .collect()
}

/// Whether `acting_client`'s focus history in the source or the destination
/// tab of `placement_snapshot` ([`list_tab_focus_history`]) differs from the
/// history the preview carries for that tab, or `session` no longer holds
/// that tab.
fn is_focus_history_changed(
    placement_snapshot: &PlacementSnapshot,
    session: &Session,
    acting_client: &Client,
) -> bool {
    placement_snapshot
        .find_source_tab_snapshot()
        .into_iter()
        .chain(placement_snapshot.destination_tab_snapshot.as_ref())
        .any(|placement_tab_snapshot| {
            session
                .tabs
                .get(&placement_tab_snapshot.tab_snapshot.tab_id)
                .is_none_or(|tab| {
                    list_tab_focus_history(acting_client, tab)
                        != placement_tab_snapshot.focus_history
                })
        })
}

/// The panes whose content a frame carries for `placement_preview`:
/// [`PlacementSnapshot::list_drawable_pane_ids`] of an available preview, and
/// none for an unavailable one.
fn list_preview_pane_ids(placement_preview: &PlacementPreview) -> Vec<PaneId> {
    match placement_preview {
        PlacementPreview::Available(placement_snapshot) => {
            placement_snapshot.list_drawable_pane_ids()
        }
        PlacementPreview::Unavailable { .. } => Vec::new(),
    }
}

#[cfg(test)]
mod tests;
