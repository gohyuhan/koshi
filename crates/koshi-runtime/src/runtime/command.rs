//! Command dispatch: the single entrypoint every requested mutation passes
//! through.
//!
//! `Server::dispatch` — reached from outside the crate through
//! [`Server::submit_command`] — validates one [`CommandEnvelope`] against live
//! state, then routes it via an exhaustive `match` on [`Command`] — one arm per
//! variant. Validation runs first: a command whose source may not issue it, or
//! whose target does not resolve, is rejected before any handler runs. A
//! command with no handler rejects with [`RejectReason::InvalidState`] and a
//! hint naming it. The match is exhaustive, so every `Command` variant has an
//! arm here.
//!
//! This file holds the dispatch table, target resolution types, the helpers
//! every handler shares, and the handlers for the commands that end a session
//! or leave one — quit, detach, detach-all, and the switch that moves one
//! client to another session. The rest live in submodules by what they act on:
//! `pane`, `tab`, `client`, `visual`, with target resolution in `resolve`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::SystemTime;

use crate::runtime::{
    bus::EventBus, render_schedule::InvalidationReason, snapshot::solve_tab, spawn_env::koshi_env,
    transaction::TransactionScope,
};
use crate::server::Server;
use koshi_core::{
    command::{
        ClearSelectionArgs, ClosePaneArgs, CloseTabArgs, Command, CommandEnvelope, CommandResult,
        CommandSource, CopyArgs, DetachArgs, FocusPaneArgs, FocusTabArgs, FocusTarget, GridPos,
        LockModeArgs, MoveTabArgs, NewPaneArgs, NewTabArgs, ResizePaneArgs, RunCommandPaneArgs,
        Selection, SelectionKind, SetSelectionArgs, SwitchSessionArgs, TabTarget,
        ToggleLockModeArgs, VisualCommand, WriteToPaneArgs,
    },
    event::{
        Event, InputMode, InputModeChanged, LayoutChanged, MouseSelectChanged, PaneFocused,
        PtyResized, RejectReason, SelectionChanged,
    },
    geometry::{Direction, PaneArea, Rect, Size},
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
    resize::{resize_with_min, ResizeError},
    solver::{fits, solve_with_min, solve_with_mode_min, PaneSizing},
    tree::LayoutNode,
};
use koshi_pane::pane::{
    lifecycle::PaneLifecycle,
    policy::PaneClosePolicy,
    state::{PaneKind, PaneRecord},
};
use koshi_pty::backend::state::{PtyBackend, PtyHandle};
use koshi_pty::resize::{compute_pty_size, resize_for_layout_change};
use koshi_session::client::{Client, ClientOrigin};
use koshi_session::session::{
    cascade::{on_child_exit, remove_pane_cascade},
    lifecycle::SessionLifecycle,
    pane_ops::{self, NewPaneSpec},
    policy::EmptyTabPolicy,
    state::Session,
    tab_ops,
};

/// The PTY size for a tab's sole root pane filling `viewport`: solve the
/// single-pane layout, take the root's content rect, and clamp it to a PTY size.
/// Shared by the new-tab path and genesis.
///
/// A pane the solve gives no content rect falls back to the whole `viewport`
/// rect.
pub(crate) fn size_root_pane(pane_id: PaneId, viewport: Size, sizing: PaneSizing) -> PtySize {
    let candidate = LayoutNode::Pane(pane_id);
    let tab_rect = Rect::at_origin(viewport);
    let rects = content_rects(&solve_with_min(&candidate, tab_rect, sizing));
    let rect = rects
        .iter()
        .find(|(id, _)| *id == pane_id)
        .and_then(|(_, content)| *content)
        .unwrap_or(tab_rect);
    compute_pty_size(rect)
}

