//! Canonical command vocabulary.
//!
//! [`Command`] and its nested enums are the single source of truth for every
//! requested mutation. These are pure data shells: no handlers, no behaviour,
//! no runtime state. Validation, target resolution, and execution all live in
//! higher layers (the session runtime); this module only names *what* may be
//! requested.
//!
//! Commands cross process boundaries (CLI IPC and plugins), so every variant
//! and arg struct contains only serde-friendly, cross-process-meaningful
//! types. **No `Instant`** — it is not `Serialize` and is opaque across
//! processes; use `SystemTime` or epoch units where a timestamp is needed.
//! No raw OS handles, no `&mut` references, and command identity is never a
//! free-form `String`.

use crate::action::ActionRef;
use crate::event::RejectReason;
use crate::geometry::Direction;
use crate::ids::{ClientId, CommandId, EventId, PaneId, PluginId, SessionId, TabId};
use crate::process::SpawnSpec;
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
    /// Rename a tab.
    RenameTab(RenameTabArgs),
    /// Move focus to a tab; next/prev/index all resolve to this.
    FocusTab(FocusTabArgs),
    /// Write raw bytes into a pane's input.
    WriteToPane(WriteToPaneArgs),
    /// Toggle the lock (pass-through) mode of the focused pane.
    ToggleLockMode,
    /// Set the lock mode explicitly.
    SetLockMode(LockModeArgs),
    /// Spawn a command in a new pane.
    RunCommandPane(RunCommandPaneArgs),
    /// Copy mode, selection, and search.
    CopyMode(CopyModeCommand),
    /// Plugin lifecycle management.
    Plugin(PluginCommand),
    /// Toggle fullscreen for the focused pane.
    TogglePaneFullscreen,
    /// Rename a pane.
    RenamePane(RenamePaneArgs),
    /// Move a tab to a new index.
    MoveTab(MoveTabArgs),
    /// Rename the current session.
    RenameSession(RenameSessionArgs),
    /// Prompt the issuing client to quit the client or session.
    Quit,
    /// Add a runtime-only keybinding to the manual keymap layer.
    SetKeyBinding(SetKeyBindingArgs),
    /// Remove a keybinding through the manual keymap layer.
    RemoveKeyBinding(RemoveKeyBindingArgs),
    /// Drop runtime keybinding customization, restoring built-in defaults.
    ResetKeyBindings(ResetKeyBindingsArgs),
}

/// The payload-free discriminant of a [`Command`] — one unit variant per
/// `Command` variant, in the same order.
///
/// The action registry ([`crate::action`]) routes a user-facing action to a
/// core command by naming its `CommandKind`; the dispatcher later rebuilds the
/// full typed `Command` from that kind plus resolved targets and args. Keeping
/// the discriminant separate from the data-carrying enum lets action metadata
/// stay `Copy` and free of placeholder args. [`Command::kind`] maps the other
/// way, and a test pins the two enums to the same variant set so they cannot
/// drift apart.
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
    /// Discriminant of [`Command::RenameTab`].
    RenameTab,
    /// Discriminant of [`Command::FocusTab`].
    FocusTab,
    /// Discriminant of [`Command::WriteToPane`].
    WriteToPane,
    /// Discriminant of [`Command::ToggleLockMode`].
    ToggleLockMode,
    /// Discriminant of [`Command::SetLockMode`].
    SetLockMode,
    /// Discriminant of [`Command::RunCommandPane`].
    RunCommandPane,
    /// Discriminant of [`Command::CopyMode`].
    CopyMode,
    /// Discriminant of [`Command::Plugin`].
    Plugin,
    /// Discriminant of [`Command::TogglePaneFullscreen`].
    TogglePaneFullscreen,
    /// Discriminant of [`Command::RenamePane`].
    RenamePane,
    /// Discriminant of [`Command::MoveTab`].
    MoveTab,
    /// Discriminant of [`Command::RenameSession`].
    RenameSession,
    /// Discriminant of [`Command::Quit`].
    Quit,
    /// Discriminant of [`Command::SetKeyBinding`].
    SetKeyBinding,
    /// Discriminant of [`Command::RemoveKeyBinding`].
    RemoveKeyBinding,
    /// Discriminant of [`Command::ResetKeyBindings`].
    ResetKeyBindings,
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
            Command::RenameTab(_) => CommandKind::RenameTab,
            Command::FocusTab(_) => CommandKind::FocusTab,
            Command::WriteToPane(_) => CommandKind::WriteToPane,
            Command::ToggleLockMode => CommandKind::ToggleLockMode,
            Command::SetLockMode(_) => CommandKind::SetLockMode,
            Command::RunCommandPane(_) => CommandKind::RunCommandPane,
            Command::CopyMode(_) => CommandKind::CopyMode,
            Command::Plugin(_) => CommandKind::Plugin,
            Command::TogglePaneFullscreen => CommandKind::TogglePaneFullscreen,
            Command::RenamePane(_) => CommandKind::RenamePane,
            Command::MoveTab(_) => CommandKind::MoveTab,
            Command::RenameSession(_) => CommandKind::RenameSession,
            Command::Quit => CommandKind::Quit,
            Command::SetKeyBinding(_) => CommandKind::SetKeyBinding,
            Command::RemoveKeyBinding(_) => CommandKind::RemoveKeyBinding,
            Command::ResetKeyBindings(_) => CommandKind::ResetKeyBindings,
        }
    }
}

