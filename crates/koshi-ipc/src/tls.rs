//! The TLS stream a remote client and the machine serving it talk over.
//!
//! One [`rustls::Connection`] holds the encryption state of one stream. Both
//! halves of a split stream share it behind a mutex, so one thread reads
//! plaintext while another writes plaintext:
//! [`split_tls`](crate::tls::split_tls) makes that pair, and
//! [`transport::frame_halves`](crate::transport::frame_halves) puts koshi's
//! frame shape on it.
//!
//! The dialling side does not use a certificate authority. It remembers the
//! sha256 of the certificate the server presented on the first connection —
//! its fingerprint, 64 lowercase hex characters — and refuses every later
//! connection that presents a different one.
//! [`PinVerifier`](crate::tls::PinVerifier) holds that rule, and still checks
//! the server's handshake signature, which is what proves the server holds the
//! private key of the certificate it presented.
//!
//! [`dial`](crate::tls::dial) takes one timeout and turns it into one
//! deadline. The connect and the whole handshake finish inside it, whatever
//! pace the server sends its bytes at, and the two halves come back carrying
//! that same deadline, so the opening exchange the caller makes finishes
//! inside it too. The name lookup that runs before the connect is the
//! operating system's own and carries no timeout, so it sits outside the
//! deadline.

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
use crate::transport::waited_out;

/// How large a helping of raw bytes [`TlsReader`] takes off the socket at a
/// time.
const RAW_CHUNK: usize = 16 * 1024;

/// The least a socket timeout may be set to. A socket timeout is counted in
/// milliseconds, so less time left than this is no time left.
const LEAST_WAIT: Duration = Duration::from_millis(1);

/// Set both of `sock`'s timeouts to the time left until `deadline`, when there
/// is one. `None` leaves the timeouts as they are, so the socket blocks for as
/// long as it takes.
///
/// Every blocking read and write calls this first, so the time left is read
/// again for each one and a peer that sends one byte at a time cannot stretch
/// a step past the deadline.
///
/// # Errors
/// [`io::ErrorKind::TimedOut`] when the deadline has passed; otherwise the
/// failure of setting a socket timeout.
fn set_timeouts_until(sock: &TcpStream, deadline: Option<Instant>) -> io::Result<()> {
    let Some(deadline) = deadline else {
        return Ok(());
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining < LEAST_WAIT {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "this step ran out of time",
        ));
    }
    sock.set_read_timeout(Some(remaining))?;
    sock.set_write_timeout(Some(remaining))?;
    Ok(())
}

/// Split a TLS stream into its reading and its writing half.
///
/// Each half gets its own handle on the same socket, so one half blocking on
/// the socket does not stop the other. Both halves share one
/// [`rustls::Connection`] behind a mutex.
///
/// Neither half starts with a deadline: both block for as long as it takes
/// until [`TlsReader::set_deadline`] or [`TlsWriter::set_deadline`] gives them
/// one.
///
/// # Errors
/// Returns the failure of duplicating the socket handle.
pub fn split_tls(conn: rustls::Connection, sock: TcpStream) -> io::Result<(TlsReader, TlsWriter)> {
    let read_sock = sock.try_clone()?;
    let shared = Arc::new(Mutex::new(conn));
    Ok((
        TlsReader {
            conn: Arc::clone(&shared),
            sock: read_sock,
            deadline: None,
            raw: Box::new([0u8; RAW_CHUNK]),
            fed: 0,
            filled: 0,
        },
        TlsWriter {
            conn: shared,
            sock,
            deadline: None,
        },
    ))
}

/// Store `deadline` on the half and, when it is `None`, clear both socket
/// timeouts so every later socket call blocks for as long as it takes.
fn store_deadline(sock: &TcpStream, slot: &mut Option<Instant>, deadline: Option<Instant>) {
    *slot = deadline;
    if deadline.is_none() {
        let _ = sock.set_read_timeout(None);
        let _ = sock.set_write_timeout(None);
    }
}

