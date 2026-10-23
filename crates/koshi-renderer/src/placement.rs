//! Pane placement composition: the tab a pane placement preview shows, as one
//! placement leaves it.
//!
//! A viewer draws the placement it previews from
//! [`PlacementSnapshot::build_placed_preview`], and confirms it with the
//! [`PlacedTabLayout`] the same call gives. The session commits a confirmed
//! placement only when its own solve of the tab equals that layout. The
//! session's frames carry the content of every pane that
//! [`PlacementSnapshot::list_drawable_pane_ids`] lists.
//!
//! Tab `[A | B]` previewed for the source pane `F` with a split `Right` of
//! `Pane(B)` gives the tab `[A | B | F]`, solved with `F` first in the acting
//! client's focus history.

use std::collections::HashSet;

use koshi_core::command::TiledPlacementTarget;
use koshi_core::geometry::{Direction, Rect, Size, SplitDirection};
use koshi_core::ids::{PaneId, TabId};
use koshi_layout::mode::LayoutMode;
use koshi_layout::placement::{
    list_placement_destinations, place_floating_pane_in_tab, place_pane_across_tabs,
    place_pane_within_tab, solve_placed_tab, PlacedTabLayout, PlacementDestinations,
    PlacementError, PlacementTarget,
};
use koshi_layout::solver::{is_layout_within_rect, solve_layout_with_sizing};
use koshi_layout::tree::LayoutNode;

use crate::snapshot::{
    list_pane_slots, PlacementSnapshot, PlacementSource, PlacementTabSnapshot, TabSnapshot,
};

/// The four sides a split may put the source pane on, in the order
/// [`PlacementSnapshot::list_placement_targets`] lists them.
const SPLIT_DIRECTIONS: [Direction; 4] = [
    Direction::Left,
    Direction::Right,
    Direction::Up,
    Direction::Down,
];

/// One placement of a preview's source pane, composed from the preview's
/// layouts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedPreview {
    /// The displayed tab as the placement leaves it: its tree, the acting
    /// client's focus history in it with the placed pane first, and its
    /// solve.
    pub placed_tab_snapshot: PlacementTabSnapshot,
    /// The displayed tab as the acting client draws it after the placement
    /// ([`solve_placed_tab`]). A confirmed placement carries it.
    pub placed_tab_layout: PlacedTabLayout,
}

/// The refusal the session gives a placement target that a preview names
/// ([`PlacementSnapshot::build_placed_preview`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementPreviewError {
    /// `tab_id` has no room for the layout the placement gives it: the
    /// destination tab with the source pane placed, or, for a placement into
    /// another tab, the source tab without the source pane.
    NoRoom {
        /// The tab with no room.
        tab_id: TabId,
    },
    /// The preview holds no destination tab size: no client that then views
    /// the destination tab has a pane area.
    NoDestinationTabSize,
    /// The preview holds nothing to check the target against: a split names
    /// another tab than the destination tab, a swap into another tab names a
    /// pane with no tab sizes, or the preview shows no tab.
    UnpreviewedTarget,
    /// The layout functions refuse the target for another reason than room.
    LayoutRefusal(PlacementError),
}

impl PlacementSnapshot {
    /// Every destination the source pane may name in the displayed tab
    /// ([`Self::find_displayed_tab_snapshot`]), as that tab draws: each stack
    /// shows the member the tab's focus history gives
    /// ([`LayoutNode::build_shown_layout_tree`]), solved tiled at the tab's
    /// size ([`list_placement_destinations`]). `None` when the preview holds
    /// no displayed tab.
    #[must_use]
    pub fn list_placement_destinations(&self) -> Option<PlacementDestinations> {
        let displayed_tab_snapshot = self.find_displayed_tab_snapshot()?;
        let tab_rect = Rect::from_size_at_origin(displayed_tab_snapshot.tab_snapshot.tab_size);
        let shown_layout_tree = displayed_tab_snapshot
            .layout_tree
            .build_shown_layout_tree(&displayed_tab_snapshot.focus_history);
        let drawn_layout_tree: &LayoutNode = shown_layout_tree
            .as_ref()
            .unwrap_or(&displayed_tab_snapshot.layout_tree);
        let layout_solve = solve_layout_with_sizing(drawn_layout_tree, tab_rect, self.pane_sizing);
        Some(list_placement_destinations(
            drawn_layout_tree,
            &layout_solve,
            tab_rect,
            self.source_pane_id,
        ))
    }

