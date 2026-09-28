//! Action vocabulary — the stable user surface.
//!
//! An *action* is what a user binds a key to or types as a CLI subcommand. It
//! is not a [`Command`](crate::command::Command): a `Command` is the runtime's
//! internal mutation type, an action is the public name config files use. Each
//! action maps to a command or to a viewer-local action.
//!
//! Every action lives in the `core:` namespace. This file holds the
//! primitives — [`ActionReference`], [`ActionMetadata`],
//! [`ActionHandlerReference`], and the static [`build_core_action_seeds`]
//! table. [`ActionRegistry`](crate::registry::ActionRegistry) loads that table
//! for lookup.

use crate::command::CommandKind;
use crate::geometry::Direction;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// The maximum length of an [`ActionName`], from the grammar
/// `^[a-z][a-z0-9-]{0,30}$` (1 leading letter + up to 30 trailing chars).
const MAX_ACTION_NAME_CHARACTER_COUNT: usize = 31;

/// Why a string is not a valid [`ActionName`].
///
/// Names follow `^[a-z][a-z0-9-]{0,30}$`: a lowercase-letter start, then up to
/// thirty more lowercase letters, digits, or hyphens. The display name shown to
/// users is free-form and lives separately in [`ActionMetadata`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionNameError {
    /// The name was empty.
    Empty,
    /// The name exceeded `MAX_ACTION_NAME_CHARACTER_COUNT` characters.
    TooLong {
        /// The offending length.
        character_count: usize,
    },
    /// The first character was not an ASCII lowercase letter.
    InvalidStart {
        /// The offending leading character.
        invalid_character: char,
    },
    /// A character after the first was outside `[a-z0-9-]`.
    InvalidChar {
        /// The offending character.
        invalid_character: char,
    },
}

impl fmt::Display for ActionNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionNameError::Empty => formatter.write_str("action name is empty"),
            ActionNameError::TooLong { character_count } => write!(
                formatter,
                "action name is {character_count} chars; the maximum is {MAX_ACTION_NAME_CHARACTER_COUNT}"
            ),
            ActionNameError::InvalidStart { invalid_character } => write!(
                formatter,
                "action name must start with a lowercase letter, found {invalid_character:?}"
            ),
            ActionNameError::InvalidChar { invalid_character } => {
                write!(
                    formatter,
                    "action name may only contain [a-z0-9-], found {invalid_character:?}"
                )
            }
        }
    }
}

impl std::error::Error for ActionNameError {}

/// The local name of an action within its namespace, validated against
/// `^[a-z][a-z0-9-]{0,30}$`.
///
/// [`ActionName::parse_action_name`] and deserialization (via
/// [`TryFrom<String>`]) both run the grammar check: a name decoded from a
/// config file or the IPC socket is always valid.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ActionName(String);

impl ActionName {
    /// Parse and validate `action_name` against the action-name grammar.
    ///
    /// # Errors
    /// Returns an [`ActionNameError`] describing the first rule the input
    /// violates.
    pub fn parse_action_name(action_name: &str) -> Result<Self, ActionNameError> {
        let mut remaining_characters = action_name.chars();
        let first_character = remaining_characters.next().ok_or(ActionNameError::Empty)?;
        if !first_character.is_ascii_lowercase() {
            return Err(ActionNameError::InvalidStart {
                invalid_character: first_character,
            });
        }
        // Every character after the first must be a lowercase letter, digit, or hyphen.
        for character in remaining_characters {
            if !(character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-') {
                return Err(ActionNameError::InvalidChar {
                    invalid_character: character,
                });
            }
        }
        // The length check runs after the charset scan: a name that is both
        // over-long and holds a bad character reports the bad character.
        let character_count = action_name.chars().count();
        if character_count > MAX_ACTION_NAME_CHARACTER_COUNT {
            return Err(ActionNameError::TooLong { character_count });
        }
        Ok(ActionName(action_name.to_string()))
    }

