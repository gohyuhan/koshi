//! Command dispatch: the single entrypoint every requested mutation passes
//! through.
//!
//! [`Runtime::dispatch`] validates one [`CommandEnvelope`] against live state,
//! then routes it via an exhaustive `match` on [`Command`] — one arm per
//! variant. Validation runs first: a command whose source may not issue it, or
//! whose target does not resolve, is rejected before any handler runs. A
//! command whose handler has not landed yet still rejects cleanly with
//! [`RejectReason::InvalidState`] and a diagnostic hint. The exhaustive match
//! is the point: a new `Command` variant cannot be added without giving it an
//! arm here, and each handler replaces its arm in place as it ships.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::thread;
use std::time::SystemTime;

use koshi_core::{
    command::{
        ClosePaneArgs, CloseTabArgs, Command, CommandEnvelope, CommandResult, CommandSource,
        CopyModeCommand, FocusPaneArgs, FocusTabArgs, FocusTarget, LockModeArgs, MoveTabArgs,
        NewPaneArgs, NewTabArgs, RenamePaneArgs, RenameSessionArgs, RenameTabArgs, ResizePaneArgs,
        RunCommandPaneArgs, TabTarget, WriteToPaneArgs,
    },
    event::{
        Event, InputMode, InputModeChanged, LayoutChanged, PaneFocused, PtyResized, RejectReason,
    },
    geometry::{Point, Rect, Size},
    ids::{ClientId, CommandId, PaneId, SessionId, TabId},
    lock::LockMode,
    naming::{generate_name, NameKind},
    process::{ExitStatus, KillPolicy, PtySize, SpawnSpec},
};
use koshi_layout::{
    content::content_rects,
    edit::{add_to_stack, split_leaf},
    focus::stack_activate,
    mode::LayoutMode,
    resize::{resize, ResizeError},
    solver::{fits, solve, solve_with_mode, MIN_PANE_SIZE},
    tree::LayoutNode,
};
use koshi_pane::pane::{lifecycle::PaneLifecycle, policy::PaneClosePolicy};
use koshi_pty::backend::state::{PtyBackend, PtyHandle};
use koshi_pty::resize::{compute_pty_size, resize_for_layout_change};
use koshi_session::client::{Client, ClientRegistry};
use koshi_session::session::{
    cascade::{on_child_exit, remove_pane_cascade},
    lifecycle::SessionLifecycle,
    pane_ops::{self, NewPaneSpec},
    policy::EmptyTabPolicy,
    session_ops,
    state::Session,
    tab_ops,
};
use koshi_terminal::engine::TerminalEngine;

use crate::runtime::{
    render_schedule::InvalidationReason, snapshot::solve_tab, state::Runtime,
    transaction::TransactionScope,
};

/// The PTY size for a tab's sole root pane filling `viewport`: solve the
/// single-pane layout, take the root's content rect, and clamp it to a PTY size.
/// Shared by the new-tab path and genesis so both size the root pane identically.
///
/// Callers that gate on minimum size (the new-tab command) check
/// [`fits`] first; genesis has no gate, and the
/// solver always places a single leaf, so the `unwrap_or` fallback is a floor,
/// not a real path.
pub(crate) fn size_root_pane(pane_id: PaneId, viewport: Size) -> PtySize {
    let candidate = LayoutNode::Pane(pane_id);
    let tab_rect = Rect::new(Point { x: 0, y: 0 }, viewport);
    let rects = content_rects(&solve(&candidate, tab_rect));
    let rect = rects
        .iter()
        .find(|(id, _)| *id == pane_id)
        .and_then(|(_, content)| *content)
        .unwrap_or(tab_rect);
    compute_pty_size(rect)
}

/// A validation failure: the reason a command was rejected, plus an optional
/// human-facing hint. The `Err` half of [`Runtime::validate`].
struct Rejection {
    reason: RejectReason,
    help: Option<String>,
}

impl Rejection {
    /// A rejection with the given reason and a hint string.
    fn new(reason: RejectReason, help: &str) -> Self {
        Rejection {
            reason,
            help: Some(help.to_string()),
        }
    }

    /// A rejection with the given reason and no hint.
    fn bare(reason: RejectReason) -> Self {
        Rejection { reason, help: None }
    }
}

/// The resolved concrete target of a [`Command::NewPane`]: the session and tab
/// the new pane joins, the source pane it splits from, and the client to
/// auto-focus it for (when one applies). All fields are `Copy`, so resolving
/// holds no borrow into the session map.
struct NewPaneTarget {
    session_id: SessionId,
    source_pane: PaneId,
    tab_id: TabId,
    focus_client: Option<ClientId>,
}

/// The resolved concrete target of a pane-addressed command
/// ([`Command::ClosePane`], [`Command::ResizePane`]): the owning session, the
/// tab whose layout holds the pane, and the pane itself. All fields are
/// `Copy`, so resolving holds no borrow into the session map.
struct PaneTarget {
    session_id: SessionId,
    tab_id: TabId,
    pane_id: PaneId,
}

/// A resolved [`Command::FocusPane`] target: the session and client whose
/// focus moves, that client's active tab — the tab the pane was resolved
/// in — and the pane taking focus. The `Ok` half of
/// [`Runtime::resolve_focus_target`].
struct FocusPaneTarget {
    session_id: SessionId,
    client_id: ClientId,
    tab_id: TabId,
    pane_id: PaneId,
}

/// The resolved default-pane context of a command that names no target: the
/// acting session, the source's client, and the pane the command acts on —
/// an in-session CLI's issuing pane, else the client's focused pane. The
/// `Ok` half of [`Runtime::resolve_default_pane`].
struct DefaultPaneTarget {
    session_id: SessionId,
    client_id: ClientId,
    pane_id: PaneId,
}

/// A resolved [`Command::NewTab`] target: the session the tab joins and the
/// client that switches onto it. The `Ok` half of
/// [`Runtime::resolve_new_tab_target`].
struct NewTabTarget {
    session_id: SessionId,
    client_id: ClientId,
}

/// A resolved [`Command::FocusTab`] target: the session, the client whose
/// view switches, and the concrete tab the target named. The `Ok` half of
/// [`Runtime::resolve_focus_tab_target`].
struct FocusTabTarget {
    session_id: SessionId,
    client_id: ClientId,
    tab_id: TabId,
}

impl Runtime {
    /// Dispatch one command and report its outcome.
    ///
    /// Every mutation enters here; nothing mutates session, layout, or pane
    /// state outside a handler reached through this method. The command is
    /// validated first (target resolution, source policy); a command that
    /// passes validation but has no handler yet is rejected with
    /// [`RejectReason::InvalidState`].
    pub fn dispatch(&mut self, envelope: CommandEnvelope) -> CommandResult {
        if let Err(rejection) = self.validate(&envelope) {
            return CommandResult::Rejected {
                command_id: envelope.id,
                reason: rejection.reason,
                help: rejection.help,
            };
        }
        match envelope.command {
            Command::NewPane(args) => {
                self.handle_new_pane(envelope.id, &envelope.source, &args, envelope.issued_at)
            }
            Command::ClosePane(args) => {
                self.handle_close_pane(envelope.id, &envelope.source, &args)
            }
            Command::ResizePane(args) => {
                self.handle_resize_pane(envelope.id, &envelope.source, &args)
            }
            Command::FocusPane(args) => {
                self.handle_focus_pane(envelope.id, &envelope.source, &args)
            }
            Command::NewTab(args) => {
                self.handle_new_tab(envelope.id, &envelope.source, &args, envelope.issued_at)
            }
            Command::CloseTab(args) => self.handle_close_tab(envelope.id, &envelope.source, &args),
            Command::RenameTab(args) => {
                self.handle_rename_tab(envelope.id, &envelope.source, &args)
            }
            Command::FocusTab(args) => self.handle_focus_tab(envelope.id, &envelope.source, &args),
            Command::WriteToPane(args) => {
                self.handle_write_to_pane(envelope.id, &envelope.source, &args)
            }
            Command::ToggleLockMode => self.handle_toggle_lock_mode(envelope.id, &envelope.source),
            Command::SetLockMode(args) => {
                self.handle_set_lock_mode(envelope.id, &envelope.source, &args)
            }
            Command::RunCommandPane(args) => {
                let new_pane_args = Self::run_command_new_pane_args(&args);
                self.handle_new_pane(
                    envelope.id,
                    &envelope.source,
                    &new_pane_args,
                    envelope.issued_at,
                )
            }
            Command::CopyMode(command) => self.handle_copy_mode(envelope.id, &command),
            Command::Plugin(_) => self.reject(envelope.id, "plugin"),
            Command::Quit => self.reject(envelope.id, "quit"),
            Command::TogglePaneFullscreen => {
                self.handle_toggle_pane_fullscreen(envelope.id, &envelope.source)
            }
            Command::RenamePane(args) => {
                self.handle_rename_pane(envelope.id, &envelope.source, &args)
            }
            Command::MoveTab(args) => self.handle_move_tab(envelope.id, &envelope.source, &args),
            Command::RenameSession(args) => {
                self.handle_rename_session(envelope.id, &envelope.source, &args)
            }
        }
    }

