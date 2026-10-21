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
//! Bit 31 of the length marks a binary frame, whose payload is raw bytes, and
//! bits 0 to 30 hold the payload's byte count. A JSON frame has bit 31 clear.
//! [`FrameWriter::send_binary`](crate::transport::FrameWriter::send_binary)
//! writes one, and
//! [`FrameReader::recv_message_or_binary`](crate::transport::FrameReader::recv_message_or_binary)
//! and
//! [`Connection::recv_message_or_binary`](crate::transport::Connection::recv_message_or_binary)
//! read JSON and binary frames alike. Every other read takes JSON frames
//! only, and a binary frame there is
//! [`IpcError::UnexpectedBinaryFrame`](crate::error::IpcError::UnexpectedBinaryFrame).
//!
//! Example — the payload `01 02 03` travels as `80 00 00 03 01 02 03`.
//!
//! [`Connection::is_peer_same_user`](crate::transport::Connection::is_peer_same_user)
//! reports whether a connected peer runs as the same OS user this process
//! does.
//! [`Connection::connect_to_own_server_within`](crate::transport::Connection::connect_to_own_server_within)
//! connects only to a server of the same OS user, and closes any other
//! connection before it writes a byte.
//!
//! A received length prefix is checked against
//! [`MAX_FRAME_BYTE_COUNT`](crate::transport::MAX_FRAME_BYTE_COUNT) before the payload
//! buffer is allocated: a length over it is refused after four bytes are
//! read.
//!
//! [`Connection::create_read_closer`](crate::transport::Connection::create_read_closer)
//! hands out a [`ReadCloser`](crate::transport::ReadCloser): the handle another
//! thread holds to end the reading side of a connection while the thread
//! serving it is blocked reading. The writing side is left alone.
//!
//! [`FrameReader::set_deadline`](crate::transport::FrameReader::set_deadline) and
//! [`FrameWriter::set_deadline`](crate::transport::FrameWriter::set_deadline)
//! end each read and write of a split connection by the moment they name.
//! [`FrameReader::set_idle_timeout`](crate::transport::FrameReader::set_idle_timeout)
//! and
//! [`FrameWriter::set_idle_timeout`](crate::transport::FrameWriter::set_idle_timeout)
//! end each socket call that gets or takes no byte for the time they name.
//!
//! The frame shape is not tied to the local socket.
//! [`build_frame_halves`](crate::transport::build_frame_halves) puts it on any other pair
//! of byte streams, such as the two halves of a TLS stream, and
//! [`Connection::split_raw`](crate::transport::Connection::split_raw) hands
//! back a local socket's two halves with no frame shape read off them, for
//! carrying somebody else's frames through.
//!
//! Dropping either end of a connection closes it at once, on every platform.
//! The peer still reads every byte written before the close, then the end of
//! the stream. A caller that writes and closes its end before the listener
//! accepts still has its bytes read once the listener accepts.

use std::io::{self, Read, Write};
#[cfg(unix)]
use std::net::Shutdown;
use std::ops::{Deref, DerefMut};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(windows)]
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(windows)]
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use interprocess::local_socket as socket;
#[cfg(unix)]
use interprocess::local_socket::traits::Listener as _;
#[cfg(unix)]
use interprocess::local_socket::traits::{RecvHalf as _, SendHalf as _};
use interprocess::local_socket::traits::{Stream as _, StreamCommon as _};
#[cfg(unix)]
use interprocess::local_socket::{ConnectOptions, ListenerOptions};
#[cfg(windows)]
use interprocess::os::windows::named_pipe::{
    local_socket as pipe_socket, pipe_mode, DuplexPipeStream, SendPipeStream,
};
#[cfg(windows)]
use interprocess::os::windows::security_descriptor::{AsSecurityDescriptor, SecurityDescriptor};
#[cfg(unix)]
use interprocess::ConnectWaitMode;
use serde::de::DeserializeOwned;
use serde::Serialize;
#[cfg(windows)]
use widestring::{U16CStr, U16CString};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    LocalFree, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE,
};
#[cfg(windows)]
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, GetSecurityInfo, SE_KERNEL_OBJECT,
};
#[cfg(windows)]
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, ImpersonateAnonymousToken, RevertToSelf, TokenOwner, TokenUser,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    TOKEN_INFORMATION_CLASS, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER,
};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
#[cfg(windows)]
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, WaitNamedPipeW, NMPWAIT_WAIT_FOREVER, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentThread, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
#[cfg(windows)]
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use crate::error::IpcError;
use crate::wire::{has_exactly_json_fields, Answer};

/// The largest frame payload either side sends or accepts: 16 MiB, for JSON
/// and binary frames alike. A received length over it is refused before the
/// payload is allocated; a message that encodes past it is refused with
/// nothing written.
pub const MAX_FRAME_BYTE_COUNT: u32 = 16 * 1024 * 1024;

/// Bit 31 of a frame's length prefix: set on a binary frame, clear on a JSON
/// frame.
pub(crate) const BINARY_FRAME_FLAG: u32 = 1 << 31;

/// One frame [`FrameReader::recv_message_or_binary`] reads.
#[derive(Debug, PartialEq, Eq)]
pub enum ReceivedFrame<Message> {
    /// A JSON frame, parsed as `Message`.
    Message(Message),
    /// The payload of a binary frame.
    Binary(Vec<u8>),
}

/// The rights a caller opens a pipe end with: `FILE_GENERIC_READ` and
/// `FILE_WRITE_DATA`, `0x0012008b`. It holds no `FILE_APPEND_DATA` (`0x4`),
/// the bit `FILE_GENERIC_WRITE` holds and `FILE_CREATE_PIPE_INSTANCE` shares.
#[cfg(windows)]
const PIPE_CALLER_ACCESS: u32 = FILE_GENERIC_READ | FILE_WRITE_DATA;

/// Map a control-socket address — the string an endpoint file stores — to the
/// Unix socket-file path it names.
#[cfg(unix)]
fn resolve_socket_name(socket_address: &str) -> io::Result<socket::Name<'_>> {
    use interprocess::local_socket::{GenericFilePath, ToFsName};
    socket_address.to_fs_name::<GenericFilePath>()
}

/// The Windows path of the named pipe at a control-socket address:
/// `\\.\pipe\<socket_address>`, as the NUL-terminated wide string the
/// Windows API takes.
///
/// Example: `koshi-router` gives `\\.\pipe\koshi-router`.
///
/// # Errors
/// [`io::ErrorKind::InvalidInput`] when `socket_address` holds a NUL
/// character.
#[cfg(windows)]
fn format_pipe_path(socket_address: &str) -> io::Result<U16CString> {
    U16CString::from_str(format!(r"\\.\pipe\{socket_address}")).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the pipe name holds a NUL character",
        )
    })
}

/// The server end of the control socket: binds the address and accepts one
/// [`Connection`] per caller.
///
/// Dropping the listener releases the address; on Unix the socket file is
/// unlinked.
#[derive(Debug)]
pub struct Listener {
    /// The Unix domain socket bound at the address.
    #[cfg(unix)]
    socket_listener: socket::Listener,
    /// The named pipe instances created at the address.
    #[cfg(windows)]
    pipe_listener: NamedPipeListener,
}

impl Listener {
    /// Bind `socket_address` and start listening. Fails if the address is already
    /// bound, does not fit the platform's socket namespace, or the OS
    /// refuses.
    pub fn bind(socket_address: &str) -> Result<Listener, IpcError> {
        #[cfg(unix)]
        {
            let platform_socket_name =
                resolve_socket_name(socket_address).map_err(convert_io_error)?;
            let socket_listener = ListenerOptions::new()
                .name(platform_socket_name)
                .create_sync()
                .map_err(convert_io_error)?;
            Ok(Listener { socket_listener })
        }
        #[cfg(windows)]
        {
            let access = build_pipe_access()?;
            let pipe_listener =
                NamedPipeListener::bind(socket_address, access).map_err(convert_io_error)?;
            Ok(Listener { pipe_listener })
        }
    }

    /// Block until a caller connects, then hand back that connection.
    ///
    /// A caller that connected, wrote and closed its end before this call is
    /// handed back too: its bytes are read, then the end of the stream.
    pub fn accept(&self) -> Result<Connection, IpcError> {
        #[cfg(unix)]
        let socket_stream = self.socket_listener.accept().map_err(convert_io_error)?;
        #[cfg(windows)]
        let socket_stream = self.pipe_listener.accept().map_err(convert_io_error)?;
        Ok(Connection::from_socket_stream(socket_stream))
    }
}

/// The size Windows is asked to reserve for each direction of a pipe
/// instance's buffer: 512 bytes. Windows takes it as a hint.
#[cfg(windows)]
const PIPE_BUFFER_BYTE_COUNT: u32 = 512;

/// The server end of a Windows named pipe. One instance at a time waits for
/// the next caller; [`accept`](Self::accept) hands that instance out once a
/// caller opens it and creates the instance the caller after it opens.
#[cfg(windows)]
#[derive(Debug)]
struct NamedPipeListener {
    /// `\\.\pipe\<socket_address>`, as Windows takes a path.
    pipe_path: U16CString,
    /// The security descriptor every instance is created with.
    access: SecurityDescriptor,
    /// The instance the next caller opens.
    listening_instance: Mutex<OwnedHandle>,
}

#[cfg(windows)]
impl NamedPipeListener {
    /// Create the first instance of `\\.\pipe\<socket_address>`, with
    /// `access`.
    ///
    /// # Errors
    /// [`io::ErrorKind::InvalidInput`] when `socket_address` holds a NUL
    /// character. Otherwise the failure of creating the instance, which
    /// includes another process already having an instance of the pipe.
    fn bind(socket_address: &str, access: SecurityDescriptor) -> io::Result<NamedPipeListener> {
        let pipe_path = format_pipe_path(socket_address)?;
        let listening_instance = create_pipe_instance(&pipe_path, &access, true)?;
        Ok(NamedPipeListener {
            pipe_path,
            access,
            listening_instance: Mutex::new(listening_instance),
        })
    }

