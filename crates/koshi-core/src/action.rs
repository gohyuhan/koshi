//! Action vocabulary — the stable, plugin-extensible user surface.
//!
//! An *action* is what a user binds a key to, types as a CLI subcommand, or a
//! plugin contributes. It is not a [`Command`](crate::command::Command): a
//! `Command` is the runtime's internal mutation type, an action is the public
//! name config files and the plugin SDK use. Each action maps to a command.
//!
//! The action set is open. Built-in actions live in the `core:` namespace,
//! plugins own `plugin:<id>:*`, and `user:` is reserved for user-defined
//! macros. This file holds the primitives — [`ActionRef`], [`ActionNamespace`],
//! [`ActionMetadata`], [`ActionHandlerRef`], and the static
//! [`core_action_seeds`] table. The mutable runtime table that loads those seeds
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
const MAX_ACTION_NAME_LEN: usize = 31;

/// Why a string is not a valid [`ActionName`].
///
/// Names follow `^[a-z][a-z0-9-]{0,30}$`: a lowercase-letter start, then up to
/// thirty more lowercase letters, digits, or hyphens. The display name shown to
/// users is free-form and lives separately in [`ActionMetadata`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionNameError {
    /// The name was empty.
    Empty,
    /// The name exceeded `MAX_ACTION_NAME_LEN` characters.
    TooLong {
        /// The offending length.
        len: usize,
    },
    /// The first character was not an ASCII lowercase letter.
    InvalidStart {
        /// The offending leading character.
        ch: char,
    },
    /// A character after the first was outside `[a-z0-9-]`.
    InvalidChar {
        /// The offending character.
        ch: char,
    },
}

impl fmt::Display for ActionNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionNameError::Empty => f.write_str("action name is empty"),
            ActionNameError::TooLong { len } => write!(
                f,
                "action name is {len} chars; the maximum is {MAX_ACTION_NAME_LEN}"
            ),
            ActionNameError::InvalidStart { ch } => write!(
                f,
                "action name must start with a lowercase letter, found {ch:?}"
            ),
            ActionNameError::InvalidChar { ch } => {
                write!(f, "action name may only contain [a-z0-9-], found {ch:?}")
            }
        }
    }
}

impl std::error::Error for ActionNameError {}

/// The local name of an action within its namespace, validated against
/// `^[a-z][a-z0-9-]{0,30}$`.
///
/// [`ActionName::new`] and deserialization (via [`TryFrom<String>`]) both run
/// the grammar check: a name decoded from a config file, the IPC socket, or a
/// plugin is always valid.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ActionName(String);

impl ActionName {
    /// Validate `name` against the action-name grammar.
    ///
    /// # Errors
    /// Returns an [`ActionNameError`] describing the first rule the input
    /// violates.
    pub fn new(name: &str) -> Result<Self, ActionNameError> {
        let mut chars = name.chars();
        let first = chars.next().ok_or(ActionNameError::Empty)?;
        if !first.is_ascii_lowercase() {
            return Err(ActionNameError::InvalidStart { ch: first });
        }
        // Every character after the first must be a lowercase letter, digit, or hyphen.
        for ch in chars {
            if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-') {
                return Err(ActionNameError::InvalidChar { ch });
            }
        }
        // The length check runs after the charset scan: a name that is both
        // over-long and holds a bad character reports the bad character.
        let len = name.chars().count();
        if len > MAX_ACTION_NAME_LEN {
            return Err(ActionNameError::TooLong { len });
        }
        Ok(ActionName(name.to_string()))
    }

    /// Borrow the validated name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ActionName {
    type Error = ActionNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        ActionName::new(&value)
    }
}