/// Arguments for [`Command::NewPane`].
///
/// One command, two structural outcomes — the dispatcher routes on the
/// flag: `stacked` adds the new pane to the source's stack (creating one
/// if needed), and otherwise the source leaf splits directionally.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NewPaneArgs {
    /// Pane to split from; `None` uses the focused pane.
    pub source: Option<PaneId>,
    /// Split direction; `None` uses the runtime's default split direction,
    /// which the layout config seeds. Unused when `stacked` is set — a stack
    /// has no direction.
    pub direction: Option<Direction>,
    /// Stack the new pane onto the source instead of splitting space.
    pub stacked: bool,
    /// Working directory; `None` inherits.
    pub cwd: Option<PathBuf>,
    /// Command to run; `None` launches the default shell.
    pub command: Option<SpawnSpec>,
    /// Client to show the new pane on. When set, that client is targeted and takes
    /// priority even over an in-session issuer; a client not attached to the target
    /// session is rejected outright (no fallback). `None` targets the issuing
    /// client (for an in-session source) or, for a source with no client, the
    /// session's sole client — a session with several attached clients and no named
    /// target is rejected rather than switching an arbitrary one.
    pub client: Option<ClientId>,
}

/// Arguments for [`Command::ClosePane`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClosePaneArgs {
    /// Pane to close; `None` closes the focused pane.
    pub pane: Option<PaneId>,
    /// Kill the pane's child immediately, overriding its close policy.
    pub force: bool,
    /// Widen the kill to the child's whole process group, so every
    /// descendant it spawned stops with it. Changes kill scope only; a
    /// `ConfirmIfBusy` pane still rejects the close while busy.
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
    /// Widen every kill to its child's whole process group, so every
    /// descendant stops with its pane. Changes kill scope only; a
    /// `ConfirmIfBusy` pane still rejects the close while busy.
    #[serde(default)]
    pub tree: bool,
}

/// Arguments for [`Command::RenameTab`]. The new name is not supplied by the
/// caller — the runtime assigns a freshly generated one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameTabArgs {
    /// Tab to rename; `None` renames the focused tab.
    pub tab: Option<TabId>,
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
    /// Whether the pane should be locked (input passed through verbatim).
    pub locked: bool,
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
    /// Split direction for the new pane; `None` defaults to a rightward
    /// split. Unused when `stacked` is set — a stack has no direction.
    pub direction: Option<Direction>,
    /// Stack the new pane onto the source pane instead of splitting space.
    pub stacked: bool,
}

/// Arguments for [`Command::RenamePane`]. The new name is not supplied by the
/// caller — the runtime assigns a freshly generated one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenamePaneArgs {
    /// Pane to rename; `None` renames the focused pane.
    pub pane: Option<PaneId>,
}

/// Arguments for [`Command::MoveTab`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveTabArgs {
    /// Tab to move; `None` moves the focused tab.
    pub tab: Option<TabId>,
    /// Destination zero-based index.
    pub index: usize,
}

/// Arguments for [`Command::RenameSession`]. The new name is not supplied by
/// the caller — the runtime assigns a freshly generated one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameSessionArgs {
    /// Session to rename; `None` targets the source's own session context.
    pub session: Option<SessionId>,
}

