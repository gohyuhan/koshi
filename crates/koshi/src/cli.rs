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
//! Action subcommands carry typed arguments and map to the core command vocabulary through
//! [`CliCommand::build_action_command`](crate::cli::CliCommand::build_action_command), which pairs
//! each with its `core:` action reference. Entity ids are parsed at this boundary: a flag accepts
//! the id exactly as koshi prints it (`pane-<uuid>`) or as a bare UUID. A session or tab argument
//! accepts the display name too: a value that reads as an id (`session-<uuid>`, `tab-<uuid>`, or a
//! bare UUID) is that id, anything else is a name.

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::time::Duration;

use clap::builder::PossibleValuesParser;
use clap::{Parser, Subcommand, ValueEnum};
use koshi_core::action::ActionReference;
use koshi_core::command::{
    ClosePaneArgs, CloseTabArgs, Command, FocusPaneArgs, FocusTabArgs, FocusTarget, LockModeArgs,
    MoveFloatingPaneArgs, MovePaneArgs, MoveTabArgs, NewPaneArgs, NewPanePlacement, NewTabArgs,
    PanePlacementAnchor, PanePlacementTarget, PlacePaneArgs, ResizePaneArgs, ScrollPaneArgs,
    SetAllFloatingPanesMinimizedArgs, SetPaneMinimizedArgs, SetPanePinnedArgs, TabTarget,
    TiledPlacementTarget, ToggleLockModeArgs, WriteToPaneArgs,
};
use koshi_core::geometry::{
    AxisPercent, Direction, FloatingPaneDimension, FloatingPaneSize, Point,
};
use koshi_core::ids::{parse_owned_session_id, parse_prefixed_uuid, OwnedSessionId};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
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
    /// creates, whatever `koshi.kdl` says. Only with `--headless`. macOS and
    /// Linux only: refused on Windows.
    #[arg(long = "allow-other-users", requires = "is_headless")]
    pub should_allow_other_users: bool,

    /// Launch with a named profile: read `profile/<name>.kdl` from the config
    /// directory and open its tabs and panes instead of a single shell.
    #[arg(long = "profile", value_name = "NAME")]
    pub profile_name: Option<String>,

    /// Run this invocation against the machine SERVER names — the name it was
    /// saved under, or the `host:port` it listens on — instead of this one.
    #[arg(
        long = "remote",
        global = true,
        value_name = "SERVER",
        display_order = 997
    )]
    pub remote_server_reference: Option<String>,

    /// The fingerprint of the certificate the server `--remote` names must
    /// present: 64 lowercase hex characters, as `koshi share grant` prints it
    /// on that machine. Needed for a server this machine has not saved, and
    /// refused for a server it has saved.
    #[arg(
        long = "certificate-fingerprint",
        global = true,
        requires = "remote_server_reference",
        value_name = "FINGERPRINT",
        display_order = 998
    )]
    pub certificate_fingerprint: Option<String>,

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
/// UUID), `<user id>/<session id>`, or a display name to look up against the
/// running sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionReference {
    /// An exact session id.
    SessionId(SessionId),
    /// An exact session id in the folder of another user of this machine, such
    /// as `1001/session-<uuid>`.
    OwnedSessionId(OwnedSessionId),
    /// A display name; it must match exactly one running session.
    SessionName(String),
}

