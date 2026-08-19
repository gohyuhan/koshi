//! Client lifecycle (attach, resize, detach) and client-mode command
//! handlers (lock mode, mouse select).

use super::*;

use std::time::Instant;

use koshi_ipc::protocol::ConnectionToken;

use crate::runtime::attach::session_structure;
use crate::runtime::bus::EventFilter;
use crate::runtime::event::AttachAccepted;
use crate::runtime::saved_view::SavedView;

impl Server {
    /// Serve one attach arriving over the control socket, in this single
    /// dispatcher turn: settle which client this is, register it on the tab it
    /// views, publish what the attach emitted, subscribe it to the events
    /// `filter` selects, and read the session's structure back.
    ///
    /// `resume` names the client record a caller asks to come back as, after
    /// the session replaced its own process image. The record is handed back
    /// when the session still holds it, the tab it was viewing still exists,
    /// and no connection is streaming for it: the arriving viewport replaces
    /// the record's, and its per-tab focus, zoom, scrollback offsets,
    /// selections, lock mode, label and colour all stay. That is the whole of
    /// the `resume` path — no `resume`, an id the session does not hold, an id
    /// whose tab is gone, an id a connection is already streaming for all mint
    /// a fresh client instead, so an attach never fails over `resume`.
    ///
    /// `resume_token` names the view a caller asks to have back, filed when
    /// that caller's last client detached. It is read on the fresh-client path
    /// alone: a `resume` claim that succeeds keeps the record it took, and the
    /// token's view is dropped. A view that comes back puts the client on the
    /// tab it names — the session's first tab when that tab is gone — and then
    /// restores the focused pane of each tab, the zoomed pane of each tab and
    /// the scroll offset of each pane — cut down to the lines that pane still
    /// retains — onto the freshly minted client. The token
    /// is consumed by this one attach: presenting it again restores nothing. A
    /// token the session holds no view under, and a token older than 120
    /// seconds, attach with a fresh view instead of failing. An attach that
    /// returns `None` reads no token and spends none: that view stands until
    /// its 120 seconds run out.
    ///
    /// Every accepted attach mints a new token, carried back on
    /// [`AttachAccepted::resume_token`].
    ///
    /// Registration and subscription land in the same turn, so the structure
    /// returned here and the queue's first event describe one continuous
    /// state: no change can slip between them. `None` when no session is
    /// running, or when the one running holds no tab to view — neither is
    /// something a client can attach to. `attached_at` is supplied by the
    /// caller; the handler never reads the clock itself.
    pub(crate) fn handle_ipc_attach(
        &mut self,
        resume: Option<ClientId>,
        resume_token: Option<ConnectionToken>,
        viewport: Size,
        filter: EventFilter,
        attached_at: SystemTime,
        remote: bool,
    ) -> Option<AttachAccepted> {
        // One process serves one session: genesis seeds exactly one and no
        // command creates another in-process.
        let session = self.sessions.values().next()?;
        let session_id = session.id;
        let first_tab = session.tabs.values().min_by_key(|tab| tab.index())?.id();

        let claimed = resume.and_then(|claimed_id| {
            let client = session.clients.get(claimed_id)?;
            let client_tab = client.active_tab();
            let streaming = self
                .subscriptions
                .iter()
                .any(|&(_, viewed)| viewed == claimed_id);
            (session.tabs.contains_key(&client_tab) && !streaming)
                .then_some((claimed_id, client_tab))
        });
        let saved = resume_token
            .and_then(|token| self.saved_views.take(&token, attached_at))
            .filter(|_| claimed.is_none());
        let (client_id, active_tab) = match claimed {
            Some(claim) => claim,
            None => {
                let session = self
                    .sessions
                    .get(&session_id)
                    .expect("session located above");
                let tab = saved
                    .as_ref()
                    .map(|view| view.active_tab)
                    .filter(|tab| session.tabs.contains_key(tab))
                    .unwrap_or(first_tab);
                (ClientId::new(), tab)
            }
        };
        self.awaiting_reconnect.remove(&client_id);

        let mut emitted = self.handle_client_attach(
            session_id,
            client_id,
            viewport,
            active_tab,
            attached_at,
            remote,
        );
        if let Some(view) = saved {
            emitted.extend(self.restore_saved_view(session_id, client_id, active_tab, &view));
        }
        self.publish_events(&emitted);

        let events = self.subscribe(client_id, filter);
        let resume_token = self.saved_views.mint(client_id);
        let session = self
            .sessions
            .get(&session_id)
            .expect("session located above");
        Some(AttachAccepted {
            client_id,
            session_id,
            structure: session_structure(session),
            events,
            ending_notice: Arc::clone(self.event_bus.ending_notice()),
            resume_token,
        })
    }

