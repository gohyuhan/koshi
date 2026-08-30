//! Framed messages over the local control socket.
//!
//! One running Koshi binds a [`Listener`](crate::transport::Listener); each
//! caller opens a [`Connection`](crate::transport::Connection) to it. On Unix
//! the socket is a Unix domain socket at a filesystem path; on Windows it is
//! a named pipe addressed by bare name (`koshi-…`, which the OS serves as
//! `\\.\pipe\koshi-…`). Both sides speak the same frame shape: a 4-byte
//! big-endian length, then that many bytes of JSON encoding one message from
//! [`protocol`](crate::protocol).
//!
//! [`Listener::bind_shared`](crate::transport::Listener::bind_shared) binds a
//! socket the other local users of this machine may open too, and
//! [`Connection::peer_is_same_user`](crate::transport::Connection::peer_is_same_user)
//! reports whether a connected peer runs as the same OS user this process
//! does.
//!
//! A received length prefix is checked against
//! [`MAX_FRAME_LEN`](crate::transport::MAX_FRAME_LEN) before the payload
//! buffer is allocated: a length over it is refused after four bytes are
//! read.
//!
//! [`Connection::read_closer`](crate::transport::Connection::read_closer)
//! hands out a [`ReadCloser`](crate::transport::ReadCloser): the handle another
//! thread holds to end the reading side of a connection while the thread
//! serving it is blocked reading. The writing side is left alone.
//!
//! The frame shape is not tied to the local socket.
//! [`frame_halves`](crate::transport::frame_halves) puts it on any other pair
//! of byte streams, such as the two halves of a TLS stream, and
//! [`Connection::split_raw`](crate::transport::Connection::split_raw) hands
//! back a local socket's two halves with no frame shape read off them, for
//! carrying somebody else's frames through.

use std::io::{self, Read, Write};
#[cfg(unix)]
use std::net::Shutdown;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use interprocess::local_socket::traits::{Listener as _, Stream as _, StreamCommon as _};
use interprocess::local_socket::{self as socket, ConnectOptions, ListenerOptions};
use interprocess::ConnectWaitMode;
use serde::de::DeserializeOwned;
use serde::Serialize;
#[cfg(windows)]
use windows_sys::Win32::Foundation::HANDLE;
#[cfg(windows)]
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, TokenUser, PSID, TOKEN_QUERY, TOKEN_USER,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::error::IpcError;

/// The largest frame either side sends or accepts: 16 MiB. A received length
/// over it is refused before the payload is allocated; a message that encodes
/// past it is refused with nothing written.
pub const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

/// What the shared control pipe grants, in Windows' [security descriptor
/// string format][sddl]: the Authenticated Users group — every user logged in
/// to this machine — may open the pipe for reading and writing.
///
/// `0x0012019f` is `FILE_GENERIC_READ | FILE_GENERIC_WRITE` spelled out: the
/// rights `GENERIC_READ | GENERIC_WRITE`, which a caller opens the pipe with,
/// map to. `FILE_APPEND_DATA` (`0x4`) inside that set is also
/// `FILE_CREATE_PIPE_INSTANCE`: the right the server uses to create the pipe
/// instance that serves the next caller.
///
/// [sddl]: https://learn.microsoft.com/en-us/windows/win32/secauthz/security-descriptor-string-format
#[cfg(windows)]
const SHARED_PIPE_ACCESS: &widestring::U16CStr = widestring::u16cstr!("D:(A;;0x0012019f;;;AU)");

/// Map a control-socket address — the string an endpoint file stores — to the
/// platform's socket name: a socket-file path on Unix, a pipe name on
/// Windows.
fn socket_name(addr: &str) -> io::Result<socket::Name<'_>> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        addr.to_fs_name::<GenericFilePath>()
    }
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        addr.to_ns_name::<GenericNamespaced>()
    }
}

/// The server end of the control socket: binds the address and accepts one
/// [`Connection`] per caller.
///
/// Dropping the listener releases the address; on Unix the socket file is
/// unlinked.
#[derive(Debug)]
pub struct Listener {
    inner: socket::Listener,
}

impl Listener {
    /// Bind `addr` and start listening. Fails if the address is already
    /// bound, does not fit the platform's socket namespace, or the OS
    /// refuses.
    pub fn bind(addr: &str) -> Result<Listener, IpcError> {
        let name = socket_name(addr).map_err(io_failure)?;
        let inner = ListenerOptions::new()
            .name(name)
            .create_sync()
            .map_err(io_failure)?;
        Ok(Listener { inner })
    }