    /// Every tiled placement target the displayed tab's destinations name
    /// ([`Self::list_placement_destinations`]): a swap, then a stack, with
    /// each target pane, then a split of each insertion span toward `Left`,
    /// `Right`, `Up` and `Down`. Empty when the preview holds no displayed
    /// tab.
    #[must_use]
    pub fn list_placement_targets(&self) -> Vec<TiledPlacementTarget> {
        let Some(placement_destinations) = self.list_placement_destinations() else {
            return Vec::new();
        };
        let mut placement_targets = Vec::new();
        for target_pane_slot in &placement_destinations.target_pane_slots {
            placement_targets.push(TiledPlacementTarget::Swap {
                target_pane_id: target_pane_slot.pane_id,
            });
            placement_targets.push(TiledPlacementTarget::Stack {
                target_pane_id: target_pane_slot.pane_id,
            });
        }
        for insertion_span in &placement_destinations.insertion_spans {
            for direction in SPLIT_DIRECTIONS {
                placement_targets.push(TiledPlacementTarget::Split {
                    destination_tab_id: self.destination_tab_id,
                    anchor: insertion_span.anchor.clone(),
                    direction,
                });
            }
        }
        placement_targets
    }

    /// The displayed tab as `placement_target` leaves it, built with the
    /// layout functions the session commits with: [`place_pane_within_tab`]
    /// within the source pane's tab, [`place_pane_across_tabs`] into another
    /// tab, and [`place_floating_pane_in_tab`] for a floating source pane. The
    /// placed tab is solved with [`solve_placed_tab`] at the tab size the
    /// session checks the placement at: the swap target's entry of
    /// `swap_tab_sizes` for a swap into another tab, else
    /// `insertion_tab_sizes`.
    ///
    /// `[A | B]` with `A` swapped with `B` gives `[B | A]`, with `A` first in
    /// the focus history.
    ///
    /// # Errors
    ///
    /// The refusal the session gives the placement:
    /// - [`PlacementPreviewError::NoRoom`]: the placed tree does not fit the
    ///   destination tab size, or, into another tab, the source tree the source
    ///   pane leaves does not fit the source tab size
    ///   ([`is_layout_within_rect`]);
    /// - [`PlacementPreviewError::NoDestinationTabSize`]: the tab sizes hold no
    ///   destination tab size;
    /// - [`PlacementPreviewError::UnpreviewedTarget`]: a split names another
    ///   tab than the destination tab, a swap into another tab names a pane
    ///   with no `swap_tab_sizes` entry, or the preview shows no tab;
    /// - [`PlacementPreviewError::LayoutRefusal`]: the layout function refuses
    ///   the placement for another reason than room.
    pub fn build_placed_preview(
        &self,
        placement_target: &TiledPlacementTarget,
    ) -> Result<PlacedPreview, PlacementPreviewError> {
        if let TiledPlacementTarget::Split {
            destination_tab_id, ..
        } = placement_target
        {
            if *destination_tab_id != self.destination_tab_id {
                return Err(PlacementPreviewError::UnpreviewedTarget);
            }
        }
        let layout_target = PlacementTarget::from_tiled_placement_target(placement_target);
        let displayed_tab_snapshot = self
            .find_displayed_tab_snapshot()
            .ok_or(PlacementPreviewError::UnpreviewedTarget)?;
        let build_layout_refusal = |placement_error: PlacementError| match placement_error {
            PlacementError::DestinationTooSmall { .. } => PlacementPreviewError::NoRoom {
                tab_id: self.destination_tab_id,
            },
            other_placement_error => PlacementPreviewError::LayoutRefusal(other_placement_error),
        };
        let (placed_layout_tree, placed_tab_size) =
            match (&self.source, &self.destination_tab_snapshot) {
                (PlacementSource::Tiled(source_tab_snapshot), None) => {
                    let tab_size = self
                        .insertion_tab_sizes
                        .destination_tab_size
                        .ok_or(PlacementPreviewError::NoDestinationTabSize)?;
                    let placed_layout_tree = place_pane_within_tab(
                        &source_tab_snapshot.layout_tree,
                        self.source_pane_id,
                        &layout_target,
                        Rect::from_size_at_origin(tab_size),
                        self.pane_sizing,
                    )
                    .map_err(build_layout_refusal)?;
                    (placed_layout_tree, tab_size)
                }
                (PlacementSource::Tiled(source_tab_snapshot), Some(destination_tab_snapshot)) => {
                    let placement_tab_sizes = match placement_target {
                        TiledPlacementTarget::Swap { target_pane_id } => {
                            self.swap_tab_sizes
                                .iter()
                                .find(|swap_tab_sizes| {
                                    swap_tab_sizes.target_pane_id == *target_pane_id
                                })
                                .ok_or(PlacementPreviewError::UnpreviewedTarget)?
                                .tab_sizes
                        }
                        TiledPlacementTarget::Split { .. } | TiledPlacementTarget::Stack { .. } => {
                            self.insertion_tab_sizes
                        }
                    };
                    let tab_size = placement_tab_sizes
                        .destination_tab_size
                        .ok_or(PlacementPreviewError::NoDestinationTabSize)?;
                    let cross_tab_placement = place_pane_across_tabs(
                        &source_tab_snapshot.layout_tree,
                        self.source_pane_id,
                        &destination_tab_snapshot.layout_tree,
                        &layout_target,
                        Rect::from_size_at_origin(tab_size),
                        self.pane_sizing,
                    )
                    .map_err(build_layout_refusal)?;
                    if let (Some(placed_source_tree), Some(source_tab_size)) = (
                        cross_tab_placement.source_tree.as_ref(),
                        placement_tab_sizes.source_tab_size,
                    ) {
                        if !is_layout_within_rect(
                            placed_source_tree,
                            Rect::from_size_at_origin(source_tab_size),
                            self.pane_sizing,
                        ) {
                            return Err(PlacementPreviewError::NoRoom {
                                tab_id: source_tab_snapshot.tab_snapshot.tab_id,
                            });
                        }
                    }
                    (cross_tab_placement.destination_tree, tab_size)
                }
                (PlacementSource::Floating, Some(destination_tab_snapshot)) => {
                    let tab_size = self
                        .insertion_tab_sizes
                        .destination_tab_size
                        .ok_or(PlacementPreviewError::NoDestinationTabSize)?;
                    let placed_layout_tree = place_floating_pane_in_tab(
                        &destination_tab_snapshot.layout_tree,
                        self.source_pane_id,
                        &layout_target,
                        Rect::from_size_at_origin(tab_size),
                        self.pane_sizing,
                    )
                    .map_err(build_layout_refusal)?;
                    (placed_layout_tree, tab_size)
                }
                (PlacementSource::Floating, None) => {
                    return Err(PlacementPreviewError::UnpreviewedTarget)
                }
            };
        Ok(self.build_placed_tab(displayed_tab_snapshot, placed_layout_tree, placed_tab_size))
    }

