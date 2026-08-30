//! Close policies: what becomes of a tab once its last pane is gone, and of the
//! session once its last tab is gone.
//!
//! [`EmptyTabPolicy`] decides a tab's fate when its last pane is removed —
//! closed on request, or gone once its shell exited. Its default,
//! [`EmptyTabPolicy::CloseTab`], closes the tab, and closing the last tab ends
//! the session. [`LastTabPolicy`] holds one value, its default
//! [`Quit`](LastTabPolicy::Quit).

use serde::{Deserialize, Serialize};

/// What happens to a tab when its last pane is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum EmptyTabPolicy {
    /// Keep the tab with no panes in it. The removed pane is not replaced.
    RespawnShell,
    /// Close the tab immediately; if it was the only tab, the session ends.
    #[default]
    CloseTab,
}

/// What happens to the session when its last tab closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LastTabPolicy {
    /// Quit the program.
    #[default]
    Quit,
}

#[cfg(test)]
mod tests;
