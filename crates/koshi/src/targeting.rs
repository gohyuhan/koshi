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

use crate::cli::{CliCommand, ResolvedTargets, SessionRef, TabRef};
use koshi_link::discovery::{self, Discovered};
use koshi_link::error::CliError;
use koshi_link::in_session::InSessionContext;
use koshi_link::ipc_client;
use koshi_link::remote_client::{self, ServerArg};

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
    // The in-session route carries no target client. A command whose client
    // rides on its source never takes that route — not even back to this
    // pane's own session.
    let names_source_client = command.source_client().is_some();

    // In-session, targeting home: the command stays on its own socket.
    // Flags given as ids ride into the command as-is (`to_action` reads
    // them); only a `--tab` NAME costs a lookup, answered by the session
    // itself.
    if let Some(context) = context {
        let stays_home = !names_source_client
            && match command.target_session() {
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

    let (session, targets) = resolve_targets(command, &found)?;

    // A probe can land back on the session this CLI runs inside (e.g.
    // `--session` naming it); then the command still travels as the pane's
    // own, keeping the issuing pane as the default target.
    match context {
        Some(context) if context.session_id == session && !names_source_client => {
            Ok(Route::InSession(targets))
        }
        _ => Ok(Route::External { session, targets }),
    }
}

/// Send `command` to the machine `server` names and hand back the
/// dispatcher's result.
///
/// `server` is the name this machine saved that server under, or the
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
    server: &str,
    command: &CliCommand,
    new_pane_direction: Direction,
) -> Result<CommandResult, CliError> {
    let named = remote_client::resolve_server(server)?;
    // This connection saves the record the dials below present.
    let (mut link, saved) =
        remote_client::connect_saved(&named, None, Some(remote_client::REPLY_WAIT))?;
    let rows = remote_client::list_remote_sessions(&mut link)?;
    // The dials below open their own connections.
    drop(link);

    let arg = ServerArg::Saved(saved);
    let found = remote_census(&arg, rows_to_ask(command.target_session(), rows));

    let (session, targets) = resolve_targets(command, &found)?;

    let (_, action) = command
        .to_action(&targets, new_pane_direction)
        .expect("only an action verb reaches the remote dispatch");
    remote_client::submit_remote(&arg, session, command.source_client(), action)
}

/// Which of a server's `rows` the census must ask, given the `--session` flag.
///
/// [`SessionRef::Id`] keeps the one row carrying that id, and no rows when
/// none does. Every other selector — a name, a pane, a tab, a client, or no
/// flag at all — keeps every row.
///
/// Example — with `--session session-<uuid>` against a server listing eight
/// sessions, one row comes back, so [`remote_census`] makes one dial.
fn rows_to_ask(session: Option<&SessionRef>, rows: Vec<RemoteSessionRow>) -> Vec<RemoteSessionRow> {
    match session {
        Some(SessionRef::Id(id)) => rows.into_iter().filter(|row| row.id == *id).collect(),
        _ => rows,
    }
}

/// Ask each session in `rows` on the machine `arg` names to describe itself,
/// as the census the targeting rules read.
///
/// One dial per row. Callers narrow `rows` with [`rows_to_ask`] first.
///
/// A session the server listed but could not describe is named on stderr and
/// counted in `Discovered::unasked`. The sessions that answered are sorted by
/// name, then by id.
fn remote_census(arg: &ServerArg, rows: Vec<RemoteSessionRow>) -> Discovered {
    let mut found = Discovered::default();
    for row in rows {
        match remote_client::fetch_remote_overview(arg, row.id) {
            Ok(overview) => found.sessions.push(overview),
            Err(error) => {
                eprintln!("koshi: session {} did not answer: {error}", row.id);
                found.unasked += 1;
            }
        }
    }
    found.sort_sessions();
    found
}

/// The session `command` targets over the census `found`, and its
/// `--session`/`--tab` flags resolved to ids.
///
/// [`pick_session`] picks the session; the resolved `session` field always
/// carries that session's id, and `tab` carries the id a `--tab` flag names
/// within it, or `None` when the flag is absent.
fn resolve_targets(
    command: &CliCommand,
    found: &Discovered,
) -> Result<(SessionId, ResolvedTargets), CliError> {
    let overview = pick_session(
        command.target_session(),
        command.target_pane(),
        command.target_tab(),
        command.target_client(),
        found,
    )?;
    let tab = command
        .target_tab()
        .map(|tab_ref| resolve_tab(overview, tab_ref))
        .transpose()?;
    let session = overview.session.id;
    Ok((
        session,
        ResolvedTargets {
            session: Some(session),
            tab,
        },
    ))
}

/// The one running session named `name`, over the census `found`.
///
/// # Errors
/// [`CliError::SessionNotFound`] when no session that answered carries the
/// name; [`CliError::IpcUnavailable`] when exactly one carries it and a
/// running session did not answer, so "exactly one" cannot be told;
/// [`CliError::CommandRejected`] with [`RejectReason::TargetAmbiguous`],
/// naming every matching id, when several carry it.
pub(crate) fn session_named<'a>(
    found: &'a Discovered,
    name: &str,
) -> Result<&'a SessionOverview, CliError> {
    let matches: Vec<&SessionOverview> = found
        .sessions
        .iter()
        .filter(|overview| overview.session.name == name)
        .collect();
    match matches.as_slice() {
        [] => Err(found.no_such_session(name)),
        [only] if found.is_complete() => Ok(only),
        [_] => Err(found.unanswered(&format!("cannot tell whether `{name}` is unique"))),
        several => {
            let ids = several
                .iter()
                .map(|overview| overview.session.id.to_string())
                .collect::<Vec<String>>()
                .join(", ");
            Err(rejected(
                RejectReason::TargetAmbiguous,
                format!("several sessions are named `{name}`: {ids}; use the session id"),
            ))
        }
    }
}

/// The count rule over the census `found`: the sole running session, when
/// every running session answered.
///
/// # Errors
/// [`CliError::CommandRejected`] with [`RejectReason::TargetAmbiguous`]
/// carrying `several` when more than one session answered;
/// [`CliError::NoSessions`] when none is running;
/// [`CliError::IpcUnavailable`] carrying `unanswered` when a running session
/// did not answer, so "exactly one" cannot be told.
pub(crate) fn the_only_session<'a>(
    found: &'a Discovered,
    several: &str,
    unanswered: &str,
) -> Result<&'a SessionOverview, CliError> {
    let mut running = found.sessions.iter();
    match (running.next(), running.next()) {
        (Some(_), Some(_)) => Err(rejected(RejectReason::TargetAmbiguous, several.to_string())),
        (Some(only), None) if found.is_complete() => Ok(only),
        (None, _) if found.is_complete() => Err(CliError::NoSessions),
        _ => Err(found.unanswered(unanswered)),
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
            SessionRef::Name(name) => session_named(found, name)?,
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
        the_only_session(
            found,
            "several sessions are running; name one with --session <name-or-id>",
            "cannot tell which session to target; name one with --session <name-or-id>",
        )?
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

/// A routing refusal, carrying `reason` and `help` as
/// [`CliError::CommandRejected`].
fn rejected(reason: RejectReason, help: String) -> CliError {
    CliError::CommandRejected {
        reason,
        help: Some(help),
    }
}

#[cfg(test)]
mod tests;
