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
    /// failure commits nothing and rejects, so a pane never exists without its
    /// process. A client is designated to view and focus the new pane — an
    /// explicit `--client` target (which wins even over the issuer, and rejects
    /// outright if not attached), else the in-session issuer, else (an external
    /// command source, tab unviewed) the session's sole client; a session with several
    /// attached clients and no named target is rejected as ambiguous, and one with
    /// no attached client at all is rejected. The designated client is switched
    /// onto the tab (if not already there) and the tab it left is reflowed. That
    /// client's zoom drops at the commit, so the new pane lands in the tiled view
    /// it was sized against; any other client's zoom is left alone. All events
    /// seal in one transaction.
    pub(super) fn handle_new_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &NewPaneArgs,
        issued_at: SystemTime,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.acting_session(command_source)?;
        let new_pane_target =
            self.resolve_new_pane_source(command_args, command_source, acting_session)?;

        // Clone the shared backend before borrowing a session: spawn and resize
        // then need no `&self` borrow, so they coexist with `&mut Session`.
        let pty_backend = Arc::clone(self.get_pty_backend());
        let pane_sizing = self.get_pane_sizing();
        // Resolve the spawn spec before the session is borrowed, so it can read
        // the terminal config off `self`: an explicit spawn specification keeps its own
        // program, a bare new pane runs the configured default shell. Either way
        // it carries koshi's terminal identity, with an explicit command's own
        // environment variables winning over it.
        let mut spawn_spec = match &command_args.spawn_spec {
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
        };
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
        // The new pane is sized against the tiled solve: splitting drops the
        // splitting client's zoom, so that client sees the tiled layout the
        // pane is sized for. Any other client zoomed on a pane of this tab
        // does not draw the new pane at all and so asks nothing of its size.
        let new_pane_id = PaneId::new();
        let edited_layout_tree = if command_args.should_stack {
            add_pane_to_stack(
                tab_state.get_layout_tree(),
                new_pane_target.source_pane_id,
                new_pane_id,
            )
        } else {
            split_leaf(
                tab_state.get_layout_tree(),
                new_pane_target.source_pane_id,
                new_pane_id,
                command_args.direction,
            )
        };
        let candidate_layout_tree =
            edited_layout_tree.map_err(|_| Rejection::from_reason(RejectReason::TargetNotFound))?;

        // Choose the viewport the split is sized against, and the client (if any)
        // designated to view the tab and focus the new pane. Fit is judged against
        // the candidate, so a split too large for the chosen viewport is rejected
        // before anything mutates.
        let (viewport, designated_client_id) = Self::resolve_new_pane_viewport(
            session,
            new_pane_target.tab_id,
            &candidate_layout_tree,
            new_pane_target.focus_client_id,
            command_args.client_id,
            pane_sizing,
        )?;

        // Solve the candidate against that viewport to size the new pane and
        // its siblings. Fit passed above, so the new pane has a real content
        // rect; a solve that still gives it no area rejects defensively,
        // before any mutation.
        let tab_rect = Rect::from_size_at_origin(viewport);
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

        // What the pane records: the directory it actually launches in (an
        // explicit spawn specification's own working directory wins over
        // `--cwd`), and the resolved spawn request itself when a spawn
        // specification was given, so the record can't disagree
        // with the process about where or what it started.
        let launch_working_directory = spawn_spec.working_directory.clone();
        let recorded_spawn_spec = command_args
            .spawn_spec
            .is_some()
            .then(|| spawn_spec.clone());
        // The in-session identity vars join the launched spec only, after the
        // record above is taken; the record keeps the caller's own environment
        // variables.
        spawn_spec
            .environment_variables
            .extend(build_koshi_environment(
                new_pane_target.session_id,
                designated_client_id,
                new_pane_id,
                koshi_paths::resolve_runtime_directory().as_deref(),
            ));

        // Launch the child BEFORE committing any state. On failure nothing was
        // registered and no view moved, so the command rejects as if it never ran.
        let child_handle = Self::spawn_child(
            pty_backend.as_ref(),
            new_pane_id,
            spawn_spec,
            new_pane_pty_size,
        )?;

        // The child is live — commit all session state through the pure op: it
        // switches the designated client onto the tab (if not already there),
        // registers the pane `Running`, swaps in the split, and focuses it. It
        // returns the previous tab of any client it moved, for the reflow below.
        let new_pane_spec = NewPaneSpec {
            working_directory: launch_working_directory,
            spawn_spec: recorded_spawn_spec,
        };
        let (previous_tab_id, mut emitted_events) = pane_ops::commit_new_pane(
            session,
            new_pane_id,
            new_pane_target.tab_id,
            candidate_layout_tree,
            designated_client_id,
            new_pane_spec,
            issued_at,
        );

        // Park the handle so a forwarder relays its output/exit, and record its
        // size so the reflows below can tell whether a later resize is a real
        // change. The terminal engine gives the child's output a grid to land in.
        self.park_pane_pty(new_pane_id, child_handle, new_pane_pty_size);
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
        // viewer, reflow its live panes to the viewport it now sizes against. A
        // tab left with no viewer has no viewport and keeps its sizes.
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
    /// viewport and each live PTY whose size changed is resized, one
    /// [`Event::PtyResized`] per applied resize, in layout order. When the
    /// close emptied the tab, the nearest surviving tab its viewers moved to
    /// reflows instead. A tab with no viewer has no viewport and keeps its
    /// sizes.
    pub(super) fn handle_close_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &ClosePaneArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.acting_session(command_source)?;
        let pane_target =
            self.resolve_pane_target(command_args.pane_id, command_source, acting_session)?;

        // Clone the shared backend before borrowing a session: the kill thread
        // takes its own handle, so no `&self` borrow crosses the commit.
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

        // Solve the tab against a deterministic viewport so focus candidates
        // rank geometrically even when no client currently views the tab.
        let tab_rect = Rect::from_size_at_origin(Self::close_viewport(session, pane_target.tab_id));

        // Closing drops the zoom of the client that closed, and of that client
        // only: the tab it edited is the tiled one it now returns to. A client
        // zoomed on a pane that survives keeps its zoom; the cascade separately
        // drops the zoom of anyone zoomed on the pane being removed, which has
        // nothing left to show. A pane closing on its own — a shell exiting, no
        // client acting — disturbs nobody else's zoom.
        if let Some(client) = command_source
            .get_client_id()
            .and_then(|client_id| session.clients.get_client_mut_by_id(client_id))
        {
            client.clear_zoom(pane_target.tab_id);
        }

        // Commit the state removal: registry drop, layout collapse, per-client
        // focus repair, empty-tab close, last-tab quit — one shared cascade.
        let mut emitted_events = remove_pane_cascade(
            session,
            pane_target.tab_id,
            pane_target.pane_id,
            tab_rect,
            pane_sizing,
            EmptyTabPolicy::default(),
            None,
        );

        // The pane is gone from state; drop its runtime bookkeeping and reflow
        // the survivors into the space it freed.
        self.release_pane_and_reflow(
            pane_target.session_id,
            pane_target.tab_id,
            pane_target.pane_id,
            pty_backend.as_ref(),
            &mut emitted_events,
        );

        super::kill_off_thread(&pty_backend, pane_target.pane_id, kill_policy);

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
        let kill_policy = if should_force_close {
            KillPolicy::Force
        } else {
            pane_record.close_policy.kill_policy()
        };
        Ok(if should_kill_process_tree {
            kill_policy.apply_tree_scope()
        } else {
            kill_policy
        })
    }

    /// Drop every per-pane record a removed pane leaves behind: its PTY handle,
    /// size cache, terminal engine, and — for every attached client — that
    /// client's scroll offset for the pane and the highlight it holds there,
    /// so no per-view map keeps a dead entry. The one release point for pane
    /// bookkeeping — every path that removes a pane funnels through here.
    pub(super) fn release_pane_bookkeeping(&mut self, session_id: SessionId, pane_id: PaneId) {
        self.pty_handle_by_pane_id.remove(&pane_id);
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
    /// Removes the pane's PTY handle, size cache, and terminal engine — output
    /// bytes still in flight for it now find no engine and are dropped — and
    /// clears each client's scroll offset and any highlight in it, then
    /// re-solves and resizes the tab that reclaims the space: the pane's own tab
    /// when it survives, else the tab its viewers moved to (the cascade's
    /// `TabFocused`). A tab left with no viewer has no viewport and keeps its
    /// sizes. Each applied resize appends one [`Event::PtyResized`] to `events`.
    ///
    /// Shared by [`handle_close_pane`](Self::handle_close_pane) and
    /// [`handle_child_exit`](Self::handle_child_exit). Killing the child and any
    /// render invalidation stay with the caller: a close kills a live child on a
    /// detached thread, while a child-exit reaps a dead one inline.
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

    /// Route a child-process exit for `pane_id` through the pane's exit policy,
    /// returning the resulting domain events.
    ///
    /// The child is already dead: the backend's watcher reaped it and set its
    /// `exited` flag before this exit became observable. [`on_child_exit`]
    /// applies the pane's exit policy: the pane is removed — its tab may close
    /// and the last tab quit — and its runtime bookkeeping is released while the
    /// survivors reflow. An exit for a pane already gone — closed while the exit
    /// waited in the inbox — is dropped.
    ///
    /// Releasing a removed pane's bookkeeping drops its PTY handle, size cache,
    /// terminal engine, and the backend's own PTY entry. The backend purge goes
    /// through `kill`, which drops the writer, joins the finished watcher, and
    /// frees the master fd; the `exited` flag the watcher set makes it send no
    /// signal to the dead child, so the purge is a bounded, inline call.
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

        // Clone the shared backend before borrowing the session: releasing the
        // pane's PTY entry then needs no `&self` across the mutation.
        let pty_backend = Arc::clone(self.get_pty_backend());
        let pane_sizing = self.get_pane_sizing();

        let session = self
            .session_by_id
            .get_mut(&session_id)
            .expect("session located above");
        // The pane is in the registry but no tab's layout holds it — a
        // registry↔layout desync (`OrphanedPaneRecord`) no valid state produces.
        // Drop the exit: a data desync must not crash the runtime.
        let Ok(tab_id) = Self::resolve_tab_id_for_pane(session, pane_id) else {
            return Vec::new();
        };

        // Solve the tab against a deterministic viewport so focus repair ranks
        // candidates geometrically even when no client currently views the tab.
        let tab_rect = Rect::from_size_at_origin(Self::close_viewport(session, tab_id));

        // Apply the exit policy: `PaneProcessExited`, then the removal cascade.
        let mut emitted_events = on_child_exit(
            session,
            tab_id,
            pane_exit,
            tab_rect,
            pane_sizing,
            EmptyTabPolicy::default(),
        );

        // Drop the removed pane's runtime bookkeeping and reflow the survivors
        // into the space it freed.
        self.release_pane_and_reflow(
            session_id,
            tab_id,
            pane_id,
            pty_backend.as_ref(),
            &mut emitted_events,
        );

        // Release the backend's own PTY entry. The child already exited, so the
        // `exited`-flag guard skips the signal — this only drops the writer,
        // joins the finished watcher, and frees the master fd.
        let _ = pty_backend.kill_pane(pane_id, KillPolicy::Force);

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
    /// opposite border in the same visual direction instead, so a resize
    /// keybinding always adjusts the pane whenever any border can move. The
    /// target pane's tab must be viewed by at least one attached client: the tab is
    /// solved against that real viewport ([`Session::tab_viewport`]), so the
    /// donating side's spare cells are measured against the exact terminal
    /// displaying the result, and a tab no viewer contributes a pane area to
    /// rejects.
    /// On success the tab's tree is swapped in — the resizing client's zoom drops,
    /// making the moved border visible to the client that moved it, while any
    /// other client's zoom stands — [`Event::LayoutChanged`] is emitted, and every
    /// live PTY whose solved size changed is resized through the shared reflow
    /// path, one [`Event::PtyResized`] each.
    pub(super) fn handle_resize_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &ResizePaneArgs,
    ) -> Result<CommandResult, Rejection> {
        if command_args.resize_amount_cells == 0 {
            return Err(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                "resize size must be non-zero",
            ));
        }
        let acting_session = self.acting_session(command_source)?;
        let pane_target =
            self.resolve_pane_target(command_args.pane_id, command_source, acting_session)?;

        let pty_backend = Arc::clone(self.get_pty_backend());

        let pane_sizing = self.get_pane_sizing();
        let (session, viewport) =
            self.resolve_session_and_viewport(pane_target.session_id, pane_target.tab_id)?;
        let tab_rect = Rect::from_size_at_origin(viewport);
        let tab_state = session
            .tabs
            .get_mut(&pane_target.tab_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;

        // The resize transaction returns a new tree and leaves the tab's
        // untouched on rejection, so a failed resize mutates nothing. When the
        // pane touches the tab edge on the named side, the opposite border
        // moves in the same visual direction instead — the pane shrinks where
        // it would have grown, and grows where it would have shrunk.
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
        tab_state.update_layout(resized_layout_tree);

        // Resizing drops the zoom of the client that resized, and of that client
        // only: a moved border is invisible under a zoom, so the client that
        // moved it returns to the tiled view to see it. Another client zoomed on
        // a pane of this tab keeps its zoom — its pane still exists, and one
        // client resizing does not disturb another client's view.
        if let Some(client) = command_source
            .get_client_id()
            .and_then(|client_id| session.clients.get_client_mut_by_id(client_id))
        {
            client.clear_zoom(pane_target.tab_id);
        }

        // The border moved: re-solve the tab and resize each live PTY whose
        // size changed.
        let mut emitted_events = vec![Event::LayoutChanged(LayoutChanged {
            tab_id: pane_target.tab_id,
        })];
        self.reflow_tab_if_viewed(
            pty_backend.as_ref(),
            pane_target.session_id,
            pane_target.tab_id,
            &mut emitted_events,
        );

        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            emitted_events,
        ))
    }

    /// Map a layout [`ResizeError`] onto the command vocabulary's rejection:
    /// a missing pane is [`RejectReason::TargetNotFound`], a pane with no
    /// neighbor on the requested side is [`RejectReason::InvalidState`], and
    /// a donor below its floor is [`RejectReason::MinSize`] carrying the spare
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
            } => Rejection::min_size(*spare_cell_count),
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
        let acting_session = self.acting_session(command_source)?;
        let pane_sizing = self.get_pane_sizing();
        let pane_target =
            Self::resolve_focus_target(command_args, command_source, acting_session, pane_sizing)?;

        let pty_backend = Arc::clone(self.get_pty_backend());

        let (session, viewport) =
            self.resolve_session_and_viewport(pane_target.session_id, pane_target.tab_id)?;
        // Zoom follows focus, and zoom is this client's own: a zoomed client
        // focusing another pane swaps what its zoom shows, while every other
        // client's view stays exactly as it was. The mode solved and checked
        // below is therefore the one THIS client will display.
        let client = session
            .clients
            .get_client_by_id(pane_target.client_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::SourceClientStale))?;
        let prior_focused_pane_id = client.get_focused_pane(pane_target.tab_id);
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
        let tab_rect = Rect::from_size_at_origin(viewport);
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
            .and_then(|stack| activate_stack_member(stack, pane_target.pane_id))
            .is_some();
        if is_stack_member_activated {
            tab_state.update_layout(candidate_layout_tree);
        }

        if prior_focused_pane_id == Some(pane_target.pane_id)
            && !is_stack_member_activated
            && !is_layout_mode_retargeted
        {
            return Ok(TransactionScope::new().commit(command_id, &mut self.event_bus));
        }

        // Move the focus — which carries this client's zoom with it — BEFORE the
        // reflow: PTY sizes are solved from what every client now displays, so
        // the zoom has to have landed on its new pane first.
        let client = session
            .clients
            .get_client_mut_by_id(pane_target.client_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::SourceClientStale))?;
        client.update_focused_pane(pane_target.tab_id, pane_target.pane_id);
        if let Some(tab_state) = session.tabs.get_mut(&pane_target.tab_id) {
            tab_state.record_focus_mru(pane_target.pane_id);
        }

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

        if prior_focused_pane_id != Some(pane_target.pane_id) {
            emitted_events.push(Event::PaneFocused(PaneFocused {
                client_id: pane_target.client_id,
                tab_id: pane_target.tab_id,
                pane_id: pane_target.pane_id,
                previous_pane_id: prior_focused_pane_id,
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
    /// keeps the view it had — its own tiled layout, its own focus, its own
    /// keys reaching its own pane — so zooming never reaches across clients.
    ///
    /// [`Self::resolve_fullscreen_target`] is the single resolver validation
    /// also used: the client is the one the caller named on the command line
    /// when there is one, else the issuer while it is still attached, else the
    /// session's sole attached client; the pane is that client's focused pane,
    /// or the issuing pane for an in-session CLI command. So
    /// `koshi toggle-pane-fullscreen --client <B>` zooms B's focused pane on
    /// B's screen while every other client of that tab stays tiled, and a CLI
    /// command from a pane whose client has gone zooms that pane for the one
    /// client still watching. An already-zoomed client toggles
    /// back to tiled whichever pane resolved; a tiled client zooms the target
    /// and, when its focus was elsewhere, moves its focus to the pane now
    /// filling its view ([`Event::PaneFocused`]). The zoom is a solve-time
    /// overlay — the tree is untouched, so toggling out restores the exact prior
    /// layout. The tab must be viewed by at least one attached client (the view
    /// change can resize real PTYs), and a viewport too small to show the pane at
    /// its content minimum rejects. Emits [`Event::LayoutChanged`] plus one
    /// [`Event::PtyResized`] per PTY whose solved size changed — a pane another
    /// client still draws tiled keeps the size that client can show, so a zoom
    /// does not always resize the child. A rejected toggle mutates nothing.
    pub(super) fn handle_toggle_pane_fullscreen(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.acting_session(command_source)?;
        let pane_sizing = self.get_pane_sizing();
        let pane_target = self.resolve_fullscreen_target(command_source, acting_session)?;
        let client_id = pane_target.client_id;

        let pty_backend = Arc::clone(self.get_pty_backend());

        let tab_id = pane_target.tab_id;
        let (session, viewport) =
            self.resolve_session_and_viewport(pane_target.session_id, tab_id)?;
        let client = session
            .clients
            .get_client_by_id(client_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::SourceClientStale))?;
        let client_layout_mode = client.get_layout_mode(tab_id);
        let prior_focused_pane_id = client.get_focused_pane(tab_id);

        let tab_state = session
            .tabs
            .get(&tab_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;

        // Flip this client's zoom. Entering solves the zoomed view first: a
        // viewport too small to show the pane at its content minimum rejects
        // before anything mutates.
        let is_zoom_entered = match client_layout_mode {
            LayoutMode::Fullscreen { .. } => false,
            LayoutMode::Tiled => {
                let fullscreen_layout_mode = LayoutMode::Fullscreen {
                    focused_pane_id: pane_target.pane_id,
                };
                let tab_rect = Rect::from_size_at_origin(viewport);
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

        // Apply the zoom to the target client, and to it alone — every other
        // client viewing this tab keeps the view it already had. Entering also
        // moves this client's focus to the zoomed pane, so its focus never sits
        // on a pane its own zoom just hid. Both land BEFORE the reflow: PTY
        // sizes come from what the clients now display.
        let client = session
            .clients
            .get_client_mut_by_id(client_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::SourceClientStale))?;
        let is_focus_moved = is_zoom_entered && prior_focused_pane_id != Some(pane_target.pane_id);
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
                tab_id,
                pane_id: pane_target.pane_id,
                previous_pane_id: prior_focused_pane_id,
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
    /// command source's default pane, and must be a terminal pane that is live — a
    /// plugin pane, which has no PTY, and a pane that has exited, is closing,
    /// or is gone all take no input ([`RejectReason::InvalidState`]). A plugin
    /// command source is [`RejectReason::Unauthorized`]: it has no `pane_write`
    /// capability.
    ///
    /// The write is a side effect that changes no session state, so a
    /// successful write commits no events; the child's response returns
    /// through the normal PTY output path. A backend write failure — the
    /// child died between the liveness check and the write — is reported to
    /// the caller as [`RejectReason::InvalidState`].
    pub(super) fn handle_write_to_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &WriteToPaneArgs,
    ) -> Result<CommandResult, Rejection> {
        // Plugin input injection requires the `pane_write` capability granted
        // by the plugin host; a plugin command source is denied.
        if matches!(command_source, CommandSource::Plugin { .. }) {
            return Err(Rejection::from_reason_and_help(
                RejectReason::Unauthorized,
                "plugin lacks the pane_write capability",
            ));
        }
        let acting_session = self.acting_session(command_source)?;
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
        // Only a terminal pane has a PTY for the bytes to land in; a plugin
        // pane reads its input through the plugin host.
        if !matches!(pane_record.get_pane_kind(), PaneKind::Terminal) {
            return Err(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                "pane is not a terminal pane",
            ));
        }
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
            .write_pane_input(pane_target.pane_id, &command_args.input_bytes)
            .is_err()
        {
            return Err(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                "pane is not accepting input",
            ));
        }
        // Bytes that reach the child count as input from the acting client, the
        // same as if typed there: the client's highlight drops and its view
        // follows back to live output. An empty payload sent nothing, so it is
        // not input and leaves both alone.
        if !command_args.input_bytes.is_empty() {
            if let Some(client_id) = command_source.get_client_id() {
                self.on_input_reached_pane(client_id, pane_target.pane_id);
            }
        }
        Ok(TransactionScope::new().commit(command_id, &mut self.event_bus))
    }

    /// Choose the viewport a new split is sized against, and the *designated*
    /// client — the one that will view the tab and focus the new pane, `None`
    /// when the tab is already viewed and no client was named.
    ///
    /// A designated client is an explicit `target_client` (the command's named
    /// `--client`, which wins even over an in-session issuer) or, when none is
    /// named, the issuing client (`focus_client_id`). When one is designated, the
    /// split is sized to the smallest of the tab's current viewers *and* that
    /// client, so it fits everyone who will see it; the caller switches the client
    /// onto the tab if it is not already there.
    ///
    /// With no designated client (an external/plugin command source that names no target):
    /// an already-viewed tab sizes to its current viewers and designates no one
    /// (the pane just appears, no view moves); an unviewed tab defaults to the
    /// session's sole client, and a session with several attached clients is
    /// [`RejectReason::TargetAmbiguous`].
    ///
    /// Each client contributes [`Client::get_pane_area`]; a designated client
    /// reporting [`PaneArea::Starving`] contributes nothing, and is rejected
    /// with [`RejectReason::MinSize`] when no other viewer gives the tab a
    /// size.
    ///
    /// `candidate_layout_tree` is the post-split tree fit is judged against. Fails
    /// [`RejectReason::MinSize`] when the split cannot fit the chosen viewport,
    /// [`RejectReason::TargetNotFound`] when the designated client (a named
    /// `target_client`, or the issuer) is not attached here — a wrong explicit
    /// target is rejected outright, never falling back — and
    /// [`RejectReason::InvalidState`] when the tab has no viewer and the session
    /// has no attached client at all. A bystander client is never switched to
    /// satisfy a command that named none.
    fn resolve_new_pane_viewport(
        session: &Session,
        tab_id: TabId,
        candidate_layout_tree: &LayoutNode,
        issuing_client_id: Option<ClientId>,
        target_client: Option<ClientId>,
        pane_sizing: PaneSizing,
    ) -> Result<(Size, Option<ClientId>), Rejection> {
        let reject_when_no_room = || {
            Rejection::from_reason_and_help(
                RejectReason::MinSize,
                "not enough space for a new pane",
            )
        };
        // The chosen viewport, paired with the client designated for it, unless
        // `candidate_layout_tree` does not fit that viewport.
        let admit_viewport = |viewport: Size, designated_client_id: Option<ClientId>| {
            if is_layout_within_rect(
                candidate_layout_tree,
                Rect::from_size_at_origin(viewport),
                pane_sizing,
            ) {
                Ok((viewport, designated_client_id))
            } else {
                Err(reject_when_no_room())
            }
        };
        let existing_viewport = session.get_tab_viewport(tab_id);

        // An explicit `--client` target wins over the issuing client — a caller
        // that names a client is honored even in-session — and must be valid: a
        // wrong target is rejected outright, never falling back to the issuer. With
        // no explicit target, the in-session issuer is used.
        if let Some(client_id) = target_client.or(issuing_client_id) {
            let target_client = session.clients.get_client_by_id(client_id).ok_or_else(|| {
                Rejection::from_reason_and_help(
                    RejectReason::TargetNotFound,
                    "target client not attached to the session",
                )
            })?;
            // The smaller of the tab's current viewport and the designated
            // client's pane area; a starving designated client adds no size.
            let viewport = match (existing_viewport, target_client.get_pane_area()) {
                (Some(existing_viewport), Some(designated_client_area)) => {
                    existing_viewport.compute_minimum_axes(designated_client_area)
                }
                (Some(existing_viewport), None) => existing_viewport,
                (None, Some(designated_client_area)) => designated_client_area,
                (None, None) => return Err(reject_when_no_room()),
            };
            return admit_viewport(viewport, Some(client_id));
        }

        // No designated client: an already-viewed tab needs no adoption.
        if let Some(viewport) = existing_viewport {
            return admit_viewport(viewport, None);
        }

        // Unviewed and no designated client: default to the session's sole
        // client; reject when there are several (name one) or none.
        let sole_client =
            Self::sole_attached_client(session, "to view the new pane's tab", "the new pane")?;
        let viewport = sole_client
            .get_pane_area()
            .ok_or_else(reject_when_no_room)?;
        admit_viewport(viewport, Some(sole_client.get_client_id()))
    }

    /// The viewport `tab_id` is solved against when a pane closes: the tab's
    /// own viewport when attached clients view it, else the smallest pane
    /// area among all attached clients that report one, else a nominal 80x24.
    /// Every leg is a drawable pane region.
    ///
    /// The 80x24 leg is reached when no attached client contributes a pane
    /// area. The value ranks the surviving panes for focus repair and nothing
    /// else: no PTY is spawned or resized from it, and the next attach
    /// re-solves the tab against the client's real terminal.
    fn close_viewport(session: &Session, tab_id: TabId) -> Size {
        session
            .get_tab_viewport(tab_id)
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
