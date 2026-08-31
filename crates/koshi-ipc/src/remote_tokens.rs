//! The machine's remote access tokens: what a grant records, where it is
//! stored, and what a presented token reaches.
//!
//! A grant hands out one secret and keeps one
//! [`TokenRecord`](crate::remote_tokens::TokenRecord). The record carries the
//! sha256 of that secret and never the secret itself. No stored field opens a
//! connection. sha256 is the 256-bit hash function from the SHA-2 family;
//! [`hash_token`](crate::remote_tokens::hash_token) writes its 32 result
//! bytes as 64 lowercase hex characters.
//!
//! A record names one [`TokenScope`](crate::remote_tokens::TokenScope): the
//! whole machine, or one session. A presented secret reaches a session only
//! when a record holds that secret's hash, still stands, and covers the
//! session. Every other case is refused.
//!
//! The whole set lives in one JSON file —
//! [`store_path`](crate::remote_tokens::store_path) — inside the private
//! koshi data directory. The file carries the format number
//! [`TOKEN_STORE_FORMAT`](crate::remote_tokens::TOKEN_STORE_FORMAT), and a
//! file carrying any other number is refused. Writes go through
//! [`koshi_storage::atomic::write_atomic`]: a reader finds the old content
//! or the new, never a half-written middle.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use koshi_core::ids::SessionId;

use crate::error::{IpcError, RemoteFile};
use crate::protocol::ConnectionToken;
use crate::remote_state::{format_mismatch, unreadable, write_private};

