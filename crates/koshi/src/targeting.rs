//! Which running session a CLI command goes to, and what its `--session`/
//! `--tab` flags mean there.
//!
//! Inside a pane the answer is almost always "this session": the identity
//! from the pane's environment routes the command over the session's own
//! socket, and no other process is consulted. A `--client` on a verb whose
//! command carries no client field leaves the in-session path, which carries
//! no target client. Example —
//! `koshi toggle-pane-fullscreen --client client-<uuid>` typed inside a pane
//! is answered by the session that client is attached to, which may be this
//! pane's own. Outside any pane — or when
//! `--session` names a different session — the routing layer asks the named
//! session directly (an explicit `--session <id>`), or reads every endpoint
//! file in the runtime directory and asks each live session to describe
//! itself, and picks the target deterministically:
//!
//! - an explicit `--session` must match exactly one running session, by id
//!   or by name;
//! - otherwise an explicit `--pane`, `--tab`, or `--client` picks the
//!   session that owns it;
//! - otherwise the count rule applies: exactly one session running is the
//!   default, several demand `--session`, none is an error.
//!
//! Ambiguity is always an error, never a guess: two sessions sharing a name,
//! or two running sessions with no flag, both refuse with a hint instead of
//! picking one. An ambiguous name names every id that matched it, so the
//! refusal itself carries the ids to retry with.
//!
//! The probing itself is [`koshi_link::discovery`]'s, the same code the listing
//! verbs use, so a session that is gone is swept here too.
//!
//! A `--remote` flag swaps out where the census comes from and nothing else:
//! the sessions on the named machine stand in for this machine's, and the same
//! precedence, the same count rule and the same refusals run over them.

use std::path::Path;

use koshi_core::command::CommandResult;
use koshi_core::discovery::SessionOverview;
use koshi_core::event::RejectReason;
use koshi_core::geometry::Direction;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_ipc::remote_wire::RemoteSessionRow;

use crate::cli::{CliCommand, ResolvedTargets, SessionReference, TabReference};
use koshi_link::discovery::{self, Discovered};
use koshi_link::error::CliError;
use koshi_link::in_session::InSessionContext;
use koshi_link::ipc_client;
use koshi_link::remote_client::{self, ServerReference};

/// Where one invocation goes: over the current pane's own session socket, or
/// to another running session as an external command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Submit over the in-session socket, as the issuing pane.
    InSession(ResolvedTargets),
    /// Submit to `session_id`'s socket as an external invocation.
    External {
        /// The session the command is sent to.
        session_id: SessionId,
        /// The command's resolved `--session`/`--tab` flags.
        targets: ResolvedTargets,
    },
}

