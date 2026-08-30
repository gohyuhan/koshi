//! What a remote client and the machine serving it say to each other over the
//! TLS stream, before any session is reached.
//!
//! The client opens with
//! [`Hello`](crate::remote_wire::RemoteClientFrame::Hello), which names the
//! doorway versions it speaks, the session protocol versions it speaks, and
//! the secret from a grant. The server settles the doorway version from the
//! first pair: the highest both ends speak. The second pair it relays, unread,
//! into the session-plane Hello it sends on the client's behalf; the session
//! server refuses a mismatch there by name. The server answers
//! [`Welcome`](crate::remote_wire::RemoteServerFrame::Welcome) carrying the
//! settled doorway version, or
//! [`Refused`](crate::remote_wire::RemoteServerFrame::Refused). After that the
//! client either lists the sessions its secret reaches, or asks to attach to
//! one. [`open`](crate::remote_wire::open) is the dialling side of that
//! opening: it dials, sends the Hello and reads the one frame answering it,
//! all inside one deadline.
//!
//! Once an [`Attach`](crate::remote_wire::RemoteClientFrame::Attach) is
//! admitted, these frames stop. The next bytes on the stream are the session
//! server's own answer frames, carried through unparsed.
//!
//! How long the halves [`open`](crate::remote_wire::open) hands back may block
//! is the caller's choice, made when it dials.
//!
//! Every refusal carries the same sentence,
//! [`REMOTE_REFUSED`](crate::remote_wire::REMOTE_REFUSED).

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use koshi_core::ids::SessionId;

use crate::error::IpcError;
use crate::protocol::ConnectionToken;
use crate::router::SessionSelector;
use crate::tls;
use crate::transport::{frame_halves, read_message, write_message, FrameReader, FrameWriter};

/// The highest doorway version this build speaks, and the one it uses when the
/// caller speaks it too.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::REMOTE_PROTOCOL`].
pub const REMOTE_PROTOCOL_VERSION: u32 = koshi_core::compat::REMOTE_PROTOCOL.max;

/// The lowest doorway version this build serves. A caller whose highest is
/// below it is refused.
pub const MIN_REMOTE_PROTOCOL_VERSION: u32 = koshi_core::compat::REMOTE_PROTOCOL.min;

/// The largest frame the server accepts before a Hello is admitted: 4 KiB.
///
/// A Hello carries four version numbers and one secret. One carrying a
/// generated secret fits inside the cap at every value the versions can hold.
pub const REMOTE_HELLO_MAX_LEN: u32 = 4096;

/// The one sentence every refusal carries: a wrong secret, a revoked one, an
/// expired one, a session that does not exist, and a session the secret holds
/// no grant for all read the same.
///
/// A doorway version that does not overlap carries [`version_refusal`]
/// instead.
pub const REMOTE_REFUSED: &str = "this server did not admit the connection";

/// The refusal a caller gets when no doorway version suits both ends, naming
/// both ranges and which end is which. The one refusal that is not
/// [`REMOTE_REFUSED`].
///
/// Example — a caller speaking 2 to 3 against a build speaking 1 to 1 reads
/// `"the caller speaks remote doorway 2 to 3, this koshi speaks 1 to 1"`.
#[must_use]
pub fn version_refusal(caller_min: u32, caller_max: u32) -> String {
    format!(
        "the caller speaks remote doorway {caller_min} to {caller_max}, \
         this koshi speaks {MIN_REMOTE_PROTOCOL_VERSION} to {REMOTE_PROTOCOL_VERSION}"
    )
}

