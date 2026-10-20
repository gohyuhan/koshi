//! Real OS-PTY backend: `portable-pty` on Unix, and on Windows a pseudoconsole
//! the backend opens itself (`crate::pseudoconsole`).
//!
//! A spawned pane gets a kernel PTY and three helper threads (reader, writer,
//! watcher), all owned through the [`crate::portable::PortablePtyBackend`]
//! pane map. The backend streams child output, queues input, terminates the
//! child under cross-platform kill policies, and tracks the exit status.
//!
//! A pane runs these three threads and no others: the reader delivers output to
//! the consumer itself, through [`crate::backend::state::PtySink`].
//!
//! One thread publishes a pane's exit. Which one it is follows from how the
//! pane's terminal reaches the end of its output:
//!
//! - A Unix terminal the pane owns a descriptor for: the reader publishes. The
//!   watcher wakes it once the child has ended, and it hands over whatever the
//!   terminal still holds before publishing.
//! - Windows: the reader publishes, and the watcher stands by behind it. Once
//!   the child has ended the watcher waits for the reader to be back on the
//!   terminal, then closes that terminal on a thread of its own. The close
//!   flushes the console's remaining output and ends the reader's pipe, and
//!   returns only once every process attached to that console has let it go.
//!   The reader reads that console to its end in every ending: on a pane the
//!   consumer has let go it drops the consumer first and discards the rest. The
//!   watcher publishes the exit itself once its deadline passes with the reader
//!   still short of the end.
//! - A Unix terminal that exposes no descriptor to wait on: the reader blocks
//!   in `read`, and the watcher stands by on a deadline and publishes.
//!
//! # A Windows pane's pseudoconsole
//!
//! Every Windows pane's terminal is a pseudoconsole that
//! `pseudoconsole::spawn_pane_program` opens with `CreatePseudoConsole`. It
//! passes `PSEUDOCONSOLE_RESIZE_QUIRK` and `PSEUDOCONSOLE_WIN32_INPUT_MODE`,
//! which are outside the documented set (`0` and
//! `PSEUDOCONSOLE_INHERIT_CURSOR`). It closes the two pipe handles it handed
//! to the call once `CreateProcessW` has started the pane program.
//!
//! `PSEUDOCONSOLE_INHERIT_CURSOR` is passed on every pane. A pseudoconsole
//! created with it writes a cursor-position request to its output and holds
//! its child's output until the process that created it replies on the
//! pseudoconsole's input. The reply is queued as the pane opens, ahead of
//! anything a user can type into it, and the pane's reader takes the request
//! itself out of the output, in `RemovesCursorRequest`.
//!
//! `tests/portable_windows.rs` opens a pane, writes to it, reads its output
//! back and closes it.

use std::{
    collections::HashMap,
    io::{ErrorKind, Read, Write},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{channel, Receiver, RecvTimeoutError, Sender},
        Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::collections::BTreeSet;

use koshi_core::{
    constant::GRACEFUL_TIMEOUT_DURATION,
    ids::PaneId,
    process::{ExitStatus, KillPolicy, PtySize, SpawnSpec},
};
#[cfg(unix)]
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty};

#[cfg(unix)]
use crate::env::build_environment_overlay;
use crate::{
    backend::state::{
        CarriedPtyPane, PtyBackend, PtySink, MAX_QUEUED_PANE_INPUT_BYTE_COUNT, UNOBSERVED_EXIT,
    },
    error::PtyError,
    kill::{PtyChildKillControl, StopRequest},
};
#[cfg(windows)]
use crate::{
    kill::PaneJob,
    pseudoconsole::{spawn_pane_program, PaneConsole, PaneProcess, SpawnedPaneProgram},
};

/// Bytes read from a pane's master end in one go.
const READ_CHUNK_BYTE_COUNT: usize = 8192;

/// One round of the reader's wait on a Unix terminal whose child has gone, and
/// one check-in of the watcher's standby.
///
/// A round in which the terminal produces nothing ends the reader's wait. The
/// standby checks in at this interval on both paths it runs: a Unix terminal
/// that exposes no descriptor to wait on, and a Windows pane whose terminal is
/// being closed.
const EXIT_PUBLISH_GRACE_DURATION: Duration = Duration::from_millis(100);

/// The longest a pane's exit is held back after its child has gone.
///
/// Bounds both waits. The reader's rounds stop here while a descendant holds
/// the terminal open and keeps printing. The standby publishes the exit itself
/// once this passes.
const EXIT_PUBLISH_LIMIT_DURATION: Duration = Duration::from_secs(1);

/// How often a Windows pane's watcher looks at whether the reader is back
/// reading the terminal, before it closes that terminal.
#[cfg(windows)]
const READER_CHECK_IN_DURATION: Duration = Duration::from_millis(100);

/// The longest [`PortablePtyBackend::flush_writers`] waits for every pane's
/// writer thread to reach the end of what it was handed.
///
/// Bounds the whole flush, not one pane. A writer blocked inside its write —
/// the child stopped reading its stdin — never reaches the end; the wait stops
/// here and the error names that pane.
const WRITER_FLUSH_LIMIT_DURATION: Duration = Duration::from_secs(1);

/// Start one of a pane's helper threads under `thread_name`.
///
/// `thread_name` is what a debugger, profiler, or crash report shows for the thread.
///
/// # Panics
/// Panics if the thread cannot be spawned, as [`std::thread::spawn`] does.
fn spawn_pty_thread<ThreadBody>(thread_name: &str, thread_body: ThreadBody) -> JoinHandle<()>
where
    ThreadBody: FnOnce() + Send + 'static,
{
    thread::Builder::new()
        .name(thread_name.to_string())
        .spawn(thread_body)
        .expect("spawn pty helper thread")
}

/// Where one pane's reader thread puts the child's output, and how it reports
/// the child's end: each chunk goes to `pty_sink` directly, and
/// `exit_receiver` is the watcher's end of a private channel, read once the
/// output is exhausted.
struct Delivery {
    /// The consumer taking this pane's output and exit.
    pty_sink: Arc<dyn PtySink>,
    /// The watcher's exit status, awaited after the last chunk.
    exit_receiver: Receiver<ExitStatus>,
    /// This pane's hand-over: whether its exit is settled, and how much of
    /// the PTY the reader has passed on.
    ///
    /// One lock holds both facts: a move between them is one step — a
    /// chunk is claimed or the pane is settled, never half of both. The
    /// reader stops on it; the watcher reads it to see whether the reader
    /// has published the exit yet.
    exit_handover_state: Arc<Mutex<ExitHandover>>,
}

/// What a pane's reader has handed over, and whether its exit is settled.
#[derive(Debug, Default)]
struct ExitHandover {
    /// Chunks the reader has begun handing over, counted before the hand-over.
    started_chunk_count: u64,
    /// Chunks it has finished handing over, counted after. Below
    /// `started_chunk_count` exactly while a chunk is in the consumer's hands.
    finished_chunk_count: u64,
    /// Whether this pane's exit is settled — delivered, or decided against.
    /// A settled pane takes no more output and is told no exit.
    is_settled: bool,
}

impl ExitHandover {
    /// Whether a chunk is in the consumer's hands right now, which means the
    /// pane's reader is not reading its terminal. Read by the watcher on the
    /// standby of a Unix terminal with no descriptor, and on the terminal
    /// close of a Windows pane.
    fn has_chunk_in_flight(&self) -> bool {
        self.started_chunk_count != self.finished_chunk_count
    }
}

/// A descriptor for `master`'s terminal that the caller owns.
///
/// The copy stays valid for as long as the caller keeps it: a pane torn down
/// while a helper thread still runs closes the pane's copy, never this one.
///
/// One copy serves the whole pane: its reader waits on it and reads it, its
/// writer writes it, and [`resize_pane`](PortablePtyBackend::resize_pane) retunes it.
/// Both directions of the terminal travel through the one descriptor.
///
/// `None` when `master` exposes no descriptor, or when the descriptor cannot
/// be duplicated.
#[cfg(unix)]
fn duplicate_terminal_file_descriptor(pty_master: &dyn MasterPty) -> Option<std::os::fd::OwnedFd> {
    pty_master.as_raw_fd().and_then(|master_file_descriptor| {
        // SAFETY: `pty_master` owns the descriptor and stays borrowed until
        // `try_clone_to_owned` returns.
        unsafe { std::os::fd::BorrowedFd::borrow_raw(master_file_descriptor) }
            .try_clone_to_owned()
            .ok()
    })
}

/// Read from a pane's terminal, reporting a closed slave as end of input.
///
/// Once the last process holding the slave open lets it go, Linux answers a
/// read on the master with `EIO`. `EIO` is reported as `Ok(0)`, the same as a
/// zero-length read; every caller here treats `Ok(0)` as the end.
#[cfg(unix)]
fn read_terminal(
    terminal_fd: &std::os::fd::OwnedFd,
    read_buffer: &mut [u8],
) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd;

    // SAFETY: `terminal_fd` is an open descriptor, and `read_buffer` is
    // writable for the `read_buffer.len()` bytes passed.
    let read_byte_count = unsafe {
        libc::read(
            terminal_fd.as_raw_fd(),
            read_buffer.as_mut_ptr().cast::<libc::c_void>(),
            read_buffer.len(),
        )
    };
    if read_byte_count >= 0 {
        return Ok(read_byte_count as usize);
    }
    let io_error = std::io::Error::last_os_error();
    match io_error.raw_os_error() {
        Some(libc::EIO) => Ok(0),
        _ => Err(io_error),
    }
}

/// Write `input_bytes` to a pane's terminal; they reach its child as typed input.
///
/// Loops until every byte is written: a partial write continues with the
/// tail. An interrupted write (`EINTR`) is retried. Any other error is
/// returned with the tail unwritten.
#[cfg(unix)]
fn write_terminal(terminal_fd: &std::os::fd::OwnedFd, input_bytes: &[u8]) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let mut written_byte_count = 0;
    while written_byte_count < input_bytes.len() {
        // SAFETY: `terminal_fd` is an open descriptor, and the tail of
        // `input_bytes` is readable for the byte count passed.
        let write_byte_count = unsafe {
            libc::write(
                terminal_fd.as_raw_fd(),
                input_bytes[written_byte_count..]
                    .as_ptr()
                    .cast::<libc::c_void>(),
                input_bytes.len() - written_byte_count,
            )
        };
        if write_byte_count >= 0 {
            written_byte_count += write_byte_count as usize;
            continue;
        }
        let io_error = std::io::Error::last_os_error();
        if io_error.kind() == ErrorKind::Interrupted {
            continue;
        }
        return Err(io_error);
    }
    Ok(())
}