/// Resolve where `command` goes and resolve its `--session`/`--tab` flags to
/// concrete ids.
///
/// With an in-session identity and no `--session` flag (or one naming the
/// current session), the command stays home: only a `--tab` given as a name
/// costs a lookup, answered by the session itself. An explicit `--session`
/// id asks that one session alone; everything else probes the runtime
/// directory's advertised sessions, skipping an endpoint nobody answers.
pub fn resolve_command_route(
    command: &CliCommand,
    in_session_context: Option<&InSessionContext>,
) -> Result<Route, CliError> {
    // The in-session route carries no target client. A command whose client
    // rides on its source never takes that route — not even back to this
    // pane's own session.
    let has_source_client = command.get_source_client_id().is_some();

    // In-session, targeting home: the command stays on its own socket.
    // Flags given as ids ride into the command as-is (`build_action_command` reads
    // them); only a `--tab` NAME costs a lookup, answered by the session
    // itself.
    if let Some(in_session_context) = in_session_context {
        let is_staying_home = !has_source_client
            && match command.get_target_session_reference() {
                None => true,
                Some(SessionReference::SessionId(session_id)) => {
                    *session_id == in_session_context.session_id
                }
                // A name may or may not be this session's; only a probe can tell.
                Some(SessionReference::SessionName(_)) => false,
            };
        if is_staying_home {
            let target_tab_id = match command.get_target_tab_reference() {
                Some(tab_reference @ TabReference::TabName(_)) => {
                    let session_overview = discovery::fetch_session_overview(
                        &ipc_client::resolve_runtime_directory()?,
                        in_session_context.session_id,
                    )?;
                    Some(resolve_target_tab(&session_overview, tab_reference)?)
                }
                _ => None,
            };
            return Ok(Route::InSession(ResolvedTargets {
                session_id: None,
                tab_id: target_tab_id,
            }));
        }
    }

    // An explicit `--session <id>` names its endpoint directly, so only that
    // session is asked. Anything else needs the whole picture — a name, an
    // owner lookup, or the count rule — so every advertised session is
    // probed; one nobody answers is skipped and its leftovers swept.
    let runtime_directory = ipc_client::resolve_runtime_directory()?;
    let discovered_sessions = match command.get_target_session_reference() {
        Some(SessionReference::SessionId(session_id)) => Discovered::from_overview(
            discovery::fetch_session_overview(&runtime_directory, *session_id)?,
        ),
        _ => discovery::fetch_all_session_overviews(&runtime_directory),
    };

    let (target_session_id, resolved_targets) =
        resolve_command_targets(command, &discovered_sessions)?;

    // A probe can land back on the session this CLI runs inside (e.g.
    // `--session` naming it); then the command still travels as the pane's
    // own, keeping the issuing pane as the default target.
    match in_session_context {
        Some(in_session_context)
            if in_session_context.session_id == target_session_id && !has_source_client =>
        {
            Ok(Route::InSession(resolved_targets))
        }
        _ => Ok(Route::External {
            session_id: target_session_id,
            targets: resolved_targets,
        }),
    }
}

/// Send `command` to the machine named by `server_reference` and hand back the
/// dispatcher's result.
///
/// `server_reference` is the name this machine saved that server under, or the
/// `host:port` it listens on. The sessions that server's secret reaches stand
/// in for this machine's census, so the explicit `--session`/`--tab`/`--pane`/
/// `--client` flags and the count rule pick the target there exactly as they
/// do here. Nothing on this side of the connection is consulted: an identity
/// in the pane environment names a session on this machine, which the named
/// machine knows nothing about.
///
/// Nothing here creates a session. A server whose secret reaches no running
/// session refuses with [`CliError::NoSessions`], the same refusal an external
/// command gets on a machine with nothing running.
///
/// `new_pane_direction` is this CLI's own `layout.new-pane-direction` setting,
/// put on a pane-opening verb that was given no `--direction`.
pub fn submit_remote(
    server_reference: &str,
    command: &CliCommand,
    new_pane_direction: Direction,
) -> Result<CommandResult, CliError> {
    let saved_server = remote_client::resolve_server(server_reference)?;
    // This connection saves the record the dials below present.
    let (mut remote_link, saved_server_record) = remote_client::connect_saved_server(
        &saved_server,
        None,
        Some(remote_client::REPLY_TIMEOUT_DURATION),
    )?;
    let remote_session_rows = remote_client::list_remote_sessions(&mut remote_link)?;
    // The dials below open their own connections.
    drop(remote_link);

    let server_reference = ServerReference::Saved(saved_server_record);
    let discovered_sessions = discover_remote_sessions(
        &server_reference,
        select_remote_rows_to_probe(command.get_target_session_reference(), remote_session_rows),
    );

    let (target_session_id, resolved_targets) =
        resolve_command_targets(command, &discovered_sessions)?;

    let (_, action) = command
        .build_action_command(&resolved_targets, new_pane_direction)
        .expect("only an action verb reaches the remote dispatch");
    remote_client::submit_remote_command(
        &server_reference,
        target_session_id,
        command.get_source_client_id(),
        action,
    )
}

