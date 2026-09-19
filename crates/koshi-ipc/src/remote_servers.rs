//! The servers a user has connected to, saved on the dialling user's own
//! machine.
//!
//! One [`SavedServer`](crate::remote_servers::SavedServer) holds the address,
//! the secret the operator handed out, the fingerprint of the certificate
//! that server presented on the first connection — or none until a
//! connection has opened — and an optional name the user chose. The user
//! types the name or the address.
//!
//! The whole set lives in one JSON file —
//! [`resolve_server_store_path`](crate::remote_servers::resolve_server_store_path) — inside the private
//! koshi data directory. The file carries the format number
//! [`SERVER_STORE_FORMAT`](crate::remote_servers::SERVER_STORE_FORMAT), and a
//! file carrying any other number is refused. Writes go through
//! [`koshi_storage::atomic::write_atomic`]: a reader finds the old content
//! or the new, never a half-written middle.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::error::{IpcError, RemoteFile};
use crate::protocol::ConnectionToken;
use crate::remote_state::{find_format_mismatch, write_owner_only};

/// The format number this build writes into every saved-server file, and the
/// only one it reads back.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::SAVED_SERVER_FORMAT`].
pub const SERVER_STORE_FORMAT: u32 = koshi_core::compat::SAVED_SERVER_FORMAT.maximum_version;

/// One server this machine has connected to.
///
/// Decoding rejects any field it does not know; a misspelled field name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedServer {
    /// The name the user chose for this server, or `None` when they chose
    /// none. The user types it in place of the address.
    pub server_name: Option<String>,
    /// Where the server listens, as `host:port`.
    pub server_address: String,
    /// The secret the operator handed out with a grant. `ConnectionToken`'s
    /// `Debug` and `Display` write it redacted.
    pub connection_token: ConnectionToken,
    /// The sha256 of the certificate this server presented on the first
    /// connection, as 64 lowercase hex characters, or `None` while no
    /// connection to it has opened. The next connection that presents a
    /// different certificate is refused; the first connection of a record
    /// holding `None` pins whatever certificate it meets. `None` leaves the
    /// file without this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate_fingerprint: Option<String>,
    /// When this server was first saved.
    pub added_at: SystemTime,
    /// When a connection to this server last opened, or `None` when none has
    /// since it was saved.
    pub last_used_at: Option<SystemTime>,
}

