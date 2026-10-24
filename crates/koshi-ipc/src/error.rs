//! IPC domain error.

use thiserror::Error;

/// A failure on the control channel.
///
/// Client-fatal — the affected connection or caller stops and the session
/// serves on: a broken link ([`Transport`](IpcError::Transport),
/// [`Disconnected`](IpcError::Disconnected)), a remote dial that fails
/// ([`ConnectRefused`](IpcError::ConnectRefused),
/// [`ConnectTimedOut`](IpcError::ConnectTimedOut),
/// [`TlsHandshakeFailed`](IpcError::TlsHandshakeFailed),
/// [`CertificateMismatch`](IpcError::CertificateMismatch)), a refused frame
/// ([`FrameTooLarge`](IpcError::FrameTooLarge),
/// [`UnexpectedBinaryFrame`](IpcError::UnexpectedBinaryFrame)), an answer from a server that
/// koshi 0.1.0 to 0.4.0 started
/// ([`PreviousReleaseAnswer`](IpcError::PreviousReleaseAnswer)), a socket address that fails
/// its trust or liveness checks
/// ([`UntrustedSocket`](IpcError::UntrustedSocket),
/// [`NoListener`](IpcError::NoListener),
/// [`PeerIsOtherUser`](IpcError::PeerIsOtherUser),
/// [`SocketBusy`](IpcError::SocketBusy)),
/// an endpoint file the caller cannot read
/// ([`EndpointFileMissing`](IpcError::EndpointFileMissing),
/// [`EndpointFileUnreadable`](IpcError::EndpointFileUnreadable),
/// [`Koshi010WindowEndpointFile`](IpcError::Koshi010WindowEndpointFile)), a program
/// file the caller cannot read
/// ([`ProgramFileUnreadable`](IpcError::ProgramFileUnreadable)), and a remote
/// access file the caller cannot read or write
/// ([`RemoteFileUnreadable`](IpcError::RemoteFileUnreadable),
/// [`RemoteFileWrite`](IpcError::RemoteFileWrite)).
///
/// Session-fatal: a failed endpoint-file write
/// ([`EndpointFileWrite`](IpcError::EndpointFileWrite)) and a failed
/// program-file write ([`ProgramFileWrite`](IpcError::ProgramFileWrite)).
/// Each happens during the server's own startup. No caller reaches a server whose endpoint file
/// and program file never land.
///
/// Recoverable: a frame that arrived whole yet does not decode
/// ([`MalformedFrame`](IpcError::MalformedFrame)). The stream is still aligned
/// on frame boundaries, and the connection answers and keeps going.
#[derive(Debug, Error)]
pub enum IpcError {
    /// The underlying transport failed.
    #[error("ipc transport error: {error_detail}")]
    Transport { error_detail: String },
    /// The peer disconnected unexpectedly.
    #[error("ipc peer disconnected")]
    Disconnected,
    /// A frame longer than
    /// [`MAX_FRAME_BYTE_COUNT`](crate::transport::MAX_FRAME_BYTE_COUNT). On receive, the
    /// length prefix named more bytes than the limit and the payload is left
    /// unread: the stream is off frame boundaries and the connection closes;
    /// `frame_byte_count` is the byte count bits 0 to 30 of the prefix named.
    /// On send, nothing was written. For a message, encoding stopped at the
    /// byte that crossed the limit, and `frame_byte_count` is the payload size
    /// the refused write reached, which for a message encoded in one piece is
    /// its full size. For a binary frame, `frame_byte_count` is the payload's
    /// byte count.
    #[error(
        "ipc frame of {frame_byte_count} bytes exceeds the {maximum_frame_byte_count}-byte limit"
    )]
    FrameTooLarge {
        frame_byte_count: u64,
        maximum_frame_byte_count: u32,
    },
    /// A binary frame, bit 31 of its length prefix set, read where only JSON
    /// frames are taken. Its payload is left unread: the stream is off frame
    /// boundaries and the connection closes.
    #[error("ipc frame is binary where a JSON frame was expected")]
    UnexpectedBinaryFrame,
    /// A frame whose bytes are not a readable message: the payload arrived
    /// whole but did not decode, or a message failed to encode.
    #[error("ipc frame is not a readable message: {error_detail}")]
    MalformedFrame { error_detail: String },
    /// An answer in the envelope that a server started by koshi 0.1.0 to
    /// 0.4.0 writes, `{"request_id": …, "result": …}`. Such a server reads no
    /// frame this build writes.
    #[error(
        "the server answered in the format of koshi 0.4.0 or older, which this koshi cannot \
         talk to"
    )]
    PreviousReleaseAnswer,
    /// A socket address that failed a trust check, named in `trust_failure_reason`: the
    /// path is not directly inside the directory it must sit in, that
    /// directory is a symbolic link, is not a directory, carries the wrong
    /// mode, or belongs to another user, or (Windows) the pipe name is
    /// outside the `koshi-` namespace.
    #[error("untrusted socket address {socket_address}: {trust_failure_reason}")]
    UntrustedSocket {
        socket_address: String,
        trust_failure_reason: String,
    },
    /// Nothing listens at the address: what is there is a leftover from a
    /// process that is gone, or nothing exists there at all.
    #[error("no koshi is listening at {socket_address}")]
    NoListener { socket_address: String },
    /// The process serving the address runs as another OS user than this
    /// process. The connection was closed before anything was written to it.
    #[error("the koshi listening at {socket_address} runs as another user")]
    PeerIsOtherUser { socket_address: String },
    /// A live listener already holds the address this process wants to bind.
    #[error("another process is already listening at {socket_address}")]
    SocketBusy { socket_address: String },
    /// No endpoint file at the path: no running koshi has advertised a
    /// control socket there.
    #[error("no endpoint file at {endpoint_file_path}")]
    EndpointFileMissing { endpoint_file_path: String },
    /// An endpoint file that exists but could not be used: reading it
    /// failed, or its bytes are not a readable endpoint file.
    #[error("endpoint file {endpoint_file_path} is unreadable: {error_detail}")]
    EndpointFileUnreadable {
        endpoint_file_path: String,
        error_detail: String,
    },
    /// An endpoint file that holds the fields `{socket, token}` alone, which a
    /// koshi 0.1.0 window writes. Such a window runs its own server in its
    /// terminal, and speaks a wire this build does not.
    #[error(
        "endpoint file {endpoint_file_path} is unreadable: a koshi 0.1.0 window wrote it, and \
         this koshi cannot talk to that window; the window ends when its terminal closes"
    )]
    Koshi010WindowEndpointFile { endpoint_file_path: String },
    /// Writing the endpoint file failed during session startup. No caller
    /// finds this session's socket.
    #[error("endpoint file {endpoint_file_path} could not be written: {error_detail}")]
    EndpointFileWrite {
        endpoint_file_path: String,
        error_detail: String,
    },
    /// A program file that exists but could not be used: reading it failed,
    /// or its bytes are not a readable program file.
    #[error("program file {program_file_path} is unreadable: {error_detail}")]
    ProgramFileUnreadable {
        program_file_path: String,
        error_detail: String,
    },
    /// Writing the program file failed during server startup. No caller
    /// finds this server's socket.
    #[error("program file {program_file_path} could not be written: {error_detail}")]
    ProgramFileWrite {
        program_file_path: String,
        error_detail: String,
    },
    /// A remote access file that exists but could not be used: reading it
    /// failed, its bytes are not readable, or its format number is not the
    /// one this build reads.
    #[error("the {remote_file} at {remote_file_path} is unreadable: {error_detail}")]
    RemoteFileUnreadable {
        remote_file: RemoteFile,
        remote_file_path: String,
        error_detail: String,
    },
    /// Writing a remote access file failed. What the caller changed never
    /// reached the disk.
    #[error("the {remote_file} at {remote_file_path} could not be written: {error_detail}")]
    RemoteFileWrite {
        remote_file: RemoteFile,
        remote_file_path: String,
        error_detail: String,
    },
    /// Nothing accepted the TCP connection at `server_address`.
    #[error(
        "{server_address} refused the connection: nothing is listening on that port. \
         if remote access is not enabled on that machine, run `koshi share open` \
         there to open the port"
    )]
    ConnectRefused { server_address: String },
    /// The TCP connection to `server_address` was still unanswered when the
    /// dial ran out of time.
    #[error(
        "connecting to {server_address} timed out: nothing answered. check that the \
         machine is up, the address and port are right, and the network path \
         allows it"
    )]
    ConnectTimedOut { server_address: String },
    /// The TCP connection to `server_address` opened and the TLS handshake on
    /// it did not finish, for the reason in `error_detail`.
    #[error("the TLS handshake with {server_address} failed: {error_detail}")]
    TlsHandshakeFailed {
        server_address: String,
        error_detail: String,
    },
    /// The server at `server_address` presented a certificate whose
    /// fingerprint is not the one the dial expected. The secret was not sent.
    ///
    /// `saved_server_label` is the name, else the address, of the saved
    /// server the dial presented the saved secret of, or `None` for a dial of
    /// a server that is not saved. With `Some("work")` the message ends in
    /// ``give it with `koshi remote edit work`.``; with `None` it ends in
    /// ``give it with --certificate-fingerprint, or with `koshi remote edit`
    /// for a saved server.``
    #[error(
        "the certificate {server_address} presented is not the expected one: expected \
         {expected_certificate_fingerprint}, presented {presented_certificate_fingerprint}. the \
         secret was not sent. if that machine's certificate changed on purpose, `koshi share \
         open` there prints the new fingerprint: {}.",
        format_new_fingerprint_step(saved_server_label.as_deref())
    )]
    CertificateMismatch {
        server_address: String,
        expected_certificate_fingerprint: String,
        presented_certificate_fingerprint: String,
        saved_server_label: Option<String>,
    },
}