/// Tell a pane's child its terminal is now `pty_size`. The kernel turns this into
/// the `SIGWINCH` a full-screen program redraws on.
#[cfg(unix)]
fn resize_terminal(terminal_fd: &std::os::fd::OwnedFd, pty_size: PtySize) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    // The pixel dimensions are sent as zero: this crate does not track them.
    let window_size = libc::winsize {
        ws_row: pty_size.row_count,
        ws_col: pty_size.column_count,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `TIOCSWINSZ` reads one `winsize` through the pointer, and
    // `window_size` lives for the call.
    let resize_ioctl_result = unsafe {
        libc::ioctl(
            terminal_fd.as_raw_fd(),
            libc::TIOCSWINSZ as _,
            std::ptr::addr_of!(window_size),
        )
    };
    if resize_ioctl_result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Set or clear the close-on-exec flag on a pane's terminal descriptor.
///
/// `should_close_on_exec = false` keeps the descriptor open across this process
/// replacing its own image; `should_close_on_exec = true` closes it in every
/// child spawned afterwards. Every other descriptor flag is left as it was.
///
/// # Errors
/// Returns the OS error if the descriptor's flags cannot be read or written.
#[cfg(unix)]
pub fn set_terminal_cloexec(
    terminal_file_descriptor: std::os::fd::RawFd,
    should_close_on_exec: bool,
) -> std::io::Result<()> {
    // SAFETY: `F_GETFD` reads no memory, and a descriptor that is not open
    // gives `-1`.
    let descriptor_flags = unsafe { libc::fcntl(terminal_file_descriptor, libc::F_GETFD) };
    if descriptor_flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let desired_descriptor_flags = if should_close_on_exec {
        descriptor_flags | libc::FD_CLOEXEC
    } else {
        descriptor_flags & !libc::FD_CLOEXEC
    };
    // SAFETY: `F_SETFD` takes the flags by value and reads no memory.
    if unsafe {
        libc::fcntl(
            terminal_file_descriptor,
            libc::F_SETFD,
            desired_descriptor_flags,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// Apple's own `ptsname_r`, declared here. `libc` 0.2.189 declares this call
// for the Linux-like targets, FreeBSD, NetBSD, Fuchsia, Hurd, Cygwin, QNX and
// illumos, and for no Apple target. macOS ships it in libSystem from 10.13.4
// onward.
#[cfg(all(unix, target_vendor = "apple"))]
extern "C" {
    fn ptsname_r(
        terminal_file_descriptor: libc::c_int,
        terminal_name_buffer: *mut libc::c_char,
        terminal_name_buffer_length: libc::size_t,
    ) -> libc::c_int;
}

#[cfg(all(unix, not(target_vendor = "apple")))]
use libc::ptsname_r;

/// The name of the terminal paired with `terminal_file_descriptor`, and `None`
/// when it names no pseudoterminal master.
///
/// `ptsname_r` answers for a master only: the slave end of that same pair, an
/// ordinary file, a pipe, a socket, and a number naming nothing open all fail
/// it. Only the return value decides that. A failure reports itself
/// differently per system: macOS returns `-1` and sets `errno`, glibc and musl
/// return the error number itself. `0` means success on all of them.
///
/// The name is the master's own identity: two masters this process holds at
/// once are paired with two different terminals. A caller that recorded the
/// name of one descriptor can tell that same master from another one.
///
/// Before → after: `terminal_file_descriptor` holds the master of
/// `/dev/ttys009` → `Some("/dev/ttys009")`. It holds that pair's slave end, an
/// open log file, or a number this process never opened → `None`.
#[cfg(unix)]
#[must_use]
pub fn find_terminal_master_name(terminal_file_descriptor: std::os::fd::RawFd) -> Option<String> {
    // 128 bytes holds every terminal name these systems report: `/dev/pts/0`
    // through `/dev/pts/1048575` on Linux, `/dev/ttys009` on macOS.
    let mut terminal_name_buffer = [0 as libc::c_char; 128];
    // SAFETY: `terminal_name_buffer` is writable for the full length passed. A
    // descriptor that is no pseudoterminal master fails the call.
    if unsafe {
        ptsname_r(
            terminal_file_descriptor,
            terminal_name_buffer.as_mut_ptr(),
            terminal_name_buffer.len(),
        )
    } != 0
    {
        return None;
    }
    // SAFETY: a successful `ptsname_r` writes the name and its terminating
    // zero into the buffer, and fails when the buffer is too small.
    let terminal_name = unsafe { std::ffi::CStr::from_ptr(terminal_name_buffer.as_ptr()) };
    terminal_name.to_str().ok().map(str::to_string)
}

/// What a [`Waker`] is built on: the kernel's own one-descriptor notification.
///
/// Linux, Android and FreeBSD: an `eventfd`. Apple and the other BSDs: a
/// kernel event queue, whose descriptor a `poll` waits on the same way.
/// Anywhere else the type is named but never built — [`Waker::new`] yields
/// `None` there — and the reader blocks in `read`.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
type WakerInner = nix::sys::eventfd::EventFd;
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
type WakerInner = nix::sys::event::Kqueue;
#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))
))]
type WakerInner = std::os::fd::OwnedFd;

/// The one descriptor a pane's reader waits on beside its terminal. Another
/// thread rings it to bring the reader back from that wait.
///
/// A doorbell. A ring means *something about this pane changed*: its child
/// ended, or the backend is parking its readers. The reader reads which of
/// those it was from the pane's own state, never from the doorbell.
///
/// A ring stays pending until the reader drains it: one that lands before the
/// reader reaches its wait is still there when it does.
/// [`drain_wake_signal`](Waker::drain_wake_signal) takes it back off, and the
/// next wait blocks again.
#[cfg(unix)]
struct Waker(WakerInner);

#[cfg(unix)]
impl Waker {
    /// A waker nothing has woken yet, or `None` when the `eventfd` cannot be
    /// created.
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    fn new() -> Option<Self> {
        use nix::sys::eventfd::{EfdFlags, EventFd};

        // `EFD_CLOEXEC`: no child spawned after this pane inherits the
        // descriptor. `EFD_NONBLOCK`: `drain_wake_signal` returns at once whether or not
        // the doorbell is ringing.
        EventFd::from_flags(EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK)
            .ok()
            .map(Waker)
    }

    /// A waker nothing has woken yet: a kernel event queue carrying one user-triggered event, which
    /// [`wake_reader`](Waker::wake_reader) fires. The event is registered with `EV_CLEAR`: fetching
    /// it resets it, and [`drain_wake_signal`](Waker::drain_wake_signal) reads the queue as quiet
    /// again. A child process does not inherit the queue.
    ///
    /// `None` when the queue cannot be created or the event cannot be
    /// registered.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    fn new() -> Option<Self> {
        use nix::sys::event::{EventFilter, EventFlag, FilterFlag, KEvent, Kqueue};

        let event_queue = Kqueue::new().ok()?;
        let add_event = KEvent::new(
            0,
            EventFilter::EVFILT_USER,
            EventFlag::EV_ADD | EventFlag::EV_CLEAR,
            FilterFlag::empty(),
            0,
            0,
        );
        event_queue.kevent(&[add_event], &mut [], None).ok()?;
        Some(Waker(event_queue))
    }

    /// Always `None`: this platform offers no one-descriptor notification, and
    /// a pane's reader blocks in `read`.
    #[cfg(all(
        unix,
        not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "macos",
            target_os = "ios",
            target_os = "tvos",
            target_os = "watchos",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))
    ))]
    fn new() -> Option<Self> {
        None
    }

    /// Ring the doorbell, and leave it ringing until it is drained.
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    fn wake_reader(&self) {
        let _ = self.0.write(1);
    }

    /// Take the ring back off: reading the count returns it to zero, and the
    /// next wait blocks again.
    ///
    /// The descriptor is non-blocking; this returns at once whether or not the
    /// doorbell was ringing.
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    fn drain_wake_signal(&self) {
        let _ = self.0.read();
    }

    /// Ring the doorbell, and leave it ringing until it is drained: firing the
    /// registered event leaves it pending on the queue.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    fn wake_reader(&self) {
        use nix::sys::event::{EventFilter, EventFlag, FilterFlag, KEvent};

        let trigger_event = KEvent::new(
            0,
            EventFilter::EVFILT_USER,
            EventFlag::empty(),
            FilterFlag::NOTE_TRIGGER,
            0,
            0,
        );
        let _ = self.0.kevent(&[trigger_event], &mut [], None);
    }

    /// Take the ring back off: the event is registered with `EV_CLEAR`, and
    /// fetching it resets it, and the next wait blocks again.
    ///
    /// The fetch carries a zero timeout; this returns at once whether or not
    /// the doorbell was ringing.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    fn drain_wake_signal(&self) {
        use nix::sys::event::{EventFilter, EventFlag, FilterFlag, KEvent};

        let mut fetched_events = [KEvent::new(
            0,
            EventFilter::EVFILT_USER,
            EventFlag::empty(),
            FilterFlag::empty(),
            0,
            0,
        )];
        let no_wait_timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let _ = self
            .0
            .kevent(&[], &mut fetched_events, Some(no_wait_timeout));
    }

    /// Does nothing: no reader waits on this platform.
    #[cfg(all(
        unix,
        not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "macos",
            target_os = "ios",
            target_os = "tvos",
            target_os = "watchos",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))
    ))]
    fn wake_reader(&self) {}

    /// Does nothing: nothing rings on this platform.
    #[cfg(all(
        unix,
        not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "macos",
            target_os = "ios",
            target_os = "tvos",
            target_os = "watchos",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))
    ))]
    fn drain_wake_signal(&self) {}
}

#[cfg(unix)]
impl std::os::fd::AsFd for Waker {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// How many of a backend's readers are inside their pump, how many have
/// parked, and whether they are being held there.
#[cfg(unix)]
#[derive(Debug, Default)]
struct GateState {
    /// Whether a reader must park at the top of its next round.
    is_paused: bool,
    /// Readers inside their pump, counted from before the thread starts.
    reader_count: usize,
    /// Readers waiting at the park.
    parked_reader_count: usize,
}

/// What tells a pane's reader why its doorbell rang, and where to park.
///
/// The doorbell says only that something changed; `child_exit` and
/// `reader_gate` are the two things it can have been.
#[cfg(unix)]
struct ReaderSignals<'a> {
    /// The doorbell, waited on beside the pane's terminal.
    reader_waker: &'a Waker,
    /// Set by the watcher before it rings: the child has ended.
    child_exit: &'a ChildExit,
    /// Where the reader parks while the backend holds its readers.
    reader_gate: &'a ReaderGate,
}

/// Holds every pane's reader at the top of its round. The backend stops
/// reading terminals without ending a thread.
///
/// A reader parks before it waits on its terminal: a parked reader holds no
/// chunk and has read nothing it did not hand over. It keeps its `Delivery`
/// throughout, and with it the pane's exit channel, and is put back to work in
/// the same process.
///
/// [`wait_until_all_readers_parked`](ReaderGate::wait_until_all_readers_parked) has no deadline: it
/// returns once every counted reader has reached the park or left its pump. A
/// round waits, reads, and hands one chunk over; a reader inside the
/// consumer's [`PtySink::accept_output_bytes`] call reaches the park once that call
/// returns.
#[cfg(unix)]
struct ReaderGate {
    /// The counts and the pause flag, read and written as one.
    gate_state: Mutex<GateState>,
    /// Wakes a parked reader on resume, and the pause on every count change.
    reader_condition: Condvar,
}

#[cfg(unix)]
impl ReaderGate {
    /// A gate holding nobody, with no reader counted yet.
    fn new() -> Self {
        ReaderGate {
            gate_state: Mutex::new(GateState::default()),
            reader_condition: Condvar::new(),
        }
    }

    /// Count one reader in and hand back its place. Called before the reader's
    /// thread starts: a pause that lands first waits for that reader to reach
    /// the park.
    fn register_reader(self: &Arc<Self>) -> ReaderTicket {
        self.gate_state.lock().expect("reader gate").reader_count += 1;
        ReaderTicket {
            reader_gate: Arc::clone(self),
        }
    }

    /// Park here while the gate is paused. Returns at once, after one lock,
    /// when it is not.
    fn park_if_paused(&self) {
        let mut gate_state = self.gate_state.lock().expect("reader gate");
        if !gate_state.is_paused {
            return;
        }
        gate_state.parked_reader_count += 1;
        self.reader_condition.notify_all();
        while gate_state.is_paused {
            gate_state = self.reader_condition.wait(gate_state).expect("reader gate");
        }
        gate_state.parked_reader_count -= 1;
    }

    /// Mark the gate paused: every reader parks at the top of its next round.
    /// The caller rings each pane's doorbell to bring a reader waiting on a
    /// quiet terminal to that top.
    fn pause_readers(&self) {
        self.gate_state.lock().expect("reader gate").is_paused = true;
    }

    /// Wait until every counted reader has parked. A reader that left its pump
    /// is no longer counted and settles this too.
    fn wait_until_all_readers_parked(&self) {
        let mut gate_state = self.gate_state.lock().expect("reader gate");
        while gate_state.parked_reader_count != gate_state.reader_count {
            gate_state = self.reader_condition.wait(gate_state).expect("reader gate");
        }
    }

    /// Put every parked reader back to work.
    fn resume_readers(&self) {
        self.gate_state.lock().expect("reader gate").is_paused = false;
        self.reader_condition.notify_all();
    }
}

/// One reader's place in the gate, released when its pump ends.
///
/// Dropped by the reader thread itself the moment it leaves the pump, and by
/// the runtime if that thread panics. A reader that will never park again is
/// not waited on.
#[cfg(unix)]
struct ReaderTicket {
    reader_gate: Arc<ReaderGate>,
}

#[cfg(unix)]
impl Drop for ReaderTicket {
    fn drop(&mut self) {
        self.reader_gate
            .gate_state
            .lock()
            .expect("reader gate")
            .reader_count -= 1;
        self.reader_gate.reader_condition.notify_all();
    }
}

impl Delivery {
    /// Deliver one chunk of `pane_id`'s output. `false` means the reader stops
    /// delivering this pane: its exit is already settled and the consumer has
    /// let it go, or the consumer refused the chunk. The reader lets the
    /// consumer go there; on Windows it stays in `read` afterwards, discarding,
    /// until the pane's console is closed. Closing that console waits for its
    /// output to be read out.
    ///
    /// Takes the chunk borrowed and copies it after claiming it. A settled pane
    /// copies nothing.
    fn deliver_output(&self, pane_id: PaneId, output_bytes: &[u8]) -> bool {
        // Checked and claimed under one lock: a reader holding a chunk never
        // reads as idle to the watcher.
        {
            let mut exit_handover_state = self.exit_handover_state.lock().expect("handover");
            if exit_handover_state.is_settled {
                return false;
            }
            exit_handover_state.started_chunk_count += 1;
        }
        // The consumer is called with the lock released: it runs for as long
        // as it likes, and the watcher can still read the chunk as in flight
        // throughout.
        let is_output_accepted = self
            .pty_sink
            .accept_output_bytes(pane_id, output_bytes.to_vec());
        let mut exit_handover_state = self.exit_handover_state.lock().expect("handover");
        exit_handover_state.finished_chunk_count += 1;
        // A consumer that refuses a chunk is done with this pane and is handed
        // no exit afterwards. Settled in the same step that releases the chunk.
        if !is_output_accepted {
            exit_handover_state.is_settled = true;
        }
        is_output_accepted
    }

    /// Whether this pane's exit is already settled. A reader that finds it
    /// settled stops without taking another chunk.
    ///
    /// Read by [`pump_waited`], the one reader that is brought back from its
    /// wait to ask.
    #[cfg(unix)]
    fn is_settled(&self) -> bool {
        self.exit_handover_state
            .lock()
            .expect("handover")
            .is_settled
    }

    /// Report `pane_id`'s child as ended, once its output is exhausted: wait
    /// for the watcher's status and hand it to the PTY sink.
    ///
    /// Returns straight away when this pane's exit is already settled: the
    /// watcher delivered it, the consumer refused a chunk, or
    /// [`kill_pane`](PortablePtyBackend::kill_pane) closed the pane. Returns without
    /// publishing when the watcher's sender is gone. The consumer is told at
    /// most once, and a child that outlives its consumer does not pin the
    /// reader thread.
    fn publish_exit_status(self, pane_id: PaneId) {
        if self
            .exit_handover_state
            .lock()
            .expect("handover")
            .is_settled
        {
            return;
        }
        let Ok(exit_status) = self.exit_receiver.recv() else {
            return;
        };
        {
            let mut exit_handover_state = self.exit_handover_state.lock().expect("handover");
            if exit_handover_state.is_settled {
                return;
            }
            exit_handover_state.is_settled = true;
        }
        self.pty_sink.accept_exit_status(pane_id, exit_status);
    }
}

/// Build one pane's reader delivery.
///
/// The reader hands each chunk to `pty_sink`, and the delivery holds the
/// receiving end the reader takes the watcher's status from. The returned
/// sender is the watcher's.
fn build_delivery(
    pty_sink: &Arc<dyn PtySink>,
    exit_handover_state: &Arc<Mutex<ExitHandover>>,
) -> (Delivery, Sender<ExitStatus>) {
    let (exit_sender, exit_receiver) = channel::<ExitStatus>();
    (
        Delivery {
            pty_sink: Arc::clone(pty_sink),
            exit_receiver,
            exit_handover_state: Arc::clone(exit_handover_state),
        },
        exit_sender,
    )
}