impl From<ActionName> for String {
    fn from(name: ActionName) -> Self {
        name.0
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
pub struct ActionRef {
    /// The namespace that owns the action.
    pub namespace: ActionNamespace,
    /// The local name within the namespace.
    pub name: ActionName,
}

impl ActionRef {
    /// Reference a built-in `core:` action.
    ///
    /// # Errors
    /// Returns an [`ActionNameError`] if `name` violates the grammar.
    pub fn core(name: &str) -> Result<Self, ActionNameError> {
        Ok(ActionRef {
            namespace: ActionNamespace::Core,
            name: ActionName::new(name)?,
        })
    }

    /// Reference an action owned by `plugin`.
    ///
    /// # Errors
    /// Returns an [`ActionNameError`] if `name` violates the grammar.
    pub fn plugin(plugin: PluginId, name: &str) -> Result<Self, ActionNameError> {
        Ok(ActionRef {
            namespace: ActionNamespace::Plugin(plugin),
            name: ActionName::new(name)?,
        })
    }

    /// Reference a `user:` macro action.
    ///
    /// # Errors
    /// Returns an [`ActionNameError`] if `name` violates the grammar.
    pub fn user(name: &str) -> Result<Self, ActionNameError> {
        Ok(ActionRef {
            namespace: ActionNamespace::User,
            name: ActionName::new(name)?,
        })
    }
}

impl fmt::Display for ActionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.namespace {
            ActionNamespace::Core => write!(f, "core:{}", self.name),
            ActionNamespace::Plugin(id) => write!(f, "plugin:{}:{}", id.as_uuid(), self.name),
            ActionNamespace::User => write!(f, "user:{}", self.name),
        }
    }
}

/// Why a string is not a valid [`ActionRef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionRefParseError {
    /// No `namespace:` prefix was present.
    MissingNamespace,
    /// The namespace prefix was not one of `core`, `plugin`, or `user`.
    UnknownNamespace {
        /// The unrecognized prefix.
        found: String,
    },
    /// A `plugin:` reference was missing the `:<name>` after its id.
    MissingPluginName,
    /// A `plugin:` reference's id was not a valid UUID.
    InvalidPluginId,
    /// The local name failed the action-name grammar.
    Name(ActionNameError),
}

impl fmt::Display for ActionRefParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionRefParseError::MissingNamespace => {
                f.write_str("action ref is missing a 'namespace:' prefix")
            }
            ActionRefParseError::UnknownNamespace { found } => write!(
                f,
                "unknown action namespace {found:?}; expected core, plugin, or user"
            ),
            ActionRefParseError::MissingPluginName => {
                f.write_str("plugin action ref must be 'plugin:<uuid>:<name>'")
            }
            ActionRefParseError::InvalidPluginId => {
                f.write_str("plugin action ref has an invalid UUID")
            }
            ActionRefParseError::Name(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ActionRefParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ActionRefParseError::Name(err) => Some(err),
            _ => None,
        }
    }
}

impl FromStr for ActionRef {
    type Err = ActionRefParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Split off the "core"/"user"/"plugin" prefix from everything after its colon.
        let (namespace, rest) = s
            .split_once(':')
            .ok_or(ActionRefParseError::MissingNamespace)?;
        match namespace {
            "core" => ActionRef::core(rest).map_err(ActionRefParseError::Name),
            "user" => ActionRef::user(rest).map_err(ActionRefParseError::Name),
            "plugin" => {
                // A plugin ref has one more segment than core/user: "<uuid>:<name>".
                let (id, name) = rest
                    .split_once(':')
                    .ok_or(ActionRefParseError::MissingPluginName)?;
                let uuid = Uuid::parse_str(id).map_err(|_| ActionRefParseError::InvalidPluginId)?;
                ActionRef::plugin(PluginId::from_uuid(uuid), name)
                    .map_err(ActionRefParseError::Name)
            }
            found => Err(ActionRefParseError::UnknownNamespace {
                found: found.to_string(),
            }),
        }
    }
}

