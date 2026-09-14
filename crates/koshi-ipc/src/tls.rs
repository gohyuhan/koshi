//! The TLS stream a remote client and the machine serving it talk over.
//!
//! One [`rustls::Connection`] holds the encryption state of one stream. Both
//! halves of a split stream share it behind a mutex; one thread reads
//! plaintext while another writes plaintext.
//! [`split_tls_stream`](crate::tls::split_tls_stream) makes that pair, and
//! [`transport::frame_halves`](crate::transport::frame_halves) puts koshi's
//! frame shape on it.
//!
//! The dialling side does not use a certificate authority. It remembers the
//! sha256 of the certificate the server presented on the first connection —
//! its fingerprint, 64 lowercase hex characters — and refuses every later
//! connection that presents a different one.
//! [`PinVerifier`](crate::tls::PinVerifier) holds that rule and still checks
//! the server's handshake signature: the proof that the server holds the
//! private key of the certificate it presented.
//!
//! [`connect_tls_stream`](crate::tls::connect_tls_stream) takes one timeout and turns it into one
//! deadline. The connect and the whole handshake finish inside it, whatever
//! pace the server sends its bytes at. The two halves come back carrying that
//! same deadline; the opening exchange the caller makes finishes inside it
//! too. The name lookup before the connect is the operating system's own,
//! carries no timeout, and sits outside the deadline.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::error::IpcError;
use crate::transport::is_io_timeout;

/// How many raw bytes [`TlsReader`] takes off the socket in one read.
const TLS_READ_BUFFER_BYTE_COUNT: usize = 16 * 1024;

/// The least a socket timeout is set to: 1 ms. Less time left than this
/// counts as no time left.
const MINIMUM_SOCKET_TIMEOUT_DURATION: Duration = Duration::from_millis(1);

/// Set both of `socket`'s timeouts to the time left until `deadline`. `None`
/// returns at once and leaves the timeouts as they are.
///
/// Every blocking read and write calls this first; each call reads the time
/// left again.
///
/// # Errors
/// [`io::ErrorKind::TimedOut`] with the text `this step ran out of time` when
/// less than [`MINIMUM_SOCKET_TIMEOUT_DURATION`] is left; otherwise the failure of setting a
/// socket timeout.
fn set_socket_timeouts_until(socket: &TcpStream, deadline: Option<Instant>) -> io::Result<()> {
    let Some(deadline) = deadline else {
        return Ok(());
    };
    let remaining_timeout_duration = deadline.saturating_duration_since(Instant::now());
    if remaining_timeout_duration < MINIMUM_SOCKET_TIMEOUT_DURATION {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "this step ran out of time",
        ));
    }
    socket.set_read_timeout(Some(remaining_timeout_duration))?;
    socket.set_write_timeout(Some(remaining_timeout_duration))?;
    Ok(())
}

/// Decrypt the bytes `tls_connection` has taken and queue whatever it answers with.
///
/// # Errors
/// [`io::ErrorKind::InvalidData`], carrying rustls's own words, when the
/// bytes do not decrypt.
fn decrypt_pending_tls_records(tls_connection: &mut rustls::Connection) -> io::Result<()> {
    tls_connection
        .process_new_packets()
        .map(|_| ())
        .map_err(|decryption_error| {
            io::Error::new(io::ErrorKind::InvalidData, decryption_error.to_string())
        })
}

/// Put every encrypted byte `tls_connection` has queued on `socket`. Each socket write is
/// given the time left until `deadline`; `None` lets it block for as long as
/// it takes.
///
/// # Errors
/// [`io::ErrorKind::TimedOut`] when no time is left before a write; otherwise
/// the socket write's own failure, which is the socket's timeout error when
/// the deadline passes during the write.
fn send_pending_tls_records(
    tls_connection: &mut rustls::Connection,
    socket: &mut TcpStream,
    deadline: Option<Instant>,
) -> io::Result<()> {
    while tls_connection.wants_write() {
        set_socket_timeouts_until(socket, deadline)?;
        tls_connection.write_tls(socket)?;
    }
    Ok(())
}