    /// Build a rejection for a command with no handler wired yet, keyed back to
    /// its originating envelope by `command_id`. `label` names the command in
    /// the human-facing hint.
    fn reject(&self, command_id: CommandId, label: &str) -> CommandResult {
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some(format!("{label} not yet implemented")),
        }
    }

    /// Route a [`Command::CopyMode`] sub-command to its handler. The
    /// exhaustive match gives each [`CopyModeCommand`] variant its own routing
    /// seam; every arm rejects with [`RejectReason::InvalidState`] until
    /// copy-mode handling lands.
    fn handle_copy_mode(&self, command_id: CommandId, command: &CopyModeCommand) -> CommandResult {
        match command {
            CopyModeCommand::Enter => self.reject(command_id, "copy mode"),
            CopyModeCommand::Exit => self.reject(command_id, "copy mode"),
            CopyModeCommand::MoveCursor(_) => self.reject(command_id, "copy mode"),
            CopyModeCommand::SetSelection(_) => self.reject(command_id, "copy mode"),
            CopyModeCommand::ClearSelection => self.reject(command_id, "copy mode"),
            CopyModeCommand::Copy(_) => self.reject(command_id, "copy mode"),
            CopyModeCommand::Search(_) => self.reject(command_id, "copy mode"),
            CopyModeCommand::SearchNext => self.reject(command_id, "copy mode"),
            CopyModeCommand::SearchPrev => self.reject(command_id, "copy mode"),
        }
    }

    /// Turn a [`Rejection`] into a [`CommandResult::Rejected`] keyed to
    /// `command_id`.
    fn rejected(command_id: CommandId, rejection: Rejection) -> CommandResult {
        CommandResult::Rejected {
            command_id,
            reason: rejection.reason,
            help: rejection.help,
        }
    }

    /// Seal `events` as one committed transaction keyed to `command_id`: emit
    /// each event into a fresh [`TransactionScope`] in order, then commit.
    fn commit_events(command_id: CommandId, events: Vec<Event>) -> CommandResult {
        let mut scope = TransactionScope::new();
        for event in events {
            scope.emit(event);
        }
        scope.commit(command_id)
    }

    /// Map [`Command::RunCommandPane`] onto the [`NewPaneArgs`] that realize it:
    /// its command is required (never the default shell), and its source
    /// pane, placement — split direction or stacking — and working directory
    /// carry through to the new-pane transaction. Shared by
    /// [`Self::dispatch`] and [`Self::resolve_target`] so the validate
    /// pre-check and the handler resolve the same anchor pane.
    fn run_command_new_pane_args(args: &RunCommandPaneArgs) -> NewPaneArgs {
        NewPaneArgs {
            source: args.source,
            direction: args.direction,
            stacked: args.stacked,
            cwd: args.cwd.clone(),
            command: Some(args.command.clone()),
            client: None,
        }
    }

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
    /// onto the tab (if not already there) and the tab it left is reflowed. A
    /// fullscreen tab drops its fullscreen at the commit, so the new pane
    /// lands in the tiled view it was sized against. All events seal in one
    /// transaction.
    fn handle_new_pane(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &NewPaneArgs,
        issued_at: SystemTime,
    ) -> CommandResult {
        let acting = match self.acting_session(source) {
            Ok(acting) => acting,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let target = match self.resolve_new_pane_source(args, source, acting) {
            Ok(target) => target,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };

        // Clone the shared backend before borrowing a session: spawn and resize
        // then need no `&self` borrow, so they coexist with `&mut Session`.
        let backend = Arc::clone(self.pty_backend());

        let Some(session) = self.sessions.get_mut(&target.session_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };

        // Build the post-edit tree without mutating anything: `--stacked` joins
        // the source's stack (creating one when the source is a plain leaf),
        // otherwise the source leaf splits directionally. A source pane that is
        // not a live leaf of the tab rejects here, before any state changes.
        let Some(tab) = session.tabs.get(&target.tab_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };
        // A pane added to a fullscreen tab lands in the tiled view — the
        // commit drops the fullscreen — so the candidate is sized against the
        // tiled solve the clients will see.
        let layout_mode = match tab.layout_mode() {
            LayoutMode::Fullscreen { .. } => LayoutMode::Tiled,
            mode => mode,
        };
        let new_pane_id = PaneId::new();
        let edited = if args.stacked {
            add_to_stack(tab.layout(), target.source_pane, new_pane_id)
        } else {
            let direction = args.direction.unwrap_or(self.default_new_pane_direction);
            split_leaf(tab.layout(), target.source_pane, new_pane_id, direction)
        };
        let candidate = match edited {
            Ok(candidate) => candidate,
            Err(_) => {
                return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
            }
        };

        // Choose the viewport the split is sized against, and the client (if any)
        // designated to view the tab and focus the new pane. Fit is judged against
        // the candidate, so a split too large for the chosen viewport is rejected
        // before anything mutates.
        let (viewport, designated) = match Self::resolve_new_pane_viewport(
            session,
            target.tab_id,
            &candidate,
            target.focus_client,
            args.client,
        ) {
            Ok(resolved) => resolved,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };

        // Solve the candidate against that viewport to size the new pane and
        // its siblings. Fit passed above, so the new pane has a real content
        // rect; a solve that still gives it no area rejects defensively,
        // before any mutation.
        let tab_rect = Rect::new(Point { x: 0, y: 0 }, viewport);
        let rects = content_rects(&solve_with_mode(&candidate, layout_mode, tab_rect));
        let Some(new_rect) = rects
            .iter()
            .find(|(pane_id, _)| *pane_id == new_pane_id)
            .and_then(|(_, content)| *content)
        else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::InvalidState));
        };
        let spawn_size = compute_pty_size(new_rect);

        // The spawn request: the requested command (its cwd falling back to
        // `--cwd` when unset, so `--cwd` reaches an explicit command), else the
        // default shell carrying `--cwd`. A new pane's env starts empty, matching
        // the record `commit_new_pane` writes.
        let spawn_spec = match &args.command {
            Some(command) => {
                let mut spec = command.clone();
                if spec.cwd.is_none() {
                    spec.cwd = args.cwd.clone();
                }
                spec
            }
            None => SpawnSpec::default_shell(args.cwd.clone(), BTreeMap::new()),
        };
        // What the pane records: the directory it actually launches in (an
        // explicit command's own cwd wins over `--cwd`), and the resolved spawn
        // request itself when a command was given, so the record can't disagree
        // with the process about where or what it started.
        let launch_cwd = spawn_spec.cwd.clone();
        let recorded_command = args.command.as_ref().map(|_| spawn_spec.clone());

        // Launch the child BEFORE committing any state. On failure nothing was
        // registered and no view moved, so the command rejects as if it never ran.
        let handle = match backend.spawn(new_pane_id, spawn_spec, spawn_size) {
            Ok(handle) => handle,
            Err(_) => {
                return CommandResult::Rejected {
                    command_id,
                    reason: RejectReason::InvalidState,
                    help: Some("failed to launch the pane's process".to_string()),
                };
            }
        };

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
        Self::park_pane_pty(
            &mut self.pty_handles,
            &mut self.pty_sizes,
            &mut self.terminal_engines,
            &self.inbox_tx,
            new_pane_id,
            handle,
            spawn_size,
        );
        // Announce the new pane's size — PaneCreated carries none.
        events.push(Event::PtyResized(PtyResized {
            pane_id: new_pane_id,
            size: spawn_size,
        }));

        // Reflow the target tab's other live panes to the new geometry (excluding
        // the pane just spawned, already sized above).
        Self::reflow_changed(
            backend.as_ref(),
            &self.pty_handles,
            &mut self.pty_sizes,
            &mut self.terminal_engines,
            rects,
            Some(new_pane_id),
            &mut events,
        );

        // Adoption moved a client off its previous tab; if that tab still has a
        // viewer, reflow its live panes to the viewport it now sizes against. A
        // tab left with no viewer has no viewport and keeps its sizes.
        if let Some(prev_tab) = prev_tab {
            Self::reflow_tab_if_viewed(
                backend.as_ref(),
                session,
                &self.pty_handles,
                &mut self.pty_sizes,
                &mut self.terminal_engines,
                prev_tab,
                &mut events,
            );
        }

        Self::commit_events(command_id, events)
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
    fn handle_close_pane(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &ClosePaneArgs,
    ) -> CommandResult {
        let acting = match self.acting_session(source) {
            Ok(acting) => acting,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let target = match self.resolve_pane_target(args.pane, source, acting) {
            Ok(target) => target,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };

        // Clone the shared backend before borrowing a session: the kill thread
        // takes its own handle, so no `&self` borrow crosses the commit.
        let backend = Arc::clone(self.pty_backend());

        let Some(session) = self.sessions.get_mut(&target.session_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };
        let Some(record) = session.panes.get(target.pane_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };

        // Pick how the child dies. `--force` overrides the pane's own policy;
        // `ConfirmIfBusy` closes only a pane whose child provably ended
        // (`Exited`) and otherwise rejects, pointing at `--force`.
        let kill_policy = if args.force {
            KillPolicy::Force
        } else {
            match record.close_policy {
                PaneClosePolicy::ConfirmIfBusy => match record.lifecycle() {
                    PaneLifecycle::Exited { .. } => record.close_policy.kill_policy(),
                    _ => {
                        return CommandResult::Rejected {
                            command_id,
                            reason: RejectReason::InvalidState,
                            help: Some(
                                "pane may be busy; pass --force to close anyway".to_string(),
                            ),
                        };
                    }
                },
                policy => policy.kill_policy(),
            }
        };

        // Solve the tab against a deterministic viewport so focus candidates
        // rank geometrically even when no client currently views the tab.
        let tab_rect = Rect::new(
            Point { x: 0, y: 0 },
            Self::close_viewport(session, target.tab_id),
        );

        // Commit the state removal: registry drop, layout collapse, per-client
        // focus repair, empty-tab close, last-tab quit — one shared cascade.
        let mut events = remove_pane_cascade(
            session,
            target.tab_id,
            target.pane_id,
            tab_rect,
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

        Self::commit_events(command_id, events)
    }

    /// Drop every per-pane record a removed pane leaves behind: its PTY handle,
    /// size cache, terminal engine, and each client's parked scrollback offset
    /// for it (so the per-view map holds no dead entries over the session's
    /// life). The one release point for pane bookkeeping — every path that
    /// removes a pane funnels through here. Explicit field refs so a caller can
    /// hold the owning session borrowed alongside.
    fn release_pane_bookkeeping(
        pty_handles: &mut HashMap<PaneId, PtyHandle>,
        pty_sizes: &mut HashMap<PaneId, PtySize>,
        terminal_engines: &mut HashMap<PaneId, TerminalEngine>,
        clients: &mut ClientRegistry,
        pane_id: PaneId,
    ) {
        pty_handles.remove(&pane_id);
        pty_sizes.remove(&pane_id);
        terminal_engines.remove(&pane_id);
        for client in clients.list_attached_mut() {
            client.set_scroll_offset(pane_id, 0);
        }
    }

    /// Drop a removed pane's runtime bookkeeping and reflow the survivors into
    /// the space it freed.
    ///
    /// Removes the pane's PTY handle, size cache, and terminal engine — output
    /// bytes still in flight for it now find no engine and are dropped — and
    /// clears any parked scrollback offset each client held for it, then
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
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };

        Self::release_pane_bookkeeping(
            &mut self.pty_handles,
            &mut self.pty_sizes,
            &mut self.terminal_engines,
            &mut session.clients,
            pane_id,
        );

        let reflow_tab = if session.tabs.contains_key(&tab_id) {
            Some(tab_id)
        } else {
            events.iter().find_map(|event| match event {
                Event::TabFocused(focused) => Some(focused.tab_id),
                _ => None,
            })
        };
        if let Some(reflow_tab) = reflow_tab {
            Self::reflow_tab_if_viewed(
                backend,
                session,
                &self.pty_handles,
                &mut self.pty_sizes,
                &mut self.terminal_engines,
                reflow_tab,
                events,
            );
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

    /// Attach a client to `session_id` viewing `active_tab`, then reconcile the
    /// affected tabs' PTY sizes and schedule a redraw.
    ///
    /// A client lives in exactly one session. If this id already lives in another
    /// session it is detached there first — reflowing the tab it leaves — so it
    /// is never recorded twice. Within the target session an id that is already
    /// attached is a re-attach: its view updates in place, keeping its per-tab
    /// focus, scrollback offsets, and lock mode, and the tab it moves off of
    /// reflows too. A fresh id is registered anew.
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
    ) -> Vec<Event> {
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
                    Self::reflow_tab_if_viewed(
                        backend.as_ref(),
                        old_session,
                        &self.pty_handles,
                        &mut self.pty_sizes,
                        &mut self.terminal_engines,
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
            Some(prior)
        } else {
            session.attach_client(Client::new(
                client_id,
                session_id,
                attached_at,
                viewport,
                active_tab,
            ));
            None
        };

        // Reflow the tab the client now views, plus — on a same-session move —
        // the one it left.
        Self::reflow_tab_if_viewed(
            backend.as_ref(),
            session,
            &self.pty_handles,
            &mut self.pty_sizes,
            &mut self.terminal_engines,
            active_tab,
            &mut events,
        );
        if let Some(prior) = prior_tab {
            if prior != active_tab {
                Self::reflow_tab_if_viewed(
                    backend.as_ref(),
                    session,
                    &self.pty_handles,
                    &mut self.pty_sizes,
                    &mut self.terminal_engines,
                    prior,
                    &mut events,
                );
            }
        }

        self.render_scheduler
            .invalidate(InvalidationReason::LayoutChanged);

        events
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
    pub fn handle_client_detach(&mut self, client_id: ClientId) -> Vec<Event> {
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
        let active_tab = session
            .detach_client(client_id)
            .map(|client| client.active_tab());

        let mut events = Vec::new();
        // Reflow the tab the client left, if any other client still views it; a
        // tab whose last viewer just left has no viewport and keeps its sizes.
        if let Some(active_tab) = active_tab {
            Self::reflow_tab_if_viewed(
                backend.as_ref(),
                session,
                &self.pty_handles,
                &mut self.pty_sizes,
                &mut self.terminal_engines,
                active_tab,
                &mut events,
            );
        }

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
    /// negative one shrinks it with that sibling gaining them. The target
    /// pane's tab must be viewed by at least one attached client: the tab is
    /// solved against that real viewport ([`Session::tab_viewport`]), so the
    /// donating side's spare cells are measured against the exact terminal
    /// displaying the result, and a tab no client currently views rejects.
    /// On success the tab's tree is swapped in — a fullscreen tab drops its
    /// fullscreen, making the moved border visible —
    /// [`Event::LayoutChanged`] is emitted, and every live PTY whose solved
    /// size changed is resized through the shared reflow path, one
    /// [`Event::PtyResized`] each.
    fn handle_resize_pane(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &ResizePaneArgs,
    ) -> CommandResult {
        if args.size == 0 {
            return CommandResult::Rejected {
                command_id,
                reason: RejectReason::InvalidState,
                help: Some("resize size must be non-zero".to_string()),
            };
        }
        let acting = match self.acting_session(source) {
            Ok(acting) => acting,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let target = match self.resolve_pane_target(args.pane, source, acting) {
            Ok(target) => target,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };

        let backend = Arc::clone(self.pty_backend());

        let Some(session) = self.sessions.get_mut(&target.session_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };
        let Some(viewport) = session.tab_viewport(target.tab_id) else {
            return Self::rejected(
                command_id,
                Rejection::new(
                    RejectReason::InvalidState,
                    "pane's tab is not viewed by any client",
                ),
            );
        };
        let tab_rect = Rect::new(Point { x: 0, y: 0 }, viewport);
        let Some(tab) = session.tabs.get_mut(&target.tab_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };

        // The resize transaction returns a new tree and leaves the tab's
        // untouched on rejection, so a failed resize mutates nothing.
        let resized = match resize(
            tab.layout(),
            tab_rect,
            target.pane_id,
            args.direction,
            args.size,
        ) {
            Ok(resized) => resized,
            Err(error) => return Self::rejected(command_id, Self::resize_rejection(&error)),
        };
        tab.update_layout(resized);
        // A layout edit returns the tab to the tiled view: resizing a pane of
        // a fullscreen tab drops the fullscreen, so the moved border is
        // visible in the reflow below.
        if matches!(tab.layout_mode(), LayoutMode::Fullscreen { .. }) {
            tab.update_layout_mode(LayoutMode::Tiled);
        }

        // The border moved: re-solve the tab and resize each live PTY whose
        // size changed.
        let mut events = vec![Event::LayoutChanged(LayoutChanged {
            tab_id: target.tab_id,
        })];
        let rects = Self::tab_content_rects(session, target.tab_id, viewport);
        Self::reflow_changed(
            backend.as_ref(),
            &self.pty_handles,
            &mut self.pty_sizes,
            &mut self.terminal_engines,
            rects,
            None,
            &mut events,
        );

        Self::commit_events(command_id, events)
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
    /// reflow to the new geometry. Fullscreen follows focus: on a fullscreen
    /// tab, focusing a pane other than the promoted one retargets the
    /// fullscreen to it — the zoomed view swaps content, the mode stays on.
    /// Emits [`Event::LayoutChanged`] plus per-pane [`Event::PtyResized`]
    /// when a stack activation or a fullscreen retarget changed the
    /// geometry, and [`Event::PaneFocused`] when the client's focus actually
    /// moved; focusing the already-focused pane of an already-active member
    /// completes with no events. A rejected focus mutates nothing.
    fn handle_focus_pane(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &FocusPaneArgs,
    ) -> CommandResult {
        let acting = match self.acting_session(source) {
            Ok(acting) => acting,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let target = match Self::resolve_focus_target(args, source, acting) {
            Ok(target) => target,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };

        let backend = Arc::clone(self.pty_backend());

        let Some(session) = self.sessions.get_mut(&target.session_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };
        let Some(viewport) = session.tab_viewport(target.tab_id) else {
            return Self::rejected(
                command_id,
                Rejection::new(
                    RejectReason::InvalidState,
                    "pane's tab is not viewed by any client",
                ),
            );
        };
        let Some(tab) = session.tabs.get_mut(&target.tab_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };

        // Fullscreen follows focus: focusing a pane hidden behind a
        // fullscreen tab retargets the fullscreen to it, so the mode the tab
        // will show is solved and checked, not the one it left.
        let effective_mode = match tab.layout_mode() {
            LayoutMode::Fullscreen { focused } if focused != target.pane_id => {
                LayoutMode::Fullscreen {
                    focused: target.pane_id,
                }
            }
            mode => mode,
        };
        let retargeted = effective_mode != tab.layout_mode();

        // Solve the tab as it will display: a pane suppressed for lack of
        // space cannot take focus.
        let tab_rect = Rect::new(Point { x: 0, y: 0 }, viewport);
        let solved = solve_with_mode(tab.layout(), effective_mode, tab_rect);
        if solved.suppressed.contains(&target.pane_id) {
            return Self::rejected(
                command_id,
                Rejection::new(
                    RejectReason::InvalidState,
                    "pane is suppressed; not enough space to show it",
                ),
            );
        }

        // A collapsed stack member is a valid target: focusing it expands it.
        // The activation mutates a candidate tree, swapped in whole, and the
        // changed geometry — the activation, the fullscreen retarget, or both
        // — reflows every affected PTY once.
        let mut events = Vec::new();
        let mut candidate = tab.layout().clone();
        let activated = candidate
            .stack_containing_mut(target.pane_id)
            .and_then(|stack| stack_activate(stack, target.pane_id))
            .is_some();
        if activated {
            tab.update_layout(candidate);
        }
        if retargeted {
            tab.update_layout_mode(effective_mode);
        }
        if activated || retargeted {
            events.push(Event::LayoutChanged(LayoutChanged {
                tab_id: target.tab_id,
            }));
            let rects = Self::tab_content_rects(session, target.tab_id, viewport);
            Self::reflow_changed(
                backend.as_ref(),
                &self.pty_handles,
                &mut self.pty_sizes,
                &mut self.terminal_engines,
                rects,
                None,
                &mut events,
            );
        }

        let Some(client) = session.clients.get_mut(target.client_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::SourceClientStale));
        };
        let prior_pane = client.focused_pane(target.tab_id);
        if prior_pane == Some(target.pane_id) && !activated && !retargeted {
            return TransactionScope::new().commit(command_id);
        }
        client.update_focused_pane(target.tab_id, target.pane_id);
        if let Some(tab) = session.tabs.get_mut(&target.tab_id) {
            tab.record_focus_mru(target.pane_id);
        }
        if prior_pane != Some(target.pane_id) {
            events.push(Event::PaneFocused(PaneFocused {
                client_id: target.client_id,
                tab_id: target.tab_id,
                pane_id: target.pane_id,
                prior_pane,
            }));
        }

        Self::commit_events(command_id, events)
    }

    /// Handle [`Command::NewTab`]: create a tab holding one fresh shell pane
    /// and switch the designated client onto it, in launch-then-commit order —
    /// no session state changes until the child process is live.
    ///
    /// The designated client — an explicit `--client` target (which wins even
    /// over the issuer, and rejects outright if not attached), else the
    /// in-session issuer, else the session's sole attached client — provides
    /// the viewport the root pane is sized against, and is the only client
    /// whose view moves. The tab's name is generated by
    /// [`koshi_core::naming::generate_name`] avoiding the session's existing
    /// tab names — the caller supplies none.
    /// The root pane runs the default shell in `--cwd`. After the
    /// commit, the tab the client left reflows to its remaining viewers'
    /// viewport; a tab left with no viewer keeps its sizes. A launch failure
    /// commits nothing and rejects, so a tab never exists without its
    /// process.
    fn handle_new_tab(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &NewTabArgs,
        issued_at: SystemTime,
    ) -> CommandResult {
        let acting = match self.acting_session(source) {
            Ok(acting) => acting,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let target = match Self::resolve_new_tab_target(args, source, acting) {
            Ok(target) => target,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        // Clone the shared backend before borrowing a session: spawn then
        // needs no `&self` borrow, so it coexists with `&mut Session`.
        let backend = Arc::clone(self.pty_backend());

        let Some(session) = self.sessions.get_mut(&target.session_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };
        let Some(client) = session.clients.get(target.client_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::SourceClientStale));
        };

        // The new tab is viewed (only) by the designated client, so its root
        // pane is sized against that client's viewport.
        let viewport = client.viewport();
        let new_pane_id = PaneId::new();
        let new_tab_id = TabId::new();
        let candidate = LayoutNode::Pane(new_pane_id);
        let tab_rect = Rect::new(Point { x: 0, y: 0 }, viewport);
        if !fits(&candidate, tab_rect, MIN_PANE_SIZE) {
            return Self::rejected(
                command_id,
                Rejection::new(RejectReason::MinSize, "not enough space for a new tab"),
            );
        }
        let spawn_size = size_root_pane(new_pane_id, viewport);

        // Resolve the tab's name before the spawn: a generated one no
        // existing tab in the session already uses.
        let name = generate_name(NameKind::Tab, |candidate| {
            session.tabs.values().any(|tab| tab.name() == candidate)
        });

        // The root pane runs the default shell carrying `--cwd`. A new pane's
        // env starts empty, matching the record `commit_new_tab` writes.
        let spawn_spec = SpawnSpec::default_shell(args.cwd.clone(), BTreeMap::new());
        let launch_cwd = spawn_spec.cwd.clone();

        // Launch the child BEFORE committing any state. On failure nothing
        // was registered and no view moved, so the command rejects as if it
        // never ran.
        let handle = match backend.spawn(new_pane_id, spawn_spec, spawn_size) {
            Ok(handle) => handle,
            Err(_) => {
                return CommandResult::Rejected {
                    command_id,
                    reason: RejectReason::InvalidState,
                    help: Some("failed to launch the pane's process".to_string()),
                };
            }
        };

        // The child is live — commit all session state through the pure op:
        // it registers the root pane `Running`, appends the tab, and switches
        // the designated client onto it. It returns the client's previous
        // tab, for the reflow below.
        let spec = NewPaneSpec {
            cwd: launch_cwd,
            command: None,
        };
        let (prev_tab, mut events) = tab_ops::commit_new_tab(
            session,
            new_tab_id,
            new_pane_id,
            name,
            Some(target.client_id),
            spec,
            issued_at,
        );

        // Park the handle so a forwarder relays its output/exit, and record its
        // size so later reflows can tell whether a resize is a real change. The
        // terminal engine gives the child's output a grid to land in.
        Self::park_pane_pty(
            &mut self.pty_handles,
            &mut self.pty_sizes,
            &mut self.terminal_engines,
            &self.inbox_tx,
            new_pane_id,
            handle,
            spawn_size,
        );
        // Announce the new pane's size — PaneCreated carries none.
        events.push(Event::PtyResized(PtyResized {
            pane_id: new_pane_id,
            size: spawn_size,
        }));

        // The client left its previous tab; if that tab still has a viewer,
        // reflow its live panes to the viewport it now sizes against. A tab
        // left with no viewer has no viewport and keeps its sizes.
        if let Some(prev_tab) = prev_tab {
            Self::reflow_tab_if_viewed(
                backend.as_ref(),
                session,
                &self.pty_handles,
                &mut self.pty_sizes,
                &mut self.terminal_engines,
                prev_tab,
                &mut events,
            );
        }

        Self::commit_events(command_id, events)
    }

    /// Handle [`Command::CloseTab`]: tear the tab and every pane in it out of
    /// the session and kill their children, in commit-then-kill order — the
    /// state removal is authoritative and immediate, the kills are
    /// best-effort and off-thread.
    ///
    /// Close policies gate the whole tab up front, all-or-nothing: without
    /// `--force`, one `ConfirmIfBusy` pane whose child has not provably
    /// exited rejects the close before anything mutates, with a hint at
    /// `--force`. `--force` force-kills every pane regardless of policy. The
    /// removal itself is [`tab_ops::close_tab`]: pane records drop, the tab
    /// goes, viewers move to the nearest surviving tab, and closing the last
    /// tab quits the session. The kills run on one detached thread per pane —
    /// a graceful kill sleeps out its grace window, so every child gets its
    /// stop request immediately and the dispatcher never stalls.
    ///
    /// After the removal, the tab the displaced viewers landed on reflows to
    /// its new viewport (it now counts the movers). A destination with no
    /// viewer — or a close that quit the session — reflows nothing.
    fn handle_close_tab(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &CloseTabArgs,
    ) -> CommandResult {
        let acting = match self.acting_session(source) {
            Ok(acting) => acting,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let tab_id = match self.resolve_tab_or_active(args.tab, source, acting) {
            Ok(tab_id) => tab_id,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let Some(session_id) = acting.map(|session| session.id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };

        // Clone the shared backend before borrowing a session: the kill
        // thread takes its own handle, so no `&self` borrow crosses the
        // commit.
        let backend = Arc::clone(self.pty_backend());

        let Some(session) = self.sessions.get_mut(&session_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };
        let Some(tab) = session.tabs.get(&tab_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };
        let pane_ids = tab.layout().leaf_panes();

        // Pick how every child dies before anything mutates — all-or-nothing.
        // `--force` overrides each pane's own policy; `ConfirmIfBusy` allows
        // the close only for a pane whose child provably ended (`Exited`) and
        // otherwise rejects the whole tab.
        let mut kills: Vec<(PaneId, KillPolicy)> = Vec::with_capacity(pane_ids.len());
        for &pane_id in &pane_ids {
            let Some(record) = session.panes.get(pane_id) else {
                continue;
            };
            let kill_policy = if args.force {
                KillPolicy::Force
            } else {
                match record.close_policy {
                    PaneClosePolicy::ConfirmIfBusy => match record.lifecycle() {
                        PaneLifecycle::Exited { .. } => record.close_policy.kill_policy(),
                        _ => {
                            return CommandResult::Rejected {
                                command_id,
                                reason: RejectReason::InvalidState,
                                help: Some(
                                    "a pane in the tab may be busy; pass --force to close anyway"
                                        .to_string(),
                                ),
                            };
                        }
                    },
                    policy => policy.kill_policy(),
                }
            };
            kills.push((pane_id, kill_policy));
        }

        // Commit the state removal: pane records drop, the tab goes, viewers
        // move to the nearest surviving tab, last-tab close quits the session.
        let mut events = tab_ops::close_tab(session, tab_id);

        // The panes are gone from state; drop their runtime bookkeeping — PTY
        // handle, size cache, terminal engine, and parked scroll offsets. Keyed
        // off the layout's own leaf list — the exact set the op removed.
        for pane_id in &pane_ids {
            Self::release_pane_bookkeeping(
                &mut self.pty_handles,
                &mut self.pty_sizes,
                &mut self.terminal_engines,
                &mut session.clients,
                *pane_id,
            );
        }

        // Displaced viewers landed on the nearest surviving tab (the
        // cascade's `TabFocused`); its viewport now counts them, so it
        // reflows. A destination with no viewport keeps its sizes.
        let destination = events.iter().find_map(|event| match event {
            Event::TabFocused(focused) => Some(focused.tab_id),
            _ => None,
        });
        if let Some(destination) = destination {
            Self::reflow_tab_if_viewed(
                backend.as_ref(),
                session,
                &self.pty_handles,
                &mut self.pty_sizes,
                &mut self.terminal_engines,
                destination,
                &mut events,
            );
        }

        // Kill the children off-thread: a graceful kill sleeps out its grace
        // window, and the dispatcher must keep draining. One thread per pane
        // so every child receives its stop request immediately; each kill
        // also purges the backend's own entry for its pane.
        for (pane_id, kill_policy) in kills {
            let backend = Arc::clone(&backend);
            let _ = thread::spawn(move || {
                let _ = backend.kill(pane_id, kill_policy);
            });
        }

        Self::commit_events(command_id, events)
    }

    /// Handle [`Command::RenameTab`]: update the tab's display name.
    ///
    /// The caller supplies no name — a gimmick name is drawn from
    /// [`generate_name`], skipping every name already on one of the
    /// session's tabs (including the target's current one, so the rename
    /// always changes it). The rename applies through
    /// [`tab_ops::rename_tab`]. Names resolve nothing, so layout, focus,
    /// and PTYs are untouched.
    fn handle_rename_tab(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &RenameTabArgs,
    ) -> CommandResult {
        let acting = match self.acting_session(source) {
            Ok(acting) => acting,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let tab_id = match self.resolve_tab_or_active(args.tab, source, acting) {
            Ok(tab_id) => tab_id,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let Some(session_id) = acting.map(|session| session.id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };
        let new_name = generate_name(NameKind::Tab, |candidate| {
            session.tabs.values().any(|tab| tab.name() == candidate)
        });

        let events = tab_ops::rename_tab(session, tab_id, new_name);

        Self::commit_events(command_id, events)
    }

    /// Handle [`Command::FocusTab`]: switch the designated client's view to
    /// the target tab.
    ///
    /// The target client — an explicit `--client` (which wins even over the
    /// issuer, and rejects outright if not attached), else the issuer, else
    /// the session's sole attached client — switches to the tab the resolver
    /// named; `next`/`prev` wrap the display order relative to that client's
    /// active tab. Re-focusing the already-active tab is a clean no-op with
    /// no events. After the switch both affected tabs reflow: the target tab
    /// now counts the arriving viewer and the left tab no longer does; a tab
    /// left with no viewer keeps its sizes. A rejected focus mutates nothing.
    fn handle_focus_tab(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &FocusTabArgs,
    ) -> CommandResult {
        let acting = match self.acting_session(source) {
            Ok(acting) => acting,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let target = match Self::resolve_focus_tab_target(args, source, acting) {
            Ok(target) => target,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };

        let backend = Arc::clone(self.pty_backend());

        let Some(session) = self.sessions.get_mut(&target.session_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };
        let Some(client) = session.clients.get(target.client_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::SourceClientStale));
        };
        let prior_tab = client.active_tab();

        // Already viewing it — nothing to do, and no events: events are
        // completed facts.
        if prior_tab == target.tab_id {
            return TransactionScope::new().commit(command_id);
        }

        let mut events = tab_ops::focus_tab(
            session,
            target.client_id,
            tab_ops::TabTarget::Id(target.tab_id),
        );

        // Both tabs' viewer sets changed: the target tab gained the arriving
        // viewer, the left tab lost it. Reflow each that still has a viewer;
        // a tab with no viewer has no viewport and keeps its sizes.
        for tab_id in [target.tab_id, prior_tab] {
            Self::reflow_tab_if_viewed(
                backend.as_ref(),
                session,
                &self.pty_handles,
                &mut self.pty_sizes,
                &mut self.terminal_engines,
                tab_id,
                &mut events,
            );
        }

        Self::commit_events(command_id, events)
    }

    /// Handle [`Command::MoveTab`]: reorder the target tab to a new display
    /// slot within its session.
    ///
    /// The target tab — an explicit `--tab`, else the issuing pane's tab for
    /// the in-session CLI, else the issuer's active tab — moves to the
    /// requested zero-based index, clamped to the valid range, and the other
    /// tabs close ranks around it. Display order is the only thing that
    /// changes: which tab each client views, layout, focus, and PTYs are all
    /// untouched. Moving a tab to the slot it already occupies is a clean
    /// no-op with no events. A rejected move mutates nothing.
    fn handle_move_tab(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &MoveTabArgs,
    ) -> CommandResult {
        let acting = match self.acting_session(source) {
            Ok(acting) => acting,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let tab_id = match self.resolve_tab_or_active(args.tab, source, acting) {
            Ok(tab_id) => tab_id,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let Some(session_id) = acting.map(|session| session.id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };

        let events = tab_ops::move_tab(session, tab_id, args.index);

        Self::commit_events(command_id, events)
    }

    /// Handle [`Command::RenameSession`]: assign the session a fresh
    /// generated display name.
    ///
    /// The target is the explicit `session` argument, else the source's own
    /// session context ([`Self::resolve_session_target`]). The caller
    /// supplies no name — a gimmick name is drawn from
    /// [`generate_name`], skipping every name already on a session
    /// (including the target's current one, so the rename always changes
    /// it). The rename applies through [`session_ops::rename_session`];
    /// tabs, layout, focus, and PTYs are untouched.
    fn handle_rename_session(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &RenameSessionArgs,
    ) -> CommandResult {
        let acting = match self.acting_session(source) {
            Ok(acting) => acting,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let session_id = match self.resolve_session_target(args.session, source, acting) {
            Ok(session_id) => session_id,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let new_name = generate_name(NameKind::Session, |candidate| {
            self.sessions
                .values()
                .any(|session| session.name == candidate)
        });
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };

        let events = session_ops::rename_session(session, new_name);

        Self::commit_events(command_id, events)
    }

    /// Handle [`Command::TogglePaneFullscreen`]: switch the target pane's tab
    /// between the tiled view and a fullscreen of that pane.
    ///
    /// The target is the command's default pane — the in-session issuing
    /// pane, else the source client's focused pane. A fullscreen tab toggles
    /// back to tiled whichever pane resolved; a tiled tab fullscreens the
    /// target and, when the acting client's focus was elsewhere, moves its
    /// focus to the pane now filling the tab ([`Event::PaneFocused`]). The
    /// mode is a solve-time overlay — the tree is untouched, so toggling out
    /// restores the exact prior layout. The tab must be viewed by at least
    /// one attached client (the mode change resizes real PTYs), and a
    /// viewport too small to show the fullscreen pane at its content minimum
    /// rejects. Emits [`Event::LayoutChanged`] plus one [`Event::PtyResized`]
    /// per PTY whose solved size changed; a rejected toggle mutates nothing.
    fn handle_toggle_pane_fullscreen(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
    ) -> CommandResult {
        let acting = match self.acting_session(source) {
            Ok(acting) => acting,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let target = match self.resolve_default_pane(source, acting) {
            Ok(target) => target,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };

        let backend = Arc::clone(self.pty_backend());

        let Some(session) = self.sessions.get_mut(&target.session_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };
        let tab_id = match Self::tab_of_pane(session, target.pane_id) {
            Ok(tab_id) => tab_id,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let Some(viewport) = session.tab_viewport(tab_id) else {
            return Self::rejected(
                command_id,
                Rejection::new(
                    RejectReason::InvalidState,
                    "pane's tab is not viewed by any client",
                ),
            );
        };
        let Some(tab) = session.tabs.get_mut(&tab_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };

        // Flip the mode. Entering solves the candidate mode first: a viewport
        // too small to show the pane at its content minimum rejects before
        // anything mutates.
        let entered = match tab.layout_mode() {
            LayoutMode::Fullscreen { .. } => {
                tab.update_layout_mode(LayoutMode::Tiled);
                false
            }
            LayoutMode::Tiled => {
                let mode = LayoutMode::Fullscreen {
                    focused: target.pane_id,
                };
                let tab_rect = Rect::new(Point { x: 0, y: 0 }, viewport);
                let solved = solve_with_mode(tab.layout(), mode, tab_rect);
                if solved.suppressed.contains(&target.pane_id) {
                    return Self::rejected(
                        command_id,
                        Rejection::new(
                            RejectReason::InvalidState,
                            "not enough space to fullscreen the pane",
                        ),
                    );
                }
                tab.update_layout_mode(mode);
                true
            }
        };

        // The mode changed: re-solve the tab and resize each live PTY whose
        // size changed.
        let mut events = vec![Event::LayoutChanged(LayoutChanged { tab_id })];
        let rects = Self::tab_content_rects(session, tab_id, viewport);
        Self::reflow_changed(
            backend.as_ref(),
            &self.pty_handles,
            &mut self.pty_sizes,
            &mut self.terminal_engines,
            rects,
            None,
            &mut events,
        );

        // Entering fullscreen watches the target pane: the acting client's
        // focus moves to it when it was elsewhere.
        if entered {
            if let Some(client) = session.clients.get_mut(target.client_id) {
                let prior_pane = client.focused_pane(tab_id);
                if prior_pane != Some(target.pane_id) {
                    client.update_focused_pane(tab_id, target.pane_id);
                    if let Some(tab) = session.tabs.get_mut(&tab_id) {
                        tab.record_focus_mru(target.pane_id);
                    }
                    events.push(Event::PaneFocused(PaneFocused {
                        client_id: target.client_id,
                        tab_id,
                        pane_id: target.pane_id,
                        prior_pane,
                    }));
                }
            }
        }

        Self::commit_events(command_id, events)
    }

    /// Handle [`Command::ToggleLockMode`]: flip the acting client between
    /// pass-through [`LockMode::Locked`] and [`LockMode::Normal`].
    ///
    /// A client already locked unlocks; a client in any other mode locks. The
    /// toggle always changes the mode, so it always emits.
    fn handle_toggle_lock_mode(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
    ) -> CommandResult {
        self.set_lock_mode(command_id, source, |current| match current {
            LockMode::Locked => LockMode::Normal,
            _ => LockMode::Locked,
        })
    }

    /// Handle [`Command::SetLockMode`]: set the acting client to
    /// [`LockMode::Locked`] when `args.locked`, else [`LockMode::Normal`].
    ///
    /// Setting the mode the client already holds is a no-op: applied, zero
    /// events.
    fn handle_set_lock_mode(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &LockModeArgs,
    ) -> CommandResult {
        let next = if args.locked {
            LockMode::Locked
        } else {
            LockMode::Normal
        };
        self.set_lock_mode(command_id, source, move |_| next)
    }

    /// Set the acting client's [`LockMode`], emitting [`Event::InputModeChanged`]
    /// only when it changes. `resolve` maps the client's current mode to the
    /// next one, so the toggle and the explicit set share one path.
    ///
    /// Lock mode is client-scoped: the target is the acting client alone — no
    /// pane is resolved, so a client with no focused pane still locks. Nothing
    /// in the layout, focus, or any PTY changes; a no-op change mutates nothing.
    fn set_lock_mode(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        resolve: impl FnOnce(LockMode) -> LockMode,
    ) -> CommandResult {
        let session_id = match self.acting_session(source) {
            Ok(Some(session)) => session.id,
            Ok(None) => {
                return Self::rejected(
                    command_id,
                    Rejection::new(RejectReason::TargetNotFound, "no session context"),
                )
            }
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let Some(client_id) = source.client_id() else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::Unauthorized));
        };

        let Some(session) = self.sessions.get_mut(&session_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };
        let Some(client) = session.clients.get_mut(client_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::SourceClientStale));
        };

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
        scope.commit(command_id)
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
    fn handle_rename_pane(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &RenamePaneArgs,
    ) -> CommandResult {
        let acting = match self.acting_session(source) {
            Ok(acting) => acting,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let target = match self.resolve_pane_target(args.pane, source, acting) {
            Ok(target) => target,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let Some(session) = self.sessions.get_mut(&target.session_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };
        let new_name = generate_name(NameKind::Pane, |candidate| {
            session
                .panes
                .list()
                .any(|record| record.title.as_deref() == Some(candidate))
        });

        let events = pane_ops::rename_pane(session, target.pane_id, new_name);

        Self::commit_events(command_id, events)
    }

    /// Handle [`Command::WriteToPane`]: inject raw bytes into a pane's child
    /// stdin. The target is an explicit `--pane` (resolved globally) or the
    /// source's default pane, and must be live — a pane that has exited, is
    /// closing, or is gone takes no input ([`RejectReason::InvalidState`]). A
    /// plugin source is denied pending the `pane_write` capability.
    ///
    /// The write is a side effect that changes no session state, so a
    /// successful write commits no events; the child's response returns
    /// through the normal PTY output path. A backend write failure — the
    /// child died between the liveness check and the write — is reported to
    /// the caller as [`RejectReason::InvalidState`].
    fn handle_write_to_pane(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &WriteToPaneArgs,
    ) -> CommandResult {
        // Plugin input injection requires the `pane_write` capability granted
        // by the plugin host; until that lands a plugin source is denied.
        if matches!(source, CommandSource::Plugin { .. }) {
            return CommandResult::Rejected {
                command_id,
                reason: RejectReason::Unauthorized,
                help: Some("plugin lacks the pane_write capability".to_string()),
            };
        }
        let acting = match self.acting_session(source) {
            Ok(acting) => acting,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let target = match self.resolve_pane_target(args.pane, source, acting) {
            Ok(target) => target,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let Some(session) = self.sessions.get(&target.session_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };
        let Some(record) = session.panes.get(target.pane_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };
        match record.lifecycle() {
            PaneLifecycle::Spawning | PaneLifecycle::Running => {}
            PaneLifecycle::Exited { .. }
            | PaneLifecycle::Closing { .. }
            | PaneLifecycle::Removed => {
                return Self::rejected(
                    command_id,
                    Rejection::new(RejectReason::InvalidState, "pane is not accepting input"),
                );
            }
        }
        if self
            .pty_backend()
            .write(target.pane_id, &args.data)
            .is_err()
        {
            return Self::rejected(
                command_id,
                Rejection::new(RejectReason::InvalidState, "pane is not accepting input"),
            );
        }
        TransactionScope::new().commit(command_id)
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
                "pane has no neighbor on that side",
            ),
            ResizeError::MinSize { spare, .. } => Rejection::new(
                RejectReason::MinSize,
                &format!("the donating pane has only {spare} spare cells to give"),
            ),
        }
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
    ) -> Result<(Size, Option<ClientId>), Rejection> {
        let fits_viewport = |viewport: Size| {
            fits(
                candidate,
                Rect::new(Point { x: 0, y: 0 }, viewport),
                MIN_PANE_SIZE,
            )
        };
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
            // client, so the pane fits every client that will view the tab.
            let viewport = match existing {
                Some(existing) => Size {
                    cols: existing.cols.min(client.viewport().cols),
                    rows: existing.rows.min(client.viewport().rows),
                },
                None => client.viewport(),
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
        let mut attached = session.clients.list_attached();
        match (attached.next(), attached.next()) {
            (None, _) => Err(Rejection::new(
                RejectReason::InvalidState,
                "no attached client to view the new pane's tab",
            )),
            (Some(only), None) => {
                let viewport = only.viewport();
                if fits_viewport(viewport) {
                    Ok((viewport, Some(only.id())))
                } else {
                    Err(wont_fit())
                }
            }
            (Some(_), Some(_)) => Err(Rejection::new(
                RejectReason::TargetAmbiguous,
                "multiple clients; name a target client for the new pane",
            )),
        }
    }

    /// The per-pane content rects of `tab_id`'s current layout solved against
    /// `viewport`, or an empty vec when the tab is gone. Used to compare a tab's
    /// geometry before and after a change so only panes whose rect actually
    /// moved are resized.
    fn tab_content_rects(
        session: &Session,
        tab_id: TabId,
        viewport: Size,
    ) -> Vec<(PaneId, Option<Rect>)> {
        session
            .tabs
            .get(&tab_id)
            .map(|tab| content_rects(&solve_tab(tab, viewport)))
            .unwrap_or_default()
    }

    /// The viewport `tab_id` is solved against when a pane closes: the tab's
    /// own viewport when attached clients view it, else the smallest viewport
    /// among all attached clients, else a nominal 80x24.
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
                    .map(|client| client.viewport())
                    .reduce(|a, b| Size {
                        cols: a.cols.min(b.cols),
                        rows: a.rows.min(b.rows),
                    })
            })
            .unwrap_or(Size { cols: 80, rows: 24 })
    }

    /// Reflow `tab_id`'s live PTYs to its current effective size when a client
    /// still views it, appending one [`Event::PtyResized`] per pane actually
    /// resized. A tab no client views has no [`Session::tab_viewport`] and keeps
    /// its sizes. The shared shape behind every "a tab's viewer set changed"
    /// reflow — the full-tab solve with no freshly-spawned pane to skip.
    fn reflow_tab_if_viewed(
        backend: &dyn PtyBackend,
        session: &Session,
        pty_handles: &HashMap<PaneId, PtyHandle>,
        pty_sizes: &mut HashMap<PaneId, PtySize>,
        terminal_engines: &mut HashMap<PaneId, TerminalEngine>,
        tab_id: TabId,
        events: &mut Vec<Event>,
    ) {
        if let Some(viewport) = session.tab_viewport(tab_id) {
            let rects = Self::tab_content_rects(session, tab_id, viewport);
            Self::reflow_changed(
                backend,
                pty_handles,
                pty_sizes,
                terminal_engines,
                rects,
                None,
                events,
            );
        }
    }

    /// Resize the live PTYs in `rects` whose size actually changed, routing the
    /// batch through the shared [`resize_for_layout_change`] executor and pushing
    /// one [`Event::PtyResized`] per pane it resized.
    ///
    /// A pane is passed to the executor only when it has a live handle, is not
    /// `skip` (the freshly-spawned pane is sized separately), and its new
    /// [`compute_pty_size`] differs from `pty_sizes` — so an unchanged pane is
    /// left alone. The executor is stateless; this owns the last-set-size cache
    /// and the terminal-engine map, updating the cache and resizing the pane's
    /// engine grid for every pane it resizes, so engine and PTY agree on size.
    fn reflow_changed(
        backend: &dyn PtyBackend,
        pty_handles: &HashMap<PaneId, PtyHandle>,
        pty_sizes: &mut HashMap<PaneId, PtySize>,
        terminal_engines: &mut HashMap<PaneId, TerminalEngine>,
        rects: Vec<(PaneId, Option<Rect>)>,
        skip: Option<PaneId>,
        events: &mut Vec<Event>,
    ) {
        let items: Vec<(PaneId, Option<Rect>)> = rects
            .into_iter()
            .filter(|(pane_id, content)| {
                Some(*pane_id) != skip
                    && pty_handles.contains_key(pane_id)
                    && match content {
                        Some(rect) => pty_sizes.get(pane_id) != Some(&compute_pty_size(*rect)),
                        None => false,
                    }
            })
            .collect();
        for result in resize_for_layout_change(backend, items) {
            if let Some(size) = result.applied {
                pty_sizes.insert(result.pane_id, size);
                if let Some(engine) = terminal_engines.get_mut(&result.pane_id) {
                    engine.resize(size);
                }
                events.push(Event::PtyResized(PtyResized {
                    pane_id: result.pane_id,
                    size,
                }));
            }
        }
    }

    /// Check a command against live state before it reaches a handler. Runs the
    /// universal checks in fixed precedence: source policy, source-client
    /// liveness, session admission, then target resolution. Returns the first
    /// failure, or `Ok(())` when the command is well-formed against current
    /// state.
    fn validate(&self, envelope: &CommandEnvelope) -> Result<(), Rejection> {
        // 1. Source policy: a client-scoped command needs a client to act for.
        if Self::requires_issuing_client(&envelope.command) && envelope.source.client_id().is_none()
        {
            return Err(Rejection::new(
                RejectReason::Unauthorized,
                "command requires an attached client",
            ));
        }

        // 2. Source-client liveness + the session this command acts in.
        let session = self.acting_session(&envelope.source)?;

        // 3. Session admission: a winding-down session takes no mutations.
        if let Some(session) = session {
            if Self::is_winding_down(session) {
                return Err(Rejection::new(
                    RejectReason::InvalidState,
                    "session is stopping",
                ));
            }
        }

        // 4. Target resolution: the pane/tab/session the command names must resolve.
        self.resolve_target(&envelope.command, &envelope.source, session)
    }

    /// Whether `command` acts on a specific client's view (its focused pane,
    /// lock mode, or copy-mode state) and so cannot be issued by a source
    /// that names no client. [`Command::FocusPane`], [`Command::FocusTab`],
    /// and [`Command::NewTab`] are absent: each resolves its own target
    /// client (explicit `client` argument, issuing client, or the session's
    /// sole attached client) in its resolver.
    fn requires_issuing_client(command: &Command) -> bool {
        matches!(
            command,
            Command::ToggleLockMode
                | Command::SetLockMode(_)
                | Command::TogglePaneFullscreen
                | Command::CopyMode(_)
        )
    }

    /// Resolve the session a command acts in from its source, and confirm the
    /// source's client (when it names one) is still attached.
    ///
    /// An in-session CLI's own `session_id` is authoritative — the session is
    /// looked up by it, and the named client must be attached *there*; an
    /// inconsistent envelope (client attached elsewhere) is rejected. A
    /// keybinding/mouse names only a client and is located by it. An external
    /// CLI naming a session must match one. A missing
    /// session is [`RejectReason::TargetNotFound`]; a client not attached where
    /// expected is [`RejectReason::SourceClientStale`]. Sources with no session
    /// context (`Plugin`, `Internal`, external with no session) resolve to `None`.
    fn acting_session(&self, source: &CommandSource) -> Result<Option<&Session>, Rejection> {
        match source {
            CommandSource::InSessionCli {
                session_id,
                client_id,
                ..
            } => {
                let session = self
                    .sessions()
                    .get(session_id)
                    .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;
                if session.clients.get(*client_id).is_some() {
                    Ok(Some(session))
                } else {
                    Err(Rejection::bare(RejectReason::SourceClientStale))
                }
            }
            CommandSource::KeyBinding { client_id } | CommandSource::Mouse { client_id } => self
                .session_for_client(*client_id)
                .map(Some)
                .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale)),
            CommandSource::ExternalCli {
                session_id: Some(session_id),
            } => self
                .sessions()
                .get(session_id)
                .map(Some)
                .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound)),
            CommandSource::ExternalCli { session_id: None }
            | CommandSource::Plugin { .. }
            | CommandSource::Internal => Ok(None),
        }
    }

    /// Resolve the pane, tab, or session context a command needs, each at its
    /// correct scope: an explicit `--pane` is global ([`Self::resolve_pane_global`]);
    /// a focus target and a focused-pane default are the client's active tab
    /// ([`Self::require_pane_in_active_tab`]); an in-session-CLI default is the
    /// acting session ([`Self::resolve_pane_in_session`]); tab targets are the
    /// acting session's tabs; session-level commands need a resolved session.
    /// The match is exhaustive so a new `Command` variant must declare its scope.
    fn resolve_target(
        &self,
        command: &Command,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<(), Rejection> {
        match command {
            Command::FocusPane(args) => Self::resolve_focus_target(args, source, session).map(drop),
            Command::ClosePane(args) => self.resolve_pane_or_focused(args.pane, source, session),
            Command::ResizePane(args) => self.resolve_pane_or_focused(args.pane, source, session),
            Command::WriteToPane(args) => self.resolve_pane_or_focused(args.pane, source, session),
            Command::RenamePane(args) => self.resolve_pane_or_focused(args.pane, source, session),
            Command::NewPane(args) => self
                .resolve_new_pane_source(args, source, session)
                .map(drop),
            // Lock mode is client-scoped: the acting client (confirmed attached
            // by `acting_session`) is the whole target — no pane or tab to resolve.
            Command::ToggleLockMode | Command::SetLockMode(_) => Ok(()),
            Command::TogglePaneFullscreen | Command::CopyMode(_) => {
                self.resolve_default_pane(source, session).map(drop)
            }
            Command::CloseTab(args) => self
                .resolve_tab_or_active(args.tab, source, session)
                .map(drop),
            Command::RenameTab(args) => self
                .resolve_tab_or_active(args.tab, source, session)
                .map(drop),
            Command::MoveTab(args) => self
                .resolve_tab_or_active(args.tab, source, session)
                .map(drop),
            Command::FocusTab(args) => {
                Self::resolve_focus_tab_target(args, source, session).map(drop)
            }
            Command::NewTab(args) => Self::resolve_new_tab_target(args, source, session).map(drop),
            Command::RunCommandPane(args) => self
                .resolve_new_pane_source(&Self::run_command_new_pane_args(args), source, session)
                .map(drop),
            Command::RenameSession(args) => self
                .resolve_session_target(args.session, source, session)
                .map(drop),
            Command::Plugin(_) | Command::Quit => Ok(()),
        }
    }

    /// Resolve a [`Command::NewPane`] to its concrete target: the session and
    /// tab the new pane joins, the source pane it splits from, and the client to
    /// auto-focus it for. Shared by [`Self::validate`] (which drops the value)
    /// and [`Self::handle_new_pane`], so both agree on one resolution.
    ///
    /// An explicit `--pane` is global: the new pane joins whatever session owns
    /// that pane, focused for the acting client only when that client is
    /// attached there. With no `--pane`, the source defaults within the acting
    /// session — an in-session CLI's captured pane, or any other client's
    /// focused pane.
    fn resolve_new_pane_source(
        &self,
        args: &NewPaneArgs,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<NewPaneTarget, Rejection> {
        match args.source {
            Some(source_pane) => {
                let owner = self
                    .session_for_pane(source_pane)
                    .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;
                if Self::is_winding_down(owner) {
                    return Err(Rejection::new(
                        RejectReason::InvalidState,
                        "session is stopping",
                    ));
                }
                let tab_id = Self::tab_of_pane(owner, source_pane)?;
                let focus_client = source
                    .client_id()
                    .filter(|client_id| owner.clients.get(*client_id).is_some());
                Ok(NewPaneTarget {
                    session_id: owner.id,
                    source_pane,
                    tab_id,
                    focus_client,
                })
            }
            None => {
                let session = Self::require_session(session)?;
                match source {
                    CommandSource::InSessionCli {
                        pane_id, client_id, ..
                    } => {
                        // The captured pane defines its own tab; confirm it is
                        // still a live, registered leaf before splitting it.
                        Self::resolve_pane_in_session(session, *pane_id)?;
                        let tab_id = Self::tab_of_pane(session, *pane_id)?;
                        Ok(NewPaneTarget {
                            session_id: session.id,
                            source_pane: *pane_id,
                            tab_id,
                            focus_client: Some(*client_id),
                        })
                    }
                    _ => {
                        let client_id = source.client_id().ok_or_else(|| {
                            Rejection::new(
                                RejectReason::TargetNotFound,
                                "no target and no focused pane to default to",
                            )
                        })?;
                        let client = session
                            .clients
                            .get(client_id)
                            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
                        let tab_id = client.active_tab();
                        let pane = client.focused_pane(tab_id).ok_or_else(|| {
                            Rejection::new(RejectReason::TargetNotFound, "no focused pane")
                        })?;
                        // The focused pane must be a live leaf of the active
                        // tab — a stale focus entry is rejected, never split.
                        Self::require_pane_in_active_tab(session, client_id, pane)?;
                        Ok(NewPaneTarget {
                            session_id: session.id,
                            source_pane: pane,
                            tab_id,
                            focus_client: Some(client_id),
                        })
                    }
                }
            }
        }
    }

    /// Resolve the pane a pane-addressed command acts on, and the session and
    /// tab that own it.
    ///
    /// An explicit pane target is global: its owning session is found by
    /// registry membership, and a winding-down owner rejects. Without one, the
    /// in-session CLI targets the pane it was issued from, and any other
    /// source targets the issuing client's focused pane in its active tab —
    /// resolved through the shared defensive helpers, so a stale focus entry
    /// is rejected, never acted on.
    fn resolve_pane_target(
        &self,
        pane: Option<PaneId>,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<PaneTarget, Rejection> {
        match pane {
            Some(pane_id) => {
                let owner = self
                    .session_for_pane(pane_id)
                    .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;
                if Self::is_winding_down(owner) {
                    return Err(Rejection::new(
                        RejectReason::InvalidState,
                        "session is stopping",
                    ));
                }
                let tab_id = Self::tab_of_pane(owner, pane_id)?;
                Ok(PaneTarget {
                    session_id: owner.id,
                    tab_id,
                    pane_id,
                })
            }
            None => {
                let session = Self::require_session(session)?;
                match source {
                    CommandSource::InSessionCli { pane_id, .. } => {
                        // The captured pane defines its own tab; confirm it is
                        // still a live, registered leaf before acting on it.
                        Self::resolve_pane_in_session(session, *pane_id)?;
                        let tab_id = Self::tab_of_pane(session, *pane_id)?;
                        Ok(PaneTarget {
                            session_id: session.id,
                            tab_id,
                            pane_id: *pane_id,
                        })
                    }
                    _ => {
                        let client_id = source.client_id().ok_or_else(|| {
                            Rejection::new(
                                RejectReason::TargetNotFound,
                                "no target and no focused pane to default to",
                            )
                        })?;
                        let client = session
                            .clients
                            .get(client_id)
                            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
                        let tab_id = client.active_tab();
                        let pane_id = client.focused_pane(tab_id).ok_or_else(|| {
                            Rejection::new(RejectReason::TargetNotFound, "no focused pane")
                        })?;
                        // The focused pane must be a live leaf of the active
                        // tab — a stale focus entry is rejected, never acted
                        // on.
                        Self::require_pane_in_active_tab(session, client_id, pane_id)?;
                        Ok(PaneTarget {
                            session_id: session.id,
                            tab_id,
                            pane_id,
                        })
                    }
                }
            }
        }
    }

    /// The id of the tab in `session` whose layout holds `pane` as a leaf, or
    /// [`RejectReason::TargetNotFound`] when no tab does.
    fn tab_of_pane(session: &Session, pane: PaneId) -> Result<TabId, Rejection> {
        session
            .tabs
            .values()
            .find(|tab| tab.layout().contains_pane(pane))
            .map(|tab| tab.id())
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))
    }

    /// Resolve an explicit pane target, or the default pane when none is given.
    fn resolve_pane_or_focused(
        &self,
        pane: Option<PaneId>,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<(), Rejection> {
        match pane {
            Some(pane) => self.resolve_pane_global(pane),
            None => self.resolve_default_pane(source, session).map(drop),
        }
    }

    /// Resolve the pane a command defaults to when it names none, scoped to the
    /// **acting session** — a default target never escapes it. An in-session CLI
    /// command targets the pane it was issued from
    /// ([`CommandSource::InSessionCli`]'s `pane_id`) within that session; any
    /// other client source targets that client's focused pane. Fails with
    /// [`RejectReason::TargetNotFound`] when there is no such context — a target
    /// is never silently guessed. Shared by validation and the target-less
    /// handlers so both apply one contract.
    fn resolve_default_pane(
        &self,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<DefaultPaneTarget, Rejection> {
        let session = Self::require_session(session)?;
        match source {
            CommandSource::InSessionCli {
                client_id, pane_id, ..
            } => {
                Self::resolve_pane_in_session(session, *pane_id)?;
                Ok(DefaultPaneTarget {
                    session_id: session.id,
                    client_id: *client_id,
                    pane_id: *pane_id,
                })
            }
            _ => match source.client_id() {
                Some(client_id) => {
                    let pane_id = Self::resolve_focused_pane(session, client_id)?;
                    Ok(DefaultPaneTarget {
                        session_id: session.id,
                        client_id,
                        pane_id,
                    })
                }
                None => Err(Rejection::new(
                    RejectReason::TargetNotFound,
                    "no target and no focused pane to default to",
                )),
            },
        }
    }

    /// Confirm `pane` exists in `session`'s registry. Used for the in-session
    /// CLI default, whose target is the captured `pane_id` (tied to the session,
    /// not to live focus) — never the global registry.
    fn resolve_pane_in_session(session: &Session, pane: PaneId) -> Result<(), Rejection> {
        if session.panes.get(pane).is_some() {
            Ok(())
        } else {
            Err(Rejection::bare(RejectReason::TargetNotFound))
        }
    }

    /// Confirm `pane` is in the client's **active tab** layout AND has a live
    /// registry record. Focus is tab-local, so every focus-derived target
    /// (explicit [`Command::FocusPane`] and the focused-pane default alike)
    /// resolves through here — a pane in another tab, absent from the registry,
    /// or in a different session is rejected.
    fn require_pane_in_active_tab(
        session: &Session,
        client_id: ClientId,
        pane: PaneId,
    ) -> Result<(), Rejection> {
        if session.panes.get(pane).is_none() {
            return Err(Rejection::bare(RejectReason::TargetNotFound));
        }
        let client = session
            .clients
            .get(client_id)
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
        let tab = session
            .tabs
            .get(&client.active_tab())
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;
        if tab.layout().contains_pane(pane) {
            Ok(())
        } else {
            Err(Rejection::new(
                RejectReason::TargetNotFound,
                "pane not in the client's active tab",
            ))
        }
    }

    /// Resolve the [`Command::FocusPane`] target: the client whose focus moves
    /// and the pane, which must live in that client's active tab. Shared by
    /// validation and [`Self::handle_focus_pane`] so both apply one contract.
    ///
    /// The target client is the explicit `client` argument when set — it wins
    /// even over an in-session issuer, and one not attached to the acting
    /// session is [`RejectReason::TargetNotFound`], never a fallback to the
    /// issuer. With no explicit target the issuing client is used; a source
    /// with no client defaults to the session's sole attached client, a
    /// session with several is [`RejectReason::TargetAmbiguous`], and one with
    /// none is [`RejectReason::InvalidState`]. Focus is tab-local, so the pane
    /// resolves through [`Self::require_pane_in_active_tab`]. A
    /// [`FocusTarget::Direction`] target is rejected as
    /// [`RejectReason::InvalidState`]: the geometric neighbor lookup is not
    /// implemented yet.
    fn resolve_focus_target(
        args: &FocusPaneArgs,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<FocusPaneTarget, Rejection> {
        let pane_id = match args.target {
            FocusTarget::Pane(pane_id) => pane_id,
            FocusTarget::Direction(_) => {
                return Err(Rejection::new(
                    RejectReason::InvalidState,
                    "directional focus not yet implemented",
                ))
            }
        };
        let session = Self::require_session(session)?;
        let client_id = match args.client.or_else(|| source.client_id()) {
            Some(client_id) => {
                if session.clients.get(client_id).is_none() {
                    return Err(Rejection::new(
                        RejectReason::TargetNotFound,
                        "target client not attached to the session",
                    ));
                }
                client_id
            }
            None => {
                let mut attached = session.clients.list_attached();
                match (attached.next(), attached.next()) {
                    (None, _) => {
                        return Err(Rejection::new(
                            RejectReason::InvalidState,
                            "no attached client whose focus could move",
                        ))
                    }
                    (Some(only), None) => only.id(),
                    (Some(_), Some(_)) => {
                        return Err(Rejection::new(
                            RejectReason::TargetAmbiguous,
                            "multiple clients; name a target client for the focus",
                        ))
                    }
                }
            }
        };
        Self::require_pane_in_active_tab(session, client_id, pane_id)?;
        let tab_id = session
            .clients
            .get(client_id)
            .map(|client| client.active_tab())
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
        Ok(FocusPaneTarget {
            session_id: session.id,
            client_id,
            tab_id,
            pane_id,
        })
    }

    /// Resolve the client's focused pane. A client with no focused pane is
    /// [`RejectReason::TargetNotFound`]; a focus pointing outside the active tab
    /// is verified and rejected too, not assumed valid.
    fn resolve_focused_pane(session: &Session, client_id: ClientId) -> Result<PaneId, Rejection> {
        let client = session
            .clients
            .get(client_id)
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
        let pane = client
            .focused_pane(client.active_tab())
            .ok_or_else(|| Rejection::new(RejectReason::TargetNotFound, "no focused pane"))?;
        Self::require_pane_in_active_tab(session, client_id, pane)?;
        Ok(pane)
    }

    /// Resolve an explicit tab target within the acting session, or the default
    /// tab when none is given. An in-session CLI defaults to the tab containing
    /// its source `pane_id` (the command targets the source pane's context, even
    /// if the client has since switched tabs); any other client source defaults
    /// to its live `active_tab`. Fails with [`RejectReason::TargetNotFound`]
    /// when there is no session/client context or the tab is gone.
    fn resolve_tab_or_active(
        &self,
        tab: Option<TabId>,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<TabId, Rejection> {
        let session = Self::require_session(session)?;
        match tab {
            Some(tab) => {
                Self::require_tab(session, tab)?;
                Ok(tab)
            }
            None => {
                if let CommandSource::InSessionCli { pane_id, .. } = source {
                    return Self::require_tab_containing_pane(session, *pane_id);
                }
                let client_id = source.client_id().ok_or_else(|| {
                    Rejection::new(
                        RejectReason::TargetNotFound,
                        "no tab target and no active tab to default to",
                    )
                })?;
                let client = session
                    .clients
                    .get(client_id)
                    .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
                let tab = client.active_tab();
                Self::require_tab(session, tab)?;
                Ok(tab)
            }
        }
    }

    /// Find the tab in `session` whose layout contains `pane`, confirming the
    /// pane also has a live registry record.
    fn require_tab_containing_pane(session: &Session, pane: PaneId) -> Result<TabId, Rejection> {
        Self::resolve_pane_in_session(session, pane)?;
        session
            .tabs
            .values()
            .find(|tab| tab.layout().contains_pane(pane))
            .map(|tab| tab.id())
            .ok_or_else(|| {
                Rejection::new(
                    RejectReason::TargetNotFound,
                    "source pane not found in any tab",
                )
            })
    }

    /// Resolve the client a tab-view command acts for: the explicit `client`
    /// argument when set — it wins even over an in-session issuer, and one
    /// not attached to the acting session is [`RejectReason::TargetNotFound`],
    /// never a fallback to the issuer. With no explicit target the issuing
    /// client is used; a source with no client defaults to the session's sole
    /// attached client, a session with several is
    /// [`RejectReason::TargetAmbiguous`], and one with none is
    /// [`RejectReason::InvalidState`]. `what` names the command's object in
    /// the rejection hints.
    fn resolve_tab_client(
        explicit: Option<ClientId>,
        source: &CommandSource,
        session: &Session,
        what: &str,
    ) -> Result<ClientId, Rejection> {
        match explicit.or_else(|| source.client_id()) {
            Some(client_id) => {
                if session.clients.get(client_id).is_none() {
                    return Err(Rejection::new(
                        RejectReason::TargetNotFound,
                        "target client not attached to the session",
                    ));
                }
                Ok(client_id)
            }
            None => {
                let mut attached = session.clients.list_attached();
                match (attached.next(), attached.next()) {
                    (None, _) => Err(Rejection::new(
                        RejectReason::InvalidState,
                        &format!("no attached client to switch onto {what}"),
                    )),
                    (Some(only), None) => Ok(only.id()),
                    (Some(_), Some(_)) => Err(Rejection::new(
                        RejectReason::TargetAmbiguous,
                        &format!("multiple clients; name a target client for {what}"),
                    )),
                }
            }
        }
    }

    /// Resolve the [`Command::NewTab`] target: the session the tab joins and
    /// the client that switches onto it ([`Self::resolve_tab_client`]).
    /// Shared by validation and [`Self::handle_new_tab`] so both apply one
    /// contract.
    fn resolve_new_tab_target(
        args: &NewTabArgs,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<NewTabTarget, Rejection> {
        let session = Self::require_session(session)?;
        let client_id = Self::resolve_tab_client(args.client, source, session, "the new tab")?;
        Ok(NewTabTarget {
            session_id: session.id,
            client_id,
        })
    }

    /// Resolve the [`Command::FocusTab`] target: the client whose view
    /// switches ([`Self::resolve_tab_client`]) and the concrete tab the
    /// target names — an id or index must match an existing tab, and
    /// `next`/`prev` step from the *target* client's active tab, wrapping at
    /// the ends. Shared by validation and [`Self::handle_focus_tab`] so both
    /// apply one contract.
    fn resolve_focus_tab_target(
        args: &FocusTabArgs,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<FocusTabTarget, Rejection> {
        let session = Self::require_session(session)?;
        let client_id = Self::resolve_tab_client(args.client, source, session, "the target tab")?;
        let client = session
            .clients
            .get(client_id)
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
        let target = match args.target {
            TabTarget::Id(id) => tab_ops::TabTarget::Id(id),
            TabTarget::Index(index) => tab_ops::TabTarget::Index(index),
            TabTarget::Next => tab_ops::TabTarget::Next,
            TabTarget::Prev => tab_ops::TabTarget::Prev,
        };
        let tab_id = tab_ops::resolve_tab_target(session, client.active_tab(), target)
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;
        Ok(FocusTabTarget {
            session_id: session.id,
            client_id,
            tab_id,
        })
    }

    /// The acting session, or [`RejectReason::TargetNotFound`] when a
    /// session-scoped command has no session context to resolve within.
    fn require_session(session: Option<&Session>) -> Result<&Session, Rejection> {
        session.ok_or_else(|| Rejection::new(RejectReason::TargetNotFound, "no session context"))
    }

    /// Resolve a [`Command::RenameSession`] to its target session. Shared by
    /// validation and [`Self::handle_rename_session`] so both apply one
    /// contract.
    ///
    /// The target is the explicit `session` argument when set — an id
    /// matching no session is [`RejectReason::TargetNotFound`], never a
    /// fallback. With no explicit target the source's own session context is
    /// used — the session the in-session CLI runs inside, the one an
    /// external CLI named on its envelope, or the keybinding issuer's
    /// session; any other source (mouse, plugin, internal, external with no
    /// session) is [`RejectReason::InvalidState`] — it must name a session.
    /// A resolved session that is winding down is
    /// [`RejectReason::InvalidState`], so a target named by id cannot slip
    /// past the session admission check.
    fn resolve_session_target(
        &self,
        explicit: Option<SessionId>,
        source: &CommandSource,
        acting: Option<&Session>,
    ) -> Result<SessionId, Rejection> {
        let session = match explicit {
            Some(session_id) => self
                .sessions
                .get(&session_id)
                .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?,
            None => match source {
                CommandSource::InSessionCli { .. }
                | CommandSource::ExternalCli {
                    session_id: Some(_),
                }
                | CommandSource::KeyBinding { .. } => Self::require_session(acting)?,
                _ => {
                    return Err(Rejection::new(
                        RejectReason::InvalidState,
                        "name a target session",
                    ))
                }
            },
        };
        if Self::is_winding_down(session) {
            return Err(Rejection::new(
                RejectReason::InvalidState,
                "session is stopping",
            ));
        }
        Ok(session.id)
    }

    /// Confirm `tab` exists in `session`.
    fn require_tab(session: &Session, tab: TabId) -> Result<(), Rejection> {
        if session.tabs.contains_key(&tab) {
            Ok(())
        } else {
            Err(Rejection::bare(RejectReason::TargetNotFound))
        }
    }

    /// Look a pane up across every session's registry, the way an explicit
    /// `--pane` target resolves. Absent everywhere is
    /// [`RejectReason::TargetNotFound`]; found in a session that is winding down
    /// is [`RejectReason::InvalidState`], so a cross-session target cannot slip
    /// past the owning session's admission check.
    fn resolve_pane_global(&self, pane: PaneId) -> Result<(), Rejection> {
        match self.session_for_pane(pane) {
            None => Err(Rejection::bare(RejectReason::TargetNotFound)),
            Some(session) if Self::is_winding_down(session) => Err(Rejection::new(
                RejectReason::InvalidState,
                "session is stopping",
            )),
            Some(_) => Ok(()),
        }
    }

    /// Whether `session` is shutting down (`Stopping`/`Stopped`) and so accepts
    /// no mutations.
    fn is_winding_down(session: &Session) -> bool {
        matches!(
            session.lifecycle(),
            SessionLifecycle::Stopping | SessionLifecycle::Stopped
        )
    }
}

#[cfg(test)]
mod tests;