impl TryFrom<String> for ActionRef {
    type Error = ActionRefParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<ActionRef> for String {
    fn from(action: ActionRef) -> Self {
        action.to_string()
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

/// A kind of entity an action can target. [`ActionMetadata::target_compat`]
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

/// Whether the runtime implements an action. Serializes in kebab-case:
/// `available`, `coming-soon`.
///
/// Introspection (`koshi actions list`/`explain`) hides `ComingSoon` actions,
/// and resolving one is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionStatus {
    /// The runtime implements this action; binding and invoking it work.
    Available,
    /// The action is seeded but the runtime has no handler for it.
    ComingSoon,
}

/// Typed schema for an action's arguments. Carries no fields. Every entry in
/// [`core_action_seeds`] leaves [`ActionMetadata::args_schema`] `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionArgsSchema {}

/// How an action is dispatched once it fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionHandlerRef {
    /// Build and dispatch the named core [`Command`](crate::command::Command).
    CoreCommand(CommandKind),
    /// Route to a plugin via a host command request.
    PluginHostCall(PluginId),
    /// Fire a sequence of actions in order (a macro); halts on first failure.
    Sequence(Vec<ActionRef>),
}

/// Everything the registry knows about one action: how to show it, what it can
/// target, and how to dispatch it.
///
/// `namespace` repeats the owning [`ActionRef`]'s namespace, so metadata handed
/// out on its own still names its owner.
/// [`ActionRegistry::register`](crate::registry::ActionRegistry::register)
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
    pub scope_class: ActionScope,
    /// Entity kinds the action can target.
    pub target_compat: Vec<TargetKind>,
    /// Typed argument schema, when the action takes arguments.
    pub args_schema: Option<ActionArgsSchema>,
    /// How the action is dispatched.
    pub handler: ActionHandlerRef,
    /// Whether the runtime implements the action.
    pub status: ActionStatus,
    /// Whether the action repeats from a held prefix: fired from a
    /// multi-chord binding, the binding's prefix stays armed and the next
    /// chord alone fires again (`<C-s> h h h` resizes three times). Declared
    /// per action here, never in a binding. Absent on the wire means `false`.
    #[serde(default)]
    pub continuous: bool,
}