/// Split a TLS stream into its reading and its writing half.
///
/// Each half gets its own handle on the same socket; one half blocking on
/// the socket does not stop the other. Both halves share one
/// [`rustls::Connection`] behind a mutex.
///
/// A half with a deadline sets both timeouts on its own handle before each of
/// its socket calls. A half with no deadline sets nothing and runs under the
/// timeouts its own handle carries.
///
/// A timeout one half sets reaches the other only on Unix, where the two
/// handles are one duplicated descriptor onto one socket. On Windows each
/// handle is its own socket descriptor and carries its own timeouts. Give each
/// half the deadline it is to run under.
///
/// Neither half starts with a deadline. Each handle keeps the timeouts
/// [`run_tls_handshake`] left on the socket it was cloned from until
/// [`TlsReader::set_deadline`] or [`TlsWriter::set_deadline`] sets or clears
/// that half's own.
///
/// # Errors
/// Returns the failure of duplicating the socket handle.
pub fn split_tls_stream(
    tls_connection: rustls::Connection,
    socket: TcpStream,
) -> io::Result<(TlsReader, TlsWriter)> {
    let read_socket = socket.try_clone()?;
    let shared_connection = Arc::new(Mutex::new(tls_connection));
    Ok((
        TlsReader {
            tls_connection: Arc::clone(&shared_connection),
            socket: read_socket,
            deadline: None,
            encrypted_read_buffer: Box::new([0u8; TLS_READ_BUFFER_BYTE_COUNT]),
            encrypted_bytes_consumed: 0,
            encrypted_byte_count: 0,
        },
        TlsWriter {
            tls_connection: shared_connection,
            socket,
            deadline: None,
        },
    ))
}

/// Store `deadline` in `deadline_slot`. When it is `None`, clear the read and the write
/// timeout on `socket` as well; a failure to clear one is ignored.
fn store_socket_deadline(
    socket: &TcpStream,
    deadline_slot: &mut Option<Instant>,
    deadline: Option<Instant>,
) {
    *deadline_slot = deadline;
    if deadline.is_none() {
        let _ = socket.set_read_timeout(None);
        let _ = socket.set_write_timeout(None);
    }
}

/// The reading half of a TLS stream: plaintext out of the encrypted bytes the
/// peer sends.
pub struct TlsReader {
    /// The encryption state, shared with the writing half.
    tls_connection: Arc<Mutex<rustls::Connection>>,
    /// This half's own handle on the socket.
    socket: TcpStream,
    /// When every read this half has left must be finished by, or `None` to
    /// block for as long as it takes.
    deadline: Option<Instant>,
    /// The buffer each socket read lands in before decryption, allocated once
    /// for the half's lifetime.
    encrypted_read_buffer: Box<[u8; TLS_READ_BUFFER_BYTE_COUNT]>,
    /// How many of `encrypted_read_buffer`'s first `encrypted_byte_count` bytes the decryption state has
    /// taken so far. The bytes between `encrypted_bytes_consumed` and `encrypted_byte_count` are handed to it
    /// before the socket is read again.
    encrypted_bytes_consumed: usize,
    /// How many bytes of `encrypted_read_buffer` the last socket read filled.
    encrypted_byte_count: usize,
}

impl fmt::Debug for TlsReader {
    /// Writes the socket and the deadline, and none of the read buffer's
    /// bytes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsReader")
            .field("socket", &self.socket)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl TlsReader {
    /// Give this half a deadline, or `None` to take its deadline away.
    ///
    /// Every socket read this half makes ends by `deadline`, however many
    /// reads it takes to produce one helping of plaintext. `None` clears both
    /// timeouts on this half's handle; a read after it blocks for as long as
    /// it takes.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        store_socket_deadline(&self.socket, &mut self.deadline, deadline);
    }
}

