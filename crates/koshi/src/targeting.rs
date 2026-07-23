//! Which running session a CLI command goes to, and what its `--session`/
//! `--tab` flags mean there.
//!
//! Inside a pane the answer is almost always "this session": the identity
//! from the pane's environment routes the command over the session's own
//! socket, and no other process is consulted. Outside any pane — or when
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
//! picking one.

use koshi_core::discovery::SessionOverview;
use koshi_core::event::RejectReason;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};

use crate::cli::{CliCommand, ResolvedTargets, SessionRef, TabRef};
use crate::error::CliError;
use crate::in_session::InSessionContext;
use crate::ipc_client;

/// Where one invocation goes: over the current pane's own session socket, or
/// to another running session as an external command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Submit over the in-session socket, as the issuing pane.
    InSession(ResolvedTargets),
    /// Submit to `session`'s socket as an external invocation.
    External {
        /// The session the command is sent to.
        session: SessionId,
        /// The command's resolved `--session`/`--tab` flags.
        targets: ResolvedTargets,
    },
}

/// Decide where `command` goes and resolve its `--session`/`--tab` flags to
/// concrete ids.
///
/// With an in-session identity and no `--session` flag (or one naming the
/// current session), the command stays home: only a `--tab` given as a name
/// costs a lookup, answered by the session itself. An explicit `--session`
/// id asks that one session alone; everything else probes the runtime
/// directory's advertised sessions, skipping an endpoint nobody answers.
pub fn route(command: &CliCommand, context: Option<&InSessionContext>) -> Result<Route, CliError> {
    // In-session, targeting home: the command stays on its own socket.
    // Flags given as ids ride into the command as-is (`to_action` reads
    // them); only a `--tab` NAME costs a lookup, answered by the session
    // itself.
    if let Some(context) = context {
        let stays_home = match command.target_session() {
            None => true,
            Some(SessionRef::Id(id)) => *id == context.session_id,
            // A name may or may not be this session's; only a probe can tell.
            Some(SessionRef::Name(_)) => false,
        };
        if stays_home {
            let tab = match command.target_tab() {
                Some(tab_ref @ TabRef::Name(_)) => {
                    let overview = ipc_client::fetch_overview(
                        &ipc_client::runtime_dir()?,
                        context.session_id,
                    )?;
                    Some(resolve_tab(&overview, tab_ref)?)
                }
                _ => None,
            };
            return Ok(Route::InSession(ResolvedTargets { session: None, tab }));
        }
    }

    // An explicit `--session <id>` names its endpoint directly, so only that
    // session is asked. Anything else needs the whole picture — a name, an
    // owner lookup, or the count rule — so every advertised session is
    // probed; one nobody answers is skipped (its stale files are swept by
    // the listing verbs, not here).
    let runtime_dir = ipc_client::runtime_dir()?;
    let overviews: Vec<SessionOverview> = match command.target_session() {
        Some(SessionRef::Id(id)) => vec![ipc_client::fetch_overview(&runtime_dir, *id)?],
        _ => ipc_client::advertised_sessions(&runtime_dir)
            .into_iter()
            .filter_map(|session_id| ipc_client::fetch_overview(&runtime_dir, session_id).ok())
            .collect(),
    };

    let overview = pick_session(
        command.target_session(),
        command.target_pane(),
        command.target_tab(),
        command.target_client(),
        &overviews,
    )?;
    let tab = command
        .target_tab()
        .map(|tab_ref| resolve_tab(overview, tab_ref))
        .transpose()?;
    let session = overview.session.id;
    let targets = ResolvedTargets {
        session: Some(session),
        tab,
    };

    // A probe can land back on the session this CLI runs inside (e.g.
    // `--session` naming it); then the command still travels as the pane's
    // own, keeping the issuing pane as the default target.
    match context {
        Some(context) if context.session_id == session => Ok(Route::InSession(targets)),
        _ => Ok(Route::External { session, targets }),
    }
}

