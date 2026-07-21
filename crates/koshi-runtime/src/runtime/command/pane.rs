//! Pane command handlers: create, close, resize, focus, fullscreen,
//! rename, and raw input injection — plus the child-exit event path and the
//! shared pane-removal bookkeeping.

use super::*;

impl Server {
    /// Handle [`Command::NewPane`]: grow the source pane's tab by one pane —
    /// stacked onto the source or split from it — and spawn it, in
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
    /// source, tab unviewed) the session's sole client; a session with several
    /// attached clients and no named target is rejected as ambiguous, and one with
    /// no attached client at all is rejected. The designated client is switched
    /// onto the tab (if not already there) and the tab it left is reflowed. That
    /// client's zoom drops at the commit, so the new pane lands in the tiled view
    /// it was sized against; any other client's zoom is left alone. All events
    /// seal in one transaction.
    pub(super) fn handle_new_pane(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &NewPaneArgs,
        issued_at: SystemTime,
    ) -> Result<CommandResult, Rejection> {
        let acting = self.acting_session(source)?;
        let target = self.resolve_new_pane_source(args, source, acting)?;

        // Clone the shared backend before borrowing a session: spawn and resize
        // then need no `&self` borrow, so they coexist with `&mut Session`.
        let backend = Arc::clone(self.pty_backend());
        let pane_min = self.effective_pane_min();
        // Resolve the spawn spec before the session is borrowed, so it can read
        // the terminal config off `self`: an explicit command keeps its own
        // program, a bare new pane runs the configured default shell. Either way
        // it carries koshi's terminal identity, with an explicit command's own
        // env winning over it.
        let spawn_spec = match &args.command {
            Some(command) => {
                let mut spec = command.clone();
                if spec.cwd.is_none() {
                    spec.cwd = args.cwd.clone();
                }
                spec.env = self.terminal_identity_env(spec.env);
                spec
            }
            None => self.default_shell_spec(args.cwd.clone(), BTreeMap::new()),
        };

        let session = self
            .sessions
            .get_mut(&target.session_id)
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;

        // Build the post-edit tree without mutating anything: `--stacked` joins
        // the source's stack (creating one when the source is a plain leaf),
        // otherwise the source leaf splits directionally. A source pane that is
        // not a live leaf of the tab rejects here, before any state changes.
        let tab = session
            .tabs
            .get(&target.tab_id)
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;
        // The new pane is sized against the tiled solve: splitting drops the
        // splitting client's zoom, so that client sees the tiled layout the
        // pane is sized for. Any other client zoomed on a pane of this tab
        // does not draw the new pane at all and so asks nothing of its size.
        let new_pane_id = PaneId::new();
        let edited = if args.stacked {
            add_to_stack(tab.layout(), target.source_pane, new_pane_id)
        } else {
            let direction = args
                .direction
                .unwrap_or(self.config.layout.new_pane_direction);
            split_leaf(tab.layout(), target.source_pane, new_pane_id, direction)
        };
        let candidate = edited.map_err(|_| Rejection::bare(RejectReason::TargetNotFound))?;

        // Choose the viewport the split is sized against, and the client (if any)
        // designated to view the tab and focus the new pane. Fit is judged against
        // the candidate, so a split too large for the chosen viewport is rejected
        // before anything mutates.
        let (viewport, designated) = Self::resolve_new_pane_viewport(
            session,
            target.tab_id,
            &candidate,
            target.focus_client,
            args.client,
            pane_min,
        )?;

        // Solve the candidate against that viewport to size the new pane and
        // its siblings. Fit passed above, so the new pane has a real content
        // rect; a solve that still gives it no area rejects defensively,
        // before any mutation.
        let tab_rect = Rect::new(Point { x: 0, y: 0 }, viewport);
        let rects = content_rects(&solve_with_mode_min(
            &candidate,
            LayoutMode::Tiled,
            tab_rect,
            pane_min,
        ));
        let new_rect = rects
            .iter()
            .find(|(pane_id, _)| *pane_id == new_pane_id)
            .and_then(|(_, content)| *content)
            .ok_or_else(|| Rejection::bare(RejectReason::InvalidState))?;
        let spawn_size = compute_pty_size(new_rect);

        // What the pane records: the directory it actually launches in (an
        // explicit command's own cwd wins over `--cwd`), and the resolved spawn
        // request itself when a command was given, so the record can't disagree
        // with the process about where or what it started.
        let launch_cwd = spawn_spec.cwd.clone();
        let recorded_command = args.command.as_ref().map(|_| spawn_spec.clone());

        // Launch the child BEFORE committing any state. On failure nothing was
        // registered and no view moved, so the command rejects as if it never ran.
        let handle = Self::spawn_child(backend.as_ref(), new_pane_id, spawn_spec, spawn_size)?;

        // The child is live — commit all session state through the pure op: it
        // switches the designated client onto the tab (if not already there),
        // registers the pane `Running`, swaps in the split, and focuses it. It
        // returns the previous tab of any client it moved, for the reflow below.
        let spec = NewPaneSpec {
            cwd: launch_cwd,
            command: recorded_command,
        };
        let (prev_tab, mut events) = pane_ops::commit_new_pane(
            session,
            new_pane_id,
            target.tab_id,
            candidate,
            designated,
            spec,
            issued_at,
        );

        // Park the handle so a forwarder relays its output/exit, and record its
        // size so the reflows below can tell whether a later resize is a real
        // change. The terminal engine gives the child's output a grid to land in.
        self.park_pane_pty(new_pane_id, handle, spawn_size);
        // Announce the new pane's size — PaneCreated carries none.
        events.push(Event::PtyResized(PtyResized {
            pane_id: new_pane_id,
            size: spawn_size,
        }));

        // Reflow the target tab's other live panes to the new geometry (excluding
        // the pane just spawned, already sized above).
        self.reflow_changed(backend.as_ref(), rects, Some(new_pane_id), &mut events);

        // Adoption moved a client off its previous tab; if that tab still has a
        // viewer, reflow its live panes to the viewport it now sizes against. A
        // tab left with no viewer has no viewport and keeps its sizes.
        if let Some(prev_tab) = prev_tab {
            self.reflow_tab_if_viewed(backend.as_ref(), target.session_id, prev_tab, &mut events);
        }

        Ok(Self::commit_events(&mut self.event_bus, command_id, events))
    }

