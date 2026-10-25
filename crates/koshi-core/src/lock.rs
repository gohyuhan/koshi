//! Client lock mode: the modal input state of one attached client.
//!
//! [`LockMode`] is the interaction mode a single client is in — whether
//! keystrokes drive the focused pane, are held verbatim for the pane, or are
//! interpreted by one of Koshi's modal layers (pane placement, the pane move
//! chooser, floating pane move, dock/undock, the floating pane tray).
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
    /// Dock/undock mode, opened by `core:begin-dock-undock`.
    ///
    /// - The viewer overlays the mode on Normal or Locked without changing the
    ///   stored base mode.
    /// - Opening the mode selects the pane that takes the viewer's input. With
    ///   a tiled pane selected, `core:confirm-dock-undock` sends the
    ///   [`Command::PlacePane`](crate::command::Command::PlacePane) that floats
    ///   it where it is drawn, and the floated pane stays selected. With a
    ///   floating pane selected, the placement keys pick where it docks, and
    ///   `core:confirm-dock-undock` sends the `PlacePane` that docks it there.
    /// - After a dock nothing is selected and the mode stays open.
    ///   `core:cancel-dock-undock` sends nothing and ends the mode.
    DockUndock,
    /// The floating pane tray, opened by `core:toggle-floating-pane-tray` or a
    /// click on the tabline's tray button `⧉`.
    ///
    /// - The viewer overlays the mode on Normal or Locked without changing the
    ///   stored base mode. Opening and closing the tray sends no command.
    /// - `core:toggle-selected-tray-tile` sends
    ///   [`Command::SetPaneMinimized`](crate::command::Command::SetPaneMinimized)
    ///   for the selected tile's pane, and
    ///   `core:toggle-minimize-all-floating-panes` sends
    ///   [`Command::SetAllFloatingPanesMinimized`](crate::command::Command::SetAllFloatingPanesMinimized).
    ///   The tray stays open after each.
    /// - `core:close-floating-pane-tray` sends nothing and ends the mode.
    FloatingPaneTray,
}

impl LockMode {
    /// Whether input that binds nothing reaches the pane in this mode.
    /// `Normal` and `Locked` pass what they do not bind; the modal layers
    /// (`PanePlacement`, `PaneMoveChoice`, `FloatingPaneMove`, `DockUndock`,
    /// `FloatingPaneTray`) own the keyboard while they are held and discard
    /// it.
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
        LockMode::DockUndock,
        LockMode::FloatingPaneTray,
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
            LockMode::DockUndock => "dock-undock",
            LockMode::FloatingPaneTray => "floating-pane-tray",
        }
    }
}

#[cfg(test)]
mod tests;