/// Arguments for [`Command::SetKeyBinding`]. The key sequence travels as the
/// string the caller typed: what a sequence means (its `<leader>` token)
/// depends on the effective leader configuration, so the runtime parses it
/// against its own settings rather than trusting the sender's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetKeyBindingArgs {
    /// Input mode the binding lands in; `None` binds in `normal`.
    pub mode: Option<String>,
    /// The key sequence to bind, in the angle grammar (`"<C-p> n"`).
    pub sequence: String,
    /// The action the sequence fires. No arguments travel with it — a manual
    /// binding is the action reference alone, like any user-authored binding.
    pub action: ActionRef,
}

/// Arguments for [`Command::RemoveKeyBinding`]. The key sequence travels as
/// a string for the same reason as [`SetKeyBindingArgs::sequence`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveKeyBindingArgs {
    /// Input mode the removal applies in; `None` removes in `normal`.
    pub mode: Option<String>,
    /// The key sequence to remove, in the angle grammar.
    pub sequence: String,
}

/// Arguments for [`Command::ResetKeyBindings`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetKeyBindingsArgs {
    /// Input mode to reset; `None` resets the whole keybindings section —
    /// every mode's runtime and user-file customization plus the timing,
    /// leader, and unlock-alternative settings.
    pub mode: Option<String>,
}

/// Copy mode, selection, and search commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CopyModeCommand {
    /// Enter copy mode.
    Enter,
    /// Leave copy mode.
    Exit,
    /// Move the copy cursor.
    MoveCursor(MoveCursorArgs),
    /// Begin or extend a selection.
    SetSelection(SetSelectionArgs),
    /// Clear the active selection.
    ClearSelection,
    /// Copy the current selection to a clipboard target.
    Copy(CopyArgs),
    /// Start a search.
    Search(SearchArgs),
    /// Jump to the next match.
    SearchNext,
    /// Jump to the previous match.
    SearchPrev,
}

/// The unit a copy-cursor move advances by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MoveUnit {
    /// A single cell.
    Cell,
    /// A word boundary.
    Word,
    /// A logical line.
    Line,
    /// A page (viewport height).
    Page,
    /// Jump to the top of scrollback (absolute; `direction` is ignored).
    Top,
    /// Jump to the bottom of scrollback (absolute; `direction` is ignored).
    Bottom,
}

/// Arguments for [`CopyModeCommand::MoveCursor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveCursorArgs {
    /// How far each step moves.
    pub unit: MoveUnit,
    /// Which way to move.
    pub direction: Direction,
}

/// The shape of a selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SelectionKind {
    /// A contiguous character range across wrapped lines.
    Character,
    /// Endpoints snapped to word boundaries.
    Word,
    /// Whole logical lines.
    Line,
    /// A rectangular column range across rows.
    Block,
}

/// A grid position that spans both the scrollback history and the currently
/// visible grid (row 0 is the oldest visible line in scrollback or the top of
/// the visible grid if scrollback is empty).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GridPos {
    /// Row number: 0 is the top of scrollback/visible area, increasing downward.
    pub row: u64,
    /// Column in cells, 0-indexed from the left.
    pub col: u16,
}

/// Arguments for [`CopyModeCommand::SetSelection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSelectionArgs {
    /// Selection shape.
    pub kind: SelectionKind,
    /// The fixed end of the selection.
    pub anchor: GridPos,
    /// The moving end of the selection.
    pub cursor: GridPos,
}

/// Which clipboard a copy targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CopyTarget {
    /// OSC 52 (a terminal escape sequence for setting the clipboard) to the
    /// outer terminal — the default, dependency-free option.
    Osc52,
    /// The native OS clipboard (behind the `native` feature).
    Native,
}

/// Arguments for [`CopyModeCommand::Copy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyArgs {
    /// Where the copied text should go.
    pub target: CopyTarget,
}

/// Arguments for [`CopyModeCommand::Search`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchArgs {
    /// The query text.
    pub query: String,
    /// Treat `query` as a regular expression rather than a literal.
    pub regex: bool,
    /// Match case-sensitively.
    pub case_sensitive: bool,
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
// The envelope carries the identity, origin, and timestamp the runtime needs
// for permissions, focus context, and diagnostics; the [`Command`] itself stays
// a pure "what" with no provenance baked in. `issued_at` is `SystemTime` (never
// `Instant`) because the envelope is serialized across processes.

