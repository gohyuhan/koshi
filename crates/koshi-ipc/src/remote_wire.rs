//! What a remote client and the machine serving it say to each other over the
//! TLS stream, before any session is reached.
//!
//! The client opens with [`Hello`](crate::remote_wire::RemoteClientFrame::Hello),
//! which names the remote protocol versions it speaks, the session protocol
//! versions it speaks, and the secret from a grant. The server settles the
//! remote protocol version from the first pair: the highest both ends speak.
//! The second pair it relays, unread, into the session-plane Hello it sends on
//! the client's behalf; the session server refuses a mismatch there by name.
//! The server answers [`Welcome`](crate::remote_wire::RemoteServerFrame::Welcome)
//! carrying the settled remote protocol version, or
//! [`Refused`](crate::remote_wire::RemoteServerFrame::Refused). After that the
//! client either lists the sessions its secret reaches, or asks to attach to
//! one. [`open_remote_connection`](crate::remote_wire::open_remote_connection)
//! is the dialling side of that opening: it dials, sends the Hello and reads
//! the one frame answering it, all inside one deadline.
//!
//! Once an [`Attach`](crate::remote_wire::RemoteClientFrame::Attach) is
//! admitted, these frames stop. The next bytes on the stream are the session
//! server's own answer frames, carried through unparsed. From then on the
//! server ends the stream once a socket write to the client takes no byte, or
//! a socket read from the client gets no byte, for the Attach's
//! `link_timeout_ms`. A socket read before the client's first byte after the
//! Attach waits for the larger of that and 20 seconds.
//!
//! Every refusal carries the same sentence,
//! [`REMOTE_REFUSED`](crate::remote_wire::REMOTE_REFUSED), but three: a remote
//! protocol range that does not overlap; an attach that arrives while the
//! router is about to restart into a new build, which carries
//! [`ROUTER_RESTARTING_MESSAGE`](crate::router::ROUTER_RESTARTING_MESSAGE);
//! and an attach to a session the secret reaches that does not answer, which
//! carries
//! [`SESSION_NOT_ANSWERING_MESSAGE`](crate::remote_wire::SESSION_NOT_ANSWERING_MESSAGE).

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use koshi_core::ids::SessionId;

use crate::error::IpcError;
use crate::protocol::ConnectionToken;
use crate::router::SessionSelector;
use crate::tls;
use crate::transport::{build_frame_halves, read_message, write_message, FrameReader, FrameWriter};

/// The highest remote protocol version this build speaks, and the one it uses
/// when the caller speaks it too.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::REMOTE_PROTOCOL`].
pub const REMOTE_PROTOCOL_VERSION: u32 = koshi_core::compat::REMOTE_PROTOCOL.maximum_version;

/// The lowest remote protocol version this build serves. A caller whose
/// highest is below it is refused.
pub const MIN_REMOTE_PROTOCOL_VERSION: u32 = koshi_core::compat::REMOTE_PROTOCOL.minimum_version;

/// The largest frame the server accepts before a Hello is admitted: 4 KiB.
///
/// A Hello carries four version numbers and one secret. One carrying a
/// generated secret fits inside the cap at every value the versions can hold.
pub const REMOTE_HELLO_MAX_BYTE_COUNT: u32 = 4096;

/// The one sentence every refusal carries: a wrong secret, a revoked one, an
/// expired one, a session that does not exist, and a session the secret holds
/// no grant for all carry it.
///
/// The refusal of a Hello whose remote protocol version the server settled
/// also carries that version in
/// [`RemoteServerFrame::Refused::remote_protocol_version`]. That refusal is the
/// same for every secret the server does not admit.
///
/// A remote protocol range that does not overlap carries
/// [`format_version_refusal`] instead, an attach that arrives while the
/// router is about to restart carries
/// [`ROUTER_RESTARTING_MESSAGE`](crate::router::ROUTER_RESTARTING_MESSAGE),
/// and an attach to a session that does not answer carries
/// [`SESSION_NOT_ANSWERING_MESSAGE`].
pub const REMOTE_REFUSED: &str = "this server did not admit the connection";

