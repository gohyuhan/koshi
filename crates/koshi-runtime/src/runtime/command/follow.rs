//! Follow: a client whose view copies the view of another client of its
//! session, its leader.
//!
//! [`Command::FollowClient`] starts, moves or ends a follow
//! ([`Client::set_followed_client_id`]). Each time
//! [`Server::record_view_changes`] runs, the session copies each leader's
//! view onto its followers ([`Server::copy_followed_views`]): the active tab,
//! the focus history and zoom there, the floating panes the leader shows with
//! the focus history and zoom inside each, and the scroll offset of each pane
//! of the leader's active tab and of every floating pane ([`FollowedView`]).
//! Floating pane positions, highlights, lock mode, mouse selection and the
//! terminal size are not copied.
//!
//! A follow ends when:
//!
//! 1. the follower's own key binding, click, wheel or pane placement, or a
//!    command that names the follower in its `client_id` argument or as its
//!    CLI target, changes a copied part of the follower's view. The change
//!    applies, then the follow ends
//!    ([`Server::list_acting_follower_views`],
//!    [`Server::clear_follows_of_changed_views`]);
//! 2. a key, paste or pane write reaches a pane for the follower, or a mouse
//!    report or wheel arrows from the follower reach a program
//!    ([`Server::clear_followed_client_id`]);
//! 3. the leader leaves the session ([`ClientRegistry::detach_client`]);
//! 4. a [`Command::FollowClient`] names nobody, or `NextPeer` passes the
//!    last peer;
//! 5. the follower puts up a highlight ([`Client::set_selection`]).
//!
//! [`ClientRegistry::detach_client`]: koshi_session::client::ClientRegistry::detach_client
//! [`FollowedView`]: koshi_session::client::FollowedView

use super::*;

use std::borrow::Cow;
use std::collections::HashSet;

use koshi_core::command::{FollowClientArgs, FollowTarget};
use koshi_core::event::SessionMemberId;
use koshi_layout::solver::LayoutSolve;
use koshi_session::client::FollowedView;
use koshi_session::session::state::SessionViewChanges;

use super::resolve::build_member_not_in_session_rejection;
use crate::runtime::event::RuntimeEvent;

/// The follow a [`Command::FollowClient`] sets: the client that follows, and
/// the client it follows from then on, or `None` for no follow.
struct FollowChange {
    follower_client_id: ClientId,
    followed_client_id: Option<ClientId>,
}

/// The copied view of one follower that an event acts for, read before the
/// event ran ([`Server::list_acting_follower_views`]).
pub(crate) struct ActingFollowerView {
    follower_client_id: ClientId,
    scroll_pane_ids: Vec<PaneId>,
    followed_view: FollowedView,
}

/// The clients `client_id`'s follow leads through: the client it follows,
/// the client that one follows, and on, nearest first. The walk stops after as
/// many steps as `session` holds clients.
///
/// Example: with C following B and B following A, C gives `[B, A]`, and A
/// gives nothing.
fn list_follow_chain(
    session: &Session,
    client_id: ClientId,
) -> impl Iterator<Item = ClientId> + '_ {
    let client_count = session.clients.count_clients();
    std::iter::successors(Some(client_id), |&chain_client_id| {
        session
            .clients
            .get_client_by_id(chain_client_id)?
            .get_followed_client_id()
    })
    .skip(1)
    .take(client_count)
}

/// Whether `follower_client_id` may follow `leader_client_id` in `session`:
/// the two differ, `session` holds the leader, and the leader's follow chain
/// ([`list_follow_chain`]) does not reach the follower.
pub(super) fn can_client_follow(
    session: &Session,
    follower_client_id: ClientId,
    leader_client_id: ClientId,
) -> bool {
    leader_client_id != follower_client_id
        && session.clients.get_client_by_id(leader_client_id).is_some()
        && !list_follow_chain(session, leader_client_id)
            .any(|chain_client_id| chain_client_id == follower_client_id)
}