/// Where a command came from. The runtime uses this to resolve focus context,
/// enforce permissions, and attribute diagnostics.
///
/// `ExternalCli` carries only an optional session target: an external command
/// with no explicit target is rejected for current-pane operations and never
/// falls back to the focused pane. `Plugin` and `Internal` have no associated
/// client.
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
        /// Client owning the pane the command was issued from.
        client_id: ClientId,
        /// Pane the command was issued from.
        pane_id: PaneId,
        /// OS path of the runtime socket the command arrived on.
        socket_path: PathBuf,
    },
    /// An external CLI invocation, optionally naming a target session.
    ExternalCli {
        /// Explicit target session; `None` means no session was resolved.
        session_id: Option<SessionId>,
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
    /// The client this source is attributed to, if any. `InSessionCli`,
    /// `KeyBinding`, and `Mouse` name a client; `ExternalCli`, `Plugin`, and
    /// `Internal` do not.
    #[must_use]
    pub const fn client_id(&self) -> Option<ClientId> {
        match self {
            CommandSource::KeyBinding { client_id }
            | CommandSource::Mouse { client_id }
            | CommandSource::InSessionCli { client_id, .. } => Some(*client_id),
            CommandSource::ExternalCli { .. }
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
        client_id: ClientId,
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
    pub const fn external_cli(session_id: Option<SessionId>) -> Self {
        CommandSource::ExternalCli { session_id }
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
    /// `client_id` does not match the client named by `source` (or names a
    /// client for a source that has none). This check stops a malformed or
    /// hostile peer from misattributing a command to another client by
    /// forging `client_id`.
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
/// `client_id` is redundant with the client named by `source`; the two must
/// agree. Deserialization is routed through `CommandEnvelopeWire` and rejects
/// any envelope where they disagree, so a value decoded from the IPC socket or
/// a plugin can never carry a forged `client_id`. In-process construction
/// should use [`CommandEnvelope::new`] (which derives the field) or pass a
/// hand-built value through [`CommandEnvelope::validate`].
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
    /// When the command was issued. `SystemTime`, never `Instant`, because the
    /// envelope crosses process boundaries.
    pub issued_at: SystemTime,
    /// The requested mutation.
    pub command: Command,
}

impl CommandEnvelope {
    /// Build an envelope, deriving `client_id` from the source so the two can
    /// never disagree. Callers supply `id` and `issued_at` so the type stays
    /// clock- and randomness-free (and tests stay deterministic).
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
    /// the envelope unchanged when it does. This is the gate every untrusted
    /// envelope (deserialized or hand-built) must pass before the runtime
    /// trusts its attribution.
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
/// first, then [`CommandEnvelope::validate`] rejects inconsistent attribution
/// via the `try_from` conversion below — so the consistency invariant holds for
/// every decoded envelope, not just those built through `new`.
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
// with an observable [`RejectReason`]. [`CliExitCode`] is the placeholder
// core-side mapping the external CLI turns a result into a process exit status
// with (full wiring lives in the CLI layer).

/// The outcome of dispatching one command, keyed back to its originating
/// [`CommandEnvelope`] by `command_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandResult {
    /// The command was applied, emitting the listed events.
    Ok {
        /// Id of the command that was applied.
        command_id: CommandId,
        /// Events the command produced, in emission order.
        emitted_events: Vec<EventId>,
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

/// Process exit status the external CLI reports. This is the placeholder
/// core-side enumeration; the full result-to-exit-code wiring lives in the CLI
/// layer. Discriminants are the actual exit numbers.
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

    /// Placeholder mapping from a [`CommandResult`] to an exit code: applied
    /// commands succeed, rejected ones report a runtime/action error. Richer
    /// reasons (session-not-found, IPC-unavailable) are surfaced by the CLI
    /// layer, not derivable from the result alone.
    #[must_use]
    pub const fn for_result(result: &CommandResult) -> Self {
        match result {
            CommandResult::Ok { .. } => CliExitCode::Success,
            CommandResult::Rejected { .. } => CliExitCode::RuntimeAction,
        }
    }
}

#[cfg(test)]
mod tests;