/// The sentence an admitted caller gets when the session its Attach names,
/// which its secret reaches, cannot be opened: the session's endpoint file
/// cannot be read, its socket cannot be reached, another OS user serves it,
/// or the Hello sent on the caller's behalf cannot be written.
pub const SESSION_NOT_ANSWERING_MESSAGE: &str = "the session did not answer";

/// The refusal a caller gets when no remote protocol version suits both ends,
/// naming both ranges and which end is which. One of the three refusals that
/// are not [`REMOTE_REFUSED`]; the others are
/// [`ROUTER_RESTARTING_MESSAGE`](crate::router::ROUTER_RESTARTING_MESSAGE) and
/// [`SESSION_NOT_ANSWERING_MESSAGE`].
///
/// Example — a caller speaking 2 to 3 against a build speaking 1 to 1 reads
/// `"the caller speaks remote protocol versions 2 to 3, this koshi speaks 1 to
/// 1"`.
#[must_use]
pub fn format_version_refusal(
    caller_minimum_remote_version: u32,
    caller_maximum_remote_version: u32,
) -> String {
    format!(
        "the caller speaks remote protocol versions {caller_minimum_remote_version} to \
         {caller_maximum_remote_version}, this koshi speaks {MIN_REMOTE_PROTOCOL_VERSION} to \
         {REMOTE_PROTOCOL_VERSION}"
    )
}

/// One message from a remote client to the machine serving it.
///
/// A field this build does not know is ignored. A client that adds one still
/// decodes here. Every field below but `link_timeout_ms` is required: a
/// misspelled name is the missing-field error for the name it displaced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteClientFrame {
    /// Opens the stream: names the remote protocol versions and the session
    /// protocol versions the client speaks, and presents the secret from a
    /// grant. Sent before any other frame.
    ///
    /// The server settles the remote protocol version from
    /// `minimum_remote_version` and `maximum_remote_version`. It carries
    /// `minimum_protocol_version` and `maximum_protocol_version` into the
    /// session-plane Hello it sends for this client, and never reads them
    /// itself.
    Hello {
        /// The lowest remote protocol version the client speaks.
        minimum_remote_version: u32,
        /// The highest remote protocol version the client speaks.
        maximum_remote_version: u32,
        /// The lowest session protocol version the client speaks.
        minimum_protocol_version: u32,
        /// The highest session protocol version the client speaks.
        maximum_protocol_version: u32,
        /// The secret the operator handed out with a grant.
        connection_token: ConnectionToken,
    },
    /// List the sessions this secret reaches.
    List,
    /// Attach to one session.
    Attach {
        /// Which session to attach to.
        session_selector: SessionSelector,
        /// The client's link timeout, in milliseconds. Once the server
        /// carries the stream to the session, each socket write to the client
        /// waits at most this long for the client to take a byte. Each socket
        /// read from the client waits at most the larger of this and 20
        /// seconds for the client's first byte, and at most this long after
        /// it. A wait past it ends the stream. A value below `1000` is refused.
        /// `None` sets no limit. Left off the wire when `None`; absent, it
        /// reads as `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        link_timeout_ms: Option<u32>,
    },
}

