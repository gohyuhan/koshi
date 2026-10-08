//! The `koshi share` commands: hand one identity a remote access token, stop
//! the tokens an identity holds, and list the grants this machine has made.
//!
//! Every verb reaches the router over the control plane, and the router is the
//! only writer of the token store. A `--session` flag names the one session a
//! grant reaches. The session is resolved here, through the same targeting the
//! discovery queries use: a name that matches no running session, or two of
//! them, is refused before the router is asked anything.
//!
//! A grant also asks the router where this machine serves remote clients. With
//! an address set and remote access switched off, the grant offers to switch
//! it on, and opens the port on a yes. The token is minted first and the offer
//! follows it: a grant that fails opens no port. A granted token stands from
//! the moment it is minted. Its secret is printed once, whatever the offer
//! does, and nothing prints it again.
//!
//! A verb run outside every pane is never refused, the revoke that cuts a live
//! connection included. A verb run in a pane is refused while any client is
//! attached to that pane's session from another machine.
//!
//! A revoke naming one session, for an identity that also holds a host-wide
//! grant, asks before it stops anything: a yes stops both grants, a no stops
//! neither.

use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::time::SystemTime;

use koshi_core::client::ClientOrigin;
use koshi_core::discovery::{ClientDiscovery, SessionOverview};
use koshi_core::event::RejectReason;
use koshi_core::ids::SessionId;
use koshi_host::host_addresses::{self, HostAddress};
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

/// Refuse a `share` verb run in a pane of a session that a client on another
/// machine is attached to.
///
/// `in_session_context` is the pane environment the calling CLI inherited.
/// [`run_share_command`] calls this only when it has one; a run outside every
/// pane is never refused. `fetch_session_overview` asks one session to
/// describe itself; the command passes
/// [`discovery::fetch_session_overview`](koshi_link::discovery::fetch_session_overview).
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
    let shared_sessions_base_directory = ipc_client::resolve_shared_sessions_base_directory();
    if let Some(in_session_context) = in_session_context {
        // The pane's own session advertises in this user's runtime directory.
        refuse_while_watched_from_another_machine(in_session_context, |session_id| {
            koshi_link::discovery::fetch_session_overview(
                &runtime_directory,
                None,
                session_id,
                None,
            )
        })?;
    }
    match command {
        ShareCommand::Grant {
            identity,
            session_reference,
            token_expiry,
        } => {
            let token_scope = resolve_token_scope(
                &runtime_directory,
                shared_sessions_base_directory.as_deref(),
                session_reference.as_ref(),
            )?
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
                    has_replaced_active_grant,
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
            let token_scope = resolve_token_scope(
                &runtime_directory,
                shared_sessions_base_directory.as_deref(),
                session_reference.as_ref(),
            )?;
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
            let token_scope = resolve_token_scope(
                &runtime_directory,
                shared_sessions_base_directory.as_deref(),
                session_reference.as_ref(),
            )?;
            match router_client::submit_router_request(
                &runtime_directory,
                RouterRequestKind::ListTokens { scope: token_scope },
            )? {
                RouterResult::Tokens(token_entries) => {
                    print!(
                        "{}",
                        output::render_share_list(&token_entries, *output_format)
                    );
                    Ok(())
                }
                unexpected_router_result => Err(build_router_refusal(&unexpected_router_result)),
            }
        }
    }
}

