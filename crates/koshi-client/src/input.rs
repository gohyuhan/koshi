//! What a keypress means, decided by the viewer that received it.
//!
//! A viewer holds its own keymap, its own input mode, and its own open
//! sequence. It answers "is this key mine?" without asking the session. Two
//! viewers of one session read different `keybinding.kdl` files, and each one
//! gets its own answer.
//!
//! What the viewer does **not** decide is what a bound action *does*. It
//! resolves a chord to a [`BoundAction`] — a name plus its arguments. The
//! attachment resolves that name through the shared action table and either
//! sends a session command or applies a viewer-local action.
//!
//! **An open sequence captures the keyboard.** Once a chord opens a
//! multi-chord binding, every key belongs to koshi until the sequence
//! resolves: a key that continues it fires the binding, and a key that
//! continues nothing is discarded while the sequence stands. Nothing typed
//! into an open sequence reaches the pane. Three keys leave the context: a
//! continuation that completes a binding, `Esc`, and the reserved unlock
//! chord. One thing that is not a key leaves it too — a sequence that is both
//! a complete binding and a longer one's prefix closes on its ambiguity
//! deadline, firing the complete binding.

use std::time::{Duration, Instant};

use koshi_config::types::BoundAction;
use koshi_core::action::{ActionReference, ClientActionKind};
use koshi_core::command::{
    Command, PanePlacementAnchor, PanePlacementTarget, PlacePaneArgs, PlacementRevision,
};
use koshi_core::geometry::{Direction, Rect};
use koshi_core::ids::{CommandId, PaneId, TabId};
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags, NamedKey, PendingKeySequence};
use koshi_core::lock::LockMode;
use koshi_core::resolve::ActionArgs;
use koshi_ipc::placement::{PanePlacementSizing, PanePlacementSnapshot, PanePlacementTabSnapshot};
use koshi_layout::mode::LayoutMode;
use koshi_layout::neighbor::select_directional_neighbor;
use koshi_layout::placement::{
    is_same_tab_placement_noop as is_layout_placement_noop, list_placement_destinations,
    InsertionSpan, PlacementDestinations,
};
use koshi_layout::solver::{solve_layout_with_mode, PaneSizing};

use crate::{Client, PendingPlacementCommand, PlacementInputAction};

#[cfg(test)]
mod tests;

/// The chord that backs out of an open multi-chord sequence.
const ESCAPE_KEY_CHORD: KeyChord = KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Esc));

/// What the viewer decided one keypress means.
///
/// Only [`Fire`](KeyOutcome::Fire) and [`PassThrough`](KeyOutcome::PassThrough)
/// leave the viewer; the other two are settled where the key was typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    /// A binding completed. The attachment resolves the name against its
    /// action table and dispatches the resulting session or viewer action.
    Fire(BoundAction),
    /// The chord opened or continued a multi-chord sequence, a held sequence
    /// swallowed a key that continues nothing, or an `Esc` closed one. The hint
    /// bar changes; nothing else does.
    Pending,
    /// Nothing bound the chord and the mode passes what it does not bind. The
    /// session encodes it for the focused pane, reading that pane's cursor-key
    /// mode at the instant it writes so the bytes cannot be stale.
    PassThrough(KeyChord),
    /// Consumed with nothing to do: no sequence is open, the chord binds
    /// nothing, and the mode is a modal one that owns the keyboard rather than
    /// passing what it does not bind.
    Discard,
}

impl Client {
    /// The viewer's current input mode.
    #[must_use]
    pub fn get_lock_mode(&self) -> LockMode {
        self.lock_mode
    }

    /// Return the keymap mode that currently owns this viewer's keyboard.
    ///
    /// Placement is a local submode layered over the stored normal or locked
    /// mode. Entering it changes key resolution only; it does not change the
    /// session lock state.
    #[must_use]
    pub(crate) fn get_active_input_mode(&self) -> LockMode {
        if self.is_placement_mode_active() {
            LockMode::PanePlacement
        } else {
            self.lock_mode
        }
    }

    /// Set the stored base input mode and clear an open sequence or unconfirmed
    /// placement interaction.
    ///
    /// Called both when this viewer's own `core:lock` fires and when the
    /// session reports a mode change aimed at this viewer (`koshi lock
    /// --client`). Held chords were typed at koshi, so a mode change drops
    /// them and no pane ever sees them.
    pub fn set_lock_mode(&mut self, lock_mode: LockMode) {
        if self.lock_mode != lock_mode {
            if self.is_placement_mode_active() {
                self.cancel_placement_mode();
            }
            self.lock_mode = lock_mode;
            self.pending_key_sequence = None;
        }
    }

