//! Client lock mode: the modal input state of one attached client.
//!
//! [`LockMode`] is the interaction mode a single client is in — whether
//! keystrokes drive the focused pane, are held verbatim for the pane, or are
//! interpreted by one of Koshi's modal layers (resize, pane, tab, scroll).
//! It is client-scoped: two clients attached to the same session hold
//! independent modes. This is the richer modal state the client tracks, as
//! distinct from the command layer's binary lock toggle (`SetLockMode`), which
//! flips into and out of [`LockMode::Locked`].

use serde::{Deserialize, Serialize};

/// The modal input state of one client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LockMode {
    /// Default mode: keystrokes go to the focused pane; Koshi keybindings reach
    /// the client through its leader.
    #[default]
    Normal,
    /// Koshi keybindings are suppressed and input is passed verbatim to the
    /// focused pane, so an application can claim keys Koshi would otherwise own.
    Locked,
    /// Resize mode: directional keys resize the focused pane instead of
    /// reaching it.
    Resize,
    /// Pane mode: keys manage panes — new, close, focus, and move — rather than
    /// reaching the focused pane.
    PaneMode,
    /// Tab mode: keys manage tabs — new, close, rename, focus, and move.
    TabMode,
    /// Scroll mode: keys navigate the focused pane's scrollback.
    ScrollMode,
}

impl LockMode {
    /// Every built-in mode, in declaration order. The startup mode
    /// registration and the keymap layers iterate this so the built-in set
    /// is defined once.
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
