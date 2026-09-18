//! Pane placement: move one existing pane to a slot the user sees.
//!
//! Two operations, both pure edits over borrowed trees:
//!
//! - a **swap** exchanges two leaves and leaves every split, weight and
//!   active index where it was;
//! - an **insertion** takes the source leaf out of its tree and splits a
//!   chosen span — one pane, one visible group, or the whole tab — beside it,
//!   then normalizes the result once.
//!
//! A pane that lands inside a stack becomes that stack's expanded member.
//! Every failure returns a typed [`PlacementError`] and the caller's trees are
//! unchanged.
//!
//! [`list_placement_destinations`] lists, from a solved layout, every slot a
//! swap may target and every span an insertion may split.

use std::collections::{HashMap, HashSet};

use koshi_core::command::PanePlacementAnchor;
use koshi_core::error::{DomainCategory, DomainError, Severity};
use koshi_core::geometry::{Direction, Point, Rect, Size, SplitDirection};
use koshi_core::ids::PaneId;
use thiserror::Error;

use crate::edit::{remove_leaf, PaneRemovalStatus};
use crate::focus::activate_stack_member;
use crate::neighbor::{compute_bottom_edge, compute_right_edge};
use crate::normalize::normalize_layout_tree;
use crate::solver::{
    compute_minimum_size, is_content_visible, is_layout_within_rect, LayoutSolve, PaneSizing,
    StackHeader,
};
use crate::tree::{compute_split_direction, LayoutNode, SplitNode};

/// Where a placement puts the source pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementTarget {
    /// Exchange the source leaf with the leaf holding `target_pane_id`.
    Swap {
        /// The pane whose slot the source takes.
        target_pane_id: PaneId,
    },
    /// Remove the source leaf and split `anchor` so the source sits on its
    /// `direction` side: `Left` and `Up` put the source before the anchor,
    /// `Right` and `Down` after it.
    Insert {
        /// The span the source is inserted beside.
        anchor: PanePlacementAnchor,
        /// The side of `anchor` the source lands on.
        direction: Direction,
    },
}

/// A rejected placement. The caller's trees are unchanged in every case.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlacementError {
    /// The source pane is not a leaf of the source tree.
    #[error("pane {pane_id} is not in the source layout")]
    SourcePaneNotFound { pane_id: PaneId },
    /// The swap target, or a pane the anchor names, is not a leaf of the
    /// destination tree.
    #[error("pane {pane_id} is not in the destination layout")]
    TargetPaneNotFound { pane_id: PaneId },
    /// The anchor names only the source pane: `Pane(source)`, or a `Group`
    /// whose members are all the source.
    #[error("pane {pane_id} cannot be inserted beside itself")]
    AnchorIsSource { pane_id: PaneId },
    /// A `Group` anchor lists `pane_id` more than once.
    #[error("group lists pane {pane_id} more than once")]
    GroupPaneDuplicated { pane_id: PaneId },
    /// No split node of the destination tree has exactly `pane_ids` as its
    /// leaf set. An empty `Group` is reported with empty `pane_ids`.
    #[error("panes {pane_ids:?} are not one group of the destination layout")]
    GroupIsNotOneSubtree { pane_ids: Vec<PaneId> },
    /// The anchor sits inside a stack. `stack_pane_ids` is the leaf set of
    /// the outermost stack above it; an insertion beside the stack names that
    /// set as a `Group`.
    #[error("anchor is inside the stack holding {stack_pane_ids:?}")]
    AnchorInsideStack { stack_pane_ids: Vec<PaneId> },
    /// In a cross-tab placement the source pane is also a leaf of the
    /// destination tree, or the swap target is also a leaf of the source
    /// tree.
    #[error("pane {pane_id} is in both layouts")]
    PaneInBothTrees { pane_id: PaneId },
    /// The destination tree after the edit needs `required_size`, more than
    /// the `available_size` of the destination tab rectangle on at least one
    /// axis.
    #[error(
        "destination needs {} by {} cells, has {} by {}",
        required_size.column_count,
        required_size.row_count,
        available_size.column_count,
        available_size.row_count
    )]
    DestinationTooSmall {
        required_size: Size,
        available_size: Size,
    },
}

