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
//! [`Connection::is_peer_same_user`](crate::transport::Connection::is_peer_same_user)
//! reports whether a connected peer runs as the same OS user this process
//! does.
//!
//! A received length prefix is checked against
//! [`MAX_FRAME_BYTE_COUNT`](crate::transport::MAX_FRAME_BYTE_COUNT) before the payload
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
pub const MAX_FRAME_BYTE_COUNT: u32 = 16 * 1024 * 1024;

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
fn resolve_socket_name(socket_address: &str) -> io::Result<socket::Name<'_>> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        socket_address.to_fs_name::<GenericFilePath>()
    }
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        socket_address.to_ns_name::<GenericNamespaced>()
    }
}

/// The server end of the control socket: binds the address and accepts one
/// [`Connection`] per caller.
///
/// Dropping the listener releases the address; on Unix the socket file is
/// unlinked.
#[derive(Debug)]
pub struct Listener {
    socket_listener: socket::Listener,
}

impl Listener {
    /// Bind `socket_address` and start listening. Fails if the address is already
    /// bound, does not fit the platform's socket namespace, or the OS
    /// refuses.
    pub fn bind(socket_address: &str) -> Result<Listener, IpcError> {
        let platform_socket_name = resolve_socket_name(socket_address).map_err(convert_io_error)?;
        let socket_listener = ListenerOptions::new()
            .name(platform_socket_name)
            .create_sync()
            .map_err(convert_io_error)?;
        Ok(Listener { socket_listener })
    }

    /// Bind `socket_address` and start listening, with the other local users of this
    /// machine able to open it.
    ///
    /// On Windows the pipe is created with a security descriptor that grants
    /// the Authenticated Users group read and write. On Unix this binds
    /// exactly as [`bind`](Self::bind) does: the socket file arrives at the
    /// mode the process umask leaves, and the caller widens it afterwards.
    pub fn bind_shared(socket_address: &str) -> Result<Listener, IpcError> {
        #[cfg(unix)]
        {
            Listener::bind(socket_address)
        }
        #[cfg(windows)]
        {
            use interprocess::os::windows::local_socket::ListenerOptionsExt;
            use interprocess::os::windows::security_descriptor::SecurityDescriptor;

            let access =
                SecurityDescriptor::deserialize(SHARED_PIPE_ACCESS).map_err(convert_io_error)?;
            let platform_socket_name =
                resolve_socket_name(socket_address).map_err(convert_io_error)?;
            let socket_listener = ListenerOptions::new()
                .name(platform_socket_name)
                .security_descriptor(access)
                .create_sync()
                .map_err(convert_io_error)?;
            Ok(Listener { socket_listener })
        }
    }

    /// Block until a caller connects, then hand back that connection.
    ///
    /// On Windows a caller that connects and gives up occupies the pipe until
    /// the next `accept` clears it.
    pub fn accept(&self) -> Result<Connection, IpcError> {
        let socket_stream = self.socket_listener.accept().map_err(convert_io_error)?;
        Ok(Connection::from_socket_stream(socket_stream))
    }
}

/// How long [`Connection::connect`] waits for the connect to complete: 2
/// seconds. On Unix the connect completes once the OS has queued it for the
/// listener. On Windows a named pipe whose instances all sit unaccepted holds
/// a connect open until the listener accepts; after this long the connect
/// ends with a timed-out error.
pub const CONNECT_WAIT_DURATION: std::time::Duration = std::time::Duration::from_secs(2);

/// One open control-socket connection. Both ends hold one: a caller's comes
/// from [`Connection::connect`], the server's from [`Listener::accept`].
#[derive(Debug)]
pub struct Connection {
    socket_stream: socket::Stream,
    /// Set by [`ReadCloser::close`]. Every read after it reports
    /// [`IpcError::Disconnected`] without touching the socket.
    is_read_closed: Arc<AtomicBool>,
}

