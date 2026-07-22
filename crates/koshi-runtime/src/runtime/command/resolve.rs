//! Target resolution and admission checks for [`Server`] command dispatch.
//!
//! Every command names — or implies — the session, client, tab, or pane it acts
//! on. The methods here turn that into a concrete, validated target, or a
//! [`Rejection`] saying why it cannot: no such pane, several clients with none
//! named, a session shutting down. [`Server::validate`] runs them as the last
//! gate before a handler mutates state, so a handler always receives a target it
//! can trust.

use super::*;

impl Server {
    /// Check a command against live state before it reaches a handler. Runs the
    /// universal checks in fixed precedence: CLI command admission, session
    /// resolution, session admission, the in-session issuing pane's liveness,
    /// the acting client for a client-scoped command, then target resolution.
    /// Returns the first failure, or `Ok(())` when the command is well-formed
    /// against current state.
    pub(super) fn validate(&self, envelope: &CommandEnvelope) -> Result<(), Rejection> {
        // 1. CLI admission: a CLI source may only submit commands the CLI's
        //    own verbs build.
        if !Self::allowed_from_source(&envelope.command, &envelope.source) {
            return Err(Rejection::new(
                RejectReason::Unauthorized,
                "command cannot be issued from the CLI",
            ));
        }

        // 2. The session this command acts in (and, for a keybinding or mouse
        //    source, the client's liveness — the session is located by it).
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

        // 4. The pane an in-session CLI command was issued from must still be
        //    alive. A pane- or session-scoped command stays valid when the
        //    client that spawned the pane is gone — the pane outlives it.
        if let CommandSource::InSessionCli { pane_id, .. } = &envelope.source {
            Self::require_live_source_pane(Self::require_session(session)?, *pane_id)?;
        }

        // 5. A client-scoped command must resolve an acting client, whatever
        //    the source: the issuer while it is attached, else the session's
        //    sole attached client.
        if Self::is_client_scoped(&envelope.command) {
            Self::resolve_acting_client(&envelope.source, Self::require_session(session)?)?;
        }

        // 6. Target resolution: the pane/tab/session the command names must resolve.
        self.resolve_target(&envelope.command, &envelope.source, session)
    }

    /// The client a client-scoped command acts on: one rule, shared by every
    /// path that needs it, so validation and the handler always pick the same
    /// client.
    ///
    /// The source's own client wins while it is attached to `session`. When it
    /// is gone — or was never named, which a pane spawned with no designated
    /// client sends — the session's sole attached client stands in, because
    /// with exactly one window attached there is only one window the command
    /// could mean. Several attached is [`RejectReason::TargetAmbiguous`] and
    /// none is [`RejectReason::SourceClientStale`]: neither has a single
    /// answer, and the command is refused rather than guessed at.
    ///
    /// On a session whose sole client is `A`, a `koshi lock` issued from a pane
    /// whose own client has since detached resolves to `A`. Attach a second
    /// client and the same command is `TargetAmbiguous`.
    pub(super) fn resolve_acting_client(
        source: &CommandSource,
        session: &Session,
    ) -> Result<ClientId, Rejection> {
        if let Some(client_id) = source.client_id() {
            if session.clients.get(client_id).is_some() {
                return Ok(client_id);
            }
        }
        let mut attached = session.clients.list_attached();
        match (attached.next(), attached.next()) {
            (Some(only), None) => Ok(only.id()),
            (Some(_), Some(_)) => Err(Rejection::new(
                RejectReason::TargetAmbiguous,
                "several clients are attached; name the target client",
            )),
            (None, _) => Err(Rejection::new(
                RejectReason::SourceClientStale,
                "run this command from an active Koshi client",
            )),
        }
    }

