//! The server side of a connection's opening handshake.
//!
//! Every connection must open with
//! [`IpcRequestKind::Hello`](crate::protocol::IpcRequestKind::Hello), which
//! names the protocol versions the caller speaks and presents a
//! [`ConnectionToken`](crate::protocol::ConnectionToken). This Koshi's token
//! lives in its endpoint file in the private (`0700`) runtime directory, so
//! holding it proves the caller is the user who started this
//! Koshi. A [`Handshake`](crate::handshake::Handshake) holds that check for
//! one connection: the server feeds it every incoming request kind, and it
//! answers with "serve it" or with the exact refusal to send back.
//!
//! Which checks a Hello meets depends on where the connection came from,
//! named by [`Peer`](crate::handshake::Peer). The listener that accepted the
//! connection fills that in from what the OS reports, never from anything the
//! caller sent.

use crate::protocol::{
    agreed_version, ConnectionToken, IpcErrorCode, IpcErrorPayload, IpcRequestKind,
    MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};

/// Where a connection came from, as the listener that accepted it reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peer {
    /// A connection from this machine.
    Local {
        /// Whether the peer process runs as the user who started this Koshi.
        same_user: bool,
        /// Whether `allow-other-users` in `koshi.kdl` is on, letting the other
        /// users of this machine reach this Koshi.
        other_users_allowed: bool,
    },
    /// A connection from another machine.
    Remote,
}

/// One connection's handshake gate, held by the server for the connection's
/// lifetime. Starts closed; an [`IpcRequestKind::Hello`] whose version range
/// overlaps this build's and which meets its [`Peer`]'s token rule opens it,
/// and every other request kind is served only while it is open.
#[derive(Debug)]
pub struct Handshake {
    /// The token this Koshi wrote to its endpoint file; a Hello that is asked
    /// for a token must present an equal one.
    expected: ConnectionToken,
    /// Where this connection came from, which decides whether its Hello is
    /// asked for the token at all.
    peer: Peer,
    /// The protocol version settled for this connection, once a Hello has
    /// been accepted on it.
    agreed: Option<u32>,
}

impl Handshake {
    /// A gate for one newly accepted connection from `peer`, closed until a
    /// Hello opens it.
    #[must_use]
    pub fn new(expected: ConnectionToken, peer: Peer) -> Handshake {
        Handshake {
            expected,
            peer,
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
    /// A [`Hello`](IpcRequestKind::Hello) is checked version first, then the
    /// rule its [`Peer`] carries. A caller whose version range does not
    /// overlap [`MIN_PROTOCOL_VERSION`]`..=`[`PROTOCOL_VERSION`] is refused as
    /// [`UnsupportedVersion`](IpcErrorCode::UnsupportedVersion) with both
    /// ranges named, whatever it came from.
    ///
    /// Then, by peer: [`Peer::Remote`] and a same-user [`Peer::Local`] must
    /// present a token equal to this Koshi's, and are refused as
    /// [`BadToken`](IpcErrorCode::BadToken) otherwise. Another user of this
    /// machine is asked for no token while `allow-other-users` is on, and is
    /// refused as [`OtherUsersOff`](IpcErrorCode::OtherUsersOff) while it is
    /// off.
    ///
    /// A Hello passing its checks settles the connection's version and opens
    /// the gate. Any other kind is accepted while the gate is open and refused
    /// as [`HelloRequired`](IpcErrorCode::HelloRequired) while it is not.
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
                match self.peer {
                    // Another user of this machine is asked for no token, so
                    // the setting alone lets them in.
                    Peer::Local {
                        same_user: false,
                        other_users_allowed,
                    } => {
                        if !other_users_allowed {
                            return Err(IpcErrorPayload {
                                code: IpcErrorCode::OtherUsersOff,
                                message: "this Koshi serves only the user who started it; \
                                          set `allow-other-users #true` in koshi.kdl to let \
                                          the other users of this machine in"
                                    .to_string(),
                            });
                        }
                    }
                    Peer::Local {
                        same_user: true, ..
                    }
                    | Peer::Remote => {
                        if *token != self.expected {
                            return Err(IpcErrorPayload {
                                code: IpcErrorCode::BadToken,
                                message: "the token presented does not match this Koshi's"
                                    .to_string(),
                            });
                        }
                    }
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
