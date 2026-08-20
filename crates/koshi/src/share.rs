//! The `koshi share` commands: hand one identity a remote access token, stop
//! the tokens an identity holds, and list the grants this machine has made.
//!
//! Every verb reaches the router over the control plane; the router is the
//! only writer of the token store. A `--session` flag names the one
//! session a grant reaches; the session is resolved here, through the same
//! targeting the discovery queries use, so a name that matches no running
//! session or two of them is refused before anything is asked of the router.
//!
//! A grant also asks the router where this machine serves remote clients. With
//! an address set and remote access switched off, the grant offers to switch
//! it on and opens the port on a yes, so one command hands out a token and
//! makes it usable. The token is minted first and the offer follows it, so a
//! grant that fails never opens a port. The secret is printed whatever the
//! offer does, and that printing is the only one: a granted token stands from
//! the moment it is made, and nothing prints its secret again.
//!
//! A verb run outside every pane is never refused: the router's socket is this
//! machine's own, no connection from another machine reaches it, and koshi
//! paints no terminal there. That covers the revoke that cuts a live
//! connection.
//!
//! A verb run in a pane is refused while any client is attached to that pane's
//! session from another machine: the session paints that pane to them too.
//!
//! A revoke naming one session, for an identity that also holds a host-wide
//! grant, asks before it stops anything: a yes stops both grants, a no stops
//! neither.

use std::io::{self, Write};
use std::path::Path;
use std::time::SystemTime;

use koshi_core::client::ClientOrigin;
use koshi_core::discovery::{ClientInfo, SessionOverview};
use koshi_core::event::RejectReason;
use koshi_core::ids::SessionId;
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::remote_tokens::TokenScope;
use koshi_ipc::router::{RouterRequestKind, RouterResult};
use koshi_ipc::wire::WireName;

use crate::cli::{Expiry, SessionRef, ShareCommand};
use crate::output::RemoteReady;
use crate::{output, prompt, targeting};
use koshi_link::error::CliError;
use koshi_link::in_session::InSessionContext;
use koshi_link::{ipc_client, router_client};

#[cfg(test)]
mod tests;

/// Whether any client attached to the session is on another machine.
///
/// True for a row naming [`ClientOrigin::Remote`], and for a row naming no
/// origin at all: a session server built before that field existed serves such
/// a row, and it does not say the client is local.
fn watched_from_another_machine(clients: &[ClientInfo]) -> bool {
    clients
        .iter()
        .any(|client| client.origin != Some(ClientOrigin::Local))
}

/// Refuse a `share` verb run in a pane of a session anyone is attached to from
/// another machine.
///
/// `grant` prints the new token's secret and `list` prints every identity
/// holding one. The session paints that pane to every client viewing its tab,
/// so a client on another machine reads whatever they printed. A pane of a
/// session nobody watches from elsewhere prints to that machine alone.
///
/// `context` is the pane environment the calling CLI inherited. [`run`] calls
/// this only when it has one: a run outside every pane prints to a terminal
/// koshi does not paint, and is never refused. `look_up` asks one session to
/// describe itself; the command passes [`ipc_client::fetch_overview`].
///
/// # Errors
/// [`CliError::CommandRejected`] with [`RejectReason::Unauthorized`] on two
/// conditions: the session lists a client that is not
/// [`ClientOrigin::Local`], and `look_up` fails. Nothing is read from or
/// written to the token store.
fn refuse_while_watched_from_another_machine(
    context: &InSessionContext,
    look_up: impl FnOnce(SessionId) -> Result<SessionOverview, CliError>,
) -> Result<(), CliError> {
    let overview = look_up(context.session_id).map_err(|error| CliError::CommandRejected {
        reason: RejectReason::Unauthorized,
        help: Some(format!(
            "this session could not say who is attached to it, so whether anyone sees this \
             pane from another machine is unknown: {error}. Run `koshi share` from a terminal \
             outside koshi."
        )),
    })?;
    if !watched_from_another_machine(&overview.clients) {
        return Ok(());
    }
    Err(CliError::CommandRejected {
        reason: RejectReason::Unauthorized,
        help: Some(
            "someone is attached to this session from another machine, and they see this \
             pane. Run `koshi share` from a terminal outside koshi."
                .to_string(),
        ),
    })
}

