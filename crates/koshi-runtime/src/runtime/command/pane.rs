//! Pane command handlers: create, close, resize, focus, fullscreen,
//! and raw input injection — plus the child-exit event path and the
//! shared pane-removal bookkeeping.

use super::*;

/// The least exit code, read as an unsigned 32-bit number, that is a Windows
/// NTSTATUS of error severity: `0xC0000000`, whose top two bits, the severity
/// field, are `0b11` ([MS-ERREF] section 2.3).
const MINIMUM_WINDOWS_ERROR_STATUS: u32 = 0xC000_0000;

/// The text the session posts as an `Error` when pane `pane_id`'s child ends
/// with `exit_status`, or `None` when the exit posts nothing.
/// `is_command_pane` is whether the pane runs a named command (its record
/// holds a spawn specification) rather than the default shell.
///
/// The first rule that matches decides:
/// 1. [`UNOBSERVED_EXIT`]: `pane …a1b2 ended; its exit status was not
///    received`.
/// 2. An exit code at or above [`MINIMUM_WINDOWS_ERROR_STATUS`] as an
///    unsigned 32-bit number: `pane …a1b2 exited 0xC0000005`.
/// 3. A signal: `pane …a1b2 killed by signal 9`.
/// 4. A non-zero exit code of a command pane: `pane …a1b2 exited 101`.
/// 5. Any other exit, such as exit code `0`, or exit code `1` of a shell
///    pane: `None`.
fn format_pane_exit_notice(
    pane_id: PaneId,
    exit_status: ExitStatus,
    is_command_pane: bool,
) -> Option<String> {
    let pane_label = pane_id.format_short_label();
    match exit_status {
        UNOBSERVED_EXIT => Some(format!(
            "{pane_label} ended; its exit status was not received"
        )),
        ExitStatus::ExitCode(exit_code)
            if exit_code.cast_unsigned() >= MINIMUM_WINDOWS_ERROR_STATUS =>
        {
            Some(format!(
                "{pane_label} exited 0x{:08X}",
                exit_code.cast_unsigned()
            ))
        }
        ExitStatus::Signaled(signal) => Some(format!("{pane_label} killed by signal {signal}")),
        ExitStatus::ExitCode(exit_code) if exit_code != 0 && is_command_pane => {
            Some(format!("{pane_label} exited {exit_code}"))
        }
        ExitStatus::ExitCode(_) => None,
    }
}

