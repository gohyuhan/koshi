//! Viewer-local actions that `koshi actions run` asks one attached client to
//! run.
//!
//! A viewer-local action, such as `core:toggle-notification-center`, changes
//! one client's own screen, so the client runs it. The session checks the
//! request as it checks a command, puts one
//! [`Delivery::ClientActionRequested`](koshi_renderer::snapshot::Delivery::ClientActionRequested)
//! on that client's queue, and answers the caller once the client answers.
//!
//! Every request gets exactly one answer:
//!
//! - the client ran the action: `Ok`;
//! - the client refused it, such as for an action that does not apply in its
//!   mode: the client's refusal;
//! - the client left the session first, the session started to end first,
//!   the session restarted before the client answered, or
//!   [`CLIENT_ACTION_ANSWER_TIMEOUT_DURATION`] passed first: a refusal that
//!   names the cause;
//! - the request arrived after the session told its clients it restarts: the
//!   [`IpcErrorCode::Restarting`] refusal, and the action did not run.
//!
//! An answer that arrives after its request was answered, or from a client
//! the request was not sent to, is dropped.

use super::*;

use std::collections::btree_map;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use koshi_ipc::protocol::{ClientActionRefusal, IpcErrorCode, IpcErrorPayload, IpcResult};

use crate::ipc_server::SESSION_RESTARTING_MESSAGE;
use crate::runtime::event::SessionEnding;

/// How long the session waits for a client's answer to a viewer action
/// request before it answers the caller itself: 5 s.
pub(crate) const CLIENT_ACTION_ANSWER_TIMEOUT_DURATION: Duration = Duration::from_secs(5);

/// The viewer action requests that wait for their client's answer, keyed by
/// the number each request carried. Numbers rise with every request, so the
/// first entry has the earliest answer deadline.
#[derive(Debug, Default)]
pub(crate) struct WaitingClientActions {
    /// The number the next request carries.
    next_client_action_request_id: u64,
    /// Each request that waits, by its number.
    waiting_client_action_by_request_id: BTreeMap<u64, WaitingClientAction>,
}

/// One viewer action request that waits for its client's answer.
#[derive(Debug)]
struct WaitingClientAction {
    /// The client the request was sent to.
    client_id: ClientId,
    /// The id the caller's answer names.
    command_id: CommandId,
    /// When the session answers for the client.
    answer_deadline: Instant,
    /// Where the caller's answer goes.
    response_sender: Sender<IpcResult>,
}

impl WaitingClientActions {
    /// The earliest answer deadline of a waiting request, or `None` when no
    /// request waits.
    pub(crate) fn find_earliest_answer_deadline(&self) -> Option<Instant> {
        self.waiting_client_action_by_request_id
            .first_key_value()
            .map(|(_, waiting_client_action)| waiting_client_action.answer_deadline)
    }
}

impl WaitingClientAction {
    /// Send the caller `Ok` when `rejection` is `None`, else the rejection.
    fn send_answer(self, rejection: Option<Rejection>) {
        let command_result = match rejection {
            None => CommandResult::Ok {
                command_id: self.command_id,
                emitted_events: Vec::new(),
            },
            Some(rejection) => Server::reject_command(self.command_id, rejection),
        };
        // A caller that stopped waiting dropped its receiver.
        let _ = self
            .response_sender
            .send(IpcResult::CommandResult(command_result));
    }
}

