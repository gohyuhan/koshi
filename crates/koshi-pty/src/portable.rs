//! Real OS-PTY backend built on the `portable-pty` crate.
//!
//! A spawned pane gets a kernel PTY and three helper threads (reader, writer,
//! watcher), all owned through the [`crate::portable::PortablePtyBackend`] pane map. The
//! implementation handles child output streaming, input queuing, process
//! termination (with cross-platform kill policies), and exit status tracking.
//!
//! Three threads is the whole per-pane cost: the reader delivers output to the
//! consumer itself, through [`crate::backend::state::PtySink`].
//!
//! One thread publishes a pane's exit, and which one it is follows from how the
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
//! - A Unix terminal that exposes no descriptor to wait on: nothing can bring
//!   the reader back, so the watcher stands by on a deadline and publishes.
//!
//! # What a Windows pane rests on
//!
//! `portable-pty` opens every Windows pane's terminal with
//! `CreatePseudoConsole`, and departs from Microsoft's reference for that call
//! in two ways: it passes flags outside the documented set, which lists only
//! `0` and `PSEUDOCONSOLE_INHERIT_CURSOR`, and it closes the two pipe handles
//! it handed to the call before `CreateProcess` runs, where the documented
//! order closes them after. Microsoft states that handle lifetimes managed
//! wrongly can deadlock a synchronous read or write.
//!
//! `PSEUDOCONSOLE_INHERIT_CURSOR` is passed on every pane. A pseudoconsole
//! created with it writes a cursor-position request to its output and holds
//! its child's output until the process that created it replies on the
//! pseudoconsole's input. The reply is queued as the pane opens, ahead of
//! anything a user can type into it, and the pane's reader takes the request
//! itself out of the output, in `RemovesCursorRequest`.
//!
//! koshi cannot change the flags from inside the crate, so
//! `tests/portable_windows.rs` pins the behaviour: it opens a pane, writes to
//! it, reads its output back and closes it.

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
    backend::state::{PtyBackend, PtyHandle, PtySink},
    env::build_env,
    error::PtyError,
    kill::{PtyChildKillControl, StopRequest},
};

/// Bytes read from a pane's master end in one go. Sized to hold a burst of
/// child output without a syscall per line.
const READ_CHUNK: usize = 8192;

/// One round of the reader's wait on a Unix terminal whose child has gone, and
/// one check-in of the watcher's standby.
///
/// A round in which the terminal produces nothing ends the reader's wait. The
/// standby checks in at the same interval on both paths it runs — a Unix
/// terminal that exposes no descriptor to wait on, and a Windows pane whose
/// terminal is being closed.
const EXIT_PUBLISH_GRACE: Duration = Duration::from_millis(100);

/// The longest a pane's exit is held back after its child has gone.
///
/// Bounds both waits. A descendant that holds the terminal open and keeps
/// printing stops the reader's rounds here. The standby publishes the exit
/// itself once this passes.
const EXIT_PUBLISH_LIMIT: Duration = Duration::from_secs(1);

/// How often a Windows pane's watcher looks at whether the reader is back
/// reading the terminal, before it closes that terminal.
#[cfg(windows)]
const READER_CHECK_IN: Duration = Duration::from_millis(100);

/// The longest [`PortablePtyBackend::flush_writers`] waits for every pane's
/// writer thread to reach the end of what it was handed.
///
/// Bounds the whole flush, not one pane. A writer blocked inside its write —
/// the child stopped reading its stdin — never reaches the end, so the wait
/// stops here and names that pane.
const WRITER_FLUSH_LIMIT: Duration = Duration::from_secs(1);

/// Start one of a pane's helper threads under `name`.
///
/// The name is what a debugger, profiler, or crash report shows for the thread.
/// Spawn failure panics, matching [`std::thread::spawn`].
fn spawn_pty_thread<F>(name: &str, body: F) -> JoinHandle<()>
where
    F: FnOnce() + Send + 'static,
{
    thread::Builder::new()
        .name(name.to_string())
        .spawn(body)
        .expect("spawn pty helper thread")
}

/// Where one pane's reader thread puts the child's output, and how it reports
/// the child's end.
///
/// The two arms are the two routes a pane can be driven through: a caller
/// polling [`PtyHandle`] gets `Channel`, a caller that implements [`PtySink`]
/// gets `Sink`, which needs no relay thread.
enum Delivery {
    /// Push each chunk onto the handle's output channel.
    Channel(Sender<Vec<u8>>),
    /// Hand each chunk to the consumer directly. `exit_receiver` is the
    /// watcher's end of a private channel, read once the output is exhausted.
    Sink {
        /// The consumer taking this pane's output and exit.
        sink: Arc<dyn PtySink>,
        /// The watcher's exit status, awaited after the last chunk.
        exit_receiver: Receiver<ExitStatus>,
        /// This pane's hand-over: whether its exit is settled, and how much of
        /// the PTY the reader has passed on.
        ///
        /// One lock rather than a field each, so every move between those
        /// facts is one step: a chunk is claimed or the pane is settled, never
        /// half of both. The reader stops on it; the watcher reads it to see
        /// whether the reader has published the exit yet.
        handover: Arc<Mutex<Handover>>,
    },
}

/// What a pane's reader has handed over, and whether its exit is settled.
#[derive(Debug, Default)]
struct Handover {
    /// Chunks the reader has begun handing over, counted before the hand-over.
    begun: u64,
    /// Chunks it has finished handing over, counted after. Below `begun`
    /// exactly while a chunk is in the consumer's hands.
    done: u64,
    /// Whether this pane's exit is settled — delivered, or decided against.
    /// A settled pane takes no more output and is told no exit.
    settled: bool,
}

impl Handover {
    /// Whether a chunk is in the consumer's hands right now, which means the
    /// pane's reader is not reading its terminal. Read by the watcher: the
    /// standby a Unix terminal with no descriptor has, and the terminal close
    /// a Windows pane has.
    ///
    /// Counts sink deliveries only. A channel consumer registers nothing here,
    /// so a channel-backed pane always reads as idle.
    fn in_flight(&self) -> bool {
        self.begun != self.done
    }
}

/// A descriptor for `master`'s terminal that the caller owns.
///
/// The caller's own copy, so it stays valid for as long as the caller keeps it
/// — a pane torn down while a helper thread still runs closes the pane's copy,
/// never this one.
///
/// One copy serves the whole pane: its reader waits on it and reads it, its
/// writer writes it, and [`resize`](PortablePtyBackend::resize) retunes it.
/// They are separate directions of the same terminal, which one descriptor
/// carries at once.
///
/// `None` when the platform exposes no descriptor, which is Windows: `MasterPty`
/// offers no ConPTY equivalent, so a Windows pane keeps `portable-pty`'s own
/// reader and writer and its reader blocks in `read`.
#[cfg(unix)]
fn own_terminal_fd(master: &dyn MasterPty) -> Option<std::os::fd::OwnedFd> {
    master.as_raw_fd().and_then(|fd| {
        // `master` is borrowed for this call, so it owns `fd` throughout.
        unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }
            .try_clone_to_owned()
            .ok()
    })
}

/// Read from a pane's terminal, reporting a closed slave as end of input.
///
/// Once the last process holding the slave open lets it go, Linux answers a
/// read on the master with `EIO` rather than zero. Both mean the same thing —
/// nothing will arrive again — so both are reported as zero, which is what
/// `portable-pty`'s own reader does and what every caller here treats as the
/// end.
#[cfg(unix)]
fn read_terminal(terminal: &std::os::fd::OwnedFd, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd;

    let read = unsafe {
        libc::read(
            terminal.as_raw_fd(),
            buf.as_mut_ptr().cast::<libc::c_void>(),
            buf.len(),
        )
    };
    if read >= 0 {
        return Ok(read as usize);
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EIO) => Ok(0),
        _ => Err(err),
    }
}

