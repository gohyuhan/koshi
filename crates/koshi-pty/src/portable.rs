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
//! - Windows: the reader publishes. Once the child is reaped the watcher closes
//!   the pane's terminal, which flushes the console's remaining output and then
//!   ends the reader's pipe. The reader reads that console to its end in every
//!   ending, because the close waits for it: on a pane the consumer has let go
//!   it drops the consumer first and discards the rest.
//! - A Unix terminal that exposes no descriptor to wait on: nothing can bring
//!   the reader back, so the watcher stands by on a deadline and publishes.

use std::{
    collections::HashMap,
    io::{ErrorKind, Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use koshi_core::{
    ids::PaneId,
    process::{ExitStatus, KillPolicy, PtySize, SpawnSpec},
};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty};

use crate::{
    backend::state::{PtyBackend, PtyHandle, PtySink},
    env::build_env,
    error::PtyError,
    kill::{PtyChildKillControl, StopRequest},
};

/// Bytes read from a pane's master end in one go. Sized to hold a burst of
/// child output without a syscall per line.
const READ_CHUNK: usize = 8192;

/// One round of the reader's wait on a Unix terminal whose child has gone.
///
/// Once the child is gone the reader waits a round at a time: a round in which
/// the terminal produces nothing means every byte the child printed has been
/// handed over, because a dead child cannot write again. The watcher's standby
/// checks in at the same interval on the one path it runs — a Unix terminal
/// that exposes no descriptor to wait on. Windows has neither wait: the
/// watcher closes the pane's terminal and the reader is carried to its end.
#[cfg(not(windows))]
const EXIT_PUBLISH_GRACE: Duration = Duration::from_millis(100);

/// The longest a pane's exit is held back after its child has gone.
///
/// Bounds both waits. A descendant that holds the terminal open and keeps
/// printing stops the reader's rounds here. On a Unix terminal that exposes no
/// descriptor, the watcher's standby gives the reader until here and then
/// publishes the exit itself. Windows holds nothing back on a clock.
#[cfg(not(windows))]
const EXIT_PUBLISH_LIMIT: Duration = Duration::from_secs(1);

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
    /// Whether a chunk is in the consumer's hands right now. Read by the
    /// watcher's standby, which only a Unix terminal with no descriptor has.
    #[cfg(not(windows))]
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
/// Waking latches: nothing drains it, so a reader that only reaches its wait
/// afterwards still finds it woken. A wake can therefore never be delivered
/// into a gap and missed.
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
        EventFd::from_flags(EfdFlags::EFD_CLOEXEC).ok().map(Waker)
    }

    /// A waker nothing has woken yet: a kernel event queue carrying one
    /// user-triggered event, registered here so [`wake`](Waker::wake) only has
    /// to fire it.
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
            EventFlag::EV_ADD,
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

    /// Wake whoever is waiting, and leave it woken.
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    fn wake(&self) {
        let _ = self.0.write(1);
    }

    /// Wake whoever is waiting, and leave it woken: firing the registered
    /// event leaves it pending on the queue, and nothing drains it.
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
}

