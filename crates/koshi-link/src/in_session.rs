//! In-session detection: reading the `KOSHI_*` variables at CLI startup.
//!
//! A `koshi` CLI run inside a koshi pane inherits the identity variables the
//! runtime injected at pane spawn: `KOSHI` (the in-session marker),
//! `KOSHI_SESSION_ID`, `KOSHI_PANE_ID`, and — when known at spawn —
//! `KOSHI_CLIENT_ID`.
//! [`InSessionContext::from_env`](crate::in_session::InSessionContext::from_env) reads
//! them once at startup. `KOSHI` absent means the CLI runs outside any
//! session (external mode). `KOSHI` present means the CLI claims in-session
//! identity, so the required variables must be present and well-formed; a
//! broken remainder is an error, never silently treated as external mode.
//!
//! The connection secret is not part of the environment: the CLI reads the
//! token from the session's endpoint file (`EndpointFile` in `koshi-ipc`)
//! when it connects.

use koshi_core::ids::{parse_prefixed_uuid, ClientId, PaneId, SessionId};
use uuid::Uuid;

use crate::error::CliError;

/// The in-session identity a `koshi` CLI inherits from its pane's
/// environment.
///
/// The values are the spawn-time ids of the pane the CLI runs inside; the
/// runtime re-validates them against live state when the CLI presents them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InSessionContext {
    /// The session the pane belongs to (`KOSHI_SESSION_ID`).
    pub session_id: SessionId,
    /// The client designated to view the pane at spawn (`KOSHI_CLIENT_ID`);
    /// a pane created with no designated client carries none.
    pub client_id: Option<ClientId>,
    /// The pane the CLI runs inside (`KOSHI_PANE_ID`).
    pub pane_id: PaneId,
}

impl InSessionContext {
    /// Read the in-session identity from this process's environment.
    ///
    /// Returns `Ok(None)` when `KOSHI` is not set (external mode),
    /// `Ok(Some(_))` when the full identity is present and well-formed, and
    /// [`CliError::InSessionEnv`] when `KOSHI` is set but the rest of the
    /// identity is missing or malformed. Presence of `KOSHI` is the marker;
    /// its value is not inspected.
    pub fn from_env() -> Result<Option<InSessionContext>, CliError> {
        Self::from_lookup(|environment_variable_name| {
            std::env::var_os(environment_variable_name).map(|environment_variable_value| {
                environment_variable_value.to_string_lossy().into_owned()
            })
        })
    }

    /// Build the identity from one lookup per environment variable name.
    fn from_lookup(
        lookup_environment_variable: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<InSessionContext>, CliError> {
        if lookup_environment_variable("KOSHI").is_none() {
            return Ok(None);
        }
        let session_id = parse_required_environment_variable_id(
            &lookup_environment_variable,
            "KOSHI_SESSION_ID",
            "session",
        )
        .map(SessionId::from_uuid)?;
        let client_id = parse_optional_environment_variable_id(
            &lookup_environment_variable,
            "KOSHI_CLIENT_ID",
            "client",
        )?
        .map(ClientId::from_uuid);
        let pane_id = parse_required_environment_variable_id(
            &lookup_environment_variable,
            "KOSHI_PANE_ID",
            "pane",
        )
        .map(PaneId::from_uuid)?;
        Ok(Some(InSessionContext {
            session_id,
            client_id,
            pane_id,
        }))
    }
}

/// Parse a variable the in-session identity requires: missing or malformed
/// reports [`CliError::InSessionEnv`] naming the variable.
fn parse_required_environment_variable_id(
    lookup_environment_variable: &impl Fn(&str) -> Option<String>,
    environment_variable_name: &str,
    identifier_prefix: &str,
) -> Result<Uuid, CliError> {
    let environment_variable_value = lookup_environment_variable(environment_variable_name)
        .ok_or_else(|| CliError::InSessionEnv {
            detail: format!("`KOSHI` is set but `{environment_variable_name}` is missing"),
        })?;
    parse_environment_variable_value(
        environment_variable_name,
        &environment_variable_value,
        identifier_prefix,
    )
}

/// Parse a variable the in-session identity may omit: absent is `Ok(None)`,
/// present-but-malformed reports [`CliError::InSessionEnv`].
fn parse_optional_environment_variable_id(
    lookup_environment_variable: &impl Fn(&str) -> Option<String>,
    environment_variable_name: &str,
    identifier_prefix: &str,
) -> Result<Option<Uuid>, CliError> {
    lookup_environment_variable(environment_variable_name)
        .map(|environment_variable_value| {
            parse_environment_variable_value(
                environment_variable_name,
                &environment_variable_value,
                identifier_prefix,
            )
        })
        .transpose()
}

/// Parse one variable's value as a `<prefix>-<uuid>` id or a bare UUID,
/// reporting the variable name and the offending value on failure.
fn parse_environment_variable_value(
    environment_variable_name: &str,
    environment_variable_value: &str,
    identifier_prefix: &str,
) -> Result<Uuid, CliError> {
    parse_prefixed_uuid(environment_variable_value, identifier_prefix).map_err(|expected_format| {
        CliError::InSessionEnv {
            detail: format!(
                "`{environment_variable_name}` is `{environment_variable_value}`: {expected_format}"
            ),
        }
    })
}

#[cfg(test)]
mod tests;