    /// Borrow the validated name.
    #[must_use]
    pub fn get_name(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ActionName {
    type Error = ActionNameError;

    fn try_from(action_name: String) -> Result<Self, Self::Error> {
        ActionName::parse_action_name(&action_name)
    }
}

impl From<ActionName> for String {
    fn from(action_name: ActionName) -> Self {
        action_name.0
    }
}

impl fmt::Display for ActionName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A reference to a built-in action.
///
/// `Display` renders the canonical wire form used everywhere an action is named
/// by string — config files and CLI output: `core:new-pane`. Serde reads and
/// writes that same string via [`FromStr`]: a keymap entry
/// `"<C-p>n" action="core:new-pane"` decodes to exactly this type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ActionReference {
    /// The action's name after the `core:` prefix.
    pub action_name: ActionName,
}

impl ActionReference {
    /// Build a reference to a built-in `core:` action.
    ///
    /// # Errors
    /// Returns an [`ActionNameError`] if `action_name` violates the grammar.
    pub fn from_core_action_name(action_name: &str) -> Result<Self, ActionNameError> {
        Ok(ActionReference {
            action_name: ActionName::parse_action_name(action_name)?,
        })
    }
}

impl fmt::Display for ActionReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "core:{}", self.action_name)
    }
}

/// Why a string is not a valid [`ActionReference`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionReferenceParseError {
    /// No `namespace:` prefix was present.
    MissingNamespace,
    /// The namespace prefix was not `core`.
    UnknownNamespace {
        /// The unrecognized prefix.
        unknown_namespace: String,
    },
    /// The name after `core:` failed the action-name grammar.
    InvalidActionName(ActionNameError),
}

impl fmt::Display for ActionReferenceParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionReferenceParseError::MissingNamespace => {
                formatter.write_str("action reference is missing a 'namespace:' prefix")
            }
            ActionReferenceParseError::UnknownNamespace { unknown_namespace } => write!(
                formatter,
                "unknown action namespace {unknown_namespace:?}; expected core"
            ),
            ActionReferenceParseError::InvalidActionName(action_name_error) => {
                write!(formatter, "{action_name_error}")
            }
        }
    }
}

impl std::error::Error for ActionReferenceParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ActionReferenceParseError::InvalidActionName(action_name_error) => {
                Some(action_name_error)
            }
            _ => None,
        }
    }
}

impl FromStr for ActionReference {
    type Err = ActionReferenceParseError;

    fn from_str(action_reference_text: &str) -> Result<Self, Self::Err> {
        let (namespace, action_name) = action_reference_text
            .split_once(':')
            .ok_or(ActionReferenceParseError::MissingNamespace)?;
        if namespace != "core" {
            return Err(ActionReferenceParseError::UnknownNamespace {
                unknown_namespace: namespace.to_string(),
            });
        }
        ActionReference::from_core_action_name(action_name)
            .map_err(ActionReferenceParseError::InvalidActionName)
    }
}

impl TryFrom<String> for ActionReference {
    type Error = ActionReferenceParseError;

    fn try_from(action_reference_text: String) -> Result<Self, Self::Error> {
        action_reference_text.parse()
    }
}

impl From<ActionReference> for String {
    fn from(action_reference: ActionReference) -> Self {
        action_reference.to_string()
    }
}

/// How broad an action's effect is. `koshi keys` and `koshi actions` output
/// print it as a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionScope {
    /// Acts on a pane within the current session.
    PaneSession,
    /// Acts on the issuing client (e.g. per-client view state).
    Client,
    /// Acts on a tab.
    Tab,
}

/// A kind of entity an action can target. [`ActionMetadata::target_kinds`]
/// lists the kinds an action accepts; `koshi actions explain` prints them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetKind {
    /// The session.
    Session,
    /// A tab.
    Tab,
    /// A pane.
    Pane,
    /// A client.
    Client,
}

/// How an action is dispatched once it fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionHandlerReference {
    /// Build and dispatch the named core [`Command`](crate::command::Command).
    CoreCommand(CommandKind),
    /// Run a viewer-local action without sending a session command.
    CoreClient(ClientActionKind),
}

/// A core action handled by the attached viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientActionKind {
    /// Open persistent pane placement mode for the focused pane.
    BeginPanePlacement,
    /// Select the visible pane target in `direction` during pane placement mode.
    SelectPaneTarget(Direction),
    /// Select the insertion edge in `direction` during pane placement mode.
    SelectPaneInsertion(Direction),
    /// Cycle the available insertion spans during pane placement mode.
    CyclePanePlacementSpan,
    /// Preview the next visible tab during pane placement mode.
    SelectNextPlacementTab,
    /// Preview the previous visible tab during pane placement mode.
    SelectPreviousPlacementTab,
    /// Confirm the selected pane placement.
    ConfirmPanePlacement,
    /// Cancel pane placement and return to the base input mode.
    CancelPanePlacement,
}

