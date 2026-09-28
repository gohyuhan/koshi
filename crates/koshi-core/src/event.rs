//! Canonical event vocabulary.
//!
//! [`Event`] and its nested enums are the single source of truth for every
//! completed fact the runtime emits. Like [`crate::command`], these are pure
//! data shells: no handlers, no behaviour, no runtime state. Emission,
//! delivery, backpressure, and privacy gating all live in higher layers; this
//! module only names *what* has happened.
//!
//! Events are append-only facts; none requests a mutation. Every variant and
//! payload holds only serde-friendly types that mean the same thing in another
//! process (IPC watchers, storage). Timestamps are `SystemTime`, never
//! `Instant`. No raw OS handles and no `&mut` references.

use crate::command::PanePlacementTarget;
use crate::geometry::{PaneArea, Size};
use crate::ids::{ClientId, CommandId, PaneId, SessionId, SubscriberId, TabId};
use crate::lock::LockMode;
use crate::process::PtySize;
use crate::selection::Selection;
use serde::{Deserialize, Serialize};

/// A completed fact emitted by the runtime.
///
/// Variants are grouped to match the sections further down the file: pane/tab
/// lifecycle, input modes, shell integration, selection, and session
/// lifecycle. Each variant wraps a like-named
/// payload struct. `Quit` wraps its [`QuitCause`]; `Restarting` carries
/// nothing.
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
    /// A tab's layout tree changed.
    LayoutChanged(LayoutChanged),
    /// A checked pane placement committed in the session.
    PanePlacementCommitted(PanePlacementCommitted),
    /// A tab was created.
    TabCreated(TabCreated),
    /// A tab was closed.
    TabClosed(TabClosed),
    /// Focus moved to a tab.
    TabFocused(TabFocused),
    /// A tab moved to a new index.
    TabMoved(TabMoved),
    /// A client has no visible pane: every pane in its tab is suppressed.
    TerminalTooSmallEntered(TerminalTooSmallEntered),
    /// Configuration reload succeeded and was atomically swapped in.
    ConfigReloaded(ConfigReloaded),

    // Input modes.
    /// The active input mode changed (normal or locked).
    InputModeChanged(InputModeChanged),
    /// A client's mouse-select mode was turned on or off.
    MouseSelectChanged(MouseSelectChanged),

    // Shell integration (OSC 133 semantic prompts).
    /// A command began running in a pane (OSC 133;C). Carries no command text.
    PaneCommandStarted(PaneCommandStarted),
    /// A command finished in a pane (OSC 133;D), with the exit code when the
    /// shell reports one. Carries no command text.
    PaneCommandFinished(PaneCommandFinished),

    // Selection.
    /// The active selection changed or was cleared. A selection appearing
    /// enters visual mode; a selection clearing leaves it. No other event
    /// reports entering or leaving visual mode.
    SelectionChanged(SelectionChanged),

    // Session lifecycle.
    /// The session is over. The payload names what ended it: a quit request,
    /// or the last tab closing, with the child exit that emptied it when one
    /// did. A terminal event — nothing follows it.
    Quit(QuitCause),
    /// The session server is replacing its own process image with the binary
    /// now on disk. The session, its panes and their child processes stay as
    /// they are; only the process running them changes. A terminal event —
    /// nothing follows it.
    Restarting,
}

impl Event {
    /// The variant's name, e.g. `"PaneCreated"`. Contains nothing from the
    /// payload, including user text.
    #[must_use]
    pub fn get_event_name(&self) -> &'static str {
        match self {
            Event::PaneCreated(_) => "PaneCreated",
            Event::PaneProcessExited(_) => "PaneProcessExited",
            Event::PaneClosing(_) => "PaneClosing",
            Event::PaneRemoved(_) => "PaneRemoved",
            Event::PaneFocused(_) => "PaneFocused",
            Event::PtyResized(_) => "PtyResized",
            Event::LayoutChanged(_) => "LayoutChanged",
            Event::PanePlacementCommitted(_) => "PanePlacementCommitted",
            Event::TabCreated(_) => "TabCreated",
            Event::TabClosed(_) => "TabClosed",
            Event::TabFocused(_) => "TabFocused",
            Event::TabMoved(_) => "TabMoved",
            Event::TerminalTooSmallEntered(_) => "TerminalTooSmallEntered",
            Event::ConfigReloaded(_) => "ConfigReloaded",
            Event::InputModeChanged(_) => "InputModeChanged",
            Event::MouseSelectChanged(_) => "MouseSelectChanged",
            Event::PaneCommandStarted(_) => "PaneCommandStarted",
            Event::PaneCommandFinished(_) => "PaneCommandFinished",
            Event::SelectionChanged(_) => "SelectionChanged",
            Event::Quit(_) => "Quit",
            Event::Restarting => "Restarting",
        }
    }
}

// ============================================================================
// Session lifecycle
// ============================================================================

/// Payload for [`Event::Quit`]: what ended the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuitCause {
    /// A quit was asked for: `core:quit` from a source naming no client,
    /// `kill-session`, the last client leaving under `auto-close-session`, or
    /// the session process being told to stop.
    Requested,
    /// The session's last tab closed.
    LastTabClosed {
        /// The tab that closed.
        tab_id: TabId,
        /// The child exit that emptied the tab, when a process exit started
        /// the close. `None` when a command closed the pane or the tab.
        pane_exit: Option<PaneProcessExited>,
    },
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
    /// The process exit code; `None` when a signal terminated the process.
    pub exit_code: Option<i32>,
    /// The signal number that terminated the process; `None` when the process
    /// exited with a code. Exactly one of `exit_code` and `signal` is `Some`.
    /// Always `None` on Windows. `Some(0)` is a signal whose number the
    /// platform did not report. Absent from serialized input decodes as `None`.
    #[serde(default)]
    pub signal: Option<i32>,
}