/// One message from a remote client to the machine serving it.
///
/// Decoding rejects any field it does not know, so a misspelled name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RemoteClientFrame {
    /// Opens the stream: names the doorway versions and the session protocol
    /// versions the client speaks, and presents the secret from a grant. Sent
    /// before any other frame.
    ///
    /// The server settles the doorway version from `min_remote_version` and
    /// `max_remote_version`. It carries `min_protocol_version` and
    /// `max_protocol_version` into the session-plane Hello it sends for this
    /// client, and never reads them itself.
    Hello {
        /// The lowest doorway version the client speaks.
        min_remote_version: u32,
        /// The highest doorway version the client speaks.
        max_remote_version: u32,
        /// The lowest session protocol version the client speaks.
        min_protocol_version: u32,
        /// The highest session protocol version the client speaks.
        max_protocol_version: u32,
        /// The secret the operator handed out with a grant.
        token: ConnectionToken,
    },
    /// List the sessions this secret reaches.
    List,
    /// Attach to one session.
    Attach {
        /// Which session to attach to.
        session: SessionSelector,
    },
}

/// One message from the machine serving a remote client back to it.
///
/// Decoding rejects any field it does not know, so a misspelled name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RemoteServerFrame {
    /// Answers [`RemoteClientFrame::Hello`]: the stream is open.
    Welcome {
        /// The doorway version both ends settled on: the highest they both
        /// speak.
        remote_version: u32,
    },
    /// The stream is not open, or the frame is not served.
    Refused {
        /// [`REMOTE_REFUSED`], or the sentence [`version_refusal`] builds
        /// when no doorway version suits both ends.
        message: String,
    },
    /// Answers [`RemoteClientFrame::List`]: one row per session this secret
    /// reaches.
    Sessions {
        /// The sessions, in the order the server holds them.
        rows: Vec<RemoteSessionRow>,
    },
}

/// Open a TLS stream to `address`, send `hello`, and read the one frame the
/// server answers it with.
///
/// `pinned` is the fingerprint saved from an earlier connection, or `None` on
/// the first connection to this server.
///
/// `timeout` bounds everything after the name lookup: the connect, the TLS
/// handshake, the Hello and the answer share one deadline. A server that
/// sends its answer one byte at a time is cut off at that deadline.
///
/// `reply_wait` says how long the halves that come back may block:
///
/// - `None` — they block for as long as it takes.
/// - `Some(wait)` — every read and write on them finishes inside `wait`,
///   counted from when the answer arrives.
///
/// Returns the two framed halves, the sha256 of the certificate the server
/// presented as 64 lowercase hex characters, and the answer.
///
/// # Errors
/// [`IpcError::ConnectRefused`] when nothing accepts the TCP connection,
/// [`IpcError::ConnectTimedOut`] when the connect deadline passes,
/// [`IpcError::TlsHandshakeFailed`] when the handshake fails, and
/// [`IpcError::CertificateChanged`] when the presented certificate does not
/// match the pinned fingerprint. [`IpcError::Transport`] naming what failed
/// for the lookup, the stream split, and a Hello or answer that ran out of
/// time. [`IpcError::Disconnected`] when the server hung up,
/// [`IpcError::FrameTooLarge`] when its answer's length prefix is past
/// [`MAX_FRAME_LEN`](crate::transport::MAX_FRAME_LEN), and
/// [`IpcError::MalformedFrame`] when its answer does not decode.
pub fn open(
    address: &str,
    pinned: Option<&str>,
    hello: &RemoteClientFrame,
    timeout: Duration,
    reply_wait: Option<Duration>,
) -> Result<(FrameReader, FrameWriter, String, RemoteServerFrame), IpcError> {
    let (mut reader, mut writer, presented) = tls::dial(address, pinned, timeout)?;
    write_message(&mut writer, hello)?;
    let answer = read_message::<RemoteServerFrame>(&mut reader)?;
    let after = reply_wait.map(|wait| Instant::now() + wait);
    reader.set_deadline(after);
    writer.set_deadline(after);
    let (reader, writer) = frame_halves(Box::new(reader), Box::new(writer));
    Ok((reader, writer, presented, answer))
}

/// One session as a remote client may see it.
///
/// Decoding rejects any field it does not know, so a misspelled name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteSessionRow {
    /// The session's stable id.
    pub id: SessionId,
    /// The session's generated display name.
    pub name: String,
}

#[cfg(test)]
mod tests;
