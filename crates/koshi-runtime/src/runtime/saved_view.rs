//! The views detached clients left behind, and the tokens that take them back.
//!
//! Every attach mints one token. When that client detaches, its view
//! ([`SavedView`]) is filed under the sha256 of that token. sha256 is the
//! 256-bit hash function from the SHA-2 family;
//! [`koshi_ipc::remote_tokens::hash_connection_token`] writes its 32 result
//! bytes as 64 lowercase hex characters. The store holds the hash and never the
//! secret.
//!
//! Presenting the token hands the view back once and drops the record, so a
//! second presentation of the same token finds nothing. A record stands for
//! 24 hours from the moment it was filed, and the store keeps at most 32 of
//! them. Presenting the token of a client that is still attached finds that
//! client ([`SavedViewStore::find_client_id_by_resume_token`]).
//!
//! The token an attach presented, when that attach claimed a client record or
//! took back a view, also stands for the attached client until the client
//! says that it read its attach answer
//! ([`SavedViewStore::hold_presented_token`]). A view filed in that time is
//! filed under both hashes, and either token takes it back once. Example:
//! `bob` presents token `A`, the session mints token `B` for the new
//! client, and the link drops before the answer that carries `B` reaches
//! `bob`. The view is filed under `A` and `B`, and `bob`'s next attach
//! with `A` takes it back.
//!
//! The store lives in memory. An image swap writes it, hashes and filed views
//! included, into the resume file
//! ([`ResumeBody::saved_view_store`](crate::resume::ResumeBody::saved_view_store)),
//! and the next image reads it back: a token minted before the swap takes its
//! view back after it, and proves its client's id
//! ([`SavedViewStore::find_client_id_by_resume_token`]). Nothing in the store
//! is sent over a socket.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use koshi_core::ids::{ClientId, FloatingPaneId, PaneId, TabId};
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::remote_tokens::hash_connection_token;
use koshi_session::client::{ArrivalNotice, Client, FloatingPaneView};

/// How long a filed view stands before it is dropped: 24 hours.
const SAVED_VIEW_LIFETIME_DURATION: Duration = Duration::from_secs(24 * 60 * 60);

/// How many filed views the store keeps: 32. Filing a view over that count
/// drops the oldest.
const MAX_SAVED_VIEW_RECORD_COUNT: usize = 32;

/// What one detached client was looking at, and the index and label it held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedView {
    /// The client's index ([`Client::get_color_index`]).
    pub color_index: u32,
    /// The client's label ([`Client::get_label`]), such as `bob-2`.
    pub label: String,
    /// The tab the client was viewing.
    pub active_tab_id: TabId,
    /// The panes the client focused in each tab, newest first
    /// ([`Client::list_focus_history`]), keyed by tab id. A tab with no entry
    /// had no focused pane.
    pub focus_history_by_tab_id: HashMap<TabId, Vec<PaneId>>,
    /// The pane the client had zoomed in each tab, keyed by tab id. A tab with
    /// no entry was tiled.
    pub zoomed_pane_id_by_tab_id: HashMap<TabId, PaneId>,
    /// How far the client had scrolled up each pane, keyed by pane id, each
    /// value the lines scrolled up from the live bottom. A pane with no entry
    /// sat at the live bottom.
    pub scroll_offset_by_pane_id: HashMap<PaneId, usize>,
    /// The client's view of each floating pane of its session, keyed by
    /// floating pane id: where it drew the floating pane, whether it minimized
    /// it, the panes it focused inside it, and the pane it zoomed there.
    pub floating_pane_view_by_floating_pane_id: HashMap<FloatingPaneId, FloatingPaneView>,
    /// The floating panes the client showed, oldest first. The last one held
    /// the client's input.
    pub shown_floating_pane_ids: Vec<FloatingPaneId>,
    /// The arrival notice the other clients had not been given when the
    /// client detached ([`Client::get_unannounced_arrival`]), or `None` when
    /// they saw it arrive.
    pub unannounced_arrival: Option<ArrivalNotice>,
}

/// One filed view: the sha256 of each token that takes it back, when it stops
/// standing, and the view itself. Either token takes the view back, once.
#[derive(Debug, Serialize, Deserialize)]
struct SavedViewRecord {
    connection_token_hashes: Vec<String>,
    expires_at: SystemTime,
    saved_view: SavedView,
}

