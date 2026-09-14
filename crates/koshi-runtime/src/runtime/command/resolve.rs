//! Target resolution and admission checks for [`Server`] command dispatch.
//!
//! Every command names — or implies — the session, client, tab, or pane it acts
//! on. The methods here turn that into a concrete, validated target, or a
//! [`Rejection`] saying why it cannot: no such pane, several clients with none
//! named, a session shutting down. [`Server::validate_command`] runs them as
//! the last gate before a handler mutates state, so a handler always receives a target it
//! can trust.

use super::*;

impl Server {
    /// Check a command against live state before it reaches a handler. Runs the
    /// universal checks in fixed precedence: CLI command admission, session
    /// resolution, session admission, the in-session issuing pane's liveness,
    /// the acting client for a client-scoped command, then target resolution.
    /// Returns the first failure, or `Ok(())` when the command is well-formed
    /// against current state.
    ///
    /// [`Server::dispatch_reporting_spare`] is the only caller and runs this
    /// before any handler, so every command crosses this gate.
    pub(super) fn validate_command(&self, envelope: &CommandEnvelope) -> Result<(), Rejection> {
        // 1. CLI admission: a CLI command source may only submit commands the CLI's
        //    own verbs build.
        if !Self::is_command_allowed_from_source(&envelope.command, &envelope.command_source) {
            return Err(Rejection::from_reason_and_help(
                RejectReason::Unauthorized,
                "command cannot be issued from the CLI",
            ));
        }

        // 2. The session this command acts in (and, for a keybinding or mouse
        //    command source, the client's liveness — the session is located by it).
        let session = self.acting_session(&envelope.command_source)?;

        // 3. Session admission: a winding-down session takes no mutations.
        if let Some(session) = session {
            if Self::is_winding_down(session) {
                return Err(Rejection::from_reason_and_help(
                    RejectReason::InvalidState,
                    "session is stopping",
                ));
            }
        }

        // 4. The pane an in-session CLI command was issued from must still be
        //    alive. A pane- or session-scoped command stays valid when the
        //    client that spawned the pane is gone — the pane outlives it.
        if let CommandSource::InSessionCli { pane_id, .. } = &envelope.command_source {
            Self::require_live_source_pane(Self::require_session(session)?, *pane_id)?;
        }

        // 5. A client-scoped command must resolve an acting client, whatever
        //    the command source: the issuer while it is attached, else the session's
        //    sole attached client.
        if Self::is_client_scoped(&envelope.command) {
            Self::resolve_acting_client(&envelope.command_source, Self::require_session(session)?)?;
        }

        // 6. Target resolution: the pane/tab/session the command names must resolve.
        self.resolve_target(&envelope.command, &envelope.command_source, session)
    }

    /// The client a client-scoped command acts on: one rule, shared by every
    /// path that needs it, so validation and the handler always pick the same
    /// client.
    ///
    /// The command source's own client wins while it is attached to `session`. When it
    /// is gone — or was never named, which a pane spawned with no designated
    /// client sends — the session's sole attached client stands in. Several
    /// attached is [`RejectReason::TargetAmbiguous`] and none is
    /// [`RejectReason::SourceClientStale`]; neither has a single answer, and
    /// the command is refused.
    ///
    /// On a session whose sole client is `A`, a `koshi lock` issued from a pane
    /// whose own client has since detached resolves to `A`. Attach a second
    /// client and the same command is `TargetAmbiguous`.
    pub(super) fn resolve_acting_client(
        command_source: &CommandSource,
        session: &Session,
    ) -> Result<ClientId, Rejection> {
        if let Some(client_id) = command_source.get_client_id() {
            if session.clients.get_client_by_id(client_id).is_some() {
                return Ok(client_id);
            }
        }
        let mut attached_clients = session.clients.list_attached_clients();
        match (attached_clients.next(), attached_clients.next()) {
            (Some(sole_client), None) => Ok(sole_client.get_client_id()),
            (Some(_), Some(_)) => Err(Rejection::from_reason_and_help(
                RejectReason::TargetAmbiguous,
                "several clients are attached; name the target client",
            )),
            (None, _) => Err(Rejection::from_reason_and_help(
                RejectReason::SourceClientStale,
                "no client is attached to the session",
            )),
        }
    }