impl Server {
    /// Ask one attached client to run `action_reference`, for a
    /// [`RuntimeEvent::IpcClientAction`] from `command_source`. The answer for
    /// `command_id` goes into `response_sender`.
    ///
    /// The checks run in this order, and the first failure is the answer:
    ///
    /// 1. The session told its clients it restarts: the
    ///    [`IpcErrorCode::Restarting`] refusal with
    ///    [`SESSION_RESTARTING_MESSAGE`]. The session told its clients it
    ///    ends: [`RejectReason::InvalidState`] `session is stopping`.
    /// 2. The session the source names
    ///    ([`Self::resolve_acting_session`]); a winding-down session is
    ///    `session is stopping`.
    /// 3. An in-session source's pane still exists
    ///    ([`Self::require_live_source_pane`]).
    /// 4. The client: the one the source names, else the acting client
    ///    ([`Self::resolve_view_client`]).
    /// 5. A client whose record came across a restart and has not attached
    ///    again: [`RejectReason::InvalidState`] `client <id> is reconnecting`.
    /// 6. A client with no subscriber, or whose queue has no room for the
    ///    request: [`RejectReason::InvalidState`] `client <id> is not reading
    ///    the session; the action did not run`.
    ///
    /// A request that passes waits for the client's answer until
    /// [`CLIENT_ACTION_ANSWER_TIMEOUT_DURATION`] from now.
    pub(crate) fn request_client_action(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        action_reference: ActionReference,
        response_sender: Sender<IpcResult>,
    ) {
        if self.event_bus.get_ending_notice().get_session_ending()
            == Some(SessionEnding::Restarting)
        {
            let _ = response_sender.send(IpcResult::Error(IpcErrorPayload {
                code: IpcErrorCode::Restarting,
                message: SESSION_RESTARTING_MESSAGE.to_owned(),
            }));
            return;
        }
        let reject_request = |rejection: Rejection| {
            let _ = response_sender.send(IpcResult::CommandResult(Self::reject_command(
                command_id, rejection,
            )));
        };
        let client_id = match self.resolve_client_action_target(command_source) {
            Ok(client_id) => client_id,
            Err(rejection) => {
                reject_request(rejection);
                return;
            }
        };
        let not_reading_rejection = || {
            Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                &format!("client {client_id} is not reading the session; the action did not run"),
            )
        };
        let Some(subscriber_id) = self.find_subscriber_id_by_client_id(client_id) else {
            reject_request(not_reading_rejection());
            return;
        };
        let client_action_request_id = self.waiting_client_actions.next_client_action_request_id;
        self.waiting_client_actions.next_client_action_request_id += 1;
        if !self.event_bus.try_send_client_action_request(
            subscriber_id,
            client_action_request_id,
            action_reference,
        ) {
            reject_request(not_reading_rejection());
            return;
        }
        self.waiting_client_actions
            .waiting_client_action_by_request_id
            .insert(
                client_action_request_id,
                WaitingClientAction {
                    client_id,
                    command_id,
                    answer_deadline: Instant::now() + CLIENT_ACTION_ANSWER_TIMEOUT_DURATION,
                    response_sender,
                },
            );
    }

    /// Checks 1 to 5 of [`Self::request_client_action`] after the restart
    /// check: the client the request goes to, or the rejection that answers
    /// it.
    fn resolve_client_action_target(
        &self,
        command_source: &CommandSource,
    ) -> Result<ClientId, Rejection> {
        if self.event_bus.get_ending_notice().get_session_ending() == Some(SessionEnding::Quit) {
            return Err(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                "session is stopping",
            ));
        }
        let session = Self::require_session(self.resolve_acting_session(command_source)?)?;
        if Self::is_winding_down(session) {
            return Err(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                "session is stopping",
            ));
        }
        if let CommandSource::InSessionCli { pane_id, .. } = command_source {
            Self::require_live_source_pane(session, *pane_id)?;
        }
        let client_id = Self::resolve_view_client(
            command_source.get_target_client_id(),
            command_source,
            session,
        )?;
        self.require_client_not_awaiting_reconnect(client_id)?;
        Ok(client_id)
    }

    /// Answer the caller of the request numbered `client_action_request_id`
    /// with `client_id`'s answer: `Ok` when `refusal` is `None`, else the
    /// refusal's reason and help. An answer to a request that does not wait,
    /// or from a client the request was not sent to, is dropped.
    pub(crate) fn answer_client_action(
        &mut self,
        client_id: ClientId,
        client_action_request_id: u64,
        refusal: Option<ClientActionRefusal>,
    ) {
        let waiting_client_action = match self
            .waiting_client_actions
            .waiting_client_action_by_request_id
            .entry(client_action_request_id)
        {
            btree_map::Entry::Occupied(waiting_entry)
                if waiting_entry.get().client_id == client_id =>
            {
                waiting_entry.remove()
            }
            _ => {
                tracing::debug!(
                    %client_id,
                    client_action_request_id,
                    "dropping an answer to a viewer action request that does not wait for this client"
                );
                return;
            }
        };
        let rejection = refusal
            .map(|refusal| Rejection::from_reason_and_help(refusal.reject_reason, &refusal.help));
        waiting_client_action.send_answer(rejection);
    }

    /// Answer every request that waits for `client_id`, which is leaving the
    /// session: [`RejectReason::TargetGone`] `client <id> left the session
    /// before it answered`.
    pub(crate) fn answer_departed_client_actions(&mut self, client_id: ClientId) {
        let departed_client_actions = self
            .waiting_client_actions
            .waiting_client_action_by_request_id
            .extract_if(.., |_, waiting_client_action| {
                waiting_client_action.client_id == client_id
            });
        for (_, waiting_client_action) in departed_client_actions {
            waiting_client_action.send_answer(Some(Rejection::from_reason_and_help(
                RejectReason::TargetGone,
                &format!("client {client_id} left the session before it answered"),
            )));
        }
    }

    /// Answer every request whose answer deadline is at or before
    /// `current_time`: [`RejectReason::InvalidState`] `client <id> did not
    /// answer within 5 s; the action runs if that client reads the request`.
    pub(crate) fn answer_overdue_client_actions(&mut self, current_time: Instant) {
        let waiting_client_actions = &mut self
            .waiting_client_actions
            .waiting_client_action_by_request_id;
        while let Some(first_entry) = waiting_client_actions.first_entry() {
            if first_entry.get().answer_deadline > current_time {
                return;
            }
            let waiting_client_action = first_entry.remove();
            let client_id = waiting_client_action.client_id;
            waiting_client_action.send_answer(Some(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                &format!(
                    "client {client_id} did not answer within {} s; the action runs if that \
                     client reads the request",
                    CLIENT_ACTION_ANSWER_TIMEOUT_DURATION.as_secs()
                ),
            )));
        }
    }

    /// Answer every waiting request, for a session that ends with
    /// `session_ending`: [`RejectReason::InvalidState`] `the session began to
    /// stop before client <id> answered; the action ran if that client read
    /// the request`, or `the session began to restart before ...`.
    pub fn answer_client_actions_at_session_ending(&mut self, session_ending: SessionEnding) {
        let ending_text = match session_ending {
            SessionEnding::Quit => "stop",
            SessionEnding::Restarting => "restart",
        };
        let waiting_client_actions = std::mem::take(
            &mut self
                .waiting_client_actions
                .waiting_client_action_by_request_id,
        );
        for waiting_client_action in waiting_client_actions.into_values() {
            let client_id = waiting_client_action.client_id;
            waiting_client_action.send_answer(Some(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                &format!(
                    "the session began to {ending_text} before client {client_id} answered; \
                     the action ran if that client read the request"
                ),
            )));
        }
    }
}

#[cfg(test)]
mod tests;
