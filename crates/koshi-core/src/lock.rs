//! Client lock mode: the modal input state of one attached client.
//!
//! [`LockMode`] is the interaction mode a single client is in — whether
//! keystrokes drive the focused pane, are held verbatim for the pane, or are
//! interpreted by one of Koshi's modal layers (pane placement, the pane move
//! chooser, floating pane move, the floating pane tray, the notification
//! center).
//! It is client-scoped: two clients attached to the same session hold
//! independent modes. The command layer's `SetLockMode` carries a `bool` and
//! moves a client only into or out of [`LockMode::Locked`].

use serde::{Deserialize, Serialize};

/// The modal input state of one client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LockMode {
    /// Default mode: keys the `normal` keymap binds fire; every other key goes
    /// to the focused pane.
    #[default]
    Normal,
    /// Keys the `locked` keymap binds fire; every other key passes verbatim to
    /// the focused pane.
    Locked,
    /// Pane placement mode, opened by `core:begin-pane-placement`.
    ///
    /// - The placement actions own the keyboard. The viewer overlays the mode
    ///   on Normal or Locked without changing the stored base mode.
    /// - Picking a target changes only this viewer's preview. A mouse pickup
    ///   also sends `FocusPane` for the picked pane.
    /// - Enter or a mouse drop sends
    ///   [`Command::PlacePane`](crate::command::Command::PlacePane), the only
    ///   command that changes the layout. Esc sends nothing.
    PanePlacement,
    /// The pane move chooser, opened by `core:begin-pane-placement` while the
    /// viewer draws at least one floating pane that it did not pin.
    ///
    /// - The viewer overlays the mode on Normal or Locked without changing the
    ///   stored base mode, as it does pane placement mode.
    /// - `core:begin-tiled-pane-placement` opens pane placement mode,
    ///   `core:begin-floating-pane-move` opens floating pane move mode, and
    ///   `core:confirm-pane-move-choice` opens the one for the kind of the
    ///   focused pane. `core:cancel-pane-move-choice` returns to the base mode.
    /// - Opening floating pane move mode sends `FocusPane` for the pane that
    ///   mode selects. Opening pane placement mode reads its preview. The
    ///   chooser itself sends no command.
    PaneMoveChoice,
    /// Floating pane move mode, opened by `core:begin-floating-pane-move`.
    ///
    /// - The viewer overlays the mode on Normal or Locked without changing the
    ///   stored base mode.
    /// - Moving and selecting a floating pane changes only this viewer's
    ///   drawing. Selecting a pane also sends `FocusPane` for it.
    /// - `core:confirm-floating-pane-move` or a mouse release sends
    ///   [`Command::MoveFloatingPane`](crate::command::Command::MoveFloatingPane),
    ///   the only command that moves the pane. Esc sends nothing.
    FloatingPaneMove,
    /// The floating pane tray, opened by `core:toggle-floating-pane-tray` or a
    /// click on the tabline's tray button `⧉`.
    ///
    /// - The viewer overlays the mode on Normal or Locked without changing the
    ///   stored base mode. Opening and closing the tray sends no command.
    /// - `core:activate-selected-tray-tile` sends
    ///   [`Command::TogglePaneMinimized`](crate::command::Command::TogglePaneMinimized)
    ///   for the selected tile's pane, and
    ///   `core:toggle-minimize-all-floating-panes` sends
    ///   [`Command::ToggleAllFloatingPanesMinimized`](crate::command::Command::ToggleAllFloatingPanesMinimized).
    ///   The tray stays open after each. On the `+ new` tile,
    ///   `core:activate-selected-tray-tile` ends the mode and sends the
    ///   [`Command::NewPane`](crate::command::Command::NewPane) that creates a
    ///   floating pane.
    /// - `core:close-floating-pane-tray` sends nothing and ends the mode.
    FloatingPaneTray,
    /// The notification center, opened by `core:toggle-notification-center`
    /// or a click on the tabline's notification button `●`, while it is
    /// open.
    ///
    /// - The viewer overlays the mode on Normal or Locked without changing the
    ///   stored base mode. The center's actions send no command.
    /// - `core:select-newer-notification` and
    ///   `core:select-older-notification` select the card above or below the
    ///   selected one. `core:dismiss-selected-notification` removes the
    ///   selected card's entry. `core:clear-notifications` removes every
    ///   entry from this viewer's center and closes it.
    /// - `core:close-notification-center` and
    ///   `core:toggle-notification-center` close the center and end the mode.
    /// - A press outside the center closes it and ends the mode.
    NotificationCenter,
}

impl LockMode {
    /// Whether input that binds nothing reaches the pane in this mode.
    /// `Normal` and `Locked` pass what they do not bind; the modal layers
    /// (`PanePlacement`, `PaneMoveChoice`, `FloatingPaneMove`,
    /// `FloatingPaneTray`, `NotificationCenter`) own the keyboard while they
    /// are held and discard it.
    #[must_use]
    pub fn should_pass_unbound_input_to_pane(self) -> bool {
        matches!(self, LockMode::Normal | LockMode::Locked)
    }

    /// Every built-in mode, in declaration order.
    pub const ALL: [LockMode; 7] = [
        LockMode::Normal,
        LockMode::Locked,
        LockMode::PanePlacement,
        LockMode::PaneMoveChoice,
        LockMode::FloatingPaneMove,
        LockMode::FloatingPaneTray,
        LockMode::NotificationCenter,
    ];

    /// The mode's canonical keymap name — the string the keybinding config
    /// groups a mode's bindings under (`modes.normal`, `modes.locked`, …).
    #[must_use]
    pub fn get_keymap_name(self) -> &'static str {
        match self {
            LockMode::Normal => "normal",
            LockMode::Locked => "locked",
            LockMode::PanePlacement => "pane-placement",
            LockMode::PaneMoveChoice => "pane-move-choice",
            LockMode::FloatingPaneMove => "floating-pane-move",
            LockMode::FloatingPaneTray => "floating-pane-tray",
            LockMode::NotificationCenter => "notification-center",
        }
    }
}

#[cfg(test)]
mod tests;