impl DomainError for PlacementError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Layout
    }

    fn get_severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// The two trees a cross-tab placement produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossTabPlacement {
    /// The source tab's tree without the moved pane. `None` when the moved
    /// pane was its only leaf.
    pub source_tree: Option<LayoutNode>,
    /// The destination tab's tree holding the moved pane.
    pub destination_tree: LayoutNode,
}

/// Place `source_pane_id` at `placement_target` inside its own tab.
///
/// `layout_tree` is the tab's tree, `tab_rect` the rectangle it solves into
/// and `pane_sizing` the caller's own [`PaneSizing`].
///
/// A swap with itself returns `layout_tree` unchanged. An insertion when the
/// source is the only leaf, anchored on the tab, returns the bare leaf.
///
/// `H[A, V[B, H[C, D]]]` with D swapped with B gives `H[A, V[D, H[C, B]]]`,
/// every weight in place.
///
/// # Errors
///
/// Every [`PlacementError`] variant except
/// [`PlacementError::PaneInBothTrees`]; see each variant for its condition.
pub fn place_pane_within_tab(
    layout_tree: &LayoutNode,
    source_pane_id: PaneId,
    placement_target: &PlacementTarget,
    tab_rect: Rect,
    pane_sizing: PaneSizing,
) -> Result<LayoutNode, PlacementError> {
    let source_path =
        layout_tree
            .find_pane_path(source_pane_id)
            .ok_or(PlacementError::SourcePaneNotFound {
                pane_id: source_pane_id,
            })?;
    match placement_target {
        PlacementTarget::Swap { target_pane_id } => {
            if *target_pane_id == source_pane_id {
                return Ok(layout_tree.clone());
            }
            let target_path = layout_tree.find_pane_path(*target_pane_id).ok_or(
                PlacementError::TargetPaneNotFound {
                    pane_id: *target_pane_id,
                },
            )?;
            let mut swapped_tree = layout_tree.clone();
            *swapped_tree.get_node_at_path_mut(&source_path) = LayoutNode::Pane(*target_pane_id);
            *swapped_tree.get_node_at_path_mut(&target_path) = LayoutNode::Pane(source_pane_id);
            expand_stacks_and_check_fit(swapped_tree, source_pane_id, tab_rect, pane_sizing)
        }
        PlacementTarget::Insert { anchor, direction } => {
            validate_anchor(layout_tree, source_pane_id, anchor)?;
            let mut removed_tree = layout_tree.clone();
            let destination_tree = match remove_leaf(&mut removed_tree, source_pane_id) {
                PaneRemovalStatus::Removed => removed_tree,
                PaneRemovalStatus::SubtreeEmpty => return Ok(LayoutNode::Pane(source_pane_id)),
                PaneRemovalStatus::PaneNotFound => unreachable!("presence checked above"),
            };
            let inserted_tree =
                insert_beside_anchor(destination_tree, source_pane_id, anchor, *direction);
            expand_stacks_and_check_fit(inserted_tree, source_pane_id, tab_rect, pane_sizing)
        }
    }
}

