//! Client lifecycle (attach, resize, detach) and the handlers of commands
//! that change one client's own state: lock mode, mouse select, and that
//! client's move, pin and minimize of a floating pane.

use super::*;

use koshi_core::geometry::PixelCellSize;
use koshi_ipc::placement::PanePlacementPreviewTarget;
use koshi_ipc::protocol::ConnectionToken;

use crate::runtime::attach::build_session_structure_snapshot;
use crate::runtime::event::AttachAccepted;
use crate::runtime::saved_view::SavedView;

impl Server {
    /// Record `cell_size` as the measured cell size of `client_id`, then reflow
    /// the tab it views and the session's floating panes. A client that no
    /// session holds changes nothing.
    /// Publishes one [`Event::PtyResized`] per resized PTY: the tab's panes
    /// first, then the floating panes
    /// ([`reflow_floating_panes`](Self::reflow_floating_panes)).
    pub(crate) fn handle_client_cell_size(
        &mut self,
        client_id: ClientId,
        cell_size: PixelCellSize,
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
        let active_tab_id = client.get_active_tab_id();
        if let Some(client) = session.clients.get_client_mut_by_id(client_id) {
            client.update_cell_size(Some(cell_size));
        }
        let pty_backend = Arc::clone(self.get_pty_backend());
        let mut emitted_events = Vec::new();
        self.reflow_tab_if_viewed(
            pty_backend.as_ref(),
            session_id,
            active_tab_id,
            &mut emitted_events,
        );
        self.reflow_floating_panes(pty_backend.as_ref(), session_id, &mut emitted_events);
        self.publish_events(&emitted_events);
    }

