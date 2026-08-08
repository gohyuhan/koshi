//! The server side of a connection's opening handshake.
//!
//! Every connection must open with
//! [`IpcRequestKind::Hello`](crate::protocol::IpcRequestKind::Hello), which
//! names the protocol versions the caller speaks and presents the
//! [`ConnectionToken`](crate::protocol::ConnectionToken) read from the
//! endpoint file. The file lives in the private (`0700`) runtime directory,
//! so holding the token proves the caller is the user who started this
//! Koshi. A [`Handshake`](crate::handshake::Handshake) holds that check for
//! one connection: the server feeds it every incoming request kind, and it
//! answers with "serve it" or with the exact refusal to send back.

use crate::protocol::{
    agreed_version, ConnectionToken, IpcErrorCode, IpcErrorPayload, IpcRequestKind,
    MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};

/// One connection's handshake gate, held by the server for the connection's
/// lifetime. Starts closed; an [`IpcRequestKind::Hello`] whose version range
/// overlaps this build's and whose token matches opens it, and every other
/// request kind is served only while it is open.
#[derive(Debug)]
pub struct Handshake {
    /// The token this Koshi wrote to its endpoint file; a Hello must present
    /// an equal one.
    expected: ConnectionToken,
    /// The protocol version settled for this connection, once a Hello has
    /// been accepted on it.
    agreed: Option<u32>,
}

impl Handshake {
    /// A gate for one newly accepted connection, closed until a Hello opens
    /// it.
    #[must_use]
    pub fn new(expected: ConnectionToken) -> Handshake {
        Handshake {
            expected,
            agreed: None,
        }
    }

    /// The protocol version this connection settled on, or `None` while no
    /// Hello has been accepted.
    ///
    /// The server puts it in
    /// [`IpcResult::Hello`](crate::protocol::IpcResult::Hello), so the caller
    /// learns which version the two of them use.
    #[must_use]
    pub fn agreed(&self) -> Option<u32> {
        self.agreed
    }

    /// The refusal for a request kind this build does not have, named `name`.
    ///
    /// A closed gate answers [`HelloRequired`](IpcErrorCode::HelloRequired),
    /// the same as any other kind arriving before a Hello, so an unopened
    /// connection learns nothing about which kinds exist. An open gate answers
    /// [`UnsupportedKind`](IpcErrorCode::UnsupportedKind) naming it, and the
    /// connection keeps serving.
    #[must_use]
    pub fn refuse_unknown(&self, name: &str) -> IpcErrorPayload {
        if self.agreed.is_none() {
            return IpcErrorPayload {
                code: IpcErrorCode::HelloRequired,
                message: format!("{name} arrived before a Hello opened the connection"),
            };
        }
        IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: format!("this Koshi has no request kind named {name}"),
        }
    }

    /// Check one incoming request kind against the connection's state.
    ///
    /// A [`Hello`](IpcRequestKind::Hello) is checked version first, then
    /// token: a caller whose version range does not overlap
    /// [`MIN_PROTOCOL_VERSION`]`..=`[`PROTOCOL_VERSION`] is refused as
    /// [`UnsupportedVersion`](IpcErrorCode::UnsupportedVersion) with both
    /// ranges named, a token that does not equal this Koshi's is refused as
    /// [`BadToken`](IpcErrorCode::BadToken), and a Hello passing both checks
    /// settles the connection's version and opens the gate. Any other kind is
    /// accepted while the gate is open and refused as
    /// [`HelloRequired`](IpcErrorCode::HelloRequired) while it is not.
    ///
    /// A second Hello re-settles the version. Both Hellos come from the same
    /// caller speaking the same range, so the answer is the one already
    /// settled.
    ///
    /// `Ok(())` means the caller serves the request — a Hello is answered
    /// with [`IpcResult::Hello`](crate::protocol::IpcResult::Hello) carrying
    /// [`agreed`](Self::agreed). An `Err` carries the refusal to send back,
    /// and the gate keeps the state it had.
    pub fn check(&mut self, kind: &IpcRequestKind) -> Result<(), IpcErrorPayload> {
        match kind {
            IpcRequestKind::Hello {
                min_protocol_version,
                max_protocol_version,
                token,
            } => {
                let Some(agreed) = agreed_version(
                    *min_protocol_version,
                    *max_protocol_version,
                    MIN_PROTOCOL_VERSION,
                    PROTOCOL_VERSION,
                ) else {
                    return Err(IpcErrorPayload {
                        code: IpcErrorCode::UnsupportedVersion,
                        message: format!(
                            "the caller speaks protocol versions \
                             {min_protocol_version} to {max_protocol_version}, \
                             this Koshi speaks {MIN_PROTOCOL_VERSION} to {PROTOCOL_VERSION}"
                        ),
                    });
                };
                if *token != self.expected {
                    return Err(IpcErrorPayload {
                        code: IpcErrorCode::BadToken,
                        message: "the token presented does not match this Koshi's".to_string(),
                    });
                }
                self.agreed = Some(agreed);
                Ok(())
            }
            other => {
                if self.agreed.is_some() {
                    Ok(())
                } else {
                    Err(IpcErrorPayload {
                        code: IpcErrorCode::HelloRequired,
                        message: format!(
                            "{} arrived before a Hello opened the connection",
                            other.name()
                        ),
                    })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