/// Place `source_pane_id`, a leaf of `source_tree`, at `placement_target` in
/// `destination_tree`, a different tab's tree.
///
/// `destination_tab_rect` is the rectangle the destination tree solves into
/// and `pane_sizing` the caller's own [`PaneSizing`].
///
/// A swap exchanges the source leaf with the target leaf across the two
/// trees. An insertion removes the source leaf from `source_tree`, normalizes
/// what remains, and splits the anchor in `destination_tree`; when the
/// source was its tree's only leaf, [`CrossTabPlacement::source_tree`] is
/// `None`.
///
/// Source `Pane(D)` and destination `H[A, V[B, C]]`, D inserted `Right` of
/// `Pane(C)`: source `None`, destination `H[A, V[B, H[C, D]]]`.
///
/// # Errors
///
/// Every [`PlacementError`] variant; see each variant for its condition.
pub fn place_pane_across_tabs(
    source_tree: &LayoutNode,
    source_pane_id: PaneId,
    destination_tree: &LayoutNode,
    placement_target: &PlacementTarget,
    destination_tab_rect: Rect,
    pane_sizing: PaneSizing,
) -> Result<CrossTabPlacement, PlacementError> {
    if destination_tree.contains_pane(source_pane_id) {
        return Err(PlacementError::PaneInBothTrees {
            pane_id: source_pane_id,
        });
    }
    let source_path =
        source_tree
            .find_pane_path(source_pane_id)
            .ok_or(PlacementError::SourcePaneNotFound {
                pane_id: source_pane_id,
            })?;
    match placement_target {
        PlacementTarget::Swap { target_pane_id } => {
            if source_tree.contains_pane(*target_pane_id) {
                return Err(PlacementError::PaneInBothTrees {
                    pane_id: *target_pane_id,
                });
            }
            let target_path = destination_tree.find_pane_path(*target_pane_id).ok_or(
                PlacementError::TargetPaneNotFound {
                    pane_id: *target_pane_id,
                },
            )?;
            let mut swapped_source_tree = source_tree.clone();
            *swapped_source_tree.get_node_at_path_mut(&source_path) =
                LayoutNode::Pane(*target_pane_id);
            let mut swapped_destination_tree = destination_tree.clone();
            *swapped_destination_tree.get_node_at_path_mut(&target_path) =
                LayoutNode::Pane(source_pane_id);
            let destination_tree = expand_stacks_and_check_fit(
                swapped_destination_tree,
                source_pane_id,
                destination_tab_rect,
                pane_sizing,
            )?;
            Ok(CrossTabPlacement {
                source_tree: Some(swapped_source_tree),
                destination_tree,
            })
        }
        PlacementTarget::Insert { anchor, direction } => {
            validate_anchor(destination_tree, source_pane_id, anchor)?;
            let mut removed_source_tree = source_tree.clone();
            let remaining_source_tree = match remove_leaf(&mut removed_source_tree, source_pane_id)
            {
                PaneRemovalStatus::Removed => Some(normalize_all_leaves(&removed_source_tree)),
                PaneRemovalStatus::SubtreeEmpty => None,
                PaneRemovalStatus::PaneNotFound => unreachable!("presence checked above"),
            };
            let inserted_tree =
                insert_beside_anchor(destination_tree.clone(), source_pane_id, anchor, *direction);
            let destination_tree = expand_stacks_and_check_fit(
                inserted_tree,
                source_pane_id,
                destination_tab_rect,
                pane_sizing,
            )?;
            Ok(CrossTabPlacement {
                source_tree: remaining_source_tree,
                destination_tree,
            })
        }
    }
}

/// Check `anchor` against `destination_tree` as it stands before the source
/// leaf is removed.
///
/// # Errors
///
/// - [`PlacementError::AnchorIsSource`]: `Pane(source_pane_id)`, or a
///   `Group` whose members are all `source_pane_id`.
/// - [`PlacementError::TargetPaneNotFound`]: a named pane is not a leaf.
/// - [`PlacementError::GroupPaneDuplicated`]: a `Group` repeats an id.
/// - [`PlacementError::GroupIsNotOneSubtree`]: no split node has exactly the
///   `Group`'s leaf set.
/// - [`PlacementError::AnchorInsideStack`]: the anchor node has a stacked
///   ancestor.
fn validate_anchor(
    destination_tree: &LayoutNode,
    source_pane_id: PaneId,
    anchor: &PanePlacementAnchor,
) -> Result<(), PlacementError> {
    let anchor_path = match anchor {
        PanePlacementAnchor::Tab => return Ok(()),
        PanePlacementAnchor::Pane(anchor_pane_id) => {
            if *anchor_pane_id == source_pane_id {
                return Err(PlacementError::AnchorIsSource {
                    pane_id: source_pane_id,
                });
            }
            destination_tree.find_pane_path(*anchor_pane_id).ok_or(
                PlacementError::TargetPaneNotFound {
                    pane_id: *anchor_pane_id,
                },
            )?
        }
        PanePlacementAnchor::Group(group_pane_ids) => {
            find_group_anchor_path(destination_tree, source_pane_id, group_pane_ids)?
        }
    };
    match find_outermost_stack_above(destination_tree, &anchor_path) {
        Some(stack_pane_ids) => Err(PlacementError::AnchorInsideStack { stack_pane_ids }),
        None => Ok(()),
    }
}

