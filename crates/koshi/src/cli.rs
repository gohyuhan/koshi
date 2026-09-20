//! Command-line grammar for the `koshi` binary: the root parser and the
//! subcommand tree.
//!
//! A bare `koshi` launches the interactive app: it spawns a new session and
//! attaches this terminal to it. The root `--headless` flag spawns the session
//! and attaches nothing. Every verb is a subcommand, `attach` and `detach`
//! included. The root `--remote` flag reaches every subcommand and names the
//! machine that invocation runs against. Parsing yields typed values only; no
//! command here talks to a runtime.
//!
//! Action subcommands carry typed arguments and map to the core command
//! vocabulary through [`CliCommand::build_action_command`](crate::cli::CliCommand::build_action_command),
//! which pairs each with its `core:` action reference. Entity ids are parsed
//! at this boundary: a flag accepts the id exactly as koshi prints it
//! (`pane-<uuid>`) or as a bare UUID. A session or tab argument accepts the
//! display name too: a value that reads as an id (`session-<uuid>`,
//! `tab-<uuid>`, or a bare UUID) is that id, anything else is a name.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use koshi_core::action::ActionReference;
use koshi_core::command::{
    ClosePaneArgs, CloseTabArgs, Command, FocusPaneArgs, FocusTabArgs, FocusTarget, LockModeArgs,
    MovePaneArgs, MoveTabArgs, NewPaneArgs, NewTabArgs, PanePlacementAnchor, PanePlacementTarget,
    PlacePaneArgs, ResizePaneArgs, RunCommandPaneArgs, ScrollPaneArgs, SwapPanesArgs, TabTarget,
    ToggleLockModeArgs, WriteToPaneArgs,
};
use koshi_core::geometry::Direction;
use koshi_core::ids::parse_prefixed_uuid;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::process::{ShellKind, SpawnSpec};

/// A parsed `koshi` invocation.
#[derive(Debug, PartialEq, Eq, Parser)]
#[command(
    name = "koshi",
    version,
    about = "A tiling terminal multiplexer",
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// Create a session, print its id, and return to the shell with nothing
    /// attached.
    #[arg(long = "headless")]
    pub is_headless: bool,

    /// Let the other users of this machine reach the session this command
    /// creates, whatever `koshi.kdl` says. Only with `--headless`.
    #[arg(long = "allow-other-users", requires = "is_headless")]
    pub should_allow_other_users: bool,

    /// Launch with a named profile: read `profile/<name>.kdl` from the config
    /// directory and open its tabs and panes instead of a single shell.
    #[arg(long = "profile", value_name = "NAME")]
    pub profile_name: Option<String>,

    /// Run this invocation against the machine SERVER names — the name it was
    /// saved under, or the `host:port` it listens on — instead of this one.
    #[arg(long = "remote", global = true, value_name = "SERVER")]
    pub remote_server_reference: Option<String>,

    /// The verb to run; absent on the bare interactive launch.
    #[command(subcommand)]
    pub command: Option<CliCommand>,
}

impl Cli {
    /// True for the bare `koshi` invocation — no subcommand, no `--headless`
    /// and no `--remote` — which launches the interactive app.
    #[must_use]
    pub fn is_interactive_launch(&self) -> bool {
        !self.is_headless && self.command.is_none() && self.remote_server_reference.is_none()
    }
}

/// A split or resize direction as typed on the command line. Converts to the
/// core [`Direction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DirectionArgument {
    /// Rightward.
    Right,
    /// Downward.
    Down,
    /// Leftward.
    Left,
    /// Upward.
    Up,
}

impl From<DirectionArgument> for Direction {
    fn from(direction_argument: DirectionArgument) -> Direction {
        match direction_argument {
            DirectionArgument::Right => Direction::Right,
            DirectionArgument::Down => Direction::Down,
            DirectionArgument::Left => Direction::Left,
            DirectionArgument::Up => Direction::Up,
        }
    }
}

/// A session named on the command line: a `session-<uuid>` id (or bare
/// UUID), or a display name to look up against the running sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionReference {
    /// An exact session id.
    SessionId(SessionId),
    /// A display name; it must match exactly one running session.
    SessionName(String),
}

impl fmt::Display for SessionReference {
    /// Writes the reference as the user named it: the session id for `Id`,
    /// the display name for `Name`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionReference::SessionId(session_id) => session_id.fmt(f),
            SessionReference::SessionName(session_name) => f.write_str(session_name),
        }
    }
}

/// A tab named on the command line: a `tab-<uuid>` id (or bare UUID), or a
/// display name to look up against the target session's tabs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabReference {
    /// An exact tab id.
    TabId(TabId),
    /// A display name; it must match exactly one tab.
    TabName(String),
}

/// Parse a session argument: an id when the value reads as one, else a
/// display name. An empty value is `Err("expected a session id or name")`.
pub fn parse_session_reference(session_argument: &str) -> Result<SessionReference, String> {
    if session_argument.is_empty() {
        return Err("expected a session id or name".to_string());
    }
    Ok(match parse_prefixed_uuid(session_argument, "session") {
        Ok(uuid) => SessionReference::SessionId(SessionId::from_uuid(uuid)),
        Err(_) => SessionReference::SessionName(session_argument.to_string()),
    })
}

/// How long a granted token works, counted from the moment the grant is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    /// The token stops working this long after it is granted.
    After(Duration),
    /// The token never stops working on its own.
    Never,
}

/// Parse a duration argument: a decimal count followed by one unit character —
/// `s` seconds, `m` minutes, `h` hours, `d` days. `30s` is thirty seconds.
///
/// `expected_error_message` is reported for every value this cannot read: an empty value, a
/// unit character that is none of the four, a count that is not a whole
/// number, and a count times its unit that overflows `u64` seconds.
fn parse_duration_argument(
    duration_argument: &str,
    expected_error_message: &'static str,
) -> Result<Duration, String> {
    let mut duration_characters = duration_argument.chars();
    let duration_unit = duration_characters
        .next_back()
        .ok_or(expected_error_message)?;
    let duration_unit_seconds: u64 = match duration_unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        _ => return Err(expected_error_message.to_string()),
    };
    let duration_unit_count: u64 = duration_characters
        .as_str()
        .parse()
        .map_err(|_| expected_error_message)?;
    let duration_seconds = duration_unit_count
        .checked_mul(duration_unit_seconds)
        .ok_or(expected_error_message)?;
    Ok(Duration::from_secs(duration_seconds))
}

