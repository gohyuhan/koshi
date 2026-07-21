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
    /// universal checks in fixed precedence: source policy, source-client
    /// liveness, session admission, then target resolution. Returns the first
    /// failure, or `Ok(())` when the command is well-formed against current
    /// state.
    pub(super) fn validate(&self, envelope: &CommandEnvelope) -> Result<(), Rejection> {
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
    /// lock mode, or selection) and so cannot be issued by a source
    /// that names no client. [`Command::FocusPane`], [`Command::FocusTab`],
    /// and [`Command::NewTab`] are absent: each resolves its own target
    /// client (explicit `client` argument, issuing client, or the session's
    /// sole attached client) in its resolver.
    pub(super) fn requires_issuing_client(command: &Command) -> bool {
        matches!(
            command,
            Command::ToggleLockMode
                | Command::SetLockMode(_)
                | Command::TogglePaneFullscreen
                | Command::Visual(_)
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
    pub(super) fn acting_session(
        &self,
        source: &CommandSource,
    ) -> Result<Option<&Session>, Rejection> {
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
            Command::ClosePane(args) => self.resolve_pane_or_focused(args.pane, source, session),
            Command::ResizePane(args) => self.resolve_pane_or_focused(args.pane, source, session),
            Command::WriteToPane(args) => self.resolve_pane_or_focused(args.pane, source, session),
            Command::RenamePane(args) => self.resolve_pane_or_focused(args.pane, source, session),
            Command::NewPane(args) => self
                .resolve_new_pane_source(args, source, session)
                .map(drop),
            // Lock mode and mouse-select are client-scoped: the acting client
            // (confirmed attached by `acting_session`) is the whole target — no
            // pane or tab to resolve.
            Command::ToggleLockMode | Command::SetLockMode(_) | Command::ToggleMouseSelect => {
                Ok(())
            }
            // A highlight command names its own pane, so there is no default to
            // resolve: the pane it names is the pane it means, and its handler
            // confirms that one still exists. Falling back to the focused pane
            // would let a command that named pane A act on pane B.
            Command::Visual(VisualCommand::SetSelection(_) | VisualCommand::ClearSelection(_)) => {
                Ok(())
            }
            // Copy carries no pane yet, so it still means the focused one.
            Command::TogglePaneFullscreen | Command::Visual(VisualCommand::Copy(_)) => {
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
            // pane, else the issuing client's focused pane. The issuer (already
            // confirmed attached by that resolution) becomes the focus client.
            None => {
                let target = self.resolve_pane_target(None, source, session)?;
                Ok(NewPaneTarget {
                    session_id: target.session_id,
                    source_pane: target.pane_id,
                    tab_id: target.tab_id,
                    focus_client: source.client_id(),
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
    pub(super) fn tab_of_pane(session: &Session, pane: PaneId) -> Result<TabId, Rejection> {
        session
            .tabs
            .values()
            .find(|tab| tab.layout().contains_pane(pane))
            .map(|tab| tab.id())
            .ok_or_else(|| Rejection::bare(RejectReason::TargetNotFound))
    }

    /// Resolve an explicit pane target, or the default pane when none is given.
    pub(super) fn resolve_pane_or_focused(
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
    pub(super) fn resolve_default_pane(
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
    /// issuer. With no explicit target the issuing client is used; a source
    /// with no client defaults to the session's sole attached client, a
    /// session with several is [`RejectReason::TargetAmbiguous`], and one with
    /// none is [`RejectReason::InvalidState`]. Focus is tab-local, so the pane
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
                Self::sole_attached_client(session, "whose focus could move", "the focus")?.id()
            }
        };
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
    /// argument when set — it wins even over an in-session issuer, and one
    /// not attached to the acting session is [`RejectReason::TargetNotFound`],
    /// never a fallback to the issuer. With no explicit target the issuing
    /// client is used; a source with no client defaults to the session's sole
    /// attached client, a session with several is
    /// [`RejectReason::TargetAmbiguous`], and one with none is
    /// [`RejectReason::InvalidState`]. `what` names the command's object in
    /// the rejection hints.
    pub(super) fn resolve_tab_client(
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
                Ok(
                    Self::sole_attached_client(session, &format!("to switch onto {what}"), what)?
                        .id(),
                )
            }
        }
    }

    /// Resolve the [`Command::NewTab`] target: the session the tab joins and
    /// the client that switches onto it ([`Self::resolve_tab_client`]).
    /// Shared by validation and [`Self::handle_new_tab`] so both apply one
    /// contract.
    pub(super) fn resolve_new_tab_target(
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
    pub(super) fn resolve_focus_tab_target(
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

    /// Look a pane up across every session's registry, the way an explicit
    /// `--pane` target resolves. Absent everywhere is
    /// [`RejectReason::TargetNotFound`]; found in a session that is winding down
    /// is [`RejectReason::InvalidState`], so a cross-session target cannot slip
    /// past the owning session's admission check.
    pub(super) fn resolve_pane_global(&self, pane: PaneId) -> Result<(), Rejection> {
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
    pub(super) fn is_winding_down(session: &Session) -> bool {
        matches!(
            session.lifecycle(),
            SessionLifecycle::Stopping | SessionLifecycle::Stopped
        )
    }
}