/// The path of the split node in `destination_tree` whose leaf set is
/// exactly `group_pane_ids`.
///
/// # Errors
///
/// - [`PlacementError::GroupIsNotOneSubtree`]: `group_pane_ids` is empty, or
///   no split node has exactly that leaf set.
/// - [`PlacementError::GroupPaneDuplicated`]: an id appears twice.
/// - [`PlacementError::TargetPaneNotFound`]: an id is not a leaf.
/// - [`PlacementError::AnchorIsSource`]: every id is `source_pane_id`.
fn find_group_anchor_path(
    destination_tree: &LayoutNode,
    source_pane_id: PaneId,
    group_pane_ids: &[PaneId],
) -> Result<Vec<usize>, PlacementError> {
    if group_pane_ids.is_empty() {
        return Err(PlacementError::GroupIsNotOneSubtree {
            pane_ids: Vec::new(),
        });
    }
    let mut seen_pane_ids: HashSet<PaneId> = HashSet::with_capacity(group_pane_ids.len());
    for &group_pane_id in group_pane_ids {
        if !seen_pane_ids.insert(group_pane_id) {
            return Err(PlacementError::GroupPaneDuplicated {
                pane_id: group_pane_id,
            });
        }
        if !destination_tree.contains_pane(group_pane_id) {
            return Err(PlacementError::TargetPaneNotFound {
                pane_id: group_pane_id,
            });
        }
    }
    if group_pane_ids
        .iter()
        .all(|&group_pane_id| group_pane_id == source_pane_id)
    {
        return Err(PlacementError::AnchorIsSource {
            pane_id: source_pane_id,
        });
    }
    find_split_path_by_leaf_set(destination_tree, group_pane_ids).ok_or_else(|| {
        PlacementError::GroupIsNotOneSubtree {
            pane_ids: group_pane_ids.to_vec(),
        }
    })
}

/// Split the node `anchor` names in `destination_tree` — a tree the source
/// leaf has already been removed from — so `source_pane_id` sits on its
/// `direction` side, then normalize the whole tree once.
///
/// `anchor` was validated by [`validate_anchor`] against the tree before the
/// removal. A `Group` resolves to the shallowest node whose leaf set is the
/// group minus the source pane.
fn insert_beside_anchor(
    mut destination_tree: LayoutNode,
    source_pane_id: PaneId,
    anchor: &PanePlacementAnchor,
    direction: Direction,
) -> LayoutNode {
    let anchor_path = match anchor {
        PanePlacementAnchor::Tab => Vec::new(),
        PanePlacementAnchor::Pane(anchor_pane_id) => destination_tree
            .find_pane_path(*anchor_pane_id)
            .expect("anchor pane validated before the source leaf was removed"),
        PanePlacementAnchor::Group(group_pane_ids) => {
            let remaining_group_pane_ids: Vec<PaneId> = group_pane_ids
                .iter()
                .copied()
                .filter(|&group_pane_id| group_pane_id != source_pane_id)
                .collect();
            find_split_path_by_leaf_set(&destination_tree, &remaining_group_pane_ids)
                .expect("anchor group validated before the source leaf was removed")
        }
    };
    let anchor_node_slot = destination_tree.get_node_at_path_mut(&anchor_path);
    let anchor_subtree = std::mem::replace(anchor_node_slot, LayoutNode::Pane(source_pane_id));
    let source_leaf = LayoutNode::Pane(source_pane_id);
    let children = match direction {
        Direction::Right | Direction::Down => vec![anchor_subtree, source_leaf],
        Direction::Left | Direction::Up => vec![source_leaf, anchor_subtree],
    };
    *anchor_node_slot = LayoutNode::Split(SplitNode::with_equal_weights(
        compute_split_direction(direction),
        children,
    ));

    normalize_all_leaves(&destination_tree)
}

