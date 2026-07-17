//! Canonical event vocabulary.
//!
//! [`Event`] and its nested enums are the single source of truth for every
//! completed fact the runtime emits. Like [`crate::command`], these are pure
//! data shells: no handlers, no behaviour, no runtime state. Emission,
//! delivery, backpressure, and privacy gating all live in higher layers; this
//! module only names *what* has happened.
//!
//! Events are append-only facts, never hidden commands — nothing here requests
//! a mutation. Events cross process boundaries (IPC watchers, plugin host,
//! storage), so every variant and payload contains only serde-friendly,
//! cross-process-meaningful types. **No `Instant`** — it is not `Serialize` and
//! is opaque across processes; timestamps use `SystemTime`. No raw OS handles
//! and no `&mut` references.
//!
//! Privacy is structural: each input payload variant encodes the classified
//! context and the resulting [`PrivacyTier`] together, and every non-public
//! variant is unit-shaped with no content field.

use crate::command::{CopyTarget, Selection};
use crate::geometry::{Point, Size};
use crate::ids::{ClientId, CommandId, PaneId, PluginId, SessionId, SubscriberId, TabId};
use crate::mouse::{MouseButton, ScrollDirection};
use crate::process::PtySize;
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// A completed fact emitted by the runtime.
///
/// Variants are grouped to match the sections further down the file: pane/tab
/// lifecycle, input modes, input privacy, mouse, delivery, selection/copy, and
/// plugins. Each variant wraps a like-named payload struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    // Pane and tab lifecycle.
    /// A pane was created and registered.
    PaneCreated(PaneCreated),
    /// A pane's child process exited.
    PaneProcessExited(PaneProcessExited),
    /// A pane's close transaction started.
    PaneClosing(PaneClosing),
    /// A pane leaf left the layout and registry.
    PaneRemoved(PaneRemoved),
    /// Focus moved to a pane.
    PaneFocused(PaneFocused),
    /// A pane's PTY was resized (emitted per affected pane after a layout solve).
    PtyResized(PtyResized),
    /// A pane's terminal content changed: a coalesced, metadata-only damage
    /// tick carrying no content. Lossy class — bursts collapse to at most one
    /// pending tick per pane per subscriber.
    PaneOutputUpdated(PaneOutputUpdated),
    /// A tab's layout tree changed.
    LayoutChanged(LayoutChanged),
    /// A tab was created.
    TabCreated(TabCreated),
    /// A tab was closed.
    TabClosed(TabClosed),
    /// Focus moved to a tab.
    TabFocused(TabFocused),
    /// A pane's display name changed.
    PaneRenamed(PaneRenamed),
    /// A tab moved to a new index.
    TabMoved(TabMoved),
    /// A tab's display name changed.
    TabRenamed(TabRenamed),
    /// The session display name changed.
    SessionRenamed(SessionRenamed),
    /// A pane became invisible because the terminal is too small.
    PaneSuppressed(PaneSuppressed),
    /// A suppressed pane became visible again after a resize.
    PaneResumed(PaneResumed),
    /// All panes are suppressed; runtime should show the too-small overlay.
    TerminalTooSmallEntered(TerminalTooSmallEntered),
    /// At least one pane regained visible area because the terminal grew
    /// back; runtime can leave the too-small overlay. Visibility changes
    /// from mode toggles (e.g. leaving fullscreen) do not emit this.
    TerminalTooSmallExited(TerminalTooSmallExited),
    /// Configuration reload succeeded and was atomically swapped in.
    ConfigReloaded(ConfigReloaded),
    /// Configuration reload failed; the previous config remains active.
    ConfigReloadFailed(ConfigReloadFailed),

    // Input modes and keybindings.
    /// The active input mode changed (normal or locked).
    InputModeChanged(InputModeChanged),
    /// A keybinding matched and resolved to a command.
    KeybindingMatched(KeybindingMatched),

    // Input privacy.
    /// A printable character was accepted for a focused pane (privacy-gated).
    PaneTyped(PaneTyped),
    /// Enter was accepted for a focused pane (privacy-gated).
    PaneEnterPressed(PaneEnterPressed),

    // Mouse input.
    /// A mouse button was pressed (client-local, hit-tested).
    MousePressed(MousePressed),
    /// A mouse button was released.
    MouseReleased(MouseReleased),
    /// The mouse was dragged with a button held.
    MouseDragged(MouseDragged),
    /// The mouse wheel scrolled.
    MouseScrolled(MouseScrolled),
    /// A mouse event was encoded and forwarded to a pane's PTY.
    PaneMouseForwarded(PaneMouseForwarded),
    /// A mouse event was delivered to a capable plugin.
    PluginMouseInput(PluginMouseInput),

    // Shell integration (OSC 133 semantic prompts).
    /// A command began running in a pane (OSC 133;C). Carries no command text.
    PaneCommandStarted(PaneCommandStarted),
    /// A command finished in a pane (OSC 133;D), with the exit code when the
    /// shell reports one. Carries no command text.
    PaneCommandFinished(PaneCommandFinished),

    // Delivery and rejection.
    /// A pane's bounded scrollback dropped lines on overflow.
    PaneScrollbackTruncated(PaneScrollbackTruncated),
    /// A subscriber's bounded queue overflowed and dropped events.
    SubscriberLagged(SubscriberLagged),
    /// A command was rejected by validation or target resolution.
    CommandRejected(CommandRejected),

    // Selection and copy.
    /// The active selection changed (or was cleared).
    ///
    /// There is no separate entered/exited event: a selection appearing IS
    /// entering visual mode and it clearing IS leaving, so this one event
    /// already carries the fact.
    SelectionChanged(SelectionChanged),
    /// A selection was copied to a clipboard target.
    Copied(Copied),

    // Plugin lifecycle.
    /// A plugin lifecycle fact.
    Plugin(PluginEvent),

    // Session lifecycle.
    /// The session is shutting down: its last tab closed, so the program
    /// quits. A terminal event — nothing follows it.
    Quit,
}

