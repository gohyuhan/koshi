//! The TLS stream a remote client and the machine serving it talk over.
//!
//! One [`rustls::Connection`] holds the encryption state of one stream. Both
//! halves of a split stream share it behind a mutex; one thread reads
//! plaintext while another writes plaintext.
//! [`split_tls`](crate::tls::split_tls) makes that pair, and
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
//! [`dial`](crate::tls::dial) takes one timeout and turns it into one
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
use crate::transport::waited_out;

/// How many raw bytes [`TlsReader`] takes off the socket in one read.
const RAW_CHUNK: usize = 16 * 1024;

/// The least a socket timeout is set to: 1 ms. Less time left than this
/// counts as no time left.
const LEAST_WAIT: Duration = Duration::from_millis(1);

/// Set both of `sock`'s timeouts to the time left until `deadline`. `None`
/// returns at once and leaves the timeouts as they are.
///
/// Every blocking read and write calls this first; each call reads the time
/// left again.
///
/// # Errors
/// [`io::ErrorKind::TimedOut`] with the text `this step ran out of time` when
/// less than [`LEAST_WAIT`] is left; otherwise the failure of setting a
/// socket timeout.
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

/// Decrypt the bytes `conn` has taken and queue whatever it answers with.
///
/// # Errors
/// [`io::ErrorKind::InvalidData`], carrying rustls's own words, when the
/// bytes do not decrypt.
fn decrypt_pending(conn: &mut rustls::Connection) -> io::Result<()> {
    conn.process_new_packets()
        .map(|_| ())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

/// Put every encrypted byte `conn` has queued on `sock`. Each socket write is
/// given the time left until `deadline`; `None` lets it block for as long as
/// it takes.
///
/// # Errors
/// [`io::ErrorKind::TimedOut`] when no time is left before a write; otherwise
/// the socket write's own failure, which is the socket's timeout error when
/// the deadline passes during the write.
fn send_pending(
    conn: &mut rustls::Connection,
    sock: &mut TcpStream,
    deadline: Option<Instant>,
) -> io::Result<()> {
    while conn.wants_write() {
        set_timeouts_until(sock, deadline)?;
        conn.write_tls(sock)?;
    }
    Ok(())
}

/// Split a TLS stream into its reading and its writing half.
///
/// Each half gets its own handle on the same socket; one half blocking on
/// the socket does not stop the other. Both halves share one
/// [`rustls::Connection`] behind a mutex.
///
/// A socket timeout belongs to the socket, not to a handle. A half with a
/// deadline sets both socket timeouts before each of its socket calls; a half
/// with no deadline sets nothing and runs under whatever timeouts the other
/// half set last.
///
/// Neither half starts with a deadline. The socket keeps the timeouts
/// [`handshake`] left on it until [`TlsReader::set_deadline`] or
/// [`TlsWriter::set_deadline`] sets or clears them.
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

/// Store `deadline` in `slot`. When it is `None`, clear both timeouts on
/// `sock` as well; a failure to clear one is ignored.
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
    /// Writes the socket and the deadline, and none of the read buffer's
    /// bytes.
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
    /// reads it takes to produce one helping of plaintext. `None` clears both
    /// timeouts on the socket; a read after it blocks for as long as it takes
    /// until the writing half sets the socket timeouts for a write of its
    /// own.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        store_deadline(&self.sock, &mut self.deadline, deadline);
    }
}

impl Read for TlsReader {
    /// Fill `buffer` with plaintext, reading and decrypting as much of the
    /// socket as it takes to produce some. An empty `buffer` is `Ok(0)` at
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
    /// [`waited_out`] recognises on every platform.
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
            // again. `read_tls` takes as many of them as its own buffer has
            // room for and reports how many; the rest wait for the next pass.
            if self.fed < self.filled {
                let mut conn = self.conn.lock().expect("tls connection");
                let mut remaining = &self.raw[self.fed..self.filled];
                self.fed += conn.read_tls(&mut remaining)?;
                decrypt_pending(&mut conn)?;
                send_pending(&mut conn, &mut self.sock, self.deadline)?;
                continue;
            }
            set_timeouts_until(&self.sock, self.deadline)?;
            // No lock is held during the socket read; the writing half keeps
            // working while this half waits.
            self.filled = self.sock.read(&mut self.raw[..])?;
            self.fed = 0;
            if self.filled == 0 {
                // End of stream: the empty read is handed to the decryption
                // state. The plaintext read above then reports a clean close
                // as `Ok(0)` and a cut stream as `UnexpectedEof`.
                let mut conn = self.conn.lock().expect("tls connection");
                conn.read_tls(&mut io::empty())?;
                decrypt_pending(&mut conn)?;
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
    /// writes it takes to put one helping of plaintext on the socket. `None`
    /// clears both timeouts on the socket; a write after it blocks for as
    /// long as it takes until the reading half sets the socket timeouts for a
    /// read of its own.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        store_deadline(&self.sock, &mut self.deadline, deadline);
    }
}

