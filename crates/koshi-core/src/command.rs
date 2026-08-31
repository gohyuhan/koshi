//! Canonical command vocabulary.
//!
//! [`Command`] and its nested enums are the single source of truth for every
//! requested mutation. These are pure data shells: no handlers, no behaviour,
//! no runtime state. Validation, target resolution, and execution all live in
//! higher layers (the session runtime); this module only names *what* may be
//! requested.
//!
//! Commands cross process boundaries (CLI IPC and plugins), so every variant
//! and arg struct holds only serde-friendly types that mean the same thing in
//! another process. No `Instant` — use `SystemTime` or epoch units for a
//! timestamp. No raw OS handles, no `&mut` references, and command identity is
//! never a free-form `String`.

use crate::event::{Event, RejectReason};
use crate::geometry::Direction;
use crate::ids::{ClientId, CommandId, PaneId, PluginId, SessionId, TabId};
use crate::process::SpawnSpec;
pub use crate::selection::{CopyTarget, GridPos, Selection, SelectionKind};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

/// A requested mutation the runtime can apply. One variant exists per command
/// the action registry can dispatch; [`Command::kind`] maps each variant to
/// its payload-free [`CommandKind`] discriminant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    /// Split-create a pane; CLI `new-pane`.
    NewPane(NewPaneArgs),
    /// Close a pane (defaults to the focused one).
    ClosePane(ClosePaneArgs),
    /// Move one of a pane's borders by whole cells: a positive size moves
    /// it outward (the pane grows), a negative size moves it inward (the
    /// pane shrinks and the neighbor gains the cells).
    ResizePane(ResizePaneArgs),
    /// Move focus to a pane.
    FocusPane(FocusPaneArgs),
    /// Create a new tab.
    NewTab(NewTabArgs),
    /// Close a tab (defaults to the focused one).
    CloseTab(CloseTabArgs),
    /// Move focus to a tab; next/prev/index all resolve to this.
    FocusTab(FocusTabArgs),
    /// Write raw bytes into a pane's input.
    WriteToPane(WriteToPaneArgs),
    /// Toggle the target client's lock (pass-through) mode.
    ToggleLockMode(ToggleLockModeArgs),
    /// Set the lock mode explicitly.
    SetLockMode(LockModeArgs),
    /// Toggle whether the acting client grabs the mouse for text selection.
    /// While on, a drag highlights in koshi even over a program that asked
    /// for the mouse.
    ToggleMouseSelect,
    /// Spawn a command in a new pane.
    RunCommandPane(RunCommandPaneArgs),
    /// Selection and copy — the commands of visual mode.
    Visual(VisualCommand),
    /// Plugin lifecycle management.
    Plugin(PluginCommand),
    /// Toggle fullscreen for the focused pane.
    TogglePaneFullscreen,
    /// Move a tab to a new index.
    MoveTab(MoveTabArgs),
    /// Prompt the issuing client to quit the client or session.
    Quit,
    /// Detach one client from the session. The session keeps running and its
    /// panes are untouched.
    Detach(DetachArgs),
    /// Detach every client attached to this session. The session keeps running
    /// and its panes are untouched.
    DetachAll,
    /// Move a client out of this session and into another one.
    SwitchSession(SwitchSessionArgs),
}

