//! Pane attention in the session server: the facts that make a pane wait for
//! a person, and the acts that end the wait (`PaneRecord::attention`).
//!
//! A pane's bell, desktop notification, or finished command raises attention
//! ([`Server::apply_program_facts`]). The signal is dropped when its kind is
//! switched off in `koshi.kdl`, when a finished command ran for less than
//! [`MIN_ATTENTION_COMMAND_SECONDS`], and when a client whose terminal
//! reported focus holds the pane as its active focused pane. A signal on a
//! pane that already waits replaces the reason and keeps `raised_at`.
//!
//! The wait ends at the first of four facts. Three come from one client: its
//! own command lands its focus on the pane while the pane it was on still
//! exists ([`Server::clear_attention_on_focus_landing`]), its terminal reports
//! `CSI I` while the pane takes its input
//! ([`Server::handle_terminal_focus_change`]), or its key, paste or mouse
//! report reaches the pane ([`Server::clear_attended_pane_attention`]). The
//! fourth comes from the program: `OSC 99 ; i=<id> : p=close` for the
//! notification that set the reason.
//!
//! Example: `cargo test` runs for 12 seconds in pane 3 while Alice reads tab
//! 2, and fails with exit code `101`. Pane 3 waits with
//! `CommandFinished { exit_code: Some(101), elapsed_secs: Some(12) }`, and
//! every attached client receives one `PaneAttentionRaised`. Alice switches to
//! the tab, her focus lands on pane 3, and every client receives one
//! `PaneAttentionCleared` naming Alice.

use std::time::{Instant, SystemTime};

use koshi_core::attention::{Attention, AttentionReason, MIN_ATTENTION_COMMAND_SECONDS};
use koshi_core::event::{
    Event, PaneAttentionCleared, PaneAttentionRaised, PaneCommandFinished, PaneCommandStarted,
};
use koshi_core::ids::{ClientId, PaneId};
use koshi_session::client::TerminalFocus;
use koshi_terminal::engine::TimedProgramFact;
use koshi_terminal::state::ProgramFact;

use crate::server::Server;