// ============================================================================
// Pane and tab lifecycle
// ============================================================================

/// Payload for [`Event::PaneCreated`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCreated {
    /// The new pane.
    pub pane_id: PaneId,
    /// The tab it belongs to.
    pub tab_id: TabId,
}

/// Payload for [`Event::PaneProcessExited`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneProcessExited {
    /// The pane whose process exited.
    pub pane_id: PaneId,
    /// The process exit code; `None` when terminated by a signal or unknown.
    pub exit_code: Option<i32>,
}

/// Payload for [`Event::PaneClosing`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneClosing {
    /// The pane whose close transaction started.
    pub pane_id: PaneId,
}

/// Payload for [`Event::PaneRemoved`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRemoved {
    /// The pane removed from the layout and registry.
    pub pane_id: PaneId,
    /// The tab it was removed from.
    pub tab_id: TabId,
}

/// Payload for [`Event::PaneFocused`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneFocused {
    /// The client whose focus moved.
    pub client_id: ClientId,
    /// The tab the focus moved in.
    pub tab_id: TabId,
    /// The newly focused pane.
    pub pane_id: PaneId,
    /// The pane that held this client's focus in the tab before, if any.
    pub prior_pane: Option<PaneId>,
}

/// Payload for [`Event::PtyResized`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtyResized {
    /// The pane whose PTY was resized.
    pub pane_id: PaneId,
    /// The new PTY dimensions in cells.
    pub size: PtySize,
}

/// Payload for [`Event::PaneOutputUpdated`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneOutputUpdated {
    /// The pane whose terminal content changed.
    pub pane_id: PaneId,
}

/// Payload for [`Event::LayoutChanged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutChanged {
    /// The tab whose layout tree changed.
    pub tab_id: TabId,
}

/// Payload for [`Event::TabCreated`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabCreated {
    /// The new tab.
    pub tab_id: TabId,
}

/// Payload for [`Event::TabClosed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabClosed {
    /// The closed tab.
    pub tab_id: TabId,
}

/// Payload for [`Event::TabFocused`].
///
/// A client's active tab is per-client state, so the payload names the client
/// whose view switched — with several clients attached, `tab_id` alone could
/// not say whose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabFocused {
    /// The client whose active tab changed.
    pub client_id: ClientId,
    /// The newly focused tab.
    pub tab_id: TabId,
    /// The tab the client was viewing before the switch. When the switch was
    /// forced by a tab close, this is the closed tab.
    pub prior_tab: TabId,
}

