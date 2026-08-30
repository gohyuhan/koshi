//! What every one-shot exchange with a running koshi does the same way.
//!
//! The CLI talks to two peers over the same framing: a session server, on that
//! session's control socket, and the router, on the router's. Both exchanges
//! read a Hello answer and settle a version from it, unwrap an answer that may
//! name a result this build does not have, and turn a transport fault into the
//! one error the CLI reports. Those steps are here once.
//!
//! What differs between the two peers is only what they are called and which
//! versions they speak, which is what [`PeerWords`](crate::talk::PeerWords)
//! carries. A settled version of 4 is outside both ranges: the session refuses
//! it with `the session settled on protocol version 4, …` and the router with
//! `the router settled on control-plane protocol version 4, …` — the same
//! sentence, each in its own peer's terms.
//!
//! What a request means, and what its answer is worth, stays with the caller.

use koshi_core::compat::Surface;
use koshi_ipc::error::IpcError;
use koshi_ipc::protocol::{IncomingResponse, IpcErrorPayload, IpcResult};
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
    pub versions: &'static str,
    /// The versions this build speaks of that protocol.
    pub surface: Surface,
}

/// The session server, on a session's own control socket.
pub const SESSION: PeerWords = PeerWords {
    peer: "session",
    versions: "protocol version",
    surface: koshi_core::compat::SESSION_PROTOCOL,
};

/// The router, on the router's socket.
pub(crate) const ROUTER: PeerWords = PeerWords {
    peer: "router",
    versions: "control-plane protocol version",
    surface: koshi_core::compat::CONTROL_PROTOCOL,
};

impl PeerWords {
    /// Check the version the peer settled on against the range this build
    /// sent.
    ///
    /// The peer picks from the range the Hello named. A version outside that
    /// range stops the exchange.
    ///
    /// Example — this build asks for 2 to 3 and the reply names 4, so the verb
    /// fails with `the session settled on protocol version 4, which is outside
    /// the 2 to 3 this koshi asked for`.
    pub fn settled_version(&self, protocol_version: u32) -> Result<(), CliError> {
        let (min, max) = (self.surface.min, self.surface.max);
        if (min..=max).contains(&protocol_version) {
            return Ok(());
        }
        Err(CliError::IpcUnavailable {
            detail: format!(
                "the {} settled on {} {protocol_version}, which is outside the {min} to {max} \
                 this koshi asked for",
                self.peer, self.versions
            ),
        })
    }

    /// The answer inside a response. A result kind this build has no name for
    /// fails the verb with [`CliError::IpcUnavailable`].
    ///
    /// `response.request_id` is not read.
    pub fn take_result<R>(&self, response: Answer<MaybeKnown<R>>) -> Result<R, CliError> {
        match response.result {
            MaybeKnown::Known(result) => Ok(result),
            MaybeKnown::Unknown { name } => Err(self.unexpected_name(&name)),
        }
    }

    /// The failure for a reply of a kind the request cannot produce, named by
    /// `result`'s wire name.
    pub fn unexpected_reply<R: WireName>(&self, result: &R) -> CliError {
        self.unexpected_name(result.wire_name())
    }

    /// The same failure named by `name` alone, as the peer spelled it — for a
    /// reply this build has no variant for.
    ///
    /// `SESSION` with `name` `"Rehomed"` gives [`CliError::IpcUnavailable`]
    /// carrying `the session answered with an unexpected Rehomed reply`.
    pub fn unexpected_name(&self, name: &str) -> CliError {
        CliError::IpcUnavailable {
            detail: format!("the {} answered with an unexpected {name} reply", self.peer),
        }
    }
}

/// A failure to talk to a peer, in the words the fault itself used:
/// [`CliError::IpcUnavailable`] carrying `error.to_string()`.
///
/// The same sentence for either peer — it names the fault, not who was on the
/// other end.
pub fn talk_failed(error: IpcError) -> CliError {
    CliError::IpcUnavailable {
        detail: error.to_string(),
    }
}

/// A refusal the peer sent at the protocol level — a bad token, a version
/// mismatch, or a request it could not read — as
/// [`CliError::IpcUnavailable`] carrying `refusal.message` unchanged.
pub fn refused(refusal: &IpcErrorPayload) -> CliError {
    CliError::IpcUnavailable {
        detail: refusal.message.clone(),
    }
}

/// The version a session settled on and the build it named in its Hello
/// answer, once the version is checked against the range this build sent. An
/// empty build string is a session that predates the version field.
///
/// # Errors
/// [`CliError::IpcUnavailable`] when the session settled on a version outside
/// the range this build asked for, refused the Hello, or answered anything
/// other than a Hello.
pub(crate) fn session_hello_version(reply: IncomingResponse) -> Result<(u32, String), CliError> {
    match SESSION.take_result(reply)? {
        IpcResult::Hello {
            protocol_version,
            version,
        } => {
            SESSION.settled_version(protocol_version)?;
            Ok((protocol_version, version))
        }
        IpcResult::Error(refusal) => Err(refused(&refusal)),
        other => Err(SESSION.unexpected_reply(&other)),
    }
}

/// The build the router named in its Hello answer, once the version it settled
/// on is checked. An empty string is a router that predates the version field.
///
/// # Errors
/// [`CliError::IpcUnavailable`] when the router settled on a version outside
/// the range this build asked for, refused the Hello, or answered anything
/// other than a Hello.
pub(crate) fn router_hello_version(reply: IncomingRouterResponse) -> Result<String, CliError> {
    match ROUTER.take_result(reply)? {
        RouterResult::Hello {
            protocol_version,
            version,
        } => {
            ROUTER.settled_version(protocol_version)?;
            Ok(version)
        }
        RouterResult::Error(refusal) => Err(refused(&refusal)),
        other => Err(ROUTER.unexpected_reply(&other)),
    }
}

/// The lowest session protocol version that carries a command's target client
/// on its source. A session that settled below it ignores the field, and a
/// command naming a client is refused before it is sent.
pub(crate) const TARGET_CLIENT_PROTOCOL: u32 = 3;

/// Check the version a session settled on against the lowest one this command
/// needs. `least` `None` accepts every settled version.
///
/// # Errors
/// [`CliError::IpcUnavailable`] when `least` is `Some(v)` and `settled` is
/// below `v`. For `settled == 2` the sentence reads `this session speaks
/// protocol 2; --client needs a session started by koshi 0.4.0 or later`. It
/// names `--client` and koshi 0.4.0 whatever `least` holds.
pub(crate) fn require_settled_version(settled: u32, least: Option<u32>) -> Result<(), CliError> {
    match least {
        Some(least) if settled < least => Err(CliError::IpcUnavailable {
            detail: format!(
                "this session speaks protocol {settled}; --client needs a session started by \
                 koshi 0.4.0 or later"
            ),
        }),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests;