/// Hand every chunk of `pane_id`'s output to `delivery`, blocking in `read` until
/// the terminal reports an end or the consumer lets the pane go.
///
/// The pump for a terminal that cannot be waited on: Windows, and a Unix
/// terminal that exposes no descriptor. Nothing interrupts a `read` here; the
/// thread stays in it until the last process holding the terminal open
/// releases it.
///
/// `true`: the terminal reached its end, or `read` failed with anything but
/// `Interrupted`; the caller reports the child's exit behind the output.
/// `false`: the consumer let the pane go mid-stream, and the terminal is still
/// open.
fn pump_blocking(terminal_reader: &mut dyn Read, delivery: &Delivery, pane_id: PaneId) -> bool {
    let mut read_buffer = [0u8; READ_CHUNK_BYTE_COUNT];
    loop {
        match terminal_reader.read(&mut read_buffer) {
            Ok(0) => return true,
            Ok(read_byte_count) => {
                if !delivery.deliver_output(pane_id, &read_buffer[..read_byte_count]) {
                    return false;
                }
            }
            Err(io_error) if io_error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return true,
        }
    }
}

/// The cursor-position request a Windows pseudoconsole writes to its output as
/// it opens. A pseudoconsole created with `PSEUDOCONSOLE_INHERIT_CURSOR` holds
/// its child's output until the process that created it answers this on the
/// pseudoconsole's input.
#[cfg(any(windows, test))]
const CURSOR_POSITION_REQUEST_BYTES: &[u8] = b"\x1b[6n";

/// The answer to [`CURSOR_POSITION_REQUEST_BYTES`]: the cursor is at row 1, column 1. Queued
/// on a pane's input as the pane opens.
#[cfg(windows)]
pub(crate) const CURSOR_POSITION_AT_HOME_RESPONSE_BYTES: &[u8] = b"\x1b[1;1R";

/// Reads a pane's output and takes the first [`CURSOR_POSITION_REQUEST_BYTES`] out of it.
///
/// [`spawn_pane`](PortablePtyBackend::spawn_pane) queues the answer to that request on
/// the pane's input; this only keeps the request itself from reaching the
/// consumer. Output ahead of it is delivered, less any tail that is still a
/// prefix of it, which is held until the next read settles it. Every byte after
/// it passes through untouched: a second request — one the pane's own program
/// made — is delivered.
///
/// Before → after: the terminal writes `\x1b[6nhello`, the output delivers
/// `hello`.
#[cfg(any(windows, test))]
struct RemovesCursorRequest<Reader: Read> {
    /// The pane's output.
    inner_reader: Reader,
    /// Output read but not yet handed to the caller, oldest first.
    pending_output_bytes: Vec<u8>,
    /// Bytes held back: they are the start of the request, and the rest of it
    /// has not been read yet. At most one byte short of the request.
    held_output_prefix: Vec<u8>,
    /// `true` once the request has been taken out. Every read after this passes
    /// straight through.
    is_done: bool,
}

#[cfg(any(windows, test))]
impl<Reader: Read> RemovesCursorRequest<Reader> {
    /// Read `inner_reader`, taking the request out of what it hands back.
    fn from_inner_reader(inner_reader: Reader) -> Self {
        Self {
            inner_reader,
            pending_output_bytes: Vec::new(),
            held_output_prefix: Vec::new(),
            is_done: false,
        }
    }

    /// Move up to `output_buffer.len()` bytes of
    /// [`pending_output_bytes`](Self::pending_output_bytes) into `output_buffer`,
    /// and return how many moved.
    fn drain_pending_output(&mut self, output_buffer: &mut [u8]) -> usize {
        let output_byte_count = self.pending_output_bytes.len().min(output_buffer.len());
        output_buffer[..output_byte_count]
            .copy_from_slice(&self.pending_output_bytes[..output_byte_count]);
        self.pending_output_bytes.drain(..output_byte_count);
        output_byte_count
    }
}

/// Where `pattern_bytes` starts in `source_bytes`.
#[cfg(any(windows, test))]
fn find_subslice_position(source_bytes: &[u8], pattern_bytes: &[u8]) -> Option<usize> {
    source_bytes
        .windows(pattern_bytes.len())
        .position(|window| window == pattern_bytes)
}

/// How many bytes at the end of `source_bytes` are a prefix of
/// `pattern_bytes`. `0` when none are. Never counts `pattern_bytes` whole: at
/// most `pattern_bytes.len() - 1`.
#[cfg(any(windows, test))]
fn compute_partial_tail_length(source_bytes: &[u8], pattern_bytes: &[u8]) -> usize {
    let partial_tail_length_limit = source_bytes.len().min(pattern_bytes.len() - 1);
    (1..=partial_tail_length_limit)
        .rev()
        .find(|&partial_length| {
            source_bytes[source_bytes.len() - partial_length..] == pattern_bytes[..partial_length]
        })
        .unwrap_or(0)
}

#[cfg(any(windows, test))]
impl<Reader: Read> Read for RemovesCursorRequest<Reader> {
    /// An empty `output_buffer` reads `0` bytes, holds whatever is pending, and
    /// reads nothing from the inner reader.
    fn read(&mut self, output_buffer: &mut [u8]) -> std::io::Result<usize> {
        if output_buffer.is_empty() {
            return Ok(0);
        }
        loop {
            if !self.pending_output_bytes.is_empty() {
                return Ok(self.drain_pending_output(output_buffer));
            }
            if self.is_done {
                return self.inner_reader.read(output_buffer);
            }
            let read_byte_count = self.inner_reader.read(output_buffer)?;
            if read_byte_count == 0 {
                // The terminal ended with no request on it: what was held back
                // is delivered as output.
                self.pending_output_bytes = std::mem::take(&mut self.held_output_prefix);
                self.is_done = true;
                if self.pending_output_bytes.is_empty() {
                    return Ok(0);
                }
                continue;
            }

            let mut output_bytes = std::mem::take(&mut self.held_output_prefix);
            output_bytes.extend_from_slice(&output_buffer[..read_byte_count]);
            match find_subslice_position(&output_bytes, CURSOR_POSITION_REQUEST_BYTES) {
                Some(request_position) => {
                    output_bytes.drain(
                        request_position..request_position + CURSOR_POSITION_REQUEST_BYTES.len(),
                    );
                    self.is_done = true;
                }
                None => {
                    let retained_output_byte_count = output_bytes.len()
                        - compute_partial_tail_length(&output_bytes, CURSOR_POSITION_REQUEST_BYTES);
                    self.held_output_prefix = output_bytes.split_off(retained_output_byte_count);
                }
            }
            self.pending_output_bytes = output_bytes;
            // Everything read was the request or the start of it: read again,
            // and report no end.
            if self.pending_output_bytes.is_empty() {
                continue;
            }
        }
    }
}