/// Everything the registry knows about one action: how to show it, what it can
/// target, and how to dispatch it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionMetadata {
    /// Human-facing name, e.g. "Create Pane to the Right".
    pub display_name: String,
    /// One-line description for `describe`/which-key output.
    pub description: String,
    /// How broad the action's effect is.
    pub scope: ActionScope,
    /// Entity kinds the action can target.
    pub target_kinds: Vec<TargetKind>,
    /// How the action is dispatched.
    pub handler: ActionHandlerReference,
    /// Whether the action repeats from a held prefix: fired from a
    /// multi-chord binding, the binding's prefix stays armed and the next
    /// chord alone fires again (`<C-s> h h h` resizes three times). Declared
    /// per action here, never in a binding.
    pub is_continuous: bool,
}

/// Build one `core:` seed entry with `is_continuous` `false`.
///
/// # Panics
/// Panics if `action_name` violates the action-name grammar.
fn build_core_action_seed(
    action_name: &'static str,
    display_name: &str,
    description: &str,
    scope: ActionScope,
    target_kinds: Vec<TargetKind>,
    handler: ActionHandlerReference,
) -> (ActionReference, ActionMetadata) {
    let action_reference = ActionReference::from_core_action_name(action_name)
        .expect("core seed action name must satisfy the action-name grammar");
    let action_metadata = ActionMetadata {
        display_name: display_name.to_string(),
        description: description.to_string(),
        scope,
        target_kinds,
        handler,
        is_continuous: false,
    };
    (action_reference, action_metadata)
}

/// The hint-bar label for `core:mouse-select` while the mode is **off**, and
/// the action's registry display name.
pub const MOUSE_SELECT_HINT: &str = "Mouse Select";

/// The hint-bar label for `core:mouse-select` while the mode is **on**. The
/// viewer swaps [`MOUSE_SELECT_HINT`] for this as it paints each frame, for as
/// long as mouse-select is on.
pub const MOUSE_UNSELECT_HINT: &str = "Mouse Unselect";

