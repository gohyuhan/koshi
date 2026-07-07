//! Command-line grammar for the `koshi` binary: the root parser, its
//! attach/detach flags, and the subcommand tree.
//!
//! A bare `koshi` launches the interactive app: it spawns a new session and
//! attaches this terminal to it. `--attach` and `--detach` are root flags
//! rather than subcommands: each is a sub-action of that client spawn,
//! redirecting it at an existing session (attach) or reversing it (detach).
//! Every other verb is a subcommand. Parsing yields typed values only; no
//! command here talks to a runtime.
//!
//! Action subcommands carry typed arguments and map to the core command
//! vocabulary through [`CliCommand::to_action`](crate::cli::CliCommand::to_action),
//! which pairs each with its `core:` action reference. Entity ids are parsed
//! at this boundary: a flag accepts the id exactly as koshi prints it
//! (`pane-<uuid>`) or as a bare UUID.

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use koshi_core::action::ActionRef;
use koshi_core::command::{
    ClosePaneArgs, CloseTabArgs, Command, FocusPaneArgs, FocusTabArgs, LockModeArgs, MoveTabArgs,
    NewPaneArgs, NewTabArgs, RenamePaneArgs, RenameSessionArgs, RenameTabArgs, ResizePaneArgs,
    RunCommandPaneArgs, TabTarget,
};
use koshi_core::geometry::Direction;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::process::{ShellKind, SpawnSpec};
use uuid::Uuid;

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

/// A split or resize direction as typed on the command line. A separate type
/// from the core [`Direction`] so `koshi-core` stays free of clap derives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DirectionArg {
    /// Rightward.
    Right,
    /// Downward.
    Down,
    /// Leftward.
    Left,
    /// Upward.
    Up,
}

impl From<DirectionArg> for Direction {
    fn from(value: DirectionArg) -> Direction {
        match value {
            DirectionArg::Right => Direction::Right,
            DirectionArg::Down => Direction::Down,
            DirectionArg::Left => Direction::Left,
            DirectionArg::Up => Direction::Up,
        }
    }
}

/// The `koshi` subcommand tree.
///
/// Lifecycle commands (`new`, `list-sessions`, `kill-session`, `doctor`) run
/// outside any session. Action subcommands carry their typed arguments and
/// map to core commands via [`CliCommand::to_action`]; execution arrives
/// with the IPC client. The remaining verbs (`action`, `config`, `plugin`,
/// `actions`, `inspect`, the `list-*` queries, `keys`) are declared bare so
/// the full grammar is visible in `--help`; each gains its argument surface
/// with the work that implements it.
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
    /// Open a new pane running a shell; its working directory and
    /// environment come from the issuing terminal.
    NewPane {
        /// Split direction; omitted uses the default split.
        #[arg(long, value_enum, value_name = "DIRECTION", conflicts_with = "stacked")]
        direction: Option<DirectionArg>,
        /// Stack the new pane onto the source pane instead of splitting.
        #[arg(long)]
        stacked: bool,
        /// Pane to split from; defaults to the focused pane.
        #[arg(long, value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane: Option<PaneId>,
    },
    /// Close a pane.
    ClosePane {
        /// Pane to close; defaults to the focused pane.
        #[arg(long, value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane: Option<PaneId>,
        /// Kill the pane's child immediately, overriding its close policy.
        #[arg(long)]
        force: bool,
    },
    /// Move one of a pane's borders: a positive size grows the pane toward
    /// the direction, a negative size shrinks it.
    ResizePane {
        /// Which of the pane's borders moves.
        #[arg(long, value_enum, value_name = "DIRECTION")]
        direction: DirectionArg,
        /// Signed number of cells the border moves; defaults to 1.
        #[arg(
            long,
            value_name = "SIZE",
            default_value_t = 1,
            allow_negative_numbers = true
        )]
        size: i16,
        /// Pane to resize; defaults to the focused pane.
        #[arg(long, value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane: Option<PaneId>,
    },
    /// Toggle fullscreen on the focused pane.
    TogglePaneFullscreen,
    /// Re-roll a pane's generated name.
    RenamePane {
        /// Pane to rename; defaults to the focused pane.
        #[arg(long, value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane: Option<PaneId>,
    },
    /// Open a new tab; its first pane inherits the issuing terminal's
    /// working directory and environment.
    NewTab,
    /// Close a tab.
    CloseTab {
        /// Tab to close; defaults to the focused tab.
        #[arg(long, value_parser = parse_tab_id, value_name = "TAB_ID")]
        tab: Option<TabId>,
        /// Kill every pane's child immediately, overriding each close policy.
        #[arg(long)]
        force: bool,
    },
    /// Focus the next tab.
    NextTab,
    /// Focus the previous tab.
    PreviousTab,
    /// Re-roll a tab's generated name.
    RenameTab {
        /// Tab to rename; defaults to the focused tab.
        #[arg(long, value_parser = parse_tab_id, value_name = "TAB_ID")]
        tab: Option<TabId>,
    },
    /// Move a tab to a new index.
    MoveTab {
        /// Destination zero-based index.
        #[arg(long, value_name = "INDEX")]
        index: usize,
        /// Tab to move; defaults to the focused tab.
        #[arg(long, value_parser = parse_tab_id, value_name = "TAB_ID")]
        tab: Option<TabId>,
    },
    /// Focus a tab by index or id.
    FocusTab {
        /// Zero-based index of the tab to focus.
        #[arg(
            long,
            value_name = "INDEX",
            conflicts_with = "tab",
            required_unless_present = "tab"
        )]
        index: Option<usize>,
        /// Id of the tab to focus.
        #[arg(long, value_parser = parse_tab_id, value_name = "TAB_ID")]
        tab: Option<TabId>,
    },
    /// Focus a pane by id.
    FocusPane {
        /// Pane to focus.
        #[arg(long, value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane: PaneId,
        /// Client whose focus moves; defaults to the issuing client.
        #[arg(long, value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client: Option<ClientId>,
    },
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
    RenameSession {
        /// Session to rename; defaults to the current session.
        #[arg(long, value_parser = parse_session_id, value_name = "SESSION_ID")]
        session: Option<SessionId>,
    },
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
    /// Open a new pane running the command given after `--`; its working
    /// directory and environment come from the issuing terminal.
    Run {
        /// Split direction; omitted uses the default split.
        #[arg(long, value_enum, value_name = "DIRECTION", conflicts_with = "stacked")]
        direction: Option<DirectionArg>,
        /// Stack the new pane onto the source pane instead of splitting.
        #[arg(long)]
        stacked: bool,
        /// The command and its arguments, given after `--`.
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Inspect and manage keybindings.
    Keys,
}