/// [`normalize_layout_tree`] with every leaf of `layout_tree` counted as
/// live. `layout_tree` holds at least one leaf.
fn normalize_all_leaves(layout_tree: &LayoutNode) -> LayoutNode {
    let live_pane_ids: HashSet<PaneId> = layout_tree.list_leaf_pane_ids().into_iter().collect();
    normalize_layout_tree(layout_tree, &live_pane_ids).expect("the tree holds a leaf")
}

/// Expand every stack above `source_pane_id` in `destination_tree`, then
/// check the tree fits `destination_tab_rect`.
///
/// # Errors
///
/// [`PlacementError::DestinationTooSmall`] when
/// [`is_layout_within_rect`] is false for the expanded tree.
fn expand_stacks_and_check_fit(
    mut destination_tree: LayoutNode,
    source_pane_id: PaneId,
    destination_tab_rect: Rect,
    pane_sizing: PaneSizing,
) -> Result<LayoutNode, PlacementError> {
    expand_stacks_holding_pane(&mut destination_tree, source_pane_id);
    if !is_layout_within_rect(&destination_tree, destination_tab_rect, pane_sizing) {
        return Err(PlacementError::DestinationTooSmall {
            required_size: compute_minimum_size(&destination_tree, pane_sizing),
            available_size: destination_tab_rect.cell_size,
        });
    }
    Ok(destination_tree)
}

/// Make the member holding `pane_id` the active member of every stack on the
/// path from the root to `pane_id`, outermost first. A pane under no stack
/// leaves the tree unchanged.
fn expand_stacks_holding_pane(layout_tree: &mut LayoutNode, pane_id: PaneId) {
    let Some(pane_path) = layout_tree.find_pane_path(pane_id) else {
        return;
    };
    for stack_depth in 0..pane_path.len() {
        let LayoutNode::Split(split) = layout_tree.get_node_at_path_mut(&pane_path[..stack_depth])
        else {
            unreachable!("every prefix of a pane path ends at a split");
        };
        if split.direction == SplitDirection::Stacked {
            activate_stack_member(split, pane_id);
        }
    }
}

/// The path of the shallowest node in `layout_tree` whose leaf set, as a
/// set, equals `pane_ids`. Pre-order: an ancestor is found before a
/// descendant with the same leaves. `None` when no node matches.
fn find_split_path_by_leaf_set(
    layout_tree: &LayoutNode,
    pane_ids: &[PaneId],
) -> Option<Vec<usize>> {
    fn find_in_subtree(
        layout_node: &LayoutNode,
        wanted_pane_ids: &[PaneId],
        node_path: &mut Vec<usize>,
    ) -> bool {
        let mut leaf_pane_ids = layout_node.list_leaf_pane_ids();
        if leaf_pane_ids.len() == wanted_pane_ids.len() {
            leaf_pane_ids.sort_unstable();
            if leaf_pane_ids == wanted_pane_ids {
                return true;
            }
        }
        let LayoutNode::Split(split) = layout_node else {
            return false;
        };
        for (child_index, child_node) in split.children.iter().enumerate() {
            node_path.push(child_index);
            if find_in_subtree(child_node, wanted_pane_ids, node_path) {
                return true;
            }
            node_path.pop();
        }
        false
    }

    let mut wanted_pane_ids = pane_ids.to_vec();
    wanted_pane_ids.sort_unstable();
    let mut node_path = Vec::new();
    find_in_subtree(layout_tree, &wanted_pane_ids, &mut node_path).then_some(node_path)
}

/// The leaf set of the outermost stacked split strictly above the node at
/// `node_path`, or `None` when no ancestor is a stack.
fn find_outermost_stack_above(
    layout_tree: &LayoutNode,
    node_path: &[usize],
) -> Option<Vec<PaneId>> {
    (0..node_path.len()).find_map(|ancestor_depth| {
        let ancestor_node = layout_tree.get_node_at_path(&node_path[..ancestor_depth]);
        match ancestor_node {
            LayoutNode::Split(split) if split.direction == SplitDirection::Stacked => {
                Some(ancestor_node.list_leaf_pane_ids())
            }
            _ => None,
        }
    })
}

