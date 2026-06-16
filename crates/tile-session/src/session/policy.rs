//! Close policies: what becomes of a tab once its last pane is gone, and of the
//! session once its last tab is gone.
//!
//! When the last pane in a tab is removed — whether closed on request or after
//! its shell exited — [`EmptyTabPolicy`] decides the tab's fate. The default is
//! [`EmptyTabPolicy::CloseTab`], so an emptied tab does not linger; when closing
//! that tab leaves the session with no tabs, [`LastTabPolicy`] decides the
//! program's fate, and its [`Quit`](LastTabPolicy::Quit) default ends the session.

use serde::{Deserialize, Serialize};

/// What happens to a tab when its last pane is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum EmptyTabPolicy {
    /// Spawn a fresh shell in the tab in place of the gone pane.
    RespawnShell,
    /// Close the tab; if it was the last tab, the session quits.
    #[default]
    CloseTab,
}

/// What happens to the session when its last tab closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LastTabPolicy {
    /// Quit the program — with no tabs left there is nothing to show.
    #[default]
    Quit,
}

#[cfg(test)]
mod tests;