impl Read for TlsReader {
    /// Fill `plaintext_buffer` with plaintext, reading and decrypting as much of the
    /// socket as it takes to produce some. An empty `plaintext_buffer` is `Ok(0)` at
    /// once.
    ///
    /// The peer closing the stream cleanly reads as end of stream, `Ok(0)`,
    /// on this read and every read after it. A peer that drops the socket
    /// without closing the stream is [`io::ErrorKind::UnexpectedEof`], again
    /// on every read after it, and bytes that do not decrypt are
    /// [`io::ErrorKind::InvalidData`].
    ///
    /// With a deadline set, each socket read and write is given the time left
    /// until that deadline. No time left before a socket call ends the read
    /// with [`io::ErrorKind::TimedOut`]; the deadline passing during a socket
    /// call ends it with the socket's own timeout error, which
    /// [`is_io_timeout`] recognises on every platform.
    fn read(&mut self, plaintext_buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            {
                let mut tls_connection = self.tls_connection.lock().expect("tls connection");
                match tls_connection.reader().read(plaintext_buffer) {
                    Ok(plaintext_byte_count) => return Ok(plaintext_byte_count),
                    Err(read_error) if read_error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(read_error) => return Err(read_error),
                }
            }
            // Bytes already off the socket are decrypted before it is read
            // again. `read_tls` takes as many of them as its own buffer has
            // room for and reports how many; the rest wait for the next pass.
            if self.encrypted_bytes_consumed < self.encrypted_byte_count {
                let mut tls_connection = self.tls_connection.lock().expect("tls connection");
                let mut unprocessed_encrypted_bytes = &self.encrypted_read_buffer
                    [self.encrypted_bytes_consumed..self.encrypted_byte_count];
                self.encrypted_bytes_consumed +=
                    tls_connection.read_tls(&mut unprocessed_encrypted_bytes)?;
                decrypt_pending_tls_records(&mut tls_connection)?;
                send_pending_tls_records(&mut tls_connection, &mut self.socket, self.deadline)?;
                continue;
            }
            set_socket_timeouts_until(&self.socket, self.deadline)?;
            // No lock is held during the socket read; the writing half keeps
            // working while this half waits.
            self.encrypted_byte_count = self.socket.read(&mut self.encrypted_read_buffer[..])?;
            self.encrypted_bytes_consumed = 0;
            if self.encrypted_byte_count == 0 {
                // End of stream: the empty read is handed to the decryption
                // state. The plaintext read above then reports a clean close
                // as `Ok(0)` and a cut stream as `UnexpectedEof`.
                let mut tls_connection = self.tls_connection.lock().expect("tls connection");
                tls_connection.read_tls(&mut io::empty())?;
                decrypt_pending_tls_records(&mut tls_connection)?;
            }
        }
    }
}

/// The writing half of a TLS stream: plaintext in, encrypted bytes onto the
/// socket.
#[derive(Debug)]
pub struct TlsWriter {
    /// The encryption state, shared with the reading half.
    tls_connection: Arc<Mutex<rustls::Connection>>,
    /// This half's own handle on the socket.
    socket: TcpStream,
    /// When every write this half has left must be finished by, or `None` to
    /// block for as long as it takes.
    deadline: Option<Instant>,
}

impl TlsWriter {
    /// Give this half a deadline, or `None` to take its deadline away.
    ///
    /// Every socket write this half makes ends by `deadline`, however many
    /// writes it takes to put one helping of plaintext on the socket. `None`
    /// clears both timeouts on this half's handle; a write after it blocks for
    /// as long as it takes.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        store_socket_deadline(&self.socket, &mut self.deadline, deadline);
    }
}

impl Write for TlsWriter {
    /// Encrypt the first part of `plaintext_bytes` and put it on the socket. Returns
    /// how many bytes it took: at most 64 KiB per call, the send buffer limit
    /// rustls applies to one plaintext write. `write_all` delivers the rest.
    ///
    /// Encrypted bytes a write that ran out of time did not put on the socket
    /// stay queued. The next write drains that queue before it offers its own
    /// plaintext, so a full queue does not make that write take `0` bytes.
    ///
    /// With a deadline set, each socket write is given the time left until
    /// that deadline. No time left before a socket write ends the call with
    /// [`io::ErrorKind::TimedOut`]; the deadline passing during one ends it
    /// with the socket's own timeout error, which [`is_io_timeout`] recognises
    /// on every platform.
    fn write(&mut self, plaintext_bytes: &[u8]) -> io::Result<usize> {
        let mut tls_connection = self.tls_connection.lock().expect("tls connection");
        // Encrypted bytes an earlier write left queued go out first. They fill
        // the same 64 KiB rustls takes one plaintext write into.
        send_pending_tls_records(&mut tls_connection, &mut self.socket, self.deadline)?;
        let written_plaintext_byte_count = tls_connection.writer().write(plaintext_bytes)?;
        send_pending_tls_records(&mut tls_connection, &mut self.socket, self.deadline)?;
        Ok(written_plaintext_byte_count)
    }