/// Run one `share` verb: resolve the scope it names, ask the router, and
/// print the rendered answer.
///
/// A grant with no `--session` reaches every session on this machine; a
/// revoke or a listing with no `--session` covers every scope. A router that
/// refuses the request is [`CliError::Runtime`] carrying the router's own
/// message.
pub fn run(command: &ShareCommand, context: Option<&InSessionContext>) -> Result<(), CliError> {
    let runtime_dir = ipc_client::runtime_dir()?;
    if let Some(context) = context {
        refuse_while_watched_from_another_machine(context, |session_id| {
            ipc_client::fetch_overview(&runtime_dir, session_id)
        })?;
    }
    match command {
        ShareCommand::Grant {
            identity,
            session,
            expires,
        } => {
            let scope = scope_of(&runtime_dir, session.as_ref())?.unwrap_or(TokenScope::HostWide);
            let expires_in = match expires {
                Expiry::After(span) => Some(*span),
                Expiry::Never => None,
            };
            let kind = RouterRequestKind::GrantToken {
                identity: identity.clone(),
                scope: scope.clone(),
                expires_in,
            };
            match router_client::router_request(&runtime_dir, kind)? {
                RouterResult::Granted { token, replaced } => {
                    let mut out = io::stdout();
                    write_grant(&mut out, &token, identity, &scope, replaced, || {
                        ready_or_unknown(remote_ready(&runtime_dir))
                    })
                    .map_err(|error| CliError::Runtime {
                        detail: format!("the grant could not be printed: {error}"),
                    })
                }
                other => Err(refusal(&other)),
            }
        }
        ShareCommand::Revoke { identity, session } => {
            let scope = scope_of(&runtime_dir, session.as_ref())?;
            revoke(identity, scope.as_ref(), prompt::yes, |kind| {
                router_client::router_request(&runtime_dir, kind)
            })
        }
        ShareCommand::List { session, format } => {
            let scope = scope_of(&runtime_dir, session.as_ref())?;
            match router_client::router_request(
                &runtime_dir,
                RouterRequestKind::ListTokens { scope },
            )? {
                RouterResult::Tokens(entries) => {
                    print!("{}", output::render_share_list(&entries, *format));
                    Ok(())
                }
                other => Err(refusal(&other)),
            }
        }
    }
}

/// Stop the grants `identity` holds, narrowed to one session when `scope`
/// names one, and print what stopped.
///
/// A revoke naming no session stops every grant the identity holds, so nothing
/// wider can survive it.
///
/// A revoke naming one session first asks the router for `identity`'s grants. A
/// host-wide grant still standing reaches that session too, and no revoke stops
/// a host-wide grant for one session alone, so this names it and asks whether to
/// stop both. A yes stops the session grant and then the host-wide one, in two
/// requests. A no stops neither and prints `nothing was revoked.`; the grants
/// are left exactly as they were.
///
/// Grants on other sessions are never touched: each request names one scope.
///
/// `confirm` is asked once, with the question to print; `prompt::yes` is what
/// the command passes. `ask` carries one control-plane request to the router
/// and hands back its answer; the command passes
/// [`router_client::router_request`].
///
/// # Errors
/// Whatever the router reports for the listing, and for the first revoke. A
/// refusal of the second revoke prints what the first one stopped, then reports
/// that the host-wide grant is still standing and names the command that stops
/// it.
fn revoke(
    identity: &str,
    scope: Option<&TokenScope>,
    confirm: impl FnOnce(&str) -> bool,
    mut ask: impl FnMut(RouterRequestKind) -> Result<RouterResult, CliError>,
) -> Result<(), CliError> {
    let Some(session @ TokenScope::Session(_)) = scope else {
        print!(
            "{}",
            output::render_share_revoke(&revoke_scope(&mut ask, identity, scope)?)
        );
        return Ok(());
    };
    if !holds_live_host_wide(&mut ask, identity)? {
        print!(
            "{}",
            output::render_share_revoke(&revoke_scope(&mut ask, identity, Some(session))?)
        );
        return Ok(());
    }

    print!(
        "{}",
        output::render_revoke_host_wide_warning(identity, session)
    );
    if !confirm(&format!(
        "stop both the grant on that session and {identity}'s host-wide grant? [y/N] "
    )) {
        println!("nothing was revoked.");
        return Ok(());
    }
    let stopped = revoke_scope(&mut ask, identity, Some(session))?;
    match revoke_scope(&mut ask, identity, Some(&TokenScope::HostWide)) {
        Ok(host_wide) => {
            let all: Vec<TokenScope> = stopped.into_iter().chain(host_wide).collect();
            print!("{}", output::render_share_revoke(&all));
            Ok(())
        }
        Err(error) => {
            print!("{}", output::render_share_revoke(&stopped));
            Err(CliError::Runtime {
                detail: format!(
                    "{identity}'s host-wide grant is still standing, and still reaches that \
                     session: {error}\n  run `koshi share revoke {identity}` to stop it"
                ),
            })
        }
    }
}

/// Ask the router to stop `identity`'s grants, narrowed to `scope` when it
/// names one, and hand back the scope of each grant that stopped.
///
/// # Errors
/// Whatever the router answers other than [`RouterResult::Revoked`].
fn revoke_scope(
    ask: &mut impl FnMut(RouterRequestKind) -> Result<RouterResult, CliError>,
    identity: &str,
    scope: Option<&TokenScope>,
) -> Result<Vec<TokenScope>, CliError> {
    let kind = RouterRequestKind::RevokeToken {
        identity: identity.to_string(),
        scope: scope.cloned(),
    };
    match ask(kind)? {
        RouterResult::Revoked(scopes) => Ok(scopes),
        other => Err(refusal(&other)),
    }
}