/// Payload for [`Event::PaneRenamed`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRenamed {
    /// The renamed pane.
    pub pane_id: PaneId,
    /// The pane's new display name.
    pub name: String,
}

/// Payload for [`Event::TabMoved`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabMoved {
    /// The moved tab.
    pub tab_id: TabId,
    /// The tab's previous zero-based index.
    pub old_index: usize,
    /// The tab's new zero-based index.
    pub new_index: usize,
}

/// Payload for [`Event::TabRenamed`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabRenamed {
    /// The renamed tab.
    pub tab_id: TabId,
    /// The tab's new display name.
    pub name: String,
}

/// Payload for [`Event::SessionRenamed`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRenamed {
    /// The renamed session.
    pub session_id: SessionId,
    /// The session's previous display name.
    pub old_name: String,
    /// The session's new display name.
    pub new_name: String,
}

/// Payload for [`Event::PaneSuppressed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSuppressed {
    /// The pane that became invisible.
    pub pane_id: PaneId,
    /// The tab containing the pane.
    pub tab_id: TabId,
}

/// Payload for [`Event::PaneResumed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneResumed {
    /// The pane that became visible again.
    pub pane_id: PaneId,
    /// The tab containing the pane.
    pub tab_id: TabId,
}

/// Payload for [`Event::TerminalTooSmallEntered`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalTooSmallEntered {
    /// The affected client viewport.
    pub client_id: ClientId,
    /// The viewport size that could not fit any pane.
    pub size: Size,
}

/// Payload for [`Event::TerminalTooSmallExited`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalTooSmallExited {
    /// The affected client viewport.
    pub client_id: ClientId,
    /// The viewport size after recovery.
    pub size: Size,
}

/// Payload for [`Event::ConfigReloaded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigReloaded {
    /// The session whose config was reloaded.
    pub session_id: SessionId,
}

/// Payload for [`Event::ConfigReloadFailed`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigReloadFailed {
    /// The session whose config reload failed.
    pub session_id: SessionId,
    /// Human-facing diagnostic.
    pub reason: String,
}

// ============================================================================
// Input modes and keybindings
// ============================================================================

/// The input mode a client is in.
///
/// Visual mode is deliberately absent: an input mode decides what a keystroke
/// does, and visual mode never interprets one — every key clears the selection
/// and reaches the program. Highlighted text is reported by
/// [`Event::SelectionChanged`], not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputMode {
    /// Normal keybinding interpretation.
    Normal,
    /// Locked: input passed through to the pane verbatim.
    Locked,
}

/// Payload for [`Event::InputModeChanged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputModeChanged {
    /// The client whose input mode changed. Lock mode is client-scoped:
    /// clients sharing a session hold independent modes.
    pub client_id: ClientId,
    /// The mode now in effect.
    pub mode: InputMode,
}

/// Payload for [`Event::KeybindingMatched`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeybindingMatched {
    /// The client whose input matched.
    pub client_id: ClientId,
    /// The command the binding resolved to.
    pub command_id: CommandId,
}

// ============================================================================
// Input privacy: typed characters and submitted lines
// ============================================================================

/// The privacy tier the runtime computes for an input event before delivery.
///
/// Tier is authoritative over plugin capability: capability can only narrow
/// what a subscriber sees, never widen past the tier. [`SensitiveBlocked`] is a
/// unit variant by design — it carries no content, so sensitive text cannot be
/// attached to an event that must never leave core.
///
/// [`SensitiveBlocked`]: PrivacyTier::SensitiveBlocked
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrivacyTier {
    /// Safe `Character`/`Line` content may be delivered.
    Public,
    /// Shape/timing only; no content.
    MetadataOnly,
    /// Content existed but is withheld.
    Redacted,
    /// Sensitive context; not even metadata leaves core.
    SensitiveBlocked,
}

