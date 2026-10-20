//! The machine's remote access tokens: what a grant records, where it is
//! stored, and what a presented token reaches.
//!
//! A grant hands out one secret and keeps one
//! [`TokenRecord`](crate::remote_tokens::TokenRecord). The record carries the
//! sha256 of that secret and never the secret itself. No stored field opens a
//! connection. sha256 is the 256-bit hash function from the SHA-2 family;
//! [`hash_connection_token`](crate::remote_tokens::hash_connection_token) writes its 32 result
//! bytes as 64 lowercase hex characters.
//!
//! A record names one [`TokenScope`](crate::remote_tokens::TokenScope): the
//! whole machine, or one session. A presented secret reaches a session only
//! when a record holds that secret's hash, still stands, and covers the
//! session. Every other case is refused.
//!
//! The whole set lives in one JSON file —
//! [`resolve_token_store_path`](crate::remote_tokens::resolve_token_store_path) — inside the
//! private koshi data directory. The file carries the format number
//! [`TOKEN_STORE_FORMAT`](crate::remote_tokens::TOKEN_STORE_FORMAT). The normal reader refuses
//! another number; [`remote_migration`](crate::remote_migration) converts format 1 grants
//! when the router starts. Writes go through [`koshi_storage::atomic::write_atomic`]: a reader
//! finds the old content or the new, never a half-written middle. A writer holds the lock at
//! [`resolve_token_store_lock_path`](crate::remote_tokens::resolve_token_store_lock_path) from
//! the read that starts its change to the write that ends it.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use koshi_core::ids::SessionId;

use crate::error::{IpcError, RemoteFile};
use crate::protocol::ConnectionToken;
use crate::remote_state::{
    build_unreadable_remote_file_error, find_format_mismatch, write_remote_file,
};

/// The format number this build writes into every store, and the only one it
/// reads back.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::TOKEN_STORE_FORMAT`].
pub const TOKEN_STORE_FORMAT: u32 = koshi_core::compat::TOKEN_STORE_FORMAT.maximum_version;

/// How long a grant stays in the store after it stopped working: 30 days.
/// [`TokenStore::remove_ended_token_records`] drops a grant revoked, or run
/// out, that long ago or longer.
pub const TOKEN_RECORD_RETENTION_DURATION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How far one grant reaches.
///
/// Decoding rejects a variant this build does not know; a scope from a newer
/// build is an error.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TokenScope {
    /// Every session on this machine, including sessions started after the
    /// grant.
    HostWide,
    /// One named session, and no other.
    Session(SessionId),
}

impl TokenScope {
    /// Whether this scope reaches `session_id`.
    #[must_use]
    pub fn is_allowed_for_session(&self, session_id: SessionId) -> bool {
        match self {
            TokenScope::HostWide => true,
            TokenScope::Session(scoped_session_id) => *scoped_session_id == session_id,
        }
    }
}

/// What the store keeps about one grant.
///
/// Decoding rejects any field it does not know; a misspelled field name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenRecord {
    /// Who the grant was handed to, in the words the operator typed.
    pub identity: String,
    /// The sha256 of the granted secret, as 64 lowercase hex characters. No
    /// field of this record holds the secret itself.
    pub token_hash: String,
    /// How far this grant reaches.
    pub scope: TokenScope,
    /// When the grant was made.
    pub issued_at: SystemTime,
    /// When the grant stops working on its own, or `None` when it never
    /// does.
    pub expires_at: Option<SystemTime>,
    /// When a presented secret last reached a session through this record,
    /// or `None` when none ever has.
    pub last_used_at: Option<SystemTime>,
    /// When an operator stopped the grant, or `None` while it still stands.
    pub revoked_at: Option<SystemTime>,
}

/// Whether a grant stamped `revoked_at` and `expires_at` still stands at
/// `current_time`: nobody revoked it, and it either never expires or expires after
/// `current_time`.
///
/// Example — `revoked_at` `None` with `expires_at` one second before `current_time`
/// gives `false`.
fn is_token_active_at(
    revoked_at: Option<SystemTime>,
    expires_at: Option<SystemTime>,
    current_time: SystemTime,
) -> bool {
    revoked_at.is_none() && expires_at.is_none_or(|expiry| expiry > current_time)
}

