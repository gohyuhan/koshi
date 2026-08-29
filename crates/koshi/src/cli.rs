//! Command-line grammar for the `koshi` binary: the root parser and the
//! subcommand tree.
//!
//! A bare `koshi` launches the interactive app: it spawns a new session and
//! attaches this terminal to it. The root `--headless` flag spawns the session
//! and attaches nothing. Every verb is a subcommand, `attach` and `detach`
//! included. The root `--remote` flag reaches supported commands and names the
//! machine that invocation runs against. Parsing yields typed values only; no
//! command here talks to a runtime.
//!
//! Action subcommands carry typed arguments and map to the core command
//! vocabulary through [`CliCommand::to_action`](crate::cli::CliCommand::to_action),
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
use koshi_core::action::ActionRef;
use koshi_core::command::{
    ClosePaneArgs, CloseTabArgs, Command, FocusPaneArgs, FocusTabArgs, FocusTarget, LockModeArgs,
    MoveTabArgs, NewPaneArgs, NewTabArgs, ResizePaneArgs, RunCommandPaneArgs, TabTarget,
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
    #[arg(long)]
    pub headless: bool,

    /// Let the other users of this machine reach the session this command
    /// creates, whatever `koshi.kdl` says. Only with `--headless`.
    #[arg(long, requires = "headless")]
    pub allow_other_users: bool,

    /// Launch with a named profile: read `profile/<name>.kdl` from the config
    /// directory and open its tabs and panes instead of a single shell.
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,

    /// Run this supported invocation against the machine SERVER names — the
    /// name it was saved under, or the `host:port` it listens on — instead of
    /// this one.
    #[arg(long, global = true, value_name = "SERVER")]
    pub remote: Option<String>,

    /// The verb to run; absent on the bare interactive launch.
    #[command(subcommand)]
    pub command: Option<CliCommand>,
}

impl Cli {
    /// True for the bare `koshi` invocation — no subcommand, no `--headless`
    /// and no `--remote` — which launches the interactive app.
    #[must_use]
    pub fn is_interactive_launch(&self) -> bool {
        !self.headless && self.command.is_none() && self.remote.is_none()
    }
}

/// A split or resize direction as typed on the command line. Converts to the
/// core [`Direction`].
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

impl CliCommand {
    /// Whether this is a discovery query: a `list-*` verb or an `inspect`
    /// form.
    #[must_use]
    pub fn is_discovery(&self) -> bool {
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
    pub fn discovery_session(&self) -> Option<&SessionRef> {
        match self {
            CliCommand::ListTabs { session, .. }
            | CliCommand::ListPanes { session, .. }
            | CliCommand::ListClients { session, .. } => session.as_ref(),
            CliCommand::Inspect {
                target: InspectTarget::Session { session, .. },
            } => Some(session),
            _ => None,
        }
    }
}

impl fmt::Display for SessionRef {
    /// Writes the reference as the user named it: the session id for `Id`,
    /// the display name for `Name`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionRef::Id(id) => id.fmt(f),
            SessionRef::Name(name) => f.write_str(name),
        }
    }
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