/// The character payload of a [`PaneTyped`] event.
///
/// Each variant encodes the classified input context and its privacy tier in
/// one value. A character is only present in [`SafePublic`]; every other
/// context is unit-shaped and carries none.
///
/// [`SafePublic`]: TypedPayload::SafePublic
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypedPayload {
    /// Safe context (echo-enabled shell line mode): the printable character.
    SafePublic(char),
    /// Sensitive context (password heuristic or protected window): redacted.
    SensitiveRedacted,
    /// Alternate-screen application: metadata only, no character.
    AlternateScreenMetadataOnly,
    /// Raw/cbreak-mode application: metadata only, no character.
    RawModeMetadataOnly,
    /// Context could not be classified (fails closed): metadata only.
    UnknownMetadataOnly,
    /// Sensitive context that must not leave core: no content, not even metadata.
    SensitiveBlocked,
}

impl TypedPayload {
    /// The [`PrivacyTier`] this payload encodes. Delivery and filtering compare
    /// against this single mapping instead of re-matching the variants.
    #[must_use]
    pub const fn tier(&self) -> PrivacyTier {
        match self {
            TypedPayload::SafePublic(_) => PrivacyTier::Public,
            TypedPayload::SensitiveRedacted => PrivacyTier::Redacted,
            TypedPayload::AlternateScreenMetadataOnly
            | TypedPayload::RawModeMetadataOnly
            | TypedPayload::UnknownMetadataOnly => PrivacyTier::MetadataOnly,
            TypedPayload::SensitiveBlocked => PrivacyTier::SensitiveBlocked,
        }
    }
}

/// Payload for [`Event::PaneTyped`].
///
/// A privacy-gated domain event, not a raw key event: a character is only
/// present when the context was safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneTyped {
    /// The pane that received the input.
    pub pane_id: PaneId,
    /// The pane's tab.
    pub tab_id: TabId,
    /// The session.
    pub session_id: SessionId,
    /// The client that produced the input.
    pub client_id: ClientId,
    /// The classified, privacy-tiered character payload.
    pub payload: TypedPayload,
    /// When the input was accepted.
    pub timestamp: SystemTime,
}

/// The submitted-line payload of a [`PaneEnterPressed`] event.
///
/// As with [`TypedPayload`], each variant encodes context and tier together:
/// the line text is only present in [`SafePublic`].
///
/// [`SafePublic`]: SubmittedLinePayload::SafePublic
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubmittedLinePayload {
    /// Safe context: a reconstructed shell command line.
    SafePublic(String),
    /// Sensitive context (the line may contain a secret): redacted.
    SensitiveRedacted,
    /// The line could not be confidently reconstructed (fails closed): metadata only.
    UnknownMetadataOnly,
    /// Sensitive context that must not leave core: no content, not even metadata.
    SensitiveBlocked,
}

impl SubmittedLinePayload {
    /// The [`PrivacyTier`] this payload encodes. [`UnknownMetadataOnly`] fails
    /// closed: a line was submitted, but its content is never exposed.
    ///
    /// [`UnknownMetadataOnly`]: SubmittedLinePayload::UnknownMetadataOnly
    #[must_use]
    pub const fn tier(&self) -> PrivacyTier {
        match self {
            SubmittedLinePayload::SafePublic(_) => PrivacyTier::Public,
            SubmittedLinePayload::SensitiveRedacted => PrivacyTier::Redacted,
            SubmittedLinePayload::UnknownMetadataOnly => PrivacyTier::MetadataOnly,
            SubmittedLinePayload::SensitiveBlocked => PrivacyTier::SensitiveBlocked,
        }
    }
}

/// Payload for [`Event::PaneEnterPressed`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneEnterPressed {
    /// The pane that received Enter.
    pub pane_id: PaneId,
    /// The pane's tab.
    pub tab_id: TabId,
    /// The session.
    pub session_id: SessionId,
    /// The client that produced the input.
    pub client_id: ClientId,
    /// The classified, privacy-tiered submitted-line payload.
    pub line: SubmittedLinePayload,
    /// When Enter was accepted.
    pub timestamp: SystemTime,
}

// ============================================================================
// Mouse input
// ============================================================================

