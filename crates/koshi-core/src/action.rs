//! Action vocabulary — the stable, plugin-extensible user surface.
//!
//! An *action* is what a user binds a key to, types as a CLI subcommand, or a
//! plugin contributes. It is not a [`Command`](crate::command::Command): a
//! `Command` is the runtime's internal mutation type, an action is the public
//! name config files and the plugin SDK use. Each action maps to a command.
//!
//! The action set is open. Built-in actions live in the `core:` namespace,
//! plugins own `plugin:<id>:*`, and `user:` is reserved for user-defined
//! macros. This file holds the primitives — [`ActionReference`], [`ActionNamespace`],
//! [`ActionMetadata`], [`ActionHandlerReference`], and the static
//! [`build_core_action_seeds`] table. The mutable runtime table that loads those seeds
//! and accepts plugin registrations is
//! [`ActionRegistry`](crate::registry::ActionRegistry).

use crate::command::CommandKind;
use crate::ids::PluginId;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

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
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionNameError::Empty => f.write_str("action name is empty"),
            ActionNameError::TooLong { character_count } => write!(
                f,
                "action name is {character_count} chars; the maximum is {MAX_ACTION_NAME_CHARACTER_COUNT}"
            ),
            ActionNameError::InvalidStart { invalid_character } => write!(
                f,
                "action name must start with a lowercase letter, found {invalid_character:?}"
            ),
            ActionNameError::InvalidChar { invalid_character } => {
                write!(
                    f,
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
/// config file, the IPC socket, or a plugin is always valid.
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
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which family an action belongs to. A `core:` and a `plugin:` action with
/// the same local name are two different actions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ActionNamespace {
    /// Built-in actions shipped by Koshi. Plugins may never register here.
    Core,
    /// Actions contributed by a plugin; the id scopes the name.
    Plugin(PluginId),
    /// Reserved for user-defined macros.
    User,
}

/// A fully-qualified reference to an action: its namespace plus local name.
///
/// `Display` renders the canonical wire form used everywhere an action is named
/// by string — config files, CLI output, and plugin messages: `core:new-pane`,
/// `plugin:<uuid>:open-status`, `user:my-macro`. Serde reads and writes that
/// same string (not a `{namespace, name}` struct) via [`FromStr`]: a keymap
/// entry `"<C-p>n" action="core:new-pane"` decodes to exactly this type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ActionReference {
    /// The namespace that owns the action.
    pub namespace: ActionNamespace,
    /// The local name within the namespace.
    pub action_name: ActionName,
}

impl ActionReference {
    /// Build a reference to a built-in `core:` action.
    ///
    /// # Errors
    /// Returns an [`ActionNameError`] if `action_name` violates the grammar.
    pub fn from_core_action_name(action_name: &str) -> Result<Self, ActionNameError> {
        Ok(ActionReference {
            namespace: ActionNamespace::Core,
            action_name: ActionName::parse_action_name(action_name)?,
        })
    }

    /// Build a reference to an action owned by `plugin_id`.
    ///
    /// # Errors
    /// Returns an [`ActionNameError`] if `action_name` violates the grammar.
    pub fn from_plugin_action_name(
        plugin_id: PluginId,
        action_name: &str,
    ) -> Result<Self, ActionNameError> {
        Ok(ActionReference {
            namespace: ActionNamespace::Plugin(plugin_id),
            action_name: ActionName::parse_action_name(action_name)?,
        })
    }

    /// Build a reference to a `user:` macro action.
    ///
    /// # Errors
    /// Returns an [`ActionNameError`] if `action_name` violates the grammar.
    pub fn from_user_action_name(action_name: &str) -> Result<Self, ActionNameError> {
        Ok(ActionReference {
            namespace: ActionNamespace::User,
            action_name: ActionName::parse_action_name(action_name)?,
        })
    }
}