impl Server {
    /// Apply `program_facts`, the facts one terminal advance of `pane_id` read,
    /// in byte order, and return their events. `reference_read_at` and
    /// `reference_read_instant` are the wall time and the instant of one
    /// moment at or after the read of every fact: a fact read `d` before
    /// `reference_read_instant` was read at `reference_read_at` minus `d`.
    ///
    /// - `CommandStarted` records its read instant as the pane's command
    ///   start and gives [`Event::PaneCommandStarted`].
    /// - `CommandFinished` takes the command start and gives
    ///   [`Event::PaneCommandFinished`], then raises
    ///   [`AttentionReason::CommandFinished`] with the whole seconds from that
    ///   start to its own read instant. A command start this server does not
    ///   hold gives `elapsed_secs: None`, which raises. Fewer than
    ///   [`MIN_ATTENTION_COMMAND_SECONDS`] raise nothing.
    /// - `Bell` raises [`AttentionReason::Bell`].
    /// - `Notify` raises [`AttentionReason::Notify`], withdrawable by its OSC 99
    ///   id when it has one.
    /// - `NotificationWithdrawn` ends the wait whose withdrawable id it names.
    ///   Another id ends nothing.
    ///
    /// A raise is dropped when `koshi.kdl` switches its kind off
    /// (`AttentionConfig::should_raise_for`), and every raise is dropped when
    /// an attached client whose terminal reported [`TerminalFocus::Focused`]
    /// holds the pane as its active focused pane. A pane that waits for nobody
    /// takes the read time of the raise as `raised_at`. A pane that already
    /// waits keeps its `raised_at`, and takes the new reason and its
    /// withdrawable id.
    ///
    /// After the last fact, one event at most reports the change, after every
    /// command event: [`Event::PaneAttentionRaised`] when the pane waits with
    /// another reason or another `raised_at` than before, and
    /// [`Event::PaneAttentionCleared`] with `client_id: None` when the pane
    /// waited before and waits no more.
    ///
    /// Example: `C` read at 14:00:00 and `D;101` read at 14:00:12.2 give
    /// `PaneCommandStarted`, then `PaneCommandFinished` and
    /// `PaneAttentionRaised` with
    /// `CommandFinished { exit_code: Some(101), elapsed_secs: Some(12) }`.
    /// `BEL ESC ] 9 ; x ESC \` read 500 times in one chunk gives one
    /// `PaneAttentionRaised`, with the reason `Notify` and the read time of
    /// the first `BEL`.
    pub(crate) fn apply_program_facts(
        &mut self,
        pane_id: PaneId,
        program_facts: Vec<TimedProgramFact>,
        reference_read_at: SystemTime,
        reference_read_instant: Instant,
    ) -> Vec<Event> {
        let mut emitted_events = Vec::new();
        let attention_before = self
            .find_pane_attention(pane_id)
            .map(|attention| (attention.reason.clone(), attention.raised_at));
        let is_pane_watched = self.is_pane_watched(pane_id);
        for timed_program_fact in program_facts {
            let read_instant = timed_program_fact.read_instant;
            let read_age = reference_read_instant.saturating_duration_since(read_instant);
            let read_at = reference_read_at
                .checked_sub(read_age)
                .unwrap_or(reference_read_at);
            match timed_program_fact.program_fact {
                ProgramFact::CommandStarted => {
                    self.command_started_at_by_pane_id
                        .insert(pane_id, read_instant);
                    emitted_events.push(Event::PaneCommandStarted(PaneCommandStarted { pane_id }));
                }
                ProgramFact::CommandFinished { exit_code } => {
                    let elapsed_secs =
                        self.command_started_at_by_pane_id
                            .remove(&pane_id)
                            .map(|started_at| {
                                let elapsed_duration =
                                    read_instant.saturating_duration_since(started_at);
                                u32::try_from(elapsed_duration.as_secs()).unwrap_or(u32::MAX)
                            });
                    emitted_events
                        .push(Event::PaneCommandFinished(PaneCommandFinished { pane_id }));
                    let is_long_enough = elapsed_secs
                        .is_none_or(|elapsed_secs| elapsed_secs >= MIN_ATTENTION_COMMAND_SECONDS);
                    if is_long_enough && !is_pane_watched {
                        self.raise_pane_attention(
                            pane_id,
                            AttentionReason::CommandFinished {
                                exit_code,
                                elapsed_secs,
                            },
                            None,
                            read_at,
                        );
                    }
                }
                ProgramFact::Bell => {
                    if !is_pane_watched {
                        self.raise_pane_attention(pane_id, AttentionReason::Bell, None, read_at);
                    }
                }
                ProgramFact::Notify {
                    title,
                    body,
                    notification_id,
                } => {
                    if !is_pane_watched {
                        self.raise_pane_attention(
                            pane_id,
                            AttentionReason::Notify { title, body },
                            notification_id,
                            read_at,
                        );
                    }
                }
                ProgramFact::NotificationWithdrawn { notification_id } => {
                    self.withdraw_pane_notification(pane_id, &notification_id);
                }
            }
        }
        emitted_events.extend(self.report_pane_attention_change(pane_id, attention_before));
        emitted_events
    }

    /// What `pane_id` waits for, or `None` for a pane that waits for nobody
    /// and a pane no session holds.
    fn find_pane_attention(&self, pane_id: PaneId) -> Option<&Attention> {
        self.get_session_for_pane(pane_id)?
            .panes
            .get_pane_record_by_id(pane_id)?
            .attention
            .as_ref()
    }

    /// Whether an attached client whose terminal reported
    /// [`TerminalFocus::Focused`] holds `pane_id` as its active focused pane.
    fn is_pane_watched(&self, pane_id: PaneId) -> bool {
        self.get_session_for_pane(pane_id).is_some_and(|session| {
            session.clients.list_attached_clients().any(|client| {
                client.get_terminal_focus() == TerminalFocus::Focused
                    && client.get_active_focused_pane_id() == Some(pane_id)
            })
        })
    }