/// One message from the machine serving a remote client back to it.
///
/// A field this build does not know is ignored. A server that adds one still
/// decodes here. Every field below but `remote_protocol_version` on `Refused`
/// is required: a misspelled name is the missing-field error for the name it
/// displaced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteServerFrame {
    /// Answers [`RemoteClientFrame::Hello`]: the stream is open.
    Welcome {
        /// The remote protocol version both ends settled on: the highest they
        /// both speak.
        remote_protocol_version: u32,
    },
    /// The stream is not open, or the frame is not served.
    Refused {
        /// [`REMOTE_REFUSED`]; the sentence [`format_version_refusal`] builds
        /// when no remote protocol version suits both ends;
        /// [`ROUTER_RESTARTING_MESSAGE`](crate::router::ROUTER_RESTARTING_MESSAGE)
        /// for an attach that arrives while the router is about to restart; or
        /// [`SESSION_NOT_ANSWERING_MESSAGE`] for an attach to a session that
        /// does not answer.
        message: String,
        /// The remote protocol version the server settled from the Hello
        /// before it refused the secret, or `None` on every other refusal.
        /// koshi 0.3.0, 0.4.0 and 0.5.0 never send it. Left off the wire when
        /// `None`; absent, it reads as `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote_protocol_version: Option<u32>,
    },
    /// Answers [`RemoteClientFrame::List`]: one row per session this secret
    /// reaches.
    Sessions {
        /// The sessions, in name then session id order.
        session_rows: Vec<RemoteSessionRow>,
    },
}

/// Open a TLS stream to `server_address`, send `hello_frame`, and read the one frame the
/// server answers it with.
///
/// `expected_certificate_fingerprint` is the fingerprint the server's certificate must have.
/// `hello_frame` is written only after the server presented that certificate.
///
/// `connection_timeout` bounds everything after the name lookup: the connect, the TLS
/// handshake, the Hello and the answer share one deadline. A server that
/// sends its answer one byte at a time is cut off at that deadline.
///
/// `reply_timeout` says how long the halves that come back may block:
///
/// - `None` — they block for as long as it takes.
/// - `Some(wait)` — every read and write on them finishes inside `wait`,
///   counted from when the answer arrives.
///
/// Returns the two framed halves and the answer.
///
/// # Errors
/// [`IpcError::ConnectRefused`] when nothing accepts the TCP connection,
/// [`IpcError::ConnectTimedOut`] when the connect deadline passes,
/// [`IpcError::TlsHandshakeFailed`] when the handshake fails, and
/// [`IpcError::CertificateMismatch`] when the presented certificate does not
/// have `expected_certificate_fingerprint`. [`IpcError::Transport`] naming what failed
/// for the lookup, the stream split, and a Hello or answer that ran out of
/// time. [`IpcError::Disconnected`] when the server hung up,
/// [`IpcError::FrameTooLarge`] when its answer's length prefix is past
/// [`MAX_FRAME_BYTE_COUNT`](crate::transport::MAX_FRAME_BYTE_COUNT), and
/// [`IpcError::MalformedFrame`] when its answer does not decode.
pub fn open_remote_connection(
    server_address: &str,
    expected_certificate_fingerprint: &str,
    hello_frame: &RemoteClientFrame,
    connection_timeout: Duration,
    reply_timeout: Option<Duration>,
) -> Result<(FrameReader, FrameWriter, RemoteServerFrame), IpcError> {
    let (mut tls_reader, mut tls_writer) = tls::connect_tls_stream(
        server_address,
        expected_certificate_fingerprint,
        connection_timeout,
    )?;
    write_message(&mut tls_writer, hello_frame)?;
    let server_response = read_message::<RemoteServerFrame>(&mut tls_reader)?;
    let reply_deadline = reply_timeout.map(|reply_timeout| Instant::now() + reply_timeout);
    tls_reader.set_deadline(reply_deadline);
    tls_writer.set_deadline(reply_deadline);
    let (frame_reader, frame_writer) =
        build_frame_halves(Box::new(tls_reader), Box::new(tls_writer));
    Ok((frame_reader, frame_writer, server_response))
}

/// One session as a remote client may see it.
///
/// A field this build does not know is ignored. A server that adds one still
/// decodes here. Both fields below are required: a misspelled name is the
/// missing-field error for the name it displaced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSessionRow {
    /// The session's stable id.
    pub session_id: SessionId,
    /// The session's generated display name.
    pub session_name: String,
}

#[cfg(test)]
mod tests;
