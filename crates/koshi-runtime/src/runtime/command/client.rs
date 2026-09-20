//! Client lifecycle (attach, resize, detach) and client-mode command
//! handlers (lock mode, mouse select).

use super::*;

use std::time::Instant;

use koshi_ipc::protocol::ConnectionToken;

use crate::runtime::attach::build_session_structure_snapshot;
use crate::runtime::bus::EventFilter;
use crate::runtime::event::AttachAccepted;
use crate::runtime::saved_view::SavedView;

impl Server {
    pub(crate) fn handle_client_cell_size(
        &mut self,
        client_id: ClientId,
        cell_size: koshi_core::geometry::PixelCellSize,
    ) {
        let Some(session_id) = self
            .get_session_for_client(client_id)
            .map(|session| session.session_id)
        else {
            return;
        };
        let Some(session) = self.session_by_id.get_mut(&session_id) else {
            return;
        };
        let Some(client) = session.clients.get_client_by_id(client_id) else {
            return;
        };
        let active_tab_id = client.get_active_tab();
        let cell_size_changed = client.get_cell_size() != Some(cell_size);
        let affected_client_ids =
            list_clients_affected_by_tabs(session, &[active_tab_id], Some(client_id));
        if let Some(client) = session.clients.get_client_mut_by_id(client_id) {
            client.update_cell_size(cell_size);
        }
        if cell_size_changed {
            advance_session_placement_revision_when_possible(session);
            advance_client_placement_revisions_when_possible(session, &affected_client_ids);
        }
        let pty_backend = Arc::clone(self.get_pty_backend());
        let mut emitted_events = Vec::new();
        self.reflow_tab_if_viewed(
            pty_backend.as_ref(),
            session_id,
            active_tab_id,
            &mut emitted_events,
        );
        self.render_scheduler.invalidate();
        self.publish_events(&emitted_events);
    }

    /// Serve one attach arriving over the control socket, in this single
    /// dispatcher turn: settle which client this is, register it on the tab it
    /// views, publish what the attach emitted, subscribe it to the events
    /// `event_filter` selects, and read the session's structure back.
    ///
    /// `resume_client_id` names the client record a caller asks to come back as, after
    /// the session replaced its own process image. The record is handed back
    /// when the session still holds it, the tab it was viewing still exists,
    /// and no connection is streaming for it: the arriving viewport, pane area
    /// and origin replace the record's, and its per-tab focus, zoom, scrollback
    /// offsets, selections, lock mode, label and colour all stay. That is the
    /// whole of the `resume_client_id` path — no `resume_client_id`, an id the session does not
    /// hold, an id whose tab is gone, an id a connection is already streaming
    /// for all mint a fresh client instead, so an attach never fails over
    /// `resume_client_id`.
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
    /// `pane_area` is recorded on the client record, `None` included, and
    /// handed back on [`AttachAccepted::pane_area`].
    ///
    /// Registration and subscription land in the same turn, so the structure
    /// returned here and the queue's first event describe one continuous
    /// state: no change can slip between them. `None` when no session is
    /// running, or when the one running holds no tab to view — neither is
    /// something a client can attach to. `attached_at` is supplied by the
    /// caller; the handler never reads the clock itself.
    // Carries the whole of one attach request: what it claims back (`resume_client_id`,
    // `resume_token`), the view it arrives with (`viewport_size`, `pane_area`), and
    // how the connection is served (`event_filter`, `attached_at`, `is_remote`).
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn handle_ipc_attach(
        &mut self,
        resume_client_id: Option<ClientId>,
        resume_token: Option<ConnectionToken>,
        viewport_size: Size,
        pane_area: Option<PaneArea>,
        event_filter: EventFilter,
        attached_at: SystemTime,
        is_remote: bool,
    ) -> Option<AttachAccepted> {
        self.handle_ipc_attach_with_cell_size(
            resume_client_id,
            resume_token,
            viewport_size,
            pane_area,
            None,
            event_filter,
            attached_at,
            is_remote,
        )
    }

