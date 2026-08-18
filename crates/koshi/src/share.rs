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
//! offer does, since a granted token is live and its secret is readable once.

use std::io::{self, Write};
use std::path::Path;

use koshi_core::client::ClientOrigin;
use koshi_core::command::{Command, DetachArgs, DetachReason};
use koshi_core::discovery::ClientInfo;
use koshi_core::event::RejectReason;
use koshi_core::ids::ClientId;
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::remote_tokens::TokenScope;
use koshi_ipc::router::{RouterRequestKind, RouterResult};
use koshi_ipc::wire::WireName;

use crate::cli::{Expiry, SessionRef, ShareCommand};
use crate::output::RemoteReady;
use crate::{output, targeting};
use koshi_link::error::CliError;
use koshi_link::in_session::InSessionContext;
use koshi_link::{ipc_client, router_client};

#[cfg(test)]
mod tests;

/// Whether `clients` holds a row at `client_id` whose origin is
/// [`ClientOrigin::Remote`].
///
/// `false` when no row carries that id; a remote row for another client does
/// not make it `true`.
fn is_remote(clients: &[ClientInfo], client_id: ClientId) -> bool {
    clients
        .iter()
        .any(|client| client.id == client_id && client.origin == ClientOrigin::Remote)
}

/// Refuse every `share` verb for a client viewing this session from another
/// machine, and detach that client.
///
/// `context` is the pane environment the calling CLI inherited. The pane's
/// designated client (`KOSHI_CLIENT_ID`) is the client this run acts as; the
/// session names that client's [`ClientOrigin`] in its discovery answer, which
/// the session sets at accept and no client can fill in.
///
/// `Ok(())` — the run may proceed. Four cases reach it: the pane names no
/// designated client, the session cannot be asked, the session no longer
/// lists that client, and the client is [`ClientOrigin::Local`].
///
/// # Errors
/// [`CliError::CommandRejected`] with [`RejectReason::Unauthorized`] for a
/// [`ClientOrigin::Remote`] client. The token store is not read or written, and
/// that client is detached first through [`Command::Detach`] carrying
/// [`DetachReason::HostOnlyRefusal`], which tells it what was refused. A detach
/// that fails changes nothing here: the verb is refused either way.
fn refuse_remote_client(runtime_dir: &Path, context: &InSessionContext) -> Result<(), CliError> {
    let Some(client_id) = context.client_id else {
        return Ok(());
    };
    let Ok(overview) = ipc_client::fetch_overview(runtime_dir, context.session_id) else {
        return Ok(());
    };
    if !is_remote(&overview.clients, client_id) {
        return Ok(());
    }

    if let Err(error) = ipc_client::submit_external_via_runtime_dir(
        runtime_dir,
        context.session_id,
        Command::Detach(DetachArgs {
            client: Some(client_id),
            reason: DetachReason::HostOnlyRefusal,
        }),
    ) {
        tracing::warn!(%client_id, %error, "the refused remote client could not be detached");
    }
    Err(CliError::CommandRejected {
        reason: RejectReason::Unauthorized,
        help: Some(
            "`koshi share` only runs on the machine hosting the session; \
             run it in a shell there"
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
        refuse_remote_client(&runtime_dir, context)?;
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
            let kind = RouterRequestKind::RevokeToken {
                identity: identity.clone(),
                scope,
            };
            match router_client::router_request(&runtime_dir, kind)? {
                RouterResult::Revoked(scopes) => {
                    print!("{}", output::render_share_revoke(&scopes));
                    Ok(())
                }
                other => Err(refusal(&other)),
            }
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
    if !prompt_yes(&question) {
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

/// Print `prompt` and read one line, answering true only for a typed yes. A
/// terminal that cannot be read answers no.
fn prompt_yes(prompt: &str) -> bool {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "Yes")
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
            session: match session_ref {
                SessionRef::Id(id) => id.to_string(),
                SessionRef::Name(name) => name.clone(),
            },
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