    /// The chords of an open sequence, for the hint bar's breadcrumb.
    #[must_use]
    pub fn get_pending_key_sequence(&self) -> Option<&KeySequence> {
        self.pending_key_sequence
            .as_ref()
            .map(|pending_key_sequence| &pending_key_sequence.sequence)
    }

    /// Apply one viewer-local action selected by the active keymap.
    pub(crate) fn apply_client_action(
        &mut self,
        client_action_kind: ClientActionKind,
    ) -> PlacementInputAction {
        match client_action_kind {
            ClientActionKind::BeginPanePlacement => self.begin_placement_mode().map_or(
                PlacementInputAction::Consumed,
                |(source_pane_id, destination_tab_id)| PlacementInputAction::ReadPlacement {
                    pane_id_to_focus: None,
                    source_pane_id,
                    destination_tab_id,
                },
            ),
            ClientActionKind::SelectPaneTarget(direction) => {
                self.select_placement_target(direction, false)
            }
            ClientActionKind::SelectPaneInsertion(direction) => {
                self.select_placement_target(direction, true)
            }
            ClientActionKind::CyclePanePlacementSpan => self.select_next_placement_span(),
            ClientActionKind::SelectNextPlacementTab => self.select_placement_tab_by_offset(1),
            ClientActionKind::SelectPreviousPlacementTab => self.select_placement_tab_by_offset(-1),
            ClientActionKind::ConfirmPanePlacement => self
                .submit_placement_command()
                .unwrap_or(PlacementInputAction::Consumed),
            ClientActionKind::CancelPanePlacement => {
                if self.is_placement_mode_active() {
                    self.cancel_placement_mode();
                }
                PlacementInputAction::Consumed
            }
        }
    }

    /// Select the next or previous visible tab for placement mode.
    fn select_placement_tab_by_offset(&mut self, tab_offset: isize) -> PlacementInputAction {
        if self.is_placement_confirmation_pending() {
            return PlacementInputAction::Consumed;
        }
        let Some(placement_mode) = self.placement_state.placement_mode.as_ref() else {
            return PlacementInputAction::Consumed;
        };
        let Some(destination_tab_index) = self
            .visible_tab_ids
            .iter()
            .position(|&tab_id| tab_id == placement_mode.destination_tab_id)
        else {
            return PlacementInputAction::Consumed;
        };
        let target_tab_index = destination_tab_index as isize + tab_offset;
        let Ok(target_tab_index) = usize::try_from(target_tab_index) else {
            return PlacementInputAction::Consumed;
        };
        let Some(&destination_tab_id) = self.visible_tab_ids.get(target_tab_index) else {
            return PlacementInputAction::Consumed;
        };
        self.select_placement_destination_tab(destination_tab_id)
            .map_or(
                PlacementInputAction::Consumed,
                |(source_pane_id, destination_tab_id)| PlacementInputAction::ReadPlacement {
                    pane_id_to_focus: None,
                    source_pane_id,
                    destination_tab_id,
                },
            )
    }