/// Build one `core:` seed entry, with `namespace` set to
/// [`ActionNamespace::Core`], `args_schema` `None`, and `continuous` `false`.
///
/// # Panics
/// Panics if `name` violates the action-name grammar.
fn core_seed(
    name: &'static str,
    display_name: &str,
    description: &str,
    scope_class: ActionScope,
    target_compat: Vec<TargetKind>,
    handler: ActionHandlerRef,
    status: ActionStatus,
) -> (ActionRef, ActionMetadata) {
    let action =
        ActionRef::core(name).expect("core seed action name must satisfy the action-name grammar");
    let metadata = ActionMetadata {
        namespace: ActionNamespace::Core,
        display_name: display_name.to_string(),
        description: description.to_string(),
        scope_class,
        target_compat,
        args_schema: None,
        handler,
        status,
        continuous: false,
    };
    (action, metadata)
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
/// other action is `Available`. The `resize-pane*` and `focus-pane*` actions
/// are `continuous`; every other action is not.
#[must_use]
pub fn core_action_seeds() -> Vec<(ActionRef, ActionMetadata)> {
    use ActionHandlerRef::CoreCommand;
    use ActionScope::{Client, Global, PaneSession, Tab};
    use ActionStatus::{Available, ComingSoon};
    use TargetKind::{Client as ClientTarget, Pane, Session, Tab as TabTarget};

    let mut seeds = vec![
        // --- Panes ---
        core_seed(
            "new-pane",
            "New Pane",
            "Split the focused pane and start a shell in the new one",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
            Available,
        ),
        core_seed(
            "new-pane-left",
            "New Pane Left",
            "Split the focused pane and open the new one on the left",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
            Available,
        ),
        core_seed(
            "new-pane-down",
            "New Pane Down",
            "Split the focused pane and open the new one below",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
            Available,
        ),
        core_seed(
            "new-pane-up",
            "New Pane Up",
            "Split the focused pane and open the new one above",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
            Available,
        ),
        core_seed(
            "new-pane-right",
            "New Pane Right",
            "Split the focused pane and open the new one on the right",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
            Available,
        ),
        core_seed(
            "new-pane-stacked",
            "New Stacked Pane",
            "Add a new pane to the focused pane's stack, sharing its space",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
            Available,
        ),
        core_seed(
            "close-pane",
            "Close Pane",
            "Close the focused pane",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ClosePane),
            Available,
        ),
        core_seed(
            "close-pane-tree",
            "Close Pane Tree",
            "Close the focused pane and kill every process it started",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ClosePane),
            Available,
        ),
        core_seed(
            "resize-pane",
            "Resize Pane",
            "Grow or shrink the focused pane along one edge",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
            Available,
        ),
        core_seed(
            "resize-pane-left",
            "Resize Pane Left",
            "Move the focused pane's border one cell to the left",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
            Available,
        ),
        core_seed(
            "resize-pane-down",
            "Resize Pane Down",
            "Move the focused pane's border one cell down",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
            Available,
        ),
        core_seed(
            "resize-pane-up",
            "Resize Pane Up",
            "Move the focused pane's border one cell up",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
            Available,
        ),
        core_seed(
            "resize-pane-right",
            "Resize Pane Right",
            "Move the focused pane's border one cell to the right",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
            Available,
        ),
        core_seed(
            "focus-pane",
            "Focus Pane",
            "Move the issuing client's focus to a pane",
            Client,
            vec![Pane, ClientTarget],
            CoreCommand(CommandKind::FocusPane),
            Available,
        ),
        core_seed(
            "focus-pane-left",
            "Focus Pane Left",
            "Move the issuing client's focus to the pane on the left",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
            Available,
        ),
        core_seed(
            "focus-pane-down",
            "Focus Pane Down",
            "Move the issuing client's focus to the pane below",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
            Available,
        ),
        core_seed(
            "focus-pane-up",
            "Focus Pane Up",
            "Move the issuing client's focus to the pane above",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
            Available,
        ),
        core_seed(
            "focus-pane-right",
            "Focus Pane Right",
            "Move the issuing client's focus to the pane on the right",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
            Available,
        ),
        core_seed(
            "toggle-pane-fullscreen",
            "Toggle Pane Fullscreen",
            "Toggle fullscreen for the focused pane",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::TogglePaneFullscreen),
            Available,
        ),
        core_seed(
            "write-to-pane",
            "Write To Pane",
            "Send text to a pane's shell, as if it had been typed there",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::WriteToPane),
            Available,
        ),
        // --- Tabs ---
        core_seed(
            "new-tab",
            "New Tab",
            "Create a new tab",
            Tab,
            vec![TabTarget],
            CoreCommand(CommandKind::NewTab),
            Available,
        ),
        core_seed(
            "close-tab",
            "Close Tab",
            "Close the focused tab",
            Tab,
            vec![TabTarget],
            CoreCommand(CommandKind::CloseTab),
            Available,
        ),
        core_seed(
            "focus-tab",
            "Focus Tab",
            "Switch the issuing client's view to a specific tab",
            Client,
            vec![TabTarget, ClientTarget],
            CoreCommand(CommandKind::FocusTab),
            Available,
        ),
        core_seed(
            "next-tab",
            "Next Tab",
            "Switch the issuing client's view to the next tab",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusTab),
            Available,
        ),
        core_seed(
            "previous-tab",
            "Previous Tab",
            "Switch the issuing client's view to the previous tab",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusTab),
            Available,
        ),
        core_seed(
            "move-tab",
            "Move Tab",
            "Move the focused tab to a new index",
            Tab,
            vec![TabTarget],
            CoreCommand(CommandKind::MoveTab),
            Available,
        ),
        // --- Session ---
        core_seed(
            "quit",
            "Quit",
            "Leave the session, ending it when auto-close-session is on and no other client stays",
            Client,
            vec![ClientTarget, Session],
            CoreCommand(CommandKind::Quit),
            Available,
        ),
        // --- Lock mode ---
        core_seed(
            "toggle-lock",
            "Toggle Lock",
            "Toggle pass-through lock mode for the issuing client",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::ToggleLockMode),
            Available,
        ),
        core_seed(
            "lock",
            "Lock",
            "Enable pass-through lock mode for the issuing client",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::SetLockMode),
            Available,
        ),
        core_seed(
            "unlock",
            "Unlock",
            "Disable pass-through lock mode for the issuing client",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::SetLockMode),
            Available,
        ),
        // --- Mouse select ---
        core_seed(
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
        core_seed(
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
    seeds.push(core_seed(
        "copy-selection",
        "Copy Selection",
        "Copy the highlighted text to a clipboard target",
        PaneSession,
        vec![Pane],
        CoreCommand(CommandKind::Visual),
        ComingSoon,
    ));

    // --- Plugin lifecycle --- (all: Global scope, no targets, Plugin command, ComingSoon)
    let plugin_seeds = [
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
    seeds.extend(plugin_seeds.map(|(name, display_name, description)| {
        core_seed(
            name,
            display_name,
            description,
            Global,
            vec![],
            CoreCommand(CommandKind::Plugin),
            ComingSoon,
        )
    }));

    // Repeat-from-prefix actions: fired from a multi-chord binding, the
    // prefix stays armed and the next chord alone fires again (`<C-s> h h h`,
    // `<C-p> ← ← ←`).
    for (action, metadata) in &mut seeds {
        if matches!(
            action.name.as_str(),
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
        ) {
            metadata.continuous = true;
        }
    }

    seeds
}

#[cfg(test)]
mod tests;