/// The PTY size for every pane in `layout` filling `viewport`: solve the whole
/// tree once, then clamp each pane's content rect to a PTY size, in layout
/// order. A multi-pane tab's panes each spawn at their tiled slice this way,
/// not the whole tab. A pane the solve suppressed for lack of space has no
/// content rect and falls back to the full tab rect — the same floor
/// [`size_root_pane`] uses.
pub(crate) fn pane_spawn_sizes(
    layout: &LayoutNode,
    viewport: Size,
    sizing: PaneSizing,
) -> Vec<(PaneId, PtySize)> {
    let tab_rect = Rect::at_origin(viewport);
    content_rects(&solve_with_min(layout, tab_rect, sizing))
        .into_iter()
        .map(|(pane, content)| (pane, compute_pty_size(content.unwrap_or(tab_rect))))
        .collect()
}

/// The overlap length of the spans `[a_start, a_start + a_len)` and
/// `[b_start, b_start + b_len)`, `0` when they are disjoint. Used by the
/// directional focus lookup to require that a neighbor actually shares rows
/// (or columns) with the pane focus moves from.
fn span_overlap(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u16 {
    let start = a_start.max(b_start);
    let end = (a_start + a_len).min(b_start + b_len);
    end.saturating_sub(start)
}

/// The tab named by the first [`Event::TabFocused`] in `events`, or `None` when
/// `events` holds none.
fn tab_focused_in(events: &[Event]) -> Option<TabId> {
    events.iter().find_map(|event| match event {
        Event::TabFocused(focused) => Some(focused.tab_id),
        _ => None,
    })
}

/// A validation failure: the reason a command was rejected, plus an optional
/// human-facing hint. The `Err` half of [`Server::validate`].
struct Rejection {
    reason: RejectReason,
    help: Option<String>,
    /// Cells the donating pane can still give, for a resize refused at a pane
    /// minimum; `None` for every other rejection. A rejection carrying this is
    /// the one [`Server::rejected`] leaves unlogged.
    spare: Option<u16>,
}

impl Rejection {
    /// A rejection with the given reason and a hint string.
    fn new(reason: RejectReason, help: &str) -> Self {
        Rejection {
            reason,
            help: Some(help.to_string()),
            spare: None,
        }
    }

    /// A rejection with the given reason and no hint.
    fn bare(reason: RejectReason) -> Self {
        Rejection {
            reason,
            help: None,
            spare: None,
        }
    }

    /// A resize refused at a pane minimum, carrying the `spare` cells the
    /// donating pane can still give in both the hint and the field.
    fn min_size(spare: u16) -> Self {
        Rejection {
            reason: RejectReason::MinSize,
            help: Some(format!(
                "the donating pane has only {spare} spare cells to give"
            )),
            spare: Some(spare),
        }
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
/// [`Server::resolve_focus_target`].
struct FocusPaneTarget {
    session_id: SessionId,
    client_id: ClientId,
    tab_id: TabId,
    pane_id: PaneId,
}

/// A resolved [`Command::TogglePaneFullscreen`] target: the session, the client
/// whose own view flips, that client's tab, and the pane the zoom fills it
/// with. The `Ok` half of [`Server::resolve_fullscreen_target`].
struct FullscreenTarget {
    session_id: SessionId,
    client_id: ClientId,
    tab_id: TabId,
    pane_id: PaneId,
}

/// A resolved [`Command::NewTab`] target: the session the tab joins and the
/// client that switches onto it. The `Ok` half of
/// [`Server::resolve_new_tab_target`].
struct NewTabTarget {
    session_id: SessionId,
    client_id: ClientId,
}

/// A resolved [`Command::FocusTab`] target: the session, the client whose
/// view switches, and the concrete tab the target named. The `Ok` half of
/// [`Server::resolve_focus_tab_target`].
struct FocusTabTarget {
    session_id: SessionId,
    client_id: ClientId,
    tab_id: TabId,
}

impl Server {
    /// Dispatch one command and report its outcome.
    ///
    /// Every mutation enters here; nothing mutates session, layout, or pane
    /// state outside a handler reached through this method. The command is
    /// validated first (target resolution, source policy); a command that
    /// passes validation but has no handler yet is rejected with
    /// [`RejectReason::InvalidState`]. A command that reaches its handler
    /// schedules a repaint, whichever entry point — key binding, IPC, or
    /// plugin — delivered it.
    pub(crate) fn dispatch(&mut self, envelope: CommandEnvelope) -> CommandResult {
        self.dispatch_reporting_spare(envelope).0
    }

    /// [`dispatch`](Self::dispatch), also handing back the cells the donating
    /// pane can still give when a resize was refused at a pane minimum.
    ///
    /// The second half is `Some` only for that refusal; every other outcome,
    /// including success, gives `None`. The mouse layer reads it to ask again
    /// for exactly the cells a border still has room to move.
    pub(crate) fn dispatch_reporting_spare(
        &mut self,
        envelope: CommandEnvelope,
    ) -> (CommandResult, Option<u16>) {
        let command_id = envelope.id;
        if let Err(rejection) = self.validate(&envelope) {
            return (Self::rejected(command_id, rejection), None);
        }
        let outcome = match envelope.command {
            Command::NewPane(args) => {
                self.handle_new_pane(command_id, &envelope.source, &args, envelope.issued_at)
            }
            Command::ClosePane(args) => self.handle_close_pane(command_id, &envelope.source, &args),
            Command::ResizePane(args) => {
                self.handle_resize_pane(command_id, &envelope.source, &args)
            }
            Command::FocusPane(args) => self.handle_focus_pane(command_id, &envelope.source, &args),
            Command::NewTab(args) => {
                self.handle_new_tab(command_id, &envelope.source, &args, envelope.issued_at)
            }
            Command::CloseTab(args) => self.handle_close_tab(command_id, &envelope.source, &args),
            Command::FocusTab(args) => self.handle_focus_tab(command_id, &envelope.source, &args),
            Command::WriteToPane(args) => {
                self.handle_write_to_pane(command_id, &envelope.source, &args)
            }
            Command::ToggleLockMode(args) => {
                self.handle_toggle_lock_mode(command_id, &envelope.source, &args)
            }
            Command::SetLockMode(args) => {
                self.handle_set_lock_mode(command_id, &envelope.source, &args)
            }
            Command::ToggleMouseSelect => {
                self.handle_toggle_mouse_select(command_id, &envelope.source)
            }
            Command::RunCommandPane(args) => {
                let new_pane_args = Self::run_command_new_pane_args(&args);
                self.handle_new_pane(
                    command_id,
                    &envelope.source,
                    &new_pane_args,
                    envelope.issued_at,
                )
            }
            Command::Visual(command) => self.handle_visual(command_id, &envelope.source, &command),
            Command::Plugin(_) => Ok(self.reject(command_id, "plugin")),
            Command::Quit => Ok(self.handle_quit(command_id, &envelope.source)),
            Command::Detach(args) => self.handle_detach(command_id, &envelope.source, &args),
            Command::DetachAll => self.handle_detach_all(command_id, &envelope.source),
            Command::TogglePaneFullscreen => {
                self.handle_toggle_pane_fullscreen(command_id, &envelope.source)
            }
            Command::MoveTab(args) => self.handle_move_tab(command_id, &envelope.source, &args),
            Command::SwitchSession(args) => {
                self.handle_switch_session(command_id, &envelope.source, &args)
            }
        };
        self.render_scheduler
            .invalidate(InvalidationReason::StatusChanged);
        match outcome {
            Ok(result) => (result, None),
            Err(rejection) => {
                let spare = rejection.spare;
                (Self::rejected(command_id, rejection), spare)
            }
        }
    }

    /// Build a rejection for a command with no handler, keyed back to its
    /// originating envelope by `command_id`, and log it at `warn`. `label` names
    /// the command in the log line and in the hint, which reads
    /// `"<label> not yet implemented"`.
    fn reject(&self, command_id: CommandId, label: &str) -> CommandResult {
        tracing::warn!(
            command_id = %command_id,
            command = label,
            "command rejected; no handler for it yet"
        );
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some(format!("{label} not yet implemented")),
        }
    }

    /// The client a command came from, for commands that act on that client's
    /// own state and can act on no other — a highlight belongs to the screen
    /// that made it, so a source whose client is gone has nothing to act on
    /// and gets [`RejectReason::SourceClientStale`] rather than the
    /// sole-attached-client stand-in [`Server::resolve_acting_client`] applies.
    ///
    /// This is the check itself, not an assertion about an earlier one:
    /// [`Self::resolve_target`] calls it for the selection commands, and the
    /// handlers call it again to get the id.
    fn issuing_client(source: &CommandSource) -> Result<ClientId, Rejection> {
        source
            .client_id()
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))
    }

    /// Confirm `pane_id` still exists in the session `client_id` is attached to.
    fn require_pane(&self, client_id: ClientId, pane_id: PaneId) -> Result<(), Rejection> {
        let exists = self
            .session_for_client(client_id)
            .is_some_and(|session| session.panes.get(pane_id).is_some());
        if exists {
            Ok(())
        } else {
            Err(Rejection::bare(RejectReason::TargetGone))
        }
    }

    /// Turn a [`Rejection`] into a [`CommandResult::Rejected`] keyed to
    /// `command_id`.
    ///
    /// Every rejection a handler or validation produces is built here, and
    /// logged here at `warn`: the command did not apply, state is untouched,
    /// and the session carries on. A border move refused at a pane minimum —
    /// the rejection carrying `spare` — is not logged.
    fn rejected(command_id: CommandId, rejection: Rejection) -> CommandResult {
        if rejection.spare.is_none() {
            tracing::warn!(
                command_id = %command_id,
                reason = %rejection.reason,
                help = rejection.help.as_deref(),
                "command rejected"
            );
        }
        CommandResult::Rejected {
            command_id,
            reason: rejection.reason,
            help: rejection.help,
        }
    }

    /// Seal `events` as one committed transaction keyed to `command_id`: emit
    /// each event into a fresh [`TransactionScope`] in order, then commit,
    /// delivering the batch to every subscriber on `bus`.
    fn commit_events(
        bus: &mut EventBus,
        command_id: CommandId,
        events: Vec<Event>,
    ) -> CommandResult {
        let mut scope = TransactionScope::new();
        for event in events {
            scope.emit(event);
        }
        scope.commit(command_id, bus)
    }

    /// Launch a pane's child process, mapping a backend failure onto the shared
    /// "failed to launch" rejection. Every launch-then-commit path calls this
    /// before mutating any session state.
    fn spawn_child(
        backend: &dyn PtyBackend,
        pane_id: PaneId,
        spec: SpawnSpec,
        size: PtySize,
    ) -> Result<PtyHandle, Rejection> {
        backend.spawn(pane_id, spec, size).map_err(|_| {
            Rejection::new(
                RejectReason::InvalidState,
                "failed to launch the pane's process",
            )
        })
    }

    /// Add koshi's configured terminal identity — `TERM` and `COLORTERM` from
    /// the `terminal` config section — to a spawned child's environment overlay,
    /// filling each only when the pane's own env has not already set it: an
    /// explicit per-pane value (a profile pane's `env`) is kept.
    pub(crate) fn terminal_identity_env(
        &self,
        mut env: BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        env.entry("TERM".to_string())
            .or_insert_with(|| self.config.terminal.term.clone());
        env.entry("COLORTERM".to_string())
            .or_insert_with(|| self.config.terminal.colorterm.clone());
        env
    }

    /// The spawn spec for a default-shell pane: the configured
    /// `terminal.default_shell` when set, otherwise the platform default from
    /// `$SHELL` / `%COMSPEC%`. Either way it carries koshi's terminal identity
    /// in its environment.
    pub(crate) fn default_shell_spec(
        &self,
        cwd: Option<PathBuf>,
        env: BTreeMap<String, String>,
    ) -> SpawnSpec {
        let env = self.terminal_identity_env(env);
        match &self.config.terminal.default_shell {
            Some(program) => SpawnSpec::shell(PathBuf::from(program), cwd, env),
            None => SpawnSpec::default_shell(cwd, env),
        }
    }

    /// Map [`Command::RunCommandPane`] onto the [`NewPaneArgs`] that realize it:
    /// its command is required (never the default shell), and its source
    /// pane, placement — split direction or stacking — and working directory
    /// carry through to the new-pane transaction. [`Self::dispatch`] and
    /// [`Self::resolve_target`] both call it, so the validate pre-check and
    /// the handler read the same anchor pane.
    fn run_command_new_pane_args(args: &RunCommandPaneArgs) -> NewPaneArgs {
        NewPaneArgs {
            source: args.source,
            tab: args.tab,
            direction: args.direction,
            stacked: args.stacked,
            cwd: args.cwd.clone(),
            command: Some(args.command.clone()),
            client: args.client,
        }
    }

    /// The live working directory of `pane`, best answer first: the shell's
    /// own OSC 7 report (when it names this machine), then the OS's answer
    /// for the child process, then the directory the pane was spawned in.
    /// `None` when nothing knows — a spawn using this then inherits koshi's
    /// own directory. Every answer is already at hand or one non-blocking
    /// OS call.
    pub(super) fn pane_live_cwd(&self, session_id: SessionId, pane: PaneId) -> Option<PathBuf> {
        if let Some(reported) = self
            .terminal_engines
            .get(&pane)
            .and_then(|engine| engine.state().current_cwd())
        {
            if is_local_host(reported.host()) {
                return Some(reported.path().to_path_buf());
            }
        }
        if let Some(cwd) = self.pty_backend().live_cwd(pane) {
            return Some(cwd);
        }
        self.sessions.get(&session_id)?.panes.get(pane)?.cwd.clone()
    }

    /// Mark the process for immediate teardown: the event loop polls the quit
    /// request before it waits for an event and after each event batch, exits
    /// once [`awaits_a_client`](Server::awaits_a_client) is false, and teardown
    /// group-kills every pane's child without the graceful window.
    pub(crate) fn request_quit(&mut self) {
        self.request_graceful_quit();
        self.immediate_shutdown = true;
    }

    /// Mark the process for teardown, keeping the graceful window: the event
    /// loop exits as above, and teardown asks each pane's process group to stop
    /// and waits up to [`GRACEFUL_TIMEOUT_DURATION`](koshi_core::constant::GRACEFUL_TIMEOUT_DURATION)
    /// before group-killing it; a stop request that cannot be delivered goes
    /// straight to the group-kill.
    pub(crate) fn request_graceful_quit(&mut self) {
        self.quit_requested = true;
    }

    /// Handle [`Command::Quit`]: a source that names a client leaves the
    /// session; a source that names none ends the process.
    ///
    /// A keybinding or a mouse action names the client that issued it, so quit
    /// removes that client alone, through the same detach [`Command::Detach`]
    /// runs ([`Server::handle_client_detach`]). `auto-close-session` then
    /// decides what happens to the session left behind: with the setting on and
    /// no other client attached the session ends, keeping the graceful window;
    /// with the setting off, or with another client still attached, the session
    /// and its panes keep running.
    ///
    /// A source that names no client — `kill-session` over the external CLI,
    /// the plugin host, the runtime itself — takes [`Self::request_quit`]
    /// instead.
    fn handle_quit(&mut self, command_id: CommandId, source: &CommandSource) -> CommandResult {
        let Some(client_id) = source.client_id() else {
            self.request_quit();
            return CommandResult::Ok {
                command_id,
                emitted_events: Vec::new(),
            };
        };

        let events = self.handle_client_detach(client_id);
        Self::commit_events(&mut self.event_bus, command_id, events)
    }

    /// Handle [`Command::Detach`]: remove the resolved client from the session
    /// and reconcile the tab it was viewing
    /// ([`Server::handle_client_detach`]). The session and its panes keep
    /// running; the other clients keep their records.
    ///
    /// The client is resolved through
    /// [`Server::resolve_target_client`], the same call validation made.
    fn handle_detach(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &DetachArgs,
    ) -> Result<CommandResult, Rejection> {
        let session = Self::require_session(self.acting_session(source)?)?;
        let client_id = Self::resolve_target_client(args.client, source, session)?;

        let events = self.handle_client_detach(client_id);
        Ok(Self::commit_events(&mut self.event_bus, command_id, events))
    }

    /// Handle [`Command::SwitchSession`]: move one client out of this session
    /// and into the session `args` names.
    ///
    /// The client is resolved by [`Server::resolve_target_client`], the same
    /// call validation made: a named client must be attached here, and no name
    /// means the issuing client. The caller resolved the target session, so this
    /// reads no other session and reaches no other process; it puts the move on
    /// the client's own subscriber queues and that client re-attaches from
    /// there.
    ///
    /// A move into this session is refused — a switch detaches before it
    /// attaches. A client whose queue is full is refused too: the move is
    /// dropped there and never replayed.
    ///
    /// The client leaving is an ordinary detach, so `auto-close-session` ends
    /// this session when the client that moved was the last one attached.
    fn handle_switch_session(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &SwitchSessionArgs,
    ) -> Result<CommandResult, Rejection> {
        // The plugin host grants `session_switch`; a plugin source holds none.
        // A plugin resolves no session, so validation refuses it before this.
        if matches!(source, CommandSource::Plugin { .. }) {
            return Err(Rejection::new(
                RejectReason::Unauthorized,
                "plugin lacks the session_switch capability",
            ));
        }
        let session = Self::require_session(self.acting_session(source)?)?;
        if args.session == session.id {
            return Err(Rejection::new(
                RejectReason::InvalidState,
                "this client is already in that session",
            ));
        }
        let client_id = Self::resolve_target_client(args.client, source, session)?;
        if !self.send_switch(client_id, args.session) {
            return Err(Rejection::new(
                RejectReason::InvalidState,
                "the client is too far behind to be moved right now; try again",
            ));
        }
        tracing::info!(
            command_id = %command_id,
            client = %client_id,
            session = %args.session,
            "a client was moved to another session"
        );
        Ok(CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        })
    }

    /// Handle [`Command::DetachAll`]: remove every client attached to the
    /// acting session, one [`Server::handle_client_detach`] each, and report
    /// the events they emitted together. A session with no attached client
    /// emits nothing.
    fn handle_detach_all(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
    ) -> Result<CommandResult, Rejection> {
        let session = Self::require_session(self.acting_session(source)?)?;
        let clients: Vec<ClientId> = session
            .clients
            .list_attached()
            .map(|client| client.id())
            .collect();

        let mut events = Vec::new();
        for client_id in clients {
            events.extend(self.handle_client_detach(client_id));
        }
        Ok(Self::commit_events(&mut self.event_bus, command_id, events))
    }

    /// The session's only attached client, or a rejection saying why — none
    /// are attached, or several are so the caller must name one. `none_tail`
    /// completes "no attached client …"; `ambiguous_noun` completes "… name a
    /// target client for …".
    ///
    /// This answers which client should *view* something, and is separate from
    /// [`Server::resolve_acting_client`], which answers which client a command
    /// *acts on*: a session with no attached client cannot show a new tab
    /// ([`RejectReason::InvalidState`]), while a command with no client to act
    /// on came from a source whose client is gone
    /// ([`RejectReason::SourceClientStale`]).
    ///
    /// On a session with two clients,
    /// `sole_attached_client(s, "to view the new pane's tab", "the new pane")`
    /// returns
    /// `Err(TargetAmbiguous, "multiple clients; name a target client for the new pane")`.
    fn sole_attached_client<'a>(
        session: &'a Session,
        none_tail: &str,
        ambiguous_noun: &str,
    ) -> Result<&'a Client, Rejection> {
        let mut attached = session.clients.list_attached();
        match (attached.next(), attached.next()) {
            (None, _) => Err(Rejection::new(
                RejectReason::InvalidState,
                &format!("no attached client {none_tail}"),
            )),
            (Some(only), None) => Ok(only),
            (Some(_), Some(_)) => Err(Rejection::new(
                RejectReason::TargetAmbiguous,
                &format!("multiple clients; name a target client for {ambiguous_noun}"),
            )),
        }
    }

    /// The size each of `tab_id`'s panes must be given, once every client
    /// viewing the tab has had its say. Empty when the tab is gone; `None` for a
    /// pane no viewer draws, which keeps that pane's PTY at its current size.
    ///
    /// **A pane's PTY has exactly one size, but its viewers may disagree about
    /// its rect** — zoom is per-client, so client A can have pane X filling the
    /// tab while client B has it tiled in a corner. The size handed to X's child
    /// is the **smallest** rect among the clients who actually draw X.
    ///
    /// A client zoomed on some *other* pane draws X not at all, so it is not
    /// one of the viewers this minimum is taken over. It still bounds X
    /// indirectly: `viewport` is the tab's shared [`Session::tab_viewport`]
    /// (the per-axis minimum terminal across every client viewing the tab,
    /// zoomed or not), every pane is solved inside it, and the renderer draws
    /// the whole tab at that size — so no pane, zoomed or tiled, may exceed it.
    ///
    /// When exactly one client views the tab (the common case), the minimum is
    /// that client's own rect and a zoom gives its pane the whole tab.
    ///
    /// Only the returned rect's SIZE is meaningful: its origin is whatever the
    /// first drawing viewer placed it at, and every consumer here reads the size
    /// alone ([`compute_pty_size`]).
    fn tab_content_rects(
        session: &Session,
        tab_id: TabId,
        viewport: Size,
        sizing: PaneSizing,
    ) -> Vec<(PaneId, Option<Rect>)> {
        let Some(tab) = session.tabs.get(&tab_id) else {
            return Vec::new();
        };
        let tab_rect = Rect::at_origin(viewport);

        // One solve per viewer, each in that client's own layout mode.
        let per_viewer: Vec<Vec<(PaneId, Option<Rect>)>> = session
            .clients
            .list_attached()
            .filter(|client| client.active_tab() == tab_id)
            .map(|client| {
                content_rects(&solve_with_mode_min(
                    tab.layout(),
                    client.layout_mode(tab_id),
                    tab_rect,
                    sizing,
                ))
            })
            .collect();

        // No viewer: no client draws any of these panes, so none of them is
        // resized and every PTY keeps the size it has.
        let Some(first) = per_viewer.first() else {
            return Vec::new();
        };

        // Merge by pane id: a pane's smallest rect across the viewers that draw
        // it. The merge keys on the id alone, so it holds however each solve
        // orders its panes.
        let mut smallest: HashMap<PaneId, Option<Rect>> = HashMap::with_capacity(first.len());
        for viewer in &per_viewer {
            for &(pane_id, content) in viewer {
                let entry = smallest.entry(pane_id).or_insert(None);
                let Some(rect) = content else {
                    // This viewer draws no content for the pane, and asks
                    // nothing of its size.
                    continue;
                };
                *entry = match *entry {
                    Some(current) => {
                        Some(Rect::new(current.origin, current.size.min_axes(rect.size)))
                    }
                    None => Some(rect),
                };
            }
        }

        // Emit in the first viewer's solve order.
        first
            .iter()
            .map(|&(pane_id, _)| (pane_id, smallest.get(&pane_id).copied().flatten()))
            .collect()
    }

    /// The target session borrowed mutably, plus the viewport `tab_id` is
    /// currently solved against. Rejects when the session is gone or when no
    /// attached client views the tab — an unviewed tab has no terminal size to
    /// solve against.
    fn session_and_viewport(
        &mut self,
        session_id: SessionId,
        tab_id: TabId,
    ) -> Result<(&mut Session, Size), Rejection> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;
        let viewport = session.tab_viewport(tab_id).ok_or_else(|| {
            Rejection::new(
                RejectReason::InvalidState,
                "pane's tab is not viewed by any client",
            )
        })?;
        Ok((session, viewport))
    }

    /// Reflow `tab_id`'s live PTYs to its current effective size when a client
    /// still views it, appending one [`Event::PtyResized`] per pane actually
    /// resized. A tab no viewer contributes a pane area to has no
    /// [`Session::tab_viewport`] and keeps its sizes. The shared shape behind
    /// every "a tab's viewer set changed" reflow — the full-tab solve with no
    /// freshly-spawned pane to skip.
    pub(crate) fn reflow_tab_if_viewed(
        &mut self,
        backend: &dyn PtyBackend,
        session_id: SessionId,
        tab_id: TabId,
        events: &mut Vec<Event>,
    ) {
        let Some(session) = self.sessions.get(&session_id) else {
            return;
        };
        let Some(viewport) = session.tab_viewport(tab_id) else {
            return;
        };
        let rects = Self::tab_content_rects(session, tab_id, viewport, self.pane_sizing());
        self.reflow_changed(backend, rects, None, events);
    }

    /// Resize the live PTYs in `rects` whose size actually changed, routing the
    /// batch through the shared [`resize_for_layout_change`] executor and pushing
    /// one [`Event::PtyResized`] per pane it resized.
    ///
    /// A pane is passed to the executor only when it has a content rect, has a
    /// live handle, is not `skip` (the freshly-spawned pane is sized
    /// separately), and its new [`compute_pty_size`] differs from `pty_sizes`.
    /// A pane with no content rect, and a pane whose size is unchanged, is left
    /// alone. The executor is stateless; this owns the last-set-size cache
    /// and the terminal-engine map, and for every pane it resizes it updates
    /// the cache and resizes that pane's engine grid to the same size.
    fn reflow_changed(
        &mut self,
        backend: &dyn PtyBackend,
        rects: Vec<(PaneId, Option<Rect>)>,
        skip: Option<PaneId>,
        events: &mut Vec<Event>,
    ) {
        let items: Vec<(PaneId, Option<Rect>)> = rects
            .into_iter()
            .filter(|&(pane_id, content)| {
                let Some(rect) = content else {
                    return false;
                };
                Some(pane_id) != skip
                    && self.pty_handles.contains_key(&pane_id)
                    && self.pty_sizes.get(&pane_id) != Some(&compute_pty_size(rect))
            })
            .collect();
        for result in resize_for_layout_change(backend, items) {
            if let Some(size) = result.applied {
                self.pty_sizes.insert(result.pane_id, size);
                if let Some(engine) = self.terminal_engines.get_mut(&result.pane_id) {
                    engine.resize(size);
                }
                events.push(Event::PtyResized(PtyResized {
                    pane_id: result.pane_id,
                    size,
                }));
            }
        }
    }
}