    /// Bind `addr` and start listening, with the other local users of this
    /// machine able to open it.
    ///
    /// On Windows the pipe is created with a security descriptor that grants
    /// the Authenticated Users group read and write. On Unix this binds
    /// exactly as [`bind`](Self::bind) does: the socket file arrives at the
    /// mode the process umask leaves, and the caller widens it afterwards.
    pub fn bind_shared(addr: &str) -> Result<Listener, IpcError> {
        #[cfg(unix)]
        {
            Listener::bind(addr)
        }
        #[cfg(windows)]
        {
            use interprocess::os::windows::local_socket::ListenerOptionsExt;
            use interprocess::os::windows::security_descriptor::SecurityDescriptor;

            let access = SecurityDescriptor::deserialize(SHARED_PIPE_ACCESS).map_err(io_failure)?;
            let name = socket_name(addr).map_err(io_failure)?;
            let inner = ListenerOptions::new()
                .name(name)
                .security_descriptor(access)
                .create_sync()
                .map_err(io_failure)?;
            Ok(Listener { inner })
        }
    }

    /// Block until a caller connects, then hand back that connection.
    ///
    /// On Windows a caller that connects and gives up occupies the pipe until
    /// the next `accept` clears it.
    pub fn accept(&self) -> Result<Connection, IpcError> {
        let stream = self.inner.accept().map_err(io_failure)?;
        Ok(Connection::new(stream))
    }
}

/// How long [`Connection::connect`] waits for the connect to complete: 2
/// seconds. On Unix the connect completes once the OS has queued it for the
/// listener. On Windows a named pipe whose instances all sit unaccepted holds
/// a connect open until the listener accepts; after this long the connect
/// ends with a timed-out error.
pub const CONNECT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// One open control-socket connection. Both ends hold one: a caller's comes
/// from [`Connection::connect`], the server's from [`Listener::accept`].
#[derive(Debug)]
pub struct Connection {
    stream: socket::Stream,
    /// Set by [`ReadCloser::close`]. Every read after it reports
    /// [`IpcError::Disconnected`] without touching the socket.
    read_closed: Arc<AtomicBool>,
}

