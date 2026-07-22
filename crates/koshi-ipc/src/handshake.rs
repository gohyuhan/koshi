//! The server side of a connection's opening handshake.
//!
//! Every connection must open with
//! [`IpcRequestKind::Hello`](crate::protocol::IpcRequestKind::Hello), which
//! names the protocol version the caller speaks and presents the
//! [`ConnectionToken`](crate::protocol::ConnectionToken) read from the
//! endpoint file. The file lives in the private (`0700`) runtime directory,
//! so holding the token proves the caller is the user who started this
//! Koshi. A [`Handshake`](crate::handshake::Handshake) holds that check for
//! one connection: the server feeds it every incoming request kind, and it
//! answers with "serve it" or with the exact refusal to send back.

use crate::protocol::{
    ConnectionToken, IpcErrorCode, IpcErrorPayload, IpcRequestKind, PROTOCOL_VERSION,
};

/// One connection's handshake gate, held by the server for the connection's
/// lifetime. Starts closed; an [`IpcRequestKind::Hello`] carrying the right
/// protocol version and token opens it, and every other request kind is
/// served only while it is open.
#[derive(Debug)]
pub struct Handshake {
    /// The token this Koshi wrote to its endpoint file; a Hello must present
    /// an equal one.
    expected: ConnectionToken,
    /// True once a Hello has been accepted on this connection.
    open: bool,
}

impl Handshake {
    /// A gate for one newly accepted connection, closed until a Hello opens
    /// it.
    #[must_use]
    pub fn new(expected: ConnectionToken) -> Handshake {
        Handshake {
            expected,
            open: false,
        }
    }

    /// Check one incoming request kind against the connection's state.
    ///
    /// A [`Hello`](IpcRequestKind::Hello) is checked version first, then
    /// token: a version other than [`PROTOCOL_VERSION`] is refused as
    /// [`UnsupportedVersion`](IpcErrorCode::UnsupportedVersion) with both
    /// versions named, a token that does not equal this Koshi's is refused as
    /// [`BadToken`](IpcErrorCode::BadToken), and a Hello passing both checks
    /// opens the gate. Any other kind is accepted while the gate is open and
    /// refused as [`HelloRequired`](IpcErrorCode::HelloRequired) while it is
    /// not.
    ///
    /// `Ok(())` means the caller serves the request — a Hello is answered
    /// with [`IpcResult::Hello`](crate::protocol::IpcResult::Hello). An `Err`
    /// carries the refusal to send back, and the gate keeps the state it had.
    pub fn check(&mut self, kind: &IpcRequestKind) -> Result<(), IpcErrorPayload> {
        match kind {
            IpcRequestKind::Hello {
                protocol_version,
                token,
            } => {
                if *protocol_version != PROTOCOL_VERSION {
                    return Err(IpcErrorPayload {
                        code: IpcErrorCode::UnsupportedVersion,
                        message: format!(
                            "the caller speaks protocol version {protocol_version}, \
                             this Koshi speaks {PROTOCOL_VERSION}"
                        ),
                    });
                }
                if *token != self.expected {
                    return Err(IpcErrorPayload {
                        code: IpcErrorCode::BadToken,
                        message: "the token presented does not match this Koshi's".to_string(),
                    });
                }
                self.open = true;
                Ok(())
            }
            other => {
                if self.open {
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
