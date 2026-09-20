//! Command dispatch: the single entrypoint every requested mutation passes
//! through.
//!
//! `Server::dispatch` — reached from outside the crate through
//! [`Server::submit_command`] — validates one [`CommandEnvelope`] against live
//! state, then routes it via an exhaustive `match` on [`Command`] — one arm per
//! variant. Validation runs first: a command whose command source may not issue it, or
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
    bus::EventBus, spawn_env::build_koshi_environment, transaction::TransactionScope,
};
use crate::server::Server;
use koshi_core::{
    command::{
        ClearSelectionArgs, ClosePaneArgs, CloseTabArgs, Command, CommandEnvelope, CommandResult,
        CommandSource, CopyArgs, DetachArgs, FocusPaneArgs, FocusTabArgs, FocusTarget,
        LockModeArgs, MovePaneArgs, MoveTabArgs, NewPaneArgs, NewTabArgs, ResizePaneArgs,
        RunCommandPaneArgs, ScrollPaneArgs, Selection, SelectionKind, SetSelectionArgs,
        SwapPanesArgs, SwitchSessionArgs, TabTarget, ToggleLockModeArgs, VisualCommand,
        WriteToPaneArgs,
    },
    event::{
        Event, InputModeChanged, LayoutChanged, MouseSelectChanged, PaneFocused, PaneProcessExited,
        PtyResized, RejectReason, SelectionChanged,
    },
    geometry::{Direction, PaneArea, Rect, Size},
    ids::{ClientId, CommandId, PaneId, SessionId, TabId},
    lock::LockMode,
    naming::{generate_name, NameKind},
    process::{ExitStatus, KillPolicy, PtySize, SpawnSpec},
};
use koshi_layout::{
    content::list_content_rects,
    edit::{add_pane_to_stack, split_leaf},
    focus::activate_stack_member,
    mode::LayoutMode,
    neighbor::select_directional_neighbor,
    placement::{place_pane_within_tab, PlacementError, PlacementTarget},
    resize::{resize_layout_with_sizing, ResizeError},
    solver::{is_layout_within_rect, solve_layout_with_mode, solve_layout_with_sizing, PaneSizing},
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
pub(crate) fn compute_root_pane_pty_size(
    pane_id: PaneId,
    viewport_size: Size,
    pane_sizing: PaneSizing,
) -> PtySize {
    let root_layout_tree = LayoutNode::Pane(pane_id);
    let tab_layout_rect = Rect::from_size_at_origin(viewport_size);
    let pane_content_rects = list_content_rects(&solve_layout_with_sizing(
        &root_layout_tree,
        tab_layout_rect,
        pane_sizing,
    ));
    let pane_content_rect = pane_content_rects
        .iter()
        .find(|(matched_pane_id, _)| *matched_pane_id == pane_id)
        .and_then(|(_, content_rect)| *content_rect)
        .unwrap_or(tab_layout_rect);
    compute_pty_size(pane_content_rect)
}

/// The PTY size for every pane in `layout` filling `viewport`: solve the whole
/// tree once, then clamp each pane's content rect to a PTY size, in layout
/// order. A multi-pane tab's panes each spawn at their tiled slice this way,
/// not the whole tab. A pane the solve suppressed for lack of space has no
/// content rect and falls back to the full tab rect — the same floor
/// [`compute_root_pane_pty_size`] uses.
pub(crate) fn compute_pane_spawn_sizes(
    layout_tree: &LayoutNode,
    viewport_size: Size,
    pane_sizing: PaneSizing,
) -> Vec<(PaneId, PtySize)> {
    let tab_layout_rect = Rect::from_size_at_origin(viewport_size);
    list_content_rects(&solve_layout_with_sizing(
        layout_tree,
        tab_layout_rect,
        pane_sizing,
    ))
    .into_iter()
    .map(|(pane_id, content_rect)| {
        (
            pane_id,
            compute_pty_size(content_rect.unwrap_or(tab_layout_rect)),
        )
    })
    .collect()
}

/// The tab named by the first [`Event::TabFocused`] in `emitted_events`, or
/// `None` when `emitted_events` holds none.
fn find_first_focused_tab_id(emitted_events: &[Event]) -> Option<TabId> {
    emitted_events.iter().find_map(|event| match event {
        Event::TabFocused(focused_tab) => Some(focused_tab.tab_id),
        _ => None,
    })
}

/// A validation failure: the reason a command was rejected, plus an optional
/// human-facing hint. The `Err` half of [`Server::validate_command`].
struct Rejection {
    reason: RejectReason,
    help: Option<String>,
    /// Cells the donating pane can still give, for a resize refused at a pane
    /// minimum; `None` for every other rejection. A rejection carrying this is
    /// the one [`Server::rejected`] leaves unlogged.
    spare_cell_count: Option<u16>,
}

impl Rejection {
    /// A rejection with the given reason and a hint string.
    fn from_reason_and_help(reason: RejectReason, help: &str) -> Self {
        Rejection {
            reason,
            help: Some(help.to_string()),
            spare_cell_count: None,
        }
    }

    /// A rejection with the given reason and no hint.
    fn from_reason(reason: RejectReason) -> Self {
        Rejection {
            reason,
            help: None,
            spare_cell_count: None,
        }
    }

    /// A resize refused at a pane minimum, carrying the `spare_cell_count` cells the
    /// donating pane can still give in both the hint and the field.
    fn from_min_size(spare_cell_count: u16) -> Self {
        Rejection {
            reason: RejectReason::MinSize,
            help: Some(format!(
                "the donating pane has only {spare_cell_count} spare cells to give"
            )),
            spare_cell_count: Some(spare_cell_count),
        }
    }
}

/// The resolved concrete target of a [`Command::NewPane`]: the session and tab
/// the new pane joins, the command source pane it splits from, and the client to
/// auto-focus it for (when one applies). All fields are `Copy`, so resolving
/// holds no borrow into the session map.
struct NewPaneTarget {
    session_id: SessionId,
    source_pane_id: PaneId,
    tab_id: TabId,
    focus_client_id: Option<ClientId>,
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

/// The resolved concrete target of a command that changes one client's own
/// view of a pane ([`Command::FocusPane`], [`Command::TogglePaneFullscreen`]):
/// the owning session, the client whose view changes, that client's active
/// tab, and the pane. The `Ok` half of both
/// [`Server::resolve_focus_target`] and
/// [`Server::resolve_fullscreen_target`]. All fields are `Copy`, so resolving
/// holds no borrow into the session map.
struct ClientPaneTarget {
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
    /// validated first (target resolution, command source policy); a command that
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
        let command_id = envelope.command_id;
        if let Err(rejection) = self.validate_command(&envelope) {
            return (Self::rejected(command_id, rejection), None);
        }
        let outcome = match envelope.command {
            Command::NewPane(command_args) => self.handle_new_pane(
                command_id,
                &envelope.command_source,
                &command_args,
                envelope.issued_at,
            ),
            Command::ClosePane(command_args) => {
                self.handle_close_pane(command_id, &envelope.command_source, &command_args)
            }
            Command::ResizePane(command_args) => {
                self.handle_resize_pane(command_id, &envelope.command_source, &command_args)
            }
            Command::MovePane(command_args) => {
                self.handle_move_pane(command_id, &envelope.command_source, &command_args)
            }
            Command::SwapPanes(command_args) => {
                self.handle_swap_panes(command_id, &envelope.command_source, &command_args)
            }
            Command::ScrollPane(command_args) => {
                self.handle_scroll_pane(command_id, &envelope.command_source, &command_args)
            }
            Command::FocusPane(command_args) => {
                self.handle_focus_pane(command_id, &envelope.command_source, &command_args)
            }
            Command::NewTab(command_args) => self.handle_new_tab(
                command_id,
                &envelope.command_source,
                &command_args,
                envelope.issued_at,
            ),
            Command::CloseTab(command_args) => {
                self.handle_close_tab(command_id, &envelope.command_source, &command_args)
            }
            Command::FocusTab(command_args) => {
                self.handle_focus_tab(command_id, &envelope.command_source, &command_args)
            }
            Command::WriteToPane(command_args) => {
                self.handle_write_to_pane(command_id, &envelope.command_source, &command_args)
            }
            Command::ToggleLockMode(command_args) => {
                self.handle_toggle_lock_mode(command_id, &envelope.command_source, &command_args)
            }
            Command::SetLockMode(command_args) => {
                self.handle_set_lock_mode(command_id, &envelope.command_source, &command_args)
            }
            Command::ToggleMouseSelect => {
                self.handle_toggle_mouse_select(command_id, &envelope.command_source)
            }
            Command::RunCommandPane(command_args) => {
                let new_pane_args = Self::run_command_new_pane_args(&command_args);
                self.handle_new_pane(
                    command_id,
                    &envelope.command_source,
                    &new_pane_args,
                    envelope.issued_at,
                )
            }
            Command::Visual(command) => {
                self.handle_visual(command_id, &envelope.command_source, &command)
            }
            Command::Plugin(_) => Ok(self.build_rejected_command_result(command_id, "plugin")),
            Command::Quit => Ok(self.handle_quit(command_id, &envelope.command_source)),
            Command::Detach(command_args) => {
                self.handle_detach(command_id, &envelope.command_source, &command_args)
            }
            Command::DetachAll => self.handle_detach_all(command_id, &envelope.command_source),
            Command::TogglePaneFullscreen => {
                self.handle_toggle_pane_fullscreen(command_id, &envelope.command_source)
            }
            Command::MoveTab(command_args) => {
                self.handle_move_tab(command_id, &envelope.command_source, &command_args)
            }
            Command::SwitchSession(command_args) => {
                self.handle_switch_session(command_id, &envelope.command_source, &command_args)
            }
        };
        self.render_scheduler.invalidate();
        match outcome {
            Ok(command_result) => (command_result, None),
            Err(rejection) => {
                let spare_cell_count = rejection.spare_cell_count;
                (Self::rejected(command_id, rejection), spare_cell_count)
            }
        }
    }

    /// Build a rejection for a command with no handler, keyed back to its
    /// originating envelope by `command_id`, and log it at `warn`. `label` names
    /// the command in the log line and in the hint, which reads
    /// `"<label> not yet implemented"`.
    fn build_rejected_command_result(&self, command_id: CommandId, label: &str) -> CommandResult {
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
    /// that made it, so a command source whose client is gone has nothing to act on
    /// and gets [`RejectReason::SourceClientStale`] rather than the
    /// sole-attached-client stand-in [`Server::resolve_acting_client`] applies.
    ///
    /// This is the check itself, not an assertion about an earlier one:
    /// [`Self::resolve_target`] calls it for the selection commands, and the
    /// handlers call it again to get the id.
    fn resolve_issuing_client_id(command_source: &CommandSource) -> Result<ClientId, Rejection> {
        command_source
            .get_client_id()
            .ok_or_else(|| Rejection::from_reason(RejectReason::SourceClientStale))
    }

    /// Confirm `pane_id` still exists in the session `client_id` is attached to.
    fn validate_pane_exists(&self, client_id: ClientId, pane_id: PaneId) -> Result<(), Rejection> {
        let is_pane_present = self
            .get_session_for_client(client_id)
            .is_some_and(|session| session.panes.get_pane_record_by_id(pane_id).is_some());
        if is_pane_present {
            Ok(())
        } else {
            Err(Rejection::from_reason(RejectReason::TargetGone))
        }
    }

    /// Turn a [`Rejection`] into a [`CommandResult::Rejected`] keyed to
    /// `command_id`.
    ///
    /// Every rejection a handler or validation produces is built here, and
    /// logged here at `warn`: the command did not apply, state is untouched,
    /// and the session carries on. A border move refused at a pane minimum —
    /// the rejection carrying `spare_cell_count` — is not logged.
    fn rejected(command_id: CommandId, rejection: Rejection) -> CommandResult {
        if rejection.spare_cell_count.is_none() {
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
    /// delivering the batch to every subscriber on `event_bus`.
    fn commit_events(
        event_bus: &mut EventBus,
        command_id: CommandId,
        events: Vec<Event>,
    ) -> CommandResult {
        let mut scope = TransactionScope::new();
        for event in events {
            scope.emit(event);
        }
        scope.commit(command_id, event_bus)
    }

    /// Launch a pane's child process, mapping a backend failure onto the shared
    /// "failed to launch" rejection. Every launch-then-commit path calls this
    /// before mutating any session state.
    fn spawn_child(
        backend: &dyn PtyBackend,
        pane_id: PaneId,
        spawn_spec: SpawnSpec,
        pty_size: PtySize,
    ) -> Result<PtyHandle, Rejection> {
        backend
            .spawn_pane(pane_id, spawn_spec, pty_size)
            .map_err(|_| {
                Rejection::from_reason_and_help(
                    RejectReason::InvalidState,
                    "failed to launch the pane's process",
                )
            })
    }

    /// Add koshi's configured terminal identity — `TERM` and `COLORTERM` from
    /// the `terminal` config section — to a spawned child's environment overlay,
    /// filling each only when the pane's own environment variables have not
    /// already set it: an explicit per-pane value is kept.
    pub(crate) fn apply_terminal_identity_environment_variables(
        &self,
        mut environment_variables: BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        environment_variables
            .entry("TERM".to_string())
            .or_insert_with(|| self.config.terminal.term.clone());
        environment_variables
            .entry("COLORTERM".to_string())
            .or_insert_with(|| self.config.terminal.colorterm.clone());
        environment_variables
    }

    /// The spawn spec for a default-shell pane: the configured
    /// `terminal.default_shell` when set, otherwise the platform default from
    /// `$SHELL` / `%COMSPEC%`. Either way it carries koshi's terminal identity
    /// in its environment.
    pub(crate) fn build_default_shell_spec(
        &self,
        working_directory: Option<PathBuf>,
        environment_variables: BTreeMap<String, String>,
    ) -> SpawnSpec {
        let environment_variables =
            self.apply_terminal_identity_environment_variables(environment_variables);
        match &self.config.terminal.default_shell {
            Some(program) => SpawnSpec::from_shell_program(
                PathBuf::from(program),
                working_directory,
                environment_variables,
            ),
            None => SpawnSpec::default_shell(working_directory, environment_variables),
        }
    }

    /// Map [`Command::RunCommandPane`] onto the [`NewPaneArgs`] that realize it:
    /// its command is required (never the default shell), and its command source
    /// pane, placement — split direction or stacking — and working directory
    /// carry through to the new-pane transaction. [`Self::dispatch`] and
    /// [`Self::resolve_target`] both call it, so the validation pre-check and
    /// the handler read the same anchor pane.
    fn run_command_new_pane_args(command_args: &RunCommandPaneArgs) -> NewPaneArgs {
        NewPaneArgs {
            source_pane_id: command_args.source_pane_id,
            tab_id: command_args.tab_id,
            direction: command_args.direction,
            should_stack: command_args.should_stack,
            working_directory: command_args.working_directory.clone(),
            spawn_spec: Some(command_args.spawn_spec.clone()),
            client_id: command_args.client_id,
        }
    }

    /// The live working directory of `pane`, best answer first: the shell's
    /// own OSC 7 report (when it names this machine), then the OS's answer
    /// for the child process, then the directory the pane was spawned in.
    /// `None` when nothing knows — a spawn using this then inherits koshi's
    /// own directory. Every answer is already at hand or one non-blocking
    /// OS call.
    pub(super) fn resolve_pane_working_directory(
        &self,
        session_id: SessionId,
        pane_id: PaneId,
    ) -> Option<PathBuf> {
        if let Some(reported_working_directory) = self
            .terminal_engine_by_pane_id
            .get(&pane_id)
            .and_then(|engine| engine.get_terminal_state().get_current_working_directory())
        {
            if is_local_host(reported_working_directory.get_host()) {
                return Some(
                    reported_working_directory
                        .get_working_directory_path()
                        .to_path_buf(),
                );
            }
        }
        if let Some(working_directory) = self.get_pty_backend().find_live_working_directory(pane_id)
        {
            return Some(working_directory);
        }
        self.session_by_id
            .get(&session_id)?
            .panes
            .get_pane_record_by_id(pane_id)?
            .working_directory
            .clone()
    }

    /// Mark the process for immediate teardown: the event loop polls the quit
    /// request before it waits for an event and after each event batch, exits
    /// once [`awaits_a_client`](Server::awaits_a_client) is false, and teardown
    /// group-kills every pane's child without the graceful window.
    pub(crate) fn request_quit(&mut self) {
        self.request_graceful_quit();
        self.should_shutdown_immediately = true;
    }

    /// Mark the process for teardown, keeping the graceful window: the event
    /// loop exits as above, and teardown asks each pane's process group to stop
    /// and waits up to [`GRACEFUL_TIMEOUT_DURATION`](koshi_core::constant::GRACEFUL_TIMEOUT_DURATION)
    /// before group-killing it; a stop request that cannot be delivered goes
    /// straight to the group-kill.
    pub(crate) fn request_graceful_quit(&mut self) {
        self.is_quit_requested = true;
    }

    /// Handle [`Command::Quit`]: a command source that names a client leaves the
    /// session; a command source that names none ends the process.
    ///
    /// A keybinding or a mouse action names the client that issued it, so quit
    /// removes that client alone, through the same detach [`Command::Detach`]
    /// runs ([`Server::handle_client_detach`]). `auto-close-session` then
    /// decides what happens to the session left behind: with the setting on and
    /// no other client attached the session ends, keeping the graceful window;
    /// with the setting off, or with another client still attached, the session
    /// and its panes keep running.
    ///
    /// A command source that names no client — `kill-session` over the external CLI,
    /// the plugin host, the runtime itself — takes [`Self::request_quit`]
    /// instead.
    fn handle_quit(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
    ) -> CommandResult {
        let Some(client_id) = command_source.get_client_id() else {
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
        command_source: &CommandSource,
        command_args: &DetachArgs,
    ) -> Result<CommandResult, Rejection> {
        let session = Self::require_session(self.resolve_acting_session(command_source)?)?;
        let client_id =
            Self::resolve_target_client(command_args.client_id, command_source, session)?;

        let events = self.handle_client_detach(client_id);
        Ok(Self::commit_events(&mut self.event_bus, command_id, events))
    }

    /// Handle [`Command::SwitchSession`]: move one client out of this session
    /// and into the session `command_args` names.
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
        command_source: &CommandSource,
        command_args: &SwitchSessionArgs,
    ) -> Result<CommandResult, Rejection> {
        // The plugin host grants `session_switch`; a plugin command source holds none.
        // A plugin resolves no session, so validation refuses it before this.
        if matches!(command_source, CommandSource::Plugin { .. }) {
            return Err(Rejection::from_reason_and_help(
                RejectReason::Unauthorized,
                "plugin lacks the session_switch capability",
            ));
        }
        let session = Self::require_session(self.resolve_acting_session(command_source)?)?;
        if command_args.session_id == session.session_id {
            return Err(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                "this client is already in that session",
            ));
        }
        let client_id =
            Self::resolve_target_client(command_args.client_id, command_source, session)?;
        if !self.send_switch(client_id, command_args.session_id) {
            return Err(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                "the client is too far behind to be moved right now; try again",
            ));
        }
        tracing::info!(
            command_id = %command_id,
            client = %client_id,
            session = %command_args.session_id,
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
        command_source: &CommandSource,
    ) -> Result<CommandResult, Rejection> {
        let session = Self::require_session(self.resolve_acting_session(command_source)?)?;
        let clients: Vec<ClientId> = session
            .clients
            .list_attached_clients()
            .map(|client| client.get_client_id())
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
    /// on came from a command source whose client is gone
    /// ([`RejectReason::SourceClientStale`]).
    ///
    /// On a session with two clients,
    /// `resolve_sole_attached_client(s, "to view the new pane's tab", "the new pane")`
    /// returns
    /// `Err(TargetAmbiguous, "multiple clients; name a target client for the new pane")`.
    fn resolve_sole_attached_client<'a>(
        session: &'a Session,
        none_tail: &str,
        ambiguous_noun: &str,
    ) -> Result<&'a Client, Rejection> {
        let mut attached_clients = session.clients.list_attached_clients();
        match (attached_clients.next(), attached_clients.next()) {
            (None, _) => Err(Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                &format!("no attached client {none_tail}"),
            )),
            (Some(only), None) => Ok(only),
            (Some(_), Some(_)) => Err(Rejection::from_reason_and_help(
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
        pane_sizing: PaneSizing,
    ) -> Vec<(PaneId, Option<Rect>)> {
        let Some(tab) = session.tabs.get(&tab_id) else {
            return Vec::new();
        };
        let tab_rect = Rect::from_size_at_origin(viewport);

        // One solve per viewer, each in that client's own layout mode.
        let per_viewer_content_rects: Vec<Vec<(PaneId, Option<Rect>)>> = session
            .clients
            .list_attached_clients()
            .filter(|client| client.get_active_tab() == tab_id)
            .map(|client| {
                list_content_rects(&solve_layout_with_mode(
                    tab.get_layout_tree(),
                    client.get_layout_mode(tab_id),
                    tab_rect,
                    pane_sizing,
                ))
            })
            .collect();

        // No viewer: no client draws any of these panes, so none of them is
        // resized and every PTY keeps the size it has.
        let Some(first_viewer_layouts) = per_viewer_content_rects.first() else {
            return Vec::new();
        };

        // Merge by pane id: a pane's smallest rect across the viewers that draw
        // it. The merge keys on the id alone, so it holds however each solve
        // orders its panes.
        let mut smallest_content_rect_by_pane_id: HashMap<PaneId, Option<Rect>> =
            HashMap::with_capacity(first_viewer_layouts.len());
        for viewer_content_rects in &per_viewer_content_rects {
            for &(pane_id, content_rect) in viewer_content_rects {
                let smallest_content_rect = smallest_content_rect_by_pane_id
                    .entry(pane_id)
                    .or_insert(None);
                let Some(rect) = content_rect else {
                    // This viewer draws no content for the pane, and asks
                    // nothing of its size.
                    continue;
                };
                *smallest_content_rect = match *smallest_content_rect {
                    Some(current_rect) => Some(Rect::from_origin_and_size(
                        current_rect.origin,
                        current_rect.cell_size.compute_minimum_axes(rect.cell_size),
                    )),
                    None => Some(rect),
                };
            }
        }

        // Emit in the first viewer's solve order.
        first_viewer_layouts
            .iter()
            .map(|&(pane_id, _)| {
                (
                    pane_id,
                    smallest_content_rect_by_pane_id
                        .get(&pane_id)
                        .copied()
                        .flatten(),
                )
            })
            .collect()
    }

    /// The target session borrowed mutably, plus the viewport `tab_id` is
    /// currently solved against. Rejects when the session is gone or when no
    /// attached client views the tab — an unviewed tab has no terminal size to
    /// solve against.
    fn resolve_session_and_viewport(
        &mut self,
        session_id: SessionId,
        tab_id: TabId,
    ) -> Result<(&mut Session, Size), Rejection> {
        let session = self
            .session_by_id
            .get_mut(&session_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        let viewport = session.get_tab_viewport(tab_id).ok_or_else(|| {
            Rejection::from_reason_and_help(
                RejectReason::InvalidState,
                "pane's tab is not viewed by any client",
            )
        })?;
        Ok((session, viewport))
    }

    /// Reflow `tab_id`'s live PTYs to its current effective size when a client
    /// still views it, appending one [`Event::PtyResized`] per pane actually
    /// resized. A tab no viewer contributes a pane area to has no
    /// [`Session::tab_viewport`] and keeps its sizes. The one spelling of a
    /// full-tab reflow: every caller that changed what the tab's viewers
    /// display — a moved border, a moved focus, a flipped zoom, a viewer
    /// joining or leaving — reaches it here, with no freshly-spawned pane to
    /// skip.
    pub(crate) fn reflow_tab_if_viewed(
        &mut self,
        backend: &dyn PtyBackend,
        session_id: SessionId,
        tab_id: TabId,
        events: &mut Vec<Event>,
    ) {
        let Some(session) = self.session_by_id.get(&session_id) else {
            return;
        };
        if let Some(cell_size) = session.get_tab_cell_size(tab_id) {
            if let Some(tab) = session.tabs.get(&tab_id) {
                for pane_id in tab.get_layout_tree().list_leaf_pane_ids() {
                    if let Some(engine) = self.terminal_engine_by_pane_id.get_mut(&pane_id) {
                        engine.set_cell_size(cell_size);
                    }
                }
            }
        }
        let Some(viewport) = session.get_tab_viewport(tab_id) else {
            return;
        };
        let rects = Self::tab_content_rects(session, tab_id, viewport, self.get_pane_sizing());
        self.reflow_changed(backend, rects, None, events);
    }

    /// Resize the live PTYs in `rects` whose size actually changed, routing the
    /// batch through the shared [`resize_for_layout_change`] executor and pushing
    /// one [`Event::PtyResized`] per pane it resized.
    ///
    /// A pane is passed to the executor only when it has a content rect, has a
    /// live handle, is not `excluded_pane_id` (the freshly-spawned pane is sized
    /// separately), and its new [`compute_pty_size`] differs from `pty_sizes`.
    /// A pane with no content rect, and a pane whose size is unchanged, is left
    /// alone. The executor is stateless; this owns the last-set-size cache
    /// and the terminal-engine map, and for every pane it resizes it updates
    /// the cache and resizes that pane's engine grid to the same size.
    fn reflow_changed(
        &mut self,
        backend: &dyn PtyBackend,
        content_rects: Vec<(PaneId, Option<Rect>)>,
        excluded_pane_id: Option<PaneId>,
        emitted_events: &mut Vec<Event>,
    ) {
        let resize_candidates: Vec<(PaneId, Option<Rect>)> = content_rects
            .into_iter()
            .filter(|&(pane_id, content_rect)| {
                let Some(content_rect) = content_rect else {
                    return false;
                };
                Some(pane_id) != excluded_pane_id
                    && self.pty_handle_by_pane_id.contains_key(&pane_id)
                    && self.pty_size_by_pane_id.get(&pane_id)
                        != Some(&compute_pty_size(content_rect))
            })
            .collect();
        for resize_outcome in resize_for_layout_change(backend, resize_candidates) {
            if let Some(pty_size) = resize_outcome.applied_pty_size {
                self.pty_size_by_pane_id
                    .insert(resize_outcome.pane_id, pty_size);
                if let Some(engine) = self
                    .terminal_engine_by_pane_id
                    .get_mut(&resize_outcome.pane_id)
                {
                    engine.resize_terminal_state(pty_size);
                }
                emitted_events.push(Event::PtyResized(PtyResized {
                    pane_id: resize_outcome.pane_id,
                    pty_size,
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
    let is_thread_started = thread::Builder::new()
        .spawn(move || {
            let _ = off_thread.kill_pane(pane_id, kill_policy);
        })
        .is_ok();
    if !is_thread_started {
        let _ = backend.kill_pane(pane_id, kill_policy);
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
    let bare_host = host
        .strip_prefix('[')
        .and_then(|bracketed_host| bracketed_host.strip_suffix(']'))
        .unwrap_or(host);
    if bare_host
        .parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
    {
        return true;
    }
    koshi_pty::working_directory::get_local_hostname()
        .is_some_and(|local_hostname| local_hostname.eq_ignore_ascii_case(host))
}

mod client;
mod pane;
mod resolve;
mod tab;
mod visual;

#[cfg(test)]
mod tests;
