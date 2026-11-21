//! The panes other clients' input reached that one subscriber's frames must
//! show.
//!
//! A frame names, for each other client of the session, the pane its keys
//! reach as peers see it (`typed_pane_id`). Input of a client can reach a
//! pane that the newest frame queued for a subscriber does not name for that
//! client: the client moved its focus, or turned a local input mode on or off,
//! between two frames of the subscriber. `PeerInputView` makes a mark for
//! each such pane. Each frame built for the subscriber names the panes of the
//! marks it holds (`recent_input_pane_ids`).
//!
//! A mark has three states (`MarkDelivery`): no queued frame names it; a
//! queued frame names it, and the session has not seen the thread that
//! writes the subscriber's connection take the first frame that named it;
//! the session saw that thread take that frame. The mark keeps the queue's
//! frame number of that first frame, and the session learns of the take
//! through the queue
//! ([`DeliverySender::request_frame_taken_notice`](crate::runtime::delivery_queue::DeliverySender::request_frame_taken_notice)).
//! A taken mark ends [`PEER_INPUT_MARK_DURATION`] after its last input, or
//! after the time the session saw the take, whichever comes last. A mark of a
//! client that is no longer present, or of a pane the session no longer
//! holds, ends at the next pass.
//!
//! Example: the newest frame queued for Alice names no pane for Bob, whose
//! floating pane tray is open. Bob's `ls` reaches pane 3, and Alice's next
//! frame names pane 3 for Bob. Her client paints slowly, and the writing
//! thread takes that frame 3 seconds after the `ls`. Her first frame at least
//! 2 seconds after that take names pane 3 no more.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use koshi_core::ids::{ClientId, PaneId};

use crate::server::Server;

/// How long a mark lasts after its last input, or after the time the session
/// saw the writing thread take the first frame that named it, whichever comes
/// last.
pub const PEER_INPUT_MARK_DURATION: Duration = Duration::from_secs(2);

/// One subscriber's record of other clients' input: the typed pane that the
/// newest frame queued for the subscriber named for each other client, and
/// the marks of the panes their input reached that a frame of the subscriber
/// must name.
#[derive(Debug, Default)]
pub(crate) struct PeerInputView {
    /// The `typed_pane_id` that the newest frame queued for the subscriber
    /// named for each other client. Empty before the first frame.
    queued_typed_pane_id_by_client_id: HashMap<ClientId, Option<PaneId>>,
    /// One mark per client and pane, oldest mark first.
    peer_input_marks: Vec<PeerInputMark>,
}

/// Input of `client_id` reached `pane_id` while the newest frame queued for
/// the subscriber named another typed pane, or none, for `client_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeerInputMark {
    /// The client whose input reached the pane.
    client_id: ClientId,
    /// The pane the input reached.
    pane_id: PaneId,
    /// When the last input of `client_id` reached `pane_id`.
    last_input_at: Instant,
    /// How far the mark got toward the subscriber's client.
    mark_delivery: MarkDelivery,
}

/// How far a mark got toward the subscriber's client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkDelivery {
    /// No frame queued for the subscriber named the mark yet.
    Unqueued,
    /// The frame numbered `frame_number` on the subscriber's queue is the
    /// first frame that named the mark, and the session has not seen the
    /// writing thread take it.
    Queued {
        /// The queue's frame number of the first frame that named the mark.
        frame_number: u64,
    },
    /// The session saw at `taken_at` that the writing thread had taken the
    /// first frame that named the mark.
    Taken {
        /// When the session saw the take.
        taken_at: Instant,
    },
}

impl PeerInputMark {
    /// When the mark ends: [`PEER_INPUT_MARK_DURATION`] after its last input,
    /// or after `taken_at`, whichever comes last. `None` before the take.
    ///
    /// Example: input at `t`, a take seen at `t + 3 s`, and no input since,
    /// give `t + 5 s`.
    fn find_expiry(&self) -> Option<Instant> {
        match self.mark_delivery {
            MarkDelivery::Taken { taken_at } => {
                Some(taken_at.max(self.last_input_at) + PEER_INPUT_MARK_DURATION)
            }
            MarkDelivery::Unqueued | MarkDelivery::Queued { .. } => None,
        }
    }
}