impl Connection {
    /// Wrap a connected stream, with its read direction open.
    fn from_socket_stream(socket_stream: socket::Stream) -> Connection {
        Connection {
            socket_stream,
            is_read_closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Connect to the listener at `socket_address`, waiting at most [`CONNECT_WAIT_DURATION`]
    /// for the connect to complete. No listener behind the address — a
    /// leftover file whose process is gone, or nothing there at all — is
    /// [`IpcError::NoListener`]; a wait that runs out is
    /// [`IpcError::Transport`] carrying the timed-out error's words.
    pub fn connect(socket_address: &str) -> Result<Connection, IpcError> {
        let platform_socket_name = resolve_socket_name(socket_address).map_err(convert_io_error)?;
        let socket_stream = ConnectOptions::new()
            .name(platform_socket_name)
            .wait_mode(ConnectWaitMode::Timeout(CONNECT_WAIT_DURATION))
            .connect_sync()
            .map_err(|error| {
                if is_no_listener_error(&error) {
                    IpcError::NoListener {
                        socket_address: socket_address.to_string(),
                    }
                } else {
                    convert_io_error(error)
                }
            })?;
        Ok(Connection::from_socket_stream(socket_stream))
    }

    /// Send one message as one frame. Blocks until the bytes are handed to
    /// the OS.
    pub fn send<Message: Serialize>(&mut self, message: &Message) -> Result<(), IpcError> {
        write_message(&mut self.socket_stream, message)
    }

    /// Read one frame and decode its message as `T`. Blocks until a whole
    /// frame arrives. A connection whose read direction is closed reports
    /// [`IpcError::Disconnected`].
    pub fn recv<Message: DeserializeOwned>(&mut self) -> Result<Message, IpcError> {
        if self.is_read_closed.load(Ordering::SeqCst) {
            return Err(IpcError::Disconnected);
        }
        read_message(&mut self.socket_stream)
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
            is_closed: Arc::clone(&self.is_read_closed),
            #[cfg(unix)]
            duplicated_socket: duplicate_unix_socket(&self.socket_stream)?,
        })
    }