    /// Does nothing: [`write`](Self::write) already put every encrypted byte
    /// on the socket.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl crate::transport::Deadlined for TlsReader {
    fn set_deadline(&mut self, deadline: Option<Instant>) {
        TlsReader::set_deadline(self, deadline);
    }
}

impl crate::transport::Deadlined for TlsWriter {
    fn set_deadline(&mut self, deadline: Option<Instant>) {
        TlsWriter::set_deadline(self, deadline);
    }
}

/// A socket that gives every read and write the time left until one deadline.
///
/// The handshake hands this to rustls in place of the socket. rustls reads
/// and writes it many times inside one call; each of those reads and writes
/// is given the time left when it starts.
struct BoundedSocket<'a> {
    /// The socket the reads and writes go to.
    socket: &'a mut TcpStream,
    /// When every read and write must be finished by.
    deadline: Instant,
}

impl Read for BoundedSocket<'_> {
    fn read(&mut self, encrypted_buffer: &mut [u8]) -> io::Result<usize> {
        set_socket_timeouts_until(self.socket, Some(self.deadline))?;
        self.socket.read(encrypted_buffer)
    }
}

impl Write for BoundedSocket<'_> {
    fn write(&mut self, socket_write_bytes: &[u8]) -> io::Result<usize> {
        set_socket_timeouts_until(self.socket, Some(self.deadline))?;
        self.socket.write(socket_write_bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.socket.flush()
    }
}

/// Run the TLS handshake on `socket` until it is done or `deadline` passes.
///
/// Every blocking read and write inside the handshake is given the time left
/// until `deadline` when it starts. A deadline already reached returns before
/// the socket is touched. The timeouts the last read or write set stay on
/// `socket` after the handshake.
///
/// # Errors
/// [`io::ErrorKind::TimedOut`] with the text `the TLS handshake did not
/// finish in time` when the deadline passes; otherwise the handshake's own
/// failure, which is [`io::ErrorKind::UnexpectedEof`] when the peer hangs up
/// mid-handshake and [`io::ErrorKind::InvalidData`] when its bytes are not a
/// handshake this side accepts.
pub fn run_tls_handshake(
    tls_connection: &mut rustls::Connection,
    socket: &mut TcpStream,
    deadline: Instant,
) -> io::Result<()> {
    let mut bounded = BoundedSocket { socket, deadline };
    while tls_connection.is_handshaking() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the TLS handshake did not finish in time",
            ));
        }
        match tls_connection.complete_io(&mut bounded) {
            Ok(_) => {}
            // A socket timeout: the loop goes back to the deadline check.
            Err(handshake_error) if is_io_timeout(&handshake_error) => {}
            Err(handshake_error) => return Err(handshake_error),
        }
    }
    Ok(())
}

/// The sha256 of one certificate's DER bytes, as 64 lowercase hex characters.
///
/// Example — the three bytes `abc` give
/// `ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad`.
#[must_use]
pub fn compute_certificate_fingerprint(certificate_der_bytes: &[u8]) -> String {
    crate::bytes::format_hex(&Sha256::digest(certificate_der_bytes))
}

/// The aws-lc-rs cryptography provider, built fresh on each call.
///
/// With rustls's `prefer-post-quantum` feature on, the key exchange groups it
/// offers are `X25519MLKEM768`, `X25519`, `SECP256R1` and `SECP384R1`, in that
/// order. With the feature off, `X25519MLKEM768` moves last.
///
/// [`connect_tls_stream`] builds its client configuration from this value. A caller that
/// builds a [`rustls::ServerConfig`], or hands a provider to an HTTP client,
/// passes this value in.
#[must_use]
pub fn build_crypto_provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// The signature algorithms [`build_crypto_provider`] verifies handshake signatures
/// with.
fn get_supported_signature_algorithms() -> WebPkiSupportedAlgorithms {
    build_crypto_provider().signature_verification_algorithms
}

/// Checks a server's certificate against the fingerprint saved from an
/// earlier connection to it.
///
/// The certificate is never checked against a certificate authority. The
/// handshake signature is checked in full: the proof that the server holds
/// the private key of the certificate it presented.
#[derive(Debug)]
pub struct PinVerifier {
    /// The fingerprint saved from an earlier connection, or `None` on the
    /// first connection to this server.
    expected_certificate_fingerprint: Option<String>,
    /// The fingerprint the server presented, filled in during the handshake.
    presented_certificate_fingerprint: Mutex<Option<String>>,
}

