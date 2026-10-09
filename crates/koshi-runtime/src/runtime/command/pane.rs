//! Pane command handlers: create, close, resize, focus, fullscreen,
//! and raw input injection — plus the child-exit event path and the
//! shared pane-removal bookkeeping.

use super::*;

impl Server {
    /// Handle [`Command::NewPane`]: grow the command source pane's tab by one pane —
    /// stacked onto the command source or split from it — and spawn it, in
    /// launch-then-commit order — no session state changes until the child
    /// process is live.
    ///
    /// The candidate tree is built and its fit preflighted, the child PTY
    /// is spawned, and only on success is the pane registered (`Running`), the
    /// tree swapped in, the sibling PTYs reflowed, and the handle parked. A launch
    /// failure commits nothing and rejects; no pane is registered without its
    /// process. A client is designated to view and focus the new pane — an
    /// explicit `--client` target (which wins even over the issuer, and rejects
    /// outright if not attached), else the in-session issuer, else (an external
    /// command source, tab unviewed) the session's sole client; a session with several
    /// attached clients and no named target is rejected as ambiguous, and one with
    /// no attached client at all is rejected. The designated client is switched
    /// onto the tab (if not already there) and the tab it left is reflowed. That
    /// client's zoom drops at the commit, and the new pane shows in the tiled
    /// view it was sized against; any other client's zoom is left alone. All events
    /// seal in one transaction.
    ///
    /// A [`NewPanePlacement::Floating`] placement goes to
    /// [`Self::handle_new_floating_pane`].
    pub(super) fn handle_new_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &NewPaneArgs,
    ) -> Result<CommandResult, Rejection> {
        let (source_pane_id, tab_id, split_direction) = match command_args.placement {
            NewPanePlacement::Split {
                source_pane_id,
                tab_id,
                direction,
            } => (source_pane_id, tab_id, Some(direction)),
            NewPanePlacement::Stacked {
                source_pane_id,
                tab_id,
            } => (source_pane_id, tab_id, None),
            NewPanePlacement::Floating {
                size,
                at,
                is_pinned,
            } => {
                return self.handle_new_floating_pane(
                    command_id,
                    command_source,
                    command_args,
                    size,
                    at,
                    is_pinned,
                );
            }
        };
        let acting_session = self.resolve_acting_session(command_source)?;
        let new_pane_target =
            self.resolve_new_pane_source(source_pane_id, tab_id, command_source, acting_session)?;

        // The shared backend is cloned before the session is borrowed; spawn
        // and resize run on the clone while `&mut Session` is held.
        let pty_backend = Arc::clone(self.get_pty_backend());
        let pane_sizing = self.get_pane_sizing();
        let mut spawn_spec = self.resolve_new_pane_spawn_spec(command_args);
        // No directory was asked for: the new pane opens where the pane it
        // splits from currently is ([`Self::resolve_pane_working_directory`]).
        if spawn_spec.working_directory.is_none() {
            spawn_spec.working_directory = self.resolve_pane_working_directory(
                new_pane_target.session_id,
                new_pane_target.source_pane_id,
            );
        }

        let session = self
            .session_by_id
            .get_mut(&new_pane_target.session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;

        // Build the post-edit tree without mutating anything: `--stacked` joins
        // the command source's stack (creating one when the command source is a plain leaf),
        // otherwise the command source leaf splits directionally. A command source pane that is
        // not a live leaf of the tab rejects here, before any state changes.
        let tab_state = session
            .tabs
            .get(&new_pane_target.tab_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        // The new pane is sized against the tiled solve. The splitting
        // client's zoom drops at the commit, and that client sees the tiled
        // layout. Another client zoomed on a pane of this tab does not draw the
        // new pane, and its view does not count toward the new pane's size.
        let new_pane_id = PaneId::new();
        let edited_layout_tree = match split_direction {
            Some(direction) => split_leaf(
                tab_state.get_layout_tree(),
                new_pane_target.source_pane_id,
                new_pane_id,
                direction,
            ),
            None => add_pane_to_stack(
                tab_state.get_layout_tree(),
                new_pane_target.source_pane_id,
                new_pane_id,
            ),
        };
        let candidate_layout_tree =
            edited_layout_tree.map_err(|_| Rejection::from_reason(RejectReason::TargetNotFound))?;

        // Choose the tab size the split is sized against, and the client (if any)
        // designated to view the tab and focus the new pane. Fit is judged against
        // the candidate: a split too large for the chosen tab size is rejected
        // before anything mutates.
        let (tab_size, designated_client_id) = Self::resolve_new_pane_tab_size(
            session,
            new_pane_target.tab_id,
            &candidate_layout_tree,
            new_pane_target.focus_client_id,
            command_args.client_id,
            pane_sizing,
        )?;

        // Solve the candidate against that tab size to size the new pane and
        // its siblings. A solve that gives the new pane no content rect rejects
        // before any mutation.
        let tab_rect = Rect::from_size_at_origin(tab_size);
        let pane_content_rects = list_content_rects(&solve_layout_with_mode(
            &candidate_layout_tree,
            LayoutMode::Tiled,
            tab_rect,
            pane_sizing,
        ));
        let new_pane_content_rect = pane_content_rects
            .iter()
            .find(|(pane_id, _)| *pane_id == new_pane_id)
            .and_then(|(_, content_rect)| *content_rect)
            .ok_or_else(|| Rejection::from_reason(RejectReason::InvalidState))?;
        let new_pane_pty_size = compute_pty_size(new_pane_content_rect);
        let previous_tab_id = designated_client_id.and_then(|client_id| {
            session
                .clients
                .get_client_by_id(client_id)
                .map(|client| client.get_active_tab_id())
        });
        let mut affected_tab_ids = vec![new_pane_target.tab_id];
        if let Some(previous_tab_id) = previous_tab_id {
            if previous_tab_id != new_pane_target.tab_id {
                affected_tab_ids.push(previous_tab_id);
            }
        }
        let affected_client_ids =
            list_clients_affected_by_tabs(session, &affected_tab_ids, designated_client_id);
        ensure_session_placement_revision_capacity(session)?;
        ensure_client_placement_revision_capacity(session, &affected_client_ids)?;

        // The child launches before any state commits. A launch failure
        // registers nothing, moves no view, and rejects the command.
        let new_pane_spec = Self::launch_new_pane_child(
            pty_backend.as_ref(),
            command_args,
            spawn_spec,
            new_pane_id,
            new_pane_target.session_id,
            designated_client_id,
            new_pane_pty_size,
        )?;

        // The child is live — commit all session state through the pure op: it
        // switches the designated client onto the tab (if not already there),
        // registers the pane `Running`, swaps in the split, and focuses it. It
        // returns the previous tab of any client it moved, for the reflow below.
        let (previous_tab_id, mut emitted_events) = pane_ops::commit_new_pane(
            session,
            new_pane_id,
            new_pane_target.tab_id,
            candidate_layout_tree,
            designated_client_id,
            new_pane_spec,
        );
        advance_session_placement_revision(session);
        advance_client_placement_revisions(session, &affected_client_ids);

        // Register the live pane and its size, and create the terminal engine
        // that receives the child's output.
        self.park_pane_pty(new_pane_id, new_pane_pty_size);
        // Announce the new pane's size — PaneCreated carries none.
        emitted_events.push(Event::PtyResized(PtyResized {
            pane_id: new_pane_id,
            pty_size: new_pane_pty_size,
        }));

        // Reflow the target tab's other live panes to the new geometry (excluding
        // the pane just spawned, already sized above).
        self.reflow_changed(
            pty_backend.as_ref(),
            pane_content_rects,
            Some(new_pane_id),
            &mut emitted_events,
        );

        // Adoption moved a client off its previous tab; if that tab still has a
        // viewer, reflow its live panes to the tab size it now sizes against. A
        // tab left with no viewer has no tab size and keeps its sizes.
        if let Some(previous_tab_id) = previous_tab_id {
            self.reflow_tab_if_viewed(
                pty_backend.as_ref(),
                new_pane_target.session_id,
                previous_tab_id,
                &mut emitted_events,
            );
        }

        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            emitted_events,
        ))
    }

    /// The spawn specification a [`Command::NewPane`] launches: an explicit
    /// spawn specification keeps its own program and fills its working
    /// directory from `command_args.working_directory` when it names none; no
    /// spawn specification runs the configured default shell in
    /// `command_args.working_directory`. Either way it carries koshi's
    /// terminal identity, with an explicit command's own environment variables
    /// winning over it.
    fn resolve_new_pane_spawn_spec(&self, command_args: &NewPaneArgs) -> SpawnSpec {
        match &command_args.spawn_spec {
            Some(requested_spawn_spec) => {
                let mut resolved_spawn_spec = requested_spawn_spec.clone();
                if resolved_spawn_spec.working_directory.is_none() {
                    resolved_spawn_spec.working_directory = command_args.working_directory.clone();
                }
                resolved_spawn_spec.environment_variables = self
                    .apply_terminal_identity_environment_variables(
                        resolved_spawn_spec.environment_variables,
                    );
                resolved_spawn_spec
            }
            None => self
                .build_default_shell_spec(command_args.working_directory.clone(), BTreeMap::new()),
        }
    }

    /// Launch the child of a new pane, and return the record the session
    /// keeps of it.
    ///
    /// The record holds the directory the child launches in, and `spawn_spec`
    /// itself when `command_args` names a spawn specification; it keeps the
    /// caller's own environment variables. The launched child also receives
    /// koshi's in-session identity variables for `session_id`,
    /// `designated_client_id` and `new_pane_id`. A launch failure is
    /// [`Self::spawn_child`]'s rejection.
    fn launch_new_pane_child(
        pty_backend: &dyn PtyBackend,
        command_args: &NewPaneArgs,
        mut spawn_spec: SpawnSpec,
        new_pane_id: PaneId,
        session_id: SessionId,
        designated_client_id: Option<ClientId>,
        pty_size: PtySize,
    ) -> Result<NewPaneSpec, Rejection> {
        let new_pane_spec = NewPaneSpec {
            working_directory: spawn_spec.working_directory.clone(),
            spawn_spec: command_args
                .spawn_spec
                .is_some()
                .then(|| spawn_spec.clone()),
        };
        spawn_spec
            .environment_variables
            .extend(build_koshi_environment(
                session_id,
                designated_client_id,
                new_pane_id,
                koshi_paths::resolve_runtime_directory().as_deref(),
            ));
        Self::spawn_child(pty_backend, new_pane_id, spawn_spec, pty_size)?;
        Ok(new_pane_spec)
    }

    /// Handle [`Command::NewPane`] with a [`NewPanePlacement::Floating`]
    /// placement: add a floating pane to the session, in launch-then-commit
    /// order. The target comes from [`Self::resolve_new_floating_pane_target`].
    ///
    /// The child spawns at the target's PTY size, in the working directory the
    /// command names, else the working directory of the target's
    /// `working_directory_pane_id`. Only then does
    /// [`pane_ops::commit_new_floating_pane`] append the member, register the
    /// pane `Running`, and store the designated client's position. A launch
    /// failure commits nothing and rejects. No client's focus moves.
    ///
    /// The session's placement revision advances, and so does the designated
    /// client's when its view of the pane is stored. Emits
    /// [`Event::PaneCreated`] with `tab_id: None`, then [`Event::PtyResized`]
    /// with the spawn size.
    fn handle_new_floating_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &NewPaneArgs,
        size: Option<FloatingPaneSize>,
        at: Option<Point>,
        is_pinned: bool,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let new_floating_pane_target = self.resolve_new_floating_pane_target(
            command_args.client_id,
            size,
            at,
            is_pinned,
            command_source,
            acting_session,
        )?;
        let session_id = new_floating_pane_target.session_id;
        let pty_backend = Arc::clone(self.get_pty_backend());
        let mut spawn_spec = self.resolve_new_pane_spawn_spec(command_args);
        if spawn_spec.working_directory.is_none() {
            if let Some(working_directory_pane_id) =
                new_floating_pane_target.working_directory_pane_id
            {
                spawn_spec.working_directory =
                    self.resolve_pane_working_directory(session_id, working_directory_pane_id);
            }
        }
        let designated_view = new_floating_pane_target
            .designated_client_id
            .map(|client_id| (client_id, new_floating_pane_target.designated_position));
        let affected_client_ids: Vec<ClientId> = designated_view
            .filter(|(_, position)| *position != FloatingPanePosition::Default)
            .map(|(client_id, _)| client_id)
            .into_iter()
            .collect();

        let session = self
            .session_by_id
            .get_mut(&session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        ensure_session_placement_revision_capacity(session)?;
        ensure_client_placement_revision_capacity(session, &affected_client_ids)?;

        let new_pane_id = PaneId::new();
        let new_pane_spec = Self::launch_new_pane_child(
            pty_backend.as_ref(),
            command_args,
            spawn_spec,
            new_pane_id,
            session_id,
            new_floating_pane_target.designated_client_id,
            new_floating_pane_target.pty_size,
        )?;

        let floating_member = FloatingMember {
            pane_id: new_pane_id,
            desired_size: new_floating_pane_target.desired_size,
            solved_size: new_floating_pane_target.solved_size,
        };
        let mut emitted_events = match pane_ops::commit_new_floating_pane(
            session,
            floating_member,
            designated_view,
            new_pane_spec,
        ) {
            Ok(emitted_events) => emitted_events,
            Err(floating_set_error) => {
                let _ = pty_backend.kill_pane(new_pane_id, KillPolicy::Force);
                return Err(Rejection::from_reason_and_help(
                    RejectReason::InvalidState,
                    &floating_set_error.to_string(),
                ));
            }
        };
        advance_session_placement_revision(session);
        advance_client_placement_revisions(session, &affected_client_ids);

        self.park_pane_pty(new_pane_id, new_floating_pane_target.pty_size);
        emitted_events.push(Event::PtyResized(PtyResized {
            pane_id: new_pane_id,
            pty_size: new_floating_pane_target.pty_size,
        }));
        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            emitted_events,
        ))
    }

    /// Handle [`Command::ClosePane`]: tear the pane out of its session and
    /// kill its child, in commit-then-kill order — the state removal is
    /// authoritative and immediate, the process kill is best-effort and
    /// off-thread.
    ///
    /// The pane's close policy picks how the child dies: `--force` overrides
    /// it with an immediate force-kill, `Graceful` requests a stop and
    /// escalates after its grace window — or at once when the stop request
    /// cannot be delivered — and `ConfirmIfBusy` proceeds only for
    /// a pane whose child already exited, rejecting otherwise with a hint at
    /// `--force`. The removal itself is the shared cascade behind shell-exit
    /// and user close: registry drop, layout collapse, per-client focus
    /// repair, and — when the last pane of the last tab goes — tab close and
    /// session quit. The kill runs on a detached thread; a graceful kill can
    /// sleep out its grace window, and the dispatcher thread keeps serving
    /// through it.
    ///
    /// After the removal the survivors reflow: the tab re-solves against its
    /// tab size and each live PTY whose size changed is resized, one
    /// [`Event::PtyResized`] per applied resize, in layout order. When the
    /// close emptied the tab, the nearest surviving tab its viewers moved to
    /// reflows instead. A tab with no viewer has no tab size and keeps its
    /// sizes.
    ///
    /// A floating pane goes to [`Self::handle_close_floating_pane`]. A close
    /// that quits the session also ends every floating pane, each under its
    /// own close policy ([`Self::end_removed_floating_panes`]).
    pub(super) fn handle_close_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &ClosePaneArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let pane_target =
            self.resolve_pane_target(command_args.pane_id, command_source, acting_session)?;
        let Some(tab_id) = pane_target.tab_id else {
            return self.handle_close_floating_pane(
                command_id,
                command_args,
                pane_target.session_id,
                pane_target.pane_id,
            );
        };

        // The shared backend is cloned before the session is borrowed; the
        // kill thread takes its own handle.
        let pty_backend = Arc::clone(self.get_pty_backend());
        let pane_sizing = self.get_pane_sizing();

        let session = self
            .session_by_id
            .get_mut(&pane_target.session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        let pane_record = session
            .panes
            .get_pane_record_by_id(pane_target.pane_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;

        let kill_policy = Self::resolve_pane_kill_policy(
            pane_record,
            command_args.should_force_close,
            command_args.should_kill_process_tree,
            "pane may be busy; pass --force to close anyway",
        )?;

        // The tab is solved against `compute_close_tab_size`; focus candidates
        // rank by geometry also when no client views the tab.
        let tab_rect = Rect::from_size_at_origin(Self::compute_close_tab_size(session, tab_id));
        let affected_client_ids = list_clients_affected_by_tabs(
            session,
            &list_tabs_affected_by_pane_close(session, tab_id, pane_target.pane_id),
            command_source.get_client_id(),
        );
        let floating_pane_kill_policies = list_floating_pane_kill_policies(session, false, false);
        ensure_session_placement_revision_capacity(session)?;
        ensure_client_placement_revision_capacity(session, &affected_client_ids)?;

        // Closing drops the zoom of the client that closed, and of that client
        // only; it returns to the tiled view. A client zoomed on a pane that
        // survives keeps its zoom. The cascade drops the zoom of every client
        // zoomed on the removed pane. A pane that closes on its own, such as a
        // shell that exits, drops no other zoom.
        if let Some(client) = command_source
            .get_client_id()
            .and_then(|client_id| session.clients.get_client_mut_by_id(client_id))
        {
            client.clear_zoom(tab_id);
        }

        // Commit the state removal: registry drop, layout collapse, per-client
        // focus repair, empty-tab close, last-tab quit — one shared cascade.
        let mut emitted_events = remove_pane_cascade(
            session,
            tab_id,
            pane_target.pane_id,
            tab_rect,
            pane_sizing,
            None,
        );
        advance_session_placement_revision(session);
        advance_client_placement_revisions(session, &affected_client_ids);

        // The pane is gone from state; drop its runtime bookkeeping and reflow
        // the survivors into the space it freed.
        self.release_pane_and_reflow(
            pane_target.session_id,
            tab_id,
            pane_target.pane_id,
            pty_backend.as_ref(),
            &mut emitted_events,
        );

        super::kill_off_thread(&pty_backend, pane_target.pane_id, kill_policy);
        self.end_removed_floating_panes(
            pane_target.session_id,
            &pty_backend,
            floating_pane_kill_policies,
        );

        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            emitted_events,
        ))
    }

    /// Pick how a pane's child dies. `should_force_close` overrides the pane's own policy
    /// with an immediate force-kill; `ConfirmIfBusy` allows the close only for
    /// a pane whose child provably ended (`Exited`) and otherwise rejects with
    /// `busy_hint`. `should_kill_process_tree` widens the picked kill to the child's whole process
    /// group.
    pub(super) fn resolve_pane_kill_policy(
        pane_record: &PaneRecord,
        should_force_close: bool,
        should_kill_process_tree: bool,
        busy_hint: &str,
    ) -> Result<KillPolicy, Rejection> {
        if !should_force_close
            && matches!(pane_record.close_policy, PaneClosePolicy::ConfirmIfBusy)
            && !matches!(pane_record.get_lifecycle(), PaneLifecycle::Exited { .. })
        {
            return Err(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                busy_hint,
            ));
        }
        Ok(compute_pane_kill_policy(
            pane_record,
            should_force_close,
            should_kill_process_tree,
        ))
    }

    /// Handle [`Command::ClosePane`] on a floating pane: remove it from the
    /// session and kill its child, in commit-then-kill order.
    ///
    /// The kill policy is picked as for a tiled pane
    /// ([`Self::resolve_pane_kill_policy`]). [`remove_floating_pane`] drops
    /// the registry record, the member and every client's view of the pane;
    /// no layout changes and nothing reflows. The session's placement
    /// revision advances, and so does the revision of every client whose
    /// floating view named the pane. Emits [`Event::PaneClosing`] and
    /// [`Event::PaneRemoved`] with `tab_id: None`.
    fn handle_close_floating_pane(
        &mut self,
        command_id: CommandId,
        command_args: &ClosePaneArgs,
        session_id: SessionId,
        pane_id: PaneId,
    ) -> Result<CommandResult, Rejection> {
        let pty_backend = Arc::clone(self.get_pty_backend());
        let session = self
            .session_by_id
            .get_mut(&session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        let pane_record = session
            .panes
            .get_pane_record_by_id(pane_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        let kill_policy = Self::resolve_pane_kill_policy(
            pane_record,
            command_args.should_force_close,
            command_args.should_kill_process_tree,
            "pane may be busy; pass --force to close anyway",
        )?;
        let affected_client_ids = list_clients_holding_floating_view(session, pane_id);
        ensure_session_placement_revision_capacity(session)?;
        ensure_client_placement_revision_capacity(session, &affected_client_ids)?;

        let emitted_events = remove_floating_pane(session, pane_id);
        advance_session_placement_revision(session);
        advance_client_placement_revisions(session, &affected_client_ids);

        self.release_pane_bookkeeping(session_id, pane_id);
        super::kill_off_thread(&pty_backend, pane_id, kill_policy);
        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            emitted_events,
        ))
    }

    /// End each floating pane `floating_pane_kill_policies` names that the
    /// session `session_id` does not hold: release its runtime bookkeeping
    /// ([`Self::release_pane_bookkeeping`]), then kill its child on a thread of
    /// its own under its listed policy. A last-tab quit removes every floating
    /// pane; a removal that quit nothing leaves every floating pane in place,
    /// and this ends none.
    pub(super) fn end_removed_floating_panes(
        &mut self,
        session_id: SessionId,
        pty_backend: &Arc<dyn PtyBackend>,
        floating_pane_kill_policies: Vec<(PaneId, KillPolicy)>,
    ) {
        for (pane_id, kill_policy) in floating_pane_kill_policies {
            let is_member = self
                .session_by_id
                .get(&session_id)
                .is_some_and(|session| session.floating_set.has_pane(pane_id));
            if is_member {
                continue;
            }
            self.release_pane_bookkeeping(session_id, pane_id);
            super::kill_off_thread(pty_backend, pane_id, kill_policy);
        }
    }

    /// Remove a pane's live ID, cached size, and terminal engine. Clear each
    /// attached client's scroll offset and selection for it. Every pane
    /// removal calls this for runtime bookkeeping.
    pub(super) fn release_pane_bookkeeping(&mut self, session_id: SessionId, pane_id: PaneId) {
        self.live_pane_ids.remove(&pane_id);
        self.pty_size_by_pane_id.remove(&pane_id);
        self.terminal_engine_by_pane_id.remove(&pane_id);
        let Some(session) = self.session_by_id.get_mut(&session_id) else {
            return;
        };
        for client in session.clients.list_attached_clients_mut() {
            client.set_scroll_offset(pane_id, 0);
            client.clear_selection(pane_id);
        }
    }

    /// Drop a removed pane's runtime bookkeeping and reflow the survivors into
    /// the space it freed.
    ///
    /// Removes the pane's live ID, size cache, and terminal engine — output
    /// bytes still in flight for it now find no engine and are dropped — and
    /// clears each client's scroll offset and any highlight in it, then
    /// re-solves and resizes the tab that reclaims the space: the pane's own tab
    /// when it survives, else the tab its viewers moved to (the cascade's
    /// `TabFocused`). A tab left with no viewer has no tab size and keeps its
    /// sizes. Each applied resize appends one [`Event::PtyResized`] to `emitted_events`.
    ///
    /// Shared by [`handle_close_pane`](Self::handle_close_pane) and
    /// [`handle_child_exit`](Self::handle_child_exit). Both callers release any
    /// backend entry after reflow. Child-exit also invalidates rendering.
    fn release_pane_and_reflow(
        &mut self,
        session_id: SessionId,
        tab_id: TabId,
        pane_id: PaneId,
        pty_backend: &dyn PtyBackend,
        emitted_events: &mut Vec<Event>,
    ) {
        self.release_pane_bookkeeping(session_id, pane_id);

        let is_tab_present = self
            .session_by_id
            .get(&session_id)
            .is_some_and(|session| session.tabs.contains_key(&tab_id));
        let reflow_tab_id = if is_tab_present {
            Some(tab_id)
        } else {
            find_first_focused_tab_id(emitted_events)
        };
        if let Some(reflow_tab_id) = reflow_tab_id {
            self.reflow_tab_if_viewed(pty_backend, session_id, reflow_tab_id, emitted_events);
        }
    }

    /// Remove a pane after a child-exit event or when resume finds it absent
    /// from the backend, and return the resulting domain events.
    ///
    /// A local backend's watcher reaps a child before reporting its exit.
    /// Resume can also report a carried pane that the backend does not drive.
    /// [`apply_child_exit`] removes a tiled pane — its tab may close and the
    /// last tab quit, which also ends every floating pane — and its runtime
    /// bookkeeping is released while the survivors reflow. A floating pane is
    /// removed at once through [`remove_floating_pane`], and nothing reflows.
    /// An exit for a pane already gone — closed while the exit waited in the
    /// inbox — is dropped.
    ///
    /// Releasing a removed pane's bookkeeping clears its live ID, size cache,
    /// and terminal engine. If the backend still holds the pane, `kill_pane`
    /// removes its entry. A local backend drops the writer, joins the watcher,
    /// and closes the terminal master; a supervisor backend sends a kill request
    /// to the helper. An absent pane returns `UnknownPane` without a kill
    /// request. A reaped local child receives no leader signal.
    pub fn handle_child_exit(&mut self, pane_id: PaneId, exit_status: ExitStatus) -> Vec<Event> {
        // Exactly one of `exit_code` and `signal` is `Some`.
        let pane_exit = match exit_status {
            ExitStatus::ExitCode(exit_code) => PaneProcessExited {
                pane_id,
                exit_code: Some(exit_code),
                signal: None,
            },
            ExitStatus::Signaled(signal) => PaneProcessExited {
                pane_id,
                exit_code: None,
                signal: Some(signal),
            },
        };

        // Find the session that owns the pane. An exit for a pane already gone
        // (closed while the exit waited in the inbox) is dropped.
        let Some(session_id) = self
            .get_session_for_pane(pane_id)
            .map(|session| session.session_id)
        else {
            return Vec::new();
        };

        // The shared backend is cloned before the session is borrowed; the
        // pane's PTY entry is released through the clone.
        let pty_backend = Arc::clone(self.get_pty_backend());
        let pane_sizing = self.get_pane_sizing();

        let session = self
            .session_by_id
            .get_mut(&session_id)
            .expect("session located above");
        let tab_id = match Self::resolve_pane_tab_id(session, pane_id) {
            Ok(Some(tab_id)) => tab_id,
            // A floating pane leaves at once: `PaneProcessExited`, then its
            // removal from the registry, the floating set and every client's
            // view. No layout collapses and no tab closes.
            Ok(None) => {
                let affected_client_ids = list_clients_holding_floating_view(session, pane_id);
                let mut emitted_events = vec![Event::PaneProcessExited(pane_exit)];
                emitted_events.extend(remove_floating_pane(session, pane_id));
                advance_session_placement_revision(session);
                advance_client_placement_revisions(session, &affected_client_ids);
                self.release_pane_bookkeeping(session_id, pane_id);
                let _ = pty_backend.kill_pane(pane_id, KillPolicy::Force);
                self.render_scheduler.invalidate();
                return emitted_events;
            }
            // No tab's layout holds the pane and it does not float: its record
            // is an `OrphanedPaneRecord`. The exit is dropped.
            Err(_) => return Vec::new(),
        };

        // The tab is solved against `compute_close_tab_size`; focus repair
        // ranks candidates by geometry also when no client views the tab.
        let tab_rect = Rect::from_size_at_origin(Self::compute_close_tab_size(session, tab_id));
        let affected_client_ids = list_clients_affected_by_tabs(
            session,
            &list_tabs_affected_by_pane_close(session, tab_id, pane_id),
            None,
        );
        let floating_pane_kill_policies = list_floating_pane_kill_policies(session, false, false);
        // `PaneProcessExited`, then the removal cascade.
        let mut emitted_events =
            apply_child_exit(session, tab_id, pane_exit, tab_rect, pane_sizing);
        advance_session_placement_revision(session);
        advance_client_placement_revisions(session, &affected_client_ids);

        // Drop the removed pane's runtime bookkeeping and reflow the survivors
        // into the space it freed.
        self.release_pane_and_reflow(
            session_id,
            tab_id,
            pane_id,
            pty_backend.as_ref(),
            &mut emitted_events,
        );

        // Release any backend entry still held for this pane. A local backend
        // sends no leader signal after its watcher reaps the child. An undriven
        // carried pane has no backend entry and returns UnknownPane.
        let _ = pty_backend.kill_pane(pane_id, KillPolicy::Force);
        self.end_removed_floating_panes(session_id, &pty_backend, floating_pane_kill_policies);

        self.render_scheduler.invalidate();

        emitted_events
    }

    /// Move one border of a pane by an exact signed cell count, then resize
    /// the affected PTYs.
    ///
    /// The border that moves is resolved by the layout crate's resize
    /// transaction: a positive `command_args.resize_amount_cells` grows the pane toward
    /// `command_args.direction` with the adjacent sibling donating the cells, a
    /// negative one shrinks it with that sibling gaining them. A pane with no
    /// border on the named side — it touches the tab edge there — moves its
    /// opposite border in the same visual direction instead. The target pane's
    /// tab must be viewed by at least one attached client: the tab is solved
    /// against that tab size ([`Session::get_tab_size`]), and the donating
    /// side's spare cells are measured against it. A tab no viewer contributes
    /// a pane area to rejects.
    /// On success the tab's tree is swapped in, the resizing client's zoom
    /// drops, any other client's zoom stands, [`Event::LayoutChanged`] is
    /// emitted, and every live PTY whose solved size changed is resized through
    /// the shared reflow path, one [`Event::PtyResized`] each.
    ///
    /// A floating pane has no sibling: it resizes through
    /// [`Self::resolve_floating_pane_resize`] and
    /// [`Self::handle_resize_floating_pane`].
    pub(super) fn handle_resize_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &ResizePaneArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let pane_target =
            self.resolve_pane_target(command_args.pane_id, command_source, acting_session)?;
        let Some(tab_id) = pane_target.tab_id else {
            let floating_pane_resize =
                self.resolve_floating_pane_resize(command_args, command_source, &pane_target)?;
            return self.handle_resize_floating_pane(command_id, floating_pane_resize);
        };

        let pty_backend = Arc::clone(self.get_pty_backend());

        let pane_sizing = self.get_pane_sizing();
        let (session, tab_size) =
            self.resolve_session_and_tab_size(pane_target.session_id, tab_id)?;
        let tab_rect = Rect::from_size_at_origin(tab_size);
        let tab_state = session
            .tabs
            .get(&tab_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;

        // The resize transaction returns a new tree and leaves the tab's
        // untouched on rejection. When the pane touches the tab edge on the
        // named side, the opposite border moves in the same visual direction.
        let resized_layout_tree = resize_layout_with_sizing(
            tab_state.get_layout_tree(),
            tab_rect,
            pane_target.pane_id,
            command_args.direction,
            command_args.resize_amount_cells,
            pane_sizing,
        )
        .or_else(|resize_error| match resize_error {
            ResizeError::NoAdjacentBorder { .. } => resize_layout_with_sizing(
                tab_state.get_layout_tree(),
                tab_rect,
                pane_target.pane_id,
                command_args.direction.compute_opposite_direction(),
                command_args.resize_amount_cells.saturating_neg(),
                pane_sizing,
            ),
            other_resize_error => Err(other_resize_error),
        })
        .map_err(|resize_error| Self::resize_rejection(&resize_error))?;
        let affected_client_ids =
            list_clients_affected_by_tabs(session, &[tab_id], command_source.get_client_id());
        ensure_session_placement_revision_capacity(session)?;
        ensure_client_placement_revision_capacity(session, &affected_client_ids)?;

        let tab_state = session
            .tabs
            .get_mut(&tab_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        tab_state.update_layout(resized_layout_tree);

        // Resizing drops the zoom of the client that resized, and of that client
        // only; it returns to the tiled view and sees the moved border. Another
        // client zoomed on a pane of this tab keeps its zoom.
        if let Some(client) = command_source
            .get_client_id()
            .and_then(|client_id| session.clients.get_client_mut_by_id(client_id))
        {
            client.clear_zoom(tab_id);
        }
        advance_session_placement_revision(session);
        advance_client_placement_revisions(session, &affected_client_ids);

        // The border moved: re-solve the tab and resize each live PTY whose
        // size changed.
        let mut emitted_events = vec![Event::LayoutChanged(LayoutChanged { tab_id })];
        self.reflow_tab_if_viewed(
            pty_backend.as_ref(),
            pane_target.session_id,
            tab_id,
            &mut emitted_events,
        );

        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            emitted_events,
        ))
    }

    /// Apply a floating pane resize that
    /// [`Self::resolve_floating_pane_resize`] resolved: store the pane's
    /// desired size and the acting client's top-left cell — pinned again at
    /// that cell for a client that pinned the pane — then re-solve every
    /// floating pane and resize each PTY whose size changed
    /// ([`Self::reflow_floating_panes`]). No other client's view changes.
    ///
    /// The session's placement revision advances, and so does the acting
    /// client's when its stored position changed. Emits one
    /// [`Event::PtyResized`] per resized PTY.
    fn handle_resize_floating_pane(
        &mut self,
        command_id: CommandId,
        floating_pane_resize: FloatingPaneResize,
    ) -> Result<CommandResult, Rejection> {
        let FloatingPaneResize {
            session_id,
            pane_id,
            client_id,
            desired_size,
            client_origin,
        } = floating_pane_resize;
        let pty_backend = Arc::clone(self.get_pty_backend());
        let session = self
            .session_by_id
            .get_mut(&session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        let previous_position = Self::require_client(session, client_id)?
            .get_floating_pane_view(pane_id)
            .position;
        let acting_client_ids: Vec<ClientId> =
            client_origin.map(|_| client_id).into_iter().collect();
        ensure_session_placement_revision_capacity(session)?;
        ensure_client_placement_revision_capacity(session, &acting_client_ids)?;

        session
            .floating_set
            .update_member_desired_size(pane_id, desired_size);
        advance_session_placement_revision(session);
        if let (Some(client_origin), Some(client)) = (
            client_origin,
            session.clients.get_client_mut_by_id(client_id),
        ) {
            match previous_position {
                FloatingPanePosition::Pinned(_) => client.pin_floating_pane(pane_id, client_origin),
                FloatingPanePosition::Default | FloatingPanePosition::Moved(_) => {
                    let _ = client.set_floating_pane_position(pane_id, client_origin);
                }
            }
            if client.get_floating_pane_view(pane_id).position != previous_position {
                let _ = client.advance_placement_revision();
            }
        }

        let mut emitted_events = Vec::new();
        self.reflow_floating_panes(pty_backend.as_ref(), session_id, &mut emitted_events);
        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            emitted_events,
        ))
    }

    /// Handle [`Command::MovePane`]: swap the selected pane with its visible
    /// neighbor in `command_args.direction` and commit the swap at once.
    ///
    /// - The neighbor comes from the issuing client's solved layout.
    /// - The swap commits through the same path as a same-tab
    ///   [`Command::PlacePane`] swap: it clears the issuing client's zoom on
    ///   the tab, emits `PanePlacementCommitted` then `LayoutChanged`, and
    ///   reflows the tab's live PTYs.
    pub(super) fn handle_move_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &MovePaneArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let pane_target =
            self.resolve_move_pane_target(command_args, command_source, acting_session)?;
        let target_pane_id = Self::find_directional_neighbor(
            self.session_by_id
                .get(&pane_target.session_id)
                .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?,
            pane_target.client_id,
            pane_target.pane_id,
            command_args.direction,
            self.get_pane_sizing(),
        )?;
        let place_pane_args = PlacePaneArgs {
            source_pane_id: pane_target.pane_id,
            placement_target: PanePlacementTarget::Swap { target_pane_id },
            expected_placement_revision: None,
        };
        let placement_target = PlacePaneTarget {
            session_id: pane_target.session_id,
            source_tab_id: pane_target.tab_id,
            destination_tab_id: pane_target.tab_id,
            client_id: pane_target.client_id,
        };
        self.apply_place_pane_within_tab(command_id, &place_pane_args, placement_target)
    }

    /// Handle [`Command::PlacePane`]: install one prepared tiled placement
    /// within one tab or across two tabs, then reflow every tab whose viewers
    /// change. Every fallible layout and revision check runs before the session
    /// commit.
    pub(super) fn apply_place_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &PlacePaneArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let placement_target =
            self.resolve_place_pane_target(command_args, command_source, acting_session)?;
        if placement_target.source_tab_id == placement_target.destination_tab_id {
            return self.apply_place_pane_within_tab(command_id, command_args, placement_target);
        }
        let pane_sizing = self.get_pane_sizing();
        let session = self
            .session_by_id
            .get(&placement_target.session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetGone))?;
        let source_tree = session
            .tabs
            .get(&placement_target.source_tab_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetGone))?
            .get_layout_tree();
        let is_source_tab_closing = matches!(
            &command_args.placement_target,
            PanePlacementTarget::Split { .. }
        ) && source_tree.list_leaf_pane_ids()
            == vec![command_args.source_pane_id];
        let landing_tab_id = if is_source_tab_closing {
            Some(
                tab_ops::find_nearest_surviving_tab(session, placement_target.source_tab_id)
                    .ok_or_else(|| Rejection::from_reason(RejectReason::TargetGone))?,
            )
        } else {
            None
        };
        let mut affected_tab_ids = vec![
            placement_target.source_tab_id,
            placement_target.destination_tab_id,
        ];
        if let Some(landing_tab_id) = landing_tab_id {
            if !affected_tab_ids.contains(&landing_tab_id) {
                affected_tab_ids.push(landing_tab_id);
            }
        }
        let affected_client_ids = list_clients_affected_by_tabs(
            session,
            &affected_tab_ids,
            Some(placement_target.client_id),
        );
        let destination_tab_size = compute_placement_destination_tab_size(
            session,
            placement_target.source_tab_id,
            placement_target.destination_tab_id,
            landing_tab_id,
            placement_target.client_id,
        )?;
        let destination_tab_rect = Rect::from_size_at_origin(destination_tab_size);
        let layout_target = match &command_args.placement_target {
            PanePlacementTarget::Swap { target_pane_id } => PlacementTarget::Swap {
                target_pane_id: *target_pane_id,
            },
            PanePlacementTarget::Split {
                anchor, direction, ..
            } => PlacementTarget::Insert {
                anchor: anchor.clone(),
                direction: *direction,
            },
        };
        let destination_tree = session
            .tabs
            .get(&placement_target.destination_tab_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetGone))?
            .get_layout_tree();
        let prepared_placement = koshi_layout::placement::place_pane_across_tabs(
            source_tree,
            command_args.source_pane_id,
            destination_tree,
            &layout_target,
            destination_tab_rect,
            pane_sizing,
        )
        .map_err(|placement_error| Self::build_placement_rejection(&placement_error))?;
        if let Some(placed_source_tree) = prepared_placement.source_tree.as_ref() {
            if let Some(source_tab_size) = compute_placement_source_tab_size(
                session,
                placement_target.source_tab_id,
                placement_target.client_id,
            ) {
                let source_tab_rect = Rect::from_size_at_origin(source_tab_size);
                if !is_layout_within_rect(placed_source_tree, source_tab_rect, pane_sizing) {
                    return Err(Rejection::from_reason_and_help(
                        RejectReason::InvalidState,
                        "pane placement does not fit the source tab",
                    ));
                }
            }
        }
        ensure_session_placement_revision_capacity(session)?;
        ensure_client_placement_revision_capacity(session, &affected_client_ids)?;

        let pty_backend = Arc::clone(self.get_pty_backend());
        let mut emitted_events = {
            let session = self
                .session_by_id
                .get_mut(&placement_target.session_id)
                .ok_or_else(|| Rejection::from_reason(RejectReason::TargetGone))?;
            let emitted_events = commit_cross_tab_placement(
                session,
                placement_target.source_tab_id,
                placement_target.destination_tab_id,
                command_args.source_pane_id,
                prepared_placement,
                placement_target.client_id,
            )
            .map_err(|_| Rejection::from_reason(RejectReason::InvalidState))?;
            advance_session_placement_revision(session);
            advance_client_placement_revisions(session, &affected_client_ids);
            emitted_events
        };
        emitted_events.insert(
            0,
            Event::PanePlacementCommitted(PanePlacementCommitted {
                command_id,
                source_pane_id: command_args.source_pane_id,
                source_tab_id: Some(placement_target.source_tab_id),
                destination_tab_id: Some(placement_target.destination_tab_id),
                placement_target: command_args.placement_target.clone(),
            }),
        );

        let mut tab_ids_to_reflow = vec![placement_target.destination_tab_id];
        if let Some(landing_tab_id) = landing_tab_id {
            if !tab_ids_to_reflow.contains(&landing_tab_id) {
                tab_ids_to_reflow.push(landing_tab_id);
            }
        }
        if !is_source_tab_closing && !tab_ids_to_reflow.contains(&placement_target.source_tab_id) {
            tab_ids_to_reflow.push(placement_target.source_tab_id);
        }
        for tab_id in tab_ids_to_reflow {
            self.reflow_tab_if_viewed(
                pty_backend.as_ref(),
                placement_target.session_id,
                tab_id,
                &mut emitted_events,
            );
        }

        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            emitted_events,
        ))
    }

    /// Apply a checked swap or insertion within one tab. A swap whose target is
    /// the source pane commits no events and does not solve or resize the tab.
    fn apply_place_pane_within_tab(
        &mut self,
        command_id: CommandId,
        command_args: &PlacePaneArgs,
        placement_target: PlacePaneTarget,
    ) -> Result<CommandResult, Rejection> {
        let layout_target = match &command_args.placement_target {
            PanePlacementTarget::Swap { target_pane_id } => {
                if *target_pane_id == command_args.source_pane_id {
                    return Ok(TransactionScope::new().commit(command_id, &mut self.event_bus));
                }
                PlacementTarget::Swap {
                    target_pane_id: *target_pane_id,
                }
            }
            PanePlacementTarget::Split {
                anchor, direction, ..
            } => PlacementTarget::Insert {
                anchor: anchor.clone(),
                direction: *direction,
            },
        };
        let pane_sizing = self.get_pane_sizing();
        let pty_backend = Arc::clone(self.get_pty_backend());
        let (session, tab_size) = self.resolve_session_and_tab_size(
            placement_target.session_id,
            placement_target.source_tab_id,
        )?;
        let tab_rect = Rect::from_size_at_origin(tab_size);
        let tab_state = session
            .tabs
            .get(&placement_target.source_tab_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        let placed_layout_tree = place_pane_within_tab(
            tab_state.get_layout_tree(),
            command_args.source_pane_id,
            &layout_target,
            tab_rect,
            pane_sizing,
        )
        .map_err(|placement_error| Self::build_placement_rejection(&placement_error))?;
        let affected_client_ids = list_clients_affected_by_tabs(
            session,
            &[placement_target.source_tab_id],
            Some(placement_target.client_id),
        );
        ensure_session_placement_revision_capacity(session)?;
        ensure_client_placement_revision_capacity(session, &affected_client_ids)?;
        let tab_state = session
            .tabs
            .get_mut(&placement_target.source_tab_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        tab_state.update_layout(placed_layout_tree);
        if let Some(client) = session
            .clients
            .get_client_mut_by_id(placement_target.client_id)
        {
            client.clear_zoom(placement_target.source_tab_id);
        }
        advance_session_placement_revision(session);
        advance_client_placement_revisions(session, &affected_client_ids);
        let mut emitted_events = vec![
            Event::PanePlacementCommitted(PanePlacementCommitted {
                command_id,
                source_pane_id: command_args.source_pane_id,
                source_tab_id: Some(placement_target.source_tab_id),
                destination_tab_id: Some(placement_target.destination_tab_id),
                placement_target: command_args.placement_target.clone(),
            }),
            Event::LayoutChanged(LayoutChanged {
                tab_id: placement_target.source_tab_id,
            }),
        ];
        self.reflow_tab_if_viewed(
            pty_backend.as_ref(),
            placement_target.session_id,
            placement_target.source_tab_id,
            &mut emitted_events,
        );
        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            emitted_events,
        ))
    }

    /// Handle [`Command::ScrollPane`]: move one client's view of one pane
    /// through scrollback without changing layout or PTY sizes.
    pub(super) fn handle_scroll_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &ScrollPaneArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let pane_target =
            self.resolve_scroll_pane_target(command_args, command_source, acting_session)?;
        let scroll_line_count = command_args.scroll_line_count.unsigned_abs() as usize;
        if command_args.scroll_line_count > 0 {
            self.scroll_up(
                pane_target.client_id,
                pane_target.pane_id,
                scroll_line_count,
            );
        } else if command_args.scroll_line_count < 0 {
            self.scroll_down(
                pane_target.client_id,
                pane_target.pane_id,
                scroll_line_count,
            );
        }
        Ok(TransactionScope::new().commit(command_id, &mut self.event_bus))
    }

    /// Map a placement failure to a command rejection: a missing source or
    /// target pane is [`RejectReason::TargetNotFound`], a destination too small
    /// is [`RejectReason::InvalidState`] `pane placement does not fit the tab`,
    /// and every other failure is [`RejectReason::InvalidState`] with no help.
    fn build_placement_rejection(placement_error: &PlacementError) -> Rejection {
        match placement_error {
            PlacementError::SourcePaneNotFound { .. }
            | PlacementError::TargetPaneNotFound { .. } => {
                Rejection::from_reason(RejectReason::TargetNotFound)
            }
            PlacementError::DestinationTooSmall { .. } => Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                "pane placement does not fit the tab",
            ),
            PlacementError::AnchorIsSource { .. }
            | PlacementError::GroupPaneDuplicated { .. }
            | PlacementError::GroupIsNotOneSubtree { .. }
            | PlacementError::AnchorInsideStack { .. }
            | PlacementError::PaneInBothTrees { .. } => {
                Rejection::from_reason(RejectReason::InvalidState)
            }
        }
    }

    /// Map a layout [`ResizeError`] onto the command vocabulary's rejection:
    /// a missing pane is [`RejectReason::TargetNotFound`], a pane with no
    /// neighbor on the requested side is [`RejectReason::InvalidState`], and
    /// a donor below its floor is [`RejectReason::MinimumSize`] carrying the spare
    /// cell count in both the hint and the rejection's own field, which the
    /// mouse layer reads to ask again for exactly those cells.
    fn resize_rejection(resize_error: &ResizeError) -> Rejection {
        match resize_error {
            ResizeError::PaneNotFound { .. } => {
                Rejection::from_reason(RejectReason::TargetNotFound)
            }
            ResizeError::NoAdjacentBorder { .. } => Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                "pane has no border to move on that axis",
            ),
            ResizeError::MinimumSizeExceeded {
                spare_cell_count, ..
            } => Rejection::from_minimum_size(*spare_cell_count),
        }
    }

    /// Handle [`Command::FocusPane`]: move the target client's focus to the
    /// target pane in its active tab. The pane comes out of
    /// [`Self::resolve_focus_target`], which takes an id target and rejects a
    /// direction target.
    ///
    /// The pane must be visible on screen: one suppressed for lack of space is
    /// [`RejectReason::InvalidState`]. A collapsed stack member is a valid
    /// target — focusing it activates its stack (the member expands, the
    /// previously active member collapses to a header) and the tab's PTYs
    /// reflow to the new geometry. Zoom follows focus, per client: when the
    /// target client has this tab zoomed, focusing another pane moves its zoom
    /// onto that pane — its zoomed view swaps content and stays on, and no other
    /// client's view moves. Emits [`Event::LayoutChanged`] plus per-pane
    /// [`Event::PtyResized`] when a stack activation or a zoom retarget changed
    /// the geometry, and [`Event::PaneFocused`] when the client's focus actually
    /// moved; focusing the already-focused pane of an already-active member
    /// completes with no events. A rejected focus mutates nothing.
    pub(super) fn handle_focus_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &FocusPaneArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let pane_sizing = self.get_pane_sizing();
        let pane_target =
            Self::resolve_focus_target(command_args, command_source, acting_session, pane_sizing)?;

        let pty_backend = Arc::clone(self.get_pty_backend());

        let (session, tab_size) =
            self.resolve_session_and_tab_size(pane_target.session_id, pane_target.tab_id)?;
        // Zoom follows focus and belongs to this client: a zoomed client that
        // focuses another pane zooms that pane, and every other client's view
        // stays as it was. The mode solved and checked below is this client's.
        let client = session
            .clients
            .get_client_by_id(pane_target.client_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::SourceClientStale))?;
        let previous_focused_pane_id = client.get_focused_pane_id(pane_target.tab_id);
        let client_layout_mode = client.get_layout_mode(pane_target.tab_id);
        let effective_layout_mode = match client_layout_mode {
            LayoutMode::Fullscreen { focused_pane_id }
                if focused_pane_id != pane_target.pane_id =>
            {
                LayoutMode::Fullscreen {
                    focused_pane_id: pane_target.pane_id,
                }
            }
            layout_mode => layout_mode,
        };
        let is_layout_mode_retargeted = effective_layout_mode != client_layout_mode;

        let tab_state = session
            .tabs
            .get_mut(&pane_target.tab_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;

        // Solve the tab as this client will display it: a pane suppressed for
        // lack of space cannot take focus.
        let tab_rect = Rect::from_size_at_origin(tab_size);
        let solved_layout = solve_layout_with_mode(
            tab_state.get_layout_tree(),
            effective_layout_mode,
            tab_rect,
            pane_sizing,
        );
        if solved_layout
            .suppressed_pane_ids
            .contains(&pane_target.pane_id)
        {
            return Err(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                "pane is suppressed; not enough space to show it",
            ));
        }

        // A collapsed stack member is a valid target: focusing it expands it.
        // The activation mutates a candidate tree, swapped in whole.
        let mut candidate_layout_tree = tab_state.get_layout_tree().clone();
        let is_stack_member_activated = candidate_layout_tree
            .find_containing_stack_mut(pane_target.pane_id)
            .is_some_and(|stack| activate_stack_member(stack, pane_target.pane_id));

        if previous_focused_pane_id == Some(pane_target.pane_id)
            && !is_stack_member_activated
            && !is_layout_mode_retargeted
        {
            return Ok(TransactionScope::new().commit(command_id, &mut self.event_bus));
        }

        let client_ids_to_advance = if is_stack_member_activated {
            list_clients_affected_by_tabs(
                session,
                &[pane_target.tab_id],
                Some(pane_target.client_id),
            )
        } else {
            vec![pane_target.client_id]
        };
        if is_stack_member_activated {
            ensure_session_placement_revision_capacity(session)?;
        }
        ensure_client_placement_revision_capacity(session, &client_ids_to_advance)?;
        if is_stack_member_activated {
            let tab_state = session
                .tabs
                .get_mut(&pane_target.tab_id)
                .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
            tab_state.update_layout(candidate_layout_tree);
        }

        // The focus, and this client's zoom with it, moves before the reflow;
        // the reflow solves PTY sizes from what every client now displays.
        let client = session
            .clients
            .get_client_mut_by_id(pane_target.client_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::SourceClientStale))?;
        client.update_focused_pane(pane_target.tab_id, pane_target.pane_id);
        if let Some(tab_state) = session.tabs.get_mut(&pane_target.tab_id) {
            tab_state.record_focus_mru(pane_target.pane_id);
        }
        if is_stack_member_activated {
            advance_session_placement_revision(session);
        }
        advance_client_placement_revisions(session, &client_ids_to_advance);

        // The activation, the zoom retarget, or both changed what is drawn:
        // announce the new geometry and resize each live PTY whose size changed.
        let mut emitted_events = Vec::new();
        if is_stack_member_activated || is_layout_mode_retargeted {
            emitted_events.push(Event::LayoutChanged(LayoutChanged {
                tab_id: pane_target.tab_id,
            }));
            self.reflow_tab_if_viewed(
                pty_backend.as_ref(),
                pane_target.session_id,
                pane_target.tab_id,
                &mut emitted_events,
            );
        }

        if previous_focused_pane_id != Some(pane_target.pane_id) {
            emitted_events.push(Event::PaneFocused(PaneFocused {
                client_id: pane_target.client_id,
                tab_id: Some(pane_target.tab_id),
                pane_id: pane_target.pane_id,
                previous_pane_id: previous_focused_pane_id,
            }));
        }

        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            emitted_events,
        ))
    }

    /// Handle [`Command::TogglePaneFullscreen`]: switch the **target client's**
    /// view of that client's tab between tiled and a zoom of the pane that
    /// client has focused.
    ///
    /// The zoom belongs to that one client. Another client viewing the same tab
    /// keeps its own layout and its own focus, and its keys reach its own pane.
    ///
    /// [`Self::resolve_fullscreen_target`] is the single resolver validation
    /// also used: the client is the one the caller named on the command line
    /// when there is one, else the issuer while it is still attached, else the
    /// session's sole attached client; the pane is that client's focused pane,
    /// or the issuing pane for an in-session CLI command. Example:
    /// `koshi toggle-pane-fullscreen --client <B>` zooms B's focused pane on
    /// B's screen while every other client of that tab stays tiled, and a CLI
    /// command from a pane whose client has gone zooms that pane for the one
    /// client still watching. An already-zoomed client toggles
    /// back to tiled whichever pane resolved; a tiled client zooms the target
    /// and, when its focus was elsewhere, moves its focus to the pane now
    /// filling its view ([`Event::PaneFocused`]). The zoom is a solve-time
    /// overlay: the tree is untouched, and toggling out restores the prior
    /// layout. The tab must be viewed by at least one attached client, and a
    /// tab size too small to show the pane at its content minimum rejects.
    /// Emits [`Event::LayoutChanged`] plus one [`Event::PtyResized`] per PTY
    /// whose solved size changed. A pane another client still draws tiled
    /// keeps the size that client shows. A rejected toggle mutates nothing.
    pub(super) fn handle_toggle_pane_fullscreen(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let pane_sizing = self.get_pane_sizing();
        let pane_target = self.resolve_fullscreen_target(command_source, acting_session)?;
        let client_id = pane_target.client_id;

        let pty_backend = Arc::clone(self.get_pty_backend());

        let tab_id = pane_target.tab_id;
        let (session, tab_size) =
            self.resolve_session_and_tab_size(pane_target.session_id, tab_id)?;
        let client = session
            .clients
            .get_client_by_id(client_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::SourceClientStale))?;
        let client_layout_mode = client.get_layout_mode(tab_id);
        let previous_focused_pane_id = client.get_focused_pane_id(tab_id);

        let tab_state = session
            .tabs
            .get(&tab_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;

        // Flip this client's zoom. Entering solves the zoomed view first: a
        // tab size too small to show the pane at its content minimum rejects
        // before anything mutates.
        let is_zoom_entered = match client_layout_mode {
            LayoutMode::Fullscreen { .. } => false,
            LayoutMode::Tiled => {
                let fullscreen_layout_mode = LayoutMode::Fullscreen {
                    focused_pane_id: pane_target.pane_id,
                };
                let tab_rect = Rect::from_size_at_origin(tab_size);
                let solved_layout = solve_layout_with_mode(
                    tab_state.get_layout_tree(),
                    fullscreen_layout_mode,
                    tab_rect,
                    pane_sizing,
                );
                if solved_layout
                    .suppressed_pane_ids
                    .contains(&pane_target.pane_id)
                {
                    return Err(Rejection::from_reason_and_help(
                        RejectReason::InvalidState,
                        "not enough space to fullscreen the pane",
                    ));
                }
                true
            }
        };
        ensure_client_placement_revision_capacity(session, &[client_id])?;

        // The zoom applies to the target client alone; every other client
        // viewing this tab keeps its view. Entering also moves this client's
        // focus to the zoomed pane. Both land before the reflow, which solves
        // PTY sizes from what the clients now display.
        let client = session
            .clients
            .get_client_mut_by_id(client_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::SourceClientStale))?;
        let is_focus_moved =
            is_zoom_entered && previous_focused_pane_id != Some(pane_target.pane_id);
        if is_zoom_entered {
            client.zoom_pane(tab_id, pane_target.pane_id);
            if is_focus_moved {
                client.update_focused_pane(tab_id, pane_target.pane_id);
            }
        } else {
            client.clear_zoom(tab_id);
        }
        if is_focus_moved {
            if let Some(tab_state) = session.tabs.get_mut(&tab_id) {
                tab_state.record_focus_mru(pane_target.pane_id);
            }
        }
        advance_client_placement_revisions(session, &[client_id]);

        // This client's view changed: re-solve the tab and resize each live PTY
        // whose size changed.
        let mut emitted_events = vec![Event::LayoutChanged(LayoutChanged { tab_id })];
        self.reflow_tab_if_viewed(
            pty_backend.as_ref(),
            pane_target.session_id,
            tab_id,
            &mut emitted_events,
        );

        if is_focus_moved {
            emitted_events.push(Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(tab_id),
                pane_id: pane_target.pane_id,
                previous_pane_id: previous_focused_pane_id,
            }));
        }

        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            emitted_events,
        ))
    }

    /// Handle [`Command::WriteToPane`]: inject raw bytes into a pane's child
    /// stdin. The target is an explicit `--pane` (resolved globally) or the
    /// command source's default pane, and must be live — a pane that has
    /// exited, is closing, or is gone takes no input
    /// ([`RejectReason::InvalidState`]).
    ///
    /// The write changes no session state, and a successful write commits no
    /// events; the child's response returns
    /// through the normal PTY output path. A backend write failure — the
    /// child died between the liveness check and the write — is reported to
    /// the caller as [`RejectReason::InvalidState`].
    pub(super) fn handle_write_to_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &WriteToPaneArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let pane_target =
            self.resolve_pane_target(command_args.pane_id, command_source, acting_session)?;
        let session = self
            .session_by_id
            .get(&pane_target.session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        let pane_record = session
            .panes
            .get_pane_record_by_id(pane_target.pane_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        match pane_record.get_lifecycle() {
            PaneLifecycle::Spawning | PaneLifecycle::Running => {}
            PaneLifecycle::Exited { .. }
            | PaneLifecycle::Closing { .. }
            | PaneLifecycle::Removed => {
                return Err(Rejection::from_reason_and_help(
                    RejectReason::InvalidState,
                    "pane is not accepting input",
                ));
            }
        }
        if self
            .get_pty_backend()
            .write_pane_input(pane_target.pane_id, &command_args.pane_input_bytes)
            .is_err()
        {
            return Err(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                "pane is not accepting input",
            ));
        }
        // Bytes that reach the child count as input from the acting client, the
        // same as if typed there: the client's highlight drops and its view
        // follows back to live output. An empty payload leaves both alone.
        if !command_args.pane_input_bytes.is_empty() {
            if let Some(client_id) = command_source.get_client_id() {
                self.handle_input_reached_pane(client_id, pane_target.pane_id);
            } else {
                self.clear_session_recovery_notice_after_pane_input(pane_target.pane_id);
            }
        }
        Ok(TransactionScope::new().commit(command_id, &mut self.event_bus))
    }

    /// Choose the tab size a new split is sized against, and the *designated*
    /// client — the one that will view the tab and focus the new pane, `None`
    /// when the tab is already viewed and no client was named.
    ///
    /// A designated client is an explicit `target_client_id` (the command's
    /// named `--client`, which wins even over an in-session issuer) or, when none is
    /// named, the issuing client (`focus_client_id`). When one is designated, the
    /// split is sized to the smallest of the tab's current viewers *and* that
    /// client, and fits every client that shows it; the caller switches the client
    /// onto the tab if it is not already there.
    ///
    /// With no designated client (an external command source that names no target):
    /// an already-viewed tab sizes to its current viewers and designates no one
    /// (the pane just appears, no view moves); an unviewed tab defaults to the
    /// session's sole client, and a session with several attached clients is
    /// [`RejectReason::TargetAmbiguous`].
    ///
    /// Each client contributes [`Client::get_pane_area`]; a designated client
    /// reporting [`PaneArea::Starving`] contributes nothing, and is rejected
    /// with [`RejectReason::MinimumSize`] when no other viewer gives the tab a
    /// size.
    ///
    /// `candidate_layout_tree` is the post-split tree fit is judged against. Fails
    /// [`RejectReason::MinimumSize`] when the split cannot fit the chosen tab size,
    /// [`RejectReason::TargetNotFound`] when the designated client (a named
    /// `target_client_id`, or the issuer) is not attached here — a wrong explicit
    /// target is rejected outright, never falling back — and
    /// [`RejectReason::InvalidState`] when the tab has no viewer and the session
    /// has no attached client at all. A bystander client is never switched to
    /// satisfy a command that named none.
    fn resolve_new_pane_tab_size(
        session: &Session,
        tab_id: TabId,
        candidate_layout_tree: &LayoutNode,
        issuing_client_id: Option<ClientId>,
        target_client_id: Option<ClientId>,
        pane_sizing: PaneSizing,
    ) -> Result<(Size, Option<ClientId>), Rejection> {
        let build_no_room_rejection = || {
            Rejection::from_reason_and_help(
                RejectReason::MinimumSize,
                "not enough space for a new pane",
            )
        };
        // The chosen tab size, paired with the client designated for it, unless
        // `candidate_layout_tree` does not fit that tab size.
        let admit_tab_size = |tab_size: Size, designated_client_id: Option<ClientId>| {
            if is_layout_within_rect(
                candidate_layout_tree,
                Rect::from_size_at_origin(tab_size),
                pane_sizing,
            ) {
                Ok((tab_size, designated_client_id))
            } else {
                Err(build_no_room_rejection())
            }
        };
        let existing_tab_size = session.get_tab_size(tab_id);

        // An explicit `--client` target wins over the issuing client — a caller
        // that names a client is honored even in-session — and must be valid: a
        // wrong target is rejected outright, never falling back to the issuer. With
        // no explicit target, the in-session issuer is used.
        if let Some(client_id) = target_client_id.or(issuing_client_id) {
            let designated_client =
                session.clients.get_client_by_id(client_id).ok_or_else(|| {
                    Rejection::from_reason_and_help(
                        RejectReason::TargetNotFound,
                        "target client not attached to the session",
                    )
                })?;
            // The smaller of the tab's current size and the designated
            // client's pane area; a starving designated client adds no size.
            let tab_size = match (existing_tab_size, designated_client.get_pane_area()) {
                (Some(existing_tab_size), Some(designated_client_area)) => {
                    existing_tab_size.compute_minimum_axes(designated_client_area)
                }
                (Some(existing_tab_size), None) => existing_tab_size,
                (None, Some(designated_client_area)) => designated_client_area,
                (None, None) => return Err(build_no_room_rejection()),
            };
            return admit_tab_size(tab_size, Some(client_id));
        }

        // No designated client: an already-viewed tab needs no adoption.
        if let Some(tab_size) = existing_tab_size {
            return admit_tab_size(tab_size, None);
        }

        // Unviewed and no designated client: default to the session's sole
        // client; reject when there are several (name one) or none.
        let sole_client = Self::resolve_sole_attached_client(
            session,
            "to view the new pane's tab",
            "the new pane",
        )?;
        let tab_size = sole_client
            .get_pane_area()
            .ok_or_else(build_no_room_rejection)?;
        admit_tab_size(tab_size, Some(sole_client.get_client_id()))
    }

    /// The tab size `tab_id` is solved against when a pane closes: the tab's
    /// own size when attached clients view it, else the smallest pane
    /// area among all attached clients that report one, else a nominal 80x24.
    /// Every leg is a drawable pane region.
    ///
    /// The 80x24 leg is reached when no attached client contributes a pane
    /// area. The value ranks the surviving panes for focus repair and nothing
    /// else: no PTY is spawned or resized from it, and the next attach
    /// re-solves the tab against the client's real terminal.
    fn compute_close_tab_size(session: &Session, tab_id: TabId) -> Size {
        session
            .get_tab_size(tab_id)
            .or_else(|| {
                session
                    .clients
                    .list_attached_clients()
                    .filter_map(|client| client.get_pane_area())
                    .reduce(Size::compute_minimum_axes)
            })
            .unwrap_or(Size {
                column_count: 80,
                row_count: 24,
            })
    }
}