impl fmt::Display for ActionReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.namespace {
            ActionNamespace::Core => write!(f, "core:{}", self.action_name),
            ActionNamespace::Plugin(plugin_id) => {
                write!(f, "plugin:{}:{}", plugin_id.get_uuid(), self.action_name)
            }
            ActionNamespace::User => write!(f, "user:{}", self.action_name),
        }
    }
}

/// Why a string is not a valid [`ActionReference`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionReferenceParseError {
    /// No `namespace:` prefix was present.
    MissingNamespace,
    /// The namespace prefix was not one of `core`, `plugin`, or `user`.
    UnknownNamespace {
        /// The unrecognized prefix.
        unknown_namespace: String,
    },
    /// A `plugin:` reference was missing the `:<name>` after its id.
    MissingPluginName,
    /// A `plugin:` reference's id was not a valid UUID.
    InvalidPluginId,
    /// The local name failed the action-name grammar.
    InvalidActionName(ActionNameError),
}

impl fmt::Display for ActionReferenceParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionReferenceParseError::MissingNamespace => {
                f.write_str("action reference is missing a 'namespace:' prefix")
            }
            ActionReferenceParseError::UnknownNamespace { unknown_namespace } => write!(
                f,
                "unknown action namespace {unknown_namespace:?}; expected core, plugin, or user"
            ),
            ActionReferenceParseError::MissingPluginName => {
                f.write_str("plugin action reference must be 'plugin:<uuid>:<name>'")
            }
            ActionReferenceParseError::InvalidPluginId => {
                f.write_str("plugin action reference has an invalid UUID")
            }
            ActionReferenceParseError::InvalidActionName(action_name_error) => {
                write!(f, "{action_name_error}")
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
        // Split off the "core"/"user"/"plugin" prefix from everything after its colon.
        let (namespace, reference_tail) = action_reference_text
            .split_once(':')
            .ok_or(ActionReferenceParseError::MissingNamespace)?;
        match namespace {
            "core" => ActionReference::from_core_action_name(reference_tail)
                .map_err(ActionReferenceParseError::InvalidActionName),
            "user" => ActionReference::from_user_action_name(reference_tail)
                .map_err(ActionReferenceParseError::InvalidActionName),
            "plugin" => {
                // A plugin reference has one more segment than core/user: "<uuid>:<name>".
                let (plugin_id_text, action_name) = reference_tail
                    .split_once(':')
                    .ok_or(ActionReferenceParseError::MissingPluginName)?;
                let plugin_uuid = Uuid::parse_str(plugin_id_text)
                    .map_err(|_| ActionReferenceParseError::InvalidPluginId)?;
                ActionReference::from_plugin_action_name(
                    PluginId::from_uuid(plugin_uuid),
                    action_name,
                )
                .map_err(ActionReferenceParseError::InvalidActionName)
            }
            unknown_namespace => Err(ActionReferenceParseError::UnknownNamespace {
                unknown_namespace: unknown_namespace.to_string(),
            }),
        }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ActionScope {
    /// Acts on a pane within the current session.
    PaneSession,
    /// Acts on the issuing client (e.g. per-client view state).
    Client,
    /// Acts on a tab.
    Tab,
    /// Acts on the whole session/instance.
    Global,
}

/// A kind of entity an action can target. [`ActionMetadata::target_kinds`]
/// lists the kinds an action accepts; `koshi actions explain` prints them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// Whether the runtime implements an action. It serializes with the variant
/// names `Available` and `ComingSoon`.
///
/// Introspection (`koshi actions list`/`explain`) hides `ComingSoon` actions,
/// and resolving one is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ActionStatus {
    /// The runtime implements this action; binding and invoking it work.
    Available,
    /// The action is seeded but the runtime has no handler for it.
    ComingSoon,
}

/// How an action is dispatched once it fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionHandlerReference {
    /// Build and dispatch the named core [`Command`](crate::command::Command).
    CoreCommand(CommandKind),
    /// Route to a plugin via a host command request.
    PluginHostCall(PluginId),
    /// Fire a sequence of actions in order (a macro); halts on first failure.
    Sequence(Vec<ActionReference>),
}