    /// Select a placement target in `direction` and retain it for confirmation.
    ///
    /// - An Arrow selects a swap with the pane `direction` reaches.
    /// - Shift+Arrow selects insertion into the smallest span that holds that
    ///   pane: in `[A | B]` with `B` in a stack, Shift+Right from `A` selects
    ///   insertion beside the whole stack.
    /// - With the whole destination tab selected, Shift+Left selects `insert
    ///   left` of that tab, and an Arrow searches as if no target were selected.
    /// - Reaching the source pane, or a target that leaves the layout as it is,
    ///   clears the target. Finding no pane keeps the current target.
    fn select_placement_target(
        &mut self,
        direction: Direction,
        is_insertion_target: bool,
    ) -> PlacementInputAction {
        if self.is_placement_confirmation_pending() {
            return PlacementInputAction::Consumed;
        }
        let Some(placement_mode) = self.placement_state.placement_mode.as_ref() else {
            return PlacementInputAction::Consumed;
        };
        let Some(placement_snapshot) = self.placement_state.placement_snapshot.as_deref() else {
            return PlacementInputAction::Consumed;
        };
        let destination_tab_id = placement_mode.destination_tab_id;
        let source_pane_id = placement_mode.source_pane_id;
        let Some(destination_tab_snapshot) =
            find_destination_tab_snapshot(placement_snapshot, destination_tab_id)
        else {
            return PlacementInputAction::Consumed;
        };
        let is_whole_tab_target_selected = matches!(
            placement_mode.placement_target,
            Some(PanePlacementTarget::Split {
                anchor: PanePlacementAnchor::Tab,
                ..
            })
        );
        let placement_target = if is_insertion_target && is_whole_tab_target_selected {
            Some(PanePlacementTarget::Split {
                destination_tab_id,
                anchor: PanePlacementAnchor::Tab,
                direction,
            })
        } else {
            let current_placement_target = if is_whole_tab_target_selected {
                None
            } else {
                placement_mode.placement_target.as_ref()
            };
            let placement_destinations = build_placement_destinations(
                destination_tab_snapshot,
                source_pane_id,
                placement_snapshot.pane_sizing,
            );
            let Some(target_pane_id) = find_directional_target_pane_id(
                &placement_destinations,
                destination_tab_snapshot,
                source_pane_id,
                current_placement_target,
                direction,
            ) else {
                return PlacementInputAction::Consumed;
            };
            if target_pane_id == source_pane_id {
                None
            } else if is_insertion_target {
                find_smallest_insertion_span(
                    &placement_destinations.insertion_spans,
                    target_pane_id,
                )
                .map(|insertion_span| PanePlacementTarget::Split {
                    destination_tab_id,
                    anchor: insertion_span.anchor.clone(),
                    direction,
                })
            } else {
                Some(PanePlacementTarget::Swap { target_pane_id })
            }
        };
        let placement_target = match placement_target {
            Some(placement_target)
                if !is_same_tab_placement_noop(placement_snapshot, &placement_target) =>
            {
                Some(placement_target)
            }
            _ => None,
        };
        let Some(placement_mode) = self.placement_state.placement_mode.as_mut() else {
            return PlacementInputAction::Consumed;
        };
        placement_mode.placement_direction = direction;
        placement_mode.placement_target = placement_target;
        PlacementInputAction::Consumed
    }

    /// Cycle the visible insertion spans from the smallest span to the largest.
    fn select_next_placement_span(&mut self) -> PlacementInputAction {
        if self.is_placement_confirmation_pending() {
            return PlacementInputAction::Consumed;
        }
        let Some(placement_mode) = self.placement_state.placement_mode.as_ref() else {
            return PlacementInputAction::Consumed;
        };
        let Some(placement_snapshot) = self.placement_state.placement_snapshot.as_deref() else {
            return PlacementInputAction::Consumed;
        };
        let destination_tab_id = placement_mode.destination_tab_id;
        let source_pane_id = placement_mode.source_pane_id;
        let placement_direction = placement_mode.placement_direction;
        let Some(destination_tab_snapshot) =
            find_destination_tab_snapshot(placement_snapshot, destination_tab_id)
        else {
            return PlacementInputAction::Consumed;
        };
        let mut insertion_spans = build_placement_destinations(
            destination_tab_snapshot,
            source_pane_id,
            placement_snapshot.pane_sizing,
        )
        .insertion_spans;
        insertion_spans.retain(|insertion_span| {
            let placement_target = PanePlacementTarget::Split {
                destination_tab_id,
                anchor: insertion_span.anchor.clone(),
                direction: placement_direction,
            };
            !is_same_tab_placement_noop(placement_snapshot, &placement_target)
        });
        insertion_spans.sort_by_key(compute_span_cell_count);
        if insertion_spans.is_empty() {
            return PlacementInputAction::Consumed;
        }
        let current_span_index = placement_mode
            .placement_target
            .as_ref()
            .and_then(get_placement_target_anchor)
            .and_then(|anchor| {
                insertion_spans
                    .iter()
                    .position(|insertion_span| insertion_span.anchor == anchor)
            });
        let next_span_index =
            current_span_index.map_or(0, |span_index| (span_index + 1) % insertion_spans.len());
        let insertion_span = &insertion_spans[next_span_index];
        let placement_target = PanePlacementTarget::Split {
            destination_tab_id,
            anchor: insertion_span.anchor.clone(),
            direction: placement_direction,
        };
        let Some(placement_mode) = self.placement_state.placement_mode.as_mut() else {
            return PlacementInputAction::Consumed;
        };
        placement_mode.placement_target = Some(placement_target);
        PlacementInputAction::Consumed
    }