impl TokenRecord {
    /// Whether this record still stands at `current_time`: nobody revoked it, and it
    /// either never expires or expires after `current_time`.
    fn is_active_at(&self, current_time: SystemTime) -> bool {
        is_token_active_at(self.revoked_at, self.expires_at, current_time)
    }

    /// When this record stopped working, as seen at `current_time`: the
    /// earlier of its revoke time and an expiry at or before `current_time`,
    /// or `None` while it still stands.
    ///
    /// Example — `revoked_at` 50 and `expires_at` 40, seen at 100, gives 40.
    fn find_ended_at(&self, current_time: SystemTime) -> Option<SystemTime> {
        let expired_at = self.expires_at.filter(|expiry| *expiry <= current_time);
        match (self.revoked_at, expired_at) {
            (Some(revoked_at), Some(expired_at)) => Some(revoked_at.min(expired_at)),
            (revoked_at, expired_at) => revoked_at.or(expired_at),
        }
    }

    /// This record without its hash.
    #[must_use]
    pub fn build_token_entry(&self) -> TokenEntry {
        TokenEntry {
            identity: self.identity.clone(),
            scope: self.scope.clone(),
            issued_at: self.issued_at,
            expires_at: self.expires_at,
            last_used_at: self.last_used_at,
            revoked_at: self.revoked_at,
        }
    }
}

/// One grant as a caller may see it: every field of a [`TokenRecord`] except
/// the hash.
///
/// A field this build does not know is ignored; a record from a newer router
/// still reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenEntry {
    /// Who the grant was handed to.
    pub identity: String,
    /// How far this grant reaches.
    pub scope: TokenScope,
    /// When the grant was made.
    pub issued_at: SystemTime,
    /// When the grant stops working on its own, or `None` when it never
    /// does.
    pub expires_at: Option<SystemTime>,
    /// When a presented secret last reached a session through this grant, or
    /// `None` when none ever has.
    pub last_used_at: Option<SystemTime>,
    /// When an operator stopped the grant, or `None` while it still stands.
    pub revoked_at: Option<SystemTime>,
}

impl TokenEntry {
    /// Whether this grant still stands at `current_time`: nobody revoked it, and it
    /// either never expires or expires after `current_time`.
    #[must_use]
    pub fn is_active_at(&self, current_time: SystemTime) -> bool {
        is_token_active_at(self.revoked_at, self.expires_at, current_time)
    }
}

/// Every grant this machine has made.
///
/// Decoding rejects any field it does not know; a misspelled field name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenStore {
    /// The format number of the file these records came from or go to.
    pub store_format: u32,
    /// One record per grant, in the order the grants were made.
    pub token_records: Vec<TokenRecord>,
}

impl Default for TokenStore {
    fn default() -> Self {
        TokenStore::new()
    }
}

impl TokenStore {
    /// An empty store at the format number this build writes.
    #[must_use]
    pub fn new() -> TokenStore {
        TokenStore {
            store_format: TOKEN_STORE_FORMAT,
            token_records: Vec::new(),
        }
    }