/// One slot a swap may target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapSlot {
    /// The pane in the slot.
    pub pane_id: PaneId,
    /// The slot's drawn rectangle: the pane's content rectangle, or its
    /// header strip for a collapsed stack member.
    pub slot_rect: Rect,
}

/// One span an insertion may split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertionSpan {
    /// The anchor that names this span in a placement.
    pub anchor: PanePlacementAnchor,
    /// The span's drawn rectangle: a pane's rectangle, the bounding
    /// rectangle of a group's members, or the tab rectangle.
    pub span_rect: Rect,
}

/// Every destination a placement of one pane may name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementDestinations {
    /// Swap targets, in the solve's pane order.
    pub swap_slots: Vec<SwapSlot>,
    /// Insertion spans: panes in pane order, then groups in pre-order, then
    /// the tab.
    pub insertion_spans: Vec<InsertionSpan>,
}

/// List the destinations `source_pane_id` may move to in `layout_tree`,
/// solved as `layout_solve` over `tab_rect`.
///
/// Swap slots: every pane other than the source whose rectangle covers
/// cells. A collapsed stack member is listed with its header strip.
///
/// Insertion spans, in this order:
///
/// 1. `Pane(p)` for every pane other than the source that shows its own
///    content and has no stacked ancestor.
/// 2. `Group(leaf_ids)` for every split node, in pre-order, that holds two or
///    more leaves, is not the whole tree, has no stacked ancestor, has no
///    suppressed leaf, and whose members' bounding rectangle covers cells.
///    A stack is listed as one group. A node whose leaf set repeats an
///    earlier group's is skipped.
/// 3. `Tab` over `tab_rect`, when some pane other than the source exists.
///
/// A tree holding only the source lists nothing. `layout_solve` is the tiled
/// solve of `layout_tree` itself: a leaf absent from `layout_solve.pane_rects`
/// is listed nowhere and counts for no group rectangle.
///
/// `H[A, V[B, H[C, D]]]` with source D lists swap slots `A, B, C`, and spans
/// `Pane(A), Pane(B), Pane(C), Group([B, C, D]), Group([C, D]), Tab`.
#[must_use]
pub fn list_placement_destinations(
    layout_tree: &LayoutNode,
    layout_solve: &LayoutSolve,
    tab_rect: Rect,
    source_pane_id: PaneId,
) -> PlacementDestinations {
    let swap_slots: Vec<SwapSlot> = layout_solve
        .pane_rects
        .iter()
        .filter(|&&(pane_id, pane_rect)| pane_id != source_pane_id && !pane_rect.is_empty())
        .map(|&(pane_id, slot_rect)| SwapSlot { pane_id, slot_rect })
        .collect();

    let mut all_pane_ids = layout_tree.list_leaf_pane_ids();
    all_pane_ids.sort_unstable();
    let has_other_pane = all_pane_ids
        .iter()
        .any(|&pane_id| pane_id != source_pane_id);

    let mut span_walk = InsertionSpanWalk {
        source_pane_id,
        pane_rect_by_id: layout_solve.pane_rects.iter().copied().collect(),
        suppressed_pane_ids: layout_solve.suppressed_pane_ids.iter().copied().collect(),
        stack_headers: &layout_solve.stack_headers,
        all_pane_ids,
        pane_spans: Vec::new(),
        group_spans: Vec::new(),
        listed_group_pane_id_sets: Vec::new(),
    };
    span_walk.collect_insertion_spans(layout_tree, false);

    let mut insertion_spans = span_walk.pane_spans;
    insertion_spans.extend(span_walk.group_spans);
    if has_other_pane {
        insertion_spans.push(InsertionSpan {
            anchor: PanePlacementAnchor::Tab,
            span_rect: tab_rect,
        });
    }

    PlacementDestinations {
        swap_slots,
        insertion_spans,
    }
}

/// The pre-order walk that lists pane spans and group spans.
struct InsertionSpanWalk<'solve> {
    source_pane_id: PaneId,
    pane_rect_by_id: HashMap<PaneId, Rect>,
    suppressed_pane_ids: HashSet<PaneId>,
    stack_headers: &'solve [StackHeader],
    /// Every leaf of the tree, sorted.
    all_pane_ids: Vec<PaneId>,
    pane_spans: Vec<InsertionSpan>,
    group_spans: Vec<InsertionSpan>,
    /// The sorted leaf set of every group listed so far.
    listed_group_pane_id_sets: Vec<Vec<PaneId>>,
}