/// The built-in action table, loaded into the runtime registry at startup.
/// `koshi actions list` prints the entries in this order.
///
/// The `begin-pane-placement` action starts the viewer-local pane placement
/// mode. Its placement actions select and confirm viewer-local targets; the
/// other command-backed actions use values their NAME bakes into the command
/// the resolver builds — `lock`/`unlock` both build `SetLockMode`; the
/// `new-pane-*`, `focus-pane-*`, and `resize-pane-*` families each build their
/// family's command with the named direction; `next-tab`/`previous-tab`/
/// `focus-tab` all build `FocusTab`.
///
/// The `resize-pane*`, `focus-pane*`, and `scroll-pane-up`/`scroll-pane-down`
/// actions are `is_continuous`; every other action is not.
#[must_use]
pub fn build_core_action_seeds() -> Vec<(ActionReference, ActionMetadata)> {
    use ActionHandlerReference::{CoreClient, CoreCommand};
    use ActionScope::{Client, PaneSession, Tab};
    use TargetKind::{Client as ClientTarget, Pane, Session, Tab as TabTarget};

    let mut action_seeds = vec![
        // --- Panes ---
        build_core_action_seed(
            "new-pane",
            "New Pane",
            "Split the focused pane and start a shell in the new one",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
        ),
        build_core_action_seed(
            "new-pane-left",
            "New Pane Left",
            "Split the focused pane and open the new one on the left",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
        ),
        build_core_action_seed(
            "new-pane-down",
            "New Pane Down",
            "Split the focused pane and open the new one below",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
        ),
        build_core_action_seed(
            "new-pane-up",
            "New Pane Up",
            "Split the focused pane and open the new one above",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
        ),
        build_core_action_seed(
            "new-pane-right",
            "New Pane Right",
            "Split the focused pane and open the new one on the right",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
        ),
        build_core_action_seed(
            "new-pane-stacked",
            "New Stacked Pane",
            "Add a new pane to the focused pane's stack, sharing its space",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
        ),
        build_core_action_seed(
            "close-pane",
            "Close Pane",
            "Close the focused pane",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ClosePane),
        ),
        build_core_action_seed(
            "close-pane-tree",
            "Close Pane Tree",
            "Close the focused pane and kill every process it started",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ClosePane),
        ),
        build_core_action_seed(
            "resize-pane",
            "Resize Pane",
            "Grow or shrink the focused pane along one edge",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
        ),
        build_core_action_seed(
            "resize-pane-left",
            "Resize Pane Left",
            "Move the focused pane's border one cell to the left",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
        ),
        build_core_action_seed(
            "resize-pane-down",
            "Resize Pane Down",
            "Move the focused pane's border one cell down",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
        ),
        build_core_action_seed(
            "resize-pane-up",
            "Resize Pane Up",
            "Move the focused pane's border one cell up",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
        ),
        build_core_action_seed(
            "resize-pane-right",
            "Resize Pane Right",
            "Move the focused pane's border one cell to the right",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
        ),
        build_core_action_seed(
            "begin-pane-placement",
            "Begin Pane Placement",
            "Open pane placement mode for the focused pane",
            PaneSession,
            vec![Pane],
            CoreClient(ClientActionKind::BeginPanePlacement),
        ),
        build_core_action_seed(
            "select-pane-target-left",
            "Select Pane Target Left",
            "Select the visible pane target to the left in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPaneTarget(Direction::Left)),
        ),
        build_core_action_seed(
            "select-pane-target-down",
            "Select Pane Target Down",
            "Select the visible pane target below in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPaneTarget(Direction::Down)),
        ),
        build_core_action_seed(
            "select-pane-target-up",
            "Select Pane Target Up",
            "Select the visible pane target above in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPaneTarget(Direction::Up)),
        ),
        build_core_action_seed(
            "select-pane-target-right",
            "Select Pane Target Right",
            "Select the visible pane target to the right in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPaneTarget(Direction::Right)),
        ),
        build_core_action_seed(
            "select-pane-insertion-left",
            "Select Pane Insertion Left",
            "Select the left insertion edge in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPaneInsertion(Direction::Left)),
        ),
        build_core_action_seed(
            "select-pane-insertion-down",
            "Select Pane Insertion Down",
            "Select the lower insertion edge in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPaneInsertion(Direction::Down)),
        ),
        build_core_action_seed(
            "select-pane-insertion-up",
            "Select Pane Insertion Up",
            "Select the upper insertion edge in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPaneInsertion(Direction::Up)),
        ),
        build_core_action_seed(
            "select-pane-insertion-right",
            "Select Pane Insertion Right",
            "Select the right insertion edge in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPaneInsertion(Direction::Right)),
        ),
        build_core_action_seed(
            "cycle-pane-placement-span",
            "Cycle Pane Placement Span",
            "Cycle the available insertion spans in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::CyclePanePlacementSpan),
        ),
        build_core_action_seed(
            "select-next-placement-tab",
            "Select Next Placement Tab",
            "Preview the next visible tab in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectNextPlacementTab),
        ),
        build_core_action_seed(
            "select-previous-placement-tab",
            "Select Previous Placement Tab",
            "Preview the previous visible tab in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPreviousPlacementTab),
        ),
        build_core_action_seed(
            "confirm-pane-placement",
            "Confirm Pane Placement",
            "Submit the selected pane placement in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::ConfirmPanePlacement),
        ),
        build_core_action_seed(
            "cancel-pane-placement",
            "Cancel Pane Placement",
            "Discard the pane placement and return to the base input mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::CancelPanePlacement),
        ),
        build_core_action_seed(
            "move-pane",
            "Move Pane",
            "Swap a pane with its visible neighbor in one direction and commit the swap at once",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::MovePane),
        ),
        build_core_action_seed(
            "place-pane",
            "Place Pane",
            "Insert a pane into another tab's tiled layout",
            PaneSession,
            vec![Pane, TabTarget],
            CoreCommand(CommandKind::PlacePane),
        ),
        build_core_action_seed(
            "focus-pane",
            "Focus Pane",
            "Move the issuing client's focus to a pane",
            Client,
            vec![Pane, ClientTarget],
            CoreCommand(CommandKind::FocusPane),
        ),
        build_core_action_seed(
            "focus-pane-left",
            "Focus Pane Left",
            "Move the issuing client's focus to the pane on the left",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
        ),
        build_core_action_seed(
            "focus-pane-down",
            "Focus Pane Down",
            "Move the issuing client's focus to the pane below",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
        ),
        build_core_action_seed(
            "focus-pane-up",
            "Focus Pane Up",
            "Move the issuing client's focus to the pane above",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
        ),
        build_core_action_seed(
            "focus-pane-right",
            "Focus Pane Right",
            "Move the issuing client's focus to the pane on the right",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
        ),
        build_core_action_seed(
            "scroll-pane",
            "Scroll Pane",
            "Scroll a pane's view by a chosen number of lines",
            Client,
            vec![ClientTarget, Pane],
            CoreCommand(CommandKind::ScrollPane),
        ),
        build_core_action_seed(
            "scroll-pane-up",
            "Scroll Pane Up",
            "Scroll the focused pane's view toward its history",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::ScrollPane),
        ),
        build_core_action_seed(
            "scroll-pane-down",
            "Scroll Pane Down",
            "Scroll the focused pane's view toward live output",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::ScrollPane),
        ),
        build_core_action_seed(
            "toggle-pane-fullscreen",
            "Toggle Pane Fullscreen",
            "Toggle fullscreen for the focused pane",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::TogglePaneFullscreen),
        ),
        build_core_action_seed(
            "write-to-pane",
            "Write To Pane",
            "Send text to a pane's shell, as if it had been typed there",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::WriteToPane),
        ),
        // --- Tabs ---
        build_core_action_seed(
            "new-tab",
            "New Tab",
            "Create a new tab",
            Tab,
            vec![TabTarget],
            CoreCommand(CommandKind::NewTab),
        ),
        build_core_action_seed(
            "close-tab",
            "Close Tab",
            "Close the focused tab",
            Tab,
            vec![TabTarget],
            CoreCommand(CommandKind::CloseTab),
        ),
        build_core_action_seed(
            "focus-tab",
            "Focus Tab",
            "Switch the issuing client's view to a specific tab",
            Client,
            vec![TabTarget, ClientTarget],
            CoreCommand(CommandKind::FocusTab),
        ),
        build_core_action_seed(
            "next-tab",
            "Next Tab",
            "Switch the issuing client's view to the next tab",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusTab),
        ),
        build_core_action_seed(
            "previous-tab",
            "Previous Tab",
            "Switch the issuing client's view to the previous tab",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusTab),
        ),
        build_core_action_seed(
            "move-tab",
            "Move Tab",
            "Move the focused tab to a new index",
            Tab,
            vec![TabTarget],
            CoreCommand(CommandKind::MoveTab),
        ),
        // --- Session ---
        build_core_action_seed(
            "quit",
            "Quit",
            "Leave the session, ending it when auto-close-session is on and no other client stays",
            Client,
            vec![ClientTarget, Session],
            CoreCommand(CommandKind::Quit),
        ),
        // --- Lock mode ---
        build_core_action_seed(
            "toggle-lock",
            "Toggle Lock",
            "Toggle pass-through lock mode for the issuing client",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::ToggleLockMode),
        ),
        build_core_action_seed(
            "lock",
            "Lock",
            "Enable pass-through lock mode for the issuing client",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::SetLockMode),
        ),
        build_core_action_seed(
            "unlock",
            "Unlock",
            "Disable pass-through lock mode for the issuing client",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::SetLockMode),
        ),
        // --- Mouse select ---
        build_core_action_seed(
            "mouse-select",
            MOUSE_SELECT_HINT,
            "Toggle grabbing the mouse for text selection, so a drag highlights \
             in koshi even over a program that asked for the mouse",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::ToggleMouseSelect),
        ),
        // --- Run ---
        build_core_action_seed(
            "run",
            "Run Command",
            "Spawn a command in a new pane",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::RunCommandPane),
        ),
    ];

    // Repeat-from-prefix actions: fired from a multi-chord binding, the
    // prefix stays armed and the next chord alone fires again (`<C-s> h h h`,
    // `<C-p> ← ← ←`).
    for (action_reference, action_metadata) in &mut action_seeds {
        if matches!(
            action_reference.action_name.get_name(),
            "resize-pane"
                | "resize-pane-left"
                | "resize-pane-down"
                | "resize-pane-up"
                | "resize-pane-right"
                | "focus-pane"
                | "focus-pane-left"
                | "focus-pane-down"
                | "focus-pane-up"
                | "focus-pane-right"
                | "scroll-pane-up"
                | "scroll-pane-down"
        ) {
            action_metadata.is_continuous = true;
        }
    }

    action_seeds
}

#[cfg(test)]
mod tests;