    /// Whether `command` may arrive from `source`. CLI sources are limited to
    /// the commands the CLI's own verbs build; everything else — selection and
    /// mouse-select commands (mouse/keybinding only), plugin commands (plugin
    /// host only) — is refused before any state is read. `Quit` is accepted
    /// from an external CLI (`kill-session`) but not from inside a pane.
    /// Non-CLI sources are unrestricted here.
    pub(super) fn allowed_from_source(command: &Command, source: &CommandSource) -> bool {
        let cli_verb = matches!(
            command,
            Command::NewPane(_)
                | Command::ClosePane(_)
                | Command::ResizePane(_)
                | Command::TogglePaneFullscreen
                | Command::RenamePane(_)
                | Command::WriteToPane(_)
                | Command::RunCommandPane(_)
                | Command::NewTab(_)
                | Command::CloseTab(_)
                | Command::RenameTab(_)
                | Command::MoveTab(_)
                | Command::FocusTab(_)
                | Command::FocusPane(_)
                | Command::SetLockMode(_)
                | Command::ToggleLockMode
                | Command::RenameSession(_)
        );
        match source {
            CommandSource::InSessionCli { .. } => cli_verb,
            CommandSource::ExternalCli { .. } => cli_verb || matches!(command, Command::Quit),
            CommandSource::KeyBinding { .. }
            | CommandSource::Mouse { .. }
            | CommandSource::Plugin { .. }
            | CommandSource::Internal => true,
        }
    }

    /// Whether `command` acts on one client's own view state (lock mode,
    /// mouse-select, zoom) and carries no other target, so
    /// [`Self::resolve_acting_client`] alone decides which client it lands on.
    ///
    /// [`Command::FocusPane`], [`Command::FocusTab`], and [`Command::NewTab`]
    /// are absent because they also accept an explicit `client` argument that
    /// outranks the source; their resolvers call the same helper for the rest.
    /// [`Command::Visual`] is absent for the opposite reason: a highlight
    /// belongs to the client that made it, so a gone issuer means the target
    /// is gone, never another client's screen ([`Self::issuing_client`]).
    /// [`Command::ToggleMouseSelect`] has no CLI verb, so
    /// [`Self::allowed_from_source`] refuses it from a CLI before this runs.
    pub(super) fn is_client_scoped(command: &Command) -> bool {
        matches!(
            command,
            Command::ToggleLockMode
                | Command::SetLockMode(_)
                | Command::ToggleMouseSelect
                | Command::TogglePaneFullscreen
        )
    }

    /// Confirm the pane an in-session CLI command was issued from is still a
    /// valid source: registered in `session` and `Spawning`, `Running`, or
    /// `Exited` (a dead pane its close policy keeps on screen can still be
    /// commanded from — a background child it left behind may clean up after
    /// itself). A pane that is `Closing`, `Removed`, or absent from the
    /// registry rejects with [`RejectReason::TargetGone`].
    pub(super) fn require_live_source_pane(
        session: &Session,
        pane_id: PaneId,
    ) -> Result<(), Rejection> {
        let alive = session.panes.get(pane_id).is_some_and(|record| {
            matches!(
                record.lifecycle(),
                PaneLifecycle::Spawning | PaneLifecycle::Running | PaneLifecycle::Exited { .. }
            )
        });
        if alive {
            Ok(())
        } else {
            Err(Rejection::new(
                RejectReason::TargetGone,
                "source pane no longer exists",
            ))
        }
    }

