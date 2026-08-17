//! The views detached clients left behind, and the tokens that take them back.
//!
//! Every attach mints one token. When that client detaches, its view — the tab
//! it was on, the pane it had focused in each tab, the pane it had zoomed in
//! each tab, and how far it had scrolled up each pane — is filed under the
//! sha256 of that token. sha256 is the 256-bit hash function from the SHA-2
//! family; [`koshi_ipc::remote_tokens::hash_token`] writes its 32 result bytes
//! as 64 lowercase hex characters. The store holds the hash and never the
//! secret.
//!
//! Presenting the token hands the view back once and drops the record, so a
//! second presentation of the same token finds nothing. A record stands for
//! 120 seconds from the moment it was filed, and the store keeps at most 32 of
//! them.
//!
//! The whole store lives in memory. Nothing here is written to disk, sent over
//! a socket, or carried across a server restart: a restart drops every saved
//! view, and every minted token then resumes nothing.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use subtle::ConstantTimeEq;

use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::remote_tokens::hash_token;
use koshi_session::client::Client;

/// How long a filed view stands before it is dropped: 120 seconds.
const LIFETIME: Duration = Duration::from_secs(120);

/// How many filed views the store keeps: 32. Filing a view over that count
/// drops the oldest.
const MAX_RECORDS: usize = 32;

/// What one detached client was looking at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedView {
    /// The tab the client was viewing.
    pub active_tab: TabId,
    /// The pane the client had focused in each tab, keyed by tab id. A tab with
    /// no entry had no focused pane.
    pub focus_by_tab: HashMap<TabId, PaneId>,
    /// The pane the client had zoomed in each tab, keyed by tab id. A tab with
    /// no entry was tiled.
    pub zoom_by_tab: HashMap<TabId, PaneId>,
    /// How far the client had scrolled up each pane, keyed by pane id, each
    /// value the lines scrolled up from the live bottom. A pane with no entry
    /// sat at the live bottom.
    pub scroll_by_pane: HashMap<PaneId, usize>,
}

/// One filed view: the sha256 of the token that takes it back, when it stops
/// standing, and the view itself.
#[derive(Debug)]
struct Record {
    hash: String,
    expires_at: SystemTime,
    view: SavedView,
}

/// The views detached clients left behind, keyed by the sha256 of the token
/// that takes each one back.
///
/// `hash_by_client` holds the hash minted for each attached client, waiting for
/// that client to detach. `records` holds the filed views, oldest first.
#[derive(Debug, Default)]
pub struct SavedViewStore {
    hash_by_client: HashMap<ClientId, String>,
    records: Vec<Record>,
}

impl SavedViewStore {
    /// Mint the token that takes back `client_id`'s view, and file that token's
    /// sha256 against `client_id`. Any hash already filed against `client_id` is
    /// replaced, so the earlier token takes back nothing.
    ///
    /// Returns the secret. This is the only place it exists.
    pub fn mint(&mut self, client_id: ClientId) -> ConnectionToken {
        let token = ConnectionToken::generate();
        self.hash_by_client.insert(client_id, hash_token(&token));
        token
    }

    /// File `client`'s view under the hash minted for it, standing until 120
    /// seconds after `now`.
    ///
    /// Files nothing when no hash stands against `client`'s id — the client was
    /// never minted, its hash was already spent by an earlier `save`, or
    /// [`forget`](Self::forget) dropped it.
    ///
    /// Drops every record that stopped standing at or before `now`, then drops
    /// oldest-first until at most 32 remain.
    ///
    /// Files nothing, and drops the hash, when `now` plus 120 seconds is past
    /// the largest `SystemTime` this platform holds.
    pub fn save(&mut self, client: &Client, now: SystemTime) {
        let Some(hash) = self.hash_by_client.remove(&client.id()) else {
            return;
        };
        let Some(expires_at) = now.checked_add(LIFETIME) else {
            return;
        };
        self.records.retain(|record| record.expires_at > now);
        self.records.push(Record {
            hash,
            expires_at,
            view: SavedView {
                active_tab: client.active_tab(),
                focus_by_tab: client.focused_panes().clone(),
                zoom_by_tab: client.zoomed_panes().clone(),
                scroll_by_pane: client.scroll_offsets().clone(),
            },
        });
        while self.records.len() > MAX_RECORDS {
            self.records.remove(0);
        }
    }

    /// Drop the hash filed against `client_id` and file no record, so the token
    /// [`mint`](Self::mint) handed out takes back nothing.
    ///
    /// Dropping a client that has no hash filed changes nothing.
    pub fn forget(&mut self, client_id: ClientId) {
        self.hash_by_client.remove(&client_id);
    }

    /// Take back the view filed under `token`, or `None` when no standing record
    /// holds that token's sha256 at `now`.
    ///
    /// Drops every record that stopped standing at or before `now`, then
    /// compares every remaining record's hash through its last byte, never
    /// stopping at the first match.
    ///
    /// The record is consumed: presenting the same token a second time returns
    /// `None`.
    pub fn take(&mut self, token: &ConnectionToken, now: SystemTime) -> Option<SavedView> {
        let presented = hash_token(token);
        self.records.retain(|record| record.expires_at > now);
        let mut admitted = None;
        for (index, record) in self.records.iter().enumerate() {
            let same: bool = record.hash.as_bytes().ct_eq(presented.as_bytes()).into();
            if same {
                admitted = Some(index);
            }
        }
        Some(self.records.remove(admitted?).view)
    }
}

#[cfg(test)]
mod tests;
