//! Real OS-PTY backend built on the `portable-pty` crate.
//!
//! A spawned pane gets a kernel PTY and three helper threads (reader, writer,
//! watcher), all owned through the [`crate::portable::PortablePtyBackend`]
//! pane map. The backend streams child output, queues input, terminates the
//! child under cross-platform kill policies, and tracks the exit status.
//!
//! Three threads is the whole per-pane cost: the reader delivers output to the
//! consumer itself, through [`crate::backend::state::PtySink`].
//!
//! One thread publishes a pane's exit. Which one it is follows from how the
//! pane's terminal reaches the end of its output:
//!
//! - A Unix terminal the pane owns a descriptor for: the reader publishes. The
//!   watcher wakes it once the child is reaped, and it hands over whatever the
//!   terminal still holds before publishing.
//! - Windows: the reader publishes, and the watcher stands by behind it. Once
//!   the child is reaped the watcher waits for the reader to be back on the
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
//! `portable-pty` opens every Windows pane's terminal with
//! `CreatePseudoConsole`. It passes flags outside the documented set (`0` and
//! `PSEUDOCONSOLE_INHERIT_CURSOR`), and it closes the two pipe handles it
//! handed to the call before `CreateProcess` runs; Microsoft's reference
//! closes them after, and states that handle lifetimes managed wrongly can
//! deadlock a synchronous read or write.
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
        atomic::{AtomicBool, Ordering},
        mpsc::{channel, Receiver, RecvTimeoutError, Sender},
        Arc, Mutex, OnceLock, Weak,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use koshi_core::{
    ids::PaneId,
    process::{ExitStatus, KillPolicy, PtySize, SpawnSpec},
};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty};

/// Only the reader park waits on a condition, and only Unix panes park.
#[cfg(unix)]
use std::sync::Condvar;