    /// Resolve the session a command acts in from its source.
    ///
    /// An in-session CLI's own `session_id` is authoritative — the session is
    /// looked up by it; whether its client must still be attached depends on
    /// the command's scope class, checked in [`Self::validate`], not here. A
    /// keybinding/mouse names only a client and is located by it — a client
    /// with no session is [`RejectReason::SourceClientStale`]. An external
    /// CLI naming a session must match one. A missing
    /// session is [`RejectReason::TargetNotFound`]. Sources with no session
    /// context (`Plugin`, `Internal`, external with no session) resolve to `None`.
    pub(super) fn acting_session(
        &self,
        source: &CommandSource,
    ) -> Result<Option<&Session>, Rejection> {
        match source {
            CommandSource::InSessionCli { session_id, .. } => self
                .sessions()
                .get(session_id)
                .map(Some)
                .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound)),
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
    /// correct scope. Pane-addressed commands go through
    /// [`Self::resolve_pane_target`] — the same resolver their handlers use, so
    /// validation and application cannot disagree about the pane: an explicit
    /// `--pane` is global, a focused-pane default is the client's active tab
    /// ([`Self::require_pane_in_active_tab`]), and an in-session-CLI default is
    /// the issuing pane within the acting session
    /// ([`Self::resolve_pane_in_session`]). Tab targets are the acting
    /// session's tabs; session-level commands need a resolved session. The
    /// match is exhaustive so a new `Command` variant must declare its scope.
    pub(super) fn resolve_target(
        &self,
        command: &Command,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<(), Rejection> {
        match command {
            Command::FocusPane(args) => {
                Self::resolve_focus_target(args, source, session, self.effective_pane_min())
                    .map(drop)
            }
            Command::ClosePane(args) => self
                .resolve_pane_target(args.pane, source, session)
                .map(drop),
            Command::ResizePane(args) => self
                .resolve_pane_target(args.pane, source, session)
                .map(drop),
            Command::WriteToPane(args) => self
                .resolve_pane_target(args.pane, source, session)
                .map(drop),
            Command::RenamePane(args) => self
                .resolve_pane_target(args.pane, source, session)
                .map(drop),
            Command::NewPane(args) => self
                .resolve_new_pane_source(args, source, session)
                .map(drop),
            // Lock mode and mouse-select are client-scoped: the acting client
            // resolved by the client-scoped check in `validate` is the whole
            // target — no pane or tab to resolve.
            Command::ToggleLockMode | Command::SetLockMode(_) | Command::ToggleMouseSelect => {
                Ok(())
            }
            // A highlight command names its own pane, so there is no default to
            // resolve: the pane it names is the pane it means, and its handler
            // confirms that one still exists. Falling back to the focused pane
            // would let a command that named pane A act on pane B. The client
            // is the issuer alone — a highlight lives on the screen that made
            // it — so it is checked here rather than through the acting-client
            // fallback.
            Command::Visual(VisualCommand::SetSelection(_) | VisualCommand::ClearSelection(_)) => {
                Self::issuing_client(source).map(drop)
            }
            // Copy carries no pane yet, so it still means the focused one.
            Command::TogglePaneFullscreen | Command::Visual(VisualCommand::Copy(_)) => {
                self.resolve_pane_target(None, source, session).map(drop)
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
    pub(super) fn resolve_new_pane_source(
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
            // With no explicit source the default-pane resolution is exactly
            // [`Self::resolve_pane_target`]'s: the in-session CLI's captured
            // pane, else the issuing client's focused pane. The issuer becomes
            // the focus client only while still attached to the owning session
            // — an in-session CLI whose client is gone still splits its pane,
            // it just focuses the new pane for nobody.
            None => {
                let target = self.resolve_pane_target(None, source, session)?;
                let focus_client = source.client_id().filter(|client_id| {
                    self.sessions
                        .get(&target.session_id)
                        .is_some_and(|owner| owner.clients.get(*client_id).is_some())
                });
                Ok(NewPaneTarget {
                    session_id: target.session_id,
                    source_pane: target.pane_id,
                    tab_id: target.tab_id,
                    focus_client,
                })
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
    pub(super) fn resolve_pane_target(
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
                        let tab_id = session
                            .clients
                            .get(client_id)
                            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?
                            .active_tab();
                        Ok(PaneTarget {
                            session_id: session.id,
                            tab_id,
                            pane_id: Self::resolve_focused_pane(session, client_id)?,
                        })
                    }
                }
            }
        }
    }

    /// The id of the tab in `session` whose layout holds `pane` as a leaf, or
    /// [`RejectReason::TargetNotFound`] when no tab does.
    pub(super) fn tab_of_pane(session: &Session, pane: PaneId) -> Result<TabId, Rejection> {
        session
            .tabs
            .values()
            .find(|tab| tab.layout().contains_pane(pane))
            .map(|tab| tab.id())
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))
    }

