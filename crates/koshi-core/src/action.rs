//! Action vocabulary — the stable, plugin-extensible user surface.
//!
//! An *action* is what a user binds a key to, types as a CLI subcommand, or a
//! plugin contributes. It is deliberately **not** the same thing as a
//! [`Command`](crate::command::Command): a `Command` is the runtime's internal
//! mutation type and is free to evolve, while an action is the public name that
//! config files and the plugin SDK depend on. The action → command mapping
//! decouples the user-facing surface from internal churn.
//!
//! Unlike commands, the action set is **open**. Built-in actions live in the
//! `core:` namespace, plugins own `plugin:<id>:*`, and `user:` is reserved for
//! user-defined macros (a later feature; the namespace is claimed now so it
//! cannot collide). This file ships the *primitives* — [`ActionRef`],
//! [`ActionNamespace`], [`ActionMetadata`], [`ActionHandlerRef`], and the static
//! [`core_action_seeds`] table. The mutable, runtime table that loads those seeds
//! and accepts plugin registrations is
//! [`ActionRegistry`](crate::registry::ActionRegistry).
//!
//! Actions are not a fixed enum: the open design ensures plugins are
//! first-class citizens on the keyboard surface, not locked out of the
//! primary user-binding interface.

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
    /// A later character was outside `[a-z0-9-]`.
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
/// The grammar is enforced on construction *and* on deserialization (via
/// [`TryFrom<String>`]), so a name decoded from a config file, the IPC socket,
/// or a plugin can never carry characters that would break display rendering or
/// collide across surfaces.
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
        // The first character must be an ASCII lowercase letter.
        match chars.next() {
            None => return Err(ActionNameError::Empty),
            Some(first) if !first.is_ascii_lowercase() => {
                return Err(ActionNameError::InvalidStart { ch: first });
            }
            Some(_) => {}
        }
        // Every character after the first must be a lowercase letter, digit, or hyphen.
        for ch in chars {
            if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-') {
                return Err(ActionNameError::InvalidChar { ch });
            }
        }
        // Checked after the charset scan so an over-long name still reports the
        // more specific bad character first when both are wrong.
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

/// Which family an action belongs to. Namespaces are typed, so a `core:` and a
/// `plugin:` action can never collide even with identical local names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ActionNamespace {
    /// Built-in actions shipped by Koshi. Plugins may never register here.
    Core,
    /// Actions contributed by a plugin; the id scopes the name.
    Plugin(PluginId),
    /// User-defined macros. Reserved now; the feature lands post-1.0.
    User,
}

/// A fully-qualified reference to an action: its namespace plus local name.
///
/// `Display` renders the canonical wire form used everywhere an action is named
/// by string — config files, CLI output, and plugin messages: `core:new-pane`,
/// `plugin:<uuid>:open-status`, `user:my-macro`. Serde round-trips through that
/// same string (not a `{namespace, name}` struct) via [`FromStr`], so a keymap
/// like `"<C-p>n" action="core:new-pane"` decodes to exactly this type and the
/// stable user-facing token is the wire format.
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

/// How broad an action's effect is. Used to describe and group actions in
/// `koshi keys`/`koshi actions` output and which-key hints.
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

/// A kind of entity an action can target. An action lists the targets it
/// accepts so the resolver and CLI can validate an explicit `--pane`/`--tab`.
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

/// Whether an action is usable today or still on the way.
///
/// The action vocabulary is seeded in full, but some actions have no runtime
/// handler yet. Introspection hides `ComingSoon` actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionStatus {
    /// The runtime implements this action; it is safe to bind and invoke.
    Available,
    /// Seeded for completeness, but not yet implemented by the runtime.
    ComingSoon,
}

/// Typed schema for an action's arguments.
///
/// Placeholder: the full typed-argument model is owned by the keybinding
/// system, which fills this in once config parsing exists. It is a named type
/// now so [`ActionMetadata`] has a stable shape and seed entries can carry
/// `None`.
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
/// `namespace` is redundant with the owning [`ActionRef`]'s namespace and is
/// kept here so metadata is self-describing when handed out on its own (e.g. to
/// a plugin querying the registry).
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
    /// Whether the runtime implements the action yet.
    pub status: ActionStatus,
}

/// Build one `core:` seed entry. Names here are compile-time constants known to
/// satisfy the grammar; an invalid one is a bug in this table and is caught by
/// the seed test rather than returned as an error. Each entry declares its own
/// [`ActionStatus`], so readiness is per-action: one member of a command family
/// can be `Available` while its siblings are still `ComingSoon`.
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
    };
    (action, metadata)
}

