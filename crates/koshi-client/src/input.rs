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

use std::collections::HashSet;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use koshi_config::types::BoundAction;
use koshi_core::action::{ActionReference, ClientActionKind};
use koshi_core::command::{
    Command, FocusPaneArgs, FocusTarget, MoveFloatingPaneArgs, PanePlacementAnchor,
    PanePlacementTarget, PlacePaneArgs, PlacementRevision, TiledPlacementTarget,
};
use koshi_core::geometry::{Direction, Point, Rect};
use koshi_core::ids::{CommandId, PaneId, TabId};
use koshi_core::key::{
    BindingModifierFlags, Key, KeyChord, KeySequence, NamedKey, PendingKeySequence,
};
use koshi_core::lock::LockMode;
use koshi_ipc::placement::{PanePlacementSizing, PanePlacementSnapshot, PanePlacementTabSnapshot};
use koshi_layout::mode::LayoutMode;
use koshi_layout::neighbor::select_directional_neighbor;
use koshi_layout::placement::{
    is_same_tab_placement_noop as is_layout_placement_noop, list_placement_destinations,
    InsertionSpan, PlacementDestinations, PlacementTarget,
};
use koshi_layout::solver::{solve_layout_with_mode, PaneSizing};
use koshi_renderer::snapshot::MouseFrame;

use crate::{
    is_focused_pane_floating, Client, FloatingPaneMoveSelection, LinkStatus, PaneMoveMode,
    PendingPlacementCommand, PlacementInputAction,
};

#[cfg(test)]
mod tests;

/// The read times of this terminal's input events, shared by its input thread
/// and the attach loop under one lock.
///
/// The input thread stamps each event it reads, a key, a mouse event, a paste,
/// a resize or a focus loss ([`stamp_input_read`](Self::stamp_input_read)), and
/// the loop records each event it takes off its channel
/// ([`note_input_taken`](Self::note_input_taken)). An event stamped and not yet
/// taken is on its way to the loop.
///
/// Example: the input thread reads `a` 400 ms after `<C-y>` opened a sequence
/// whose deadline is 500 ms out, and the loop is still drawing a frame at
/// 600 ms. [`find_input_judge_time`](Self::find_input_judge_time) answers
/// `None` until the loop takes `a`, and `a` then continues the sequence.
#[derive(Debug, Default)]
pub(crate) struct InputReadClock {
    input_read_times: Mutex<InputReadTimes>,
}

/// The stamp of the last input event the input thread read, and of the last
/// one the loop took. `None` before the first one.
#[derive(Debug, Default)]
struct InputReadTimes {
    last_read_at: Option<Instant>,
    last_taken_read_at: Option<Instant>,
}

impl InputReadTimes {
    /// Whether the input thread read an input event that the loop has not
    /// taken.
    fn has_untaken_input(&self) -> bool {
        match (self.last_read_at, self.last_taken_read_at) {
            (Some(last_read_at), Some(last_taken_read_at)) => last_read_at > last_taken_read_at,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }
}

impl InputReadClock {
    /// Stamp an input event the input thread read at `current_time`, and
    /// return the stamp: `current_time`, or one nanosecond past the previous
    /// stamp when `current_time` is at or before it. Each stamp is greater
    /// than the one before it. Example: two events read at the same clock
    /// reading `t` get `t` and `t + 1 ns`.
    pub(crate) fn stamp_input_read(&self, current_time: Instant) -> Instant {
        let mut input_read_times = self.lock_input_read_times();
        let read_at = match input_read_times.last_read_at {
            Some(last_read_at) if current_time <= last_read_at => {
                last_read_at + Duration::from_nanos(1)
            }
            _ => current_time,
        };
        input_read_times.last_read_at = Some(read_at);
        read_at
    }

    /// Record that the loop took the input event stamped `read_at` off its
    /// channel. Input events reach the loop in the order they were read.
    pub(crate) fn note_input_taken(&self, read_at: Instant) {
        self.lock_input_read_times().last_taken_read_at = Some(read_at);
    }

    /// Whether the input thread read an input event that the loop has not
    /// taken.
    pub(crate) fn has_untaken_input(&self) -> bool {
        self.lock_input_read_times().has_untaken_input()
    }

    /// The instant to judge the input timers at, an open sequence's ambiguity
    /// deadline, a placement tab hover and a selection drag's edge scroll: the
    /// clock read under the lock, or `None` while an input event the input
    /// thread read has not reached the loop.
    pub(crate) fn find_input_judge_time(&self) -> Option<Instant> {
        let input_read_times = self.lock_input_read_times();
        if input_read_times.has_untaken_input() {
            return None;
        }
        Some(Instant::now())
    }