/// Every server this machine has connected to.
///
/// Decoding rejects any field it does not know; a misspelled field name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerStore {
    /// The format number of the file these records came from or go to.
    pub store_format: u32,
    /// One record per server, in the order they were saved. Saving an address
    /// again moves its record to the end.
    pub saved_servers: Vec<SavedServer>,
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
            store_format: SERVER_STORE_FORMAT,
            saved_servers: Vec::new(),
        }
    }

    /// Read the server store at `server_store_path`.
    ///
    /// A path with no file is an empty store: this machine has connected to
    /// nothing yet. A file that cannot be read, whose bytes are not a
    /// readable store, or whose format number is not
    /// [`SERVER_STORE_FORMAT`] is [`IpcError::RemoteFileUnreadable`].
    pub fn load_server_store_from_path(server_store_path: &Path) -> Result<ServerStore, IpcError> {
        let build_unreadable_server_store_error =
            |error_detail: String| IpcError::RemoteFileUnreadable {
                remote_file: RemoteFile::SavedServers,
                remote_file_path: server_store_path.display().to_string(),
                error_detail,
            };
        let server_store_bytes = match std::fs::read(server_store_path) {
            Ok(server_store_bytes) => server_store_bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ServerStore::new())
            }
            Err(io_error) => return Err(build_unreadable_server_store_error(io_error.to_string())),
        };
        let server_store: ServerStore =
            serde_json::from_slice(&server_store_bytes).map_err(|decode_error| {
                build_unreadable_server_store_error(decode_error.to_string())
            })?;
        if let Some(format_error) =
            find_format_mismatch(server_store.store_format, SERVER_STORE_FORMAT)
        {
            return Err(build_unreadable_server_store_error(format_error));
        }
        Ok(server_store)
    }

    /// Write this server store at `server_store_path`, replacing whatever is there, and create
    /// the directory holding it when it is missing.
    ///
    /// The file is restricted to the owning user: mode `0600` on Unix, set on
    /// an existing file before the replace; the new file carries it too. On
    /// Windows the file takes the data directory's owner-scoped ACLs. The
    /// directory itself gets mode `0700` on Unix.
    ///
    /// Any failure along the way is [`IpcError::RemoteFileWrite`].
    pub fn write_server_store_to_path(&self, server_store_path: &Path) -> Result<(), IpcError> {
        write_owner_only(server_store_path, self).map_err(|error_detail| {
            IpcError::RemoteFileWrite {
                remote_file: RemoteFile::SavedServers,
                remote_file_path: server_store_path.display().to_string(),
                error_detail,
            }
        })
    }

    /// The server `server_reference` names.
    ///
    /// A selector matching more than one record is [`SavedServerLookup::Ambiguous`], never
    /// [`SavedServerLookup::NotSaved`].
    #[must_use]
    pub fn find_saved_server(&self, server_reference: &str) -> SavedServerLookup<'_> {
        match self.find_saved_server_match(server_reference) {
            SavedServerMatch::One { saved_server_index } => {
                SavedServerLookup::Saved(&self.saved_servers[saved_server_index])
            }
            SavedServerMatch::None => SavedServerLookup::NotSaved,
            SavedServerMatch::Many => SavedServerLookup::Ambiguous,
        }
    }

    /// Save `saved_server` at the end of `saved_servers`, dropping whatever record already
    /// holds that address.
    ///
    /// Saving an address again replaces its record: the secret and the pinned
    /// fingerprint are the new ones, and the record moves to the end.
    ///
    /// Keeps three rules:
    ///
    /// 1. one address appears once — the replace above,
    /// 2. one name appears once,
    /// 3. no name is another record's address.
    ///
    /// Rules 2 and 3 refuse. Rule 3 is checked in both directions: `saved_server`'s
    /// name against every other saved server's address, and `saved_server`'s address
    /// against every other saved server's name.
    ///
    /// The store is not written; the caller does that.
    ///
    /// # Errors
    /// [`ServerNameTakenError`] carrying the word and the address of the record that
    /// already answers to it.
    pub fn save_server(&mut self, saved_server: SavedServer) -> Result<(), ServerNameTakenError> {
        // Rules 2 and 3: another record answering to this record's name.
        if let Some(server_name) = saved_server.server_name.as_deref() {
            if let Some(name_holding_server) =
                self.find_server_holding_name(server_name, &saved_server.server_address)
            {
                return Err(ServerNameTakenError {
                    server_name: server_name.to_string(),
                    server_address: name_holding_server.server_address.clone(),
                });
            }
        }
        // Rule 3, the other direction: another record answering to this
        // record's address.
        if let Some(name_holding_server) = self
            .find_server_holding_name(&saved_server.server_address, &saved_server.server_address)
        {
            return Err(ServerNameTakenError {
                server_name: saved_server.server_address.clone(),
                server_address: name_holding_server.server_address.clone(),
            });
        }
        let saved_server_address = saved_server.server_address.clone();
        self.saved_servers.retain(|existing_saved_server| {
            existing_saved_server.server_address != saved_server_address
        });
        self.saved_servers.push(saved_server);
        Ok(())
    }

    /// Whether `name` is free to give to the server at `address`.
    ///
    /// True when no record other than the one at `address` answers to `name`,
    /// by its own name or by its own address. The record at `address` may keep
    /// a name it already holds.
    #[must_use]
    pub fn is_server_name_free(&self, server_name: &str, server_address: &str) -> bool {
        self.find_server_holding_name(server_name, server_address)
            .is_none()
    }

    /// The record other than the one at `address` that answers to `name`, by
    /// its own name or by its own address. `None` when no record does, and the
    /// first of them when several do.
    fn find_server_holding_name(
        &self,
        server_name: &str,
        server_address: &str,
    ) -> Option<&SavedServer> {
        self.saved_servers
            .iter()
            .filter(|saved_server| saved_server.server_address != server_address)
            .find(|saved_server| {
                saved_server.server_name.as_deref() == Some(server_name)
                    || saved_server.server_address == server_name
            })
    }

    /// Drop the saved server `server_reference` names, returning its address.
    ///
    /// `None` when no record answers to `server_reference`, and `None` when more than one
    /// does; nothing is removed in either case. [`ServerStore::find_saved_server`] tells the
    /// two apart.
    ///
    /// The store is not written; the caller does that.
    pub fn forget_saved_server(&mut self, server_reference: &str) -> Option<String> {
        let SavedServerMatch::One { saved_server_index } =
            self.find_saved_server_match(server_reference)
        else {
            return None;
        };
        Some(self.saved_servers.remove(saved_server_index).server_address)
    }

    /// Put `connection_token` on the server `server_reference` names, returning its address.
    ///
    /// `None` when no record answers to `server_reference`, and `None` when more than one
    /// does; no secret is written in either case.
    ///
    /// The store is not written; the caller does that.
    pub fn set_connection_token(
        &mut self,
        server_reference: &str,
        connection_token: ConnectionToken,
    ) -> Option<String> {
        let SavedServerMatch::One { saved_server_index } =
            self.find_saved_server_match(server_reference)
        else {
            return None;
        };
        let saved_server = &mut self.saved_servers[saved_server_index];
        saved_server.connection_token = connection_token;
        Some(saved_server.server_address.clone())
    }

    /// Put `certificate_fingerprint` on the server `server_reference` names.
    ///
    /// Nothing changes when no record answers to `arg`, and nothing changes
    /// when more than one does.
    ///
    /// The store is not written; the caller does that.
    pub fn pin_certificate_fingerprint(
        &mut self,
        server_reference: &str,
        certificate_fingerprint: String,
    ) {
        if let SavedServerMatch::One { saved_server_index } =
            self.find_saved_server_match(server_reference)
        {
            self.saved_servers[saved_server_index].certificate_fingerprint =
                Some(certificate_fingerprint);
        }
    }

    /// Stamp the last-used time of the server `server_reference` names with `last_used_at`.
    ///
    /// Nothing changes when no record answers to `arg`, and nothing changes
    /// when more than one does.
    ///
    /// The store is not written; the caller does that.
    pub fn mark_server_used(&mut self, server_reference: &str, last_used_at: SystemTime) {
        if let SavedServerMatch::One { saved_server_index } =
            self.find_saved_server_match(server_reference)
        {
            self.saved_servers[saved_server_index].last_used_at = Some(last_used_at);
        }
    }

    /// Where in `saved_servers` the server `server_reference` names sits:
    /// [`SavedServerMatch::One`] with its index, [`SavedServerMatch::None`]
    /// when no saved server answers to it, and [`SavedServerMatch::Many`] when
    /// more than one does.
    ///
    /// `server_reference` is matched against every saved server's name and address. Two
    /// matches are [`SavedServerMatch::Many`], never a pick.
    /// [`ServerStore::save_server`] refuses every way a store this build wrote could
    /// hold such a pair; a hand-written file can.
    ///
    /// Example — with a record named `work` at `desk.local:7654` and another
    /// at `laptop.local:7654`, `work` and both addresses each name one record.
    /// With two records both named `work`, `work` names neither.
    fn find_saved_server_match(&self, server_reference: &str) -> SavedServerMatch {
        let mut matched_saved_server_indices = self
            .saved_servers
            .iter()
            .enumerate()
            .filter(|(_, saved_server)| {
                saved_server.server_name.as_deref() == Some(server_reference)
                    || saved_server.server_address == server_reference
            })
            .map(|(saved_server_index, _)| saved_server_index);
        let Some(only_saved_server_index) = matched_saved_server_indices.next() else {
            return SavedServerMatch::None;
        };
        if matched_saved_server_indices.next().is_some() {
            return SavedServerMatch::Many;
        }
        SavedServerMatch::One {
            saved_server_index: only_saved_server_index,
        }
    }
}

