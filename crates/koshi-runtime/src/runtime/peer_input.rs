//! The panes other clients' input reached that one subscriber's frames must
//! show.
//!
//! A frame names, for each other client of the session, the pane its keys
//! reach as peers see it (`typed_pane_id`). Input of a client can reach a
//! pane that the last frame sent to a subscriber does not name for that
//! client: the client moved its focus, or turned a local input mode on or off,
//! between two frames of the subscriber. [`PeerInputView`] makes a mark for
//! each such pane. Each frame built for the subscriber names the panes of the
//! marks it holds (`recent_input_pane_ids`). A mark that a sent frame named
//! ends [`PEER_INPUT_MARK_DURATION`] after the last input it records.
//!
//! Example: the last frame sent to Alice names no pane for Bob, whose
//! floating pane tray is open. Bob's `ls` reaches pane 3: Alice's next frame
//! names pane 3 for Bob, and her first frame at least 2 seconds after the
//! `ls` names it no more.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use koshi_core::ids::{ClientId, PaneId};

/// How long a mark that a sent frame named lasts after the last input it
/// records ([`PeerInputView::remove_expired_marks`]).
pub(crate) const PEER_INPUT_MARK_DURATION: Duration = Duration::from_secs(2);

/// One subscriber's record of other clients' input: the typed pane that the
/// last frame sent to the subscriber named for each other client, and the
/// marks of the panes their input reached that a frame of the subscriber must
/// name.
#[derive(Debug, Default)]
pub(crate) struct PeerInputView {
    /// The `typed_pane_id` that the last frame sent to the subscriber named
    /// for each other client. Empty before the first frame.
    sent_typed_pane_id_by_client_id: HashMap<ClientId, Option<PaneId>>,
    /// One mark per client and pane, oldest mark first.
    peer_input_marks: Vec<PeerInputMark>,
}

/// Input of `client_id` reached `pane_id` while the last frame sent to the
/// subscriber named another typed pane, or none, for `client_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeerInputMark {
    /// The client whose input reached the pane.
    client_id: ClientId,
    /// The pane the input reached.
    pane_id: PaneId,
    /// When the last input of `client_id` reached `pane_id`.
    last_input_at: Instant,
    /// Whether a frame sent to the subscriber since the mark was made named
    /// it.
    is_sent: bool,
}

impl PeerInputMark {
    /// Whether a sent frame named this mark and its last input is at least
    /// [`PEER_INPUT_MARK_DURATION`] before `current_time`.
    fn is_expired(&self, current_time: Instant) -> bool {
        self.is_sent && self.last_input_at + PEER_INPUT_MARK_DURATION <= current_time
    }
}

impl PeerInputView {
    /// Record that input of `client_id` reached `pane_id` at `input_at`.
    ///
    /// A mark of `client_id` in `pane_id` takes `input_at` as its last input.
    /// With no such mark, a last sent frame that names `pane_id` as the typed
    /// pane of `client_id` makes no mark, and any other last frame, or no
    /// frame yet, makes one. Returns whether a mark was made: the subscriber's
    /// next frame names it.
    ///
    /// Example: the last frame names pane 5 for client `2`. Input of client
    /// `2` in pane 5 returns `false`, and input of client `2` in pane 3
    /// returns `true`.
    pub(crate) fn record_input(
        &mut self,
        client_id: ClientId,
        pane_id: PaneId,
        input_at: Instant,
    ) -> bool {
        if let Some(peer_input_mark) = self.peer_input_marks.iter_mut().find(|peer_input_mark| {
            peer_input_mark.client_id == client_id && peer_input_mark.pane_id == pane_id
        }) {
            peer_input_mark.last_input_at = input_at;
            return false;
        }
        if self.sent_typed_pane_id_by_client_id.get(&client_id) == Some(&Some(pane_id)) {
            return false;
        }
        self.peer_input_marks.push(PeerInputMark {
            client_id,
            pane_id,
            last_input_at: input_at,
            is_sent: false,
        });
        true
    }

    /// The panes of the marks of `client_id`, oldest mark first, leaving out
    /// `typed_pane_id`.
    pub(crate) fn list_mark_pane_ids(
        &self,
        client_id: ClientId,
        typed_pane_id: Option<PaneId>,
    ) -> Vec<PaneId> {
        self.peer_input_marks
            .iter()
            .filter(|peer_input_mark| {
                peer_input_mark.client_id == client_id
                    && Some(peer_input_mark.pane_id) != typed_pane_id
            })
            .map(|peer_input_mark| peer_input_mark.pane_id)
            .collect()
    }

    /// Record that a frame naming `peer_typed_pane_ids`, each other client
    /// with its `typed_pane_id`, was sent to the subscriber. Every mark held
    /// now counts as named by a sent frame.
    pub(crate) fn record_frame_sent(
        &mut self,
        peer_typed_pane_ids: impl IntoIterator<Item = (ClientId, Option<PaneId>)>,
    ) {
        self.sent_typed_pane_id_by_client_id = peer_typed_pane_ids.into_iter().collect();
        for peer_input_mark in &mut self.peer_input_marks {
            peer_input_mark.is_sent = true;
        }
    }

    /// Remove each mark that a sent frame named whose last input is at least
    /// [`PEER_INPUT_MARK_DURATION`] before `current_time`.
    pub(crate) fn remove_expired_marks(&mut self, current_time: Instant) {
        self.peer_input_marks
            .retain(|peer_input_mark| !peer_input_mark.is_expired(current_time));
    }

    /// Whether a mark that a sent frame named is expired at `current_time`:
    /// the subscriber's next frame names it no more.
    pub(crate) fn has_expired_mark(&self, current_time: Instant) -> bool {
        self.peer_input_marks
            .iter()
            .any(|peer_input_mark| peer_input_mark.is_expired(current_time))
    }

    /// The earliest time after `current_time` at which a mark that a sent
    /// frame named expires, or `None` when no such mark expires after it.
    pub(crate) fn find_next_mark_expiry(&self, current_time: Instant) -> Option<Instant> {
        self.peer_input_marks
            .iter()
            .filter(|peer_input_mark| peer_input_mark.is_sent)
            .map(|peer_input_mark| peer_input_mark.last_input_at + PEER_INPUT_MARK_DURATION)
            .filter(|mark_expiry| *mark_expiry > current_time)
            .min()
    }
}

#[cfg(test)]
mod tests;