impl Connection {
    /// Wrap a connected stream, with its read direction open.
    fn new(stream: socket::Stream) -> Connection {
        Connection {
            stream,
            read_closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Connect to the listener at `addr`, waiting at most [`CONNECT_WAIT`]
    /// for the connect to complete. No listener behind the address — a
    /// leftover file whose process is gone, or nothing there at all — is
    /// [`IpcError::NoListener`]; a wait that runs out is
    /// [`IpcError::Transport`] carrying the timed-out error's words.
    pub fn connect(addr: &str) -> Result<Connection, IpcError> {
        let name = socket_name(addr).map_err(io_failure)?;
        let stream = ConnectOptions::new()
            .name(name)
            .wait_mode(ConnectWaitMode::Timeout(CONNECT_WAIT))
            .connect_sync()
            .map_err(|error| {
                if no_listener_error(&error) {
                    IpcError::NoListener {
                        addr: addr.to_string(),
                    }
                } else {
                    io_failure(error)
                }
            })?;
        Ok(Connection::new(stream))
    }

    /// Send one message as one frame. Blocks until the bytes are handed to
    /// the OS.
    pub fn send<T: Serialize>(&mut self, message: &T) -> Result<(), IpcError> {
        write_message(&mut self.stream, message)
    }

    /// Read one frame and decode its message as `T`. Blocks until a whole
    /// frame arrives. A connection whose read direction is closed reports
    /// [`IpcError::Disconnected`].
    pub fn recv<T: DeserializeOwned>(&mut self) -> Result<T, IpcError> {
        if self.read_closed.load(Ordering::SeqCst) {
            return Err(IpcError::Disconnected);
        }
        read_message(&mut self.stream)
    }

    /// Take the handle on this connection's read direction, for another thread
    /// to close with while this one reads.
    ///
    /// The socket is duplicated; the handle stays usable after the connection
    /// is split, moved to another thread or dropped. One connection may hand
    /// out several; each closes the same read direction.
    ///
    /// # Errors
    /// Returns the failure of duplicating the socket.
    pub fn read_closer(&self) -> Result<ReadCloser, IpcError> {
        Ok(ReadCloser {
            closed: Arc::clone(&self.read_closed),
            #[cfg(unix)]
            socket: duplicate_socket(&self.stream)?,
        })
    }

    /// Report whether the peer process runs as the same OS user as this
    /// process: on Unix its effective user id, on Windows the user in its
    /// process token. The OS reports the peer's identity through the socket;
    /// a peer cannot forge it.
    ///
    /// Failing to learn the peer's identity is an error.
    pub fn peer_is_same_user(&self) -> Result<bool, IpcError> {
        let creds = self.stream.peer_creds().map_err(io_failure)?;
        #[cfg(unix)]
        {
            let peer = creds.euid().ok_or_else(|| IpcError::Transport {
                detail: "the socket reported no peer user id".to_string(),
            })?;
            Ok(peer == unsafe { libc::geteuid() })
        }
        #[cfg(windows)]
        {
            let pid = creds.pid().ok_or_else(|| IpcError::Transport {
                detail: "the pipe reported no peer process id".to_string(),
            })?;
            let peer_process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if peer_process.is_null() {
                return Err(io_failure(io::Error::last_os_error()));
            }
            let peer_process = unsafe { OwnedHandle::from_raw_handle(peer_process) };
            let peer = token_user(peer_process.as_raw_handle())?;
            let own = token_user(unsafe { GetCurrentProcess() })?;
            Ok(unsafe { EqualSid(sid_of(&peer), sid_of(&own)) } != 0)
        }
    }

    /// Split the connection into its reading and its writing half; one
    /// thread reads frames while another writes them. Both halves speak the
    /// same frame shape the whole connection did.
    ///
    /// The connection is consumed: after this, [`send`](Self::send) and
    /// [`recv`](Self::recv) are the halves' own methods.
    #[must_use]
    pub fn split(self) -> (FrameReader, FrameWriter) {
        let (recv_half, send_half) = self.stream.split();
        (
            FrameReader {
                half: Box::new(recv_half),
                closed: self.read_closed,
            },
            FrameWriter {
                half: Box::new(send_half),
            },
        )
    }

    /// Split the connection into its reading and its writing half, with no
    /// framing: each half carries the bytes as they arrive and as they are
    /// written.
    ///
    /// The connection is consumed.
    #[must_use]
    pub fn split_raw(self) -> (RawReader, RawWriter) {
        let (recv_half, send_half) = self.stream.split();
        (RawReader(recv_half), RawWriter(send_half))
    }
}

/// A stream half that can be told when its reads and writes must give up.
///
/// A local socket half takes the deadline and ignores it.
pub trait Deadlined: Send {
    /// Every read and write after this finishes by `at`, or blocks for as long
    /// as it takes when `at` is `None`.
    fn set_deadline(&mut self, at: Option<Instant>);
}

impl Deadlined for socket::RecvHalf {
    /// Does nothing: a local socket read blocks for as long as it takes,
    /// whatever `at` says.
    fn set_deadline(&mut self, _at: Option<Instant>) {}
}

impl Deadlined for socket::SendHalf {
    /// Does nothing: a local socket write blocks for as long as it takes,
    /// whatever `at` says.
    fn set_deadline(&mut self, _at: Option<Instant>) {}
}

/// The reading half of a stream, with a deadline it may be given later.
pub trait DeadlinedRead: Read + Deadlined {}
impl<T: Read + Deadlined> DeadlinedRead for T {}

/// The writing half of a stream, with a deadline it may be given later.
pub trait DeadlinedWrite: Write + Deadlined {}
impl<T: Write + Deadlined> DeadlinedWrite for T {}

/// Wrap a byte-stream pair as the two halves of a framed connection; a
/// stream that is not a local socket then speaks the same frame shape a
/// [`Connection`] does.
///
/// The reader starts open: no [`ReadCloser`] reaches these halves.
///
/// Each half keeps whatever deadline it already carries, and
/// [`FrameReader::set_deadline`] and [`FrameWriter::set_deadline`] reach it
/// through the box.
#[must_use]
pub fn frame_halves(
    reader: Box<dyn DeadlinedRead>,
    writer: Box<dyn DeadlinedWrite>,
) -> (FrameReader, FrameWriter) {
    (
        FrameReader {
            half: reader,
            closed: Arc::new(AtomicBool::new(false)),
        },
        FrameWriter { half: writer },
    )
}

/// The reading half of a split [`Connection`]. Sends nothing.
pub struct FrameReader {
    half: Box<dyn DeadlinedRead>,
    /// Set by [`ReadCloser::close`]. Every read after it reports
    /// [`IpcError::Disconnected`] without touching the socket.
    closed: Arc<AtomicBool>,
}

impl FrameReader {
    /// Give this half a deadline, or `None` to take its deadline away.
    ///
    /// Example — an attached client holds a deadline through the frames that
    /// join it to a session, and none afterwards.
    pub fn set_deadline(&mut self, at: Option<Instant>) {
        self.half.set_deadline(at);
    }
}

impl std::fmt::Debug for FrameReader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FrameReader")
            .field("closed", &self.closed.load(Ordering::SeqCst))
            .finish()
    }
}