/// The payload-free discriminant of a [`Command`] — one unit variant per
/// `Command` variant, in the same order.
///
/// The action registry ([`crate::action`]) routes a user-facing action to a
/// core command by naming its `CommandKind`; the dispatcher then rebuilds the
/// full typed `Command` from that kind plus resolved targets and args.
/// [`Command::kind`] maps the other way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CommandKind {
    /// Discriminant of [`Command::NewPane`].
    NewPane,
    /// Discriminant of [`Command::ClosePane`].
    ClosePane,
    /// Discriminant of [`Command::ResizePane`].
    ResizePane,
    /// Discriminant of [`Command::FocusPane`].
    FocusPane,
    /// Discriminant of [`Command::NewTab`].
    NewTab,
    /// Discriminant of [`Command::CloseTab`].
    CloseTab,
    /// Discriminant of [`Command::FocusTab`].
    FocusTab,
    /// Discriminant of [`Command::WriteToPane`].
    WriteToPane,
    /// Discriminant of [`Command::ToggleLockMode`].
    ToggleLockMode,
    /// Discriminant of [`Command::SetLockMode`].
    SetLockMode,
    /// Discriminant of [`Command::ToggleMouseSelect`].
    ToggleMouseSelect,
    /// Discriminant of [`Command::RunCommandPane`].
    RunCommandPane,
    /// Discriminant of [`Command::Visual`].
    Visual,
    /// Discriminant of [`Command::Plugin`].
    Plugin,
    /// Discriminant of [`Command::TogglePaneFullscreen`].
    TogglePaneFullscreen,
    /// Discriminant of [`Command::MoveTab`].
    MoveTab,
    /// Discriminant of [`Command::Quit`].
    Quit,
    /// Discriminant of [`Command::Detach`].
    Detach,
    /// Discriminant of [`Command::DetachAll`].
    DetachAll,
    /// Discriminant of [`Command::SwitchSession`].
    SwitchSession,
}

impl Command {
    /// The payload-free [`CommandKind`] discriminant of this command.
    #[must_use]
    pub const fn kind(&self) -> CommandKind {
        match self {
            Command::NewPane(_) => CommandKind::NewPane,
            Command::ClosePane(_) => CommandKind::ClosePane,
            Command::ResizePane(_) => CommandKind::ResizePane,
            Command::FocusPane(_) => CommandKind::FocusPane,
            Command::NewTab(_) => CommandKind::NewTab,
            Command::CloseTab(_) => CommandKind::CloseTab,
            Command::FocusTab(_) => CommandKind::FocusTab,
            Command::WriteToPane(_) => CommandKind::WriteToPane,
            Command::ToggleLockMode(_) => CommandKind::ToggleLockMode,
            Command::SetLockMode(_) => CommandKind::SetLockMode,
            Command::ToggleMouseSelect => CommandKind::ToggleMouseSelect,
            Command::RunCommandPane(_) => CommandKind::RunCommandPane,
            Command::Visual(_) => CommandKind::Visual,
            Command::Plugin(_) => CommandKind::Plugin,
            Command::TogglePaneFullscreen => CommandKind::TogglePaneFullscreen,
            Command::MoveTab(_) => CommandKind::MoveTab,
            Command::Quit => CommandKind::Quit,
            Command::Detach(_) => CommandKind::Detach,
            Command::DetachAll => CommandKind::DetachAll,
            Command::SwitchSession(_) => CommandKind::SwitchSession,
        }
    }
}

/// Arguments for [`Command::NewPane`].
///
/// The dispatcher routes on `stacked`: set, the new pane joins the source's
/// stack, creating one if needed; unset, the source leaf splits
/// directionally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewPaneArgs {
    /// Pane to split from; `None` uses the focused pane.
    pub source: Option<PaneId>,
    /// Tab the new pane joins when no source pane names one: the split
    /// anchor becomes that tab's most recently focused pane (its first pane
    /// in layout order until one is focused). Ignored when `source` is set —
    /// a source pane's own tab wins.
    #[serde(default)]
    pub tab: Option<TabId>,
    /// Split direction, always named by the client that issues the command:
    /// the direction its own `layout.new-pane-direction` setting resolves to,
    /// or the one the action or CLI flag states outright. Unused when
    /// `stacked` is set — a stack has no direction.
    pub direction: Direction,
    /// Stack the new pane onto the source instead of splitting space.
    pub stacked: bool,
    /// Working directory; `None` inherits.
    pub cwd: Option<PathBuf>,
    /// Command to run; `None` launches the default shell.
    pub command: Option<SpawnSpec>,
    /// Client to show the new pane on.
    ///
    /// - `Some(client)`: that client is targeted, even over an in-session
    ///   issuer. A client not attached to the target session is rejected;
    ///   there is no fallback.
    /// - `None`: the issuing client; for a source with no client, the
    ///   session's sole client. A session with several attached clients and
    ///   no named target is rejected.
    pub client: Option<ClientId>,
}