/// Parse an expiry argument: the word `never`, or a decimal count followed by
/// one unit character — `s` seconds, `m` minutes, `h` hours, `d` days.
///
/// A count times its unit that overflows `u64` seconds is an error.
pub fn parse_expiry(expiry_argument: &str) -> Result<Expiry, String> {
    const EXPECTED_EXPIRY_ERROR_MESSAGE: &str =
        "expected a length such as 30s, 15m, 24h or 7d, or the word never";

    if expiry_argument == "never" {
        return Ok(Expiry::Never);
    }
    Ok(Expiry::After(parse_duration_argument(
        expiry_argument,
        EXPECTED_EXPIRY_ERROR_MESSAGE,
    )?))
}

/// Parse a `--since` flag value: a decimal count followed by one unit
/// character — `s` seconds, `m` minutes, `h` hours, `d` days. Every value this
/// cannot read is `Err("expected a length such as 30s, 15m, 24h or 7d")`.
fn parse_event_age(event_age_argument: &str) -> Result<Duration, String> {
    parse_duration_argument(
        event_age_argument,
        "expected a length such as 30s, 15m, 24h or 7d",
    )
}

/// Parse a `--filter` flag value: any text an event name may contain. An empty
/// value is `Err("expected part of an event name, such as pane or TabMoved")`.
fn parse_event_filter(filter_argument: &str) -> Result<String, String> {
    if filter_argument.is_empty() {
        return Err("expected part of an event name, such as pane or TabMoved".to_string());
    }
    Ok(filter_argument.to_string())
}

/// Parse a `--tab` flag value: an id when the value reads as one, else a
/// display name. An empty value is `Err("expected a tab id or name")`.
fn parse_tab_reference(tab_reference_argument: &str) -> Result<TabReference, String> {
    if tab_reference_argument.is_empty() {
        return Err("expected a tab id or name".to_string());
    }
    Ok(match parse_prefixed_uuid(tab_reference_argument, "tab") {
        Ok(uuid) => TabReference::TabId(TabId::from_uuid(uuid)),
        Err(_) => TabReference::TabName(tab_reference_argument.to_string()),
    })
}

/// The `--session`/`--tab` flags of one invocation, resolved to concrete ids
/// (a name looked up against the running sessions). The routing layer builds
/// this before [`CliCommand::build_action_command`]; a verb without those flags takes
/// `default()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResolvedTargets {
    /// The resolved `--session` value.
    pub session_id: Option<SessionId>,
    /// The resolved `--tab` value.
    pub tab_id: Option<TabId>,
}

/// The output format of a discovery query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable aligned columns.
    Table,
    /// Machine-readable JSON.
    Json,
}

