//! The server side of a connection's opening handshake.
//!
//! Every connection must open with
//! [`IpcRequestKind::Hello`](crate::protocol::IpcRequestKind::Hello), which
//! names the protocol versions the caller speaks and presents a
//! [`ConnectionToken`](crate::protocol::ConnectionToken). This Koshi's token
//! lives in its endpoint file in the private (`0700`) runtime directory;
//! presenting it proves the caller is the user who started this Koshi. A
//! [`Handshake`](crate::handshake::Handshake) holds that check for one
//! connection: the server feeds it every incoming request kind, and it
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
//! protocol carries its own version range and its own words in `GateWords`,
//! and each refusal reads in that protocol's own terms.

use crate::protocol::{
    compute_agreed_protocol_version, ConnectionToken, IpcErrorCode, IpcErrorPayload,
    IpcRequestKind, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};

/// Where a connection came from, as the listener that accepted it reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peer {
    /// A connection from this machine.
    Local {
        /// Whether the peer process runs as the user who started this Koshi.
        is_same_user: bool,
        /// Whether `allow-other-users` in `koshi.kdl` is on, letting the other
        /// users of this machine reach this Koshi.
        is_other_user_access_allowed: bool,
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
    pub(crate) minimum_protocol_version: u32,
    /// The highest version of this protocol that this build speaks.
    pub(crate) maximum_protocol_version: u32,
}

/// The handshake rule every protocol's gate runs, held for one connection's
/// lifetime: the token a Hello must present, the version settled once one is
/// accepted, and the words this protocol refuses in.
#[derive(Debug)]
pub(crate) struct VersionGate {
    /// The token this build wrote to its endpoint file; a Hello that is asked
    /// for a token must present an equal one.
    expected_connection_token: ConnectionToken,
    /// The protocol version settled for this connection, once a Hello has
    /// been accepted on it.
    agreed_protocol_version: Option<u32>,
    /// What this protocol calls itself and the versions it speaks.
    words: GateWords,
}

impl VersionGate {
    /// A gate for one newly accepted connection, closed until a Hello opens
    /// it.
    pub(crate) fn from_expected_token_and_words(
        expected_connection_token: ConnectionToken,
        words: GateWords,
    ) -> VersionGate {
        VersionGate {
            expected_connection_token,
            agreed_protocol_version: None,
            words,
        }
    }

    /// The protocol version this connection settled on, or `None` while no
    /// Hello has been accepted.
    pub(crate) fn get_agreed_protocol_version(&self) -> Option<u32> {
        self.agreed_protocol_version
    }