use crate::{
    backend::state::{CarriedPtyPane, PtyBackend, PtyHandle, PtySink, UNOBSERVED_EXIT},
    env::build_environment_overlay,
    error::PtyError,
    kill::{PtyChildKillControl, StopRequest},
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
/// the child's end.
///
/// A caller polling [`PtyHandle`] gets `Channel`; a caller that implements
/// [`PtySink`] gets `Sink`, which needs no relay thread.
enum Delivery {
    /// Push each chunk onto the handle's output channel.
    Channel(Sender<Vec<u8>>),
    /// Hand each chunk to the consumer directly. `exit_receiver` is the
    /// watcher's end of a private channel, read once the output is exhausted.
    Sink {
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
    },
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
    ///
    /// Counts PTY sink deliveries only. A channel consumer registers nothing here;
    /// a channel-backed pane always reads as idle.
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
        // `pty_master` is borrowed for this call and owns the descriptor throughout.
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
    let descriptor_flags = unsafe { libc::fcntl(terminal_file_descriptor, libc::F_GETFD) };
    if descriptor_flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let desired_descriptor_flags = if should_close_on_exec {
        descriptor_flags | libc::FD_CLOEXEC
    } else {
        descriptor_flags & !libc::FD_CLOEXEC
    };
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
    // The call writes the name and its terminating zero into the buffer.
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
/// A doorbell. A ring means *something about this pane changed*: its child was
/// reaped, or the backend is parking its readers. The reader reads which of
/// those it was from the pane's own state, never from the doorbell.
///
/// A ring stays pending until the reader drains it: one that lands before the
/// reader reaches its wait is still there when it does.
/// [`drain`](Waker::drain) takes it back off, and the next wait blocks again.
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

    /// A waker nothing has woken yet: a kernel event queue carrying one
    /// user-triggered event, which [`wake_reader`](Waker::wake_reader) fires. The event is
    /// registered with `EV_CLEAR`: fetching it resets it, and
    /// [`drain_wake_signal`](Waker::drain_wake_signal) reads the queue as quiet again. A child process
    /// does not inherit the queue.
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
/// The doorbell says only that something changed; `child_exited` and
/// `reader_gate` are the two things it can have been.
#[cfg(unix)]
struct ReaderSignals<'a> {
    /// The doorbell, waited on beside the pane's terminal.
    reader_waker: &'a Waker,
    /// Set by the watcher before it rings: the child has been reaped.
    child_exited: &'a AtomicBool,
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

    /// Park here while the gate is paused. Returns at once when it is not: an
    /// unpaused round costs one uncontended lock.
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
impl ReaderTicket {
    /// The gate this place is in, which the reader parks at.
    fn get_reader_gate(&self) -> &ReaderGate {
        &self.reader_gate
    }
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
    /// Deliver one chunk of `pane`'s output. `false` means the reader stops
    /// delivering this pane: its exit is already settled and the consumer has
    /// let it go, or the consumer refused the chunk. The reader lets the
    /// consumer go there; on Windows it stays in `read` afterwards, discarding,
    /// until the pane's console is closed. Closing that console waits for its
    /// output to be read out.
    ///
    /// Takes the chunk borrowed and copies it after claiming it. A settled pane
    /// copies nothing.
    fn deliver_output(&self, pane_id: PaneId, output_bytes: &[u8]) -> bool {
        match self {
            Delivery::Channel(output_sender) => output_sender.send(output_bytes.to_vec()).is_ok(),
            Delivery::Sink {
                pty_sink,
                exit_handover_state,
                ..
            } => {
                // Checked and claimed under one lock: a reader holding a chunk
                // never reads as idle to the watcher.
                {
                    let mut exit_handover_state = exit_handover_state.lock().expect("handover");
                    if exit_handover_state.is_settled {
                        return false;
                    }
                    exit_handover_state.started_chunk_count += 1;
                }
                // The consumer is called with the lock released: it runs for
                // as long as it likes, and the watcher can still read the
                // chunk as in flight throughout.
                let was_output_accepted =
                    pty_sink.accept_output_bytes(pane_id, output_bytes.to_vec());
                let mut exit_handover_state = exit_handover_state.lock().expect("handover");
                exit_handover_state.finished_chunk_count += 1;
                // A consumer that refuses a chunk is done with this pane and
                // is handed no exit afterwards. Settled in the same step that
                // releases the chunk.
                if !was_output_accepted {
                    exit_handover_state.is_settled = true;
                }
                was_output_accepted
            }
        }
    }

    /// Whether this pane's exit is already settled. A reader that finds it
    /// settled stops without taking another chunk. Always `false` under a
    /// channel consumer, which settles nothing.
    ///
    /// Read by [`pump_waited`], the one reader that is brought back from its
    /// wait to ask.
    #[cfg(unix)]
    fn is_settled(&self) -> bool {
        match self {
            Delivery::Channel(_) => false,
            Delivery::Sink {
                exit_handover_state,
                ..
            } => exit_handover_state.lock().expect("handover").is_settled,
        }
    }

    /// Report `pane_id`'s child as ended, once its output is exhausted. A PTY sink
    /// waits here for the watcher's status. Under a channel consumer this does
    /// nothing: the exit is read off the handle.
    ///
    /// Returns straight away when this pane's exit is already settled: the
    /// watcher delivered it, the consumer refused a chunk, or
    /// [`kill_pane`](PortablePtyBackend::kill_pane) closed the pane. Returns without
    /// publishing when the watcher's sender is gone. The consumer is told at
    /// most once, and a child that outlives its consumer does not pin the
    /// reader thread.
    fn publish_exit_status(self, pane_id: PaneId) {
        let Delivery::Sink {
            pty_sink,
            exit_receiver,
            exit_handover_state,
        } = self
        else {
            return;
        };
        if exit_handover_state.lock().expect("handover").is_settled {
            return;
        }
        let Ok(exit_status) = exit_receiver.recv() else {
            return;
        };
        {
            let mut exit_handover_state = exit_handover_state.lock().expect("handover");
            if exit_handover_state.is_settled {
                return;
            }
            exit_handover_state.is_settled = true;
        }
        pty_sink.accept_exit_status(pane_id, exit_status);
    }
}

/// Build one pane's caller handle and its reader's delivery, plus the watcher's
/// own reference to the sink.
///
/// With a `pty_sink` the handle carries no channels, the reader hands each chunk
/// to that consumer, and the delivery holds the receiving end the reader takes
/// the watcher's status from. Without one the handle keeps the receiving ends of
/// both channels and the reader pushes each chunk onto the output sender.
///
/// The returned sender is the watcher's in both cases. The fourth value is
/// `Some(pty_sink)` only in the first case, and it is what the watcher publishes the
/// exit through on the paths where the reader cannot.
fn build_delivery(
    pty_sink: Option<Arc<dyn PtySink>>,
    pane_id: PaneId,
    exit_handover_state: &Arc<Mutex<ExitHandover>>,
) -> (
    PtyHandle,
    Delivery,
    Sender<ExitStatus>,
    Option<Arc<dyn PtySink>>,
) {
    let Some(pty_sink) = pty_sink else {
        let (pty_handle, output_sender, exit_sender) = PtyHandle::from_pane_id(pane_id);
        return (
            pty_handle,
            Delivery::Channel(output_sender),
            exit_sender,
            None,
        );
    };
    let (exit_sender, exit_receiver) = channel::<ExitStatus>();
    (
        PtyHandle::from_detached_pane_id(pane_id),
        Delivery::Sink {
            pty_sink: Arc::clone(&pty_sink),
            exit_receiver,
            exit_handover_state: Arc::clone(exit_handover_state),
        },
        exit_sender,
        Some(pty_sink),
    )
}

/// Hand every chunk of `pane`'s output to `delivery`, blocking in `read` until
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
const CURSOR_POSITION_AT_HOME_RESPONSE_BYTES: &[u8] = b"\x1b[1;1R";

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

/// Where `needle` starts in `haystack`.
#[cfg(any(windows, test))]
fn find_subslice_position(source_bytes: &[u8], pattern_bytes: &[u8]) -> Option<usize> {
    source_bytes
        .windows(pattern_bytes.len())
        .position(|window| window == pattern_bytes)
}

/// How many bytes at the end of `haystack` are a prefix of `needle`. `0` when
/// none are. Never counts `needle` whole: at most `needle.len() - 1`.
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
/// The watcher rings the doorbell once it has reaped the child, and the ring
/// reaches a reader that a descendant holding the terminal open keeps waiting.
/// The ring stays pending until this pump drains it: one that lands before the
/// reader reaches its wait is still there when it does.
///
/// The doorbell only says something changed; the pump reads
/// `reader_signals.child_exited`
/// to see what. The grace rounds start when that flag says the child is
/// reaped, never on the ring alone: the watcher stores the flag before it
/// rings, and `reader_signals.reader_gate` rings the same doorbell to bring the
/// reader to its park. From then on the wait runs in rounds of `grace_duration`, and a round in
/// which the terminal produces nothing ends the pump: everything the dead
/// child printed has been handed over, and the caller publishes the exit
/// behind it. Rounds stop `limit_duration` after the ring that carried the flag, which
/// bounds a descendant that holds the terminal open and keeps printing.
///
/// Each round opens at the gate's park, before the wait. A reader held there
/// has read nothing it did not hand over.
///
/// The pump also ends when the terminal reports its end, when `read` fails
/// with anything but `Interrupted`, when the consumer refuses a chunk, when a
/// ring finds the pane settled, and when the wait fails with anything but
/// `EINTR`. [`kill_pane`](PortablePtyBackend::kill_pane) settles the pane and then
/// kills the child: the ring the watcher fires on reaping it finds the pane
/// settled, and the pump stops without starting a round.
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
                    if reader_signals.child_exited.load(Ordering::SeqCst) {
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
/// `grace` until `deadline`; `true` says the deadline passed with the reader
/// still short of the end.
///
/// A chunk in the consumer's hands holds the answer back for as long as it is
/// in flight, however far past `deadline` that runs.
///
/// `false` means stop without publishing: the reader settled, or `cancel`
/// carried a value — [`kill_pane`](PortablePtyBackend::kill_pane) closing the pane — or
/// its sender was dropped, which is the backend shutting down. `kill` settles
/// the exit before it sends.
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

/// Wait until `pane`'s reader is back reading its terminal. The caller closes
/// the terminal after this.
///
/// Windows waits for a pseudoconsole's output pipe to be read out before
/// `ClosePseudoConsole` returns, and the pane's reader is that pipe's one
/// reader. A reader still handing a chunk to its consumer is not reading it.
///
/// Checks every `check_in`. Returns once no chunk is in the consumer's hands,
/// once `cancel` carries a value — [`kill_pane`](PortablePtyBackend::kill_pane) closing
/// the pane — or once its sender is dropped, which is the backend shutting
/// down.
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

/// A pane's terminal, as the backend holds it for resizing.
///
/// The `Owned` arm is one descriptor the pane opened for itself, shared with
/// its reader and writer threads: the whole pane spends a single descriptor on
/// its terminal. `Crate` keeps `portable-pty`'s master, for a terminal that
/// exposes no descriptor to share.
enum Terminal {
    /// The pane's own descriptor, also held by its reader and writer.
    #[cfg(unix)]
    Owned(Arc<std::os::fd::OwnedFd>),
    /// `portable-pty`'s master, resized through the crate.
    ///
    /// A slot the watcher shares. On Windows the watcher takes the master out
    /// and drops it once the child is reaped, which closes the pane's
    /// terminal; the slot is empty from then on.
    Crate(Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>),
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
            Terminal::Crate(pty_master_slot) => {
                match pty_master_slot.lock().expect("terminal").as_ref() {
                    Some(master_pty) => master_pty
                        .resize(build_portable_pty_size(pty_size))
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
    /// `portable-pty`'s reader, blocked in `read` until the terminal ends.
    Crate(Box<dyn Read + Send>),
}

/// What a pane's writer thread sends the child's input with.
enum WriteSide {
    /// The pane's own descriptor — the same one its reader holds; both
    /// directions of the terminal share it.
    #[cfg(unix)]
    Owned(Arc<std::os::fd::OwnedFd>),
    /// `portable-pty`'s writer, which reports the end of input itself when
    /// dropped.
    Crate(Box<dyn Write + Send>),
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

/// What a pane's watcher thread does after it has reaped the child, chosen by
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
        /// The pane's master, taken out and dropped to close the console.
        pty_master_slot: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
        /// Where the standby publishes the exit. `None` under a channel
        /// consumer, which reads the exit off its own handle. Held weakly: a
        /// watcher waiting on a long-lived child keeps no consumer alive.
        pty_sink: Option<Weak<dyn PtySink>>,
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
/// The watcher's whole tail once the child is reaped, on both paths that have
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
/// Settles whether or not it publishes. `publish` is
/// [`should_publish_exit`]'s answer.
fn settle_exit_and_publish_status(
    pty_sink: &Arc<dyn PtySink>,
    exit_handover_state: &Mutex<ExitHandover>,
    pane_id: PaneId,
    exit_status: ExitStatus,
    should_publish_exit_status: bool,
) {
    let was_already_settled = std::mem::replace(
        &mut exit_handover_state.lock().expect("handover").is_settled,
        true,
    );
    if should_publish_exit_status && !was_already_settled {
        pty_sink.accept_exit_status(pane_id, exit_status);
    }
}

/// Kills the wrapped child on drop unless [`release_child`](ChildGuard::release_child)ed.
///
/// Dropping a `portable-pty` child does not terminate the process. The guard
/// wraps the child through [`spawn_pane`](PortablePtyBackend::spawn_pane)'s fallible
/// setup: a step after launch that returns early drops the guard, which kills
/// the child. [`release_child`](ChildGuard::release_child) hands the child to the watcher
/// thread, and the guard kills nothing after that.
struct ChildGuard {
    child_process: Option<Box<dyn Child + Send + Sync>>,
}

impl ChildGuard {
    fn from_child(child_process: Box<dyn Child + Send + Sync>) -> Self {
        ChildGuard {
            child_process: Some(child_process),
        }
    }

    /// Take the child out, leaving the guard inert (no kill on drop).
    fn release_child(mut self) -> Box<dyn Child + Send + Sync> {
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

impl std::ops::Deref for ChildGuard {
    type Target = dyn Child + Send + Sync;

    fn deref(&self) -> &Self::Target {
        self.child_process
            .as_deref()
            .expect("child present until disarmed")
    }
}

/// Start a pane's writer thread on `side`, and hand back the channel its input
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
fn start_writer(write_side: WriteSide) -> Sender<WriterMessage> {
    let (writer_sender, writer_receiver) = channel::<WriterMessage>();

    let _ = spawn_pty_thread("koshi-pty-write", move || {
        let mut write_side = write_side;
        while let Ok(writer_message) = writer_receiver.recv() {
            match writer_message {
                WriterMessage::Bytes(input_bytes) => match &mut write_side {
                    #[cfg(unix)]
                    WriteSide::Owned(terminal_fd) => {
                        let _ = write_terminal(terminal_fd, &input_bytes);
                    }
                    WriteSide::Crate(pty_writer) => {
                        let _ = pty_writer
                            .write_all(&input_bytes)
                            .and_then(|_| pty_writer.flush());
                    }
                },
                WriterMessage::Barrier(barrier_sender) => {
                    let _ = barrier_sender.send(());
                }
                WriterMessage::Stop => break,
            }
        }
    });

    writer_sender
}

/// Start the reader thread of a pane that owns its terminal descriptor.
///
/// It waits on the descriptor beside its doorbell, hands each chunk to
/// `delivery`, and parks at the gate `ticket` holds a place in whenever the
/// backend holds its readers. Once the pump ends it drops `ticket` — the gate
/// stops waiting for it — and then reports the child's end, which waits for the
/// watcher's status.
#[cfg(unix)]
fn start_owned_reader(
    delivery: Delivery,
    pane_id: PaneId,
    terminal_fd: Arc<std::os::fd::OwnedFd>,
    reader_waker: Arc<Waker>,
    child_exited: Arc<AtomicBool>,
    reader_ticket: ReaderTicket,
) -> JoinHandle<()> {
    spawn_pty_thread("koshi-pty-read", move || {
        pump_waited(
            &delivery,
            pane_id,
            &terminal_fd,
            ReaderSignals {
                reader_waker: &reader_waker,
                child_exited: &child_exited,
                reader_gate: reader_ticket.get_reader_gate(),
            },
            EXIT_PUBLISH_GRACE_DURATION,
            EXIT_PUBLISH_LIMIT_DURATION,
        );
        drop(reader_ticket);
        delivery.publish_exit_status(pane_id);
    })
}

/// The pane threads a watcher releases once it holds the child's exit status.
struct WatchRelease {
    /// Flipped once the child is reaped;
    /// [`kill_pane`](PortablePtyBackend::kill_pane) reads it before signalling the
    /// leader.
    child_exited: Arc<AtomicBool>,
    /// The status itself, kept on the pane;
    /// [`list_carried_panes`](PortablePtyBackend::list_carried_panes) hands it to the
    /// next process image.
    child_exit_status: Arc<OnceLock<ExitStatus>>,
    /// Carries the status to whoever publishes it.
    exit_status_sender: Sender<ExitStatus>,
    /// Releases the pane's writer thread.
    writer_stop_sender: Sender<WriterMessage>,
    /// The reader's doorbell, rung once the child is reaped; the reader takes
    /// the last of the child's output.
    /// `None` for a reader that cannot be brought back from its `read`.
    #[cfg(unix)]
    reader_waker: Option<Arc<Waker>>,
}

impl WatchRelease {
    /// Record `status` on the pane, mark the child gone, hand the status on,
    /// release the writer, and ring the reader's doorbell.
    ///
    /// The status is stored before the flag: a reader of the flag finds the
    /// status already stored.
    fn publish_exit_status(&self, exit_status: ExitStatus) {
        let _ = self.child_exit_status.set(exit_status);
        self.child_exited.store(true, Ordering::SeqCst);
        let _ = self.exit_status_sender.send(exit_status);
        let _ = self.writer_stop_sender.send(WriterMessage::Stop);
        #[cfg(unix)]
        if let Some(reader_waker) = &self.reader_waker {
            reader_waker.wake_reader();
        }
    }
}

/// Wait for child `child_process_id` to end, and report how it ended.
///
/// What a pane taken back through [`adopt`](PortablePtyBackend::adopt) reaps
/// its child with, when no image before it saw the child end: the swap left the
/// process id but no `portable-pty` child to wait on. An interrupted wait is
/// retried, and a stop or a continue is not an end. `ECHILD` — the child was
/// reaped elsewhere — is reported as [`ExitStatus::ExitCode`]`(-1)`, the same
/// value a failed wait reports.
#[cfg(unix)]
fn wait_for_child(child_process_id: u32) -> ExitStatus {
    use nix::errno::Errno;
    use nix::sys::wait::{waitpid, WaitStatus};
    use nix::unistd::Pid;

    let waited_process_id = Pid::from_raw(child_process_id as i32);
    loop {
        match waitpid(waited_process_id, None) {
            Ok(WaitStatus::Exited(_, code)) => return ExitStatus::ExitCode(code),
            Ok(WaitStatus::Signaled(_, signal, _)) => return ExitStatus::Signaled(signal as i32),
            Ok(_) => {}
            Err(Errno::EINTR) => {}
            Err(_) => return UNOBSERVED_EXIT,
        }
    }
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
    /// The watcher's [`WriterMessage::Stop`], queued once the child has exited,
    /// releases the writer — including on a pane left open past its child's
    /// death. This is not the only `Sender` on the channel: the watcher holds a
    /// clone to send that `Stop` with, and dropping this one (on
    /// `kill`/teardown) leaves the channel open until the watcher ends too.
    /// `kill` joins the watcher; the `Stop` has been queued by the time `kill`
    /// returns.
    ///
    /// A writer already blocked inside its write — the child stopped reading
    /// while a `setsid` descendant still holds the slave open (Linux; macOS
    /// `revoke`s it) — cannot be interrupted; like the reader it exits only once
    /// that descriptor finally closes. `kill` never joins it: its thread and
    /// that descriptor stay until then, and the dispatcher is never blocked.
    /// [`flush_writers`](PortablePtyBackend::flush_writers) sends its barrier on
    /// this same channel, and names the pane whose writer is in that state.
    writer_sender: Sender<WriterMessage>,
    /// Kill handle for the child process:
    /// [`force_kill_child`](PtyChildKillControl::force_kill_child) signals the child alone,
    /// [`force_kill_process_tree`](PtyChildKillControl::force_kill_process_tree) the whole group, and the two stop
    /// requests ask them to exit on their own.
    kill_control: PtyChildKillControl,
    /// Flipped to `true` by the watcher thread the moment the child exits; read
    /// by [`kill_pane`](PortablePtyBackend::kill_pane) before it signals the leader.
    child_exited: Arc<AtomicBool>,
    /// How the child ended, filled in by the watcher thread alongside
    /// `child_exited`.
    /// Empty while the child runs. Read by
    /// [`carried_panes`](PortablePtyBackend::list_carried_panes), which is how a
    /// status this process observed reaches the process image that replaces it:
    /// a reaped child cannot be waited on twice.
    child_exit_status: Arc<OnceLock<ExitStatus>>,
    /// Reader thread: drains the pane's terminal to wherever this backend
    /// delivers — the handle's output channel, or the sink. Under a sink it
    /// also publishes the child's exit once that output has run dry, behind
    /// the last of the child's output.
    ///
    /// Not joined on teardown: the slave descriptor can outlive the child (a child
    /// that `setsid`s into a new process group), and a join could block until
    /// that descriptor closes. The thread exits once the descriptor closes, and under a PTY sink
    /// it lets the consumer go on the first chunk it reads after the pane's
    /// exit is settled. Retained: the struct owns the handle.
    ///
    /// Where the terminal can be waited on, the thread comes back on
    /// `reader_wake` and ends within one round of the pane being closed,
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
    /// Watcher thread: blocks on the child, records exit status, and flips
    /// `child_exited`.
    watcher_thread: JoinHandle<()>,
    /// Whether this pane's exit is settled — the state the reader and watcher
    /// share. [`kill_pane`](PortablePtyBackend::kill_pane) sets it before killing
    /// anything: a caller closing a pane is handed no exit for it by either
    /// thread. Under no PTY sink nothing reads it.
    exit_handover_state: Arc<Mutex<ExitHandover>>,
    /// Wakes the watcher out of the wait it is in.
    ///
    /// [`kill_pane`](PortablePtyBackend::kill_pane) sends on this before joining the
    /// watcher: tearing a pane down returns without sitting through the
    /// rounds. What stops the exit being published is
    /// `exit_handover_state.is_settled`,
    /// which `kill` sets first. Two waits listen: the standby of a Unix
    /// terminal that exposes no descriptor, and a Windows watcher waiting for
    /// the reader before it closes the terminal. A Unix pane that owns its
    /// descriptor has neither, and there the send is a no-op.
    exit_wait_cancel_sender: Sender<()>,
}

/// Real OS-PTY backend built on the `portable-pty` crate. Each spawned pane gets
/// a kernel PTY plus three helper threads (reader, writer, watcher); the backend
/// owns them all through one pane map, keyed by [`PaneId`].
pub struct PortablePtyBackend {
    /// Every live pane's PTY, threads, and kill handle, keyed by [`PaneId`].
    /// Locked: [`spawn_pane`](PtyBackend::spawn_pane), [`resize_pane`](PtyBackend::resize_pane),
    /// [`write_pane_input`](PtyBackend::write_pane_input), and [`kill_pane`](PtyBackend::kill_pane) can all be
    /// called from different dispatcher calls.
    pane_by_id: Mutex<HashMap<PaneId, PaneEntry>>,
    /// Where spawned panes deliver output and exit. `None` routes both through
    /// each pane's [`PtyHandle`] channels, which the caller polls or relays;
    /// `Some` has the reader thread hand them to the consumer directly, with
    /// no relay thread per pane.
    pty_sink: Option<Arc<dyn PtySink>>,
    /// Every pane reader's park, shared by each reader thread that owns its
    /// terminal descriptor. Driven by
    /// [`pause_readers`](PortablePtyBackend::pause_readers) and
    /// [`resume_readers`](PortablePtyBackend::resume_readers).
    #[cfg(unix)]
    reader_gate: Arc<ReaderGate>,
}

impl PortablePtyBackend {
    /// Creates a new, empty PTY backend with no active panes, delivering each
    /// pane's output and exit through its own [`PtyHandle`] channels.
    pub fn new() -> Self {
        PortablePtyBackend {
            pane_by_id: Mutex::new(HashMap::new()),
            pty_sink: None,
            #[cfg(unix)]
            reader_gate: Arc::new(ReaderGate::new()),
        }
    }

    /// Creates a new, empty PTY backend that hands every pane's output and exit
    /// to `pty_sink` from the pane's own reader thread.
    ///
    /// No per-pane relay thread exists: delivering a chunk is a single
    /// function call.
    pub fn with_pty_sink(pty_sink: Arc<dyn PtySink>) -> Self {
        PortablePtyBackend {
            pane_by_id: Mutex::new(HashMap::new()),
            pty_sink: Some(pty_sink),
            #[cfg(unix)]
            reader_gate: Arc::new(ReaderGate::new()),
        }
    }

    /// Hold every pane's reader at the top of its round: nothing more is read
    /// from a terminal until the readers are resumed.
    ///
    /// A reader parks before it waits on its terminal: a paused backend has no
    /// chunk in anyone's hands and no byte read but undelivered. Each parked
    /// reader keeps its `Delivery`, and with it the pane's exit channel, and
    /// [`resume_readers`](PortablePtyBackend::resume_readers) puts it back to
    /// work in this same process. A reader whose pump already ended is no
    /// longer counted and does not hold this up.
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
        self.reader_gate.wait_until_all_readers_parked();
        Ok(())
    }

    /// Does nothing and answers `Ok(())`. On Windows the process that holds
    /// the panes is never replaced, and its readers are never held still.
    #[cfg(windows)]
    pub fn pause_readers(&self) -> Result<(), PtyError> {
        Ok(())
    }

    /// Put every parked reader back to work. No byte is lost and no false exit
    /// is published: each reader carries on from the top of the round it parked
    /// in.
    #[cfg(unix)]
    pub fn resume_readers(&self) {
        self.reader_gate.resume_readers();
    }

    /// Does nothing. This backend's readers are never held still.
    #[cfg(windows)]
    pub fn resume_readers(&self) {}

    /// Wait until every pane's writer thread has written what it was handed:
    /// no byte this backend took for a child is still queued.
    ///
    /// Each pane is sent a barrier on the channel its bytes travel; an answer
    /// to that barrier means every byte queued before it is on the pane's
    /// terminal. A pane whose writer thread has already ended is passed over:
    /// its child is gone.
    ///
    /// The write-direction counterpart of
    /// [`pause_readers`](PortablePtyBackend::pause_readers). A process about
    /// to replace its own image calls both: the writer threads and their
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
    /// A `terminal_fd` of `None` marks exactly the panes
    /// [`pause_readers`](PortablePtyBackend::pause_readers) refuses. A pane
    /// gets its own descriptor and its reader's doorbell together or gets
    /// neither: a caller that finds a descriptor for every pane here knows
    /// every reader can park.
    ///
    /// A pane whose child this process already reaped carries the exit status
    /// its watcher observed. Readers being held still does not hold a watcher
    /// still: a child that ends during the hand-over is reaped here, and a
    /// reaped child cannot be waited on again.
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
                    Terminal::Crate(_) => None,
                },
                process_id: pane_entry.kill_control.get_child_process_id(),
                pty_size: pane_entry.pty_size,
                exit_status: pane_entry.child_exit_status.get().copied(),
            })
            .collect()
    }

    /// The process id of `pane`'s child, or `None` when this backend does not
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
    /// three threads, the same channels, the same kill behaviour — around a
    /// terminal and a child that are already running. `pty_size` is recorded as the
    /// pane's last size; the terminal carried that size across the swap, and
    /// nothing is written to it: the child is sent no `SIGWINCH`.
    ///
    /// `carried_exit_status` is how the child ended, as the image before this one observed it
    /// — [`CarriedPtyPane::exit_status`]. The watcher publishes that status straight
    /// away and waits on nothing: the process that reaped the child took the
    /// status out of the kernel with it. The pane's reader still hands over
    /// whatever the terminal holds before that exit reaches the consumer.
    ///
    /// `None` means no image has seen this child end, and the watcher reaps it
    /// with `waitpid` on that one process id. `portable-pty` resets `SIGCHLD`
    /// only inside `pre_exec`, on the child side of the fork (`portable-pty`
    /// 0.9.0, `src/unix.rs`, `spawn_command`): no parent-side reaper competes
    /// for the status. A child reaped by something outside this backend answers
    /// `ECHILD` and is reported as [`ExitStatus::ExitCode`]`(-1)`.
    ///
    /// Before → after: a pane running `sh -c 'sleep 1; exit 3'` is restarted
    /// 0.9 s in, and the child ends while the images swap → the old image's
    /// watcher reaps code 3 and carries it here, and the pane comes back
    /// reporting `ExitCode(3)` rather than the `ExitCode(-1)` of a wait that
    /// found nothing.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] if this platform offers no one-descriptor wake
    /// for the reader, which is what parks it, and [`PtyError::Spawn`] —
    /// `pane <id> is already open` — when this backend already drives
    /// `pane_id`. `terminal_fd` is closed on either error, and a refused
    /// take-back leaves the live pane and the child behind `pid` untouched.
    #[cfg(unix)]
    pub fn adopt(
        &self,
        pane_id: PaneId,
        terminal_fd: std::os::fd::OwnedFd,
        process_id: u32,
        pty_size: PtySize,
        carried_exit_status: Option<ExitStatus>,
    ) -> Result<PtyHandle, PtyError> {
        // Where this pane's output goes, built exactly as `spawn` builds it.
        let exit_handover_state = Arc::new(Mutex::new(ExitHandover::default()));
        let (exit_wait_cancel_sender, exit_wait_cancel_receiver) = channel::<()>();
        // The terminal exposes a descriptor: the reader reaches the end of
        // the output itself, and no watcher stands by to be cancelled.
        drop(exit_wait_cancel_receiver);
        let (pty_handle, delivery, exit_status_sender, _) =
            build_delivery(self.pty_sink.clone(), pane_id, &exit_handover_state);

        let waker = Arc::new(Waker::new().ok_or(PtyError::Io {
            detail: "this platform offers no one-descriptor wake for a pane reader".to_string(),
        })?);
        let terminal_handle = Arc::new(terminal_fd);
        let child_exited = Arc::new(AtomicBool::new(false));
        let child_exit_status = Arc::new(OnceLock::new());

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
            Arc::clone(&child_exited),
            self.reader_gate.register_reader(),
        );
        let writer_sender = start_writer(WriteSide::Owned(Arc::clone(&terminal_handle)));

        // The watcher publishes the carried status, or reaps the process id
        // itself when no image saw the child end: the swap left no
        // `portable-pty` child to wait on. Everything after that is what a
        // spawned pane's watcher does.
        let watch_release = WatchRelease {
            child_exited: Arc::clone(&child_exited),
            child_exit_status: Arc::clone(&child_exit_status),
            exit_status_sender,
            writer_stop_sender: writer_sender.clone(),
            reader_waker: Some(Arc::clone(&waker)),
        };
        let watcher_thread = spawn_pty_thread("koshi-pty-watch", move || {
            watch_release.publish_exit_status(
                carried_exit_status.unwrap_or_else(|| wait_for_child(process_id)),
            );
        });

        pane_by_id.insert(
            pane_id,
            PaneEntry {
                terminal: Terminal::Owned(terminal_handle),
                pty_size,
                writer_sender,
                kill_control: PtyChildKillControl::from_process_id(process_id),
                child_exited,
                child_exit_status,
                exit_handover_state,
                exit_wait_cancel_sender,
                reader_thread,
                reader_waker: Some(waker),
                watcher_thread,
            },
        );
        drop(pane_by_id);
        Ok(pty_handle)
    }
}

impl Default for PortablePtyBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PtyBackend for PortablePtyBackend {
    /// Open a fresh PTY, launch `spawn_spec` as a child inside it, and wire up its I/O.
    ///
    /// The child runs detached on three background threads owned by the
    /// backend: a **reader** (master output → wherever this backend delivers),
    /// a **writer** (input channel → master; writes never block the
    /// dispatcher), and a **watcher** (`child.wait()` → exit channel, flips the
    /// `child_exited` flag, and releases the writer).
    ///
    /// Where the output goes depends on how the backend was built. Under
    /// [`with_pty_sink`](PortablePtyBackend::with_pty_sink) the reader hands each chunk
    /// to the PTY sink and publishes the exit itself once the output runs out, and
    /// the returned [`crate::backend::state::PtyHandle`] carries no channels.
    /// On Windows the watcher closes the pane's terminal once the child is
    /// reaped, which brings the reader to that end; on a Unix terminal that
    /// exposes no descriptor to wait on the watcher publishes the exit itself,
    /// once output stops arriving. A pane always learns its child ended. The
    /// reader then stops on its next chunk: a descendant still printing into a
    /// closed pane's terminal is not forwarded. Without a PTY sink the handle
    /// carries the output and exit channels for the caller to poll or relay.
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
    ) -> Result<PtyHandle, PtyError> {
        // 1. Decide where this pane's output goes, and build the caller's handle
        //    to match, in `build_delivery`. Under a sink the reader publishes the
        //    status once the child's output has run out: a consumer never sees
        //    the child end while output is still coming.
        //
        //    The watcher takes a second reference to the sink for the paths
        //    where it publishes the exit itself: a Unix terminal that exposes
        //    no descriptor to wait on, and a Windows pane. `handover` carries
        //    both facts under one lock — whether the pane's exit is settled,
        //    and how much the reader has handed over — and the watcher never
        //    sees half a transition.
        let exit_handover_state = Arc::new(Mutex::new(ExitHandover::default()));
        // The watcher's one interruptible wait: a Unix terminal with no
        // descriptor stands by on it, and a Windows pane waits on it for its
        // reader before closing the terminal. `kill` sends on it.
        let (exit_wait_cancel_sender, exit_wait_cancel_receiver) = channel::<()>();
        let (pty_handle, delivery, exit_status_sender, watcher_pty_sink) =
            build_delivery(self.pty_sink.clone(), pane_id, &exit_handover_state);
        // 2. Open the PTY pair sized to the pane. The pair is two linked ends:
        //    `master` stays with us, `slave` becomes the child's terminal.
        let pty_system = native_pty_system();
        let pty_pair = pty_system
            .openpty(build_portable_pty_size(pty_size))
            .map_err(|io_error| PtyError::Spawn {
                detail: io_error.to_string(),
            })?;

        // 3. Build the launch command from the spawn specification (program,
        // arguments, working directory, environment variables)...
        let mut command_builder = CommandBuilder::new(spawn_spec.program.as_os_str());
        command_builder.args(&spawn_spec.arguments);
        // Resolve the working directory before launch: an explicit path wins;
        // an absent path
        // inherits koshi's process cwd, matching `SpawnSpec`'s contract.
        match &spawn_spec.working_directory {
            Some(working_directory) => command_builder.cwd(working_directory),
            None => {
                if let Ok(working_directory) = std::env::current_dir() {
                    command_builder.cwd(working_directory);
                }
            }
        }

        //    ...including the environment. `CommandBuilder` is never cleared:
        //    the child inherits the full parent env, kept as `OsString`, and
        //    non-UTF-8 vars survive intact. `build_environment_overlay` returns only koshi's
        //    overlay (terminal identity + shell bootstrap + the spawn
        //    specification's environment variables), and
        //    applying each key with `command_builder.env` overwrites the inherited value.
        //    On Windows `portable-pty` folds env names case-insensitively: an
        //    override such as `PATH` replaces a differently-cased inherited
        //    key (`Path`).
        for (environment_variable_name, environment_variable_value) in
            build_environment_overlay(&spawn_spec)
        {
            command_builder.env(environment_variable_name, environment_variable_value);
        }

        //    ...and launch it on the slave end. The child now owns the slave as
        //    its stdin/stdout/stderr; we keep `child_guard` only to wait on / kill it.
        //    A `portable-pty` child is not terminated by being dropped;
        //    `ChildGuard` kills the child when any step below returns early.
        let child_guard =
            ChildGuard::from_child(pty_pair.slave.spawn_command(command_builder).map_err(
                |spawn_error| PtyError::Spawn {
                    detail: spawn_error.to_string(),
                },
            )?);

        let process_id = child_guard.process_id().ok_or(PtyError::Spawn {
            detail: "child has no PID".to_string(),
        })?;

        // 4. Build the kill control right away. On Windows this assigns the
        //    child to its Job Object, first thing after the spawn.
        //
        //    The child is already running when the assignment happens. A
        //    grandchild it forks before that assignment lands stays outside the
        //    Job Object, where `KillPolicy::Tree` does not reach it.
        #[cfg(unix)]
        let kill_control = PtyChildKillControl::from_process_id(process_id);
        #[cfg(windows)]
        let kill_control = PtyChildKillControl::from_process_id_and_handle(
            process_id,
            child_guard.as_raw_handle().ok_or(PtyError::Spawn {
                detail: "child has no process handle".to_string(),
            })?,
        )?;

        // 5. Drop OUR copy of the slave. The child kept its own; once the child
        //    exits and the kernel closes its end, the terminal reports EOF, and
        //    the reader thread (step 9) stops.
        drop(pty_pair.slave);

        // 6. Decide how this pane reaches its terminal, and pull the exit flag.
        //
        //    A terminal that exposes a descriptor is opened once, here, and
        //    that one descriptor serves the whole pane: its reader waits on it
        //    and reads it, its writer writes it, and `resize` retunes it. The
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
        //    Windows exposes none of this: there the pane keeps the crate's
        //    own reader, writer and master, and its reader blocks in `read`.
        #[cfg(unix)]
        let owned_terminal_parts = duplicate_terminal_file_descriptor(&*pty_pair.master)
            .zip(Waker::new())
            .map(|(terminal_fd, reader_waker)| (Arc::new(terminal_fd), Arc::new(reader_waker)));
        #[cfg(not(unix))]
        let owned_terminal_parts: Option<(Arc<()>, Arc<()>)> = None;

        // The doorbell's other two holders: the reader waits on it, the watcher
        // rings it once the child is reaped, and the pane entry rings it to
        // park the reader.
        #[cfg(unix)]
        let watcher_reader_waker = owned_terminal_parts
            .as_ref()
            .map(|(_, reader_waker)| Arc::clone(reader_waker));
        #[cfg(unix)]
        let entry_reader_waker = owned_terminal_parts
            .as_ref()
            .map(|(_, reader_waker)| Arc::clone(reader_waker));

        //    The watcher's tail — what it does once the child is reaped —
        //    follows from the same choice: a reader that can be waited on
        //    publishes the exit itself, a Unix terminal with no descriptor
        //    leaves the watcher to publish, and Windows closes the terminal and
        //    then stands by behind the reader.
        let (terminal_read_side, terminal_write_side, terminal_resource, watcher_tail) =
            match owned_terminal_parts {
                #[cfg(unix)]
                Some((terminal_fd, reader_waker)) => (
                    ReadSide::Owned(Arc::clone(&terminal_fd), reader_waker),
                    WriteSide::Owned(Arc::clone(&terminal_fd)),
                    Terminal::Owned(terminal_fd),
                    WatcherTail::ReaderPublishes,
                ),
                #[cfg(not(unix))]
                Some(_) => unreachable!("no platform without a descriptor owns one"),
                None => {
                    let terminal_reader =
                        pty_pair
                            .master
                            .try_clone_reader()
                            .map_err(|io_error| PtyError::Spawn {
                                detail: io_error.to_string(),
                            })?;
                    let pty_writer =
                        pty_pair
                            .master
                            .take_writer()
                            .map_err(|io_error| PtyError::Spawn {
                                detail: io_error.to_string(),
                            })?;
                    // The master goes in a slot the watcher shares: on Windows it
                    // takes it out and drops it once the child is reaped, which
                    // closes the console and ends the reader's pipe.
                    let pty_master_slot = Arc::new(Mutex::new(Some(pty_pair.master)));
                    #[cfg(windows)]
                    let watcher_tail = WatcherTail::CloseTerminal {
                        pty_master_slot: Arc::clone(&pty_master_slot),
                        pty_sink: watcher_pty_sink.as_ref().map(Arc::downgrade),
                    };
                    #[cfg(not(windows))]
                    let watcher_tail = match watcher_pty_sink.as_ref() {
                        Some(pty_sink) => {
                            WatcherTail::WaitForReaderThenPublish(Arc::downgrade(pty_sink))
                        }
                        None => WatcherTail::ReaderPublishes,
                    };
                    (
                        ReadSide::Crate(terminal_reader),
                        WriteSide::Crate(pty_writer),
                        Terminal::Crate(pty_master_slot),
                        watcher_tail,
                    )
                }
            };

        let child_exited = Arc::new(AtomicBool::new(false));
        let child_exit_status = Arc::new(OnceLock::new());

        // 7. Take the pane map now and hold it until this pane is in it. A
        //    short-lived child can be reaped and its exit handed to the
        //    consumer before this call returns; a `kill` from inside that call
        //    blocks here until the insert lands, and then finds the pane. None
        //    of the threads started below touch this map.
        //
        //    An id the backend already holds is refused here, with the map
        //    locked. The entry it would replace keeps its terminal and its I/O
        //    threads, and the guard is still armed, so it kills the child
        //    launched above.
        let mut pane_by_id = self.pane_by_id.lock().unwrap();
        if pane_by_id.contains_key(&pane_id) {
            return Err(PtyError::Spawn {
                detail: format!("pane {pane_id} is already open"),
            });
        }

        // That refusal is the last step that returns early: the watcher thread
        // below owns the child and reaps it. Disarm the guard.
        let child_process = child_guard.release_child();

        // 8. Writer thread: drain the input channel onto the terminal. A write
        //    to a child that has stopped reading blocks only that thread,
        //    never the dispatcher. Started before the reader and before the
        //    watcher, which queues its `Stop` on this channel.
        let writer_sender = start_writer(terminal_write_side);

        //    A Windows pseudoconsole asks where the cursor is as it opens and
        //    holds its child's output until that is answered. The answer is
        //    queued here, ahead of the pane being reachable by
        //    [`write_pane_input`](PortablePtyBackend::write_pane_input): nothing a user types
        //    reaches the terminal before it.
        #[cfg(windows)]
        let _ = writer_sender.send(WriterMessage::Bytes(
            CURSOR_POSITION_AT_HOME_RESPONSE_BYTES.to_vec(),
        ));

        // 9. Reader thread: wait on the terminal, handing each chunk of
        //    child output to `delivery` until EOF (child gone) or the consumer
        //    goes away. Once the output has run out a sink can be told the
        //    child ended; already-settled panes return from `finish` without
        //    waiting.
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
                Arc::clone(&child_exited),
                self.reader_gate.register_reader(),
            ),
            ReadSide::Crate(terminal_reader) => spawn_pty_thread("koshi-pty-read", move || {
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

        // 10. Watcher thread: block on `child.wait()`, map the OS exit status
        //    into koshi's `ExitStatus`, then release the pane's other threads —
        //    flip `exited` (read by `kill` before it signals the leader),
        //    publish the status on the exit channel, stop the writer, and ring
        //    the reader's doorbell: the reader takes the last of the output.
        //
        //    What it does after that is its tail, decided in step 6: bring the
        //    reader to the end of the terminal, or stand by and publish the
        //    exit itself. On Windows the tail waits for the reader to be back
        //    on the terminal and then closes the pane's console, which that
        //    reader is there to read out. `kill` wakes both waits: closing a
        //    pane returns without sitting through either.
        let watch_release = WatchRelease {
            child_exited: Arc::clone(&child_exited),
            child_exit_status: Arc::clone(&child_exit_status),
            exit_status_sender,
            writer_stop_sender: writer_sender.clone(),
            #[cfg(unix)]
            reader_waker: watcher_reader_waker,
        };
        let watcher_exit_handover_state = Arc::clone(&exit_handover_state);
        let watcher_thread = spawn_pty_thread("koshi-pty-watch", move || {
            let mut child_process = child_process;
            let exit_status = match child_process.wait() {
                Ok(process_exit_status) => parse_portable_exit_status(process_exit_status),
                Err(_) => UNOBSERVED_EXIT,
            };
            watch_release.publish_exit_status(exit_status);

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
                    if let Some(pty_sink) = pty_sink {
                        wait_for_reader_before_publishing(
                            &exit_wait_cancel_receiver,
                            &watcher_exit_handover_state,
                            &pty_sink,
                            pane_id,
                            exit_status,
                        );
                    }
                }
                #[cfg(not(windows))]
                WatcherTail::WaitForReaderThenPublish(pty_sink) => {
                    wait_for_reader_before_publishing(
                        &exit_wait_cancel_receiver,
                        &watcher_exit_handover_state,
                        &pty_sink,
                        pane_id,
                        exit_status,
                    );
                }
            }
        });

        // 11. Retain the terminal, writer, killer, flag and both thread handles
        //    under the pane id, then hand the caller its polling handle.
        pane_by_id.insert(
            pane_id,
            PaneEntry {
                terminal: terminal_resource,
                pty_size,
                writer_sender,
                kill_control,
                child_exited,
                child_exit_status,
                exit_handover_state,
                exit_wait_cancel_sender,
                reader_thread,
                #[cfg(unix)]
                reader_waker: entry_reader_waker,
                watcher_thread,
            },
        );
        drop(pane_by_id);
        Ok(pty_handle)
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

        pane_entry
            .writer_sender
            .send(WriterMessage::Bytes(input_bytes.to_vec()))
            .map_err(|send_error| PtyError::Io {
                detail: send_error.to_string(),
            })
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
        // watcher has reaped it: a recycled PID can belong to an unrelated
        // process. `Tree` and `GracefulTree`'s closing group-kill signal the
        // whole group/job (`killpg` / `TerminateJobObject`), which stays valid
        // while any member lives, and fire unconditionally: the leader can
        // exit while a same-group descendant keeps running, and the group-kill
        // still reaps it. The `exited` flag tracks only the leader, not
        // whether the group is empty.
        match kill_policy {
            KillPolicy::Force => {
                if !pane_entry.child_exited.load(Ordering::SeqCst) {
                    let _ = pane_entry.kill_control.force_kill_child();
                }
            }
            KillPolicy::Tree => {
                let _ = pane_entry.kill_control.force_kill_process_tree();
            }
            KillPolicy::Graceful { timeout_duration } => {
                if !pane_entry.child_exited.load(Ordering::SeqCst) {
                    // Ask the leader to exit and give it the grace window; SIGKILL when the
                    // window runs out, or at once when the request never reached it.
                    if !is_child_stopped_within_grace(
                        pane_entry.kill_control.request_child_stop(),
                        &pane_entry.child_exited,
                        timeout_duration,
                    ) {
                        let _ = pane_entry.kill_control.force_kill_child();
                    }
                }
            }
            KillPolicy::GracefulTree { timeout_duration } => {
                if !pane_entry.child_exited.load(Ordering::SeqCst) {
                    // Ask the whole group to exit — every member gets the stop request and
                    // the grace window — then wait for the leader. The wait is skipped
                    // only when no member received the request.
                    is_child_stopped_within_grace(
                        pane_entry.kill_control.request_process_tree_stop(),
                        &pane_entry.child_exited,
                        timeout_duration,
                    );
                }

                // Group-kill even when the leader already exited: a disowned
                // descendant can keep the group alive past its leader, and
                // `killpg`/`TerminateJobObject` reaps it with the rest.
                let _ = pane_entry.kill_control.force_kill_process_tree();
            }
        }

        // Wake the watcher out of the wait it is in: closing a pane returns
        // without sitting through the rounds. Two waits listen: the standby of
        // a Unix terminal with no descriptor, and a Windows watcher's wait for
        // the reader before it closes the terminal.
        let _ = pane_entry.exit_wait_cancel_sender.send(());
        drop(pane_entry.writer_sender);
        // Joined unless this *is* the watcher: a consumer handed an exit by the
        // watcher may close the pane from inside that call, and a thread that
        // joins itself never returns. The watcher has nothing left to do after
        // handing the exit over.
        if pane_entry.watcher_thread.thread().id() != thread::current().id() {
            let _ = pane_entry.watcher_thread.join();
        }

        Ok(())
    }
    fn find_live_working_directory(&self, pane_id: PaneId) -> Option<std::path::PathBuf> {
        let pane_by_id = self.pane_by_id.lock().unwrap();
        let pane_entry = pane_by_id.get(&pane_id)?;
        // A reaped leader's PID can already belong to an unrelated process:
        // no directory is answered for it.
        if pane_entry.child_exited.load(Ordering::SeqCst) {
            return None;
        }
        crate::working_directory::get_process_working_directory(
            pane_entry.kill_control.get_child_process_id(),
        )
    }
}

