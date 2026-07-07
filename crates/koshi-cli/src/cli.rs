//! Command-line grammar for the `koshi` binary: the root parser, its
//! attach/detach flags, and the subcommand tree.
//!
//! A bare `koshi` launches the interactive app: it spawns a new session and
//! attaches this terminal to it. `--attach` and `--detach` are root flags
//! rather than subcommands: each is a sub-action of that client spawn,
//! redirecting it at an existing session (attach) or reversing it (detach).
//! Every other verb is a subcommand. Parsing yields typed values only; no
//! command here talks to a runtime.

use clap::{Parser, Subcommand};

/// A parsed `koshi` invocation.
#[derive(Debug, PartialEq, Eq, Parser)]
#[command(
    name = "koshi",
    version,
    about = "A tiling terminal multiplexer",
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// Attach this client to the session with the given id instead of
    /// creating a new session.
    #[arg(long, value_name = "SESSION_ID", conflicts_with = "detach")]
    pub attach: Option<String>,

    /// Detach from a session: with an id, every client attached to that
    /// session detaches; without one, the client's current session detaches.
    #[arg(long, value_name = "SESSION_ID", num_args = 0..=1)]
    pub detach: Option<Option<String>>,

    /// The verb to run; absent on the bare interactive launch.
    #[command(subcommand)]
    pub command: Option<CliCommand>,
}

impl Cli {
    /// True for the bare `koshi` invocation — no subcommand and no
    /// attach/detach flag — which launches the interactive app.
    #[must_use]
    pub fn is_interactive_launch(&self) -> bool {
        self.attach.is_none() && self.detach.is_none() && self.command.is_none()
    }
}

/// The `koshi` subcommand tree.
///
/// Lifecycle commands (`new`, `list-sessions`, `kill-session`, `doctor`) run
/// outside any session. The in-session control commands are declared here so
/// the full grammar is visible in `--help`; their argument surfaces arrive
/// with the action-argument work, and execution arrives with the IPC client.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum CliCommand {
    /// Create a new session (its name is system-generated).
    New,
    /// List running sessions.
    ListSessions,
    /// Kill a session; without a name, targets the only running session.
    KillSession {
        /// Session to kill.
        session: Option<String>,
    },
    /// Check the local koshi installation and environment.
    Doctor,
    /// Run a named action (explicit, script-friendly namespace).
    Action,
    /// Open a new pane.
    NewPane,
    /// Close the focused pane.
    ClosePane,
    /// Resize the focused pane.
    ResizePane,
    /// Toggle fullscreen on the focused pane.
    TogglePaneFullscreen,
    /// Re-roll the focused pane's generated name.
    RenamePane,
    /// Open a new tab.
    NewTab,
    /// Close the focused tab.
    CloseTab,
    /// Focus the next tab.
    NextTab,
    /// Focus the previous tab.
    PreviousTab,
    /// Re-roll the focused tab's generated name.
    RenameTab,
    /// Move the focused tab to a new index.
    MoveTab,
    /// Focus a tab by index or id.
    FocusTab,
    /// Focus a pane by direction or id.
    FocusPane,
    /// Enter locked input mode.
    Lock,
    /// Leave locked input mode.
    Unlock,
    /// Toggle locked input mode.
    ToggleLock,
    /// Inspect and validate configuration.
    Config,
    /// Manage plugins.
    Plugin,
    /// Re-roll a session's generated name.
    RenameSession,
    /// Introspect the action registry.
    Actions,
    /// Inspect a session, tab, pane, or client.
    Inspect,
    /// List tabs in a session.
    ListTabs,
    /// List panes in a session.
    ListPanes,
    /// List clients attached to a session.
    ListClients,
    /// Open a new pane running a command.
    Run,
    /// Inspect and manage keybindings.
    Keys,
}

#[cfg(test)]
mod tests;
