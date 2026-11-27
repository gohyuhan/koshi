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
//! own command lands its focus on the pane
//! ([`Server::clear_attention_on_focus_landing`]), its terminal reports
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
use koshi_terminal::state::ProgramFact;

use crate::server::Server;

impl Server {
    /// Apply `program_facts`, the facts one terminal advance of `pane_id` read,
    /// in byte order, and return their events in the same order. `read_at` is
    /// the wall time the bytes were read, and `current_time` the instant the
    /// engine read them.
    ///
    /// - `CommandStarted` records `current_time` as the pane's command start
    ///   and gives [`Event::PaneCommandStarted`].
    /// - `CommandFinished` takes the command start and gives
    ///   [`Event::PaneCommandFinished`], then raises
    ///   [`AttentionReason::CommandFinished`] with the whole seconds since that
    ///   start. A command start this server does not hold gives
    ///   `elapsed_secs: None`, which raises. Fewer than
    ///   [`MIN_ATTENTION_COMMAND_SECONDS`] raise nothing.
    /// - `Bell` raises [`AttentionReason::Bell`].
    /// - `Notify` raises [`AttentionReason::Notify`], withdrawable by its OSC 99
    ///   id when it has one.
    /// - `NotificationWithdrawn` clears the attention whose withdrawable id it
    ///   names, with `client_id: None`. Another id clears nothing.
    ///
    /// Example: `C` read at 14:00:00 and `D;101` read at 14:00:12.2 give
    /// `PaneCommandStarted`, then `PaneCommandFinished` and
    /// `PaneAttentionRaised` with
    /// `CommandFinished { exit_code: Some(101), elapsed_secs: Some(12) }`.
    pub(crate) fn apply_program_facts(
        &mut self,
        pane_id: PaneId,
        program_facts: Vec<ProgramFact>,
        read_at: SystemTime,
        current_time: Instant,
    ) -> Vec<Event> {
        let mut emitted_events = Vec::new();
        for program_fact in program_facts {
            match program_fact {
                ProgramFact::CommandStarted => {
                    self.command_started_at_by_pane_id
                        .insert(pane_id, current_time);
                    emitted_events.push(Event::PaneCommandStarted(PaneCommandStarted { pane_id }));
                }
                ProgramFact::CommandFinished { exit_code } => {
                    let elapsed_secs =
                        self.command_started_at_by_pane_id
                            .remove(&pane_id)
                            .map(|started_at| {
                                let elapsed_duration =
                                    current_time.saturating_duration_since(started_at);
                                u32::try_from(elapsed_duration.as_secs()).unwrap_or(u32::MAX)
                            });
                    emitted_events
                        .push(Event::PaneCommandFinished(PaneCommandFinished { pane_id }));
                    let is_long_enough = elapsed_secs
                        .is_none_or(|elapsed_secs| elapsed_secs >= MIN_ATTENTION_COMMAND_SECONDS);
                    if is_long_enough {
                        emitted_events.extend(self.raise_pane_attention(
                            pane_id,
                            AttentionReason::CommandFinished {
                                exit_code,
                                elapsed_secs,
                            },
                            None,
                            read_at,
                        ));
                    }
                }
                ProgramFact::Bell => {
                    emitted_events.extend(self.raise_pane_attention(
                        pane_id,
                        AttentionReason::Bell,
                        None,
                        read_at,
                    ));
                }
                ProgramFact::Notify {
                    title,
                    body,
                    notification_id,
                } => {
                    emitted_events.extend(self.raise_pane_attention(
                        pane_id,
                        AttentionReason::Notify { title, body },
                        notification_id,
                        read_at,
                    ));
                }
                ProgramFact::NotificationWithdrawn { notification_id } => {
                    emitted_events
                        .extend(self.withdraw_pane_notification(pane_id, &notification_id));
                }
            }
        }
        emitted_events
    }

    /// Make `pane_id` wait for a person with `reason`, first read at
    /// `read_at`. `withdrawable_notification_id` is the OSC 99 id that
    /// withdraws the attention, or `None`.
    ///
    /// Returns `None`, with nothing changed, when `koshi.kdl` switches off
    /// `reason`'s kind (`AttentionConfig::should_raise_for`), when no session
    /// holds the pane, and when an attached client whose terminal reported
    /// [`TerminalFocus::Focused`] holds the pane as its active focused pane.
    ///
    /// A pane that waits for nobody takes `raised_at: read_at`, and the call
    /// returns [`Event::PaneAttentionRaised`]. A pane that already waits keeps
    /// its `raised_at` and takes `withdrawable_notification_id`. With another
    /// reason, it takes `reason` and the call returns the event with the kept
    /// `raised_at`. With the same reason, the call returns `None`.
    fn raise_pane_attention(
        &mut self,
        pane_id: PaneId,
        reason: AttentionReason,
        withdrawable_notification_id: Option<String>,
        read_at: SystemTime,
    ) -> Option<Event> {
        if !self
            .config
            .attention
            .should_raise_for(reason.get_reason_kind())
        {
            return None;
        }
        let session = self.get_session_for_pane_mut(pane_id)?;
        let is_pane_watched = session.clients.list_attached_clients().any(|client| {
            client.get_terminal_focus() == TerminalFocus::Focused
                && client.get_active_focused_pane_id() == Some(pane_id)
        });
        if is_pane_watched {
            return None;
        }
        let tab_id = session
            .find_tab_by_pane_id(pane_id)
            .map(|tab| tab.get_tab_id());
        let pane_record = session.panes.get_pane_record_mut_by_id(pane_id)?;
        let raised_at = match pane_record.attention.as_mut() {
            Some(attention) => {
                attention.withdrawable_notification_id = withdrawable_notification_id;
                if attention.reason == reason {
                    return None;
                }
                attention.reason = reason.clone();
                attention.raised_at
            }
            None => {
                pane_record.attention = Some(Attention {
                    raised_at: read_at,
                    reason: reason.clone(),
                    withdrawable_notification_id,
                });
                read_at
            }
        };
        Some(Event::PaneAttentionRaised(PaneAttentionRaised {
            pane_id,
            tab_id,
            reason,
            raised_at,
        }))
    }

    /// End `pane_id`'s wait because its program withdrew the OSC 99
    /// notification `notification_id`. Returns the
    /// [`Event::PaneAttentionCleared`] with `client_id: None` when the
    /// pane's attention is withdrawable by `notification_id`, and `None`, with
    /// nothing changed, for every other attention and for a pane that waits
    /// for nobody.
    fn withdraw_pane_notification(
        &mut self,
        pane_id: PaneId,
        notification_id: &str,
    ) -> Option<Event> {
        let is_withdrawable = self
            .get_session_for_pane(pane_id)?
            .panes
            .get_pane_record_by_id(pane_id)?
            .attention
            .as_ref()?
            .withdrawable_notification_id
            .as_deref()
            == Some(notification_id);
        if !is_withdrawable {
            return None;
        }
        self.clear_pane_attention(pane_id, None)
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
    ///
    /// Example: Alice reads tab 2 while pane 3 in tab 1 waits. Her
    /// `core:focus-tab` to tab 1 lands her input on pane 3, and pane 3 stops
    /// waiting. The same switch from a script's `koshi focus-tab` is not a
    /// key binding of Alice's, and the caller does not call this.
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