/// Which remote session records the census must ask, given the `--session` flag.
///
/// [`SessionReference::SessionId`] keeps the one record carrying that session id, and no
/// records when none does. Every other selector — a name, a pane, a tab, a
/// client, or no flag at all — keeps every record.
///
/// Example — with `--session session-<uuid>` against a server listing eight
/// sessions, one record comes back, so [`discover_remote_sessions`] makes one
/// dial.
fn select_remote_rows_to_probe(
    session_reference: Option<&SessionReference>,
    remote_session_rows: Vec<RemoteSessionRow>,
) -> Vec<RemoteSessionRow> {
    match session_reference {
        Some(SessionReference::SessionId(session_id)) => remote_session_rows
            .into_iter()
            .filter(|remote_session_row| remote_session_row.session_id == *session_id)
            .collect(),
        _ => remote_session_rows,
    }
}

/// Ask each session in `remote_session_rows` on the machine `server_reference`
/// names to describe itself,
/// as the census the targeting rules read.
///
/// One dial per record. Callers narrow `remote_session_rows` with
/// [`select_remote_rows_to_probe`] first.
///
/// A session the server listed but could not describe is named on stderr and
/// counted in `Discovered::unasked_session_count`. The sessions that answered are sorted by
/// name, then by id.
fn discover_remote_sessions(
    server_reference: &ServerReference,
    remote_session_rows: Vec<RemoteSessionRow>,
) -> Discovered {
    let mut discovered_sessions = Discovered::default();
    for remote_session_row in remote_session_rows {
        match remote_client::fetch_remote_overview(server_reference, remote_session_row.session_id)
        {
            Ok(session_overview) => discovered_sessions.sessions.push(session_overview),
            Err(remote_error) => {
                eprintln!(
                    "koshi: session {} did not answer: {remote_error}",
                    remote_session_row.session_id
                );
                discovered_sessions.unasked_session_count += 1;
            }
        }
    }
    discovered_sessions.sort_sessions();
    discovered_sessions
}

/// The session `command` targets over the discovered sessions, and its
/// `--session`/`--tab` flags resolved to ids.
///
/// [`select_target_session`] picks the session; the resolved `session_id` field
/// always carries that session's id, and `tab_id` carries the id a `--tab` flag
/// names within it, or `None` when the flag is absent.
fn resolve_command_targets(
    command: &CliCommand,
    discovered_sessions: &Discovered,
) -> Result<(SessionId, ResolvedTargets), CliError> {
    let session_overview = select_target_session(
        command.get_target_session_reference(),
        command.get_target_pane_id(),
        command.get_target_tab_reference(),
        command.get_target_client_id(),
        discovered_sessions,
    )?;
    let target_tab_id = command
        .get_target_tab_reference()
        .map(|tab_reference| resolve_target_tab(session_overview, tab_reference))
        .transpose()?;
    let target_session_id = session_overview.session.session_id;
    Ok((
        target_session_id,
        ResolvedTargets {
            session_id: Some(target_session_id),
            tab_id: target_tab_id,
        },
    ))
}

/// The one running session named `session_name`, over the discovered sessions.
///
/// # Errors
/// [`CliError::SessionNotFound`] when no session that answered carries the
/// session name; [`CliError::IpcUnavailable`] when exactly one carries it and a
/// running session did not answer, so "exactly one" cannot be told;
/// [`CliError::CommandRejected`] with [`RejectReason::TargetAmbiguous`],
/// naming every matching id, when several carry it.
pub(crate) fn find_session_by_name<'a>(
    discovered_sessions: &'a Discovered,
    session_name: &str,
) -> Result<&'a SessionOverview, CliError> {
    let matching_session_overviews: Vec<&SessionOverview> = discovered_sessions
        .sessions
        .iter()
        .filter(|session_overview| session_overview.session.session_name == session_name)
        .collect();
    match matching_session_overviews.as_slice() {
        [] => Err(discovered_sessions.build_missing_session_error(session_name)),
        [only_session_overview] if discovered_sessions.is_complete() => Ok(only_session_overview),
        [_] => Err(discovered_sessions
            .build_unanswered_error(&format!("cannot tell whether `{session_name}` is unique"))),
        multiple_session_overviews => {
            let matching_session_ids = multiple_session_overviews
                .iter()
                .map(|session_overview| session_overview.session.session_id.to_string())
                .collect::<Vec<String>>()
                .join(", ");
            Err(build_target_rejection(
                RejectReason::TargetAmbiguous,
                format!(
                    "several sessions are named `{session_name}`: {matching_session_ids}; use the session id"
                ),
            ))
        }
    }
}

