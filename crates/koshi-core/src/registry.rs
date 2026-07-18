//! The live action registry — the runtime's mutable table of every action koshi
//! can perform right now.
//!
//! [`action`](crate::action) defines what an action reference looks like and
//! ships the built-in set; this module holds the table at run time. The table
//! changes while koshi runs: a plugin load adds its `plugin:<id>:*` entries, an
//! unload removes them.
//!
//! The registry answers one question: given this [`ActionRef`], what do we
//! know about it? Mapping keys to actions is the keymap's job; turning an
//! action into a [`Command`](crate::command::Command) is the resolver's.
//!
//! One process holds one registry, mutated only by the dispatcher thread.
//! Plugins never hold a reference — they ask the dispatcher to register or
//! unregister for them.
//!
//! [`register`](ActionRegistry::register) accepts `plugin:` references only
//! (`core:` is seeded once and permanent, `user:` macros come later), and it
//! trusts only the `caller` the host authenticated: the reference's namespace,
//! the metadata's namespace, and the handler's target must all name that
//! caller, and the handler must be the caller's own
//! [`PluginHostCall`](crate::action::ActionHandlerRef::PluginHostCall) — the
//! one door where per-command capability checks happen. So a plugin can never
//! touch another plugin's namespace or a built-in action.
//!
//! [`version`](ActionRegistry::version) counts successful adds and removes;
//! a consumer caching a derived view (the which-key hint bar) rebuilds only
//! when the counter moved.

use std::collections::HashMap;
use std::fmt;

use crate::action::{
    core_action_seeds, ActionHandlerRef, ActionMetadata, ActionNamespace, ActionRef,
};
use crate::error::{DomainCategory, DomainError, Severity};
use crate::ids::PluginId;

/// The number of entries a single plugin may hold in the registry at once.
/// Registration past it is refused.
///
/// This bounds the entry count, not the bytes: a plugin supplies its own
/// `display_name` and `description`, whose lengths the host validates before it
/// reaches the registry.
pub const MAX_PLUGIN_ACTIONS: usize = 32;