#[cfg(unix)]
impl std::os::fd::AsFd for Waker {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
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
/// The watcher fires `wake` once it has reaped the child, and that is what
/// reaches a reader a descendant holding the terminal open would otherwise keep
/// waiting for as long as that descendant runs. The wake latches, so one that
/// lands before the reader reaches its wait is still there when it does. From
/// then on the wait runs in rounds of `grace`, and a round in which the
/// terminal produces nothing ends the pump: a dead child cannot write again, so
/// everything it printed has been handed over and the caller can publish the
/// exit behind it. Rounds stop `limit` after the wake, which bounds a
/// descendant that holds the terminal open and keeps printing.
///
/// [`kill`](PortablePtyBackend::kill) settles the pane and then kills the
/// child, so the wake the watcher fires on reaping it finds the pane settled
/// and the pump stops rather than starting a round.
#[cfg(unix)]
fn pump_waited(
    delivery: &Delivery,
    pane: PaneId,
    terminal: &std::os::fd::OwnedFd,
    wake: &Waker,
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
        let mut woken = false;
        let readable = match ends_at {
            None => {
                let mut fds = [
                    PollFd::new(terminal.as_fd(), PollFlags::POLLIN),
                    PollFd::new(wake.as_fd(), PollFlags::POLLIN),
                ];
                match poll(&mut fds, PollTimeout::NONE) {
                    Ok(_) => {}
                    // A signal caught during the wait is not an end: wait again.
                    Err(Errno::EINTR) => continue,
                    Err(_) => return,
                }
                if stirred(&fds[1]) {
                    if delivery.settled() {
                        return;
                    }
                    ends_at = Some(Instant::now() + limit);
                    woken = true;
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
/// The reader settles once the child's output has run out, which keeps the
/// exit behind the last of that output. Checking in every `grace` until
/// `deadline` gives it that chance; `true` says the deadline passed with the
/// reader still short of the end, which is a Unix terminal that exposed no
/// descriptor to wait on. The watcher already holds the status, so it
/// publishes.
///
/// A chunk in the consumer's hands holds the answer back for as long as it is
/// in flight, however far past `deadline` that runs, so an exit is never
/// published over output already on its way.
///
/// `false` means stop without publishing: the reader settled, or `cancel`
/// carried a value — [`kill`](PortablePtyBackend::kill) closing the pane — or
/// its sender was dropped, which is the backend shutting down. `kill` settles
/// the exit before it sends, so returning here promptly keeps its join short.
#[cfg(not(windows))]
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
    /// Close the pane's terminal so the reader reaches the end. Dropping the
    /// master closes the console, which flushes its remaining output and then
    /// ends the reader's pipe.
    #[cfg(windows)]
    CloseTerminal(Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>),
    /// Stand by on a deadline and publish the exit to this sink, for a terminal
    /// the reader cannot be brought back from.
    #[cfg(not(windows))]
    StandBy(Arc<dyn PtySink>),
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
    writer: Sender<WriterMsg>,
    /// Kill handle for the child process; `kill()` sends the terminating signal.
    killer: PtyChildKillControl,
    /// Flipped to `true` by the watcher thread the moment the child exits; read
    /// by [`kill`](PortablePtyBackend::kill) to avoid signalling a dead process.
    exited: Arc<AtomicBool>,
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
    /// whatever still holds the slave open. On Windows it blocks in `read`
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
    /// Watcher thread: blocks on the child, records exit status, flips `exited`.
    watcher: JoinHandle<()>,
    /// Whether this pane's exit is settled — the state the reader and watcher
    /// share. [`kill`](PortablePtyBackend::kill) sets it before killing
    /// anything, so a caller closing a pane is never handed an exit for it by
    /// whichever thread gets there first. Under no sink nothing reads it.
    handover: Arc<Mutex<Handover>>,
    /// Wakes the watcher out of its standby wait.
    ///
    /// [`kill`](PortablePtyBackend::kill) sends on this before joining the
    /// watcher, so tearing a pane down returns straight away instead of
    /// sitting through the rounds. What stops the exit being published is
    /// `handover.settled`, which `kill` sets first. Only a Unix terminal that
    /// exposes no descriptor stands by, so elsewhere nothing is listening and
    /// the send is a no-op.
    exit_grace_cancel: Sender<()>,
}

/// Real OS-PTY backend built on the `portable-pty` crate. Each spawned pane gets
/// a kernel PTY plus three helper threads (reader, writer, watcher); the backend
/// owns them all through the [`PaneEntry`] map.
pub struct PortablePtyBackend {
    /// Every live pane's PTY, threads, and kill handle, keyed by [`PaneId`].
    /// Locked because [`spawn`](PtyBackend::spawn), [`resize`](PtyBackend::resize),
    /// [`write`](PtyBackend::write), and [`kill`](PtyBackend::kill) can all be
    /// called from different dispatcher calls.
    panes: Mutex<HashMap<PaneId, PaneEntry>>,
    /// Where spawned panes deliver output and exit. `None` routes both through
    /// each pane's [`PtyHandle`] channels, which the caller polls or relays;
    /// `Some` has the reader thread hand them to the consumer directly, so no
    /// relay thread exists per pane.
    sink: Option<Arc<dyn PtySink>>,
}

impl PortablePtyBackend {
    /// Creates a new, empty PTY backend with no active panes, delivering each
    /// pane's output and exit through its own [`PtyHandle`] channels.
    pub fn new() -> Self {
        PortablePtyBackend {
            panes: Mutex::new(HashMap::new()),
            sink: None,
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
        }
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
        let (exit_grace_cancel, exit_grace_rx) = channel::<()>();
        // Only a Unix terminal that exposes no descriptor to wait on stands by,
        // so that is the one build where the cancel has a receiver.
        #[cfg(windows)]
        drop(exit_grace_rx);
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
        // A Windows watcher closes the pane's terminal instead of publishing,
        // so it keeps no hold on the sink.
        #[cfg(windows)]
        drop(watcher_sink);

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

        // The waker's second holder: the reader waits on it, the watcher fires
        // it, and it is dropped when the watcher thread ends.
        #[cfg(unix)]
        let watcher_wake = owned.as_ref().map(|(_, wake)| Arc::clone(wake));

        //    The watcher's tail — what it does once the child is reaped —
        //    follows from the same choice: a reader that can be waited on or
        //    brought to an end publishes the exit itself, and only a Unix
        //    terminal with no descriptor leaves the watcher to publish.
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
                let tail = WatcherTail::CloseTerminal(Arc::clone(&slot));
                #[cfg(not(windows))]
                let tail = match watcher_sink {
                    Some(sink) => WatcherTail::StandBy(sink),
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

        // 8. Reader thread: wait on the terminal, handing each chunk of
        //    child output to `delivery` until EOF (child gone) or the consumer
        //    goes away. Once the output has run out a sink can be told the
        //    child ended; already-settled panes return from `finish` without
        //    waiting.
        let reader_thread = spawn_pty_thread("koshi-pty-read", move || {
            match read_side {
                #[cfg(unix)]
                ReadSide::Owned(terminal, wake) => {
                    pump_waited(
                        &delivery,
                        pane_id,
                        &terminal,
                        &wake,
                        EXIT_PUBLISH_GRACE,
                        EXIT_PUBLISH_LIMIT,
                    );
                    delivery.finish(pane_id);
                }
                ReadSide::Crate(mut reader) => {
                    if pump_blocking(&mut *reader, &delivery, pane_id) {
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
                        drain_terminal(&mut *reader);
                    }
                }
            }
        });

        // 9. Writer thread: drain the input
        //    channel onto it, so a write to a child that has stopped reading
        //    blocks only this thread, never the dispatcher. It parks in `recv`
        //    with no timer, so an idle pane costs no wakeups. It exits on either
        //    teardown path: the channel closing (the entry's `Sender` dropped by
        //    `kill`/teardown → `Disconnected`), or the [`WriterMsg::Stop`] the
        //    watcher queues once the child is gone, so a pane kept open past its
        //    child's death still reclaims the thread. `Stop` travels the same
        //    channel as the bytes, so every write queued before the child exited
        //    is written first. Started before the watcher, which needs a sender
        //    of its own to queue that `Stop` on.
        let (writer_sender, writer_receiver) = channel::<WriterMsg>();

        let _ = spawn_pty_thread("koshi-pty-write", move || {
            let mut write_side = write_side;
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
                    WriterMsg::Stop => break,
                }
            }
            // Nothing is written to the terminal on the way out. The writer
            // stops once the child is gone or the pane is closed.
        });

        // 10. Watcher thread: block on `child.wait()`, map the OS exit status
        //    into our `ExitStatus`, flip `exited` so `kill` won't signal a
        //    corpse, publish the status on the exit channel, then release the
        //    writer thread, which is parked waiting for exactly that.
        //
        //    What it does after that is its tail, decided in step 6: bring the
        //    reader to the end of the terminal, or stand by and publish the
        //    exit itself. `kill` wakes the standby, so closing a pane returns
        //    without sitting through it. On Windows the tail closes the pane's
        //    console, which the reader is there to read out.
        let exited_w = Arc::clone(&exited);
        let writer_stop = writer_sender.clone();
        #[cfg(not(windows))]
        let watcher_handover = Arc::clone(&handover);
        let watcher_thread = spawn_pty_thread("koshi-pty-watch", move || {
            let mut child = child; // owns it; wait() needs &mut
            let status = match child.wait() {
                Ok(s) => map_status(s),
                Err(_) => ExitStatus::ExitCode(-1),
            };
            exited_w.store(true, Ordering::SeqCst); // tell kill() it's dead
            let _ = exit_sender.send(status);
            let _ = writer_stop.send(WriterMsg::Stop);
            // Bring the reader back from its wait: whatever the terminal still
            // holds is the last of this child's output, and once it has been
            // handed over the reader publishes the exit behind it.
            #[cfg(unix)]
            if let Some(wake) = &watcher_wake {
                wake.wake();
            }

            match watcher_tail {
                // The reader reaches the end of the terminal itself and
                // publishes the exit behind the last of the output.
                #[cfg(not(windows))]
                WatcherTail::ReaderPublishes => {}
                // Closing the console flushes what it still holds and then ends
                // the reader's pipe. The pane's reader is what consumes that
                // flush, which is what lets this return.
                #[cfg(windows)]
                WatcherTail::CloseTerminal(slot) => {
                    let master = slot.lock().expect("terminal").take();
                    drop(master);
                }
                #[cfg(not(windows))]
                WatcherTail::StandBy(sink) => {
                    let publish = should_publish_exit(
                        &exit_grace_rx,
                        &watcher_handover,
                        Instant::now() + EXIT_PUBLISH_LIMIT,
                        EXIT_PUBLISH_GRACE,
                    );
                    // Both endings settle the pane's exit, so the reader stops
                    // and no second exit can reach the consumer — including
                    // from a reader that reaches the end of the PTY after a
                    // `kill`.
                    let already_settled = {
                        let mut held = watcher_handover.lock().expect("handover");
                        let was = held.settled;
                        held.settled = true;
                        was
                    };
                    if publish && !already_settled {
                        sink.exit(pane_id, status);
                    }
                }
            }
        });

        // 11. Retain the terminal, writer, killer, flag and both thread handles
        //    under the pane id, then hand the caller its polling handle.
        panes.insert(
            pane_id,
            PaneEntry {
                terminal,
                writer: writer_sender,
                killer,
                exited,
                handover,
                exit_grace_cancel,
                reader: reader_thread,
                watcher: watcher_thread,
            },
        );
        drop(panes);
        Ok(handle)
    }
    fn resize(&self, pane: PaneId, size: PtySize) -> Result<(), PtyError> {
        let panes = self.panes.lock().unwrap();
        let Some(entry) = panes.get(&pane) else {
            return Err(PtyError::UnknownPane { pane });
        };
        entry.terminal.resize(size)
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

        // Wake the watcher out of its standby wait rather than joining through
        // it, so closing a pane returns without sitting through the rounds.
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
/// prefix (the fallback) — never a bare trailing word, because some glibc
/// descriptions end in a non-signal ordinal (e.g. `"User defined signal 1"` is
/// SIGUSR1 = 10, not signal 1). Otherwise we map the known glibc descriptions;
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