    /// Whether `command` may arrive from `command source`. CLI sources are limited to
    /// the commands the CLI's own verbs build; everything else — selection and
    /// mouse-select commands (mouse/keybinding only), plugin commands (plugin
    /// host only) — is refused before any state is read. `Quit` is accepted
    /// from an external CLI (`kill-session`) but not from inside a pane.
    /// `Detach` and `DetachAll` are accepted from both, since a client detaches
    /// itself from inside the session and by id from outside it.
    /// `SwitchSession` is accepted from both, since an in-pane
    /// `koshi attach <session>` sends it.
    /// Non-CLI sources are unrestricted here.
    pub(super) fn is_command_allowed_from_source(
        command: &Command,
        command_source: &CommandSource,
    ) -> bool {
        let is_cli_command_allowed = matches!(
            command,
            Command::NewPane(_)
                | Command::ClosePane(_)
                | Command::ResizePane(_)
                | Command::TogglePaneFullscreen
                | Command::WriteToPane(_)
                | Command::RunCommandPane(_)
                | Command::NewTab(_)
                | Command::CloseTab(_)
                | Command::MoveTab(_)
                | Command::FocusTab(_)
                | Command::FocusPane(_)
                | Command::SetLockMode(_)
                | Command::ToggleLockMode(_)
                | Command::Detach(_)
                | Command::DetachAll
                | Command::SwitchSession(_)
        );
        match command_source {
            CommandSource::InSessionCli { .. } => is_cli_command_allowed,
            CommandSource::ExternalCli { .. } => {
                is_cli_command_allowed || matches!(command, Command::Quit)
            }
            CommandSource::KeyBinding { .. }
            | CommandSource::Mouse { .. }
            | CommandSource::Plugin { .. }
            | CommandSource::Internal => true,
        }
    }

    /// Whether `command` acts on one client's own view state (mouse-select)
    /// and carries no other target, so
    /// [`Self::resolve_acting_client`] alone decides which client it lands on.
    ///
    /// [`Command::FocusPane`], [`Command::FocusTab`], [`Command::NewTab`],
    /// [`Command::SetLockMode`], [`Command::ToggleLockMode`], and
    /// [`Command::SwitchSession`] are absent: they also accept an explicit
    /// `client` argument that outranks the command source, and their resolvers call
    /// the same helper for the rest.
    /// [`Command::TogglePaneFullscreen`] is absent for the same reason: it
    /// accepts an explicit target client on its command source
    /// ([`CommandSource::target_client`]) that outranks the issuer, and
    /// [`Self::resolve_fullscreen_target`] applies the same ladder the lock
    /// commands use.
    /// [`Command::Visual`] is absent too: a highlight
    /// belongs to the client that made it, so a gone issuer means the target
    /// is gone, never another client's screen ([`Self::resolve_issuing_client_id`]).
    /// [`Command::ToggleMouseSelect`] has no CLI verb, so
    /// [`Self::is_command_allowed_from_source`] refuses it from a CLI before this runs.
    pub(super) fn is_client_scoped(command: &Command) -> bool {
        matches!(command, Command::ToggleMouseSelect)
    }

    /// Confirm the pane an in-session CLI command was issued from is still a
    /// valid command source: registered in `session` and `Spawning`, `Running`, or
    /// `Exited` (a dead pane its close policy keeps on screen can still be
    /// commanded from — a background child it left behind may clean up after
    /// itself). A pane that is `Closing`, `Removed`, or absent from the
    /// registry rejects with [`RejectReason::TargetGone`].
    pub(super) fn require_live_source_pane(
        session: &Session,
        pane_id: PaneId,
    ) -> Result<(), Rejection> {
        let is_source_pane_live =
            session
                .panes
                .get_pane_record_by_id(pane_id)
                .is_some_and(|pane_record| {
                    matches!(
                        pane_record.get_lifecycle(),
                        PaneLifecycle::Spawning
                            | PaneLifecycle::Running
                            | PaneLifecycle::Exited { .. }
                    )
                });
        if is_source_pane_live {
            Ok(())
        } else {
            Err(Rejection::from_reason_and_help(
                RejectReason::TargetGone,
                "source pane no longer exists",
            ))
        }
    }