/// The count rule over the discovered sessions: the sole running session, when
/// every running session answered.
///
/// # Errors
/// [`CliError::CommandRejected`] with [`RejectReason::TargetAmbiguous`]
/// carrying the ambiguous-session message when more than one session answered;
/// [`CliError::NoSessions`] when none is running;
/// [`CliError::IpcUnavailable`] carrying the incomplete-discovery message when a running session
/// did not answer, so "exactly one" cannot be told.
pub(crate) fn select_only_session<'a>(
    discovered_sessions: &'a Discovered,
    ambiguous_sessions_message: &str,
    incomplete_discovery_message: &str,
) -> Result<&'a SessionOverview, CliError> {
    let mut running_session_overviews = discovered_sessions.sessions.iter();
    match (
        running_session_overviews.next(),
        running_session_overviews.next(),
    ) {
        (Some(_), Some(_)) => Err(build_target_rejection(
            RejectReason::TargetAmbiguous,
            ambiguous_sessions_message.to_string(),
        )),
        (Some(only_session_overview), None) if discovered_sessions.is_complete() => {
            Ok(only_session_overview)
        }
        (None, _) if discovered_sessions.is_complete() => Err(CliError::NoSessions),
        _ => Err(discovered_sessions.build_unanswered_error(incomplete_discovery_message)),
    }
}

/// Pick the one running session an external command targets. Precedence:
/// the explicit `--session`, else the owner of an explicit `--pane`/`--tab`/
/// `--client`, else the count rule (one running session is the default).
/// Whatever picked it, every explicitly named pane and client must then
/// belong to the picked session — a mismatch refuses rather than retargets.
///
/// Every branch that would answer "nowhere", "there is only this one", or
/// "exactly one has this name" needs a complete census: with a running
/// session unasked ([`Discovered::is_complete`]), the command is refused
/// rather than aimed at whichever session did answer.
fn select_target_session<'a>(
    session_reference: Option<&SessionReference>,
    target_pane_id: Option<PaneId>,
    target_tab_reference: Option<&TabReference>,
    target_client_id: Option<ClientId>,
    discovered_sessions: &'a Discovered,
) -> Result<&'a SessionOverview, CliError> {
    let session_overviews = discovered_sessions.sessions.as_slice();
    let selected_session_overview = if let Some(session_reference) = session_reference {
        match session_reference {
            SessionReference::SessionId(session_id) => session_overviews
                .iter()
                .find(|session_overview| session_overview.session.session_id == *session_id)
                .ok_or_else(|| {
                    discovered_sessions.build_missing_session_error(&session_id.to_string())
                })?,
            SessionReference::SessionName(session_name) => {
                find_session_by_name(discovered_sessions, session_name)?
            }
        }
    } else if let Some(pane_id) = target_pane_id {
        session_overviews
            .iter()
            .find(|session_overview| {
                session_overview
                    .panes
                    .iter()
                    .any(|pane_record| pane_record.pane_id == pane_id)
            })
            .ok_or_else(|| {
                discovered_sessions.build_missing_target_error("pane", &pane_id.to_string())
            })?
    } else if let Some(tab_reference) = target_tab_reference {
        select_session_for_tab(tab_reference, discovered_sessions)?
    } else if let Some(client_id) = target_client_id {
        session_overviews
            .iter()
            .find(|session_overview| {
                session_overview
                    .clients
                    .iter()
                    .any(|attached_client| attached_client.client_id == client_id)
            })
            .ok_or_else(|| {
                discovered_sessions.build_missing_target_error("client", &client_id.to_string())
            })?
    } else {
        select_only_session(
            discovered_sessions,
            "several sessions are running; name one with --session <name-or-id>",
            "cannot tell which session to target; name one with --session <name-or-id>",
        )?
    };

    // An explicit pane or client must live in the picked session, whichever
    // rule picked it.
    if let Some(pane_id) = target_pane_id {
        if !selected_session_overview
            .panes
            .iter()
            .any(|pane_record| pane_record.pane_id == pane_id)
        {
            return Err(build_target_rejection(
                RejectReason::TargetNotFound,
                format!(
                    "pane {pane_id} is not in session `{}`",
                    selected_session_overview.session.session_name
                ),
            ));
        }
    }
    if let Some(client_id) = target_client_id {
        if !selected_session_overview
            .clients
            .iter()
            .any(|attached_client| attached_client.client_id == client_id)
        {
            return Err(build_target_rejection(
                RejectReason::TargetNotFound,
                format!(
                    "client {client_id} is not attached to session `{}`",
                    selected_session_overview.session.session_name
                ),
            ));
        }
    }
    Ok(selected_session_overview)
}