impl fmt::Display for SessionReference {
    /// Writes the reference as the user named it: the session id, the user id
    /// and the session id joined by `/`, or the display name.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionReference::SessionId(session_id) => session_id.fmt(formatter),
            SessionReference::OwnedSessionId(owned_session_id) => owned_session_id.fmt(formatter),
            SessionReference::SessionName(session_name) => formatter.write_str(session_name),
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

/// Parse a session argument: `<user id>/<session id>` for a value with a `/`,
/// read by [`parse_owned_session_id`], an id when the value reads as one, else
/// a display name. An empty value is `Err("expected a session id or name")`,
/// and a value with a `/` that is not that form is the error
/// [`parse_owned_session_id`] gives.
pub fn parse_session_reference(session_argument: &str) -> Result<SessionReference, String> {
    if session_argument.is_empty() {
        return Err("expected a session id or name".to_string());
    }
    if let Some(owned_session_id) = parse_owned_session_id(session_argument)? {
        return Ok(SessionReference::OwnedSessionId(owned_session_id));
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
/// (a name looked up against the running sessions), and the pane a pane verb
/// given no `--pane` acts on. The routing layer builds this before
/// [`CliCommand::build_action_command`]; a verb without those flags takes
/// `default()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResolvedTargets {
    /// The resolved `--session` value.
    pub session_id: Option<SessionId>,
    /// The resolved `--tab` value, or the destination tab the routing layer
    /// reads for a bare `dock-pane`.
    pub tab_id: Option<TabId>,
    /// The pane a pane verb given no `--pane` acts on: the pane this CLI runs
    /// in, when the routing layer sends the command to that pane's own session
    /// as an external command because it names `--client`. `None` on every
    /// other route.
    pub pane_id: Option<PaneId>,
}

/// The output format of a discovery query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable aligned columns.
    Table,
    /// Machine-readable JSON.
    Json,
}

/// The flags `new-pane` and `run` share: where the new pane opens, and the
/// client that shows it.
#[derive(Debug, PartialEq, Eq, clap::Args)]
pub struct NewPaneFlags {
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
    /// Open the pane floating above the tiled panes, in no tab. The client
    /// that `--client` names, else the issuing client, else the session's only
    /// attached client, shows and focuses it when that client can draw
    /// floating panes; every other client holds it minimized. Refused with
    /// `--direction`, `--stacked`, `--pane` and `--tab`.
    #[arg(
        long = "floating",
        conflicts_with_all = ["direction", "should_stack", "pane_id", "tab_reference"]
    )]
    should_float: bool,
    /// Pin the floating pane at its top-left cell on the client that shows it.
    #[arg(long = "pinned", requires = "should_float")]
    should_pin: bool,
    /// Column of the floating pane's top-left cell, counted from the left edge
    /// of the pane area; needs `--y`. Omitted takes the default placement.
    #[arg(long = "x", value_name = "COLUMN", requires_all = ["should_float", "top_left_row"])]
    top_left_column: Option<u16>,
    /// Row of the floating pane's top-left cell, counted from the top edge of
    /// the pane area; needs `--x`.
    #[arg(long = "y", value_name = "ROW", requires_all = ["should_float", "top_left_column"])]
    top_left_row: Option<u16>,
    /// Width of the floating pane: columns (`80`) or a percent of the pane area
    /// (`60%`); needs `--rows`. Omitted takes 60% of each axis.
    #[arg(
        long = "cols",
        value_parser = parse_floating_pane_dimension,
        value_name = "COLUMNS",
        requires_all = ["should_float", "floating_pane_height"]
    )]
    floating_pane_width: Option<FloatingPaneDimension>,
    /// Height of the floating pane: rows (`20`) or a percent of the pane area
    /// (`60%`); needs `--cols`.
    #[arg(
        long = "rows",
        value_parser = parse_floating_pane_dimension,
        value_name = "ROWS",
        requires_all = ["should_float", "floating_pane_width"]
    )]
    floating_pane_height: Option<FloatingPaneDimension>,
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
    /// Client that shows and focuses the new pane. Outside koshi with no
    /// `--pane` or `--tab`, the new pane splits that client's focused pane.
    /// Without it, the issuing client shows and focuses the new pane. Outside
    /// koshi without it, the new pane moves no focus on a tab that a client
    /// already shows, and the session's only attached client shows and
    /// focuses it on a tab that no client shows.
    #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
    client_id: Option<ClientId>,
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
/// carry `--format` and render through [`crate::output`].
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
        /// Name to save a server reached for the first time under: `--remote
        /// NAME` then reaches it. A server already saved refuses this flag.
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
    /// Open a new pane running a shell; it starts in the current working
    /// directory of the pane it splits from, with the environment the session
    /// process started with plus koshi's own variables.
    NewPane {
        #[command(flatten)]
        new_pane_flags: NewPaneFlags,
    },
    /// Close a pane.
    ClosePane {
        /// Pane to close; defaults to the focused pane.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: Option<PaneId>,
        /// Kill the pane's child immediately instead of asking it to stop first.
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
        /// Client whose view of a floating pane keeps the edge opposite the
        /// moved border in place; defaults to the issuing client, else the
        /// session's only attached one.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Swap a tiled pane with its visible neighbor in one step, with no
    /// preview and no confirm. With `--x` and `--y` instead, move a floating
    /// pane on one client's screen.
    MovePane {
        /// Direction in which to choose the visible neighbor.
        #[arg(
            long,
            value_enum,
            value_name = "DIRECTION",
            required_unless_present = "top_left_column",
            conflicts_with_all = ["top_left_column", "client_id"]
        )]
        direction: Option<DirectionArgument>,
        /// Pane to move; defaults to the focused pane. Required with `--x`.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: Option<PaneId>,
        /// Column of the floating pane's new top-left cell, counted from the
        /// left edge of the pane area; needs `--y` and `--pane`.
        #[arg(long = "x", value_name = "COLUMN", requires_all = ["top_left_row", "pane_id"])]
        top_left_column: Option<u16>,
        /// Row of the floating pane's new top-left cell, counted from the top
        /// edge of the pane area; needs `--x`.
        #[arg(long = "y", value_name = "ROW", requires = "top_left_column")]
        top_left_row: Option<u16>,
        /// Client whose view of the floating pane moves; defaults to the
        /// issuing client, else the session's only attached one. Only with
        /// `--x`.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Insert a pane into a tab's tiled layout: on one side of the whole tab,
    /// or into the stack of another pane. A floating pane docks.
    PlacePane {
        /// Pane to place; defaults to the pane this command runs in, else the
        /// focused pane.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: Option<PaneId>,
        /// Destination tab, by id or name.
        #[arg(
            long = "tab",
            value_parser = parse_tab_reference,
            value_name = "TAB",
            required_unless_present = "stack_onto_pane_id",
            conflicts_with = "stack_onto_pane_id"
        )]
        tab_reference: Option<TabReference>,
        /// Side of the destination tab where the pane lands.
        #[arg(
            long,
            value_enum,
            value_name = "DIRECTION",
            required_unless_present = "stack_onto_pane_id",
            conflicts_with = "stack_onto_pane_id"
        )]
        direction: Option<DirectionArgument>,
        /// Tiled pane whose stack the pane joins as the open member; a pane in
        /// no stack forms a new stack with it.
        #[arg(long = "stack-onto", value_parser = parse_pane_id, value_name = "PANE_ID")]
        stack_onto_pane_id: Option<PaneId>,
        /// Client whose committed view supplies the destination sizing.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Float a tiled or stacked pane above the tiled panes, at the top-left
    /// cell and the size where the client draws it, each axis cut to 90% of
    /// the smallest pane area that an attached client draws floating panes in.
    UndockPane {
        /// Pane to float; defaults to the pane this command runs in, else the
        /// focused pane.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: Option<PaneId>,
        /// Column of the floating pane's top-left cell, counted from the left
        /// edge of the pane area; needs `--y`.
        #[arg(long = "x", value_name = "COLUMN", requires = "top_left_row")]
        top_left_column: Option<u16>,
        /// Row of the floating pane's top-left cell, counted from the top edge
        /// of the pane area; needs `--x`.
        #[arg(long = "y", value_name = "ROW", requires = "top_left_column")]
        top_left_row: Option<u16>,
        /// Client that shows and focuses the floating pane when it can draw
        /// floating panes; defaults to the issuing client, else the session's
        /// only attached one.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Dock a floating pane into a tab's tiled layout: on one side of the
    /// whole tab, or into the stack of a tiled pane. A tiled pane moves there.
    DockPane {
        /// Floating pane to dock; defaults to the pane this command runs in, else
        /// the focused pane.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: Option<PaneId>,
        /// Destination tab, by id or name; defaults to the tab the `--client`
        /// client views, else the tab of the tiled pane this command runs in,
        /// else the tab the issuing client or the session's only attached
        /// client views, else the session's only tab.
        #[arg(
            long = "tab",
            value_parser = parse_tab_reference,
            value_name = "TAB",
            conflicts_with = "stack_onto_pane_id"
        )]
        tab_reference: Option<TabReference>,
        /// Side of the destination tab where the pane lands; defaults to
        /// `right`.
        #[arg(
            long,
            value_enum,
            value_name = "DIRECTION",
            conflicts_with = "stack_onto_pane_id"
        )]
        direction: Option<DirectionArgument>,
        /// Tiled pane whose stack the pane joins as the open member; a pane in
        /// no stack forms a new stack with it.
        #[arg(long = "stack-onto", value_parser = parse_pane_id, value_name = "PANE_ID")]
        stack_onto_pane_id: Option<PaneId>,
        /// Client that moves to the destination tab; defaults to the issuing
        /// client, else the session's only attached one.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Pin a floating pane on one client's screen: it stays at its top-left
    /// cell, and the moves and the left or top edge resizes of that client
    /// are refused.
    PinPane {
        /// Floating pane to pin; defaults to the pane this command runs in, else
        /// the focused floating pane.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: Option<PaneId>,
        /// Client whose view of the pane is pinned; defaults to the issuing
        /// client, else the session's only attached one.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Unpin a floating pane on one client's screen; it stays at its pinned
    /// cell.
    UnpinPane {
        /// Floating pane to unpin; defaults to the pane this command runs in,
        /// else the focused floating pane.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: Option<PaneId>,
        /// Client whose view of the pane is unpinned; defaults to the issuing
        /// client, else the session's only attached one.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Minimize a floating pane on one client's screen, or with `--all` every
    /// floating pane that client shows.
    MinimizePane {
        /// Floating pane to minimize; defaults to the pane this command runs in,
        /// else the focused floating pane.
        #[arg(
            long = "pane",
            value_parser = parse_pane_id,
            value_name = "PANE_ID",
            conflicts_with = "should_minimize_all_floating_panes"
        )]
        pane_id: Option<PaneId>,
        /// Minimize every floating pane the client shows.
        #[arg(long = "all")]
        should_minimize_all_floating_panes: bool,
        /// Client whose view changes; defaults to the issuing client, else the
        /// session's only attached one.
        #[arg(long = "client", value_parser = parse_client_id, value_name = "CLIENT_ID")]
        client_id: Option<ClientId>,
    },
    /// Restore a minimized floating pane on one client's screen, or with
    /// `--all` every floating pane that client minimized. The last pane
    /// restored is drawn on top and focused.
    RestorePane {
        /// Floating pane to restore; defaults to the pane this command runs in,
        /// else the focused floating pane.
        #[arg(
            long = "pane",
            value_parser = parse_pane_id,
            value_name = "PANE_ID",
            conflicts_with = "should_restore_all_floating_panes"
        )]
        pane_id: Option<PaneId>,
        /// Restore every floating pane the client minimized.
        #[arg(long = "all")]
        should_restore_all_floating_panes: bool,
        /// Client whose view changes; defaults to the issuing client, else the
        /// session's only attached one.
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
        /// Text to type into the pane. Text that starts with `-`, such as `-la`,
        /// is typed as text, not read as a flag.
        #[arg(value_name = "TEXT", allow_hyphen_values = true)]
        input_text: String,
        /// Pane to type into; defaults to the focused pane.
        #[arg(long = "pane", value_parser = parse_pane_id, value_name = "PANE_ID")]
        pane_id: Option<PaneId>,
        /// Leave the text at the prompt instead of pressing Enter after it.
        #[arg(long = "no-enter")]
        should_leave_input_at_prompt: bool,
    },
    /// Open a new tab; its first pane starts in the current working directory
    /// of the focused pane of the terminal that switches onto the tab, with the
    /// environment the session process started with plus koshi's own
    /// variables.
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
        /// Kill every pane's child immediately instead of asking it to stop first.
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
    /// Focus a pane by id, or the next or previous floating pane.
    FocusPane {
        /// Pane to focus.
        #[arg(
            long = "pane",
            value_parser = parse_pane_id,
            value_name = "PANE_ID",
            required_unless_present_any = [
                "should_focus_next_floating_pane",
                "should_focus_previous_floating_pane"
            ],
            conflicts_with_all = [
                "should_focus_next_floating_pane",
                "should_focus_previous_floating_pane"
            ]
        )]
        pane_id: Option<PaneId>,
        /// Show and focus the floating pane after the focused one, in the order
        /// the panes became floating, wrapping past the last to the first.
        #[arg(
            long = "next-floating",
            conflicts_with = "should_focus_previous_floating_pane"
        )]
        should_focus_next_floating_pane: bool,
        /// Show and focus the floating pane before the focused one, in the
        /// order the panes became floating, wrapping past the first to the
        /// last.
        #[arg(long = "previous-floating")]
        should_focus_previous_floating_pane: bool,
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
    /// Grant, revoke and list remote access tokens, and open the port remote
    /// clients connect to.
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
    /// Install a newer koshi release the way this koshi was installed, then
    /// restart the running servers into the koshi program on disk. A Homebrew
    /// install runs `brew upgrade`; a build from source downloads nothing.
    Update,
    /// Restart the router and every running session into the koshi program on
    /// disk; every pane keeps running.
    RestartServers,
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
    /// Open a new pane running the command given after `--`; it starts in the
    /// current working directory of the pane it splits from, with the
    /// environment the session process started with plus koshi's own
    /// variables.
    Run {
        #[command(flatten)]
        new_pane_flags: NewPaneFlags,
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
        /// this, and so does a command that starts a second router once the
        /// first one it started ended with no router answering.
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
        /// `koshi.kdl` says. No effect on Windows.
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
/// granted, and the port remote clients connect to. Every verb asks the
/// router, which owns the token store and the port.
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
    /// Open the port remote clients connect to, on the address the router read
    /// from koshi.kdl, and print the commands that connect. Mints no token.
    Open,
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

/// The `koshi remote` subcommands: the servers this machine has saved. Every
/// verb reads or writes that store; none of them prints a saved secret.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum RemoteCommand {
    /// Save a server this machine can dial, asking for its name, its address,
    /// the fingerprint of its certificate and its secret in turn.
    New,
    /// Change what one saved server holds, asking for its name, its address,
    /// the fingerprint of its certificate and its secret in turn, with the
    /// current value kept on an empty answer.
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
    /// with no room, the stacks, each floating pane with what each client
    /// draws for it, and each client's focus.
    DumpLayout {
        /// Narrow the tabs to one tab, by id or name; every floating pane still
        /// prints.
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
}

/// The `koshi keys` subcommands: read-only keymap introspection. Every verb
/// renders locally from the built-in defaults plus the user's keybinding file.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum KeysCommand {
    /// List effective keybindings per mode.
    List {
        /// Limit the listing to one built-in input mode.
        #[arg(
            long = "mode",
            value_name = "MODE",
            value_parser = PossibleValuesParser::new(LockMode::ALL.map(LockMode::get_keymap_name))
        )]
        input_mode_name: Option<String>,
        /// Limit the listing to bindings authored by one layer.
        #[arg(long, value_enum, value_name = "SCOPE")]
        scope: Option<KeymapScope>,
        /// Output format.
        #[arg(
            long = "format",
            value_enum,
            value_name = "FORMAT",
            default_value = "table"
        )]
        output_format: OutputFormat,
    },
    /// Describe the binding on a key sequence and every longer binding that
    /// starts with it: action, source layer, and metadata.
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
            | CliCommand::PlacePane { .. }
            | CliCommand::UndockPane { .. }
            | CliCommand::DockPane { .. }
            | CliCommand::PinPane { .. }
            | CliCommand::UnpinPane { .. }
            | CliCommand::MinimizePane { .. }
            | CliCommand::RestorePane { .. }
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
            | CliCommand::Update
            | CliCommand::RestartServers
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
    /// flag given directly as an id is used as-is. A `--pane` flag wins over
    /// [`ResolvedTargets::pane_id`]: `resize-pane`, `scroll-pane`,
    /// `move-pane --direction`, `place-pane`, `undock-pane`, `dock-pane`,
    /// `pin-pane`, `unpin-pane`, `minimize-pane` and `restore-pane` given no
    /// `--pane` act on that pane. `--all` names no pane.
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
    /// `restart-servers`, `version`, `server-version`, `share`, `remote`, and the
    /// hidden `serve-router`, `serve-session`, `serve-pty-supervisor` and
    /// `resume-support`.
    #[must_use]
    pub fn build_action_command(
        &self,
        resolved_targets: &ResolvedTargets,
        new_pane_direction: Direction,
    ) -> Option<(ActionReference, Command)> {
        let (action_name, command) = match self {
            CliCommand::NewPane { new_pane_flags } => (
                "new-pane",
                Command::NewPane(new_pane_flags.build_new_pane_args(
                    resolved_targets.tab_id,
                    new_pane_direction,
                    None,
                )),
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
                client_id: _,
            } => (
                "resize-pane",
                Command::ResizePane(ResizePaneArgs {
                    pane_id: pane_id.or(resolved_targets.pane_id),
                    direction: Direction::from(*direction),
                    resize_amount_cells: *resize_amount_cells,
                }),
            ),
            CliCommand::MovePane {
                direction,
                pane_id,
                top_left_column,
                top_left_row,
                client_id: _,
            } => {
                // The parser takes either `--direction`, or `--x`, `--y` and
                // `--pane` together.
                match (direction, top_left_column, top_left_row, pane_id) {
                    (Some(direction), _, _, _) => (
                        "move-pane",
                        Command::MovePane(MovePaneArgs {
                            pane_id: pane_id.or(resolved_targets.pane_id),
                            direction: Direction::from(*direction),
                        }),
                    ),
                    (None, Some(column), Some(row), Some(pane_id)) => (
                        "move-floating-pane",
                        Command::MoveFloatingPane(MoveFloatingPaneArgs {
                            pane_id: *pane_id,
                            top_left_cell: Point {
                                column: *column,
                                row: *row,
                            },
                        }),
                    ),
                    _ => unreachable!("clap requires --direction, or --x, --y and --pane"),
                }
            }
            CliCommand::PlacePane {
                pane_id,
                tab_reference,
                direction,
                stack_onto_pane_id,
                client_id: _,
            } => (
                "place-pane",
                Command::PlacePane(PlacePaneArgs {
                    source_pane_id: pane_id.or(resolved_targets.pane_id),
                    placement_target: PanePlacementTarget::Tiled(build_tiled_placement_target(
                        *stack_onto_pane_id,
                        resolved_targets
                            .tab_id
                            .or(resolve_tab_reference_id(tab_reference)),
                        direction.map(Direction::from),
                    )),
                }),
            ),
            CliCommand::UndockPane {
                pane_id,
                top_left_column,
                top_left_row,
                client_id: _,
            } => {
                let top_left_cell = top_left_column
                    .zip(*top_left_row)
                    .map(|(column, row)| Point { column, row });
                (
                    "undock-pane",
                    Command::PlacePane(PlacePaneArgs {
                        source_pane_id: pane_id.or(resolved_targets.pane_id),
                        placement_target: PanePlacementTarget::Floating { top_left_cell },
                    }),
                )
            }
            CliCommand::DockPane {
                pane_id,
                tab_reference,
                direction,
                stack_onto_pane_id,
                client_id: _,
            } => (
                "dock-pane",
                Command::PlacePane(PlacePaneArgs {
                    source_pane_id: pane_id.or(resolved_targets.pane_id),
                    placement_target: PanePlacementTarget::Tiled(build_tiled_placement_target(
                        *stack_onto_pane_id,
                        resolved_targets
                            .tab_id
                            .or(resolve_tab_reference_id(tab_reference)),
                        Some(direction.map(Direction::from).unwrap_or(Direction::Right)),
                    )),
                }),
            ),
            CliCommand::PinPane {
                pane_id,
                client_id: _,
            } => (
                "pin-pane",
                Command::SetPanePinned(SetPanePinnedArgs {
                    pane_id: pane_id.or(resolved_targets.pane_id),
                    is_pinned: true,
                }),
            ),
            CliCommand::UnpinPane {
                pane_id,
                client_id: _,
            } => (
                "unpin-pane",
                Command::SetPanePinned(SetPanePinnedArgs {
                    pane_id: pane_id.or(resolved_targets.pane_id),
                    is_pinned: false,
                }),
            ),
            CliCommand::MinimizePane {
                pane_id,
                should_minimize_all_floating_panes,
                client_id: _,
            } => build_floating_pane_minimize_action(
                pane_id.or(resolved_targets.pane_id),
                *should_minimize_all_floating_panes,
                true,
            ),
            CliCommand::RestorePane {
                pane_id,
                should_restore_all_floating_panes,
                client_id: _,
            } => build_floating_pane_minimize_action(
                pane_id.or(resolved_targets.pane_id),
                *should_restore_all_floating_panes,
                false,
            ),
            CliCommand::ScrollPane {
                scroll_line_count,
                pane_id,
                client_id: _,
            } => (
                "scroll-pane",
                Command::ScrollPane(ScrollPaneArgs {
                    pane_id: pane_id.or(resolved_targets.pane_id),
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
                let mut pane_input_bytes = input_text.clone().into_bytes();
                if !should_leave_input_at_prompt {
                    pane_input_bytes.push(b'\r');
                }
                (
                    "write-to-pane",
                    Command::WriteToPane(WriteToPaneArgs {
                        pane_id: *pane_id,
                        pane_input_bytes,
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
                    focus_target: TabTarget::Previous,
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
            CliCommand::FocusPane {
                pane_id,
                should_focus_next_floating_pane,
                should_focus_previous_floating_pane,
                client_id,
            } => {
                // The parser takes exactly one of `--pane`, `--next-floating`
                // and `--previous-floating`.
                let (action_name, focus_target) = match (
                    pane_id,
                    should_focus_next_floating_pane,
                    should_focus_previous_floating_pane,
                ) {
                    (Some(pane_id), false, false) => ("focus-pane", FocusTarget::Pane(*pane_id)),
                    (None, true, false) => {
                        ("focus-next-floating-pane", FocusTarget::NextFloatingPane)
                    }
                    (None, false, true) => (
                        "focus-previous-floating-pane",
                        FocusTarget::PreviousFloatingPane,
                    ),
                    _ => unreachable!(
                        "clap takes exactly one of --pane, --next-floating and --previous-floating"
                    ),
                };
                (
                    action_name,
                    Command::FocusPane(FocusPaneArgs {
                        focus_target,
                        client_id: *client_id,
                    }),
                )
            }
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
                new_pane_flags,
                command_arguments,
            } => (
                "run",
                Command::NewPane(new_pane_flags.build_new_pane_args(
                    resolved_targets.tab_id,
                    new_pane_direction,
                    Some(build_spawn_spec_from_arguments(command_arguments)),
                )),
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
            | CliCommand::Update
            | CliCommand::RestartServers
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
            CliCommand::NewPane { new_pane_flags } | CliCommand::Run { new_pane_flags, .. } => {
                new_pane_flags.session_reference.as_ref()
            }
            CliCommand::NewTab {
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
            CliCommand::NewPane { new_pane_flags } | CliCommand::Run { new_pane_flags, .. } => {
                new_pane_flags.tab_reference.as_ref()
            }
            CliCommand::CloseTab { tab_reference, .. }
            | CliCommand::MoveTab { tab_reference, .. }
            | CliCommand::FocusTab { tab_reference, .. }
            | CliCommand::PlacePane { tab_reference, .. }
            | CliCommand::DockPane { tab_reference, .. } => tab_reference.as_ref(),
            _ => None,
        }
    }

    /// The explicit pane this invocation names, for the verbs that take one.
    /// The routing layer reads it to find the session owning that pane.
    #[must_use]
    pub fn get_target_pane_id(&self) -> Option<PaneId> {
        match self {
            CliCommand::NewPane { new_pane_flags } | CliCommand::Run { new_pane_flags, .. } => {
                new_pane_flags.pane_id
            }
            CliCommand::ClosePane { pane_id, .. }
            | CliCommand::ResizePane { pane_id, .. }
            | CliCommand::ScrollPane { pane_id, .. }
            | CliCommand::Input { pane_id, .. }
            | CliCommand::MovePane { pane_id, .. }
            | CliCommand::UndockPane { pane_id, .. }
            | CliCommand::PinPane { pane_id, .. }
            | CliCommand::UnpinPane { pane_id, .. }
            | CliCommand::MinimizePane { pane_id, .. }
            | CliCommand::RestorePane { pane_id, .. }
            | CliCommand::FocusPane { pane_id, .. } => *pane_id,
            CliCommand::PlacePane {
                pane_id,
                stack_onto_pane_id,
                ..
            }
            | CliCommand::DockPane {
                pane_id,
                stack_onto_pane_id,
                ..
            } => pane_id.or(*stack_onto_pane_id),
            _ => None,
        }
    }

    /// The explicit client this invocation names, for the verbs that take
    /// one. The routing layer reads it to find the session that client is
    /// attached to.
    #[must_use]
    pub fn get_target_client_id(&self) -> Option<ClientId> {
        match self {
            CliCommand::NewPane { new_pane_flags } | CliCommand::Run { new_pane_flags, .. } => {
                new_pane_flags.client_id
            }
            CliCommand::NewTab { client_id, .. }
            | CliCommand::NextTab { client_id }
            | CliCommand::PreviousTab { client_id }
            | CliCommand::FocusTab { client_id, .. }
            | CliCommand::FocusPane { client_id, .. }
            | CliCommand::Lock { client_id }
            | CliCommand::Unlock { client_id }
            | CliCommand::ToggleLock { client_id }
            | CliCommand::TogglePaneFullscreen { client_id }
            | CliCommand::ScrollPane { client_id, .. }
            | CliCommand::ResizePane { client_id, .. }
            | CliCommand::MovePane { client_id, .. }
            | CliCommand::PlacePane { client_id, .. }
            | CliCommand::UndockPane { client_id, .. }
            | CliCommand::DockPane { client_id, .. }
            | CliCommand::PinPane { client_id, .. }
            | CliCommand::UnpinPane { client_id, .. }
            | CliCommand::MinimizePane { client_id, .. }
            | CliCommand::RestorePane { client_id, .. } => *client_id,
            _ => None,
        }
    }

    /// The client this invocation names that no [`Command`] carries; it rides
    /// on the command's source instead
    /// ([`CommandSource::ExternalCli`](koshi_core::command::CommandSource::ExternalCli)).
    /// `toggle-pane-fullscreen`, `scroll-pane`, `resize-pane`, `move-pane`,
    /// `place-pane`, `undock-pane`, `dock-pane`, `pin-pane`, `unpin-pane`,
    /// `minimize-pane` and `restore-pane` answer their `--client` value; every
    /// other verb answers `None`, and a client-taking one puts its client in
    /// the command's own arguments, which travel on both routes.
    /// [`CommandSource::InSessionCli`](koshi_core::command::CommandSource::InSessionCli)
    /// carries no client, and a command with one here never takes the
    /// in-session route ([`crate::targeting::resolve_command_route`]).
    #[must_use]
    pub fn get_source_client_id(&self) -> Option<ClientId> {
        match self {
            CliCommand::TogglePaneFullscreen { client_id }
            | CliCommand::ScrollPane { client_id, .. }
            | CliCommand::ResizePane { client_id, .. }
            | CliCommand::MovePane { client_id, .. }
            | CliCommand::PlacePane { client_id, .. }
            | CliCommand::UndockPane { client_id, .. }
            | CliCommand::DockPane { client_id, .. }
            | CliCommand::PinPane { client_id, .. }
            | CliCommand::UnpinPane { client_id, .. }
            | CliCommand::MinimizePane { client_id, .. }
            | CliCommand::RestorePane { client_id, .. } => *client_id,
            _ => None,
        }
    }

    /// Whether this invocation docks a pane into a tab it names neither by
    /// `--tab` nor by `--stack-onto`: a bare `dock-pane`. The routing layer then
    /// reads the destination tab from the target session's overview.
    #[must_use]
    pub fn is_bare_dock_pane(&self) -> bool {
        matches!(
            self,
            CliCommand::DockPane {
                tab_reference: None,
                stack_onto_pane_id: None,
                ..
            }
        )
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

/// The tiled destination of `place-pane` and `dock-pane`: the stack holding
/// `stack_onto_pane_id` when it is set, else the `direction` side of the whole
/// tab `destination_tab_id`.
///
/// Panics when `stack_onto_pane_id` is `None` and `destination_tab_id` or
/// `direction` is `None`; the parser and the routing layer give both.
fn build_tiled_placement_target(
    stack_onto_pane_id: Option<PaneId>,
    destination_tab_id: Option<TabId>,
    direction: Option<Direction>,
) -> TiledPlacementTarget {
    match stack_onto_pane_id {
        Some(target_pane_id) => TiledPlacementTarget::Stack { target_pane_id },
        None => TiledPlacementTarget::Split {
            destination_tab_id: destination_tab_id
                .expect("clap or target routing gives --tab without --stack-onto"),
            anchor: PanePlacementAnchor::Tab,
            direction: direction.expect("clap or dock-pane's default gives --direction"),
        },
    }
}

/// The action name and command of `minimize-pane` (`is_minimized: true`) and
/// `restore-pane` (`false`). With `should_apply_to_all_floating_panes`:
/// `minimize-all-floating-panes` or `restore-all-floating-panes` with
/// [`Command::SetAllFloatingPanesMinimized`]. Else `minimize-pane` or
/// `restore-pane` with [`Command::SetPaneMinimized`] for `pane_id`.
fn build_floating_pane_minimize_action(
    pane_id: Option<PaneId>,
    should_apply_to_all_floating_panes: bool,
    is_minimized: bool,
) -> (&'static str, Command) {
    if should_apply_to_all_floating_panes {
        let action_name = if is_minimized {
            "minimize-all-floating-panes"
        } else {
            "restore-all-floating-panes"
        };
        return (
            action_name,
            Command::SetAllFloatingPanesMinimized(SetAllFloatingPanesMinimizedArgs {
                is_minimized,
            }),
        );
    }
    let action_name = if is_minimized {
        "minimize-pane"
    } else {
        "restore-pane"
    };
    (
        action_name,
        Command::SetPaneMinimized(SetPaneMinimizedArgs {
            pane_id,
            is_minimized,
        }),
    )
}

/// The id inside a `--tab` flag given directly as one; a name (or no flag)
/// yields `None` and needs the routing layer's lookup.
fn resolve_tab_reference_id(tab_reference: &Option<TabReference>) -> Option<TabId> {
    match tab_reference {
        Some(TabReference::TabId(tab_id)) => Some(*tab_id),
        _ => None,
    }
}

impl NewPaneFlags {
    /// The `NewPane` arguments these flags ask for, running `spawn_spec`, or
    /// the shell when it is `None`.
    ///
    /// With `--floating`, the new pane floats: at `--x`/`--y` when given, at
    /// `--cols` by `--rows` when given, pinned with `--pinned`. Else it joins
    /// the stack of `--pane` with `--stacked`, else `--pane` splits toward
    /// `--direction`, or toward `new_pane_direction` when `--direction` is
    /// absent. The tab is `resolved_tab_id`, else the id a `--tab` flag gives
    /// directly. The working directory stays empty.
    fn build_new_pane_args(
        &self,
        resolved_tab_id: Option<TabId>,
        new_pane_direction: Direction,
        spawn_spec: Option<SpawnSpec>,
    ) -> NewPaneArgs {
        let source_pane_id = self.pane_id;
        let tab_id = resolved_tab_id.or(resolve_tab_reference_id(&self.tab_reference));
        let placement = if self.should_float {
            let size = self
                .floating_pane_width
                .zip(self.floating_pane_height)
                .map(|(width, height)| FloatingPaneSize { width, height });
            let top_left_cell = self
                .top_left_column
                .zip(self.top_left_row)
                .map(|(column, row)| Point { column, row });
            NewPanePlacement::Floating {
                size,
                top_left_cell,
                is_pinned: self.should_pin,
            }
        } else if self.should_stack {
            NewPanePlacement::Stacked {
                source_pane_id,
                tab_id,
            }
        } else {
            NewPanePlacement::Split {
                source_pane_id,
                tab_id,
                direction: self
                    .direction
                    .map(Direction::from)
                    .unwrap_or(new_pane_direction),
            }
        };
        NewPaneArgs {
            placement,
            working_directory: None,
            spawn_spec,
            client_id: self.client_id,
        }
    }
}

/// Build the [`SpawnSpec`] for a `run` invocation's trailing argv: the first
/// token is the program, the rest its arguments. The working directory and
/// environment stay empty: the session starts the program in the current
/// working directory of the pane it splits from, with the environment the
/// session process started with plus koshi's own variables.
///
/// Panics when `command_arguments` is empty.
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

/// Parse a `--cols` or `--rows` value: a cell count from `1` to `65535`
/// (`80` → `Cells(80)`), or a percent from `1` to `100` followed by `%`
/// (`60%` → `Percent(60)`).
fn parse_floating_pane_dimension(
    dimension_argument: &str,
) -> Result<FloatingPaneDimension, String> {
    if let Some(percent_digits) = dimension_argument.strip_suffix('%') {
        return match percent_digits.parse::<u8>().map(AxisPercent::try_from) {
            Ok(Ok(axis_percent)) => Ok(FloatingPaneDimension::Percent(axis_percent)),
            _ => Err(format!(
                "`{dimension_argument}` is not a percent from 1% to 100%"
            )),
        };
    }
    dimension_argument
        .parse::<NonZeroU16>()
        .map(FloatingPaneDimension::Cells)
        .map_err(|_| {
            format!(
                "`{dimension_argument}` is not a size; give cells from 1 to 65535, like `80`, or a percent, like `60%`"
            )
        })
}

#[cfg(test)]
mod tests;