/// The step that gives a dial the new fingerprint of a server's certificate.
///
/// Example — `Some("work")` gives ``give it with `koshi remote edit work` ``.
/// `None` gives ``give it with --certificate-fingerprint, or with `koshi
/// remote edit` for a saved server``.
fn format_new_fingerprint_step(saved_server_label: Option<&str>) -> String {
    match saved_server_label {
        Some(saved_server_label) => {
            format!("give it with `koshi remote edit {saved_server_label}`")
        }
        None => "give it with --certificate-fingerprint, or with `koshi remote edit` for a saved \
                 server"
            .to_string(),
    }
}

/// Which of the four files under `remote/` an
/// [`IpcError::RemoteFileUnreadable`] or [`IpcError::RemoteFileWrite`] names.
/// `Display` writes `saved servers file`, `remote access certificate`,
/// `remote access record` or `remote access token store`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteFile {
    /// The servers this user has dialled and saved, on the dialling machine.
    SavedServers,
    /// This machine's own certificate for the remote listener.
    Certificate,
    /// The record that remote access was switched on for this machine.
    RemoteAccessRecord,
    /// The remote access grants this machine has handed out.
    TokenStore,
}

impl std::fmt::Display for RemoteFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let remote_file_name = match self {
            Self::SavedServers => "saved servers file",
            Self::Certificate => "remote access certificate",
            Self::RemoteAccessRecord => "remote access record",
            Self::TokenStore => "remote access token store",
        };
        formatter.write_str(remote_file_name)
    }
}

#[cfg(test)]
mod tests;
