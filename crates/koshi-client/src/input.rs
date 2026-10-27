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
//!
//! **The hint bar shows an open sequence after `which-key-delay-ms`.** The
//! delay runs from the sequence's first chord. Until it ends, the hint bar
//! keeps the face it had before that chord, so a sequence that closes sooner
//! never changes the bar. Once the bar shows the sequence, each further chord
//! of it changes the bar at once. Example: with a delay of 300 ms, `<C-p> n`
//! typed within 300 ms draws nothing, and `<C-p>` left open for 300 ms draws
//! the `<C-p>` breadcrumb and the chords that continue it.
//!
//! **Viewer-local modes run here too.** Pane placement mode, the move
//! chooser, floating pane move mode, dock/undock mode, and the floating pane
//! tray each own the keyboard while they are on and the viewer is linked.
//! `Client::apply_client_action` runs one of their actions: it changes the
//! mode, or returns the session command that the mode sends.

use std::collections::HashSet;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use koshi_config::types::BoundAction;
use koshi_core::action::{ActionReference, ClientActionKind};
use koshi_core::command::{
    Command, FocusPaneArgs, FocusTarget, MoveFloatingPaneArgs, PanePlacementAnchor,
    PanePlacementTarget, PlacePaneArgs, TiledPlacementTarget,
};
use koshi_core::geometry::{Direction, Point, Rect};
use koshi_core::ids::{CommandId, PaneId};
use koshi_core::key::{
    BindingModifierFlags, Key, KeyChord, KeySequence, NamedKey, PendingKeySequence,
};
use koshi_core::lock::LockMode;
use koshi_layout::neighbor::select_directional_neighbor;
use koshi_layout::placement::{
    find_stack_target_pane_id, is_same_tab_placement_noop as is_layout_placement_noop,
    InsertionSpan, PlacementDestinations, PlacementTarget,
};
use koshi_renderer::snapshot::{FloatingPane, MouseFrame, PlacementSnapshot, TabSnapshot};

use crate::{
    is_focused_pane_floating, Client, DockUndockMode, FloatingPaneMoveSelection, LinkStatus,
    PaneMoveMode, PlacementInputAction, PlacementModeLifetime, PlacementTargetOutcome,
    TiledPaneSelection,
};

#[cfg(test)]
pub(crate) mod tests;

/// The read times of this terminal's input events, shared by its input thread
/// and the attach loop under one lock.
///
/// The input thread stamps the events of each read of the terminal it sends,
/// keys, mouse events, a paste, a resize or a focus loss
/// ([`stamp_input_read`](Self::stamp_input_read)), and the loop records each
/// read it takes off its channel ([`note_input_taken`](Self::note_input_taken)).
/// A read stamped and not yet taken is on its way to the loop.
///
/// Example: the input thread reads `a` 400 ms after `<C-y>` opened a sequence
/// whose deadline is 500 ms out, and the loop is still drawing a frame at
/// 600 ms. [`find_input_judge_time`](Self::find_input_judge_time) answers
/// `None` until the loop takes `a`, and `a` then continues the sequence.
#[derive(Debug, Default)]
pub(crate) struct InputReadClock {
    input_read_times: Mutex<InputReadTimes>,
}