impl Server {
    /// Handle [`Command::NewPane`]: grow the command source pane's tab by one pane —
    /// stacked onto the command source or split from it — and spawn it, in
    /// launch-then-commit order — no session state changes until the child
    /// process is live.
    ///
    /// The candidate tree is built and its fit preflighted, the child PTY
    /// is spawned, and only on success is the pane registered, the
    /// tree swapped in, the sibling PTYs reflowed, and the handle parked. A launch
    /// failure commits nothing and rejects; no pane is registered without its
    /// process. The source pane and the client that views and focuses the new
    /// pane come from [`Self::resolve_new_pane_source`]: an external command
    /// naming `--client` and no `--pane` or `--tab` splits the pane that takes
    /// that client's input. A client is designated to view and focus the new
    /// pane — an explicit `--client` target (which wins even over the issuer,
    /// and rejects outright if not attached), else the attached issuer, else
    /// (an external command source, tab unviewed) the session's sole client; a
    /// session with several attached clients and no named target is rejected as
    /// ambiguous, and one with no attached client at all is rejected. An
    /// external command source naming no client on a tab a client already
    /// views designates no client, and no client's focus moves. The designated
    /// client is switched onto the tab (if not already there) and the tab it
    /// left is reflowed. That client's zoom drops at the commit, and the new
    /// pane shows in the tiled view it was sized against; any other client's
    /// zoom is left alone. All events seal in one transaction.
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
                top_left_cell,
                is_pinned,
            } => {
                return self.handle_new_floating_pane(
                    command_id,
                    command_source,
                    command_args,
                    size,
                    top_left_cell,
                    is_pinned,
                );
            }
        };
        let acting_session = self.resolve_acting_session(command_source)?;
        let new_pane_target = self.resolve_new_pane_source(
            source_pane_id,
            tab_id,
            command_args.client_id,
            command_source,
            acting_session,
        )?;

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
            pane_sizing,
        )?;

        // Solve the candidate against that tab size, with a stack that holds
        // the new pane showing it, to size the new pane. A solve that gives the
        // new pane no content rect rejects before any mutation.
        let new_pane_shown_layout_tree =
            candidate_layout_tree.build_shown_layout_tree(&[new_pane_id]);
        let new_pane_content_rect = find_content_rect(
            &solve_layout_with_mode(
                new_pane_shown_layout_tree
                    .as_ref()
                    .unwrap_or(&candidate_layout_tree),
                LayoutMode::Tiled,
                Rect::from_size_at_origin(tab_size),
                pane_sizing,
            ),
            new_pane_id,
        )
        .ok_or_else(|| Rejection::from_reason(RejectReason::InvalidState))?;
        let new_pane_pty_size = compute_pty_size(new_pane_content_rect);
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
            self.config.pane_clipboard_writes.should_allow_writes,
        )?;

        // The child is live — commit all session state through the pure op: it
        // switches the designated client onto the tab (if not already there),
        // registers the pane, swaps in the split, and focuses it. It
        // returns the previous tab of any client it moved, for the reflow below.
        let (previous_tab_id, mut emitted_events) = pane_ops::commit_new_pane(
            session,
            new_pane_id,
            new_pane_target.tab_id,
            candidate_layout_tree,
            designated_client_id,
            new_pane_spec,
        );

        // Register the live pane and its size, and create the terminal engine
        // that receives the child's output.
        self.park_pane_pty(new_pane_id, new_pane_pty_size);
        // Announce the new pane's size — PaneCreated carries none.
        emitted_events.push(Event::PtyResized(PtyResized {
            pane_id: new_pane_id,
            pty_size: new_pane_pty_size,
        }));

        // Reflow the target tab's live panes to the smallest rect a viewer
        // draws each in. The pane just spawned resizes only when that rect
        // differs from the size it was spawned at.
        self.reflow_tab_if_viewed(
            pty_backend.as_ref(),
            new_pane_target.session_id,
            new_pane_target.tab_id,
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

        Ok(self.commit_events(command_id, emitted_events))
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
    /// koshi's in-session environment for `session_id`,
    /// `designated_client_id` and `new_pane_id`, with the stand-in clipboard
    /// tools when `should_add_clipboard_tools` is `true`
    /// ([`add_koshi_environment`]). A launch failure is
    /// [`Self::spawn_child`]'s rejection.
    #[allow(clippy::too_many_arguments)]
    fn launch_new_pane_child(
        pty_backend: &dyn PtyBackend,
        command_args: &NewPaneArgs,
        mut spawn_spec: SpawnSpec,
        new_pane_id: PaneId,
        session_id: SessionId,
        designated_client_id: Option<ClientId>,
        pty_size: PtySize,
        should_add_clipboard_tools: bool,
    ) -> Result<NewPaneSpec, Rejection> {
        let new_pane_spec = NewPaneSpec {
            working_directory: spawn_spec.working_directory.clone(),
            spawn_spec: command_args
                .spawn_spec
                .is_some()
                .then(|| spawn_spec.clone()),
        };
        add_koshi_environment(
            &mut spawn_spec.environment_variables,
            session_id,
            designated_client_id,
            new_pane_id,
            koshi_paths::resolve_runtime_directory().as_deref(),
            should_add_clipboard_tools,
        );
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
    /// pane, store the designated client's position, and focus the
    /// pane for the designated client, drawn above its other floating panes.
    /// The new pane then takes that client's input. A launch failure commits
    /// nothing and rejects. No other client's focus moves. Every attached
    /// client gains a view of the pane.
    ///
    /// Emits [`Event::PaneCreated`] with `tab_id: None`, the
    /// designated client's [`Event::PaneFocused`] with `tab_id: None`, then
    /// [`Event::PtyResized`] with the spawn size.
    fn handle_new_floating_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &NewPaneArgs,
        size: Option<FloatingPaneSize>,
        top_left_cell: Option<Point>,
        is_pinned: bool,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let new_floating_pane_target = self.resolve_new_floating_pane_target(
            command_args.client_id,
            size,
            top_left_cell,
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

        let session = self
            .session_by_id
            .get_mut(&session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;

        let new_pane_id = PaneId::new();
        let new_pane_spec = Self::launch_new_pane_child(
            pty_backend.as_ref(),
            command_args,
            spawn_spec,
            new_pane_id,
            session_id,
            new_floating_pane_target.designated_client_id,
            new_floating_pane_target.pty_size,
            self.config.pane_clipboard_writes.should_allow_writes,
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

        self.park_pane_pty(new_pane_id, new_floating_pane_target.pty_size);
        emitted_events.push(Event::PtyResized(PtyResized {
            pane_id: new_pane_id,
            pty_size: new_floating_pane_target.pty_size,
        }));
        Ok(self.commit_events(command_id, emitted_events))
    }

    /// Handle [`Command::ClosePane`]: tear the pane out of its session and
    /// kill its child, in commit-then-kill order — the state removal is
    /// authoritative and immediate, the process kill is best-effort and
    /// off-thread.
    ///
    /// `--force` kills the child at once. Without it, the close requests a
    /// stop and escalates after [`GRACEFUL_TIMEOUT_DURATION`] — or at once
    /// when the stop request cannot be delivered. `--tree` widens the kill to
    /// the child's whole process group ([`compute_pane_kill_policy`]).
    /// The removal itself is the shared cascade behind shell-exit
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
    /// that quits the session also ends every floating pane with a graceful
    /// close ([`Self::end_removed_floating_panes`]).
    pub(super) fn handle_close_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &ClosePaneArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let pane_target = self.resolve_pane_target(
            command_args.pane_id,
            command_source.get_target_client_id(),
            command_source,
            acting_session,
        )?;
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

        let session = self
            .session_by_id
            .get_mut(&pane_target.session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        let kill_policy = compute_pane_kill_policy(
            command_args.should_force_close,
            command_args.should_kill_process_tree,
        );

        let floating_pane_ids = session.floating_set.list_pane_ids();

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
        let mut emitted_events = remove_pane_cascade(session, tab_id, pane_target.pane_id, None);

        // The pane is gone from state; drop its runtime bookkeeping and reflow
        // the survivors into the space it freed.
        self.release_pane_and_reflow(
            pane_target.session_id,
            tab_id,
            pane_target.pane_id,
            pty_backend.as_ref(),
            &mut emitted_events,
        );

        self.kill_pane_off_thread(pane_target.pane_id, kill_policy);
        self.end_removed_floating_panes(
            pane_target.session_id,
            floating_pane_ids,
            compute_pane_kill_policy(false, false),
        );

        Ok(self.commit_events(command_id, emitted_events))
    }

    /// Handle [`Command::ClosePane`] on a floating pane: remove it from the
    /// session and kill its child, in commit-then-kill order.
    ///
    /// The kill policy is picked as for a tiled pane
    /// ([`compute_pane_kill_policy`]). [`remove_floating_pane`] drops
    /// the registry record, the member and every client's view of the pane,
    /// and moves the input of each client that focused the pane; no layout
    /// changes and nothing reflows. Emits [`Event::PaneClosing`] and [`Event::PaneRemoved`]
    /// with `tab_id: None`, then one [`Event::PaneFocused`] for each client
    /// whose input moved.
    fn handle_close_floating_pane(
        &mut self,
        command_id: CommandId,
        command_args: &ClosePaneArgs,
        session_id: SessionId,
        pane_id: PaneId,
    ) -> Result<CommandResult, Rejection> {
        let session = self
            .session_by_id
            .get_mut(&session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        let kill_policy = compute_pane_kill_policy(
            command_args.should_force_close,
            command_args.should_kill_process_tree,
        );

        let emitted_events = remove_floating_pane(session, pane_id);

        self.release_pane_bookkeeping(session_id, pane_id);
        self.kill_pane_off_thread(pane_id, kill_policy);
        Ok(self.commit_events(command_id, emitted_events))
    }

    /// End each pane of `floating_pane_ids` that the session `session_id` does
    /// not hold as a floating pane: release its runtime bookkeeping
    /// ([`Self::release_pane_bookkeeping`]), then kill its child under
    /// `kill_policy` ([`Self::kill_pane_off_thread`]). A last-tab quit removes
    /// every floating pane; a removal that quit nothing leaves every floating
    /// pane in place, and this ends none.
    pub(super) fn end_removed_floating_panes(
        &mut self,
        session_id: SessionId,
        floating_pane_ids: Vec<PaneId>,
        kill_policy: KillPolicy,
    ) {
        for pane_id in floating_pane_ids {
            let is_member = self
                .session_by_id
                .get(&session_id)
                .is_some_and(|session| session.floating_set.has_pane(pane_id));
            if is_member {
                continue;
            }
            self.release_pane_bookkeeping(session_id, pane_id);
            self.kill_pane_off_thread(pane_id, kill_policy);
        }
    }

    /// Remove a pane's live ID, cached size, terminal engine, timed renders,
    /// and every mouse button held in it, with no release written. Close and
    /// drop its output queue ([`Self::remove_pane_output_queue`]). Clear each
    /// attached client's scroll offset and selection for it. Every pane removal
    /// calls this for runtime bookkeeping.
    pub(super) fn release_pane_bookkeeping(&mut self, session_id: SessionId, pane_id: PaneId) {
        self.live_pane_ids.remove(&pane_id);
        self.pty_size_by_pane_id.remove(&pane_id);
        self.terminal_engine_by_pane_id.remove(&pane_id);
        self.pane_animation_deadlines
            .set_pane_render_deadline(pane_id, None);
        self.pane_synchronized_output_deadlines
            .set_pane_render_deadline(pane_id, None);
        self.remove_pane_output_queue(pane_id);
        self.remove_held_mouse_buttons_in_pane(pane_id);
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
    /// Removes the pane's live ID, size cache, and terminal engine, closes its
    /// output queue and drops the output still in it
    /// ([`Self::release_pane_bookkeeping`]), and clears each client's scroll
    /// offset and any highlight in it, then re-solves and resizes the tab that
    /// reclaims the space: the pane's own tab when it survives, else the tab
    /// its viewers moved to (the cascade's `TabFocused`). A tab left with no
    /// viewer has no tab size and keeps its sizes. Each applied resize appends
    /// one [`Event::PtyResized`] to `emitted_events`.
    ///
    /// Shared by [`handle_close_pane`](Self::handle_close_pane) and
    /// [`handle_child_exit`](Self::handle_child_exit). Both callers release any
    /// backend entry after reflow. [`handle_child_exit`](Self::handle_child_exit)
    /// then marks the view of every subscriber changed.
    fn release_pane_and_reflow(
        &mut self,
        session_id: SessionId,
        tab_id: TabId,
        pane_id: PaneId,
        pty_backend: &dyn PtyBackend,
        emitted_events: &mut Vec<Event>,
    ) {
        self.release_pane_bookkeeping(session_id, pane_id);
        self.reflow_tab_or_landing_tab(session_id, tab_id, pty_backend, emitted_events);
    }

    /// Re-solve and resize the tab that reclaims the space a pane left in
    /// `tab_id` ([`Self::reflow_tab_if_viewed`]): `tab_id` when the session
    /// still holds it, else the tab the first [`Event::TabFocused`] in
    /// `emitted_events` names, where the closed tab's viewers moved. With
    /// neither, nothing reflows.
    fn reflow_tab_or_landing_tab(
        &mut self,
        session_id: SessionId,
        tab_id: TabId,
        pty_backend: &dyn PtyBackend,
        emitted_events: &mut Vec<Event>,
    ) {
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
    /// A local backend's watcher sees a child end before reporting its exit,
    /// and on Unix keeps it a zombie until `kill_pane` reaps it. Resume can also
    /// report a carried pane that the backend does not drive.
    /// [`apply_child_exit`] removes a tiled pane — its tab may close and the
    /// last tab quit, which also ends every floating pane — and its runtime
    /// bookkeeping is released while the survivors reflow. A floating pane is
    /// removed at once through [`remove_floating_pane`], and nothing reflows.
    /// An exit for a pane no session holds, or whose record no tab holds,
    /// changes no session, and `kill_pane` still releases any backend entry
    /// for it.
    ///
    /// Releasing a removed pane's bookkeeping clears its live ID, size cache,
    /// and terminal engine. If the backend still holds the pane, `kill_pane`
    /// removes its entry. A local backend drops the writer, joins the watcher,
    /// and closes the terminal master; a supervisor backend sends a kill request
    /// to the helper. An absent pane returns `UnknownPane` without a kill
    /// request. A local child that ended receives no leader signal.
    ///
    /// When the removal leaves the session running, the exit posts the text
    /// `format_pane_exit_notice` gives as one `Error` to the session's
    /// notification queue ([`Self::post_session_notification`]), such as
    /// `pane …a1b2 killed by signal 9`. An exit that quits the session posts
    /// nothing, and neither does an exit for a pane no session holds, such as
    /// a pane a command already closed.
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
        // changes no session; any backend entry still held for it is released.
        let Some(session_id) = self
            .get_session_for_pane(pane_id)
            .map(|session| session.session_id)
        else {
            let _ = self.get_pty_backend().kill_pane(pane_id, KillPolicy::Force);
            return Vec::new();
        };

        // The shared backend is cloned before the session is borrowed; the
        // pane's PTY entry is released through the clone.
        let pty_backend = Arc::clone(self.get_pty_backend());

        let session = self
            .session_by_id
            .get_mut(&session_id)
            .expect("session located above");
        let is_command_pane = session
            .panes
            .get_pane_record_by_id(pane_id)
            .is_some_and(|pane_record| pane_record.spawn_spec.is_some());
        let pane_exit_notice = format_pane_exit_notice(pane_id, exit_status, is_command_pane);
        let tab_id = match Self::resolve_pane_tab_id(session, pane_id) {
            Ok(Some(tab_id)) => tab_id,
            // A floating pane leaves at once: `PaneProcessExited`, then its
            // removal from the registry, the floating set and every client's
            // view, then a `PaneFocused` for each client whose input it held.
            // No layout collapses and no tab closes.
            Ok(None) => {
                let mut emitted_events = vec![Event::PaneProcessExited(pane_exit)];
                emitted_events.extend(remove_floating_pane(session, pane_id));
                self.release_pane_bookkeeping(session_id, pane_id);
                let _ = pty_backend.kill_pane(pane_id, KillPolicy::Force);
                if let Some(pane_exit_notice) = pane_exit_notice {
                    self.post_session_notification(
                        session_id,
                        NotificationSeverity::Error,
                        &pane_exit_notice,
                    );
                }
                return emitted_events;
            }
            // No tab's layout holds the pane and it does not float: its record
            // is an `OrphanedPaneRecord`. The exit changes no session; any
            // backend entry still held for it is released.
            Err(_) => {
                let _ = pty_backend.kill_pane(pane_id, KillPolicy::Force);
                return Vec::new();
            }
        };

        let floating_pane_ids = session.floating_set.list_pane_ids();
        // `PaneProcessExited`, then the removal cascade.
        let mut emitted_events = apply_child_exit(session, tab_id, pane_exit);

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
        // sends no leader signal after its watcher saw the child end. An
        // undriven carried pane has no backend entry and returns UnknownPane.
        let _ = pty_backend.kill_pane(pane_id, KillPolicy::Force);
        self.end_removed_floating_panes(
            session_id,
            floating_pane_ids,
            compute_pane_kill_policy(false, false),
        );
        let has_quit_session = emitted_events
            .iter()
            .any(|emitted_event| matches!(emitted_event, Event::Quit(_)));
        if !has_quit_session {
            if let Some(pane_exit_notice) = pane_exit_notice {
                self.post_session_notification(
                    session_id,
                    NotificationSeverity::Error,
                    &pane_exit_notice,
                );
            }
        }

        emitted_events
    }

    /// Move one border of a pane by up to a signed cell count, then resize
    /// the affected PTYs. Returns the result and, for a tiled pane, the cells
    /// the border moved; `None` for a floating pane.
    ///
    /// The border that moves is resolved by the layout crate's resize
    /// transaction: a positive `command_args.resize_amount_cells` grows the pane toward
    /// `command_args.direction` with the adjacent sibling donating the cells, a
    /// negative one shrinks it with that sibling gaining them. The border
    /// moves as far as the donor's minimum size allows: in an `80`-column tab
    /// split `40`/`40`, the right pane grown left by `100` grows by `36`, and
    /// its neighbor keeps the `4` columns of its minimum. A pane with no
    /// border on the named side — it touches the tab edge there — moves its
    /// opposite border in the same visual direction instead. The target pane's
    /// tab must be viewed by at least one attached client: the tab is solved
    /// against that tab size ([`Session::get_tab_size`]), and the donating
    /// side's spare cells are measured against it. A tab no viewer contributes
    /// a pane area to rejects.
    /// On success the tab's tree is swapped in, [`Event::LayoutChanged`] is
    /// emitted, and every live PTY whose solved size changed is resized through
    /// the shared reflow path, one [`Event::PtyResized`] each. The resizing
    /// client's zoom drops: the client the command source names
    /// ([`CommandSource::get_target_client_id`]), else the issuing client. Any
    /// other client's zoom stands.
    ///
    /// A border that cannot move one cell changes nothing: it is
    /// [`RejectReason::MinimumSize`] when the donor holds no cell above its
    /// minimum size, and [`RejectReason::InvalidState`] when the layout holds
    /// the border in place ([`Self::resize_rejection`]).
    ///
    /// A floating pane has no sibling: it resizes through
    /// [`Self::resolve_floating_pane_resize`] and
    /// [`Self::handle_resize_floating_pane`].
    pub(super) fn handle_resize_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &ResizePaneArgs,
    ) -> Result<(CommandResult, Option<u16>), Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let pane_target = self.resolve_pane_target(
            command_args.pane_id,
            command_source.get_target_client_id(),
            command_source,
            acting_session,
        )?;
        let Some(tab_id) = pane_target.tab_id else {
            let floating_pane_resize =
                self.resolve_floating_pane_resize(command_args, command_source, &pane_target)?;
            return self
                .handle_resize_floating_pane(command_id, floating_pane_resize)
                .map(|command_result| (command_result, None));
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
        let (resized_layout_tree, moved_cell_count) = resize_layout_with_sizing(
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
        let resizing_client_id = command_source
            .get_target_client_id()
            .or(command_source.get_client_id());

        // Resizing drops the zoom of the resizing client, and of that client
        // only: the client the command source names, else the issuing client.
        // It returns to the tiled view and sees the moved border. Another
        // client zoomed on a pane of this tab keeps its zoom.
        if let Some(client) =
            resizing_client_id.and_then(|client_id| session.clients.get_client_mut_by_id(client_id))
        {
            client.clear_zoom(tab_id);
        }
        let solve_events = session.update_tab_layout(tab_id, resized_layout_tree);

        // The border moved: resize each live PTY whose size changed.
        let mut emitted_events = vec![Event::LayoutChanged(LayoutChanged { tab_id })];
        emitted_events.extend(solve_events);
        self.reflow_tab_if_viewed(
            pty_backend.as_ref(),
            pane_target.session_id,
            tab_id,
            &mut emitted_events,
        );

        Ok((
            self.commit_events(command_id, emitted_events),
            Some(moved_cell_count),
        ))
    }

    /// Apply a floating pane resize that
    /// [`Self::resolve_floating_pane_resize`] resolved: store the pane's
    /// desired size and the acting client's top-left cell as a moved cell,
    /// then re-solve every floating pane and resize each PTY whose size
    /// changed ([`Self::reflow_floating_panes`]). A client that pinned the pane
    /// keeps its pinned cell. No other client's view changes.
    ///
    /// Emits one [`Event::PtyResized`] per resized PTY.
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
            top_left_cell,
        } = floating_pane_resize;
        let pty_backend = Arc::clone(self.get_pty_backend());
        let session = self
            .session_by_id
            .get_mut(&session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        Self::require_client(session, client_id)?;

        session
            .floating_set
            .update_member_desired_size(pane_id, desired_size);
        if let (Some(top_left_cell), Some(client)) = (
            top_left_cell,
            session.clients.get_client_mut_by_id(client_id),
        ) {
            let _ = client.set_floating_pane_position(pane_id, top_left_cell);
        }

        let mut emitted_events = Vec::new();
        self.reflow_floating_panes(pty_backend.as_ref(), session_id, &mut emitted_events);
        Ok(self.commit_events(command_id, emitted_events))
    }

    /// Handle [`Command::MovePane`]: swap the selected pane with its visible
    /// neighbor in `command_args.direction` and commit the swap at once.
    ///
    /// - The neighbor comes from the issuing client's solved layout.
    /// - The swap commits through the same path as a same-tab
    ///   [`Command::PlacePane`] swap: it clears the issuing client's zoom on
    ///   the tab, emits `PanePlacementCommitted` then `LayoutChanged`, and
    ///   reflows the tab's live PTYs. No focus moves.
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
        )?;
        let tiled_placement_target = TiledPlacementTarget::Swap { target_pane_id };
        let place_pane_args = PlacePaneArgs {
            source_pane_id: Some(pane_target.pane_id),
            placement_target: PanePlacementTarget::Tiled(tiled_placement_target.clone()),
        };
        let placement_target = PlacePaneTarget {
            session_id: pane_target.session_id,
            source_pane_id: pane_target.pane_id,
            source_tab_id: pane_target.tab_id,
            destination_tab_id: pane_target.tab_id,
            client_id: pane_target.client_id,
        };
        self.apply_place_pane_within_tab(
            command_id,
            &place_pane_args,
            &tiled_placement_target,
            placement_target,
            None,
        )
    }

    /// Run the pane placement that `client_id` confirmed from its preview
    /// ([`ConfirmPanePlacement`](koshi_ipc::protocol::IpcRequestKind::ConfirmPanePlacement)):
    /// a [`Command::PlacePane`] of `source_pane_id` to
    /// `tiled_placement_target` from the client's key binding, checked as
    /// [`Self::dispatch`] checks a command. The placement commits only when the
    /// destination tab, as the client draws it after the placement, equals
    /// `expected_placed_tab_layout`
    /// ([`Self::validate_expected_placed_tab_layout`]).
    ///
    /// An applied placement records `command_id` as the client's last applied
    /// placement ([`Client::set_last_applied_placement_command_id`]). A
    /// refused placement changes nothing, and a
    /// [`CommandRejected`](koshi_ipc::event::SessionEvent::CommandRejected)
    /// naming `command_id`, `fired_action`, the reason and the hint is
    /// delivered to the client ([`Self::send_command_rejection`]). `fired_action`
    /// is the action the client's key fired to confirm, or `None` for a mouse
    /// drop.
    pub(crate) fn confirm_pane_placement(
        &mut self,
        client_id: ClientId,
        command_id: CommandId,
        source_pane_id: PaneId,
        tiled_placement_target: TiledPlacementTarget,
        expected_placed_tab_layout: &PlacedTabLayout,
        fired_action: Option<ActionReference>,
    ) -> CommandResult {
        let command_source = CommandSource::from_key_binding(client_id);
        let place_pane_args = PlacePaneArgs {
            source_pane_id: Some(source_pane_id),
            placement_target: PanePlacementTarget::Tiled(tiled_placement_target),
        };
        let envelope = CommandEnvelope::from_parts(
            command_id,
            command_source.clone(),
            Command::PlacePane(place_pane_args.clone()),
        );
        let outcome = self.validate_command(&envelope).and_then(|()| {
            self.apply_place_pane(
                command_id,
                &command_source,
                &place_pane_args,
                Some(expected_placed_tab_layout),
            )
        });
        match outcome {
            Ok(command_result) => {
                if let Some(client) = self.get_client_mut(client_id) {
                    client.set_last_applied_placement_command_id(command_id);
                }
                command_result
            }
            Err(rejection) => {
                self.send_command_rejection(
                    client_id,
                    command_id,
                    fired_action,
                    rejection.reason,
                    rejection.help.clone(),
                );
                Self::reject_command(command_id, rejection)
            }
        }
    }

    /// Handle [`Command::PlacePane`]. A tiled target installs one prepared
    /// placement within one tab or across two tabs, then reflows every tab
    /// whose viewers change. A tiled target from a floating source pane docks
    /// the pane ([`Self::apply_dock_pane`]). A
    /// [`PanePlacementTarget::Floating`] target undocks the pane
    /// ([`Self::apply_undock_pane`]). Every fallible layout check runs before
    /// the session commit.
    ///
    /// Within one tab, a floating pane that takes a client's input keeps it,
    /// and only a focus the placement collapses moves
    /// ([`Self::apply_place_pane_within_tab`]). Across two tabs, the acting
    /// client moves to the destination tab and focuses the placed pane, which
    /// takes its input ([`commit_cross_tab_placement`]).
    ///
    /// With `expected_placed_tab_layout` set, a tiled target commits only when
    /// the destination tab, as the acting client draws it after the
    /// placement, equals it ([`Self::validate_expected_placed_tab_layout`]).
    pub(super) fn apply_place_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &PlacePaneArgs,
        expected_placed_tab_layout: Option<&PlacedTabLayout>,
    ) -> Result<CommandResult, Rejection> {
        let tiled_placement_target = match &command_args.placement_target {
            PanePlacementTarget::Tiled(tiled_placement_target) => tiled_placement_target,
            PanePlacementTarget::Floating { top_left_cell } => {
                return self.apply_undock_pane(
                    command_id,
                    command_source,
                    command_args,
                    *top_left_cell,
                );
            }
        };
        let acting_session = self.resolve_acting_session(command_source)?;
        let placement_target = match self.resolve_tiled_placement_target(
            command_args,
            tiled_placement_target,
            command_source,
            acting_session,
        )? {
            ResolvedTiledPlacement::TiledSource(placement_target) => placement_target,
            ResolvedTiledPlacement::FloatingSource(dock_target) => {
                return self.apply_dock_pane(
                    command_id,
                    command_args,
                    tiled_placement_target,
                    dock_target,
                    expected_placed_tab_layout,
                );
            }
        };
        if placement_target.source_tab_id == placement_target.destination_tab_id {
            return self.apply_place_pane_within_tab(
                command_id,
                command_args,
                tiled_placement_target,
                placement_target,
                expected_placed_tab_layout,
            );
        }
        self.apply_place_pane_across_tabs(
            command_id,
            command_args,
            tiled_placement_target,
            placement_target,
            expected_placed_tab_layout,
        )
    }

    /// Apply a checked swap, insertion or stack of the source pane into another
    /// tab: the layout crate builds both trees, [`commit_cross_tab_placement`]
    /// installs them, and the destination tab, the source tab or the tab its
    /// viewers land on, and each other tab a moving client left, reflows. An
    /// insertion or a stack of the source tab's only pane closes that tab.
    ///
    /// Both trees are checked against the tab sizes they take once the moving
    /// clients switch tabs ([`compute_placement_tab_sizes`]): the acting
    /// client and every client whose input is on the source pane move to the
    /// destination tab, and every client whose input is on the swapped pane
    /// moves to the source tab.
    fn apply_place_pane_across_tabs(
        &mut self,
        command_id: CommandId,
        command_args: &PlacePaneArgs,
        tiled_placement_target: &TiledPlacementTarget,
        placement_target: PlacePaneTarget,
        expected_placed_tab_layout: Option<&PlacedTabLayout>,
    ) -> Result<CommandResult, Rejection> {
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
        let is_source_tab_closing =
            !matches!(tiled_placement_target, TiledPlacementTarget::Swap { .. })
                && source_tree.list_leaf_pane_ids() == vec![placement_target.source_pane_id];
        let landing_tab_id = if is_source_tab_closing {
            Some(
                tab_ops::find_nearest_surviving_tab(session, placement_target.source_tab_id)
                    .ok_or_else(|| Rejection::from_reason(RejectReason::TargetGone))?,
            )
        } else {
            None
        };
        let swapped_pane_id = match tiled_placement_target {
            TiledPlacementTarget::Swap { target_pane_id } => Some(*target_pane_id),
            TiledPlacementTarget::Split { .. } | TiledPlacementTarget::Stack { .. } => None,
        };
        let placement_tab_sizes = compute_placement_tab_sizes(
            session,
            placement_target.source_pane_id,
            placement_target.destination_tab_id,
            swapped_pane_id,
            placement_target.client_id,
        );
        Self::require_client(session, placement_target.client_id)?;
        let destination_tab_size = require_placement_destination_tab_size(placement_tab_sizes)?;
        let destination_tab_rect = Rect::from_size_at_origin(destination_tab_size);
        let layout_target = PlacementTarget::from_tiled_placement_target(tiled_placement_target);
        let destination_tree = session
            .tabs
            .get(&placement_target.destination_tab_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetGone))?
            .get_layout_tree();
        let prepared_placement = koshi_layout::placement::place_pane_across_tabs(
            source_tree,
            placement_target.source_pane_id,
            destination_tree,
            &layout_target,
            destination_tab_rect,
            pane_sizing,
        )
        .map_err(|placement_error| Self::build_placement_rejection(&placement_error))?;
        if let (Some(placed_source_tree), Some(source_tab_size)) = (
            prepared_placement.source_tree.as_ref(),
            placement_tab_sizes.source_tab_size,
        ) {
            let source_tab_rect = Rect::from_size_at_origin(source_tab_size);
            if !is_layout_within_rect(placed_source_tree, source_tab_rect, pane_sizing) {
                return Err(Rejection::from_reason_and_help(
                    RejectReason::InvalidState,
                    "pane placement does not fit the source tab",
                ));
            }
        }
        Self::validate_expected_placed_tab_layout(
            session,
            placement_target.client_id,
            placement_target.destination_tab_id,
            &prepared_placement.destination_tree,
            placement_target.source_pane_id,
            destination_tab_size,
            expected_placed_tab_layout,
        )?;

        let pty_backend = Arc::clone(self.get_pty_backend());
        let mut emitted_events = vec![Event::PanePlacementCommitted(PanePlacementCommitted {
            command_id,
            source_pane_id: placement_target.source_pane_id,
            source_tab_id: Some(placement_target.source_tab_id),
            destination_tab_id: Some(placement_target.destination_tab_id),
            placement_target: command_args.placement_target.clone(),
        })];
        {
            let session = self
                .session_by_id
                .get_mut(&placement_target.session_id)
                .ok_or_else(|| Rejection::from_reason(RejectReason::TargetGone))?;
            emitted_events.extend(
                commit_cross_tab_placement(
                    session,
                    placement_target.source_tab_id,
                    placement_target.destination_tab_id,
                    placement_target.source_pane_id,
                    prepared_placement,
                    placement_target.client_id,
                )
                .map_err(|_| Rejection::from_reason(RejectReason::InvalidState))?,
            );
        }

        let mut tab_ids_to_reflow = vec![placement_target.destination_tab_id];
        if let Some(landing_tab_id) = landing_tab_id {
            if !tab_ids_to_reflow.contains(&landing_tab_id) {
                tab_ids_to_reflow.push(landing_tab_id);
            }
        }
        if !is_source_tab_closing && !tab_ids_to_reflow.contains(&placement_target.source_tab_id) {
            tab_ids_to_reflow.push(placement_target.source_tab_id);
        }
        for tab_id in list_tab_ids_switched_away_from(&emitted_events) {
            if tab_id != placement_target.source_tab_id && !tab_ids_to_reflow.contains(&tab_id) {
                tab_ids_to_reflow.push(tab_id);
            }
        }
        for tab_id in tab_ids_to_reflow {
            self.reflow_tab_if_viewed(
                pty_backend.as_ref(),
                placement_target.session_id,
                tab_id,
                &mut emitted_events,
            );
        }

        Ok(self.commit_events(command_id, emitted_events))
    }

    /// Undock the tiled or stacked pane `undock_target.source_pane_id`: make it
    /// a floating pane of its session. The target comes from
    /// [`Self::resolve_undock_pane_target`].
    ///
    /// The pane asks for the cells of the rect the acting client draws it in
    /// ([`find_lifted_floating_pane_rect`]), a stack member the acting client
    /// draws on its header asks for the rect it opens into, and a pane the
    /// acting client does not draw asks for [`DEFAULT_FLOATING_PANE_SIZE`]. The
    /// acting client stores `top_left_cell` when it is set, else the rect's
    /// top-left cell, else its default placement. A `40x19` pane drawn at
    /// `(40, 0)` floats as `Cells(40)` by `Cells(19)` at `(40, 0)`, or at
    /// `(5, 3)` with `top_left_cell` `(5, 3)`.
    ///
    /// The pane keeps its id, its PTY and its terminal state.
    /// [`commit_undock_placement`] adds the member at
    /// [`Session::compute_new_floating_pane_size`], takes the leaf out of its
    /// tab, closes a tab left with no pane, and shows the pane for the acting
    /// client and for every other client whose input is on the pane, each when
    /// it can draw floating panes. Such another client holds the pane at the
    /// top-left cell of the rect it drew the pane in. Every other client holds
    /// the pane minimized at its default placement.
    ///
    /// Every attached client gains a view of the pane.
    ///
    /// Emits [`Event::PanePlacementCommitted`] with `destination_tab_id: None`,
    /// then the commit's events, then one [`Event::PtyResized`] per pane the
    /// source tab, or the tab its viewers land on, resizes
    /// ([`Self::reflow_tab_or_landing_tab`]), then one per floating pane whose
    /// size changed, the undocked pane included
    /// ([`Self::reflow_floating_panes`]).
    fn apply_undock_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &PlacePaneArgs,
        top_left_cell: Option<Point>,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let undock_target =
            self.resolve_undock_pane_target(command_args, command_source, acting_session)?;
        let pane_sizing = self.get_pane_sizing();
        let pty_backend = Arc::clone(self.get_pty_backend());
        let session = self
            .session_by_id
            .get_mut(&undock_target.session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetGone))?;

        let lifted_floating_pane_rect = find_lifted_floating_pane_rect(
            session,
            undock_target.client_id,
            undock_target.source_tab_id,
            undock_target.source_pane_id,
        );
        let (desired_size, lifted_position) = match lifted_floating_pane_rect {
            Some((desired_size, lifted_top_left_cell)) => (
                desired_size,
                FloatingPanePosition::Moved(lifted_top_left_cell),
            ),
            None => (DEFAULT_FLOATING_PANE_SIZE, FloatingPanePosition::Default),
        };
        let acting_position = match top_left_cell {
            Some(top_left_cell) => FloatingPanePosition::Moved(top_left_cell),
            None => lifted_position,
        };
        let floating_member = FloatingMember {
            pane_id: undock_target.source_pane_id,
            desired_size,
            solved_size: session
                .compute_new_floating_pane_size(desired_size, pane_sizing.minimum_size),
        };
        let mut emitted_events = vec![Event::PanePlacementCommitted(PanePlacementCommitted {
            command_id,
            source_pane_id: undock_target.source_pane_id,
            source_tab_id: Some(undock_target.source_tab_id),
            destination_tab_id: None,
            placement_target: command_args.placement_target.clone(),
        })];
        emitted_events.extend(
            commit_undock_placement(
                session,
                undock_target.source_tab_id,
                floating_member,
                (undock_target.client_id, acting_position),
            )
            .map_err(|placement_commit_error| {
                Rejection::from_reason_and_help(
                    RejectReason::InvalidState,
                    &placement_commit_error.to_string(),
                )
            })?,
        );

        self.reflow_tab_or_landing_tab(
            undock_target.session_id,
            undock_target.source_tab_id,
            pty_backend.as_ref(),
            &mut emitted_events,
        );
        self.reflow_floating_panes(
            pty_backend.as_ref(),
            undock_target.session_id,
            &mut emitted_events,
        );
        Ok(self.commit_events(command_id, emitted_events))
    }

    /// Dock the floating pane `dock_target.source_pane_id` into
    /// `dock_target`'s destination tab at `tiled_placement_target`: an
    /// insertion beside a pane, a group or the whole tab, or a stack onto a
    /// tiled pane. The target comes from [`Self::resolve_tiled_placement_target`].
    ///
    /// The destination tree is built against the destination tab's size once
    /// the acting client and every client whose input is on the pane view it
    /// ([`compute_placement_tab_sizes`]) with [`place_floating_pane_in_tab`],
    /// then [`commit_dock_placement`] takes the pane out of the floating panes
    /// and installs the tree. The pane keeps its id, its PTY and its terminal
    /// state.
    ///
    /// A tree that does not fit the destination is refused with
    /// [`RejectReason::InvalidState`] `tab <name> has no room for another
    /// pane`; every other layout refusal is
    /// [`Self::build_placement_rejection`]'s. A refusal changes nothing.
    ///
    /// Every attached client's floating view drops the pane.
    ///
    /// Emits [`Event::PanePlacementCommitted`] with `source_tab_id: None`,
    /// then the commit's events, then one [`Event::PtyResized`] per pane the
    /// destination tab resizes, the docked pane included, then one per pane
    /// each tab that a moving client left resizes
    /// ([`Self::reflow_tab_if_viewed`]).
    fn apply_dock_pane(
        &mut self,
        command_id: CommandId,
        command_args: &PlacePaneArgs,
        tiled_placement_target: &TiledPlacementTarget,
        dock_target: DockPaneTarget,
        expected_placed_tab_layout: Option<&PlacedTabLayout>,
    ) -> Result<CommandResult, Rejection> {
        let pane_sizing = self.get_pane_sizing();
        let pty_backend = Arc::clone(self.get_pty_backend());
        let session = self
            .session_by_id
            .get_mut(&dock_target.session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetGone))?;
        let destination_tab = session
            .tabs
            .get(&dock_target.destination_tab_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetGone))?;
        Self::require_client(session, dock_target.client_id)?;
        let destination_tab_size =
            require_placement_destination_tab_size(compute_placement_tab_sizes(
                session,
                dock_target.source_pane_id,
                dock_target.destination_tab_id,
                None,
                dock_target.client_id,
            ))?;
        let destination_tree = place_floating_pane_in_tab(
            destination_tab.get_layout_tree(),
            dock_target.source_pane_id,
            &PlacementTarget::from_tiled_placement_target(tiled_placement_target),
            Rect::from_size_at_origin(destination_tab_size),
            pane_sizing,
        )
        .map_err(|placement_error| match placement_error {
            PlacementError::DestinationTooSmall { .. } => Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                &format!(
                    "tab {} has no room for another pane",
                    destination_tab.get_tab_name()
                ),
            ),
            other_placement_error => Self::build_placement_rejection(&other_placement_error),
        })?;
        Self::validate_expected_placed_tab_layout(
            session,
            dock_target.client_id,
            dock_target.destination_tab_id,
            &destination_tree,
            dock_target.source_pane_id,
            destination_tab_size,
            expected_placed_tab_layout,
        )?;

        let mut emitted_events = vec![Event::PanePlacementCommitted(PanePlacementCommitted {
            command_id,
            source_pane_id: dock_target.source_pane_id,
            source_tab_id: None,
            destination_tab_id: Some(dock_target.destination_tab_id),
            placement_target: command_args.placement_target.clone(),
        })];
        emitted_events.extend(
            commit_dock_placement(
                session,
                dock_target.source_pane_id,
                dock_target.destination_tab_id,
                destination_tree,
                dock_target.client_id,
            )
            .map_err(|placement_commit_error| {
                Rejection::from_reason_and_help(
                    RejectReason::InvalidState,
                    &placement_commit_error.to_string(),
                )
            })?,
        );

        let mut tab_ids_to_reflow = vec![dock_target.destination_tab_id];
        for tab_id in list_tab_ids_switched_away_from(&emitted_events) {
            if !tab_ids_to_reflow.contains(&tab_id) {
                tab_ids_to_reflow.push(tab_id);
            }
        }
        for tab_id in tab_ids_to_reflow {
            self.reflow_tab_if_viewed(
                pty_backend.as_ref(),
                dock_target.session_id,
                tab_id,
                &mut emitted_events,
            );
        }
        Ok(self.commit_events(command_id, emitted_events))
    }

    /// Apply a checked swap, insertion or stack within one tab. A swap or a
    /// stack whose target is the source pane commits no events and does not
    /// solve or resize the tab.
    ///
    /// When the source pane lands collapsed to a stack header in the acting
    /// client's own view of the tab ([`LayoutNode::is_pane_collapsed`] with
    /// its focus history), the acting client focuses it there, which opens it
    /// in that view. The tab records it as its most-recent focus, and an
    /// [`Event::PaneFocused`] names the pane the client focused in the tab
    /// before as `previous_pane_id`. A floating pane that takes the acting
    /// client's input keeps it. `place-pane --pane B --stack-onto A` from a
    /// client focused on `A` in `A | B` gives the stack `[A, B]` with `A`
    /// active in the tab's tree, and `B` open and focused for that client. No
    /// other client's focus moves.
    ///
    /// A swap or a stack onto the source pane with `expected_placed_tab_layout`
    /// set is checked against the tab as it is, then commits no events.
    fn apply_place_pane_within_tab(
        &mut self,
        command_id: CommandId,
        command_args: &PlacePaneArgs,
        tiled_placement_target: &TiledPlacementTarget,
        placement_target: PlacePaneTarget,
        expected_placed_tab_layout: Option<&PlacedTabLayout>,
    ) -> Result<CommandResult, Rejection> {
        let is_source_pane_target = match tiled_placement_target {
            TiledPlacementTarget::Swap { target_pane_id }
            | TiledPlacementTarget::Stack { target_pane_id } => {
                *target_pane_id == placement_target.source_pane_id
            }
            TiledPlacementTarget::Split { .. } => false,
        };
        if is_source_pane_target && expected_placed_tab_layout.is_none() {
            return Ok(self.commit_events(command_id, Vec::new()));
        }
        let layout_target = PlacementTarget::from_tiled_placement_target(tiled_placement_target);
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
            placement_target.source_pane_id,
            &layout_target,
            tab_rect,
            pane_sizing,
        )
        .map_err(|placement_error| Self::build_placement_rejection(&placement_error))?;
        Self::validate_expected_placed_tab_layout(
            session,
            placement_target.client_id,
            placement_target.source_tab_id,
            &placed_layout_tree,
            placement_target.source_pane_id,
            tab_size,
            expected_placed_tab_layout,
        )?;
        if is_source_pane_target {
            return Ok(self.commit_events(command_id, Vec::new()));
        }
        if let Some(client) = session
            .clients
            .get_client_mut_by_id(placement_target.client_id)
        {
            client.clear_zoom(placement_target.source_tab_id);
        }
        let solve_events =
            session.update_tab_layout(placement_target.source_tab_id, placed_layout_tree);
        let is_placed_pane_collapsed = session
            .clients
            .get_client_by_id(placement_target.client_id)
            .zip(session.tabs.get(&placement_target.source_tab_id))
            .is_some_and(|(client, tab_state)| {
                tab_state.get_layout_tree().is_pane_collapsed(
                    placement_target.source_pane_id,
                    client.list_focus_history(placement_target.source_tab_id),
                )
            });
        let placed_pane_focused_event = if is_placed_pane_collapsed {
            update_client_tab_focus(
                session,
                placement_target.client_id,
                placement_target.source_tab_id,
                placement_target.source_pane_id,
            )
        } else {
            None
        };
        let mut emitted_events = vec![
            Event::PanePlacementCommitted(PanePlacementCommitted {
                command_id,
                source_pane_id: placement_target.source_pane_id,
                source_tab_id: Some(placement_target.source_tab_id),
                destination_tab_id: Some(placement_target.destination_tab_id),
                placement_target: command_args.placement_target.clone(),
            }),
            Event::LayoutChanged(LayoutChanged {
                tab_id: placement_target.source_tab_id,
            }),
        ];
        emitted_events.extend(solve_events);
        emitted_events.extend(placed_pane_focused_event);
        self.reflow_tab_if_viewed(
            pty_backend.as_ref(),
            placement_target.session_id,
            placement_target.source_tab_id,
            &mut emitted_events,
        );
        Ok(self.commit_events(command_id, emitted_events))
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
        Ok(self.commit_events(command_id, Vec::new()))
    }

    /// Refuse a confirmed placement whose destination tab, as the acting
    /// client draws it once the placement put `source_pane_id` in
    /// `placed_layout_tree`, differs from `expected_placed_tab_layout`:
    /// [`RejectReason::InvalidState`] `placement preview is stale; confirm
    /// again`. The drawn tab is [`solve_placed_tab`] of `placed_layout_tree`
    /// at `destination_tab_size`, with the acting client's focus history in
    /// `destination_tab_id` and the session's pane sizing. `None` checks
    /// nothing.
    fn validate_expected_placed_tab_layout(
        session: &Session,
        acting_client_id: ClientId,
        destination_tab_id: TabId,
        placed_layout_tree: &LayoutNode,
        source_pane_id: PaneId,
        destination_tab_size: Size,
        expected_placed_tab_layout: Option<&PlacedTabLayout>,
    ) -> Result<(), Rejection> {
        let Some(expected_placed_tab_layout) = expected_placed_tab_layout else {
            return Ok(());
        };
        let acting_client = Self::require_client(session, acting_client_id)?;
        let placed_tab_solve = solve_placed_tab(
            placed_layout_tree,
            source_pane_id,
            acting_client.list_focus_history(destination_tab_id),
            destination_tab_size,
            session.get_pane_sizing(),
        );
        let placed_tab_layout = PlacedTabLayout {
            tab_id: destination_tab_id,
            tab_size: destination_tab_size,
            layout_solve: placed_tab_solve.layout_solve,
        };
        if placed_tab_layout != *expected_placed_tab_layout {
            return Err(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                "placement preview is stale; confirm again",
            ));
        }
        Ok(())
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

    /// Map a layout [`ResizeError`] onto the command vocabulary's rejection,
    /// naming the pane and the border of the last resize tried:
    /// - a missing pane is [`RejectReason::TargetNotFound`];
    /// - a pane with no neighbor on either side of the axis is
    ///   [`RejectReason::InvalidState`] `<pane> has no left or right border to
    ///   move`, or `top or bottom` for `Up` and `Down`;
    /// - a border whose donor holds no cell above its minimum size, at that
    ///   size or suppressed for lack of room, is
    ///   [`RejectReason::MinimumSize`], unlogged: `<pane> cannot grow left
    ///   here; the pane next to it has no room to give` on a grow, `<pane>
    ///   cannot shrink from the left here; it has no room to give` on a
    ///   shrink;
    /// - a border the layout holds in place is [`RejectReason::InvalidState`],
    ///   unlogged: `<pane> cannot grow left here; the layout holds this border
    ///   in place`, or `<pane> cannot shrink from the left here; the layout
    ///   holds this border in place`.
    ///
    /// A grow names the direction (`left`, `right`, `up`, `down`), a shrink
    /// the border (`left`, `right`, `top`, `bottom`).
    fn resize_rejection(resize_error: &ResizeError) -> Rejection {
        match *resize_error {
            ResizeError::PaneNotFound { .. } => {
                Rejection::from_reason(RejectReason::TargetNotFound)
            }
            ResizeError::NoAdjacentBorder { pane_id, direction } => {
                let axis_border_names = match direction {
                    Direction::Left | Direction::Right => "left or right",
                    Direction::Up | Direction::Down => "top or bottom",
                };
                Rejection::from_reason_and_help(
                    RejectReason::InvalidState,
                    &format!("{pane_id} has no {axis_border_names} border to move"),
                )
            }
            ResizeError::DonorAtMinimumSize {
                pane_id,
                direction,
                is_pane_growing,
            } => {
                let cause = if is_pane_growing {
                    "the pane next to it has no room to give"
                } else {
                    "it has no room to give"
                };
                Rejection::from_unmovable_border(
                    RejectReason::MinimumSize,
                    Self::format_unmovable_border_help(pane_id, direction, is_pane_growing, cause),
                )
            }
            ResizeError::BorderHeld {
                pane_id,
                direction,
                is_pane_growing,
            } => Rejection::from_unmovable_border(
                RejectReason::InvalidState,
                Self::format_unmovable_border_help(
                    pane_id,
                    direction,
                    is_pane_growing,
                    "the layout holds this border in place",
                ),
            ),
        }
    }

    /// The hint for a border of `pane_id` that cannot move one cell:
    /// `<pane> cannot grow <direction> here; <cause>` when `is_pane_growing`,
    /// else `<pane> cannot shrink from the <side> here; <cause>`. `Left`
    /// growing with `the pane next to it has no room to give` gives
    /// `pane-… cannot grow left here; the pane next to it has no room to
    /// give`.
    pub(super) fn format_unmovable_border_help(
        pane_id: PaneId,
        direction: Direction,
        is_pane_growing: bool,
        cause: &str,
    ) -> String {
        if !is_pane_growing {
            let side_name = format_side_name(direction);
            return format!("{pane_id} cannot shrink from the {side_name} here; {cause}");
        }
        let direction_name = match direction {
            Direction::Left => "left",
            Direction::Right => "right",
            Direction::Up => "up",
            Direction::Down => "down",
        };
        format!("{pane_id} cannot grow {direction_name} here; {cause}")
    }

    /// Handle [`Command::FocusPane`]: move the target client's focus to the
    /// target pane, which then takes that client's input. The pane comes out
    /// of [`Self::resolve_focus_target`]. A floating pane goes to
    /// [`Self::handle_focus_floating_pane`]. A tiled pane in the client's
    /// active tab takes the client's input, and every floating pane the
    /// client shows is minimized for it ([`apply_tiled_pane_focus`]).
    ///
    /// A tiled pane must have room in the solve of the client's layout mode
    /// ([`Session::compute_layout_mode_solve`]): one suppressed for lack of
    /// space is [`RejectReason::InvalidState`]. A stack member collapsed to its
    /// header in the client's own view of the tab
    /// ([`LayoutNode::is_pane_collapsed`] with its focus history) is a valid
    /// target: focusing it opens it in that view, the member the client had
    /// open collapses to its header, and the tab's PTYs reflow to what every
    /// viewer now draws. The tab's tree and every other client's view stay as
    /// they are. Zoom follows focus, per client: when the target client has
    /// this tab zoomed, focusing another pane moves its zoom onto that pane —
    /// its zoomed view swaps content and stays on, and no other client's view
    /// moves. Emits [`Event::LayoutChanged`] plus per-pane
    /// [`Event::PtyResized`] when the focus opened a stack member or
    /// retargeted the zoom, then one [`Event::PaneMinimizedChanged`] per
    /// floating pane minimized, then [`Event::PaneFocused`] when the client's
    /// input actually moved, naming as `previous_pane_id` the pane that took it
    /// before; focusing the pane that already takes the client's input
    /// completes with no events. A rejected focus mutates nothing.
    pub(super) fn handle_focus_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &FocusPaneArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let pane_target =
            match Self::resolve_focus_target(command_args, command_source, acting_session)? {
                FocusPaneTarget::Tiled(client_pane_target) => client_pane_target,
                FocusPaneTarget::Floating(floating_pane_view_target) => {
                    return self.handle_focus_floating_pane(command_id, &floating_pane_view_target);
                }
            };

        let pty_backend = Arc::clone(self.get_pty_backend());

        // A tab that no attached client contributes a pane area to is refused.
        let (session, _tab_size) =
            self.resolve_session_and_tab_size(pane_target.session_id, pane_target.tab_id)?;
        // Zoom follows focus and belongs to this client: a zoomed client that
        // focuses another pane zooms that pane, and every other client's view
        // stays as it was. The mode solved and checked below is this client's.
        let client = Self::require_client(session, pane_target.client_id)?;
        let previous_focused_pane_id = client.get_active_focused_pane_id();
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

        // A pane suppressed for lack of space cannot take focus.
        let is_pane_suppressed = session
            .compute_layout_mode_solve(pane_target.tab_id, effective_layout_mode)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?
            .suppressed_pane_ids
            .contains(&pane_target.pane_id);
        if is_pane_suppressed {
            return Err(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                "pane is suppressed; not enough space to show it",
            ));
        }

        // A stack member collapsed in this client's view opens for this client
        // alone: its focus history decides which member its view shows.
        let is_stack_member_opened = client_layout_mode == LayoutMode::Tiled
            && session
                .tabs
                .get(&pane_target.tab_id)
                .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?
                .get_layout_tree()
                .is_pane_collapsed(
                    pane_target.pane_id,
                    client.list_focus_history(pane_target.tab_id),
                );

        if previous_focused_pane_id == Some(pane_target.pane_id)
            && !is_stack_member_opened
            && !is_layout_mode_retargeted
        {
            return Ok(self.commit_events(command_id, Vec::new()));
        }

        // The focus, and this client's zoom with it, moves before the reflow;
        // the reflow solves PTY sizes from what every client now displays.
        let client = Self::require_client_mut(session, pane_target.client_id)?;
        let focus_events = apply_tiled_pane_focus(client, pane_target.tab_id, pane_target.pane_id);
        if let Some(tab_state) = session.tabs.get_mut(&pane_target.tab_id) {
            tab_state.record_focus_mru(pane_target.pane_id);
        }

        // Opening a stack member, the zoom retarget, or both changed what this
        // client draws: announce the new geometry and resize each live PTY
        // whose size changed.
        let mut emitted_events = Vec::new();
        if is_stack_member_opened || is_layout_mode_retargeted {
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
        emitted_events.extend(focus_events);

        Ok(self.commit_events(command_id, emitted_events))
    }

    /// Show the floating pane `floating_pane_view_target` names for its client,
    /// draw it above that client's other floating panes and focus it
    /// ([`Client::focus_floating_pane`]). The pane then takes the client's
    /// input.
    ///
    /// A pane that already takes the client's input completes with no events.
    /// Otherwise one [`Event::PaneMinimizedChanged`] with
    /// `is_minimized: false` reports a pane the client had minimized, and one
    /// [`Event::PaneFocused`] with `tab_id: None` names the pane and, as
    /// `previous_pane_id`, the pane that took the client's input before. Every
    /// other client's view stays as it is.
    fn handle_focus_floating_pane(
        &mut self,
        command_id: CommandId,
        floating_pane_view_target: &FloatingPaneViewTarget,
    ) -> Result<CommandResult, Rejection> {
        let FloatingPaneViewTarget {
            session_id,
            client_id,
            pane_id,
        } = *floating_pane_view_target;
        let session = self
            .session_by_id
            .get_mut(&session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetGone))?;
        let previous_focused_pane_id =
            Self::require_client(session, client_id)?.get_active_focused_pane_id();
        if previous_focused_pane_id == Some(pane_id) {
            return Ok(self.commit_events(command_id, Vec::new()));
        }
        let client = Self::require_client_mut(session, client_id)?;
        let is_pane_restored = client.focus_floating_pane(pane_id);
        let mut emitted_events: Vec<Event> = Vec::new();
        if is_pane_restored {
            emitted_events.extend(build_pane_minimized_changed_events(
                client_id,
                &[pane_id],
                false,
            ));
        }
        emitted_events.extend(build_pane_focused_event(client, previous_focused_pane_id));
        Ok(self.commit_events(command_id, emitted_events))
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
    /// filling its view ([`Event::PaneFocused`]). When the tab is the client's
    /// active tab, that pane also takes over the client's input, and every
    /// floating pane the client shows is minimized for it
    /// ([`apply_tiled_pane_focus`]). The zoom
    /// is a solve-time overlay: the tree is untouched, and toggling out
    /// restores the prior layout. The tab must be viewed by at least one attached client, and a
    /// tab size too small to show the pane at its content minimum rejects.
    /// Emits [`Event::LayoutChanged`] plus one [`Event::PtyResized`] per PTY
    /// whose solved size changed, then, on entry, one
    /// [`Event::PaneMinimizedChanged`] per floating pane minimized and the
    /// [`Event::PaneFocused`] of a focus that moved. A pane another client
    /// still draws tiled keeps the size that client shows. A rejected toggle
    /// mutates nothing.
    pub(super) fn handle_toggle_pane_fullscreen(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let pane_target = self.resolve_fullscreen_target(command_source, acting_session)?;
        let client_id = pane_target.client_id;

        let pty_backend = Arc::clone(self.get_pty_backend());

        let tab_id = pane_target.tab_id;
        // A tab that no attached client contributes a pane area to is refused.
        let (session, _tab_size) =
            self.resolve_session_and_tab_size(pane_target.session_id, tab_id)?;
        let client = Self::require_client(session, client_id)?;
        let client_layout_mode = client.get_layout_mode(tab_id);

        // Flip this client's zoom. Entering solves the zoomed view first: a
        // tab size too small to show the pane at its content minimum rejects
        // before anything mutates.
        let is_zoom_entered = match client_layout_mode {
            LayoutMode::Fullscreen { .. } => false,
            LayoutMode::Tiled => {
                let fullscreen_layout_mode = LayoutMode::Fullscreen {
                    focused_pane_id: pane_target.pane_id,
                };
                let is_pane_suppressed = session
                    .compute_layout_mode_solve(tab_id, fullscreen_layout_mode)
                    .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?
                    .suppressed_pane_ids
                    .contains(&pane_target.pane_id);
                if is_pane_suppressed {
                    return Err(Rejection::from_reason_and_help(
                        RejectReason::InvalidState,
                        "not enough space to fullscreen the pane",
                    ));
                }
                true
            }
        };

        // The zoom applies to the target client alone; every other client
        // viewing this tab keeps its view. Entering also moves this client's
        // focus to the zoomed pane. Both land before the reflow, which solves
        // PTY sizes from what the clients now display.
        let client = Self::require_client_mut(session, client_id)?;
        let previous_focused_pane_id = client.get_active_focused_pane_id();
        let mut focus_events = Vec::new();
        if is_zoom_entered {
            client.zoom_pane(tab_id, pane_target.pane_id);
            focus_events = apply_tiled_pane_focus(client, tab_id, pane_target.pane_id);
        } else {
            client.clear_zoom(tab_id);
        }
        let is_focus_moved =
            is_zoom_entered && previous_focused_pane_id != Some(pane_target.pane_id);
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

        emitted_events.extend(focus_events);

        Ok(self.commit_events(command_id, emitted_events))
    }

    /// Handle [`Command::WriteToPane`]: inject raw bytes into a pane's child
    /// stdin. The target is an explicit `--pane` (resolved globally) or the
    /// command source's default pane.
    ///
    /// The write changes no session state, and a successful write commits no
    /// events; the child's response returns
    /// through the normal PTY output path. A backend write failure is
    /// reported to the caller as [`RejectReason::InvalidState`] `pane is not
    /// accepting input`: for example, the backend holds no entry for the pane,
    /// the pane's writer ended with its child, or the pane's input queue is
    /// full ([`PtyError::InputQueueFull`](koshi_pty::error::PtyError::InputQueueFull)).
    pub(super) fn handle_write_to_pane(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &WriteToPaneArgs,
    ) -> Result<CommandResult, Rejection> {
        let acting_session = self.resolve_acting_session(command_source)?;
        let pane_target = self.resolve_pane_target(
            command_args.pane_id,
            command_source.get_target_client_id(),
            command_source,
            acting_session,
        )?;
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
            }
        }
        Ok(self.commit_events(command_id, Vec::new()))
    }

    /// Choose the tab size a new split is sized against, and the *designated*
    /// client — the one that will view the tab and focus the new pane, `None`
    /// when the tab is already viewed and no client was named.
    ///
    /// The designated client is `focus_client_id` when set: the client the
    /// command names, else its issuer while attached
    /// ([`Self::resolve_new_pane_source`]). The split is then sized to the
    /// smallest of the tab's current viewers *and* that client, and fits every
    /// client that shows it; the caller switches the client onto the tab if it
    /// is not already there.
    ///
    /// With no `focus_client_id` (an external command source that names no
    /// client): an already-viewed tab sizes to its current viewers and
    /// designates no one (the pane just appears, no view moves); an unviewed
    /// tab defaults to the session's sole client, and a session with several
    /// attached clients is [`RejectReason::TargetAmbiguous`].
    ///
    /// Each client contributes [`Client::get_pane_area`]; a designated client
    /// reporting [`PaneArea::Starving`] contributes nothing, and is rejected
    /// with [`RejectReason::MinimumSize`] when no other viewer gives the tab a
    /// size.
    ///
    /// `candidate_layout_tree` is the post-split tree fit is judged against. Fails
    /// [`RejectReason::MinimumSize`] when the split cannot fit the chosen tab size,
    /// and [`RejectReason::InvalidState`] when the tab has no viewer and the
    /// session has no attached client at all. A bystander client is never
    /// switched to satisfy a command that named none.
    fn resolve_new_pane_tab_size(
        session: &Session,
        tab_id: TabId,
        candidate_layout_tree: &LayoutNode,
        focus_client_id: Option<ClientId>,
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

        // The focus client is designated: the client the command names, else
        // its attached issuer.
        if let Some(client_id) = focus_client_id {
            let designated_client = Self::require_client(session, client_id)?;
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
}

/// The destination tab size of `placement_tab_sizes`
/// ([`compute_placement_tab_sizes`]).
///
/// # Errors
///
/// [`RejectReason::InvalidState`] `destination tab has no drawable client
/// view` when none of the clients that then view the destination tab has a
/// pane area.
fn require_placement_destination_tab_size(
    placement_tab_sizes: PlacementTabSizes,
) -> Result<Size, Rejection> {
    placement_tab_sizes.destination_tab_size.ok_or_else(|| {
        Rejection::from_reason_and_help(
            RejectReason::InvalidState,
            "destination tab has no drawable client view",
        )
    })
}