/// The panes whose scroll offsets a follow of a leader viewing `tab_id`
/// copies: every pane of `tab_id`'s tree, then every pane of every floating
/// pane of `session`.
fn list_followed_pane_ids(session: &Session, tab_id: TabId) -> Vec<PaneId> {
    let mut followed_pane_ids = session
        .tabs
        .get(&tab_id)
        .map(|tab| tab.get_layout_tree().list_leaf_pane_ids())
        .unwrap_or_default();
    for floating_pane in session.floating_set.list_floating_panes() {
        followed_pane_ids.extend(floating_pane.list_pane_ids());
    }
    followed_pane_ids
}

/// The clients `runtime_event` acts for as their own input:
///
/// - the client of a [`RuntimeEvent::ClientMouse`] or a
///   [`RuntimeEvent::ConfirmPanePlacement`];
/// - for a [`RuntimeEvent::Ipc`], the client of a key binding or mouse
///   source, the CLI target ([`CommandSource::get_target_client_id`]), and
///   the command's own `client_id` argument
///   ([`Command::get_client_id_argument`]).
///
/// Every other event acts for no client.
fn list_acting_client_ids(runtime_event: &RuntimeEvent) -> [Option<ClientId>; 3] {
    match runtime_event {
        RuntimeEvent::ClientMouse { client_id, .. }
        | RuntimeEvent::ConfirmPanePlacement { client_id, .. } => [Some(*client_id), None, None],
        RuntimeEvent::Ipc {
            command_envelope, ..
        } => {
            let source_client_id = match command_envelope.command_source {
                CommandSource::KeyBinding { client_id } | CommandSource::Mouse { client_id } => {
                    Some(client_id)
                }
                CommandSource::InSessionCli { .. } | CommandSource::ExternalCli { .. } => None,
            };
            [
                source_client_id,
                command_envelope.command_source.get_target_client_id(),
                command_envelope.command.get_client_id_argument(),
            ]
        }
        RuntimeEvent::Resize { .. }
        | RuntimeEvent::CellSize { .. }
        | RuntimeEvent::ClientDetached { .. }
        | RuntimeEvent::ClientAttachAnswerReceived { .. }
        | RuntimeEvent::KeyInput { .. }
        | RuntimeEvent::ClientKeyboard { .. }
        | RuntimeEvent::MouseInput { .. }
        | RuntimeEvent::OuterTerminalFocusLost { .. }
        | RuntimeEvent::HostPaste { .. }
        | RuntimeEvent::ClientPaste { .. }
        | RuntimeEvent::ClientActionAnswer { .. }
        | RuntimeEvent::SetPanePlacementPreview { .. }
        | RuntimeEvent::SetViewerLocalInputMode { .. }
        | RuntimeEvent::PtyReadable { .. }
        | RuntimeEvent::Quit
        | RuntimeEvent::IpcClientAction { .. }
        | RuntimeEvent::IpcAttach { .. }
        | RuntimeEvent::IpcDiscovery { .. }
        | RuntimeEvent::IpcLayout { .. }
        | RuntimeEvent::IpcRestart { .. }
        | RuntimeEvent::IpcClipboardCopyPart { .. }
        | RuntimeEvent::IpcPrepare { .. }
        | RuntimeEvent::IpcClipboardCopyPrepare { .. }
        | RuntimeEvent::IpcClipboardCopy { .. }
        | RuntimeEvent::DropUnclaimedClients { .. }
        | RuntimeEvent::QueueRoomFreed => [None, None, None],
    }
}