/// Write `bytes` to a pane's terminal, which reaches its child as typed input.
///
/// Loops until every byte is written, so a partial write finishes rather than
/// silently dropping the tail. Retries an interrupted write.
#[cfg(unix)]
fn write_terminal(terminal: &std::os::fd::OwnedFd, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let mut written = 0;
    while written < bytes.len() {
        let wrote = unsafe {
            libc::write(
                terminal.as_raw_fd(),
                bytes[written..].as_ptr().cast::<libc::c_void>(),
                bytes.len() - written,
            )
        };
        if wrote >= 0 {
            written += wrote as usize;
            continue;
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
    Ok(())
}

/// Tell a pane's child its terminal is now `size`, which the kernel turns into
/// the `SIGWINCH` a full-screen program redraws on.
#[cfg(unix)]
fn resize_terminal(terminal: &std::os::fd::OwnedFd, size: PtySize) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    // The pixel dimensions this crate does not track are sent as zero, which
    // is what a terminal with no pixel geometry reports.
    let ws = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let res = unsafe {
        libc::ioctl(
            terminal.as_raw_fd(),
            libc::TIOCSWINSZ as _,
            std::ptr::addr_of!(ws),
        )
    };
    if res == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Set or clear the close-on-exec flag on a pane's terminal descriptor.
///
/// Cleared before this process replaces its own image, so the descriptor
/// survives into the new one; set again once the new image has taken the pane
/// back, so no child spawned afterwards inherits another pane's terminal. Every
/// other descriptor flag is left as it was.
///
/// # Errors
/// Returns the OS error if the descriptor's flags cannot be read or written.
#[cfg(unix)]
pub fn set_terminal_cloexec(fd: std::os::fd::RawFd, on: bool) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let wanted = if on {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, wanted) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// Apple's own `ptsname_r`. `libc` 0.2.189 declares this call for the
// Linux-like targets, FreeBSD, NetBSD, Fuchsia, Hurd, Cygwin, QNX and illumos,
// and for no Apple target, so the Apple declaration is written out here. macOS
// ships it in libSystem from 10.13.4 onward.
#[cfg(all(unix, target_vendor = "apple"))]
extern "C" {
    fn ptsname_r(fd: libc::c_int, buf: *mut libc::c_char, buflen: libc::size_t) -> libc::c_int;
}

#[cfg(all(unix, not(target_vendor = "apple")))]
use libc::ptsname_r;

/// The name of the terminal the pseudoterminal master on `fd` is paired with,
/// and `None` when `fd` names no such master.
///
/// `ptsname_r` answers for a master only: the slave end of that same pair, an
/// ordinary file, a pipe, a socket, and a number naming nothing open all fail
/// it. Only the return value decides that. A failure reports itself
/// differently per system: macOS returns `-1` and sets `errno`, glibc and musl
/// return the error number itself. `0` means success on all of them.
///
/// The name is the master's own identity: two masters this process holds at
/// once are paired with two different terminals, so a caller that recorded the
/// name of one descriptor can tell that same master from another one.
///
/// Before → after: `fd` holds the master of `/dev/ttys009` →
/// `Some("/dev/ttys009")`. `fd` holds that pair's slave end, an open log file,
/// or a number this process never opened → `None`.
#[cfg(unix)]
#[must_use]
pub fn terminal_master_name(fd: std::os::fd::RawFd) -> Option<String> {
    // Wide enough for every terminal name these systems report: `/dev/pts/0`
    // through `/dev/pts/1048575` on Linux, `/dev/ttys009` on macOS.
    let mut name = [0 as libc::c_char; 128];
    if unsafe { ptsname_r(fd, name.as_mut_ptr(), name.len()) } != 0 {
        return None;
    }
    // The call writes the name and its terminating zero into the buffer.
    let written = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
    written.to_str().ok().map(str::to_string)
}

/// What a [`Waker`] is built on: the kernel's own one-descriptor notification.
///
/// Linux and FreeBSD have `eventfd`; the other BSDs have a kernel event queue,
/// whose descriptor a `poll` can wait on just as well. Anywhere else the type
/// is named but never built — [`Waker::new`] yields `None` there — so the
/// reader blocks in `read` the way it does on Windows.
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

/// The one descriptor a pane's reader waits on beside its terminal, so another
/// thread can bring it back from that wait.
///
/// A doorbell. Ringing it means *something about this pane changed*: its child
/// was reaped, or the backend is parking its readers. The reader reads which of
/// those it was from the pane's own state, never from the doorbell.
///
/// A ring stays pending until the reader drains it, so one that lands before
/// the reader reaches its wait is still there when it does and can never be
/// missed. [`drain`](Waker::drain) is what takes it back off, so the next wait
/// blocks again.
#[cfg(unix)]
struct Waker(WakerInner);

#[cfg(unix)]
impl Waker {
    /// A waker nothing has woken yet, or `None` where the platform offers no
    /// one-descriptor notification.
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    fn new() -> Option<Self> {
        use nix::sys::eventfd::{EfdFlags, EventFd};

        // Close-on-exec, so a pane opened now is not handed to every child
        // spawned after it. A kernel event queue is never inherited, so the
        // other arm needs no equivalent.
        //
        // Non-blocking, so `drain` answers at once whether or not the doorbell
        // is ringing, the same way the other arm's zero timeout does.
        EventFd::from_flags(EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK)
            .ok()
            .map(Waker)
    }

    /// A waker nothing has woken yet: a kernel event queue carrying one
    /// user-triggered event, registered here so [`wake`](Waker::wake) only has
    /// to fire it.
    ///
    /// `EV_CLEAR` is what makes [`drain`](Waker::drain) work: fetching the
    /// event resets it, so the queue reads as quiet again.
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

        let queue = Kqueue::new().ok()?;
        let add = KEvent::new(
            0,
            EventFilter::EVFILT_USER,
            EventFlag::EV_ADD | EventFlag::EV_CLEAR,
            FilterFlag::empty(),
            0,
            0,
        );
        queue.kevent(&[add], &mut [], None).ok()?;
        Some(Waker(queue))
    }

    /// No one-descriptor notification here, so a pane's reader blocks in
    /// `read` instead of waiting.
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
    fn wake(&self) {
        let _ = self.0.write(1);
    }

    /// Take the ring back off, so the next wait blocks again: reading the
    /// count returns it to zero.
    ///
    /// The descriptor is non-blocking, so this returns whether or not the
    /// doorbell was ringing.
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    fn drain(&self) {
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
    fn wake(&self) {
        use nix::sys::event::{EventFilter, EventFlag, FilterFlag, KEvent};

        let trigger = KEvent::new(
            0,
            EventFilter::EVFILT_USER,
            EventFlag::empty(),
            FilterFlag::NOTE_TRIGGER,
            0,
            0,
        );
        let _ = self.0.kevent(&[trigger], &mut [], None);
    }

    /// Take the ring back off, so the next wait blocks again: the event is
    /// registered with `EV_CLEAR`, so fetching it resets it.
    ///
    /// The fetch carries a zero timeout, so it returns whether or not the
    /// doorbell was ringing.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    fn drain(&self) {
        use nix::sys::event::{EventFilter, EventFlag, FilterFlag, KEvent};

        let mut fetched = [KEvent::new(
            0,
            EventFilter::EVFILT_USER,
            EventFlag::empty(),
            FilterFlag::empty(),
            0,
            0,
        )];
        let at_once = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let _ = self.0.kevent(&[], &mut fetched, Some(at_once));
    }

    /// Nothing waits here, so nothing has to be woken.
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
    fn wake(&self) {}

    /// Nothing rings here, so nothing has to be drained.
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
    fn drain(&self) {}
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
    paused: bool,
    /// Readers inside their pump, counted from before the thread starts.
    live: usize,
    /// Readers waiting at the park.
    parked: usize,
}

/// What tells a pane's reader why its doorbell rang, and where to park.
///
/// The doorbell says only that something changed; `exited` and `gate` are the
/// two things it can have been.
#[cfg(unix)]
struct ReaderSignals<'a> {
    /// The doorbell, waited on beside the pane's terminal.
    wake: &'a Waker,
    /// Set by the watcher before it rings: the child has been reaped.
    exited: &'a AtomicBool,
    /// Where the reader parks while the backend holds its readers.
    gate: &'a ReaderGate,
}

/// Holds every pane's reader at the top of its round, so the backend can stop
/// reading terminals without ending a thread.
///
/// A reader parks before it waits on its terminal, so a parked reader holds no
/// chunk and has read nothing it did not hand over. It keeps its `Delivery`
/// throughout, and with it the pane's exit channel, so it can be put back to
/// work in the same process.
///
/// [`wait_all_parked`](ReaderGate::wait_all_parked) has no deadline: every
/// counted reader either reaches the park or leaves its pump. A round only
/// waits, reads, and hands one chunk over, and handing a chunk over is a send
/// on an unbounded channel, which never blocks.
#[cfg(unix)]
struct ReaderGate {
    /// The counts and the pause flag, read and written as one.
    state: Mutex<GateState>,
    /// Wakes a parked reader on resume, and the pause on every count change.
    waiting: Condvar,
}

#[cfg(unix)]
impl ReaderGate {
    /// A gate holding nobody, with no reader counted yet.
    fn new() -> Self {
        ReaderGate {
            state: Mutex::new(GateState::default()),
            waiting: Condvar::new(),
        }
    }

    /// Count one reader in and hand back its place. Taken before the thread
    /// starts, so a pause that lands first still waits for that reader to
    /// reach the park.
    fn enter(self: &Arc<Self>) -> ReaderTicket {
        self.state.lock().expect("reader gate").live += 1;
        ReaderTicket(Arc::clone(self))
    }

    /// Park here while the gate is paused. Returns at once otherwise, so an
    /// unpaused round costs one uncontended lock.
    fn park_if_paused(&self) {
        let mut state = self.state.lock().expect("reader gate");
        if !state.paused {
            return;
        }
        state.parked += 1;
        self.waiting.notify_all();
        while state.paused {
            state = self.waiting.wait(state).expect("reader gate");
        }
        state.parked -= 1;
    }

    /// Tell every reader to park at the top of its next round. Ringing each
    /// pane's doorbell is what brings a reader waiting on a quiet terminal to
    /// that top.
    fn pause(&self) {
        self.state.lock().expect("reader gate").paused = true;
    }

    /// Wait until every counted reader has parked. A reader that left its pump
    /// is no longer counted, so it settles this too.
    fn wait_all_parked(&self) {
        let mut state = self.state.lock().expect("reader gate");
        while state.parked != state.live {
            state = self.waiting.wait(state).expect("reader gate");
        }
    }

    /// Put every parked reader back to work.
    fn resume(&self) {
        self.state.lock().expect("reader gate").paused = false;
        self.waiting.notify_all();
    }
}

/// One reader's place in the gate, released when its pump ends.
///
/// Dropped by the reader thread itself the moment it leaves the pump, and by
/// the runtime if that thread panics, so a reader that will never park again is
/// never waited on.
#[cfg(unix)]
struct ReaderTicket(Arc<ReaderGate>);

#[cfg(unix)]
impl ReaderTicket {
    /// The gate this place is in, which the reader parks at.
    fn gate(&self) -> &ReaderGate {
        &self.0
    }
}

#[cfg(unix)]
impl Drop for ReaderTicket {
    fn drop(&mut self) {
        self.0.state.lock().expect("reader gate").live -= 1;
        self.0.waiting.notify_all();
    }
}

impl Delivery {
    /// Deliver one chunk of `pane`'s output. `false` means the reader should
    /// stop delivering this pane: its exit is already settled and the consumer
    /// has let it go, or the consumer refused the chunk. The reader lets the
    /// consumer go there; on Windows it stays in `read` afterwards, discarding,
    /// because closing the pane's console waits for its output to be read out.
    ///
    /// Takes the chunk borrowed and copies it after claiming it below. A
    /// settled pane copies nothing.
    fn output(&self, pane: PaneId, bytes: &[u8]) -> bool {
        match self {
            Delivery::Channel(sender) => sender.send(bytes.to_vec()).is_ok(),
            Delivery::Sink { sink, handover, .. } => {
                // Checked and claimed in one step, so a reader holding a chunk
                // never reads as idle to the watcher.
                {
                    let mut held = handover.lock().expect("handover");
                    if held.settled {
                        return false;
                    }
                    held.begun += 1;
                }
                // The consumer is called with the lock released: it runs for
                // as long as it likes, and the watcher can still read the
                // chunk as in flight throughout.
                let taken = sink.output(pane, bytes.to_vec());
                let mut held = handover.lock().expect("handover");
                held.done += 1;
                // A consumer that refuses a chunk is done with this pane, so
                // it must not be handed an exit afterwards. Settled in the
                // same step that releases the chunk.
                if !taken {
                    held.settled = true;
                }
                taken
            }
        }
    }

    /// Whether this pane's exit is already settled, so its reader can stop
    /// without taking another chunk. Always `false` under a channel consumer,
    /// which settles nothing.
    ///
    /// Read by [`pump_waited`], the one reader that can be brought back from
    /// its wait to ask.
    #[cfg(unix)]
    fn settled(&self) -> bool {
        match self {
            Delivery::Channel(_) => false,
            Delivery::Sink { handover, .. } => handover.lock().expect("handover").settled,
        }
    }