/// Arguments for [`Command::ClosePane`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClosePaneArgs {
    /// Pane to close; `None` closes the focused pane.
    pub pane: Option<PaneId>,
    /// Kill the pane's child immediately, overriding its close policy.
    pub force: bool,
    /// Kill the child's whole process group: every descendant it spawned
    /// stops with it. Changes kill scope only; a `ConfirmIfBusy` pane still
    /// rejects the close while busy.
    #[serde(default)]
    pub tree: bool,
}

/// Arguments for [`Command::ResizePane`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizePaneArgs {
    /// Pane to resize; `None` resizes the focused pane.
    pub pane: Option<PaneId>,
    /// Which of the pane's borders moves.
    pub direction: Direction,
    /// Signed number of cells the border moves. Positive moves it outward —
    /// the pane grows toward `direction` and the neighbor on that side
    /// donates the cells; negative moves it inward — the pane shrinks and
    /// that neighbor gains the cells. Zero is rejected at dispatch.
    pub size: i16,
}

/// The pane a [`Command::FocusPane`] moves focus to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FocusTarget {
    /// A pane named by id.
    Pane(PaneId),
    /// The nearest pane in a direction from the client's focused pane,
    /// resolved geometrically against the solved layout.
    Direction(Direction),
}

/// Arguments for [`Command::FocusPane`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusPaneArgs {
    /// Pane to focus, by id or by direction from the focused pane.
    pub target: FocusTarget,
    /// Client whose focus moves; resolved by the same rules as
    /// [`NewPaneArgs::client`].
    pub client: Option<ClientId>,
}

/// Arguments for [`Command::NewTab`]. The tab's name is not supplied by the
/// caller — the runtime assigns a freshly generated one.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NewTabArgs {
    /// Working directory for the tab's first pane; `None` inherits.
    pub cwd: Option<PathBuf>,
    /// Client that switches onto the new tab; resolved by the same rules as
    /// [`NewPaneArgs::client`].
    pub client: Option<ClientId>,
}

/// Arguments for [`Command::CloseTab`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CloseTabArgs {
    /// Tab to close; `None` closes the focused tab.
    pub tab: Option<TabId>,
    /// Kill every pane's child immediately, overriding each close policy.
    pub force: bool,
    /// Kill each child's whole process group: every descendant stops with
    /// its pane. Changes kill scope only; a `ConfirmIfBusy` pane still
    /// rejects the close while busy.
    #[serde(default)]
    pub tree: bool,
}

/// Where [`Command::FocusTab`] should move focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TabTarget {
    /// The next tab, wrapping around.
    Next,
    /// The previous tab, wrapping around.
    Prev,
    /// A zero-based tab index.
    Index(usize),
    /// A specific tab.
    Id(TabId),
}

/// Arguments for [`Command::FocusTab`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusTabArgs {
    /// Which tab to focus.
    pub target: TabTarget,
    /// Client whose view switches; resolved by the same rules as
    /// [`NewPaneArgs::client`].
    pub client: Option<ClientId>,
}

/// Arguments for [`Command::WriteToPane`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WriteToPaneArgs {
    /// Pane to write to; `None` writes to the focused pane.
    pub pane: Option<PaneId>,
    /// Raw bytes to inject into the pane's input.
    pub data: Vec<u8>,
}

/// Arguments for [`Command::SetLockMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockModeArgs {
    /// Whether the client should be locked (input passed through verbatim).
    pub locked: bool,
    /// Client whose lock mode changes; resolved by the same rules as
    /// [`NewPaneArgs::client`].
    #[serde(default)]
    pub client: Option<ClientId>,
}