impl Server {
    /// Handle [`Command::FollowClient`]: set the client the follower follows
    /// ([`Self::resolve_follow_change`]). Emits no event. The first copy of
    /// the leader's view runs when the event ends
    /// ([`Self::copy_followed_views`]).
    pub(super) fn handle_follow_client(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &FollowClientArgs,
    ) -> Result<CommandResult, Rejection> {
        let session = Self::require_session(self.resolve_acting_session(command_source)?)?;
        let session_id = session.session_id;
        let follow_change = self.resolve_follow_change(command_args, command_source, session)?;
        let follower = self
            .session_by_id
            .get_mut(&session_id)
            .and_then(|session| {
                session
                    .clients
                    .get_client_mut_by_id(follow_change.follower_client_id)
            })
            .ok_or_else(|| Rejection::from_reason(RejectReason::SourceClientStale))?;
        follower.set_followed_client_id(follow_change.followed_client_id);
        Ok(self.commit_events(command_id, Vec::new()))
    }

    /// Resolve a [`Command::FollowClient`]: the follower is
    /// [`Self::resolve_view_client`] of `client_id`, and is not reconnecting
    /// ([`Self::require_client_not_awaiting_reconnect`]). Then
    /// `follow_target` decides whom it follows:
    ///
    /// - a [`FollowTarget::Client`] names that client, when
    ///   [`Self::require_followable_client`] accepts it;
    /// - a [`FollowTarget::NextPeer`] names the client that
    ///   [`Self::resolve_next_followed_client`] gives;
    /// - a [`FollowTarget::Nobody`] names nobody.
    ///
    /// Validation and [`Self::handle_follow_client`] both call it.
    fn resolve_follow_change(
        &self,
        command_args: &FollowClientArgs,
        command_source: &CommandSource,
        session: &Session,
    ) -> Result<FollowChange, Rejection> {
        let follower_client_id =
            Self::resolve_view_client(command_args.client_id, command_source, session)?;
        self.require_client_not_awaiting_reconnect(follower_client_id)?;
        let followed_client_id = match command_args.follow_target {
            FollowTarget::Client(leader_client_id) => {
                self.require_followable_client(session, follower_client_id, leader_client_id)?;
                Some(leader_client_id)
            }
            FollowTarget::NextPeer => {
                self.resolve_next_followed_client(session, follower_client_id)?
            }
            FollowTarget::Nobody => None,
        };
        Ok(FollowChange {
            follower_client_id,
            followed_client_id,
        })
    }

    /// Validate a [`Command::FollowClient`] in `session`: run
    /// [`Self::resolve_follow_change`] and drop its value.
    pub(super) fn validate_follow_client(
        &self,
        command_args: &FollowClientArgs,
        command_source: &CommandSource,
        session: &Session,
    ) -> Result<(), Rejection> {
        self.resolve_follow_change(command_args, command_source, session)
            .map(drop)
    }

