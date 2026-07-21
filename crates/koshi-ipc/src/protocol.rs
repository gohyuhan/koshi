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
//! This module is the vocabulary only: framing, sockets, and the checks
//! themselves belong to the transport and server layers.

use std::fmt;

use koshi_core::command::{CommandEnvelope, CommandResult};
use koshi_core::discovery::SessionOverview;
use koshi_core::redact::REDACTED;
use serde::{Deserialize, Serialize};

/// The protocol version this build speaks. A connection whose
/// [`IpcRequestKind::Hello`] names a different version is refused with
/// [`IpcErrorCode::UnsupportedVersion`].
pub const PROTOCOL_VERSION: u32 = 1;

/// The secret a connection presents to prove it belongs to the user who
/// started this Koshi.
///
/// Each running Koshi generates one and writes it to its endpoint file in the
/// private runtime directory, so being able to read the value is itself the
/// proof. `Debug` and `Display` print `***`: the token belongs in that file and
/// on the wire, never in a log, a snapshot, or an event payload.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnectionToken(String);

impl ConnectionToken {
    /// Wrap an already-generated secret.
    #[must_use]
    pub fn new(secret: impl Into<String>) -> Self {
        ConnectionToken(secret.into())
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
    /// not reveal how many leading bytes a caller guessed right. Secrets of
    /// different lengths are refused immediately: Koshi generates every token
    /// at one length, so the length is not a secret.
    fn eq(&self, other: &Self) -> bool {
        let (ours, theirs) = (self.0.as_bytes(), other.0.as_bytes());
        if ours.len() != theirs.len() {
            return false;
        }
        ours.iter()
            .zip(theirs)
            .fold(0u8, |differences, (ours_byte, theirs_byte)| {
                differences | (ours_byte ^ theirs_byte)
            })
            == 0
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcRequest {
    /// Caller-chosen id, repeated in the response that answers this request.
    /// Unique among the requests in flight on one connection.
    pub request_id: u64,
    /// What is being asked.
    pub kind: IpcRequestKind,
}

/// What a request asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcRequestKind {
    /// Opens the connection: names the protocol version the caller speaks and
    /// presents the token. Sent before any other kind.
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

/// One message answering an [`IpcRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The connection is open: versions agree and the token matched.
    Accepted,
    /// What dispatching the submitted command produced.
    CommandResult(CommandResult),
    /// The session's full description.
    Overview(SessionOverview),
    /// The request was refused.
    Error(IpcErrorPayload),
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