/// Arguments for [`Command::ToggleLockMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToggleLockModeArgs {
    /// Client whose lock mode flips; resolved by the same rules as
    /// [`NewPaneArgs::client`].
    #[serde(default)]
    pub client: Option<ClientId>,
}

/// Arguments for [`Command::RunCommandPane`]. The pane's display name is not
/// supplied by the caller — names are only ever system-generated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCommandPaneArgs {
    /// The command to spawn.
    pub command: SpawnSpec,
    /// Working directory; `None` inherits.
    pub cwd: Option<PathBuf>,
    /// Pane to split from; `None` uses the focused pane.
    pub source: Option<PaneId>,
    /// Tab the new pane joins when no source pane names one; resolved by the
    /// same rules as [`NewPaneArgs::tab`].
    #[serde(default)]
    pub tab: Option<TabId>,
    /// Split direction for the new pane, resolved by the issuing client the
    /// same way [`NewPaneArgs::direction`] is. Unused when `stacked` is set —
    /// a stack has no direction.
    pub direction: Direction,
    /// Stack the new pane onto the source pane instead of splitting space.
    pub stacked: bool,
    /// Client to show the new pane on; resolved by the same rules as
    /// [`NewPaneArgs::client`].
    #[serde(default)]
    pub client: Option<ClientId>,
}

/// Arguments for [`Command::MoveTab`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveTabArgs {
    /// Tab to move; `None` moves the focused tab.
    pub tab: Option<TabId>,
    /// Destination zero-based index.
    pub index: usize,
}

/// Arguments for [`Command::Detach`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DetachArgs {
    /// Client that detaches; resolved by the same rules as
    /// [`NewPaneArgs::client`].
    #[serde(default)]
    pub client: Option<ClientId>,
}

/// Arguments for [`Command::SwitchSession`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchSessionArgs {
    /// Client to move; `None` moves the issuing client. A session with several
    /// attached clients and no named target is rejected.
    #[serde(default)]
    pub client: Option<ClientId>,
    /// Session the client moves to. The caller resolves it; this session never
    /// looks a name up.
    pub session: SessionId,
}

/// Selection and copy commands — the commands of visual mode.
///
/// A client is in visual mode while text is highlighted. A mouse drag over a
/// pane's content starts a selection, and a click or any input that reaches
/// the pane's program drops it. There is no `Enter`/`Exit` variant: a
/// selection appearing enters visual mode, and it clearing leaves.
///
/// There is no copy cursor: selecting is the mouse's alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VisualCommand {
    /// Begin or extend a selection in one pane. Issued by the mouse layer as a
    /// drag moves.
    SetSelection(SetSelectionArgs),
    /// Clear one pane's selection, leaving visual mode for that pane.
    ClearSelection(ClearSelectionArgs),
    /// Copy the current selection to a clipboard target.
    Copy(CopyArgs),
}

/// Arguments for [`VisualCommand::SetSelection`].
///
/// The pane is named, never inferred: each pane keeps its own highlight, and
/// one client can have several up at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSelectionArgs {
    /// The pane to highlight in.
    pub pane: PaneId,
    /// The highlight to put there, replacing any the pane already had.
    pub selection: Selection,
}

/// Arguments for [`VisualCommand::ClearSelection`].
///
/// The pane is named, never inferred, as in [`SetSelectionArgs`]. Clearing a
/// pane that has no highlight is not an error and changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClearSelectionArgs {
    /// The pane whose highlight is dropped.
    pub pane: PaneId,
}