impl PeerInputView {
    /// Record that input of `client_id` reached `pane_id` at `input_at`.
    ///
    /// A mark of `client_id` in `pane_id` takes `input_at` as its last input.
    /// With no such mark, a newest queued frame that names `pane_id` as the
    /// typed pane of `client_id` makes no mark, and any other newest frame,
    /// or no frame yet, makes one. Returns whether a mark was made: the
    /// subscriber's next frame names it.
    ///
    /// Example: the newest queued frame names pane 5 for client `2`. Input of
    /// client `2` in pane 5 returns `false`, and input of client `2` in pane
    /// 3 returns `true`.
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
        if self.queued_typed_pane_id_by_client_id.get(&client_id) == Some(&Some(pane_id)) {
            return false;
        }
        self.peer_input_marks.push(PeerInputMark {
            client_id,
            pane_id,
            last_input_at: input_at,
            mark_delivery: MarkDelivery::Unqueued,
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

    /// Whether the view holds any mark.
    pub(crate) fn has_marks(&self) -> bool {
        !self.peer_input_marks.is_empty()
    }

    /// Record that the frame numbered `frame_number`, which names
    /// `peer_typed_pane_ids`, each other client with its `typed_pane_id`, was
    /// put on the subscriber's queue. Every mark that no queued frame named
    /// counts as queued at `frame_number`. A queued mark keeps its number.
    pub(crate) fn record_frame_queued(
        &mut self,
        frame_number: u64,
        peer_typed_pane_ids: impl IntoIterator<Item = (ClientId, Option<PaneId>)>,
    ) {
        self.queued_typed_pane_id_by_client_id = peer_typed_pane_ids.into_iter().collect();
        for peer_input_mark in &mut self.peer_input_marks {
            if peer_input_mark.mark_delivery == MarkDelivery::Unqueued {
                peer_input_mark.mark_delivery = MarkDelivery::Queued { frame_number };
            }
        }
    }

    /// The lowest frame number of a queued mark. `None` when no mark is
    /// queued.
    pub(crate) fn find_first_queued_frame_number(&self) -> Option<u64> {
        self.peer_input_marks
            .iter()
            .filter_map(|peer_input_mark| match peer_input_mark.mark_delivery {
                MarkDelivery::Queued { frame_number } => Some(frame_number),
                MarkDelivery::Unqueued | MarkDelivery::Taken { .. } => None,
            })
            .min()
    }

    /// Record that the session saw at `taken_at` that the writing thread took
    /// the frame numbered `taken_frame_number`. Every mark queued at that
    /// frame number or a lower one counts as taken at `taken_at`.
    ///
    /// Example: marks queued at frames 1 and 2. A take of frame 1 counts the
    /// first mark as taken, and the second stays queued.
    pub(crate) fn record_frame_taken(&mut self, taken_frame_number: u64, taken_at: Instant) {
        for peer_input_mark in &mut self.peer_input_marks {
            if let MarkDelivery::Queued { frame_number } = peer_input_mark.mark_delivery {
                if frame_number <= taken_frame_number {
                    peer_input_mark.mark_delivery = MarkDelivery::Taken { taken_at };
                }
            }
        }
    }

    /// Remove each mark whose client and pane `is_mark_target_present` does
    /// not accept, in any state.
    pub(crate) fn remove_absent_marks(
        &mut self,
        is_mark_target_present: impl Fn(ClientId, PaneId) -> bool,
    ) {
        self.peer_input_marks.retain(|peer_input_mark| {
            is_mark_target_present(peer_input_mark.client_id, peer_input_mark.pane_id)
        });
    }

    /// Remove each mark whose expiry ([`PeerInputMark::find_expiry`]) is at or
    /// before `current_time`. Returns whether a mark was removed: the
    /// subscriber's next frame names that pane no more.
    pub(crate) fn remove_expired_marks(&mut self, current_time: Instant) -> bool {
        let mark_count = self.peer_input_marks.len();
        self.peer_input_marks.retain(|peer_input_mark| {
            peer_input_mark
                .find_expiry()
                .is_none_or(|mark_expiry| mark_expiry > current_time)
        });
        self.peer_input_marks.len() != mark_count
    }

    /// The earliest expiry of a taken mark ([`PeerInputMark::find_expiry`]),
    /// past or not. `None` when no mark is taken.
    pub(crate) fn find_next_mark_expiry(&self) -> Option<Instant> {
        self.peer_input_marks
            .iter()
            .filter_map(PeerInputMark::find_expiry)
            .min()
    }
}

impl Server {
    /// Record that input of `client_id` reached `pane_id` now, in the record
    /// of each subscription that views another client of the same session
    /// ([`PeerInputView::record_input`]). A subscription whose record makes a
    /// mark has its view marked changed: its next frame names `pane_id` for
    /// `client_id`. A client no session holds records nothing.
    ///
    /// Five paths call it, each once the pane took the bytes: keystrokes
    /// ([`Server::handle_key_inputs`]), pasted text
    /// ([`Server::handle_host_paste`]), a `core:write-to-pane` write from a key
    /// binding or a mouse action of `client_id`, a forwarded mouse report
    /// ([`Server::forward_mouse_to_pane`]), and alternate-scroll arrows
    /// ([`Server::write_alternate_scroll_arrows`]).
    ///
    /// Example: Alice's newest queued frame names no pane for Bob, whose
    /// floating pane tray is open, and Bob's `ls` reaches pane 3: Alice's
    /// frame is due, and it names pane 3 in Bob's `recent_input_pane_ids`.
    pub(crate) fn record_client_input_in_pane(&mut self, client_id: ClientId, pane_id: PaneId) {
        let Some(session_id) = self
            .get_session_for_client(client_id)
            .map(|session| session.session_id)
        else {
            return;
        };
        let input_at = Instant::now();
        for subscription_index in 0..self.subscriptions.len() {
            let viewing_client_id = self.subscriptions[subscription_index].client_id;
            let is_other_client_of_session = viewing_client_id != client_id
                && self
                    .get_session_for_client(viewing_client_id)
                    .is_some_and(|session| session.session_id == session_id);
            if !is_other_client_of_session {
                continue;
            }
            let frame_subscription = &mut self.subscriptions[subscription_index];
            if frame_subscription
                .peer_input_view
                .record_input(client_id, pane_id, input_at)
            {
                frame_subscription.frame_pace.mark_view_changed();
            }
        }
    }

    /// Count the queued marks of subscription `subscription_index` as taken at
    /// `current_time`, from the lowest frame number up, for each frame the
    /// writing thread has taken
    /// ([`EventBus::request_frame_taken_notice`](crate::runtime::bus::EventBus::request_frame_taken_notice)).
    /// The first frame that still waits keeps a request: the writing thread
    /// hands a
    /// [`RuntimeEvent::QueuedFrameTaken`](crate::runtime::event::RuntimeEvent::QueuedFrameTaken)
    /// to the dispatcher once it takes that frame, and the pass that event
    /// starts counts again.
    ///
    /// Example: Bob's mark is queued at frame 1, and Carol's at frame 2. The
    /// writing thread took frame 1, and frame 2 waits for a paint report.
    /// Bob's mark counts as taken, and Carol's waits for the take of frame 2.
    pub(crate) fn record_peer_input_frames_taken(
        &mut self,
        subscription_index: usize,
        current_time: Instant,
    ) {
        let frame_subscription = &mut self.subscriptions[subscription_index];
        while let Some(first_queued_frame_number) = frame_subscription
            .peer_input_view
            .find_first_queued_frame_number()
        {
            if self.event_bus.request_frame_taken_notice(
                frame_subscription.subscriber_id,
                first_queued_frame_number,
            ) {
                return;
            }
            frame_subscription
                .peer_input_view
                .record_frame_taken(first_queued_frame_number, current_time);
        }
    }

    /// Bring each subscriber's marks of other clients' input up to date at
    /// `current_time`:
    ///
    /// 1. Remove each mark of a client that is not present in the session the
    ///    subscriber views ([`Server::list_present_clients`]), or of a pane
    ///    that session no longer holds. A subscriber whose client no session
    ///    holds loses every mark.
    /// 2. Count each queued mark as taken once the writing thread took the
    ///    first frame that named it
    ///    ([`Server::record_peer_input_frames_taken`]).
    /// 3. Remove each expired mark ([`PeerInputView::remove_expired_marks`]),
    ///    and mark the subscriber's view changed when one was removed.
    ///
    /// A subscriber with no mark is skipped.
    pub(crate) fn update_peer_input_marks(&mut self, current_time: Instant) {
        for subscription_index in 0..self.subscriptions.len() {
            if !self.subscriptions[subscription_index]
                .peer_input_view
                .has_marks()
            {
                continue;
            }
            let viewing_client_id = self.subscriptions[subscription_index].client_id;
            let mut peer_input_view =
                std::mem::take(&mut self.subscriptions[subscription_index].peer_input_view);
            match self.get_session_for_client(viewing_client_id) {
                Some(session) => peer_input_view.remove_absent_marks(|client_id, pane_id| {
                    session.panes.get_pane_record_by_id(pane_id).is_some()
                        && self
                            .list_present_clients(session)
                            .any(|present_client| present_client.get_client_id() == client_id)
                }),
                None => peer_input_view.remove_absent_marks(|_, _| false),
            }
            self.subscriptions[subscription_index].peer_input_view = peer_input_view;
            self.record_peer_input_frames_taken(subscription_index, current_time);
            let frame_subscription = &mut self.subscriptions[subscription_index];
            if frame_subscription
                .peer_input_view
                .remove_expired_marks(current_time)
            {
                frame_subscription.frame_pace.mark_view_changed();
            }
        }
    }
}

#[cfg(test)]
mod tests;