/// The `koshi` subcommand tree.
///
/// Lifecycle commands (`list-sessions`, `kill-session`, `attach`, `detach`,
/// `doctor`) run outside any session, except a bare `detach`, which names this
/// pane's own client. Action subcommands carry their typed arguments and map
/// to core commands via [`CliCommand::build_action_command`]. The discovery queries
/// (`inspect`, the `list-*` verbs) carry typed target and `--format`
/// arguments; their answers are rendered by [`crate::output`]. `actions`
/// introspects the action registry through its `list`/`explain` subcommands,
/// and `keys` introspects the keymap through its own subcommand tree.
/// `config` validates and migrates files locally. `share` reaches the router
/// over the control plane; the router is the only writer of the remote access
/// token store. `remote` reads and writes the servers this machine has saved,
/// and reaches no network. `version` prints this program's own build, and
/// `server-version` asks each running koshi server for the build it runs; both
/// carry `--format` and render through [`crate::output`]. `plugin` takes no
/// arguments.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum CliCommand {
    /// List running sessions, here and on every saved server that answers.
    ListSessions {
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
    /// Kill a session; without a name, targets the only running session.
    KillSession {
        /// Session to kill, by id or name.
        #[arg(value_parser = parse_session_reference, value_name = "SESSION")]
        session_reference: Option<SessionReference>,
    },
    /// Attach this terminal to a running session as a second window onto it.
    Attach {
        /// Session to attach to, by id or name; without one, pick from the
        /// sessions running for this user and on the saved servers.
        #[arg(value_name = "SESSION")]
        session_argument: Option<String>,
        /// Save a server reached for the first time under this name, so subsequent
        /// commands name it instead of its address.
        #[arg(long, requires = "remote_server_reference", value_name = "NAME")]
        save_as: Option<String>,
    },
    /// Detach one client, or with `--all` every client of a session. The
    /// session keeps running and its panes are untouched.
    Detach {
        /// Without `--all`: the client to detach, by client id, session id, or
        /// session name. With `--all`: the session whose clients all detach,
        /// by id or name. Without a value, this pane's own client or session.
        #[arg(value_name = "CLIENT_OR_SESSION")]
        detach_target: Option<String>,
        /// Detach every client attached to the session instead of one client.
        #[arg(long = "all")]
        should_detach_all_clients: bool,
    },
    /// Check the local koshi installation and environment.
    Doctor {
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
    /// Open a new pane running a shell; its working directory and
    /// environment come from the issuing terminal.
    NewPane {
        /// Split direction; omitted follows your `layout.new-pane-direction`
        /// setting.
        #[arg(
            long = "direction",
            value_enum,
            value_name = "DIRECTION",
            conflicts_with = "should_stack"
        )]
        direction: Option<DirectionArgument>,
        /// Stack the new pane onto the source pane instead of splitting.
        #[arg(long = "stacked")]
        should_stack: bool,
        /// Pane to split from; defaults to the focused pane.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: Option<PaneId>,
        /// Session receiving the pane, by id or name; defaults to the current
        /// session, else the only running one.
        #[arg(long = "session", value_parser = parse_session_reference, value_name = "SESSION")]
        session_reference: Option<SessionReference>,
        /// Tab receiving the pane, by id or name; the split anchors on that
        /// tab's most recently focused pane. Defaults to the source pane's tab.
        #[arg(
            long = "tab",
            value_parser = parse_tab_reference,
            value_name = "TAB",
            conflicts_with = "pane_id"
        )]
        tab_reference: Option<TabReference>,
        /// Client that shows and focuses the new pane; defaults to the
        /// issuing client, else the session's only attached one.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Close a pane.
    ClosePane {
        /// Pane to close; defaults to the focused pane.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: Option<PaneId>,
        /// Kill the pane's child immediately, overriding its close policy.
        #[arg(long = "force")]
        should_force_close: bool,
    },
    /// Move one of a pane's borders: a positive size grows the pane toward
    /// the direction, a negative size shrinks it.
    ResizePane {
        /// Which of the pane's borders moves.
        #[arg(long, value_enum, value_name = "DIRECTION")]
        direction: DirectionArgument,
        /// Signed number of cells the border moves; defaults to 1.
        #[arg(
            long = "size",
            value_name = "SIZE",
            default_value_t = 1,
            allow_negative_numbers = true
        )]
        resize_amount_cells: i16,
        /// Pane to resize; defaults to the focused pane.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: Option<PaneId>,
    },
    /// Move a pane into the slot of its visible neighbor.
    MovePane {
        /// Direction in which to choose the visible neighbor.
        #[arg(long, value_enum, value_name = "DIRECTION")]
        direction: DirectionArgument,
        /// Pane to move; defaults to the focused pane.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: Option<PaneId>,
    },
    /// Exchange two pane occupants.
    SwapPanes {
        /// Pane whose occupant receives the source pane's slot.
        #[arg(long = "with", value_parser = parse_pane_id, value_name = "PANE_ID")]
        target_pane_id: PaneId,
        /// Pane whose occupant moves; defaults to the focused pane.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: Option<PaneId>,
    },
    /// Insert a pane into another tab's tiled layout.
    PlacePane {
        /// Pane to place.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: PaneId,
        /// Destination tab, by id or name.
        #[arg(
            long = "tab",
            value_parser = parse_tab_reference,
            value_name = "TAB",
            required = true
        )]
        tab_reference: TabReference,
        /// Side of the destination tab where the pane lands.
        #[arg(long, value_enum, value_name = "DIRECTION")]
        direction: DirectionArgument,
        /// Client whose committed view supplies the destination sizing.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Scroll one client's view of a pane.
    ScrollPane {
        /// Signed scroll line count; positive moves toward history.
        #[arg(long = "lines", value_name = "LINES", allow_negative_numbers = true)]
        scroll_line_count: i32,
        /// Pane whose view scrolls; defaults to the target client's focused pane.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: Option<PaneId>,
        /// Client whose view scrolls; defaults to the issuing client.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Toggle fullscreen on the focused pane.
    TogglePaneFullscreen {
        /// Client whose own view goes fullscreen; defaults to the issuing
        /// client, else the session's only attached one.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Type text into a pane's shell, as if it had been typed there. The text
    /// is followed by Enter, so the shell runs it; `--no-enter` leaves it
    /// waiting at the prompt.
    Input {
        /// Text to type into the pane. Text starting with `-` is taken as text,
        /// not as a flag, so a scripted line is passed through whatever it says.
        #[arg(value_name = "TEXT", allow_hyphen_values = true)]
        input_text: String,
        /// Pane to type into; defaults to the focused pane.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: Option<PaneId>,
        /// Leave the text at the prompt instead of pressing Enter after it.
        #[arg(long = "no-enter")]
        should_leave_input_at_prompt: bool,
    },
    /// Open a new tab; its first pane inherits the issuing terminal's
    /// working directory and environment.
    NewTab {
        /// Session the tab joins, by id or name; defaults to the current
        /// session, else the only running one.
        #[arg(long = "session", value_parser = parse_session_reference, value_name = "SESSION")]
        session_reference: Option<SessionReference>,
        /// Client that switches onto the new tab; defaults to the issuing
        /// client, else the session's only attached one.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Close a tab.
    CloseTab {
        /// Tab to close, by id or name; defaults to the focused tab.
        #[arg(long = "tab", value_parser = parse_tab_reference, value_name = "TAB")]
        tab_reference: Option<TabReference>,
        /// Session owning the tab, by id or name; defaults to the current
        /// session, else the only running one.
        #[arg(long = "session", value_parser = parse_session_reference, value_name = "SESSION")]
        session_reference: Option<SessionReference>,
        /// Kill every pane's child immediately, overriding each close policy.
        #[arg(long = "force")]
        should_force_close: bool,
    },
    /// Focus the next tab.
    NextTab {
        /// Client whose view switches; defaults to the issuing client.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Focus the previous tab.
    PreviousTab {
        /// Client whose view switches; defaults to the issuing client.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Move a tab to a new index.
    MoveTab {
        /// Destination zero-based index.
        #[arg(long = "index", value_name = "INDEX")]
        tab_index: usize,
        /// Tab to move, by id or name; defaults to the focused tab.
        #[arg(long = "tab", value_parser = parse_tab_reference, value_name = "TAB")]
        tab_reference: Option<TabReference>,
    },
    /// Focus a tab by index, id, or name.
    FocusTab {
        /// Zero-based index of the tab to focus.
        #[arg(
            long = "index",
            value_name = "INDEX",
            conflicts_with = "tab_reference",
            required_unless_present = "tab_reference"
        )]
        tab_index: Option<usize>,
        /// Tab to focus, by id or name.
        #[arg(long = "tab", value_parser = parse_tab_reference, value_name = "TAB")]
        tab_reference: Option<TabReference>,
        /// Client whose view switches; defaults to the issuing client.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Focus a pane by id.
    FocusPane {
        /// Pane to focus.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: PaneId,
        /// Client whose focus moves; defaults to the issuing client.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Enter locked input mode.
    Lock {
        /// Client to lock; defaults to the issuing client, else the
        /// session's only attached one.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Leave locked input mode.
    Unlock {
        /// Client to unlock; defaults to the issuing client, else the
        /// session's only attached one.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Toggle locked input mode.
    ToggleLock {
        /// Client whose lock flips; defaults to the issuing client, else the
        /// session's only attached one.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Inspect, validate, and migrate configuration.
    Config {
        /// What to do with the config.
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Grant, revoke and list remote access tokens.
    Share {
        /// What to do with the tokens.
        #[command(subcommand)]
        command: ShareCommand,
    },
    /// Save, change, list, forget and re-secret the servers this machine has
    /// saved.
    Remote {
        /// What to do with the saved servers.
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Print diagnostics for a bug report.
    Debug {
        /// Which dump to print.
        #[command(subcommand)]
        command: DebugCommand,
    },
    /// Manage plugins. Hidden from help until the plugin host exists;
    /// invoking it reports the runtime as unavailable.
    #[command(hide = true)]
    Plugin,
    /// Download and install the latest koshi release.
    Update,
    /// Print the version of the koshi program running this command.
    Version {
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
    /// Print the version of every running koshi server: this machine's
    /// router, and each running session.
    ServerVersion {
        /// Report this session alone, by id or name, and leave out the
        /// router.
        #[arg(long = "session", value_parser = parse_session_reference, value_name = "SESSION")]
        session_reference: Option<SessionReference>,
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
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
        inspect_target: InspectTarget,
    },
    /// List tabs across every running session.
    ListTabs {
        /// Narrow the listing to one session, by id or name.
        #[arg(long = "session", value_parser = parse_session_reference, value_name = "SESSION")]
        session_reference: Option<SessionReference>,
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
    /// List panes across every running session.
    ListPanes {
        /// Narrow the listing to one session, by id or name.
        #[arg(long = "session", value_parser = parse_session_reference, value_name = "SESSION")]
        session_reference: Option<SessionReference>,
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
    /// List clients attached across every running session.
    ListClients {
        /// Narrow the listing to one session, by id or name.
        #[arg(long = "session", value_parser = parse_session_reference, value_name = "SESSION")]
        session_reference: Option<SessionReference>,
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
    /// Open a new pane running the command given after `--`; its working
    /// directory and environment come from the issuing terminal.
    Run {
        /// Split direction; omitted follows your `layout.new-pane-direction`
        /// setting.
        #[arg(
            long = "direction",
            value_enum,
            value_name = "DIRECTION",
            conflicts_with = "should_stack"
        )]
        direction: Option<DirectionArgument>,
        /// Stack the new pane onto the source pane instead of splitting.
        #[arg(long = "stacked")]
        should_stack: bool,
        /// Pane to split from; defaults to the focused pane.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: Option<PaneId>,
        /// Session receiving the pane, by id or name; defaults to the current
        /// session, else the only running one.
        #[arg(long = "session", value_parser = parse_session_reference, value_name = "SESSION")]
        session_reference: Option<SessionReference>,
        /// Tab receiving the pane, by id or name; the split anchors on that
        /// tab's most recently focused pane. Defaults to the source pane's tab.
        #[arg(
            long = "tab",
            value_parser = parse_tab_reference,
            value_name = "TAB",
            conflicts_with = "pane_id"
        )]
        tab_reference: Option<TabReference>,
        /// Client that shows and focuses the new pane; defaults to the
        /// issuing client, else the session's only attached one.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
        /// The command and its arguments, given after `--`.
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command_arguments: Vec<String>,
    },
    /// Inspect keybindings.
    Keys {
        /// What to do.
        #[command(subcommand)]
        command: KeysCommand,
    },
    /// Run the router process.
    #[command(hide = true)]
    ServeRouter {
        /// Runtime directory to serve; defaults to this user's own.
        #[arg(long = "runtime-dir", value_name = "DIRECTORY")]
        runtime_directory: Option<PathBuf>,
        /// Wait for the router lock instead of yielding to the router that
        /// holds it. A router restarting into a newly installed binary passes
        /// this.
        #[arg(long = "wait-for-lock")]
        should_wait_for_lock: bool,
    },
    /// Run one session's server process.
    #[command(hide = true)]
    ServeSession {
        /// The session's id, which the router picked.
        #[arg(value_parser = parse_session_id, value_name = "SESSION_ID")]
        session_id: SessionId,
        /// The session's display name, which the router generated.
        #[arg(value_name = "SESSION_NAME")]
        session_name: String,
        /// Runtime directory to serve; defaults to this user's own.
        #[arg(long = "runtime-dir", value_name = "DIRECTORY")]
        runtime_directory: Option<PathBuf>,
        /// Open this profile's tabs and panes instead of one shell.
        #[arg(long = "profile", value_name = "NAME")]
        profile_name: Option<String>,
        /// Let the other users of this machine reach this session, whatever
        /// `koshi.kdl` says.
        #[arg(long = "allow-other-users")]
        should_allow_other_users: bool,
        /// Come up from the state at this path instead of seeding a new
        /// session. The image being replaced wrote it; this one reads it once
        /// and removes it.
        #[arg(long = "resume", value_name = "PATH")]
        resume_state_path: Option<PathBuf>,
        /// The secret the link to the process holding this session's panes
        /// presents. Windows only, and only on a resume run.
        #[arg(long, value_name = "TOKEN")]
        supervisor_token: Option<String>,
        /// The process id of the process holding this session's panes, which
        /// its link address is derived from. Windows only, and only on a
        /// resume run.
        #[arg(long, value_name = "PID")]
        supervisor_pid: Option<u32>,
    },
    /// Run the process holding one session's panes.
    #[command(hide = true)]
    ServePtySupervisor {
        /// The session whose panes this process holds.
        #[arg(value_parser = parse_session_id, value_name = "SESSION_ID")]
        session_id: SessionId,
        /// The secret a link presents at Hello, which the session server
        /// generated.
        #[arg(value_name = "TOKEN")]
        supervisor_token: String,
        /// Runtime directory to serve; defaults to this user's own.
        #[arg(long = "runtime-dir", value_name = "DIRECTORY")]
        runtime_directory: Option<PathBuf>,
    },
    /// Print which resume-file formats this build takes back, as one JSON line.
    #[command(hide = true)]
    ResumeSupport,
}

/// Local config operations.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum ConfigCommand {
    /// Print the platform config directory.
    Path,
    /// Explain one file-qualified config key.
    Explain {
        /// Key to explain, such as `koshi.pane.min-cols`.
        config_key: String,
    },
    /// Validate every known config file without changing it.
    Check,
    /// Validate then migrate every known config file.
    Migrate,
}

/// The `koshi share` subcommands: the remote access tokens this machine has
/// granted. Every verb asks the router, which owns the token store.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum ShareCommand {
    /// Hand one identity a fresh token and print it once.
    Grant {
        /// Who the token is handed to, in the words you type here.
        #[arg(value_name = "IDENTITY")]
        identity: String,
        /// The one session the token reaches, by id or name. Without this
        /// flag the token reaches every session on this machine.
        #[arg(long = "session", value_parser = parse_session_reference, value_name = "SESSION")]
        session_reference: Option<SessionReference>,
        /// How long the token works: a length such as `30s`, `15m`, `24h` or
        /// `7d`, or the word `never`.
        #[arg(
            long = "expires",
            value_parser = parse_expiry,
            value_name = "DURATION",
            default_value = "24h"
        )]
        token_expiry: Expiry,
    },
    /// Stop the tokens one identity holds.
    Revoke {
        /// Whose tokens stop working.
        #[arg(value_name = "IDENTITY")]
        identity: String,
        /// The one grant that stops working, named by the session it reaches,
        /// by id or name. Without this flag every grant that identity holds
        /// stops working.
        #[arg(long = "session", value_parser = parse_session_reference, value_name = "SESSION")]
        session_reference: Option<SessionReference>,
    },
    /// List the grants this machine has made.
    List {
        /// List only the grants that reach this one session, by id or name.
        /// A grant that reaches every session on this machine is listed here
        /// too.
        #[arg(long = "session", value_parser = parse_session_reference, value_name = "SESSION")]
        session_reference: Option<SessionReference>,
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
}

/// The `koshi remote` subcommands: the servers this machine has connected to,
/// saved on this machine. Every verb reads or writes that store; none of them
/// prints a saved secret.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum RemoteCommand {
    /// Save a server this machine can dial, asking for its name, its address
    /// and its secret in turn.
    New,
    /// Change what one saved server holds, asking for its name, its address
    /// and its secret in turn, with the current value kept on an empty
    /// answer.
    Edit {
        /// Server to change, by the name it was saved under or its address.
        #[arg(value_name = "SERVER")]
        server_reference: String,
    },
    /// List the servers this machine has saved.
    List {
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
    /// Drop one saved server, so nothing on this machine holds its secret.
    Forget {
        /// Server to drop, by the name it was saved under or its address.
        #[arg(value_name = "SERVER")]
        server_reference: String,
    },
    /// Replace the secret of one saved server, after the machine serving it
    /// granted a fresh one.
    SetSecret {
        /// Server whose secret is replaced, by the name it was saved under or
        /// its address.
        #[arg(value_name = "SERVER")]
        server_reference: String,
    },
}

/// The `koshi debug` subcommands: read-only dumps for a bug report.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum DebugCommand {
    /// Print every running session's full record — its tabs, panes and
    /// clients — with each pane's command arguments hidden.
    DumpState {
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
    /// Print each tab's split tree, the rectangles it solves to, the panes
    /// with no room, the stacks, and each client's focus.
    DumpLayout {
        /// Narrow the answer to one tab, by id or name.
        #[arg(long = "tab", value_parser = parse_tab_reference, value_name = "TAB")]
        tab_reference: Option<TabReference>,
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
    /// Print the events each running session published most recently, oldest
    /// first. Each line names the event and the ids it named, never any
    /// content it carried.
    Events {
        /// Keep only the events recorded within this much of now, e.g. `30s`,
        /// `5m`, `2h`, `7d`.
        #[arg(long = "since", value_parser = parse_event_age, value_name = "LENGTH")]
        event_age_limit: Option<Duration>,
        /// Keep only the events whose name contains this text, matched
        /// ignoring case, e.g. `pane` or `TabMoved`.
        #[arg(long = "filter", value_parser = parse_event_filter, value_name = "NAME")]
        event_name_filter: Option<String>,
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
}

/// Which keymap layer authored a binding, as typed on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KeymapScope {
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
/// renders locally from the built-in defaults plus the user's keybinding file.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum KeysCommand {
    /// List effective keybindings per mode.
    List {
        /// Limit the listing to one input mode.
        #[arg(long = "mode", value_name = "MODE")]
        input_mode_name: Option<String>,
        /// Limit the listing to bindings authored by one layer.
        #[arg(long, value_enum, value_name = "SCOPE")]
        scope: Option<KeymapScope>,
        /// List plugin-recommended bindings instead of effective ones.
        #[arg(long = "recommended")]
        is_recommended: bool,
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
    /// Describe a key sequence: its action, source layer, and metadata.
    Describe {
        /// The key sequence, in the angle grammar (`"<C-p> n"`).
        #[arg(value_name = "KEY_SEQUENCE")]
        key_sequence_text: String,
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
    /// Report keybinding conflicts, dead bindings, and warnings.
    Conflicts {
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
    /// Dry-run a keybinding file: parse and conflict-check it without
    /// applying anything.
    Validate {
        /// Path of the keybinding KDL file to check.
        #[arg(value_name = "PATH")]
        keybinding_file_path: PathBuf,
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
}

/// The entity kinds `koshi inspect` reports on. Each takes the id exactly as
/// koshi prints it (`<kind>-<uuid>`) or as a bare UUID; a session or a tab
/// takes its display name too.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum InspectTarget {
    /// Report a session: name, creation time, clients, and pane count.
    Session {
        /// Session to inspect, by id or name.
        #[arg(value_parser = parse_session_reference, value_name = "SESSION")]
        session_reference: SessionReference,
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
    /// Report a tab: name, position, active pane, and pane count.
    Tab {
        /// Tab to inspect, by id or name.
        #[arg(value_parser = parse_tab_reference, value_name = "TAB")]
        tab_reference: TabReference,
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
    /// Report a pane: location, title, cwd, command, state, and rectangle.
    Pane {
        /// Pane to inspect.
        #[arg(value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: PaneId,
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
    /// Report a client: session, attach time, viewport, focus, and lock state.
    Client {
        /// Client to inspect.
        #[arg(value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: ClientId,
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
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
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
    /// Explain one action: its scope, target compatibility, internal command,
    /// and usage examples.
    Explain {
        /// Action to explain, as a bare name (`new-pane`) or full ref
        /// (`core:new-pane`).
        action_reference_text: String,
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
}

impl CliCommand {
    /// Whether this subcommand travels through the session control socket.
    #[must_use]
    pub fn is_action_verb(&self) -> bool {
        match self {
            CliCommand::NewPane { .. }
            | CliCommand::ClosePane { .. }
            | CliCommand::ResizePane { .. }
            | CliCommand::MovePane { .. }
            | CliCommand::SwapPanes { .. }
            | CliCommand::PlacePane { .. }
            | CliCommand::ScrollPane { .. }
            | CliCommand::TogglePaneFullscreen { .. }
            | CliCommand::Input { .. }
            | CliCommand::NewTab { .. }
            | CliCommand::CloseTab { .. }
            | CliCommand::NextTab { .. }
            | CliCommand::PreviousTab { .. }
            | CliCommand::MoveTab { .. }
            | CliCommand::FocusTab { .. }
            | CliCommand::FocusPane { .. }
            | CliCommand::Lock { .. }
            | CliCommand::Unlock { .. }
            | CliCommand::ToggleLock { .. }
            | CliCommand::Run { .. } => true,
            CliCommand::ListSessions { .. }
            | CliCommand::KillSession { .. }
            | CliCommand::Attach { .. }
            | CliCommand::Detach { .. }
            | CliCommand::Doctor { .. }
            | CliCommand::Config { .. }
            | CliCommand::Share { .. }
            | CliCommand::Remote { .. }
            | CliCommand::Debug { .. }
            | CliCommand::Plugin
            | CliCommand::Update
            | CliCommand::Version { .. }
            | CliCommand::ServerVersion { .. }
            | CliCommand::Actions { .. }
            | CliCommand::Inspect { .. }
            | CliCommand::ListTabs { .. }
            | CliCommand::ListPanes { .. }
            | CliCommand::ListClients { .. }
            | CliCommand::Keys { .. }
            | CliCommand::ServeRouter { .. }
            | CliCommand::ServeSession { .. }
            | CliCommand::ServePtySupervisor { .. }
            | CliCommand::ResumeSupport => false,
        }
    }

    /// The typed action this subcommand requests: its `core:` action
    /// reference paired with the fully-built core [`Command`].
    ///
    /// `resolved_targets` carries this invocation's `--session`/`--tab` flags already
    /// resolved to ids (a name looked up against the running sessions); the
    /// routing layer builds it, and a verb without those flags passes
    /// `ResolvedTargets::default()`. A resolved target wins; without one, a
    /// flag given directly as an id is used as-is.
    ///
    /// `new_pane_direction` is this CLI's own `layout.new-pane-direction`
    /// setting, read from `koshi.kdl` by
    /// [`config::resolve_new_pane_direction`](koshi_link::config::resolve_new_pane_direction). A
    /// pane-opening verb given no `--direction` splits toward it.
    ///
    /// `None` for the verbs that are not actions — the lifecycle commands
    /// (`list-sessions`, `kill-session`, `attach`, `detach`, `doctor`), the
    /// read-only discovery and local queries (`inspect`, the `list-*` verbs,
    /// `actions`, `keys`, `config`, and the `debug` dumps), `update`,
    /// `version`, `server-version`, `share`, `remote`, `plugin`, and the
    /// hidden `serve-router`, `serve-session`, `serve-pty-supervisor` and
    /// `resume-support`.
    #[must_use]
    pub fn build_action_command(
        &self,
        resolved_targets: &ResolvedTargets,
        new_pane_direction: Direction,
    ) -> Option<(ActionReference, Command)> {
        let (action_name, command) = match self {
            CliCommand::NewPane {
                direction,
                should_stack,
                pane_id,
                session_reference: _,
                tab_reference,
                client_id,
            } => (
                "new-pane",
                Command::NewPane(NewPaneArgs {
                    source_pane_id: *pane_id,
                    tab_id: resolved_targets
                        .tab_id
                        .or(resolve_tab_reference_id(tab_reference)),
                    direction: direction.map(Direction::from).unwrap_or(new_pane_direction),
                    should_stack: *should_stack,
                    working_directory: None,
                    spawn_spec: None,
                    client_id: *client_id,
                }),
            ),
            CliCommand::ClosePane {
                pane_id,
                should_force_close,
            } => (
                "close-pane",
                Command::ClosePane(ClosePaneArgs {
                    pane_id: *pane_id,
                    should_force_close: *should_force_close,
                    should_kill_process_tree: false,
                }),
            ),
            CliCommand::ResizePane {
                direction,
                resize_amount_cells,
                pane_id,
            } => (
                "resize-pane",
                Command::ResizePane(ResizePaneArgs {
                    pane_id: *pane_id,
                    direction: Direction::from(*direction),
                    resize_amount_cells: *resize_amount_cells,
                }),
            ),
            CliCommand::MovePane { direction, pane_id } => (
                "move-pane",
                Command::MovePane(MovePaneArgs {
                    pane_id: *pane_id,
                    direction: Direction::from(*direction),
                }),
            ),
            CliCommand::SwapPanes {
                target_pane_id,
                pane_id,
            } => (
                "swap-panes",
                Command::SwapPanes(SwapPanesArgs {
                    source_pane_id: *pane_id,
                    target_pane_id: *target_pane_id,
                }),
            ),
            CliCommand::PlacePane {
                pane_id,
                tab_reference,
                direction,
                client_id: _,
            } => (
                "place-pane",
                Command::PlacePane(PlacePaneArgs {
                    source_pane_id: *pane_id,
                    placement_target: PanePlacementTarget::Split {
                        destination_tab_id: resolved_targets
                            .tab_id
                            .or(resolve_tab_reference_id(&Some(tab_reference.clone())))
                            .expect("clap and target routing require --tab"),
                        anchor: PanePlacementAnchor::Tab,
                        direction: Direction::from(*direction),
                    },
                    expected_placement_revision: None,
                }),
            ),
            CliCommand::ScrollPane {
                scroll_line_count,
                pane_id,
                client_id: _,
            } => (
                "scroll-pane",
                Command::ScrollPane(ScrollPaneArgs {
                    pane_id: *pane_id,
                    scroll_line_count: *scroll_line_count,
                }),
            ),
            CliCommand::TogglePaneFullscreen { client_id: _ } => {
                ("toggle-pane-fullscreen", Command::TogglePaneFullscreen)
            }
            CliCommand::Input {
                input_text,
                pane_id,
                should_leave_input_at_prompt,
            } => {
                // The text alone sits at the shell prompt; the text plus `\r`,
                // the byte the Enter key sends, runs as a line.
                let mut input_bytes = input_text.clone().into_bytes();
                if !should_leave_input_at_prompt {
                    input_bytes.push(b'\r');
                }
                (
                    "write-to-pane",
                    Command::WriteToPane(WriteToPaneArgs {
                        pane_id: *pane_id,
                        input_bytes,
                    }),
                )
            }
            CliCommand::NewTab {
                session_reference: _,
                client_id,
            } => (
                "new-tab",
                Command::NewTab(NewTabArgs {
                    working_directory: None,
                    client_id: *client_id,
                }),
            ),
            CliCommand::CloseTab {
                tab_reference,
                session_reference: _,
                should_force_close,
            } => (
                "close-tab",
                Command::CloseTab(CloseTabArgs {
                    tab_id: resolved_targets
                        .tab_id
                        .or(resolve_tab_reference_id(tab_reference)),
                    should_force_close: *should_force_close,
                    should_kill_process_tree: false,
                }),
            ),
            CliCommand::NextTab { client_id } => (
                "next-tab",
                Command::FocusTab(FocusTabArgs {
                    focus_target: TabTarget::Next,
                    client_id: *client_id,
                }),
            ),
            CliCommand::PreviousTab { client_id } => (
                "previous-tab",
                Command::FocusTab(FocusTabArgs {
                    focus_target: TabTarget::Prev,
                    client_id: *client_id,
                }),
            ),
            CliCommand::MoveTab {
                tab_index,
                tab_reference,
            } => (
                "move-tab",
                Command::MoveTab(MoveTabArgs {
                    tab_id: resolved_targets
                        .tab_id
                        .or(resolve_tab_reference_id(tab_reference)),
                    target_tab_index: *tab_index,
                }),
            ),
            CliCommand::FocusTab {
                tab_index,
                tab_reference,
                client_id,
            } => {
                // The parser enforces exactly one of the two flags, and the
                // routing layer resolves a `--tab` name to its id.
                let focus_target = match (
                    tab_index,
                    resolved_targets
                        .tab_id
                        .or(resolve_tab_reference_id(tab_reference)),
                ) {
                    (Some(tab_index), None) => TabTarget::Index(*tab_index),
                    (None, Some(tab_id)) => TabTarget::Id(tab_id),
                    _ => unreachable!(
                        "clap enforces exactly one of --index/--tab, and routing resolves a tab name"
                    ),
                };
                (
                    "focus-tab",
                    Command::FocusTab(FocusTabArgs {
                        focus_target,
                        client_id: *client_id,
                    }),
                )
            }
            CliCommand::FocusPane { pane_id, client_id } => (
                "focus-pane",
                Command::FocusPane(FocusPaneArgs {
                    focus_target: FocusTarget::Pane(*pane_id),
                    client_id: *client_id,
                }),
            ),
            CliCommand::Lock { client_id } => (
                "lock",
                Command::SetLockMode(LockModeArgs {
                    is_locked: true,
                    client_id: *client_id,
                }),
            ),
            CliCommand::Unlock { client_id } => (
                "unlock",
                Command::SetLockMode(LockModeArgs {
                    is_locked: false,
                    client_id: *client_id,
                }),
            ),
            CliCommand::ToggleLock { client_id } => (
                "toggle-lock",
                Command::ToggleLockMode(ToggleLockModeArgs {
                    client_id: *client_id,
                }),
            ),
            CliCommand::Run {
                direction,
                should_stack,
                pane_id,
                session_reference: _,
                tab_reference,
                client_id,
                command_arguments,
            } => (
                "run",
                Command::RunCommandPane(RunCommandPaneArgs {
                    spawn_spec: build_spawn_spec_from_arguments(command_arguments),
                    working_directory: None,
                    source_pane_id: *pane_id,
                    tab_id: resolved_targets
                        .tab_id
                        .or(resolve_tab_reference_id(tab_reference)),
                    direction: direction.map(Direction::from).unwrap_or(new_pane_direction),
                    should_stack: *should_stack,
                    client_id: *client_id,
                }),
            ),
            CliCommand::ListSessions { .. }
            | CliCommand::KillSession { .. }
            | CliCommand::Attach { .. }
            | CliCommand::Detach { .. }
            | CliCommand::Doctor { .. }
            | CliCommand::Config { .. }
            | CliCommand::Share { .. }
            | CliCommand::Remote { .. }
            | CliCommand::Debug { .. }
            | CliCommand::Plugin
            | CliCommand::Update
            | CliCommand::Version { .. }
            | CliCommand::ServerVersion { .. }
            | CliCommand::Actions { .. }
            | CliCommand::Inspect { .. }
            | CliCommand::ListTabs { .. }
            | CliCommand::ListPanes { .. }
            | CliCommand::ListClients { .. }
            | CliCommand::Keys { .. }
            | CliCommand::ServeRouter { .. }
            | CliCommand::ServeSession { .. }
            | CliCommand::ServePtySupervisor { .. }
            | CliCommand::ResumeSupport => return None,
        };
        let action_reference = ActionReference::from_core_action_name(action_name)
            .expect("CLI action names are constants satisfying the action-name grammar");
        Some((action_reference, command))
    }

    /// The `--session` flag of this invocation, for the verbs that take one.
    /// The routing layer reads it to pick which running session the command
    /// is sent to.
    #[must_use]
    pub fn get_target_session_reference(&self) -> Option<&SessionReference> {
        match self {
            CliCommand::NewPane {
                session_reference, ..
            }
            | CliCommand::Run {
                session_reference, ..
            }
            | CliCommand::NewTab {
                session_reference, ..
            }
            | CliCommand::CloseTab {
                session_reference, ..
            } => session_reference.as_ref(),
            _ => None,
        }
    }

    /// The `--tab` flag of this invocation, for the verbs that take one. The
    /// routing layer resolves it to a concrete tab id within the target
    /// session.
    #[must_use]
    pub fn get_target_tab_reference(&self) -> Option<&TabReference> {
        match self {
            CliCommand::NewPane { tab_reference, .. }
            | CliCommand::Run { tab_reference, .. }
            | CliCommand::CloseTab { tab_reference, .. }
            | CliCommand::MoveTab { tab_reference, .. }
            | CliCommand::FocusTab { tab_reference, .. } => tab_reference.as_ref(),
            CliCommand::PlacePane { tab_reference, .. } => Some(tab_reference),
            _ => None,
        }
    }

    /// The explicit pane this invocation names, for the verbs that take one.
    /// The routing layer reads it to find the session owning that pane.
    #[must_use]
    pub fn get_target_pane_id(&self) -> Option<PaneId> {
        match self {
            CliCommand::NewPane { pane_id, .. }
            | CliCommand::Run { pane_id, .. }
            | CliCommand::ClosePane { pane_id, .. }
            | CliCommand::ResizePane { pane_id, .. }
            | CliCommand::ScrollPane { pane_id, .. }
            | CliCommand::Input { pane_id, .. } => *pane_id,
            CliCommand::MovePane { pane_id, .. } => *pane_id,
            CliCommand::PlacePane { pane_id, .. } => Some(*pane_id),
            CliCommand::SwapPanes {
                pane_id,
                target_pane_id,
            } => (*pane_id).or(Some(*target_pane_id)),
            CliCommand::FocusPane { pane_id, .. } => Some(*pane_id),
            _ => None,
        }
    }

    /// The explicit client this invocation names, for the verbs that take
    /// one. The routing layer reads it to find the session that client is
    /// attached to.
    #[must_use]
    pub fn get_target_client_id(&self) -> Option<ClientId> {
        match self {
            CliCommand::NewPane { client_id, .. }
            | CliCommand::Run { client_id, .. }
            | CliCommand::NewTab { client_id, .. }
            | CliCommand::NextTab { client_id }
            | CliCommand::PreviousTab { client_id }
            | CliCommand::FocusTab { client_id, .. }
            | CliCommand::FocusPane { client_id, .. }
            | CliCommand::Lock { client_id }
            | CliCommand::Unlock { client_id }
            | CliCommand::ToggleLock { client_id }
            | CliCommand::TogglePaneFullscreen { client_id }
            | CliCommand::ScrollPane { client_id, .. }
            | CliCommand::PlacePane { client_id, .. } => *client_id,
            _ => None,
        }
    }

    /// The client this invocation names that no [`Command`] carries; it rides
    /// on the command's source instead
    /// ([`CommandSource::ExternalCli`](koshi_core::command::CommandSource::ExternalCli)).
    /// `toggle-pane-fullscreen` and `scroll-pane` answer `Some`: every other
    /// client-taking verb puts its client in the command's own arguments, which travel on
    /// both routes.
    /// [`CommandSource::InSessionCli`](koshi_core::command::CommandSource::InSessionCli)
    /// carries no client, and a command with one here never takes the
    /// in-session route ([`crate::targeting::resolve_command_route`]).
    #[must_use]
    pub fn get_source_client_id(&self) -> Option<ClientId> {
        match self {
            CliCommand::TogglePaneFullscreen { client_id }
            | CliCommand::ScrollPane { client_id, .. }
            | CliCommand::PlacePane { client_id, .. } => *client_id,
            _ => None,
        }
    }

    /// Whether this is a discovery query: a `list-*` verb or an `inspect`
    /// form.
    #[must_use]
    pub fn is_discovery_query(&self) -> bool {
        matches!(
            self,
            CliCommand::ListSessions { .. }
                | CliCommand::ListTabs { .. }
                | CliCommand::ListPanes { .. }
                | CliCommand::ListClients { .. }
                | CliCommand::Inspect { .. }
        )
    }

    /// The one session a discovery query is scoped to, by id or name: a
    /// listing's `--session` flag, or the session an `inspect session` names.
    /// Every other query spans all running sessions.
    #[must_use]
    pub fn get_discovery_session_reference(&self) -> Option<&SessionReference> {
        match self {
            CliCommand::ListTabs {
                session_reference, ..
            }
            | CliCommand::ListPanes {
                session_reference, ..
            }
            | CliCommand::ListClients {
                session_reference, ..
            } => session_reference.as_ref(),
            CliCommand::Inspect {
                inspect_target:
                    InspectTarget::Session {
                        session_reference, ..
                    },
            } => Some(session_reference),
            _ => None,
        }
    }
}

/// The id inside a `--tab` flag given directly as one; a name (or no flag)
/// yields `None` and needs the routing layer's lookup.
fn resolve_tab_reference_id(tab_reference: &Option<TabReference>) -> Option<TabId> {
    match tab_reference {
        Some(TabReference::TabId(tab_id)) => Some(*tab_id),
        _ => None,
    }
}

/// Build the [`SpawnSpec`] for a `run` invocation's trailing argv: the first
/// token is the program, the rest its arguments. The working directory and
/// environment stay empty — they are filled from the issuing terminal when
/// the command is sent.
///
/// Panics when `argv` is empty.
fn build_spawn_spec_from_arguments(command_arguments: &[String]) -> SpawnSpec {
    let program = PathBuf::from(&command_arguments[0]);
    let shell_kind = ShellKind::from_program(&program);
    SpawnSpec {
        program,
        arguments: command_arguments[1..].to_vec(),
        working_directory: None,
        environment_variables: BTreeMap::new(),
        shell_kind,
    }
}

// Each id parser takes the id exactly as koshi prints it (`<prefix>-<uuid>`)
// or as a bare UUID. A value carrying another kind's prefix is rejected:
// `parse_pane_id("tab-<uuid>")` is an error, not a pane id.

/// Parse a session id argument into a [`SessionId`].
fn parse_session_id(session_argument: &str) -> Result<SessionId, String> {
    parse_prefixed_uuid(session_argument, "session").map(SessionId::from_uuid)
}

/// Parse a `--pane` flag value into a [`PaneId`].
fn parse_pane_id(pane_argument: &str) -> Result<PaneId, String> {
    parse_prefixed_uuid(pane_argument, "pane").map(PaneId::from_uuid)
}

/// Parse a `--client` flag value into a [`ClientId`].
fn parse_client_id(client_argument: &str) -> Result<ClientId, String> {
    parse_prefixed_uuid(client_argument, "client").map(ClientId::from_uuid)
}

#[cfg(test)]
mod tests;