    /// Make `pane_id` wait for a person with `reason`, read at `read_at`.
    /// `withdrawable_notification_id` is the OSC 99 id that withdraws the
    /// attention, or `None`.
    ///
    /// Nothing changes when `koshi.kdl` switches off `reason`'s kind
    /// (`AttentionConfig::should_raise_for`) and when no session holds the
    /// pane. A pane that waits for nobody takes `raised_at: read_at`. A pane
    /// that already waits keeps its `raised_at`, and takes `reason` and
    /// `withdrawable_notification_id`.
    fn raise_pane_attention(
        &mut self,
        pane_id: PaneId,
        reason: AttentionReason,
        withdrawable_notification_id: Option<String>,
        read_at: SystemTime,
    ) {
        if !self
            .config
            .attention
            .should_raise_for(reason.get_reason_kind())
        {
            return;
        }
        let Some(pane_record) = self
            .get_session_for_pane_mut(pane_id)
            .and_then(|session| session.panes.get_pane_record_mut_by_id(pane_id))
        else {
            return;
        };
        match pane_record.attention.as_mut() {
            Some(attention) => {
                attention.reason = reason;
                attention.withdrawable_notification_id = withdrawable_notification_id;
            }
            None => {
                pane_record.attention = Some(Attention {
                    raised_at: read_at,
                    reason,
                    withdrawable_notification_id,
                });
            }
        }
    }

    /// End `pane_id`'s wait because its program withdrew the OSC 99
    /// notification `notification_id`, when the pane's attention is
    /// withdrawable by `notification_id`. Every other attention, and a pane
    /// that waits for nobody, stays as it is.
    fn withdraw_pane_notification(&mut self, pane_id: PaneId, notification_id: &str) {
        let Some(pane_record) = self
            .get_session_for_pane_mut(pane_id)
            .and_then(|session| session.panes.get_pane_record_mut_by_id(pane_id))
        else {
            return;
        };
        let is_withdrawable = pane_record.attention.as_ref().is_some_and(|attention| {
            attention.withdrawable_notification_id.as_deref() == Some(notification_id)
        });
        if is_withdrawable {
            pane_record.attention = None;
        }
    }

    /// The event that reports how `pane_id`'s attention changed since
    /// `attention_before`, the reason and `raised_at` it had before:
    /// [`Event::PaneAttentionRaised`] when it waits with another reason or
    /// another `raised_at`, [`Event::PaneAttentionCleared`] with
    /// `client_id: None` when it waited and waits no more, and `None` when
    /// nothing changed or no session holds the pane.
    fn report_pane_attention_change(
        &self,
        pane_id: PaneId,
        attention_before: Option<(AttentionReason, SystemTime)>,
    ) -> Option<Event> {
        let session = self.get_session_for_pane(pane_id)?;
        let attention_after = session
            .panes
            .get_pane_record_by_id(pane_id)?
            .attention
            .as_ref();
        let tab_id = session
            .find_tab_by_pane_id(pane_id)
            .map(|tab| tab.get_tab_id());
        let Some(attention_after) = attention_after else {
            attention_before.as_ref()?;
            return Some(Event::PaneAttentionCleared(PaneAttentionCleared {
                pane_id,
                tab_id,
                client_id: None,
            }));
        };
        let is_unchanged = attention_before.is_some_and(|(reason_before, raised_at_before)| {
            reason_before == attention_after.reason && raised_at_before == attention_after.raised_at
        });
        if is_unchanged {
            return None;
        }
        Some(Event::PaneAttentionRaised(PaneAttentionRaised {
            pane_id,
            tab_id,
            reason: attention_after.reason.clone(),
            raised_at: attention_after.raised_at,
        }))
    }