    /// Confirm `pane` exists in `session`'s registry. Used for the in-session
    /// CLI default, whose target is the captured `pane_id` (tied to the session,
    /// not to live focus) — never the global registry.
    pub(super) fn resolve_pane_in_session(
        session: &Session,
        pane: PaneId,
    ) -> Result<(), Rejection> {
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
    pub(super) fn require_pane_in_active_tab(
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
    /// issuer. With no explicit target the acting client decides
    /// ([`Self::resolve_acting_client`]). Focus is tab-local, so the pane
    /// resolves through [`Self::require_pane_in_active_tab`]. A
    /// [`FocusTarget::Direction`] target resolves geometrically from the
    /// target client's focused pane over the solved layout
    /// ([`Self::directional_neighbor`]); no pane in that direction is
    /// [`RejectReason::TargetNotFound`].
    pub(super) fn resolve_focus_target(
        args: &FocusPaneArgs,
        source: &CommandSource,
        session: Option<&Session>,
        min: Size,
    ) -> Result<FocusPaneTarget, Rejection> {
        let session = Self::require_session(session)?;
        let client_id = Self::resolve_view_client(args.client, source, session)?;
        let pane_id = match args.target {
            FocusTarget::Pane(pane_id) => pane_id,
            FocusTarget::Direction(direction) => {
                let from = Self::resolve_focused_pane(session, client_id)?;
                Self::directional_neighbor(session, client_id, from, direction, min)?
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

    /// The nearest pane in `direction` from `from`, over the client's active
    /// tab solved at its current pane region.
    ///
    /// A candidate qualifies when its whole box lies on the far side of
    /// `from`'s edge in that direction and the two boxes overlap on the
    /// perpendicular axis — a pane diagonally offset with no shared span is
    /// not a neighbor. The nearest qualifying edge wins; among equals the
    /// larger perpendicular overlap does. Suppressed and zero-area panes
    /// never qualify. No qualifying pane is
    /// [`RejectReason::TargetNotFound`].
    pub(super) fn directional_neighbor(
        session: &Session,
        client_id: ClientId,
        from: PaneId,
        direction: Direction,
        min: Size,
    ) -> Result<PaneId, Rejection> {
        let client = session
            .clients
            .get(client_id)
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
        let tab_id = client.active_tab();
        let tab = session
            .tabs
            .get(&tab_id)
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;
        let viewport = session
            .tab_viewport(tab_id)
            .ok_or_else(|| Rejection::bare(RejectReason::InvalidState))?;
        // Directional focus moves within what THIS client sees, so the tab is
        // solved in this client's own mode: a zoomed client draws one pane and
        // has no neighbour to move to.
        let solve = solve_tab(tab, client.layout_mode(tab_id), viewport, min);
        let suppressed: HashSet<PaneId> = solve.suppressed.iter().copied().collect();
        let from_rect = solve
            .panes
            .iter()
            .find(|(pane_id, _)| *pane_id == from)
            .map(|(_, rect)| *rect)
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))?;

        let mut best: Option<(PaneId, u16, u16)> = None;
        for &(pane_id, rect) in &solve.panes {
            if pane_id == from || suppressed.contains(&pane_id) || rect.is_empty() {
                continue;
            }
            // Distance between the facing edges; `None` when the candidate is
            // not on the far side.
            let distance = match direction {
                Direction::Left => (rect.origin.x + rect.size.cols <= from_rect.origin.x)
                    .then(|| from_rect.origin.x - (rect.origin.x + rect.size.cols)),
                Direction::Right => (rect.origin.x >= from_rect.origin.x + from_rect.size.cols)
                    .then(|| rect.origin.x - (from_rect.origin.x + from_rect.size.cols)),
                Direction::Up => (rect.origin.y + rect.size.rows <= from_rect.origin.y)
                    .then(|| from_rect.origin.y - (rect.origin.y + rect.size.rows)),
                Direction::Down => (rect.origin.y >= from_rect.origin.y + from_rect.size.rows)
                    .then(|| rect.origin.y - (from_rect.origin.y + from_rect.size.rows)),
            };
            let Some(distance) = distance else {
                continue;
            };
            let overlap = match direction {
                Direction::Left | Direction::Right => span_overlap(
                    from_rect.origin.y,
                    from_rect.size.rows,
                    rect.origin.y,
                    rect.size.rows,
                ),
                Direction::Up | Direction::Down => span_overlap(
                    from_rect.origin.x,
                    from_rect.size.cols,
                    rect.origin.x,
                    rect.size.cols,
                ),
            };
            if overlap == 0 {
                continue;
            }
            let better = match best {
                None => true,
                Some((_, best_distance, best_overlap)) => {
                    distance < best_distance
                        || (distance == best_distance && overlap > best_overlap)
                }
            };
            if better {
                best = Some((pane_id, distance, overlap));
            }
        }
        best.map(|(pane_id, _, _)| pane_id).ok_or_else(|| {
            Rejection::new(RejectReason::TargetNotFound, "no pane in that direction")
        })
    }

    /// Resolve the client's focused pane. A client with no focused pane is
    /// [`RejectReason::TargetNotFound`]; a focus pointing outside the active tab
    /// is verified and rejected too, not assumed valid.
    pub(super) fn resolve_focused_pane(
        session: &Session,
        client_id: ClientId,
    ) -> Result<PaneId, Rejection> {
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
    pub(super) fn resolve_tab_or_active(
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
    pub(super) fn require_tab_containing_pane(
        session: &Session,
        pane: PaneId,
    ) -> Result<TabId, Rejection> {
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
    /// argument when set — it wins even over an in-session issuer, and one not
    /// attached to the acting session is [`RejectReason::TargetNotFound`],
    /// never a fallback to the issuer. With no explicit target the acting
    /// client decides ([`Self::resolve_acting_client`]).
    pub(super) fn resolve_view_client(
        explicit: Option<ClientId>,
        source: &CommandSource,
        session: &Session,
    ) -> Result<ClientId, Rejection> {
        match explicit {
            Some(client_id) => {
                if session.clients.get(client_id).is_none() {
                    return Err(Rejection::new(
                        RejectReason::TargetNotFound,
                        "target client not attached to the session",
                    ));
                }
                Ok(client_id)
            }
            None => Self::resolve_acting_client(source, session),
        }
    }

    /// Resolve the [`Command::NewTab`] target: the session the tab joins and
    /// the client that switches onto it ([`Self::resolve_view_client`]).
    /// Shared by validation and [`Self::handle_new_tab`] so both apply one
    /// contract.
    pub(super) fn resolve_new_tab_target(
        args: &NewTabArgs,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<NewTabTarget, Rejection> {
        let session = Self::require_session(session)?;
        let client_id = Self::resolve_view_client(args.client, source, session)?;
        Ok(NewTabTarget {
            session_id: session.id,
            client_id,
        })
    }

    /// Resolve the [`Command::FocusTab`] target: the client whose view
    /// switches ([`Self::resolve_view_client`]) and the concrete tab the
    /// target names — an id or index must match an existing tab, and
    /// `next`/`prev` step from the *target* client's active tab, wrapping at
    /// the ends. Shared by validation and [`Self::handle_focus_tab`] so both
    /// apply one contract.
    pub(super) fn resolve_focus_tab_target(
        args: &FocusTabArgs,
        source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<FocusTabTarget, Rejection> {
        let session = Self::require_session(session)?;
        let client_id = Self::resolve_view_client(args.client, source, session)?;
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
    pub(super) fn require_session(session: Option<&Session>) -> Result<&Session, Rejection> {
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
    pub(super) fn resolve_session_target(
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
    pub(super) fn require_tab(session: &Session, tab: TabId) -> Result<(), Rejection> {
        if session.tabs.contains_key(&tab) {
            Ok(())
        } else {
            Err(Rejection::bare(RejectReason::TargetNotFound))
        }
    }

    /// Whether `session` is shutting down (`Stopping`/`Stopped`) and so accepts
    /// no mutations.
    pub(super) fn is_winding_down(session: &Session) -> bool {
        matches!(
            session.lifecycle(),
            SessionLifecycle::Stopping | SessionLifecycle::Stopped
        )
    }
}