    /// Select insertion at the right edge of the whole destination tab when the
    /// retained snapshot previews a tab other than the source pane's tab, no
    /// target is selected, and no drag is active. Tab from `[A | B]` onto `[C]`
    /// selects `insert right` of that tab, and Enter gives `[C | A]`.
    pub(crate) fn select_whole_tab_insertion_target(&mut self) {
        if self.placement_state.placement_drag.is_some() || self.is_placement_confirmation_pending()
        {
            return;
        }
        let Some(placement_snapshot) = self.placement_state.placement_snapshot.as_deref() else {
            return;
        };
        let Some(placement_mode) = self.placement_state.placement_mode.as_mut() else {
            return;
        };
        if placement_mode.placement_target.is_some()
            || placement_snapshot.destination_tab_id == placement_snapshot.source_tab_id
        {
            return;
        }
        placement_mode.placement_direction = Direction::Right;
        placement_mode.placement_target = Some(PanePlacementTarget::Split {
            destination_tab_id: placement_snapshot.destination_tab_id,
            anchor: PanePlacementAnchor::Tab,
            direction: Direction::Right,
        });
    }

    /// Build the `PlacePane` command for the selected target, record it as the
    /// pending placement command under a new command id, and return it to
    /// send.
    ///
    /// - Returns `Consumed` while a placement command is pending.
    /// - Returns `Consumed` and clears the target when the target leaves the
    ///   layout as it is, such as a swap of `pane-123` with itself.
    /// - Returns `None` with no placement mode or no target.
    pub(crate) fn submit_placement_command(&mut self) -> Option<PlacementInputAction> {
        let placement_mode = self.placement_state.placement_mode.as_ref()?;
        if placement_mode.pending_placement_command.is_some() {
            return Some(PlacementInputAction::Consumed);
        }
        let placement_target = placement_mode.placement_target.clone()?;
        if self
            .placement_state
            .placement_snapshot
            .as_deref()
            .is_some_and(|placement_snapshot| {
                is_same_tab_placement_noop(placement_snapshot, &placement_target)
            })
        {
            let placement_mode = self.placement_state.placement_mode.as_mut()?;
            placement_mode.placement_target = None;
            return Some(PlacementInputAction::Consumed);
        }
        let command = Command::PlacePane(PlacePaneArgs {
            source_pane_id: placement_mode.source_pane_id,
            placement_target,
            expected_placement_revision: Some(PlacementRevision {
                session_revision: self.placement_state.session_placement_revision,
                client_revision: self.placement_state.client_placement_revision,
            }),
        });
        let command_id = CommandId::new();
        let placement_mode = self.placement_state.placement_mode.as_mut()?;
        placement_mode.pending_placement_command = Some(PendingPlacementCommand {
            command_id,
            is_committed: false,
        });
        Some(PlacementInputAction::SubmitPlacement {
            command_id,
            command,
        })
    }