/// The reading half of a TLS stream: plaintext out of the encrypted bytes the
/// peer sends.
pub struct TlsReader {
    /// The encryption state, shared with the writing half.
    conn: Arc<Mutex<rustls::Connection>>,
    /// This half's own handle on the socket.
    sock: TcpStream,
    /// When every read this half has left must be finished by, or `None` to
    /// block for as long as it takes.
    deadline: Option<Instant>,
    /// The buffer each socket read lands in before decryption, allocated once
    /// for the half's lifetime.
    raw: Box<[u8; RAW_CHUNK]>,
    /// How many of `raw`'s first `filled` bytes the decryption state has
    /// taken so far. The bytes between `fed` and `filled` are handed to it
    /// before the socket is read again.
    fed: usize,
    /// How many bytes of `raw` the last socket read filled.
    filled: usize,
}

impl fmt::Debug for TlsReader {
    /// Writes the half without its read buffer's bytes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsReader")
            .field("sock", &self.sock)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl TlsReader {
    /// Give this half a deadline, or `None` to take its deadline away.
    ///
    /// Every socket read this half makes ends by `deadline`, however many
    /// reads it takes to produce one helping of plaintext. Taking the deadline
    /// away clears the timeouts already on the socket, so every read after it
    /// blocks for as long as it takes.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        store_deadline(&self.sock, &mut self.deadline, deadline);
    }
}

impl Read for TlsReader {
    /// Fill `buffer` with plaintext, reading and decrypting as much of the
    /// socket as it takes to produce some.
    ///
    /// The peer closing the stream cleanly reads as end of stream, `Ok(0)`. A
    /// peer that drops the socket without closing the stream is
    /// [`io::ErrorKind::UnexpectedEof`], and bytes that do not decrypt are
    /// [`io::ErrorKind::InvalidData`].
    ///
    /// With a deadline set, each socket read and write is given the time left
    /// until that deadline, and [`io::ErrorKind::TimedOut`] ends the read once
    /// no time is left.
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            {
                let mut conn = self.conn.lock().expect("tls connection");
                match conn.reader().read(buffer) {
                    Ok(plain) => return Ok(plain),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error),
                }
            }
            // Bytes already off the socket are decrypted before it is read
            // again. `read_tls` reports how many bytes it took, and takes
            // more of the rest on the next pass, once `process_new_packets`
            // has made room.
            if self.fed < self.filled {
                let mut conn = self.conn.lock().expect("tls connection");
                let mut remaining = &self.raw[self.fed..self.filled];
                self.fed += conn.read_tls(&mut remaining)?;
                conn.process_new_packets().map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                })?;
                while conn.wants_write() {
                    set_timeouts_until(&self.sock, self.deadline)?;
                    conn.write_tls(&mut self.sock)?;
                }
                continue;
            }
            set_timeouts_until(&self.sock, self.deadline)?;
            // The socket read blocks with no lock held, so the writing half
            // keeps working while this half waits.
            self.filled = self.sock.read(&mut self.raw[..])?;
            self.fed = 0;
            if self.filled == 0 {
                // End of stream: hand the empty read to the decryption state,
                // so the plaintext read above reports a clean close as `Ok(0)`
                // and a cut stream as `UnexpectedEof`.
                let mut conn = self.conn.lock().expect("tls connection");
                conn.read_tls(&mut io::empty())?;
                conn.process_new_packets().map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                })?;
            }
        }
    }
}

/// The writing half of a TLS stream: plaintext in, encrypted bytes onto the
/// socket.
#[derive(Debug)]
pub struct TlsWriter {
    /// The encryption state, shared with the reading half.
    conn: Arc<Mutex<rustls::Connection>>,
    /// This half's own handle on the socket.
    sock: TcpStream,
    /// When every write this half has left must be finished by, or `None` to
    /// block for as long as it takes.
    deadline: Option<Instant>,
}

impl TlsWriter {
    /// Give this half a deadline, or `None` to take its deadline away.
    ///
    /// Every socket write this half makes ends by `deadline`, however many
    /// writes it takes to put one helping of plaintext on the socket. Taking
    /// the deadline away clears the timeouts already on the socket, so every
    /// write after it blocks for as long as it takes.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        store_deadline(&self.sock, &mut self.deadline, deadline);
    }
}

impl Write for TlsWriter {
    /// Encrypt `bytes` and put them on the socket. Every byte handed in is
    /// taken.
    ///
    /// With a deadline set, each socket write is given the time left until
    /// that deadline, and [`io::ErrorKind::TimedOut`] ends the write once no
    /// time is left.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut conn = self.conn.lock().expect("tls connection");
        let taken = conn.writer().write(bytes)?;
        while conn.wants_write() {
            set_timeouts_until(&self.sock, self.deadline)?;
            conn.write_tls(&mut self.sock)?;
        }
        Ok(taken)
    }

    /// Does nothing: [`write`](Self::write) already put every encrypted byte
    /// on the socket.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl crate::transport::Deadlined for TlsReader {
    fn set_deadline(&mut self, at: Option<Instant>) {
        TlsReader::set_deadline(self, at);
    }
}