/// The built-in action table, loaded into the runtime registry at startup.
///
/// Every entry is in the `core:` namespace. Some user-facing actions share a
/// [`CommandKind`] and differ only by a preset argument the resolver supplies
/// later — `lock`/`unlock` both build `SetLockMode`; `next-tab`/`previous-tab`/
/// `focus-tab` all build `FocusTab`; the `copy-mode-*` actions all build
/// `CopyMode`. Those presets are carried by the (currently deferred)
/// [`ActionArgsSchema`], not duplicated into [`CommandKind`].
///
/// Each entry declares its own [`ActionStatus`]. The `copy-mode-*` and
/// `plugin-*` actions and `quit` have no runtime handler yet and are seeded
/// `ComingSoon`, so introspection hides them; every other action is
/// `Available`. Status is per-action, so a family lands one member at a time
/// rather than all at once.
#[must_use]
pub fn core_action_seeds() -> Vec<(ActionRef, ActionMetadata)> {
    use ActionHandlerRef::CoreCommand;
    use ActionScope::{Client, Global, PaneSession, Tab};
    use ActionStatus::{Available, ComingSoon};
    use TargetKind::{Client as ClientTarget, Pane, Session, Tab as TabTarget};

    vec![
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
            "close-pane",
            "Close Pane",
            "Close the focused pane",
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
            "focus-pane",
            "Focus Pane",
            "Move the issuing client's focus to a pane",
            Client,
            vec![Pane, ClientTarget],
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
            "rename-pane",
            "Rename Pane",
            "Assign a fresh generated name to the focused pane",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::RenamePane),
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
            "rename-tab",
            "Rename Tab",
            "Assign a fresh generated name to the focused tab",
            Tab,
            vec![TabTarget],
            CoreCommand(CommandKind::RenameTab),
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
            "rename-session",
            "Rename Session",
            "Assign a fresh generated name to the current session, or one named by id",
            Global,
            vec![Session],
            CoreCommand(CommandKind::RenameSession),
            Available,
        ),
        core_seed(
            "quit",
            "Quit",
            "Prompt the issuing client to quit the client or session",
            Client,
            vec![ClientTarget, Session],
            CoreCommand(CommandKind::Quit),
            ComingSoon,
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
        // --- Copy mode ---
        core_seed(
            "copy-mode-enter",
            "Enter Copy Mode",
            "Enter copy mode in the focused pane",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::CopyMode),
            ComingSoon,
        ),
        core_seed(
            "copy-mode-exit",
            "Exit Copy Mode",
            "Leave copy mode",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::CopyMode),
            ComingSoon,
        ),
        core_seed(
            "copy-mode-move-cursor",
            "Move Copy Cursor",
            "Move the copy-mode cursor",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::CopyMode),
            ComingSoon,
        ),
        core_seed(
            "copy-mode-set-selection",
            "Set Selection",
            "Begin or extend the copy-mode selection",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::CopyMode),
            ComingSoon,
        ),
        core_seed(
            "copy-mode-clear-selection",
            "Clear Selection",
            "Clear the active copy-mode selection",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::CopyMode),
            ComingSoon,
        ),
        core_seed(
            "copy-mode-copy",
            "Copy Selection",
            "Copy the current selection to a clipboard target",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::CopyMode),
            ComingSoon,
        ),
        core_seed(
            "copy-mode-search",
            "Search",
            "Start a search in copy mode",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::CopyMode),
            ComingSoon,
        ),
        core_seed(
            "copy-mode-search-next",
            "Search Next",
            "Jump to the next search match",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::CopyMode),
            ComingSoon,
        ),
        core_seed(
            "copy-mode-search-prev",
            "Search Previous",
            "Jump to the previous search match",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::CopyMode),
            ComingSoon,
        ),
        // --- Plugin lifecycle ---
        core_seed(
            "plugin-install",
            "Install Plugin",
            "Install a plugin from a source",
            Global,
            vec![],
            CoreCommand(CommandKind::Plugin),
            ComingSoon,
        ),
        core_seed(
            "plugin-uninstall",
            "Uninstall Plugin",
            "Remove an installed plugin",
            Global,
            vec![],
            CoreCommand(CommandKind::Plugin),
            ComingSoon,
        ),
        core_seed(
            "plugin-enable",
            "Enable Plugin",
            "Enable an installed plugin",
            Global,
            vec![],
            CoreCommand(CommandKind::Plugin),
            ComingSoon,
        ),
        core_seed(
            "plugin-disable",
            "Disable Plugin",
            "Disable an installed plugin",
            Global,
            vec![],
            CoreCommand(CommandKind::Plugin),
            ComingSoon,
        ),
        core_seed(
            "plugin-update",
            "Update Plugin",
            "Update a plugin to its latest version",
            Global,
            vec![],
            CoreCommand(CommandKind::Plugin),
            ComingSoon,
        ),
        core_seed(
            "plugin-reload",
            "Reload Plugin",
            "Reload a plugin in place",
            Global,
            vec![],
            CoreCommand(CommandKind::Plugin),
            ComingSoon,
        ),
    ]
}

#[cfg(test)]
mod tests;