/// The session owning an explicitly named tab: by id, the one session whose
/// tab list holds it; by name, the name must match exactly one tab across
/// every running session — matches spanning several sessions demand the tab
/// id or `--session`, matches all in one session resolve to that session
/// (the duplicate-tab refusal is [`resolve_target_tab`]'s), and a sole match counts
/// only when every running session answered.
fn select_session_for_tab<'a>(
    tab_reference: &TabReference,
    discovered_sessions: &'a Discovered,
) -> Result<&'a SessionOverview, CliError> {
    match tab_reference {
        TabReference::TabId(tab_id) => discovered_sessions
            .sessions
            .iter()
            .find(|session_overview| {
                session_overview
                    .tabs
                    .iter()
                    .any(|tab_record| tab_record.tab_id == *tab_id)
            })
            .ok_or_else(|| {
                discovered_sessions.build_missing_target_error("tab", &tab_id.to_string())
            }),
        TabReference::TabName(tab_name) => {
            let matching_tab_locations: Vec<(&SessionOverview, TabId)> = discovered_sessions
                .sessions
                .iter()
                .flat_map(|session_overview| {
                    session_overview
                        .tabs
                        .iter()
                        .filter(|tab_record| tab_record.tab_name == *tab_name)
                        .map(move |tab_record| (session_overview, tab_record.tab_id))
                })
                .collect();
            match matching_tab_locations.as_slice() {
                [(only_session_overview, _)] if discovered_sessions.is_complete() => {
                    Ok(*only_session_overview)
                }
                [_] => Err(discovered_sessions.build_unanswered_error(&format!(
                    "cannot tell whether tab `{tab_name}` is unique"
                ))),
                [] => Err(discovered_sessions
                    .build_missing_target_error("tab named", &format!("`{tab_name}`"))),
                multiple_tab_locations => {
                    let has_one_session_owner =
                        multiple_tab_locations.windows(2).all(|tab_location_pair| {
                            tab_location_pair[0].0.session.session_id
                                == tab_location_pair[1].0.session.session_id
                        });
                    if has_one_session_owner {
                        // One session owns every match, so the session answer
                        // is that session; the duplicate-tab refusal, with the
                        // ids, is resolve_target_tab's.
                        return Ok(multiple_tab_locations[0].0);
                    }
                    let tab_locations = multiple_tab_locations
                        .iter()
                        .map(|(session_overview, tab_id)| {
                            format!(
                                "{tab_id} in session `{}`",
                                session_overview.session.session_name
                            )
                        })
                        .collect::<Vec<String>>()
                        .join(", ");
                    Err(build_target_rejection(
                        RejectReason::TargetAmbiguous,
                        format!(
                            "several tabs are named `{tab_name}`: {tab_locations}; use the tab id or --session"
                        ),
                    ))
                }
            }
        }
    }
}

