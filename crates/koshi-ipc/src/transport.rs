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
//! [`Connection::create_read_closer`](crate::transport::Connection::create_read_closer)
//! hands out a [`ReadCloser`](crate::transport::ReadCloser): the handle another
//! thread holds to end the reading side of a connection while the thread
//! serving it is blocked reading. The writing side is left alone.
//!
//! [`FrameReader::set_deadline`](crate::transport::FrameReader::set_deadline) and
//! [`FrameWriter::set_deadline`](crate::transport::FrameWriter::set_deadline)
//! end each read and write of a split connection by the moment they name.
//!
//! The frame shape is not tied to the local socket.
//! [`build_frame_halves`](crate::transport::build_frame_halves) puts it on any other pair
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
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(windows)]
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::{Listener as _, Stream as _, StreamCommon as _};
#[cfg(unix)]
use interprocess::local_socket::traits::{RecvHalf as _, SendHalf as _};
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
#[cfg(windows)]
use windows_sys::Win32::System::IO::CancelIoEx;

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
/// [sddl]:
/// https://learn.microsoft.com/en-us/windows/win32/secauthz/security-descriptor-string-format
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
    /// Example: a Windows pipe whose one instance holds a caller the listener
    /// has not accepted gives a second caller [`IpcError::Transport`] after
    /// `connect_wait_duration`.
    pub fn connect_within(
        socket_address: &str,
        connect_wait_duration: Duration,
    ) -> Result<Connection, IpcError> {
        let platform_socket_name = resolve_socket_name(socket_address).map_err(convert_io_error)?;
        let socket_stream = ConnectOptions::new()
            .name(platform_socket_name)
            .wait_mode(ConnectWaitMode::Timeout(connect_wait_duration))
            .connect_sync()
            .map_err(|connect_error| {
                if is_no_listener_error(&connect_error) {
                    IpcError::NoListener {
                        socket_address: socket_address.to_string(),
                    }
                } else {
                    convert_io_error(connect_error)
                }
            })?;
        Ok(Connection::from_socket_stream(socket_stream))
    }

    /// Send one message as one frame. Blocks until the bytes are handed to
    /// the OS.
    pub fn send<Message: Serialize>(&mut self, message: &Message) -> Result<(), IpcError> {
        write_message(&mut self.socket_stream, message)
    }

    /// Read one frame and decode its message as `Message`. Blocks until a
    /// whole frame arrives. A connection whose read direction is closed reports
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
    pub fn create_read_closer(&self) -> Result<ReadCloser, IpcError> {
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
    ///
    /// Neither half starts with a deadline.
    /// [`FrameReader::set_deadline`] and [`FrameWriter::set_deadline`] give
    /// one.
    #[must_use]
    pub fn split(self) -> (FrameReader, FrameWriter) {
        let (reader_half, writer_half) = self.socket_stream.split();
        (
            FrameReader {
                reader_half: Box::new(LocalSocketReader::from_recv_half(reader_half)),
                is_closed: self.is_read_closed,
            },
            FrameWriter {
                writer_half: Box::new(LocalSocketWriter::from_send_half(writer_half)),
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
pub trait Deadlined: Send {
    /// Every read and write after this finishes by `deadline`, or blocks for as long
    /// as it takes when `deadline` is `None`.
    fn set_deadline(&mut self, deadline: Option<Instant>);
}

/// The least time left a step under a deadline starts with: 1 ms. Less time
/// left than this counts as no time left.
const MINIMUM_STEP_TIME_LEFT_DURATION: Duration = Duration::from_millis(1);

/// The time left until `deadline`. A read or write under `deadline` runs out
/// of time once this fails.
///
/// # Errors
/// [`io::ErrorKind::TimedOut`] with the text `this step ran out of time` when
/// less than 1 ms is left.
pub fn compute_time_left_until(deadline: Instant) -> io::Result<Duration> {
    let time_left = deadline.saturating_duration_since(Instant::now());
    if time_left < MINIMUM_STEP_TIME_LEFT_DURATION {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "this step ran out of time",
        ));
    }
    Ok(time_left)
}

/// The reading half of a split local socket [`Connection`]. A read under a
/// deadline ends by that deadline; a read with no deadline blocks for as long
/// as it takes.
///
/// A read that runs out of time is [`io::ErrorKind::TimedOut`] with the text
/// `this step ran out of time`.
struct LocalSocketReader {
    /// Dropped before `recv_half`: its thread stops before the pipe closes.
    #[cfg(windows)]
    pipe_step_canceller: PipeStepCanceller,
    /// The socket's own reading half.
    recv_half: socket::RecvHalf,
    /// When every read must be finished by, or `None` for no limit.
    deadline: Option<Instant>,
}

impl LocalSocketReader {
    /// A reader over `recv_half` with no deadline.
    fn from_recv_half(recv_half: socket::RecvHalf) -> LocalSocketReader {
        #[cfg(windows)]
        let socket::RecvHalf::NamedPipe(pipe_half) = &recv_half;
        LocalSocketReader {
            #[cfg(windows)]
            pipe_step_canceller: PipeStepCanceller::from_pipe_handle(PipeHandle(
                pipe_half.as_handle().as_raw_handle(),
            )),
            recv_half,
            deadline: None,
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
}

impl Read for LocalSocketReader {
    /// Read from the socket. Under a deadline the read ends by it.
    ///
    /// On Unix each attempt first sets the socket's receive timeout to the
    /// time left; a timed-out attempt is tried again with the new time left
    /// until less than 1 ms is left. On Windows the read runs while
    /// `PipeStepCanceller` watches the deadline.
    ///
    /// # Errors
    /// [`io::ErrorKind::TimedOut`] with the text `this step ran out of time`
    /// once the deadline is reached; otherwise the read's own failure.
    fn read(&mut self, read_buffer: &mut [u8]) -> io::Result<usize> {
        let Some(deadline) = self.deadline else {
            return self.recv_half.read(read_buffer);
        };
        let recv_half = &self.recv_half;
        #[cfg(unix)]
        {
            run_socket_step_until(
                deadline,
                |time_left| recv_half.set_timeout(Some(time_left)),
                || {
                    let mut recv_half_reference = recv_half;
                    recv_half_reference.read(read_buffer)
                },
            )
        }
        #[cfg(windows)]
        {
            self.pipe_step_canceller.run_pipe_step_until(deadline, || {
                let mut recv_half_reference = recv_half;
                recv_half_reference.read(read_buffer)
            })
        }
    }
}

/// The writing half of a split local socket [`Connection`]. A write under a
/// deadline ends by that deadline; a write with no deadline blocks for as long
/// as it takes.
///
/// A write that runs out of time is [`io::ErrorKind::TimedOut`] with the text
/// `this step ran out of time`.
struct LocalSocketWriter {
    /// Dropped before `send_half`: its thread stops before the pipe closes.
    #[cfg(windows)]
    pipe_step_canceller: PipeStepCanceller,
    /// The socket's own writing half.
    send_half: socket::SendHalf,
    /// When every write must be finished by, or `None` for no limit.
    deadline: Option<Instant>,
}

impl LocalSocketWriter {
    /// A writer over `send_half` with no deadline.
    fn from_send_half(send_half: socket::SendHalf) -> LocalSocketWriter {
        #[cfg(windows)]
        let socket::SendHalf::NamedPipe(pipe_half) = &send_half;
        LocalSocketWriter {
            #[cfg(windows)]
            pipe_step_canceller: PipeStepCanceller::from_pipe_handle(PipeHandle(
                pipe_half.as_handle().as_raw_handle(),
            )),
            send_half,
            deadline: None,
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
}

impl Write for LocalSocketWriter {
    /// Write to the socket. Under a deadline the write ends by it, the same
    /// way [`LocalSocketReader`]'s read does.
    ///
    /// # Errors
    /// [`io::ErrorKind::TimedOut`] with the text `this step ran out of time`
    /// once the deadline is reached; otherwise the write's own failure.
    fn write(&mut self, write_bytes: &[u8]) -> io::Result<usize> {
        let Some(deadline) = self.deadline else {
            return self.send_half.write(write_bytes);
        };
        let send_half = &self.send_half;
        #[cfg(unix)]
        {
            run_socket_step_until(
                deadline,
                |time_left| send_half.set_timeout(Some(time_left)),
                || {
                    let mut send_half_reference = send_half;
                    send_half_reference.write(write_bytes)
                },
            )
        }
        #[cfg(windows)]
        {
            self.pipe_step_canceller.run_pipe_step_until(deadline, || {
                let mut send_half_reference = send_half;
                send_half_reference.write(write_bytes)
            })
        }
    }

    /// Flush the socket's writing half.
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
        let time_left = compute_time_left_until(deadline)?;
        let _ = set_socket_timeout(time_left);
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

/// A named pipe's handle, carried to the thread of a [`PipeStepCanceller`].
#[cfg(windows)]
#[derive(Clone, Copy)]
struct PipeHandle(HANDLE);

// SAFETY: a pipe handle names a kernel object; any thread of the process may
// pass it to `CancelIoEx`.
#[cfg(windows)]
unsafe impl Send for PipeHandle {}

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
        compute_time_left_until(deadline)?;
        self.start_canceller_thread()?;
        self.set_running_step_deadline(Some(deadline));
        let step_outcome = run_step();
        self.set_running_step_deadline(None);
        if step_outcome.is_err() {
            compute_time_left_until(deadline)?;
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
pub fn build_frame_halves(
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
    /// Read one frame and decode its message as `Message`. Blocks until a
    /// whole frame arrives. The peer closing its writing end, and a read direction
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
/// [`Connection::create_read_closer`].
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
    let socket::Stream::UdSocket(unix_socket) = socket_stream;
    unix_socket.inner().try_clone().map_err(convert_io_error)
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

/// Read one frame and decode its JSON payload as `Message`. The length prefix is
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

/// Read the user of `process_handle`'s token: the bytes `GetTokenInformation`
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
