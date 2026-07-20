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
//!
//! This file holds the dispatch table, target resolution types, and the
//! helpers every handler shares. The handlers themselves live in submodules
//! by what they act on: `pane`, `tab`, `client`, `visual`, with target
//! resolution in `resolve`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::SystemTime;

use crate::runtime::{
    render_schedule::InvalidationReason, snapshot::solve_tab, state::Runtime,
    transaction::TransactionScope,
};
use koshi_core::{
    command::{
        ClearSelectionArgs, ClosePaneArgs, CloseTabArgs, Command, CommandEnvelope, CommandResult,
        CommandSource, FocusPaneArgs, FocusTabArgs, FocusTarget, LockModeArgs, MoveTabArgs,
        NewPaneArgs, NewTabArgs, RenamePaneArgs, RenameSessionArgs, RenameTabArgs, ResizePaneArgs,
        RunCommandPaneArgs, SetSelectionArgs, TabTarget, VisualCommand, WriteToPaneArgs,
    },
    event::{
        Event, InputMode, InputModeChanged, LayoutChanged, PaneFocused, PtyResized, RejectReason,
        SelectionChanged,
    },
    geometry::{Direction, Point, Rect, Size},
    ids::{ClientId, CommandId, PaneId, SessionId, TabId},
    lock::LockMode,
    naming::{generate_name, NameKind},
    process::{ExitStatus, KillPolicy, PtySize, ShellKind, SpawnSpec},
};
use koshi_layout::{
    content::content_rects,
    edit::{add_to_stack, split_leaf},
    focus::stack_activate,
    mode::LayoutMode,
    resize::{resize_with_min, ResizeError},
    solver::{fits, solve_with_min, solve_with_mode_min},
    tree::LayoutNode,
};
use koshi_pane::pane::{
    lifecycle::PaneLifecycle,
    policy::PaneClosePolicy,
    state::{PaneKind, PaneRecord},
};
use koshi_pty::backend::state::{PtyBackend, PtyHandle};
use koshi_pty::resize::{compute_pty_size, resize_for_layout_change};
use koshi_session::client::{pane_viewport, Client};
use koshi_session::session::{
    cascade::{on_child_exit, remove_pane_cascade},
    lifecycle::SessionLifecycle,
    pane_ops::{self, NewPaneSpec},
    policy::EmptyTabPolicy,
    session_ops,
    state::Session,
    tab_ops,
};