/// Resolve a `--tab` flag within the target session: an id must be one of
/// the session's tabs, and a name must match exactly one of them.
fn resolve_target_tab(
    session_overview: &SessionOverview,
    tab_reference: &TabReference,
) -> Result<TabId, CliError> {
    match tab_reference {
        TabReference::TabId(tab_id) => {
            if session_overview
                .tabs
                .iter()
                .any(|tab_record| tab_record.tab_id == *tab_id)
            {
                Ok(*tab_id)
            } else {
                Err(build_target_rejection(
                    RejectReason::TargetNotFound,
                    format!(
                        "tab {tab_id} is not in session `{}`",
                        session_overview.session.session_name
                    ),
                ))
            }
        }
        TabReference::TabName(tab_name) => {
            let matching_tab_ids: Vec<TabId> = session_overview
                .tabs
                .iter()
                .filter(|tab_record| tab_record.tab_name == *tab_name)
                .map(|tab_record| tab_record.tab_id)
                .collect();
            match matching_tab_ids.as_slice() {
                [only_tab_id] => Ok(*only_tab_id),
                [] => Err(build_target_rejection(
                    RejectReason::TargetNotFound,
                    format!(
                        "no tab named `{tab_name}` in session `{}`",
                        session_overview.session.session_name
                    ),
                )),
                multiple_tab_ids => {
                    let matching_tab_ids_text = multiple_tab_ids
                        .iter()
                        .map(|tab_id| tab_id.to_string())
                        .collect::<Vec<String>>()
                        .join(", ");
                    Err(build_target_rejection(
                        RejectReason::TargetAmbiguous,
                        format!(
                            "several tabs are named `{tab_name}` in session `{}`: {matching_tab_ids_text}; use the tab id",
                            session_overview.session.session_name
                        ),
                    ))
                }
            }
        }
    }
}

/// The sessions a `--session` flag puts in scope: an id asks that one
/// session alone, a name is looked up over a full census and scopes to the
/// one session it matches, and an absent flag scopes to every session that
/// answered.
pub fn resolve_session_scope(
    runtime_directory: &Path,
    session_reference: Option<&SessionReference>,
) -> Result<Discovered, CliError> {
    match session_reference {
        None => Ok(discovery::fetch_all_session_overviews(runtime_directory)),
        Some(SessionReference::SessionId(session_id)) => Ok(Discovered::from_overview(
            discovery::fetch_session_overview(runtime_directory, *session_id)?,
        )),
        Some(session_reference) => {
            let discovered_sessions = discovery::fetch_all_session_overviews(runtime_directory);
            let selected_session_overview = select_target_session(
                Some(session_reference),
                None,
                None,
                None,
                &discovered_sessions,
            )?;
            Ok(Discovered::from_overview(selected_session_overview.clone()))
        }
    }
}

/// The tab a `--tab` flag names, over the sessions in scope: an id passes
/// straight through with no lookup, and a name must match exactly one tab of
/// exactly one session.
pub fn resolve_tab_reference(
    discovered_sessions: &Discovered,
    tab_reference: &TabReference,
) -> Result<TabId, CliError> {
    match tab_reference {
        TabReference::TabId(tab_id) => Ok(*tab_id),
        TabReference::TabName(_) => resolve_target_tab(
            select_session_for_tab(tab_reference, discovered_sessions)?,
            tab_reference,
        ),
    }
}

/// A routing refusal, carrying `reason` and `help` as
/// [`CliError::CommandRejected`].
fn build_target_rejection(reason: RejectReason, help: String) -> CliError {
    CliError::CommandRejected {
        reason,
        help: Some(help),
    }
}

#[cfg(test)]
mod tests;
