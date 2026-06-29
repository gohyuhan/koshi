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

use tile_core::{
    command::{Command, CommandEnvelope, CommandResult, CommandSource, TabTarget},
    event::RejectReason,
    ids::{ClientId, CommandId, PaneId, TabId},
};
use tile_session::session::{lifecycle::SessionLifecycle, state::Session};

use crate::runtime::state::Runtime;

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
            Command::NewPane(_) => self.reject(envelope.id, "new pane"),
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
            if matches!(
                session.lifecycle(),
                SessionLifecycle::Stopping | SessionLifecycle::Stopped
            ) {
                return Err(Rejection::new(
                    RejectReason::InvalidState,
                    "session is stopping",
                ));
            }
        }

        // 4. Target resolution: the pane the command names must exist.
        self.resolve_target(&envelope.command, &envelope.source, session)
    }

    /// Whether `command` acts on a specific client's view (its focus or lock
    /// mode) and so cannot be issued by a source that names no client.
    fn is_client_scoped(command: &Command) -> bool {
        matches!(
            command,
            Command::FocusPane(_)
                | Command::ToggleLockMode
                | Command::SetLockMode(_)
                | Command::TogglePaneFullscreen
        )
    }

    /// Resolve the session a command acts in from its source, and confirm the
    /// source's client (when it names one) is still attached.
    ///
    /// A source that names a client must match an attached client somewhere, or
    /// the client has detached ([`RejectReason::SourceClientStale`]). An
    /// external CLI invocation naming a session must match one
    /// ([`RejectReason::TargetNotFound`]). Sources with no session context
    /// (`Plugin`, `Internal`, external with no session) resolve to `None`.
    fn acting_session(&self, source: &CommandSource) -> Result<Option<&Session>, Rejection> {
        match source.client_id() {
            Some(client_id) => self
                .sessions()
                .values()
                .find(|session| session.clients.get(client_id).is_some())
                .map(Some)
                .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale)),
            None => match source {
                CommandSource::ExternalCli {
                    session_id: Some(session_id),
                } => self
                    .sessions()
                    .get(session_id)
                    .map(Some)
                    .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound)),
                _ => Ok(None),
            },
        }
    }

    /// Resolve the pane or tab target a command names, if any. An explicit pane
    /// id is looked up globally; a `None` pane/tab target falls back to the
    /// source client's focused pane or active tab. Commands that name neither
    /// resolve to `Ok(())`.
    fn resolve_target(
        &self,
        command: &Command,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<(), Rejection> {
        match command {
            Command::FocusPane(args) => self.resolve_pane_global(args.pane),
            Command::ClosePane(args) => self.resolve_pane_or_focused(args.pane, source, session),
            Command::ResizePane(args) => self.resolve_pane_or_focused(args.pane, source, session),
            Command::WriteToPane(args) => self.resolve_pane_or_focused(args.pane, source, session),
            Command::RenamePane(args) => self.resolve_pane_or_focused(args.pane, source, session),
            Command::NewPane(args) => match args.source {
                Some(pane) => self.resolve_pane_global(pane),
                None => Ok(()),
            },
            Command::ToggleLockMode | Command::SetLockMode(_) | Command::TogglePaneFullscreen => {
                self.resolve_focused(source, session)
            }
            Command::CloseTab(args) => self.resolve_tab_or_active(args.tab, source, session),
            Command::RenameTab(args) => self.resolve_tab_or_active(args.tab, source, session),
            Command::MoveTab(args) => self.resolve_tab_or_active(args.tab, source, session),
            Command::FocusTab(args) => self.resolve_tab_target(args.target, session),
            _ => Ok(()),
        }
    }

    /// Resolve an explicit pane target, or the focused pane when none is given.
    fn resolve_pane_or_focused(
        &self,
        pane: Option<PaneId>,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<(), Rejection> {
        match pane {
            Some(pane) => self.resolve_pane_global(pane),
            None => self.resolve_focused(source, session),
        }
    }

    /// Resolve the source client's focused pane. Fails with
    /// [`RejectReason::TargetNotFound`] when there is no session/client context
    /// to default from — a target is never silently guessed.
    fn resolve_focused(
        &self,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<(), Rejection> {
        match (session, source.client_id()) {
            (Some(session), Some(client_id)) => self.resolve_focused_pane(session, client_id),
            _ => Err(Rejection::new(
                RejectReason::TargetNotFound,
                "no target and no focused pane to default to",
            )),
        }
    }

    /// Confirm the client has a focused pane in its active tab.
    fn resolve_focused_pane(
        &self,
        session: &Session,
        client_id: ClientId,
    ) -> Result<(), Rejection> {
        let client = session
            .clients
            .get(client_id)
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
        if client.focused_pane(client.active_tab()).is_some() {
            Ok(())
        } else {
            Err(Rejection::new(
                RejectReason::TargetNotFound,
                "no focused pane",
            ))
        }
    }

    /// Resolve an explicit tab target within the acting session, or the source
    /// client's active tab when none is given. Fails with
    /// [`RejectReason::TargetNotFound`] when there is no session/client context
    /// to default from — a target is never silently guessed.
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

    /// Resolve a [`Command::FocusTab`] target within the acting session. An id
    /// or index must match an existing tab; `Next`/`Prev` need at least one tab
    /// to move among.
    fn resolve_tab_target(
        &self,
        target: TabTarget,
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
                if session.tabs.is_empty() {
                    Err(Rejection::new(
                        RejectReason::TargetNotFound,
                        "no tabs to focus",
                    ))
                } else {
                    Ok(())
                }
            }
        }
    }

    /// The acting session, or [`RejectReason::TargetNotFound`] when a tab
    /// command has no session context to resolve within.
    fn require_session(session: Option<&Session>) -> Result<&Session, Rejection> {
        session.ok_or_else(|| {
            Rejection::new(
                RejectReason::TargetNotFound,
                "no session context for tab target",
            )
        })
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
    /// [`RejectReason::TargetNotFound`].
    fn resolve_pane_global(&self, pane: PaneId) -> Result<(), Rejection> {
        if self
            .sessions()
            .values()
            .any(|session| session.panes.get(pane).is_some())
        {
            Ok(())
        } else {
            Err(Rejection::bare(RejectReason::TargetNotFound))
        }
    }

    /// Re-check, at the moment a mutation is about to apply, that a pane
    /// resolved earlier is still present; a pane that has since vanished is
    /// [`RejectReason::TargetGone`]. The apply-time transaction that calls this
    /// is not wired in yet.
    #[allow(dead_code)]
    fn recheck_pane_present(&self, pane: PaneId) -> Result<(), Rejection> {
        if self
            .sessions()
            .values()
            .any(|session| session.panes.get(pane).is_some())
        {
            Ok(())
        } else {
            Err(Rejection::bare(RejectReason::TargetGone))
        }
    }
}

#[cfg(test)]
mod tests;