    /// Block until a caller opens the listening instance. Then wrap a second
    /// handle to the opened instance as a stream with [`wrap_pipe_handle`],
    /// create the instance the next caller opens, and hand the stream out.
    ///
    /// A caller that opened the instance, wrote and closed its end before this
    /// call counts as connected: the stream reads its bytes, then the end of
    /// the stream.
    ///
    /// # Errors
    /// The failure of the wait, of copying the opened instance's handle, of
    /// [`wrap_pipe_handle`], or of creating the next instance. On each one
    /// the opened instance stays the listening one, and the next call hands it
    /// out without a wait.
    fn accept(&self) -> io::Result<socket::Stream> {
        let mut listening_instance = self
            .listening_instance
            .lock()
            .unwrap_or_else(|poison_error| poison_error.into_inner());
        wait_for_pipe_caller(&listening_instance)?;
        let socket_stream = wrap_pipe_handle(listening_instance.try_clone()?)?;
        let next_instance = match create_pipe_instance(&self.pipe_path, &self.access, false) {
            Ok(next_instance) => next_instance,
            Err(create_error) => {
                socket_stream.mark_written_bytes_flushed();
                return Err(create_error);
            }
        };
        *listening_instance = next_instance;
        drop(listening_instance);
        Ok(socket_stream)
    }
}