    /// Serve one attach arriving over the control socket, in this single
    /// dispatcher turn: settle which client this is, register it on the tab it
    /// views, publish what the attach emitted, subscribe it to every event the
    /// session publishes, and read the session's structure back.
    ///
    /// `resume_client_id` names the client record a caller asks to come back as, after
    /// the session replaced its own process image. The record is handed back
    /// when the session still holds it, the tab it was viewing still exists,
    /// no connection is streaming for it, and `resume_token` is the token that
    /// record's last attach minted. The arriving viewport, pane area
    /// and origin replace the record's, and its per-tab focus, zoom, scrollback
    /// offsets, selections, floating view, lock mode, mouse selection mode,
    /// label and color all stay. That is the
    /// whole of the `resume_client_id` path — no `resume_client_id`, an id the session does not
    /// hold, an id whose tab is gone, an id a connection is already streaming
    /// for, and an id presented with no token or another client's token all
    /// mint a fresh client instead, so an attach never fails over
    /// `resume_client_id`. Example: after a swap, a viewer that names the
    /// owner's client id with no token attaches as a fresh client, and the
    /// owner's own attach with its token takes its record back.
    ///
    /// `resume_token` names the view a caller asks to have back, filed when
    /// that caller's last client detached. A token that proves a
    /// `resume_client_id` claim has no view filed under it: a client's view is
    /// filed only once its record goes. A view that comes back puts the client on the
    /// tab it names — the session's first tab when that tab is gone — and then
    /// restores the rest of the view onto the freshly minted client
    /// ([`Self::restore_saved_view`]). The token
    /// is consumed by this one attach: presenting it again restores nothing. A
    /// token the session holds no view under, and a token older than 24
    /// hours, attach with a fresh view instead of failing. An attach that
    /// returns `None` reads no token and spends none: that view stands until
    /// its 24 hours run out.
    ///
    /// A token minted for a client that is still attached, or for a record
    /// that came across a swap and still awaits reconnect, takes that client
    /// over: its view is filed under the token ([`Self::save_client_view`])
    /// and restored onto the fresh client, and its record is removed with no
    /// quit, before the fresh client registers. Its subscription goes with
    /// the record, and every mouse button it holds is released
    /// ([`Self::release_client_mouse_buttons`]). Example: a viewer on another
    /// machine dials again before the session read that its first link
    /// dropped; the dial gets the view back, and the session holds one client
    /// for that viewer, not two.
    ///
    /// Every accepted attach mints a new token, carried back on
    /// [`AttachAccepted::resume_token`].
    ///
    /// `pane_area` is recorded on the client record, `None` included, and
    /// handed back on [`AttachAccepted::pane_area`]. `cell_size` is the
    /// terminal's measured cell size in pixels, recorded before the tab
    /// reflows; `None` records no measurement.
    ///
    /// Registration and subscription land in the same turn, so the structure
    /// returned here and the queue's first event describe one continuous
    /// state: no change can slip between them. `None` when no session is
    /// running, or when the one running holds no tab to view — neither is
    /// something a client can attach to. `attached_at` is supplied by the
    /// caller; the handler never reads the clock itself. The subscription
    /// gets a frame at most `max_frames_per_second` times a second
    /// ([`subscribe`](Self::subscribe)).
    ///
    /// `placement_preview_target` is named as the client's placement preview
    /// after its view is restored and before it subscribes
    /// ([`set_pane_placement_preview`](Self::set_pane_placement_preview)).
    /// The client's first frame carries the target's preview. `None` names no
    /// target.
    // Carries the whole of one attach request: what it claims back (`resume_client_id`,
    // `resume_token`), the view it arrives with (`viewport_size`, `pane_area`,
    // `cell_size`, `placement_preview_target`), and how the connection is
    // served (`attached_at`, `is_remote`, `max_frames_per_second`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn handle_ipc_attach(
        &mut self,
        resume_client_id: Option<ClientId>,
        resume_token: Option<ConnectionToken>,
        viewport_size: Size,
        pane_area: Option<PaneArea>,
        cell_size: Option<PixelCellSize>,
        attached_at: SystemTime,
        is_remote: bool,
        max_frames_per_second: u16,
        placement_preview_target: Option<PanePlacementPreviewTarget>,
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
            let client_active_tab_id = client.get_active_tab_id();
            let is_streaming = self
                .find_subscriber_id_by_client_id(claimed_client_id)
                .is_some();
            let is_claim_proven = resume_token.as_ref().is_some_and(|resume_token| {
                self.saved_view_store
                    .find_client_id_by_resume_token(resume_token)
                    == Some(claimed_client_id)
            });
            (session.tabs.contains_key(&client_active_tab_id) && !is_streaming && is_claim_proven)
                .then_some((claimed_client_id, client_active_tab_id))
        });
        let mut emitted_events = Vec::new();
        // A token minted for a client the session still holds, attached or
        // awaiting its re-attach: that client's view is filed under the token
        // and taken below, and its record goes.
        if let (None, Some(resume_token)) = (claimed_client_view, resume_token.as_ref()) {
            if let Some(previous_client_id) = self
                .saved_view_store
                .find_client_id_by_resume_token(resume_token)
            {
                self.client_ids_awaiting_reconnect
                    .remove(&previous_client_id);
                self.save_client_view(previous_client_id, attached_at);
                self.remove_client_record(previous_client_id, &mut emitted_events);
            }
        }
        let saved_view = resume_token.and_then(|resume_token| {
            self.saved_view_store
                .take_saved_view(&resume_token, attached_at)
        });
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

        emitted_events.extend(self.handle_client_attach(
            session_id,
            client_id,
            viewport_size,
            pane_area,
            active_tab_id,
            cell_size,
            attached_at,
            is_remote,
        ));
        if let Some(saved_view) = saved_view {
            emitted_events.extend(self.restore_saved_view(
                session_id,
                client_id,
                active_tab_id,
                &saved_view,
            ));
        }
        self.publish_events(&emitted_events);
        self.set_pane_placement_preview(client_id, placement_preview_target);

        let (deliveries, writer_handoff) = self.subscribe(client_id, max_frames_per_second);
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
            writer_handoff,
            ending_notice: Arc::clone(self.event_bus.get_ending_notice()),
            resume_token,
            pane_area,
        })
    }

    /// Put `saved_view` back on `client_id` in `session_id`, then reconcile the
    /// PTY sizes of `active_tab_id`.
    ///
    /// Applies the focus history of each tab first, oldest pane first, so the
    /// saved newest pane ends up focused and the panes the attach focused come
    /// after the saved ones. Then the zoomed pane of each tab, then the scroll
    /// offset of each pane, then the floating view
    /// ([`Client::replace_floating_view`]). Saved `[a, b]` over an attach
    /// focus `[c]` gives `[a, b, c]`.
    /// [`Client::update_focused_pane`] moves an existing zoom onto the pane it
    /// focuses; the zoom pass runs after it, and leaves each tab zoomed on the
    /// pane `saved_view` names.
    ///
    /// A focus history pane or a zoom entry naming a tab the session does not
    /// hold, or a pane that tab's layout does not hold, is dropped instead of
    /// applied: with none of a tab's saved panes applied, that tab keeps the
    /// focus the attach gave it and stays tiled. A scroll
    /// entry naming a pane the session does not hold is dropped, and that
    /// pane sits at the live bottom. A floating view entry naming a pane that
    /// is not a floating pane of the session is dropped, and the newest
    /// floating pane still shown takes the floating focus. A floating pane the
    /// saved view holds no entry for is minimized at the default placement. A
    /// scroll offset past the lines its pane still retains is cut down to that
    /// count — an offset of 500 onto a pane retaining 120 lines restores 120,
    /// and onto a pane whose scrollback was erased restores 0, the live
    /// bottom. A restored zoom changes the size the tab's panes solve to, so
    /// the tab reflows and one [`Event::PtyResized`] is returned for each pane
    /// whose PTY size changed.
    ///
    /// A restore that moves the client's input off the pane the attach put it
    /// on returns one [`Event::PaneFocused`] naming the pane that takes the
    /// input after the restore ([`build_pane_focused_event`]), with
    /// `tab_id: None` for a floating pane. The events of the tab's solve
    /// ([`Self::reflow_tab_if_viewed`]) and its [`Event::PtyResized`] events
    /// follow it. A client that cannot draw floating panes minimizes, in that
    /// solve, every floating pane the restored view shows.
    ///
    /// Returns no event when the session or the client is gone.
    fn restore_saved_view(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        active_tab_id: TabId,
        saved_view: &SavedView,
    ) -> Vec<Event> {
        let pty_backend = Arc::clone(self.get_pty_backend());
        let mut emitted_events = Vec::new();
        let Some(session) = self.session_by_id.get_mut(&session_id) else {
            return emitted_events;
        };
        let floating_pane_ids = session.floating_set.list_pane_ids();
        let tab_by_id = &session.tabs;
        let pane_registry = &session.panes;
        let Some(client) = session.clients.get_client_mut_by_id(client_id) else {
            return emitted_events;
        };
        let previous_focused_pane_id = client.get_active_focused_pane_id();
        let is_pane_in_tab_layout = |tab_id: TabId, pane_id: PaneId| {
            tab_by_id
                .get(&tab_id)
                .is_some_and(|tab| tab.get_layout_tree().has_pane(pane_id))
        };
        for (&tab_id, saved_focus_history) in &saved_view.focus_history_by_tab_id {
            for &pane_id in saved_focus_history.iter().rev() {
                if is_pane_in_tab_layout(tab_id, pane_id) {
                    client.update_focused_pane(tab_id, pane_id);
                }
            }
        }
        for (&tab_id, &pane_id) in &saved_view.zoomed_pane_id_by_tab_id {
            if is_pane_in_tab_layout(tab_id, pane_id) {
                client.zoom_pane(tab_id, pane_id);
            }
        }
        for (&pane_id, &scroll_offset) in &saved_view.scroll_offset_by_pane_id {
            if pane_registry.get_pane_record_by_id(pane_id).is_some() {
                let retained_line_count =
                    self.terminal_engine_by_pane_id
                        .get(&pane_id)
                        .map_or(0, |terminal_engine| {
                            terminal_engine
                                .get_terminal_state()
                                .get_scrollback()
                                .get_retained_line_count()
                        });
                client.set_scroll_offset(pane_id, scroll_offset.min(retained_line_count));
            }
        }
        client.replace_floating_view(
            &saved_view.floating_pane_view_by_pane_id,
            &saved_view.shown_floating_pane_ids,
            &floating_pane_ids,
        );
        let pane_focused_event = build_pane_focused_event(client, previous_focused_pane_id);
        emitted_events.extend(pane_focused_event);

        self.reflow_tab_if_viewed(
            pty_backend.as_ref(),
            session_id,
            active_tab_id,
            &mut emitted_events,
        );

        emitted_events
    }

    /// Attach a client to `session_id` viewing `active_tab_id`, then reconcile the
    /// affected tabs' PTY sizes and mark the view of every subscriber changed.
    ///
    /// A client lives in exactly one session. If this id already lives in another
    /// session it is detached there first — reflowing the tab it leaves — so it
    /// is never recorded twice. Within the target session an id that is already
    /// attached is a re-attach: its view updates in place, keeping its per-tab
    /// focus, zoom, scrollback offsets, selections, floating view, lock mode,
    /// mouse selection mode, label and color, and the tab it moves off of
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
    /// Records `pane_area` and `cell_size` on the client, `None` included: a re-attach that reports
    /// none replaces an earlier report. `cell_size` is recorded before the tab reflows. The viewer
    /// joins each affected tab's size ([`Session::get_tab_size`], the per-axis
    /// minimum of every viewing client's pane area; a client reporting [`PaneArea::Starving`]
    /// contributes none), so a smaller client shrinks a tab and a departing one lets it grow: the
    /// tab's live panes reflow to the new size, one [`Event::PtyResized`] each. A tab with no
    /// tab size keeps its sizes. After the tabs, the session's floating panes reflow to the new
    /// [`Session::get_shared_floating_viewport`], and so do the floating panes of a session the
    /// client moved out of. An attach naming an unknown session, or a tab the session
    /// does not hold, is dropped. `attached_at` is supplied by the producer; the handler never
    /// reads the clock itself.
    // Carries the whole of one attach: where it lands (`session_id`,
    // `client_id`, `active_tab_id`), the view it arrives with (`viewport_size`,
    // `pane_area`, `cell_size`), and where it came from (`attached_at`,
    // `is_remote`).
    #[allow(clippy::too_many_arguments)]
    pub fn handle_client_attach(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        viewport_size: Size,
        pane_area: Option<PaneArea>,
        active_tab_id: TabId,
        cell_size: Option<PixelCellSize>,
        attached_at: SystemTime,
        is_remote: bool,
    ) -> Vec<Event> {
        let client_origin = if is_remote {
            ClientOrigin::Remote
        } else {
            ClientOrigin::Local
        };
        let pty_backend = Arc::clone(self.get_pty_backend());
        let mut emitted_events = Vec::new();

        // Validate the target: an attach naming an unknown session, or a tab the
        // session does not hold, is dropped.
        match self.session_by_id.get(&session_id) {
            Some(session) if session.tabs.contains_key(&active_tab_id) => {}
            _ => return Vec::new(),
        }
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
                    .map(|client| client.get_active_tab_id());
                if let Some(old_tab_id) = old_tab_id {
                    self.reflow_tab_if_viewed(
                        pty_backend.as_ref(),
                        old_session_id,
                        old_tab_id,
                        &mut emitted_events,
                    );
                    self.reflow_floating_panes(
                        pty_backend.as_ref(),
                        old_session_id,
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
        let previous_tab_id = if let Some(client) = session.clients.get_client_mut_by_id(client_id)
        {
            let previous_tab_id = client.get_active_tab_id();
            client.update_viewport_size(viewport_size);
            client.update_pane_area(pane_area);
            client.update_active_tab_id(active_tab_id);
            client.update_cell_size(cell_size);
            client.update_origin(client_origin);
            Some(previous_tab_id)
        } else {
            let label = generate_name(NameKind::Client, |candidate_label| {
                session
                    .clients
                    .list_attached_clients()
                    .any(|client| client.get_label() == candidate_label)
            });
            let color_index = (0..=u8::MAX)
                .find(|candidate_color_index| {
                    !session
                        .clients
                        .list_attached_clients()
                        .any(|client| client.get_color_index() == *candidate_color_index)
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
                color_index,
            );
            client.update_cell_size(cell_size);
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

        // A client with no focus in the tab it now views lands on that tab's
        // landing pane. A client that already focused a pane here keeps it.
        tab_ops::land_focus(session, client_id, active_tab_id, &mut emitted_events);

        // Reflow the tab the client now views, plus — on a same-session move —
        // the one it left.
        self.reflow_tab_if_viewed(
            pty_backend.as_ref(),
            session_id,
            active_tab_id,
            &mut emitted_events,
        );
        if let Some(previous_tab_id) = previous_tab_id {
            if previous_tab_id != active_tab_id {
                self.reflow_tab_if_viewed(
                    pty_backend.as_ref(),
                    session_id,
                    previous_tab_id,
                    &mut emitted_events,
                );
            }
        }
        self.reflow_floating_panes(pty_backend.as_ref(), session_id, &mut emitted_events);

        emitted_events
    }

    /// Update one client's full terminal viewport, reconcile the active tab's
    /// pane region and PTYs, reflow the session's floating panes to the new
    /// [`Session::get_shared_floating_viewport`], then mark the view of every
    /// subscriber changed.
    ///
    /// `pane_area` and `cell_size` replace the client's reports, `None`
    /// included. A resize reporting [`PaneArea::Starving`] from the tab's only
    /// viewer resizes no PTY of that tab; that client's next frame carries
    /// every pane suppressed.
    pub fn handle_client_resize(
        &mut self,
        client_id: ClientId,
        viewport_size: Size,
        pane_area: Option<PaneArea>,
        cell_size: Option<PixelCellSize>,
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
        let active_tab_id = client.get_active_tab_id();
        let Some(client) = session.clients.get_client_mut_by_id(client_id) else {
            return Vec::new();
        };
        client.update_viewport_size(viewport_size);
        client.update_pane_area(pane_area);
        client.update_cell_size(cell_size);

        let mut emitted_events = Vec::new();
        self.reflow_tab_if_viewed(
            pty_backend.as_ref(),
            session_id,
            active_tab_id,
            &mut emitted_events,
        );
        self.reflow_floating_panes(pty_backend.as_ref(), session_id, &mut emitted_events);
        emitted_events
    }

    /// File the view `client_id` is leaving behind ([`SavedView`]), under the
    /// token that client's attach minted. The record stands for 24 hours
    /// from `detached_at`, and presenting the minted token within that time
    /// hands the view back once.
    ///
    /// `detached_at` is when the producer saw the connection end, supplied by
    /// the producer; the handler never reads the clock itself.
    ///
    /// Files nothing in three cases. A client that is still awaiting reconnect
    /// files nothing and keeps the hash that stands against its id, so only
    /// the attach that presents its token claims its record. A client no
    /// session holds files nothing and its minted hash is dropped, so the
    /// token it was handed takes back nothing. A client whose attach minted no
    /// token files nothing.
    ///
    /// Reads the client's record: call it before that record is removed by
    /// [`handle_client_detach`](Self::handle_client_detach),
    /// [`handle_returning_client_detach`](Self::handle_returning_client_detach),
    /// or an attach that takes the client over.
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
    /// was viewing and of the session's floating panes, and mark the view of
    /// every subscriber changed.
    ///
    /// Removing the client hands back its record, whose active tab names the
    /// tab whose viewer set shrank. The departing viewer is dropped from that
    /// tab's size, so if larger viewers remain the tab grows back: its
    /// live panes reflow to the new [`Session::get_tab_size`], one
    /// [`Event::PtyResized`] each. When it was the last viewer the tab has no
    /// tab size and keeps its sizes. The session's floating panes then reflow
    /// to the new [`Session::get_shared_floating_viewport`]: they grow when the
    /// departing client was the smallest, and keep their sizes when no client
    /// is left.
    ///
    /// Every mouse button the client holds is released first
    /// (`release_client_mouse_buttons`). A detach for a client this runtime
    /// does not hold releases those buttons and changes nothing else.
    ///
    /// Every subscription registered as viewing this client is dropped with the
    /// record, closing the sending end of each one's queue.
    ///
    /// Runs for every detach trigger but a returning client's: an attached
    /// client's connection that ends with a client that does not attach again,
    /// the [`Command::Detach`] / [`Command::DetachAll`] execution arms, and a
    /// [`Command::Quit`] whose command source names a client. Target
    /// resolution happens at command resolution before this is reached. With
    /// `auto-close-session` on, a detach that leaves the session with no
    /// client requests a graceful quit. A client that may attach again goes
    /// through `handle_returning_client_detach`.
    ///
    /// A detach for a client that is still awaiting reconnect is dropped.
    /// `handle_drop_unclaimed_clients` removes that client from the set and
    /// detaches it when the grace window closes.
    pub fn handle_client_detach(&mut self, client_id: ClientId) -> Vec<Event> {
        if self.client_ids_awaiting_reconnect.contains(&client_id) {
            return Vec::new();
        }

        let mut emitted_events = Vec::new();
        let session_id_of_removed_client =
            self.remove_client_record(client_id, &mut emitted_events);
        let is_session_left_without_client = session_id_of_removed_client
            .and_then(|session_id| self.session_by_id.get(&session_id))
            .is_some_and(|session| !session.clients.has_clients());

        // `auto-close-session` ends the session when its last client leaves.
        // Each pane's child is asked to stop and given the graceful window
        // before it is killed; a stop request that cannot be delivered goes
        // straight to the kill.
        if is_session_left_without_client && self.config.should_auto_close_session {
            self.request_graceful_quit();
        }

        emitted_events
    }

    /// Detach the client `client_id`, which may attach again with the resume
    /// token its attach minted: release every mouse button it holds, remove
    /// its record, reflow the tab it was viewing and the session's floating
    /// panes, and mark the view of every subscriber changed, as
    /// [`handle_client_detach`](Self::handle_client_detach) does. Requests no
    /// quit: with `auto-close-session` on, a session this leaves with no
    /// client keeps running.
    ///
    /// Returns one [`Event::PtyResized`] per reflowed pane. A detach for a
    /// client that is still awaiting reconnect, or that no session holds, is
    /// dropped and returns no event.
    ///
    /// Example: a client on another machine whose link dropped is removed,
    /// and the session keeps running for that client to attach again.
    pub(crate) fn handle_returning_client_detach(&mut self, client_id: ClientId) -> Vec<Event> {
        if self.client_ids_awaiting_reconnect.contains(&client_id) {
            return Vec::new();
        }
        let mut emitted_events = Vec::new();
        self.remove_client_record(client_id, &mut emitted_events);
        emitted_events
    }

    /// Release every mouse button the client `client_id` holds
    /// ([`release_client_mouse_buttons`](Self::release_client_mouse_buttons)),
    /// then remove its record, reflow the tab it was viewing and the session's
    /// floating panes, and mark the view of every subscriber changed, as
    /// [`handle_client_detach`](Self::handle_client_detach) states. Pushes one
    /// [`Event::PtyResized`] per reflowed pane onto `emitted_events`. Requests
    /// no quit.
    ///
    /// The id of the session that held the client, once its record is removed.
    /// `None` for a client no session holds, which changes nothing but the
    /// release.
    fn remove_client_record(
        &mut self,
        client_id: ClientId,
        emitted_events: &mut Vec<Event>,
    ) -> Option<SessionId> {
        self.release_client_mouse_buttons(client_id);
        // A detach for a client no session holds is dropped.
        let session = self.get_session_for_client_mut(client_id)?;
        let session_id = session.session_id;
        // Removing the client returns its record; its `active_tab_id` is the tab
        // whose tab size may now grow.
        let removed_client = session
            .detach_client(client_id)
            .expect("the session found for the client holds it");
        let active_tab_id = removed_client.get_active_tab_id();
        self.unsubscribe_client(client_id);

        // Reflow the tab the client left, if any other client still views it; a
        // tab whose last viewer just left has no tab size and keeps its sizes.
        let pty_backend = Arc::clone(self.get_pty_backend());
        self.reflow_tab_if_viewed(
            pty_backend.as_ref(),
            session_id,
            active_tab_id,
            emitted_events,
        );
        self.reflow_floating_panes(pty_backend.as_ref(), session_id, emitted_events);

        Some(session_id)
    }

    /// Detach every client whose record came across an image swap and has not
    /// attached again by `unclaimed_client_deadline`.
    ///
    /// Each one's view is filed under the token its last attach minted, as
    /// [`save_client_view`](Self::save_client_view) files one with
    /// `unclaimed_client_deadline` as the detach time: that token takes the view
    /// back within 24 hours. A client carried with no resume-token hash files
    /// nothing. Each one's record is then removed as
    /// [`handle_client_detach`](Self::handle_client_detach) removes one, and its
    /// tab and the session's floating panes reflow. No quit is requested: a
    /// session these removals leave with no client keeps running, with
    /// `auto-close-session` on or off. A client that attached again is no
    /// longer in the set: in the usual case no client is removed and no event
    /// is emitted. The removals run in client-id order, and the events they
    /// emit arrive in that order.
    ///
    /// `unclaimed_client_deadline` is when the grace window closed, supplied by the producer;
    /// the handler never reads the clock to decide anything.
    pub(crate) fn handle_drop_unclaimed_clients(
        &mut self,
        unclaimed_client_deadline: SystemTime,
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
            unclaimed_client_count = unclaimed_client_ids.len(),
            waited_ms = SystemTime::now()
                .duration_since(unclaimed_client_deadline)
                .unwrap_or_default()
                .as_millis(),
            "detaching the clients that did not attach again after the restart"
        );
        let mut emitted_events = Vec::new();
        for client_id in unclaimed_client_ids {
            self.save_client_view(client_id, unclaimed_client_deadline);
            self.remove_client_record(client_id, &mut emitted_events);
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
    /// Setting the mode the client already holds leaves the mode as it is:
    /// applied, zero events.
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
    /// next one. The toggle and the explicit set both run through here.
    ///
    /// Lock mode targets a client alone — the explicit `client` argument when
    /// set, else the acting client — no pane is resolved, so a client with no
    /// focused pane still locks. Nothing in the layout, focus, or any PTY
    /// changes.
    ///
    /// A command whose source is [`CommandSource::KeyBinding`] naming the
    /// target client came from that client's own connection: the client
    /// records `command_id` as its last applied lock command, also when the
    /// mode stays as it was. A command from any other source changes only the
    /// mode.
    fn set_lock_mode(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        explicit_client_id: Option<ClientId>,
        resolve_lock_mode: impl FnOnce(LockMode) -> LockMode,
    ) -> Result<CommandResult, Rejection> {
        let (client_id, client) =
            self.resolve_acting_client_mut(command_source, explicit_client_id)?;

        if *command_source == CommandSource::from_key_binding(client_id) {
            client.set_last_applied_lock_command_id(command_id);
        }
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
    fn resolve_acting_client_mut(
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
        let client = Self::require_client_mut(session, client_id)?;
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
        let (client_id, client) = self.resolve_acting_client_mut(command_source, None)?;
        let is_mouse_selection_enabled = client.toggle_mouse_selection();
        let mut transaction_scope = TransactionScope::new();
        transaction_scope.emit(Event::MouseSelectChanged(MouseSelectChanged {
            client_id,
            is_enabled: is_mouse_selection_enabled,
        }));
        Ok(transaction_scope.commit(command_id, &mut self.event_bus))
    }

    /// Handle [`Command::MoveFloatingPane`]: store `Moved(top_left_cell)` as
    /// the acting client's position of the floating pane
    /// ([`Self::resolve_move_floating_pane_target`]).
    ///
    /// A changed position emits one [`Event::FloatingPaneMoved`]. A client
    /// that already stores `Moved(top_left_cell)` changes nothing and emits
    /// nothing. The pane's size and every other client's view stay as they
    /// are.
    ///
    /// A command whose source is [`CommandSource::Mouse`] or
    /// [`CommandSource::KeyBinding`] naming the acting client came from that
    /// client's own connection: the client records `command_id` as its last
    /// applied floating pane move, also when the position stays as it was.
    pub(super) fn handle_move_floating_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &MoveFloatingPaneArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let FloatingPaneViewTarget {
            session_id,
            client_id,
            pane_id,
        } = self.resolve_move_floating_pane_target(command_args, command_source, acting_session)?;
        let session = self
            .session_by_id
            .get_mut(&session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetGone))?;
        let client = Self::require_client(session, client_id)?;
        let is_position_changed = Self::require_floating_pane_view(client, pane_id)?.position
            != FloatingPanePosition::Moved(command_args.top_left_cell);
        let client = Self::require_client_mut(session, client_id)?;
        if *command_source == CommandSource::from_mouse(client_id)
            || *command_source == CommandSource::from_key_binding(client_id)
        {
            client.set_last_applied_floating_pane_move_command_id(command_id);
        }
        if !is_position_changed {
            return Ok(TransactionScope::new().commit(command_id, &mut self.event_bus));
        }
        let _ = client.set_floating_pane_position(pane_id, command_args.top_left_cell);
        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            vec![Event::FloatingPaneMoved(FloatingPaneMoved {
                client_id,
                pane_id,
                top_left_cell: command_args.top_left_cell,
            })],
        ))
    }

    /// Handle [`Command::SetPanePinned`]: pin or unpin the floating pane in
    /// the acting client's view ([`Self::resolve_pane_pin_target`]).
    ///
    /// A pin stores `Pinned` at the cell where the client draws the pane, or
    /// would draw it, and an unpin stores `Moved` at the pinned cell. Either
    /// change emits one [`Event::PanePinChanged`]. A view that already has the
    /// asked state changes nothing and emits nothing. Every other client's
    /// view stays as it is.
    pub(super) fn handle_set_pane_pinned(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &SetPanePinnedArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let Some((
            FloatingPaneViewTarget {
                session_id,
                client_id,
                pane_id,
            },
            pane_pin_change,
        )) = self.resolve_pane_pin_target(command_args, command_source, acting_session)?
        else {
            return Ok(TransactionScope::new().commit(command_id, &mut self.event_bus));
        };
        let session = self
            .session_by_id
            .get_mut(&session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetGone))?;
        let client = Self::require_client_mut(session, client_id)?;
        match pane_pin_change {
            PanePinChange::Pin(top_left_cell) => client.pin_floating_pane(pane_id, top_left_cell),
            PanePinChange::Unpin => client.unpin_floating_pane(pane_id),
        }
        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            vec![Event::PanePinChanged(PanePinChanged {
                client_id,
                pane_id,
                is_pinned: command_args.is_pinned,
            })],
        ))
    }

    /// Handle [`Command::SetPaneMinimized`]: minimize or restore the floating
    /// pane in the acting client's view ([`Self::resolve_pane_minimize_target`]).
    ///
    /// A minimize stops the client drawing the pane
    /// ([`Client::minimize_floating_pane`]); a focus on it moves to the newest
    /// floating pane the client still shows, else to its active tab's focused
    /// pane. A restore shows the pane above the client's other floating panes
    /// and focuses it ([`Client::focus_floating_pane`]). Either change emits
    /// one [`Event::PaneMinimizedChanged`], then one [`Event::PaneFocused`] when
    /// the client's input moved to a pane. A view that already has the asked
    /// state changes nothing and emits nothing. Every other client's view
    /// stays as it is.
    pub(super) fn handle_set_pane_minimized(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &SetPaneMinimizedArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let Some(FloatingPaneViewTarget {
            session_id,
            client_id,
            pane_id,
        }) = self.resolve_pane_minimize_target(command_args, command_source, acting_session)?
        else {
            return Ok(TransactionScope::new().commit(command_id, &mut self.event_bus));
        };
        let session = self
            .session_by_id
            .get_mut(&session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetGone))?;
        let client = Self::require_client_mut(session, client_id)?;
        let previous_focused_pane_id = client.get_active_focused_pane_id();
        if command_args.is_minimized {
            let _ = client.minimize_floating_pane(pane_id);
        } else {
            let _ = client.focus_floating_pane(pane_id);
        }
        let mut emitted_events: Vec<Event> =
            build_pane_minimized_changed_events(client_id, &[pane_id], command_args.is_minimized)
                .collect();
        emitted_events.extend(build_pane_focused_event(client, previous_focused_pane_id));
        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            emitted_events,
        ))
    }

    /// Handle [`Command::SetAllFloatingPanesMinimized`]: minimize every
    /// floating pane the acting client shows, oldest shown first, or restore
    /// every floating pane it minimized, in the order the panes became
    /// floating.
    ///
    /// The acting client is the target client the command source names
    /// ([`CommandSource::get_target_client_id`]), else the issuer while it is
    /// attached, else, for a CLI source, the session's sole attached client
    /// ([`Self::resolve_view_client`]). After a minimize, the client's input
    /// returns to its active tab's focused pane
    /// ([`Client::minimize_shown_floating_panes`]). After a restore, each pane
    /// is shown and focused in turn ([`Client::focus_floating_pane`]), so the
    /// last pane restored is drawn on top and focused: floats `a`, `b`, `c`
    /// that became floating in that order, with `a` and `c` minimized and `b`
    /// shown, restore `a`, then `c`, giving `[b, a, c]` with `c` focused.
    ///
    /// A restore by a client that cannot draw floating panes
    /// ([`Session::can_client_draw_floating_panes`]) is
    /// [`RejectReason::InvalidState`] `no floating pane can be drawn on this
    /// client`, changing nothing.
    ///
    /// A change emits one [`Event::PaneMinimizedChanged`] per changed pane, in
    /// that order,
    /// then one [`Event::PaneFocused`] when the client's input moved to a
    /// pane. With no pane to change, nothing changes and nothing is emitted.
    /// Every other client's view stays as it is.
    pub(super) fn handle_set_all_floating_panes_minimized(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &SetAllFloatingPanesMinimizedArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = Self::require_session(self.resolve_acting_session(command_source)?)?;
        let session_id = acting_session.session_id;
        let client_id = Self::resolve_view_client(
            command_source.get_target_client_id(),
            command_source,
            acting_session,
        )?;
        let session = self
            .session_by_id
            .get_mut(&session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetGone))?;
        let client = Self::require_client(session, client_id)?;
        let changing_pane_ids: Vec<PaneId> = if command_args.is_minimized {
            client.list_shown_floating_pane_ids().to_vec()
        } else {
            session
                .floating_set
                .list_members()
                .iter()
                .map(|floating_member| floating_member.pane_id)
                .filter(|&pane_id| !client.is_floating_pane_shown(pane_id))
                .collect()
        };
        if changing_pane_ids.is_empty() {
            return Ok(TransactionScope::new().commit(command_id, &mut self.event_bus));
        }
        if !command_args.is_minimized {
            Self::require_client_drawing_floating_panes(
                session,
                client,
                "no floating pane can be drawn on this client",
            )?;
        }
        let client = Self::require_client_mut(session, client_id)?;
        let previous_focused_pane_id = client.get_active_focused_pane_id();
        if command_args.is_minimized {
            let _ = client.minimize_shown_floating_panes();
        } else {
            for &pane_id in &changing_pane_ids {
                let _ = client.focus_floating_pane(pane_id);
            }
        }
        let mut emitted_events: Vec<Event> = build_pane_minimized_changed_events(
            client_id,
            &changing_pane_ids,
            command_args.is_minimized,
        )
        .collect();
        emitted_events.extend(build_pane_focused_event(client, previous_focused_pane_id));
        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            emitted_events,
        ))
    }
}