/// Arguments for [`VisualCommand::Copy`].
///
/// The pane is named, never inferred, as in [`SetSelectionArgs`]: a client can
/// have a highlight up in several panes at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyArgs {
    /// The pane whose highlight is copied.
    pub pane: PaneId,
    /// Where the copied text goes.
    pub target: CopyTarget,
    /// Whether blanks at the end of each copied row are dropped.
    ///
    /// A terminal row is padded to the pane's full width with blank cells: a
    /// highlight over `hello` in an 80-column pane covers 75 trailing blanks.
    /// `true` copies `hello`; `false` copies `hello` followed by those blanks.
    pub trim_trailing_whitespace: bool,
}

/// Plugin lifecycle commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginCommand {
    /// Install a plugin from a source.
    Install(InstallPluginArgs),
    /// Remove an installed plugin.
    Uninstall(UninstallPluginArgs),
    /// Enable an installed plugin.
    Enable(EnablePluginArgs),
    /// Disable an installed plugin.
    Disable(DisablePluginArgs),
    /// Update a plugin to its latest version.
    Update(UpdatePluginArgs),
    /// Reload a plugin in place.
    Reload(ReloadPluginArgs),
}

/// Arguments for [`PluginCommand::Install`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallPluginArgs {
    /// Where to fetch the plugin from (path, URL, or registry ref).
    pub source: String,
}

/// Arguments for [`PluginCommand::Uninstall`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UninstallPluginArgs {
    /// The plugin to remove.
    pub plugin: PluginId,
}

/// Arguments for [`PluginCommand::Enable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnablePluginArgs {
    /// The plugin to enable.
    pub plugin: PluginId,
}

/// Arguments for [`PluginCommand::Disable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisablePluginArgs {
    /// The plugin to disable.
    pub plugin: PluginId,
}

/// Arguments for [`PluginCommand::Update`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePluginArgs {
    /// The plugin to update.
    pub plugin: PluginId,
}

/// Arguments for [`PluginCommand::Reload`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadPluginArgs {
    /// The plugin to reload.
    pub plugin: PluginId,
}

// === Command envelope and source metadata ===
//
// Every command that crosses a boundary (keybinding dispatch, IPC socket,
// plugin host call, internal lifecycle) travels inside one [`CommandEnvelope`].
// The envelope carries the identity, origin, and timestamp; the [`Command`]
// itself carries no provenance. `issued_at` is a `SystemTime`, never an
// `Instant`.

/// Where a command came from. The runtime uses this to resolve focus context,
/// enforce permissions, and attribute diagnostics.
///
/// `ExternalCli` carries an optional session target and an optional target
/// client: an external command with no explicit target acts through the
/// session's sole attached client, and is rejected when several are attached
/// and none is named. `Plugin` and `Internal` have no associated client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandSource {
    /// A keybinding fired by an attached client.
    KeyBinding {
        /// The client whose keypress triggered the command.
        client_id: ClientId,
    },
    /// A mouse action from an attached client.
    Mouse {
        /// The client that generated the mouse event.
        client_id: ClientId,
    },
    /// An in-session CLI command delivered over the runtime socket. Always
    /// targets the source pane's current runtime context.
    InSessionCli {
        /// Session the issuing CLI process belongs to.
        session_id: SessionId,
        /// Client that asked for the pane when it was spawned; `None` when the
        /// pane was created with no designated client (its shell then has no
        /// `KOSHI_CLIENT_ID` to report). Pane- and session-scoped commands
        /// work without one; client-scoped commands need an attached client.
        client_id: Option<ClientId>,
        /// Pane the command was issued from.
        pane_id: PaneId,
        /// OS path of the runtime socket the command arrived on.
        socket_path: PathBuf,
    },
    /// An external CLI invocation — a `koshi` command typed outside any
    /// pane — optionally naming a target session and a target client.
    ExternalCli {
        /// Explicit target session; `None` means no session was resolved.
        session_id: Option<SessionId>,
        /// The client the caller named on the command line, for a command whose
        /// own arguments carry no client field; `None` when the invocation named
        /// none. Read by [`CommandSource::target_client`]. A source names this
        /// client and an issuing client separately, and this one is never the
        /// issuer.
        #[serde(default)]
        target_client: Option<ClientId>,
    },
    /// A command issued by a plugin.
    Plugin {
        /// The plugin that issued the command.
        plugin_id: PluginId,
    },
    /// A command the runtime issued to itself (lifecycle, internal wiring).
    Internal,
}