    /// Every pane a frame draws for this preview, each once, in the order
    /// first drawn: each visible pane of the displayed tab, then each pane
    /// visible in the displayed tab after a placement of
    /// [`Self::list_placement_targets`] that the session accepts
    /// ([`Self::build_placed_preview`]). Empty when the preview holds no
    /// displayed tab.
    ///
    /// Only a pane of the displayed tab that it does not draw now, or the
    /// source pane, can be new in a placed tab. For a source pane outside the
    /// displayed tab, a pane that is a stack member on its own, outside any
    /// split inside the stack, and that its stack does not show, stays hidden
    /// in every placed tab. Placements are built in target order until every
    /// pane that can be new is listed, and none is built when no pane can be
    /// new. In `[A | B]`, previewed for a floating `F`, the first accepted
    /// placement lists `F` and no other placement is built.
    ///
    /// Tab `[A | stack(X, Y)]` with `X` shown, previewed for a pane `F` of
    /// another tab, lists `A`, `X` and `F`: no placement of `F` opens `Y`.
    #[must_use]
    pub fn list_drawable_pane_ids(&self) -> Vec<PaneId> {
        let Some(displayed_tab_snapshot) = self.find_displayed_tab_snapshot() else {
            return Vec::new();
        };
        let mut drawable_pane_ids: Vec<PaneId> = displayed_tab_snapshot
            .tab_snapshot
            .pane_slots
            .iter()
            .filter(|pane_slot| pane_slot.is_visible)
            .map(|pane_slot| pane_slot.pane_id)
            .collect();
        let layout_tree = &displayed_tab_snapshot.layout_tree;
        let mut hidden_pane_ids: HashSet<PaneId> =
            layout_tree.list_leaf_pane_ids().into_iter().collect();
        hidden_pane_ids.insert(self.source_pane_id);
        for drawable_pane_id in &drawable_pane_ids {
            hidden_pane_ids.remove(drawable_pane_id);
        }
        let mut pending_split_nodes = if layout_tree.has_pane(self.source_pane_id) {
            Vec::new()
        } else {
            vec![layout_tree]
        };
        while let Some(LayoutNode::Split(split_node)) = pending_split_nodes.pop() {
            let shown_child_index = (split_node.direction == SplitDirection::Stacked)
                .then(|| split_node.get_shown_child_index(&displayed_tab_snapshot.focus_history));
            for (child_index, child_node) in split_node.children.iter().enumerate() {
                let is_collapsed_stack_member = shown_child_index
                    .is_some_and(|shown_child_index| shown_child_index != child_index);
                match child_node {
                    LayoutNode::Split(_) => pending_split_nodes.push(child_node),
                    LayoutNode::Pane(pane_id) if is_collapsed_stack_member => {
                        hidden_pane_ids.remove(pane_id);
                    }
                    LayoutNode::Pane(_) => {}
                }
            }
        }
        if hidden_pane_ids.is_empty() {
            return drawable_pane_ids;
        }
        for placement_target in self.list_placement_targets() {
            let Ok(placed_preview) = self.build_placed_preview(&placement_target) else {
                continue;
            };
            for pane_slot in &placed_preview.placed_tab_snapshot.tab_snapshot.pane_slots {
                if pane_slot.is_visible && hidden_pane_ids.remove(&pane_slot.pane_id) {
                    drawable_pane_ids.push(pane_slot.pane_id);
                }
            }
            if hidden_pane_ids.is_empty() {
                break;
            }
        }
        drawable_pane_ids
    }