/// The stamp of the last read the input thread sent, and of the last one the
/// loop took. `None` before the first one.
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
    /// Stamp the events of one read the input thread made at `current_time`,
    /// and return the stamp: `current_time`, or one nanosecond past the
    /// previous stamp when `current_time` is at or before it. Each stamp is
    /// greater than the one before it. Example: two reads at the same clock
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

    /// Record that the loop took the read stamped `read_at` off its channel.
    /// Reads reach the loop in the order they were made.
    pub(crate) fn note_input_taken(&self, read_at: Instant) {
        self.lock_input_read_times().last_taken_read_at = Some(read_at);
    }

    /// Whether the input thread read an input event that the loop has not
    /// taken.
    pub(crate) fn has_untaken_input(&self) -> bool {
        self.lock_input_read_times().has_untaken_input()
    }

    /// The instant to judge the input timers at, an open sequence's ambiguity
    /// deadline and its hint deadline, a placement tab hover and a selection
    /// drag's edge scroll: the clock read under the lock, or `None` while an
    /// input event the input thread read has not reached the loop.
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
    /// bar changes only when it shows the sequence before or after the chord
    /// ([`Client::is_continuation_hint_hidden`]); nothing else changes.
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
    /// The viewer's stored base input mode. A local submode leaves it as it
    /// is; `get_active_input_mode` names the mode that owns the keyboard.
    #[must_use]
    pub fn get_lock_mode(&self) -> LockMode {
        self.lock_mode
    }

    /// Return the keymap mode that currently owns this viewer's keyboard.
    ///
    /// The floating pane tray, the open notification center, dock/undock
    /// mode, pane placement mode, the move chooser, and floating pane move mode
    /// are local submodes layered over the stored normal or locked mode
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

    /// Return the local submode on: `FloatingPaneTray` while the floating pane
    /// tray is open, `NotificationCenter` while the notification center is
    /// open, `DockUndock` while dock/undock mode is on, also while its
    /// pane placement mode is active, `PanePlacement` while pane placement
    /// mode is active, `PaneMoveChoice` while the move chooser is on, and
    /// `FloatingPaneMove` while floating pane move mode is on. `None` when no
    /// submode is on.
    #[must_use]
    pub(crate) fn find_viewer_local_input_mode(&self) -> Option<LockMode> {
        if self.floating_pane_tray.is_some() {
            return Some(LockMode::FloatingPaneTray);
        }
        if self.notification_history.is_notification_center_open() {
            return Some(LockMode::NotificationCenter);
        }
        if self.dock_undock_mode.is_some() {
            return Some(LockMode::DockUndock);
        }
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
    /// unconfirmed placement interaction, the move chooser, floating pane move
    /// mode that waits for no move, dock/undock mode that waits for no undock
    /// or dock, and the floating pane tray, and close an open notification
    /// center (`close_notification_center`).
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
            self.close_dock_undock_mode();
            self.close_floating_pane_tray();
            self.close_notification_center();
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

    /// Leave dock/undock mode: end the pane placement mode of its selected
    /// floating pane, and drop any open key sequence. Does nothing when the
    /// mode is off, and while it waits for the session's answer to the undock
    /// or the dock it sent
    /// ([`is_dock_undock_command_pending`](Self::is_dock_undock_command_pending)).
    pub(crate) fn close_dock_undock_mode(&mut self) {
        if self.dock_undock_mode.is_none() || self.is_dock_undock_command_pending() {
            return;
        }
        self.dock_undock_mode = None;
        self.cancel_placement_mode();
        self.pending_key_sequence = None;
    }

    /// End what a loss of the outer terminal's focus ends: a grab-handle
    /// drag, pane placement mode that waits for no placement, the move
    /// chooser, floating pane move mode that waits for no move, dock/undock
    /// mode that waits for no undock or dock, and the floating pane tray.
    /// Sends nothing.
    pub(crate) fn handle_outer_terminal_focus_lost(&mut self) {
        self.cancel_floating_pane_move_drag();
        self.cancel_placement_mode();
        self.close_pane_move_mode();
        self.close_dock_undock_mode();
        self.close_floating_pane_tray();
    }

    /// Whether dock/undock mode waits for the session's answer to the undock
    /// of its selected tiled pane, or to the dock of its selected floating
    /// pane.
    #[must_use]
    pub(crate) fn is_dock_undock_command_pending(&self) -> bool {
        let Some(dock_undock_mode) = self.dock_undock_mode else {
            return false;
        };
        let is_undock_pending =
            dock_undock_mode
                .tiled_pane_selection
                .is_some_and(|tiled_pane_selection| {
                    tiled_pane_selection.pending_undock_command_id.is_some()
                });
        is_undock_pending || self.is_placement_confirmation_pending()
    }

    /// Drop the undock `command_id` names, which the session rejected, and
    /// return `true`. The tiled pane stays selected, and a drag that began on
    /// it ends. Returns `false`, changing nothing, when `command_id` names no
    /// undock that dock/undock mode waits for.
    pub(crate) fn reject_undock_command(&mut self, command_id: CommandId) -> bool {
        let Some(tiled_pane_selection) = self
            .dock_undock_mode
            .as_mut()
            .and_then(|dock_undock_mode| dock_undock_mode.tiled_pane_selection.as_mut())
            .filter(|tiled_pane_selection| {
                tiled_pane_selection.pending_undock_command_id == Some(command_id)
            })
        else {
            return false;
        };
        tiled_pane_selection.pending_undock_command_id = None;
        self.cancel_floating_pane_move_drag();
        true
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
    /// [`set_lock_mode`](Self::set_lock_mode). An open notification center
    /// closes ([`close_notification_center`](Self::close_notification_center)),
    /// also when `lock_mode` is the mode already on.
    pub(crate) fn apply_lock_command(&mut self, lock_command_id: CommandId, lock_mode: LockMode) {
        self.close_notification_center();
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

    /// The chords of an open sequence. The hint bar draws them as a
    /// breadcrumb while [`is_continuation_hint_hidden`](Self::is_continuation_hint_hidden)
    /// is `false`.
    #[must_use]
    pub fn get_pending_key_sequence(&self) -> Option<&KeySequence> {
        self.pending_key_sequence
            .as_ref()
            .map(|pending_key_sequence| &pending_key_sequence.sequence)
    }

    /// Whether an open sequence waits for its hint deadline, `which-key-delay-ms`
    /// after its first chord, before the hint bar shows it. `false` when no
    /// sequence is open. [`expire_key_sequence`](Self::expire_key_sequence)
    /// at or past the deadline makes it `false`.
    #[must_use]
    pub fn is_continuation_hint_hidden(&self) -> bool {
        self.pending_key_sequence
            .as_ref()
            .is_some_and(|pending_key_sequence| {
                pending_key_sequence
                    .continuation_hint_hidden_until
                    .is_some()
            })
    }

    /// Apply one viewer-local action selected by the active keymap, against
    /// `mouse_frame`, the frame this viewer last painted. With no painted
    /// frame, `None`, this viewer draws no floating pane. Returns the command
    /// or the placement confirmation the attachment sends next, or
    /// [`PlacementInputAction::Consumed`] when the action sends neither.
    ///
    /// The floating pane tray and the notification center refuse each
    /// other's actions: while the tray is open, a notification center action
    /// does nothing and the tray stays open, and while the center is open, a
    /// floating pane tray action does nothing and the center stays open. Any
    /// other action that is not a tray action closes the tray first
    /// ([`close_floating_pane_tray`](Self::close_floating_pane_tray)), and any
    /// other action that is not a center action closes the center first
    /// ([`close_notification_center`](Self::close_notification_center)).
    ///
    /// Example: `<A-n>` bound to `core:toggle-notification-center` in the
    /// `floating-pane-tray` mode does nothing while the tray is open.
    pub(crate) fn apply_client_action(
        &mut self,
        client_action_kind: ClientActionKind,
        mouse_frame: Option<&MouseFrame>,
    ) -> PlacementInputAction {
        let is_floating_pane_tray_action = matches!(
            client_action_kind,
            ClientActionKind::ToggleFloatingPaneTray
                | ClientActionKind::CloseFloatingPaneTray
                | ClientActionKind::SelectTrayTile(_)
                | ClientActionKind::ToggleSelectedTrayTile
        );
        let is_notification_center_action = matches!(
            client_action_kind,
            ClientActionKind::ToggleNotificationCenter
                | ClientActionKind::CloseNotificationCenter
                | ClientActionKind::ClearNotifications
                | ClientActionKind::SelectNewerNotification
                | ClientActionKind::SelectOlderNotification
                | ClientActionKind::DismissSelectedNotification
        );
        let is_floating_pane_tray_open = self.floating_pane_tray.is_some();
        let is_notification_center_open = self.notification_history.is_notification_center_open();
        if (is_notification_center_action && is_floating_pane_tray_open)
            || (is_floating_pane_tray_action && is_notification_center_open)
        {
            return PlacementInputAction::Consumed;
        }
        if !is_floating_pane_tray_action {
            self.close_floating_pane_tray();
        }
        if !is_notification_center_action {
            self.close_notification_center();
        }
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
            ClientActionKind::TogglePanePlacementStack => self.toggle_placement_stack(),
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
            ClientActionKind::BeginDockUndock => self.begin_dock_undock(mouse_frame),
            ClientActionKind::ConfirmDockUndock => self.confirm_dock_undock(),
            ClientActionKind::CancelDockUndock => {
                self.close_dock_undock_mode();
                PlacementInputAction::Consumed
            }
            ClientActionKind::ToggleFloatingPaneTray => {
                self.toggle_floating_pane_tray(mouse_frame);
                PlacementInputAction::Consumed
            }
            ClientActionKind::CloseFloatingPaneTray => {
                self.close_floating_pane_tray();
                PlacementInputAction::Consumed
            }
            ClientActionKind::SelectTrayTile(direction) => {
                self.select_tray_tile(direction, mouse_frame);
                PlacementInputAction::Consumed
            }
            ClientActionKind::ToggleSelectedTrayTile => self.toggle_selected_tray_tile(mouse_frame),
            ClientActionKind::ToggleNotificationCenter => {
                self.toggle_notification_center(mouse_frame);
                PlacementInputAction::Consumed
            }
            ClientActionKind::CloseNotificationCenter => {
                self.close_notification_center();
                PlacementInputAction::Consumed
            }
            ClientActionKind::ClearNotifications => {
                self.clear_notifications();
                PlacementInputAction::Consumed
            }
            ClientActionKind::SelectNewerNotification => {
                self.select_adjacent_notification(true, mouse_frame);
                PlacementInputAction::Consumed
            }
            ClientActionKind::SelectOlderNotification => {
                self.select_adjacent_notification(false, mouse_frame);
                PlacementInputAction::Consumed
            }
            ClientActionKind::DismissSelectedNotification => {
                self.dismiss_selected_notification(mouse_frame);
                PlacementInputAction::Consumed
            }
        }
    }

    /// Whether the move chooser and dock/undock mode refuse to open:
    /// dock/undock mode is on, pane placement shows, or floating pane move
    /// mode waits for the move it sent.
    fn is_move_or_dock_undock_entry_refused(&self) -> bool {
        self.dock_undock_mode.is_some()
            || self.is_pane_placement_visible()
            || self.is_floating_pane_move_pending()
    }

    /// Open dock/undock mode with the pane that holds this viewer's input in
    /// `mouse_frame` selected, ending the move chooser and floating pane move
    /// mode, and dropping an open key sequence and every gesture a held button
    /// drives ([`end_held_mouse_gestures`](Self::end_held_mouse_gestures)).
    ///
    /// - A floating pane opens pane placement mode for it, which previews the
    ///   frame's active tab with the insertion at the right edge of that whole
    ///   tab selected, and the result is `Consumed`.
    /// - A tiled pane stays selected for Enter, and the result is `Consumed`.
    /// - With no painted frame, or a frame that focuses no pane, nothing is
    ///   selected.
    ///
    /// Returns `Consumed`, changing nothing, while
    /// [`is_move_or_dock_undock_entry_refused`](Self::is_move_or_dock_undock_entry_refused).
    ///
    /// `<C-p> u` with floating pane `pane-9` focused over tab `#1` `[pane-3]`
    /// selects `pane-9`, and Enter then gives `[pane-3 | pane-9]`.
    fn begin_dock_undock(&mut self, mouse_frame: Option<&MouseFrame>) -> PlacementInputAction {
        if self.is_move_or_dock_undock_entry_refused() {
            return PlacementInputAction::Consumed;
        }
        self.close_pane_move_mode();
        self.end_held_mouse_gestures();
        self.pending_key_sequence = None;
        self.dock_undock_mode = Some(DockUndockMode::default());
        let Some(mouse_frame) = mouse_frame else {
            return PlacementInputAction::Consumed;
        };
        let Some(selected_pane_id) = mouse_frame.client_snapshot.focused_pane_id else {
            return PlacementInputAction::Consumed;
        };
        if !mouse_frame
            .session_snapshot
            .is_floating_pane(selected_pane_id)
        {
            self.dock_undock_mode = Some(DockUndockMode {
                tiled_pane_selection: Some(TiledPaneSelection {
                    pane_id: selected_pane_id,
                    pending_undock_command_id: None,
                }),
            });
            return PlacementInputAction::Consumed;
        }
        let destination_tab_id = mouse_frame.client_snapshot.active_tab_id;
        if !self.open_placement_mode(
            selected_pane_id,
            None,
            destination_tab_id,
            PlacementModeLifetime::UntilCancelled,
        ) {
            return PlacementInputAction::Consumed;
        }
        if let Some(placement_mode) = self.placement_state.placement_mode.as_mut() {
            placement_mode.placement_target = Some(TiledPlacementTarget::Split {
                destination_tab_id,
                anchor: PanePlacementAnchor::Tab,
                direction: Direction::Right,
            });
        }
        PlacementInputAction::Consumed
    }

    /// Answer Enter in dock/undock mode: float the selected tiled pane where
    /// this viewer draws it ([`submit_undock_command`](Self::submit_undock_command)),
    /// or dock the selected floating pane at the selected target
    /// ([`submit_placement_command`](Self::submit_placement_command)).
    /// Returns `Consumed`, sending nothing, outside the mode, while the undock
    /// it sent waits for the session, and with nothing selected or no target
    /// selected.
    fn confirm_dock_undock(&mut self) -> PlacementInputAction {
        let Some(dock_undock_mode) = self.dock_undock_mode else {
            return PlacementInputAction::Consumed;
        };
        match dock_undock_mode.tiled_pane_selection {
            Some(TiledPaneSelection {
                pane_id,
                pending_undock_command_id: None,
            }) => self.submit_undock_command(pane_id),
            Some(_) => PlacementInputAction::Consumed,
            None => self
                .submit_placement_command()
                .unwrap_or(PlacementInputAction::Consumed),
        }
    }

    /// Select tiled pane `pane_id` in dock/undock mode, and send the
    /// `PlacePane` that floats it where this viewer draws it, under a new
    /// command id that the mode waits for.
    ///
    /// Enter with tiled pane `pane-7` selected sends `PlacePane {
    /// source_pane_id: Some(pane-7), placement_target: Floating { top_left_cell:
    /// None } }`.
    pub(crate) fn submit_undock_command(&mut self, pane_id: PaneId) -> PlacementInputAction {
        let command_id = CommandId::new();
        self.dock_undock_mode = Some(DockUndockMode {
            tiled_pane_selection: Some(TiledPaneSelection {
                pane_id,
                pending_undock_command_id: Some(command_id),
            }),
        });
        PlacementInputAction::SubmitCommand {
            command_id,
            command: Command::PlacePane(PlacePaneArgs {
                source_pane_id: Some(pane_id),
                placement_target: PanePlacementTarget::Floating {
                    top_left_cell: None,
                },
            }),
        }
    }

    /// Open the move chooser when `mouse_frame` draws an unpinned floating
    /// pane, and pane placement mode for the active tab's focused pane
    /// otherwise
    /// ([`begin_tiled_pane_placement`](Self::begin_tiled_pane_placement)).
    /// Opening the chooser ends floating pane move mode and drops an open key
    /// sequence. Returns `Consumed`, changing nothing, while
    /// [`is_move_or_dock_undock_entry_refused`](Self::is_move_or_dock_undock_entry_refused).
    ///
    /// `<C-p> m` with floating pane `pane-9` drawn unpinned opens the chooser.
    /// `<C-p> m` with no floating pane drawn previews the focused tiled pane.
    fn begin_pane_move(&mut self, mouse_frame: Option<&MouseFrame>) -> PlacementInputAction {
        if self.is_move_or_dock_undock_entry_refused() {
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

    /// Open pane placement mode for the active tab's focused pane, which
    /// previews the active tab ([`begin_placement_mode`](Self::begin_placement_mode)),
    /// and return `Consumed`.
    fn begin_tiled_pane_placement(&mut self) -> PlacementInputAction {
        self.begin_placement_mode();
        PlacementInputAction::Consumed
    }

    /// Open floating pane move mode for the floating pane that `mouse_frame`
    /// draws unpinned and focuses, or else for the front-most floating pane it
    /// draws unpinned, and return a `FocusPane` for that pane.
    ///
    /// Dock/undock mode and active pane placement mode end first. Opening the
    /// mode drops an open key sequence and every gesture a held button drives
    /// ([`end_held_mouse_gestures`](Self::end_held_mouse_gestures)). With no
    /// such floating pane, or no painted frame, the move chooser and floating
    /// pane move mode end. Returns `Consumed`, changing nothing, while a
    /// placement waits for the session, while floating pane move mode waits
    /// for the move it sent, and while dock/undock mode waits for its undock
    /// or dock.
    ///
    /// With floating panes `a`, `b` and `c` drawn back to front and a tiled
    /// pane focused, the mode selects `c` and focuses it.
    fn begin_floating_pane_move(
        &mut self,
        mouse_frame: Option<&MouseFrame>,
    ) -> PlacementInputAction {
        if self.is_placement_confirmation_pending()
            || self.is_floating_pane_move_pending()
            || self.is_dock_undock_command_pending()
        {
            return PlacementInputAction::Consumed;
        }
        self.close_dock_undock_mode();
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
            .floating_panes
            .iter()
            .map(FloatingPane::get_pane_id)
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
        self.select_placement_destination_tab(destination_tab_id);
        PlacementInputAction::Consumed
    }

    /// Select a placement target in `direction` and retain it for confirmation.
    ///
    /// - An Arrow selects a swap with the pane `direction` reaches.
    /// - Shift+Arrow selects insertion into the smallest span that holds that
    ///   pane: in `[A | B]` with `B` in a stack, Shift+Right from `A` selects
    ///   insertion beside the whole stack.
    /// - With the whole destination tab selected, Shift+Left selects `insert
    ///   left` of that tab, and an Arrow searches as if no target were selected.
    /// - With a floating source pane, an Arrow that reaches a pane selects the
    ///   insertion Shift+Arrow selects there: from the whole tab `[A | B]`,
    ///   Right selects `insert right` of `B`.
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
        let should_select_pane_insertion =
            is_insertion_target || placement_mode.source_tab_id.is_none();
        let Some(placement_snapshot) = self.get_placement_snapshot() else {
            return PlacementInputAction::Consumed;
        };
        let destination_tab_id = placement_mode.destination_tab_id;
        let source_pane_id = placement_mode.source_pane_id;
        let (Some(destination_tab_snapshot), Some(placement_destinations)) = (
            placement_snapshot.find_displayed_tab_snapshot(),
            placement_snapshot.list_placement_destinations(),
        ) else {
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
            let Some(target_pane_id) = find_directional_target_pane_id(
                &placement_destinations,
                &destination_tab_snapshot.tab_snapshot,
                source_pane_id,
                current_placement_target,
                direction,
            ) else {
                return PlacementInputAction::Consumed;
            };
            if target_pane_id == source_pane_id {
                None
            } else if should_select_pane_insertion {
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
        let Some(placement_snapshot) = self.get_placement_snapshot() else {
            return PlacementInputAction::Consumed;
        };
        let destination_tab_id = placement_mode.destination_tab_id;
        let placement_direction = placement_mode.placement_direction;
        let Some(placement_destinations) = placement_snapshot.list_placement_destinations() else {
            return PlacementInputAction::Consumed;
        };
        let mut insertion_spans = placement_destinations.insertion_spans;
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
            .map(resolve_placement_target_anchor)
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

    /// Switch the selected target between beside a pane, a whole stack or the
    /// whole tab, and the stack of what that span holds:
    ///
    /// - `Swap { p }` becomes `Stack { p }`.
    /// - A `Split` becomes `Stack { m }`, with `m` the pane its anchor names
    ///   in the held preview ([`find_stack_target_pane_id`]): pane `p` for a
    ///   span beside `p`, and the member a stack shows open for a span beside
    ///   that stack, or for the whole tab when one stack fills the tab.
    /// - `Stack { p }` becomes `Swap { p }` for a tiled source pane, and, for
    ///   a floating source pane, the insertion beside the smallest span that
    ///   holds `p`, on the edge of the last insertion.
    /// - A target that leaves the layout as it is clears the target.
    ///
    /// Returns `Consumed`, changing nothing, while a placement command is
    /// pending, with no preview, and with no target, a group that is not one
    /// stack, a whole tab that a directional split fills, or no span that
    /// holds `p`. In `[A | B]` with `A` the source, `s` on `Swap { B }`
    /// selects `Stack { B }`, and Enter gives the stack `[B, A]`. In
    /// `[P | stack(A, B open)]` with a floating source `F`, `s` on the
    /// insertion beside the stack selects `Stack { B }`, and Enter gives the
    /// stack `[A, B, F]`. In a tab that holds only `stack(A, B open)`, `s` on
    /// the whole-tab insertion selects `Stack { B }` the same way.
    fn toggle_placement_stack(&mut self) -> PlacementInputAction {
        if self.is_placement_confirmation_pending() {
            return PlacementInputAction::Consumed;
        }
        let Some(placement_mode) = self.placement_state.placement_mode.as_ref() else {
            return PlacementInputAction::Consumed;
        };
        let Some(placement_snapshot) = self.get_placement_snapshot() else {
            return PlacementInputAction::Consumed;
        };
        let placement_target = match placement_mode.placement_target.as_ref() {
            Some(TiledPlacementTarget::Swap { target_pane_id }) => TiledPlacementTarget::Stack {
                target_pane_id: *target_pane_id,
            },
            Some(TiledPlacementTarget::Split { anchor, .. }) => {
                let Some(target_pane_id) = placement_snapshot
                    .find_displayed_tab_snapshot()
                    .and_then(|displayed_tab_snapshot| {
                        find_stack_target_pane_id(
                            &displayed_tab_snapshot.layout_tree,
                            anchor,
                            &displayed_tab_snapshot.focus_history,
                        )
                    })
                else {
                    return PlacementInputAction::Consumed;
                };
                TiledPlacementTarget::Stack { target_pane_id }
            }
            Some(TiledPlacementTarget::Stack { target_pane_id })
                if placement_mode.source_tab_id.is_some() =>
            {
                TiledPlacementTarget::Swap {
                    target_pane_id: *target_pane_id,
                }
            }
            Some(TiledPlacementTarget::Stack { target_pane_id }) => {
                let destination_tab_id = placement_mode.destination_tab_id;
                let Some(placement_destinations) = placement_snapshot.list_placement_destinations()
                else {
                    return PlacementInputAction::Consumed;
                };
                let insertion_spans = placement_destinations.insertion_spans;
                let Some(insertion_span) =
                    find_smallest_insertion_span(&insertion_spans, *target_pane_id)
                else {
                    return PlacementInputAction::Consumed;
                };
                TiledPlacementTarget::Split {
                    destination_tab_id,
                    anchor: insertion_span.anchor.clone(),
                    direction: placement_mode.placement_direction,
                }
            }
            None => return PlacementInputAction::Consumed,
        };
        let is_placement_target_noop =
            is_same_tab_placement_noop(placement_snapshot, &placement_target);
        let Some(placement_mode) = self.placement_state.placement_mode.as_mut() else {
            return PlacementInputAction::Consumed;
        };
        placement_mode.placement_target = (!is_placement_target_noop).then_some(placement_target);
        PlacementInputAction::Consumed
    }

    /// Select insertion at the right edge of the whole destination tab when the
    /// held preview shows a tab other than the source pane's tab, no target is
    /// selected, and no drag is active. A floating source pane over this
    /// viewer's active tab gets no target there. Tab from `[A | B]` onto `[C]`
    /// selects `insert right` of that tab, and Enter gives `[C | A]`.
    pub(crate) fn select_whole_tab_insertion_target(&mut self) {
        if self.placement_state.placement_drag.is_some() || self.is_placement_confirmation_pending()
        {
            return;
        }
        let Some(placement_snapshot) = self.get_placement_snapshot() else {
            return;
        };
        let destination_tab_id = placement_snapshot.destination_tab_id;
        let is_source_drawn_on_destination = match placement_snapshot.find_source_tab_snapshot() {
            Some(source_tab_snapshot) => {
                source_tab_snapshot.tab_snapshot.tab_id == destination_tab_id
            }
            None => self.active_tab_id == Some(destination_tab_id),
        };
        let Some(placement_mode) = self.placement_state.placement_mode.as_mut() else {
            return;
        };
        if placement_mode.placement_target.is_some() || is_source_drawn_on_destination {
            return;
        }
        placement_mode.placement_direction = Direction::Right;
        placement_mode.placement_target = Some(TiledPlacementTarget::Split {
            destination_tab_id,
            anchor: PanePlacementAnchor::Tab,
            direction: Direction::Right,
        });
    }

    /// Confirm the selected target: record it as the pending placement under a
    /// new command id, and return the `ConfirmPanePlacement` to send, which
    /// carries the destination tab as the held preview draws it after the
    /// placement ([`PlacementSnapshot::build_placed_preview`]).
    ///
    /// - Returns `Consumed` while a placement is pending, and while this
    ///   viewer holds no preview of the target's tab yet.
    /// - Returns `Consumed` and clears the target when the target leaves the
    ///   layout as it is, such as a swap of `pane-123` with itself.
    /// - Returns `Consumed` and keeps the target when the session refuses it in
    ///   the held preview, such as a dock into a tab with no room for the pane.
    /// - Returns `None` with no placement mode or no target.
    pub(crate) fn submit_placement_command(&mut self) -> Option<PlacementInputAction> {
        let placement_mode = self.placement_state.placement_mode.as_ref()?;
        if placement_mode.pending_placement_command_id.is_some() {
            return Some(PlacementInputAction::Consumed);
        }
        let placement_target = placement_mode.placement_target.as_ref()?;
        let source_pane_id = placement_mode.source_pane_id;
        let Some(placement_snapshot) = self.get_placement_snapshot() else {
            return Some(PlacementInputAction::Consumed);
        };
        if is_same_tab_placement_noop(placement_snapshot, placement_target) {
            self.placement_state
                .placement_mode
                .as_mut()?
                .placement_target = None;
            return Some(PlacementInputAction::Consumed);
        }
        self.update_placement_target_outcome();
        let Some(PlacementTargetOutcome {
            placement_target,
            placed_preview: Ok(placed_preview),
            ..
        }) = self.get_placement_target_outcome()
        else {
            return Some(PlacementInputAction::Consumed);
        };
        let placement_target = placement_target.clone();
        let expected_placed_tab_layout = Box::new(placed_preview.placed_tab_layout.clone());
        let command_id = CommandId::new();
        self.placement_state
            .placement_mode
            .as_mut()?
            .pending_placement_command_id = Some(command_id);
        Some(PlacementInputAction::ConfirmPlacement {
            command_id,
            source_pane_id,
            placement_target,
            expected_placed_tab_layout,
        })
    }

    /// Decide what `chord` means in this viewer's current mode.
    ///
    /// `<C-l>` while locked yields `Fire(core:unlock)` whatever the keymap
    /// says; `<C-p>` in the default keymap opens the pane group and yields
    /// `Pending`; a plain `a` with nothing bound yields `PassThrough('a')`.
    /// Escape while a floating pane's grab handle is held drops that move and
    /// any open key sequence, and yields `Discard`.
    ///
    /// A sequence `chord` opens hides its hint until `current_time` plus the
    /// keymap's which-key delay, or not at all when the delay is zero. A
    /// sequence `chord` continues keeps the hint deadline of its first chord,
    /// or keeps showing its hint.
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

        // The hint deadline of the open sequence, or of a sequence this chord
        // opens.
        let continuation_hint_hidden_until = match open_key_sequence.as_ref() {
            Some(open_key_sequence) => open_key_sequence.continuation_hint_hidden_until,
            None => {
                let which_key_delay = self.keymap_catalog.get_which_key_delay();
                (!which_key_delay.is_zero()).then(|| current_time + which_key_delay)
            }
        };

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
                self.rearm_continuous_sequence_prefix(
                    &bound_action,
                    &key_sequence,
                    continuation_hint_hidden_until,
                );
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
                    continuation_hint_hidden_until,
                });
                KeyOutcome::Pending
            }
            (None, false) => match open_key_sequence {
                // Escape leaves an open sequence: the held chords are dropped
                // and the Escape itself is consumed rather than typed.
                Some(_) if chord == ESCAPE_KEY_CHORD => KeyOutcome::Pending,
                // A key that continues nothing is discarded, and the sequence
                // stands unchanged, both deadlines included.
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

    /// How long until an open sequence's earlier deadline: its ambiguity
    /// deadline or its hint deadline. A sequence with neither, a prefix-only
    /// sequence whose hint shows, returns `None`. While an input event this
    /// terminal's input thread read has not reached the loop
    /// (`InputReadClock::has_untaken_input`), the answer is `None` too: that
    /// event's arrival wakes the loop. Example: `<C-p>` pressed at `t` with a
    /// which-key delay of 300 ms returns `Some(300 ms)` at `t`, and `None` once
    /// [`expire_key_sequence`](Self::expire_key_sequence) ran at `t + 300 ms`.
    #[must_use]
    pub fn compute_next_key_wakeup(&self, current_time: Instant) -> Option<Duration> {
        if self.input_read_clock.has_untaken_input() {
            return None;
        }
        let pending_key_sequence = self.pending_key_sequence.as_ref()?;
        [
            pending_key_sequence.ambiguity_deadline,
            pending_key_sequence.continuation_hint_hidden_until,
        ]
        .into_iter()
        .flatten()
        .min()
        .map(|next_deadline| next_deadline.saturating_duration_since(current_time))
    }

    /// Run the open sequence's deadlines that `current_time` reached, in this
    /// order:
    ///
    /// 1. A passed ambiguity deadline fires the sequence's complete binding,
    ///    which the call returns. The sequence closes, unless the binding's
    ///    action is continuous and its prefix opens again.
    /// 2. A passed hint deadline of the sequence still open makes the hint bar
    ///    show it ([`is_continuation_hint_hidden`](Self::is_continuation_hint_hidden)
    ///    turns `false`).
    ///
    /// A sequence whose binding a keymap change retired while it waited
    /// resolves to nothing: the held chords are dropped, never typed at the
    /// pane, and the call returns `None`. Example: `<C-y>` binds `core:quit`
    /// and opens `<C-y> a`, the chord timeout is 500 ms and the which-key delay
    /// is 800 ms: at `t + 500 ms` the call returns `core:quit`, and the hint
    /// bar never shows `<C-y>`.
    pub fn expire_key_sequence(&mut self, current_time: Instant) -> Option<BoundAction> {
        let due_key_sequence = self.pending_key_sequence.take_if(|pending_key_sequence| {
            pending_key_sequence
                .ambiguity_deadline
                .is_some_and(|ambiguity_deadline| ambiguity_deadline <= current_time)
        });
        let fired_bound_action = due_key_sequence.and_then(|due_key_sequence| {
            let bound_action = self
                .keymap_catalog
                .match_sequence(self.get_active_input_mode(), &due_key_sequence.sequence)
                .exact_bound_action?;
            self.rearm_continuous_sequence_prefix(
                &bound_action,
                &due_key_sequence.sequence,
                due_key_sequence.continuation_hint_hidden_until,
            );
            Some(bound_action)
        });
        if let Some(pending_key_sequence) = self.pending_key_sequence.as_mut() {
            if pending_key_sequence
                .continuation_hint_hidden_until
                .is_some_and(|hidden_until| hidden_until <= current_time)
            {
                pending_key_sequence.continuation_hint_hidden_until = None;
            }
        }
        fired_bound_action
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
    /// it. It carries `continuation_hint_hidden_until`, the hint deadline of
    /// the sequence that fired, and no ambiguity deadline.
    fn rearm_continuous_sequence_prefix(
        &mut self,
        bound_action: &BoundAction,
        key_sequence: &KeySequence,
        continuation_hint_hidden_until: Option<Instant>,
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
            continuation_hint_hidden_until,
        });
    }
}

/// Return whether an interactive target preserves the source tab's layout. A
/// floating source pane, and a destination other than the source pane's tab,
/// never do.
pub(crate) fn is_same_tab_placement_noop(
    placement_snapshot: &PlacementSnapshot,
    placement_target: &TiledPlacementTarget,
) -> bool {
    let Some(source_tab_snapshot) =
        placement_snapshot
            .find_source_tab_snapshot()
            .filter(|source_tab_snapshot| {
                source_tab_snapshot.tab_snapshot.tab_id == placement_snapshot.destination_tab_id
            })
    else {
        return false;
    };
    if let TiledPlacementTarget::Split {
        destination_tab_id, ..
    } = placement_target
    {
        if *destination_tab_id != source_tab_snapshot.tab_snapshot.tab_id {
            return false;
        }
    }
    let layout_target = PlacementTarget::from_tiled_placement_target(placement_target);
    let tab_rect = Rect::from_size_at_origin(source_tab_snapshot.tab_snapshot.tab_size);
    is_layout_placement_noop(
        &source_tab_snapshot.layout_tree,
        placement_snapshot.source_pane_id,
        &layout_target,
        tab_rect,
        placement_snapshot.pane_sizing,
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
        | TiledPlacementTarget::Stack { target_pane_id }
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
            .find(|insertion_span| match &insertion_span.anchor {
                PanePlacementAnchor::Group(span_pane_ids) => span_pane_ids == pane_ids,
                PanePlacementAnchor::Pane(_) | PanePlacementAnchor::Tab => false,
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
    destination_tab_snapshot: &TabSnapshot,
    source_pane_id: PaneId,
    current_placement_target: Option<&TiledPlacementTarget>,
    direction: Direction,
) -> Option<PaneId> {
    let mut candidate_pane_rects: Vec<(PaneId, Rect)> = placement_destinations
        .target_pane_slots
        .iter()
        .map(|target_pane_slot| (target_pane_slot.pane_id, target_pane_slot.slot_rect))
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

/// Return the insertion anchor named by a placement target: the target pane
/// of a swap or a stack, and the anchor of a split.
fn resolve_placement_target_anchor(placement_target: &TiledPlacementTarget) -> PanePlacementAnchor {
    match placement_target {
        TiledPlacementTarget::Swap { target_pane_id }
        | TiledPlacementTarget::Stack { target_pane_id } => {
            PanePlacementAnchor::Pane(*target_pane_id)
        }
        TiledPlacementTarget::Split { anchor, .. } => anchor.clone(),
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
