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
//! picking one. An ambiguous name names every id that matched it, so the
//! refusal itself carries the ids to retry with.
//!
//! The probing itself is [`koshi_link::discovery`]'s, the same code the listing
//! verbs use, so a session that is gone is swept here too.

use std::path::Path;

use koshi_core::discovery::SessionOverview;
use koshi_core::event::RejectReason;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};

use crate::cli::{CliCommand, ResolvedTargets, SessionRef, TabRef};
use koshi_link::discovery::{self, Discovered};
use koshi_link::error::CliError;
use koshi_link::in_session::InSessionContext;
use koshi_link::ipc_client;

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
                    let overview =
                        discovery::fetch_one(&ipc_client::runtime_dir()?, context.session_id)?;
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
    // probed; one nobody answers is skipped and its leftovers swept.
    let runtime_dir = ipc_client::runtime_dir()?;
    let found = match command.target_session() {
        Some(SessionRef::Id(id)) => Discovered::of(discovery::fetch_one(&runtime_dir, *id)?),
        _ => discovery::fetch_all(&runtime_dir),
    };

    let overview = pick_session(
        command.target_session(),
        command.target_pane(),
        command.target_tab(),
        command.target_client(),
        &found,
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
///
/// Every branch that would answer "nowhere", "there is only this one", or
/// "exactly one has this name" needs a complete census: with a running
/// session unasked ([`Discovered::is_complete`]), the command is refused
/// rather than aimed at whichever session did answer.
fn pick_session<'a>(
    session: Option<&SessionRef>,
    pane: Option<PaneId>,
    tab: Option<&TabRef>,
    client: Option<ClientId>,
    found: &'a Discovered,
) -> Result<&'a SessionOverview, CliError> {
    let overviews = found.sessions.as_slice();
    let picked = if let Some(session_ref) = session {
        match session_ref {
            SessionRef::Id(id) => overviews
                .iter()
                .find(|overview| overview.session.id == *id)
                .ok_or_else(|| found.no_such_session(&id.to_string()))?,
            SessionRef::Name(name) => {
                let matches: Vec<&SessionOverview> = overviews
                    .iter()
                    .filter(|overview| overview.session.name == *name)
                    .collect();
                match matches.as_slice() {
                    [only] if found.is_complete() => *only,
                    [_] => {
                        return Err(
                            found.unanswered(&format!("cannot tell whether `{name}` is unique"))
                        );
                    }
                    [] => return Err(found.no_such_session(name)),
                    several => {
                        let ids = several
                            .iter()
                            .map(|overview| overview.session.id.to_string())
                            .collect::<Vec<String>>()
                            .join(", ");
                        return Err(rejected(
                            RejectReason::TargetAmbiguous,
                            format!(
                                "several sessions are named `{name}`: {ids}; use the session id"
                            ),
                        ));
                    }
                }
            }
        }
    } else if let Some(pane_id) = pane {
        overviews
            .iter()
            .find(|overview| overview.panes.iter().any(|pane| pane.id == pane_id))
            .ok_or_else(|| found.missing("pane", &pane_id.to_string()))?
    } else if let Some(tab_ref) = tab {
        pick_session_by_tab(tab_ref, found)?
    } else if let Some(client_id) = client {
        overviews
            .iter()
            .find(|overview| {
                overview
                    .clients
                    .iter()
                    .any(|attached| attached.id == client_id)
            })
            .ok_or_else(|| found.missing("client", &client_id.to_string()))?
    } else {
        let mut running = overviews.iter();
        match (running.next(), running.next()) {
            (Some(_), Some(_)) => {
                return Err(rejected(
                    RejectReason::TargetAmbiguous,
                    "several sessions are running; name one with --session <name-or-id>"
                        .to_string(),
                ))
            }
            // The count rule only holds over a complete census: one session
            // answering while another stayed silent is not "exactly one".
            (Some(only), None) if found.is_complete() => only,
            (None, _) if found.is_complete() => return Err(CliError::NoSessions),
            _ => {
                return Err(found.unanswered(
                    "cannot tell which session to target; name one with --session <name-or-id>",
                ))
            }
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
/// every running session — matches spanning several sessions demand the tab
/// id or `--session`, matches all in one session resolve to that session
/// (the duplicate-tab refusal is [`resolve_tab`]'s), and a sole match counts
/// only when every running session answered.
fn pick_session_by_tab<'a>(
    tab_ref: &TabRef,
    found: &'a Discovered,
) -> Result<&'a SessionOverview, CliError> {
    match tab_ref {
        TabRef::Id(tab_id) => found
            .sessions
            .iter()
            .find(|overview| overview.tabs.iter().any(|tab| tab.id == *tab_id))
            .ok_or_else(|| found.missing("tab", &tab_id.to_string())),
        TabRef::Name(name) => {
            let matches: Vec<(&SessionOverview, TabId)> = found
                .sessions
                .iter()
                .flat_map(|overview| {
                    overview
                        .tabs
                        .iter()
                        .filter(|tab| tab.name == *name)
                        .map(move |tab| (overview, tab.id))
                })
                .collect();
            match matches.as_slice() {
                [(only, _)] if found.is_complete() => Ok(*only),
                [_] => {
                    Err(found.unanswered(&format!("cannot tell whether tab `{name}` is unique")))
                }
                [] => Err(found.missing("tab named", &format!("`{name}`"))),
                several => {
                    let one_owner = several
                        .windows(2)
                        .all(|pair| pair[0].0.session.id == pair[1].0.session.id);
                    if one_owner {
                        // One session owns every match, so the session answer
                        // is that session; the duplicate-tab refusal, with the
                        // ids, is resolve_tab's.
                        return Ok(several[0].0);
                    }
                    let places = several
                        .iter()
                        .map(|(overview, tab_id)| {
                            format!("{tab_id} in session `{}`", overview.session.name)
                        })
                        .collect::<Vec<String>>()
                        .join(", ");
                    Err(rejected(
                        RejectReason::TargetAmbiguous,
                        format!(
                            "several tabs are named `{name}`: {places}; use the tab id or --session"
                        ),
                    ))
                }
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
            let matches: Vec<TabId> = overview
                .tabs
                .iter()
                .filter(|tab| tab.name == *name)
                .map(|tab| tab.id)
                .collect();
            match matches.as_slice() {
                [only] => Ok(*only),
                [] => Err(rejected(
                    RejectReason::TargetNotFound,
                    format!(
                        "no tab named `{name}` in session `{}`",
                        overview.session.name
                    ),
                )),
                several => {
                    let ids = several
                        .iter()
                        .map(|tab_id| tab_id.to_string())
                        .collect::<Vec<String>>()
                        .join(", ");
                    Err(rejected(
                        RejectReason::TargetAmbiguous,
                        format!(
                            "several tabs are named `{name}` in session `{}`: {ids}; use the tab id",
                            overview.session.name
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
pub fn scope_sessions(
    runtime_dir: &Path,
    session: Option<&SessionRef>,
) -> Result<Discovered, CliError> {
    match session {
        None => Ok(discovery::fetch_all(runtime_dir)),
        Some(SessionRef::Id(id)) => Ok(Discovered::of(discovery::fetch_one(runtime_dir, *id)?)),
        Some(session_ref) => {
            let found = discovery::fetch_all(runtime_dir);
            let picked = pick_session(Some(session_ref), None, None, None, &found)?;
            Ok(Discovered::of(picked.clone()))
        }
    }
}

/// The tab a `--tab` flag names, over the sessions in scope: an id passes
/// straight through with no lookup, and a name must match exactly one tab of
/// exactly one session.
pub fn tab_by_ref(found: &Discovered, tab_ref: &TabRef) -> Result<TabId, CliError> {
    match tab_ref {
        TabRef::Id(tab_id) => Ok(*tab_id),
        TabRef::Name(_) => resolve_tab(pick_session_by_tab(tab_ref, found)?, tab_ref),
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