    /// Refuse `leader_client_id` as the client `follower_client_id` follows:
    ///
    /// - the follower itself: [`RejectReason::InvalidState`]
    ///   `client-<id> cannot follow itself`;
    /// - a client `session` does not hold: [`RejectReason::TargetNotFound`]
    ///   ``client client-<id> is not attached to session \`work\```;
    /// - a client that is reconnecting
    ///   ([`Self::require_client_not_awaiting_reconnect`]);
    /// - a client whose follow chain reaches the follower:
    ///   [`RejectReason::InvalidState`]
    ///   `following client-<leader> makes a loop: it follows client-<id>`,
    ///   naming the client the leader follows.
    fn require_followable_client(
        &self,
        session: &Session,
        follower_client_id: ClientId,
        leader_client_id: ClientId,
    ) -> Result<(), Rejection> {
        if leader_client_id == follower_client_id {
            return Err(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                &format!("{follower_client_id} cannot follow itself"),
            ));
        }
        let Some(leader) = session.clients.get_client_by_id(leader_client_id) else {
            return Err(build_member_not_in_session_rejection(
                session,
                SessionMemberId::Client(leader_client_id),
            ));
        };
        self.require_client_not_awaiting_reconnect(leader_client_id)?;
        let is_loop = list_follow_chain(session, leader_client_id)
            .any(|chain_client_id| chain_client_id == follower_client_id);
        if let Some(leader_followed_client_id) = leader.get_followed_client_id().filter(|_| is_loop)
        {
            return Err(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                &format!(
                    "following {leader_client_id} makes a loop: it follows {leader_followed_client_id}"
                ),
            ));
        }
        Ok(())
    }

    /// The client a [`FollowTarget::NextPeer`] makes `follower_client_id`
    /// follow. The follower's peers are the other clients of `session`,
    /// ordered by `color_index`, then by client id. The first peer after the
    /// one the follower follows, or the first peer when it follows nobody,
    /// that is not reconnecting and that the follower can follow
    /// ([`can_client_follow`]) is the result. Past the last peer, a
    /// follower that follows a peer gets `None`: its follow ends.
    ///
    /// A follower that follows nobody and finds no peer is
    /// [`RejectReason::InvalidState`] `no other client to follow`.
    ///
    /// Example: with peers 2 and 3, repeated calls give 2, then 3, then
    /// `None`.
    fn resolve_next_followed_client(
        &self,
        session: &Session,
        follower_client_id: ClientId,
    ) -> Result<Option<ClientId>, Rejection> {
        let current_followed_client_id = session
            .clients
            .get_client_by_id(follower_client_id)
            .and_then(Client::get_followed_client_id);
        let mut peers: Vec<&Client> = session
            .clients
            .list_attached_clients()
            .filter(|peer| peer.get_client_id() != follower_client_id)
            .collect();
        peers.sort_by_key(|peer| (peer.get_color_index(), peer.get_client_id()));
        let first_candidate_index = current_followed_client_id
            .and_then(|followed_client_id| {
                peers
                    .iter()
                    .position(|peer| peer.get_client_id() == followed_client_id)
            })
            .map_or(0, |followed_peer_index| followed_peer_index + 1);
        let next_followed_client_id = peers[first_candidate_index..]
            .iter()
            .map(|peer| peer.get_client_id())
            .find(|&peer_client_id| {
                !self.client_ids_awaiting_reconnect.contains(&peer_client_id)
                    && can_client_follow(session, follower_client_id, peer_client_id)
            });
        match (next_followed_client_id, current_followed_client_id) {
            (Some(next_followed_client_id), _) => Ok(Some(next_followed_client_id)),
            (None, Some(_)) => Ok(None),
            (None, None) => Err(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                "no other client to follow",
            )),
        }
    }

    /// Copy each follower's leader's view onto the follower, in each session
    /// of `view_changes_by_session_id` ([`Self::copy_leader_view`]).
    ///
    /// The followers of a session go leader first: a follower whose leader
    /// follows nobody, then a follower of that follower, and on. A pair is
    /// compared when the frame of the leader or of the follower shows a change
    /// in `view_changes_by_session_id` ([`Self::is_client_frame_changed`]),
    /// or when this call copied onto the leader. A pair with a client that is
    /// reconnecting (`client_ids_awaiting_reconnect`) is not compared. The
    /// panes whose scroll offsets a compare reads ([`list_followed_pane_ids`])
    /// are listed once per leader in each call.
    ///
    /// Returns the followers whose view a copy changed.
    ///
    /// Example: C follows B, B follows A, and A switches tab. B's view copies
    /// A's, then C's view copies B's, and the call returns `{B, C}`.
    pub(crate) fn copy_followed_views(
        &mut self,
        view_changes_by_session_id: &HashMap<SessionId, SessionViewChanges>,
    ) -> HashSet<ClientId> {
        let mut copied_client_ids = HashSet::new();
        let mut followed_pane_ids_by_leader_client_id: HashMap<ClientId, Vec<PaneId>> =
            HashMap::new();
        for &session_id in view_changes_by_session_id.keys() {
            for (follower_client_id, leader_client_id) in
                self.list_follow_pairs_leader_first(session_id)
            {
                if self
                    .client_ids_awaiting_reconnect
                    .contains(&follower_client_id)
                    || self
                        .client_ids_awaiting_reconnect
                        .contains(&leader_client_id)
                {
                    continue;
                }
                let is_compared = copied_client_ids.contains(&leader_client_id)
                    || self.is_client_frame_changed(leader_client_id, view_changes_by_session_id)
                    || self.is_client_frame_changed(follower_client_id, view_changes_by_session_id);
                if !is_compared {
                    continue;
                }
                let Some(session) = self.session_by_id.get(&session_id) else {
                    continue;
                };
                let followed_pane_ids = followed_pane_ids_by_leader_client_id
                    .entry(leader_client_id)
                    .or_insert_with(|| {
                        session
                            .clients
                            .get_client_by_id(leader_client_id)
                            .map(|leader| {
                                list_followed_pane_ids(session, leader.get_active_tab_id())
                            })
                            .unwrap_or_default()
                    });
                if self.copy_leader_view(
                    session_id,
                    follower_client_id,
                    leader_client_id,
                    followed_pane_ids,
                ) {
                    copied_client_ids.insert(follower_client_id);
                }
            }
        }
        copied_client_ids
    }

    /// Each `(follower, leader)` pair of the session `session_id`, ordered
    /// by the length of the follower's follow chain ([`list_follow_chain`]),
    /// then by follower id. Empty for a session this server does not hold.
    fn list_follow_pairs_leader_first(&self, session_id: SessionId) -> Vec<(ClientId, ClientId)> {
        let Some(session) = self.session_by_id.get(&session_id) else {
            return Vec::new();
        };
        let mut ranked_follow_pairs: Vec<(usize, ClientId, ClientId)> = session
            .clients
            .list_attached_clients()
            .filter_map(|follower| {
                let leader_client_id = follower.get_followed_client_id()?;
                let follower_client_id = follower.get_client_id();
                let follow_chain_length = list_follow_chain(session, follower_client_id).count();
                Some((follow_chain_length, follower_client_id, leader_client_id))
            })
            .collect();
        ranked_follow_pairs.sort_unstable();
        ranked_follow_pairs
            .into_iter()
            .map(|(_, follower_client_id, leader_client_id)| (follower_client_id, leader_client_id))
            .collect()
    }

    /// Make `follower_client_id`'s view equal to the view it takes when it
    /// follows `leader_client_id` ([`Client::build_view_to_follow`]), with
    /// the scroll offsets of the panes of `scroll_pane_ids`. Returns `false`,
    /// changing nothing, when the follower's view equals that view already
    /// ([`Client::is_view_followed`]) or the session does not hold both
    /// clients.
    ///
    /// The follower's view takes that view ([`Client::apply_followed_view`]),
    /// and a follower on another tab then switches to the leader's tab
    /// ([`tab_ops::focus_tab`]). The events, in order:
    ///
    /// 1. the [`Event::TabFocused`] of a tab switch, with the events of its
    ///    solves;
    /// 2. [`Event::LayoutChanged`] when the follower stays on its tab and its
    ///    solve of that tab changed;
    /// 3. one [`Event::FloatingPaneLayoutChanged`] per floating pane whose
    ///    solve for the follower changed;
    /// 4. one [`Event::FloatingPaneMinimizedChanged`] per floating pane the
    ///    follower stops showing, then one per floating pane it starts
    ///    showing;
    /// 5. the [`Event::PaneFocused`] of a move of the follower's input
    ///    ([`build_pane_focused_event`]);
    /// 6. one [`Event::PtyResized`] per resized PTY: both tabs of a tab
    ///    switch reflow, else the leader's tab when its focus history or zoom
    ///    changed ([`Self::reflow_tab_if_viewed`]), and the floating panes
    ///    when the floating view changed ([`Self::reflow_floating_panes`]).
    ///
    /// A copy of scroll offsets alone emits no event and solves nothing.
    fn copy_leader_view(
        &mut self,
        session_id: SessionId,
        follower_client_id: ClientId,
        leader_client_id: ClientId,
        scroll_pane_ids: &[PaneId],
    ) -> bool {
        let Some(session) = self.session_by_id.get_mut(&session_id) else {
            return false;
        };
        let (Some(leader), Some(follower)) = (
            session.clients.get_client_by_id(leader_client_id),
            session.clients.get_client_by_id(follower_client_id),
        ) else {
            return false;
        };
        if follower.is_view_followed(leader, scroll_pane_ids) {
            return false;
        }
        let leader_tab_id = leader.get_active_tab_id();
        let view_to_follow = follower.build_view_to_follow(leader, scroll_pane_ids);
        let follower_view = follower.build_followed_view(scroll_pane_ids);
        let previous_tab_id = follower_view.active_tab_id;
        let is_tab_switched = previous_tab_id != leader_tab_id;
        let previous_input_pane_id = follower.get_active_focused_pane_id();
        let is_tab_view_changed = follower.list_focus_history(leader_tab_id)
            != view_to_follow.focus_history.as_slice()
            || follower.get_zoomed_pane_id(leader_tab_id) != view_to_follow.zoomed_pane_id;
        let previous_tab_solve: Option<LayoutSolve> = if is_tab_view_changed && !is_tab_switched {
            session
                .compute_client_solve(follower, leader_tab_id)
                .map(Cow::into_owned)
        } else {
            None
        };
        let previous_floating_pane_solves: Vec<(FloatingPaneId, Option<LayoutSolve>)> =
            view_to_follow
                .floating_pane_focus_by_floating_pane_id
                .iter()
                .filter(|(floating_pane_id, floating_pane_focus)| {
                    follower_view
                        .floating_pane_focus_by_floating_pane_id
                        .get(floating_pane_id)
                        != Some(floating_pane_focus)
                })
                .map(|(&floating_pane_id, _)| {
                    let previous_solve = session
                        .compute_client_floating_pane_solve(follower, floating_pane_id)
                        .cloned();
                    (floating_pane_id, previous_solve)
                })
                .collect();
        let is_floating_view_changed = follower_view.shown_floating_pane_ids
            != view_to_follow.shown_floating_pane_ids
            || !previous_floating_pane_solves.is_empty();

        let floating_set = &session.floating_set;
        let Some(follower) = session.clients.get_client_mut_by_id(follower_client_id) else {
            return false;
        };
        follower.apply_followed_view(&view_to_follow, floating_set);
        let mut emitted_events = Vec::new();
        if is_tab_switched {
            emitted_events.extend(tab_ops::focus_tab(
                session,
                follower_client_id,
                tab_ops::TabTarget::Id(leader_tab_id),
            ));
        }
        let Some(follower) = session.clients.get_client_by_id(follower_client_id) else {
            return false;
        };
        if let Some(previous_tab_solve) = previous_tab_solve {
            if session
                .compute_client_solve(follower, leader_tab_id)
                .as_deref()
                != Some(&previous_tab_solve)
            {
                emitted_events.push(Event::LayoutChanged(LayoutChanged {
                    tab_id: leader_tab_id,
                }));
            }
        }
        for (floating_pane_id, previous_solve) in previous_floating_pane_solves {
            let is_solve_changed = session
                .compute_client_floating_pane_solve(follower, floating_pane_id)
                != previous_solve.as_ref();
            if let Some((_, floating_pane)) = session
                .floating_set
                .find_floating_pane_by_id(floating_pane_id)
                .filter(|_| is_solve_changed)
            {
                emitted_events.push(Event::FloatingPaneLayoutChanged(
                    FloatingPaneLayoutChanged {
                        pane_ids: floating_pane.list_pane_ids(),
                    },
                ));
            }
        }
        let hidden_floating_pane_ids: Vec<FloatingPaneId> = follower_view
            .shown_floating_pane_ids
            .iter()
            .copied()
            .filter(|floating_pane_id| {
                !view_to_follow
                    .shown_floating_pane_ids
                    .contains(floating_pane_id)
            })
            .collect();
        let revealed_floating_pane_ids: Vec<FloatingPaneId> = view_to_follow
            .shown_floating_pane_ids
            .iter()
            .copied()
            .filter(|floating_pane_id| {
                !follower_view
                    .shown_floating_pane_ids
                    .contains(floating_pane_id)
            })
            .collect();
        emitted_events.extend(build_floating_pane_minimized_changed_events(
            follower_client_id,
            &session.floating_set,
            &hidden_floating_pane_ids,
            true,
        ));
        emitted_events.extend(build_floating_pane_minimized_changed_events(
            follower_client_id,
            &session.floating_set,
            &revealed_floating_pane_ids,
            false,
        ));
        emitted_events.extend(build_pane_focused_event(follower, previous_input_pane_id));

        let pty_backend = Arc::clone(self.get_pty_backend());
        if is_tab_switched {
            for tab_id in [leader_tab_id, previous_tab_id] {
                self.reflow_tab_if_viewed(
                    pty_backend.as_ref(),
                    session_id,
                    tab_id,
                    &mut emitted_events,
                );
            }
        } else if is_tab_view_changed {
            self.reflow_tab_if_viewed(
                pty_backend.as_ref(),
                session_id,
                leader_tab_id,
                &mut emitted_events,
            );
        }
        if is_floating_view_changed {
            self.reflow_floating_panes(pty_backend.as_ref(), session_id, &mut emitted_events);
        }
        self.publish_events(&emitted_events);
        true
    }

    /// The copied view ([`FollowedView`]) of each client that
    /// `runtime_event` acts for and that follows a client, read before the
    /// event runs ([`list_acting_client_ids`]), with the scroll offsets of
    /// the panes of that client's active tab and of every floating pane.
    /// Empty when no such client follows anyone.
    pub(crate) fn list_acting_follower_views(
        &self,
        runtime_event: &RuntimeEvent,
    ) -> Vec<ActingFollowerView> {
        list_acting_client_ids(runtime_event)
            .into_iter()
            .flatten()
            .filter_map(|client_id| {
                let session = self.get_session_for_client(client_id)?;
                let client = session.clients.get_client_by_id(client_id)?;
                client.get_followed_client_id()?;
                let scroll_pane_ids = list_followed_pane_ids(session, client.get_active_tab_id());
                let followed_view = client.build_followed_view(&scroll_pane_ids);
                Some(ActingFollowerView {
                    follower_client_id: client_id,
                    scroll_pane_ids,
                    followed_view,
                })
            })
            .collect()
    }

    /// End the follow of each client of `acting_follower_views` whose copied
    /// view, read again now over the same panes, differs from the view read
    /// before the event ([`Self::list_acting_follower_views`]). A client that
    /// left the session, or that follows nobody, changes nothing.
    pub(crate) fn clear_follows_of_changed_views(
        &mut self,
        acting_follower_views: Vec<ActingFollowerView>,
    ) {
        for acting_follower_view in acting_follower_views {
            let Some(client) = self.get_client_mut(acting_follower_view.follower_client_id) else {
                continue;
            };
            if client.build_followed_view(&acting_follower_view.scroll_pane_ids)
                != acting_follower_view.followed_view
            {
                client.set_followed_client_id(None);
            }
        }
    }

    /// End the follow of `client_id`. A client that follows nobody, or that
    /// no session holds, changes nothing and is not marked changed.
    pub(crate) fn clear_followed_client_id(&mut self, client_id: ClientId) {
        let is_following = self
            .get_session_for_client(client_id)
            .and_then(|session| session.clients.get_client_by_id(client_id))
            .is_some_and(|client| client.get_followed_client_id().is_some());
        if !is_following {
            return;
        }
        if let Some(client) = self.get_client_mut(client_id) {
            client.set_followed_client_id(None);
        }
    }
}

#[cfg(test)]
mod tests;
