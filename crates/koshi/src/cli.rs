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
    ClosePaneArgs, CloseTabArgs, Command, FocusPaneArgs, FocusTabArgs, FocusTarget, LockModeArgs,
    MoveTabArgs, NewPaneArgs, NewTabArgs, RenamePaneArgs, RenameSessionArgs, RenameTabArgs,
    ResizePaneArgs, RunCommandPaneArgs, TabTarget, ToggleLockModeArgs, WriteToPaneArgs,
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

    /// Launch with a named profile: read `profile/<name>.kdl` from the config
    /// directory and open its tabs and panes instead of a single shell.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["attach", "detach"])]
    pub profile: Option<String>,

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

/// A session named on the command line: a `session-<uuid>` id (or bare
/// UUID), or a display name to look up against the running sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRef {
    /// An exact session id.
    Id(SessionId),
    /// A display name; it must match exactly one running session.
    Name(String),
}

/// A tab named on the command line: a `tab-<uuid>` id (or bare UUID), or a
/// display name to look up against the target session's tabs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabRef {
    /// An exact tab id.
    Id(TabId),
    /// A display name; it must match exactly one tab.
    Name(String),
}

/// Parse a `--session` flag value: an id when the value reads as one, else a
/// display name.
fn parse_session_ref(value: &str) -> Result<SessionRef, String> {
    if value.is_empty() {
        return Err("expected a session id or name".to_string());
    }
    Ok(match parse_prefixed_uuid(value, "session") {
        Ok(uuid) => SessionRef::Id(SessionId::from_uuid(uuid)),
        Err(_) => SessionRef::Name(value.to_string()),
    })
}

/// Parse a `--tab` flag value: an id when the value reads as one, else a
/// display name.
fn parse_tab_ref(value: &str) -> Result<TabRef, String> {
    if value.is_empty() {
        return Err("expected a tab id or name".to_string());
    }
    Ok(match parse_prefixed_uuid(value, "tab") {
        Ok(uuid) => TabRef::Id(TabId::from_uuid(uuid)),
        Err(_) => TabRef::Name(value.to_string()),
    })
}

/// The `--session`/`--tab` flags of one invocation, resolved to concrete ids
/// (a name looked up against the running sessions). The routing layer builds
/// this before [`CliCommand::to_action`]; a verb without those flags takes
/// `default()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResolvedTargets {
    /// The resolved `--session` value, for the verbs whose command carries it.
    pub session: Option<SessionId>,
    /// The resolved `--tab` value.
    pub tab: Option<TabId>,
}

/// The output format of a discovery query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FormatArg {
    /// Human-readable aligned columns.
    Table,
    /// Machine-readable JSON.
    Json,
}