    /// Put `view` back on `client_id` in `session_id`, then reconcile the PTY
    /// sizes of `active_tab` and schedule a redraw.
    ///
    /// Applies the focused pane of each tab first, then the zoomed pane of each
    /// tab, then the scroll offset of each pane.
    /// [`Client::update_focused_pane`] moves an existing zoom onto the pane it
    /// focuses; the zoom pass runs after it, and leaves each tab zoomed on the
    /// pane `view` names.
    ///
    /// An entry naming a tab the session no longer holds, or a pane the session
    /// no longer holds, is dropped instead of applied: that tab keeps no focus
    /// and stays tiled, and that pane sits at the live bottom. A scroll offset
    /// past the lines its pane still retains is cut down to that count — an
    /// offset of 500 onto a pane retaining 120 lines restores 120, and onto a
    /// pane whose scrollback was erased restores 0, which sits at the live
    /// bottom and holds the view no longer. A restored zoom
    /// changes the size the tab's panes solve to, so the tab reflows and one
    /// [`Event::PtyResized`] is returned for each pane whose PTY size changed.
    /// Returns no event when the session or the client is gone.
    fn restore_saved_view(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        active_tab: TabId,
        view: &SavedView,
    ) -> Vec<Event> {
        // Clone the shared backend before borrowing the session: the reflow then
        // needs no `&self` across the mutation.
        let backend = Arc::clone(self.pty_backend());
        let mut events = Vec::new();

        {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return events;
            };
            let tabs = &session.tabs;
            let panes = &session.panes;
            let Some(client) = session.clients.get_mut(client_id) else {
                return events;
            };
            for (&tab_id, &pane_id) in &view.focus_by_tab {
                if tabs.contains_key(&tab_id) && panes.get(pane_id).is_some() {
                    client.update_focused_pane(tab_id, pane_id);
                }
            }
            for (&tab_id, &pane_id) in &view.zoom_by_tab {
                if tabs.contains_key(&tab_id) && panes.get(pane_id).is_some() {
                    client.zoom_pane(tab_id, pane_id);
                }
            }
            for (&pane_id, &offset) in &view.scroll_by_pane {
                if panes.get(pane_id).is_some() {
                    let retained = self
                        .terminal_engines
                        .get(&pane_id)
                        .map_or(0, |engine| engine.state().scrollback().len());
                    client.set_scroll_offset(pane_id, offset.min(retained));
                }
            }
        }

        self.reflow_tab_if_viewed(backend.as_ref(), session_id, active_tab, &mut events);
        self.render_scheduler
            .invalidate(InvalidationReason::LayoutChanged);

