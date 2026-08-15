//! What every one-shot exchange with a running koshi does the same way.
//!
//! The CLI talks to two peers over the same framing: a session server, on that
//! session's control socket, and the router, on the router's. Both exchanges
//! settle a version at the Hello, unwrap an answer that may name a result this
//! build does not have, and turn a transport fault into the one error the CLI
//! reports. Those four steps are here once.
//!
//! What differs between the two peers is only what they are called and which
//! versions they speak, which is what [`PeerWords`](crate::talk::PeerWords) carries. The session's
//! refusal reads `the session settled on protocol version 3, …` and the
//! router's reads `the router settled on control-plane protocol version 3, …`
//! — the same sentence, each in its own peer's terms.
//!
//! What a request means, and what its answer is worth, stays with the caller.

use koshi_core::compat::Surface;
use koshi_ipc::error::IpcError;
use koshi_ipc::protocol::IpcErrorPayload;
use koshi_ipc::wire::{Answer, MaybeKnown, WireName};

use crate::error::CliError;

/// What one peer is called, and which versions of its protocol this build
/// speaks.
///
/// The two values a message needs, so a refusal from either peer reads in that
/// peer's own terms.
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
    /// range is not one this koshi offered, so the exchange stops here.
    ///
    /// Example — this build asks for 2 to 2 and the reply names 3, so the verb
    /// fails with `the session settled on protocol version 3, which is outside
    /// the 2 to 2 this koshi asked for`.
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

    /// The answer inside a response, or an error when the peer named a result
    /// this build does not have.
    ///
    /// A result kind this build has no name for is a protocol violation, so
    /// the verb fails.
    pub fn take_result<R>(&self, response: Answer<MaybeKnown<R>>) -> Result<R, CliError> {
        match response.result {
            MaybeKnown::Known(result) => Ok(result),
            MaybeKnown::Unknown { name } => Err(self.unexpected_name(&name)),
        }
    }

    /// The peer answered with a result kind the request cannot produce — a
    /// protocol violation, not an outcome.
    pub fn unexpected_reply<R: WireName>(&self, result: &R) -> CliError {
        self.unexpected_name(result.wire_name())
    }

    /// The same failure, named by the reply's wire name alone — for a reply
    /// this build has no variant for.
    pub fn unexpected_name(&self, name: &str) -> CliError {
        CliError::IpcUnavailable {
            detail: format!("the {} answered with an unexpected {name} reply", self.peer),
        }
    }
}

/// A transport failure mid-exchange: the peer was reachable but the
/// conversation could not finish.
///
/// The same sentence for either peer, since it names the fault and not who was
/// on the other end.
pub fn talk_failed(error: IpcError) -> CliError {
    CliError::IpcUnavailable {
        detail: error.to_string(),
    }
}

/// The peer refused a request at the protocol level — a bad token, a version
/// mismatch, or a request it could not read.
pub fn refused(refusal: &IpcErrorPayload) -> CliError {
    CliError::IpcUnavailable {
        detail: refusal.message.clone(),
    }
}

#[cfg(test)]
mod tests;