impl Write for TlsWriter {
    /// Encrypt the first part of `bytes` and put it on the socket. Returns
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
    /// with the socket's own timeout error, which [`waited_out`] recognises
    /// on every platform.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut conn = self.conn.lock().expect("tls connection");
        // Encrypted bytes an earlier write left queued go out first. They fill
        // the same 64 KiB rustls takes one plaintext write into.
        send_pending(&mut conn, &mut self.sock, self.deadline)?;
        let taken = conn.writer().write(bytes)?;
        send_pending(&mut conn, &mut self.sock, self.deadline)?;
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
/// The handshake hands this to rustls in place of the socket. rustls reads
/// and writes it many times inside one call; each of those reads and writes
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
/// until `deadline` when it starts. A deadline already reached returns before
/// the socket is touched. The timeouts the last read or write set stay on
/// `sock` after the handshake.
///
/// # Errors
/// [`io::ErrorKind::TimedOut`] with the text `the TLS handshake did not
/// finish in time` when the deadline passes; otherwise the handshake's own
/// failure, which is [`io::ErrorKind::UnexpectedEof`] when the peer hangs up
/// mid-handshake and [`io::ErrorKind::InvalidData`] when its bytes are not a
/// handshake this side accepts.
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
            // A socket timeout: the loop goes back to the deadline check.
            Err(error) if waited_out(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// The sha256 of one certificate's DER bytes, as 64 lowercase hex characters.
///
/// Example — the three bytes `abc` give
/// `ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad`.
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
/// The certificate is never checked against a certificate authority. The
/// handshake signature is checked in full: the proof that the server holds
/// the private key of the certificate it presented.
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
    /// Records the fingerprint of `end_entity` as [`seen`](Self::seen), then
    /// accepts it when no fingerprint is pinned or the pinned one is equal,
    /// and refuses it otherwise with `the pinned certificate is <pinned>, the
    /// server presented <presented>`.
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
/// `address` is `host:port`. The lookup is the operating system's own, and
/// the first address it returns is the one dialled. Returns the two halves of
/// the stream and the fingerprint the server presented, which the caller
/// saves on a first connection.
///
/// `timeout` bounds everything after the name lookup: the connect and the
/// whole handshake share one deadline, and both halves come back holding
/// that deadline; the caller's opening exchange finishes inside it as well.
/// [`TlsReader::set_deadline`] and [`TlsWriter::set_deadline`] with `None`
/// end the window once the caller is past that exchange.
///
/// # Errors
/// [`IpcError::ConnectRefused`] when nothing accepts the connection,
/// [`IpcError::ConnectTimedOut`] when `timeout` names an instant the clock
/// cannot reach, when no time is left after the lookup, or when the connect is
/// unanswered at the deadline, and
/// [`IpcError::TlsHandshakeFailed`] when the handshake on an open connection
/// does not finish, carrying [`handshake`]'s own words. A fingerprint that
/// does not match the pinned one is [`IpcError::CertificateChanged`].
/// [`IpcError::Transport`] names the rest: the lookup (`<address> could not
/// be looked up: invalid socket address` for an address with no port), a
/// connect that failed for another reason, a server that presented no
/// certificate, and a stream that did not split.
pub fn dial(
    address: &str,
    expected_fingerprint: Option<&str>,
    timeout: Duration,
) -> Result<(TlsReader, TlsWriter, String), IpcError> {
    let deadline =
        Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| IpcError::ConnectTimedOut {
                address: address.to_string(),
            })?;
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
    // The stream is named by the IP address it reached. An IP address sends
    // no server name.
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