impl PinVerifier {
    /// A verifier that accepts whatever certificate the server presents when
    /// `expected_certificate_fingerprint` is `None`, and only that one fingerprint otherwise.
    #[must_use]
    pub fn from_expected_certificate_fingerprint(
        expected_certificate_fingerprint: Option<&str>,
    ) -> PinVerifier {
        PinVerifier {
            expected_certificate_fingerprint: expected_certificate_fingerprint.map(str::to_string),
            presented_certificate_fingerprint: Mutex::new(None),
        }
    }

    /// The fingerprint the server presented, or `None` while the handshake
    /// has not reached the server's certificate.
    #[must_use]
    pub fn get_presented_certificate_fingerprint(&self) -> Option<String> {
        self.presented_certificate_fingerprint
            .lock()
            .expect("pinned certificate")
            .clone()
    }
}

impl ServerCertVerifier for PinVerifier {
    /// Records the fingerprint of `end_entity_certificate` as
    /// [`get_presented_certificate_fingerprint`](Self::get_presented_certificate_fingerprint), then
    /// accepts it when no fingerprint is pinned or the pinned one is equal,
    /// and refuses it otherwise with `the pinned certificate is <pinned>, the
    /// server presented <presented>`.
    fn verify_server_cert(
        &self,
        end_entity_certificate: &CertificateDer<'_>,
        _intermediate_certificates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let presented_certificate_fingerprint =
            compute_certificate_fingerprint(end_entity_certificate.as_ref());
        *self
            .presented_certificate_fingerprint
            .lock()
            .expect("pinned certificate") = Some(presented_certificate_fingerprint.clone());
        match &self.expected_certificate_fingerprint {
            None => Ok(ServerCertVerified::assertion()),
            Some(pinned_certificate_fingerprint)
                if *pinned_certificate_fingerprint == presented_certificate_fingerprint =>
            {
                Ok(ServerCertVerified::assertion())
            }
            Some(pinned_certificate_fingerprint) => Err(rustls::Error::General(format!(
                "the pinned certificate is {pinned_certificate_fingerprint}, the server presented {presented_certificate_fingerprint}"
            ))),
        }
    }

    fn verify_tls12_signature(
        &self,
        handshake_message: &[u8],
        certificate: &CertificateDer<'_>,
        signed_message: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            handshake_message,
            certificate,
            signed_message,
            &get_supported_signature_algorithms(),
        )
    }

    fn verify_tls13_signature(
        &self,
        handshake_message: &[u8],
        certificate: &CertificateDer<'_>,
        signed_message: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            handshake_message,
            certificate,
            signed_message,
            &get_supported_signature_algorithms(),
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        get_supported_signature_algorithms().supported_schemes()
    }
}