impl CommandSource {
    /// The client this source is attributed to, if any. `KeyBinding` and
    /// `Mouse` always name a client; `InSessionCli` names one when the issuing
    /// pane was spawned for a client; `ExternalCli`, `Plugin`, and `Internal`
    /// never do. `ExternalCli` never names a client here even when the
    /// invocation named one — that client is a target the caller chose, read
    /// through [`Self::target_client`], not the issuer.
    #[must_use]
    pub const fn client_id(&self) -> Option<ClientId> {
        match self {
            CommandSource::KeyBinding { client_id } | CommandSource::Mouse { client_id } => {
                Some(*client_id)
            }
            CommandSource::InSessionCli { client_id, .. } => *client_id,
            CommandSource::ExternalCli { .. }
            | CommandSource::Plugin { .. }
            | CommandSource::Internal => None,
        }
    }

    /// The client the caller explicitly named as this command's target. Only
    /// [`CommandSource::ExternalCli`] carries one; every other source returns
    /// `None`. A target that is not attached to the acting session is refused,
    /// never replaced by a fallback.
    #[must_use]
    pub const fn target_client(&self) -> Option<ClientId> {
        match self {
            CommandSource::ExternalCli { target_client, .. } => *target_client,
            CommandSource::KeyBinding { .. }
            | CommandSource::Mouse { .. }
            | CommandSource::InSessionCli { .. }
            | CommandSource::Plugin { .. }
            | CommandSource::Internal => None,
        }
    }

    /// Construct a [`CommandSource::KeyBinding`].
    #[must_use]
    pub const fn key_binding(client_id: ClientId) -> Self {
        CommandSource::KeyBinding { client_id }
    }

    /// Construct a [`CommandSource::Mouse`].
    #[must_use]
    pub const fn mouse(client_id: ClientId) -> Self {
        CommandSource::Mouse { client_id }
    }

    /// Construct a [`CommandSource::InSessionCli`].
    #[must_use]
    pub const fn in_session_cli(
        session_id: SessionId,
        client_id: Option<ClientId>,
        pane_id: PaneId,
        socket_path: PathBuf,
    ) -> Self {
        CommandSource::InSessionCli {
            session_id,
            client_id,
            pane_id,
            socket_path,
        }
    }

    /// Construct a [`CommandSource::ExternalCli`].
    #[must_use]
    pub const fn external_cli(
        session_id: Option<SessionId>,
        target_client: Option<ClientId>,
    ) -> Self {
        CommandSource::ExternalCli {
            session_id,
            target_client,
        }
    }

    /// Construct a [`CommandSource::Plugin`].
    #[must_use]
    pub const fn plugin(plugin_id: PluginId) -> Self {
        CommandSource::Plugin { plugin_id }
    }
}

/// Why a [`CommandEnvelope`] is not internally consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandEnvelopeError {
    /// `client_id` does not match the client named by `source`, or names a
    /// client for a source that has none.
    ClientIdMismatch,
}

impl std::fmt::Display for CommandEnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandEnvelopeError::ClientIdMismatch => {
                f.write_str("envelope client_id does not match its source")
            }
        }
    }
}

impl std::error::Error for CommandEnvelopeError {}

/// One command crossing a boundary, with its identity, origin, and timestamp.
///
/// `client_id` mirrors the client named by `source`; the two must agree.
/// Deserialization is routed through `CommandEnvelopeWire`, which rejects any
/// envelope where they disagree. [`CommandEnvelope::new`] derives the field;
/// [`CommandEnvelope::validate`] checks a hand-built value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "CommandEnvelopeWire")]
pub struct CommandEnvelope {
    /// Unique id for this command transaction.
    pub id: CommandId,
    /// Where the command originated.
    pub source: CommandSource,
    /// Client the command is attributed to; mirrors the source's client when it
    /// names one, and is `None` for sources that do not.
    pub client_id: Option<ClientId>,
    /// When the command was issued, as wall-clock time. The envelope crosses
    /// process boundaries.
    pub issued_at: SystemTime,
    /// The requested mutation.
    pub command: Command,
}

