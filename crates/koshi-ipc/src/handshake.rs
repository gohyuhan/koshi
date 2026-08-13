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
//!
//! The rule itself — settle the version, check the token, open the gate, and
//! refuse every other kind until it is open — is one `VersionGate`, shared
//! with the control-plane gate
//! [`RouterHandshake`](crate::router::RouterHandshake) and the supervisor-link
//! gate [`SupervisorHandshake`](crate::supervisor::SupervisorHandshake). Each
//! protocol carries its own version range and its own words in `GateWords`, so
//! the three refusals read in each protocol's own terms.

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

/// What one protocol calls itself in its gate's refusals, and the versions
/// that protocol speaks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GateWords {
    /// The side the gate serves, written as "this {peer}", e.g. `"Koshi"`,
    /// `"router"`, `"supervisor"`.
    pub(crate) peer: &'static str,
    /// Whose token a Hello must present, e.g. `"this Koshi's"`,
    /// `"the router's"`.
    pub(crate) token_owner: &'static str,
    /// The side the gate judges, e.g. `"caller"`, `"session server"`.
    pub(crate) caller: &'static str,
    /// What this protocol calls its version numbers, e.g.
    /// `"protocol versions"`, `"control-plane protocol versions"`.
    pub(crate) versions: &'static str,
    /// What this protocol calls one accepted connection, e.g. `"connection"`,
    /// `"link"`.
    pub(crate) channel: &'static str,
    /// The lowest version of this protocol that this build speaks.
    pub(crate) min_version: u32,
    /// The highest version of this protocol that this build speaks.
    pub(crate) max_version: u32,
}

/// The handshake rule every protocol's gate runs, held for one connection's
/// lifetime: the token a Hello must present, the version settled once one is
/// accepted, and the words this protocol refuses in.
#[derive(Debug)]
pub(crate) struct VersionGate {
    /// The token this build wrote to its endpoint file; a Hello that is asked
    /// for a token must present an equal one.
    expected: ConnectionToken,
    /// The protocol version settled for this connection, once a Hello has
    /// been accepted on it.
    agreed: Option<u32>,
    /// What this protocol calls itself and the versions it speaks.
    words: GateWords,
}

impl VersionGate {
    /// A gate for one newly accepted connection, closed until a Hello opens
    /// it.
    pub(crate) fn new(expected: ConnectionToken, words: GateWords) -> VersionGate {
        VersionGate {
            expected,
            agreed: None,
            words,
        }
    }

    /// The protocol version this connection settled on, or `None` while no
    /// Hello has been accepted.
    pub(crate) fn agreed(&self) -> Option<u32> {
        self.agreed
    }

    /// The version both sides use, given the range `min` to `max` the caller
    /// speaks: the highest they both have. `Err` names both ranges as
    /// [`UnsupportedVersion`](IpcErrorCode::UnsupportedVersion).
    pub(crate) fn version(&self, min: u32, max: u32) -> Result<u32, IpcErrorPayload> {
        agreed_version(min, max, self.words.min_version, self.words.max_version).ok_or_else(|| {
            IpcErrorPayload {
                code: IpcErrorCode::UnsupportedVersion,
                message: format!(
                    "the {} speaks {} {min} to {max}, this {} speaks {} to {}",
                    self.words.caller,
                    self.words.versions,
                    self.words.peer,
                    self.words.min_version,
                    self.words.max_version
                ),
            }
        })
    }

    /// `Ok(())` when `token` equals the one this build holds, and
    /// [`BadToken`](IpcErrorCode::BadToken) otherwise.
    pub(crate) fn token(&self, token: &ConnectionToken) -> Result<(), IpcErrorPayload> {
        if *token != self.expected {
            return Err(IpcErrorPayload {
                code: IpcErrorCode::BadToken,
                message: format!(
                    "the token presented does not match {}",
                    self.words.token_owner
                ),
            });
        }
        Ok(())
    }

    /// Open the gate on `agreed`, the version this connection settled on.
    pub(crate) fn open(&mut self, agreed: u32) {
        self.agreed = Some(agreed);
    }

    /// Check a Hello whose only rule is its token: the version range first,
    /// then the token, and the gate opens once both pass. A refusal leaves the
    /// gate as it was.
    pub(crate) fn hello(
        &mut self,
        min: u32,
        max: u32,
        token: &ConnectionToken,
    ) -> Result<(), IpcErrorPayload> {
        let agreed = self.version(min, max)?;
        self.token(token)?;
        self.open(agreed);
        Ok(())
    }

    /// Check a request kind that is not a Hello, named `name`: served while
    /// the gate is open, refused as
    /// [`HelloRequired`](IpcErrorCode::HelloRequired) while it is closed.
    pub(crate) fn other(&self, name: &str) -> Result<(), IpcErrorPayload> {
        if self.agreed.is_some() {
            return Ok(());
        }
        Err(self.hello_required(name))
    }

    /// The refusal for a request kind this build does not have, named `name`.
    ///
    /// A closed gate answers [`HelloRequired`](IpcErrorCode::HelloRequired),
    /// the same as any other kind arriving before a Hello, so an unopened
    /// connection learns nothing about which kinds exist. An open gate answers
    /// [`UnsupportedKind`](IpcErrorCode::UnsupportedKind) naming it, and the
    /// connection keeps serving.
    pub(crate) fn refuse_unknown(&self, name: &str) -> IpcErrorPayload {
        if self.agreed.is_none() {
            return self.hello_required(name);
        }
        IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: format!("this {} has no request kind named {name}", self.words.peer),
        }
    }

    /// The refusal a closed gate answers the kind named `name` with.
    fn hello_required(&self, name: &str) -> IpcErrorPayload {
        IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: format!(
                "{name} arrived before a Hello opened the {}",
                self.words.channel
            ),
        }
    }
}

/// What the session protocol's gate calls itself, and the versions it speaks.
const SESSION_WORDS: GateWords = GateWords {
    peer: "Koshi",
    token_owner: "this Koshi's",
    caller: "caller",
    versions: "protocol versions",
    channel: "connection",
    min_version: MIN_PROTOCOL_VERSION,
    max_version: PROTOCOL_VERSION,
};

/// One connection's handshake gate, held by the server for the connection's
/// lifetime. Starts closed; an [`IpcRequestKind::Hello`] whose version range
/// overlaps this build's and which meets its [`Peer`]'s token rule opens it,
/// and every other request kind is served only while it is open.
#[derive(Debug)]
pub struct Handshake {
    /// The version range, the token and the settled version, in the session
    /// protocol's words.
    gate: VersionGate,
    /// Where this connection came from, which decides whether its Hello is
    /// asked for the token at all.
    peer: Peer,
}

impl Handshake {
    /// A gate for one newly accepted connection from `peer`, closed until a
    /// Hello opens it.
    #[must_use]
    pub fn new(expected: ConnectionToken, peer: Peer) -> Handshake {
        Handshake {
            gate: VersionGate::new(expected, SESSION_WORDS),
            peer,
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
        self.gate.agreed()
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
        self.gate.refuse_unknown(name)
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
                let agreed = self
                    .gate
                    .version(*min_protocol_version, *max_protocol_version)?;
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
                    | Peer::Remote => self.gate.token(token)?,
                }
                self.gate.open(agreed);
                Ok(())
            }
            other => self.gate.other(other.name()),
        }
    }
}

#[cfg(test)]
mod tests;
