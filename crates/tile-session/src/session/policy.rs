//! Tab-level empty-tab policy: what becomes of a tab once its last pane is gone.
//!
//! When the last pane in a tab is removed — whether closed on request or after
//! its shell exited — [`EmptyTabPolicy`] decides the tab's fate. The default is
//! [`EmptyTabPolicy::CloseTab`] (per `TILE_23`); when closing that tab leaves
//! the session with no tabs, the session-level last-tab policy quits the
//! program.

use serde::{Deserialize, Serialize};

/// What happens to a tab when its last pane is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum EmptyTabPolicy {
    /// Keep the now-empty tab as a dead-pane placeholder, visible and focusable.
    KeepDeadPlaceholder,
    /// Spawn a fresh shell in the tab in place of the gone pane.
    RespawnShell,
    /// Close the tab; if it was the last tab, the session quits.
    #[default]
    CloseTab,
}

#[cfg(test)]
mod tests;