    /// Read the store at `token_store_path`.
    ///
    /// A path with no file is an empty store: this machine has granted
    /// nothing yet. A file that cannot be read, whose bytes are not a
    /// readable store, or whose format number is not
    /// [`TOKEN_STORE_FORMAT`] is [`IpcError::RemoteFileUnreadable`] naming
    /// [`RemoteFile::TokenStore`].
    pub fn load_token_store_from_path(token_store_path: &Path) -> Result<TokenStore, IpcError> {
        let build_refusal = |error_detail: String| {
            build_unreadable_remote_file_error(
                RemoteFile::TokenStore,
                token_store_path,
                error_detail,
            )
        };
        let token_store_bytes = match std::fs::read(token_store_path) {
            Ok(token_store_bytes) => token_store_bytes,
            Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(TokenStore::new())
            }
            Err(io_error) => return Err(build_refusal(io_error.to_string())),
        };
        let token_store: TokenStore = serde_json::from_slice(&token_store_bytes)
            .map_err(|parse_error| build_refusal(parse_error.to_string()))?;
        if let Some(format_error) =
            find_format_mismatch(token_store.store_format, TOKEN_STORE_FORMAT)
        {
            return Err(build_refusal(format_error));
        }
        Ok(token_store)
    }

    /// Write this store at `token_store_path`, replacing whatever is there, and create
    /// the directory holding it when it is missing.
    ///
    /// The file is restricted to the owning user: mode `0600` on Unix, set on
    /// an existing file before the replace; the new file carries it too. On
    /// Windows the file takes the data directory's owner-scoped ACLs. The
    /// directory itself gets mode `0700` on Unix.
    ///
    /// Any failure along the way is [`IpcError::RemoteFileWrite`] naming
    /// [`RemoteFile::TokenStore`].
    pub fn write_token_store_to_path(&self, token_store_path: &Path) -> Result<(), IpcError> {
        write_remote_file(RemoteFile::TokenStore, token_store_path, self)
    }

    /// Hand `identity` a fresh secret on `scope` and keep its hash.
    ///
    /// Returns the secret to show the operator once, and whether this call
    /// stopped a grant that was still standing at `issued_at`. A record that
    /// was already revoked or already expired is replaced with the rest, and
    /// reports `false`: nothing that still worked stopped working.
    ///
    /// One record per identity and scope: every record `identity` held on
    /// `scope` is dropped, and the new record goes at the end. The store is
    /// not written; the caller does that.
    pub fn grant_token(
        &mut self,
        identity: String,
        scope: TokenScope,
        issued_at: SystemTime,
        expires_at: Option<SystemTime>,
    ) -> (ConnectionToken, bool) {
        let connection_token = ConnectionToken::generate();
        let has_replaced_active_grant = self.token_records.iter().any(|token_record| {
            token_record.identity == identity
                && token_record.scope == scope
                && token_record.is_active_at(issued_at)
        });
        self.token_records.retain(|token_record| {
            token_record.identity != identity || token_record.scope != scope
        });
        self.token_records.push(TokenRecord {
            identity,
            token_hash: hash_connection_token(&connection_token),
            scope,
            issued_at,
            expires_at,
            last_used_at: None,
            revoked_at: None,
        });
        (connection_token, has_replaced_active_grant)
    }

    /// Stop every standing grant `identity` holds, narrowed to one scope when
    /// `scope` is given.
    ///
    /// Returns the scope of each grant this call stopped. A record an earlier
    /// call already stopped keeps that earlier time and stays out of the
    /// answer. The store is not written; the caller does that.
    pub fn revoke_token_grants(
        &mut self,
        identity: &str,
        scope: Option<&TokenScope>,
        current_time: SystemTime,
    ) -> Vec<TokenScope> {
        let mut revoked_scopes = Vec::new();
        for token_record in &mut self.token_records {
            if token_record.identity != identity || token_record.revoked_at.is_some() {
                continue;
            }
            if scope.is_some_and(|requested_scope| *requested_scope != token_record.scope) {
                continue;
            }
            token_record.revoked_at = Some(current_time);
            revoked_scopes.push(token_record.scope.clone());
        }
        revoked_scopes
    }

    /// What `connection_token` reaches at `current_time`.
    ///
    /// The presented secret is hashed once, then every record is walked, in
    /// order, and each hash compared through its last byte; a hash whose
    /// length differs is unequal at once, with no byte compared. The walk runs
    /// to the end and reads no record out of a map. Returns the scope of the
    /// last record holding that hash that still stands at `current_time`, and
    /// `None` when no record does. No record changes.
    ///
    /// The caller checks the scope against the session it wants with
    /// [`TokenScope::is_allowed_for_session`].
    #[must_use]
    pub fn find_token_scope(
        &self,
        connection_token: &ConnectionToken,
        current_time: SystemTime,
    ) -> Option<TokenScope> {
        let presented_token_hash = hash_connection_token(connection_token);
        let mut matching_token_scope = None;
        for token_record in &self.token_records {
            let is_token_hash_matching: bool = token_record
                .token_hash
                .as_bytes()
                .ct_eq(presented_token_hash.as_bytes())
                .into();
            if is_token_hash_matching && token_record.is_active_at(current_time) {
                matching_token_scope = Some(&token_record.scope);
            }
        }
        matching_token_scope.cloned()
    }

    /// Stamp `used_at` as the last-used time of every record holding
    /// `token_hash` whose last-used time is unset or earlier. Returns whether
    /// a record changed.
    ///
    /// Example — a record last used at 300 keeps 300 for a `used_at` of 200,
    /// and takes 400 for a `used_at` of 400.
    pub fn update_token_last_used(&mut self, token_hash: &str, used_at: SystemTime) -> bool {
        let mut has_changed = false;
        for token_record in &mut self.token_records {
            let is_newer_use = token_record
                .last_used_at
                .is_none_or(|last_used_at| last_used_at < used_at);
            if token_record.token_hash == token_hash && is_newer_use {
                token_record.last_used_at = Some(used_at);
                has_changed = true;
            }
        }
        has_changed
    }

    /// Drop every record that stopped working — revoked, or run out —
    /// [`TOKEN_RECORD_RETENTION_DURATION`] or longer before `current_time`.
    /// A record whose stop time lies after `current_time` stays.
    ///
    /// Example — with `current_time` at day 40, a grant revoked on day 5 is
    /// dropped and one revoked on day 15 stays.
    pub fn remove_ended_token_records(&mut self, current_time: SystemTime) {
        self.token_records.retain(|token_record| {
            let Some(ended_at) = token_record.find_ended_at(current_time) else {
                return true;
            };
            current_time
                .duration_since(ended_at)
                .map_or(true, |ended_duration| {
                    ended_duration < TOKEN_RECORD_RETENTION_DURATION
                })
        });
    }

    /// Every grant without its hash, narrowed to the grants that reach one
    /// scope when `requested_scope` is given.
    ///
    /// A session scope lists every grant that reaches that session: a
    /// host-wide grant is listed beside the grants scoped to the session
    /// itself. A host-wide scope lists the host-wide grants alone.
    ///
    /// Sorted by identity, then by scope with host-wide before session and
    /// sessions by id.
    #[must_use]
    pub fn list_token_entries(&self, requested_scope: Option<&TokenScope>) -> Vec<TokenEntry> {
        let mut token_entries: Vec<TokenEntry> = self
            .token_records
            .iter()
            .filter(|token_record| {
                requested_scope.is_none_or(|requested_scope| match requested_scope {
                    TokenScope::HostWide => token_record.scope == TokenScope::HostWide,
                    TokenScope::Session(session_id) => {
                        token_record.scope.is_allowed_for_session(*session_id)
                    }
                })
            })
            .map(TokenRecord::build_token_entry)
            .collect();
        token_entries.sort_by(|left_token_entry, right_token_entry| {
            left_token_entry
                .identity
                .cmp(&right_token_entry.identity)
                .then_with(|| left_token_entry.scope.cmp(&right_token_entry.scope))
        });
        token_entries
    }
}

/// Where the remote access token store lives: `remote/tokens` under
/// `data_directory`.
///
/// Callers resolve `data_directory` through `koshi_paths::resolve_data_directory()`.
#[must_use]
pub fn resolve_token_store_path(data_directory: &Path) -> PathBuf {
    data_directory.join("remote").join("tokens")
}

/// Where the lock that guards a change to the token store at
/// `token_store_path` lives: `tokens.lock` in the same directory.
///
/// A writer holds this file's advisory lock from the read that starts its
/// change to the write that ends it. [`TokenStore::write_token_store_to_path`]
/// renames a new file over `token_store_path`, and leaves this path alone. The
/// file stays empty.
#[must_use]
pub fn resolve_token_store_lock_path(token_store_path: &Path) -> PathBuf {
    token_store_path.with_file_name("tokens.lock")
}

/// The sha256 of `connection_token`'s secret, as 64 lowercase hex characters.
#[must_use]
pub fn hash_connection_token(connection_token: &ConnectionToken) -> String {
    crate::bytes::format_hex(&Sha256::digest(connection_token.expose_secret().as_bytes()))
}

#[cfg(test)]
mod tests;