impl crate::transport::Deadlined for TlsWriter {
    fn set_deadline(&mut self, at: Option<Instant>) {
        TlsWriter::set_deadline(self, at);
    }
}

/// A socket that gives every read and write the time left until one deadline.
///
/// The handshake hands this to rustls in place of the socket, and rustls reads
/// and writes it many times inside one call, so each of those reads and writes
/// is given the time left when it starts.
struct BoundedSocket<'a> {
    /// The socket the reads and writes go to.
    sock: &'a mut TcpStream,
    /// When every read and write must be finished by.
    deadline: Instant,
}

impl Read for BoundedSocket<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        set_timeouts_until(self.sock, Some(self.deadline))?;
        self.sock.read(buffer)
    }
}

impl Write for BoundedSocket<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        set_timeouts_until(self.sock, Some(self.deadline))?;
        self.sock.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sock.flush()
    }
}

/// Run the TLS handshake on `sock` until it is done or `deadline` passes.
///
/// Every blocking read and write inside the handshake is given the time left
/// until `deadline` when it starts, so a peer that sends one byte at a time
/// cannot stretch the handshake past it.
///
/// # Errors
/// [`io::ErrorKind::TimedOut`] when the deadline passes; otherwise the
/// handshake's own failure.
pub fn handshake(
    conn: &mut rustls::Connection,
    sock: &mut TcpStream,
    deadline: Instant,
) -> io::Result<()> {
    let mut bounded = BoundedSocket { sock, deadline };
    while conn.is_handshaking() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the TLS handshake did not finish in time",
            ));
        }
        match conn.complete_io(&mut bounded) {
            Ok(_) => {}
            // A socket timeout: the loop reads the clock again, and the
            // deadline check above ends the handshake.
            Err(error) if waited_out(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// The sha256 of one certificate's DER bytes, as 64 lowercase hex characters.
#[must_use]
pub fn fingerprint(der: &[u8]) -> String {
    crate::bytes::hex(&Sha256::digest(der))
}

/// The aws-lc-rs cryptography provider, built fresh on each call.
///
/// With rustls's `prefer-post-quantum` feature on, the key exchange groups it
/// offers are `X25519MLKEM768`, `X25519`, `SECP256R1` and `SECP384R1`, in that
/// order. With the feature off, `X25519MLKEM768` moves last.
///
/// [`dial`] builds its client configuration from this value. A caller that
/// builds a [`rustls::ServerConfig`], or hands a provider to an HTTP client,
/// passes this value in.
#[must_use]
pub fn crypto_provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// The signature algorithms [`crypto_provider`] verifies handshake signatures
/// with.
fn signature_algorithms() -> WebPkiSupportedAlgorithms {
    crypto_provider().signature_verification_algorithms
}

/// Checks a server's certificate against the fingerprint saved from an
/// earlier connection to it.
///
/// The certificate itself is never checked against a certificate authority:
/// koshi's servers sign their own. The handshake signature is checked in
/// full, which is what proves the server holds the private key of the
/// certificate it presented.
#[derive(Debug)]
pub struct PinVerifier {
    /// The fingerprint saved from an earlier connection, or `None` on the
    /// first connection to this server.
    expected: Option<String>,
    /// The fingerprint the server presented, filled in during the handshake.
    seen: Mutex<Option<String>>,
}

impl PinVerifier {
    /// A verifier that accepts whatever certificate the server presents when
    /// `expected` is `None`, and only that one fingerprint otherwise.
    #[must_use]
    pub fn new(expected: Option<&str>) -> PinVerifier {
        PinVerifier {
            expected: expected.map(str::to_string),
            seen: Mutex::new(None),
        }
    }

    /// The fingerprint the server presented, or `None` while the handshake
    /// has not reached the server's certificate.
    #[must_use]
    pub fn seen(&self) -> Option<String> {
        self.seen.lock().expect("pinned certificate").clone()
    }
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let presented = fingerprint(end_entity.as_ref());
        *self.seen.lock().expect("pinned certificate") = Some(presented.clone());
        match &self.expected {
            None => Ok(ServerCertVerified::assertion()),
            Some(pinned) if *pinned == presented => Ok(ServerCertVerified::assertion()),
            Some(pinned) => Err(rustls::Error::General(format!(
                "the pinned certificate is {pinned}, the server presented {presented}"
            ))),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &signature_algorithms())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &signature_algorithms())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        signature_algorithms().supported_schemes()
    }
}