impl InsertionSpanWalk<'_> {
    /// Visit `layout_node` and its subtree in pre-order. `is_inside_stack`
    /// is `true` when some ancestor of `layout_node` is a stack.
    fn collect_insertion_spans(&mut self, layout_node: &LayoutNode, is_inside_stack: bool) {
        match layout_node {
            LayoutNode::Pane(pane_id) => {
                if !is_inside_stack {
                    self.collect_pane_span(*pane_id);
                }
            }
            LayoutNode::Split(split) => {
                if !is_inside_stack {
                    self.collect_group_span(layout_node.list_leaf_pane_ids());
                }
                let is_child_inside_stack =
                    is_inside_stack || split.direction == SplitDirection::Stacked;
                for child_node in &split.children {
                    self.collect_insertion_spans(child_node, is_child_inside_stack);
                }
            }
        }
    }

    /// List `pane_id` when it is not the source and shows its own content.
    fn collect_pane_span(&mut self, pane_id: PaneId) {
        if pane_id == self.source_pane_id {
            return;
        }
        let Some(&pane_rect) = self.pane_rect_by_id.get(&pane_id) else {
            return;
        };
        if is_content_visible(pane_id, pane_rect, self.stack_headers) {
            self.pane_spans.push(InsertionSpan {
                anchor: PanePlacementAnchor::Pane(pane_id),
                span_rect: pane_rect,
            });
        }
    }

    /// List `group_pane_ids` when it holds two or more leaves, is not the
    /// whole tree, repeats no listed group, has no suppressed member, and
    /// its members cover cells.
    fn collect_group_span(&mut self, group_pane_ids: Vec<PaneId>) {
        let mut sorted_group_pane_ids = group_pane_ids.clone();
        sorted_group_pane_ids.sort_unstable();
        if group_pane_ids.len() < 2
            || sorted_group_pane_ids == self.all_pane_ids
            || self
                .listed_group_pane_id_sets
                .contains(&sorted_group_pane_ids)
            || group_pane_ids
                .iter()
                .any(|pane_id| self.suppressed_pane_ids.contains(pane_id))
        {
            return;
        }
        let member_rects = group_pane_ids
            .iter()
            .filter_map(|pane_id| self.pane_rect_by_id.get(pane_id).copied());
        let Some(span_rect) = compute_bounding_rect(member_rects) else {
            return;
        };
        self.group_spans.push(InsertionSpan {
            anchor: PanePlacementAnchor::Group(group_pane_ids),
            span_rect,
        });
        self.listed_group_pane_id_sets.push(sorted_group_pane_ids);
    }
}

/// The smallest rectangle covering every non-empty rectangle in `rects`, or
/// `None` when none covers cells.
///
/// `(40, 0, 80, 20)` and `(40, 20, 40, 20)` give `(40, 0, 80, 40)`.
fn compute_bounding_rect(rects: impl Iterator<Item = Rect>) -> Option<Rect> {
    let mut non_empty_rects = rects.filter(|rect| !rect.is_empty());
    let first_rect = non_empty_rects.next()?;
    let mut left_edge = first_rect.origin.column;
    let mut top_edge = first_rect.origin.row;
    let mut right_edge = compute_right_edge(first_rect);
    let mut bottom_edge = compute_bottom_edge(first_rect);
    for rect in non_empty_rects {
        left_edge = left_edge.min(rect.origin.column);
        top_edge = top_edge.min(rect.origin.row);
        right_edge = right_edge.max(compute_right_edge(rect));
        bottom_edge = bottom_edge.max(compute_bottom_edge(rect));
    }
    Some(Rect::from_origin_and_size(
        Point {
            column: left_edge,
            row: top_edge,
        },
        Size {
            column_count: right_edge - left_edge,
            row_count: bottom_edge - top_edge,
        },
    ))
}

#[cfg(test)]
mod tests;