/// The format number this build writes into every store, and the only one it
/// reads back.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::TOKEN_STORE_FORMAT`].
pub const TOKEN_STORE_FORMAT: u32 = koshi_core::compat::TOKEN_STORE_FORMAT.max;

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
    /// Whether this scope reaches `session`.
    #[must_use]
    pub fn covers(&self, session: SessionId) -> bool {
        match self {
            TokenScope::HostWide => true,
            TokenScope::Session(id) => *id == session,
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
    pub hash: String,
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
/// `now`: nobody revoked it, and it either never expires or expires after
/// `now`.
///
/// Example — `revoked_at` `None` with `expires_at` one second before `now`
/// gives `false`.
fn still_stands(
    revoked_at: Option<SystemTime>,
    expires_at: Option<SystemTime>,
    now: SystemTime,
) -> bool {
    revoked_at.is_none() && expires_at.is_none_or(|expiry| expiry > now)
}

impl TokenRecord {
    /// Whether this record still stands at `now`: nobody revoked it, and it
    /// either never expires or expires after `now`.
    fn is_live(&self, now: SystemTime) -> bool {
        still_stands(self.revoked_at, self.expires_at, now)
    }

    /// This record without its hash.
    #[must_use]
    pub fn entry(&self) -> TokenEntry {
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
    /// Whether this grant still stands at `now`: nobody revoked it, and it
    /// either never expires or expires after `now`.
    #[must_use]
    pub fn is_live(&self, now: SystemTime) -> bool {
        still_stands(self.revoked_at, self.expires_at, now)
    }
}

/// What a presented secret reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// A live record holds that secret's hash and its scope covers the
    /// session asked for.
    Admitted,
    /// Everything else.
    Refused,
}

/// Every grant this machine has made.
///
/// Decoding rejects any field it does not know; a misspelled field name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenStore {
    /// The format number of the file these records came from or go to.
    pub format: u32,
    /// One record per grant, in the order the grants were made.
    pub records: Vec<TokenRecord>,
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
            format: TOKEN_STORE_FORMAT,
            records: Vec::new(),
        }
    }

    /// Read the store at `path`.
    ///
    /// A path with no file is an empty store: this machine has granted
    /// nothing yet. A file that cannot be read, whose bytes are not a
    /// readable store, or whose format number is not
    /// [`TOKEN_STORE_FORMAT`] is [`IpcError::RemoteFileUnreadable`] naming
    /// [`RemoteFile::TokenStore`].
    pub fn read(path: &Path) -> Result<TokenStore, IpcError> {
        let refused = |detail: String| unreadable(RemoteFile::TokenStore, path, detail);
        let data = match std::fs::read(path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(TokenStore::new())
            }
            Err(error) => return Err(refused(error.to_string())),
        };
        let store: TokenStore =
            serde_json::from_slice(&data).map_err(|error| refused(error.to_string()))?;
        if let Some(detail) = format_mismatch(store.format, TOKEN_STORE_FORMAT) {
            return Err(refused(detail));
        }
        Ok(store)
    }

    /// Write this store at `path`, replacing whatever is there, and create
    /// the directory holding it when it is missing.
    ///
    /// The file is restricted to the owning user: mode `0600` on Unix, set on
    /// an existing file before the replace; the new file carries it too. On
    /// Windows the file takes the data directory's owner-scoped ACLs. The
    /// directory itself gets mode `0700` on Unix.
    ///
    /// Any failure along the way is [`IpcError::RemoteFileWrite`] naming
    /// [`RemoteFile::TokenStore`].
    pub fn write(&self, path: &Path) -> Result<(), IpcError> {
        write_private(RemoteFile::TokenStore, path, self)
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
    pub fn grant(
        &mut self,
        identity: String,
        scope: TokenScope,
        issued_at: SystemTime,
        expires_at: Option<SystemTime>,
    ) -> (ConnectionToken, bool) {
        let token = ConnectionToken::generate();
        let replaced = self.records.iter().any(|record| {
            record.identity == identity && record.scope == scope && record.is_live(issued_at)
        });
        self.records
            .retain(|record| record.identity != identity || record.scope != scope);
        self.records.push(TokenRecord {
            identity,
            hash: hash_token(&token),
            scope,
            issued_at,
            expires_at,
            last_used_at: None,
            revoked_at: None,
        });
        (token, replaced)
    }

    /// Stop every standing grant `identity` holds, narrowed to one scope when
    /// `scope` is given.
    ///
    /// Returns the scope of each grant this call stopped. A record an earlier
    /// call already stopped keeps that earlier time and stays out of the
    /// answer. The store is not written; the caller does that.
    pub fn revoke(
        &mut self,
        identity: &str,
        scope: Option<&TokenScope>,
        now: SystemTime,
    ) -> Vec<TokenScope> {
        let mut stopped = Vec::new();
        for record in &mut self.records {
            if record.identity != identity || record.revoked_at.is_some() {
                continue;
            }
            if scope.is_some_and(|wanted| *wanted != record.scope) {
                continue;
            }
            record.revoked_at = Some(now);
            stopped.push(record.scope.clone());
        }
        stopped
    }

    /// Where in `records` the last record sits that holds the hash `presented`,
    /// still stands at `now`, and whose scope `reaches` accepts. `None` when no
    /// record does.
    ///
    /// Every record is walked, in order, and each hash compared through its
    /// last byte; a hash whose length differs from `presented` is unequal at
    /// once, with no byte compared. The walk runs to the end and reads no
    /// record out of a map.
    fn last_match(
        &self,
        presented: &str,
        now: SystemTime,
        reaches: impl Fn(&TokenScope) -> bool,
    ) -> Option<usize> {
        let mut found = None;
        for (index, record) in self.records.iter().enumerate() {
            let same_hash: bool = record.hash.as_bytes().ct_eq(presented.as_bytes()).into();
            if same_hash && record.is_live(now) && reaches(&record.scope) {
                found = Some(index);
            }
        }
        found
    }

    /// What `token` reaches on `session` at `now`.
    ///
    /// The presented secret is hashed once, then every record is walked and
    /// each hash compared through its last byte. The answer is
    /// [`Resolution::Admitted`] when a record holds that hash, still stands at
    /// `now`, and covers `session`; every other case is
    /// [`Resolution::Refused`]. Admitting stamps the last such record's
    /// last-used time with `now`. The store is not written; the caller does
    /// that.
    pub fn resolve(
        &mut self,
        token: &ConnectionToken,
        session: SessionId,
        now: SystemTime,
    ) -> Resolution {
        let presented = hash_token(token);
        match self.last_match(&presented, now, |scope| scope.covers(session)) {
            Some(index) => {
                self.records[index].last_used_at = Some(now);
                Resolution::Admitted
            }
            None => Resolution::Refused,
        }
    }

    /// What `token` reaches at `now`, without naming a session.
    ///
    /// The presented secret is hashed once, then every record is walked and
    /// each hash compared through its last byte. The walk runs to the end and
    /// reads no record out of a map. Returns the scope of the last live record
    /// holding that hash, and `None` when no record does. Admitting stamps
    /// that record's last-used time with `now`. The store is not written; the
    /// caller does that.
    ///
    /// The caller checks the scope against the session it wants with
    /// [`TokenScope::covers`].
    pub fn admit(&mut self, token: &ConnectionToken, now: SystemTime) -> Option<TokenScope> {
        let presented = hash_token(token);
        let index = self.last_match(&presented, now, |_| true)?;
        self.records[index].last_used_at = Some(now);
        Some(self.records[index].scope.clone())
    }

    /// Every grant without its hash, narrowed to the grants that reach one
    /// scope when `scope` is given.
    ///
    /// A session scope lists every grant that reaches that session: a
    /// host-wide grant is listed beside the grants scoped to the session
    /// itself. A host-wide scope lists the host-wide grants alone.
    ///
    /// Sorted by identity, then by scope with host-wide before session and
    /// sessions by id.
    #[must_use]
    pub fn entries(&self, scope: Option<&TokenScope>) -> Vec<TokenEntry> {
        let mut entries: Vec<TokenEntry> = self
            .records
            .iter()
            .filter(|record| {
                scope.is_none_or(|wanted| match wanted {
                    TokenScope::HostWide => record.scope == TokenScope::HostWide,
                    TokenScope::Session(session) => record.scope.covers(*session),
                })
            })
            .map(TokenRecord::entry)
            .collect();
        entries.sort_by(|left, right| {
            left.identity
                .cmp(&right.identity)
                .then_with(|| left.scope.cmp(&right.scope))
        });
        entries
    }
}

/// Where the remote access token store lives: `remote/tokens` under
/// `data_dir`.
///
/// Callers resolve `data_dir` through `koshi_paths::data_dir()`.
#[must_use]
pub fn store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("remote").join("tokens")
}

/// The sha256 of `token`'s secret, as 64 lowercase hex characters.
#[must_use]
pub fn hash_token(token: &ConnectionToken) -> String {
    crate::bytes::hex(&Sha256::digest(token.expose().as_bytes()))
}

#[cfg(test)]
mod tests;