    /// The read times, taken over from a thread that panicked holding them.
    fn lock_input_read_times(&self) -> MutexGuard<'_, InputReadTimes> {
        self.input_read_times
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// The chord that backs out of an open multi-chord sequence.
const ESCAPE_KEY_CHORD: KeyChord =
    KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Esc));

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
    /// session encodes it for the focused pane with that pane's cursor-key mode
    /// at the instant it writes.
    PassThrough(KeyChord),
    /// Consumed, and nothing reaches the session: no sequence is open, the
    /// chord binds nothing, and the mode discards what it does not bind; or
    /// an `Esc` ended a floating pane's grab-handle drag and dropped any open
    /// sequence, so the pane and the hint bar are drawn again.
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
    /// Pane placement mode, the move chooser, and floating pane move mode are
    /// local submodes layered over the stored normal or locked mode
    /// ([`find_viewer_local_input_mode`](Self::find_viewer_local_input_mode)).
    /// Entering one changes key resolution only; it does not change the
    /// session lock state. A submode owns the keyboard only while the viewer
    /// is [`LinkStatus::Linked`]: with no link, the stored base mode does, and
    /// `<C-q>` in the default keymap reaches `core:quit`.
    #[must_use]
    pub(crate) fn get_active_input_mode(&self) -> LockMode {
        if self.link_status != LinkStatus::Linked {
            return self.lock_mode;
        }
        self.find_viewer_local_input_mode()
            .unwrap_or(self.lock_mode)
    }

    /// Return the local submode on: `PanePlacement` while pane placement mode
    /// is active, `PaneMoveChoice` while the move chooser is on, and
    /// `FloatingPaneMove` while floating pane move mode is on. `None` when no
    /// submode is on.
    #[must_use]
    pub(crate) fn find_viewer_local_input_mode(&self) -> Option<LockMode> {
        if self.is_placement_mode_active() {
            return Some(LockMode::PanePlacement);
        }
        match self.pane_move_mode {
            Some(PaneMoveMode::Choice) => Some(LockMode::PaneMoveChoice),
            Some(PaneMoveMode::FloatingPaneMove(_)) => Some(LockMode::FloatingPaneMove),
            None => None,
        }
    }

    /// Set the stored base input mode and clear an open sequence, an
    /// unconfirmed placement interaction, the move chooser, and floating pane
    /// move mode that waits for no move.
    ///
    /// Called both when this viewer's own `core:lock` fires and when the
    /// session reports a mode change aimed at this viewer (`koshi lock
    /// --client`). A mode change drops the held chords, and no pane receives
    /// them.
    pub fn set_lock_mode(&mut self, lock_mode: LockMode) {
        if self.lock_mode != lock_mode {
            if self.is_placement_mode_active() {
                self.cancel_placement_mode();
            }
            self.close_pane_move_mode();
            self.lock_mode = lock_mode;
            self.pending_key_sequence = None;
        }
    }

    /// End the move chooser or floating pane move mode, dropping its
    /// selection, the cell its arrow keys moved the pane to, and any open key
    /// sequence. A grab-handle drag under way and a floating pane move already
    /// sent stay. Does nothing when neither mode is on, and while floating
    /// pane move mode waits for the move it sent
    /// ([`is_floating_pane_move_pending`](Self::is_floating_pane_move_pending)).
    pub(crate) fn close_pane_move_mode(&mut self) {
        if self.is_floating_pane_move_pending() {
            return;
        }
        if self.pane_move_mode.take().is_some() {
            self.pending_key_sequence = None;
        }
    }

    /// Whether floating pane move mode waits for the session's answer to the
    /// move it sent.
    #[must_use]
    pub(crate) fn is_floating_pane_move_pending(&self) -> bool {
        matches!(
            self.pane_move_mode,
            Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
                pending_move_command_id: Some(_),
                ..
            }))
        )
    }

    /// Switch to `lock_mode`, the mode that lock command `lock_command_id`
    /// sets, before the session runs that command, and wait for it: painted
    /// frames leave the lock mode as it is until one names `lock_command_id`
    /// as the last applied lock command. The switch is
    /// [`set_lock_mode`](Self::set_lock_mode).
    pub(crate) fn apply_lock_command(&mut self, lock_command_id: CommandId, lock_mode: LockMode) {
        self.set_lock_mode(lock_mode);
        self.pending_lock_command_id = Some(lock_command_id);
    }

    /// The lock command whose lock mode this viewer shows before the session
    /// runs it, or `None` when no lock command waits.
    #[must_use]
    pub(crate) fn get_pending_lock_command_id(&self) -> Option<CommandId> {
        self.pending_lock_command_id
    }

    /// Wait for lock command `pending_lock_command_id` before painted frames
    /// set the lock mode again, or, with `None`, let the next painted frame set
    /// it.
    pub(crate) fn set_pending_lock_command_id(
        &mut self,
        pending_lock_command_id: Option<CommandId>,
    ) {
        self.pending_lock_command_id = pending_lock_command_id;
    }

    /// The chords of an open sequence, for the hint bar's breadcrumb.
    #[must_use]
    pub fn get_pending_key_sequence(&self) -> Option<&KeySequence> {
        self.pending_key_sequence
            .as_ref()
            .map(|pending_key_sequence| &pending_key_sequence.sequence)
    }

    /// Apply one viewer-local action selected by the active keymap, against
    /// `mouse_frame`, the frame this viewer last painted. With no painted
    /// frame, `None`, this viewer draws no floating pane.
    pub(crate) fn apply_client_action(
        &mut self,
        client_action_kind: ClientActionKind,
        mouse_frame: Option<&MouseFrame>,
    ) -> PlacementInputAction {
        match client_action_kind {
            ClientActionKind::BeginPanePlacement => self.begin_pane_move(mouse_frame),
            ClientActionKind::BeginTiledPanePlacement => {
                self.close_pane_move_mode();
                self.begin_tiled_pane_placement()
            }
            ClientActionKind::BeginFloatingPaneMove => self.begin_floating_pane_move(mouse_frame),
            ClientActionKind::ConfirmPaneMoveChoice => self.confirm_pane_move_choice(mouse_frame),
            ClientActionKind::CancelPaneMoveChoice => {
                if self.pane_move_mode == Some(PaneMoveMode::Choice) {
                    self.close_pane_move_mode();
                }
                PlacementInputAction::Consumed
            }
            ClientActionKind::MoveFloatingPane(direction) => {
                self.move_floating_pane_draft(direction, mouse_frame)
            }
            ClientActionKind::SelectNextFloatingPane => {
                self.select_floating_pane_by_offset(1, mouse_frame)
            }
            ClientActionKind::SelectPreviousFloatingPane => {
                self.select_floating_pane_by_offset(-1, mouse_frame)
            }
            ClientActionKind::ConfirmFloatingPaneMove => {
                self.submit_floating_pane_move_draft(mouse_frame)
            }
            ClientActionKind::CancelFloatingPaneMove => {
                if matches!(self.pane_move_mode, Some(PaneMoveMode::FloatingPaneMove(_))) {
                    self.close_pane_move_mode();
                    self.cancel_floating_pane_move_drag();
                }
                PlacementInputAction::Consumed
            }
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
            ClientActionKind::UndockOrDockPane => self.submit_undock_pane(),
        }
    }

    /// Send `PlacePane` that floats the pane holding this viewer's input in
    /// the newest painted frame at the default placement, with no expected
    /// placement revision. Returns `Consumed`, sending nothing, when that
    /// frame focuses no pane.
    ///
    /// `<C-p> u` with tiled pane `pane-7` focused sends
    /// `PlacePane { source_pane_id: pane-7, placement_target: Floating {
    /// top_left_cell: None }, expected_placement_revision: None }`.
    fn submit_undock_pane(&self) -> PlacementInputAction {
        let Some(source_pane_id) = self.focused_pane_id else {
            return PlacementInputAction::Consumed;
        };
        PlacementInputAction::SubmitCommand {
            command_id: CommandId::new(),
            command: Command::PlacePane(PlacePaneArgs {
                source_pane_id,
                placement_target: PanePlacementTarget::Floating {
                    top_left_cell: None,
                },
                expected_placement_revision: None,
            }),
        }
    }

    /// Open the move chooser when `mouse_frame` draws an unpinned floating
    /// pane, and pane placement mode for the active tab's focused pane
    /// otherwise
    /// ([`begin_tiled_pane_placement`](Self::begin_tiled_pane_placement)).
    /// Opening the chooser ends floating pane move mode and drops an open key
    /// sequence. Returns `Consumed`, changing nothing, while pane placement
    /// shows and while floating pane move mode waits for the move it sent.
    ///
    /// `<C-p> m` with floating pane `pane-9` drawn unpinned opens the chooser.
    /// `<C-p> m` with no floating pane drawn previews the focused tiled pane.
    fn begin_pane_move(&mut self, mouse_frame: Option<&MouseFrame>) -> PlacementInputAction {
        if self.is_pane_placement_visible() || self.is_floating_pane_move_pending() {
            return PlacementInputAction::Consumed;
        }
        let has_movable_floating_pane = mouse_frame.is_some_and(|mouse_frame| {
            !self.list_movable_floating_pane_ids(mouse_frame).is_empty()
        });
        if has_movable_floating_pane {
            self.pane_move_mode = Some(PaneMoveMode::Choice);
            self.pending_key_sequence = None;
            return PlacementInputAction::Consumed;
        }
        self.close_pane_move_mode();
        self.begin_tiled_pane_placement()
    }

    /// Open pane placement mode for the active tab's focused pane and return
    /// the preview read of the active tab. Returns `Consumed` when pane
    /// placement mode does not open
    /// ([`begin_placement_mode`](Self::begin_placement_mode)).
    fn begin_tiled_pane_placement(&mut self) -> PlacementInputAction {
        self.begin_placement_mode().map_or(
            PlacementInputAction::Consumed,
            |(source_pane_id, destination_tab_id)| PlacementInputAction::ReadPlacement {
                pane_id_to_focus: None,
                source_pane_id,
                destination_tab_id,
            },
        )
    }

    /// Open floating pane move mode for the floating pane that `mouse_frame`
    /// draws unpinned and focuses, or else for the front-most floating pane it
    /// draws unpinned, and return a `FocusPane` for that pane.
    ///
    /// Active pane placement mode ends first. Opening the mode drops an open
    /// key sequence and every gesture a held button drives
    /// ([`end_held_mouse_gestures`](Self::end_held_mouse_gestures)). With no
    /// such floating pane, or no painted frame, the move chooser and floating
    /// pane move mode end. Returns `Consumed`, changing nothing, while a
    /// placement waits for the session and while floating pane move mode
    /// waits for the move it sent.
    ///
    /// With floating panes `a`, `b` and `c` drawn back to front and a tiled
    /// pane focused, the mode selects `c` and focuses it.
    fn begin_floating_pane_move(
        &mut self,
        mouse_frame: Option<&MouseFrame>,
    ) -> PlacementInputAction {
        if self.is_placement_confirmation_pending() || self.is_floating_pane_move_pending() {
            return PlacementInputAction::Consumed;
        }
        if self.is_placement_mode_active() {
            self.cancel_placement_mode();
        }
        let Some(mouse_frame) = mouse_frame else {
            self.close_pane_move_mode();
            return PlacementInputAction::Consumed;
        };
        let movable_pane_ids = self.list_movable_floating_pane_ids(mouse_frame);
        let selected_pane_id = match mouse_frame.client_snapshot.focused_pane_id {
            Some(focused_pane_id) if movable_pane_ids.contains(&focused_pane_id) => {
                Some(focused_pane_id)
            }
            _ => movable_pane_ids.last().copied(),
        };
        let Some(selected_pane_id) = selected_pane_id else {
            self.close_pane_move_mode();
            return PlacementInputAction::Consumed;
        };
        self.end_held_mouse_gestures();
        self.pending_key_sequence = None;
        self.pane_move_mode = Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
            selected_pane_id,
            draft_top_left_cell: None,
            pending_move_command_id: None,
        }));
        self.build_floating_pane_focus_input_action(selected_pane_id)
    }

    /// Open floating pane move mode when `mouse_frame` focuses a floating
    /// pane ([`begin_floating_pane_move`](Self::begin_floating_pane_move)),
    /// and pane placement mode for the active tab's focused pane otherwise
    /// ([`begin_tiled_pane_placement`](Self::begin_tiled_pane_placement)).
    /// With no painted frame, the focused pane counts as tiled.
    fn confirm_pane_move_choice(
        &mut self,
        mouse_frame: Option<&MouseFrame>,
    ) -> PlacementInputAction {
        let is_focused_pane_floating = mouse_frame.is_some_and(|mouse_frame| {
            is_focused_pane_floating(&mouse_frame.session_snapshot, &mouse_frame.client_snapshot)
        });
        if is_focused_pane_floating {
            return self.begin_floating_pane_move(mouse_frame);
        }
        self.close_pane_move_mode();
        self.begin_tiled_pane_placement()
    }

    /// Move the floating pane that floating pane move mode selected one cell
    /// in `direction` in this viewer's view, and keep the cell as the mode's
    /// draft until Enter sends it.
    ///
    /// The move starts from the cell `mouse_frame` draws the pane at with this
    /// viewer's current draft, and stops at the pane area's edges: the draft
    /// is the cell the frame draws the moved pane at
    /// ([`FrameLayout::list_drawn_floating_pane_slots`]). Nothing reaches the
    /// session. Returns `Consumed`, changing nothing, outside floating pane
    /// move mode, while a sent floating pane move waits for the session, while
    /// the mode waits for the move it sent
    /// ([`is_floating_pane_move_pending`](Self::is_floating_pane_move_pending)),
    /// while a grab-handle drag is under way, and when the frame does not draw
    /// the selected pane or draws it pinned.
    ///
    /// A `20x8` pane drawn at `(10, 4)` moves to `(11, 4)` on Right, and a pane
    /// drawn at column `0` stays at column `0` on Left.
    ///
    /// [`FrameLayout::list_drawn_floating_pane_slots`]: koshi_renderer::snapshot::FrameLayout::list_drawn_floating_pane_slots
    fn move_floating_pane_draft(
        &mut self,
        direction: Direction,
        mouse_frame: Option<&MouseFrame>,
    ) -> PlacementInputAction {
        let Some(PaneMoveMode::FloatingPaneMove(floating_pane_move_selection)) =
            self.pane_move_mode
        else {
            return PlacementInputAction::Consumed;
        };
        let Some(mouse_frame) = mouse_frame else {
            return PlacementInputAction::Consumed;
        };
        if self.sent_floating_pane_move.is_some()
            || self.is_floating_pane_move_pending()
            || self.is_floating_pane_move_drag_active()
        {
            return PlacementInputAction::Consumed;
        }
        let selected_pane_id = floating_pane_move_selection.selected_pane_id;
        let Some(floating_pane_slot) = self
            .build_frame_layout_with_current_chrome(mouse_frame)
            .find_drawn_floating_pane_slot(selected_pane_id)
        else {
            return PlacementInputAction::Consumed;
        };
        if floating_pane_slot.is_pinned {
            return PlacementInputAction::Consumed;
        }
        let drawn_top_left_cell = floating_pane_slot.outer_rect.origin;
        let moved_top_left_cell = match direction {
            Direction::Left => Point {
                column: drawn_top_left_cell.column.saturating_sub(1),
                row: drawn_top_left_cell.row,
            },
            Direction::Right => Point {
                column: drawn_top_left_cell.column.saturating_add(1),
                row: drawn_top_left_cell.row,
            },
            Direction::Up => Point {
                column: drawn_top_left_cell.column,
                row: drawn_top_left_cell.row.saturating_sub(1),
            },
            Direction::Down => Point {
                column: drawn_top_left_cell.column,
                row: drawn_top_left_cell.row.saturating_add(1),
            },
        };
        self.set_floating_pane_move_draft(Some(moved_top_left_cell));
        let clamped_top_left_cell = self
            .build_frame_layout_with_current_chrome(mouse_frame)
            .find_drawn_floating_pane_slot(selected_pane_id)
            .map(|floating_pane_slot| floating_pane_slot.outer_rect.origin);
        self.set_floating_pane_move_draft(clamped_top_left_cell);
        PlacementInputAction::Consumed
    }

    /// Select the floating pane `pane_index_offset` steps after the selected
    /// one, `1` for the next and `-1` for the previous, among the floating
    /// panes that `mouse_frame` draws unpinned, in the order the panes became
    /// floating, wrapping around at either end. The new selection drops the
    /// draft and any grab-handle drag, and returns a `FocusPane` for the pane.
    ///
    /// A selected pane that is not one of those panes moves the selection to
    /// the first of them for `1` and to the last for `-1`. Returns `Consumed`,
    /// changing nothing, outside floating pane move mode, while the mode waits
    /// for the move it sent, and when the step lands on the selected pane.
    ///
    /// With floating panes `a`, `b` and `c` made in that order and `c`
    /// selected, `-1` selects `b`, then `a`, then `c`.
    fn select_floating_pane_by_offset(
        &mut self,
        pane_index_offset: isize,
        mouse_frame: Option<&MouseFrame>,
    ) -> PlacementInputAction {
        let Some(PaneMoveMode::FloatingPaneMove(floating_pane_move_selection)) =
            self.pane_move_mode
        else {
            return PlacementInputAction::Consumed;
        };
        if floating_pane_move_selection
            .pending_move_command_id
            .is_some()
        {
            return PlacementInputAction::Consumed;
        }
        let Some(mouse_frame) = mouse_frame else {
            return PlacementInputAction::Consumed;
        };
        let movable_slot_pane_ids: HashSet<PaneId> = self
            .list_movable_floating_pane_ids(mouse_frame)
            .into_iter()
            .collect();
        let movable_pane_ids: Vec<PaneId> = mouse_frame
            .session_snapshot
            .floating_pane_ids
            .iter()
            .copied()
            .filter(|pane_id| movable_slot_pane_ids.contains(pane_id))
            .collect();
        let movable_pane_count = movable_pane_ids.len();
        if movable_pane_count == 0 {
            return PlacementInputAction::Consumed;
        }
        let selected_pane_index = movable_pane_ids
            .iter()
            .position(|&pane_id| pane_id == floating_pane_move_selection.selected_pane_id);
        let next_pane_index = match selected_pane_index {
            Some(selected_pane_index) => (selected_pane_index as isize + pane_index_offset)
                .rem_euclid(movable_pane_count as isize)
                as usize,
            None if pane_index_offset > 0 => 0,
            None => movable_pane_count - 1,
        };
        let next_pane_id = movable_pane_ids[next_pane_index];
        if next_pane_id == floating_pane_move_selection.selected_pane_id {
            return PlacementInputAction::Consumed;
        }
        self.cancel_floating_pane_move_drag();
        self.pane_move_mode = Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
            selected_pane_id: next_pane_id,
            draft_top_left_cell: None,
            pending_move_command_id: None,
        }));
        self.build_floating_pane_focus_input_action(next_pane_id)
    }

    /// Send the cell the arrow keys moved the selected floating pane to as
    /// one `MoveFloatingPane` under a new command id, record it as the sent
    /// floating pane move that the mode waits for, and drop the draft
    /// ([`record_sent_floating_pane_move`](Self::record_sent_floating_pane_move)).
    ///
    /// The move names the cell `mouse_frame` draws the pane at, inside the
    /// pane area. A draft at the cell the session places the pane at drops
    /// the draft and sends nothing, and so does a frame that does not draw the
    /// pane or draws it pinned. Returns `Consumed`, changing nothing, outside
    /// floating pane move mode, with no draft, and while a grab-handle drag is
    /// under way.
    fn submit_floating_pane_move_draft(
        &mut self,
        mouse_frame: Option<&MouseFrame>,
    ) -> PlacementInputAction {
        let Some(PaneMoveMode::FloatingPaneMove(floating_pane_move_selection)) =
            self.pane_move_mode
        else {
            return PlacementInputAction::Consumed;
        };
        if floating_pane_move_selection.draft_top_left_cell.is_none()
            || self.is_floating_pane_move_drag_active()
        {
            return PlacementInputAction::Consumed;
        }
        let Some(mouse_frame) = mouse_frame else {
            return PlacementInputAction::Consumed;
        };
        let pane_id = floating_pane_move_selection.selected_pane_id;
        let drawn_floating_pane_slot = self
            .build_frame_layout_with_current_chrome(mouse_frame)
            .find_drawn_floating_pane_slot(pane_id);
        let session_floating_pane_slot = mouse_frame
            .session_snapshot
            .floating_pane_slots
            .iter()
            .find(|floating_pane_slot| floating_pane_slot.pane_id == pane_id);
        self.set_floating_pane_move_draft(None);
        let (Some(drawn_floating_pane_slot), Some(session_floating_pane_slot)) =
            (drawn_floating_pane_slot, session_floating_pane_slot)
        else {
            return PlacementInputAction::Consumed;
        };
        let top_left_cell = drawn_floating_pane_slot.outer_rect.origin;
        if drawn_floating_pane_slot.is_pinned
            || top_left_cell == session_floating_pane_slot.outer_rect.origin
        {
            return PlacementInputAction::Consumed;
        }
        let command_id = self.record_sent_floating_pane_move(pane_id, top_left_cell);
        PlacementInputAction::SubmitCommand {
            command_id,
            command: Command::MoveFloatingPane(MoveFloatingPaneArgs {
                pane_id,
                top_left_cell,
            }),
        }
    }

    /// Set the cell the arrow keys moved floating pane move mode's selected
    /// pane to, or drop it with `None`. Does nothing outside floating pane
    /// move mode.
    pub(crate) fn set_floating_pane_move_draft(&mut self, draft_top_left_cell: Option<Point>) {
        if let Some(PaneMoveMode::FloatingPaneMove(floating_pane_move_selection)) =
            self.pane_move_mode.as_mut()
        {
            floating_pane_move_selection.draft_top_left_cell = draft_top_left_cell;
        }
    }

    /// The floating panes that `mouse_frame` draws unpinned with this viewer's
    /// current chrome, back to front.
    fn list_movable_floating_pane_ids(&self, mouse_frame: &MouseFrame) -> Vec<PaneId> {
        self.build_frame_layout_with_current_chrome(mouse_frame)
            .list_drawn_floating_pane_slots()
            .filter(|floating_pane_slot| !floating_pane_slot.is_pinned)
            .map(|floating_pane_slot| floating_pane_slot.pane_id)
            .collect()
    }

    /// A key-binding `FocusPane` for the floating pane `pane_id`, which also
    /// raises it, whichever pane the painted frame focuses.
    fn build_floating_pane_focus_input_action(&self, pane_id: PaneId) -> PlacementInputAction {
        PlacementInputAction::SubmitCommand {
            command_id: CommandId::new(),
            command: Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Pane(pane_id),
                client_id: Some(self.client_id),
            }),
        }
    }

    /// Select the next or previous visible tab for placement mode.
    fn select_placement_tab_by_offset(&mut self, tab_index_offset: isize) -> PlacementInputAction {
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
        let target_tab_index = destination_tab_index as isize + tab_index_offset;
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
            Some(TiledPlacementTarget::Split {
                anchor: PanePlacementAnchor::Tab,
                ..
            })
        );
        let placement_target = if is_insertion_target && is_whole_tab_target_selected {
            Some(TiledPlacementTarget::Split {
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
                .map(|insertion_span| TiledPlacementTarget::Split {
                    destination_tab_id,
                    anchor: insertion_span.anchor.clone(),
                    direction,
                })
            } else {
                Some(TiledPlacementTarget::Swap { target_pane_id })
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
            let placement_target = TiledPlacementTarget::Split {
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
            .and_then(resolve_placement_target_anchor)
            .and_then(|anchor| {
                insertion_spans
                    .iter()
                    .position(|insertion_span| insertion_span.anchor == anchor)
            });
        let next_span_index =
            current_span_index.map_or(0, |span_index| (span_index + 1) % insertion_spans.len());
        let insertion_span = &insertion_spans[next_span_index];
        let placement_target = TiledPlacementTarget::Split {
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
        placement_mode.placement_target = Some(TiledPlacementTarget::Split {
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
            placement_target: PanePlacementTarget::Tiled(placement_target),
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
        Some(PlacementInputAction::SubmitCommand {
            command_id,
            command,
        })
    }

    /// Decide what `chord` means in this viewer's current mode.
    ///
    /// `<C-l>` while locked yields `Fire(core:unlock)` whatever the keymap
    /// says; `<C-p>` in the default keymap opens the pane group and yields
    /// `Pending`; a plain `a` with nothing bound yields `PassThrough('a')`.
    /// Escape while a floating pane's grab handle is held drops that move and
    /// any open key sequence, and yields `Discard`.
    pub fn resolve_key(&mut self, chord: KeyChord, current_time: Instant) -> KeyOutcome {
        if chord == ESCAPE_KEY_CHORD && self.cancel_floating_pane_move_drag() {
            self.pending_key_sequence = None;
            return KeyOutcome::Discard;
        }
        let active_input_mode = self.get_active_input_mode();
        let open_key_sequence = self.pending_key_sequence.take();

        // In locked base mode the unlock chord fires before the keymap is
        // read. While placement mode owns the keyboard, this check is skipped
        // and the placement keymap resolves the chord.
        if active_input_mode == LockMode::Locked && chord == self.keymap_catalog.get_unlock_chord()
        {
            return KeyOutcome::Fire(build_unlock_bound_action());
        }

        // The open sequence's chords with this one after them, or this one on
        // its own.
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
                self.rearm_continuous_sequence_prefix(&bound_action, &key_sequence);
                KeyOutcome::Fire(bound_action)
            }
            (exact_bound_action, true) => {
                // A prefix-only sequence waits for its next chord with no
                // deadline; only exact-plus-longer ambiguity arms one, and
                // reaching it fires the exact binding.
                let ambiguity_deadline = exact_bound_action
                    .is_some()
                    .then(|| current_time + self.keymap_catalog.get_chord_timeout());
                self.pending_key_sequence = Some(PendingKeySequence {
                    sequence: key_sequence,
                    ambiguity_deadline,
                });
                KeyOutcome::Pending
            }
            (None, false) => match open_key_sequence {
                // Escape leaves an open sequence: the held chords are dropped
                // and the Escape itself is consumed rather than typed.
                Some(_) if chord == ESCAPE_KEY_CHORD => KeyOutcome::Pending,
                // A key that continues nothing is discarded, and the sequence
                // stands unchanged, deadline included.
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

    /// How long until an open sequence's ambiguity deadline. A prefix-only
    /// sequence carries no deadline and returns `None`. While an input event
    /// this terminal's input thread read has not reached the loop
    /// (`InputReadClock::has_untaken_input`), the answer is `None` too: that
    /// event's arrival wakes the loop.
    #[must_use]
    pub fn compute_next_key_wakeup(&self, current_time: Instant) -> Option<Duration> {
        if self.input_read_clock.has_untaken_input() {
            return None;
        }
        self.pending_key_sequence
            .as_ref()
            .and_then(|pending_key_sequence| pending_key_sequence.ambiguity_deadline)
            .map(|ambiguity_deadline| ambiguity_deadline.saturating_duration_since(current_time))
    }

    /// Fire the open sequence's complete binding if its ambiguity deadline has
    /// passed.
    ///
    /// A sequence whose binding a keymap change retired while it waited
    /// resolves to nothing: the held chords are dropped, never typed at the
    /// pane, and the call returns `None`.
    pub fn expire_key_sequence(&mut self, current_time: Instant) -> Option<BoundAction> {
        let is_ambiguity_deadline_due = self
            .pending_key_sequence
            .as_ref()
            .and_then(|pending_key_sequence| pending_key_sequence.ambiguity_deadline)
            .is_some_and(|ambiguity_deadline| ambiguity_deadline <= current_time);
        if !is_ambiguity_deadline_due {
            return None;
        }
        let pending_key_sequence = self.pending_key_sequence.take()?;
        let bound_action = self
            .keymap_catalog
            .match_sequence(self.get_active_input_mode(), &pending_key_sequence.sequence)
            .exact_bound_action?;
        self.rearm_continuous_sequence_prefix(&bound_action, &pending_key_sequence.sequence);
        Some(bound_action)
    }

    /// The read times of this terminal's input events, shared with its input
    /// thread.
    pub(crate) fn get_input_read_clock(&self) -> &InputReadClock {
        &self.input_read_clock
    }

    /// Re-open the prefix of a sequence whose action the registry marks
    /// `is_continuous`, so a repeated final chord repeats the action: `<C-p> r →`
    /// leaves `<C-p> r` open, and each further `→` resizes again.
    ///
    /// Only multi-chord sequences have a prefix to hold. The re-armed prefix
    /// captures the keyboard like any other open sequence, so a key that
    /// resizes nothing is discarded and the prefix stands until `Esc` leaves
    /// it.
    fn rearm_continuous_sequence_prefix(
        &mut self,
        bound_action: &BoundAction,
        key_sequence: &KeySequence,
    ) {
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
            ambiguity_deadline: None,
        });
    }
}

/// Return whether an interactive target preserves the source tab's layout.
pub(crate) fn is_same_tab_placement_noop(
    placement_snapshot: &PanePlacementSnapshot,
    placement_target: &TiledPlacementTarget,
) -> bool {
    if placement_snapshot.source_tab_id != placement_snapshot.destination_tab_id {
        return false;
    }
    if let TiledPlacementTarget::Split {
        destination_tab_id, ..
    } = placement_target
    {
        if *destination_tab_id != placement_snapshot.source_tab_id {
            return false;
        }
    }
    let layout_target = PlacementTarget::from_tiled_placement_target(placement_target);
    let source_tab_snapshot = &placement_snapshot.source_tab_snapshot;
    let tab_rect = Rect::from_size_at_origin(source_tab_snapshot.tab_size);
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
    let tab_rect = Rect::from_size_at_origin(destination_tab_snapshot.tab_size);
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
    placement_target: &TiledPlacementTarget,
    candidate_pane_rects: &[(PaneId, Rect)],
    insertion_spans: &[InsertionSpan],
) -> Option<Rect> {
    match placement_target {
        TiledPlacementTarget::Swap { target_pane_id }
        | TiledPlacementTarget::Split {
            anchor: PanePlacementAnchor::Pane(target_pane_id),
            ..
        } => candidate_pane_rects
            .iter()
            .find(|(pane_id, _)| pane_id == target_pane_id)
            .map(|(_, pane_rect)| *pane_rect),
        TiledPlacementTarget::Split {
            anchor: PanePlacementAnchor::Group(pane_ids),
            ..
        } => insertion_spans
            .iter()
            .find(|insertion_span| {
                insertion_span.anchor == PanePlacementAnchor::Group(pane_ids.clone())
            })
            .map(|insertion_span| insertion_span.span_rect),
        TiledPlacementTarget::Split {
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
    current_placement_target: Option<&TiledPlacementTarget>,
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
    let first_candidate_pane_and_rect = candidate_pane_rects.first().copied();
    let current_pane_rect = current_placement_target
        .and_then(|placement_target| {
            find_placement_target_rect(
                placement_target,
                &candidate_pane_rects,
                &placement_destinations.insertion_spans,
            )
        })
        .or_else(|| source_pane_slot.map(|source_pane_slot| source_pane_slot.outer_rect))
        .or_else(|| first_candidate_pane_and_rect.map(|(_, pane_rect)| pane_rect))?;
    let neighbor_pane_id =
        select_directional_neighbor(current_pane_rect, &candidate_pane_rects, direction);
    if neighbor_pane_id.is_none() && current_placement_target.is_none() {
        return first_candidate_pane_and_rect.map(|(pane_id, _)| pane_id);
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
    u32::from(insertion_span.span_rect.size.column_count)
        * u32::from(insertion_span.span_rect.size.row_count)
}

/// Return the insertion anchor named by a placement target.
fn resolve_placement_target_anchor(
    placement_target: &TiledPlacementTarget,
) -> Option<PanePlacementAnchor> {
    match placement_target {
        TiledPlacementTarget::Swap { target_pane_id } => {
            Some(PanePlacementAnchor::Pane(*target_pane_id))
        }
        TiledPlacementTarget::Split { anchor, .. } => Some(anchor.clone()),
    }
}

/// The binding the unlock chord fires, built here and never looked up in the
/// keymap. The escape from locked mode holds whatever any config layer says.
fn build_unlock_bound_action() -> BoundAction {
    BoundAction {
        action_reference: ActionReference::from_core_action_name("unlock")
            .expect("the reserved unlock action name satisfies the action-name grammar"),
    }
}