/// Open a TLS stream to `address`, refusing a certificate whose fingerprint
/// is not `expected_fingerprint`.
///
/// `address` is `host:port`. Returns the two halves of the stream and the
/// fingerprint the server presented, which the caller saves on a first
/// connection.
///
/// `timeout` bounds everything after the name lookup: the connect and the
/// whole handshake share one deadline, and both halves come back holding that
/// deadline, so the caller's opening exchange finishes inside it as well.
/// [`TlsReader::set_deadline`] and [`TlsWriter::set_deadline`] with `None` end
/// the window once the caller is past that exchange.
///
/// # Errors
/// [`IpcError::ConnectRefused`] when nothing accepts the connection,
/// [`IpcError::ConnectTimedOut`] when the connect is unanswered at the
/// deadline, and [`IpcError::TlsHandshakeFailed`] when the handshake on an
/// open connection does not finish. A fingerprint that does not match the
/// pinned one is [`IpcError::CertificateChanged`]. [`IpcError::Transport`]
/// names the rest: the lookup, a connect that failed for another reason, a
/// server that presented no certificate, and a stream that did not split.
pub fn dial(
    address: &str,
    expected_fingerprint: Option<&str>,
    timeout: Duration,
) -> Result<(TlsReader, TlsWriter, String), IpcError> {
    let deadline = Instant::now() + timeout;
    let resolved = address
        .to_socket_addrs()
        .map_err(|error| failed(format!("{address} could not be looked up: {error}")))?
        .next()
        .ok_or_else(|| failed(format!("{address} names no address")))?;
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(IpcError::ConnectTimedOut {
            address: address.to_string(),
        });
    }
    let mut sock =
        TcpStream::connect_timeout(&resolved, left).map_err(|error| match error.kind() {
            io::ErrorKind::ConnectionRefused => IpcError::ConnectRefused {
                address: address.to_string(),
            },
            io::ErrorKind::TimedOut => IpcError::ConnectTimedOut {
                address: address.to_string(),
            },
            _ => failed(format!("{address} could not be reached: {error}")),
        })?;

    let verifier = Arc::new(PinVerifier::new(expected_fingerprint));
    let config = ClientConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .expect("aws-lc-rs supports every default protocol version")
        .dangerous()
        .with_custom_certificate_verifier(Arc::clone(&verifier) as Arc<dyn ServerCertVerifier>)
        .with_no_client_auth();
    // The certificate is pinned by fingerprint, so the stream is named by the
    // address it reached: an IP address, which sends no server name.
    let name = ServerName::try_from(resolved.ip().to_string())
        .map_err(|error| failed(format!("{address} is not a usable server name: {error}")))?;
    let client = ClientConnection::new(Arc::new(config), name).map_err(|error| {
        failed(format!(
            "the TLS stream to {address} could not start: {error}"
        ))
    })?;
    let mut conn = rustls::Connection::Client(client);

    if let Err(error) = handshake(&mut conn, &mut sock, deadline) {
        return Err(match (expected_fingerprint, verifier.seen()) {
            (Some(pinned), Some(presented)) if pinned != presented => {
                IpcError::CertificateChanged {
                    address: address.to_string(),
                    pinned: pinned.to_string(),
                    presented,
                }
            }
            _ => IpcError::TlsHandshakeFailed {
                address: address.to_string(),
                detail: error.to_string(),
            },
        });
    }
    let presented = verifier
        .seen()
        .ok_or_else(|| failed(format!("{address} presented no certificate")))?;
    let (mut reader, mut writer) = split_tls(conn, sock)
        .map_err(|error| failed(format!("the stream to {address} could not split: {error}")))?;
    reader.set_deadline(Some(deadline));
    writer.set_deadline(Some(deadline));
    Ok((reader, writer, presented))
}

/// A dial that did not open, named in plain words.
fn failed(detail: String) -> IpcError {
    IpcError::Transport { detail }
}

#[cfg(test)]
mod tests;