/// Pick the one running session an external command targets. Precedence:
/// the explicit `--session`, else the owner of an explicit `--pane`/`--tab`/
/// `--client`, else the count rule (one running session is the default).
/// Whatever picked it, every explicitly named pane and client must then
/// belong to the picked session — a mismatch refuses rather than retargets.
fn pick_session<'a>(
    session: Option<&SessionRef>,
    pane: Option<PaneId>,
    tab: Option<&TabRef>,
    client: Option<ClientId>,
    overviews: &'a [SessionOverview],
) -> Result<&'a SessionOverview, CliError> {
    let picked = if let Some(session_ref) = session {
        match session_ref {
            SessionRef::Id(id) => overviews
                .iter()
                .find(|overview| overview.session.id == *id)
                .ok_or_else(|| CliError::SessionNotFound {
                    session: id.to_string(),
                })?,
            SessionRef::Name(name) => {
                let mut matches = overviews
                    .iter()
                    .filter(|overview| overview.session.name == *name);
                match (matches.next(), matches.next()) {
                    (Some(only), None) => only,
                    (Some(_), Some(_)) => {
                        return Err(rejected(
                            RejectReason::TargetAmbiguous,
                            format!("several sessions are named `{name}`; use the session id"),
                        ))
                    }
                    (None, _) => {
                        return Err(CliError::SessionNotFound {
                            session: name.clone(),
                        })
                    }
                }
            }
        }
    } else if let Some(pane_id) = pane {
        overviews
            .iter()
            .find(|overview| overview.panes.iter().any(|pane| pane.id == pane_id))
            .ok_or_else(|| {
                rejected(
                    RejectReason::TargetNotFound,
                    format!("pane {pane_id} is not in any running session"),
                )
            })?
    } else if let Some(tab_ref) = tab {
        pick_session_by_tab(tab_ref, overviews)?
    } else if let Some(client_id) = client {
        overviews
            .iter()
            .find(|overview| {
                overview
                    .clients
                    .iter()
                    .any(|attached| attached.id == client_id)
            })
            .ok_or_else(|| {
                rejected(
                    RejectReason::TargetNotFound,
                    format!("client {client_id} is not attached to any running session"),
                )
            })?
    } else {
        let mut running = overviews.iter();
        match (running.next(), running.next()) {
            (Some(only), None) => only,
            (Some(_), Some(_)) => {
                return Err(rejected(
                    RejectReason::TargetAmbiguous,
                    "several sessions are running; name one with --session <name-or-id>"
                        .to_string(),
                ))
            }
            (None, _) => return Err(CliError::NoSessions),
        }
    };

    // An explicit pane or client must live in the picked session, whichever
    // rule picked it.
    if let Some(pane_id) = pane {
        if !picked.panes.iter().any(|pane| pane.id == pane_id) {
            return Err(rejected(
                RejectReason::TargetNotFound,
                format!("pane {pane_id} is not in session `{}`", picked.session.name),
            ));
        }
    }
    if let Some(client_id) = client {
        if !picked
            .clients
            .iter()
            .any(|attached| attached.id == client_id)
        {
            return Err(rejected(
                RejectReason::TargetNotFound,
                format!(
                    "client {client_id} is not attached to session `{}`",
                    picked.session.name
                ),
            ));
        }
    }
    Ok(picked)
}

/// The session owning an explicitly named tab: by id, the one session whose
/// tab list holds it; by name, the name must match exactly one tab across
/// every running session — two sessions with a same-named tab demand the tab
/// id or `--session`.
fn pick_session_by_tab<'a>(
    tab_ref: &TabRef,
    overviews: &'a [SessionOverview],
) -> Result<&'a SessionOverview, CliError> {
    match tab_ref {
        TabRef::Id(tab_id) => overviews
            .iter()
            .find(|overview| overview.tabs.iter().any(|tab| tab.id == *tab_id))
            .ok_or_else(|| {
                rejected(
                    RejectReason::TargetNotFound,
                    format!("tab {tab_id} is not in any running session"),
                )
            }),
        TabRef::Name(name) => {
            let mut owners = overviews
                .iter()
                .filter(|overview| overview.tabs.iter().any(|tab| tab.name == *name));
            match (owners.next(), owners.next()) {
                (Some(only), None) => Ok(only),
                (Some(_), Some(_)) => Err(rejected(
                    RejectReason::TargetAmbiguous,
                    format!(
                        "several sessions have a tab named `{name}`; use the tab id or --session"
                    ),
                )),
                (None, _) => Err(rejected(
                    RejectReason::TargetNotFound,
                    format!("no running session has a tab named `{name}`"),
                )),
            }
        }
    }
}

/// Resolve a `--tab` flag within the target session: an id must be one of
/// the session's tabs, and a name must match exactly one of them.
fn resolve_tab(overview: &SessionOverview, tab_ref: &TabRef) -> Result<TabId, CliError> {
    match tab_ref {
        TabRef::Id(tab_id) => {
            if overview.tabs.iter().any(|tab| tab.id == *tab_id) {
                Ok(*tab_id)
            } else {
                Err(rejected(
                    RejectReason::TargetNotFound,
                    format!("tab {tab_id} is not in session `{}`", overview.session.name),
                ))
            }
        }
        TabRef::Name(name) => {
            let mut matches = overview.tabs.iter().filter(|tab| tab.name == *name);
            match (matches.next(), matches.next()) {
                (Some(only), None) => Ok(only.id),
                (Some(_), Some(_)) => Err(rejected(
                    RejectReason::TargetAmbiguous,
                    format!(
                        "several tabs are named `{name}` in session `{}`; use the tab id",
                        overview.session.name
                    ),
                )),
                (None, _) => Err(rejected(
                    RejectReason::TargetNotFound,
                    format!(
                        "no tab named `{name}` in session `{}`",
                        overview.session.name
                    ),
                )),
            }
        }
    }
}

/// A routing refusal, shaped like a session's own rejection so the reason
/// and hint print the same way and exit with the same code.
fn rejected(reason: RejectReason, help: String) -> CliError {
    CliError::CommandRejected {
        reason,
        help: Some(help),
    }
}

#[cfg(test)]
mod tests;