/// The views detached clients left behind, each filed under the sha256 of
/// every token that takes it back.
///
/// `connection_token_hash_by_client_id` holds the hash minted for each attached client,
/// waiting for that client to detach. `presented_token_hash_by_client_id`
/// holds the hash of the token a client's attach presented when that attach
/// claimed a record or took back a view, standing until that client proves it
/// holds its attach answer. `saved_view_records` holds the
/// filed views, oldest first.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SavedViewStore {
    connection_token_hash_by_client_id: HashMap<ClientId, String>,
    presented_token_hash_by_client_id: HashMap<ClientId, String>,
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

    /// Keep the sha256 of `presented_token`, the token `client_id`'s attach
    /// presented, standing for `client_id` until
    /// [`release_presented_token`](Self::release_presented_token). Until then
    /// the token finds `client_id`
    /// ([`find_client_id_by_resume_token`](Self::find_client_id_by_resume_token)),
    /// and the view `client_id` leaves is filed under it too
    /// ([`save_client_view`](Self::save_client_view)). A hash already kept for
    /// `client_id` is replaced.
    pub fn hold_presented_token(&mut self, client_id: ClientId, presented_token: &ConnectionToken) {
        self.presented_token_hash_by_client_id
            .insert(client_id, hash_connection_token(presented_token));
    }

    /// Drop the hash [`hold_presented_token`](Self::hold_presented_token) kept
    /// for `client_id`: from then on that token finds nothing and takes back
    /// nothing `client_id` leaves. A client with no hash kept changes nothing.
    pub fn release_presented_token(&mut self, client_id: ClientId) {
        self.presented_token_hash_by_client_id.remove(&client_id);
    }

    /// File `client`'s view under the hash minted for it and the hash of the
    /// token its attach presented, standing until 24 hours after
    /// `observed_time`. Either token takes the view back, once. Both hashes are
    /// dropped from `client`'s id.
    ///
    /// Files nothing when no hash stands against `client`'s id — the client was
    /// never minted and holds no presented token, its hashes were already spent
    /// by an earlier [`save_client_view`](Self::save_client_view), or
    /// [`forget_client_resume_token`](Self::forget_client_resume_token) dropped
    /// them.
    ///
    /// Drops every record that stopped standing at or before `observed_time`, then drops
    /// oldest-first until at most 32 remain.
    ///
    /// Files nothing, and drops both hashes, when `observed_time` plus 24 hours
    /// is past the largest `SystemTime` this platform holds.
    pub fn save_client_view(&mut self, client: &Client, observed_time: SystemTime) {
        let client_id = client.get_client_id();
        let connection_token_hashes: Vec<String> = [
            self.connection_token_hash_by_client_id.remove(&client_id),
            self.presented_token_hash_by_client_id.remove(&client_id),
        ]
        .into_iter()
        .flatten()
        .collect();
        if connection_token_hashes.is_empty() {
            return;
        }
        let Some(expires_at) = observed_time.checked_add(SAVED_VIEW_LIFETIME_DURATION) else {
            return;
        };
        self.saved_view_records
            .retain(|saved_view_record| saved_view_record.expires_at > observed_time);
        self.saved_view_records.push(SavedViewRecord {
            connection_token_hashes,
            expires_at,
            saved_view: SavedView {
                color_index: client.get_color_index(),
                label: client.get_label().to_owned(),
                active_tab_id: client.get_active_tab_id(),
                focus_history_by_tab_id: client.list_focus_histories().clone(),
                zoomed_pane_id_by_tab_id: client.list_zoomed_pane_ids().clone(),
                scroll_offset_by_pane_id: client.list_scroll_offsets().clone(),
                floating_pane_view_by_floating_pane_id: client.list_floating_pane_views(),
                shown_floating_pane_ids: client.list_shown_floating_pane_ids().to_vec(),
                unannounced_arrival: client.get_unannounced_arrival(),
            },
        });
        while self.saved_view_records.len() > MAX_SAVED_VIEW_RECORD_COUNT {
            self.saved_view_records.remove(0);
        }
    }

    /// Drop the hash filed against `client_id` and the hash of the token its
    /// attach presented, and file no record, so neither token takes back
    /// anything.
    ///
    /// Dropping a client that has no hash filed changes nothing.
    pub fn forget_client_resume_token(&mut self, client_id: ClientId) {
        self.connection_token_hash_by_client_id.remove(&client_id);
        self.presented_token_hash_by_client_id.remove(&client_id);
    }

    /// Drop the hash filed against `client_id` when its attach minted a token,
    /// and keep the hash of the token its attach presented: a
    /// [`save_client_view`](Self::save_client_view) after it files the view
    /// under the presented token alone. Dropping a client that has no minted
    /// hash changes nothing.
    pub fn forget_minted_resume_token(&mut self, client_id: ClientId) {
        self.connection_token_hash_by_client_id.remove(&client_id);
    }

    /// The client whose attach minted `resume_token`, or whose attach presented
    /// it and still holds it ([`hold_presented_token`](Self::hold_presented_token)),
    /// and whose hash still waits for its detach. `None` when no such hash is
    /// that token's sha256.
    ///
    /// Compares every waiting hash through its last byte, never stopping at
    /// the first match. Changes nothing in the store.
    #[must_use]
    pub fn find_client_id_by_resume_token(
        &self,
        resume_token: &ConnectionToken,
    ) -> Option<ClientId> {
        let presented_token_hash = hash_connection_token(resume_token);
        let mut matched_client_id = None;
        let waiting_token_hashes = self
            .connection_token_hash_by_client_id
            .iter()
            .chain(&self.presented_token_hash_by_client_id);
        for (&client_id, connection_token_hash) in waiting_token_hashes {
            if is_same_token_hash(connection_token_hash, &presented_token_hash) {
                matched_client_id = Some(client_id);
            }
        }
        matched_client_id
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
            for connection_token_hash in &saved_view_record.connection_token_hashes {
                if is_same_token_hash(connection_token_hash, &presented_token_hash) {
                    matched_record_index = Some(record_index);
                }
            }
        }
        Some(
            self.saved_view_records
                .remove(matched_record_index?)
                .saved_view,
        )
    }
}

/// Whether `stored_token_hash` and `presented_token_hash` are the same text,
/// compared through the last byte of each whatever the first difference.
fn is_same_token_hash(stored_token_hash: &str, presented_token_hash: &str) -> bool {
    stored_token_hash
        .as_bytes()
        .ct_eq(presented_token_hash.as_bytes())
        .into()
}

#[cfg(test)]
mod tests;
