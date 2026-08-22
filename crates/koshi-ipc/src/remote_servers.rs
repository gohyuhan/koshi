//! The servers a user has connected to, saved on the dialling user's own
//! machine, so nothing is retyped on the next connection.
//!
//! One [`SavedServer`](crate::remote_servers::SavedServer) holds the address,
//! the secret the operator handed out, the fingerprint of the certificate
//! that server presented on the first connection — or none until a
//! connection has opened — and an optional name the user chose. After the
//! first connection the user types the name, not the address.
//!
//! The whole set lives in one JSON file —
//! [`store_path`](crate::remote_servers::store_path) — inside the private
//! koshi data directory. The file carries the format number
//! [`SERVER_STORE_FORMAT`](crate::remote_servers::SERVER_STORE_FORMAT), and a
//! file carrying any other number is refused. Writes go through
//! [`koshi_storage::atomic::write_atomic`], so a reader finds the old content
//! or the new, never a half-written middle.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::error::{IpcError, RemoteFile};
use crate::protocol::ConnectionToken;
use crate::remote_state::write_owner_only;

/// The format number this build writes into every saved-server file, and the
/// only one it reads back.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::SAVED_SERVER_FORMAT`].
pub const SERVER_STORE_FORMAT: u32 = koshi_core::compat::SAVED_SERVER_FORMAT.max;

/// One server this machine has connected to.
///
/// Decoding rejects any field it does not know, so a misspelled name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedServer {
    /// The name the user chose for this server, or `None` when they chose
    /// none. The user types it in place of the address.
    pub name: Option<String>,
    /// Where the server listens, as `host:port`.
    pub address: String,
    /// The secret the operator handed out with a grant. `ConnectionToken`'s
    /// `Debug` and `Display` write it redacted.
    pub secret: ConnectionToken,
    /// The sha256 of the certificate this server presented on the first
    /// connection, as 64 lowercase hex characters, or `None` while no
    /// connection to it has opened. A later connection that presents a
    /// different certificate is refused; the first connection of a record
    /// holding `None` pins whatever certificate it meets. `None` leaves the
    /// file without this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// When this server was first saved.
    pub added_at: SystemTime,
    /// When a connection to this server last opened, or `None` when none has
    /// since it was saved.
    pub last_used_at: Option<SystemTime>,
}

/// Every server this machine has connected to.
///
/// Decoding rejects any field it does not know, so a misspelled name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerStore {
    /// The format number of the file these records came from or go to.
    pub format: u32,
    /// One record per server, in the order they were first saved.
    pub records: Vec<SavedServer>,
}

impl Default for ServerStore {
    fn default() -> Self {
        ServerStore::new()
    }
}

impl ServerStore {
    /// An empty store at the format number this build writes.
    #[must_use]
    pub fn new() -> ServerStore {
        ServerStore {
            format: SERVER_STORE_FORMAT,
            records: Vec::new(),
        }
    }