impl CliCommand {
    /// The typed action this subcommand requests: its `core:` action
    /// reference paired with the fully-built core [`Command`].
    ///
    /// `None` for the verbs that are not actions — the lifecycle commands
    /// (`new`, `list-sessions`, `kill-session`, `doctor`) and the verbs whose
    /// argument surfaces are not built yet (`action`, `config`, `plugin`,
    /// `actions`, `inspect`, the `list-*` queries, `keys`).
    #[must_use]
    pub fn to_action(&self) -> Option<(ActionRef, Command)> {
        let (name, command) = match self {
            CliCommand::NewPane {
                direction,
                stacked,
                pane,
            } => (
                "new-pane",
                Command::NewPane(NewPaneArgs {
                    source: *pane,
                    direction: direction.map(Direction::from),
                    stacked: *stacked,
                    cwd: None,
                    command: None,
                    client: None,
                }),
            ),
            CliCommand::ClosePane { pane, force } => (
                "close-pane",
                Command::ClosePane(ClosePaneArgs {
                    pane: *pane,
                    force: *force,
                }),
            ),
            CliCommand::ResizePane {
                direction,
                size,
                pane,
            } => (
                "resize-pane",
                Command::ResizePane(ResizePaneArgs {
                    pane: *pane,
                    direction: Direction::from(*direction),
                    size: *size,
                }),
            ),
            CliCommand::TogglePaneFullscreen => {
                ("toggle-pane-fullscreen", Command::TogglePaneFullscreen)
            }
            CliCommand::RenamePane { pane } => (
                "rename-pane",
                Command::RenamePane(RenamePaneArgs { pane: *pane }),
            ),
            CliCommand::NewTab => ("new-tab", Command::NewTab(NewTabArgs::default())),
            CliCommand::CloseTab { tab, force } => (
                "close-tab",
                Command::CloseTab(CloseTabArgs {
                    tab: *tab,
                    force: *force,
                }),
            ),
            CliCommand::NextTab => (
                "next-tab",
                Command::FocusTab(FocusTabArgs {
                    target: TabTarget::Next,
                    client: None,
                }),
            ),
            CliCommand::PreviousTab => (
                "previous-tab",
                Command::FocusTab(FocusTabArgs {
                    target: TabTarget::Prev,
                    client: None,
                }),
            ),
            CliCommand::RenameTab { tab } => (
                "rename-tab",
                Command::RenameTab(RenameTabArgs { tab: *tab }),
            ),
            CliCommand::MoveTab { index, tab } => (
                "move-tab",
                Command::MoveTab(MoveTabArgs {
                    tab: *tab,
                    index: *index,
                }),
            ),
            CliCommand::FocusTab { index, tab } => {
                // The parser enforces exactly one of the two flags.
                let target = match (index, tab) {
                    (Some(index), None) => TabTarget::Index(*index),
                    (None, Some(tab)) => TabTarget::Id(*tab),
                    _ => unreachable!("clap enforces exactly one of --index/--tab"),
                };
                (
                    "focus-tab",
                    Command::FocusTab(FocusTabArgs {
                        target,
                        client: None,
                    }),
                )
            }
            CliCommand::FocusPane { pane, client } => (
                "focus-pane",
                Command::FocusPane(FocusPaneArgs {
                    pane: *pane,
                    client: *client,
                }),
            ),
            CliCommand::Lock => ("lock", Command::SetLockMode(LockModeArgs { locked: true })),
            CliCommand::Unlock => (
                "unlock",
                Command::SetLockMode(LockModeArgs { locked: false }),
            ),
            CliCommand::ToggleLock => ("toggle-lock", Command::ToggleLockMode),
            CliCommand::RenameSession { session } => (
                "rename-session",
                Command::RenameSession(RenameSessionArgs { session: *session }),
            ),
            CliCommand::Run {
                direction,
                stacked,
                command,
            } => (
                "run",
                Command::RunCommandPane(RunCommandPaneArgs {
                    command: spawn_spec_from_argv(command),
                    cwd: None,
                    direction: direction.map(Direction::from),
                    stacked: *stacked,
                }),
            ),
            CliCommand::New
            | CliCommand::ListSessions
            | CliCommand::KillSession { .. }
            | CliCommand::Doctor
            | CliCommand::Action
            | CliCommand::Config
            | CliCommand::Plugin
            | CliCommand::Actions
            | CliCommand::Inspect
            | CliCommand::ListTabs
            | CliCommand::ListPanes
            | CliCommand::ListClients
            | CliCommand::Keys => return None,
        };
        let action = ActionRef::core(name)
            .expect("CLI action names are constants satisfying the action-name grammar");
        Some((action, command))
    }
}

