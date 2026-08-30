//! Client lock mode: the modal input state of one attached client.
//!
//! [`LockMode`] is the interaction mode a single client is in — whether
//! keystrokes drive the focused pane, are held verbatim for the pane, or are
//! interpreted by one of Koshi's modal layers (resize, pane, tab, scroll).
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
    /// Resize mode: directional keys resize the focused pane instead of
    /// reaching it.
    Resize,
    /// Pane mode: keys manage panes — new, close, focus, and move — rather than
    /// reaching the focused pane.
    PaneMode,
    /// Tab mode: keys manage tabs — new, close, focus, and move.
    TabMode,
    /// Scroll mode: keys navigate the focused pane's scrollback.
    ScrollMode,
}

impl LockMode {
    /// Whether input that binds nothing reaches the pane in this mode.
    /// `Normal` and `Locked` pass what they do not bind; the modal layers
    /// (`Resize`, `PaneMode`, `TabMode`, `ScrollMode`) own the keyboard while
    /// they are held and discard it.
    #[must_use]
    pub fn passes_to_pane(self) -> bool {
        matches!(self, LockMode::Normal | LockMode::Locked)
    }

    /// Every built-in mode, in declaration order.
    pub const ALL: [LockMode; 6] = [
        LockMode::Normal,
        LockMode::Locked,
        LockMode::Resize,
        LockMode::PaneMode,
        LockMode::TabMode,
        LockMode::ScrollMode,
    ];

    /// The mode's canonical keymap name — the string the keybinding config
    /// groups a mode's bindings under (`modes.normal`, `modes.locked`, …).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            LockMode::Normal => "normal",
            LockMode::Locked => "locked",
            LockMode::Resize => "resize",
            LockMode::PaneMode => "pane",
            LockMode::TabMode => "tab",
            LockMode::ScrollMode => "scroll",
        }
    }
}

#[cfg(test)]
mod tests;