/// Why an [`ActionRegistry::register`] call was refused. Each variant carries
/// the reference or plugin it rejected, so a diagnostic can name the offender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// The reference is already in the table.
    Duplicate {
        /// The reference that is already registered.
        action: ActionRef,
    },
    /// The reference is in a namespace only koshi itself may write to.
    ReservedNamespace {
        /// The reference whose namespace is not `plugin:`.
        action: ActionRef,
    },
    /// The reference belongs to a plugin other than the caller, which would let
    /// one plugin claim a name in another's namespace.
    ForeignNamespace {
        /// The reference the caller does not own.
        action: ActionRef,
        /// The plugin the caller was authenticated as.
        caller: PluginId,
    },
    /// The metadata's namespace names a different owner than the reference
    /// does. The two restate one fact, so they must agree.
    NamespaceMismatch {
        /// The reference whose metadata disagreed with it.
        action: ActionRef,
    },
    /// The metadata does not dispatch through the owning plugin's host call.
    /// A plugin action reaches the runtime only that way, so every command it
    /// performs passes the capability check the host makes on each host call.
    InvalidHandler {
        /// The reference whose handler was not its owner's host call.
        action: ActionRef,
    },
    /// The caller already holds [`MAX_PLUGIN_ACTIONS`] actions.
    PluginCapExceeded {
        /// The plugin the caller was authenticated as, which reached its cap.
        caller: PluginId,
        /// The cap that was reached.
        cap: usize,
    },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::Duplicate { action } => {
                write!(f, "action {action} is already registered")
            }
            RegistryError::ReservedNamespace { action } => write!(
                f,
                "action {action} is in a reserved namespace; only plugin: actions may be registered"
            ),
            RegistryError::ForeignNamespace { action, caller } => write!(
                f,
                "action {action} is not owned by {caller}, which may only register in its own namespace"
            ),
            RegistryError::NamespaceMismatch { action } => write!(
                f,
                "action {action} carries metadata for a different namespace"
            ),
            RegistryError::InvalidHandler { action } => write!(
                f,
                "action {action} must dispatch through its owning plugin's host call"
            ),
            RegistryError::PluginCapExceeded { caller, cap } => {
                write!(f, "{caller} already holds the maximum of {cap} actions")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

impl DomainError for RegistryError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Plugin
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// Every action koshi can perform right now, keyed by reference.
///
/// Built with [`new`](ActionRegistry::new), which loads the built-in `core:`
/// table. Plugins add and remove their own entries on top of it.
#[derive(Debug)]
pub struct ActionRegistry {
    /// Each known action and what the runtime knows about it.
    entries: HashMap<ActionRef, ActionMetadata>,
    /// Successful adds and removes since startup. See the module docs.
    version: u64,
}

impl ActionRegistry {
    /// Build a registry holding the built-in `core:` actions, at version 0.
    #[must_use]
    pub fn new() -> Self {
        ActionRegistry {
            entries: core_action_seeds().into_iter().collect(),
            version: 0,
        }
    }

    /// Add `caller`'s action to the table and bump [`version`](Self::version).
    ///
    /// `caller` is the plugin the host authenticated, and is the only fact here
    /// the registry trusts. `action` and `metadata` arrive together from the
    /// same request, so they are checked against `caller` rather than against
    /// each other: the reference is in `caller`'s namespace, the metadata
    /// repeats that namespace, and the handler is `caller`'s own
    /// [`PluginHostCall`](ActionHandlerRef::PluginHostCall). Every command a
    /// plugin action performs therefore passes through the host call the
    /// runtime capability-checks.
    ///
    /// # Errors
    /// - [`RegistryError::ReservedNamespace`] if `action` is a `core:` or
    ///   `user:` reference.
    /// - [`RegistryError::ForeignNamespace`] if `action` belongs to a plugin
    ///   other than `caller`.
    /// - [`RegistryError::NamespaceMismatch`] if `metadata.namespace` names a
    ///   different owner than `action` does.
    /// - [`RegistryError::InvalidHandler`] if `metadata.handler` is anything
    ///   other than `caller`'s own host call.
    /// - [`RegistryError::Duplicate`] if `action` is already registered.
    /// - [`RegistryError::PluginCapExceeded`] if `caller` already holds
    ///   [`MAX_PLUGIN_ACTIONS`] actions.
    pub fn register(
        &mut self,
        caller: PluginId,
        action: ActionRef,
        metadata: ActionMetadata,
    ) -> Result<(), RegistryError> {
        // 1. The reference itself must sit in a `plugin:` namespace, and it
        // must be `caller`'s own plugin, not another one's.
        match action.namespace {
            ActionNamespace::Core | ActionNamespace::User => {
                return Err(RegistryError::ReservedNamespace { action })
            }
            ActionNamespace::Plugin(owner) if owner != caller => {
                return Err(RegistryError::ForeignNamespace { action, caller })
            }
            ActionNamespace::Plugin(_) => {}
        }

        // 2. The metadata must restate the same namespace as the reference.
        if metadata.namespace != action.namespace {
            return Err(RegistryError::NamespaceMismatch { action });
        }

        // 3. The handler must route back through `caller`'s own host call, so
        // every command the action performs passes that plugin's capability
        // check.
        if metadata.handler != ActionHandlerRef::PluginHostCall(caller) {
            return Err(RegistryError::InvalidHandler { action });
        }

        // 4. The reference must not already be registered.
        if self.entries.contains_key(&action) {
            return Err(RegistryError::Duplicate { action });
        }

        // 5. `caller` must not already hold the maximum number of entries.
        // ponytail: scan to count; a per-plugin counter is the upgrade once the
        // table holds hundreds of entries.
        let held = self
            .entries
            .keys()
            .filter(|held| matches!(held.namespace, ActionNamespace::Plugin(id) if id == caller))
            .count();
        if held >= MAX_PLUGIN_ACTIONS {
            return Err(RegistryError::PluginCapExceeded {
                caller,
                cap: MAX_PLUGIN_ACTIONS,
            });
        }

        self.entries.insert(action, metadata);
        self.version += 1;
        Ok(())
    }

    /// Remove one of `caller`'s actions, returning the metadata it held.
    ///
    /// A plugin removes only what it owns, mirroring [`register`](Self::register):
    /// `caller` is the authenticated owner, and an `action` in any other
    /// namespace — `core:`, `user:`, or another plugin's — leaves the table
    /// untouched. Returns `None` whenever nothing was removed, and the version
    /// bumps only when an entry was.
    pub fn unregister(&mut self, caller: PluginId, action: &ActionRef) -> Option<ActionMetadata> {
        if action.namespace != ActionNamespace::Plugin(caller) {
            return None;
        }
        let metadata = self.entries.remove(action)?;
        self.version += 1;
        Some(metadata)
    }

    /// Look an action up. `None` means the reference names no known action — a
    /// binding pointing at it is an orphan, e.g. because its plugin unloaded.
    #[must_use]
    pub fn lookup(&self, action: &ActionRef) -> Option<&ActionMetadata> {
        self.entries.get(action)
    }

    /// Every action in `namespace`, in unspecified order. A caller that renders
    /// the result sorts it.
    pub fn list_by_namespace(
        &self,
        namespace: ActionNamespace,
    ) -> impl Iterator<Item = (&ActionRef, &ActionMetadata)> + '_ {
        self.entries
            .iter()
            .filter(move |(action, _)| action.namespace == namespace)
    }

    /// How many adds and removes have succeeded since startup.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }
}

impl Default for ActionRegistry {
    fn default() -> Self {
        ActionRegistry::new()
    }
}

#[cfg(test)]
pub(crate) mod tests;