    /// Decide what `chord` means in this viewer's current mode.
    ///
    /// `<C-l>` while locked yields `Fire(core:unlock)` whatever the keymap
    /// says; `<C-p>` in the default keymap yields `Pending` because it opens
    /// the pane group; a plain `a` with nothing bound yields
    /// `PassThrough('a')`.
    pub fn resolve_key(&mut self, chord: KeyChord, current_time: Instant) -> KeyOutcome {
        let active_input_mode = self.get_active_input_mode();
        let open_key_sequence = self.pending_key_sequence.take();

        // The guaranteed escape from locked base mode remains ahead of the
        // keymap. Placement mode owns the keyboard while it is active, so its
        // pane map has priority over this base-mode escape.
        if !self.is_placement_mode_active()
            && self.lock_mode == LockMode::Locked
            && chord == self.keymap_catalog.get_unlock_chord()
        {
            return KeyOutcome::Fire(build_unlock_bound_action());
        }

        // The open sequence's chords with this one after them, or this one on
        // its own. One keypress allocates the chord list once and the sequence
        // once.
        let key_sequence = match open_key_sequence.as_ref() {
            Some(open_key_sequence) => {
                let held_chords = open_key_sequence.sequence.list_chords();
                let mut remaining_chords = Vec::with_capacity(held_chords.len());
                remaining_chords.extend_from_slice(&held_chords[1..]);
                remaining_chords.push(chord);
                KeySequence::from_first_and_rest(held_chords[0], remaining_chords)
            }
            None => KeySequence::from(chord),
        };

        let sequence_match = self
            .keymap_catalog
            .match_sequence(active_input_mode, &key_sequence);
        match (
            sequence_match.exact_bound_action,
            sequence_match.has_longer_key_sequence,
        ) {
            (Some(bound_action), false) => {
                self.rearm_continuous(&bound_action, &key_sequence);
                KeyOutcome::Fire(bound_action)
            }
            (exact_bound_action, true) => {
                // A prefix-only sequence waits for its next chord with no
                // deadline; only exact-plus-longer ambiguity arms one, and
                // reaching it fires the exact binding.
                let deadline = exact_bound_action
                    .is_some()
                    .then(|| current_time + self.keymap_catalog.get_chord_timeout());
                self.pending_key_sequence = Some(PendingKeySequence {
                    sequence: key_sequence,
                    deadline,
                });
                KeyOutcome::Pending
            }
            (None, false) => match open_key_sequence {
                // Escape leaves an open sequence: the held chords are dropped
                // and the Escape itself is consumed rather than typed.
                Some(_) if chord == ESCAPE_KEY_CHORD => KeyOutcome::Pending,
                // A key that continues nothing is discarded and the sequence
                // stands unchanged, deadline included: the viewer is inside a
                // koshi context, so a key that context cannot use goes nowhere
                // rather than surprising the program underneath.
                Some(held_key_sequence) => {
                    self.pending_key_sequence = Some(held_key_sequence);
                    KeyOutcome::Pending
                }
                // No sequence is open, so the key is the user's own to type.
                None if active_input_mode.should_pass_unbound_input_to_pane() => {
                    KeyOutcome::PassThrough(chord)
                }
                None => KeyOutcome::Discard,
            },
        }
    }

    /// How long until an open sequence's ambiguity deadline, so the event loop
    /// can wake for it. Prefix-only sequences carry no deadline and never wake
    /// it.
    #[must_use]
    pub fn next_key_wakeup(&self, current_time: Instant) -> Option<Duration> {
        self.pending_key_sequence
            .as_ref()
            .and_then(|pending_key_sequence| pending_key_sequence.deadline)
            .map(|deadline| deadline.saturating_duration_since(current_time))
    }

    /// Fire the open sequence's complete binding if its ambiguity deadline has
    /// passed.
    ///
    /// The deadline was armed because the sequence was itself a complete
    /// binding, so it normally still is. A keymap change can retire that
    /// binding while the sequence waits; the held chords then resolve to
    /// nothing and are dropped, never typed at the pane.
    pub fn expire_key_sequence(&mut self, current_time: Instant) -> Option<BoundAction> {
        let is_deadline_due = self
            .pending_key_sequence
            .as_ref()
            .and_then(|pending_key_sequence| pending_key_sequence.deadline)
            .is_some_and(|deadline| deadline <= current_time);
        if !is_deadline_due {
            return None;
        }
        let pending_key_sequence = self.pending_key_sequence.take()?;
        let bound_action = self
            .keymap_catalog
            .match_sequence(self.get_active_input_mode(), &pending_key_sequence.sequence)
            .exact_bound_action?;
        self.rearm_continuous(&bound_action, &pending_key_sequence.sequence);
        Some(bound_action)
    }

    /// Re-open the prefix of a sequence whose action the registry marks
    /// `continuous`, so a repeated final chord repeats the action: `<C-p> r →`
    /// leaves `<C-p> r` open, and each further `→` resizes again.
    ///
    /// Only multi-chord sequences have a prefix to hold. The re-armed prefix
    /// captures the keyboard like any other open sequence, so a key that
    /// resizes nothing is discarded and the prefix stands until `Esc` leaves
    /// it.
    fn rearm_continuous(&mut self, bound_action: &BoundAction, key_sequence: &KeySequence) {
        let is_continuous = self
            .registry
            .find_action_metadata(&bound_action.action_reference)
            .is_some_and(|action_metadata| action_metadata.is_continuous);
        let sequence_chords = key_sequence.list_chords();
        if !is_continuous || sequence_chords.len() < 2 {
            return;
        }
        self.pending_key_sequence = Some(PendingKeySequence {
            sequence: KeySequence::from_first_and_rest(
                sequence_chords[0],
                sequence_chords[1..sequence_chords.len() - 1].to_vec(),
            ),
            deadline: None,
        });
    }
}