/// Whether `identity` holds a host-wide grant that still stands right now.
///
/// # Errors
/// Whatever the router answers other than [`RouterResult::Tokens`].
fn holds_live_host_wide(
    ask: &mut impl FnMut(RouterRequestKind) -> Result<RouterResult, CliError>,
    identity: &str,
) -> Result<bool, CliError> {
    let held = match ask(RouterRequestKind::ListTokens { scope: None })? {
        RouterResult::Tokens(entries) => entries,
        other => return Err(refusal(&other)),
    };
    let now = SystemTime::now();
    Ok(held.iter().any(|entry| {
        entry.identity == identity && entry.scope == TokenScope::HostWide && entry.is_live(now)
    }))
}

/// Write a grant to `out`: the secret first, then what it can reach.
///
/// Writes the secret block, flushes `out`, calls `ready`, then writes what
/// `ready` returned. `ready` may prompt and may fail; nothing it does happens
/// before the flush.
///
/// # Errors
/// Whatever `out` reports.
fn write_grant<W: Write>(
    out: &mut W,
    token: &ConnectionToken,
    identity: &str,
    scope: &TokenScope,
    replaced: bool,
    ready: impl FnOnce() -> RemoteReady,
) -> io::Result<()> {
    write!(
        out,
        "{}",
        output::render_share_grant(token, identity, scope, replaced)
    )?;
    out.flush()?;
    let ready = ready();
    write!(out, "{}", output::render_remote_ready(identity, &ready))
}

/// What a grant closes with, given what asking the router produced.
///
/// `Ok` passes the answer through. `Err` writes the error to stderr and returns
/// [`RemoteReady::Unknown`], never [`RemoteReady::Off`].
fn ready_or_unknown(asked: Result<RemoteReady, CliError>) -> RemoteReady {
    match asked {
        Ok(ready) => ready,
        Err(error) => {
            eprintln!("remote access was left as it is: {error}");
            RemoteReady::Unknown
        }
    }
}

/// What a fresh grant can reach, and the offer that changes the answer.
///
/// Asks the router for the listen address, whether remote access is switched
/// on, and whether the port is held right now, then:
///
/// - no address — [`RemoteReady::NoAddress`], nothing asked;
/// - on and listening — [`RemoteReady::On`], nothing asked;
/// - otherwise prompts, and a yes sends [`RouterRequestKind::EnableRemote`].
///
/// A yes that opens the port is [`RemoteReady::On`]. A no is
/// [`RemoteReady::Off`] when remote access was off, and
/// [`RemoteReady::Blocked`] when it was on. A refused enable is
/// [`RemoteReady::Blocked`].
fn remote_ready(runtime_dir: &Path) -> Result<RemoteReady, CliError> {
    let status = router_client::router_request(runtime_dir, RouterRequestKind::RemoteStatus)?;
    let (address, enabled, listening) = match status {
        RouterResult::RemoteStatus {
            address,
            enabled,
            listening,
            ..
        } => (address, enabled, listening),
        other => return Err(refusal(&other)),
    };
    let Some(address) = address else {
        return Ok(RemoteReady::NoAddress);
    };
    if enabled && listening {
        return Ok(RemoteReady::On { address });
    }
    let question = if enabled {
        println!("remote access is on, and nothing is listening on {address}.");
        format!("try to open {address} now? [y/N] ")
    } else {
        println!("remote access is off.");
        format!("turn it on and open {address}? [y/N] ")
    };
    if !prompt::yes(&question) {
        return Ok(if enabled {
            RemoteReady::Blocked { address }
        } else {
            RemoteReady::Off
        });
    }
    match router_client::router_request(runtime_dir, RouterRequestKind::EnableRemote)? {
        RouterResult::RemoteEnabled { address, .. } => Ok(RemoteReady::On { address }),
        RouterResult::Error(_) => Ok(RemoteReady::Blocked { address }),
        other => Err(refusal(&other)),
    }
}

/// The scope a `--session` flag names: `None` when the flag is absent, else
/// the id of the one running session the flag resolves to.
///
/// A name matching no running session, or two of them, comes back as the
/// targeting layer's own refusal.
fn scope_of(
    runtime_dir: &Path,
    session: Option<&SessionRef>,
) -> Result<Option<TokenScope>, CliError> {
    let Some(session_ref) = session else {
        return Ok(None);
    };
    let found = targeting::scope_sessions(runtime_dir, Some(session_ref))?;
    let overview = found
        .sessions
        .first()
        .ok_or_else(|| CliError::SessionNotFound {
            session: session_ref.to_string(),
        })?;
    Ok(Some(TokenScope::Session(overview.session.id)))
}

/// The failure behind an answer that is not the one the request asks for: the
/// router's own message when it refused, else the reply naming a result the
/// request cannot produce.
fn refusal(result: &RouterResult) -> CliError {
    match result {
        RouterResult::Error(payload) => CliError::Runtime {
            detail: payload.message.clone(),
        },
        other => CliError::IpcUnavailable {
            detail: format!(
                "the router answered with an unexpected {} reply",
                other.wire_name()
            ),
        },
    }
}