    /// Report `pane`'s child as ended, once its output is exhausted. A sink
    /// waits here for the watcher's status; a channel consumer reads the exit
    /// off its own handle, so there is nothing to do.
    ///
    /// Returns straight away if this pane's exit is already settled — the
    /// watcher delivered it because the PTY never reported an end, the
    /// consumer refused a chunk, or [`kill`](PortablePtyBackend::kill) closed
    /// the pane. The consumer is told at most once either way, and not waiting
    /// for a status nobody will publish is what keeps a child that outlives
    /// its consumer from pinning the reader thread for as long as it runs.
    fn finish(self, pane: PaneId) {
        if let Delivery::Sink {
            sink,
            exit_receiver,
            handover,
        } = self
        {
            if handover.lock().expect("handover").settled {
                return;
            }
            let Ok(status) = exit_receiver.recv() else {
                return;
            };
            {
                let mut held = handover.lock().expect("handover");
                if held.settled {
                    return;
                }
                held.settled = true;
            }
            sink.exit(pane, status);
        }
    }
}

/// Hand every chunk of `pane`'s output to `delivery`, blocking in `read` until
/// the terminal reports an end or the consumer lets the pane go.
///
/// The pump for a terminal that cannot be waited on: Windows, and a Unix
/// terminal that exposes no descriptor. Nothing interrupts a `read` here, so
/// the thread stays in it until the last process holding the terminal open
/// releases it.
///
/// `true` is the terminal reaching its end, so the caller can report the
/// child's exit behind the output. `false` is the consumer letting the pane go
/// mid-stream, which leaves the terminal still open.
fn pump_blocking(reader: &mut dyn Read, delivery: &Delivery, pane: PaneId) -> bool {
    let mut buf = [0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return true,
            Ok(n) => {
                if !delivery.output(pane, &buf[..n]) {
                    return false;
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return true,
        }
    }
}

/// The cursor-position request a Windows pseudoconsole writes to its output as
/// it opens. A pseudoconsole created with `PSEUDOCONSOLE_INHERIT_CURSOR` holds
/// its child's output until the process that created it answers this on the
/// pseudoconsole's input.
#[cfg(any(windows, test))]
const CURSOR_REQUEST: &[u8] = b"\x1b[6n";

/// The answer to [`CURSOR_REQUEST`]: the cursor is at row 1, column 1. Queued
/// on a pane's input as the pane opens.
#[cfg(windows)]
const CURSOR_AT_HOME: &[u8] = b"\x1b[1;1R";

/// Reads a pane's output and takes the first [`CURSOR_REQUEST`] out of it.
///
/// [`spawn`](PortablePtyBackend::spawn) queues the answer to that request on
/// the pane's input; this only keeps the request itself from reaching the
/// consumer. Output ahead of it is delivered, less any tail that is still a
/// prefix of it, which is held until the next read settles it. Every byte after
/// it passes through untouched, so a second request — one the pane's own
/// program made — is delivered.
///
/// Before → after: the terminal writes `\x1b[6nhello`, the output delivers
/// `hello`.
#[cfg(any(windows, test))]
struct RemovesCursorRequest<R: Read> {
    /// The pane's output.
    inner: R,
    /// Output read but not yet handed to the caller, oldest first.
    pending: Vec<u8>,
    /// Bytes held back because they are the start of the request and the rest
    /// of it has not been read yet. At most one byte short of the request.
    held: Vec<u8>,
    /// `true` once the request has been taken out. Every read after this passes
    /// straight through.
    done: bool,
}

#[cfg(any(windows, test))]
impl<R: Read> RemovesCursorRequest<R> {
    /// Read `inner`, taking the request out of what it hands back.
    fn new(inner: R) -> Self {
        Self {
            inner,
            pending: Vec::new(),
            held: Vec::new(),
            done: false,
        }
    }

    /// Move up to `buf.len()` bytes of [`pending`](Self::pending) into `buf`,
    /// and return how many moved.
    fn drain_pending(&mut self, buf: &mut [u8]) -> usize {
        let take = self.pending.len().min(buf.len());
        buf[..take].copy_from_slice(&self.pending[..take]);
        self.pending.drain(..take);
        take
    }
}

/// Where `needle` starts in `haystack`.
#[cfg(any(windows, test))]
fn position_of(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// How many bytes at the end of `haystack` are a prefix of `needle`. `0` when
/// none are. Never counts `needle` whole: at most `needle.len() - 1`.
#[cfg(any(windows, test))]
fn partial_tail(haystack: &[u8], needle: &[u8]) -> usize {
    let longest = haystack.len().min(needle.len() - 1);
    (1..=longest)
        .rev()
        .find(|&len| haystack[haystack.len() - len..] == needle[..len])
        .unwrap_or(0)
}

#[cfg(any(windows, test))]
impl<R: Read> Read for RemovesCursorRequest<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if !self.pending.is_empty() {
                return Ok(self.drain_pending(buf));
            }
            if self.done {
                return self.inner.read(buf);
            }
            let read = self.inner.read(buf)?;
            if read == 0 {
                // The terminal ended with no request on it: what was held back
                // is delivered as output.
                self.pending = std::mem::take(&mut self.held);
                self.done = true;
                if self.pending.is_empty() {
                    return Ok(0);
                }
                continue;
            }

            let mut seen = std::mem::take(&mut self.held);
            seen.extend_from_slice(&buf[..read]);
            match position_of(&seen, CURSOR_REQUEST) {
                Some(at) => {
                    seen.drain(at..at + CURSOR_REQUEST.len());
                    self.done = true;
                }
                None => {
                    let keep = seen.len() - partial_tail(&seen, CURSOR_REQUEST);
                    self.held = seen.split_off(keep);
                }
            }
            self.pending = seen;
            // Everything read was the request or the start of it. Read again:
            // `Ok(0)` here would report an end the terminal has not reached.
            if self.pending.is_empty() {
                continue;
            }
        }
    }
}