    /// Resolve the session a command acts in from its command source.
    ///
    /// An in-session CLI's own `session_id` is authoritative — the session is
    /// looked up by it; whether its client must still be attached depends on
    /// the command's scope class, checked in [`Self::validate_command`], not here. A
    /// keybinding/mouse names only a client and is located by it — a client
    /// with no session is [`RejectReason::SourceClientStale`]. An external
    /// CLI naming a session must match one. A missing
    /// session is [`RejectReason::TargetNotFound`]. Sources with no session
    /// context (`Plugin`, `Internal`, external with no session) resolve to `None`.
    pub(super) fn acting_session(
        &self,
        command_source: &CommandSource,
    ) -> Result<Option<&Session>, Rejection> {
        match command_source {
            CommandSource::InSessionCli { session_id, .. }
            | CommandSource::ExternalCli {
                session_id: Some(session_id),
                ..
            } => self
                .list_sessions()
                .get(session_id)
                .map(Some)
                .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound)),
            CommandSource::KeyBinding { client_id } | CommandSource::Mouse { client_id } => self
                .get_session_for_client(*client_id)
                .map(Some)
                .ok_or_else(|| Rejection::from_reason(RejectReason::SourceClientStale)),
            CommandSource::ExternalCli {
                session_id: None, ..
            }
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
        command_source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<(), Rejection> {
        match command {
            Command::FocusPane(command_args) => Self::resolve_focus_target(
                command_args,
                command_source,
                session,
                self.get_pane_sizing(),
            )
            .map(drop),
            Command::ClosePane(command_args) => self
                .resolve_pane_target(command_args.pane_id, command_source, session)
                .map(drop),
            Command::ResizePane(command_args) => self
                .resolve_pane_target(command_args.pane_id, command_source, session)
                .map(drop),
            Command::WriteToPane(command_args) => self
                .resolve_pane_target(command_args.pane_id, command_source, session)
                .map(drop),
            Command::NewPane(command_args) => self
                .resolve_new_pane_source(command_args, command_source, session)
                .map(drop),
            // Lock mode targets a client alone: the explicit `client` argument
            // when set, else the acting client — no pane or tab to resolve.
            Command::SetLockMode(command_args) => Self::resolve_view_client(
                command_args.client_id,
                command_source,
                Self::require_session(session)?,
            )
            .map(drop),
            Command::ToggleLockMode(command_args) => Self::resolve_view_client(
                command_args.client_id,
                command_source,
                Self::require_session(session)?,
            )
            .map(drop),
            // Mouse-select is client-scoped: the acting client resolved by the
            // client-scoped check in `validate_command` is the whole target.
            Command::ToggleMouseSelect => Ok(()),
            // A highlight command names its own pane, so there is no default to
            // resolve: the pane it names is the pane it means, and its handler
            // confirms that one still exists. Falling back to the focused pane
            // would let a command that named pane A act on pane B. The client
            // is the issuer alone — a highlight lives on the screen that made
            // it — so it is checked here rather than through the acting-client
            // fallback.
            Command::Visual(VisualCommand::SetSelection(_) | VisualCommand::ClearSelection(_)) => {
                Self::resolve_issuing_client_id(command_source).map(drop)
            }
            // A zoom flips one client's own view of the pane that client is
            // looking at. The pane and the client resolve together through one
            // helper the handler shares.
            Command::TogglePaneFullscreen => self
                .resolve_fullscreen_target(command_source, session)
                .map(drop),
            // Copy carries no pane yet, so it still means the focused one.
            Command::Visual(VisualCommand::Copy(_)) => self
                .resolve_pane_target(None, command_source, session)
                .map(drop),
            Command::CloseTab(command_args) => self
                .resolve_tab_or_active(command_args.tab_id, command_source, session)
                .map(drop),
            Command::MoveTab(command_args) => self
                .resolve_tab_or_active(command_args.tab_id, command_source, session)
                .map(drop),
            Command::FocusTab(command_args) => {
                Self::resolve_focus_tab_target(command_args, command_source, session).map(drop)
            }
            Command::NewTab(command_args) => {
                Self::resolve_new_tab_target(command_args, command_source, session).map(drop)
            }
            Command::RunCommandPane(command_args) => self
                .resolve_new_pane_source(
                    &Self::run_command_new_pane_args(command_args),
                    command_source,
                    session,
                )
                .map(drop),
            // Detach names one client, resolved and vetted here so the handler
            // receives a client it may remove.
            Command::Detach(command_args) => Self::resolve_target_client(
                command_args.client_id,
                command_source,
                Self::require_session(session)?,
            )
            .map(drop),
            // The switch names one client the same way a detach does, and the
            // handler resolves it again through the same helper.
            Command::SwitchSession(command_args) => Self::resolve_target_client(
                command_args.client_id,
                command_source,
                Self::require_session(session)?,
            )
            .map(drop),
            Command::Plugin(_) | Command::DetachAll | Command::Quit => Ok(()),
        }
    }