impl PaneProcessExited {
    /// `true` unless the process exited with code `0` and no signal: a non-zero
    /// code, any present `signal`, and an unobserved exit (`exit_code:
    /// Some(-1)`) are all failures. A value carrying both `exit_code: Some(0)`
    /// and a `signal` is a failure.
    #[must_use]
    pub fn is_failure(&self) -> bool {
        self.signal.is_some() || self.exit_code != Some(0)
    }
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
    pub previous_pane_id: Option<PaneId>,
}

/// Payload for [`Event::PtyResized`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtyResized {
    /// The pane whose PTY was resized.
    pub pane_id: PaneId,
    /// The new PTY dimensions in cells.
    pub pty_size: PtySize,
}

/// Payload for [`Event::LayoutChanged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutChanged {
    /// The tab whose layout tree changed.
    pub tab_id: TabId,
}

/// Payload for [`Event::PanePlacementCommitted`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanePlacementCommitted {
    /// The command whose transaction committed this placement.
    pub command_id: CommandId,
    /// The pane placed in the destination layout.
    pub source_pane_id: PaneId,
    /// The tab that owned the pane before the placement.
    pub source_tab_id: TabId,
    /// The tab that owns the pane after the placement.
    pub destination_tab_id: TabId,
    /// The checked swap or insertion target used by the committed transaction.
    pub placement_target: PanePlacementTarget,
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
/// The active tab is per-client state; `client_id` names the client whose
/// view switched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabFocused {
    /// The client whose active tab changed.
    pub client_id: ClientId,
    /// The newly focused tab.
    pub tab_id: TabId,
    /// The tab the client was viewing before the switch. When the switch was
    /// forced by a tab close, this is the closed tab.
    pub previous_tab_id: TabId,
}

/// Payload for [`Event::TabMoved`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabMoved {
    /// The moved tab.
    pub tab_id: TabId,
    /// The tab's previous zero-based index.
    pub previous_tab_index: usize,
    /// The tab's new zero-based index.
    pub new_tab_index: usize,
}

/// Why a client has no visible pane area.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalTooSmallCause {
    /// The client's viewport cannot fit the built-in chrome and pane minimum.
    #[default]
    Terminal,
    /// The client's own edge regions leave no pane area.
    Regions,
    /// Another client's pane area sets the shared minimum.
    OtherClient(ClientId),
}

/// Payload for [`Event::TerminalTooSmallEntered`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalTooSmallEntered {
    /// The affected client.
    pub client_id: ClientId,
    /// The viewport size that could not fit any pane.
    pub viewport_size: Size,
    /// The pane area the client reported, or `None` when it reported no area.
    #[serde(default)]
    pub pane_area: Option<PaneArea>,
    /// The reason the client has no visible pane area.
    #[serde(default)]
    pub cause: TerminalTooSmallCause,
}

/// Payload for [`Event::ConfigReloaded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigReloaded {
    /// The session whose config was reloaded.
    pub session_id: SessionId,
}

// ============================================================================
// Input modes
// ============================================================================

/// Payload for [`Event::InputModeChanged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputModeChanged {
    /// The client whose input mode changed. Lock mode is client-scoped:
    /// clients sharing a session hold independent modes.
    pub client_id: ClientId,
    /// The mode now in effect.
    ///
    /// Visual mode is not one of these: a highlight changes nothing about how a
    /// key is interpreted. A key bound to a koshi shortcut still fires it; a key
    /// that reaches the pane's program clears that pane's highlight on the way.
    /// The highlight itself is reported by [`Event::SelectionChanged`].
    pub lock_mode: LockMode,
}

/// Payload for [`Event::MouseSelectChanged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseSelectChanged {
    /// The client whose mouse-select mode changed. Mouse select is
    /// client-scoped: clients sharing a session hold independent modes.
    pub client_id: ClientId,
    /// Whether the client now grabs the mouse for text selection.
    pub is_enabled: bool,
}

// ============================================================================
// Shell integration (OSC 133 semantic prompts)
// ============================================================================

/// Payload for [`Event::PaneCommandStarted`].
///
/// Emitted when a shell reports that a command started, via OSC 133;C — a
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
// Delivery and rejection reasons
// ============================================================================

/// A subscriber's report of the deliveries it missed while its queue was full,
/// carried by the snapshot that returns it to live delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriberLagged {
    /// The subscriber whose queue overflowed.
    pub subscriber_id: SubscriberId,
    /// How many deliveries were dropped.
    pub dropped_event_count: u64,
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
    MinimumSize,
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectReason::TargetGone => formatter.write_str("target no longer exists"),
            RejectReason::TargetAmbiguous => {
                formatter.write_str("target matched more than one; specify an explicit id")
            }
            RejectReason::TargetNotFound => formatter.write_str("no target matched"),
            RejectReason::SourceClientStale => formatter.write_str("source client has detached"),
            RejectReason::Unauthorized => formatter.write_str("command not permitted"),
            RejectReason::InvalidState => formatter.write_str("invalid in the current state"),
            RejectReason::MinimumSize => formatter.write_str("below minimum size"),
        }
    }
}

// ============================================================================
// Selection and copy
// ============================================================================

/// Payload for [`Event::SelectionChanged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionChanged {
    /// The client whose selection changed. A selection belongs to one client:
    /// two clients viewing the same pane select independently.
    pub client_id: ClientId,
    /// The pane the selection is in.
    pub pane_id: PaneId,
    /// The current selection, or `None` when cleared.
    pub selection: Option<Selection>,
}

#[cfg(test)]
pub(crate) mod tests;
