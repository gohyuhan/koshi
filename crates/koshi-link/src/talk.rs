//! What every one-shot exchange with a running koshi does the same way.
//!
//! The CLI talks to two peers over the same framing: a session server, on that
//! session's control socket, and the router, on the router's. Both exchanges
//! read a Hello answer and settle a version from it, unwrap an answer that may
//! name a result this build does not have, and turn a transport fault into the
//! error the CLI reports: `IpcUnavailable`, or `PreviousReleaseServer` for a
//! reply in the envelope of koshi 0.4.0 or older. Those steps are here once.
//!
//! What differs between the two peers is only what they are called and which
//! versions they speak, which is what [`PeerWords`]
//! carries. A settled version of 4 is outside both ranges: the session refuses
//! it with `the session settled on protocol version 4, …` and the router with
//! `the router settled on control-plane protocol version 4, …` — the same
//! sentence, each in its own peer's terms.
//!
//! What a request means, and what its answer is worth, stays with the caller.

use koshi_core::command::CommandResult;
use koshi_core::compat::Surface;
use koshi_core::text::sanitize_reported_text;
use koshi_ipc::error::IpcError;
use koshi_ipc::protocol::{IncomingResponse, IpcErrorCode, IpcErrorPayload, IpcResult};
use koshi_ipc::router::{IncomingRouterResponse, RouterResult};
use koshi_ipc::wire::{Answer, MaybeKnown, WireName};

use crate::error::CliError;

/// What one peer is called, and which versions of its protocol this build
/// speaks.
#[derive(Debug, Clone, Copy)]
pub struct PeerWords {
    /// The far side, written as "the {peer}", e.g. `"session"`, `"router"`.
    pub peer: &'static str,
    /// What this protocol calls one of its version numbers, e.g.
    /// `"protocol version"`, `"control-plane protocol version"`.
    pub version_label: &'static str,
    /// The versions this build speaks of that protocol.
    pub protocol_surface: Surface,
}

/// The session server, on a session's own control socket.
pub const SESSION_PEER_WORDS: PeerWords = PeerWords {
    peer: "session",
    version_label: "protocol version",
    protocol_surface: koshi_core::compat::SESSION_PROTOCOL,
};

/// The router, on the router's socket.
pub(crate) const ROUTER_PEER_WORDS: PeerWords = PeerWords {
    peer: "router",
    version_label: "control-plane protocol version",
    protocol_surface: koshi_core::compat::CONTROL_PROTOCOL,
};

impl PeerWords {
    /// Check the version the peer settled on against the range this build
    /// sent.
    ///
    /// The peer picks from the range the Hello named. A version outside that
    /// range stops the exchange.
    ///
    /// Example — this build asks for 4 to 4 and the reply names 5, so the verb
    /// fails with `the session settled on protocol version 5, which is outside
    /// the 4 to 4 this koshi asked for`.
    pub fn validate_settled_protocol_version(&self, protocol_version: u32) -> Result<(), CliError> {
        let minimum_protocol_version = self.protocol_surface.minimum_version;
        let maximum_protocol_version = self.protocol_surface.maximum_version;
        if (minimum_protocol_version..=maximum_protocol_version).contains(&protocol_version) {
            return Ok(());
        }
        Err(CliError::IpcUnavailable {
            detail: format!(
                "the {} settled on {} {protocol_version}, which is outside the {minimum_protocol_version} to {maximum_protocol_version} \
                 this koshi asked for",
                self.peer, self.version_label
            ),
        })
    }

    /// The answer inside a response. A result kind this build has no name for
    /// fails the verb with [`CliError::IpcUnavailable`].
    ///
    /// `response.request_id` is not read.
    pub fn take_response_result<Response>(
        &self,
        incoming_response: Answer<MaybeKnown<Response>>,
    ) -> Result<Response, CliError> {
        match incoming_response.answer_result {
            MaybeKnown::Known(known_result) => Ok(known_result),
            MaybeKnown::Unknown { variant_name } => {
                Err(self.build_unexpected_wire_name_error(&variant_name))
            }
        }
    }

    /// The failure for a reply of a kind the request cannot produce, named by
    /// `response_result`'s wire name.
    pub fn build_unexpected_reply_error<Response: WireName>(
        &self,
        response_result: &Response,
    ) -> CliError {
        self.build_unexpected_wire_name_error(response_result.get_wire_name())
    }

    /// The same failure named by `wire_name` alone, filtered by
    /// [`sanitize_reported_text`] — for a reply this build has no variant for.
    ///
    /// `SESSION_PEER_WORDS` with `wire_name` `"Rehomed"` gives [`CliError::IpcUnavailable`]
    /// carrying `the session answered with an unexpected Rehomed reply`.
    /// `wire_name` `"\u{1b}[2JRehomed"` gives the same sentence.
    pub fn build_unexpected_wire_name_error(&self, wire_name: &str) -> CliError {
        CliError::IpcUnavailable {
            detail: format!(
                "the {} answered with an unexpected {} reply",
                self.peer,
                sanitize_reported_text(wire_name)
            ),
        }
    }
}