    /// Handle [`Command::ClosePane`]: tear the pane out of its session and
    /// kill its child, in commit-then-kill order — the state removal is
    /// authoritative and immediate, the process kill is best-effort and
    /// off-thread.
    ///
    /// The pane's close policy picks how the child dies: `--force` overrides
    /// it with an immediate force-kill, `Graceful` requests a stop and
    /// escalates after its grace window, and `ConfirmIfBusy` proceeds only for
    /// a pane whose child already exited, rejecting otherwise with a hint at
    /// `--force`. The removal itself is the shared cascade behind shell-exit
    /// and user close: registry drop, layout collapse, per-client focus
    /// repair, and — when the last pane of the last tab goes — tab close and
    /// session quit. The kill runs on a detached thread because a graceful
    /// kill sleeps out its grace window, and the dispatcher thread must never
    /// stall.
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
        source: &CommandSource,
        args: &ClosePaneArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting = self.acting_session(source)?;
        let target = self.resolve_pane_target(args.pane, source, acting)?;

        // Clone the shared backend before borrowing a session: the kill thread
        // takes its own handle, so no `&self` borrow crosses the commit.
        let backend = Arc::clone(self.pty_backend());
        let pane_min = self.effective_pane_min();

        let session = self
            .sessions
            .get_mut(&target.session_id)
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;
        let record = session
            .panes
            .get(target.pane_id)
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;

        let kill_policy = Self::pick_kill_policy(
            record,
            args.force,
            args.tree,
            "pane may be busy; pass --force to close anyway",
        )?;

        // Solve the tab against a deterministic viewport so focus candidates
        // rank geometrically even when no client currently views the tab.
        let tab_rect = Rect::new(
            Point { x: 0, y: 0 },
            Self::close_viewport(session, target.tab_id),
        );

        // Closing drops the zoom of the client that closed, and of that client
        // only: the tab it edited is the tiled one it now returns to. A client
        // zoomed on a pane that survives keeps its zoom; the cascade separately
        // drops the zoom of anyone zoomed on the pane being removed, which has
        // nothing left to show. A pane closing on its own — a shell exiting, no
        // client acting — disturbs nobody else's zoom.
        if let Some(client_id) = source.client_id() {
            if let Some(client) = session.clients.get_mut(client_id) {
                client.clear_zoom(target.tab_id);
            }
        }

        // Commit the state removal: registry drop, layout collapse, per-client
        // focus repair, empty-tab close, last-tab quit — one shared cascade.
        let mut events = remove_pane_cascade(
            session,
            target.tab_id,
            target.pane_id,
            tab_rect,
            pane_min,
            EmptyTabPolicy::default(),
        );