        events
    }

    /// Attach a client to `session_id` viewing `active_tab`, then reconcile the
    /// affected tabs' PTY sizes and schedule a redraw.
    ///
    /// A client lives in exactly one session. If this id already lives in another
    /// session it is detached there first — reflowing the tab it leaves — so it
    /// is never recorded twice. Within the target session an id that is already
    /// attached is a re-attach: its view updates in place, keeping its per-tab
    /// focus, scrollback offsets, and lock mode, and the tab it moves off of
    /// reflows too. A fresh id is registered anew with a generated
    /// `C-<adjective>-<noun>` label that no client in the session already
    /// holds, and the lowest palette index no attached client is painted in.
    ///
    /// `remote` names where the connection carrying this attach came from. It
    /// is recorded as the client's [`ClientOrigin`]: [`ClientOrigin::Remote`]
    /// when true, [`ClientOrigin::Local`] otherwise. A re-attach overwrites the
    /// origin the client already carried.
    ///
    /// The viewer joins each affected tab's effective size
    /// ([`Session::tab_viewport`], the per-axis minimum across every client
    /// viewing it), so a smaller client shrinks a tab and a departing one lets it
    /// grow: the tab's live panes reflow to the new size, one
    /// [`Event::PtyResized`] each. A tab no client views has no viewport and
    /// keeps its sizes. The attach always invalidates
    /// [`InvalidationReason::LayoutChanged`] so every client repaints from the
    /// reconciled snapshot. An attach naming an unknown session, or a tab the
    /// session does not hold, is dropped. `attached_at` is supplied by the
    /// producer; the handler never reads the clock itself.
    pub fn handle_client_attach(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        viewport: Size,
        active_tab: TabId,
        attached_at: SystemTime,
        remote: bool,
    ) -> Vec<Event> {
        let origin = if remote {
            ClientOrigin::Remote
        } else {
            ClientOrigin::Local
        };
        // Clone the shared backend before borrowing the session: the reflow then
        // needs no `&self` across the mutation.
        let backend = Arc::clone(self.pty_backend());
        let mut events = Vec::new();

        // Validate the target: an attach naming an unknown session, or a tab the
        // session does not hold, is dropped so no client views a tab the renderer
        // cannot solve.
        match self.sessions.get(&session_id) {
            Some(session) if session.tabs.contains_key(&active_tab) => {}
            _ => return Vec::new(),
        }

        // If the id already lives in a different session, detach it there first —
        // reflowing the tab it leaves — so it is never held in two registries.
        if let Some(old_session_id) = self.session_for_client(client_id).map(|session| session.id) {
            if old_session_id != session_id {
                let old_session = self
                    .sessions
                    .get_mut(&old_session_id)
                    .expect("session located above");
                let old_tab = old_session
                    .detach_client(client_id)
                    .map(|client| client.active_tab());
                if let Some(old_tab) = old_tab {
                    self.reflow_tab_if_viewed(
                        backend.as_ref(),
                        old_session_id,
                        old_tab,
                        &mut events,
                    );
                }
            }
        }

        let session = self
            .sessions
            .get_mut(&session_id)
            .expect("target session validated above");

        // A same-session re-attach updates the view in place, preserving the
        // client's accumulated state and yielding the tab it moved off of; a
        // fresh id is registered anew and has no prior tab.
        let prior_tab = if let Some(client) = session.clients.get_mut(client_id) {
            let prior = client.active_tab();
            client.update_viewport(viewport);
            client.update_active_tab(active_tab);
            client.update_origin(origin);
            Some(prior)
        } else {
            let label = generate_name(NameKind::Client, |candidate| {
                session
                    .clients
                    .list_attached()
                    .any(|client| client.label() == candidate)
            });
            let colour = (0..=u8::MAX)
                .find(|candidate| {
                    !session
                        .clients
                        .list_attached()
                        .any(|client| client.colour() == *candidate)
                })
                // Every palette index is in use, so this client shares one.
                .unwrap_or(0);
            session.attach_client(Client::new(
                client_id,
                session_id,
                attached_at,
                viewport,
                active_tab,
                origin,
                label,
                colour,
            ));
            None
        };

        // A client with no focus in the tab it now views starts on that tab's
        // most recent pane, which a session records when the tab is created. A
        // client that already focused a pane here keeps it. Without this a
        // client attaching to a session created with nothing attached has no
        // focused pane, and every key it types reaches no shell.
        let landed_on = session
            .tabs
            .get(&active_tab)
            .and_then(|tab| tab.focus_mru().first().copied());
        if let (Some(pane_id), Some(client)) = (landed_on, session.clients.get_mut(client_id)) {
            if client.focused_pane(active_tab).is_none() {
                client.update_focused_pane(active_tab, pane_id);
                events.push(Event::PaneFocused(PaneFocused {
                    client_id,
                    tab_id: active_tab,
                    pane_id,
                    prior_pane: None,
                }));
            }
        }

        // Reflow the tab the client now views, plus — on a same-session move —
        // the one it left.
        self.reflow_tab_if_viewed(backend.as_ref(), session_id, active_tab, &mut events);
        if let Some(prior) = prior_tab {
            if prior != active_tab {
                self.reflow_tab_if_viewed(backend.as_ref(), session_id, prior, &mut events);
            }
        }

        self.render_scheduler
            .invalidate(InvalidationReason::LayoutChanged);

        events
    }

    /// Update one client's full terminal viewport, reconcile the active tab's
    /// pane region and PTYs, then schedule a frame for the new terminal size.
    pub fn handle_client_resize(&mut self, client_id: ClientId, viewport: Size) -> Vec<Event> {
        let backend = Arc::clone(self.pty_backend());
        let Some(session_id) = self.session_for_client(client_id).map(|session| session.id) else {
            return Vec::new();
        };
        let session = self
            .sessions
            .get_mut(&session_id)
            .expect("session located above");
        let Some(client) = session.clients.get_mut(client_id) else {
            return Vec::new();
        };
        let active_tab = client.active_tab();
        client.update_viewport(viewport);

        let mut events = Vec::new();
        self.reflow_tab_if_viewed(backend.as_ref(), session_id, active_tab, &mut events);
        self.render_scheduler
            .invalidate(InvalidationReason::TerminalResize);
        events
    }

    /// File the view `client_id` is leaving behind, under the token that
    /// client's attach minted: the tab it is on, the pane it has focused in
    /// each tab, the pane it has zoomed in each tab, and how far it has
    /// scrolled up each pane. The record stands for 120 seconds from
    /// `detached_at`, and presenting the minted token within that window hands
    /// the view back once.
    ///
    /// `detached_at` is when the producer saw the connection end, supplied by
    /// the producer; the handler never reads the clock itself.
    ///
    /// Files nothing in three cases. A client that is still awaiting reconnect
    /// files nothing and keeps whatever hash stands against its id; a process
    /// that came back from a restart starts with an empty store, so a client
    /// awaiting reconnect there has no hash to keep. A client no session holds
    /// files nothing and its minted hash is dropped, so the token it was handed
    /// takes back nothing. A client whose attach minted no token files nothing.
    ///
    /// Call this before [`handle_client_detach`](Self::handle_client_detach),
    /// which removes the record read here.
    pub(crate) fn save_view_of(&mut self, client_id: ClientId, detached_at: SystemTime) {
        if self.awaiting_reconnect.contains(&client_id) {
            return;
        }
        let Some(session_id) = self.session_for_client(client_id).map(|session| session.id) else {
            self.saved_views.forget(client_id);
            return;
        };
        let Some(client) = self
            .sessions
            .get(&session_id)
            .and_then(|session| session.clients.get(client_id))
        else {
            self.saved_views.forget(client_id);
            return;
        };
        self.saved_views.save(client, detached_at);
    }

    /// Detach the client `client_id`, then reconcile the PTY sizes of the tab it
    /// was viewing and schedule a redraw.
    ///
    /// Removing the client hands back its record, whose `active_tab` names the
    /// tab whose viewer set shrank. The departing viewer is dropped from that
    /// tab's effective size, so if larger viewers remain the tab grows back: its
    /// live panes reflow to the new [`Session::tab_viewport`], one
    /// [`Event::PtyResized`] each. When it was the last viewer the tab has no
    /// viewport and keeps its sizes. The detach always invalidates
    /// [`InvalidationReason::LayoutChanged`] so the remaining clients repaint. A
    /// detach for a client this runtime does not hold is dropped.
    ///
    /// Every subscription registered as viewing this client is dropped with the
    /// record, closing the sending end of each one's queue.
    ///
    /// Runs for every detach trigger: a connection drop (either half of an
    /// attached client's connection ending), the [`Command::Detach`] /
    /// [`Command::DetachAll`] execution arms, and a [`Command::Quit`] whose
    /// source names a client. Target resolution happens at command resolution
    /// before this is reached. With `auto-close-session` on, a detach that
    /// leaves the session with no client requests a graceful quit.
    ///
    /// A detach for a client that is still awaiting reconnect is dropped: that
    /// record's fate belongs to the grace window, which detaches it through
    /// `handle_drop_unclaimed_clients` after removing it from the set.
    pub fn handle_client_detach(&mut self, client_id: ClientId) -> Vec<Event> {
        if self.awaiting_reconnect.contains(&client_id) {
            return Vec::new();
        }

        // Clone the shared backend before borrowing the session: the reflow then
        // needs no `&self` across the mutation.
        let backend = Arc::clone(self.pty_backend());

        // Find the session holding the client, then take it by key so the reflow
        // keeps its disjoint field borrows. A detach for a client already gone is
        // dropped.
        let Some(session_id) = self.session_for_client(client_id).map(|session| session.id) else {
            return Vec::new();
        };
        let session = self
            .sessions
            .get_mut(&session_id)
            .expect("session located above");

        // Removing the client returns its record; its `active_tab` is the tab
        // whose effective size may now grow.
        let removed = session.detach_client(client_id);
        let active_tab = removed.as_ref().map(|client| client.active_tab());
        // Read while the session is still borrowed: a client did leave, and none
        // is left attached.
        let session_emptied = removed.is_some() && session.clients.is_empty();
        self.unsubscribe_client(client_id);

        let mut events = Vec::new();
        // Reflow the tab the client left, if any other client still views it; a
        // tab whose last viewer just left has no viewport and keeps its sizes.
        if let Some(active_tab) = active_tab {
            self.reflow_tab_if_viewed(backend.as_ref(), session_id, active_tab, &mut events);
        }

        self.render_scheduler
            .invalidate(InvalidationReason::LayoutChanged);

        // `auto-close-session` ends the session when its last client leaves.
        // Each pane's child is asked to stop and given the graceful window
        // before it is killed; a stop request that cannot be delivered goes
        // straight to the kill.
        if session_emptied && self.config.auto_close_session {
            self.request_graceful_quit();
        }

        events
    }

    /// Detach every client whose record came across an image swap and has not
    /// attached again by `deadline`.
    ///
    /// Each one goes through
    /// [`handle_client_detach`](Self::handle_client_detach), so its tab reflows
    /// and `auto-close-session` still ends a session left with no client. A
    /// client that attached again already left the set, so the usual case
    /// detaches nobody and emits nothing. The detaches run in client-id order,
    /// so the events they emit arrive in one settled order.
    ///
    /// `deadline` is when the grace window closed, supplied by the producer;
    /// the handler never reads the clock to decide anything.
    pub(crate) fn handle_drop_unclaimed_clients(&mut self, deadline: Instant) -> Vec<Event> {
        if self.awaiting_reconnect.is_empty() {
            return Vec::new();
        }
        let mut unclaimed: Vec<ClientId> = std::mem::take(&mut self.awaiting_reconnect)
            .into_iter()
            .collect();
        unclaimed.sort();
        tracing::info!(
            unclaimed = unclaimed.len(),
            waited_ms = Instant::now()
                .saturating_duration_since(deadline)
                .as_millis(),
            "detaching the clients that did not attach again after the restart"
        );
        let mut events = Vec::new();
        for client_id in unclaimed {
            events.extend(self.handle_client_detach(client_id));
        }
        events
    }

    /// Handle [`Command::ToggleLockMode`]: flip the target client between
    /// pass-through [`LockMode::Locked`] and [`LockMode::Normal`].
    ///
    /// A client already locked unlocks; a client in any other mode locks. The
    /// toggle always changes the mode, so it always emits.
    pub(super) fn handle_toggle_lock_mode(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &ToggleLockModeArgs,
    ) -> Result<CommandResult, Rejection> {
        self.set_lock_mode(command_id, source, args.client, |current| match current {
            LockMode::Locked => LockMode::Normal,
            _ => LockMode::Locked,
        })
    }

    /// Handle [`Command::SetLockMode`]: set the target client to
    /// [`LockMode::Locked`] when `args.locked`, else [`LockMode::Normal`].
    ///
    /// Setting the mode the client already holds is a no-op: applied, zero
    /// events.
    pub(super) fn handle_set_lock_mode(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &LockModeArgs,
    ) -> Result<CommandResult, Rejection> {
        let next = if args.locked {
            LockMode::Locked
        } else {
            LockMode::Normal
        };
        self.set_lock_mode(command_id, source, args.client, move |_| next)
    }

    /// Set the target client's [`LockMode`], emitting [`Event::InputModeChanged`]
    /// only when it changes. `resolve` maps the client's current mode to the
    /// next one, so the toggle and the explicit set share one path.
    ///
    /// Lock mode targets a client alone — the explicit `client` argument when
    /// set, else the acting client — no pane is resolved, so a client with no
    /// focused pane still locks. Nothing in the layout, focus, or any PTY
    /// changes; a no-op change mutates nothing.
    fn set_lock_mode(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        explicit: Option<ClientId>,
        resolve: impl FnOnce(LockMode) -> LockMode,
    ) -> Result<CommandResult, Rejection> {
        let (client_id, client) = self.acting_client_mut(source, explicit)?;

        let current = client.lock_mode();
        let next = resolve(current);
        let mut scope = TransactionScope::new();
        if next != current {
            client.update_lock_mode(next);
            scope.emit(Event::InputModeChanged(InputModeChanged {
                client_id,
                mode: Self::input_mode(next),
            }));
        }
        Ok(scope.commit(command_id, &mut self.event_bus))
    }

    /// The target client's mutable record, for commands that act on one
    /// client alone (the lock and mouse-select commands). The client is the
    /// one [`Self::resolve_view_client`] picks — the explicit target when
    /// given, else the acting client — so the record mutated here is the same
    /// one [`Self::validate`] admitted the command against.
    fn acting_client_mut(
        &mut self,
        source: &CommandSource,
        explicit: Option<ClientId>,
    ) -> Result<(ClientId, &mut Client), Rejection> {
        let acting = Self::require_session(self.acting_session(source)?)?;
        let session_id = acting.id;
        let client_id = Self::resolve_view_client(explicit, source, acting)?;
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;
        let client = session
            .clients
            .get_mut(client_id)
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
        Ok((client_id, client))
    }

    /// Handle [`Command::ToggleMouseSelect`]: flip whether the acting client
    /// grabs the mouse for text selection and emit [`Event::MouseSelectChanged`]
    /// carrying the new value.
    ///
    /// Client-scoped like the lock commands: the target is the acting client
    /// alone, no pane is resolved. It changes only how the client's mouse
    /// gestures route — koshi selection versus the program — never the layout,
    /// focus, or any PTY. The event is what carries the new value to the
    /// viewer, which routes its own mouse events against its own copy.
    pub(super) fn handle_toggle_mouse_select(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
    ) -> Result<CommandResult, Rejection> {
        let (client_id, client) = self.acting_client_mut(source, None)?;
        let on = client.toggle_mouse_select();
        let mut scope = TransactionScope::new();
        scope.emit(Event::MouseSelectChanged(MouseSelectChanged {
            client_id,
            on,
        }));
        Ok(scope.commit(command_id, &mut self.event_bus))
    }

    /// Map a [`LockMode`] to the wire-facing [`InputMode`] carried on
    /// [`Event::InputModeChanged`]. The lock commands only ever produce
    /// [`LockMode::Normal`] or [`LockMode::Locked`]; the modal layers report as
    /// [`InputMode::Normal`].
    fn input_mode(mode: LockMode) -> InputMode {
        match mode {
            LockMode::Locked => InputMode::Locked,
            _ => InputMode::Normal,
        }
    }
}