/// Build the [`SpawnSpec`] for a `run` invocation's trailing argv: the first
/// token is the program, the rest its arguments. The working directory and
/// environment stay empty — they are filled from the issuing terminal when
/// the command is sent.
fn spawn_spec_from_argv(argv: &[String]) -> SpawnSpec {
    let program = PathBuf::from(&argv[0]);
    let shell_kind = ShellKind::from_program(&program);
    SpawnSpec {
        program,
        args: argv[1..].to_vec(),
        cwd: None,
        env: BTreeMap::new(),
        shell_kind,
    }
}

/// Parse an entity id as koshi prints it (`<prefix>-<uuid>`) or as a bare
/// UUID. A mismatched prefix does not strip, so an id of the wrong kind is
/// rejected rather than silently accepted.
fn parse_prefixed_uuid(value: &str, prefix: &str) -> Result<Uuid, String> {
    let bare = value
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('-'))
        .unwrap_or(value);
    Uuid::parse_str(bare).map_err(|_| format!("expected `{prefix}-<uuid>` or a bare UUID"))
}

/// Parse a `--pane` flag value into a [`PaneId`].
fn parse_pane_id(value: &str) -> Result<PaneId, String> {
    parse_prefixed_uuid(value, "pane").map(PaneId::from_uuid)
}

/// Parse a `--tab` flag value into a [`TabId`].
fn parse_tab_id(value: &str) -> Result<TabId, String> {
    parse_prefixed_uuid(value, "tab").map(TabId::from_uuid)
}

/// Parse a `--session` flag value into a [`SessionId`].
fn parse_session_id(value: &str) -> Result<SessionId, String> {
    parse_prefixed_uuid(value, "session").map(SessionId::from_uuid)
}

/// Parse a `--client` flag value into a [`ClientId`].
fn parse_client_id(value: &str) -> Result<ClientId, String> {
    parse_prefixed_uuid(value, "client").map(ClientId::from_uuid)
}

#[cfg(test)]
mod tests;