/// Read `reader` to its end, discarding everything.
///
/// The pane's reader runs this once its consumer has let the pane go. Closing
/// a pane's console waits for the output it still holds to be read out.
#[cfg(windows)]
fn drain_terminal(terminal_reader: &mut dyn Read) {
    let mut read_buffer = [0u8; READ_CHUNK_BYTE_COUNT];
    loop {
        match terminal_reader.read(&mut read_buffer) {
            Ok(0) => return,
            Ok(_) => {}
            Err(io_error) if io_error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

/// Hand every chunk of `pane_id`'s output to `delivery`, waiting on the terminal
/// beside `reader_signals.reader_waker`; the thread never blocks in `read`.
///
/// While the child runs, the wait carries no timer: an idle pane costs no
/// wakeups. The wait ends when the terminal has something, or when the
/// doorbell rings.
///
/// The watcher rings the doorbell once it has seen the child end, and the ring
/// reaches a reader that a descendant holding the terminal open keeps waiting.
/// The ring stays pending until this pump drains it: one that lands before the
/// reader reaches its wait is still there when it does.
///
/// The doorbell only says something changed; the pump reads
/// `reader_signals.child_exit` to see what. The grace rounds start when that
/// record says the child has ended, never on the ring alone: the watcher stores
/// the record before it rings, and `reader_signals.reader_gate` rings the same
/// doorbell to bring the reader to its park. From then on the wait runs in
/// rounds of `grace_duration`, and a round in which the terminal produces
/// nothing ends the pump: everything the dead child printed has been handed
/// over, and the caller publishes the exit behind it. Rounds stop
/// `limit_duration` after the ring that found the child ended, which bounds a
/// descendant that holds the terminal open and keeps printing.
///
/// Each round opens at the gate's park, before the wait. A reader held there
/// has read nothing it did not hand over.
///
/// The pump also ends when the terminal reports its end, when `read` fails
/// with anything but `Interrupted`, when the consumer refuses a chunk, when a
/// ring finds the pane settled, and when the wait fails with anything but
/// `EINTR`. [`kill_pane`](PortablePtyBackend::kill_pane) settles the pane and then
/// kills the child: the ring that kill fires, and the one the watcher fires
/// once the child has ended, find the pane settled, and the pump stops without
/// starting a round.
#[cfg(unix)]
fn pump_waited(
    delivery: &Delivery,
    pane_id: PaneId,
    terminal_fd: &std::os::fd::OwnedFd,
    reader_signals: ReaderSignals<'_>,
    grace_duration: Duration,
    limit_duration: Duration,
) {
    use nix::errno::Errno;
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::AsFd;

    /// Whether the wait reported anything at all on `poll_descriptor`. Bytes, a slave that
    /// closed, and a descriptor that went bad all mean the same thing here:
    /// call `read` and let it say which.
    fn has_poll_event(poll_descriptor: &PollFd) -> bool {
        poll_descriptor
            .revents()
            .is_some_and(|poll_events| !poll_events.is_empty())
    }

    let grace_poll_timeout = PollTimeout::try_from(grace_duration).unwrap_or(PollTimeout::MAX);
    let mut read_buffer = [0u8; READ_CHUNK_BYTE_COUNT];
    // `Some` from the moment the child is known to have gone, carrying the
    // point the rounds stop at.
    let mut exit_deadline: Option<Instant> = None;

    loop {
        reader_signals.reader_gate.park_if_paused();

        let mut is_woken = false;
        let has_terminal_event = match exit_deadline {
            None => {
                let mut poll_file_descriptors = [
                    PollFd::new(terminal_fd.as_fd(), PollFlags::POLLIN),
                    PollFd::new(reader_signals.reader_waker.as_fd(), PollFlags::POLLIN),
                ];
                match poll(&mut poll_file_descriptors, PollTimeout::NONE) {
                    Ok(_) => {}
                    // A signal caught during the wait is not an end: wait again.
                    Err(Errno::EINTR) => continue,
                    Err(_) => return,
                }
                if has_poll_event(&poll_file_descriptors[1]) {
                    // Take the ring off before reading the state. A ring that
                    // lands after this leaves the descriptor readable, and the
                    // next round sees it.
                    reader_signals.reader_waker.drain_wake_signal();
                    if delivery.is_settled() {
                        return;
                    }
                    // A ring from the gate leaves the rounds alone: the child
                    // is still running, and the pump loops back to the park.
                    if reader_signals.child_exit.has_exited() {
                        exit_deadline = Some(Instant::now() + limit_duration);
                        is_woken = true;
                    }
                }
                has_poll_event(&poll_file_descriptors[0])
            }
            Some(_) => {
                let mut poll_file_descriptors =
                    [PollFd::new(terminal_fd.as_fd(), PollFlags::POLLIN)];
                match poll(&mut poll_file_descriptors, grace_poll_timeout) {
                    Ok(_) => {}
                    Err(Errno::EINTR) => continue,
                    Err(_) => return,
                }
                has_poll_event(&poll_file_descriptors[0])
            }
        };

        if has_terminal_event {
            match read_terminal(terminal_fd, &mut read_buffer) {
                Ok(0) => return,
                Ok(read_byte_count) => {
                    if !delivery.deliver_output(pane_id, &read_buffer[..read_byte_count]) {
                        return;
                    }
                }
                Err(io_error) if io_error.kind() == ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        } else if exit_deadline.is_some() && !is_woken {
            // A whole round with nothing from a terminal whose child has gone.
            // The round the wake itself landed in is not one of these: it ran
            // until the wake arrived, not for its own length.
            return;
        }

        if exit_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return;
        }
    }
}

/// Stand by for a pane's reader to settle the child's exit, answering whether
/// the watcher has to publish it instead.
///
/// The reader settles once the child's output has run out. Checks in every
/// `grace_duration` until `exit_deadline`; `true` says the deadline passed with
/// the reader still short of the end.
///
/// A chunk in the consumer's hands holds the answer back for as long as it is
/// in flight, however far past `exit_deadline` that runs.
///
/// `false` means stop without publishing: the reader settled, or
/// `cancel_receiver` carried a value — [`kill_pane`](PortablePtyBackend::kill_pane)
/// closing the pane — or its sender was dropped, which is the backend shutting
/// down. `kill_pane` settles the exit before it sends.
fn should_publish_exit(
    cancel_receiver: &Receiver<()>,
    exit_handover_state: &Mutex<ExitHandover>,
    exit_deadline: Instant,
    grace_duration: Duration,
) -> bool {
    loop {
        match cancel_receiver.recv_timeout(grace_duration) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return false,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let (is_settled, has_chunk_in_flight) = {
            let exit_handover_state = exit_handover_state.lock().expect("handover");
            (
                exit_handover_state.is_settled,
                exit_handover_state.has_chunk_in_flight(),
            )
        };
        if is_settled {
            return false;
        }
        if Instant::now() >= exit_deadline && !has_chunk_in_flight {
            return true;
        }
    }
}

/// Wait until the pane's reader is back reading its terminal. The caller
/// closes the terminal after this.
///
/// Windows waits for a pseudoconsole's output pipe to be read out before
/// `ClosePseudoConsole` returns, and the pane's reader is that pipe's one
/// reader. A reader still handing a chunk to its consumer is not reading it.
///
/// Checks every `check_in_duration`. Returns once no chunk is in the consumer's hands, once
/// `cancel_receiver` carries a value — [`kill_pane`](PortablePtyBackend::kill_pane) closing the
/// pane — or once its sender is dropped, which is the backend shutting down.
#[cfg(windows)]
fn wait_for_reader_to_read_again(
    cancel_receiver: &Receiver<()>,
    exit_handover_state: &Mutex<ExitHandover>,
    check_in_duration: Duration,
) {
    loop {
        if !exit_handover_state
            .lock()
            .expect("handover")
            .has_chunk_in_flight()
        {
            return;
        }
        match cancel_receiver.recv_timeout(check_in_duration) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// A pane's master end: `portable-pty`'s master.
#[cfg(unix)]
type PtyMaster = Box<dyn MasterPty + Send>;

/// A pane's master end: the pane's pseudoconsole.
#[cfg(windows)]
type PtyMaster = PaneConsole;

/// A pane's child process: `portable-pty`'s child.
#[cfg(unix)]
type PtyChild = Box<dyn Child + Send + Sync>;

/// A pane's child process: the pane program.
#[cfg(windows)]
type PtyChild = PaneProcess;

/// A pane's terminal, as the backend holds it for resizing.
///
/// The `Owned` arm is one descriptor the pane opened for itself, shared with
/// its reader and writer threads: the whole pane spends a single descriptor on
/// its terminal. `Master` keeps the pane's master end: `portable-pty`'s master
/// for a Unix terminal that exposes no descriptor to share, and the pane's
/// pseudoconsole on Windows.
enum Terminal {
    /// The pane's own descriptor, also held by its reader and writer.
    #[cfg(unix)]
    Owned(Arc<std::os::fd::OwnedFd>),
    /// The pane's master end, in a slot the watcher shares. On Windows the
    /// watcher takes the pseudoconsole out and drops it once the child has
    /// ended, which closes the pane's terminal; the slot is empty from then on.
    Master(Arc<Mutex<Option<PtyMaster>>>),
}

impl Terminal {
    /// Tell the child its terminal is now `pty_size`. A terminal already closed
    /// takes the new size as a no-op.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] if the kernel refuses the new size.
    fn resize(&self, pty_size: PtySize) -> Result<(), PtyError> {
        let build_io_error = |io_detail: String| PtyError::Io { detail: io_detail };
        match self {
            #[cfg(unix)]
            Terminal::Owned(terminal_fd) => resize_terminal(terminal_fd, pty_size)
                .map_err(|io_error| build_io_error(io_error.to_string())),
            Terminal::Master(pty_master_slot) => {
                match pty_master_slot.lock().expect("terminal").as_ref() {
                    #[cfg(unix)]
                    Some(master_pty) => master_pty
                        .resize(build_portable_pty_size(pty_size))
                        .map_err(|io_error| build_io_error(io_error.to_string())),
                    #[cfg(windows)]
                    Some(pane_console) => pane_console
                        .resize(pty_size)
                        .map_err(|io_error| build_io_error(io_error.to_string())),
                    // The terminal is closed: nothing is left to retune.
                    None => Ok(()),
                }
            }
        }
    }
}

/// What a pane's reader thread drains its terminal with.
enum ReadSide {
    /// The pane's own descriptor, waited on beside a [`Waker`] that brings the
    /// reader back from that wait.
    #[cfg(unix)]
    Owned(Arc<std::os::fd::OwnedFd>, Arc<Waker>),
    /// A reader blocked in `read` until the terminal ends: `portable-pty`'s
    /// reader on Unix, the read end of the pseudoconsole's output pipe on
    /// Windows.
    Blocking(Box<dyn Read + Send>),
}

/// What a pane's writer thread sends the child's input with.
enum WriteSide {
    /// The pane's own descriptor — the same one its reader holds; both
    /// directions of the terminal share it.
    #[cfg(unix)]
    Owned(Arc<std::os::fd::OwnedFd>),
    /// A writer of the terminal's own, which reports the end of input when
    /// dropped: `portable-pty`'s writer on Unix, the write end of the
    /// pseudoconsole's input pipe on Windows.
    Blocking(Box<dyn Write + Send>),
}

/// What the per-pane writer thread accepts on its channel.
enum WriterMessage {
    /// Bytes to write to the child's stdin.
    Bytes(Vec<u8>),
    /// Answer on this channel, which the writer does once it reaches this
    /// message. Queued by [`PortablePtyBackend::flush_writers`], and answered
    /// after every earlier [`WriterMessage::Bytes`] has been written.
    Barrier(Sender<()>),
    /// Stop and release the thread: queued by the watcher once the child has
    /// exited. The writer wakes on no timer.
    Stop,
}

/// What a pane's watcher thread does after it has seen the child end, chosen by
/// how that pane's terminal can reach the end of its output.
enum WatcherTail {
    /// Nothing: the reader reaches the end on its own and publishes the exit.
    #[cfg(not(windows))]
    ReaderPublishes,
    /// Close the pane's terminal, which brings the reader to the end, and
    /// stand by on a deadline for the exit. Dropping the master closes the console,
    /// which flushes its remaining output and then ends the reader's pipe. The
    /// close runs on a thread of its own and returns only once every process
    /// attached to that console has let it go. The standby publishes the exit
    /// to `pty_sink` when the reader has not settled it by the deadline.
    #[cfg(windows)]
    CloseTerminal {
        /// The pane's pseudoconsole, taken out and dropped to close it.
        pty_master_slot: Arc<Mutex<Option<PtyMaster>>>,
        /// Where the standby publishes the exit. Held weakly: a watcher
        /// waiting on a long-lived child keeps no consumer alive.
        pty_sink: Weak<dyn PtySink>,
    },
    /// Stand by on a deadline and publish the exit to this PTY sink, for a terminal
    /// the reader cannot be brought back from. Held weakly: a watcher waiting
    /// on a long-lived child keeps no consumer alive.
    #[cfg(not(windows))]
    WaitForReaderThenPublish(Weak<dyn PtySink>),
}

/// Give `pane_id`'s reader until the standby's deadline to settle the exit, then
/// settle it here and publish `exit_status` if the reader never did.
///
/// The watcher's whole tail once the child has ended, on both paths that have
/// one: a Unix terminal exposing no descriptor, and a Windows pane whose
/// console is being closed. Nothing is settled or published when `pty_sink` no
/// longer has an owner.
fn wait_for_reader_before_publishing(
    cancel_receiver: &Receiver<()>,
    exit_handover_state: &Mutex<ExitHandover>,
    pty_sink: &Weak<dyn PtySink>,
    pane_id: PaneId,
    exit_status: ExitStatus,
) {
    let should_publish_exit_status = should_publish_exit(
        cancel_receiver,
        exit_handover_state,
        Instant::now() + EXIT_PUBLISH_LIMIT_DURATION,
        EXIT_PUBLISH_GRACE_DURATION,
    );
    if let Some(pty_sink) = pty_sink.upgrade() {
        settle_exit_and_publish_status(
            &pty_sink,
            exit_handover_state,
            pane_id,
            exit_status,
            should_publish_exit_status,
        );
    }
}

/// Settle `pane_id`'s exit, and hand `exit_status` to `pty_sink` when
/// `should_publish_exit_status` is `true`
/// and nothing settled it first.
///
/// Settles whether or not it publishes. `should_publish_exit_status` is
/// [`should_publish_exit`]'s answer.
fn settle_exit_and_publish_status(
    pty_sink: &Arc<dyn PtySink>,
    exit_handover_state: &Mutex<ExitHandover>,
    pane_id: PaneId,
    exit_status: ExitStatus,
    should_publish_exit_status: bool,
) {
    let is_already_settled = std::mem::replace(
        &mut exit_handover_state.lock().expect("handover").is_settled,
        true,
    );
    if should_publish_exit_status && !is_already_settled {
        pty_sink.accept_exit_status(pane_id, exit_status);
    }
}

/// Kills the wrapped child on drop unless [`release_child`](ChildGuard::release_child)ed.
///
/// Dropping a `portable-pty` child or a Windows pane program does not terminate
/// the process. The guard wraps the child through
/// [`spawn_pane`](PortablePtyBackend::spawn_pane)'s fallible setup: a step
/// after launch that returns early drops the guard, which kills the child.
/// [`release_child`](ChildGuard::release_child) hands the child to the watcher
/// thread, and the guard kills nothing after that.
struct ChildGuard {
    child_process: Option<PtyChild>,
}

impl ChildGuard {
    fn from_child(child_process: PtyChild) -> Self {
        ChildGuard {
            child_process: Some(child_process),
        }
    }

    /// Take the child out, leaving the guard inert (no kill on drop).
    fn release_child(mut self) -> PtyChild {
        self.child_process
            .take()
            .expect("child present until disarmed")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child_process) = self.child_process.take() {
            let _ = child_process.kill();
        }
    }
}

#[cfg(unix)]
impl std::ops::Deref for ChildGuard {
    type Target = dyn Child + Send + Sync;

    fn deref(&self) -> &Self::Target {
        self.child_process
            .as_deref()
            .expect("child present until disarmed")
    }
}

/// Start a pane's writer thread on `write_side`, and hand back the channel its input
/// is queued on.
///
/// The thread parks in `recv` with no timer: an idle pane costs no wakeups. It
/// ends on either teardown path: the channel closing, or the
/// [`WriterMessage::Stop`] the watcher queues once the child is gone. `Stop`
/// travels the same channel as the bytes: every write queued before the child
/// exited is written first. Nothing is written to the terminal on the way out.
/// A write that fails is dropped, and the thread takes the next message.
///
/// A [`WriterMessage::Barrier`] travels that same channel and is answered where it
/// sits in it: its answer means every byte queued before it is on the
/// terminal.
///
/// After each [`WriterMessage::Bytes`] it has written, or failed to write, the
/// thread takes that message's length off `queued_input_byte_count`.
fn start_writer(
    write_side: WriteSide,
    queued_input_byte_count: Arc<AtomicUsize>,
) -> Sender<WriterMessage> {
    let (writer_sender, writer_receiver) = channel::<WriterMessage>();

    let _ = spawn_pty_thread("koshi-pty-write", move || {
        let mut write_side = write_side;
        while let Ok(writer_message) = writer_receiver.recv() {
            match writer_message {
                WriterMessage::Bytes(input_bytes) => {
                    match &mut write_side {
                        #[cfg(unix)]
                        WriteSide::Owned(terminal_fd) => {
                            let _ = write_terminal(terminal_fd, &input_bytes);
                        }
                        WriteSide::Blocking(pty_writer) => {
                            let _ = pty_writer
                                .write_all(&input_bytes)
                                .and_then(|_| pty_writer.flush());
                        }
                    }
                    queued_input_byte_count.fetch_sub(input_bytes.len(), Ordering::SeqCst);
                }
                WriterMessage::Barrier(barrier_sender) => {
                    let _ = barrier_sender.send(());
                }
                WriterMessage::Stop => break,
            }
        }
    });

    writer_sender
}

/// Queue `input_bytes` for `pane_id`'s writer thread on `writer_sender`, and
/// count them in `queued_input_byte_count` until that thread has written them.
///
/// # Errors
/// [`PtyError::InputQueueFull`] when the bytes already counted plus
/// `input_bytes` would pass [`MAX_QUEUED_PANE_INPUT_BYTE_COUNT`]: nothing is
/// queued or counted. [`PtyError::Io`] when the writer thread has ended: the
/// count is taken back.
fn queue_pane_input(
    pane_id: PaneId,
    writer_sender: &Sender<WriterMessage>,
    queued_input_byte_count: &AtomicUsize,
    input_bytes: &[u8],
) -> Result<(), PtyError> {
    queued_input_byte_count
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |queued_byte_count| {
            queued_byte_count
                .checked_add(input_bytes.len())
                .filter(|counted_byte_count| {
                    *counted_byte_count <= MAX_QUEUED_PANE_INPUT_BYTE_COUNT
                })
        })
        .map_err(|queued_byte_count| PtyError::InputQueueFull {
            pane_id,
            queued_byte_count,
        })?;
    writer_sender
        .send(WriterMessage::Bytes(input_bytes.to_vec()))
        .map_err(|send_error| {
            queued_input_byte_count.fetch_sub(input_bytes.len(), Ordering::SeqCst);
            PtyError::Io {
                detail: send_error.to_string(),
            }
        })
}

/// Start the reader thread of a pane that owns its terminal descriptor.
///
/// It waits on the descriptor beside its doorbell, hands each chunk to
/// `delivery`, and parks at the gate `reader_ticket` holds a place in whenever
/// the backend holds its readers. Once the pump ends it drops `reader_ticket` — the gate
/// stops waiting for it — and then reports the child's end, which waits for the
/// watcher's status.
#[cfg(unix)]
fn start_owned_reader(
    delivery: Delivery,
    pane_id: PaneId,
    terminal_fd: Arc<std::os::fd::OwnedFd>,
    reader_waker: Arc<Waker>,
    child_exit: Arc<ChildExit>,
    reader_ticket: ReaderTicket,
) -> JoinHandle<()> {
    spawn_pty_thread("koshi-pty-read", move || {
        pump_waited(
            &delivery,
            pane_id,
            &terminal_fd,
            ReaderSignals {
                reader_waker: &reader_waker,
                child_exit: &child_exit,
                reader_gate: &reader_ticket.reader_gate,
            },
            EXIT_PUBLISH_GRACE_DURATION,
            EXIT_PUBLISH_LIMIT_DURATION,
        );
        drop(reader_ticket);
        delivery.publish_exit_status(pane_id);
    })
}

/// How a pane's child ended, as the pane's watcher learned it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ObservedChildExit {
    /// How the child ended.
    exit_status: ExitStatus,
    /// `true` when the ended child stays a zombie of this process until
    /// [`kill_pane`](PortablePtyBackend::kill_pane) reaps it: its process ID
    /// and its process group ID stay reserved until then. `false` when an
    /// earlier process image reaped it, or when its end could not be waited on.
    #[cfg(unix)]
    is_zombie_kept: bool,
}

/// How a pane's child ended, and whether the pane's watcher reaps it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ChildExitRecord {
    /// How the child ended. `None` while it runs.
    observed_exit: Option<ObservedChildExit>,
    /// `true` once [`kill_pane`](PortablePtyBackend::kill_pane) stopped
    /// waiting for a child that still ran ([`ChildExit::leave_child_running`]).
    /// The watcher then reaps the child's zombie when the child ends.
    is_left_running: bool,
}

/// How a pane's child ended, once its watcher saw it end, and the wake-up of
/// every thread that waits for that.
#[derive(Default)]
struct ChildExit {
    /// How the child ended, and who reaps it.
    exit_record: Mutex<ChildExitRecord>,
    /// Notified once `exit_record.observed_exit` is set.
    exit_published: Condvar,
}

impl ChildExit {
    /// The lock on `exit_record`, taken back from a thread that panicked
    /// while it held it.
    fn lock_exit_record(&self) -> MutexGuard<'_, ChildExitRecord> {
        self.exit_record
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Record `observed_exit` as how the child ended, and wake every thread in
    /// [`wait_for_exit`](Self::wait_for_exit). An exit already recorded stays.
    ///
    /// Returns `true` when [`kill_pane`](PortablePtyBackend::kill_pane) left
    /// the child running ([`leave_child_running`](Self::leave_child_running)):
    /// the caller reaps a kept zombie then.
    fn publish_observed_exit(&self, observed_exit: ObservedChildExit) -> bool {
        let is_left_running = {
            let mut exit_record = self.lock_exit_record();
            exit_record.observed_exit.get_or_insert(observed_exit);
            exit_record.is_left_running
        };
        self.exit_published.notify_all();
        is_left_running
    }

    /// Stop waiting for a child that still runs, and hand the reap of its
    /// zombie to the watcher.
    ///
    /// Returns `true` when the child still ran: the watcher reaps it when it
    /// ends. Returns `false`, and changes nothing, when the exit was recorded
    /// first: the caller reaps it.
    fn leave_child_running(&self) -> bool {
        let mut exit_record = self.lock_exit_record();
        if exit_record.observed_exit.is_some() {
            return false;
        }
        exit_record.is_left_running = true;
        true
    }

    /// How the child ended, or `None` while it runs.
    fn get_exit_status(&self) -> Option<ExitStatus> {
        self.lock_exit_record()
            .observed_exit
            .map(|observed_exit| observed_exit.exit_status)
    }

    /// How the child ended, for the process image that takes this pane back
    /// next. `None` while the child runs, and on Unix also while its zombie is
    /// kept: that image waits on the process ID itself.
    fn get_carried_exit_status(&self) -> Option<ExitStatus> {
        let observed_exit = self.lock_exit_record().observed_exit?;
        #[cfg(unix)]
        if observed_exit.is_zombie_kept {
            return None;
        }
        Some(observed_exit.exit_status)
    }

    /// Whether the child has ended.
    fn has_exited(&self) -> bool {
        self.get_exit_status().is_some()
    }

    /// Whether the child's process ID, and the process group ID it names, are
    /// still reserved for this pane: `true` while the child runs, and after it
    /// ended while its zombie is kept.
    #[cfg(unix)]
    fn is_process_id_reserved(&self) -> bool {
        self.lock_exit_record()
            .observed_exit
            .is_none_or(|observed_exit| observed_exit.is_zombie_kept)
    }

    /// Whether the ended child stays a zombie that
    /// [`kill_pane`](PortablePtyBackend::kill_pane) reaps. `false` while the
    /// child runs.
    #[cfg(unix)]
    fn is_zombie_kept(&self) -> bool {
        self.lock_exit_record()
            .observed_exit
            .is_some_and(|observed_exit| observed_exit.is_zombie_kept)
    }

    /// Wait up to `wait_duration` for the child to end, and say whether it has.
    /// A child that already ended returns `true` at once.
    fn wait_for_exit(&self, wait_duration: Duration) -> bool {
        let (exit_record, _) = self
            .exit_published
            .wait_timeout_while(self.lock_exit_record(), wait_duration, |exit_record| {
                exit_record.observed_exit.is_none()
            })
            .unwrap_or_else(PoisonError::into_inner);
        exit_record.observed_exit.is_some()
    }
}

/// The pane threads a watcher releases once it holds the child's exit status.
struct WatchRelease {
    /// The record of how the child ended, kept on the pane:
    /// [`kill_pane`](PortablePtyBackend::kill_pane) reads it before signalling
    /// the leader, and
    /// [`list_carried_panes`](PortablePtyBackend::list_carried_panes) hands it to
    /// the next process image.
    child_exit: Arc<ChildExit>,
    /// Carries the status to whoever publishes it.
    exit_status_sender: Sender<ExitStatus>,
    /// Releases the pane's writer thread.
    writer_stop_sender: Sender<WriterMessage>,
    /// The reader's doorbell, rung once the child has ended; the reader takes
    /// the last of the child's output.
    /// `None` for a reader that cannot be brought back from its `read`.
    #[cfg(unix)]
    reader_waker: Option<Arc<Waker>>,
    /// The child's process ID, reaped here when
    /// [`kill_pane`](PortablePtyBackend::kill_pane) left the child running.
    #[cfg(unix)]
    child_process_id: u32,
    /// The backend's set of children left running, which `child_process_id`
    /// leaves once this release reaps it.
    #[cfg(unix)]
    left_running_child_process_ids: Arc<Mutex<BTreeSet<u32>>>,
}

impl WatchRelease {
    /// Record `observed_exit` on the pane, hand its status on, release the
    /// writer, and ring the reader's doorbell. On Unix, a child that
    /// [`kill_pane`](PortablePtyBackend::kill_pane) left running has its kept
    /// zombie reaped first, and leaves `left_running_child_process_ids`.
    fn publish_exit_status(&self, observed_exit: ObservedChildExit) {
        let is_left_running = self.child_exit.publish_observed_exit(observed_exit);
        #[cfg(unix)]
        if is_left_running {
            if observed_exit.is_zombie_kept {
                reap_child(self.child_process_id);
            }
            lock_left_running_child_process_ids(&self.left_running_child_process_ids)
                .remove(&self.child_process_id);
        }
        #[cfg(windows)]
        let _ = is_left_running;
        let _ = self.exit_status_sender.send(observed_exit.exit_status);
        let _ = self.writer_stop_sender.send(WriterMessage::Stop);
        #[cfg(unix)]
        if let Some(reader_waker) = &self.reader_waker {
            reader_waker.wake_reader();
        }
    }
}

/// Wait for child `child_process_id` to end, and report how it ended, leaving
/// it a zombie: its process ID and process group ID stay reserved until
/// [`reap_child`] runs.
///
/// `waitid` with `WEXITED | WNOWAIT` is retried on `EINTR`. A child that
/// ended with a code gives [`ExitStatus::ExitCode`], one a signal ended gives
/// [`ExitStatus::Signaled`] with that signal's number, and both keep the
/// zombie. Any other failure, such as `ECHILD` for a child this process cannot
/// wait on, gives [`UNOBSERVED_EXIT`] with no zombie kept.
///
/// Example: a child that ran `exit 5` gives `ExitCode(5)`, and a child that
/// `kill -TERM` ended gives `Signaled(15)`.
#[cfg(unix)]
fn wait_for_child_exit(child_process_id: u32) -> ObservedChildExit {
    loop {
        // SAFETY: an all-zero `siginfo_t` is valid, and `waitid` writes the
        // child's state into it.
        let mut child_signal_information: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `waitid` writes only into `child_signal_information`.
        let wait_result = unsafe {
            libc::waitid(
                libc::P_PID,
                child_process_id,
                &mut child_signal_information,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if wait_result == 0 {
            // SAFETY: `waitid` answered for an ended child, which fills the
            // status field.
            let child_status = unsafe { child_signal_information.si_status() };
            let exit_status = match child_signal_information.si_code {
                libc::CLD_EXITED => ExitStatus::ExitCode(child_status),
                libc::CLD_KILLED | libc::CLD_DUMPED => ExitStatus::Signaled(child_status),
                _ => UNOBSERVED_EXIT,
            };
            return ObservedChildExit {
                exit_status,
                is_zombie_kept: true,
            };
        }
        if std::io::Error::last_os_error().kind() != ErrorKind::Interrupted {
            return ObservedChildExit {
                exit_status: UNOBSERVED_EXIT,
                is_zombie_kept: false,
            };
        }
    }
}

/// The lock on a backend's set of children left running, taken back from a
/// thread that panicked while it held it.
#[cfg(unix)]
fn lock_left_running_child_process_ids(
    left_running_child_process_ids: &Mutex<BTreeSet<u32>>,
) -> MutexGuard<'_, BTreeSet<u32>> {
    left_running_child_process_ids
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Reap the zombie child `child_process_id` that [`wait_for_child_exit`]
/// kept, which returns its process ID and process group ID to the system.
///
/// `waitpid` with `WNOHANG` never waits: a child that still runs is left
/// alone, and an ID that names no child of this process is passed over. `0`
/// and an ID above `i32::MAX`, which `waitpid` reads as a process group, are
/// passed over too.
#[cfg(unix)]
fn reap_child(child_process_id: u32) {
    let Ok(waited_process_id) = libc::pid_t::try_from(child_process_id) else {
        return;
    };
    if waited_process_id <= 0 {
        return;
    }
    // SAFETY: a null status pointer makes `waitpid` write no status.
    unsafe { libc::waitpid(waited_process_id, std::ptr::null_mut(), libc::WNOHANG) };
}

/// Everything the backend retains for one live pane, keyed by [`PaneId`].
///
/// A PTY is a pair of linked endpoints. The **slave** end became the child
/// process's controlling terminal (its stdin/stdout/stderr) and was handed off
/// when the child was spawned; the **master** end stays here. Bytes we write to
/// the master reach the child as if typed at a keyboard; bytes the child prints
/// come back out of the master for us to read.
///
/// One descriptor carries both of those directions: a pane holds a single
/// copy of the master, and its reader, writer and resize all use that one.
/// With the reader's waker, a pane spends two descriptors in total.
struct PaneEntry {
    /// This pane's terminal. While it is held the kernel keeps the pair open,
    /// and [`resize_pane`](PortablePtyBackend::resize_pane) retunes the window size
    /// through it.
    terminal: Terminal,
    /// The last size this pane's terminal was set to: what it was spawned or
    /// taken back at, then whatever the newest successful
    /// [`resize_pane`](PortablePtyBackend::resize_pane) carried.
    pty_size: PtySize,
    /// Input channel to the per-pane writer thread. Bytes sent here are written
    /// to the master, and reach the child, on that thread: a child that has
    /// stopped reading its stdin blocks only the writer thread, never the
    /// dispatcher.
    ///
    /// A [`WriterMessage::Stop`] releases the writer once the input queued
    /// before it is written. The watcher queues one once the child has exited,
    /// including on a pane left open past its child's death, and holds a clone
    /// of this sender to do so. [`kill_pane`](PortablePtyBackend::kill_pane)
    /// queues one before it waits for the child.
    ///
    /// A writer already blocked inside its write — the child stopped reading
    /// while a `setsid` descendant still holds the slave open (Linux; macOS
    /// `revoke`s it) — cannot be interrupted; like the reader it exits only once
    /// that descriptor finally closes. `kill_pane` never joins it: its thread and
    /// that descriptor stay until then, and the dispatcher is never blocked.
    /// [`flush_writers`](PortablePtyBackend::flush_writers) sends its barrier on
    /// this same channel, and names the pane whose writer is in that state.
    writer_sender: Sender<WriterMessage>,
    /// How many bytes [`queue_pane_input`] queued on `writer_sender` that the
    /// writer thread has not finished writing.
    queued_input_byte_count: Arc<AtomicUsize>,
    /// Kill handle for the child process:
    /// [`force_kill_child`](PtyChildKillControl::force_kill_child) signals the child alone,
    /// [`force_kill_process_tree`](PtyChildKillControl::force_kill_process_tree) the whole group,
    /// and the two stop requests ask them to exit on their own.
    kill_control: PtyChildKillControl,
    /// How the child ended, recorded by the watcher thread the moment it saw
    /// the child end. Read by [`kill_pane`](PortablePtyBackend::kill_pane)
    /// before it signals the leader, and by
    /// [`list_carried_panes`](PortablePtyBackend::list_carried_panes), which is
    /// how a status this process observed reaches the process image that
    /// replaces it.
    child_exit: Arc<ChildExit>,
    /// Reader thread: drains the pane's terminal to the backend's sink, and
    /// publishes the child's exit once that output has run dry, behind the last
    /// of the child's output.
    ///
    /// Not joined on teardown. The slave descriptor can outlive the child (a
    /// child that `setsid`s into a new process group); the thread exits once
    /// the descriptor closes, and it lets the consumer go on the first chunk it
    /// reads after the pane's exit is settled. Retained: the struct owns the
    /// handle.
    ///
    /// Where the terminal can be waited on, the thread comes back on
    /// `reader_waker` and ends within one round of the pane being closed,
    /// whatever still holds the slave open. That doorbell also parks it:
    /// [`pause_readers`](PortablePtyBackend::pause_readers) rings it to bring
    /// the thread to the top of its round and hold it there.
    ///
    /// On Windows it blocks in `read` until the watcher closes the pane's
    /// terminal, which flushes the console and ends the pipe this thread is
    /// reading. The close waits for the console to be read out, and the thread
    /// reads it to its end even once the consumer has let the pane go,
    /// discarding what it reads from then on. On a Unix terminal that exposes
    /// no descriptor it blocks in `read` until that terminal closes, the same way
    /// the writer can, and the watcher publishes the exit instead; the pane's
    /// end never depends on this thread getting there.
    #[expect(dead_code)]
    reader_thread: JoinHandle<()>,
    /// The reader's doorbell, the same one this pane's watcher rings.
    ///
    /// Rung by [`pause_readers`](PortablePtyBackend::pause_readers) to bring
    /// the reader to its park. `None` for a terminal that exposes no
    /// descriptor: that reader blocks in `read` and can never reach the park.
    #[cfg(unix)]
    reader_waker: Option<Arc<Waker>>,
    /// Watcher thread: blocks on the child, and records its exit status in
    /// `child_exit`.
    watcher_thread: JoinHandle<()>,
    /// Whether this pane's exit is settled — the state the reader and watcher
    /// share. [`kill_pane`](PortablePtyBackend::kill_pane) sets it before killing
    /// anything: a caller closing a pane is handed no exit for it by either
    /// thread. Under no PTY sink nothing reads it.
    exit_handover_state: Arc<Mutex<ExitHandover>>,
    /// Wakes the watcher out of the wait it is in.
    ///
    /// [`kill_pane`](PortablePtyBackend::kill_pane) sends on this before it
    /// waits for the child: tearing a pane down returns without sitting through
    /// the rounds. What stops the exit being published is
    /// `exit_handover_state.is_settled`,
    /// which `kill_pane` sets first. Two waits listen: the standby of a Unix
    /// terminal that exposes no descriptor, and a Windows watcher waiting for
    /// the reader before it closes the terminal. A Unix pane that owns its
    /// descriptor has neither, and there the send is a no-op.
    exit_wait_cancel_sender: Sender<()>,
}

/// Real OS-PTY backend: `portable-pty` on Unix, a pseudoconsole of its own on
/// Windows. Each spawned pane gets a PTY plus three helper threads (reader,
/// writer, watcher); the backend owns them all through one pane map, keyed by
/// [`PaneId`].
pub struct PortablePtyBackend {
    /// Every live pane's PTY, threads, and kill handle, keyed by [`PaneId`]. Locked:
    /// [`spawn_pane`](PtyBackend::spawn_pane), [`resize_pane`](PtyBackend::resize_pane),
    /// [`write_pane_input`](PtyBackend::write_pane_input), and [`kill_pane`](PtyBackend::kill_pane)
    /// can all be called from different dispatcher calls.
    pane_by_id: Mutex<HashMap<PaneId, PaneEntry>>,
    /// Where spawned panes deliver output and exit: each pane's reader thread
    /// hands both to it directly.
    pty_sink: Arc<dyn PtySink>,
    /// Every pane reader's park, shared by each reader thread that owns its
    /// terminal descriptor. Driven by
    /// [`pause_readers`](PortablePtyBackend::pause_readers),
    /// [`wait_until_readers_parked`](PortablePtyBackend::wait_until_readers_parked)
    /// and [`resume_readers`](PortablePtyBackend::resume_readers).
    #[cfg(unix)]
    reader_gate: Arc<ReaderGate>,
    /// The process IDs of the children that
    /// [`kill_pane`](PtyBackend::kill_pane) left running, and of those an
    /// earlier process image left running
    /// ([`watch_left_running_children`](Self::watch_left_running_children)).
    /// An ID leaves the set once its child is reaped.
    #[cfg(unix)]
    left_running_child_process_ids: Arc<Mutex<BTreeSet<u32>>>,
}

impl PortablePtyBackend {
    /// Creates a new, empty PTY backend that hands every pane's output and exit
    /// to `pty_sink` from the pane's own reader thread.
    pub fn with_pty_sink(pty_sink: Arc<dyn PtySink>) -> Self {
        PortablePtyBackend {
            pane_by_id: Mutex::new(HashMap::new()),
            pty_sink,
            #[cfg(unix)]
            reader_gate: Arc::new(ReaderGate::new()),
            #[cfg(unix)]
            left_running_child_process_ids: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    /// The process IDs of the children this backend left running and has not
    /// reaped yet, in ascending order. The process image that replaces this
    /// one hands them to its own
    /// [`watch_left_running_children`](Self::watch_left_running_children).
    #[cfg(unix)]
    pub fn list_left_running_child_process_ids(&self) -> Vec<u32> {
        lock_left_running_child_process_ids(&self.left_running_child_process_ids)
            .iter()
            .copied()
            .collect()
    }

    /// Wait on each child in `child_process_ids`, which an earlier process
    /// image left running, and reap it when it ends.
    ///
    /// Each ID joins the set
    /// [`list_left_running_child_process_ids`](Self::list_left_running_child_process_ids)
    /// lists, and a thread of its own waits on it, reaps it, and takes it out
    /// of that set. An ID that names no child of this process leaves the set
    /// at once. An ID whose thread cannot start stays in the set and is not
    /// reaped, and a warning names it.
    #[cfg(unix)]
    pub fn watch_left_running_children(&self, child_process_ids: &[u32]) {
        for &child_process_id in child_process_ids {
            lock_left_running_child_process_ids(&self.left_running_child_process_ids)
                .insert(child_process_id);
            let left_running_child_process_ids = Arc::clone(&self.left_running_child_process_ids);
            let spawn_result = thread::Builder::new()
                .name("koshi-pty-reap".to_string())
                .spawn(move || {
                    if wait_for_child_exit(child_process_id).is_zombie_kept {
                        reap_child(child_process_id);
                    }
                    lock_left_running_child_process_ids(&left_running_child_process_ids)
                        .remove(&child_process_id);
                });
            if let Err(spawn_error) = spawn_result {
                tracing::warn!(
                    child_process_id,
                    %spawn_error,
                    "a child an earlier image left running could not be watched; it is not reaped"
                );
            }
        }
    }

    /// Tell every pane's reader to park at the top of its next round, and
    /// ring each pane's doorbell to bring a reader waiting on a quiet terminal
    /// to that top. Returns without waiting:
    /// [`wait_until_readers_parked`](PortablePtyBackend::wait_until_readers_parked)
    /// waits for the parks. Nothing more is read from a terminal once every
    /// reader has parked, until the readers are resumed.
    ///
    /// Between this call and its park, each reader delivers at most one more
    /// chunk. A reader inside the consumer's [`PtySink::accept_output_bytes`]
    /// call finishes that call first.
    ///
    /// A reader parks before it waits on its terminal: a parked backend has no
    /// chunk in anyone's hands and no byte read but undelivered. Each parked
    /// reader keeps its `Delivery`, and with it the pane's exit channel, and
    /// [`resume_readers`](PortablePtyBackend::resume_readers) puts it back to
    /// work in this same process.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] naming the first pane whose terminal exposes no
    /// descriptor: that reader blocks in `read` and can never reach the park.
    /// Nothing is paused and nothing is rung in that case.
    #[cfg(unix)]
    pub fn pause_readers(&self) -> Result<(), PtyError> {
        {
            let pane_by_id = self.pane_by_id.lock().unwrap();
            for (pane_id, pane_entry) in pane_by_id.iter() {
                if pane_entry.reader_waker.is_none() {
                    let error_detail = format!(
                        "pane {pane_id} has no terminal descriptor, so its reader cannot park"
                    );
                    return Err(PtyError::Io {
                        detail: error_detail,
                    });
                }
            }
            // The flag is set before any doorbell rings: a reader brought to
            // the top of its round finds the gate paused.
            self.reader_gate.pause_readers();
            for waker in pane_by_id
                .values()
                .filter_map(|pane_entry| pane_entry.reader_waker.as_ref())
            {
                waker.wake_reader();
            }
        }
        Ok(())
    }

    /// Wait until every reader that [`pause_readers`](PortablePtyBackend::pause_readers)
    /// told to park has parked. A reader whose pump already ended is no longer
    /// counted and does not hold this up. A reader waiting inside the
    /// consumer's [`PtySink::accept_output_bytes`] call holds this up until
    /// that call returns.
    #[cfg(unix)]
    pub fn wait_until_readers_parked(&self) {
        self.reader_gate.wait_until_all_readers_parked();
    }

    /// Put every parked reader back to work. No byte is lost and no false exit
    /// is published: each reader carries on from the top of the round it parked
    /// in.
    #[cfg(unix)]
    pub fn resume_readers(&self) {
        self.reader_gate.resume_readers();
    }

    /// Wait until every pane's writer thread has written what it was handed:
    /// no byte this backend took for a child is still queued.
    ///
    /// Each pane is sent a barrier on the channel its bytes travel; an answer
    /// to that barrier means every byte queued before it is on the pane's
    /// terminal. A pane whose writer thread has already ended is passed over:
    /// its child is gone.
    ///
    /// The write-direction counterpart of `pause_readers` on Unix. A process
    /// about to replace its own image calls both: the writer threads and their
    /// queues die with the old image.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] naming the first pane whose writer did not
    /// answer within one second, which bounds the whole call. That writer is
    /// blocked inside its write — a child that stopped reading its stdin does
    /// this — and the bytes behind it are still queued.
    pub fn flush_writers(&self) -> Result<(), PtyError> {
        let writer_barriers: Vec<(PaneId, Receiver<()>)> = {
            let pane_by_id = self.pane_by_id.lock().unwrap();
            pane_by_id
                .iter()
                .filter_map(|(pane_id, pane_entry)| {
                    let (barrier_sender, barrier_receiver) = channel::<()>();
                    pane_entry
                        .writer_sender
                        .send(WriterMessage::Barrier(barrier_sender))
                        .ok()
                        .map(|()| (*pane_id, barrier_receiver))
                })
                .collect()
        };

        let flush_deadline = Instant::now() + WRITER_FLUSH_LIMIT_DURATION;
        for (pane_id, barrier_receiver) in writer_barriers {
            let remaining_duration = flush_deadline.saturating_duration_since(Instant::now());
            // A writer thread that ended between the send and here drops the
            // barrier with the rest of its queue, which reads as disconnected.
            if let Err(RecvTimeoutError::Timeout) =
                barrier_receiver.recv_timeout(remaining_duration)
            {
                let error_detail = format!(
                    "pane {pane_id} is still writing what it was handed, so it cannot settle"
                );
                return Err(PtyError::Io {
                    detail: error_detail,
                });
            }
        }
        Ok(())
    }

    /// One record per live pane: what a new process image needs to take each
    /// pane back.
    ///
    /// On Unix, a `terminal_fd` of `None` marks exactly the panes
    /// `pause_readers` refuses. A pane gets its own descriptor and its
    /// reader's doorbell together or gets neither: a caller that finds a
    /// descriptor for every pane here knows every reader can park.
    ///
    /// A pane carries an exit status only when the next process image cannot
    /// wait for its child: an earlier image reaped the child, or its end could
    /// not be waited on. A child that ended here stays a zombie, carries `None`,
    /// and the next image waits on its process ID and reads the same status.
    pub fn list_carried_panes(&self) -> Vec<CarriedPtyPane> {
        #[cfg(unix)]
        use std::os::fd::AsRawFd;

        let pane_by_id = self.pane_by_id.lock().unwrap();
        pane_by_id
            .iter()
            .map(|(pane_id, pane_entry)| CarriedPtyPane {
                pane_id: *pane_id,
                #[cfg(unix)]
                terminal_fd: match &pane_entry.terminal {
                    Terminal::Owned(terminal_fd) => Some(terminal_fd.as_raw_fd()),
                    Terminal::Master(_) => None,
                },
                process_id: pane_entry.kill_control.get_child_process_id(),
                pty_size: pane_entry.pty_size,
                exit_status: pane_entry.child_exit.get_carried_exit_status(),
            })
            .collect()
    }

    /// The process id of `pane_id`'s child, or `None` when this backend does not
    /// hold that pane.
    #[must_use]
    pub fn get_child_process_id(&self, pane_id: PaneId) -> Option<u32> {
        let pane_by_id = self.pane_by_id.lock().unwrap();
        pane_by_id
            .get(&pane_id)
            .map(|pane_entry| pane_entry.kill_control.get_child_process_id())
    }

    /// Take a pane back from a terminal descriptor and a child process id, as
    /// the process image that replaced another one does.
    ///
    /// Builds the same pane [`spawn_pane`](PtyBackend::spawn_pane) builds — the same
    /// three threads, the same PTY sink, the same kill behaviour — around a
    /// terminal and a child that are already running. `pty_size` is recorded as the
    /// pane's last size; the terminal carried that size across the swap, and
    /// nothing is written to it: the child is sent no `SIGWINCH`.
    ///
    /// `carried_exit_status` is how the child ended, as an earlier image
    /// reaped it — [`CarriedPtyPane::exit_status`]. The pane records that
    /// status before this call returns, and the watcher publishes it and waits
    /// on nothing: the image that reaped the child took the status out of the
    /// kernel with it. No zombie is kept, so [`kill_pane`](PtyBackend::kill_pane)
    /// reaps nothing and signals neither the process ID nor its group. The
    /// pane's reader still hands over whatever the terminal holds before that
    /// exit reaches the consumer.
    ///
    /// `None` means no image reaped this child, and the watcher waits on that
    /// one process ID with `waitid` and `WEXITED | WNOWAIT`, which keeps an
    /// ended child a zombie as a spawned pane's watcher does. A child that
    /// ended before this call is a zombie already, and its status reads at
    /// once. `portable-pty` resets `SIGCHLD` only inside `pre_exec`, on the
    /// child side of the fork (`portable-pty` 0.9.0, `src/unix.rs`,
    /// `spawn_command`): no parent-side reaper competes for the status. A child
    /// reaped by something outside this backend answers `ECHILD` and is
    /// reported as [`ExitStatus::ExitCode`]`(-1)`.
    ///
    /// Before → after: a pane running `sh -c 'sleep 1; exit 3'` is restarted
    /// 0.9 s in, and the child ends while the images swap → the old image's
    /// watcher keeps it a zombie and carries `None`, and this image's watcher
    /// reads `ExitCode(3)` from that zombie.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] if this platform offers no one-descriptor wake
    /// for the reader, which is what parks it, and [`PtyError::Spawn`] —
    /// `pane <id> is already open` — when this backend already drives
    /// `pane_id`. `terminal_fd` is closed on either error, and a refused
    /// take-back leaves the live pane and the child behind `process_id` untouched.
    #[cfg(unix)]
    pub fn adopt(
        &self,
        pane_id: PaneId,
        terminal_fd: std::os::fd::OwnedFd,
        process_id: u32,
        pty_size: PtySize,
        carried_exit_status: Option<ExitStatus>,
    ) -> Result<(), PtyError> {
        // Where this pane's output goes, built exactly as `spawn_pane` builds it.
        let exit_handover_state = Arc::new(Mutex::new(ExitHandover::default()));
        let (exit_wait_cancel_sender, exit_wait_cancel_receiver) = channel::<()>();
        // The terminal exposes a descriptor: the reader reaches the end of
        // the output itself, and no watcher stands by to be cancelled.
        drop(exit_wait_cancel_receiver);
        let (delivery, exit_status_sender) = build_delivery(&self.pty_sink, &exit_handover_state);

        let waker = Arc::new(Waker::new().ok_or(PtyError::Io {
            detail: "this platform offers no one-descriptor wake for a pane reader".to_string(),
        })?);
        let terminal_handle = Arc::new(terminal_fd);
        let child_exit = Arc::new(ChildExit::default());

        // Take the pane map and hold it until this pane is in it. An id the
        // backend already holds is refused here, with the map locked: the entry
        // it would replace keeps its terminal and its I/O threads, and nothing
        // below starts a thread for a pane that is already open.
        let mut pane_by_id = self.pane_by_id.lock().unwrap();
        if pane_by_id.contains_key(&pane_id) {
            return Err(PtyError::Spawn {
                detail: format!("pane {pane_id} is already open"),
            });
        }

        let reader_thread = start_owned_reader(
            delivery,
            pane_id,
            Arc::clone(&terminal_handle),
            Arc::clone(&waker),
            Arc::clone(&child_exit),
            self.reader_gate.register_reader(),
        );
        let queued_input_byte_count = Arc::new(AtomicUsize::new(0));
        let writer_sender = start_writer(
            WriteSide::Owned(Arc::clone(&terminal_handle)),
            Arc::clone(&queued_input_byte_count),
        );

        // A carried status is recorded on the pane now; the watcher publishes
        // it, or waits for the process id itself when no image reaped the
        // child. Everything after that is what a spawned pane's watcher does.
        let carried_observed_exit = carried_exit_status.map(|exit_status| ObservedChildExit {
            exit_status,
            is_zombie_kept: false,
        });
        if let Some(observed_exit) = carried_observed_exit {
            child_exit.publish_observed_exit(observed_exit);
        }
        let watch_release = WatchRelease {
            child_exit: Arc::clone(&child_exit),
            exit_status_sender,
            writer_stop_sender: writer_sender.clone(),
            reader_waker: Some(Arc::clone(&waker)),
            child_process_id: process_id,
            left_running_child_process_ids: Arc::clone(&self.left_running_child_process_ids),
        };
        let watcher_thread = spawn_pty_thread("koshi-pty-watch", move || {
            let observed_exit =
                carried_observed_exit.unwrap_or_else(|| wait_for_child_exit(process_id));
            watch_release.publish_exit_status(observed_exit);
        });

        pane_by_id.insert(
            pane_id,
            PaneEntry {
                terminal: Terminal::Owned(terminal_handle),
                pty_size,
                writer_sender,
                queued_input_byte_count,
                kill_control: PtyChildKillControl::from_process_id(process_id),
                child_exit,
                exit_handover_state,
                exit_wait_cancel_sender,
                reader_thread,
                reader_waker: Some(waker),
                watcher_thread,
            },
        );
        drop(pane_by_id);
        Ok(())
    }
}

impl PtyBackend for PortablePtyBackend {
    /// Open a fresh PTY, launch `spawn_spec` as a child inside it, and wire up its I/O.
    ///
    /// The child runs detached on three background threads owned by the
    /// backend: a **reader** (master output → wherever this backend delivers),
    /// a **writer** (input channel → master; writes never block the
    /// dispatcher), and a **watcher** (waits for the child to end → records
    /// how it ended on the pane, sends it on the exit channel, and releases the
    /// writer). On Unix the ended child stays a zombie until `kill_pane` reaps
    /// it.
    ///
    /// The reader hands each chunk to the backend's PTY sink and publishes the
    /// exit itself once the output runs out. On Windows the watcher closes the
    /// pane's terminal once the child has ended, which brings the reader to
    /// that end; on a Unix terminal that exposes no descriptor to wait on the
    /// watcher publishes the exit itself, once output stops arriving. A pane
    /// always learns its child ended. The reader then stops on its next chunk:
    /// a descendant still printing into a closed pane's terminal is not
    /// forwarded.
    ///
    /// # Errors
    /// Returns [`PtyError::Spawn`] if the PTY can't be opened, the command can't
    /// be launched, the master's reader/writer can't be taken, or the backend
    /// already holds `pane_id` — `pane <id> is already open`. A refused
    /// respawn kills the child it launched and leaves the live pane untouched.
    fn spawn_pane(
        &self,
        pane_id: PaneId,
        spawn_spec: SpawnSpec,
        pty_size: PtySize,
    ) -> Result<(), PtyError> {
        // 1. Build the reader's delivery in
        //    `build_delivery`. The reader publishes the status once the
        //    child's output has run out: a consumer never sees the child end
        //    while output is still coming.
        //
        //    The watcher takes a second reference to the sink for the paths
        //    where it publishes the exit itself: a Unix terminal that exposes
        //    no descriptor to wait on, and a Windows pane. `exit_handover_state` carries
        //    both facts under one lock — whether the pane's exit is settled,
        //    and how much the reader has handed over — and the watcher never
        //    sees half a transition.
        let exit_handover_state = Arc::new(Mutex::new(ExitHandover::default()));
        // The watcher's one interruptible wait: a Unix terminal with no
        // descriptor stands by on it, and a Windows pane waits on it for its
        // reader before closing the terminal. `kill_pane` sends on it.
        let (exit_wait_cancel_sender, exit_wait_cancel_receiver) = channel::<()>();
        let (delivery, exit_status_sender) = build_delivery(&self.pty_sink, &exit_handover_state);
        // 2 to 5. Start the child on a terminal of its own and build its kill
        //    control. On Windows one call opens the pane's pseudoconsole and
        //    starts the program inside the pane's jobs, and the kill control
        //    shares the program's process handle.
        #[cfg(unix)]
        let (pty_master, child_guard, kill_control, process_id) = {
            // 2. Open the PTY pair sized to the pane. The pair is two linked
            //    ends: `master` stays with us, `slave` becomes the child's
            //    terminal.
            let pty_pair = native_pty_system()
                .openpty(build_portable_pty_size(pty_size))
                .map_err(|io_error| PtyError::Spawn {
                    detail: io_error.to_string(),
                })?;

            // 3. Build the launch command from the spawn specification
            //    (program, arguments, working directory, environment
            //    variables)...
            let mut command_builder = CommandBuilder::new(spawn_spec.program.as_os_str());
            command_builder.args(&spawn_spec.arguments);
            // Resolve the working directory before launch: an explicit path
            // wins; an absent path inherits koshi's process cwd, matching
            // `SpawnSpec`'s contract.
            match &spawn_spec.working_directory {
                Some(working_directory) => command_builder.cwd(working_directory),
                None => {
                    if let Ok(working_directory) = std::env::current_dir() {
                        command_builder.cwd(working_directory);
                    }
                }
            }

            //    ...including the environment. `CommandBuilder` is never
            //    cleared: the child starts from the full parent env, kept as
            //    `OsString`, and non-UTF-8 vars survive intact.
            //    `build_environment_overlay` returns only koshi's overlay
            //    (terminal identity + shell bootstrap + the spawn
            //    specification's environment variables), and applying each key
            //    with `command_builder.env` replaces the value that the child
            //    gets without it.
            for (environment_variable_name, environment_variable_value) in
                build_environment_overlay(&spawn_spec)
            {
                command_builder.env(environment_variable_name, environment_variable_value);
            }

            //    ...and launch it on the slave end. The child now owns the
            //    slave as its stdin/stdout/stderr; we keep `child_guard` only
            //    to wait on / kill it. A `portable-pty` child is not
            //    terminated by being dropped; `ChildGuard` kills the child
            //    when any step below returns early.
            let child_guard =
                ChildGuard::from_child(pty_pair.slave.spawn_command(command_builder).map_err(
                    |spawn_error| PtyError::Spawn {
                        detail: spawn_error.to_string(),
                    },
                )?);

            let process_id = child_guard.process_id().ok_or(PtyError::Spawn {
                detail: "child has no PID".to_string(),
            })?;

            // 4. Build the kill control. The child leads its own process
            //    group from its start (`setsid`), and a process it starts stays
            //    in that group until it moves to another.
            let kill_control = PtyChildKillControl::from_process_id(process_id);

            // 5. Drop OUR copy of the slave. The child kept its own; once the
            //    child exits and the kernel closes its end, the terminal
            //    reports EOF, and the reader thread (step 9) stops.
            drop(pty_pair.slave);
            (pty_pair.master, child_guard, kill_control, process_id)
        };
        #[cfg(windows)]
        let (pty_master, child_guard, kill_control, output_reader, input_writer) = {
            let pane_job = PaneJob::create()?;
            let SpawnedPaneProgram {
                pane_console,
                output_reader,
                input_writer,
                pane_process,
            } = spawn_pane_program(&spawn_spec, pty_size, &pane_job)?;
            let kill_control = PtyChildKillControl::from_pane_job(
                pane_process.get_process_id(),
                pane_process.share_process_handle(),
                pane_job,
            );
            (
                pane_console,
                ChildGuard::from_child(pane_process),
                kill_control,
                output_reader,
                input_writer,
            )
        };

        // 6. Decide how this pane reaches its terminal, and pull the exit flag.
        //
        //    A terminal that exposes a descriptor is opened once, here, and
        //    that one descriptor serves the whole pane: its reader waits on it
        //    and reads it, its writer writes it, and `resize_pane` retunes it. The
        //    reader also gets a [`Waker`] — one more descriptor — which the
        //    watcher rings to bring it back from a wait a descendant holding
        //    the terminal keeps open. Two descriptors per pane in total.
        //
        //    The copy is taken here, where the master is plainly alive, and
        //    owned from then on: a pane torn down while its threads still run
        //    closes its own copy and never theirs. `portable-pty`'s master is
        //    left to drop at the end of this call, which closes the descriptor
        //    it was holding.
        //
        //    Windows exposes none of this: there the pane keeps the
        //    pseudoconsole's two pipe ends and the pseudoconsole itself, and
        //    its reader blocks in `read`.
        #[cfg(unix)]
        let owned_terminal_parts = duplicate_terminal_file_descriptor(&*pty_master)
            .zip(Waker::new())
            .map(|(terminal_fd, reader_waker)| (Arc::new(terminal_fd), Arc::new(reader_waker)));

        // The doorbell's other two holders: the reader waits on it, the watcher
        // rings it once the child has ended, and the pane entry rings it to
        // park the reader.
        #[cfg(unix)]
        let watcher_reader_waker = owned_terminal_parts
            .as_ref()
            .map(|(_, reader_waker)| Arc::clone(reader_waker));
        #[cfg(unix)]
        let entry_reader_waker = owned_terminal_parts
            .as_ref()
            .map(|(_, reader_waker)| Arc::clone(reader_waker));

        //    The watcher's tail — what it does once the child has ended —
        //    follows from the same choice: a reader that can be waited on
        //    publishes the exit itself, a Unix terminal with no descriptor
        //    leaves the watcher to publish, and Windows closes the terminal and
        //    then stands by behind the reader.
        #[cfg(unix)]
        let (terminal_read_side, terminal_write_side, terminal_resource, watcher_tail) =
            match owned_terminal_parts {
                Some((terminal_fd, reader_waker)) => (
                    ReadSide::Owned(Arc::clone(&terminal_fd), reader_waker),
                    WriteSide::Owned(Arc::clone(&terminal_fd)),
                    Terminal::Owned(terminal_fd),
                    WatcherTail::ReaderPublishes,
                ),
                None => {
                    let terminal_reader =
                        pty_master
                            .try_clone_reader()
                            .map_err(|io_error| PtyError::Spawn {
                                detail: io_error.to_string(),
                            })?;
                    let pty_writer =
                        pty_master
                            .take_writer()
                            .map_err(|io_error| PtyError::Spawn {
                                detail: io_error.to_string(),
                            })?;
                    (
                        ReadSide::Blocking(terminal_reader),
                        WriteSide::Blocking(pty_writer),
                        Terminal::Master(Arc::new(Mutex::new(Some(pty_master)))),
                        WatcherTail::WaitForReaderThenPublish(Arc::downgrade(&self.pty_sink)),
                    )
                }
            };
        // The pseudoconsole goes in a slot the watcher shares: the watcher takes
        // it out and drops it once the child has ended, which closes the
        // console and ends the reader's pipe.
        #[cfg(windows)]
        let (terminal_read_side, terminal_write_side, terminal_resource, watcher_tail) = {
            let pty_master_slot = Arc::new(Mutex::new(Some(pty_master)));
            (
                ReadSide::Blocking(Box::new(output_reader)),
                WriteSide::Blocking(Box::new(input_writer)),
                Terminal::Master(Arc::clone(&pty_master_slot)),
                WatcherTail::CloseTerminal {
                    pty_master_slot,
                    pty_sink: Arc::downgrade(&self.pty_sink),
                },
            )
        };

        let child_exit = Arc::new(ChildExit::default());

        // 7. Take the pane map now and hold it until this pane is in it. A
        //    short-lived child can end and its exit be handed to the
        //    consumer before this call returns; a `kill_pane` from inside that call
        //    blocks here until the insert lands, and then finds the pane. None
        //    of the threads started below touch this map.
        //
        //    An id the backend already holds is refused here, with the map
        //    locked. The entry it would replace keeps its terminal and its I/O
        //    threads, and the guard is still armed, so it kills the child
        //    launched above. The child ends first, then the pane's two ends of
        //    the terminal close, then the terminal itself.
        let mut pane_by_id = self.pane_by_id.lock().unwrap();
        if pane_by_id.contains_key(&pane_id) {
            drop(child_guard);
            drop(terminal_read_side);
            drop(terminal_write_side);
            drop(terminal_resource);
            drop(watcher_tail);
            return Err(PtyError::Spawn {
                detail: format!("pane {pane_id} is already open"),
            });
        }

        // That refusal is the last step that returns early: the watcher thread
        // below owns the child and waits for its end. Disarm the guard.
        let child_process = child_guard.release_child();

        // 8. Writer thread: drain the input channel onto the terminal. A write
        //    to a child that has stopped reading blocks only that thread,
        //    never the dispatcher. Started before the reader and before the
        //    watcher, which queues its `Stop` on this channel.
        let queued_input_byte_count = Arc::new(AtomicUsize::new(0));
        let writer_sender = start_writer(terminal_write_side, Arc::clone(&queued_input_byte_count));

        //    A Windows pseudoconsole asks where the cursor is as it opens and
        //    holds its child's output until that is answered. The answer is
        //    queued here, ahead of the pane being reachable by
        //    [`write_pane_input`](PortablePtyBackend::write_pane_input): nothing a user types
        //    reaches the terminal before it.
        #[cfg(windows)]
        let _ = queue_pane_input(
            pane_id,
            &writer_sender,
            &queued_input_byte_count,
            CURSOR_POSITION_AT_HOME_RESPONSE_BYTES,
        );

        // 9. Reader thread: wait on the terminal, handing each chunk of
        //    child output to `delivery` until EOF (child gone) or the consumer
        //    goes away. Once the output has run out a sink can be told the
        //    child ended; already-settled panes return from
        //    `publish_exit_status` without waiting.
        //
        //    A reader that owns its terminal descriptor is counted into the
        //    gate here, before its thread starts: a pause landing first still
        //    waits for it to reach the park. A reader that blocks in `read`
        //    can never park and is never counted.
        //
        //    On Windows this reader takes the terminal's opening
        //    cursor-position request out of the output it hands over.
        let reader_thread = match terminal_read_side {
            #[cfg(unix)]
            ReadSide::Owned(terminal_fd, reader_waker) => start_owned_reader(
                delivery,
                pane_id,
                terminal_fd,
                reader_waker,
                Arc::clone(&child_exit),
                self.reader_gate.register_reader(),
            ),
            ReadSide::Blocking(terminal_reader) => spawn_pty_thread("koshi-pty-read", move || {
                #[cfg(windows)]
                let mut terminal_reader = RemovesCursorRequest::from_inner_reader(terminal_reader);
                #[cfg(not(windows))]
                let mut terminal_reader = terminal_reader;
                if pump_blocking(&mut terminal_reader, &delivery, pane_id) {
                    delivery.publish_exit_status(pane_id);
                } else {
                    // The consumer has let the pane go: release it now.
                    // Nothing about this pane reaches it again, and it is
                    // not held for the read below.
                    drop(delivery);
                    // Closing the pane's console waits for the output it
                    // still holds to be read out, and this thread is the
                    // pane's one reader: it reads the console to its end,
                    // discarding, before it stops.
                    #[cfg(windows)]
                    drain_terminal(&mut terminal_reader);
                }
            }),
        };

        // 10. Watcher thread: wait for the child to end — on Unix with
        //    `wait_for_child_exit`, which leaves it a zombie that `kill_pane`
        //    reaps, and on Windows with `PaneProcess::wait_for_exit_code` —
        //    map the OS exit status into koshi's `ExitStatus`, then release
        //    the pane's other threads: record the status in `child_exit` (read
        //    by `kill_pane` before it signals the leader), publish it on the
        //    exit channel, stop the writer, and ring the reader's doorbell: the
        //    reader takes the last of the output.
        //
        //    What it does after that is its tail, decided in step 6: bring the
        //    reader to the end of the terminal, or stand by and publish the
        //    exit itself. On Windows the tail waits for the reader to be back
        //    on the terminal and then closes the pane's console, which that
        //    reader is there to read out. `kill_pane` wakes both waits: closing a
        //    pane returns without sitting through either.
        let watch_release = WatchRelease {
            child_exit: Arc::clone(&child_exit),
            exit_status_sender,
            writer_stop_sender: writer_sender.clone(),
            #[cfg(unix)]
            reader_waker: watcher_reader_waker,
            #[cfg(unix)]
            child_process_id: process_id,
            #[cfg(unix)]
            left_running_child_process_ids: Arc::clone(&self.left_running_child_process_ids),
        };
        let watcher_exit_handover_state = Arc::clone(&exit_handover_state);
        let watcher_thread = spawn_pty_thread("koshi-pty-watch", move || {
            #[cfg(unix)]
            let observed_exit = {
                drop(child_process);
                wait_for_child_exit(process_id)
            };
            #[cfg(windows)]
            let observed_exit = {
                let exit_status = match child_process.wait_for_exit_code() {
                    Ok(exit_code) => ExitStatus::ExitCode(exit_code as i32),
                    Err(_) => UNOBSERVED_EXIT,
                };
                ObservedChildExit { exit_status }
            };
            watch_release.publish_exit_status(observed_exit);

            match watcher_tail {
                // The reader reaches the end of the terminal itself and
                // publishes the exit behind the last of the output.
                #[cfg(not(windows))]
                WatcherTail::ReaderPublishes => {}
                // Closing the console flushes what it still holds and then ends
                // the reader's pipe. The close returns once the pane's reader
                // has taken that flush and every process attached to the
                // console has let it go: it waits for the reader first, then
                // runs on its own thread, and this thread stands by behind it.
                #[cfg(windows)]
                WatcherTail::CloseTerminal {
                    pty_master_slot,
                    pty_sink,
                } => {
                    wait_for_reader_to_read_again(
                        &exit_wait_cancel_receiver,
                        &watcher_exit_handover_state,
                        READER_CHECK_IN_DURATION,
                    );
                    let pty_master = pty_master_slot.lock().expect("terminal").take();
                    spawn_pty_thread("koshi-pty-close", move || drop(pty_master));
                    wait_for_reader_before_publishing(
                        &exit_wait_cancel_receiver,
                        &watcher_exit_handover_state,
                        &pty_sink,
                        pane_id,
                        observed_exit.exit_status,
                    );
                }
                #[cfg(not(windows))]
                WatcherTail::WaitForReaderThenPublish(pty_sink) => {
                    wait_for_reader_before_publishing(
                        &exit_wait_cancel_receiver,
                        &watcher_exit_handover_state,
                        &pty_sink,
                        pane_id,
                        observed_exit.exit_status,
                    );
                }
            }
        });

        // 11. Retain the terminal, writer, killer, exit record and both thread
        //    handles under the pane id, then hand the caller its handle.
        pane_by_id.insert(
            pane_id,
            PaneEntry {
                terminal: terminal_resource,
                pty_size,
                writer_sender,
                queued_input_byte_count,
                kill_control,
                child_exit,
                exit_handover_state,
                exit_wait_cancel_sender,
                reader_thread,
                #[cfg(unix)]
                reader_waker: entry_reader_waker,
                watcher_thread,
            },
        );
        drop(pane_by_id);
        Ok(())
    }
    fn resize_pane(&self, pane_id: PaneId, pty_size: PtySize) -> Result<(), PtyError> {
        let mut pane_by_id = self.pane_by_id.lock().unwrap();
        let Some(pane_entry) = pane_by_id.get_mut(&pane_id) else {
            return Err(PtyError::UnknownPane { pane_id });
        };
        pane_entry.terminal.resize(pty_size)?;
        // Recorded only once the kernel took it: a carried pane names the
        // size its child was told.
        pane_entry.pty_size = pty_size;
        Ok(())
    }
    fn write_pane_input(&self, pane_id: PaneId, input_bytes: &[u8]) -> Result<(), PtyError> {
        let pane_by_id = self.pane_by_id.lock().unwrap();
        let Some(pane_entry) = pane_by_id.get(&pane_id) else {
            return Err(PtyError::UnknownPane { pane_id });
        };
        queue_pane_input(
            pane_id,
            &pane_entry.writer_sender,
            &pane_entry.queued_input_byte_count,
            input_bytes,
        )
    }
    fn kill_pane(&self, pane_id: PaneId, kill_policy: KillPolicy) -> Result<(), PtyError> {
        let pane_entry = self
            .pane_by_id
            .lock()
            .unwrap()
            .remove(&pane_id)
            .ok_or(PtyError::UnknownPane { pane_id })?;

        // Settle the pane's exit before anything dies: neither helper thread
        // publishes an exit for a settled pane, and the reader stops on its
        // next chunk, which stops forwarding a descendant still printing into
        // the terminal.
        pane_entry
            .exit_handover_state
            .lock()
            .expect("handover")
            .is_settled = true;

        // `Force`/`Graceful` signal the leader PID and are skipped once the
        // watcher has seen it end. `Tree` and `GracefulTree`'s closing
        // group-kill signal the whole group/job (`killpg` /
        // `TerminateJobObject`) also after the leader ended: the leader can
        // exit while a same-group descendant keeps running, and the group-kill
        // still ends it. `child_exit` records only the leader, not whether the
        // group is empty. On Unix a leader this process saw end stays a zombie
        // until this call reaps it at its end, so its PID and its process group
        // ID still name this pane's processes for every signal below. A leader
        // an earlier image reaped reserves neither, and its group is not
        // signalled. Windows names the group by its job handle.
        #[cfg(unix)]
        let is_process_group_reserved = pane_entry.child_exit.is_process_id_reserved();
        #[cfg(windows)]
        let is_process_group_reserved = true;
        match kill_policy {
            KillPolicy::Force => {
                if !pane_entry.child_exit.has_exited() {
                    let _ = pane_entry.kill_control.force_kill_child();
                }
            }
            KillPolicy::Tree => {
                if is_process_group_reserved {
                    let _ = pane_entry.kill_control.force_kill_process_tree();
                }
            }
            KillPolicy::Graceful { timeout_duration } => {
                if !pane_entry.child_exit.has_exited() {
                    // Ask the leader to exit and give it the grace window; SIGKILL when the
                    // window runs out, or at once when the request never reached it.
                    if !is_child_stopped_within_grace(
                        pane_entry.kill_control.request_child_stop(),
                        &pane_entry.child_exit,
                        timeout_duration,
                    ) {
                        let _ = pane_entry.kill_control.force_kill_child();
                    }
                }
            }
            KillPolicy::GracefulTree { timeout_duration } => {
                if !pane_entry.child_exit.has_exited() {
                    // Ask the whole group to exit — every member gets the stop request and
                    // the grace window — then wait for the leader. The wait is skipped
                    // only when no member received the request.
                    is_child_stopped_within_grace(
                        pane_entry.kill_control.request_process_tree_stop(),
                        &pane_entry.child_exit,
                        timeout_duration,
                    );
                }

                // Group-kill even when the leader already exited: a disowned
                // descendant can keep the group alive past its leader, and
                // `killpg`/`TerminateJobObject` ends it with the rest.
                if is_process_group_reserved {
                    let _ = pane_entry.kill_control.force_kill_process_tree();
                }
            }
        }

        let PaneEntry {
            terminal,
            writer_sender,
            kill_control,
            child_exit,
            exit_wait_cancel_sender,
            #[cfg(unix)]
            reader_waker,
            watcher_thread,
            ..
        } = pane_entry;

        // Let go of the terminal: the writer stops at the `Stop` once the
        // input queued before it is written, the reader leaves at its
        // doorbell, and this call drops its own hold. On Unix the last
        // descriptor to close hangs the terminal up: the kernel sends SIGHUP
        // to the leader and to the foreground process group, whatever user
        // they run as.
        let _ = writer_sender.send(WriterMessage::Stop);
        drop(writer_sender);
        #[cfg(unix)]
        if let Some(reader_waker) = &reader_waker {
            reader_waker.wake_reader();
        }
        drop(terminal);

        // Wake the watcher out of the wait it is in: closing a pane returns
        // without sitting through the rounds. Two waits listen: the standby of
        // a Unix terminal with no descriptor, and a Windows watcher's wait for
        // the reader before it closes the terminal.
        let _ = exit_wait_cancel_sender.send(());

        // A leader still running `GRACEFUL_TIMEOUT_DURATION` after the hangup
        // is left running, and joins `left_running_child_process_ids` until
        // its watcher reaps it.
        if !child_exit.wait_for_exit(GRACEFUL_TIMEOUT_DURATION) {
            let child_process_id = kill_control.get_child_process_id();
            #[cfg(unix)]
            lock_left_running_child_process_ids(&self.left_running_child_process_ids)
                .insert(child_process_id);
            if child_exit.leave_child_running() {
                tracing::warn!(
                    ?pane_id,
                    child_process_id,
                    "the closed pane's program still runs: it outlived every signal and the hangup, and is left running"
                );
                return Ok(());
            }
            #[cfg(unix)]
            lock_left_running_child_process_ids(&self.left_running_child_process_ids)
                .remove(&child_process_id);
        }

        // Joined unless this *is* the watcher: a consumer handed an exit by the
        // watcher may close the pane from inside that call, and a thread that
        // joins itself never returns. The watcher has nothing left to do after
        // handing the exit over.
        if watcher_thread.thread().id() != thread::current().id() {
            let _ = watcher_thread.join();
        }
        // The watcher has seen the leader end. On Unix a zombie it kept is
        // reaped here.
        #[cfg(unix)]
        if child_exit.is_zombie_kept() {
            reap_child(kill_control.get_child_process_id());
        }

        Ok(())
    }
    fn find_live_working_directory(&self, pane_id: PaneId) -> Option<std::path::PathBuf> {
        let pane_by_id = self.pane_by_id.lock().unwrap();
        let pane_entry = pane_by_id.get(&pane_id)?;
        // A leader that ended has no live working directory.
        if pane_entry.child_exit.has_exited() {
            return None;
        }
        crate::working_directory::get_process_working_directory(
            pane_entry.kill_control.get_child_process_id(),
        )
    }
}

/// Whether the leader is gone after being asked to stop.
///
/// Waits on `child_exit` for up to `grace_duration` when anything received the
/// stop request, including a group where only part of it did, and returns
/// whether the leader ended in that window. Returns `false` at once, spending
/// no grace window, when nothing received it.
fn is_child_stopped_within_grace(
    stop_request: StopRequest,
    child_exit: &ChildExit,
    grace_duration: Duration,
) -> bool {
    match stop_request {
        StopRequest::Delivered | StopRequest::Unknown => child_exit.wait_for_exit(grace_duration),
        StopRequest::NotDelivered => false,
    }
}

/// Convert koshi's [`PtySize`] into `portable-pty`'s own size type, zeroing
/// the pixel dimensions `portable-pty` accepts but this crate does not track.
#[cfg(unix)]
fn build_portable_pty_size(pty_size: PtySize) -> portable_pty::PtySize {
    portable_pty::PtySize {
        rows: pty_size.row_count,
        cols: pty_size.column_count,
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[cfg(test)]
mod tests;