/// Open the caller's end of the named pipe at `socket_address`, overlapped,
/// with [`PIPE_CALLER_ACCESS`], and with the impersonation level
/// `SECURITY_IDENTIFICATION`: the server may read this process's user, and
/// cannot act as it. While every instance holds a caller the listener has not
/// accepted (`ERROR_PIPE_BUSY`), wait with `WaitNamedPipeW` for a free
/// instance, for the time left until `connect_wait_duration` after the first
/// attempt, and open again.
///
/// # Errors
/// [`io::ErrorKind::InvalidInput`] when `socket_address` holds a NUL
/// character. The failure `CreateFileW` reports for anything but a busy pipe,
/// such as `ERROR_FILE_NOT_FOUND` when no pipe has that name. The failure
/// `WaitNamedPipeW` reports, such as `ERROR_SEM_TIMEOUT` when no instance
/// came free in the time left. [`io::ErrorKind::TimedOut`] when less than 1 ms
/// is left before a wait.
#[cfg(windows)]
fn open_pipe_caller_end(
    socket_address: &str,
    connect_wait_duration: Duration,
) -> io::Result<OwnedHandle> {
    let connect_deadline = Instant::now() + connect_wait_duration;
    let pipe_path = format_pipe_path(socket_address)?;
    loop {
        // SAFETY: `pipe_path` is NUL-terminated, and `CreateFileW` accepts NULL
        // security attributes and a NULL template.
        let pipe_handle = unsafe {
            CreateFileW(
                pipe_path.as_ptr(),
                PIPE_CALLER_ACCESS,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                std::ptr::null_mut(),
            )
        };
        if pipe_handle != INVALID_HANDLE_VALUE {
            // SAFETY: `pipe_handle` is the valid handle that `CreateFileW`
            // returned, and nothing else owns it.
            return Ok(unsafe { OwnedHandle::from_raw_handle(pipe_handle) });
        }
        let open_error = io::Error::last_os_error();
        if open_error.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) {
            return Err(open_error);
        }
        let remaining_duration = connect_deadline.saturating_duration_since(Instant::now());
        if remaining_duration < MINIMUM_STEP_REMAINING_DURATION {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        let wait_milliseconds = u32::try_from(remaining_duration.as_millis())
            .unwrap_or(u32::MAX)
            .min(NMPWAIT_WAIT_FOREVER - 1);
        // SAFETY: `pipe_path` is NUL-terminated.
        if unsafe { WaitNamedPipeW(pipe_path.as_ptr(), wait_milliseconds) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
}

/// Wrap `pipe_handle`, a caller's end or an instance of a named pipe, as a
/// byte-mode local socket stream that keeps `pipe_handle`.
///
/// `interprocess` opens the pipe once more while it wraps a handle
/// (`ReOpenFile` with `GENERIC_READ | GENERIC_WRITE`). When that open
/// succeeds, it connects a new caller end to a free instance of the pipe, and
/// the stream keeps that end in place of `pipe_handle`. The wrap runs while
/// this thread impersonates the anonymous logon token, and the pipe refuses
/// that open with `ERROR_ACCESS_DENIED`.
///
/// # Errors
/// The failure of impersonating the anonymous logon token, or of reading the
/// pipe's flags.
///
/// # Panics
/// Panics when `RevertToSelf` fails and this thread still impersonates the
/// anonymous logon token.
#[cfg(windows)]
fn wrap_pipe_handle(pipe_handle: OwnedHandle) -> io::Result<socket::Stream> {
    // SAFETY: `GetCurrentThread` gives this thread's pseudo handle, which
    // needs no closing.
    if unsafe { ImpersonateAnonymousToken(GetCurrentThread()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let wrap_outcome = DuplexPipeStream::<pipe_mode::Bytes>::try_from(pipe_handle);
    assert!(
        // SAFETY: `RevertToSelf` takes no arguments.
        unsafe { RevertToSelf() } != 0,
        "RevertToSelf failed: the thread still impersonates the anonymous logon token"
    );
    let pipe_stream = wrap_outcome?;
    Ok(socket::Stream::from(pipe_socket::Stream::from(pipe_stream)))
}

/// Create one instance of the named pipe at `pipe_path`: duplex, byte mode,
/// overlapped, callers on other machines refused, any number of instances.
/// It carries `access`. With `is_first_instance` the creation fails when any
/// process already has an instance of the pipe.
///
/// # Errors
/// The failure `CreateNamedPipeW` reports.
#[cfg(windows)]
fn create_pipe_instance(
    pipe_path: &U16CStr,
    access: &SecurityDescriptor,
    is_first_instance: bool,
) -> io::Result<OwnedHandle> {
    let security_attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
            .expect("SECURITY_ATTRIBUTES is a few bytes long"),
        lpSecurityDescriptor: access.as_sd().cast_mut(),
        bInheritHandle: 0,
    };
    let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if is_first_instance {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    // SAFETY: `pipe_path` is NUL-terminated, and `security_attributes` and the
    // descriptor in `access` live for the call.
    let pipe_instance = unsafe {
        CreateNamedPipeW(
            pipe_path.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER_BYTE_COUNT,
            PIPE_BUFFER_BYTE_COUNT,
            0,
            &security_attributes,
        )
    };
    if pipe_instance == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe_instance` is the valid handle that `CreateNamedPipeW`
    // returned, and nothing else owns it.
    Ok(unsafe { OwnedHandle::from_raw_handle(pipe_instance) })
}

/// Block until a caller opens `pipe_instance`. A caller that opened it before
/// this call counts at once, also when it has closed its end since: Windows
/// reports `ERROR_PIPE_CONNECTED` for the one, `ERROR_NO_DATA` for the other.
///
/// # Errors
/// The failure of creating the wait's event, of `ConnectNamedPipe`, or of the
/// wait.
#[cfg(windows)]
fn wait_for_pipe_caller(pipe_instance: &OwnedHandle) -> io::Result<()> {
    // SAFETY: `CreateEventW` accepts NULL security attributes and a NULL name.
    let connect_event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if connect_event.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `connect_event` is the non-NULL handle that `CreateEventW`
    // returned, and nothing else owns it.
    let connect_event = unsafe { OwnedHandle::from_raw_handle(connect_event) };
    // SAFETY: an all-zero `OVERLAPPED` is valid.
    let mut connect_overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    connect_overlapped.hEvent = connect_event.as_raw_handle();
    // SAFETY: `pipe_instance` is open, and `connect_overlapped` outlives the
    // connect: a pending connect ends in `GetOverlappedResult` below, which
    // waits for it.
    if unsafe { ConnectNamedPipe(pipe_instance.as_raw_handle(), &mut connect_overlapped) } != 0 {
        return Ok(());
    }
    let connect_error = io::Error::last_os_error();
    let connect_error_code = connect_error
        .raw_os_error()
        .and_then(|error_code| u32::try_from(error_code).ok());
    match connect_error_code {
        Some(ERROR_PIPE_CONNECTED | ERROR_NO_DATA) => return Ok(()),
        Some(ERROR_IO_PENDING) => {}
        _ => return Err(connect_error),
    }
    let mut transferred_byte_count: u32 = 0;
    // SAFETY: `connect_overlapped` belongs to the pending connect, and
    // `transferred_byte_count` is a writable `u32`.
    let is_connected = unsafe {
        GetOverlappedResult(
            pipe_instance.as_raw_handle(),
            &connect_overlapped,
            &mut transferred_byte_count,
            1,
        )
    } != 0;
    if !is_connected {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// How long [`Connection::connect`] waits for the connect to complete: 2
/// seconds. On Unix the connect completes once the OS has queued it for the
/// listener. On Windows the connect waits for a free pipe instance: a named
/// pipe whose instances all hold callers the listener has not accepted keeps
/// the connect waiting until the listener accepts. After this long the connect
/// ends with a timed-out error.
pub const CONNECT_WAIT_DURATION: std::time::Duration = std::time::Duration::from_secs(2);

/// One open control-socket connection. Both ends hold one: a caller's comes
/// from [`Connection::connect`], the server's from [`Listener::accept`].
#[derive(Debug)]
pub struct Connection {
    socket_stream: WritableSocketEnd<socket::Stream>,
    /// Set by [`ReadCloser::close`]. Every read after it reports
    /// [`IpcError::Disconnected`] without touching the socket.
    is_read_closed: Arc<AtomicBool>,
    /// The pipe's read direction, shared with the reading half this
    /// connection splits into and with every [`ReadCloser`] taken from it.
    #[cfg(windows)]
    pipe_read_watch: Arc<PipeReadWatch>,
}

impl Connection {
    /// Wrap a connected stream, with its read direction open.
    fn from_socket_stream(socket_stream: socket::Stream) -> Connection {
        #[cfg(windows)]
        let socket::Stream::NamedPipe(pipe_stream) = &socket_stream;
        #[cfg(windows)]
        let pipe_read_watch = Arc::new(PipeReadWatch::from_pipe_handle(PipeHandle(
            pipe_stream.inner().as_handle().as_raw_handle(),
        )));
        Connection {
            socket_stream: WritableSocketEnd::from_socket_end(socket_stream),
            is_read_closed: Arc::new(AtomicBool::new(false)),
            #[cfg(windows)]
            pipe_read_watch,
        }
    }

    /// Connect to the listener at `socket_address`, waiting at most [`CONNECT_WAIT_DURATION`]
    /// for the connect to complete, as [`connect_within`](Self::connect_within) does.
    pub fn connect(socket_address: &str) -> Result<Connection, IpcError> {
        Connection::connect_within(socket_address, CONNECT_WAIT_DURATION)
    }

    /// Connect to the listener at `socket_address`, waiting at most
    /// `connect_wait_duration` for the connect to complete. No listener behind
    /// the address — a leftover file whose process is gone, or nothing there
    /// at all — is [`IpcError::NoListener`]; a wait that runs out is
    /// [`IpcError::Transport`] carrying the timed-out error's words.
    ///
    /// On Windows the connect opens the pipe `\\.\pipe\<socket_address>`.
    ///
    /// Example: a Windows pipe whose one instance holds a caller the listener
    /// has not accepted gives a second caller [`IpcError::Transport`] after
    /// `connect_wait_duration`.
    pub fn connect_within(
        socket_address: &str,
        connect_wait_duration: Duration,
    ) -> Result<Connection, IpcError> {
        #[cfg(unix)]
        let connect_outcome = {
            let platform_socket_name =
                resolve_socket_name(socket_address).map_err(convert_io_error)?;
            ConnectOptions::new()
                .name(platform_socket_name)
                .wait_mode(ConnectWaitMode::Timeout(connect_wait_duration))
                .connect_sync()
        };
        #[cfg(windows)]
        let connect_outcome =
            open_pipe_caller_end(socket_address, connect_wait_duration).and_then(wrap_pipe_handle);
        let socket_stream = connect_outcome
            .map_err(|connect_error| convert_connect_error(socket_address, connect_error))?;
        Ok(Connection::from_socket_stream(socket_stream))
    }

    /// Connect to the listener at `socket_address` as
    /// [`connect_within`](Self::connect_within) does, and keep the connection
    /// only when this process's OS user serves it. Nothing is written before
    /// the check, and a connection that fails it is closed.
    ///
    /// On Unix the server's user is the effective user id the socket reports
    /// for the listening process, as [`is_peer_same_user`](Self::is_peer_same_user)
    /// reads it. On Windows it is the owner of the pipe: the user that created
    /// the pipe's first instance, or the user its security descriptor names.
    /// [`bind`](Listener::bind) names this process's user.
    ///
    /// # Errors
    /// What `connect_within` gives, the failure of reading the server's user,
    /// and [`IpcError::PeerIsOtherUser`] naming `socket_address` when another
    /// user serves it.
    ///
    /// Example: on Windows, another user who created the pipe
    /// `\\.\pipe\koshi-<session id>` while that session restarts gives
    /// [`IpcError::PeerIsOtherUser`].
    pub fn connect_to_own_server_within(
        socket_address: &str,
        connect_wait_duration: Duration,
    ) -> Result<Connection, IpcError> {
        #[cfg(unix)]
        {
            let connection = Connection::connect_within(socket_address, connect_wait_duration)?;
            if !connection.is_peer_same_user()? {
                return Err(IpcError::PeerIsOtherUser {
                    socket_address: socket_address.to_string(),
                });
            }
            Ok(connection)
        }
        #[cfg(windows)]
        {
            let pipe_handle = open_pipe_caller_end(socket_address, connect_wait_duration)
                .map_err(|connect_error| convert_connect_error(socket_address, connect_error))?;
            if !is_pipe_owned_by_own_user(&pipe_handle)? {
                return Err(IpcError::PeerIsOtherUser {
                    socket_address: socket_address.to_string(),
                });
            }
            let socket_stream = wrap_pipe_handle(pipe_handle)
                .map_err(|connect_error| convert_connect_error(socket_address, connect_error))?;
            Ok(Connection::from_socket_stream(socket_stream))
        }
    }

    /// Send one message as one frame. Blocks until the bytes are handed to
    /// the OS.
    pub fn send<Message: Serialize>(&mut self, message: &Message) -> Result<(), IpcError> {
        write_message(&mut self.socket_stream, message).map(|_| ())
    }

    /// Read one frame and parse its message as `Message`. Blocks until a
    /// whole frame arrives. A connection whose read direction is closed reports
    /// [`IpcError::Disconnected`].
    pub fn recv<Message: DeserializeOwned>(&mut self) -> Result<Message, IpcError> {
        if self.is_read_closed.load(Ordering::SeqCst) {
            return Err(IpcError::Disconnected);
        }
        read_message(&mut self.build_frame_source())
    }

    /// Read one frame and parse it as an [`Answer`] carrying `Response`, as
    /// [`parse_answer`] does. Blocks until a whole frame arrives. A connection
    /// whose read direction is closed reports [`IpcError::Disconnected`].
    pub fn recv_answer<Response: DeserializeOwned>(
        &mut self,
    ) -> Result<Answer<Response>, IpcError> {
        if self.is_read_closed.load(Ordering::SeqCst) {
            return Err(IpcError::Disconnected);
        }
        read_answer(&mut self.build_frame_source())
    }

    /// Read one frame: a JSON frame parsed as `Message`, or a binary frame's
    /// payload. Blocks until a whole frame arrives. A connection whose read
    /// direction is closed reports [`IpcError::Disconnected`].
    pub fn recv_message_or_binary<Message: DeserializeOwned>(
        &mut self,
    ) -> Result<ReceivedFrame<Message>, IpcError> {
        if self.is_read_closed.load(Ordering::SeqCst) {
            return Err(IpcError::Disconnected);
        }
        read_message_or_binary(&mut self.build_frame_source())
    }

    /// The reader this connection's frames are read through: the socket on
    /// Unix, and on Windows the pipe with each read run through the
    /// connection's `PipeReadWatch`.
    fn build_frame_source(&mut self) -> impl Read + '_ {
        #[cfg(unix)]
        {
            &mut *self.socket_stream
        }
        #[cfg(windows)]
        {
            WatchedPipeReader {
                socket_stream: &mut self.socket_stream,
                pipe_read_watch: &self.pipe_read_watch,
            }
        }
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
    pub fn create_read_closer(&self) -> Result<ReadCloser, IpcError> {
        Ok(ReadCloser {
            is_closed: Arc::clone(&self.is_read_closed),
            #[cfg(unix)]
            duplicated_socket: duplicate_unix_socket(&self.socket_stream)?,
            #[cfg(windows)]
            pipe_read_watch: Arc::clone(&self.pipe_read_watch),
        })
    }

    /// Report whether the peer process runs as the same OS user as this
    /// process: on Unix its effective user id, which the socket reports. On
    /// Windows the pipe reports the peer's process id, and the user is read
    /// from that process's token.
    ///
    /// Failing to learn the peer's identity is an error. On Windows that
    /// includes a peer process of another user: opening it fails with
    /// `ERROR_ACCESS_DENIED`.
    pub fn is_peer_same_user(&self) -> Result<bool, IpcError> {
        let credentials = self.socket_stream.peer_creds().map_err(convert_io_error)?;
        #[cfg(unix)]
        {
            let peer_user_id = credentials.euid().ok_or_else(|| IpcError::Transport {
                error_detail: "the socket reported no peer user id".to_string(),
            })?;
            // SAFETY: `geteuid` takes no arguments and always succeeds.
            let effective_user_id = unsafe { libc::geteuid() };
            Ok(peer_user_id == effective_user_id)
        }
        #[cfg(windows)]
        {
            let process_id = credentials.pid().ok_or_else(|| IpcError::Transport {
                error_detail: "the pipe reported no peer process id".to_string(),
            })?;
            // SAFETY: `OpenProcess` takes its arguments by value and reads no
            // memory.
            let peer_process_handle =
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
            if peer_process_handle.is_null() {
                return Err(convert_io_error(io::Error::last_os_error()));
            }
            // SAFETY: `peer_process_handle` is the non-NULL handle that
            // `OpenProcess` returned, and nothing else owns it.
            let peer_process_handle = unsafe { OwnedHandle::from_raw_handle(peer_process_handle) };
            let peer_token_buffer =
                read_token_information_buffer(peer_process_handle.as_raw_handle(), TokenUser)?;
            let own_token_buffer =
                // SAFETY: `GetCurrentProcess` takes no arguments and gives a
                // pseudo handle that needs no closing.
                read_token_information_buffer(unsafe { GetCurrentProcess() }, TokenUser)?;
            // SAFETY: both buffers were filled for `TokenUser` and live for the
            // call.
            Ok(unsafe {
                EqualSid(
                    get_user_sid_from_token_buffer(&peer_token_buffer),
                    get_user_sid_from_token_buffer(&own_token_buffer),
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
    ///
    /// Neither half starts with a deadline or an idle timeout.
    /// [`FrameReader::set_deadline`] and [`FrameWriter::set_deadline`] give
    /// a deadline, and [`FrameReader::set_idle_timeout`] and
    /// [`FrameWriter::set_idle_timeout`] give an idle timeout.
    #[must_use]
    pub fn split(self) -> (FrameReader, FrameWriter) {
        let (reader_half, writer_half) = self.socket_stream.split();
        (
            FrameReader {
                reader_half: Box::new(LocalSocketReader::from_recv_half(
                    reader_half,
                    #[cfg(windows)]
                    self.pipe_read_watch,
                )),
                is_closed: self.is_read_closed,
            },
            FrameWriter {
                writer_half: Box::new(LocalSocketWriter::from_send_half(writer_half)),
                last_frame_written_at: Instant::now(),
                written_byte_count: 0,
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
        (
            RawReader {
                recv_half: reader_half,
                #[cfg(windows)]
                pipe_read_watch: self.pipe_read_watch,
            },
            RawWriter(writer_half),
        )
    }
}

/// A local socket end that [`WritableSocketEnd`] holds: a whole socket, or
/// the writing half of a split one.
trait LocalSocketEnd {
    /// Mark every byte written to this named pipe end as flushed. Dropping
    /// the end then closes the pipe at once.
    #[cfg(windows)]
    fn mark_written_bytes_flushed(&self);
}

impl LocalSocketEnd for socket::Stream {
    #[cfg(windows)]
    fn mark_written_bytes_flushed(&self) {
        let socket::Stream::NamedPipe(pipe_stream) = self;
        pipe_stream.inner().assume_flushed();
    }
}

impl LocalSocketEnd for socket::SendHalf {
    #[cfg(windows)]
    fn mark_written_bytes_flushed(&self) {
        let socket::SendHalf::NamedPipe(pipe_half) = self;
        let send_pipe_stream: &SendPipeStream<pipe_mode::Bytes> = pipe_half.as_ref();
        send_pipe_stream.assume_flushed();
    }
}

/// A local socket end this process writes to: a whole [`Connection`]'s
/// socket, or the writing half of a split one.
///
/// Dropping the end closes it at once. On Windows the drop first marks the
/// bytes written to the named pipe end as flushed
/// (`LocalSocketEnd::mark_written_bytes_flushed`), and `interprocess` then
/// closes the pipe with no wait. The peer still reads every byte written
/// before the close, then the end of the stream.
///
/// Example: a session drops its end of a client's connection right after it
/// writes `Detached`. The drop returns before the client reads anything, and
/// the client still reads `Detached`, then the end of the stream.
#[derive(Debug)]
struct WritableSocketEnd<SocketEnd: LocalSocketEnd> {
    /// The socket end. `None` only once [`split`](Self::split) took it.
    socket_end: Option<SocketEnd>,
}

impl<SocketEnd: LocalSocketEnd> WritableSocketEnd<SocketEnd> {
    /// Hold `socket_end`.
    fn from_socket_end(socket_end: SocketEnd) -> Self {
        WritableSocketEnd {
            socket_end: Some(socket_end),
        }
    }
}

impl WritableSocketEnd<socket::Stream> {
    /// Split the socket into its reading half and its writing half.
    fn split(mut self) -> (socket::RecvHalf, WritableSocketEnd<socket::SendHalf>) {
        let socket_stream = self
            .socket_end
            .take()
            .expect("the socket end is held until the end is split");
        let (recv_half, send_half) = socket_stream.split();
        (recv_half, WritableSocketEnd::from_socket_end(send_half))
    }
}

impl<SocketEnd: LocalSocketEnd> Deref for WritableSocketEnd<SocketEnd> {
    type Target = SocketEnd;

    fn deref(&self) -> &SocketEnd {
        self.socket_end
            .as_ref()
            .expect("the socket end is held until the end is split")
    }
}

impl<SocketEnd: LocalSocketEnd> DerefMut for WritableSocketEnd<SocketEnd> {
    fn deref_mut(&mut self) -> &mut SocketEnd {
        self.socket_end
            .as_mut()
            .expect("the socket end is held until the end is split")
    }
}

impl<SocketEnd: LocalSocketEnd + Write> Write for WritableSocketEnd<SocketEnd> {
    /// Write `write_bytes` to the socket end.
    fn write(&mut self, write_bytes: &[u8]) -> io::Result<usize> {
        let socket_end: &mut SocketEnd = self;
        socket_end.write(write_bytes)
    }

    /// Return `Ok` at once: a write hands its bytes to the OS, and this end
    /// holds none of them.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<SocketEnd: LocalSocketEnd> Write for &WritableSocketEnd<SocketEnd>
where
    for<'socket_end> &'socket_end SocketEnd: Write,
{
    /// Write `write_bytes` to the socket end.
    fn write(&mut self, write_bytes: &[u8]) -> io::Result<usize> {
        let mut socket_end: &SocketEnd = self;
        socket_end.write(write_bytes)
    }

    /// Return `Ok` at once: a write hands its bytes to the OS, and this end
    /// holds none of them.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
impl<SocketEnd: LocalSocketEnd> Drop for WritableSocketEnd<SocketEnd> {
    /// Mark the bytes written to the end as flushed. The end then closes with
    /// no wait.
    fn drop(&mut self) {
        if let Some(socket_end) = &self.socket_end {
            socket_end.mark_written_bytes_flushed();
        }
    }
}

/// A stream half that can be told when its reads and writes must give up.
pub trait Deadlined: Send {
    /// Every read and write after this finishes by `deadline`, or blocks for as long
    /// as it takes when `deadline` is `None`.
    fn set_deadline(&mut self, deadline: Option<Instant>);

    /// Every socket call after this that gets or takes no byte for
    /// `idle_timeout` ends with a timeout, or waits for as long as it takes
    /// when `idle_timeout` is `None`. A half that makes several socket calls
    /// for one read or write gives each socket call its own `idle_timeout`.
    /// Each socket call ends by the instant [`compute_step_deadline`] gives
    /// when it starts.
    fn set_idle_timeout(&mut self, idle_timeout: Option<Duration>);
}

/// The least time left a step under a deadline starts with: 1 ms. Less time
/// left than this counts as no time left.
const MINIMUM_STEP_REMAINING_DURATION: Duration = Duration::from_millis(1);

/// The time left until `deadline`. A read or write under `deadline` runs out
/// of time once this fails.
///
/// # Errors
/// [`io::ErrorKind::TimedOut`] with the text `this step ran out of time` when
/// less than 1 ms is left.
pub fn compute_remaining_duration_until(deadline: Instant) -> io::Result<Duration> {
    let remaining_duration = deadline.saturating_duration_since(Instant::now());
    if remaining_duration < MINIMUM_STEP_REMAINING_DURATION {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "this step ran out of time",
        ));
    }
    Ok(remaining_duration)
}

/// The instant one socket call that starts now must end by: the earlier of
/// `deadline` and the time now plus `idle_timeout`, or `None` when neither
/// limits it. An `idle_timeout` too large to add to the time now limits
/// nothing.
///
/// Example: a deadline 10 seconds away and a 30-second idle timeout give the
/// deadline. A deadline 60 seconds away and the same idle timeout give the
/// time now plus 30 seconds.
#[must_use]
pub fn compute_step_deadline(
    deadline: Option<Instant>,
    idle_timeout: Option<Duration>,
) -> Option<Instant> {
    let idle_deadline =
        idle_timeout.and_then(|idle_timeout| Instant::now().checked_add(idle_timeout));
    match (deadline, idle_deadline) {
        (Some(deadline), Some(idle_deadline)) => Some(deadline.min(idle_deadline)),
        (Some(deadline), None) => Some(deadline),
        (None, idle_deadline) => idle_deadline,
    }
}

/// The reading half of a split local socket [`Connection`]. A read under a
/// deadline or an idle timeout ends by the earlier of the deadline and the
/// time the read starts plus the idle timeout; a read with neither blocks for
/// as long as it takes.
///
/// A read that runs out of time is [`io::ErrorKind::TimedOut`] with the text
/// `this step ran out of time`.
struct LocalSocketReader {
    /// Dropped before `recv_half`: its thread stops before the pipe closes.
    #[cfg(windows)]
    pipe_step_canceller: PipeStepCanceller,
    /// The pipe's read direction, shared with the connection's
    /// [`ReadCloser`]s.
    #[cfg(windows)]
    pipe_read_watch: Arc<PipeReadWatch>,
    /// The socket's own reading half.
    recv_half: socket::RecvHalf,
    /// When every read must be finished by, or `None` for no limit.
    deadline: Option<Instant>,
    /// The longest one read waits for a byte, or `None` for no limit.
    idle_timeout: Option<Duration>,
}

impl LocalSocketReader {
    /// A reader over `recv_half` with no deadline and no idle timeout. On
    /// Windows each read runs through `pipe_read_watch`, the read direction of
    /// the pipe `recv_half` reads.
    fn from_recv_half(
        recv_half: socket::RecvHalf,
        #[cfg(windows)] pipe_read_watch: Arc<PipeReadWatch>,
    ) -> LocalSocketReader {
        LocalSocketReader {
            #[cfg(windows)]
            pipe_step_canceller: PipeStepCanceller::from_pipe_handle(pipe_read_watch.pipe_handle),
            #[cfg(windows)]
            pipe_read_watch,
            recv_half,
            deadline: None,
            idle_timeout: None,
        }
    }
}

impl Deadlined for LocalSocketReader {
    /// Store `deadline`. On Unix `None` also clears the socket's receive
    /// timeout; a failure to clear it is ignored.
    fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
        #[cfg(unix)]
        if deadline.is_none() {
            let _ = self.recv_half.set_timeout(None);
        }
    }

    /// Store `idle_timeout`. On Unix `None` also clears the socket's receive
    /// timeout; a failure to clear it is ignored.
    fn set_idle_timeout(&mut self, idle_timeout: Option<Duration>) {
        self.idle_timeout = idle_timeout;
        #[cfg(unix)]
        if idle_timeout.is_none() {
            let _ = self.recv_half.set_timeout(None);
        }
    }
}

impl Read for LocalSocketReader {
    /// Read from the socket. Under a deadline or an idle timeout the read ends
    /// by the instant [`compute_step_deadline`] gives when it starts.
    ///
    /// On Unix each attempt first sets the socket's receive timeout to the
    /// time left; a timed-out attempt is tried again with the new time left
    /// until less than 1 ms is left. On Windows the read runs while
    /// `PipeStepCanceller` watches that instant, and runs through the pipe's
    /// `PipeReadWatch`: a read direction a [`ReadCloser`] closed gives
    /// `Ok(0)`.
    ///
    /// # Errors
    /// [`io::ErrorKind::TimedOut`] with the text `this step ran out of time`
    /// once that instant is reached; otherwise the read's own failure.
    fn read(&mut self, read_buffer: &mut [u8]) -> io::Result<usize> {
        let Some(step_deadline) = compute_step_deadline(self.deadline, self.idle_timeout) else {
            #[cfg(unix)]
            return self.recv_half.read(read_buffer);
            #[cfg(windows)]
            return self
                .pipe_read_watch
                .run_pipe_read(|| self.recv_half.read(read_buffer));
        };
        let recv_half = &self.recv_half;
        #[cfg(unix)]
        {
            run_socket_step_until(
                step_deadline,
                |remaining_duration| recv_half.set_timeout(Some(remaining_duration)),
                || {
                    let mut recv_half_reference = recv_half;
                    recv_half_reference.read(read_buffer)
                },
            )
        }
        #[cfg(windows)]
        {
            let pipe_read_watch = &self.pipe_read_watch;
            self.pipe_step_canceller
                .run_pipe_step_until(step_deadline, || {
                    pipe_read_watch.run_pipe_read(|| {
                        let mut recv_half_reference = recv_half;
                        recv_half_reference.read(read_buffer)
                    })
                })
        }
    }
}

/// The writing half of a split local socket [`Connection`]. A write under a
/// deadline or an idle timeout ends by the earlier of the deadline and the
/// time the write starts plus the idle timeout; a write with neither blocks
/// for as long as it takes.
///
/// A write that runs out of time is [`io::ErrorKind::TimedOut`] with the text
/// `this step ran out of time`.
struct LocalSocketWriter {
    /// Dropped before `send_half`: its thread stops before the pipe closes.
    #[cfg(windows)]
    pipe_step_canceller: PipeStepCanceller,
    /// The socket's own writing half.
    send_half: WritableSocketEnd<socket::SendHalf>,
    /// When every write must be finished by, or `None` for no limit.
    deadline: Option<Instant>,
    /// The longest one write waits for the socket to take a byte, or `None`
    /// for no limit.
    idle_timeout: Option<Duration>,
}

impl LocalSocketWriter {
    /// A writer over `send_half` with no deadline and no idle timeout.
    fn from_send_half(send_half: WritableSocketEnd<socket::SendHalf>) -> LocalSocketWriter {
        #[cfg(windows)]
        let socket::SendHalf::NamedPipe(pipe_half) = &*send_half;
        LocalSocketWriter {
            #[cfg(windows)]
            pipe_step_canceller: PipeStepCanceller::from_pipe_handle(PipeHandle(
                pipe_half.as_handle().as_raw_handle(),
            )),
            send_half,
            deadline: None,
            idle_timeout: None,
        }
    }
}

impl Deadlined for LocalSocketWriter {
    /// Store `deadline`. On Unix `None` also clears the socket's send timeout;
    /// a failure to clear it is ignored.
    fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
        #[cfg(unix)]
        if deadline.is_none() {
            let _ = self.send_half.set_timeout(None);
        }
    }

    /// Store `idle_timeout`. On Unix `None` also clears the socket's send
    /// timeout; a failure to clear it is ignored.
    fn set_idle_timeout(&mut self, idle_timeout: Option<Duration>) {
        self.idle_timeout = idle_timeout;
        #[cfg(unix)]
        if idle_timeout.is_none() {
            let _ = self.send_half.set_timeout(None);
        }
    }
}

impl Write for LocalSocketWriter {
    /// Write to the socket. Under a deadline or an idle timeout the write ends
    /// by the instant [`compute_step_deadline`] gives when it starts, the same
    /// way [`LocalSocketReader`]'s read does.
    ///
    /// # Errors
    /// [`io::ErrorKind::TimedOut`] with the text `this step ran out of time`
    /// once that instant is reached; otherwise the write's own failure.
    fn write(&mut self, write_bytes: &[u8]) -> io::Result<usize> {
        let Some(step_deadline) = compute_step_deadline(self.deadline, self.idle_timeout) else {
            return self.send_half.write(write_bytes);
        };
        let send_half = &self.send_half;
        #[cfg(unix)]
        {
            run_socket_step_until(
                step_deadline,
                |remaining_duration| send_half.set_timeout(Some(remaining_duration)),
                || {
                    let mut send_half_reference = send_half;
                    send_half_reference.write(write_bytes)
                },
            )
        }
        #[cfg(windows)]
        {
            self.pipe_step_canceller
                .run_pipe_step_until(step_deadline, || {
                    let mut send_half_reference = send_half;
                    send_half_reference.write(write_bytes)
                })
        }
    }

    /// Return `Ok` at once, as [`WritableSocketEnd`]'s flush does.
    fn flush(&mut self) -> io::Result<()> {
        self.send_half.flush()
    }
}

/// Run `run_step` on a Unix socket so that it ends by `deadline`.
///
/// Each attempt first gives `set_socket_timeout` the time left. An attempt
/// that ends in a timeout ([`is_io_timeout`]) is made again with the new time
/// left. A timeout the socket refuses leaves the
/// attempt to run without it: macOS refuses it on a socket whose peer has
/// closed, and a read there hands back what the peer sent and then the end of
/// the stream, and a write fails at once.
///
/// # Errors
/// [`io::ErrorKind::TimedOut`] with the text `this step ran out of time` when
/// less than 1 ms is left before an attempt; otherwise the attempt's own
/// failure.
#[cfg(unix)]
fn run_socket_step_until<StepOutput>(
    deadline: Instant,
    set_socket_timeout: impl Fn(Duration) -> io::Result<()>,
    mut run_step: impl FnMut() -> io::Result<StepOutput>,
) -> io::Result<StepOutput> {
    loop {
        let remaining_duration = compute_remaining_duration_until(deadline)?;
        let _ = set_socket_timeout(remaining_duration);
        match run_step() {
            Err(step_error) if is_io_timeout(&step_error) => {}
            step_outcome => return step_outcome,
        }
    }
}

/// How often [`PipeStepCanceller`]'s thread cancels a pipe's waiting reads and
/// writes again while a step past its deadline still runs: every 10 ms.
#[cfg(windows)]
const PIPE_CANCEL_REPEAT_INTERVAL_DURATION: Duration = Duration::from_millis(10);

/// A named pipe's handle, carried to the thread of a [`PipeStepCanceller`]
/// and held by a [`PipeReadWatch`].
#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
struct PipeHandle(HANDLE);

// SAFETY: a pipe handle names a kernel object; any thread of the process may
// pass it to `CancelIoEx`.
#[cfg(windows)]
unsafe impl Send for PipeHandle {}

// SAFETY: the handle is only ever passed by value to `CancelIoEx`, which any
// thread may call at the same time as another.
#[cfg(windows)]
unsafe impl Sync for PipeHandle {}

/// What a [`PipeStepCanceller`] and its thread share.
#[cfg(windows)]
#[derive(Default)]
struct PipeStepWatch {
    /// The deadline of the step running now, or `None` while no step under a
    /// deadline runs.
    running_step_deadline: Option<Instant>,
    /// Set when the half is dropped; the thread then ends without touching the
    /// pipe again.
    is_half_dropped: bool,
}

/// The canceller of one named pipe half's steps: a thread that calls
/// `CancelIoEx` on the pipe once the deadline of the step running on it
/// passes, and again every [`PIPE_CANCEL_REPEAT_INTERVAL_DURATION`] while that
/// step still runs.
///
/// Both halves of one pipe share one handle: a cancel ends every read and
/// write waiting on the pipe at that moment, whichever half started it.
///
/// The thread starts with the first step under a deadline, and ends once the
/// half is dropped.
#[cfg(windows)]
struct PipeStepCanceller {
    /// The pipe the thread cancels steps on.
    pipe_handle: PipeHandle,
    /// The step running now and whether the half is dropped, with the signal
    /// that wakes the thread when either changes.
    shared_step_watch: Arc<(Mutex<PipeStepWatch>, Condvar)>,
    /// Whether the thread runs.
    is_thread_started: bool,
}

#[cfg(windows)]
impl PipeStepCanceller {
    /// A canceller for `pipe_handle`, with no thread started.
    fn from_pipe_handle(pipe_handle: PipeHandle) -> PipeStepCanceller {
        PipeStepCanceller {
            pipe_handle,
            shared_step_watch: Arc::new((Mutex::new(PipeStepWatch::default()), Condvar::new())),
            is_thread_started: false,
        }
    }

    /// Run `run_step` on the pipe so that it ends by `deadline`: the thread
    /// cancels it once the deadline passes.
    ///
    /// # Errors
    /// [`io::ErrorKind::TimedOut`] with the text `this step ran out of time`
    /// when less than 1 ms is left before the step, or when the step fails with
    /// less than 1 ms left; otherwise the failure of starting the thread, or the
    /// step's own failure.
    fn run_pipe_step_until<StepOutput>(
        &mut self,
        deadline: Instant,
        run_step: impl FnOnce() -> io::Result<StepOutput>,
    ) -> io::Result<StepOutput> {
        compute_remaining_duration_until(deadline)?;
        self.start_canceller_thread()?;
        self.set_running_step_deadline(Some(deadline));
        let step_outcome = run_step();
        self.set_running_step_deadline(None);
        if step_outcome.is_err() {
            compute_remaining_duration_until(deadline)?;
        }
        step_outcome
    }

    /// Start the thread, once.
    ///
    /// # Errors
    /// Returns the failure of starting the thread.
    fn start_canceller_thread(&mut self) -> io::Result<()> {
        if self.is_thread_started {
            return Ok(());
        }
        let pipe_handle = self.pipe_handle;
        let shared_step_watch = Arc::clone(&self.shared_step_watch);
        std::thread::Builder::new()
            .name("koshi-pipe-deadline".to_string())
            .spawn(move || run_pipe_step_canceller(pipe_handle, &shared_step_watch))?;
        self.is_thread_started = true;
        Ok(())
    }

    /// Record `running_step_deadline` as the deadline of the step running now,
    /// and wake the thread.
    fn set_running_step_deadline(&self, running_step_deadline: Option<Instant>) {
        let (step_watch, step_watch_changed) = &*self.shared_step_watch;
        step_watch
            .lock()
            .expect("pipe step watch")
            .running_step_deadline = running_step_deadline;
        step_watch_changed.notify_one();
    }
}

#[cfg(windows)]
impl Drop for PipeStepCanceller {
    /// Mark the half dropped and wake the thread, which ends without touching
    /// the pipe again.
    fn drop(&mut self) {
        let (step_watch, step_watch_changed) = &*self.shared_step_watch;
        step_watch.lock().expect("pipe step watch").is_half_dropped = true;
        step_watch_changed.notify_one();
    }
}

/// The body of a [`PipeStepCanceller`]'s thread: wait for a step under a
/// deadline, then call `CancelIoEx` on `pipe_handle` once its deadline has
/// passed, and again every [`PIPE_CANCEL_REPEAT_INTERVAL_DURATION`] while it
/// still runs. Ends once the half is dropped.
///
/// `CancelIoEx` runs while the watch is locked, so it never runs after the
/// half's drop has marked the watch.
#[cfg(windows)]
fn run_pipe_step_canceller(
    pipe_handle: PipeHandle,
    shared_step_watch: &(Mutex<PipeStepWatch>, Condvar),
) {
    let (step_watch, step_watch_changed) = shared_step_watch;
    let mut step_watch_guard = step_watch.lock().expect("pipe step watch");
    while !step_watch_guard.is_half_dropped {
        let Some(running_step_deadline) = step_watch_guard.running_step_deadline else {
            step_watch_guard = step_watch_changed
                .wait(step_watch_guard)
                .expect("pipe step watch");
            continue;
        };
        let now = Instant::now();
        let wait_duration = if now < running_step_deadline {
            running_step_deadline - now
        } else {
            // SAFETY: the half that owns the pipe is not dropped while the
            // watch is locked and unmarked, so the handle is open.
            unsafe { CancelIoEx(pipe_handle.0, std::ptr::null()) };
            PIPE_CANCEL_REPEAT_INTERVAL_DURATION
        };
        step_watch_guard = step_watch_changed
            .wait_timeout(step_watch_guard, wait_duration)
            .expect("pipe step watch")
            .0;
    }
}

/// The read direction of one named pipe, shared by the [`Connection`], the
/// reading half it splits into, and every [`ReadCloser`] taken from it.
///
/// Each read on the pipe runs through [`run_pipe_read`](Self::run_pipe_read).
/// [`close_read_direction`](Self::close_read_direction) ends the read that
/// runs at that moment, and every read from then on gives `Ok(0)`, the end of
/// the stream.
///
/// Example: a session's reading thread waits in a read on a client's pipe, and
/// the session's writing thread closes the read direction. The read ends with
/// `Ok(0)`, and the reading thread sees the end of the stream.
#[cfg(windows)]
#[derive(Debug)]
struct PipeReadWatch {
    /// The pipe the reads run on.
    pipe_handle: PipeHandle,
    /// Whether the read direction is closed and whether a read runs, under
    /// one lock.
    read_state: Mutex<PipeReadState>,
    /// Notified each time a read ends.
    read_ended: Condvar,
}

/// What a [`PipeReadWatch`] holds under its lock.
#[cfg(windows)]
#[derive(Debug, Default)]
struct PipeReadState {
    /// Set by [`PipeReadWatch::close_read_direction`]; never cleared.
    is_read_closed: bool,
    /// Set while a read runs on the pipe.
    is_read_running: bool,
}

#[cfg(windows)]
impl PipeReadWatch {
    /// A watch over `pipe_handle`, with the read direction open and no read
    /// running.
    fn from_pipe_handle(pipe_handle: PipeHandle) -> PipeReadWatch {
        PipeReadWatch {
            pipe_handle,
            read_state: Mutex::new(PipeReadState::default()),
            read_ended: Condvar::new(),
        }
    }

    /// Run `run_read`, one read on the pipe. A read direction closed before
    /// the read starts gives `Ok(0)` and runs no read. A read that ends while
    /// the read direction is closed gives `Ok(0)`, whatever it read or failed
    /// with.
    fn run_pipe_read(&self, run_read: impl FnOnce() -> io::Result<usize>) -> io::Result<usize> {
        {
            let mut read_state = self.lock_read_state();
            if read_state.is_read_closed {
                return Ok(0);
            }
            read_state.is_read_running = true;
        }
        let read_outcome = run_read();
        let mut read_state = self.lock_read_state();
        read_state.is_read_running = false;
        self.read_ended.notify_all();
        if read_state.is_read_closed {
            return Ok(0);
        }
        read_outcome
    }

    /// Close the read direction, and return once no read runs on the pipe.
    /// A read that runs is cancelled with `CancelIoEx`, again every
    /// [`PIPE_CANCEL_REPEAT_INTERVAL_DURATION`] until it ends. The cancel also
    /// ends a write waiting on the pipe at that moment.
    fn close_read_direction(&self) {
        let mut read_state = self.lock_read_state();
        read_state.is_read_closed = true;
        while read_state.is_read_running {
            // SAFETY: a read runs, so the reader that holds the pipe is not
            // dropped, and the handle is open.
            unsafe { CancelIoEx(self.pipe_handle.0, std::ptr::null()) };
            read_state = self
                .read_ended
                .wait_timeout(read_state, PIPE_CANCEL_REPEAT_INTERVAL_DURATION)
                .expect("pipe read watch")
                .0;
        }
    }

    /// Lock the watch's state.
    ///
    /// # Panics
    /// When a thread panicked while it held the lock.
    fn lock_read_state(&self) -> MutexGuard<'_, PipeReadState> {
        self.read_state.lock().expect("pipe read watch")
    }
}

/// A whole [`Connection`]'s pipe, read with each read run through the
/// connection's [`PipeReadWatch`].
#[cfg(windows)]
struct WatchedPipeReader<'connection> {
    /// The connection's pipe.
    socket_stream: &'connection mut socket::Stream,
    /// The connection's read direction.
    pipe_read_watch: &'connection PipeReadWatch,
}

#[cfg(windows)]
impl Read for WatchedPipeReader<'_> {
    /// Read from the pipe, as [`PipeReadWatch::run_pipe_read`] states.
    fn read(&mut self, read_buffer: &mut [u8]) -> io::Result<usize> {
        self.pipe_read_watch
            .run_pipe_read(|| self.socket_stream.read(read_buffer))
    }
}

/// The reading half of a stream, with a deadline and an idle timeout it may be
/// given later.
pub trait DeadlinedRead: Read + Deadlined {}
impl<T: Read + Deadlined> DeadlinedRead for T {}

/// The writing half of a stream, with a deadline and an idle timeout it may be
/// given later.
pub trait DeadlinedWrite: Write + Deadlined {}
impl<T: Write + Deadlined> DeadlinedWrite for T {}

/// Wrap a byte-stream pair as the two halves of a framed connection; a
/// stream that is not a local socket then speaks the same frame shape a
/// [`Connection`] does.
///
/// The reader starts open: no [`ReadCloser`] reaches these halves.
///
/// Each half keeps whatever deadline and idle timeout it already carries.
/// [`FrameReader::set_deadline`], [`FrameWriter::set_deadline`],
/// [`FrameReader::set_idle_timeout`] and [`FrameWriter::set_idle_timeout`]
/// reach it through the box.
#[must_use]
pub fn build_frame_halves(
    reader_half: Box<dyn DeadlinedRead>,
    writer_half: Box<dyn DeadlinedWrite>,
) -> (FrameReader, FrameWriter) {
    (
        FrameReader {
            reader_half,
            is_closed: Arc::new(AtomicBool::new(false)),
        },
        FrameWriter {
            writer_half,
            last_frame_written_at: Instant::now(),
            written_byte_count: 0,
        },
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

    /// Give each socket read of this half at most `idle_timeout` to get a
    /// byte, or `None` to take the idle timeout away. A peer that sends
    /// nothing for `idle_timeout` ends the read with a timeout error, however
    /// long the whole frame takes to arrive. The deadline still applies: each
    /// socket read ends by the earlier of the deadline and the time it starts
    /// plus `idle_timeout`. An `idle_timeout` too large to add to the current
    /// time limits nothing.
    ///
    /// Example: with a 30-second idle timeout, a frame whose bytes arrive 10
    /// seconds apart is read whole, and a peer that sends nothing for 30
    /// seconds ends the read.
    pub fn set_idle_timeout(&mut self, idle_timeout: Option<Duration>) {
        self.reader_half.set_idle_timeout(idle_timeout);
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
    /// Read one frame and parse its message as `Message`. Blocks until a
    /// whole frame arrives. The peer closing its writing end, and a read direction
    /// this side closed, are both [`IpcError::Disconnected`].
    pub fn recv<Message: DeserializeOwned>(&mut self) -> Result<Message, IpcError> {
        if self.is_closed.load(Ordering::SeqCst) {
            return Err(IpcError::Disconnected);
        }
        read_message(&mut self.reader_half)
    }

    /// Read one frame and parse it as an [`Answer`] carrying `Response`, as
    /// [`parse_answer`] does. Blocks until a whole frame arrives. The peer
    /// closing its writing end, and a read direction this side closed, are both
    /// [`IpcError::Disconnected`].
    pub fn recv_answer<Response: DeserializeOwned>(
        &mut self,
    ) -> Result<Answer<Response>, IpcError> {
        if self.is_closed.load(Ordering::SeqCst) {
            return Err(IpcError::Disconnected);
        }
        read_answer(&mut self.reader_half)
    }

    /// Read one frame: a JSON frame parsed as `Message`, or a binary frame's
    /// payload. Blocks until a whole frame arrives. The peer closing its
    /// writing end, and a read direction this side closed, are both
    /// [`IpcError::Disconnected`].
    pub fn recv_message_or_binary<Message: DeserializeOwned>(
        &mut self,
    ) -> Result<ReceivedFrame<Message>, IpcError> {
        if self.is_closed.load(Ordering::SeqCst) {
            return Err(IpcError::Disconnected);
        }
        read_message_or_binary(&mut self.reader_half)
    }
}

/// The handle on one connection's read direction, held by a thread other than
/// the one reading that connection. Taken with
/// [`Connection::create_read_closer`].
///
/// The handle keeps working after the connection is split: it closes the read
/// direction of a [`Connection`], and of the [`FrameReader`] or the
/// [`RawReader`] it splits into.
#[derive(Debug)]
pub struct ReadCloser {
    /// Shared with the connection this handle came from.
    is_closed: Arc<AtomicBool>,
    /// The connection's socket, duplicated. Both descriptors name one socket;
    /// shutting this one's read direction shuts the connection's.
    #[cfg(unix)]
    duplicated_socket: UnixStream,
    /// The pipe's read direction, shared with the connection and the reading
    /// half it splits into.
    #[cfg(windows)]
    pipe_read_watch: Arc<PipeReadWatch>,
}

impl ReadCloser {
    /// Close the connection's read direction: every [`Connection::recv`] and
    /// [`FrameReader::recv`] from here reports [`IpcError::Disconnected`], and
    /// every [`RawReader`] read gives `Ok(0)`. A read the reader is already
    /// blocked in ends the same way.
    ///
    /// On Unix the socket's read direction is shut. On Windows the read
    /// waiting on the pipe is cancelled, and the close returns once it has
    /// ended, as `PipeReadWatch::close_read_direction` states. That cancel
    /// also ends a write waiting on the pipe at that moment.
    ///
    /// The writing direction stays open: a write that starts after the close
    /// returns goes out.
    ///
    /// Closing an already-closed read direction changes nothing.
    pub fn close(&self) {
        self.is_closed.store(true, Ordering::SeqCst);
        #[cfg(unix)]
        let _ = self.duplicated_socket.shutdown(Shutdown::Read);
        #[cfg(windows)]
        self.pipe_read_watch.close_read_direction();
    }
}

/// Duplicate the socket a connection reads and writes. Unix only: a Windows
/// named pipe carries no read direction to shut on its own.
#[cfg(unix)]
fn duplicate_unix_socket(socket_stream: &socket::Stream) -> Result<UnixStream, IpcError> {
    let socket::Stream::UdSocket(unix_socket) = socket_stream;
    unix_socket.inner().try_clone().map_err(convert_io_error)
}

/// The writing half of a split [`Connection`]. Reads nothing.
pub struct FrameWriter {
    writer_half: Box<dyn DeadlinedWrite>,
    /// When the last frame this half wrote went out whole, or when this half
    /// was built while none has.
    last_frame_written_at: Instant,
    /// The bytes of every frame this half wrote whole, each frame's 4-byte
    /// length prefix included.
    written_byte_count: u64,
}

impl FrameWriter {
    /// Give this half a deadline, or `None` to take its deadline away. The
    /// same rule [`FrameReader::set_deadline`] states.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.writer_half.set_deadline(deadline);
    }

    /// Give each socket write of this half at most `idle_timeout` for the
    /// socket to take a byte, or `None` to take the idle timeout away. A peer
    /// that takes no byte for `idle_timeout` ends the send with a timeout
    /// error, however long the whole frame takes to go out. The deadline still
    /// applies: each socket write ends by the earlier of the deadline and the
    /// time it starts plus `idle_timeout`. An `idle_timeout` too large to add
    /// to the current time limits nothing.
    ///
    /// Example: with a 30-second idle timeout, a frame whose parts the peer
    /// takes 10 seconds apart goes out whole, and a peer that takes no byte for
    /// 30 seconds ends the send.
    pub fn set_idle_timeout(&mut self, idle_timeout: Option<Duration>) {
        self.writer_half.set_idle_timeout(idle_timeout);
    }
}

impl std::fmt::Debug for FrameWriter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("FrameWriter").finish()
    }
}

impl FrameWriter {
    /// Send one message as one frame. Blocks until the bytes are handed to
    /// the OS, under this half's deadline and its idle timeout
    /// ([`set_idle_timeout`](Self::set_idle_timeout)). A frame that goes out
    /// whole sets [`get_last_frame_written_at`](Self::get_last_frame_written_at)
    /// to the time it finished; a failed send leaves it as it was.
    pub fn send<Message: Serialize>(&mut self, message: &Message) -> Result<(), IpcError> {
        let frame_byte_count = write_message(&mut self.writer_half, message)?;
        self.last_frame_written_at = Instant::now();
        self.written_byte_count += frame_byte_count as u64;
        Ok(())
    }

    /// Send `payload_bytes` as one binary frame, under the same deadline, idle
    /// timeout and [`get_last_frame_written_at`](Self::get_last_frame_written_at)
    /// rules as [`send`](Self::send). A payload past [`MAX_FRAME_BYTE_COUNT`]
    /// is [`IpcError::FrameTooLarge`] with nothing written.
    pub fn send_binary(&mut self, payload_bytes: &[u8]) -> Result<(), IpcError> {
        self.send_binary_with_head(payload_bytes, &[])
    }

    /// Send one binary frame whose payload is `frame_head` then `frame_body`,
    /// under the same rules as [`send_binary`](Self::send_binary). The length
    /// prefix and `frame_head` go out in one write and `frame_body` in a
    /// second, straight from the caller's buffer.
    ///
    /// Example: a head of 18 bytes and a body of 1048576 bytes write 22
    /// bytes, then 1048576 bytes.
    pub fn send_binary_with_head(
        &mut self,
        frame_head: &[u8],
        frame_body: &[u8],
    ) -> Result<(), IpcError> {
        let frame_byte_count = write_binary_frame(&mut self.writer_half, frame_head, frame_body)?;
        self.last_frame_written_at = Instant::now();
        self.written_byte_count += frame_byte_count as u64;
        Ok(())
    }

    /// When the last frame [`send`](Self::send),
    /// [`send_binary`](Self::send_binary) or
    /// [`send_binary_with_head`](Self::send_binary_with_head) wrote went out
    /// whole, or when this half was built while none has.
    #[must_use]
    pub fn get_last_frame_written_at(&self) -> Instant {
        self.last_frame_written_at
    }

    /// The bytes of every frame [`send`](Self::send),
    /// [`send_binary`](Self::send_binary) and
    /// [`send_binary_with_head`](Self::send_binary_with_head) wrote whole,
    /// each frame's 4-byte length prefix included. A failed send adds
    /// nothing.
    ///
    /// Example: `send(&"hi")` then `send_binary(&[1, 2, 3])` gives
    /// `(4 + 4) + (4 + 3)`, 15.
    #[must_use]
    pub fn get_written_byte_count(&self) -> u64 {
        self.written_byte_count
    }
}

/// The reading half of a [`Connection::split_raw`]: the bytes as they arrive,
/// with no frame shape read off them.
#[derive(Debug)]
pub struct RawReader {
    /// The socket's own reading half.
    recv_half: socket::RecvHalf,
    /// The pipe's read direction, shared with the connection's
    /// [`ReadCloser`]s.
    #[cfg(windows)]
    pipe_read_watch: Arc<PipeReadWatch>,
}

impl Read for RawReader {
    /// Read the bytes that arrived. A read direction a [`ReadCloser`] closed
    /// gives `Ok(0)`, the end of the stream. On Windows the read runs through
    /// the pipe's `PipeReadWatch`.
    fn read(&mut self, raw_buffer: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            self.recv_half.read(raw_buffer)
        }
        #[cfg(windows)]
        {
            self.pipe_read_watch
                .run_pipe_read(|| self.recv_half.read(raw_buffer))
        }
    }
}

/// The writing half of a [`Connection::split_raw`]: the bytes go out as
/// given, with no frame shape written around them.
#[derive(Debug)]
pub struct RawWriter(WritableSocketEnd<socket::SendHalf>);

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
    fn write(&mut self, payload_chunk_bytes: &[u8]) -> io::Result<usize> {
        let frame_byte_count = self.frame_bytes.len() - 4 + payload_chunk_bytes.len();
        if frame_byte_count > MAX_FRAME_BYTE_COUNT as usize {
            self.overflow_byte_count = Some(frame_byte_count as u64);
            return Err(io::Error::other("frame over cap"));
        }
        self.frame_bytes.extend_from_slice(payload_chunk_bytes);
        Ok(payload_chunk_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Encode `message` and write it as one frame: 4-byte big-endian length, then
/// the JSON bytes. The whole frame goes out in one `write_all`. Returns the
/// frame's byte count, the length prefix included. A message past
/// [`MAX_FRAME_BYTE_COUNT`] is refused with nothing written, and its encoding
/// stops at the byte that crossed the cap.
pub(crate) fn write_message<Message: Serialize>(
    writer: &mut impl Write,
    message: &Message,
) -> Result<usize, IpcError> {
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
        .map_err(convert_io_error)?;
    Ok(frame_buffer.frame_bytes.len())
}

/// Write one binary frame whose payload is `frame_head` then `frame_body`:
/// the 4-byte big-endian byte count with [`BINARY_FRAME_FLAG`] set and
/// `frame_head` go out in one `write_all`, then `frame_body` in a second
/// `write_all` when it holds a byte. Returns the frame's byte count, the
/// length prefix included. A payload past [`MAX_FRAME_BYTE_COUNT`] is refused
/// with nothing written.
pub(crate) fn write_binary_frame(
    writer: &mut impl Write,
    frame_head: &[u8],
    frame_body: &[u8],
) -> Result<usize, IpcError> {
    let payload_byte_count = frame_head.len() + frame_body.len();
    let prefixed_payload_byte_count = u32::try_from(payload_byte_count)
        .ok()
        .filter(|payload_byte_count| *payload_byte_count <= MAX_FRAME_BYTE_COUNT)
        .ok_or(IpcError::FrameTooLarge {
            frame_byte_count: payload_byte_count as u64,
            maximum_frame_byte_count: MAX_FRAME_BYTE_COUNT,
        })?;
    let mut leading_bytes = Vec::with_capacity(4 + frame_head.len());
    leading_bytes
        .extend_from_slice(&(prefixed_payload_byte_count | BINARY_FRAME_FLAG).to_be_bytes());
    leading_bytes.extend_from_slice(frame_head);
    writer.write_all(&leading_bytes).map_err(convert_io_error)?;
    if !frame_body.is_empty() {
        writer.write_all(frame_body).map_err(convert_io_error)?;
    }
    Ok(4 + payload_byte_count)
}

/// Read one JSON frame and parse its payload as `Message`. The length prefix is
/// checked against [`MAX_FRAME_BYTE_COUNT`] before the payload buffer is allocated.
pub(crate) fn read_message<Message: DeserializeOwned>(
    reader: &mut impl Read,
) -> Result<Message, IpcError> {
    let payload_bytes = read_json_frame_payload(reader)?;
    parse_message(&payload_bytes)
}

/// Read one frame: a JSON frame parsed as `Message`, or a binary frame's
/// payload. The length prefix is checked against [`MAX_FRAME_BYTE_COUNT`]
/// before the payload buffer is allocated.
pub(crate) fn read_message_or_binary<Message: DeserializeOwned>(
    reader: &mut impl Read,
) -> Result<ReceivedFrame<Message>, IpcError> {
    let frame_header = read_frame_header(reader)?;
    let payload_bytes = read_frame_payload(reader, frame_header.payload_byte_count)?;
    if frame_header.is_binary_frame {
        return Ok(ReceivedFrame::Binary(payload_bytes));
    }
    parse_message(&payload_bytes).map(ReceivedFrame::Message)
}

/// Read one JSON frame and parse its payload as [`parse_answer`] does.
pub(crate) fn read_answer<Response: DeserializeOwned>(
    reader: &mut impl Read,
) -> Result<Answer<Response>, IpcError> {
    let payload_bytes = read_json_frame_payload(reader)?;
    parse_answer(&payload_bytes)
}

fn parse_message<Message: DeserializeOwned>(payload_bytes: &[u8]) -> Result<Message, IpcError> {
    serde_json::from_slice(payload_bytes).map_err(|parse_error| IpcError::MalformedFrame {
        error_detail: parse_error.to_string(),
    })
}

/// Parse `answer_bytes` as an [`Answer`] carrying `Response`.
///
/// # Errors
/// [`IpcError::PreviousReleaseAnswer`] for an object whose only fields are
/// `request_id` and `result`, the answer envelope of a server that koshi 0.1.0
/// to 0.4.0 started. [`IpcError::MalformedFrame`] for any other bytes that do
/// not parse.
///
/// Example — `{"request_id":null,"result":{"Error":{"code":"malformed_request",
/// "message":"…"}}}`, a koshi 0.4.0 session's answer to this build's Hello, is
/// [`IpcError::PreviousReleaseAnswer`].
pub fn parse_answer<Response: DeserializeOwned>(
    answer_bytes: &[u8],
) -> Result<Answer<Response>, IpcError> {
    serde_json::from_slice(answer_bytes).map_err(|parse_error| {
        if has_exactly_json_fields(answer_bytes, &["request_id", "result"]) {
            IpcError::PreviousReleaseAnswer
        } else {
            IpcError::MalformedFrame {
                error_detail: parse_error.to_string(),
            }
        }
    })
}

/// What one frame's length prefix says.
struct FrameHeader {
    /// Whether bit 31 marks the frame binary.
    is_binary_frame: bool,
    /// The payload's byte count, bits 0 to 30, at most [`MAX_FRAME_BYTE_COUNT`].
    payload_byte_count: u32,
}

/// Read one frame's 4-byte length prefix. A byte count past
/// [`MAX_FRAME_BYTE_COUNT`] is [`IpcError::FrameTooLarge`], with the payload
/// left unread.
fn read_frame_header(reader: &mut impl Read) -> Result<FrameHeader, IpcError> {
    let mut length_prefix_bytes = [0u8; 4];
    reader
        .read_exact(&mut length_prefix_bytes)
        .map_err(convert_io_error)?;
    let length_prefix = u32::from_be_bytes(length_prefix_bytes);
    let payload_byte_count = length_prefix & !BINARY_FRAME_FLAG;
    if payload_byte_count > MAX_FRAME_BYTE_COUNT {
        return Err(IpcError::FrameTooLarge {
            frame_byte_count: u64::from(payload_byte_count),
            maximum_frame_byte_count: MAX_FRAME_BYTE_COUNT,
        });
    }
    Ok(FrameHeader {
        is_binary_frame: length_prefix & BINARY_FRAME_FLAG != 0,
        payload_byte_count,
    })
}

/// Read the `payload_byte_count` bytes of one frame's payload.
fn read_frame_payload(
    reader: &mut impl Read,
    payload_byte_count: u32,
) -> Result<Vec<u8>, IpcError> {
    let mut payload_bytes = vec![0u8; payload_byte_count as usize];
    reader
        .read_exact(&mut payload_bytes)
        .map_err(convert_io_error)?;
    Ok(payload_bytes)
}

/// Read one JSON frame's payload. A binary frame is
/// [`IpcError::UnexpectedBinaryFrame`], with its payload left unread.
fn read_json_frame_payload(reader: &mut impl Read) -> Result<Vec<u8>, IpcError> {
    let frame_header = read_frame_header(reader)?;
    if frame_header.is_binary_frame {
        return Err(IpcError::UnexpectedBinaryFrame);
    }
    read_frame_payload(reader, frame_header.payload_byte_count)
}

/// Read one part of `process_handle`'s token: the bytes `GetTokenInformation`
/// writes for `token_information_class`. For `TokenUser` they start with a
/// [`TOKEN_USER`], and for `TokenOwner` with a [`TOKEN_OWNER`], whose SID
/// pointer points into the rest of the same buffer. The buffer is `u64`: both
/// structures need 8-byte alignment.
#[cfg(windows)]
fn read_token_information_buffer(
    process_handle: HANDLE,
    token_information_class: TOKEN_INFORMATION_CLASS,
) -> Result<Vec<u64>, IpcError> {
    let mut token_handle: HANDLE = std::ptr::null_mut();
    // SAFETY: `process_handle` is open, and `token_handle` is a writable
    // `HANDLE`.
    if unsafe { OpenProcessToken(process_handle, TOKEN_QUERY, &mut token_handle) } == 0 {
        return Err(convert_io_error(io::Error::last_os_error()));
    }
    // SAFETY: `OpenProcessToken` succeeded, so `token_handle` is open and
    // nothing else owns it.
    let token_handle = unsafe { OwnedHandle::from_raw_handle(token_handle) };
    let mut required_buffer_byte_count: u32 = 0;
    // SAFETY: a NULL buffer of length `0` makes the call write only the byte
    // count it needs, to `required_buffer_byte_count`.
    unsafe {
        GetTokenInformation(
            token_handle.as_raw_handle(),
            token_information_class,
            std::ptr::null_mut(),
            0,
            &mut required_buffer_byte_count,
        );
    }
    let mut token_buffer = vec![0u64; (required_buffer_byte_count as usize).div_ceil(8).max(1)];
    // SAFETY: `token_buffer` is writable for the byte length passed.
    let is_token_information_filled = unsafe {
        GetTokenInformation(
            token_handle.as_raw_handle(),
            token_information_class,
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

/// The user SID inside a buffer [`read_token_information_buffer`] filled for
/// `TokenUser`. It stays valid only while that buffer lives.
///
/// # Safety
/// `token_buffer` is a buffer that [`read_token_information_buffer`] filled
/// for `TokenUser`.
#[cfg(windows)]
unsafe fn get_user_sid_from_token_buffer(token_buffer: &[u64]) -> PSID {
    // SAFETY: the caller passes a buffer that starts with a `TOKEN_USER`,
    // aligned to 8 bytes.
    unsafe { (*token_buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid }
}

/// The owner SID inside a buffer [`read_token_information_buffer`] filled for
/// `TokenOwner`: the owner the token gives the objects it creates. It stays
/// valid only while that buffer lives.
///
/// # Safety
/// `token_buffer` is a buffer that [`read_token_information_buffer`] filled
/// for `TokenOwner`.
#[cfg(windows)]
unsafe fn get_owner_sid_from_token_buffer(token_buffer: &[u64]) -> PSID {
    // SAFETY: the caller passes a buffer that starts with a `TOKEN_OWNER`,
    // aligned to 8 bytes.
    unsafe { (*token_buffer.as_ptr().cast::<TOKEN_OWNER>()).Owner }
}

/// Build the security descriptor of a pipe this process creates, from
/// Windows' [security descriptor string format][sddl]: this process's user
/// owns the pipe (`O:`) and has every right on it (`FA`), and no one else has
/// any access.
///
/// Example: for the user `S-1-5-21-1-2-3-1001` the text is
/// `O:S-1-5-21-1-2-3-1001D:(A;;FA;;;S-1-5-21-1-2-3-1001)`.
///
/// # Errors
/// The failure of reading this process's user, or of parsing the text.
///
/// [sddl]:
/// https://learn.microsoft.com/en-us/windows/win32/secauthz/security-descriptor-string-format
#[cfg(windows)]
fn build_pipe_access() -> Result<SecurityDescriptor, IpcError> {
    let own_user_sid = format_own_user_sid()?;
    let access_text = U16CString::from_str(format!("O:{own_user_sid}D:(A;;FA;;;{own_user_sid})"))
        .expect("a SID holds no NUL character");
    SecurityDescriptor::deserialize(&access_text).map_err(convert_io_error)
}

/// This process's user SID as text, such as `S-1-5-21-1-2-3-1001`.
///
/// # Errors
/// The failure of reading this process's token, or of converting its user SID.
#[cfg(windows)]
fn format_own_user_sid() -> Result<String, IpcError> {
    let own_token_buffer =
        // SAFETY: `GetCurrentProcess` takes no arguments and gives a pseudo
        // handle that needs no closing.
        read_token_information_buffer(unsafe { GetCurrentProcess() }, TokenUser)?;
    let mut sid_text_pointer: *mut u16 = std::ptr::null_mut();
    // SAFETY: `own_token_buffer` was filled for `TokenUser`, and
    // `sid_text_pointer` is a writable pointer.
    let is_converted = unsafe {
        ConvertSidToStringSidW(
            get_user_sid_from_token_buffer(&own_token_buffer),
            &mut sid_text_pointer,
        )
    };
    if is_converted == 0 {
        return Err(convert_io_error(io::Error::last_os_error()));
    }
    let _sid_text_allocation = LocalAllocation(sid_text_pointer.cast());
    // SAFETY: `ConvertSidToStringSidW` succeeded, so `sid_text_pointer` is a
    // NUL-terminated string that `_sid_text_allocation` keeps alive.
    Ok(unsafe { U16CStr::from_ptr_str(sid_text_pointer) }.to_string_lossy())
}

/// Report whether the pipe that `pipe_handle` is an end of is owned by this
/// process's user, or by the owner this process's token gives the objects it
/// creates: the Administrators group for an elevated administrator. The
/// handle needs `READ_CONTROL`, which [`PIPE_CALLER_ACCESS`] holds.
///
/// # Errors
/// The failure `GetSecurityInfo` reports for the pipe's owner, or the failure
/// of reading this process's token.
#[cfg(windows)]
fn is_pipe_owned_by_own_user(pipe_handle: &OwnedHandle) -> Result<bool, IpcError> {
    let mut owner_sid: PSID = std::ptr::null_mut();
    let mut pipe_security_descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `pipe_handle` is open, and each out pointer is a writable local
    // or NULL.
    let lookup_status = unsafe {
        GetSecurityInfo(
            pipe_handle.as_raw_handle(),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner_sid,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut pipe_security_descriptor,
        )
    };
    if lookup_status != ERROR_SUCCESS {
        return Err(convert_io_error(io::Error::from_raw_os_error(
            lookup_status as i32,
        )));
    }
    let _pipe_security_descriptor_allocation = LocalAllocation(pipe_security_descriptor);
    // SAFETY: `GetCurrentProcess` takes no arguments and gives a pseudo handle
    // that needs no closing.
    let own_process_handle = unsafe { GetCurrentProcess() };
    let user_token_buffer = read_token_information_buffer(own_process_handle, TokenUser)?;
    // SAFETY: `owner_sid` points into the descriptor that
    // `_pipe_security_descriptor_allocation` keeps alive, and
    // `user_token_buffer` was filled for `TokenUser`.
    if unsafe {
        EqualSid(
            owner_sid,
            get_user_sid_from_token_buffer(&user_token_buffer),
        )
    } != 0
    {
        return Ok(true);
    }
    let owner_token_buffer = read_token_information_buffer(own_process_handle, TokenOwner)?;
    // SAFETY: `owner_sid` points into the descriptor that
    // `_pipe_security_descriptor_allocation` keeps alive, and
    // `owner_token_buffer` was filled for `TokenOwner`.
    let is_default_owner = unsafe {
        EqualSid(
            owner_sid,
            get_owner_sid_from_token_buffer(&owner_token_buffer),
        )
    };
    Ok(is_default_owner != 0)
}

/// Memory a Windows function allocated for its caller. Dropping it frees the
/// memory with `LocalFree`.
#[cfg(windows)]
struct LocalAllocation(*mut std::ffi::c_void);

#[cfg(windows)]
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: `self.0` is memory a Windows function allocated for this
        // caller to free with `LocalFree`, and this drop frees it once.
        unsafe { LocalFree(self.0) };
    }
}

/// Accept connections on `listener` until `is_shutting_down` is set, handing
/// each accepted connection to `serve_connection`. A failed accept goes to
/// `report_accept_error`, then sleeps `retry_delay`, and the loop continues.
/// The flag is read between the accept and the dispatch: the connection
/// accepted after the flag is set is dropped, not served.
pub fn accept_until_shutdown(
    listener: &Listener,
    is_shutting_down: &AtomicBool,
    retry_delay: std::time::Duration,
    mut report_accept_error: impl FnMut(&IpcError),
    mut serve_connection: impl FnMut(Connection),
) {
    loop {
        let connection = listener.accept();
        if is_shutting_down.load(Ordering::SeqCst) {
            break;
        }
        match connection {
            Ok(connection) => serve_connection(connection),
            Err(accept_error) => {
                report_accept_error(&accept_error);
                std::thread::sleep(retry_delay);
            }
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

/// Classify a connect failure at `socket_address`: the failures
/// [`is_no_listener_error`] names become [`IpcError::NoListener`], and every
/// other one goes through [`convert_io_error`].
fn convert_connect_error(socket_address: &str, connect_error: io::Error) -> IpcError {
    if is_no_listener_error(&connect_error) {
        return IpcError::NoListener {
            socket_address: socket_address.to_string(),
        };
    }
    convert_io_error(connect_error)
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
/// The peer-gone kinds are [`UnexpectedEof`](io::ErrorKind::UnexpectedEof),
/// [`BrokenPipe`](io::ErrorKind::BrokenPipe),
/// [`ConnectionReset`](io::ErrorKind::ConnectionReset),
/// [`ConnectionAborted`](io::ErrorKind::ConnectionAborted) and
/// [`NotConnected`](io::ErrorKind::NotConnected) (`ENOTCONN`, `Socket is not
/// connected`).
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