/// What a selector found in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavedServerLookup<'a> {
    /// One record answers to it.
    Saved(&'a SavedServer),
    /// No record answers to it.
    NotSaved,
    /// More than one record answers to it.
    Ambiguous,
}

/// How many records a selector matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SavedServerMatch {
    /// Exactly one, at this index.
    One { saved_server_index: usize },
    /// None.
    None,
    /// More than one.
    Many,
}

/// A server name another saved server already answers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerNameTakenError {
    /// The name that is already in use.
    pub server_name: String,
    /// The address of the record already holding it.
    pub server_address: String,
}

impl std::fmt::Display for ServerNameTakenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the name {} already belongs to {}; run `koshi remote forget {}` first, \
             or pick another name",
            self.server_name, self.server_address, self.server_name
        )
    }
}

/// Where the saved-server store lives: `remote/servers` under `data_dir`.
///
/// Callers resolve `data_directory` through `koshi_paths::resolve_data_directory()`.
#[must_use]
pub fn resolve_server_store_path(data_directory: &Path) -> PathBuf {
    data_directory.join("remote").join("servers")
}

/// Where the lock that guards a change to the saved-server store lives:
/// `remote/servers.lock` under `data_dir`.
///
/// A writer holds this file's advisory lock from the read that starts its
/// change to the write that ends it. [`ServerStore::write_server_store_to_path`] renames a new file
/// over [`resolve_server_store_path`]'s result, and leaves this path alone.
///
/// The file stays empty. It carries the lock and no content.
#[must_use]
pub fn resolve_server_store_lock_path(data_directory: &Path) -> PathBuf {
    data_directory.join("remote").join("servers.lock")
}

#[cfg(test)]
mod tests;