    /// Serve one attach with the terminal's initial cell measurement.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn handle_ipc_attach_with_cell_size(
        &mut self,
        resume_client_id: Option<ClientId>,
        resume_token: Option<ConnectionToken>,
        viewport_size: Size,
        pane_area: Option<PaneArea>,
        cell_size: Option<koshi_core::geometry::PixelCellSize>,
        event_filter: EventFilter,
        attached_at: SystemTime,
        is_remote: bool,
    ) -> Option<AttachAccepted> {
        let session = self.get_sole_session()?;
        let session_id = session.session_id;
        let first_tab_id = session
            .tabs
            .values()
            .min_by_key(|tab| tab.get_tab_index())?
            .get_tab_id();

        let claimed_client_view = resume_client_id.and_then(|claimed_client_id| {
            let client = session.clients.get_client_by_id(claimed_client_id)?;
            let client_active_tab_id = client.get_active_tab();
            let is_streaming = self
                .subscriptions
                .iter()
                .any(|&(_, viewed_client_id)| viewed_client_id == claimed_client_id);
            (session.tabs.contains_key(&client_active_tab_id) && !is_streaming)
                .then_some((claimed_client_id, client_active_tab_id))
        });
        let saved_view = resume_token
            .and_then(|resume_token| {
                self.saved_view_store
                    .take_saved_view(&resume_token, attached_at)
            })
            .filter(|_| claimed_client_view.is_none());
        let (client_id, active_tab_id) = match claimed_client_view {
            Some(claimed_client_view) => claimed_client_view,
            None => {
                let session = self
                    .session_by_id
                    .get(&session_id)
                    .expect("session located above");
                let active_tab_id = saved_view
                    .as_ref()
                    .map(|saved_view| saved_view.active_tab_id)
                    .filter(|tab_id| session.tabs.contains_key(tab_id))
                    .unwrap_or(first_tab_id);
                (ClientId::new(), active_tab_id)
            }
        };
        self.client_ids_awaiting_reconnect.remove(&client_id);

        let mut emitted_events = self.handle_client_attach_with_cell_size(
            session_id,
            client_id,
            viewport_size,
            pane_area,
            active_tab_id,
            cell_size,
            attached_at,
            is_remote,
        );
        if let Some(saved_view) = saved_view {
            emitted_events.extend(self.restore_saved_view(
                session_id,
                client_id,
                active_tab_id,
                &saved_view,
            ));
        }
        self.publish_events(&emitted_events);

        let deliveries = self.subscribe(client_id, event_filter);
        let resume_token = self.saved_view_store.mint_resume_token(client_id);
        let session = self
            .session_by_id
            .get(&session_id)
            .expect("session located above");
        Some(AttachAccepted {
            client_id,
            session_id,
            session_structure: build_session_structure_snapshot(session),
            deliveries,
            ending_notice: Arc::clone(self.event_bus.ending_notice()),
            resume_token,
            pane_area,
        })
    }

    /// Put `saved_view` back on `client_id` in `session_id`, then reconcile the
    /// PTY sizes of `active_tab_id` and schedule a redraw.
    ///
    /// Applies the focused pane of each tab first, then the zoomed pane of each
    /// tab, then the scroll offset of each pane.
    /// [`Client::update_focused_pane`] moves an existing zoom onto the pane it
    /// focuses; the zoom pass runs after it, and leaves each tab zoomed on the
    /// pane `saved_view` names.
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
    ///
    /// A restored focus that moves `active_tab_id`'s focused pane off the one the
    /// attach put it on returns one [`Event::PaneFocused`] naming the restored
    /// pane, so the event stream names the pane the client actually views.
    ///
    /// Returns no event when the session or the client is gone.
    fn restore_saved_view(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        active_tab_id: TabId,
        saved_view: &SavedView,
    ) -> Vec<Event> {
        // Clone the shared backend before borrowing the session: the reflow then
        // needs no `&self` across the mutation.
        let pty_backend = Arc::clone(self.get_pty_backend());
        let mut emitted_events = Vec::new();
        let Some(session) = self.session_by_id.get(&session_id) else {
            return emitted_events;
        };
        if session.clients.get_client_by_id(client_id).is_none() {
            return emitted_events;
        }
        // The restored pane and the one it replaces, set when the focus pass
        // moves `active_tab_id` off the pane the attach focused.
        let mut focus_change = None;

        {
            let session = self
                .session_by_id
                .get_mut(&session_id)
                .expect("session located above");
            let tab_by_id = &session.tabs;
            let pane_registry = &session.panes;
            let Some(client) = session.clients.get_client_mut_by_id(client_id) else {
                return emitted_events;
            };
            let focused_panes_before = client.list_focused_panes().clone();
            let zoomed_panes_before = client.list_zoomed_panes().clone();
            let prior_pane_id = client.get_focused_pane(active_tab_id);
            for (&tab_id, &pane_id) in &saved_view.focused_pane_id_by_tab_id {
                if tab_by_id.contains_key(&tab_id)
                    && pane_registry.get_pane_record_by_id(pane_id).is_some()
                {
                    client.update_focused_pane(tab_id, pane_id);
                }
            }
            if let Some(pane_id) = client
                .get_focused_pane(active_tab_id)
                .filter(|&restored_pane_id| Some(restored_pane_id) != prior_pane_id)
            {
                focus_change = Some((pane_id, prior_pane_id));
            }
            for (&tab_id, &pane_id) in &saved_view.zoomed_pane_id_by_tab_id {
                if tab_by_id.contains_key(&tab_id)
                    && pane_registry.get_pane_record_by_id(pane_id).is_some()
                {
                    client.zoom_pane(tab_id, pane_id);
                }
            }
            for (&pane_id, &scroll_offset) in &saved_view.scroll_offset_by_pane_id {
                if pane_registry.get_pane_record_by_id(pane_id).is_some() {
                    let retained_line_count = self.terminal_engine_by_pane_id.get(&pane_id).map_or(
                        0,
                        |terminal_engine| {
                            terminal_engine
                                .get_terminal_state()
                                .get_scrollback()
                                .get_retained_line_count()
                        },
                    );
                    client.set_scroll_offset(pane_id, scroll_offset.min(retained_line_count));
                }
            }
            let client_view_changed = focused_panes_before != *client.list_focused_panes()
                || zoomed_panes_before != *client.list_zoomed_panes();
            if client_view_changed {
                advance_client_placement_revisions_when_possible(session, &[client_id]);
            }
        }

        self.reflow_tab_if_viewed(
            pty_backend.as_ref(),
            session_id,
            active_tab_id,
            &mut emitted_events,
        );
        if let Some((pane_id, previous_pane_id)) = focus_change {
            emitted_events.push(Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: active_tab_id,
                pane_id,
                previous_pane_id,
            }));
        }
        self.render_scheduler.invalidate();

        emitted_events
    }

    /// Attach a client to `session_id` viewing `active_tab_id`, then reconcile the
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
    /// A fresh id also takes the session's starting lock: a session seeded from
    /// a profile carrying `lock` registers its first client in
    /// [`LockMode::Locked`] and emits [`Event::InputModeChanged`] for it. The
    /// flag is spent by that attach, so every client after it starts in
    /// [`LockMode::Normal`].
    ///
    /// `is_remote` names whether the connection carrying this attach came from another
    /// process. It
    /// is recorded as the client's [`ClientOrigin`]: [`ClientOrigin::Remote`]
    /// when true, [`ClientOrigin::Local`] otherwise. A re-attach overwrites the
    /// origin the client already carried.
    ///
    /// Records `pane_area` on the client, `None` included: a re-attach that
    /// reports none replaces an earlier report. The viewer joins each affected
    /// tab's effective size ([`Session::get_tab_viewport`], the per-axis minimum of
    /// every viewing client's pane area; a client reporting
    /// [`PaneArea::Starving`] contributes none), so a smaller client shrinks a
    /// tab and a departing one lets it grow: the tab's live panes reflow to the
    /// new size, one [`Event::PtyResized`] each. A tab with no effective size
    /// keeps its sizes. The attach always marks the
    /// screen stale so every client repaints from the reconciled snapshot. An attach naming an unknown session, or a tab the
    /// session does not hold, is dropped. `attached_at` is supplied by the
    /// producer; the handler never reads the clock itself.
    // Carries the whole of one attach: where it lands (`session_id`,
    // `client_id`, `active_tab_id`), the view it arrives with (`viewport_size`,
    // `pane_area`), and where it came from (`attached_at`, `is_remote`).
    #[allow(clippy::too_many_arguments)]
    pub fn handle_client_attach(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        viewport_size: Size,
        pane_area: Option<PaneArea>,
        active_tab_id: TabId,
        attached_at: SystemTime,
        is_remote: bool,
    ) -> Vec<Event> {
        self.handle_client_attach_with_cell_size(
            session_id,
            client_id,
            viewport_size,
            pane_area,
            active_tab_id,
            None,
            attached_at,
            is_remote,
        )
    }

    /// Register a client and apply the terminal's initial cell measurement
    /// before the tab is reflowed.
    #[allow(clippy::too_many_arguments)]
    pub fn handle_client_attach_with_cell_size(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        viewport_size: Size,
        pane_area: Option<PaneArea>,
        active_tab_id: TabId,
        cell_size: Option<koshi_core::geometry::PixelCellSize>,
        attached_at: SystemTime,
        is_remote: bool,
    ) -> Vec<Event> {
        let client_origin = if is_remote {
            ClientOrigin::Remote
        } else {
            ClientOrigin::Local
        };
        // Clone the shared backend before borrowing the session: the reflow then
        // needs no `&self` across the mutation.
        let pty_backend = Arc::clone(self.get_pty_backend());
        let mut emitted_events = Vec::new();

        // Validate the target: an attach naming an unknown session, or a tab the
        // session does not hold, is dropped.
        match self.session_by_id.get(&session_id) {
            Some(session) if session.tabs.contains_key(&active_tab_id) => {}
            _ => return Vec::new(),
        }
        let target_client_was_existing = self
            .session_by_id
            .get(&session_id)
            .is_some_and(|session| session.clients.get_client_by_id(client_id).is_some());
        // If the id already lives in a different session, detach it there first
        // and reflow the tab it leaves. One id is never held in two registries.
        if let Some(old_session_id) = self
            .get_session_for_client(client_id)
            .map(|session| session.session_id)
        {
            if old_session_id != session_id {
                let old_session = self
                    .session_by_id
                    .get_mut(&old_session_id)
                    .expect("session located above");
                let old_tab_id = old_session
                    .detach_client(client_id)
                    .map(|client| client.get_active_tab());
                if let Some(old_tab_id) = old_tab_id {
                    let old_affected_client_ids =
                        list_clients_affected_by_tabs(old_session, &[old_tab_id], None);
                    advance_session_placement_revision_when_possible(old_session);
                    advance_client_placement_revisions_when_possible(
                        old_session,
                        &old_affected_client_ids,
                    );
                }
                if let Some(old_tab_id) = old_tab_id {
                    self.reflow_tab_if_viewed(
                        pty_backend.as_ref(),
                        old_session_id,
                        old_tab_id,
                        &mut emitted_events,
                    );
                }
            }
        }

        let session = self
            .session_by_id
            .get_mut(&session_id)
            .expect("target session validated above");

        // A same-session re-attach updates the view in place, preserving the
        // client's accumulated state and yielding the tab it moved off of; a
        // fresh id is registered anew and has no prior tab.
        let prior_tab_id = if let Some(client) = session.clients.get_client_mut_by_id(client_id) {
            let previous_tab_id = client.get_active_tab();
            client.update_viewport(viewport_size);
            client.update_pane_area(pane_area);
            client.update_active_tab(active_tab_id);
            client.replace_cell_size(cell_size);
            client.update_origin(client_origin);
            Some(previous_tab_id)
        } else {
            let label = generate_name(NameKind::Client, |candidate| {
                session
                    .clients
                    .list_attached_clients()
                    .any(|client| client.get_label() == candidate)
            });
            let color = (0..=u8::MAX)
                .find(|candidate| {
                    !session
                        .clients
                        .list_attached_clients()
                        .any(|client| client.get_color() == *candidate)
                })
                // Every palette index is in use: this client takes index 0,
                // which another client already holds.
                .unwrap_or(0);
            let mut client = Client::from_attachment(
                client_id,
                session_id,
                attached_at,
                viewport_size,
                pane_area,
                active_tab_id,
                client_origin,
                label,
                color,
            );
            client.replace_cell_size(cell_size);
            // A profile carrying `lock` hands its starting mode to the first
            // client that attaches, and the flag is spent there.
            if session.take_start_lock() {
                client.update_lock_mode(LockMode::Locked);
                emitted_events.push(Event::InputModeChanged(InputModeChanged {
                    client_id,
                    lock_mode: LockMode::Locked,
                }));
            }
            session.attach_client(client);
            None
        };

        // A client with no focus in the tab it now views starts on that tab's
        // most recent pane, which a session records when the tab is created. A
        // client that already focused a pane here keeps it.
        let landed_pane_id = session
            .tabs
            .get(&active_tab_id)
            .and_then(|tab| tab.list_focus_mru().first().copied());
        if let (Some(pane_id), Some(client)) = (
            landed_pane_id,
            session.clients.get_client_mut_by_id(client_id),
        ) {
            if client.get_focused_pane(active_tab_id).is_none() {
                client.update_focused_pane(active_tab_id, pane_id);
                emitted_events.push(Event::PaneFocused(PaneFocused {
                    client_id,
                    tab_id: active_tab_id,
                    pane_id,
                    previous_pane_id: None,
                }));
            }
        }

        let mut affected_tab_ids = vec![active_tab_id];
        if let Some(prior_tab_id) = prior_tab_id {
            if prior_tab_id != active_tab_id {
                affected_tab_ids.push(prior_tab_id);
            }
        }
        let affected_client_ids =
            list_clients_affected_by_tabs(session, &affected_tab_ids, Some(client_id));
        let clients_to_advance = affected_client_ids
            .into_iter()
            .filter(|affected_client_id| {
                target_client_was_existing || *affected_client_id != client_id
            })
            .collect::<Vec<_>>();
        advance_session_placement_revision_when_possible(session);
        advance_client_placement_revisions_when_possible(session, &clients_to_advance);

        // Reflow the tab the client now views, plus — on a same-session move —
        // the one it left.
        self.reflow_tab_if_viewed(
            pty_backend.as_ref(),
            session_id,
            active_tab_id,
            &mut emitted_events,
        );
        if let Some(previous_tab_id) = prior_tab_id {
            if previous_tab_id != active_tab_id {
                self.reflow_tab_if_viewed(
                    pty_backend.as_ref(),
                    session_id,
                    previous_tab_id,
                    &mut emitted_events,
                );
            }
        }

        self.render_scheduler.invalidate();

        emitted_events
    }

    /// Update one client's full terminal viewport, reconcile the active tab's
    /// pane region and PTYs, then schedule a frame for the new terminal size.
    ///
    /// `pane_area` replaces the client's report, `None` included. A resize
    /// reporting [`PaneArea::Starving`] from the tab's only viewer resizes no
    /// PTY; that client's next frame carries every pane suppressed.
    pub fn handle_client_resize(
        &mut self,
        client_id: ClientId,
        viewport_size: Size,
        pane_area: Option<PaneArea>,
    ) -> Vec<Event> {
        self.handle_client_resize_with_cell_size(client_id, viewport_size, pane_area, None)
    }

    /// Update one client's viewport and optional measured cell dimensions.
    pub(crate) fn handle_client_resize_with_cell_size(
        &mut self,
        client_id: ClientId,
        viewport_size: Size,
        pane_area: Option<PaneArea>,
        cell_size: Option<koshi_core::geometry::PixelCellSize>,
    ) -> Vec<Event> {
        let pty_backend = Arc::clone(self.get_pty_backend());
        let Some(session_id) = self
            .get_session_for_client(client_id)
            .map(|session| session.session_id)
        else {
            return Vec::new();
        };
        let session = self
            .session_by_id
            .get_mut(&session_id)
            .expect("session located above");
        let Some(client) = session.clients.get_client_by_id(client_id) else {
            return Vec::new();
        };
        let active_tab_id = client.get_active_tab();
        let view_changed = client.get_viewport_size() != viewport_size
            || client.get_reported_pane_area() != pane_area
            || client.get_cell_size() != cell_size;
        let affected_client_ids =
            list_clients_affected_by_tabs(session, &[active_tab_id], Some(client_id));
        let Some(client) = session.clients.get_client_mut_by_id(client_id) else {
            return Vec::new();
        };
        client.update_viewport(viewport_size);
        client.update_pane_area(pane_area);
        client.replace_cell_size(cell_size);
        if view_changed {
            advance_session_placement_revision_when_possible(session);
            advance_client_placement_revisions_when_possible(session, &affected_client_ids);
        }

        let mut emitted_events = Vec::new();
        self.reflow_tab_if_viewed(
            pty_backend.as_ref(),
            session_id,
            active_tab_id,
            &mut emitted_events,
        );
        self.render_scheduler.invalidate();
        emitted_events
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
    pub(crate) fn save_client_view(&mut self, client_id: ClientId, detached_at: SystemTime) {
        if self.client_ids_awaiting_reconnect.contains(&client_id) {
            return;
        }
        let Some(session_id) = self
            .get_session_for_client(client_id)
            .map(|session| session.session_id)
        else {
            self.saved_view_store.forget_client_resume_token(client_id);
            return;
        };
        let Some(client) = self
            .session_by_id
            .get(&session_id)
            .and_then(|session| session.clients.get_client_by_id(client_id))
        else {
            self.saved_view_store.forget_client_resume_token(client_id);
            return;
        };
        self.saved_view_store.save_client_view(client, detached_at);
    }

    /// Detach the client `client_id`, then reconcile the PTY sizes of the tab it
    /// was viewing and schedule a redraw.
    ///
    /// Removing the client hands back its record, whose `active_tab` names the
    /// tab whose viewer set shrank. The departing viewer is dropped from that
    /// tab's effective size, so if larger viewers remain the tab grows back: its
    /// live panes reflow to the new [`Session::get_tab_viewport`], one
    /// [`Event::PtyResized`] each. When it was the last viewer the tab has no
    /// viewport and keeps its sizes. The detach always marks the screen stale so
    /// the remaining clients repaint. A detach for a client this runtime does
    /// not hold is dropped.
    ///
    /// Every subscription registered as viewing this client is dropped with the
    /// record, closing the sending end of each one's queue.
    ///
    /// Runs for every detach trigger: a connection drop (either half of an
    /// attached client's connection ending), the [`Command::Detach`] /
    /// [`Command::DetachAll`] execution arms, and a [`Command::Quit`] whose
    /// command source names a client. Target resolution happens at command resolution
    /// before this is reached. With `auto-close-session` on, a detach that
    /// leaves the session with no client requests a graceful quit.
    ///
    /// A detach for a client that is still awaiting reconnect is dropped: that
    /// record's fate belongs to the grace window, which detaches it through
    /// `handle_drop_unclaimed_clients` after removing it from the set.
    pub fn handle_client_detach(&mut self, client_id: ClientId) -> Vec<Event> {
        if self.client_ids_awaiting_reconnect.contains(&client_id) {
            return Vec::new();
        }

        // Clone the shared backend before borrowing the session: the reflow then
        // needs no `&self` across the mutation.
        let pty_backend = Arc::clone(self.get_pty_backend());

        // A detach for a client no session holds is dropped.
        let Some(session_id) = self
            .get_session_for_client(client_id)
            .map(|session| session.session_id)
        else {
            return Vec::new();
        };
        let Some(session) = self.session_by_id.get(&session_id) else {
            return Vec::new();
        };
        let Some(removed_client) = session.clients.get_client_by_id(client_id) else {
            return Vec::new();
        };
        let active_tab_id = removed_client.get_active_tab();
        let affected_client_ids: Vec<ClientId> =
            list_clients_affected_by_tabs(session, &[active_tab_id], None)
                .into_iter()
                .filter(|affected_client_id| *affected_client_id != client_id)
                .collect();
        let session = self
            .session_by_id
            .get_mut(&session_id)
            .expect("session located above");

        // Removing the client returns its record; its `active_tab` is the tab
        // whose effective size may now grow.
        let removed_client = session.detach_client(client_id);
        let active_tab_id = removed_client
            .as_ref()
            .map(|client| client.get_active_tab());
        if removed_client.is_some() {
            advance_session_placement_revision_when_possible(session);
            advance_client_placement_revisions_when_possible(session, &affected_client_ids);
        }
        // A client did leave, and none is left attached.
        let is_session_empty = removed_client.is_some() && !session.clients.has_clients();
        self.unsubscribe_client(client_id);

        let mut emitted_events = Vec::new();
        // Reflow the tab the client left, if any other client still views it; a
        // tab whose last viewer just left has no viewport and keeps its sizes.
        if let Some(active_tab_id) = active_tab_id {
            self.reflow_tab_if_viewed(
                pty_backend.as_ref(),
                session_id,
                active_tab_id,
                &mut emitted_events,
            );
        }

        self.render_scheduler.invalidate();

        // `auto-close-session` ends the session when its last client leaves.
        // Each pane's child is asked to stop and given the graceful window
        // before it is killed; a stop request that cannot be delivered goes
        // straight to the kill.
        if is_session_empty && self.config.should_auto_close_session {
            self.request_graceful_quit();
        }

        emitted_events
    }

    /// Detach every client whose record came across an image swap and has not
    /// attached again by `unclaimed_client_deadline`.
    ///
    /// Each one goes through
    /// [`handle_client_detach`](Self::handle_client_detach), so its tab reflows
    /// and `auto-close-session` still ends a session left with no client. A
    /// client that attached again already left the set, so the usual case
    /// detaches nobody and emits nothing. The detaches run in client-id order,
    /// so the events they emit arrive in one settled order.
    ///
    /// `unclaimed_client_deadline` is when the grace window closed, supplied by the producer;
    /// the handler never reads the clock to decide anything.
    pub(crate) fn handle_drop_unclaimed_clients(
        &mut self,
        unclaimed_client_deadline: Instant,
    ) -> Vec<Event> {
        if self.client_ids_awaiting_reconnect.is_empty() {
            return Vec::new();
        }
        let mut unclaimed_client_ids: Vec<ClientId> =
            std::mem::take(&mut self.client_ids_awaiting_reconnect)
                .into_iter()
                .collect();
        unclaimed_client_ids.sort();
        tracing::info!(
            unclaimed = unclaimed_client_ids.len(),
            waited_ms = Instant::now()
                .saturating_duration_since(unclaimed_client_deadline)
                .as_millis(),
            "detaching the clients that did not attach again after the restart"
        );
        let mut emitted_events = Vec::new();
        for client_id in unclaimed_client_ids {
            emitted_events.extend(self.handle_client_detach(client_id));
        }
        emitted_events
    }

    /// Handle [`Command::ToggleLockMode`]: flip the target client between
    /// pass-through [`LockMode::Locked`] and [`LockMode::Normal`].
    ///
    /// A client already locked unlocks; a client in any other mode locks. The
    /// toggle always changes the mode, so it always emits.
    pub(super) fn handle_toggle_lock_mode(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &ToggleLockModeArgs,
    ) -> Result<CommandResult, Rejection> {
        self.set_lock_mode(
            command_id,
            command_source,
            command_args.client_id,
            |current_lock_mode| match current_lock_mode {
                LockMode::Locked => LockMode::Normal,
                _ => LockMode::Locked,
            },
        )
    }

    /// Handle [`Command::SetLockMode`]: set the target client to
    /// [`LockMode::Locked`] when `command_args.locked`, else [`LockMode::Normal`].
    ///
    /// Setting the mode the client already holds is a no-op: applied, zero
    /// events.
    pub(super) fn handle_set_lock_mode(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &LockModeArgs,
    ) -> Result<CommandResult, Rejection> {
        let requested_lock_mode = if command_args.is_locked {
            LockMode::Locked
        } else {
            LockMode::Normal
        };
        self.set_lock_mode(
            command_id,
            command_source,
            command_args.client_id,
            move |_| requested_lock_mode,
        )
    }

    /// Set the target client's [`LockMode`], emitting [`Event::InputModeChanged`]
    /// only when it changes. `resolve_lock_mode` maps the client's current mode to the
    /// next one, so the toggle and the explicit set share one path.
    ///
    /// Lock mode targets a client alone — the explicit `client` argument when
    /// set, else the acting client — no pane is resolved, so a client with no
    /// focused pane still locks. Nothing in the layout, focus, or any PTY
    /// changes; a no-op change mutates nothing.
    fn set_lock_mode(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        explicit_client_id: Option<ClientId>,
        resolve_lock_mode: impl FnOnce(LockMode) -> LockMode,
    ) -> Result<CommandResult, Rejection> {
        let (client_id, client) = self.acting_client_mut(command_source, explicit_client_id)?;

        let current_lock_mode = client.get_lock_mode();
        let next_lock_mode = resolve_lock_mode(current_lock_mode);
        let mut transaction_scope = TransactionScope::new();
        if next_lock_mode != current_lock_mode {
            client.update_lock_mode(next_lock_mode);
            transaction_scope.emit(Event::InputModeChanged(InputModeChanged {
                client_id,
                lock_mode: next_lock_mode,
            }));
        }
        Ok(transaction_scope.commit(command_id, &mut self.event_bus))
    }

    /// The target client's mutable record, for commands that act on one
    /// client alone (the lock and mouse-select commands). The client is the
    /// one [`Self::resolve_view_client`] picks — the explicit target when
    /// given, else the acting client — so the record mutated here is the same
    /// one [`Self::validate_command`] admitted the command against.
    fn acting_client_mut(
        &mut self,
        command_source: &CommandSource,
        explicit_client_id: Option<ClientId>,
    ) -> Result<(ClientId, &mut Client), Rejection> {
        let acting_session = Self::require_session(self.resolve_acting_session(command_source)?)?;
        let session_id = acting_session.session_id;
        let client_id =
            Self::resolve_view_client(explicit_client_id, command_source, acting_session)?;
        let session = self
            .session_by_id
            .get_mut(&session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        let client = session
            .clients
            .get_client_mut_by_id(client_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::SourceClientStale))?;
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
        command_source: &CommandSource,
    ) -> Result<CommandResult, Rejection> {
        let (client_id, client) = self.acting_client_mut(command_source, None)?;
        let is_mouse_selection_enabled = client.toggle_mouse_selection();
        let mut transaction_scope = TransactionScope::new();
        transaction_scope.emit(Event::MouseSelectChanged(MouseSelectChanged {
            client_id,
            is_enabled: is_mouse_selection_enabled,
        }));
        Ok(transaction_scope.commit(command_id, &mut self.event_bus))
    }
}