/// Everything the registry knows about one action: how to show it, what it can
/// target, and how to dispatch it.
///
/// `namespace` repeats the owning [`ActionReference`]'s namespace, so metadata handed
/// out on its own still names its owner.
/// [`ActionRegistry::register_action`](crate::registry::ActionRegistry::register_action)
/// refuses an entry whose two namespaces disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionMetadata {
    /// The namespace the action belongs to.
    pub namespace: ActionNamespace,
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
    /// Whether the runtime implements the action.
    pub action_status: ActionStatus,
    /// Whether the action repeats from a held prefix: fired from a
    /// multi-chord binding, the binding's prefix stays armed and the next
    /// chord alone fires again (`<C-s> h h h` resizes three times). Declared
    /// per action here, never in a binding. Absent on the wire means `false`.
    #[serde(default)]
    pub is_continuous: bool,
}

/// Build one `core:` seed entry, with `namespace` set to
/// [`ActionNamespace::Core`] and `is_continuous` `false`.
///
/// # Panics
/// Panics if `name` violates the action-name grammar.
fn build_core_action_seed(
    action_name: &'static str,
    display_name: &str,
    description: &str,
    scope: ActionScope,
    target_kinds: Vec<TargetKind>,
    handler: ActionHandlerReference,
    action_status: ActionStatus,
) -> (ActionReference, ActionMetadata) {
    let action_reference = ActionReference::from_core_action_name(action_name)
        .expect("core seed action name must satisfy the action-name grammar");
    let metadata = ActionMetadata {
        namespace: ActionNamespace::Core,
        display_name: display_name.to_string(),
        description: description.to_string(),
        scope,
        target_kinds,
        handler,
        action_status,
        is_continuous: false,
    };
    (action_reference, metadata)
}

/// The hint-bar label for `core:mouse-select` while the mode is **off**, and
/// the action's registry display name.
pub const MOUSE_SELECT_HINT: &str = "Mouse Select";

/// The hint-bar label for `core:mouse-select` while the mode is **on**. The
/// viewer swaps [`MOUSE_SELECT_HINT`] for this as it paints each frame, for as
/// long as mouse-select is on.
pub const MOUSE_UNSELECT_HINT: &str = "Mouse Unselect";