    /// `displayed_tab_snapshot` with `placed_layout_tree` solved at
    /// `placed_tab_size` with the source pane first in its focus history
    /// ([`solve_placed_tab`]).
    fn build_placed_tab(
        &self,
        displayed_tab_snapshot: &PlacementTabSnapshot,
        placed_layout_tree: LayoutNode,
        placed_tab_size: Size,
    ) -> PlacedPreview {
        let placed_tab_solve = solve_placed_tab(
            &placed_layout_tree,
            self.source_pane_id,
            &displayed_tab_snapshot.focus_history,
            placed_tab_size,
            self.pane_sizing,
        );
        let tab_id = displayed_tab_snapshot.tab_snapshot.tab_id;
        let layout_solve = placed_tab_solve.layout_solve;
        let placed_tab_snapshot = PlacementTabSnapshot {
            layout_tree: placed_layout_tree,
            focus_history: placed_tab_solve.focus_history,
            tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: displayed_tab_snapshot.tab_snapshot.tab_name.clone(),
                pane_slots: list_pane_slots(&layout_solve),
                tab_size: placed_tab_size,
                stack_headers: layout_solve.stack_headers.clone(),
                layout_mode: LayoutMode::Tiled,
                is_every_pane_suppressed: layout_solve.is_every_pane_suppressed,
                gap_cell_count: self.pane_sizing.gap_cell_count,
            },
        };
        PlacedPreview {
            placed_tab_snapshot,
            placed_tab_layout: PlacedTabLayout {
                tab_id,
                tab_size: placed_tab_size,
                layout_solve,
            },
        }
    }
}

#[cfg(test)]
mod tests;