/// Stop the grants `identity` holds, narrowed to one session when `token_scope`
/// names one, and print what stopped.
///
/// A revoke naming no session stops every grant the identity holds.
///
/// A revoke naming one session first asks the router for `identity`'s grants.
/// When a host-wide grant still stands, this names it and asks whether to stop
/// both. A yes stops the session grant and then the host-wide one, in two
/// requests. A no stops neither and prints `nothing was revoked.`; the grants
/// are left exactly as they were.
///
/// Grants on other sessions are never touched: each request names one scope.
///
/// `confirm_revoke` is asked once, with the warning and the question to print;
/// `prompt::read_yes_answer` is what the command passes, which prints both on
/// standard error. `request_router` carries one control-plane request to the router
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

    if !confirm_revoke(&format!(
        "{}stop both the grant on that session and {identity}'s host-wide grant? [y/N] ",
        output::render_revoke_host_wide_warning(identity, session_scope)
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

/// Ask the router to stop `identity`'s grants, narrowed to `token_scope` when it
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
/// - otherwise asks on standard error, and a yes sends
///   [`RouterRequestKind::EnableRemote`]. With remote access off, the question
///   reads `remote access is off.` then `turn it on and open <address>? [y/N]`;
///   with it on and nothing listening, `remote access is on, and nothing is
///   listening on <address>.` then `try to open <address> now? [y/N]`.
///
/// A yes that opens the port is [`RemoteReady::On`]. A no is
/// [`RemoteReady::Off`] when remote access was off, and
/// [`RemoteReady::Blocked`] when it was on. A refused enable is
/// [`RemoteReady::Blocked`].
fn resolve_remote_access_ready(runtime_directory: &Path) -> Result<RemoteReady, CliError> {
    let remote_status_response =
        router_client::submit_router_request(runtime_directory, RouterRequestKind::RemoteStatus)?;
    let (remote_listen_address, is_remote_access_enabled, is_remote_listener_active) =
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
    let Some(remote_listen_address) = remote_listen_address else {
        return Ok(RemoteReady::NoAddress);
    };
    if is_remote_access_enabled && is_remote_listener_active {
        return Ok(RemoteReady::On {
            host_addresses: list_host_addresses_for_listen_address(
                remote_listen_address,
                host_addresses::list_host_addresses,
            ),
            remote_listen_address,
        });
    }
    let prompt_text = if is_remote_access_enabled {
        format!(
            "remote access is on, and nothing is listening on {remote_listen_address}.\n\
             try to open {remote_listen_address} now? [y/N] "
        )
    } else {
        format!("remote access is off.\nturn it on and open {remote_listen_address}? [y/N] ")
    };
    if !prompt::read_yes_answer(&prompt_text) {
        return Ok(if is_remote_access_enabled {
            RemoteReady::Blocked {
                remote_listen_address,
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
            host_addresses: list_host_addresses_for_listen_address(
                remote_listen_address,
                host_addresses::list_host_addresses,
            ),
            remote_listen_address,
        }),
        RouterResult::Error(_) => Ok(RemoteReady::Blocked {
            remote_listen_address,
        }),
        unexpected_router_result => Err(build_router_refusal(&unexpected_router_result)),
    }
}

/// The addresses of this machine a client on another machine can connect to
/// when the remote listener serves on `remote_listen_address`.
///
/// `0.0.0.0` gives the IPv4 entries of `list_machine_addresses()` and `[::]`
/// its IPv6 entries, in the order it gives them. Every other address gives
/// none and does not call `list_machine_addresses`. The command passes
/// [`host_addresses::list_host_addresses`].
///
/// Example: `0.0.0.0:7654` on a machine with `192.168.1.20` and `2001:db8::20`
/// gives `192.168.1.20`.
fn list_host_addresses_for_listen_address(
    remote_listen_address: SocketAddr,
    list_machine_addresses: impl FnOnce() -> Vec<HostAddress>,
) -> Vec<HostAddress> {
    if !remote_listen_address.ip().is_unspecified() {
        return Vec::new();
    }
    list_machine_addresses()
        .into_iter()
        .filter(|host_address| host_address.ip_address.is_ipv4() == remote_listen_address.is_ipv4())
        .collect()
}

/// The scope a `--session` flag names: `None` when the flag is absent, else
/// the id of the one running session the flag resolves to.
///
/// A name matching no running session, or two of them, comes back as the
/// targeting layer's own refusal. The running sessions are those
/// `runtime_directory` and `shared_sessions_base_directory` advertise.
fn resolve_token_scope(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_reference: Option<&SessionReference>,
) -> Result<Option<TokenScope>, CliError> {
    let Some(session_reference) = session_reference else {
        return Ok(None);
    };
    let discovered_sessions = targeting::resolve_session_scope(
        runtime_directory,
        shared_sessions_base_directory,
        Some(session_reference),
    )?;
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
                unexpected_router_result.get_wire_name()
            ),
        },
    }
}
