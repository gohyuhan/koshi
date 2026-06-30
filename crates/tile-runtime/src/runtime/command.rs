//! Command dispatch: the single entrypoint every requested mutation passes
//! through.
//!
//! [`Runtime::dispatch`] validates one [`CommandEnvelope`] against live state,
//! then routes it via an exhaustive `match` on [`Command`] — one arm per
//! variant. Validation runs first: a command whose source may not issue it, or
//! whose target does not resolve, is rejected before any handler runs. Handlers
//! do not exist yet (they land in later command tasks), so a command that
//! *passes* validation still rejects cleanly with [`RejectReason::InvalidState`]
//! and a diagnostic hint. The exhaustive match is the point: a new `Command`
//! variant cannot be added without giving it an arm here, and each handler
//! replaces its arm in place as it ships.

use std::time::SystemTime;

use tile_core::{
    command::{Command, CommandEnvelope, CommandResult, CommandSource, NewPaneArgs, TabTarget},
    event::RejectReason,
    geometry::Direction,
    ids::{ClientId, CommandId, PaneId, SessionId, TabId},
};
use tile_session::session::{
    lifecycle::SessionLifecycle,
    pane_ops::{self, NewPaneError, NewPaneSpec},
    state::Session,
};

use crate::runtime::{state::Runtime, transaction::TransactionScope};

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
            Command::ClosePane(_) => self.reject(envelope.id, "close pane"),
            Command::ResizePane(_) => self.reject(envelope.id, "resize pane"),
            Command::FocusPane(_) => self.reject(envelope.id, "focus pane"),
            Command::NewTab(_) => self.reject(envelope.id, "new tab"),
            Command::CloseTab(_) => self.reject(envelope.id, "close tab"),
            Command::RenameTab(_) => self.reject(envelope.id, "rename tab"),
            Command::FocusTab(_) => self.reject(envelope.id, "focus tab"),
            Command::WriteToPane(_) => self.reject(envelope.id, "write to pane"),
            Command::ToggleLockMode => self.reject(envelope.id, "toggle lock mode"),
            Command::SetLockMode(_) => self.reject(envelope.id, "set lock mode"),
            Command::RunCommandPane(_) => self.reject(envelope.id, "run command pane"),
            Command::CopyMode(_) => self.reject(envelope.id, "copy mode"),
            Command::Plugin(_) => self.reject(envelope.id, "plugin"),
            Command::TogglePaneFullscreen => self.reject(envelope.id, "toggle pane fullscreen"),
            Command::RenamePane(_) => self.reject(envelope.id, "rename pane"),
            Command::MoveTab(_) => self.reject(envelope.id, "move tab"),
            Command::RenameSession(_) => self.reject(envelope.id, "rename session"),
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

    /// Turn a [`Rejection`] into a [`CommandResult::Rejected`] keyed to
    /// `command_id`.
    fn rejected(command_id: CommandId, rejection: Rejection) -> CommandResult {
        CommandResult::Rejected {
            command_id,
            reason: rejection.reason,
            help: rejection.help,
        }
    }

    /// Handle [`Command::NewPane`]: split the source pane's tab, register a
    /// `Spawning` pane, auto-focus it, and seal the resulting events. PTY spawn
    /// is a later task; this is the pure state transaction.
    fn handle_new_pane(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &NewPaneArgs,
        issued_at: SystemTime,
    ) -> CommandResult {
        // The stacked / in-place variants are a separate task; route them
        // explicitly until it lands rather than splitting directionally.
        if args.stacked || args.in_place {
            return CommandResult::Rejected {
                command_id,
                reason: RejectReason::InvalidState,
                help: Some("stacked/in-place new-pane pending".to_string()),
            };
        }

        let acting = match self.acting_session(source) {
            Ok(acting) => acting,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };
        let target = match self.resolve_new_pane_source(args, source, acting) {
            Ok(target) => target,
            Err(rejection) => return Self::rejected(command_id, rejection),
        };

        let direction = args.direction.unwrap_or(Direction::Right);
        let Some(session) = self.sessions.get_mut(&target.session_id) else {
            return Self::rejected(command_id, Rejection::bare(RejectReason::TargetNotFound));
        };

        let spec = NewPaneSpec {
            name: args.name.clone(),
            cwd: args.cwd.clone(),
            command: args.command.clone(),
        };
        match pane_ops::new_pane(
            session,
            target.source_pane,
            target.tab_id,
            direction,
            target.focus_client,
            spec,
            issued_at,
        ) {
            Ok(events) => {
                let mut scope = TransactionScope::new();
                for event in events {
                    scope.emit(event);
                }
                scope.commit(command_id)
            }
            Err(NewPaneError::SourceNotFound) => CommandResult::Rejected {
                command_id,
                reason: RejectReason::TargetNotFound,
                help: None,
            },
            Err(NewPaneError::WontFit) => CommandResult::Rejected {
                command_id,
                reason: RejectReason::MinSize,
                help: Some("not enough space to split".to_string()),
            },
        }
    }

    /// Check a command against live state before it reaches a handler. Runs the
    /// universal checks in fixed precedence: source policy, source-client
    /// liveness, session admission, then target resolution. Returns the first
    /// failure, or `Ok(())` when the command is well-formed against current
    /// state.
    fn validate(&self, envelope: &CommandEnvelope) -> Result<(), Rejection> {
        // 1. Source policy: a client-scoped command needs a client to act for.
        if Self::is_client_scoped(&envelope.command) && envelope.source.client_id().is_none() {
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
    /// active tab, lock mode, or copy-mode state) and so cannot be issued by a
    /// source that names no client.
    fn is_client_scoped(command: &Command) -> bool {
        matches!(
            command,
            Command::FocusPane(_)
                | Command::FocusTab(_)
                | Command::ToggleLockMode
                | Command::SetLockMode(_)
                | Command::TogglePaneFullscreen
                | Command::CopyMode(_)
        )
    }

    /// Resolve the session a command acts in from its source, and confirm the
    /// source's client (when it names one) is still attached.
    ///
    /// An in-session CLI's own `session_id` is authoritative — the session is
    /// looked up by it, and the named client must be attached *there*, so an
    /// inconsistent envelope (client attached elsewhere) is rejected rather than
    /// acting on the wrong session. A keybinding/mouse names only a client and is
    /// located by it. An external CLI naming a session must match one. A missing
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
                .sessions()
                .values()
                .find(|session| session.clients.get(*client_id).is_some())
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
            Command::FocusPane(args) => self.resolve_focus_target(args.pane, source, session),
            Command::ClosePane(args) => self.resolve_pane_or_focused(args.pane, source, session),
            Command::ResizePane(args) => self.resolve_pane_or_focused(args.pane, source, session),
            Command::WriteToPane(args) => self.resolve_pane_or_focused(args.pane, source, session),
            Command::RenamePane(args) => self.resolve_pane_or_focused(args.pane, source, session),
            Command::NewPane(args) => self
                .resolve_new_pane_source(args, source, session)
                .map(drop),
            Command::ToggleLockMode
            | Command::SetLockMode(_)
            | Command::TogglePaneFullscreen
            | Command::CopyMode(_) => self.resolve_default_pane(source, session),
            Command::CloseTab(args) => self.resolve_tab_or_active(args.tab, source, session),
            Command::RenameTab(args) => self.resolve_tab_or_active(args.tab, source, session),
            Command::MoveTab(args) => self.resolve_tab_or_active(args.tab, source, session),
            Command::FocusTab(args) => self.resolve_tab_target(args.target, source, session),
            Command::RunCommandPane(_) => self.resolve_default_pane(source, session),
            Command::NewTab(_) | Command::RenameSession(_) => {
                Self::require_session(session).map(drop)
            }
            Command::Plugin(_) => Ok(()),
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
                    .sessions()
                    .values()
                    .find(|session| session.panes.get(source_pane).is_some())
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
            None => self.resolve_default_pane(source, session),
        }
    }

    /// Resolve the pane a command defaults to when it names none, scoped to the
    /// **acting session** — a default target never escapes it. An in-session CLI
    /// command targets the pane it was issued from
    /// ([`CommandSource::InSessionCli`]'s `pane_id`) within that session; any
    /// other client source targets that client's focused pane. Fails with
    /// [`RejectReason::TargetNotFound`] when there is no such context — a target
    /// is never silently guessed.
    fn resolve_default_pane(
        &self,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<(), Rejection> {
        let session = Self::require_session(session)?;
        match source {
            CommandSource::InSessionCli { pane_id, .. } => {
                Self::resolve_pane_in_session(session, *pane_id)
            }
            _ => match source.client_id() {
                Some(client_id) => Self::resolve_focused_pane(session, client_id),
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

    /// Resolve the [`Command::FocusPane`] target against the source client's
    /// active tab.
    fn resolve_focus_target(
        &self,
        pane: PaneId,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<(), Rejection> {
        let session = Self::require_session(session)?;
        let client_id = source.client_id().ok_or_else(|| {
            Rejection::new(
                RejectReason::Unauthorized,
                "command requires an attached client",
            )
        })?;
        Self::require_pane_in_active_tab(session, client_id, pane)
    }

    /// Resolve the client's focused pane. A client with no focused pane is
    /// [`RejectReason::TargetNotFound`]; a focus pointing outside the active tab
    /// is rejected too, rather than trusting the focus invariant.
    fn resolve_focused_pane(session: &Session, client_id: ClientId) -> Result<(), Rejection> {
        let client = session
            .clients
            .get(client_id)
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
        let pane = client
            .focused_pane(client.active_tab())
            .ok_or_else(|| Rejection::new(RejectReason::TargetNotFound, "no focused pane"))?;
        Self::require_pane_in_active_tab(session, client_id, pane)
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
    ) -> Result<(), Rejection> {
        let session = Self::require_session(session)?;
        match tab {
            Some(tab) => Self::require_tab(session, tab),
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
                Self::require_tab(session, client.active_tab())
            }
        }
    }

    /// Find the tab in `session` whose layout contains `pane`, confirming the
    /// pane also has a live registry record.
    fn require_tab_containing_pane(session: &Session, pane: PaneId) -> Result<(), Rejection> {
        Self::resolve_pane_in_session(session, pane)?;
        if session
            .tabs
            .values()
            .any(|tab| tab.layout().contains_pane(pane))
        {
            Ok(())
        } else {
            Err(Rejection::new(
                RejectReason::TargetNotFound,
                "source pane not found in any tab",
            ))
        }
    }

    /// Resolve a [`Command::FocusTab`] target within the acting session. An id
    /// or index must match an existing tab; `Next`/`Prev` move relative to the
    /// source client's active tab, which must itself exist (a stale active tab is
    /// rejected, not silently no-opped by the handler).
    fn resolve_tab_target(
        &self,
        target: TabTarget,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<(), Rejection> {
        let session = Self::require_session(session)?;
        match target {
            TabTarget::Id(tab) => Self::require_tab(session, tab),
            TabTarget::Index(index) => {
                if session.tabs.values().any(|tab| tab.index() == index) {
                    Ok(())
                } else {
                    Err(Rejection::bare(RejectReason::TargetNotFound))
                }
            }
            TabTarget::Next | TabTarget::Prev => {
                let client_id = source
                    .client_id()
                    .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
                let client = session
                    .clients
                    .get(client_id)
                    .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
                Self::require_tab(session, client.active_tab())
            }
        }
    }

    /// The acting session, or [`RejectReason::TargetNotFound`] when a
    /// session-scoped command has no session context to resolve within.
    fn require_session(session: Option<&Session>) -> Result<&Session, Rejection> {
        session.ok_or_else(|| Rejection::new(RejectReason::TargetNotFound, "no session context"))
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
        match self
            .sessions()
            .values()
            .find(|session| session.panes.get(pane).is_some())
        {
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