/// `tab_id`, plus the nearest surviving tab when `pane_id` is the last pane in
/// `tab_id`: the tabs whose viewers change when that pane closes.
fn list_tabs_affected_by_pane_close(
    session: &Session,
    tab_id: TabId,
    pane_id: PaneId,
) -> Vec<TabId> {
    let mut affected_tab_ids = vec![tab_id];
    let is_last_pane_in_tab = session
        .tabs
        .get(&tab_id)
        .is_some_and(|tab| tab.get_layout_tree().list_leaf_pane_ids() == vec![pane_id]);
    if is_last_pane_in_tab {
        if let Some(landing_tab_id) = tab_ops::find_nearest_surviving_tab(session, tab_id) {
            affected_tab_ids.push(landing_tab_id);
        }
    }
    affected_tab_ids
}

/// Compute the destination tab's size after the acting client follows the
/// placed pane into it.
pub(super) fn compute_placement_destination_tab_size(
    session: &Session,
    source_tab_id: TabId,
    destination_tab_id: TabId,
    landing_tab_id: Option<TabId>,
    acting_client_id: ClientId,
) -> Result<Size, Rejection> {
    session
        .clients
        .list_attached_clients()
        .filter_map(|client| {
            let is_acting_client = client.get_client_id() == acting_client_id;
            let is_source_viewer_landing_in_destination = landing_tab_id
                == Some(destination_tab_id)
                && client.get_active_tab_id() == source_tab_id;
            let effective_tab_id = if is_acting_client || is_source_viewer_landing_in_destination {
                destination_tab_id
            } else {
                client.get_active_tab_id()
            };
            if effective_tab_id == destination_tab_id {
                client.get_pane_area()
            } else {
                None
            }
        })
        .reduce(Size::compute_minimum_axes)
        .ok_or_else(|| {
            Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                "destination tab has no drawable client view",
            )
        })
}

/// Compute the source tab's size after the acting client leaves it.
fn compute_placement_source_tab_size(
    session: &Session,
    source_tab_id: TabId,
    acting_client_id: ClientId,
) -> Option<Size> {
    session
        .clients
        .list_attached_clients()
        .filter(|client| {
            client.get_client_id() != acting_client_id
                && client.get_active_tab_id() == source_tab_id
        })
        .filter_map(|client| client.get_pane_area())
        .reduce(Size::compute_minimum_axes)
}