/// The built-in action table, loaded into the runtime registry at startup.
/// `koshi actions list` prints the `Available` entries in this order.
///
/// Every entry is in the `core:` namespace. Actions sharing a [`CommandKind`]
/// differ only by the values their NAME bakes into the command the resolver
/// builds — `lock`/`unlock` both build `SetLockMode`; the `new-pane-*`,
/// `focus-pane-*`, and `resize-pane-*` families each build their family's
/// command with the named direction; `next-tab`/`previous-tab`/`focus-tab`
/// all build `FocusTab`.
///
/// The `copy-selection` and `plugin-*` actions are seeded `ComingSoon`; every
/// other action is `Available`. The `resize-pane*`, `focus-pane*`, and
/// `scroll-pane*` actions are `continuous`; every other action is not.
#[must_use]
pub fn build_core_action_seeds() -> Vec<(ActionReference, ActionMetadata)> {
    use ActionHandlerReference::CoreCommand;
    use ActionScope::{Client, Global, PaneSession, Tab};
    use ActionStatus::{Available, ComingSoon};
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
            Available,
        ),
        build_core_action_seed(
            "new-pane-left",
            "New Pane Left",
            "Split the focused pane and open the new one on the left",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
            Available,
        ),
        build_core_action_seed(
            "new-pane-down",
            "New Pane Down",
            "Split the focused pane and open the new one below",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
            Available,
        ),
        build_core_action_seed(
            "new-pane-up",
            "New Pane Up",
            "Split the focused pane and open the new one above",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
            Available,
        ),
        build_core_action_seed(
            "new-pane-right",
            "New Pane Right",
            "Split the focused pane and open the new one on the right",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
            Available,
        ),
        build_core_action_seed(
            "new-pane-stacked",
            "New Stacked Pane",
            "Add a new pane to the focused pane's stack, sharing its space",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
            Available,
        ),
        build_core_action_seed(
            "close-pane",
            "Close Pane",
            "Close the focused pane",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ClosePane),
            Available,
        ),
        build_core_action_seed(
            "close-pane-tree",
            "Close Pane Tree",
            "Close the focused pane and kill every process it started",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ClosePane),
            Available,
        ),
        build_core_action_seed(
            "resize-pane",
            "Resize Pane",
            "Grow or shrink the focused pane along one edge",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
            Available,
        ),
        build_core_action_seed(
            "resize-pane-left",
            "Resize Pane Left",
            "Move the focused pane's border one cell to the left",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
            Available,
        ),
        build_core_action_seed(
            "resize-pane-down",
            "Resize Pane Down",
            "Move the focused pane's border one cell down",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
            Available,
        ),
        build_core_action_seed(
            "resize-pane-up",
            "Resize Pane Up",
            "Move the focused pane's border one cell up",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
            Available,
        ),
        build_core_action_seed(
            "resize-pane-right",
            "Resize Pane Right",
            "Move the focused pane's border one cell to the right",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
            Available,
        ),
        build_core_action_seed(
            "move-pane",
            "Move Pane",
            "Move the focused pane into the slot of a neighboring pane",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::MovePane),
            Available,
        ),
        build_core_action_seed(
            "move-pane-left",
            "Move Pane Left",
            "Move the focused pane into the slot of its left neighbor",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::MovePane),
            Available,
        ),
        build_core_action_seed(
            "move-pane-down",
            "Move Pane Down",
            "Move the focused pane into the slot of its lower neighbor",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::MovePane),
            Available,
        ),
        build_core_action_seed(
            "move-pane-up",
            "Move Pane Up",
            "Move the focused pane into the slot of its upper neighbor",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::MovePane),
            Available,
        ),
        build_core_action_seed(
            "move-pane-right",
            "Move Pane Right",
            "Move the focused pane into the slot of its right neighbor",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::MovePane),
            Available,
        ),
        build_core_action_seed(
            "swap-panes",
            "Swap Panes",
            "Exchange two pane occupants within one tab",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::SwapPanes),
            Available,
        ),
        build_core_action_seed(
            "focus-pane",
            "Focus Pane",
            "Move the issuing client's focus to a pane",
            Client,
            vec![Pane, ClientTarget],
            CoreCommand(CommandKind::FocusPane),
            Available,
        ),
        build_core_action_seed(
            "focus-pane-left",
            "Focus Pane Left",
            "Move the issuing client's focus to the pane on the left",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
            Available,
        ),
        build_core_action_seed(
            "focus-pane-down",
            "Focus Pane Down",
            "Move the issuing client's focus to the pane below",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
            Available,
        ),
        build_core_action_seed(
            "focus-pane-up",
            "Focus Pane Up",
            "Move the issuing client's focus to the pane above",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
            Available,
        ),
        build_core_action_seed(
            "focus-pane-right",
            "Focus Pane Right",
            "Move the issuing client's focus to the pane on the right",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
            Available,
        ),
        build_core_action_seed(
            "scroll-pane",
            "Scroll Pane",
            "Scroll a pane's view by a chosen number of lines",
            Client,
            vec![ClientTarget, Pane],
            CoreCommand(CommandKind::ScrollPane),
            Available,
        ),
        build_core_action_seed(
            "scroll-pane-up",
            "Scroll Pane Up",
            "Scroll the focused pane's view toward its history",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::ScrollPane),
            Available,
        ),
        build_core_action_seed(
            "scroll-pane-down",
            "Scroll Pane Down",
            "Scroll the focused pane's view toward live output",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::ScrollPane),
            Available,
        ),
        build_core_action_seed(
            "toggle-pane-fullscreen",
            "Toggle Pane Fullscreen",
            "Toggle fullscreen for the focused pane",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::TogglePaneFullscreen),
            Available,
        ),
        build_core_action_seed(
            "write-to-pane",
            "Write To Pane",
            "Send text to a pane's shell, as if it had been typed there",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::WriteToPane),
            Available,
        ),
        // --- Tabs ---
        build_core_action_seed(
            "new-tab",
            "New Tab",
            "Create a new tab",
            Tab,
            vec![TabTarget],
            CoreCommand(CommandKind::NewTab),
            Available,
        ),
        build_core_action_seed(
            "close-tab",
            "Close Tab",
            "Close the focused tab",
            Tab,
            vec![TabTarget],
            CoreCommand(CommandKind::CloseTab),
            Available,
        ),
        build_core_action_seed(
            "focus-tab",
            "Focus Tab",
            "Switch the issuing client's view to a specific tab",
            Client,
            vec![TabTarget, ClientTarget],
            CoreCommand(CommandKind::FocusTab),
            Available,
        ),
        build_core_action_seed(
            "next-tab",
            "Next Tab",
            "Switch the issuing client's view to the next tab",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusTab),
            Available,
        ),
        build_core_action_seed(
            "previous-tab",
            "Previous Tab",
            "Switch the issuing client's view to the previous tab",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusTab),
            Available,
        ),
        build_core_action_seed(
            "move-tab",
            "Move Tab",
            "Move the focused tab to a new index",
            Tab,
            vec![TabTarget],
            CoreCommand(CommandKind::MoveTab),
            Available,
        ),
        // --- Session ---
        build_core_action_seed(
            "quit",
            "Quit",
            "Leave the session, ending it when auto-close-session is on and no other client stays",
            Client,
            vec![ClientTarget, Session],
            CoreCommand(CommandKind::Quit),
            Available,
        ),
        // --- Lock mode ---
        build_core_action_seed(
            "toggle-lock",
            "Toggle Lock",
            "Toggle pass-through lock mode for the issuing client",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::ToggleLockMode),
            Available,
        ),
        build_core_action_seed(
            "lock",
            "Lock",
            "Enable pass-through lock mode for the issuing client",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::SetLockMode),
            Available,
        ),
        build_core_action_seed(
            "unlock",
            "Unlock",
            "Disable pass-through lock mode for the issuing client",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::SetLockMode),
            Available,
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
            Available,
        ),
        // --- Run ---
        build_core_action_seed(
            "run",
            "Run Command",
            "Spawn a command in a new pane",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::RunCommandPane),
            Available,
        ),
    ];

    // --- Copy the selection --- (PaneSession scope, Pane target, Visual command,
    // ComingSoon)
    //
    // The only action of visual mode. Starting a selection (a mouse drag) and
    // dropping it (a click, or any input reaching the pane's program) have no
    // action name. The mouse layer issues `SetSelection` and `ClearSelection`
    // directly.
    action_seeds.push(build_core_action_seed(
        "copy-selection",
        "Copy Selection",
        "Copy the highlighted text to a clipboard target",
        PaneSession,
        vec![Pane],
        CoreCommand(CommandKind::Visual),
        ComingSoon,
    ));

    // --- Plugin lifecycle --- (all: Global scope, no targets, Plugin command, ComingSoon)
    let plugin_action_seeds = [
        (
            "plugin-install",
            "Install Plugin",
            "Install a plugin from a source",
        ),
        (
            "plugin-uninstall",
            "Uninstall Plugin",
            "Remove an installed plugin",
        ),
        (
            "plugin-enable",
            "Enable Plugin",
            "Enable an installed plugin",
        ),
        (
            "plugin-disable",
            "Disable Plugin",
            "Disable an installed plugin",
        ),
        (
            "plugin-update",
            "Update Plugin",
            "Update a plugin to its latest version",
        ),
        ("plugin-reload", "Reload Plugin", "Reload a plugin in place"),
    ];
    action_seeds.extend(
        plugin_action_seeds.map(|(action_name, display_name, description)| {
            build_core_action_seed(
                action_name,
                display_name,
                description,
                Global,
                vec![],
                CoreCommand(CommandKind::Plugin),
                ComingSoon,
            )
        }),
    );

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