/// Return whether an interactive target preserves the source tab's layout.
pub(crate) fn is_same_tab_placement_noop(
    placement_snapshot: &PanePlacementSnapshot,
    placement_target: &PanePlacementTarget,
) -> bool {
    if placement_snapshot.source_tab_id != placement_snapshot.destination_tab_id {
        return false;
    }
    if let PanePlacementTarget::Split {
        destination_tab_id, ..
    } = placement_target
    {
        if *destination_tab_id != placement_snapshot.source_tab_id {
            return false;
        }
    }
    let layout_target = crate::terminal::build_layout_placement_target(placement_target);
    let source_tab_snapshot = &placement_snapshot.source_tab_snapshot;
    let tab_rect = Rect::from_size_at_origin(source_tab_snapshot.effective_cell_size);
    let pane_sizing = PaneSizing {
        minimum_size: placement_snapshot.pane_sizing.minimum_size,
        gap_cell_count: placement_snapshot.pane_sizing.gap_cell_count,
    };
    is_layout_placement_noop(
        &source_tab_snapshot.layout_tree,
        placement_snapshot.source_pane_id,
        &layout_target,
        tab_rect,
        pane_sizing,
    )
}

/// Return the tab snapshot that owns `destination_tab_id`: `source_tab_snapshot`
/// for `source_tab_id`, `destination_tab_snapshot` for `destination_tab_id`, and
/// `None` for any other tab.
pub(crate) fn find_destination_tab_snapshot(
    placement_snapshot: &PanePlacementSnapshot,
    destination_tab_id: TabId,
) -> Option<&PanePlacementTabSnapshot> {
    if placement_snapshot.source_tab_id == destination_tab_id {
        Some(&placement_snapshot.source_tab_snapshot)
    } else if placement_snapshot.destination_tab_id == destination_tab_id {
        placement_snapshot.destination_tab_snapshot.as_ref()
    } else {
        None
    }
}

/// List the keyboard and mouse destinations for one placement snapshot tab.
pub(crate) fn build_placement_destinations(
    destination_tab_snapshot: &PanePlacementTabSnapshot,
    source_pane_id: PaneId,
    pane_sizing: PanePlacementSizing,
) -> PlacementDestinations {
    let pane_sizing = PaneSizing {
        minimum_size: pane_sizing.minimum_size,
        gap_cell_count: pane_sizing.gap_cell_count,
    };
    let tab_rect = Rect::from_size_at_origin(destination_tab_snapshot.effective_cell_size);
    let layout_solve = solve_layout_with_mode(
        &destination_tab_snapshot.layout_tree,
        LayoutMode::Tiled,
        tab_rect,
        pane_sizing,
    );
    list_placement_destinations(
        &destination_tab_snapshot.layout_tree,
        &layout_solve,
        tab_rect,
        source_pane_id,
    )
}

/// Return the rectangle named by a placement target: a pane's slot from
/// `candidate_pane_rects`, where a collapsed stack member's slot is its header
/// strip, or a group's or the tab's span from `insertion_spans`.
fn find_placement_target_rect(
    placement_target: &PanePlacementTarget,
    candidate_pane_rects: &[(PaneId, Rect)],
    insertion_spans: &[InsertionSpan],
) -> Option<Rect> {
    match placement_target {
        PanePlacementTarget::Swap { target_pane_id }
        | PanePlacementTarget::Split {
            anchor: PanePlacementAnchor::Pane(target_pane_id),
            ..
        } => candidate_pane_rects
            .iter()
            .find(|(pane_id, _)| pane_id == target_pane_id)
            .map(|(_, pane_rect)| *pane_rect),
        PanePlacementTarget::Split {
            anchor: PanePlacementAnchor::Group(pane_ids),
            ..
        } => insertion_spans
            .iter()
            .find(|insertion_span| {
                insertion_span.anchor == PanePlacementAnchor::Group(pane_ids.clone())
            })
            .map(|insertion_span| insertion_span.span_rect),
        PanePlacementTarget::Split {
            anchor: PanePlacementAnchor::Tab,
            ..
        } => insertion_spans
            .iter()
            .find(|insertion_span| insertion_span.anchor == PanePlacementAnchor::Tab)
            .map(|insertion_span| insertion_span.span_rect),
    }
}

