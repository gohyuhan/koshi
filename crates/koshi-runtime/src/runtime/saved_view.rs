//! The views detached clients left behind, and the tokens that take them back.
//!
//! Every attach mints one token. When that client detaches, its view — the tab
//! it was on, the pane it had focused in each tab, the pane it had zoomed in
//! each tab, and how far it had scrolled up each pane — is filed under the
//! sha256 of that token. sha256 is the 256-bit hash function from the SHA-2
//! family; [`koshi_ipc::remote_tokens::hash_connection_token`] writes its 32 result bytes
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
use koshi_ipc::remote_tokens::hash_connection_token;
use koshi_session::client::Client;

/// How long a filed view stands before it is dropped: 120 seconds.
const SAVED_VIEW_LIFETIME_DURATION: Duration = Duration::from_secs(120);

/// How many filed views the store keeps: 32. Filing a view over that count
/// drops the oldest.
const MAX_SAVED_VIEW_RECORD_COUNT: usize = 32;

/// What one detached client was looking at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedView {
    /// The tab the client was viewing.
    pub active_tab_id: TabId,
    /// The pane the client had focused in each tab, keyed by tab id. A tab with
    /// no entry had no focused pane.
    pub focused_pane_id_by_tab_id: HashMap<TabId, PaneId>,
    /// The pane the client had zoomed in each tab, keyed by tab id. A tab with
    /// no entry was tiled.
    pub zoomed_pane_id_by_tab_id: HashMap<TabId, PaneId>,
    /// How far the client had scrolled up each pane, keyed by pane id, each
    /// value the lines scrolled up from the live bottom. A pane with no entry
    /// sat at the live bottom.
    pub scroll_offset_by_pane_id: HashMap<PaneId, usize>,
}

/// One filed view: the sha256 of the token that takes it back, when it stops
/// standing, and the view itself.
#[derive(Debug)]
struct SavedViewRecord {
    connection_token_hash: String,
    expires_at: SystemTime,
    saved_view: SavedView,
}

/// The views detached clients left behind, keyed by the sha256 of the token
/// that takes each one back.
///
/// `connection_token_hash_by_client_id` holds the hash minted for each attached client,
/// waiting for that client to detach. `saved_view_records` holds the filed
/// views, oldest first.
#[derive(Debug, Default)]
pub struct SavedViewStore {
    connection_token_hash_by_client_id: HashMap<ClientId, String>,
    saved_view_records: Vec<SavedViewRecord>,
}

impl SavedViewStore {
    /// Mint the token that takes back `client_id`'s view, and file that token's
    /// sha256 against `client_id`. Any hash already filed against `client_id` is
    /// replaced, so the earlier token takes back nothing.
    ///
    /// Returns the secret. This is the only place it exists.
    pub fn mint_resume_token(&mut self, client_id: ClientId) -> ConnectionToken {
        let resume_token = ConnectionToken::generate();
        self.connection_token_hash_by_client_id
            .insert(client_id, hash_connection_token(&resume_token));
        resume_token
    }

    /// File `client`'s view under the hash minted for it, standing until 120
    /// seconds after `observed_time`.
    ///
    /// Files nothing when no hash stands against `client`'s id — the client was
    /// never minted, its hash was already spent by an earlier
    /// [`save_client_view`](Self::save_client_view), or
    /// [`forget_client_resume_token`](Self::forget_client_resume_token) dropped
    /// it.
    ///
    /// Drops every record that stopped standing at or before `observed_time`, then drops
    /// oldest-first until at most 32 remain.
    ///
    /// Files nothing, and drops the hash, when `observed_time` plus 120 seconds is past
    /// the largest `SystemTime` this platform holds.
    pub fn save_client_view(&mut self, client: &Client, observed_time: SystemTime) {
        let Some(connection_token_hash) = self
            .connection_token_hash_by_client_id
            .remove(&client.get_client_id())
        else {
            return;
        };
        let Some(expires_at) = observed_time.checked_add(SAVED_VIEW_LIFETIME_DURATION) else {
            return;
        };
        self.saved_view_records
            .retain(|saved_view_record| saved_view_record.expires_at > observed_time);
        self.saved_view_records.push(SavedViewRecord {
            connection_token_hash,
            expires_at,
            saved_view: SavedView {
                active_tab_id: client.get_active_tab(),
                focused_pane_id_by_tab_id: client.list_focused_panes().clone(),
                zoomed_pane_id_by_tab_id: client.list_zoomed_panes().clone(),
                scroll_offset_by_pane_id: client.list_scroll_offsets().clone(),
            },
        });
        while self.saved_view_records.len() > MAX_SAVED_VIEW_RECORD_COUNT {
            self.saved_view_records.remove(0);
        }
    }

    /// Drop the hash filed against `client_id` and file no record, so the token
    /// [`mint_resume_token`](Self::mint_resume_token) handed out takes back
    /// nothing.
    ///
    /// Dropping a client that has no hash filed changes nothing.
    pub fn forget_client_resume_token(&mut self, client_id: ClientId) {
        self.connection_token_hash_by_client_id.remove(&client_id);
    }

    /// Take back the view filed under `resume_token`, or `None` when no standing
    /// record holds that token's sha256 at `observed_time`.
    ///
    /// Drops every record that stopped standing at or before `observed_time`, then
    /// compares every remaining record's hash through its last byte, never
    /// stopping at the first match.
    ///
    /// The record is consumed: presenting the same token a second time returns
    /// `None`.
    pub fn take_saved_view(
        &mut self,
        resume_token: &ConnectionToken,
        observed_time: SystemTime,
    ) -> Option<SavedView> {
        let presented_token_hash = hash_connection_token(resume_token);
        self.saved_view_records
            .retain(|saved_view_record| saved_view_record.expires_at > observed_time);
        let mut matched_record_index = None;
        for (record_index, saved_view_record) in self.saved_view_records.iter().enumerate() {
            let is_matching_token_hash: bool = saved_view_record
                .connection_token_hash
                .as_bytes()
                .ct_eq(presented_token_hash.as_bytes())
                .into();
            if is_matching_token_hash {
                matched_record_index = Some(record_index);
            }
        }
        Some(
            self.saved_view_records
                .remove(matched_record_index?)
                .saved_view,
        )
    }
}

#[cfg(test)]
mod tests;