impl FrameReader {
    /// Read one frame and decode its message as `T`. Blocks until a whole
    /// frame arrives. The peer closing its writing end, and a read direction
    /// this side closed, are both [`IpcError::Disconnected`].
    pub fn recv<T: DeserializeOwned>(&mut self) -> Result<T, IpcError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(IpcError::Disconnected);
        }
        read_message(&mut self.half)
    }
}

/// The handle on one connection's read direction, held by a thread other than
/// the one reading that connection. Taken with
/// [`Connection::read_closer`].
///
/// The handle keeps working after the connection is split: it closes the read
/// direction of both a [`Connection`] and the [`FrameReader`] it splits into.
#[derive(Debug)]
pub struct ReadCloser {
    /// Shared with the connection this handle came from.
    closed: Arc<AtomicBool>,
    /// The connection's socket, duplicated. Both descriptors name one socket;
    /// shutting this one's read direction shuts the connection's.
    #[cfg(unix)]
    socket: UnixStream,
}

impl ReadCloser {
    /// Close the connection's read direction: every [`Connection::recv`] and
    /// [`FrameReader::recv`] from here reports [`IpcError::Disconnected`]. The
    /// writing direction stays open; a reply written after this goes out.
    ///
    /// On Unix a read the reader is already blocked in ends as well: the
    /// socket's read direction is shut. A Windows named pipe has no half-close:
    /// a read already waiting on the pipe ends when its peer sends the next
    /// frame or hangs up, and every read after that one reports end of stream.
    ///
    /// Closing an already-closed read direction changes nothing.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        #[cfg(unix)]
        let _ = self.socket.shutdown(Shutdown::Read);
    }
}

/// Duplicate the socket a connection reads and writes. Unix only: a Windows
/// named pipe carries no read direction to shut on its own.
#[cfg(unix)]
fn duplicate_socket(stream: &socket::Stream) -> Result<UnixStream, IpcError> {
    let socket::Stream::UdSocket(uds) = stream;
    uds.inner().try_clone().map_err(io_failure)
}

/// The writing half of a split [`Connection`]. Reads nothing.
pub struct FrameWriter {
    half: Box<dyn DeadlinedWrite>,
}

impl FrameWriter {
    /// Give this half a deadline, or `None` to take its deadline away. The
    /// same rule [`FrameReader::set_deadline`] states.
    pub fn set_deadline(&mut self, at: Option<Instant>) {
        self.half.set_deadline(at);
    }
}

impl std::fmt::Debug for FrameWriter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("FrameWriter").finish()
    }
}

impl FrameWriter {
    /// Send one message as one frame. Blocks until the bytes are handed to
    /// the OS.
    pub fn send<T: Serialize>(&mut self, message: &T) -> Result<(), IpcError> {
        write_message(&mut self.half, message)
    }
}

/// The reading half of a [`Connection::split_raw`]: the bytes as they arrive,
/// with no frame shape read off them.
#[derive(Debug)]
pub struct RawReader(socket::RecvHalf);

impl Read for RawReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

/// The writing half of a [`Connection::split_raw`]: the bytes go out as
/// given, with no frame shape written around them.
#[derive(Debug)]
pub struct RawWriter(socket::SendHalf);

impl Write for RawWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Buffer for one outgoing frame: 4 placeholder length bytes, then the JSON
/// payload as encoding produces it. Refuses the write that crosses
/// [`MAX_FRAME_LEN`], stopping the encoder mid-message; the buffer never
/// grows past the cap.
struct FrameBuffer {
    /// The frame being built: 4 placeholder bytes, then the payload so far.
    bytes: Vec<u8>,
    /// Set by the refused write: the payload size that write reached.
    overflow: Option<u64>,
}