/// Payload for [`Event::MousePressed`].
///
/// Position is a client-local cell coordinate; the runtime never sees raw
/// screen coordinates. `pane` is `None` when the press landed on a Koshi-owned
/// region (border, tabline, statusline) rather than pane content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MousePressed {
    /// The client that produced the event.
    pub client_id: ClientId,
    /// The hit-tested pane, if any.
    pub pane: Option<PaneId>,
    /// The client-local cell position.
    pub position: Point,
    /// The button pressed.
    pub button: MouseButton,
}

/// Payload for [`Event::MouseReleased`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseReleased {
    /// The client that produced the event.
    pub client_id: ClientId,
    /// The hit-tested pane, if any.
    pub pane: Option<PaneId>,
    /// The client-local cell position.
    pub position: Point,
    /// The button released.
    pub button: MouseButton,
}

/// Payload for [`Event::MouseDragged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseDragged {
    /// The client that produced the event.
    pub client_id: ClientId,
    /// The hit-tested pane, if any.
    pub pane: Option<PaneId>,
    /// The client-local cell position.
    pub position: Point,
    /// The button held during the drag.
    pub button: MouseButton,
}

/// Payload for [`Event::MouseScrolled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseScrolled {
    /// The client that produced the event.
    pub client_id: ClientId,
    /// The hit-tested pane, if any.
    pub pane: Option<PaneId>,
    /// The client-local cell position.
    pub position: Point,
    /// The wheel direction.
    pub direction: ScrollDirection,
}

/// Payload for [`Event::PaneMouseForwarded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneMouseForwarded {
    /// The pane the encoded mouse sequence was sent to.
    pub pane_id: PaneId,
}

/// Payload for [`Event::PluginMouseInput`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginMouseInput {
    /// The plugin the mouse input was delivered to.
    pub plugin_id: PluginId,
}

// ============================================================================
// Shell integration (OSC 133 semantic prompts)
// ============================================================================

/// Payload for [`Event::PaneCommandStarted`].
///
/// Emitted when a shell reports it just ran a command via OSC 133;C, a
/// terminal escape sequence shells emit around each command for prompt
/// integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCommandStarted {
    /// The pane whose shell reported a command starting.
    pub pane_id: PaneId,
}

/// Payload for [`Event::PaneCommandFinished`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCommandFinished {
    /// The pane whose shell reported the command ending.
    pub pane_id: PaneId,
    /// The command's exit code, when the shell reports one.
    pub exit_code: Option<i32>,
}

// ============================================================================
// Delivery and rejection
// ============================================================================

/// Payload for [`Event::PaneScrollbackTruncated`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneScrollbackTruncated {
    /// The pane whose scrollback overflowed.
    pub pane_id: PaneId,
    /// How many lines were dropped from the bounded buffer.
    pub dropped_lines: u64,
    /// How many bytes were dropped from the bounded buffer.
    pub dropped_bytes: u64,
}

/// The delivery class of an event, used when reporting drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventClass {
    /// High-frequency, value-only events that may coalesce or drop.
    Lossy,
    /// State-transition facts that must not be silently lost.
    Critical,
}

/// Payload for [`Event::SubscriberLagged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriberLagged {
    /// The subscriber whose queue overflowed.
    pub subscriber_id: SubscriberId,
    /// How many events were dropped.
    pub dropped_count: u64,
    /// The class of the dropped events.
    pub event_class: EventClass,
}

/// Why a command was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    /// The resolved target disappeared before the mutation applied.
    TargetGone,
    /// An explicit target matched more than one entity; never guessed.
    TargetAmbiguous,
    /// An explicit target matched nothing.
    TargetNotFound,
    /// A client-scoped command's source client detached.
    SourceClientStale,
    /// A capability or authorization check failed.
    Unauthorized,
    /// The command is invalid in the current state.
    InvalidState,
    /// A resize would drop a pane below its minimum size.
    MinSize,
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectReason::TargetGone => f.write_str("target no longer exists"),
            RejectReason::TargetAmbiguous => {
                f.write_str("target matched more than one; specify an explicit id")
            }
            RejectReason::TargetNotFound => f.write_str("no target matched"),
            RejectReason::SourceClientStale => f.write_str("source client has detached"),
            RejectReason::Unauthorized => f.write_str("command not permitted"),
            RejectReason::InvalidState => f.write_str("invalid in the current state"),
            RejectReason::MinSize => f.write_str("below minimum size"),
        }
    }
}