    /// The client a command names in its own `client` argument, resolved by
    /// [`Self::resolve_view_client`]. A [`RejectReason::TargetAmbiguous`] hint
    /// from that call is replaced with one listing every attached client id to
    /// choose from.
    ///
    /// Shared by [`Command::Detach`], which removes the client it resolves, and
    /// [`Command::SwitchSession`], which moves it to another session. Validation
    /// and the handler both call it.
    pub(super) fn resolve_target_client(
        explicit_client_id: Option<ClientId>,
        command_source: &CommandSource,
        session: &Session,
    ) -> Result<ClientId, Rejection> {
        Self::resolve_view_client(explicit_client_id, command_source, session).map_err(
            |rejection| {
                if rejection.reason == RejectReason::TargetAmbiguous {
                    let client_id_strings: Vec<String> = session
                        .clients
                        .list_attached_clients()
                        .map(|client| client.get_client_id().to_string())
                        .collect();
                    Rejection::from_reason_and_help(
                        RejectReason::TargetAmbiguous,
                        &format!(
                            "several clients are attached; specify the client: {}",
                            client_id_strings.join(", ")
                        ),
                    )
                } else {
                    rejection
                }
            },
        )
    }

    /// Resolve a [`Command::NewPane`] to its concrete target: the session and
    /// tab the new pane joins, the command source pane it splits from, and the client to
    /// auto-focus it for. Shared by [`Self::validate_command`] (which drops the value)
    /// and [`Self::handle_new_pane`], so both agree on one resolution.
    ///
    /// An explicit `--pane` is global: the new pane joins whatever session owns
    /// that pane, focused for the acting client only when that client is
    /// attached there. An explicit `--tab` (with no `--pane`) picks the tab
    /// within the acting session and anchors the split on that tab's most
    /// recently focused pane ([`Self::resolve_tab_anchor_pane_id`]). With neither, the
    /// command source defaults within the acting session — an in-session CLI's
    /// captured pane, or the acting client's focused pane.
    pub(super) fn resolve_new_pane_source(
        &self,
        command_args: &NewPaneArgs,
        command_source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<NewPaneTarget, Rejection> {
        match (command_args.source_pane_id, command_args.tab_id) {
            // An explicit tab picks where the pane lands; the split anchors on
            // that tab's most recently focused pane. The issuer becomes the
            // focus client only while still attached to the acting session.
            (None, Some(tab_id)) => {
                let session = Self::require_session(session)?;
                let source_pane_id = Self::resolve_tab_anchor_pane_id(session, tab_id)?;
                let focus_client_id = command_source
                    .get_client_id()
                    .filter(|client_id| session.clients.get_client_by_id(*client_id).is_some());
                Ok(NewPaneTarget {
                    session_id: session.session_id,
                    source_pane_id,
                    tab_id,
                    focus_client_id,
                })
            }
            // An explicit command source pane wins outright, and with none the default
            // pane stands in — the in-session CLI's captured pane, else the
            // acting client's focused pane. Both resolve through
            // [`Self::resolve_pane_target`], so the pane's own tab is the
            // receiving tab. The issuer becomes the focus client only while
            // still attached to the owning session — an in-session CLI whose
            // client is gone still splits its pane, it just focuses the new pane
            // for nobody.
            (source_pane_id, _) => {
                let pane_target =
                    self.resolve_pane_target(source_pane_id, command_source, session)?;
                let focus_client_id = command_source.get_client_id().filter(|client_id| {
                    self.session_by_id
                        .get(&pane_target.session_id)
                        .is_some_and(|owner_session| {
                            owner_session.clients.get_client_by_id(*client_id).is_some()
                        })
                });
                Ok(NewPaneTarget {
                    session_id: pane_target.session_id,
                    source_pane_id: pane_target.pane_id,
                    tab_id: pane_target.tab_id,
                    focus_client_id,
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
    /// command source targets the target client's focused pane in its active tab —
    /// the client the caller named ([`CommandSource::target_client`]) when
    /// there is one, else the issuer while attached, else the session's sole
    /// attached client ([`Self::resolve_view_client`]) — so an external CLI
    /// acts exactly where a keypress on that client would. Resolved through the
    /// shared defensive helpers, so a stale focus entry is rejected, never
    /// acted on.
    ///
    /// With clients A and B attached and B named as the target, the result is
    /// B's focused pane in B's active tab; with neither named, it is
    /// [`RejectReason::TargetAmbiguous`].
    pub(super) fn resolve_pane_target(
        &self,
        requested_pane_id: Option<PaneId>,
        command_source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<PaneTarget, Rejection> {
        match requested_pane_id {
            Some(pane_id) => {
                let owner = self
                    .get_session_for_pane(pane_id)
                    .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
                if Self::is_winding_down(owner) {
                    return Err(Rejection::from_reason_and_help(
                        RejectReason::InvalidState,
                        "session is stopping",
                    ));
                }
                let tab_id = Self::resolve_tab_id_for_pane(owner, pane_id)?;
                Ok(PaneTarget {
                    session_id: owner.session_id,
                    tab_id,
                    pane_id,
                })
            }
            None => {
                let session = Self::require_session(session)?;
                match command_source {
                    CommandSource::InSessionCli { pane_id, .. } => {
                        // The captured pane defines its own tab; confirm it is
                        // still a live, registered leaf before acting on it.
                        Self::resolve_pane_in_session(session, *pane_id)?;
                        let tab_id = Self::resolve_tab_id_for_pane(session, *pane_id)?;
                        Ok(PaneTarget {
                            session_id: session.session_id,
                            tab_id,
                            pane_id: *pane_id,
                        })
                    }
                    _ => {
                        let client_id = Self::resolve_view_client(
                            command_source.get_target_client_id(),
                            command_source,
                            session,
                        )?;
                        let tab_id = Self::require_client(session, client_id)?.get_active_tab();
                        Ok(PaneTarget {
                            session_id: session.session_id,
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
    pub(super) fn resolve_tab_id_for_pane(
        session: &Session,
        pane_id: PaneId,
    ) -> Result<TabId, Rejection> {
        session
            .tabs
            .values()
            .find(|tab_state| tab_state.get_layout_tree().contains_pane(pane_id))
            .map(|tab_state| tab_state.get_tab_id())
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))
    }

    /// The pane a tab-addressed `new-pane` splits: the tab's most recently
    /// focused pane, or — until anything in the tab has been focused — its
    /// first pane in layout order. [`RejectReason::TargetNotFound`] when the
    /// tab is gone.
    pub(super) fn resolve_tab_anchor_pane_id(
        session: &Session,
        tab_id: TabId,
    ) -> Result<PaneId, Rejection> {
        let tab_state = session
            .tabs
            .get(&tab_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        if let Some(&pane_id) = tab_state.list_focus_mru().first() {
            return Ok(pane_id);
        }
        tab_state
            .get_layout_tree()
            .list_leaf_pane_ids()
            .first()
            .copied()
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))
    }

    /// Confirm `pane` exists in `session`'s registry. Used for the in-session
    /// CLI default, whose target is the captured `pane_id` (tied to the session,
    /// not to live focus) — never the global registry.
    pub(super) fn resolve_pane_in_session(
        session: &Session,
        pane_id: PaneId,
    ) -> Result<(), Rejection> {
        if session.panes.get_pane_record_by_id(pane_id).is_some() {
            Ok(())
        } else {
            Err(Rejection::from_reason(RejectReason::TargetNotFound))
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
        pane_id: PaneId,
    ) -> Result<(), Rejection> {
        if session.panes.get_pane_record_by_id(pane_id).is_none() {
            return Err(Rejection::from_reason(RejectReason::TargetNotFound));
        }
        let client_record = Self::require_client(session, client_id)?;
        let tab_state = session
            .tabs
            .get(&client_record.get_active_tab())
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        if tab_state.get_layout_tree().contains_pane(pane_id) {
            Ok(())
        } else {
            Err(Rejection::from_reason_and_help(
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
        command_args: &FocusPaneArgs,
        command_source: &CommandSource,
        session: Option<&Session>,
        pane_sizing: PaneSizing,
    ) -> Result<ClientPaneTarget, Rejection> {
        let session = Self::require_session(session)?;
        let client_id = Self::resolve_view_client(command_args.client_id, command_source, session)?;
        let pane_id = match command_args.focus_target {
            FocusTarget::Pane(pane_id) => pane_id,
            FocusTarget::Direction(direction) => {
                let source_pane_id = Self::resolve_focused_pane(session, client_id)?;
                Self::directional_neighbor(
                    session,
                    client_id,
                    source_pane_id,
                    direction,
                    pane_sizing,
                )?
            }
        };
        Self::require_pane_in_active_tab(session, client_id, pane_id)?;
        let tab_id = Self::require_client(session, client_id)?.get_active_tab();
        Ok(ClientPaneTarget {
            session_id: session.session_id,
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
        source_pane_id: PaneId,
        direction: Direction,
        pane_sizing: PaneSizing,
    ) -> Result<PaneId, Rejection> {
        let client_record = Self::require_client(session, client_id)?;
        let tab_id = client_record.get_active_tab();
        let tab_record = session
            .tabs
            .get(&tab_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        let tab_viewport_size = session
            .get_tab_viewport(tab_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::InvalidState))?;
        // Directional focus moves within what THIS client sees, so the tab is
        // solved in this client's own mode: a zoomed client draws one pane and
        // has no neighbour to move to.
        let solved_layout = crate::runtime::snapshot::solve_tab_layout(
            tab_record,
            client_record.get_layout_mode(tab_id),
            tab_viewport_size,
            pane_sizing,
        );
        let source_pane_rect = solved_layout
            .pane_rects
            .iter()
            .find(|(pane_id, _)| *pane_id == source_pane_id)
            .map(|(_, pane_rect)| *pane_rect)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;

        let mut best_neighbor: Option<(PaneId, u16, u16)> = None;
        for &(pane_id, pane_rect) in &solved_layout.pane_rects {
            if pane_id == source_pane_id
                || solved_layout.suppressed_pane_ids.contains(&pane_id)
                || pane_rect.is_empty()
            {
                continue;
            }
            // Distance between the facing edges; `None` when the candidate is
            // not on the far side.
            let edge_distance = match direction {
                Direction::Left => (pane_rect.origin.column + pane_rect.cell_size.column_count
                    <= source_pane_rect.origin.column)
                    .then(|| {
                        source_pane_rect.origin.column
                            - (pane_rect.origin.column + pane_rect.cell_size.column_count)
                    }),
                Direction::Right => (pane_rect.origin.column
                    >= source_pane_rect.origin.column + source_pane_rect.cell_size.column_count)
                    .then(|| {
                        pane_rect.origin.column
                            - (source_pane_rect.origin.column
                                + source_pane_rect.cell_size.column_count)
                    }),
                Direction::Up => (pane_rect.origin.row + pane_rect.cell_size.row_count
                    <= source_pane_rect.origin.row)
                    .then(|| {
                        source_pane_rect.origin.row
                            - (pane_rect.origin.row + pane_rect.cell_size.row_count)
                    }),
                Direction::Down => (pane_rect.origin.row
                    >= source_pane_rect.origin.row + source_pane_rect.cell_size.row_count)
                    .then(|| {
                        pane_rect.origin.row
                            - (source_pane_rect.origin.row + source_pane_rect.cell_size.row_count)
                    }),
            };
            let Some(edge_distance) = edge_distance else {
                continue;
            };
            let perpendicular_overlap = match direction {
                Direction::Left | Direction::Right => compute_span_overlap(
                    source_pane_rect.origin.row,
                    source_pane_rect.cell_size.row_count,
                    pane_rect.origin.row,
                    pane_rect.cell_size.row_count,
                ),
                Direction::Up | Direction::Down => compute_span_overlap(
                    source_pane_rect.origin.column,
                    source_pane_rect.cell_size.column_count,
                    pane_rect.origin.column,
                    pane_rect.cell_size.column_count,
                ),
            };
            if perpendicular_overlap == 0 {
                continue;
            }
            let is_better_neighbor =
                best_neighbor.is_none_or(|(_, best_edge_distance, best_perpendicular_overlap)| {
                    edge_distance < best_edge_distance
                        || (edge_distance == best_edge_distance
                            && perpendicular_overlap > best_perpendicular_overlap)
                });
            if is_better_neighbor {
                best_neighbor = Some((pane_id, edge_distance, perpendicular_overlap));
            }
        }
        best_neighbor.map(|(pane_id, _, _)| pane_id).ok_or_else(|| {
            Rejection::from_reason_and_help(
                RejectReason::TargetNotFound,
                "no pane in that direction",
            )
        })
    }

    /// Resolve the client's focused pane. A client with no focused pane is
    /// [`RejectReason::TargetNotFound`]; a focus pointing outside the active tab
    /// is verified and rejected too, not assumed valid.
    pub(super) fn resolve_focused_pane(
        session: &Session,
        client_id: ClientId,
    ) -> Result<PaneId, Rejection> {
        let client = Self::require_client(session, client_id)?;
        let focused_pane_id = client
            .get_focused_pane(client.get_active_tab())
            .ok_or_else(|| {
                Rejection::from_reason_and_help(RejectReason::TargetNotFound, "no focused pane")
            })?;
        Self::require_pane_in_active_tab(session, client_id, focused_pane_id)?;
        Ok(focused_pane_id)
    }

    /// Resolve an explicit tab target within the acting session, or the default
    /// tab when none is given. An in-session CLI defaults to the tab containing
    /// its command source `pane_id` (the command targets the command source pane's context, even
    /// if the client has since switched tabs); any other command source defaults to the
    /// acting client's live `active_tab` ([`Self::resolve_acting_client`] — the
    /// issuer while attached, else the session's sole attached client). Fails
    /// with [`RejectReason::TargetNotFound`] when there is no session context
    /// or the tab is gone.
    pub(super) fn resolve_tab_or_active(
        &self,
        requested_tab_id: Option<TabId>,
        command_source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<TabId, Rejection> {
        let session = Self::require_session(session)?;
        match requested_tab_id {
            Some(tab_id) => {
                Self::require_tab(session, tab_id)?;
                Ok(tab_id)
            }
            None => {
                if let CommandSource::InSessionCli { pane_id, .. } = command_source {
                    return Self::require_tab_containing_pane(session, *pane_id);
                }
                let client_id = Self::resolve_acting_client(command_source, session)?;
                let client = Self::require_client(session, client_id)?;
                let active_tab_id = client.get_active_tab();
                Self::require_tab(session, active_tab_id)?;
                Ok(active_tab_id)
            }
        }
    }

    /// Find the tab in `session` whose layout contains `pane`, confirming the
    /// pane also has a live registry record.
    pub(super) fn require_tab_containing_pane(
        session: &Session,
        pane_id: PaneId,
    ) -> Result<TabId, Rejection> {
        Self::resolve_pane_in_session(session, pane_id)?;
        Self::resolve_tab_id_for_pane(session, pane_id).map_err(|_| {
            Rejection::from_reason_and_help(
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
        explicit_client_id: Option<ClientId>,
        command_source: &CommandSource,
        session: &Session,
    ) -> Result<ClientId, Rejection> {
        match explicit_client_id {
            Some(client_id) => {
                if session.clients.get_client_by_id(client_id).is_none() {
                    return Err(Rejection::from_reason_and_help(
                        RejectReason::TargetNotFound,
                        "target client not attached to the session",
                    ));
                }
                Ok(client_id)
            }
            None => Self::resolve_acting_client(command_source, session),
        }
    }

    /// Resolve the [`Command::NewTab`] target: the session the tab joins and
    /// the client that switches onto it ([`Self::resolve_view_client`]).
    /// Shared by validation and [`Self::handle_new_tab`] so both apply one
    /// contract.
    pub(super) fn resolve_new_tab_target(
        command_args: &NewTabArgs,
        command_source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<NewTabTarget, Rejection> {
        let session = Self::require_session(session)?;
        let client_id = Self::resolve_view_client(command_args.client_id, command_source, session)?;
        Ok(NewTabTarget {
            session_id: session.session_id,
            client_id,
        })
    }

    /// Resolve the [`Command::TogglePaneFullscreen`] target: the pane the zoom
    /// fills the view with and the client whose own view flips. Shared by
    /// validation and [`Self::handle_toggle_pane_fullscreen`] so both apply one
    /// contract.
    ///
    /// Both halves go through the target client the command source names
    /// ([`CommandSource::get_target_client_id`]), so the pane is the one that client
    /// is looking at. Against a session with clients A and B,
    /// `koshi toggle-pane-fullscreen --client <B>` zooms B's focused pane on
    /// B's screen and leaves A tiled. A named client not attached to the acting
    /// session is [`RejectReason::TargetNotFound`].
    pub(super) fn resolve_fullscreen_target(
        &self,
        command_source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<ClientPaneTarget, Rejection> {
        let pane_target = self.resolve_pane_target(None, command_source, session)?;
        let client_id = Self::resolve_view_client(
            command_source.get_target_client_id(),
            command_source,
            Self::require_session(session)?,
        )?;
        Ok(ClientPaneTarget {
            session_id: pane_target.session_id,
            client_id,
            tab_id: pane_target.tab_id,
            pane_id: pane_target.pane_id,
        })
    }

    /// Resolve the [`Command::FocusTab`] target: the client whose view
    /// switches ([`Self::resolve_view_client`]) and the concrete tab the
    /// target names — an id or index must match an existing tab, and
    /// `next`/`prev` step from the *target* client's active tab, wrapping at
    /// the ends. Shared by validation and [`Self::handle_focus_tab`] so both
    /// apply one contract.
    pub(super) fn resolve_focus_tab_target(
        command_args: &FocusTabArgs,
        command_source: &CommandSource,
        session: Option<&Session>,
    ) -> Result<FocusTabTarget, Rejection> {
        let session = Self::require_session(session)?;
        let client_id = Self::resolve_view_client(command_args.client_id, command_source, session)?;
        let client = Self::require_client(session, client_id)?;
        let tab_target = match command_args.focus_target {
            TabTarget::Id(tab_id) => tab_ops::TabTarget::Id(tab_id),
            TabTarget::Index(tab_index) => tab_ops::TabTarget::Index(tab_index),
            TabTarget::Next => tab_ops::TabTarget::Next,
            TabTarget::Prev => tab_ops::TabTarget::Prev,
        };
        let tab_id = tab_ops::resolve_tab_target(session, client.get_active_tab(), tab_target)
            .ok_or_else(|| Rejection::from_reason(RejectReason::TargetNotFound))?;
        Ok(FocusTabTarget {
            session_id: session.session_id,
            client_id,
            tab_id,
        })
    }

    /// The acting session, or [`RejectReason::TargetNotFound`] when a
    /// session-scoped command has no session context to resolve within.
    pub(super) fn require_session(session: Option<&Session>) -> Result<&Session, Rejection> {
        session.ok_or_else(|| {
            Rejection::from_reason_and_help(RejectReason::TargetNotFound, "no session context")
        })
    }

    /// The client `client_id` names in `session`, or
    /// [`RejectReason::SourceClientStale`] when no client of that id is
    /// attached there.
    pub(super) fn require_client(
        session: &Session,
        client_id: ClientId,
    ) -> Result<&Client, Rejection> {
        session
            .clients
            .get_client_by_id(client_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::SourceClientStale))
    }

    /// Confirm `tab` exists in `session`.
    pub(super) fn require_tab(session: &Session, tab_id: TabId) -> Result<(), Rejection> {
        if session.tabs.contains_key(&tab_id) {
            Ok(())
        } else {
            Err(Rejection::from_reason(RejectReason::TargetNotFound))
        }
    }

    /// Whether `session` is shutting down (`Stopping`/`Stopped`) and so accepts
    /// no mutations.
    pub(super) fn is_winding_down(session: &Session) -> bool {
        matches!(
            session.get_lifecycle(),
            SessionLifecycle::Stopping | SessionLifecycle::Stopped
        )
    }
}