/// The `koshi` subcommand tree.
///
/// Lifecycle commands (`new`, `list-sessions`, `kill-session`, `doctor`) run
/// outside any session. Action subcommands carry their typed arguments and
/// map to core commands via [`CliCommand::to_action`]; execution arrives
/// with the IPC client. The discovery queries (`inspect`, the `list-*`
/// verbs) carry typed target and `--format` arguments; their answers are
/// rendered by [`crate::output`]. `actions` introspects the action registry
/// through its `list`/`explain` subcommands, and `keys` introspects the
/// keymap through its own subcommand tree. The remaining verbs (`config`,
/// `plugin`) are declared bare so the full grammar is visible in `--help`;
/// each gains its argument surface with the work that implements it.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum CliCommand {
    /// Create a new session (its name is system-generated).
    New,
    /// List running sessions.
    ListSessions {
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// Kill a session; without a name, targets the only running session.
    KillSession {
        /// Session to kill.
        session: Option<String>,
    },
    /// Check the local koshi installation and environment.
    Doctor,
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
        /// Session receiving the pane, by id or name; defaults to the current
        /// session, else the only running one.
        #[arg(long, value_parser = parse_session_ref, value_name = "SESSION")]
        session: Option<SessionRef>,
        /// Tab receiving the pane, by id or name; the split anchors on that
        /// tab's most recently focused pane. Defaults to the source pane's tab.
        #[arg(long, value_parser = parse_tab_ref, value_name = "TAB", conflicts_with = "pane")]
        tab: Option<TabRef>,
        /// Client that shows and focuses the new pane; defaults to the
        /// issuing client, else the session's only attached one.
        #[arg(long, value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client: Option<ClientId>,
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
    /// Type text into a pane's shell, as if it had been typed there. The text
    /// is followed by Enter, so the shell runs it; `--no-enter` leaves it
    /// waiting at the prompt.
    Input {
        /// Text to type into the pane. Text starting with `-` is taken as text,
        /// not as a flag, so a scripted line is passed through whatever it says.
        #[arg(value_name = "TEXT", allow_hyphen_values = true)]
        text: String,
        /// Pane to type into; defaults to the focused pane.
        #[arg(long, value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane: Option<PaneId>,
        /// Leave the text at the prompt instead of pressing Enter after it.
        #[arg(long)]
        no_enter: bool,
    },
    /// Open a new tab; its first pane inherits the issuing terminal's
    /// working directory and environment.
    NewTab {
        /// Session the tab joins, by id or name; defaults to the current
        /// session, else the only running one.
        #[arg(long, value_parser = parse_session_ref, value_name = "SESSION")]
        session: Option<SessionRef>,
    },
    /// Close a tab.
    CloseTab {
        /// Tab to close, by id or name; defaults to the focused tab.
        #[arg(long, value_parser = parse_tab_ref, value_name = "TAB")]
        tab: Option<TabRef>,
        /// Session owning the tab, by id or name; defaults to the current
        /// session, else the only running one.
        #[arg(long, value_parser = parse_session_ref, value_name = "SESSION")]
        session: Option<SessionRef>,
        /// Kill every pane's child immediately, overriding each close policy.
        #[arg(long)]
        force: bool,
    },
    /// Focus the next tab.
    NextTab {
        /// Client whose view switches; defaults to the issuing client.
        #[arg(long, value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client: Option<ClientId>,
    },
    /// Focus the previous tab.
    PreviousTab {
        /// Client whose view switches; defaults to the issuing client.
        #[arg(long, value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client: Option<ClientId>,
    },
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
        /// Client whose view switches; defaults to the issuing client.
        #[arg(long, value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client: Option<ClientId>,
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
    Lock {
        /// Client to lock; defaults to the issuing client, else the
        /// session's only attached one.
        #[arg(long, value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client: Option<ClientId>,
    },
    /// Leave locked input mode.
    Unlock {
        /// Client to unlock; defaults to the issuing client, else the
        /// session's only attached one.
        #[arg(long, value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client: Option<ClientId>,
    },
    /// Toggle locked input mode.
    ToggleLock {
        /// Client whose lock flips; defaults to the issuing client, else the
        /// session's only attached one.
        #[arg(long, value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client: Option<ClientId>,
    },
    /// Inspect and validate configuration.
    Config,
    /// Manage plugins.
    Plugin,
    /// Download and install the latest koshi release.
    Update,
    /// Re-roll a session's generated name.
    RenameSession {
        /// Session to rename, by id or name; defaults to the current session.
        #[arg(long, value_parser = parse_session_ref, value_name = "SESSION")]
        session: Option<SessionRef>,
    },
    /// Introspect the action registry.
    Actions {
        /// What to introspect.
        #[command(subcommand)]
        command: ActionsCommand,
    },
    /// Inspect a session, tab, pane, or client.
    Inspect {
        /// What to inspect.
        #[command(subcommand)]
        target: InspectTarget,
    },
    /// List tabs in a session.
    ListTabs {
        /// Session to list; defaults to the only running session.
        #[arg(long, value_parser = parse_session_id, value_name = "SESSION_ID")]
        session: Option<SessionId>,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// List panes in a session.
    ListPanes {
        /// Session to list; defaults to the only running session.
        #[arg(long, value_parser = parse_session_id, value_name = "SESSION_ID")]
        session: Option<SessionId>,
        /// Limit the listing to one tab.
        #[arg(long, value_parser = parse_tab_id, value_name = "TAB_ID")]
        tab: Option<TabId>,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// List clients attached to a session.
    ListClients {
        /// Session to list; defaults to the only running session.
        #[arg(long, value_parser = parse_session_id, value_name = "SESSION_ID")]
        session: Option<SessionId>,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// Open a new pane running the command given after `--`; its working
    /// directory and environment come from the issuing terminal.
    Run {
        /// Split direction; omitted uses the default split.
        #[arg(long, value_enum, value_name = "DIRECTION", conflicts_with = "stacked")]
        direction: Option<DirectionArg>,
        /// Stack the new pane onto the source pane instead of splitting.
        #[arg(long)]
        stacked: bool,
        /// Pane to split from; defaults to the focused pane.
        #[arg(long, value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane: Option<PaneId>,
        /// Session receiving the pane, by id or name; defaults to the current
        /// session, else the only running one.
        #[arg(long, value_parser = parse_session_ref, value_name = "SESSION")]
        session: Option<SessionRef>,
        /// Tab receiving the pane, by id or name; the split anchors on that
        /// tab's most recently focused pane. Defaults to the source pane's tab.
        #[arg(long, value_parser = parse_tab_ref, value_name = "TAB", conflicts_with = "pane")]
        tab: Option<TabRef>,
        /// Client that shows and focuses the new pane; defaults to the
        /// issuing client, else the session's only attached one.
        #[arg(long, value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client: Option<ClientId>,
        /// The command and its arguments, given after `--`.
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Inspect keybindings.
    Keys {
        /// What to do.
        #[command(subcommand)]
        command: KeysCommand,
    },
}

/// Which keymap layer authored a binding, as typed on the command line. A
/// separate type from the config crate's `LayerOrigin` so `koshi-config`
/// stays free of clap derives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScopeArg {
    /// The built-in default binding table.
    Default,
    /// The user's keybinding file.
    User,
    /// Per-named-session overrides.
    Session,
    /// Bindings a layout file declares.
    Layout,
}

/// The `koshi keys` subcommands: read-only keymap introspection. Every verb
/// renders locally from the built-in defaults plus the user's keybinding
/// file; the file is the single mutation surface — koshi has no runtime
/// keybinding edits.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum KeysCommand {
    /// List effective keybindings per mode.
    List {
        /// Limit the listing to one input mode.
        #[arg(long, value_name = "MODE")]
        mode: Option<String>,
        /// Limit the listing to bindings authored by one layer.
        #[arg(long, value_enum, value_name = "SCOPE")]
        scope: Option<ScopeArg>,
        /// List plugin-recommended bindings instead of effective ones.
        #[arg(long)]
        recommended: bool,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// Describe a key sequence: its action, source layer, and metadata.
    Describe {
        /// The key sequence, in the angle grammar (`"<C-p> n"`).
        #[arg(value_name = "KEY_SEQUENCE")]
        sequence: String,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// Report keybinding conflicts, dead bindings, and warnings.
    Conflicts {
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// Dry-run a keybinding file: parse and conflict-check it without
    /// applying anything.
    Validate {
        /// Path of the keybinding KDL file to check.
        #[arg(value_name = "PATH")]
        path: PathBuf,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
}

/// The entity kinds `koshi inspect` reports on. Each takes the id exactly as
/// koshi prints it (`<kind>-<uuid>`) or as a bare UUID.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum InspectTarget {
    /// Report a session: name, creation time, clients, and pane count.
    Session {
        /// Session to inspect.
        #[arg(value_parser = parse_session_id, value_name = "SESSION_ID")]
        session: SessionId,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// Report a tab: name, position, active pane, and pane count.
    Tab {
        /// Tab to inspect.
        #[arg(value_parser = parse_tab_id, value_name = "TAB_ID")]
        tab: TabId,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// Report a pane: location, title, cwd, command, state, and rectangle.
    Pane {
        /// Pane to inspect.
        #[arg(value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane: PaneId,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// Report a client: session, attach time, viewport, focus, and lock state.
    Client {
        /// Client to inspect.
        #[arg(value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client: ClientId,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
}

/// The `koshi actions` introspection subcommands: list the supported actions or
/// explain one. Both read the static action table and render through
/// [`crate::output`]; neither needs a running session.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum ActionsCommand {
    /// List every supported action with its internal command and scope.
    List {
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// Explain one action: its scope, target compatibility, internal command,
    /// and usage examples.
    Explain {
        /// Action to explain, as a bare name (`new-pane`) or full ref
        /// (`core:new-pane`).
        action: String,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
}

impl CliCommand {
    /// The typed action this subcommand requests: its `core:` action
    /// reference paired with the fully-built core [`Command`].
    ///
    /// `targets` carries this invocation's `--session`/`--tab` flags already
    /// resolved to ids (a name looked up against the running sessions); the
    /// routing layer builds it, and a verb without those flags passes
    /// `ResolvedTargets::default()`. A resolved target wins; without one, a
    /// flag given directly as an id is used as-is, so only a flag given as a
    /// NAME needs the routing layer's lookup.
    ///
    /// `None` for the verbs that are not actions — the lifecycle commands
    /// (`new`, `list-sessions`, `kill-session`, `doctor`), the read-only
    /// discovery and introspection queries (`inspect`, the `list-*` verbs,
    /// `actions`, and `keys`, all rendered locally), and the verbs whose
    /// argument surfaces are not built yet (`config`, `plugin`).
    #[must_use]
    pub fn to_action(&self, targets: &ResolvedTargets) -> Option<(ActionRef, Command)> {
        let (name, command) = match self {
            CliCommand::NewPane {
                direction,
                stacked,
                pane,
                session: _,
                tab,
                client,
            } => (
                "new-pane",
                Command::NewPane(NewPaneArgs {
                    source: *pane,
                    tab: targets.tab.or(tab_ref_id(tab)),
                    direction: direction.map(Direction::from),
                    stacked: *stacked,
                    cwd: None,
                    command: None,
                    client: *client,
                }),
            ),
            CliCommand::ClosePane { pane, force } => (
                "close-pane",
                Command::ClosePane(ClosePaneArgs {
                    pane: *pane,
                    force: *force,
                    tree: false,
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
            CliCommand::Input {
                text,
                pane,
                no_enter,
            } => {
                // A shell runs a line when it reads a carriage return — the
                // byte the Enter key sends — so the text alone sits at the
                // prompt and the text plus `\r` runs.
                let mut data = text.clone().into_bytes();
                if !no_enter {
                    data.push(b'\r');
                }
                (
                    "write-to-pane",
                    Command::WriteToPane(WriteToPaneArgs { pane: *pane, data }),
                )
            }
            CliCommand::NewTab { session: _ } => {
                ("new-tab", Command::NewTab(NewTabArgs::default()))
            }
            CliCommand::CloseTab {
                tab,
                session: _,
                force,
            } => (
                "close-tab",
                Command::CloseTab(CloseTabArgs {
                    tab: targets.tab.or(tab_ref_id(tab)),
                    force: *force,
                    tree: false,
                }),
            ),
            CliCommand::NextTab { client } => (
                "next-tab",
                Command::FocusTab(FocusTabArgs {
                    target: TabTarget::Next,
                    client: *client,
                }),
            ),
            CliCommand::PreviousTab { client } => (
                "previous-tab",
                Command::FocusTab(FocusTabArgs {
                    target: TabTarget::Prev,
                    client: *client,
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
            CliCommand::FocusTab { index, tab, client } => {
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
                        client: *client,
                    }),
                )
            }
            CliCommand::FocusPane { pane, client } => (
                "focus-pane",
                Command::FocusPane(FocusPaneArgs {
                    target: FocusTarget::Pane(*pane),
                    client: *client,
                }),
            ),
            CliCommand::Lock { client } => (
                "lock",
                Command::SetLockMode(LockModeArgs {
                    locked: true,
                    client: *client,
                }),
            ),
            CliCommand::Unlock { client } => (
                "unlock",
                Command::SetLockMode(LockModeArgs {
                    locked: false,
                    client: *client,
                }),
            ),
            CliCommand::ToggleLock { client } => (
                "toggle-lock",
                Command::ToggleLockMode(ToggleLockModeArgs { client: *client }),
            ),
            CliCommand::RenameSession { session } => (
                "rename-session",
                Command::RenameSession(RenameSessionArgs {
                    session: targets.session.or(session_ref_id(session)),
                }),
            ),
            CliCommand::Run {
                direction,
                stacked,
                pane,
                session: _,
                tab,
                client,
                command,
            } => (
                "run",
                Command::RunCommandPane(RunCommandPaneArgs {
                    command: spawn_spec_from_argv(command),
                    cwd: None,
                    source: *pane,
                    tab: targets.tab.or(tab_ref_id(tab)),
                    direction: direction.map(Direction::from),
                    stacked: *stacked,
                    client: *client,
                }),
            ),
            CliCommand::New
            | CliCommand::ListSessions { .. }
            | CliCommand::KillSession { .. }
            | CliCommand::Doctor
            | CliCommand::Config
            | CliCommand::Plugin
            | CliCommand::Update
            | CliCommand::Actions { .. }
            | CliCommand::Inspect { .. }
            | CliCommand::ListTabs { .. }
            | CliCommand::ListPanes { .. }
            | CliCommand::ListClients { .. }
            | CliCommand::Keys { .. } => return None,
        };
        let action = ActionRef::core(name)
            .expect("CLI action names are constants satisfying the action-name grammar");
        Some((action, command))
    }

    /// The `--session` flag of this invocation, for the verbs that take one.
    /// The routing layer reads it to pick which running session the command
    /// is sent to.
    #[must_use]
    pub fn target_session(&self) -> Option<&SessionRef> {
        match self {
            CliCommand::NewPane { session, .. }
            | CliCommand::Run { session, .. }
            | CliCommand::NewTab { session }
            | CliCommand::CloseTab { session, .. }
            | CliCommand::RenameSession { session } => session.as_ref(),
            _ => None,
        }
    }

    /// The `--tab` flag of this invocation, for the verbs that take one. The
    /// routing layer resolves it to a concrete tab id within the target
    /// session.
    #[must_use]
    pub fn target_tab(&self) -> Option<&TabRef> {
        match self {
            CliCommand::NewPane { tab, .. }
            | CliCommand::Run { tab, .. }
            | CliCommand::CloseTab { tab, .. } => tab.as_ref(),
            _ => None,
        }
    }

    /// The explicit pane this invocation names, for the verbs that take one.
    /// The routing layer reads it to find the session owning that pane.
    #[must_use]
    pub fn target_pane(&self) -> Option<PaneId> {
        match self {
            CliCommand::NewPane { pane, .. }
            | CliCommand::Run { pane, .. }
            | CliCommand::ClosePane { pane, .. }
            | CliCommand::ResizePane { pane, .. }
            | CliCommand::RenamePane { pane }
            | CliCommand::Input { pane, .. } => *pane,
            CliCommand::FocusPane { pane, .. } => Some(*pane),
            _ => None,
        }
    }

    /// The explicit client this invocation names, for the verbs that take
    /// one. The routing layer reads it to find the session that client is
    /// attached to.
    #[must_use]
    pub fn target_client(&self) -> Option<ClientId> {
        match self {
            CliCommand::NewPane { client, .. }
            | CliCommand::Run { client, .. }
            | CliCommand::NextTab { client }
            | CliCommand::PreviousTab { client }
            | CliCommand::FocusTab { client, .. }
            | CliCommand::FocusPane { client, .. }
            | CliCommand::Lock { client }
            | CliCommand::Unlock { client }
            | CliCommand::ToggleLock { client } => *client,
            _ => None,
        }
    }
}

/// The id inside a `--tab` flag given directly as one; a name (or no flag)
/// yields `None` and needs the routing layer's lookup.
fn tab_ref_id(tab: &Option<TabRef>) -> Option<TabId> {
    match tab {
        Some(TabRef::Id(id)) => Some(*id),
        _ => None,
    }
}

/// The id inside a `--session` flag given directly as one; a name (or no
/// flag) yields `None` and needs the routing layer's lookup.
fn session_ref_id(session: &Option<SessionRef>) -> Option<SessionId> {
    match session {
        Some(SessionRef::Id(id)) => Some(*id),
        _ => None,
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
pub(crate) fn parse_prefixed_uuid(value: &str, prefix: &str) -> Result<Uuid, String> {
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