    /// Report whether the peer process runs as the same OS user as this
    /// process: on Unix its effective user id, on Windows the user in its
    /// process token. The OS reports the peer's identity through the socket;
    /// a peer cannot forge it.
    ///
    /// Failing to learn the peer's identity is an error.
    pub fn is_peer_same_user(&self) -> Result<bool, IpcError> {
        let credentials = self.socket_stream.peer_creds().map_err(convert_io_error)?;
        #[cfg(unix)]
        {
            let peer_user_id = credentials.euid().ok_or_else(|| IpcError::Transport {
                error_detail: "the socket reported no peer user id".to_string(),
            })?;
            Ok(peer_user_id == unsafe { libc::geteuid() })
        }
        #[cfg(windows)]
        {
            let process_id = credentials.pid().ok_or_else(|| IpcError::Transport {
                error_detail: "the pipe reported no peer process id".to_string(),
            })?;
            let peer_process_handle =
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
            if peer_process_handle.is_null() {
                return Err(convert_io_error(io::Error::last_os_error()));
            }
            let peer_process_handle = unsafe { OwnedHandle::from_raw_handle(peer_process_handle) };
            let peer_token_buffer = read_token_user_buffer(peer_process_handle.as_raw_handle())?;
            let own_token_buffer = read_token_user_buffer(unsafe { GetCurrentProcess() })?;
            Ok(unsafe {
                EqualSid(
                    get_sid_from_token_buffer(&peer_token_buffer),
                    get_sid_from_token_buffer(&own_token_buffer),
                )
            } != 0)
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
        let (reader_half, writer_half) = self.socket_stream.split();
        (
            FrameReader {
                reader_half: Box::new(reader_half),
                is_closed: self.is_read_closed,
            },
            FrameWriter {
                writer_half: Box::new(writer_half),
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
        let (reader_half, writer_half) = self.socket_stream.split();
        (RawReader(reader_half), RawWriter(writer_half))
    }
}

/// A stream half that can be told when its reads and writes must give up.
///
/// A local socket half takes the deadline and ignores it.
pub trait Deadlined: Send {
    /// Every read and write after this finishes by `deadline`, or blocks for as long
    /// as it takes when `deadline` is `None`.
    fn set_deadline(&mut self, deadline: Option<Instant>);
}

impl Deadlined for socket::RecvHalf {
    /// Does nothing: a local socket read blocks for as long as it takes,
    /// whatever `at` says.
    fn set_deadline(&mut self, _deadline: Option<Instant>) {}
}

impl Deadlined for socket::SendHalf {
    /// Does nothing: a local socket write blocks for as long as it takes,
    /// whatever `at` says.
    fn set_deadline(&mut self, _deadline: Option<Instant>) {}
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
    reader_half: Box<dyn DeadlinedRead>,
    writer_half: Box<dyn DeadlinedWrite>,
) -> (FrameReader, FrameWriter) {
    (
        FrameReader {
            reader_half,
            is_closed: Arc::new(AtomicBool::new(false)),
        },
        FrameWriter { writer_half },
    )
}

/// The reading half of a split [`Connection`]. Sends nothing.
pub struct FrameReader {
    reader_half: Box<dyn DeadlinedRead>,
    /// Set by [`ReadCloser::close`]. Every read after it reports
    /// [`IpcError::Disconnected`] without touching the socket.
    is_closed: Arc<AtomicBool>,
}

impl FrameReader {
    /// Give this half a deadline, or `None` to take its deadline away.
    ///
    /// Example — an attached client holds a deadline through the frames that
    /// join it to a session, and none afterwards.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.reader_half.set_deadline(deadline);
    }
}

impl std::fmt::Debug for FrameReader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FrameReader")
            .field("is_closed", &self.is_closed.load(Ordering::SeqCst))
            .finish()
    }
}

impl FrameReader {
    /// Read one frame and decode its message as `T`. Blocks until a whole
    /// frame arrives. The peer closing its writing end, and a read direction
    /// this side closed, are both [`IpcError::Disconnected`].
    pub fn recv<Message: DeserializeOwned>(&mut self) -> Result<Message, IpcError> {
        if self.is_closed.load(Ordering::SeqCst) {
            return Err(IpcError::Disconnected);
        }
        read_message(&mut self.reader_half)
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
    is_closed: Arc<AtomicBool>,
    /// The connection's socket, duplicated. Both descriptors name one socket;
    /// shutting this one's read direction shuts the connection's.
    #[cfg(unix)]
    duplicated_socket: UnixStream,
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
        self.is_closed.store(true, Ordering::SeqCst);
        #[cfg(unix)]
        let _ = self.duplicated_socket.shutdown(Shutdown::Read);
    }
}

/// Duplicate the socket a connection reads and writes. Unix only: a Windows
/// named pipe carries no read direction to shut on its own.
#[cfg(unix)]
fn duplicate_unix_socket(socket_stream: &socket::Stream) -> Result<UnixStream, IpcError> {
    let socket::Stream::UdSocket(uds) = socket_stream;
    uds.inner().try_clone().map_err(convert_io_error)
}

/// The writing half of a split [`Connection`]. Reads nothing.
pub struct FrameWriter {
    writer_half: Box<dyn DeadlinedWrite>,
}

impl FrameWriter {
    /// Give this half a deadline, or `None` to take its deadline away. The
    /// same rule [`FrameReader::set_deadline`] states.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.writer_half.set_deadline(deadline);
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
    pub fn send<Message: Serialize>(&mut self, message: &Message) -> Result<(), IpcError> {
        write_message(&mut self.writer_half, message)
    }
}