impl Write for FrameBuffer {
    fn write(&mut self, chunk: &[u8]) -> io::Result<usize> {
        let reached = self.bytes.len() - 4 + chunk.len();
        if reached > MAX_FRAME_LEN as usize {
            self.overflow = Some(reached as u64);
            return Err(io::Error::other("frame over cap"));
        }
        self.bytes.extend_from_slice(chunk);
        Ok(chunk.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Encode `message` and write it as one frame: 4-byte big-endian length, then
/// the JSON bytes. The whole frame goes out in one `write_all`. A message
/// past [`MAX_FRAME_LEN`] is refused with nothing written, and its encoding
/// stops at the byte that crossed the cap.
pub(crate) fn write_message<T: Serialize>(
    writer: &mut impl Write,
    message: &T,
) -> Result<(), IpcError> {
    let mut frame = FrameBuffer {
        bytes: vec![0u8; 4],
        overflow: None,
    };
    if let Err(error) = serde_json::to_writer(&mut frame, message) {
        return Err(match frame.overflow {
            Some(len) => IpcError::FrameTooLarge {
                len,
                max: MAX_FRAME_LEN,
            },
            None => IpcError::MalformedFrame {
                detail: error.to_string(),
            },
        });
    }
    let len = (frame.bytes.len() - 4) as u32;
    frame.bytes[..4].copy_from_slice(&len.to_be_bytes());
    writer.write_all(&frame.bytes).map_err(io_failure)
}

/// Read one frame and decode its JSON payload as `T`. The length prefix is
/// checked against [`MAX_FRAME_LEN`] before the payload buffer is allocated.
pub(crate) fn read_message<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T, IpcError> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header).map_err(io_failure)?;
    let len = u32::from_be_bytes(header);
    if len > MAX_FRAME_LEN {
        return Err(IpcError::FrameTooLarge {
            len: u64::from(len),
            max: MAX_FRAME_LEN,
        });
    }
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).map_err(io_failure)?;
    serde_json::from_slice(&payload).map_err(|error| IpcError::MalformedFrame {
        detail: error.to_string(),
    })
}

/// Read the user of `process`'s token: the bytes `GetTokenInformation`
/// writes, which start with a [`TOKEN_USER`] whose `Sid` points into the rest
/// of the same buffer. The buffer is `u64`: [`TOKEN_USER`] needs 8-byte
/// alignment.
#[cfg(windows)]
fn token_user(process: HANDLE) -> Result<Vec<u64>, IpcError> {
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io_failure(io::Error::last_os_error()));
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    // The first call writes no data; it reports the byte count to allocate.
    let mut needed: u32 = 0;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
    let filled = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 8) as u32,
            &mut needed,
        )
    };
    if filled == 0 {
        return Err(io_failure(io::Error::last_os_error()));
    }
    Ok(buffer)
}

/// The `Sid` pointer inside a buffer [`token_user`] filled. It stays valid
/// only while that buffer lives.
#[cfg(windows)]
fn sid_of(buffer: &[u64]) -> PSID {
    unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid }
}

/// Accept connections on `listener` until `shutting_down` is set, handing
/// each accepted connection to `serve`. A failed accept sleeps `retry_delay`
/// and the loop continues. The flag is read between the accept and the
/// dispatch: the connection accepted after the flag is set is dropped, not
/// served.
pub fn accept_until_shutdown(
    listener: &Listener,
    shutting_down: &AtomicBool,
    retry_delay: std::time::Duration,
    mut serve: impl FnMut(Connection),
) {
    loop {
        let connection = listener.accept();
        if shutting_down.load(Ordering::SeqCst) {
            break;
        }
        match connection {
            Ok(connection) => serve(connection),
            Err(_) => std::thread::sleep(retry_delay),
        }
    }
}

/// Whether `error` is a socket read or write timeout. Unix reports one as
/// [`WouldBlock`](io::ErrorKind::WouldBlock), Windows as
/// [`TimedOut`](io::ErrorKind::TimedOut).
#[must_use]
pub fn waited_out(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// True for the connect failures that mean "nothing answers at this
/// address": the connection was refused (a socket file with no listener
/// behind it), nothing exists at the address, or (Unix) the file at the
/// address is not a socket. Linux refuses a non-socket file with
/// `ECONNREFUSED`, macOS with `ENOTSOCK`; both are checked.
fn no_listener_error(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
    ) {
        return true;
    }
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ENOTSOCK) {
        return true;
    }
    false
}

/// Classify an IO failure: the kinds that mean "the peer is gone" become
/// [`IpcError::Disconnected`]; everything else keeps its text as
/// [`IpcError::Transport`].
///
/// [`NotConnected`](io::ErrorKind::NotConnected) is in that first set: macOS
/// reports a read from a socket whose peer has closed as `ENOTCONN`, where
/// Linux reports end of stream.
fn io_failure(error: io::Error) -> IpcError {
    match error.kind() {
        io::ErrorKind::UnexpectedEof
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::NotConnected => IpcError::Disconnected,
        _ => IpcError::Transport {
            detail: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests;