/// A failure to talk to a peer, in the words the fault itself used:
/// [`CliError::IpcUnavailable`] carrying `error.to_string()` filtered by
/// [`sanitize_reported_text`]. [`IpcError::PreviousReleaseAnswer`] is
/// [`CliError::PreviousReleaseServer`] carrying that same sentence.
///
/// The same sentence for either peer — it names the fault, not who was on the
/// other end. [`IpcError::MalformedFrame`] carries the decoder's own message,
/// which quotes the field or variant name the peer sent.
pub fn build_ipc_unavailable_error(ipc_error: IpcError) -> CliError {
    let detail = sanitize_reported_text(&ipc_error.to_string());
    match ipc_error {
        IpcError::PreviousReleaseAnswer => CliError::PreviousReleaseServer { detail },
        _ => CliError::IpcUnavailable { detail },
    }
}

/// `command_result` with a rejection's hint filtered by [`sanitize_reported_text`].
/// An applied result is unchanged, and so is a rejection carrying no hint.
///
/// The hint is written by the session that answered, which is another user's
/// process for a session reached through the shared directory and another
/// machine's for one reached over TLS. A hint of `"\u{1b}[2Jattach first"`
/// comes back as `"[2Jattach first"`.
pub(crate) fn filter_rejection_hint(command_result: CommandResult) -> CommandResult {
    match command_result {
        CommandResult::Rejected {
            command_id,
            reason,
            help,
        } => CommandResult::Rejected {
            command_id,
            reason,
            help: help.as_deref().map(sanitize_reported_text),
        },
        applied_command_result => applied_command_result,
    }
}

/// A refusal the peer sent at the protocol level, carrying `refusal.message`
/// filtered by [`sanitize_reported_text`]: [`CliError::ProtocolVersionRefused`]
/// for [`IpcErrorCode::UnsupportedVersion`], and [`CliError::IpcUnavailable`]
/// for every other code, such as a bad token or a request the peer could not
/// read.
pub fn build_peer_refusal_error(refusal: &IpcErrorPayload) -> CliError {
    let detail = sanitize_reported_text(&refusal.message);
    if refusal.code == IpcErrorCode::UnsupportedVersion {
        CliError::ProtocolVersionRefused { detail }
    } else {
        CliError::IpcUnavailable { detail }
    }
}

/// The version a session settled on and the build it named in its Hello
/// answer, once the version is checked against the range this build sent.
///
/// The build string is written by the session that answered, which is another
/// user's process for a session reached through the shared directory and
/// another machine's for one reached over TLS, and `koshi server-version`
/// prints it. It is filtered by [`sanitize_reported_text`], so a build of
/// `"\u{1b}[2J0.3.0"` comes back as `"[2J0.3.0"`.
///
/// # Errors
/// - A refused Hello: what [`build_peer_refusal_error`] gives, which is
///   [`CliError::ProtocolVersionRefused`] for a refused protocol version.
/// - [`CliError::IpcUnavailable`] when the session settled on a version outside
///   the range this build asked for, or answered anything other than a Hello.
pub(crate) fn parse_session_hello_version(
    incoming_response: IncomingResponse,
) -> Result<(u32, String), CliError> {
    match SESSION_PEER_WORDS.take_response_result(incoming_response)? {
        IpcResult::Hello {
            protocol_version,
            build_version,
        } => {
            SESSION_PEER_WORDS.validate_settled_protocol_version(protocol_version)?;
            Ok((protocol_version, sanitize_reported_text(&build_version)))
        }
        IpcResult::Error(refusal) => Err(build_peer_refusal_error(&refusal)),
        unexpected_result => {
            Err(SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
        }
    }
}

/// The build the router named in its Hello answer, filtered by
/// [`sanitize_reported_text`], once the version it settled on is checked.
///
/// # Errors
/// - A refused Hello: what [`build_peer_refusal_error`] gives, which is
///   [`CliError::ProtocolVersionRefused`] for a refused protocol version.
/// - [`CliError::IpcUnavailable`] when the router settled on a version outside
///   the range this build asked for, or answered anything other than a Hello.
pub(crate) fn parse_router_hello_version(
    incoming_response: IncomingRouterResponse,
) -> Result<String, CliError> {
    match ROUTER_PEER_WORDS.take_response_result(incoming_response)? {
        RouterResult::Hello {
            protocol_version,
            build_version,
        } => {
            ROUTER_PEER_WORDS.validate_settled_protocol_version(protocol_version)?;
            Ok(sanitize_reported_text(&build_version))
        }
        RouterResult::Error(refusal) => Err(build_peer_refusal_error(&refusal)),
        unexpected_result => {
            Err(ROUTER_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
        }
    }
}

#[cfg(test)]
mod tests;