    /// End `pane_id`'s wait. `client_id` is the client that attended the
    /// pane, or `None` when the program withdrew its notification. Returns the
    /// [`Event::PaneAttentionCleared`] naming `client_id`, and `None` for a
    /// pane that waits for nobody or that no session holds.
    fn clear_pane_attention(
        &mut self,
        pane_id: PaneId,
        client_id: Option<ClientId>,
    ) -> Option<Event> {
        let session = self.get_session_for_pane_mut(pane_id)?;
        session
            .panes
            .get_pane_record_mut_by_id(pane_id)?
            .attention
            .take()?;
        let tab_id = session
            .find_tab_by_pane_id(pane_id)
            .map(|tab| tab.get_tab_id());
        Some(Event::PaneAttentionCleared(PaneAttentionCleared {
            pane_id,
            tab_id,
            client_id,
        }))
    }

    /// End `pane_id`'s wait because `client_id` attended it, and publish the
    /// [`Event::PaneAttentionCleared`] naming `client_id`. A pane that waits
    /// for nobody publishes nothing.
    pub(crate) fn clear_attended_pane_attention(&mut self, client_id: ClientId, pane_id: PaneId) {
        if let Some(cleared_event) = self.clear_pane_attention(pane_id, Some(client_id)) {
            self.publish_events(&[cleared_event]);
        }
    }

    /// The pane that takes `client_id`'s input
    /// ([`Client::get_active_focused_pane_id`](koshi_session::client::Client::get_active_focused_pane_id)),
    /// or `None` for a client no session holds and a client with no such pane.
    pub(crate) fn find_active_focused_pane_id(&self, client_id: ClientId) -> Option<PaneId> {
        self.get_session_for_client(client_id)?
            .clients
            .get_client_by_id(client_id)?
            .get_active_focused_pane_id()
    }

    /// End the wait of the pane that takes `client_id`'s input after a command
    /// of `client_id`'s own key binding or mouse, when that pane is not
    /// `focused_pane_id_before`, the pane that took the input before the
    /// command ([`clear_attended_pane_attention`](Self::clear_attended_pane_attention)).
    /// When the command removed `focused_pane_id_before` from every session,
    /// nothing ends.
    ///
    /// Example: Alice reads tab 2 while pane 3 in tab 1 waits. Her
    /// `core:focus-tab` to tab 1 lands her input on pane 3, and pane 3 stops
    /// waiting. The same switch from a script's `koshi focus-tab` is not a
    /// key binding of Alice's, and the caller does not call this. Her
    /// `core:close-pane` on the last pane of tab 2 also moves her input to
    /// pane 3, and pane 3 keeps waiting.
    pub(crate) fn clear_attention_on_focus_landing(
        &mut self,
        client_id: ClientId,
        focused_pane_id_before: Option<PaneId>,
    ) {
        let Some(focused_pane_id) = self.find_active_focused_pane_id(client_id) else {
            return;
        };
        if Some(focused_pane_id) == focused_pane_id_before {
            return;
        }
        let is_pane_before_removed = focused_pane_id_before
            .is_some_and(|pane_id_before| self.get_session_for_pane(pane_id_before).is_none());
        if is_pane_before_removed {
            return;
        }
        self.clear_attended_pane_attention(client_id, focused_pane_id);
    }

    /// Record the keyboard focus `client_id`'s terminal reported:
    /// [`TerminalFocus::Focused`] for `is_terminal_focused` `true` (`CSI I`),
    /// [`TerminalFocus::Unfocused`] for `false` (`CSI O`). A `CSI I` also ends
    /// the wait of the pane that takes the client's input, naming the client
    /// ([`clear_attended_pane_attention`](Self::clear_attended_pane_attention)).
    /// A client no session holds changes nothing.
    pub(crate) fn handle_terminal_focus_change(
        &mut self,
        client_id: ClientId,
        is_terminal_focused: bool,
    ) {
        let terminal_focus = if is_terminal_focused {
            TerminalFocus::Focused
        } else {
            TerminalFocus::Unfocused
        };
        let Some(client) = self.get_client_mut(client_id) else {
            return;
        };
        client.set_terminal_focus(terminal_focus);
        if !is_terminal_focused {
            return;
        }
        let Some(focused_pane_id) = client.get_active_focused_pane_id() else {
            return;
        };
        self.clear_attended_pane_attention(client_id, focused_pane_id);
    }
}

#[cfg(test)]
mod tests;