/// The PTY size for a tab's sole root pane filling `viewport`: solve the
/// single-pane layout, take the root's content rect, and clamp it to a PTY size.
/// Shared by the new-tab path and genesis so both size the root pane identically.
///
/// Callers that gate on minimum size (the new-tab command) check
/// [`fits`] first; genesis has no gate, and the
/// solver always places a single leaf, so the `unwrap_or` fallback is a floor,
/// not a real path.
pub(crate) fn size_root_pane(pane_id: PaneId, viewport: Size, min: Size) -> PtySize {
    let candidate = LayoutNode::Pane(pane_id);
    let tab_rect = Rect::new(Point { x: 0, y: 0 }, viewport);
    let rects = content_rects(&solve_with_min(&candidate, tab_rect, min));
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
/// [`size_root_pane`] uses — so its child still starts at a usable size.
pub(crate) fn pane_spawn_sizes(
    layout: &LayoutNode,
    viewport: Size,
    min: Size,
) -> Vec<(PaneId, PtySize)> {
    let tab_rect = Rect::new(Point { x: 0, y: 0 }, viewport);
    content_rects(&solve_with_min(layout, tab_rect, min))
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
    /// [`RejectReason::InvalidState`]. A command that reaches its handler
    /// schedules a repaint, so a mutation shows regardless of which entry
    /// point — key binding, IPC, or plugin — delivered it.
    pub fn dispatch(&mut self, envelope: CommandEnvelope) -> CommandResult {
        let command_id = envelope.id;
        if let Err(rejection) = self.validate(&envelope) {
            return Self::rejected(command_id, rejection);
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
            Command::RenameTab(args) => self.handle_rename_tab(command_id, &envelope.source, &args),
            Command::FocusTab(args) => self.handle_focus_tab(command_id, &envelope.source, &args),
            Command::WriteToPane(args) => {
                self.handle_write_to_pane(command_id, &envelope.source, &args)
            }
            Command::ToggleLockMode => self.handle_toggle_lock_mode(command_id, &envelope.source),
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
            Command::Quit => Ok(self.handle_quit(command_id)),
            Command::TogglePaneFullscreen => {
                self.handle_toggle_pane_fullscreen(command_id, &envelope.source)
            }
            Command::RenamePane(args) => {
                self.handle_rename_pane(command_id, &envelope.source, &args)
            }
            Command::MoveTab(args) => self.handle_move_tab(command_id, &envelope.source, &args),
            Command::RenameSession(args) => {
                self.handle_rename_session(command_id, &envelope.source, &args)
            }
        };
        self.render_scheduler
            .invalidate(InvalidationReason::StatusChanged);
        outcome.unwrap_or_else(|rejection| Self::rejected(command_id, rejection))
    }

    /// Build a rejection for a command with no handler wired yet, keyed back to
    /// its originating envelope by `command_id`, and log it. `label` names the
    /// command in both the human-facing hint and the log line.
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
    /// own state and have no other target.
    ///
    /// [`Self::validate`] rejects such a command before any handler runs when
    /// its source names no client, so reaching this with a clientless source
    /// would mean the command escaped that gate.
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
    /// Every rejection a handler or validation produces is built here, so this
    /// is where one is logged. It is a warning: the command simply did not
    /// apply, state is untouched, and the session carries on.
    fn rejected(command_id: CommandId, rejection: Rejection) -> CommandResult {
        tracing::warn!(
            command_id = %command_id,
            reason = %rejection.reason,
            help = rejection.help.as_deref(),
            "command rejected"
        );
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
    /// filling each only when the pane's own env has not already set it, so an
    /// explicit per-pane value (a profile pane's `env`) still wins.
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
            Some(program) => {
                let program = PathBuf::from(program);
                let shell_kind = ShellKind::from_program(&program);
                SpawnSpec {
                    program,
                    args: Vec::new(),
                    cwd,
                    env,
                    shell_kind,
                }
            }
            None => SpawnSpec::default_shell(cwd, env),
        }
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
    ) -> Result<CommandResult, Rejection> {
        let acting = self.acting_session(source)?;
        let session_id = self.resolve_session_target(args.session, source, acting)?;
        let new_name = generate_name(NameKind::Session, |candidate| {
            self.sessions
                .values()
                .any(|session| session.name == candidate)
        });
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;

        let events = session_ops::rename_session(session, new_name);

        Ok(Self::commit_events(command_id, events))
    }

    /// Handle [`Command::Quit`]: mark the process for immediate teardown.
    ///
    /// Sets the quit request the event loop polls after each event batch and
    /// flags zero-grace shutdown, so the loop exits on this iteration and
    /// teardown group-kills every pane's child without the graceful window.
    fn handle_quit(&mut self, command_id: CommandId) -> CommandResult {
        self.quit_requested = true;
        self.immediate_shutdown = true;
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    }

    /// The session's only attached client, or a rejection saying why — none
    /// are attached, or several are so the caller must name one. `none_tail`
    /// completes "no attached client …"; `ambiguous_noun` completes "… name a
    /// target client for …".
    ///
    /// On a session with two clients,
    /// `sole_attached_client(s, "whose focus could move", "the focus")` returns
    /// `Err(TargetAmbiguous, "multiple clients; name a target client for the focus")`.
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
    /// is therefore the **smallest** rect among the clients who actually draw X,
    /// which is the largest grid every one of them can show in full: nobody is
    /// ever shown a grid too big to fit, so no client has to crop.
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
        min: Size,
    ) -> Vec<(PaneId, Option<Rect>)> {
        let Some(tab) = session.tabs.get(&tab_id) else {
            return Vec::new();
        };
        let tab_rect = Rect::new(Point { x: 0, y: 0 }, viewport);

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
                    min,
                ))
            })
            .collect();

        // No viewer: no client draws any of these panes, so none of them is
        // resized and every PTY keeps the size it has. Unreachable from the
        // callers, which all resolve a `tab_viewport` first — and that is `Some`
        // only when this same filter finds a viewer.
        let Some(first) = per_viewer.first() else {
            return Vec::new();
        };

        // Merge by pane id, not by position: a pane's smallest rect across the
        // viewers that draw it. Keying on the id means the merge cannot depend on
        // two different solves listing their panes in the same order, so a change
        // to either traversal can never quietly hand a pane another pane's size.
        let mut smallest: HashMap<PaneId, Option<Rect>> = HashMap::new();
        for viewer in &per_viewer {
            for &(pane_id, content) in viewer {
                let Some(rect) = content else {
                    // This viewer draws no content for the pane, so it asks
                    // nothing of its size.
                    smallest.entry(pane_id).or_insert(None);
                    continue;
                };
                let entry = smallest.entry(pane_id).or_insert(Some(rect));
                *entry = Some(match *entry {
                    Some(current) => Rect::new(
                        current.origin,
                        Size {
                            cols: current.size.cols.min(rect.size.cols),
                            rows: current.size.rows.min(rect.size.rows),
                        },
                    ),
                    None => rect,
                });
            }
        }

        // Emit in the first viewer's solve order, so the result keeps the stable
        // pane order every consumer of this function already sees.
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
    /// resized. A tab no client views has no [`Session::tab_viewport`] and keeps
    /// its sizes. The shared shape behind every "a tab's viewer set changed"
    /// reflow — the full-tab solve with no freshly-spawned pane to skip.
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
        let rects = Self::tab_content_rects(session, tab_id, viewport, self.effective_pane_min());
        self.reflow_changed(backend, rects, None, events);
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
        &mut self,
        backend: &dyn PtyBackend,
        rects: Vec<(PaneId, Option<Rect>)>,
        skip: Option<PaneId>,
        events: &mut Vec<Event>,
    ) {
        let items: Vec<(PaneId, Option<Rect>)> = rects
            .into_iter()
            .filter(|(pane_id, content)| {
                Some(*pane_id) != skip
                    && self.pty_handles.contains_key(pane_id)
                    && match content {
                        Some(rect) => self.pty_sizes.get(pane_id) != Some(&compute_pty_size(*rect)),
                        None => false,
                    }
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

mod client;
mod pane;
mod resolve;
mod tab;
mod visual;

#[cfg(test)]
mod tests;
