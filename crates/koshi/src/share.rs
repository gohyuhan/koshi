//! The `koshi share` commands: hand one identity a remote access token, stop
//! the tokens an identity holds, and list the grants this machine has made.
//!
//! Every verb reaches the router over the control plane; the router is the
//! only writer of the token store. A `--session` flag names the one
//! session a grant reaches; the session is resolved here, through the same
//! targeting the discovery queries use, so a name that matches no running
//! session or two of them is refused before anything is asked of the router.

use std::path::Path;

use koshi_ipc::remote_tokens::TokenScope;
use koshi_ipc::router::{RouterRequestKind, RouterResult};
use koshi_ipc::wire::WireName;

use crate::cli::{Expiry, SessionRef, ShareCommand};
use crate::error::CliError;
use crate::{ipc_client, output, router_client, targeting};

#[cfg(test)]
mod tests;

/// Run one `share` verb: resolve the scope it names, ask the router, and
/// print the rendered answer.
///
/// A grant with no `--session` reaches every session on this machine; a
/// revoke or a listing with no `--session` covers every scope. A router that
/// refuses the request is [`CliError::Runtime`] carrying the router's own
/// message.
pub fn run(command: &ShareCommand) -> Result<(), CliError> {
    let runtime_dir = ipc_client::runtime_dir()?;
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
                    print!(
                        "{}",
                        output::render_share_grant(&token, identity, &scope, replaced)
                    );
                    Ok(())
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
