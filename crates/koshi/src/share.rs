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
use koshi_core::discovery::{ClientDiscovery, SessionOverview};
use koshi_core::event::RejectReason;
use koshi_core::ids::SessionId;
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::remote_tokens::TokenScope;
use koshi_ipc::router::{RouterRequestKind, RouterResult};
use koshi_ipc::wire::WireName;

use crate::cli::{Expiry, SessionReference, ShareCommand};
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
fn has_client_from_another_machine(client_discoveries: &[ClientDiscovery]) -> bool {
    client_discoveries
        .iter()
        .any(|client_discovery| client_discovery.origin != Some(ClientOrigin::Local))
}

/// Refuse a `share` verb run in a pane of a session anyone is attached to from
/// another machine.
///
/// `grant` prints the new token's secret and `list` prints every identity
/// holding one. The session paints that pane to every client viewing its tab,
/// so a client on another machine reads whatever they printed. A pane of a
/// session nobody watches from elsewhere prints to that machine alone.
///
/// `in_session_context` is the pane environment the calling CLI inherited.
/// [`run_share_command`] calls
/// this only when it has one: a run outside every pane prints to a terminal
/// koshi does not paint, and is never refused. `fetch_session_overview` asks one session to
/// describe itself; the command passes [`ipc_client::fetch_session_overview`].
///
/// # Errors
/// [`CliError::CommandRejected`] with [`RejectReason::Unauthorized`] on two
/// conditions: the session lists a client that is not
/// [`ClientOrigin::Local`], and `fetch_session_overview` fails. Nothing is read from or
/// written to the token store.
fn refuse_while_watched_from_another_machine(
    in_session_context: &InSessionContext,
    fetch_session_overview: impl FnOnce(SessionId) -> Result<SessionOverview, CliError>,
) -> Result<(), CliError> {
    let session_overview = fetch_session_overview(in_session_context.session_id)
        .map_err(|overview_error| CliError::CommandRejected {
        reason: RejectReason::Unauthorized,
        help: Some(format!(
            "this session could not say who is attached to it, so whether anyone sees this \
             pane from another machine is unknown: {overview_error}. Run `koshi share` from a terminal \
             outside koshi."
        )),
    })?;
    if !has_client_from_another_machine(&session_overview.clients) {
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
pub fn run_share_command(
    command: &ShareCommand,
    in_session_context: Option<&InSessionContext>,
) -> Result<(), CliError> {
    let runtime_directory = ipc_client::resolve_runtime_directory()?;
    if let Some(in_session_context) = in_session_context {
        refuse_while_watched_from_another_machine(in_session_context, |session_id| {
            ipc_client::fetch_session_overview(&runtime_directory, session_id)
        })?;
    }
    match command {
        ShareCommand::Grant {
            identity,
            session_reference,
            token_expiry,
        } => {
            let token_scope = resolve_token_scope(&runtime_directory, session_reference.as_ref())?
                .unwrap_or(TokenScope::HostWide);
            let expiration_duration = match token_expiry {
                Expiry::After(expiration_duration) => Some(*expiration_duration),
                Expiry::Never => None,
            };
            let router_request_kind = RouterRequestKind::GrantToken {
                identity: identity.clone(),
                scope: token_scope.clone(),
                expires_in: expiration_duration,
            };
            match router_client::submit_router_request(&runtime_directory, router_request_kind)? {
                RouterResult::Granted {
                    connection_token,
                    did_replace_active_grant: has_replaced_active_grant,
                } => {
                    let mut output_writer = io::stdout();
                    write_share_grant(
                        &mut output_writer,
                        &connection_token,
                        identity,
                        &token_scope,
                        has_replaced_active_grant,
                        || {
                            resolve_remote_ready_or_unknown(resolve_remote_access_ready(
                                &runtime_directory,
                            ))
                        },
                    )
                    .map_err(|write_error| CliError::Runtime {
                        detail: format!("the grant could not be printed: {write_error}"),
                    })
                }
                unexpected_router_result => Err(build_router_refusal(&unexpected_router_result)),
            }
        }
        ShareCommand::Revoke {
            identity,
            session_reference,
        } => {
            let token_scope = resolve_token_scope(&runtime_directory, session_reference.as_ref())?;
            revoke_share_grants(
                identity,
                token_scope.as_ref(),
                prompt::read_yes_answer,
                |router_request_kind| {
                    router_client::submit_router_request(&runtime_directory, router_request_kind)
                },
            )
        }
        ShareCommand::List {
            session_reference,
            output_format,
        } => {
            let token_scope = resolve_token_scope(&runtime_directory, session_reference.as_ref())?;
            match router_client::submit_router_request(
                &runtime_directory,
                RouterRequestKind::ListTokens { scope: token_scope },
            )? {
                RouterResult::Tokens(entries) => {
                    print!("{}", output::render_share_list(&entries, *output_format));
                    Ok(())
                }
                unexpected_router_result => Err(build_router_refusal(&unexpected_router_result)),
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
/// `confirm_revoke` is asked once, with the question to print; `prompt::read_yes_answer` is what
/// the command passes. `ask` carries one control-plane request to the router
/// and hands back its answer; the command passes
/// [`router_client::submit_router_request`].
///
/// # Errors
/// Whatever the router reports for the listing, and for the first revoke. A
/// refusal of the second revoke prints what the first one stopped, then reports
/// that the host-wide grant is still standing and names the command that stops
/// it.
fn revoke_share_grants(
    identity: &str,
    token_scope: Option<&TokenScope>,
    confirm_revoke: impl FnOnce(&str) -> bool,
    mut request_router: impl FnMut(RouterRequestKind) -> Result<RouterResult, CliError>,
) -> Result<(), CliError> {
    let Some(session_scope @ TokenScope::Session(_)) = token_scope else {
        print!(
            "{}",
            output::render_share_revoke(&revoke_token_scope(
                &mut request_router,
                identity,
                token_scope,
            )?)
        );
        return Ok(());
    };
    if !has_active_host_wide_grant(&mut request_router, identity)? {
        print!(
            "{}",
            output::render_share_revoke(&revoke_token_scope(
                &mut request_router,
                identity,
                Some(session_scope),
            )?)
        );
        return Ok(());
    }

    print!(
        "{}",
        output::render_revoke_host_wide_warning(identity, session_scope)
    );
    if !confirm_revoke(&format!(
        "stop both the grant on that session and {identity}'s host-wide grant? [y/N] "
    )) {
        println!("nothing was revoked.");
        return Ok(());
    }
    let revoked_session_scopes =
        revoke_token_scope(&mut request_router, identity, Some(session_scope))?;
    match revoke_token_scope(&mut request_router, identity, Some(&TokenScope::HostWide)) {
        Ok(revoked_host_wide_scopes) => {
            let revoked_scopes: Vec<TokenScope> = revoked_session_scopes
                .into_iter()
                .chain(revoked_host_wide_scopes)
                .collect();
            print!("{}", output::render_share_revoke(&revoked_scopes));
            Ok(())
        }
        Err(host_wide_revoke_error) => {
            print!("{}", output::render_share_revoke(&revoked_session_scopes));
            Err(CliError::Runtime {
                detail: format!(
                    "{identity}'s host-wide grant is still standing, and still reaches that \
                     session: {host_wide_revoke_error}\n  run `koshi share revoke {identity}` to stop it"
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
fn revoke_token_scope(
    request_router: &mut impl FnMut(RouterRequestKind) -> Result<RouterResult, CliError>,
    identity: &str,
    token_scope: Option<&TokenScope>,
) -> Result<Vec<TokenScope>, CliError> {
    let router_request_kind = RouterRequestKind::RevokeToken {
        identity: identity.to_string(),
        scope: token_scope.cloned(),
    };
    match request_router(router_request_kind)? {
        RouterResult::Revoked(revoked_scopes) => Ok(revoked_scopes),
        unexpected_router_result => Err(build_router_refusal(&unexpected_router_result)),
    }
}

/// Whether `identity` holds a host-wide grant that still stands right now.
///
/// # Errors
/// Whatever the router answers other than [`RouterResult::Tokens`].
fn has_active_host_wide_grant(
    request_router: &mut impl FnMut(RouterRequestKind) -> Result<RouterResult, CliError>,
    identity: &str,
) -> Result<bool, CliError> {
    let active_token_entries = match request_router(RouterRequestKind::ListTokens { scope: None })?
    {
        RouterResult::Tokens(token_entries) => token_entries,
        unexpected_router_result => return Err(build_router_refusal(&unexpected_router_result)),
    };
    let current_time = SystemTime::now();
    Ok(active_token_entries.iter().any(|token_entry| {
        token_entry.identity == identity
            && token_entry.scope == TokenScope::HostWide
            && token_entry.is_active_at(current_time)
    }))
}

/// Write a share grant to `output_writer`: the secret first, then what it can reach.
///
/// Writes the secret block, flushes `output_writer`, calls `resolve_remote_ready`, then writes what
/// `resolve_remote_ready` returned. It may prompt and may fail; nothing it does happens
/// before the flush.
///
/// # Errors
/// Whatever `output_writer` reports.
fn write_share_grant<Writer: Write>(
    output_writer: &mut Writer,
    connection_token: &ConnectionToken,
    identity: &str,
    token_scope: &TokenScope,
    has_replaced_active_grant: bool,
    resolve_remote_ready: impl FnOnce() -> RemoteReady,
) -> io::Result<()> {
    write!(
        output_writer,
        "{}",
        output::render_share_grant(
            connection_token,
            identity,
            token_scope,
            has_replaced_active_grant,
        )
    )?;
    output_writer.flush()?;
    let remote_ready = resolve_remote_ready();
    write!(
        output_writer,
        "{}",
        output::render_remote_ready(identity, &remote_ready)
    )
}

/// What a grant closes with, given what asking the router produced.
///
/// `Ok` passes the answer through. `Err` writes the error to stderr and returns
/// [`RemoteReady::Unknown`], never [`RemoteReady::Off`].
fn resolve_remote_ready_or_unknown(
    remote_ready_result: Result<RemoteReady, CliError>,
) -> RemoteReady {
    match remote_ready_result {
        Ok(remote_ready) => remote_ready,
        Err(remote_ready_error) => {
            eprintln!("remote access was left as it is: {remote_ready_error}");
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
fn resolve_remote_access_ready(runtime_directory: &Path) -> Result<RemoteReady, CliError> {
    let remote_status_response =
        router_client::submit_router_request(runtime_directory, RouterRequestKind::RemoteStatus)?;
    let (remote_address, is_remote_access_enabled, is_remote_listener_active) =
        match remote_status_response {
            RouterResult::RemoteStatus {
                remote_listen_address,
                is_remote_access_enabled,
                is_listening,
                ..
            } => (
                remote_listen_address,
                is_remote_access_enabled,
                is_listening,
            ),
            unexpected_router_result => {
                return Err(build_router_refusal(&unexpected_router_result))
            }
        };
    let Some(remote_address) = remote_address else {
        return Ok(RemoteReady::NoAddress);
    };
    if is_remote_access_enabled && is_remote_listener_active {
        return Ok(RemoteReady::On {
            remote_listen_address: remote_address,
        });
    }
    let prompt_text = if is_remote_access_enabled {
        println!("remote access is on, and nothing is listening on {remote_address}.");
        format!("try to open {remote_address} now? [y/N] ")
    } else {
        println!("remote access is off.");
        format!("turn it on and open {remote_address}? [y/N] ")
    };
    if !prompt::read_yes_answer(&prompt_text) {
        return Ok(if is_remote_access_enabled {
            RemoteReady::Blocked {
                remote_listen_address: remote_address,
            }
        } else {
            RemoteReady::Off
        });
    }
    match router_client::submit_router_request(runtime_directory, RouterRequestKind::EnableRemote)?
    {
        RouterResult::RemoteEnabled {
            remote_listen_address,
            ..
        } => Ok(RemoteReady::On {
            remote_listen_address,
        }),
        RouterResult::Error(_) => Ok(RemoteReady::Blocked {
            remote_listen_address: remote_address,
        }),
        unexpected_router_result => Err(build_router_refusal(&unexpected_router_result)),
    }
}

/// The scope a `--session` flag names: `None` when the flag is absent, else
/// the id of the one running session the flag resolves to.
///
/// A name matching no running session, or two of them, comes back as the
/// targeting layer's own refusal.
fn resolve_token_scope(
    runtime_directory: &Path,
    session_reference: Option<&SessionReference>,
) -> Result<Option<TokenScope>, CliError> {
    let Some(session_reference) = session_reference else {
        return Ok(None);
    };
    let discovered_sessions =
        targeting::resolve_session_scope(runtime_directory, Some(session_reference))?;
    let session_overview =
        discovered_sessions
            .sessions
            .first()
            .ok_or_else(|| CliError::SessionNotFound {
                session_name: session_reference.to_string(),
            })?;
    Ok(Some(TokenScope::Session(
        session_overview.session.session_id,
    )))
}

/// The failure behind an answer that is not the one the request asks for: the
/// router's own message when it refused, else the reply naming a result the
/// request cannot produce.
fn build_router_refusal(router_result: &RouterResult) -> CliError {
    match router_result {
        RouterResult::Error(error_payload) => CliError::Runtime {
            detail: error_payload.message.clone(),
        },
        unexpected_router_result => CliError::IpcUnavailable {
            detail: format!(
                "the router answered with an unexpected {} reply",
                unexpected_router_result.wire_name()
            ),
        },
    }
}