/// Whether the leader is gone after being asked to stop.
///
/// Polls [`is_child_exit_observed_within_grace`] for up to `grace_duration` when anything received the stop
/// request, including a group where only part of it did. Returns `false` at
/// once, spending no grace window, when nothing received it.
fn is_child_stopped_within_grace(
    stop_request: StopRequest,
    child_exited: &AtomicBool,
    grace_duration: Duration,
) -> bool {
    match stop_request {
        StopRequest::Delivered | StopRequest::Unknown => {
            is_child_exit_observed_within_grace(child_exited, grace_duration)
        }
        StopRequest::NotDelivered => false,
    }
}

/// Poll the watcher's `child_exited` flag every 25ms until it flips or
/// `grace_duration`
/// elapses, returning whether the child exited within the window. The
/// grace-window wait behind [`is_child_stopped_within_grace`].
fn is_child_exit_observed_within_grace(
    child_exited: &AtomicBool,
    grace_duration: Duration,
) -> bool {
    let exit_deadline = Instant::now() + grace_duration;
    while Instant::now() < exit_deadline {
        if child_exited.load(Ordering::SeqCst) {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    child_exited.load(Ordering::SeqCst)
}

/// Convert koshi's [`PtySize`] into `portable-pty`'s own size type, zeroing
/// the pixel dimensions `portable-pty` accepts but this crate does not track.
fn build_portable_pty_size(pty_size: PtySize) -> portable_pty::PtySize {
    portable_pty::PtySize {
        rows: pty_size.row_count,
        cols: pty_size.column_count,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Convert `portable-pty`'s exit status into koshi's own [`ExitStatus`]:
/// a signal name (Unix only) maps to [`ExitStatus::Signaled`] via
/// [`parse_signal_number`],
/// anything else maps to [`ExitStatus::ExitCode`].
fn parse_portable_exit_status(portable_exit_status: portable_pty::ExitStatus) -> ExitStatus {
    match portable_exit_status.signal() {
        Some(signal_description) => ExitStatus::Signaled(parse_signal_number(signal_description)),
        None => ExitStatus::ExitCode(portable_exit_status.exit_code() as i32),
    }
}

/// Recover a Unix signal number from portable-pty's exit status string.
///
/// portable-pty discards the raw `WTERMSIG` and hands back `strsignal(3)` text,
/// never the `SIG*` mnemonic. That text is platform-specific:
/// - macOS/BSD: `"<description>: <n>"` — e.g. `"Terminated: 15"`
/// - Linux/glibc: `"<description>"` — e.g. `"Terminated"` (no number)
/// - portable-pty's fallback when `strsignal` returns null: `"Signal <n>"`
///
/// The number is parsed ONLY when it follows a `": "` (macOS) or the
/// `"Signal "` prefix (the fallback) — never a bare trailing word. Some glibc
/// descriptions end in a non-signal ordinal: `"User defined signal 1"` is
/// SIGUSR1 = 10, not signal 1. A bare description is mapped through the table
/// below; an unrecognised one yields 0. Reachable only for Unix children: on
/// Windows `signal()` is always `None`, and `parse_portable_exit_status` takes the exit-code
/// arm.
fn parse_signal_number(signal_description: &str) -> i32 {
    // macOS appends ": <n>" — the real number is after the colon.
    if let Some((_, signal_number_text)) = signal_description.rsplit_once(": ") {
        if let Ok(signal_number) = signal_number_text.parse::<i32>() {
            return signal_number;
        }
    }
    // portable-pty's null-strsignal fallback is "Signal <n>".
    if let Some(signal_number) = signal_description
        .strip_prefix("Signal ")
        .and_then(|signal_number_text| signal_number_text.parse::<i32>().ok())
    {
        return signal_number;
    }
    // Linux glibc: bare description, no trailing number.
    match signal_description {
        "Hangup" => 1,
        "Interrupt" => 2,
        "Quit" => 3,
        "Illegal instruction" => 4,
        "Trace/breakpoint trap" => 5,
        "Aborted" => 6,
        "Bus error" => 7,
        "Floating point exception" => 8,
        "Killed" => 9,
        "User defined signal 1" => 10,
        "Segmentation fault" => 11,
        "User defined signal 2" => 12,
        "Broken pipe" => 13,
        "Alarm clock" => 14,
        "Terminated" => 15,
        _ => 0,
    }
}

#[cfg(test)]
mod tests;
