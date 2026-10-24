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
//! `koshi share open` opens the port with no question and mints no token, then
//! prints the connect commands a grant prints. On a port that is already open
//! it opens nothing and prints the same commands, fingerprint included.
//!
//! The router reads `remote-listen` when it starts. Both verbs compare that
//! address with the one `koshi.kdl` names when the verb starts, and name the
//! step that makes the router read the file again when the two differ.
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
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use koshi_config::layer::PartialKoshiConfig;
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
use crate::output::{ConfigMismatch, RemoteReady};
use crate::{output, prompt, targeting};
use koshi_link::config::{load_server_config_file, resolve_server_config};
use koshi_link::error::CliError;
use koshi_link::in_session::InSessionContext;
use koshi_link::router_client::RemoteAccessStatus;
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
/// message. `config_directory` holds the `koshi.kdl` this koshi read at start,
/// and `app_config_layer` holds the settings that read gave. A grant and an
/// open compare its `remote-listen` address with the address the router read.
/// A `config_directory` of `None` names no file.
///
/// An open that leaves the port shut prints nothing on standard output and is
/// [`CliError::Runtime`] carrying the block [`output::render_remote_ready`]
/// gives, without its last newline.
pub fn run_share_command(
    command: &ShareCommand,
    in_session_context: Option<&InSessionContext>,
    config_directory: Option<&Path>,
    app_config_layer: Option<&PartialKoshiConfig>,
) -> Result<(), CliError> {
    let runtime_directory = ipc_client::resolve_runtime_directory()?;
    let config_file_path: Option<PathBuf> =
        config_directory.map(|config_directory| config_directory.join("koshi.kdl"));
    let configured_address = resolve_server_config(app_config_layer).remote_listen_address;
    if let Some(in_session_context) = in_session_context {
        // The pane's own session advertises in this user's runtime directory.
        refuse_while_watched_from_another_machine(in_session_context, |session_id| {
            koshi_link::discovery::fetch_session_overview(
                &runtime_directory,
                None,
                session_id,
                None,
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
                                config_file_path.as_deref(),
                                configured_address,
                                prompt::read_yes_answer,
                                |router_request_kind| {
                                    router_client::submit_router_request(
                                        &runtime_directory,
                                        router_request_kind,
                                    )
                                },
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
        ShareCommand::Open => {
            // The offer is answered yes, and no question is printed.
            let remote_ready = resolve_remote_access_ready(
                config_file_path.as_deref(),
                configured_address,
                |_| true,
                |router_request_kind| {
                    router_client::submit_router_request(&runtime_directory, router_request_kind)
                },
            )?;
            let rendered_remote_ready = output::render_remote_ready(None, &remote_ready);
            match remote_ready {
                RemoteReady::On { .. } => {
                    print!("{rendered_remote_ready}");
                    Ok(())
                }
                RemoteReady::NoAddress
                | RemoteReady::Off
                | RemoteReady::Unknown
                | RemoteReady::Blocked { .. }
                | RemoteReady::ConfigMismatched { .. } => Err(CliError::Runtime {
                    detail: rendered_remote_ready.trim_end().to_string(),
                }),
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
        output::render_remote_ready(Some(identity), &remote_ready)
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
/// Asks the router for the listen address it read when it started, the
/// `koshi.kdl` it read it from, whether remote access is switched on, whether
/// the port is held right now, and the fingerprint of the certificate the
/// listener presents. Then compares that address with `configured_address`,
/// the address the `koshi.kdl` at `config_file_path` named when this koshi
/// read it at start, as [`find_config_mismatch`] does, and:
///
/// - on and listening — [`RemoteReady::On`] carrying that fingerprint and the
///   mismatch, if any, nothing asked; a router that names no fingerprint then
///   is a [`CliError::Runtime`];
/// - a mismatch, the port shut — [`RemoteReady::ConfigMismatched`], nothing
///   asked;
/// - no address — [`RemoteReady::NoAddress`], nothing asked;
/// - otherwise asks `confirm_enable`, and a yes sends
///   [`RouterRequestKind::EnableRemote`]. With remote access off, the question
///   reads `remote access is off.` then `turn it on and open <address>? [y/N]`;
///   with it on and nothing listening, `remote access is on, and nothing is
///   listening on <address>.` then `try to open <address> now? [y/N]`.
///
/// A yes that opens the port is [`RemoteReady::On`] carrying the fingerprint
/// the enable answer names. A no is [`RemoteReady::Off`] when remote access
/// was off, and [`RemoteReady::Blocked`] with no refusal when it was on. A
/// refused enable is [`RemoteReady::Blocked`] carrying the router's sentence
/// and whether remote access was on before the request. A refused enable
/// writes no record and leaves that answer as it was.
///
/// `confirm_enable` is asked at most once, with the question to print; a grant
/// passes `prompt::read_yes_answer`, which prints it on standard error, and
/// `share open` passes a function that answers yes and prints nothing.
/// `request_router` carries one control-plane request to the router and hands
/// back its answer; the command passes
/// [`router_client::submit_router_request`].
///
/// # Errors
/// Whatever `request_router` reports. [`CliError::Runtime`] when the router
/// refuses the status request, and when it serves and names no fingerprint.
/// [`CliError::IpcUnavailable`] when an answer names a result its request
/// cannot produce.
fn resolve_remote_access_ready(
    config_file_path: Option<&Path>,
    configured_address: Option<SocketAddr>,
    confirm_enable: impl FnOnce(&str) -> bool,
    mut request_router: impl FnMut(RouterRequestKind) -> Result<RouterResult, CliError>,
) -> Result<RemoteReady, CliError> {
    let remote_access_status =
        RemoteAccessStatus::from_router_result(request_router(RouterRequestKind::RemoteStatus)?)
            .map_err(|unexpected_router_result| build_router_refusal(&unexpected_router_result))?;
    let config_mismatch = find_config_mismatch(
        remote_access_status.remote_listen_address,
        remote_access_status.config_file_path.as_deref(),
        config_file_path,
        configured_address,
    );
    let RemoteAccessStatus {
        remote_listen_address: router_listen_address,
        is_remote_access_enabled,
        is_listening,
        certificate_fingerprint: served_certificate_fingerprint,
        ..
    } = remote_access_status;
    let Some(remote_listen_address) = router_listen_address else {
        return Ok(match config_mismatch {
            Some(config_mismatch) => RemoteReady::ConfigMismatched {
                router_listen_address: None,
                config_mismatch,
            },
            None => RemoteReady::NoAddress,
        });
    };
    if is_remote_access_enabled && is_listening {
        let Some(certificate_fingerprint) = served_certificate_fingerprint else {
            return Err(CliError::Runtime {
                detail: format!(
                    "the router serves {remote_listen_address} and named no certificate \
                     fingerprint"
                ),
            });
        };
        return Ok(RemoteReady::On {
            host_addresses: list_host_addresses_for_listen_address(
                remote_listen_address,
                host_addresses::list_host_addresses,
            ),
            remote_listen_address,
            certificate_fingerprint,
            config_mismatch,
        });
    }
    if let Some(config_mismatch) = config_mismatch {
        return Ok(RemoteReady::ConfigMismatched {
            router_listen_address: Some(remote_listen_address),
            config_mismatch,
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
    if !confirm_enable(&prompt_text) {
        return Ok(if is_remote_access_enabled {
            RemoteReady::Blocked {
                remote_listen_address,
                is_remote_access_enabled,
                open_refusal: None,
            }
        } else {
            RemoteReady::Off
        });
    }
    match request_router(RouterRequestKind::EnableRemote)? {
        RouterResult::RemoteEnabled {
            remote_listen_address,
            certificate_fingerprint,
        } => Ok(RemoteReady::On {
            host_addresses: list_host_addresses_for_listen_address(
                remote_listen_address,
                host_addresses::list_host_addresses,
            ),
            remote_listen_address,
            certificate_fingerprint,
            config_mismatch: None,
        }),
        RouterResult::Error(refusal_payload) => Ok(RemoteReady::Blocked {
            remote_listen_address,
            is_remote_access_enabled,
            open_refusal: Some(refusal_payload.message),
        }),
        unexpected_router_result => Err(build_router_refusal(&unexpected_router_result)),
    }
}

/// Why the listen address the running router read when it started is not the
/// one `koshi.kdl` names, or `None` when they agree.
///
/// `router_listen_address` and `router_config_file_path` are what the router
/// answered: the address it read and the `koshi.kdl` it read it from.
/// `config_file_path` is the `koshi.kdl` this koshi read at start, and
/// `configured_address` the address that read gave.
///
/// 1. The router's file is this koshi's file: a `configured_address` that
///    differs is [`ConfigMismatch::FileChanged`].
/// 2. The router names another file: that file is read now, and an address
///    that differs from the router's is [`ConfigMismatch::FileChanged`]
///    naming that file. Else a `configured_address` that differs is
///    [`ConfigMismatch::OtherFile`].
/// 3. The router names no file: a `configured_address` that differs is
///    [`ConfigMismatch::OtherFile`] with no router file.
///
/// A file that is absent, unreadable, or unparseable names no address, as
/// [`load_server_config_file`] reads it.
///
/// Example: a router that read no address from
/// `/home/user/.config/koshi/koshi.kdl`, which now names `127.0.0.1:7654`,
/// gives `FileChanged` naming that file and `127.0.0.1:7654`.
pub(crate) fn find_config_mismatch(
    router_listen_address: Option<SocketAddr>,
    router_config_file_path: Option<&Path>,
    config_file_path: Option<&Path>,
    configured_address: Option<SocketAddr>,
) -> Option<ConfigMismatch> {
    let Some(router_config_file_path) = router_config_file_path else {
        if configured_address == router_listen_address {
            return None;
        }
        return Some(ConfigMismatch::OtherFile {
            router_config_file_path: None,
            config_file_path: config_file_path.map(Path::to_path_buf),
            configured_address,
        });
    };
    let router_configured_address = if config_file_path == Some(router_config_file_path) {
        configured_address
    } else {
        load_server_config_file(router_config_file_path).remote_listen_address
    };
    if router_configured_address != router_listen_address {
        return Some(ConfigMismatch::FileChanged {
            config_file_path: router_config_file_path.to_path_buf(),
            configured_address: router_configured_address,
        });
    }
    if configured_address == router_listen_address {
        return None;
    }
    Some(ConfigMismatch::OtherFile {
        router_config_file_path: Some(router_config_file_path.to_path_buf()),
        config_file_path: config_file_path.map(Path::to_path_buf),
        configured_address,
    })
}

/// The addresses of this machine a client on another machine can connect to
/// when the remote listener serves on `remote_listen_address`.
///
/// `0.0.0.0` and `[::ffff:0.0.0.0]` give the IPv4 entries of
/// `list_machine_addresses()`, and `[::]` its IPv6 entries, in the order it
/// gives them. Every other address gives none and does not call
/// `list_machine_addresses`. The command passes
/// [`host_addresses::list_host_addresses`].
///
/// Example: `0.0.0.0:7654` on a machine with `192.0.2.20` and `2001:db8::20`
/// gives `192.0.2.20`.
fn list_host_addresses_for_listen_address(
    remote_listen_address: SocketAddr,
    list_machine_addresses: impl FnOnce() -> Vec<HostAddress>,
) -> Vec<HostAddress> {
    let listen_ip_address = remote_listen_address.ip().to_canonical();
    if !listen_ip_address.is_unspecified() {
        return Vec::new();
    }
    list_machine_addresses()
        .into_iter()
        .filter(|host_address| host_address.ip_address.is_ipv4() == listen_ip_address.is_ipv4())
        .collect()
}

/// The scope a `--session` flag names: `None` when the flag is absent, else
/// the id of the one running session the flag resolves to.
///
/// A name matching no running session, or two of them, comes back as the
/// targeting layer's own refusal. The running sessions are those
/// `runtime_directory` advertises: a session another user of this machine
/// started is not found. `<user id>/<session id>` is
/// [`CliError::SessionNotFound`] naming it, or [`CliError::InvalidArgs`] when
/// the user id is this user's.
fn resolve_token_scope(
    runtime_directory: &Path,
    session_reference: Option<&SessionReference>,
) -> Result<Option<TokenScope>, CliError> {
    let Some(session_reference) = session_reference else {
        return Ok(None);
    };
    let discovered_sessions =
        targeting::resolve_session_scope(runtime_directory, None, Some(session_reference))?;
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
