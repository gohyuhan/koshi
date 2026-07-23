//! Wire messages for the control socket.
//!
//! An exchange is one [`IpcRequest`] and the [`IpcResponse`] answering it. The
//! response repeats the request's `request_id` so a caller can match the two,
//! and names no request at all when the bytes it received could not be read.
//!
//! Every connection opens with [`IpcRequestKind::Hello`]. It settles the two
//! facts that hold for the whole connection — the protocol version both sides
//! speak, and the [`ConnectionToken`] proving the caller is the user who
//! started this Koshi — so no later request repeats them.
//!
//! This module is the vocabulary only: framing and sockets belong to the
//! transport layer, and the Hello checks to
//! [`handshake`](crate::handshake).

use std::fmt;

use koshi_core::command::{CommandEnvelope, CommandResult};
use koshi_core::discovery::SessionOverview;
use koshi_core::redact::REDACTED;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

/// The protocol version this build speaks. A connection whose
/// [`IpcRequestKind::Hello`] names a different version is refused with
/// [`IpcErrorCode::UnsupportedVersion`].
///
/// Any change to the shape of a wire struct bumps this: the version is the
/// only thing standing between two builds that no longer agree on the
/// bytes. Version 2 carries `SessionOverview`'s tab records naming their
/// session and its pane records dropping their solved rectangle, so a
/// version-1 peer's overview no longer decodes here.
pub const PROTOCOL_VERSION: u32 = 2;

/// The secret a connection presents to prove it belongs to the user who
/// started this Koshi.
///
/// Each running Koshi generates one and writes it to its
/// [endpoint file](crate::endpoint::EndpointFile) in the private runtime
/// directory, so being able to read the value is itself the proof.
///
/// Two ways out of this type, and only two:
///
/// - `Serialize` and [`expose`](Self::expose) write the **real secret**. They
///   exist for the endpoint file and the socket, which cannot work without it.
///   `serde_json::to_string(&hello)` yields `{"protocol_version":2,
///   "token":"k7Qx…"}`, secret included.
/// - `Debug` and `Display` write `***`, so a token that reaches a log line, a
///   trace, or an error dump reveals nothing.
///
/// Anything describing a request in a log takes the second form, or
/// [`IpcRequestKind::name`], which carries no payload at all.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnectionToken(String);

impl ConnectionToken {
    /// Wrap an already-generated secret.
    #[must_use]
    pub fn new(secret: impl Into<String>) -> Self {
        ConnectionToken(secret.into())
    }

    /// Generate a fresh secret: 32 bytes from the operating system's
    /// cryptographic random source, written as 64 lowercase hex characters.
    /// Every generated token has this one length.
    #[must_use]
    pub fn generate() -> Self {
        const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .expect("every supported platform provides the system random source");
        let mut secret = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            secret.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
            secret.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
        }
        ConnectionToken(secret)
    }

    /// The secret itself, for writing it to the endpoint file.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl PartialEq for ConnectionToken {
    /// Two secrets of the same length are compared byte by byte to the end,
    /// never stopping at the first mismatch, so how long the answer takes does
    /// not reveal how many leading bytes a caller guessed right. `subtle`
    /// holds that property through optimization by reading each byte's verdict
    /// back through a volatile load, which the compiler may not fold away.
    ///
    /// Secrets of different lengths are refused at once: Koshi generates every
    /// token at one length, so a token's length is not a secret.
    fn eq(&self, other: &Self) -> bool {
        self.0.as_bytes().ct_eq(other.0.as_bytes()).into()
    }
}

impl Eq for ConnectionToken {}

impl fmt::Debug for ConnectionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ConnectionToken({REDACTED})")
    }
}

impl fmt::Display for ConnectionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

/// One message from a caller to a running Koshi.
///
/// Decoding rejects any field it does not know, so a misspelled name is an
/// error rather than a field that quietly keeps its default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcRequest {
    /// Caller-chosen id, repeated in the response that answers this request.
    /// Unique among the requests in flight on one connection.
    pub request_id: u64,
    /// What is being asked.
    pub kind: IpcRequestKind,
}

/// What a request asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum IpcRequestKind {
    /// Opens the connection: names the protocol version the caller speaks and
    /// presents the token. Sent before any other kind.
    ///
    /// Sending it again on an open connection is allowed and changes nothing:
    /// the version and token are checked again and the same answer comes back,
    /// since checking them alters no state.
    Hello {
        /// The protocol version the caller speaks.
        protocol_version: u32,
        /// The secret read from the endpoint file.
        token: ConnectionToken,
    },
    /// Dispatch a command against the session.
    SubmitCommand(Box<CommandEnvelope>),
    /// Ask the session to describe itself in full. The caller narrows the
    /// answer to the query it was asked.
    Discovery,
}

impl IpcRequestKind {
    /// The kind's name, e.g. `"SubmitCommand"`. Carries no payload, so it is
    /// safe on a log line even though a payload can hold the connection token
    /// or text the user typed.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            IpcRequestKind::Hello { .. } => "Hello",
            IpcRequestKind::SubmitCommand(_) => "SubmitCommand",
            IpcRequestKind::Discovery => "Discovery",
        }
    }
}

/// One message answering an [`IpcRequest`].
///
/// Decoding rejects any field it does not know. `request_id` carries meaning
/// when it is absent, so a misspelled `request_id` must fail loudly instead of
/// reading as the "could not be read" answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcResponse {
    /// The `request_id` of the request being answered, or `None` when the
    /// bytes received were too malformed to read one — a caller that sent
    /// request 7 and reads `None` knows the answer belongs to no request of
    /// its own.
    pub request_id: Option<u64>,
    /// The answer itself.
    pub result: IpcResult,
}

/// The answer to a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcResult {
    /// Answers [`IpcRequestKind::Hello`]: the connection is open, because the
    /// versions agree and the token matched.
    Hello,
    /// What dispatching the submitted command produced.
    CommandResult(CommandResult),
    /// The session's full description.
    Overview(SessionOverview),
    /// The request was refused.
    Error(IpcErrorPayload),
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcErrorPayload {
    /// The refusal, as a value a caller can branch on.
    pub code: IpcErrorCode,
    /// A human-facing sentence naming what was wrong.
    pub message: String,
}

/// The kinds of refusal a request can meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcErrorCode {
    /// The token presented does not match the session's.
    BadToken,
    /// The caller speaks a protocol version this build does not. The message
    /// names both versions.
    UnsupportedVersion,
    /// The bytes received are not a request this build can read.
    MalformedRequest,
    /// A request arrived before [`IpcRequestKind::Hello`] opened the
    /// connection.
    HelloRequired,
}

#[cfg(test)]
mod tests;