/// End `pane_id`'s child under `kill_policy` on a thread of its own.
///
/// A graceful kill sleeps out its grace window, so the dispatcher keeps
/// draining while the kill runs. The kill also purges the backend's own entry
/// for the pane, even when the child already exited.
///
/// A thread the operating system will not start — the process is at its thread
/// limit — runs the kill on this thread instead, which blocks the dispatcher
/// for the grace window rather than ending the process.
pub(super) fn kill_off_thread(
    backend: &Arc<dyn PtyBackend>,
    pane_id: PaneId,
    kill_policy: KillPolicy,
) {
    let off_thread = Arc::clone(backend);
    let started = thread::Builder::new()
        .spawn(move || {
            let _ = off_thread.kill(pane_id, kill_policy);
        })
        .is_ok();
    if !started {
        let _ = backend.kill(pane_id, kill_policy);
    }
}

/// Whether an OSC 7 report's host names this machine: no authority (`None`,
/// which `file:///path` gives), `localhost` in any case, any loopback IP
/// address, or the machine's own hostname in any case. Every other host is
/// `false`.
///
/// A loopback address counts however it is written: `127.0.0.1`, any other
/// address of `127.0.0.0/8`, `::1`, and `0:0:0:0:0:0:0:1`, each bare or
/// bracketed as the URI form writes it (`file://[::1]/…`).
fn is_local_host(host: Option<&str>) -> bool {
    let Some(host) = host else {
        return true;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let bare = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    if bare
        .parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
    {
        return true;
    }
    koshi_pty::cwd::local_hostname().is_some_and(|name| name.eq_ignore_ascii_case(host))
}

mod client;
mod pane;
mod resolve;
mod tab;
mod visual;

#[cfg(test)]
mod tests;