        // The pane is gone from state; drop its runtime bookkeeping and reflow
        // the survivors into the space it freed.
        self.release_pane_and_reflow(
            target.session_id,
            target.tab_id,
            target.pane_id,
            backend.as_ref(),
            &mut events,
        );

        // Kill the child off-thread: a graceful kill sleeps out its grace
        // window, and the dispatcher must keep draining. The kill also purges
        // the backend's own entry for the pane, even when the child already
        // exited.
        let pane_id = target.pane_id;
        let _ = thread::spawn(move || {
            let _ = backend.kill(pane_id, kill_policy);
        });

        Ok(Self::commit_events(&mut self.event_bus, command_id, events))
    }

    /// Pick how a pane's child dies. `force` overrides the pane's own policy
    /// with an immediate force-kill; `ConfirmIfBusy` allows the close only for
    /// a pane whose child provably ended (`Exited`) and otherwise rejects with
    /// `busy_hint`. `tree` widens the picked kill to the child's whole process
    /// group.
    pub(super) fn pick_kill_policy(
        record: &PaneRecord,
        force: bool,
        tree: bool,
        busy_hint: &str,
    ) -> Result<KillPolicy, Rejection> {
        let kill_policy = if force {
            KillPolicy::Force
        } else {
            match record.close_policy {
                PaneClosePolicy::ConfirmIfBusy => match record.lifecycle() {
                    PaneLifecycle::Exited { .. } => record.close_policy.kill_policy(),
                    _ => return Err(Rejection::new(RejectReason::InvalidState, busy_hint)),
                },
                policy => policy.kill_policy(),
            }
        };
        Ok(if tree {
            kill_policy.tree_scoped()
        } else {
            kill_policy
        })
    }

    /// Drop every per-pane record a removed pane leaves behind: its PTY handle,
    /// size cache, terminal engine, each client's scroll offset for it (so the
    /// per-view map holds no dead entries over the session's life), any client
    /// highlight that was in it, and any drag that was selecting in it. The one
    /// release point for pane bookkeeping — every path that removes a pane
    /// funnels through here.
    pub(super) fn release_pane_bookkeeping(&mut self, session_id: SessionId, pane_id: PaneId) {
        self.pty_handles.remove(&pane_id);
        self.pty_sizes.remove(&pane_id);
        self.terminal_engines.remove(&pane_id);
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        for client in session.clients.list_attached_mut() {
            client.set_scroll_offset(pane_id, 0);
            client.clear_selection(pane_id);
            // A drag selecting in this pane has nothing left to select: the
            // gesture ends with the pane rather than outliving it.
            if client
                .selection_drag()
                .is_some_and(|drag| drag.pane == pane_id)
            {
                client.set_selection_drag(None);
            }
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
        backend: &dyn PtyBackend,
        events: &mut Vec<Event>,
    ) {
        self.release_pane_bookkeeping(session_id, pane_id);

        let tab_survives = self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.tabs.contains_key(&tab_id));
        let reflow_tab = if tab_survives {
            Some(tab_id)
        } else {
            events.iter().find_map(|event| match event {
                Event::TabFocused(focused) => Some(focused.tab_id),
                _ => None,
            })
        };
        if let Some(reflow_tab) = reflow_tab {
            self.reflow_tab_if_viewed(backend, session_id, reflow_tab, events);
        }
    }

    /// Route a child-process exit for `pane_id` through the pane's exit policy,
    /// returning the resulting domain events.
    ///
    /// The child is already dead: the backend's watcher reaped it and set its
    /// `exited` flag before this exit became observable. [`on_child_exit`]
    /// applies the pane's exit policy — a `CloseOnExit` pane is removed (its tab
    /// may close and the last tab quit) and its runtime bookkeeping is released
    /// while the survivors reflow; a `RespawnShell` pane keeps its slot and its
    /// bookkeeping, its lifecycle advancing to `Spawning`. An exit for a pane
    /// already gone — closed while the exit waited in the inbox — is dropped.
    ///
    /// Releasing a removed pane's bookkeeping drops its PTY handle, size cache,
    /// terminal engine, and the backend's own PTY entry. The backend purge goes
    /// through `kill`, which drops the writer, joins the finished watcher, and
    /// frees the master fd; the `exited` flag the watcher set makes it send no
    /// signal to the dead child, so the purge is a bounded, inline call.
    ///
    /// `exited_at` is supplied by the producer that observed the exit; the
    /// handler never reads the clock itself.
    pub fn handle_child_exit(
        &mut self,
        pane_id: PaneId,
        status: ExitStatus,
        exited_at: SystemTime,
    ) -> Vec<Event> {
        // A signal-terminated child carries no numeric code; the session models
        // that as `None`.
        let exit_code = match status {
            ExitStatus::ExitCode(code) => Some(code),
            ExitStatus::Signaled(_) => None,
        };

        // Find the session that owns the pane. An exit for a pane already gone
        // (closed while the exit waited in the inbox) is dropped.
        let Some(session_id) = self.session_for_pane(pane_id).map(|session| session.id) else {
            return Vec::new();
        };

        // Clone the shared backend before borrowing the session: releasing the
        // pane's PTY entry then needs no `&self` across the mutation.
        let backend = Arc::clone(self.pty_backend());
        let pane_min = self.effective_pane_min();

        let session = self
            .sessions
            .get_mut(&session_id)
            .expect("session located above");
        // The pane is in the registry but no tab's layout holds it — a
        // registry↔layout desync (`OrphanedPaneRecord`) no valid state produces.
        // Drop the exit: a data desync must not crash the runtime.
        let Ok(tab_id) = Self::tab_of_pane(session, pane_id) else {
            return Vec::new();
        };

        // Solve the tab against a deterministic viewport so focus repair ranks
        // candidates geometrically even when no client currently views the tab.
        let tab_rect = Rect::new(Point { x: 0, y: 0 }, Self::close_viewport(session, tab_id));

        // Apply the exit policy: `PaneProcessExited`, then either the removal
        // cascade (`CloseOnExit`) or a lifecycle advance to `Spawning`
        // (`RespawnShell`).
        let mut events = on_child_exit(
            session,
            tab_id,
            pane_id,
            exit_code,
            exited_at,
            tab_rect,
            pane_min,
            EmptyTabPolicy::default(),
        );

        // A respawning pane keeps its slot and bookkeeping — nothing to release.
        if session.panes.get(pane_id).is_some() {
            return events;
        }

        // The policy removed the pane: drop its runtime bookkeeping and reflow
        // the survivors into the space it freed.
        self.release_pane_and_reflow(session_id, tab_id, pane_id, backend.as_ref(), &mut events);

        // Release the backend's own PTY entry. The child already exited, so the
        // `exited`-flag guard skips the signal — this only drops the writer,
        // joins the finished watcher, and frees the master fd.
        let _ = backend.kill(pane_id, KillPolicy::Force);

        self.render_scheduler
            .invalidate(InvalidationReason::LayoutChanged);

        events
    }

    /// Move one border of a pane by an exact signed cell count, then resize
    /// the affected PTYs.
    ///
    /// The border that moves is resolved by the layout crate's resize
    /// transaction: a positive `args.size` grows the pane toward
    /// `args.direction` with the adjacent sibling donating the cells, a
    /// negative one shrinks it with that sibling gaining them. A pane with no
    /// border on the named side — it touches the tab edge there — moves its
    /// opposite border in the same visual direction instead, so a resize
    /// keybinding always adjusts the pane whenever any border can move. The
    /// target pane's tab must be viewed by at least one attached client: the tab is
    /// solved against that real viewport ([`Session::tab_viewport`]), so the
    /// donating side's spare cells are measured against the exact terminal
    /// displaying the result, and a tab no client currently views rejects.
    /// On success the tab's tree is swapped in — the resizing client's zoom drops,
    /// making the moved border visible to the client that moved it, while any
    /// other client's zoom stands — [`Event::LayoutChanged`] is emitted, and every
    /// live PTY whose solved size changed is resized through the shared reflow
    /// path, one [`Event::PtyResized`] each.
    pub(super) fn handle_resize_pane(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &ResizePaneArgs,
    ) -> Result<CommandResult, Rejection> {
        if args.size == 0 {
            return Err(Rejection::new(
                RejectReason::InvalidState,
                "resize size must be non-zero",
            ));
        }
        let acting = self.acting_session(source)?;
        let target = self.resolve_pane_target(args.pane, source, acting)?;

        let backend = Arc::clone(self.pty_backend());

        let pane_min = self.effective_pane_min();
        let (session, viewport) = self.session_and_viewport(target.session_id, target.tab_id)?;
        let tab_rect = Rect::new(Point { x: 0, y: 0 }, viewport);
        let tab = session
            .tabs
            .get_mut(&target.tab_id)
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;

        // The resize transaction returns a new tree and leaves the tab's
        // untouched on rejection, so a failed resize mutates nothing. When the
        // pane touches the tab edge on the named side, the opposite border
        // moves in the same visual direction instead — the pane shrinks where
        // it would have grown, and grows where it would have shrunk.
        let resized = resize_with_min(
            tab.layout(),
            tab_rect,
            target.pane_id,
            args.direction,
            args.size,
            pane_min,
        )
        .or_else(|error| match error {
            ResizeError::NoAdjacentBorder { .. } => resize_with_min(
                tab.layout(),
                tab_rect,
                target.pane_id,
                args.direction.opposite(),
                args.size.saturating_neg(),
                pane_min,
            ),
            other => Err(other),
        })
        .map_err(|error| Self::resize_rejection(&error))?;
        tab.update_layout(resized);

        // Resizing drops the zoom of the client that resized, and of that client
        // only: a moved border is invisible under a zoom, so the client that
        // moved it returns to the tiled view to see it. Another client zoomed on
        // a pane of this tab keeps its zoom — its pane still exists, and one
        // client resizing does not disturb another client's view.
        if let Some(client_id) = source.client_id() {
            if let Some(client) = session.clients.get_mut(client_id) {
                client.clear_zoom(target.tab_id);
            }
        }

        // The border moved: re-solve the tab and resize each live PTY whose
        // size changed.
        let mut events = vec![Event::LayoutChanged(LayoutChanged {
            tab_id: target.tab_id,
        })];
        let rects = Self::tab_content_rects(session, target.tab_id, viewport, pane_min);
        self.reflow_changed(backend.as_ref(), rects, None, &mut events);

        Ok(Self::commit_events(&mut self.event_bus, command_id, events))
    }

    /// Map a layout [`ResizeError`] onto the command vocabulary's rejection:
    /// a missing pane is [`RejectReason::TargetNotFound`], a pane with no
    /// neighbor on the requested side is [`RejectReason::InvalidState`], and
    /// a donor below its floor is [`RejectReason::MinSize`] with the spare
    /// cell count in the hint.
    fn resize_rejection(error: &ResizeError) -> Rejection {
        match error {
            ResizeError::PaneNotFound { .. } => Rejection::bare(RejectReason::TargetNotFound),
            ResizeError::NoAdjacentBorder { .. } => Rejection::new(
                RejectReason::InvalidState,
                "pane has no border to move on that axis",
            ),
            ResizeError::MinSize { spare, .. } => Rejection::new(
                RejectReason::MinSize,
                &format!("the donating pane has only {spare} spare cells to give"),
            ),
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
        source: &CommandSource,
        args: &FocusPaneArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting = self.acting_session(source)?;
        let pane_min = self.effective_pane_min();
        let target = Self::resolve_focus_target(args, source, acting, pane_min)?;

        let backend = Arc::clone(self.pty_backend());

        let (session, viewport) = self.session_and_viewport(target.session_id, target.tab_id)?;
        // Zoom follows focus, and zoom is this client's own: a zoomed client
        // focusing another pane swaps what its zoom shows, while every other
        // client's view stays exactly as it was. The mode solved and checked
        // below is therefore the one THIS client will display.
        let client = session
            .clients
            .get(target.client_id)
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
        let prior_pane = client.focused_pane(target.tab_id);
        let client_mode = client.layout_mode(target.tab_id);
        let effective_mode = match client_mode {
            LayoutMode::Fullscreen { focused } if focused != target.pane_id => {
                LayoutMode::Fullscreen {
                    focused: target.pane_id,
                }
            }
            mode => mode,
        };
        let retargeted = effective_mode != client_mode;

        let tab = session
            .tabs
            .get_mut(&target.tab_id)
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;

        // Solve the tab as this client will display it: a pane suppressed for
        // lack of space cannot take focus.
        let tab_rect = Rect::new(Point { x: 0, y: 0 }, viewport);
        let solved = solve_with_mode_min(tab.layout(), effective_mode, tab_rect, pane_min);
        if solved.suppressed.contains(&target.pane_id) {
            return Err(Rejection::new(
                RejectReason::InvalidState,
                "pane is suppressed; not enough space to show it",
            ));
        }

        // A collapsed stack member is a valid target: focusing it expands it.
        // The activation mutates a candidate tree, swapped in whole.
        let mut candidate = tab.layout().clone();
        let activated = candidate
            .stack_containing_mut(target.pane_id)
            .and_then(|stack| stack_activate(stack, target.pane_id))
            .is_some();
        if activated {
            tab.update_layout(candidate);
        }

        if prior_pane == Some(target.pane_id) && !activated && !retargeted {
            return Ok(TransactionScope::new().commit(command_id, &mut self.event_bus));
        }

        // Move the focus — which carries this client's zoom with it — BEFORE the
        // reflow: PTY sizes are solved from what every client now displays, so
        // the zoom has to have landed on its new pane first.
        let client = session
            .clients
            .get_mut(target.client_id)
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
        client.update_focused_pane(target.tab_id, target.pane_id);
        if let Some(tab) = session.tabs.get_mut(&target.tab_id) {
            tab.record_focus_mru(target.pane_id);
        }

        // The activation, the zoom retarget, or both changed what is drawn:
        // announce the new geometry and resize each live PTY whose size changed.
        let mut events = Vec::new();
        if activated || retargeted {
            events.push(Event::LayoutChanged(LayoutChanged {
                tab_id: target.tab_id,
            }));
            let rects = Self::tab_content_rects(session, target.tab_id, viewport, pane_min);
            self.reflow_changed(backend.as_ref(), rects, None, &mut events);
        }

        if prior_pane != Some(target.pane_id) {
            events.push(Event::PaneFocused(PaneFocused {
                client_id: target.client_id,
                tab_id: target.tab_id,
                pane_id: target.pane_id,
                prior_pane,
            }));
        }

        Ok(Self::commit_events(&mut self.event_bus, command_id, events))
    }

    /// Handle [`Command::TogglePaneFullscreen`]: switch the **acting client's**
    /// view of the target pane's tab between tiled and a zoom of that pane.
    ///
    /// The zoom belongs to that one client. Another client viewing the same tab
    /// keeps the view it had — its own tiled layout, its own focus, its own
    /// keys reaching its own pane — so zooming never reaches across clients.
    ///
    /// The target is the command's default pane — the in-session issuing pane,
    /// else the source client's focused pane. An already-zoomed client toggles
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
        source: &CommandSource,
    ) -> Result<CommandResult, Rejection> {
        let acting = self.acting_session(source)?;
        let pane_min = self.effective_pane_min();
        let target = self.resolve_default_pane(source, acting)?;

        let backend = Arc::clone(self.pty_backend());

        let session = self
            .sessions
            .get_mut(&target.session_id)
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;
        let tab_id = Self::tab_of_pane(session, target.pane_id)?;
        let viewport = session.tab_viewport(tab_id).ok_or_else(|| {
            Rejection::new(
                RejectReason::InvalidState,
                "pane's tab is not viewed by any client",
            )
        })?;
        let client = session
            .clients
            .get(target.client_id)
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
        let client_mode = client.layout_mode(tab_id);
        let prior_pane = client.focused_pane(tab_id);

        let tab = session
            .tabs
            .get(&tab_id)
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;

        // Flip this client's zoom. Entering solves the zoomed view first: a
        // viewport too small to show the pane at its content minimum rejects
        // before anything mutates.
        let entered = match client_mode {
            LayoutMode::Fullscreen { .. } => false,
            LayoutMode::Tiled => {
                let mode = LayoutMode::Fullscreen {
                    focused: target.pane_id,
                };
                let tab_rect = Rect::new(Point { x: 0, y: 0 }, viewport);
                let solved = solve_with_mode_min(tab.layout(), mode, tab_rect, pane_min);
                if solved.suppressed.contains(&target.pane_id) {
                    return Err(Rejection::new(
                        RejectReason::InvalidState,
                        "not enough space to fullscreen the pane",
                    ));
                }
                true
            }
        };

        // Apply the zoom to the acting client, and to it alone — every other
        // client viewing this tab keeps the view it already had. Entering also
        // moves this client's focus to the zoomed pane, so its focus never sits
        // on a pane its own zoom just hid. Both land BEFORE the reflow: PTY
        // sizes come from what the clients now display.
        let client = session
            .clients
            .get_mut(target.client_id)
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
        let focus_moved = entered && prior_pane != Some(target.pane_id);
        if entered {
            client.zoom_pane(tab_id, target.pane_id);
            if focus_moved {
                client.update_focused_pane(tab_id, target.pane_id);
            }
        } else {
            client.clear_zoom(tab_id);
        }
        if focus_moved {
            if let Some(tab) = session.tabs.get_mut(&tab_id) {
                tab.record_focus_mru(target.pane_id);
            }
        }

        // This client's view changed: re-solve the tab and resize each live PTY
        // whose size changed.
        let mut events = vec![Event::LayoutChanged(LayoutChanged { tab_id })];
        let rects = Self::tab_content_rects(session, tab_id, viewport, pane_min);
        self.reflow_changed(backend.as_ref(), rects, None, &mut events);

        if focus_moved {
            events.push(Event::PaneFocused(PaneFocused {
                client_id: target.client_id,
                tab_id,
                pane_id: target.pane_id,
                prior_pane,
            }));
        }

        Ok(Self::commit_events(&mut self.event_bus, command_id, events))
    }

    /// Handle [`Command::RenamePane`]: update the pane's display title.
    ///
    /// The target resolves like ClosePane/ResizePane — an explicit pane by a
    /// global owner scan, else the issuing pane (in-session CLI) or the
    /// source client's focused pane. The caller supplies no name — a gimmick
    /// name is drawn from [`generate_name`], skipping every title already on
    /// one of the owning session's panes (including the target's current
    /// one, so the rename always changes it). The rename applies through
    /// [`pane_ops::rename_pane`]. Titles resolve nothing, so layout, focus,
    /// and PTYs are untouched.
    pub(super) fn handle_rename_pane(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &RenamePaneArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting = self.acting_session(source)?;
        let target = self.resolve_pane_target(args.pane, source, acting)?;
        let session = self
            .sessions
            .get_mut(&target.session_id)
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;
        let new_name = generate_name(NameKind::Pane, |candidate| {
            session
                .panes
                .list()
                .any(|record| record.title.as_deref() == Some(candidate))
        });

        let events = pane_ops::rename_pane(session, target.pane_id, new_name);

        Ok(Self::commit_events(&mut self.event_bus, command_id, events))
    }

    /// Handle [`Command::WriteToPane`]: inject raw bytes into a pane's child
    /// stdin. The target is an explicit `--pane` (resolved globally) or the
    /// source's default pane, and must be a terminal pane that is live — a
    /// plugin pane, which has no PTY, and a pane that has exited, is closing,
    /// or is gone all take no input ([`RejectReason::InvalidState`]). A plugin
    /// source is denied pending the `pane_write` capability.
    ///
    /// The write is a side effect that changes no session state, so a
    /// successful write commits no events; the child's response returns
    /// through the normal PTY output path. A backend write failure — the
    /// child died between the liveness check and the write — is reported to
    /// the caller as [`RejectReason::InvalidState`].
    pub(super) fn handle_write_to_pane(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &WriteToPaneArgs,
    ) -> Result<CommandResult, Rejection> {
        // Plugin input injection requires the `pane_write` capability granted
        // by the plugin host; until that lands a plugin source is denied.
        if matches!(source, CommandSource::Plugin { .. }) {
            return Err(Rejection::new(
                RejectReason::Unauthorized,
                "plugin lacks the pane_write capability",
            ));
        }
        let acting = self.acting_session(source)?;
        let target = self.resolve_pane_target(args.pane, source, acting)?;
        let session = self
            .sessions
            .get(&target.session_id)
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;
        let record = session
            .panes
            .get(target.pane_id)
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;
        // Only a terminal pane has a PTY for the bytes to land in; a plugin
        // pane reads its input through the plugin host.
        if !matches!(record.kind(), PaneKind::Terminal) {
            return Err(Rejection::new(
                RejectReason::InvalidState,
                "pane is not a terminal pane",
            ));
        }
        match record.lifecycle() {
            PaneLifecycle::Spawning | PaneLifecycle::Running => {}
            PaneLifecycle::Exited { .. }
            | PaneLifecycle::Closing { .. }
            | PaneLifecycle::Removed => {
                return Err(Rejection::new(
                    RejectReason::InvalidState,
                    "pane is not accepting input",
                ));
            }
        }
        if self
            .pty_backend()
            .write(target.pane_id, &args.data)
            .is_err()
        {
            return Err(Rejection::new(
                RejectReason::InvalidState,
                "pane is not accepting input",
            ));
        }
        // Bytes that reach the child count as input from the acting client, the
        // same as if typed there: the client's highlight drops and its view
        // follows back to live output. An empty payload sent nothing, so it is
        // not input and leaves both alone.
        if !args.data.is_empty() {
            if let Some(client_id) = source.client_id() {
                self.on_input_reached_pane(client_id, target.pane_id);
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
    /// named, the issuing client (`focus_client`). When one is designated, the
    /// split is sized to the smallest of the tab's current viewers *and* that
    /// client, so it fits everyone who will see it; the caller switches the client
    /// onto the tab if it is not already there.
    ///
    /// With no designated client (an external/plugin source that names no target):
    /// an already-viewed tab sizes to its current viewers and designates no one
    /// (the pane just appears, no view moves); an unviewed tab defaults to the
    /// session's sole client, and a session with several attached clients is
    /// [`RejectReason::TargetAmbiguous`] (a bystander is never switched to satisfy
    /// a command that named no client).
    ///
    /// `candidate` is the post-split tree fit is judged against. Fails
    /// [`RejectReason::MinSize`] when the split cannot fit the chosen viewport,
    /// [`RejectReason::TargetNotFound`] when the designated client (a named
    /// `target_client`, or the issuer) is not attached here — a wrong explicit
    /// target is rejected outright, never falling back — and
    /// [`RejectReason::InvalidState`] when the tab has no viewer and the session
    /// has no attached client at all.
    fn resolve_new_pane_viewport(
        session: &Session,
        tab_id: TabId,
        candidate: &LayoutNode,
        focus_client: Option<ClientId>,
        target_client: Option<ClientId>,
        min: Size,
    ) -> Result<(Size, Option<ClientId>), Rejection> {
        let fits_viewport =
            |viewport: Size| fits(candidate, Rect::new(Point { x: 0, y: 0 }, viewport), min);
        let wont_fit = || Rejection::new(RejectReason::MinSize, "not enough space for a new pane");
        let existing = session.tab_viewport(tab_id);

        // An explicit `--client` target wins over the issuing client — a caller
        // that names a client is honored even in-session — and must be valid: a
        // wrong target is rejected outright, never falling back to the issuer. With
        // no explicit target, the in-session issuer is used.
        if let Some(client_id) = target_client.or(focus_client) {
            let client = session.clients.get(client_id).ok_or_else(|| {
                Rejection::new(
                    RejectReason::TargetNotFound,
                    "target client not attached to the session",
                )
            })?;
            // Size to the smallest of the current viewers and the designated
            // client — each as its drawable pane region — so the pane fits
            // every client that will view the tab.
            let designated = pane_viewport(client.viewport());
            let viewport = match existing {
                Some(existing) => Size {
                    cols: existing.cols.min(designated.cols),
                    rows: existing.rows.min(designated.rows),
                },
                None => designated,
            };
            return if fits_viewport(viewport) {
                Ok((viewport, Some(client_id)))
            } else {
                Err(wont_fit())
            };
        }

        // No designated client: an already-viewed tab needs no adoption.
        if let Some(viewport) = existing {
            return if fits_viewport(viewport) {
                Ok((viewport, None))
            } else {
                Err(wont_fit())
            };
        }

        // Unviewed and no designated client: default to the session's sole
        // client; reject when there are several (name one) or none.
        let only =
            Self::sole_attached_client(session, "to view the new pane's tab", "the new pane")?;
        let viewport = pane_viewport(only.viewport());
        if fits_viewport(viewport) {
            Ok((viewport, Some(only.id())))
        } else {
            Err(wont_fit())
        }
    }

    /// The viewport `tab_id` is solved against when a pane closes: the tab's
    /// own viewport when attached clients view it, else the smallest pane
    /// region among all attached clients, else a nominal 80x24. Every leg is
    /// a drawable pane region, so the ranking geometry matches what a viewer
    /// would actually see.
    ///
    /// The 80x24 leg is reached only when the session has no attached client
    /// at all — a headless close issued through the CLI, where no terminal
    /// size exists anywhere. Focus repair still needs a concrete rect to rank
    /// the surviving panes geometrically, and that ranking is this value's
    /// sole consumer: no PTY is spawned or resized from it (a tab with no
    /// viewer keeps its PTY sizes), and the next attach re-solves the tab
    /// against the client's real terminal.
    fn close_viewport(session: &Session, tab_id: TabId) -> Size {
        session
            .tab_viewport(tab_id)
            .or_else(|| {
                session
                    .clients
                    .list_attached()
                    .map(|client| pane_viewport(client.viewport()))
                    .reduce(|a, b| Size {
                        cols: a.cols.min(b.cols),
                        rows: a.rows.min(b.rows),
                    })
            })
            .unwrap_or(Size { cols: 80, rows: 24 })
    }
}
