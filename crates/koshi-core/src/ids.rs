//! Typed identifiers.
//!
//! Newtype wrappers around UUIDv7, giving each domain entity a distinct,
//! non-interchangeable identifier type. UUIDv7 is time-ordered, so freshly
//! minted IDs sort by creation order. Every ID is `Copy`, hashable, ordered
//! (usable as a `BTreeMap` key), and `serde`-serializable; `Display` renders
//! a human-readable prefixed form
//! (e.g. `pane-0192f0c1-…`). No `From<&str>` parsing is provided — IDs are
//! either generated (`new`) or wrapped from an existing UUID (`from_uuid`).
//!
//! The seven types below share the same shape: a wrapped [`Uuid`], a `new`
//! constructor that mints a fresh id, a `from_uuid` constructor that wraps an
//! existing one, an `as_uuid` accessor, and a `Display` impl that prefixes the
//! id with its entity name.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// Identifies a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionId(Uuid);

impl SessionId {
    /// Generate a new time-ordered identifier (UUIDv7).
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wrap an existing UUID without generating a new one.
    #[must_use]
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Borrow the underlying UUID.
    #[must_use]
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "session-{}", self.0)
    }
}

/// Identifies a connected client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ClientId(Uuid);

impl ClientId {
    /// Generate a new time-ordered identifier (UUIDv7).
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wrap an existing UUID without generating a new one.
    #[must_use]
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Borrow the underlying UUID.
    #[must_use]
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for ClientId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "client-{}", self.0)
    }
}

/// Identifies a tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TabId(Uuid);

impl TabId {
    /// Generate a new time-ordered identifier (UUIDv7).
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wrap an existing UUID without generating a new one.
    #[must_use]
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Borrow the underlying UUID.
    #[must_use]
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for TabId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TabId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tab-{}", self.0)
    }
}

/// Identifies a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PaneId(Uuid);

impl PaneId {
    /// Generate a new time-ordered identifier (UUIDv7).
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wrap an existing UUID without generating a new one.
    #[must_use]
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Borrow the underlying UUID.
    #[must_use]
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for PaneId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for PaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pane-{}", self.0)
    }
}

/// Identifies a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PluginId(Uuid);

impl PluginId {
    /// Generate a new time-ordered identifier (UUIDv7).
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wrap an existing UUID without generating a new one.
    #[must_use]
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Borrow the underlying UUID.
    #[must_use]
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for PluginId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "plugin-{}", self.0)
    }
}

/// Identifies a dispatched command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CommandId(Uuid);

impl CommandId {
    /// Generate a new time-ordered identifier (UUIDv7).
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wrap an existing UUID without generating a new one.
    #[must_use]
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Borrow the underlying UUID.
    #[must_use]
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for CommandId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "command-{}", self.0)
    }
}

/// Identifies an event subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SubscriberId(Uuid);

impl SubscriberId {
    /// Generate a new time-ordered identifier (UUIDv7).
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wrap an existing UUID without generating a new one.
    #[must_use]
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Borrow the underlying UUID.
    #[must_use]
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for SubscriberId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SubscriberId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "subscriber-{}", self.0)
    }
}

#[cfg(test)]
mod tests;