/// Open a TLS stream to `address`, refusing a certificate whose fingerprint
/// is not `expected_certificate_fingerprint`.
///
/// `address` is `host:port`. The lookup is the operating system's own, and
/// the first address it returns is the one dialled. Returns the two halves of
/// the stream and the fingerprint the server presented, which the caller
/// saves on a first connection.
///
/// `connection_timeout` bounds everything after the name lookup: the connect and the
/// whole handshake share one deadline, and both halves come back holding
/// that deadline; the caller's opening exchange finishes inside it as well.
/// [`TlsReader::set_deadline`] and [`TlsWriter::set_deadline`] with `None`
/// end the window once the caller is past that exchange.
///
/// # Errors
/// [`IpcError::ConnectRefused`] when nothing accepts the connection,
/// [`IpcError::ConnectTimedOut`] when `connection_timeout` names an instant the clock
/// cannot reach, when no time is left after the lookup, or when the connect is
/// unanswered at the deadline, and
/// [`IpcError::TlsHandshakeFailed`] when the handshake on an open connection
/// does not finish, carrying [`run_tls_handshake`]'s own words. A fingerprint that
/// does not match the pinned one is [`IpcError::CertificateChanged`].
/// [`IpcError::Transport`] names the rest: the lookup (`<address> could not
/// be looked up: invalid socket address` for an address with no port), a
/// connect that failed for another reason, a server that presented no
/// certificate, and a stream that did not split.
pub fn connect_tls_stream(
    server_address: &str,
    expected_certificate_fingerprint: Option<&str>,
    connection_timeout: Duration,
) -> Result<(TlsReader, TlsWriter, String), IpcError> {
    let deadline = Instant::now()
        .checked_add(connection_timeout)
        .ok_or_else(|| IpcError::ConnectTimedOut {
            server_address: server_address.to_string(),
        })?;
    let resolved_server_address = server_address
        .to_socket_addrs()
        .map_err(|io_error| {
            build_transport_error(format!(
                "{server_address} could not be looked up: {io_error}"
            ))
        })?
        .next()
        .ok_or_else(|| build_transport_error(format!("{server_address} names no address")))?;
    let remaining_connection_duration = deadline.saturating_duration_since(Instant::now());
    if remaining_connection_duration.is_zero() {
        return Err(IpcError::ConnectTimedOut {
            server_address: server_address.to_string(),
        });
    }
    let mut socket =
        TcpStream::connect_timeout(&resolved_server_address, remaining_connection_duration)
            .map_err(|io_error| match io_error.kind() {
                io::ErrorKind::ConnectionRefused => IpcError::ConnectRefused {
                    server_address: server_address.to_string(),
                },
                io::ErrorKind::TimedOut => IpcError::ConnectTimedOut {
                    server_address: server_address.to_string(),
                },
                _ => build_transport_error(format!(
                    "{server_address} could not be reached: {io_error}"
                )),
            })?;

    let certificate_verifier = Arc::new(PinVerifier::from_expected_certificate_fingerprint(
        expected_certificate_fingerprint,
    ));
    let client_config = ClientConfig::builder_with_provider(build_crypto_provider())
        .with_safe_default_protocol_versions()
        .expect("aws-lc-rs supports every default protocol version")
        .dangerous()
        .with_custom_certificate_verifier(
            Arc::clone(&certificate_verifier) as Arc<dyn ServerCertVerifier>
        )
        .with_no_client_auth();
    // The stream is named by the IP address it reached. An IP address sends
    // no server name.
    let server_name =
        ServerName::try_from(resolved_server_address.ip().to_string()).map_err(|io_error| {
            build_transport_error(format!(
                "{server_address} is not a usable server name: {io_error}"
            ))
        })?;
    let tls_client =
        ClientConnection::new(Arc::new(client_config), server_name).map_err(|io_error| {
            build_transport_error(format!(
                "the TLS stream to {server_address} could not start: {io_error}"
            ))
        })?;
    let mut tls_connection = rustls::Connection::Client(tls_client);

    if let Err(handshake_error) = run_tls_handshake(&mut tls_connection, &mut socket, deadline) {
        return Err(
            match (
                expected_certificate_fingerprint,
                certificate_verifier.get_presented_certificate_fingerprint(),
            ) {
                (Some(pinned_certificate_fingerprint), Some(presented_certificate_fingerprint))
                    if pinned_certificate_fingerprint != presented_certificate_fingerprint =>
                {
                    IpcError::CertificateChanged {
                        server_address: server_address.to_string(),
                        pinned_certificate: pinned_certificate_fingerprint.to_string(),
                        presented_certificate: presented_certificate_fingerprint,
                    }
                }
                _ => IpcError::TlsHandshakeFailed {
                    server_address: server_address.to_string(),
                    error_detail: handshake_error.to_string(),
                },
            },
        );
    }
    let presented_certificate_fingerprint = certificate_verifier
        .get_presented_certificate_fingerprint()
        .ok_or_else(|| {
            build_transport_error(format!("{server_address} presented no certificate"))
        })?;
    let (mut reader, mut writer) =
        split_tls_stream(tls_connection, socket).map_err(|io_error| {
            build_transport_error(format!(
                "the stream to {server_address} could not split: {io_error}"
            ))
        })?;
    reader.set_deadline(Some(deadline));
    writer.set_deadline(Some(deadline));
    Ok((reader, writer, presented_certificate_fingerprint))
}

/// Build a transport error for a connection step that did not open.
fn build_transport_error(error_detail: String) -> IpcError {
    IpcError::Transport { error_detail }
}

#[cfg(test)]
mod tests;