/// The reading half of a [`Connection::split_raw`]: the bytes as they arrive,
/// with no frame shape read off them.
#[derive(Debug)]
pub struct RawReader(socket::RecvHalf);

impl Read for RawReader {
    fn read(&mut self, raw_buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(raw_buffer)
    }
}

/// The writing half of a [`Connection::split_raw`]: the bytes go out as
/// given, with no frame shape written around them.
#[derive(Debug)]
pub struct RawWriter(socket::SendHalf);

impl Write for RawWriter {
    fn write(&mut self, raw_bytes: &[u8]) -> io::Result<usize> {
        self.0.write(raw_bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Buffer for one outgoing frame: 4 placeholder length bytes, then the JSON
/// payload as encoding produces it. Refuses the write that crosses
/// [`MAX_FRAME_BYTE_COUNT`], stopping the encoder mid-message; the buffer never
/// grows past the cap.
struct FrameBuffer {
    /// The frame being built: 4 placeholder bytes, then the payload so far.
    frame_bytes: Vec<u8>,
    /// Set by the refused write: the payload size that write reached.
    overflow_byte_count: Option<u64>,
}

impl Write for FrameBuffer {
    fn write(&mut self, frame_bytes: &[u8]) -> io::Result<usize> {
        let frame_byte_count = self.frame_bytes.len() - 4 + frame_bytes.len();
        if frame_byte_count > MAX_FRAME_BYTE_COUNT as usize {
            self.overflow_byte_count = Some(frame_byte_count as u64);
            return Err(io::Error::other("frame over cap"));
        }
        self.frame_bytes.extend_from_slice(frame_bytes);
        Ok(frame_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Encode `message` and write it as one frame: 4-byte big-endian length, then
/// the JSON bytes. The whole frame goes out in one `write_all`. A message
/// past [`MAX_FRAME_BYTE_COUNT`] is refused with nothing written, and its encoding
/// stops at the byte that crossed the cap.
pub(crate) fn write_message<Message: Serialize>(
    writer: &mut impl Write,
    message: &Message,
) -> Result<(), IpcError> {
    let mut frame_buffer = FrameBuffer {
        frame_bytes: vec![0u8; 4],
        overflow_byte_count: None,
    };
    if let Err(serialization_error) = serde_json::to_writer(&mut frame_buffer, message) {
        return Err(match frame_buffer.overflow_byte_count {
            Some(frame_byte_count) => IpcError::FrameTooLarge {
                frame_byte_count,
                maximum_frame_byte_count: MAX_FRAME_BYTE_COUNT,
            },
            None => IpcError::MalformedFrame {
                error_detail: serialization_error.to_string(),
            },
        });
    }
    let frame_byte_count = (frame_buffer.frame_bytes.len() - 4) as u32;
    frame_buffer.frame_bytes[..4].copy_from_slice(&frame_byte_count.to_be_bytes());
    writer
        .write_all(&frame_buffer.frame_bytes)
        .map_err(convert_io_error)
}

/// Read one frame and decode its JSON payload as `T`. The length prefix is
/// checked against [`MAX_FRAME_BYTE_COUNT`] before the payload buffer is allocated.
pub(crate) fn read_message<Message: DeserializeOwned>(
    reader: &mut impl Read,
) -> Result<Message, IpcError> {
    let mut frame_header_bytes = [0u8; 4];
    reader
        .read_exact(&mut frame_header_bytes)
        .map_err(convert_io_error)?;
    let frame_byte_count = u32::from_be_bytes(frame_header_bytes);
    if frame_byte_count > MAX_FRAME_BYTE_COUNT {
        return Err(IpcError::FrameTooLarge {
            frame_byte_count: u64::from(frame_byte_count),
            maximum_frame_byte_count: MAX_FRAME_BYTE_COUNT,
        });
    }
    let mut payload_bytes = vec![0u8; frame_byte_count as usize];
    reader
        .read_exact(&mut payload_bytes)
        .map_err(convert_io_error)?;
    serde_json::from_slice(&payload_bytes).map_err(|decode_error| IpcError::MalformedFrame {
        error_detail: decode_error.to_string(),
    })
}

/// Read the user of `process`'s token: the bytes `GetTokenInformation`
/// writes, which start with a [`TOKEN_USER`] whose `Sid` points into the rest
/// of the same buffer. The buffer is `u64`: [`TOKEN_USER`] needs 8-byte
/// alignment.
#[cfg(windows)]
fn read_token_user_buffer(process_handle: HANDLE) -> Result<Vec<u64>, IpcError> {
    let mut token_handle: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process_handle, TOKEN_QUERY, &mut token_handle) } == 0 {
        return Err(convert_io_error(io::Error::last_os_error()));
    }
    let token_handle = unsafe { OwnedHandle::from_raw_handle(token_handle) };
    // The first call writes no data; it reports the byte count to allocate.
    let mut required_buffer_byte_count: u32 = 0;
    unsafe {
        GetTokenInformation(
            token_handle.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut required_buffer_byte_count,
        );
    }
    let mut token_buffer = vec![0u64; (required_buffer_byte_count as usize).div_ceil(8).max(1)];
    let is_token_information_filled = unsafe {
        GetTokenInformation(
            token_handle.as_raw_handle(),
            TokenUser,
            token_buffer.as_mut_ptr().cast(),
            (token_buffer.len() * 8) as u32,
            &mut required_buffer_byte_count,
        )
    };
    if is_token_information_filled == 0 {
        return Err(convert_io_error(io::Error::last_os_error()));
    }
    Ok(token_buffer)
}

/// The `Sid` pointer inside a buffer [`read_token_user_buffer`] filled. It stays valid
/// only while that buffer lives.
#[cfg(windows)]
fn get_sid_from_token_buffer(token_buffer: &[u64]) -> PSID {
    unsafe { (*token_buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid }
}

/// Accept connections on `listener` until `is_shutting_down` is set, handing
/// each accepted connection to `serve_connection`. A failed accept sleeps `retry_delay`
/// and the loop continues. The flag is read between the accept and the
/// dispatch: the connection accepted after the flag is set is dropped, not
/// served.
pub fn accept_until_shutdown(
    listener: &Listener,
    is_shutting_down: &AtomicBool,
    retry_delay: std::time::Duration,
    mut serve_connection: impl FnMut(Connection),
) {
    loop {
        let connection = listener.accept();
        if is_shutting_down.load(Ordering::SeqCst) {
            break;
        }
        match connection {
            Ok(connection) => serve_connection(connection),
            Err(_) => std::thread::sleep(retry_delay),
        }
    }
}

/// Whether `io_error` is a socket read or write timeout. Unix reports one as
/// [`WouldBlock`](io::ErrorKind::WouldBlock), Windows as
/// [`TimedOut`](io::ErrorKind::TimedOut).
#[must_use]
pub fn is_io_timeout(io_error: &io::Error) -> bool {
    matches!(
        io_error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// True for the connect failures that mean "nothing answers at this
/// address": the connection was refused (a socket file with no listener
/// behind it), nothing exists at the address, or (Unix) the file at the
/// address is not a socket. Linux refuses a non-socket file with
/// `ECONNREFUSED`, macOS with `ENOTSOCK`; both are checked.
fn is_no_listener_error(io_error: &io::Error) -> bool {
    if matches!(
        io_error.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
    ) {
        return true;
    }
    #[cfg(unix)]
    if io_error.raw_os_error() == Some(libc::ENOTSOCK) {
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
fn convert_io_error(io_error: io::Error) -> IpcError {
    match io_error.kind() {
        io::ErrorKind::UnexpectedEof
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::NotConnected => IpcError::Disconnected,
        _ => IpcError::Transport {
            error_detail: io_error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests;
