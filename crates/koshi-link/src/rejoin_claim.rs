//! The client record a `koshi attach` started by another koshi build asks its
//! session to hand back.
//!
//! A client whose session restarted into a build that refuses its protocol
//! version runs `koshi attach <session id>` from the path it was started from.
//! It sets [`REJOIN_CLIENT_ID_VARIABLE_NAME`] and [`REJOIN_TOKEN_VARIABLE_NAME`]
//! on that command, and the command reads them once with
//! [`RejoinClaim::from_env`]. A koshi build that does not know the variables
//! ignores them, and its attach takes a new client.

use koshi_core::ids::ClientId;
use koshi_ipc::protocol::ConnectionToken;

use crate::environment_variable::{parse_environment_variable_id, read_environment_variable};
use crate::error::CliError;

#[cfg(test)]
mod tests;

/// The environment variable that holds the id of the client to come back as,
/// as `client-<uuid>`: `KOSHI_REJOIN_CLIENT_ID`.
pub const REJOIN_CLIENT_ID_VARIABLE_NAME: &str = "KOSHI_REJOIN_CLIENT_ID";

/// The environment variable that holds the resume token the last attach of
/// that client minted: `KOSHI_REJOIN_TOKEN`.
pub const REJOIN_TOKEN_VARIABLE_NAME: &str = "KOSHI_REJOIN_TOKEN";

/// A client record to ask a session for, and the resume token that proves the
/// claim. The session hands the record back when it still holds it, the tab
/// it views still exists, no connection streams for it, and the token is the
/// one its last attach minted. It mints a new client otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejoinClaim {
    /// The client to come back as.
    pub client_id: ClientId,
    /// The secret the last attach of `client_id` minted.
    pub resume_token: ConnectionToken,
}

impl RejoinClaim {
    /// Read the claim from this process's environment.
    ///
    /// Neither variable set gives `Ok(None)`. Both set gives the claim: the
    /// id is read as `client-<uuid>` or a bare UUID, and the token is taken
    /// as it stands. A value that is not UTF-8 is read with each bad sequence
    /// replaced by `U+FFFD`.
    ///
    /// # Errors
    /// [`CliError::InvalidArgs`]. One variable set without the other gives
    /// `` `KOSHI_REJOIN_CLIENT_ID` is set and `KOSHI_REJOIN_TOKEN` is not ``,
    /// and the reverse. An id that does not read gives
    /// `` `KOSHI_REJOIN_CLIENT_ID` is `pane-1`: expected `client-<uuid>` or a
    /// bare UUID ``.
    pub fn from_env() -> Result<Option<RejoinClaim>, CliError> {
        Self::from_environment_lookup(read_environment_variable)
    }

    /// Build the claim from one lookup per environment variable name, as
    /// [`RejoinClaim::from_env`] states.
    fn from_environment_lookup(
        lookup_environment_variable: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<RejoinClaim>, CliError> {
        let client_id_text = lookup_environment_variable(REJOIN_CLIENT_ID_VARIABLE_NAME);
        let resume_token_text = lookup_environment_variable(REJOIN_TOKEN_VARIABLE_NAME);
        let (client_id_text, resume_token_text) = match (client_id_text, resume_token_text) {
            (None, None) => return Ok(None),
            (Some(client_id_text), Some(resume_token_text)) => (client_id_text, resume_token_text),
            (Some(_), None) => {
                return Err(CliError::InvalidArgs {
                    detail: format!(
                        "`{REJOIN_CLIENT_ID_VARIABLE_NAME}` is set and `{REJOIN_TOKEN_VARIABLE_NAME}` is not"
                    ),
                })
            }
            (None, Some(_)) => {
                return Err(CliError::InvalidArgs {
                    detail: format!(
                        "`{REJOIN_TOKEN_VARIABLE_NAME}` is set and `{REJOIN_CLIENT_ID_VARIABLE_NAME}` is not"
                    ),
                })
            }
        };
        let client_uuid = parse_environment_variable_id(
            REJOIN_CLIENT_ID_VARIABLE_NAME,
            &client_id_text,
            "client",
        )
        .map_err(|detail| CliError::InvalidArgs { detail })?;
        Ok(Some(RejoinClaim {
            client_id: ClientId::from_uuid(client_uuid),
            resume_token: ConnectionToken::from_secret(resume_token_text),
        }))
    }
}