    /// Read the store at `path`.
    ///
    /// A path with no file is an empty store: this machine has connected to
    /// nothing yet. A file that cannot be read, whose bytes are not a
    /// readable store, or whose format number is not
    /// [`SERVER_STORE_FORMAT`] is [`IpcError::RemoteFileUnreadable`].
    pub fn read(path: &Path) -> Result<ServerStore, IpcError> {
        let unreadable = |detail: String| IpcError::RemoteFileUnreadable {
            file: RemoteFile::SavedServers,
            path: path.display().to_string(),
            detail,
        };
        let data = match std::fs::read(path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ServerStore::new())
            }
            Err(error) => return Err(unreadable(error.to_string())),
        };
        let store: ServerStore =
            serde_json::from_slice(&data).map_err(|error| unreadable(error.to_string()))?;
        if store.format != SERVER_STORE_FORMAT {
            return Err(unreadable(format!(
                "format {} is not the {SERVER_STORE_FORMAT} this build reads",
                store.format
            )));
        }
        Ok(store)
    }

    /// Write this store at `path`, replacing whatever is there, and create
    /// the directory holding it when it is missing.
    ///
    /// The file is restricted to the owning user: mode `0600` on Unix, set on
    /// an existing file before the replace so the new file carries it too. On
    /// Windows the file takes the data directory's owner-scoped ACLs. The
    /// directory itself gets mode `0700` on Unix.
    ///
    /// Any failure along the way is [`IpcError::RemoteFileWrite`].
    pub fn write(&self, path: &Path) -> Result<(), IpcError> {
        write_owner_only(path, self).map_err(|detail| IpcError::RemoteFileWrite {
            file: RemoteFile::SavedServers,
            path: path.display().to_string(),
            detail,
        })
    }

    /// The server `arg` names.
    ///
    /// A selector matching more than one record is [`Lookup::Ambiguous`], never
    /// [`Lookup::NotSaved`].
    #[must_use]
    pub fn find(&self, arg: &str) -> Lookup<'_> {
        match self.index_of(arg) {
            Match::One(index) => Lookup::Saved(&self.records[index]),
            Match::None => Lookup::NotSaved,
            Match::Many => Lookup::Ambiguous,
        }
    }

    /// Save `server`, taking the place of whatever record already holds that
    /// address.
    ///
    /// One address is one machine, so saving an address again replaces its
    /// record: the secret and the pinned fingerprint are the new ones.
    ///
    /// Keeps three rules:
    ///
    /// 1. one address appears once — the replace above,
    /// 2. one name appears once,
    /// 3. no name is another record's address.
    ///
    /// Rules 2 and 3 refuse. Rule 3 is checked in both directions: `server`'s
    /// name against every other record's address, and `server`'s address
    /// against every other record's name.
    ///
    /// The store is not written; the caller does that.
    ///
    /// # Errors
    /// [`NameTaken`] carrying the word and the address of the record that
    /// already answers to it.
    pub fn save(&mut self, server: SavedServer) -> Result<(), NameTaken> {
        // Rules 2 and 3: another record answering to this record's name.
        if let Some(name) = server.name.as_deref() {
            if let Some(holder) = self.name_holder(name, &server.address) {
                return Err(NameTaken {
                    name: name.to_string(),
                    address: holder.address.clone(),
                });
            }
        }
        // Rule 3, the other direction: another record answering to this
        // record's address.
        if let Some(holder) = self.name_holder(&server.address, &server.address) {
            return Err(NameTaken {
                name: server.address.clone(),
                address: holder.address.clone(),
            });
        }
        self.records
            .retain(|record| record.address != server.address);
        self.records.push(server);
        Ok(())
    }

    /// Whether `name` is free to give to the server at `address`.
    ///
    /// True when no record other than the one at `address` answers to `name`,
    /// by its own name or by its own address. The record at `address` may keep
    /// a name it already holds.
    #[must_use]
    pub fn name_free_for(&self, name: &str, address: &str) -> bool {
        self.name_holder(name, address).is_none()
    }

    /// The record other than the one at `address` that answers to `name`, by
    /// its own name or by its own address. `None` when no record does, and the
    /// first of them when several do.
    fn name_holder(&self, name: &str, address: &str) -> Option<&SavedServer> {
        self.records
            .iter()
            .filter(|record| record.address != address)
            .find(|record| record.name.as_deref() == Some(name) || record.address == name)
    }

    /// Drop the server `arg` names, returning its address.
    ///
    /// `None` when no record answers to `arg`, and `None` when more than one
    /// does; nothing is removed in either case. [`ServerStore::find`] tells the
    /// two apart.
    ///
    /// The store is not written; the caller does that.
    pub fn forget(&mut self, arg: &str) -> Option<String> {
        let Match::One(index) = self.index_of(arg) else {
            return None;
        };
        Some(self.records.remove(index).address)
    }

    /// Put `secret` on the server `arg` names, returning its address.
    ///
    /// `None` when no record answers to `arg`, and `None` when more than one
    /// does; no secret is written in either case.
    ///
    /// The store is not written; the caller does that.
    pub fn set_secret(&mut self, arg: &str, secret: ConnectionToken) -> Option<String> {
        let Match::One(index) = self.index_of(arg) else {
            return None;
        };
        self.records[index].secret = secret;
        Some(self.records[index].address.clone())
    }

    /// Put `fingerprint` on the server `arg` names.
    ///
    /// Nothing changes when no record answers to `arg`, and nothing changes
    /// when more than one does.
    ///
    /// The store is not written; the caller does that.
    pub fn pin(&mut self, arg: &str, fingerprint: String) {
        if let Match::One(index) = self.index_of(arg) {
            self.records[index].fingerprint = Some(fingerprint);
        }
    }

    /// Stamp the last-used time of the server `arg` names with `now`.
    ///
    /// Nothing changes when no record answers to `arg`, and nothing changes
    /// when more than one does.
    ///
    /// The store is not written; the caller does that.
    pub fn touch(&mut self, arg: &str, now: SystemTime) {
        if let Match::One(index) = self.index_of(arg) {
            self.records[index].last_used_at = Some(now);
        }
    }

    /// Where in `records` the server `arg` names sits: [`Match::One`] with its
    /// index, [`Match::None`] when no record answers to it, and
    /// [`Match::Many`] when more than one does.
    ///
    /// `arg` is matched against every record's name and every record's
    /// address. Two matches are [`Match::Many`], never a pick.
    /// [`ServerStore::save`] refuses every way a store this build wrote could
    /// hold such a pair; a hand-written file can.
    ///
    /// Example — with a record named `work` at `desk.local:7654` and another
    /// at `laptop.local:7654`, `work` and both addresses each name one record.
    /// With two records both named `work`, `work` names neither.
    fn index_of(&self, arg: &str) -> Match {
        let mut matched = self
            .records
            .iter()
            .enumerate()
            .filter(|(_, record)| record.name.as_deref() == Some(arg) || record.address == arg)
            .map(|(index, _)| index);
        let Some(only) = matched.next() else {
            return Match::None;
        };
        if matched.next().is_some() {
            return Match::Many;
        }
        Match::One(only)
    }
}

/// What a selector found in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// One record answers to it.
    Saved(&'a SavedServer),
    /// No record answers to it.
    NotSaved,
    /// More than one record answers to it.
    Ambiguous,
}

/// How many records a selector matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Match {
    /// Exactly one, at this index.
    One(usize),
    /// None.
    None,
    /// More than one.
    Many,
}

/// A name another machine already answers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameTaken {
    /// The name that is already in use.
    pub name: String,
    /// The address of the record already holding it.
    pub address: String,
}

impl std::fmt::Display for NameTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the name {} already belongs to {}; run `koshi remote forget {}` first, \
             or pick another name",
            self.name, self.address, self.name
        )
    }
}

/// Where the saved-server store lives: `remote/servers` under `data_dir`.
///
/// Callers resolve `data_dir` through `koshi_paths::data_dir()`.
#[must_use]
pub fn store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("remote").join("servers")
}

#[cfg(test)]
mod tests;