impl CommandEnvelope {
    /// Build an envelope, deriving `client_id` from `source`. The caller
    /// supplies `id` and `issued_at`; this reads no clock and draws no random
    /// value.
    #[must_use]
    pub fn new(
        id: CommandId,
        source: CommandSource,
        issued_at: SystemTime,
        command: Command,
    ) -> Self {
        let client_id = source.client_id();
        CommandEnvelope {
            id,
            source,
            client_id,
            issued_at,
            command,
        }
    }

    /// Check that `client_id` matches the client named by `source`, returning
    /// the envelope unchanged when it does. Deserialization runs this check on
    /// every envelope.
    ///
    /// # Errors
    /// Returns [`CommandEnvelopeError::ClientIdMismatch`] if the two disagree.
    pub fn validate(self) -> Result<Self, CommandEnvelopeError> {
        if self.client_id == self.source.client_id() {
            Ok(self)
        } else {
            Err(CommandEnvelopeError::ClientIdMismatch)
        }
    }
}

/// Unvalidated wire shape for [`CommandEnvelope`]. Deserialization lands here
/// first, then the `try_from` conversion below runs
/// [`CommandEnvelope::validate`], which rejects inconsistent attribution.
#[derive(Deserialize)]
struct CommandEnvelopeWire {
    id: CommandId,
    source: CommandSource,
    client_id: Option<ClientId>,
    issued_at: SystemTime,
    command: Command,
}

impl TryFrom<CommandEnvelopeWire> for CommandEnvelope {
    type Error = CommandEnvelopeError;

    fn try_from(wire: CommandEnvelopeWire) -> Result<Self, Self::Error> {
        CommandEnvelope {
            id: wire.id,
            source: wire.source,
            client_id: wire.client_id,
            issued_at: wire.issued_at,
            command: wire.command,
        }
        .validate()
    }
}

// === Command results and rejection ===
//
// A command never silently no-ops: dispatching one always yields a
// [`CommandResult`], either applied (with the events it emitted) or rejected
// with an observable [`RejectReason`]. [`CliExitCode`] names the process exit
// statuses the external CLI reports; the CLI layer picks which one a result
// maps to.

/// The outcome of dispatching one command, keyed back to its originating
/// [`CommandEnvelope`] by `command_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandResult {
    /// The command was applied, emitting the listed events.
    Ok {
        /// Id of the command that was applied.
        command_id: CommandId,
        /// Events the command produced, in emission order.
        emitted_events: Vec<Event>,
    },
    /// The command was rejected and applied nothing.
    Rejected {
        /// Id of the command that was rejected.
        command_id: CommandId,
        /// Why the command was rejected.
        reason: RejectReason,
        /// Optional human-facing hint for resolving the rejection.
        help: Option<String>,
    },
}

/// Process exit status the external CLI reports. Discriminants are the actual
/// exit numbers. The full result-to-exit-code wiring lives in the CLI layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliExitCode {
    /// The command succeeded.
    Success = 0,
    /// A runtime or action error (e.g. a rejected command).
    RuntimeAction = 1,
    /// A CLI usage or config validation error.
    UsageOrConfig = 2,
    /// The named session was not found.
    SessionNotFound = 3,
    /// The runtime IPC endpoint was unavailable.
    IpcUnavailable = 4,
}

impl CliExitCode {
    /// The numeric exit code this variant reports to the OS.
    #[must_use]
    pub const fn code(self) -> i32 {
        self as i32
    }
}

#[cfg(test)]
mod tests;