/// Return the pane that `direction` reaches in `placement_destinations`.
///
/// The candidates are the swap slots plus the source pane's own slot when
/// `destination_tab_snapshot` holds it. The search starts from the rectangle of
/// `current_placement_target`, else the source pane's slot, else the first
/// candidate. With no current target and no pane in `direction`, it returns the
/// first candidate. `None` means there is no pane to select. In `[A | B | C]`
/// with source `A` and no target, Right returns `B`, and Right again from `B`
/// returns `C`.
fn find_directional_target_pane_id(
    placement_destinations: &PlacementDestinations,
    destination_tab_snapshot: &PanePlacementTabSnapshot,
    source_pane_id: PaneId,
    current_placement_target: Option<&PanePlacementTarget>,
    direction: Direction,
) -> Option<PaneId> {
    let mut candidate_pane_rects: Vec<(PaneId, Rect)> = placement_destinations
        .swap_slots
        .iter()
        .map(|swap_slot| (swap_slot.pane_id, swap_slot.slot_rect))
        .collect();
    let source_pane_slot = destination_tab_snapshot
        .pane_slots
        .iter()
        .find(|pane_slot| {
            pane_slot.pane_id == source_pane_id
                && pane_slot.is_visible
                && !pane_slot.is_suppressed
                && !pane_slot.outer_rect.is_empty()
        });
    if let Some(source_pane_slot) = source_pane_slot {
        candidate_pane_rects.push((source_pane_id, source_pane_slot.outer_rect));
    }
    let first_candidate = candidate_pane_rects.first().copied();
    let current_pane_rect = current_placement_target
        .and_then(|placement_target| {
            find_placement_target_rect(
                placement_target,
                &candidate_pane_rects,
                &placement_destinations.insertion_spans,
            )
        })
        .or_else(|| source_pane_slot.map(|source_pane_slot| source_pane_slot.outer_rect))
        .or_else(|| first_candidate.map(|(_, pane_rect)| pane_rect))?;
    let neighbor_pane_id =
        select_directional_neighbor(current_pane_rect, &candidate_pane_rects, direction);
    if neighbor_pane_id.is_none() && current_placement_target.is_none() {
        return first_candidate.map(|(pane_id, _)| pane_id);
    }
    neighbor_pane_id
}

/// Return the smallest span in `insertion_spans` that holds `target_pane_id`:
/// its own pane span, a group that holds it, or the whole tab. In `[A | B]`
/// with `B` in a stack, the smallest span that holds `B` is the stack's group.
fn find_smallest_insertion_span(
    insertion_spans: &[InsertionSpan],
    target_pane_id: PaneId,
) -> Option<&InsertionSpan> {
    insertion_spans
        .iter()
        .filter(|insertion_span| match &insertion_span.anchor {
            PanePlacementAnchor::Pane(anchor_pane_id) => *anchor_pane_id == target_pane_id,
            PanePlacementAnchor::Group(group_pane_ids) => group_pane_ids.contains(&target_pane_id),
            PanePlacementAnchor::Tab => true,
        })
        .min_by_key(|insertion_span| compute_span_cell_count(insertion_span))
}

/// Return the number of cells `insertion_span` covers: a 4x3 span covers `12`.
fn compute_span_cell_count(insertion_span: &InsertionSpan) -> u32 {
    u32::from(insertion_span.span_rect.cell_size.column_count)
        * u32::from(insertion_span.span_rect.cell_size.row_count)
}

/// Return the insertion anchor named by a placement target.
fn get_placement_target_anchor(
    placement_target: &PanePlacementTarget,
) -> Option<PanePlacementAnchor> {
    match placement_target {
        PanePlacementTarget::Swap { target_pane_id } => {
            Some(PanePlacementAnchor::Pane(*target_pane_id))
        }
        PanePlacementTarget::Split { anchor, .. } => Some(anchor.clone()),
    }
}

/// The binding the unlock chord fires, built here and never looked up in the
/// keymap. The escape from locked mode holds whatever any config layer says.
fn build_unlock_bound_action() -> BoundAction {
    BoundAction {
        action_reference: ActionReference::from_core_action_name("unlock")
            .expect("the reserved unlock action name satisfies the action-name grammar"),
        action_arguments: ActionArgs::None,
    }
}