/// Payload for [`Event::CommandRejected`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRejected {
    /// The command that was rejected.
    pub id: CommandId,
    /// Why it was rejected.
    pub reason: RejectReason,
}

// ============================================================================
// Selection and copy
// ============================================================================

/// Payload for [`Event::SelectionChanged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionChanged {
    /// The client whose selection changed. A selection belongs to one client:
    /// two clients viewing the same pane select independently, so the pane
    /// alone cannot say whose selection this is.
    pub client_id: ClientId,
    /// The pane the selection is in.
    pub pane_id: PaneId,
    /// The current selection, or `None` when cleared.
    pub selection: Option<Selection>,
}

/// Payload for [`Event::Copied`].
///
/// Carries only the byte length of the copied text, never the text itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Copied {
    /// The client that copied. A copy belongs to one client, and for
    /// [`CopyTarget::Osc52`] it names the terminal that received the escape:
    /// OSC 52 reaches one client's outer terminal, not every attached one.
    pub client_id: ClientId,
    /// The pane the text was copied from.
    pub pane_id: PaneId,
    /// Where the text was copied to.
    pub target: CopyTarget,
    /// The byte length of the copied text.
    pub byte_len: usize,
}

// ============================================================================
// Plugin lifecycle
// ============================================================================

/// Plugin lifecycle facts. Internal/runtime events; not delivered to plugins
/// without management-read capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginEvent {
    /// A plugin was installed.
    Installed(PluginInstalled),
    /// A plugin was uninstalled.
    Uninstalled(PluginUninstalled),
    /// A plugin was enabled.
    Enabled(PluginEnabled),
    /// A plugin was disabled.
    Disabled(PluginDisabled),
    /// A plugin was updated.
    Updated(PluginUpdated),
    /// A plugin was reloaded in place.
    Reloaded(PluginReloaded),
    /// A plugin failed to load.
    LoadFailed(PluginLoadFailed),
    /// A plugin was unloaded.
    Unloaded(PluginUnloaded),
    /// A plugin was marked broken after repeated failures.
    Broken(PluginBroken),
    /// A plugin doctor/diagnostic run completed.
    DoctorCompleted(PluginDoctorCompleted),
}

/// Payload for [`PluginEvent::Installed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInstalled {
    /// The installed plugin.
    pub plugin_id: PluginId,
}

/// Payload for [`PluginEvent::Uninstalled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginUninstalled {
    /// The uninstalled plugin.
    pub plugin_id: PluginId,
}

/// Payload for [`PluginEvent::Enabled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginEnabled {
    /// The enabled plugin.
    pub plugin_id: PluginId,
}

/// Payload for [`PluginEvent::Disabled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginDisabled {
    /// The disabled plugin.
    pub plugin_id: PluginId,
}

/// Payload for [`PluginEvent::Updated`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginUpdated {
    /// The updated plugin.
    pub plugin_id: PluginId,
}

/// Payload for [`PluginEvent::Reloaded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginReloaded {
    /// The reloaded plugin.
    pub plugin_id: PluginId,
}

/// Payload for [`PluginEvent::LoadFailed`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginLoadFailed {
    /// The plugin that failed to load.
    pub plugin_id: PluginId,
    /// A human-readable failure reason.
    pub reason: String,
}

/// Payload for [`PluginEvent::Unloaded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginUnloaded {
    /// The unloaded plugin.
    pub plugin_id: PluginId,
}

/// Payload for [`PluginEvent::Broken`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginBroken {
    /// The plugin marked broken.
    pub plugin_id: PluginId,
    /// A human-readable reason it was disabled.
    pub reason: String,
}

/// Payload for [`PluginEvent::DoctorCompleted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginDoctorCompleted {
    /// The plugin the diagnostic ran against.
    pub plugin_id: PluginId,
}

#[cfg(test)]
mod tests;