    /// The version both sides use, given the caller's range from
    /// `caller_minimum_protocol_version` to `caller_maximum_protocol_version`
    /// speaks: the highest they both have. `Err` names both ranges as
    /// [`UnsupportedVersion`](IpcErrorCode::UnsupportedVersion).
    pub(crate) fn negotiate_protocol_version(
        &self,
        caller_minimum_protocol_version: u32,
        caller_maximum_protocol_version: u32,
    ) -> Result<u32, IpcErrorPayload> {
        compute_agreed_protocol_version(
            caller_minimum_protocol_version,
            caller_maximum_protocol_version,
            self.words.minimum_protocol_version,
            self.words.maximum_protocol_version,
        )
        .ok_or_else(|| IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: format!(
                "the {} speaks {} {caller_minimum_protocol_version} to \
                     {caller_maximum_protocol_version}, this {} speaks {} to {}",
                self.words.caller,
                self.words.versions,
                self.words.peer,
                self.words.minimum_protocol_version,
                self.words.maximum_protocol_version
            ),
        })
    }

    /// `Ok(())` when `connection_token` equals the one this build holds, and
    /// [`BadToken`](IpcErrorCode::BadToken) otherwise.
    pub(crate) fn validate_connection_token(
        &self,
        connection_token: &ConnectionToken,
    ) -> Result<(), IpcErrorPayload> {
        if *connection_token != self.expected_connection_token {
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

    /// Open the gate on `agreed_protocol_version`, the version this connection settled on.
    pub(crate) fn set_agreed_protocol_version(&mut self, agreed_protocol_version: u32) {
        self.agreed_protocol_version = Some(agreed_protocol_version);
    }

    /// Check a Hello whose only rule is its token: the version range first,
    /// then the token, and the gate opens once both pass. A refusal leaves the
    /// gate as it was.
    pub(crate) fn validate_hello(
        &mut self,
        caller_minimum_protocol_version: u32,
        caller_maximum_protocol_version: u32,
        connection_token: &ConnectionToken,
    ) -> Result<(), IpcErrorPayload> {
        let agreed_protocol_version = self.negotiate_protocol_version(
            caller_minimum_protocol_version,
            caller_maximum_protocol_version,
        )?;
        self.validate_connection_token(connection_token)?;
        self.set_agreed_protocol_version(agreed_protocol_version);
        Ok(())
    }

    /// Check a request kind that is not a Hello, named `name`: served while
    /// the gate is open, refused as
    /// [`HelloRequired`](IpcErrorCode::HelloRequired) while it is closed.
    pub(crate) fn validate_non_hello_request_kind(
        &self,
        request_kind_name: &str,
    ) -> Result<(), IpcErrorPayload> {
        if self.agreed_protocol_version.is_some() {
            return Ok(());
        }
        Err(self.build_hello_required_error(request_kind_name))
    }

    /// The refusal for a request kind this build does not have, named `name`.
    ///
    /// A closed gate answers [`HelloRequired`](IpcErrorCode::HelloRequired),
    /// the same as any other kind arriving before a Hello; an unopened
    /// connection is told nothing about which kinds exist. An open gate
    /// answers [`UnsupportedKind`](IpcErrorCode::UnsupportedKind) naming it,
    /// and the connection keeps serving.
    pub(crate) fn build_unknown_request_kind_error(
        &self,
        request_kind_name: &str,
    ) -> IpcErrorPayload {
        if self.agreed_protocol_version.is_none() {
            return self.build_hello_required_error(request_kind_name);
        }
        IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: format!(
                "this {} has no request kind named {request_kind_name}",
                self.words.peer
            ),
        }
    }

    /// The refusal a closed gate answers the kind named `name` with.
    fn build_hello_required_error(&self, request_kind_name: &str) -> IpcErrorPayload {
        IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: format!(
                "{request_kind_name} arrived before a Hello opened the {}",
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
    minimum_protocol_version: MIN_PROTOCOL_VERSION,
    maximum_protocol_version: PROTOCOL_VERSION,
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
    /// Whether a Hello on this connection said it carries a caller on another
    /// machine. Latched: once a Hello sets it, no later Hello clears it.
    is_remote_caller: bool,
}

impl Handshake {
    /// A gate for one newly accepted connection from `peer`, closed until a
    /// Hello opens it.
    #[must_use]
    pub fn from_expected_token_and_peer(
        expected_connection_token: ConnectionToken,
        peer: Peer,
    ) -> Handshake {
        Handshake {
            gate: VersionGate::from_expected_token_and_words(
                expected_connection_token,
                SESSION_WORDS,
            ),
            peer,
            is_remote_caller: false,
        }
    }

    /// Whether this connection carries a caller on another machine.
    ///
    /// `false` until an accepted Hello says otherwise, and `true` from the
    /// first accepted Hello that does. A refused Hello leaves this unchanged,
    /// and a later accepted Hello saying `false` leaves this `true`.
    #[must_use]
    pub fn is_remote_caller(&self) -> bool {
        self.is_remote_caller
    }

    /// The protocol version this connection settled on, or `None` while no
    /// Hello has been accepted.
    ///
    /// The server puts it in
    /// [`IpcResult::Hello`](crate::protocol::IpcResult::Hello), so the caller
    /// learns which version the two of them use.
    #[must_use]
    pub fn get_agreed_protocol_version(&self) -> Option<u32> {
        self.gate.get_agreed_protocol_version()
    }

    /// The refusal for a request kind this build does not have, named `name`.
    ///
    /// A closed gate answers [`HelloRequired`](IpcErrorCode::HelloRequired),
    /// the same as any other kind arriving before a Hello; an unopened
    /// connection is told nothing about which kinds exist. An open gate
    /// answers [`UnsupportedKind`](IpcErrorCode::UnsupportedKind) naming it,
    /// and the connection keeps serving.
    #[must_use]
    pub fn build_unknown_request_kind_error(&self, request_kind_name: &str) -> IpcErrorPayload {
        self.gate
            .build_unknown_request_kind_error(request_kind_name)
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
    /// A second accepted Hello settles the version again from its own range.
    ///
    /// `Ok(())` means the caller serves the request — a Hello is answered
    /// with [`IpcResult::Hello`](crate::protocol::IpcResult::Hello) carrying
    /// [`get_agreed_protocol_version`](Self::get_agreed_protocol_version). An `Err` carries the refusal to send back,
    /// and the gate keeps the state it had.
    pub fn validate_request_kind(
        &mut self,
        request_kind: &IpcRequestKind,
    ) -> Result<(), IpcErrorPayload> {
        match request_kind {
            IpcRequestKind::Hello {
                min_protocol_version,
                max_protocol_version,
                connection_token,
                is_remote,
            } => {
                let agreed_protocol_version = self
                    .gate
                    .negotiate_protocol_version(*min_protocol_version, *max_protocol_version)?;
                match self.peer {
                    // Another user of this machine is asked for no token; the
                    // setting alone decides.
                    Peer::Local {
                        is_same_user: false,
                        is_other_user_access_allowed: false,
                    } => {
                        return Err(IpcErrorPayload {
                            code: IpcErrorCode::OtherUsersOff,
                            message: "this Koshi serves only the user who started it; \
                                      set `allow-other-users #true` in koshi.kdl to let \
                                      the other users of this machine in"
                                .to_string(),
                        });
                    }
                    Peer::Local {
                        is_same_user: false,
                        is_other_user_access_allowed: true,
                    } => {}
                    Peer::Local {
                        is_same_user: true, ..
                    }
                    | Peer::Remote => self.gate.validate_connection_token(connection_token)?,
                }
                self.is_remote_caller |= *is_remote;
                self.gate
                    .set_agreed_protocol_version(agreed_protocol_version);
                Ok(())
            }
            request_kind => self
                .gate
                .validate_non_hello_request_kind(request_kind.get_request_kind_name()),
        }
    }
}

impl crate::plane::Gate for Handshake {
    type RequestKind = IpcRequestKind;

    fn get_agreed_protocol_version(&self) -> Option<u32> {
        Handshake::get_agreed_protocol_version(self)
    }

    fn build_unknown_request_kind_error(&self, request_kind_name: &str) -> IpcErrorPayload {
        Handshake::build_unknown_request_kind_error(self, request_kind_name)
    }

    fn validate_request_kind(
        &mut self,
        request_kind: &IpcRequestKind,
    ) -> Result<(), IpcErrorPayload> {
        Handshake::validate_request_kind(self, request_kind)
    }

    fn is_hello(request_kind: &IpcRequestKind) -> bool {
        matches!(request_kind, IpcRequestKind::Hello { .. })
    }
}

#[cfg(test)]
mod tests;