/// Read `reader` to its end, discarding everything.
///
/// Closing a pane's console waits for the output it still holds to be read
/// out. The pane's reader runs this once its consumer has let the pane go:
/// those bytes belong to a pane nobody is listening to, but the console's
/// close still waits for them.
#[cfg(windows)]
fn drain_terminal(reader: &mut dyn Read) {
    let mut buf = [0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

/// Hand every chunk of `pane`'s output to `delivery`, waiting on the terminal
/// rather than blocking in `read`, so `wake` can bring the reader back.
///
/// While the child runs, the wait carries no timer, so an idle pane costs no
/// wakeups. It ends when the terminal has something, or when `wake` stirs.
///
/// The watcher rings `wake` once it has reaped the child, and that is what
/// reaches a reader a descendant holding the terminal open would otherwise keep
/// waiting for as long as that descendant runs. The ring stays pending until
/// this pump drains it, so one that lands before the reader reaches its wait is
/// still there when it does.
///
/// The doorbell only says something changed, so the pump reads `exited` to see
/// what. The grace rounds start when that flag says the child is reaped, never
/// because the descriptor stirred: the watcher stores the flag before it rings,
/// and `gate` rings the same doorbell to bring the reader to its park. From
/// then on the wait runs in rounds of `grace`, and a round in which the
/// terminal produces nothing ends the pump: a dead child cannot write again, so
/// everything it printed has been handed over and the caller can publish the
/// exit behind it. Rounds stop `limit` after the ring, which bounds a
/// descendant that holds the terminal open and keeps printing.
///
/// Each round opens at `gate`'s park, before the wait, so a reader held there
/// has read nothing it did not hand over.
///
/// [`kill`](PortablePtyBackend::kill) settles the pane and then kills the
/// child, so the ring the watcher fires on reaping it finds the pane settled
/// and the pump stops rather than starting a round.
#[cfg(unix)]
fn pump_waited(
    delivery: &Delivery,
    pane: PaneId,
    terminal: &std::os::fd::OwnedFd,
    signals: ReaderSignals<'_>,
    grace: Duration,
    limit: Duration,
) {
    use nix::errno::Errno;
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::AsFd;

    /// Whether the wait reported anything at all on `fd`. Bytes, a slave that
    /// closed, and a descriptor that went bad all mean the same thing here:
    /// call `read` and let it say which.
    fn stirred(fd: &PollFd) -> bool {
        fd.revents().is_some_and(|revents| !revents.is_empty())
    }

    let round = PollTimeout::try_from(grace).unwrap_or(PollTimeout::MAX);
    let mut buf = [0u8; READ_CHUNK];
    // `Some` from the moment the child is known to have gone, carrying the
    // point the rounds stop at.
    let mut ends_at: Option<Instant> = None;

    loop {
        signals.gate.park_if_paused();

        let mut woken = false;
        let readable = match ends_at {
            None => {
                let mut fds = [
                    PollFd::new(terminal.as_fd(), PollFlags::POLLIN),
                    PollFd::new(signals.wake.as_fd(), PollFlags::POLLIN),
                ];
                match poll(&mut fds, PollTimeout::NONE) {
                    Ok(_) => {}
                    // A signal caught during the wait is not an end: wait again.
                    Err(Errno::EINTR) => continue,
                    Err(_) => return,
                }
                if stirred(&fds[1]) {
                    // Take the ring off first, so the next round waits again.
                    // A ring that lands after this leaves the descriptor
                    // readable, so that round sees it too.
                    signals.wake.drain();
                    if delivery.settled() {
                        return;
                    }
                    // A ring from the gate leaves the rounds alone: the child
                    // is still running, and the pump loops back to the park.
                    if signals.exited.load(Ordering::SeqCst) {
                        ends_at = Some(Instant::now() + limit);
                        woken = true;
                    }
                }
                stirred(&fds[0])
            }
            Some(_) => {
                let mut fds = [PollFd::new(terminal.as_fd(), PollFlags::POLLIN)];
                match poll(&mut fds, round) {
                    Ok(_) => {}
                    Err(Errno::EINTR) => continue,
                    Err(_) => return,
                }
                stirred(&fds[0])
            }
        };

        if readable {
            match read_terminal(terminal, &mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    if !delivery.output(pane, &buf[..n]) {
                        return;
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        } else if ends_at.is_some() && !woken {
            // A whole round with nothing from a terminal whose child has gone.
            // The round the wake itself landed in is not one of these: it ran
            // until the wake arrived, not for its own length.
            return;
        }

        if ends_at.is_some_and(|end| Instant::now() >= end) {
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
/// carried a value — [`kill`](PortablePtyBackend::kill) closing the pane — or
/// its sender was dropped, which is the backend shutting down. `kill` settles
/// the exit before it sends, so returning here promptly keeps its join short.
fn should_publish_exit(
    cancel: &Receiver<()>,
    handover: &Mutex<Handover>,
    deadline: Instant,
    grace: Duration,
) -> bool {
    use std::sync::mpsc::RecvTimeoutError;

    loop {
        match cancel.recv_timeout(grace) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return false,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let (settled, in_flight) = {
            let held = handover.lock().expect("handover");
            (held.settled, held.in_flight())
        };
        if settled {
            return false;
        }
        if Instant::now() >= deadline && !in_flight {
            return true;
        }
    }
}

/// Wait until `pane`'s reader is back reading its terminal, so the terminal can
/// be closed.
///
/// Windows waits for a pseudoconsole's output pipe to be read out before
/// `ClosePseudoConsole` returns, and the pane's reader is that pipe's one
/// reader. A reader still handing a chunk to its consumer is not reading it, so
/// the wait happens here, where [`kill`](PortablePtyBackend::kill) can end it.
///
/// Returns once no chunk is in the consumer's hands, or once `cancel` carries a
/// value — `kill` closing the pane — or its sender is dropped, which is the
/// backend shutting down. `kill` lets the pane's parked send go before it
/// sends, so the reader is on its way back to the terminal either way.
#[cfg(windows)]
fn wait_for_the_reader_to_read_again(
    cancel: &Receiver<()>,
    handover: &Mutex<Handover>,
    check_in: Duration,
) {
    loop {
        if !handover.lock().expect("handover").in_flight() {
            return;
        }
        match cancel.recv_timeout(check_in) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// A pane's terminal, as the backend holds it for resizing.
///
/// The `Owned` arm is one descriptor the pane opened for itself, shared with
/// its reader and writer threads, so the whole pane spends a single descriptor
/// on its terminal. `Crate` keeps `portable-pty`'s master instead, for a
/// terminal that exposes no descriptor to share.
enum Terminal {
    /// The pane's own descriptor, also held by its reader and writer.
    #[cfg(unix)]
    Owned(Arc<std::os::fd::OwnedFd>),
    /// `portable-pty`'s master, resized through the crate.
    ///
    /// A slot the watcher shares, so it can take the master out and drop it
    /// once the child is reaped — which is how a Windows pane's terminal is
    /// closed. Empty from that point on.
    Crate(Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>),
}

impl Terminal {
    /// Tell the child its terminal is now `size`. A terminal already closed
    /// takes the new size as a no-op.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] if the kernel refuses the new size.
    fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        let failed = |detail: String| PtyError::Io { detail };
        match self {
            #[cfg(unix)]
            Terminal::Owned(fd) => resize_terminal(fd, size).map_err(|e| failed(e.to_string())),
            Terminal::Crate(slot) => match slot.lock().expect("terminal").as_ref() {
                Some(master) => master
                    .resize(to_pp_size(size))
                    .map_err(|e| failed(e.to_string())),
                // The child is gone and its terminal closed with it, so there
                // is nothing left to retune.
                None => Ok(()),
            },
        }
    }
}

/// What a pane's reader thread drains its terminal with.
enum ReadSide {
    /// The pane's own descriptor, waited on beside a [`Waker`] so the reader
    /// can be brought back from that wait.
    #[cfg(unix)]
    Owned(Arc<std::os::fd::OwnedFd>, Arc<Waker>),
    /// `portable-pty`'s reader, blocked in `read` until the terminal ends.
    Crate(Box<dyn Read + Send>),
}

/// What a pane's writer thread sends the child's input with.
enum WriteSide {
    /// The pane's own descriptor — the same one its reader holds, since the
    /// two directions of a terminal share it.
    #[cfg(unix)]
    Owned(Arc<std::os::fd::OwnedFd>),
    /// `portable-pty`'s writer, which reports the end of input itself when
    /// dropped.
    Crate(Box<dyn Write + Send>),
}

/// What the per-pane writer thread accepts on its channel.
enum WriterMsg {
    /// Bytes to write to the child's stdin.
    Bytes(Vec<u8>),
    /// Answer on this channel, which the writer does once it reaches this
    /// message. Queued by [`PortablePtyBackend::flush_writers`], and answered
    /// after every earlier [`WriterMsg::Bytes`] has been written.
    Barrier(Sender<()>),
    /// Stop and release the thread: queued by the watcher once the child has
    /// exited, so the writer never has to wake on a timer to notice.
    Stop,
}

/// What a pane's watcher thread does after it has reaped the child, chosen by
/// how that pane's terminal can reach the end of its output.
enum WatcherTail {
    /// Nothing: the reader reaches the end on its own and publishes the exit.
    #[cfg(not(windows))]
    ReaderPublishes,
    /// Close the pane's terminal so the reader reaches the end, and stand by
    /// on a deadline for the exit. Dropping the master closes the console,
    /// which flushes its remaining output and then ends the reader's pipe. The
    /// close runs on a thread of its own and returns only once every process
    /// attached to that console has let it go. The standby publishes the exit
    /// to `sink` when the reader has not settled it by the deadline.
    #[cfg(windows)]
    CloseTerminal {
        /// The pane's master, taken out and dropped to close the console.
        terminal: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
        /// Where the standby publishes the exit. `None` under a channel
        /// consumer, which reads the exit off its own handle. Held weakly, so a
        /// watcher waiting on a long-lived child keeps no consumer alive.
        sink: Option<Weak<dyn PtySink>>,
    },
    /// Stand by on a deadline and publish the exit to this sink, for a terminal
    /// the reader cannot be brought back from. Held weakly, so a watcher
    /// waiting on a long-lived child keeps no consumer alive.
    #[cfg(not(windows))]
    StandBy(Weak<dyn PtySink>),
}

/// Give `pane`'s reader until the standby's deadline to settle the exit, then
/// settle it here and publish `status` if the reader never did.
///
/// The watcher's whole tail once the child is reaped, on both paths that have
/// one: a Unix terminal exposing no descriptor, and a Windows pane whose
/// console is being closed. Nothing is published when `sink` no longer has an
/// owner — the consumer is gone and the exit has nowhere to go.
fn stand_by_for_the_reader(
    cancel: &Receiver<()>,
    handover: &Mutex<Handover>,
    sink: &Weak<dyn PtySink>,
    pane: PaneId,
    status: ExitStatus,
) {
    let publish = should_publish_exit(
        cancel,
        handover,
        Instant::now() + EXIT_PUBLISH_LIMIT,
        EXIT_PUBLISH_GRACE,
    );
    if let Some(sink) = sink.upgrade() {
        settle_and_publish(&sink, handover, pane, status, publish);
    }
}

/// Settle `pane`'s exit, and hand `status` to `sink` when `publish` is `true`
/// and nothing settled it first.
///
/// Settles whether or not it publishes. `publish` is
/// [`should_publish_exit`]'s answer.
fn settle_and_publish(
    sink: &Arc<dyn PtySink>,
    handover: &Mutex<Handover>,
    pane: PaneId,
    status: ExitStatus,
    publish: bool,
) {
    let already_settled = {
        let mut held = handover.lock().expect("handover");
        let was = held.settled;
        held.settled = true;
        was
    };
    if publish && !already_settled {
        sink.exit(pane, status);
    }
}

/// Kills the wrapped child on drop unless [`disarm`](ChildGuard::disarm)ed.
///
/// Dropping a `portable-pty` child does not terminate the process, so this
/// guards [`spawn`](PortablePtyBackend::spawn)'s fallible setup: if any step
/// after launch returns early, the child is killed rather than leaked as an
/// orphan with no owner. Once the watcher thread takes ownership of the child,
/// the guard is disarmed.
struct ChildGuard(Option<Box<dyn Child + Send + Sync>>);

impl ChildGuard {
    fn new(child: Box<dyn Child + Send + Sync>) -> Self {
        ChildGuard(Some(child))
    }

    /// Take the child out, leaving the guard inert (no kill on drop).
    fn disarm(mut self) -> Box<dyn Child + Send + Sync> {
        self.0.take().expect("child present until disarmed")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
        }
    }
}

impl std::ops::Deref for ChildGuard {
    type Target = dyn Child + Send + Sync;

    fn deref(&self) -> &Self::Target {
        self.0.as_deref().expect("child present until disarmed")
    }
}

/// Start a pane's writer thread on `side`, and hand back the channel its input
/// is queued on.
///
/// The thread parks in `recv` with no timer, so an idle pane costs no wakeups.
/// It ends on either teardown path: the channel closing, or the
/// [`WriterMsg::Stop`] the watcher queues once the child is gone. `Stop`
/// travels the same channel as the bytes, so every write queued before the
/// child exited is written first. Nothing is written to the terminal on the way
/// out.
///
/// A [`WriterMsg::Barrier`] travels that same channel and is answered where it
/// sits in it, so its answer means every byte queued before it is on the
/// terminal.
fn start_writer(side: WriteSide) -> Sender<WriterMsg> {
    let (writer_sender, writer_receiver) = channel::<WriterMsg>();

    let _ = spawn_pty_thread("koshi-pty-write", move || {
        let mut write_side = side;
        while let Ok(message) = writer_receiver.recv() {
            match message {
                WriterMsg::Bytes(bytes) => match &mut write_side {
                    #[cfg(unix)]
                    WriteSide::Owned(terminal) => {
                        let _ = write_terminal(terminal, &bytes);
                    }
                    WriteSide::Crate(writer) => {
                        let _ = writer.write_all(&bytes).and_then(|_| writer.flush());
                    }
                },
                WriterMsg::Barrier(answer) => {
                    let _ = answer.send(());
                }
                WriterMsg::Stop => break,
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
    terminal: Arc<std::os::fd::OwnedFd>,
    wake: Arc<Waker>,
    exited: Arc<AtomicBool>,
    ticket: ReaderTicket,
) -> JoinHandle<()> {
    spawn_pty_thread("koshi-pty-read", move || {
        pump_waited(
            &delivery,
            pane_id,
            &terminal,
            ReaderSignals {
                wake: &wake,
                exited: &exited,
                gate: ticket.gate(),
            },
            EXIT_PUBLISH_GRACE,
            EXIT_PUBLISH_LIMIT,
        );
        drop(ticket);
        delivery.finish(pane_id);
    })
}

/// The pane threads a watcher releases once it holds the child's exit status.
struct WatchRelease {
    /// Flipped so [`kill`](PortablePtyBackend::kill) never signals a reaped
    /// child.
    exited: Arc<AtomicBool>,
    /// The status itself, kept on the pane so
    /// [`carried_panes`](PortablePtyBackend::carried_panes) can hand it to the
    /// next process image.
    exit: Arc<OnceLock<ExitStatus>>,
    /// Carries the status to whoever publishes it.
    exit_sender: Sender<ExitStatus>,
    /// Releases the pane's writer thread.
    writer_stop: Sender<WriterMsg>,
    /// The reader's doorbell, rung so it takes the last of the child's output.
    /// `None` for a reader that cannot be brought back from its `read`.
    #[cfg(unix)]
    wake: Option<Arc<Waker>>,
}

impl WatchRelease {
    /// Record `status` on the pane, mark the child gone, hand the status on,
    /// release the writer, and ring the reader's doorbell.
    ///
    /// The status is stored before the flag, so a watcher's flag is never seen
    /// ahead of its status.
    fn publish(&self, status: ExitStatus) {
        let _ = self.exit.set(status);
        self.exited.store(true, Ordering::SeqCst);
        let _ = self.exit_sender.send(status);
        let _ = self.writer_stop.send(WriterMsg::Stop);
        #[cfg(unix)]
        if let Some(wake) = &self.wake {
            wake.wake();
        }
    }
}

/// Wait for child `pid` to end, and report how it ended.
///
/// What a pane taken back through [`adopt`](PortablePtyBackend::adopt) reaps
/// its child with, when no image before it saw the child end: the swap left the
/// process id but no `portable-pty` child to wait on. An interrupted wait is
/// retried, and a stop or a continue is not an end. `ECHILD` — the child was
/// reaped elsewhere — is reported as [`ExitStatus::ExitCode`]`(-1)`, the same
/// value a failed wait reports.
#[cfg(unix)]
fn wait_for_child(pid: u32) -> ExitStatus {
    use nix::errno::Errno;
    use nix::sys::wait::{waitpid, WaitStatus};
    use nix::unistd::Pid;

    let pid = Pid::from_raw(pid as i32);
    loop {
        match waitpid(pid, None) {
            Ok(WaitStatus::Exited(_, code)) => return ExitStatus::ExitCode(code),
            Ok(WaitStatus::Signaled(_, signal, _)) => return ExitStatus::Signaled(signal as i32),
            Ok(_) => {}
            Err(Errno::EINTR) => {}
            Err(_) => return ExitStatus::ExitCode(-1),
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
/// One descriptor carries both of those directions, so a pane holds a single
/// copy of the master and its reader, writer and resize all use that one. With
/// the reader's waker, a pane spends two descriptors in total.
pub struct PaneEntry {
    /// This pane's terminal. Held so the kernel keeps the pair open and so
    /// [`resize`](PortablePtyBackend::resize) can retune the window size.
    terminal: Terminal,
    /// The last size this pane's terminal was set to: what it was spawned or
    /// taken back at, then whatever the newest successful
    /// [`resize`](PortablePtyBackend::resize) carried.
    size: PtySize,
    /// Input channel to the per-pane writer thread. Bytes sent here are written
    /// to the master (and so reach the child) off the dispatcher, so a child that
    /// has stopped reading its stdin blocks only the writer thread, never the
    /// dispatcher.
    ///
    /// What releases the writer is the watcher's [`WriterMsg::Stop`], queued
    /// once the child has exited — including for a pane left open past its
    /// child's death. This is not the only `Sender` on the channel: the watcher
    /// holds a clone to send that `Stop` with, so dropping this one (on
    /// `kill`/teardown) leaves the channel open until the watcher ends too.
    /// `kill` joins the watcher, so by the time it returns the writer has been
    /// released either way.
    ///
    /// A writer already blocked *inside*
    /// `write_all` — child stopped reading while a `setsid` descendant still holds
    /// the slave open (Linux; macOS `revoke`s it) — cannot be interrupted; like the
    /// reader it detaches and exits only once that fd finally closes. `kill` never
    /// joins it, so this can leak a thread + fd in that case but never blocks the
    /// dispatcher.
    /// [`flush_writers`](PortablePtyBackend::flush_writers) sends its barrier on
    /// this same channel, and names the pane whose writer is in that state.
    writer: Sender<WriterMsg>,
    /// Kill handle for the child process; `kill()` sends the terminating signal.
    killer: PtyChildKillControl,
    /// Flipped to `true` by the watcher thread the moment the child exits; read
    /// by [`kill`](PortablePtyBackend::kill) to avoid signalling a dead process.
    exited: Arc<AtomicBool>,
    /// How the child ended, filled in by the watcher thread alongside `exited`.
    /// Empty while the child runs. Read by
    /// [`carried_panes`](PortablePtyBackend::carried_panes), which is how a
    /// status this process observed reaches the process image that replaces it:
    /// a reaped child cannot be waited on twice.
    exit: Arc<OnceLock<ExitStatus>>,
    /// Reader thread: drains the pane's terminal to wherever this backend
    /// delivers — the handle's output channel, or the sink. Under a sink it also
    /// publishes the child's exit, once that output has run dry, which keeps
    /// that exit behind the last of the child's output.
    ///
    /// Not joined on teardown: the slave fd may outlive the child (e.g., when the
    /// child `setsid`s into a new process group), so the thread could block forever
    /// if joined. It exits once the fd closes, and under a sink it lets the
    /// consumer go on the first chunk it reads after the pane's exit is settled
    /// — those bytes belong to a pane the consumer has let go. Retained so the
    /// struct owns the handle.
    ///
    /// Where the terminal can be waited on, the thread comes back on
    /// `reader_wake` and ends within one round of the pane being closed,
    /// whatever still holds the slave open. That doorbell also parks it:
    /// [`pause_readers`](PortablePtyBackend::pause_readers) rings it to bring
    /// the thread to the top of its round and hold it there.
    ///
    /// On Windows it blocks in `read`
    /// until the watcher closes the pane's terminal, which flushes the console
    /// and ends the pipe this thread is reading; it reads that console to its
    /// end even once the consumer has let the pane go, because the close waits
    /// for it — it just discards what it reads from then on. On a Unix terminal
    /// that
    /// exposes no descriptor it blocks in `read` until that fd closes, the same
    /// way the writer can, and the watcher publishes the exit instead; the
    /// pane's end never depends on this thread getting there.
    #[expect(dead_code)]
    reader: JoinHandle<()>,
    /// The reader's doorbell, the same one this pane's watcher rings.
    ///
    /// Rung by [`pause_readers`](PortablePtyBackend::pause_readers) to bring
    /// the reader to its park. `None` for a terminal that exposes no
    /// descriptor: that reader blocks in `read` and can never reach the park.
    #[cfg(unix)]
    reader_wake: Option<Arc<Waker>>,
    /// Watcher thread: blocks on the child, records exit status, flips `exited`.
    watcher: JoinHandle<()>,
    /// Whether this pane's exit is settled — the state the reader and watcher
    /// share. [`kill`](PortablePtyBackend::kill) sets it before killing
    /// anything, so a caller closing a pane is never handed an exit for it by
    /// whichever thread gets there first. Under no sink nothing reads it.
    handover: Arc<Mutex<Handover>>,
    /// Wakes the watcher out of the wait it is in.
    ///
    /// [`kill`](PortablePtyBackend::kill) sends on this before joining the
    /// watcher, so tearing a pane down returns straight away instead of
    /// sitting through the rounds. What stops the exit being published is
    /// `handover.settled`, which `kill` sets first. Two waits listen: the
    /// standby of a Unix terminal that exposes no descriptor, and a Windows
    /// watcher waiting for the reader before it closes the terminal. A Unix
    /// pane that owns its descriptor has neither, so there the send is a
    /// no-op.
    exit_grace_cancel: Sender<()>,
}

/// One live pane, as a process about to replace its own image hands it on.
///
/// The descriptor and the process id are what the next image needs to take the
/// pane back; the size is what that image must record as the window the child
/// already has; the exit is how the child ended, when this process saw it end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CarriedPtyPane {
    /// The pane this record is for.
    pub pane_id: PaneId,
    /// The pane's own terminal descriptor. `None` for a terminal that exposes
    /// none, which no image can carry.
    #[cfg(unix)]
    pub terminal_fd: Option<std::os::fd::RawFd>,
    /// The child's process id, waited on again once the pane is taken back.
    pub pid: u32,
    /// The last size the pane's terminal was set to.
    pub size: PtySize,
    /// How the pane's child ended, if this process's watcher reaped it. `None`
    /// while the child runs, and the next image waits on the process id itself.
    pub exit: Option<ExitStatus>,
}

/// Real OS-PTY backend built on the `portable-pty` crate. Each spawned pane gets
/// a kernel PTY plus three helper threads (reader, writer, watcher); the backend
/// owns them all through the [`PaneEntry`] map.
pub struct PortablePtyBackend {
    /// Every live pane's PTY, threads, and kill handle, keyed by [`PaneId`].
    /// Locked: [`spawn`](PtyBackend::spawn), [`resize`](PtyBackend::resize),
    /// [`write`](PtyBackend::write), and [`kill`](PtyBackend::kill) can all be
    /// called from different dispatcher calls.
    panes: Mutex<HashMap<PaneId, PaneEntry>>,
    /// Where spawned panes deliver output and exit. `None` routes both through
    /// each pane's [`PtyHandle`] channels, which the caller polls or relays;
    /// `Some` has the reader thread hand them to the consumer directly, so no
    /// relay thread exists per pane.
    sink: Option<Arc<dyn PtySink>>,
    /// Every pane reader's park, shared by each reader thread that owns its
    /// terminal descriptor. Driven by
    /// [`pause_readers`](PortablePtyBackend::pause_readers) and
    /// [`resume_readers`](PortablePtyBackend::resume_readers).
    #[cfg(unix)]
    readers: Arc<ReaderGate>,
}

impl PortablePtyBackend {
    /// Creates a new, empty PTY backend with no active panes, delivering each
    /// pane's output and exit through its own [`PtyHandle`] channels.
    pub fn new() -> Self {
        PortablePtyBackend {
            panes: Mutex::new(HashMap::new()),
            sink: None,
            #[cfg(unix)]
            readers: Arc::new(ReaderGate::new()),
        }
    }

    /// Creates a new, empty PTY backend that hands every pane's output and exit
    /// to `sink` from the pane's own reader thread.
    ///
    /// This is the shape an event loop wants: no per-pane relay thread, since
    /// delivering a chunk is a single function call.
    pub fn with_sink(sink: Arc<dyn PtySink>) -> Self {
        PortablePtyBackend {
            panes: Mutex::new(HashMap::new()),
            sink: Some(sink),
            #[cfg(unix)]
            readers: Arc::new(ReaderGate::new()),
        }
    }

    /// Hold every pane's reader at the top of its round, so nothing is read
    /// from a terminal without being handed to the consumer.
    ///
    /// A reader parks before it waits on its terminal, so a paused backend has
    /// no chunk in anyone's hands and no byte read but undelivered. Each parked
    /// reader keeps its `Delivery`, and with it the pane's exit channel, so
    /// [`resume_readers`](PortablePtyBackend::resume_readers) puts it back to
    /// work in this same process. A reader whose child already ended is no
    /// longer counted and does not hold this up.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] naming the first pane whose terminal exposes no
    /// descriptor: that reader blocks in `read` and can never reach the park.
    /// Nothing is paused and nothing is rung in that case.
    #[cfg(unix)]
    pub fn pause_readers(&self) -> Result<(), PtyError> {
        {
            let panes = self.panes.lock().unwrap();
            for (pane, entry) in panes.iter() {
                if entry.reader_wake.is_none() {
                    let detail = format!(
                        "pane {pane} has no terminal descriptor, so its reader cannot park"
                    );
                    return Err(PtyError::Io { detail });
                }
            }
            // The flag is set before any doorbell rings, so a reader brought to
            // the top of its round always finds the gate paused.
            self.readers.pause();
            for wake in panes
                .values()
                .filter_map(|entry| entry.reader_wake.as_ref())
            {
                wake.wake();
            }
        }
        self.readers.wait_all_parked();
        Ok(())
    }

    /// This backend runs inside the process that holds the panes, and that
    /// process is never replaced, so its readers are never held still.
    #[cfg(windows)]
    pub fn pause_readers(&self) -> Result<(), PtyError> {
        Ok(())
    }

    /// Put every parked reader back to work. No byte is lost and no false exit
    /// is published: each reader carries on from the top of the round it parked
    /// in.
    #[cfg(unix)]
    pub fn resume_readers(&self) {
        self.readers.resume();
    }

    /// This backend's readers are never held still, so there is nothing to put
    /// back to work.
    #[cfg(windows)]
    pub fn resume_readers(&self) {}

    /// Wait until every pane's writer thread has written what it was handed, so
    /// no byte this backend took for a child is still queued.
    ///
    /// Each pane is sent a barrier on the channel its bytes travel, so an
    /// answer to that barrier means every byte queued before it is on the
    /// pane's terminal. A pane whose writer thread has already ended is passed
    /// over: its child is gone, so nothing is waiting to be told anything.
    ///
    /// This is the write direction of what
    /// [`pause_readers`](PortablePtyBackend::pause_readers) does for the read
    /// direction. A process about to replace its own image needs both: the
    /// writer threads and their queues die with the old image.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] naming the first pane whose writer did not
    /// answer within one second, which bounds the whole call. That writer is
    /// blocked inside its write — a child that stopped reading its stdin does
    /// this — and the bytes behind it are still queued.
    pub fn flush_writers(&self) -> Result<(), PtyError> {
        let answers: Vec<(PaneId, Receiver<()>)> = {
            let panes = self.panes.lock().unwrap();
            panes
                .iter()
                .filter_map(|(pane, entry)| {
                    let (answer, answer_rx) = channel::<()>();
                    entry
                        .writer
                        .send(WriterMsg::Barrier(answer))
                        .ok()
                        .map(|()| (*pane, answer_rx))
                })
                .collect()
        };

        let deadline = Instant::now() + WRITER_FLUSH_LIMIT;
        for (pane, answer_rx) in answers {
            let left = deadline.saturating_duration_since(Instant::now());
            // A writer thread that ended between the send and here drops the
            // barrier with the rest of its queue, which reads as disconnected.
            if let Err(RecvTimeoutError::Timeout) = answer_rx.recv_timeout(left) {
                let detail =
                    format!("pane {pane} is still writing what it was handed, so it cannot settle");
                return Err(PtyError::Io { detail });
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
    /// neither, so a caller that finds a descriptor for every pane here knows
    /// every reader can park.
    ///
    /// A pane whose child this process already reaped carries the exit status
    /// its watcher observed. Readers being held still does not hold a watcher
    /// still, so a child that ends during the hand-over is reaped here and can
    /// never be waited on again.
    pub fn carried_panes(&self) -> Vec<CarriedPtyPane> {
        #[cfg(unix)]
        use std::os::fd::AsRawFd;

        let panes = self.panes.lock().unwrap();
        panes
            .iter()
            .map(|(pane, entry)| CarriedPtyPane {
                pane_id: *pane,
                #[cfg(unix)]
                terminal_fd: match &entry.terminal {
                    Terminal::Owned(fd) => Some(fd.as_raw_fd()),
                    Terminal::Crate(_) => None,
                },
                pid: entry.killer.pid(),
                size: entry.size,
                exit: entry.exit.get().copied(),
            })
            .collect()
    }

    /// Take a pane back from a terminal descriptor and a child process id, as
    /// the process image that replaced another one does.
    ///
    /// Builds the same pane [`spawn`](PtyBackend::spawn) builds — the same
    /// three threads, the same channels, the same kill behaviour — around a
    /// terminal and a child that are already running. `size` is recorded as the
    /// pane's last size; the terminal carried that size across the swap, so
    /// nothing is written to it and the child is sent no `SIGWINCH`.
    ///
    /// `exit` is how the child ended, as the image before this one observed it
    /// — [`CarriedPtyPane::exit`]. The watcher publishes that status straight
    /// away and waits on nothing: the process that reaped the child took the
    /// status out of the kernel with it. The pane's reader still hands over
    /// whatever the terminal holds before that exit reaches the consumer.
    ///
    /// `None` means no image has seen this child end, and the watcher reaps it
    /// with `waitpid` on that one process id. `portable-pty` resets `SIGCHLD`
    /// only inside `pre_exec`, on the child side of the fork (`portable-pty`
    /// 0.9.0, `src/unix.rs`, `spawn_command`), so no parent-side reaper competes
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
    /// for the reader, which is what parks it.
    #[cfg(unix)]
    pub fn adopt(
        &self,
        pane_id: PaneId,
        terminal_fd: std::os::fd::OwnedFd,
        pid: u32,
        size: PtySize,
        exit: Option<ExitStatus>,
    ) -> Result<PtyHandle, PtyError> {
        // Where this pane's output goes, built exactly as `spawn` builds it.
        let handover = Arc::new(Mutex::new(Handover::default()));
        let (exit_grace_cancel, exit_grace_rx) = channel::<()>();
        // The terminal exposes a descriptor, so the reader reaches the end of
        // the output itself and no watcher stands by to be cancelled.
        drop(exit_grace_rx);
        let (handle, delivery, exit_sender) = match self.sink.clone() {
            Some(sink) => {
                let (exit_sender, exit_receiver) = channel::<ExitStatus>();
                (
                    PtyHandle::detached(pane_id),
                    Delivery::Sink {
                        sink,
                        exit_receiver,
                        handover: Arc::clone(&handover),
                    },
                    exit_sender,
                )
            }
            None => {
                let (handle, output_sender, exit_sender) = PtyHandle::new(pane_id);
                (handle, Delivery::Channel(output_sender), exit_sender)
            }
        };

        let wake = Arc::new(Waker::new().ok_or(PtyError::Io {
            detail: "this platform offers no one-descriptor wake for a pane reader".to_string(),
        })?);
        let terminal = Arc::new(terminal_fd);
        let exited = Arc::new(AtomicBool::new(false));
        let exit_seen = Arc::new(OnceLock::new());

        // Every fallible step is past, so nothing below returns early holding
        // the pane map. The caller owns the id and must not reuse a live one.
        let mut panes = self.panes.lock().unwrap();
        debug_assert!(
            !panes.contains_key(&pane_id),
            "adopt into an already-live pane id {pane_id}; kill it first"
        );

        let reader_thread = start_owned_reader(
            delivery,
            pane_id,
            Arc::clone(&terminal),
            Arc::clone(&wake),
            Arc::clone(&exited),
            self.readers.enter(),
        );
        let writer_sender = start_writer(WriteSide::Owned(Arc::clone(&terminal)));

        // The swap left no `portable-pty` child to wait on, so the watcher
        // reaps the process id itself — unless the image before it already
        // reaped the child and carried the status here. Everything after that
        // is what a spawned pane's watcher does.
        let release = WatchRelease {
            exited: Arc::clone(&exited),
            exit: Arc::clone(&exit_seen),
            exit_sender,
            writer_stop: writer_sender.clone(),
            wake: Some(Arc::clone(&wake)),
        };
        let watcher_thread = spawn_pty_thread("koshi-pty-watch", move || {
            release.publish(exit.unwrap_or_else(|| wait_for_child(pid)));
        });

        panes.insert(
            pane_id,
            PaneEntry {
                terminal: Terminal::Owned(terminal),
                size,
                writer: writer_sender,
                killer: PtyChildKillControl::new(pid),
                exited,
                exit: exit_seen,
                handover,
                exit_grace_cancel,
                reader: reader_thread,
                reader_wake: Some(wake),
                watcher: watcher_thread,
            },
        );
        drop(panes);
        Ok(handle)
    }
}

impl Default for PortablePtyBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PtyBackend for PortablePtyBackend {
    /// Open a fresh PTY, launch `spec` as a child inside it, and wire up its I/O.
    ///
    /// The child runs detached on three background threads owned by the
    /// backend: a **reader** (master output → wherever this backend delivers),
    /// a **writer** (input channel → master, so writes never block the
    /// dispatcher), and a **watcher** (`child.wait()` → exit channel, flips the
    /// `exited` flag, and releases the writer).
    ///
    /// Where the output goes depends on how the backend was built. Under
    /// [`with_sink`](PortablePtyBackend::with_sink) the reader hands each chunk
    /// to the sink and publishes the exit itself once the output runs out, and
    /// the returned [`crate::backend::state::PtyHandle`] carries no channels.
    /// On Windows the watcher closes the pane's terminal once the child is
    /// reaped, which is what brings the reader to that end; on a Unix terminal
    /// that exposes no descriptor to wait on the watcher publishes the exit
    /// itself, once output stops arriving. A pane always learns its child
    /// ended. The reader then stops on its next chunk, so a descendant
    /// still printing into a closed pane's terminal is not forwarded.
    /// Otherwise the handle carries the output and exit channels for the
    /// caller to poll or relay.
    ///
    /// # Errors
    /// Returns [`PtyError::Spawn`] if the PTY can't be opened, the command can't
    /// be launched, or the master's reader/writer can't be taken.
    fn spawn(
        &self,
        pane_id: PaneId,
        spec: SpawnSpec,
        size: PtySize,
    ) -> Result<PtyHandle, PtyError> {
        // 1. Decide where this pane's output goes, and build the caller's handle
        //    to match. With a sink the reader hands each chunk straight to the
        //    consumer and the handle carries no channels, so the caller starts
        //    no relay thread for the pane; without one the handle keeps the
        //    consuming ends and the threads below feed the producing ends.
        //
        //    `exit_sender` is the watcher's in both cases. Under a sink its
        //    receiver is private to the reader, which publishes the status once
        //    the child's output has run out — that is what keeps a consumer
        //    from seeing the child end while output is still coming.
        //
        //    The watcher takes a second reference to the sink for the one path
        //    where it publishes the exit itself: a Unix terminal that exposes
        //    no descriptor to wait on. `handover` carries both facts under one
        //    lock: whether the pane's exit is settled, and how much the reader
        //    has handed over, so the watcher never sees half a transition.
        let handover = Arc::new(Mutex::new(Handover::default()));
        // The watcher's one interruptible wait: a Unix terminal with no
        // descriptor stands by on it, and a Windows pane waits on it for its
        // reader before closing the terminal. `kill` sends on it.
        let (exit_grace_cancel, exit_grace_rx) = channel::<()>();
        let (handle, delivery, exit_sender, watcher_sink) = match self.sink.clone() {
            Some(sink) => {
                let (exit_sender, exit_receiver) = channel::<ExitStatus>();
                (
                    PtyHandle::detached(pane_id),
                    Delivery::Sink {
                        sink: Arc::clone(&sink),
                        exit_receiver,
                        handover: Arc::clone(&handover),
                    },
                    exit_sender,
                    Some(sink),
                )
            }
            None => {
                let (handle, output_sender, exit_sender) = PtyHandle::new(pane_id);
                (handle, Delivery::Channel(output_sender), exit_sender, None)
            }
        };
        // 2. Open the PTY pair sized to the pane. The pair is two linked ends:
        //    `master` stays with us, `slave` becomes the child's terminal.
        let pty = native_pty_system();
        let pair = pty.openpty(to_pp_size(size)).map_err(|e| PtyError::Spawn {
            detail: e.to_string(),
        })?;

        // 3. Build the launch command from the spec (program, args, cwd, env)...
        let mut cmd = CommandBuilder::new(spec.program.as_os_str());
        for a in &spec.args {
            cmd.arg(a);
        }
        // Resolve cwd before launch: an explicit path wins; an absent path
        // inherits koshi's process cwd, matching `SpawnSpec`'s contract.
        match &spec.cwd {
            Some(cwd) => cmd.cwd(cwd),
            None => {
                if let Ok(cwd) = std::env::current_dir() {
                    cmd.cwd(cwd);
                }
            }
        }

        //    ...including the environment. `CommandBuilder` is deliberately NOT
        //    cleared, so the child inherits the full parent env — kept as
        //    `OsString`, so non-UTF-8 vars survive intact. `build_env` returns
        //    only koshi's overlay (terminal identity + shell bootstrap +
        //    `spec.env`); applying each key with `cmd.env` overwrites the
        //    inherited value. On Windows `portable-pty` folds env names
        //    case-insensitively, so an override (e.g. `PATH`) replaces a
        //    differently-cased inherited key (`Path`) rather than duplicating it.
        let pty_env = build_env(&spec);
        for (k, v) in pty_env {
            cmd.env(k.as_str(), v.as_str());
        }

        //    ...and launch it on the slave end. The child now owns the slave as
        //    its stdin/stdout/stderr; we keep `child` only to wait on / kill it.
        //    A `portable-pty` child is not terminated by being dropped, so wrap it
        //    in `ChildGuard`: if any step below returns early, the guard kills the
        //    child instead of leaking an orphan with no owner.
        let child =
            ChildGuard::new(pair.slave.spawn_command(cmd).map_err(|e| PtyError::Spawn {
                detail: e.to_string(),
            })?);

        let pid = child.process_id().ok_or(PtyError::Spawn {
            detail: "child has no PID".to_string(),
        })?;

        // 4. Build the kill control right away. On Windows this assigns the child
        //    to its Job Object, so do it as early as possible after spawn.
        //
        //    NOTE: we do not reinvent portable-pty's spawn (no CREATE_SUSPENDED or
        //    job-at-creation), so the child is already running here. A program
        //    that forks a grandchild in the instant before this assignment can
        //    leave that grandchild outside the Job Object, where `KillPolicy::Tree`
        //    cannot reach it. Closing that window entirely needs control over
        //    `CreateProcess` that portable-pty does not expose.
        #[cfg(unix)]
        let killer = PtyChildKillControl::new(pid);
        #[cfg(windows)]
        let killer = PtyChildKillControl::new(
            pid,
            child.as_raw_handle().ok_or(PtyError::Spawn {
                detail: "child has no process handle".to_string(),
            })?,
        )?;

        // 5. Drop OUR copy of the slave. The child kept its own; once the child
        //    exits and the kernel closes its end, the terminal reports
        //    EOF — that is how the reader thread (step 7) learns to stop.
        drop(pair.slave);

        // 6. Decide how this pane reaches its terminal, and pull the exit flag.
        //
        //    A terminal that exposes a descriptor is opened once, here, and
        //    that one descriptor serves the whole pane: its reader waits on it
        //    and reads it, its writer writes it, and `resize` retunes it. The
        //    reader also gets a [`Waker`] — one more descriptor — so the
        //    watcher can bring it back from a wait a descendant holding the
        //    terminal keeps open. Two descriptors per pane in total.
        //
        //    The copy is taken here, where the master is plainly alive, and
        //    owned from then on, so a pane torn down while its threads still
        //    run closes its own copy and never theirs. `portable-pty`'s master
        //    is left to drop at the end of this call, which closes the
        //    descriptor it was holding.
        //
        //    Windows exposes none of this, so there the pane keeps the crate's
        //    own reader, writer and master, and its reader blocks in `read`.
        #[cfg(unix)]
        let owned = own_terminal_fd(&*pair.master)
            .zip(Waker::new())
            .map(|(fd, wake)| (Arc::new(fd), Arc::new(wake)));
        #[cfg(not(unix))]
        let owned: Option<(Arc<()>, Arc<()>)> = None;

        // The doorbell's other two holders: the reader waits on it, the watcher
        // rings it once the child is reaped, and the pane entry rings it to
        // park the reader.
        #[cfg(unix)]
        let watcher_wake = owned.as_ref().map(|(_, wake)| Arc::clone(wake));
        #[cfg(unix)]
        let entry_wake = owned.as_ref().map(|(_, wake)| Arc::clone(wake));

        //    The watcher's tail — what it does once the child is reaped —
        //    follows from the same choice: a reader that can be waited on
        //    publishes the exit itself, a Unix terminal with no descriptor
        //    leaves the watcher to publish, and Windows closes the terminal and
        //    then stands by behind the reader.
        let (read_side, write_side, terminal, watcher_tail) = match owned {
            #[cfg(unix)]
            Some((fd, wake)) => (
                ReadSide::Owned(Arc::clone(&fd), wake),
                WriteSide::Owned(Arc::clone(&fd)),
                Terminal::Owned(fd),
                WatcherTail::ReaderPublishes,
            ),
            #[cfg(not(unix))]
            Some(_) => unreachable!("no platform without a descriptor owns one"),
            None => {
                let reader = pair
                    .master
                    .try_clone_reader()
                    .map_err(|e| PtyError::Spawn {
                        detail: e.to_string(),
                    })?;
                let writer = pair.master.take_writer().map_err(|e| PtyError::Spawn {
                    detail: e.to_string(),
                })?;
                // The master goes in a slot the watcher shares: on Windows it
                // takes it out and drops it once the child is reaped, which
                // closes the console and ends the reader's pipe.
                let slot = Arc::new(Mutex::new(Some(pair.master)));
                #[cfg(windows)]
                let tail = WatcherTail::CloseTerminal {
                    terminal: Arc::clone(&slot),
                    sink: watcher_sink.as_ref().map(Arc::downgrade),
                };
                #[cfg(not(windows))]
                let tail = match watcher_sink.as_ref() {
                    Some(sink) => WatcherTail::StandBy(Arc::downgrade(sink)),
                    None => WatcherTail::ReaderPublishes,
                };
                (
                    ReadSide::Crate(reader),
                    WriteSide::Crate(writer),
                    Terminal::Crate(slot),
                    tail,
                )
            }
        };

        let exited = Arc::new(AtomicBool::new(false));
        let exit_seen = Arc::new(OnceLock::new());

        // Every fallible step is past: the watcher thread below now owns the child
        // and is responsible for reaping it, so disarm the guard.
        let child = child.disarm();

        // 7. Take the pane map now and hold it until this pane is in it. Every
        //    fallible step is behind us, so nothing below can return early
        //    holding the lock. A short-lived child can be reaped and its exit
        //    handed to the consumer before this call returns, and a consumer
        //    that closes the pane inside that call has to find it: blocking
        //    `kill` here until the insert lands is what makes it. None of the
        //    threads started below touch this map, so nothing they do waits on
        //    the lock.
        //
        //    The caller owns the id and must not reuse a live one — spawning
        //    over a live entry would drop its terminal and I/O threads on the
        //    floor.
        let mut panes = self.panes.lock().unwrap();
        debug_assert!(
            !panes.contains_key(&pane_id),
            "spawn into an already-live pane id {pane_id}; kill it before respawning"
        );

        // 8. Writer thread: drain the input channel onto the terminal, so a
        //    write to a child that has stopped reading blocks only that thread,
        //    never the dispatcher. Started before the reader and before the
        //    watcher, which queues its `Stop` on this channel.
        let writer_sender = start_writer(write_side);

        //    A Windows pseudoconsole asks where the cursor is as it opens and
        //    holds its child's output until that is answered. The answer is
        //    queued here, ahead of the pane being reachable by
        //    [`write`](PortablePtyBackend::write), so nothing a user types can
        //    reach the terminal before it.
        #[cfg(windows)]
        let _ = writer_sender.send(WriterMsg::Bytes(CURSOR_AT_HOME.to_vec()));

        // 9. Reader thread: wait on the terminal, handing each chunk of
        //    child output to `delivery` until EOF (child gone) or the consumer
        //    goes away. Once the output has run out a sink can be told the
        //    child ended; already-settled panes return from `finish` without
        //    waiting.
        //
        //    A reader that owns its terminal descriptor is counted into the
        //    gate here, before its thread starts, so a pause landing first
        //    still waits for it to reach the park. A reader that blocks in
        //    `read` can never park and is never counted.
        //
        //    On Windows this reader takes the terminal's opening
        //    cursor-position request out of the output it hands over.
        let reader_thread = match read_side {
            #[cfg(unix)]
            ReadSide::Owned(terminal, wake) => start_owned_reader(
                delivery,
                pane_id,
                terminal,
                wake,
                Arc::clone(&exited),
                self.readers.enter(),
            ),
            ReadSide::Crate(reader) => spawn_pty_thread("koshi-pty-read", move || {
                #[cfg(windows)]
                let mut reader = RemovesCursorRequest::new(reader);
                #[cfg(not(windows))]
                let mut reader = reader;
                if pump_blocking(&mut reader, &delivery, pane_id) {
                    delivery.finish(pane_id);
                } else {
                    // The consumer has let the pane go: release it now, so
                    // nothing about this pane reaches it again and it is
                    // not held for the read below.
                    drop(delivery);
                    // Closing the pane's console waits for the output it
                    // still holds to be read out, and this thread is the
                    // pane's one reader — so it reads the console to its
                    // end, discarding, before it stops.
                    #[cfg(windows)]
                    drain_terminal(&mut reader);
                }
            }),
        };

        // 10. Watcher thread: block on `child.wait()`, map the OS exit status
        //    into koshi's `ExitStatus`, then release the pane's other threads —
        //    flip `exited` so `kill` won't signal a corpse, publish the status
        //    on the exit channel, stop the writer, and ring the reader's
        //    doorbell so it takes the last of the output.
        //
        //    What it does after that is its tail, decided in step 6: bring the
        //    reader to the end of the terminal, or stand by and publish the
        //    exit itself. On Windows the tail waits for the reader to be back
        //    on the terminal and then closes the pane's console, which that
        //    reader is there to read out. `kill` wakes both waits, so closing a
        //    pane returns without sitting through either.
        let release = WatchRelease {
            exited: Arc::clone(&exited),
            exit: Arc::clone(&exit_seen),
            exit_sender,
            writer_stop: writer_sender.clone(),
            #[cfg(unix)]
            wake: watcher_wake,
        };
        let watcher_handover = Arc::clone(&handover);
        let watcher_thread = spawn_pty_thread("koshi-pty-watch", move || {
            let mut child = child; // owns it; wait() needs &mut
            let status = match child.wait() {
                Ok(s) => map_status(s),
                Err(_) => ExitStatus::ExitCode(-1),
            };
            release.publish(status);

            match watcher_tail {
                // The reader reaches the end of the terminal itself and
                // publishes the exit behind the last of the output.
                #[cfg(not(windows))]
                WatcherTail::ReaderPublishes => {}
                // Closing the console flushes what it still holds and then ends
                // the reader's pipe. The close returns once the pane's reader
                // has taken that flush and every process attached to the console
                // has let it go, so it waits for the reader first and then runs
                // on its own thread, and this thread stands by behind it.
                #[cfg(windows)]
                WatcherTail::CloseTerminal { terminal, sink } => {
                    wait_for_the_reader_to_read_again(
                        &exit_grace_rx,
                        &watcher_handover,
                        READER_CHECK_IN,
                    );
                    let master = terminal.lock().expect("terminal").take();
                    spawn_pty_thread("koshi-pty-close", move || drop(master));
                    if let Some(sink) = sink {
                        stand_by_for_the_reader(
                            &exit_grace_rx,
                            &watcher_handover,
                            &sink,
                            pane_id,
                            status,
                        );
                    }
                }
                #[cfg(not(windows))]
                WatcherTail::StandBy(sink) => {
                    stand_by_for_the_reader(
                        &exit_grace_rx,
                        &watcher_handover,
                        &sink,
                        pane_id,
                        status,
                    );
                }
            }
        });

        // 11. Retain the terminal, writer, killer, flag and both thread handles
        //    under the pane id, then hand the caller its polling handle.
        panes.insert(
            pane_id,
            PaneEntry {
                terminal,
                size,
                writer: writer_sender,
                killer,
                exited,
                exit: exit_seen,
                handover,
                exit_grace_cancel,
                reader: reader_thread,
                #[cfg(unix)]
                reader_wake: entry_wake,
                watcher: watcher_thread,
            },
        );
        drop(panes);
        Ok(handle)
    }
    fn resize(&self, pane: PaneId, size: PtySize) -> Result<(), PtyError> {
        let mut panes = self.panes.lock().unwrap();
        let Some(entry) = panes.get_mut(&pane) else {
            return Err(PtyError::UnknownPane { pane });
        };
        entry.terminal.resize(size)?;
        // Recorded only once the kernel took it, so a carried pane names the
        // size its child was actually told.
        entry.size = size;
        Ok(())
    }
    fn write(&self, pane: PaneId, bytes: &[u8]) -> Result<(), PtyError> {
        let panes = self.panes.lock().unwrap();
        let Some(entry) = panes.get(&pane) else {
            return Err(PtyError::UnknownPane { pane });
        };

        entry
            .writer
            .send(WriterMsg::Bytes(bytes.to_vec()))
            .map_err(|e| PtyError::Io {
                detail: e.to_string(),
            })
    }
    fn kill(&self, pane: PaneId, kill_policy: KillPolicy) -> Result<(), PtyError> {
        let entry = self
            .panes
            .lock()
            .unwrap()
            .remove(&pane)
            .ok_or(PtyError::UnknownPane { pane })?;

        // Settle the pane's exit before anything dies. The caller is closing
        // this pane and has no use for an exit event about it, and either
        // helper thread could otherwise reach the child's end first and
        // publish one. This also stops the reader on its next chunk, so a
        // descendant still printing into the terminal stops being forwarded.
        entry.handover.lock().expect("handover").settled = true;

        // `Force`/`Graceful` signal the leader PID, so skip them once the watcher
        // has reaped it — a recycled PID could belong to an unrelated process.
        // `Tree` and `GracefulTree`'s closing group-kill signal the whole
        // group/job (`killpg` / `TerminateJobObject`), which stays valid while
        // any member lives, so they fire unconditionally: the leader can exit
        // while a same-group descendant keeps running, and the group-kill must
        // still reap it (the `exited` flag tracks only the leader, not whether
        // the group is empty).
        match kill_policy {
            KillPolicy::Force => {
                if !entry.exited.load(Ordering::SeqCst) {
                    let _ = entry.killer.force();
                }
            }
            KillPolicy::Tree => {
                let _ = entry.killer.tree();
            }
            KillPolicy::Graceful { timeout } => {
                if !entry.exited.load(Ordering::SeqCst) {
                    // Ask the leader to exit and give it the grace window; SIGKILL when the
                    // window runs out, or at once when the request never reached it.
                    if !stopped_within_grace(entry.killer.request_stop(), &entry.exited, timeout) {
                        let _ = entry.killer.force();
                    }
                }
            }
            KillPolicy::GracefulTree { timeout } => {
                if !entry.exited.load(Ordering::SeqCst) {
                    // Ask the whole group to exit — every member gets the stop request and
                    // the grace window — then wait for the leader. The wait is skipped
                    // only when no member received the request.
                    stopped_within_grace(entry.killer.request_stop_tree(), &entry.exited, timeout);
                }

                // Group-kill even when the leader already exited: a disowned
                // descendant can keep the group alive past its leader, and
                // `killpg`/`TerminateJobObject` reaps it with the rest.
                let _ = entry.killer.tree();
            }
        }

        // Wake the watcher out of the wait it is in rather than joining through
        // it, so closing a pane returns without sitting through the rounds: the
        // standby of a Unix terminal with no descriptor, and a Windows
        // watcher's wait for the reader before it closes the terminal.
        let _ = entry.exit_grace_cancel.send(());
        drop(entry.writer);
        // Joined unless this *is* the watcher: a consumer handed an exit by the
        // watcher may close the pane from inside that call, and a thread that
        // joins itself waits for itself and never returns. The watcher has
        // nothing left to do after handing the exit over, so skipping the join
        // costs nothing.
        if entry.watcher.thread().id() != thread::current().id() {
            let _ = entry.watcher.join();
        }

        Ok(())
    }
    fn live_cwd(&self, pane: PaneId) -> Option<std::path::PathBuf> {
        let panes = self.panes.lock().unwrap();
        let entry = panes.get(&pane)?;
        // A reaped leader's PID can already belong to an unrelated process,
        // and its directory would be a stranger's answer.
        if entry.exited.load(Ordering::SeqCst) {
            return None;
        }
        crate::cwd::process_cwd(entry.killer.pid())
    }
}

/// Whether the leader is gone after being asked to stop.
///
/// Polls [`wait_for_exit`] for up to `timeout` when anything received the stop
/// request, including a group where only part of it did. Returns `false` at
/// once when nothing received it, so no grace window is spent.
fn stopped_within_grace(requested: StopRequest, exited: &AtomicBool, timeout: Duration) -> bool {
    match requested {
        StopRequest::Delivered | StopRequest::Unknown => wait_for_exit(exited, timeout),
        StopRequest::NotDelivered => false,
    }
}

/// Poll the watcher's `exited` flag every 25ms until it flips or `timeout`
/// elapses, returning whether the child exited within the window. The
/// grace-window wait behind [`stopped_within_grace`].
fn wait_for_exit(exited: &AtomicBool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if exited.load(Ordering::SeqCst) {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    exited.load(Ordering::SeqCst)
}

/// Convert koshi's [`PtySize`] into `portable-pty`'s own size type, zeroing
/// the pixel dimensions `portable-pty` accepts but this crate does not track.
fn to_pp_size(s: PtySize) -> portable_pty::PtySize {
    portable_pty::PtySize {
        rows: s.rows,
        cols: s.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Convert `portable-pty`'s exit status into koshi's own [`ExitStatus`]:
/// a signal name (Unix only) maps to [`ExitStatus::Signaled`] via [`sig_no`],
/// anything else maps to [`ExitStatus::ExitCode`].
fn map_status(s: portable_pty::ExitStatus) -> ExitStatus {
    match s.signal() {
        Some(name) => ExitStatus::Signaled(sig_no(name)),
        None => ExitStatus::ExitCode(s.exit_code() as i32),
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
/// We parse the number ONLY when it follows a `": "` (macOS) or the `"Signal "`
/// prefix (the fallback) — never a bare trailing word. Some glibc descriptions
/// end in a non-signal ordinal: `"User defined signal 1"` is SIGUSR1 = 10, not
/// signal 1. Otherwise we map the known glibc descriptions;
/// an unrecognised one yields 0. Reachable only for Unix children — on Windows
/// `signal()` is always `None`, so `map_status` takes the exit-code arm.
fn sig_no(desc: &str) -> i32 {
    // macOS appends ": <n>" — the real number is after the colon.
    if let Some((_, n)) = desc.rsplit_once(": ") {
        if let Ok(n) = n.parse::<i32>() {
            return n;
        }
    }
    // portable-pty's null-strsignal fallback is "Signal <n>".
    if let Some(n) = desc
        .strip_prefix("Signal ")
        .and_then(|n| n.parse::<i32>().ok())
    {
        return n;
    }
    // Linux glibc: bare description, no trailing number.
    match desc {
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