/// Parse a session argument: an id when the value reads as one, else a
/// display name. An empty value is `Err("expected a session id or name")`.
pub fn parse_session_ref(value: &str) -> Result<SessionRef, String> {
    if value.is_empty() {
        return Err("expected a session id or name".to_string());
    }
    Ok(match parse_prefixed_uuid(value, "session") {
        Ok(uuid) => SessionRef::Id(SessionId::from_uuid(uuid)),
        Err(_) => SessionRef::Name(value.to_string()),
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

/// Parse a length argument: a decimal count followed by one unit character —
/// `s` seconds, `m` minutes, `h` hours, `d` days. `30s` is thirty seconds.
///
/// `expected` is reported for every value this cannot read: an empty value, a
/// unit character that is none of the four, a count that is not a whole
/// number, and a count times its unit that overflows `u64` seconds.
fn parse_length(value: &str, expected: &'static str) -> Result<Duration, String> {
    let mut characters = value.chars();
    let unit = characters.next_back().ok_or(expected)?;
    let unit_seconds: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        _ => return Err(expected.to_string()),
    };
    let count: u64 = characters.as_str().parse().map_err(|_| expected)?;
    let seconds = count.checked_mul(unit_seconds).ok_or(expected)?;
    Ok(Duration::from_secs(seconds))
}

/// Parse an expiry argument: the word `never`, or a decimal count followed by
/// one unit character — `s` seconds, `m` minutes, `h` hours, `d` days.
///
/// A count times its unit that overflows `u64` seconds is an error.
pub fn parse_expiry(value: &str) -> Result<Expiry, String> {
    const EXPECTED: &str = "expected a length such as 30s, 15m, 24h or 7d, or the word never";

    if value == "never" {
        return Ok(Expiry::Never);
    }
    Ok(Expiry::After(parse_length(value, EXPECTED)?))
}

/// Parse a `--since` flag value: a decimal count followed by one unit
/// character — `s` seconds, `m` minutes, `h` hours, `d` days. Every value this
/// cannot read is `Err("expected a length such as 30s, 15m, 24h or 7d")`.
fn parse_since(value: &str) -> Result<Duration, String> {
    parse_length(value, "expected a length such as 30s, 15m, 24h or 7d")
}

/// Parse a `--filter` flag value: any text an event name may contain. An empty
/// value is `Err("expected part of an event name, such as pane or TabMoved")`,
/// since every name contains the empty string.
fn parse_event_filter(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err("expected part of an event name, such as pane or TabMoved".to_string());
    }
    Ok(value.to_string())
}

/// Parse a `--tab` flag value: an id when the value reads as one, else a
/// display name. An empty value is `Err("expected a tab id or name")`.
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
/// Lifecycle commands (`list-sessions`, `kill-session`, `attach`, `detach`,
/// `doctor`) run outside any session, except a bare `detach`, which names this
/// pane's own client. Action subcommands carry their typed arguments and map
/// to core commands via [`CliCommand::to_action`]. The discovery queries
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
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// Kill a session; without a name, targets the only running session.
    KillSession {
        /// Session to kill, by id or name.
        #[arg(value_parser = parse_session_ref, value_name = "SESSION")]
        session: Option<SessionRef>,
    },
    /// Attach this terminal to a running session as a second window onto it.
    Attach {
        /// Session to attach to, by id or name; without one, pick from the
        /// sessions running for this user and on the saved servers.
        #[arg(value_name = "SESSION")]
        session: Option<String>,
        /// Save a server reached for the first time under this name, so later
        /// commands name it instead of its address.
        #[arg(long, requires = "remote", value_name = "NAME")]
        save_as: Option<String>,
    },
    /// Detach one client, or with `--all` every client of a session. The
    /// session keeps running and its panes are untouched.
    Detach {
        /// Without `--all`: the client to detach, by client id, session id, or
        /// session name. With `--all`: the session whose clients all detach,
        /// by id or name. Without a value, this pane's own client or session.
        #[arg(value_name = "CLIENT_OR_SESSION")]
        target: Option<String>,
        /// Detach every client attached to the session instead of one client.
        #[arg(long)]
        all: bool,
    },
    /// Check the local koshi installation and environment.
    Doctor {
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// Open a new pane running a shell; its working directory and
    /// environment come from the issuing terminal.
    NewPane {
        /// Split direction; omitted follows your `layout.new-pane-direction`
        /// setting.
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
    TogglePaneFullscreen {
        /// Client whose own view goes fullscreen; defaults to the issuing
        /// client, else the session's only attached one.
        #[arg(long, value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client: Option<ClientId>,
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
        /// Client that switches onto the new tab; defaults to the issuing
        /// client, else the session's only attached one.
        #[arg(long, value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client: Option<ClientId>,
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
    /// Move a tab to a new index.
    MoveTab {
        /// Destination zero-based index.
        #[arg(long, value_name = "INDEX")]
        index: usize,
        /// Tab to move, by id or name; defaults to the focused tab.
        #[arg(long, value_parser = parse_tab_ref, value_name = "TAB")]
        tab: Option<TabRef>,
    },
    /// Focus a tab by index, id, or name.
    FocusTab {
        /// Zero-based index of the tab to focus.
        #[arg(
            long,
            value_name = "INDEX",
            conflicts_with = "tab",
            required_unless_present = "tab"
        )]
        index: Option<usize>,
        /// Tab to focus, by id or name.
        #[arg(long, value_parser = parse_tab_ref, value_name = "TAB")]
        tab: Option<TabRef>,
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
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// Print the version of every running koshi server: this machine's
    /// router, and each running session.
    ServerVersion {
        /// Report this session alone, by id or name, and leave out the
        /// router.
        #[arg(long, value_parser = parse_session_ref, value_name = "SESSION")]
        session: Option<SessionRef>,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
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
    /// List tabs across every running session.
    ListTabs {
        /// Narrow the listing to one session, by id or name.
        #[arg(long, value_parser = parse_session_ref, value_name = "SESSION")]
        session: Option<SessionRef>,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// List panes across every running session.
    ListPanes {
        /// Narrow the listing to one session, by id or name.
        #[arg(long, value_parser = parse_session_ref, value_name = "SESSION")]
        session: Option<SessionRef>,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// List clients attached across every running session.
    ListClients {
        /// Narrow the listing to one session, by id or name.
        #[arg(long, value_parser = parse_session_ref, value_name = "SESSION")]
        session: Option<SessionRef>,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// Open a new pane running the command given after `--`; its working
    /// directory and environment come from the issuing terminal.
    Run {
        /// Split direction; omitted follows your `layout.new-pane-direction`
        /// setting.
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
    /// Run the router process.
    #[command(hide = true)]
    ServeRouter {
        /// Runtime directory to serve; defaults to this user's own.
        #[arg(long, value_name = "DIR")]
        runtime_dir: Option<PathBuf>,
        /// Wait for the router lock instead of yielding to the router that
        /// holds it. A router restarting into a newly installed binary passes
        /// this.
        #[arg(long)]
        wait_for_lock: bool,
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
        #[arg(long, value_name = "DIR")]
        runtime_dir: Option<PathBuf>,
        /// Open this profile's tabs and panes instead of one shell.
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,
        /// Let the other users of this machine reach this session, whatever
        /// `koshi.kdl` says.
        #[arg(long)]
        allow_other_users: bool,
        /// Come up from the state at this path instead of seeding a new
        /// session. The image being replaced wrote it; this one reads it once
        /// and removes it.
        #[arg(long, value_name = "PATH")]
        resume: Option<PathBuf>,
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
        token: String,
        /// Runtime directory to serve; defaults to this user's own.
        #[arg(long, value_name = "DIR")]
        runtime_dir: Option<PathBuf>,
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
        key: String,
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
        #[arg(long, value_parser = parse_session_ref, value_name = "SESSION")]
        session: Option<SessionRef>,
        /// How long the token works: a length such as `30s`, `15m`, `24h` or
        /// `7d`, or the word `never`.
        #[arg(long, value_parser = parse_expiry, value_name = "DURATION", default_value = "24h")]
        expires: Expiry,
    },
    /// Stop the tokens one identity holds.
    Revoke {
        /// Whose tokens stop working.
        #[arg(value_name = "IDENTITY")]
        identity: String,
        /// The one grant that stops working, named by the session it reaches,
        /// by id or name. Without this flag every grant that identity holds
        /// stops working.
        #[arg(long, value_parser = parse_session_ref, value_name = "SESSION")]
        session: Option<SessionRef>,
    },
    /// List the grants this machine has made.
    List {
        /// List only the grants that reach this one session, by id or name.
        /// A grant that reaches every session on this machine is listed here
        /// too.
        #[arg(long, value_parser = parse_session_ref, value_name = "SESSION")]
        session: Option<SessionRef>,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
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
        server: String,
    },
    /// List the servers this machine has saved.
    List {
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// Drop one saved server, so nothing on this machine holds its secret.
    Forget {
        /// Server to drop, by the name it was saved under or its address.
        #[arg(value_name = "SERVER")]
        server: String,
    },
    /// Replace the secret of one saved server, after the machine serving it
    /// granted a fresh one.
    SetSecret {
        /// Server whose secret is replaced, by the name it was saved under or
        /// its address.
        #[arg(value_name = "SERVER")]
        server: String,
    },
}

/// The `koshi debug` subcommands: read-only dumps for a bug report.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum DebugCommand {
    /// Print every running session's full record — its tabs, panes and
    /// clients — with each pane's command arguments hidden.
    DumpState {
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// Print each tab's split tree, the rectangles it solves to, the panes
    /// with no room, the stacks, and each client's focus.
    DumpLayout {
        /// Narrow the answer to one tab, by id or name.
        #[arg(long, value_parser = parse_tab_ref, value_name = "TAB")]
        tab: Option<TabRef>,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// Print the newest records from every local session log file.
    TailLog {
        /// Keep only records stamped within this much of now, e.g. `10m`, with
        /// their physical continuation lines.
        #[arg(long, value_parser = parse_since, value_name = "LENGTH")]
        since: Option<Duration>,
    },
    /// Print the events each running session published most recently, oldest
    /// first. Each line names the event and the ids it named, never any
    /// content it carried.
    Events {
        /// Keep only the events recorded within this much of now, e.g. `30s`,
        /// `5m`, `2h`, `7d`.
        #[arg(long, value_parser = parse_since, value_name = "LENGTH")]
        since: Option<Duration>,
        /// Keep only the events whose name contains this text, matched
        /// ignoring case, e.g. `pane` or `TabMoved`.
        #[arg(long, value_parser = parse_event_filter, value_name = "NAME")]
        filter: Option<String>,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
}

/// Which keymap layer authored a binding, as typed on the command line.
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
/// renders locally from the built-in defaults plus the user's keybinding file.
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
/// koshi prints it (`<kind>-<uuid>`) or as a bare UUID; a session or a tab
/// takes its display name too.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum InspectTarget {
    /// Report a session: name, creation time, clients, and pane count.
    Session {
        /// Session to inspect, by id or name.
        #[arg(value_parser = parse_session_ref, value_name = "SESSION")]
        session: SessionRef,
        /// Output format.
        #[arg(long, value_enum, value_name = "FORMAT", default_value = "table")]
        format: FormatArg,
    },
    /// Report a tab: name, position, active pane, and pane count.
    Tab {
        /// Tab to inspect, by id or name.
        #[arg(value_parser = parse_tab_ref, value_name = "TAB")]
        tab: TabRef,
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
    /// flag given directly as an id is used as-is.
    ///
    /// `new_pane_direction` is this CLI's own `layout.new-pane-direction`
    /// setting, read from `koshi.kdl` by
    /// [`config::new_pane_direction`](koshi_link::config::new_pane_direction). A
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
    pub fn to_action(
        &self,
        targets: &ResolvedTargets,
        new_pane_direction: Direction,
    ) -> Option<(ActionRef, Command)> {
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
                    direction: direction.map(Direction::from).unwrap_or(new_pane_direction),
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
            CliCommand::TogglePaneFullscreen { client: _ } => {
                ("toggle-pane-fullscreen", Command::TogglePaneFullscreen)
            }
            CliCommand::Input {
                text,
                pane,
                no_enter,
            } => {
                // The text alone sits at the shell prompt; the text plus `\r`,
                // the byte the Enter key sends, runs as a line.
                let mut data = text.clone().into_bytes();
                if !no_enter {
                    data.push(b'\r');
                }
                (
                    "write-to-pane",
                    Command::WriteToPane(WriteToPaneArgs { pane: *pane, data }),
                )
            }
            CliCommand::NewTab { session: _, client } => (
                "new-tab",
                Command::NewTab(NewTabArgs {
                    cwd: None,
                    client: *client,
                }),
            ),
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
            CliCommand::MoveTab { index, tab } => (
                "move-tab",
                Command::MoveTab(MoveTabArgs {
                    tab: targets.tab.or(tab_ref_id(tab)),
                    index: *index,
                }),
            ),
            CliCommand::FocusTab { index, tab, client } => {
                // The parser enforces exactly one of the two flags, and the
                // routing layer resolves a `--tab` name to its id.
                let target = match (index, targets.tab.or(tab_ref_id(tab))) {
                    (Some(index), None) => TabTarget::Index(*index),
                    (None, Some(tab)) => TabTarget::Id(tab),
                    _ => unreachable!(
                        "clap enforces exactly one of --index/--tab, and routing resolves a tab name"
                    ),
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
                    direction: direction.map(Direction::from).unwrap_or(new_pane_direction),
                    stacked: *stacked,
                    client: *client,
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
            | CliCommand::NewTab { session, .. }
            | CliCommand::CloseTab { session, .. } => session.as_ref(),
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
            | CliCommand::CloseTab { tab, .. }
            | CliCommand::MoveTab { tab, .. }
            | CliCommand::FocusTab { tab, .. } => tab.as_ref(),
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
            | CliCommand::NewTab { client, .. }
            | CliCommand::NextTab { client }
            | CliCommand::PreviousTab { client }
            | CliCommand::FocusTab { client, .. }
            | CliCommand::FocusPane { client, .. }
            | CliCommand::Lock { client }
            | CliCommand::Unlock { client }
            | CliCommand::ToggleLock { client }
            | CliCommand::TogglePaneFullscreen { client } => *client,
            _ => None,
        }
    }

    /// The client this invocation names that no [`Command`] carries, so it
    /// rides on the command's source instead
    /// ([`CommandSource::external_cli`](koshi_core::command::CommandSource::external_cli)).
    /// Only `toggle-pane-fullscreen` has one: every other client-taking verb
    /// puts its client in the command's own arguments, which travel on both
    /// routes.
    /// [`CommandSource::InSessionCli`](koshi_core::command::CommandSource::InSessionCli)
    /// has no field to carry this one, so a command with one here never takes
    /// the in-session route ([`crate::targeting::route`]).
    #[must_use]
    pub fn source_client(&self) -> Option<ClientId> {
        match self {
            CliCommand::TogglePaneFullscreen { client } => *client,
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

/// Build the [`SpawnSpec`] for a `run` invocation's trailing argv: the first
/// token is the program, the rest its arguments. The working directory and
/// environment stay empty — they are filled from the issuing terminal when
/// the command is sent.
///
/// Panics when `argv` is empty.
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

// Each id parser takes the id exactly as koshi prints it (`<prefix>-<uuid>`)
// or as a bare UUID. A value carrying another kind's prefix is rejected:
// `parse_pane_id("tab-<uuid>")` is an error, not a pane id.

/// Parse a session id argument into a [`SessionId`].
fn parse_session_id(value: &str) -> Result<SessionId, String> {
    parse_prefixed_uuid(value, "session").map(SessionId::from_uuid)
}

/// Parse a `--pane` flag value into a [`PaneId`].
fn parse_pane_id(value: &str) -> Result<PaneId, String> {
    parse_prefixed_uuid(value, "pane").map(PaneId::from_uuid)
}

/// Parse a `--client` flag value into a [`ClientId`].
fn parse_client_id(value: &str) -> Result<ClientId, String> {
    parse_prefixed_uuid(value, "client").map(ClientId::from_uuid)
}

#[cfg(test)]
mod tests;
