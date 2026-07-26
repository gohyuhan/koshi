//! Real OS-PTY backend built on the `portable-pty` crate.
//!
//! A spawned pane gets a kernel PTY and three helper threads (reader, writer,
//! watcher), all owned through the [`crate::portable::PortablePtyBackend`] pane map. The
//! implementation handles child output streaming, input queuing, process
//! termination (with cross-platform kill policies), and exit status tracking.
//!
//! Three threads is the whole per-pane cost. The reader delivers output to the
//! consumer itself — see [`crate::backend::state::PtySink`] — so nothing has to
//! run alongside a pane to move its bytes along.

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
    kill::PtyChildKillControl,
};

/// Bytes read from a pane's master end in one go. Sized to hold a burst of
/// child output without a syscall per line.
const READ_CHUNK: usize = 8192;

/// Start one of a pane's helper threads under `name`.
///
/// Naming them makes a debugger, a profiler, or a crash report attribute the
/// thread to koshi rather than showing an anonymous worker. Spawn failure
/// panics, matching [`std::thread::spawn`].
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
/// gets `Sink` and saves the relay thread the channel route needs.
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
    },
}

impl Delivery {
    /// Deliver one chunk of `pane`'s output. `false` means the consumer is
    /// gone and the reader should stop.
    fn output(&self, pane: PaneId, bytes: Vec<u8>) -> bool {
        match self {
            Delivery::Channel(sender) => sender.send(bytes).is_ok(),
            Delivery::Sink { sink, .. } => sink.output(pane, bytes),
        }
    }

    /// Report `pane`'s child as ended, once its output is exhausted. A sink
    /// waits here for the watcher's status; a channel consumer reads the exit
    /// off its own handle, so there is nothing to do.
    fn finish(self, pane: PaneId) {
        if let Delivery::Sink {
            sink,
            exit_receiver,
        } = self
        {
            if let Ok(status) = exit_receiver.recv() {
                sink.exit(pane, status);
            }
        }
    }
}

/// What the per-pane writer thread accepts on its channel.
enum WriterMsg {
    /// Bytes to write to the child's stdin.
    Bytes(Vec<u8>),
    /// Stop and release the thread: queued by the watcher once the child has
    /// exited, so the writer never has to wake on a timer to notice.
    Stop,
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
pub struct PaneEntry {
    /// Master end of the PTY. Held so the kernel keeps the pair open and so
    /// [`resize`](PortablePtyBackend::resize) can retune the window size.
    master: Box<dyn MasterPty + Send>,
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
    /// Reader thread: drains the master's read half to wherever this backend
    /// delivers — the handle's output channel, or the sink. Under a sink it also
    /// publishes the child's exit, once the read half has run dry.
    ///
    /// Not joined on teardown: the slave fd may outlive the child (e.g., when the
    /// child `setsid`s into a new process group), so the thread could block forever
    /// if joined. It exits once the fd closes, and under a sink once the watcher
    /// has handed it the exit status to publish. Retained so the struct owns the
    /// handle.
    #[expect(dead_code)]
    reader: JoinHandle<()>,
    /// Watcher thread: blocks on the child, records exit status, flips `exited`.
    watcher: JoinHandle<()>,
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
    /// This is the shape an event loop wants: without it each pane needs a
    /// thread of its own to move chunks from the pane's channel onto the loop's
    /// queue, so a session's thread count grows a whole thread per pane for
    /// work that is a single function call here.
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
    /// Otherwise the handle carries the output and exit channels for the caller
    /// to poll or relay.
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
        let (handle, delivery, exit_sender) = match self.sink.clone() {
            Some(sink) => {
                let (exit_sender, exit_receiver) = channel::<ExitStatus>();
                (
                    PtyHandle::detached(pane_id),
                    Delivery::Sink {
                        sink,
                        exit_receiver,
                    },
                    exit_sender,
                )
            }
            None => {
                let (handle, output_sender, exit_sender) = PtyHandle::new(pane_id);
                (handle, Delivery::Channel(output_sender), exit_sender)
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
        //    exits and the kernel closes its end, the master's read half reports
        //    EOF — that is how the reader thread (step 7) learns to stop.
        drop(pair.slave);

        // 6. Pull the master's read/write halves and the exit flag. `reader` is a
        //    cloned read half (child output); `writer` is its write half (child
        //    input); `exited` is the flag the watcher flips and `kill` reads.
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| PtyError::Spawn {
                detail: e.to_string(),
            })?;
        let writer = pair.master.take_writer().map_err(|e| PtyError::Spawn {
            detail: e.to_string(),
        })?;
        let exited = Arc::new(AtomicBool::new(false));

        // Every fallible step is past: the watcher thread below now owns the child
        // and is responsible for reaping it, so disarm the guard.
        let child = child.disarm();

        // 7. Reader thread: block on the master read half, handing each chunk of
        //    child output to `delivery` until EOF (child gone) or the consumer
        //    goes away.
        let reader_thread = spawn_pty_thread("koshi-pty-read", move || {
            let mut buf = [0u8; READ_CHUNK];
            let mut reader = reader;
            // Cleared only when the consumer reports itself gone, which is the
            // one ending that must not wait for the child: `finish` blocks for
            // the watcher's status, and there would be nobody left to give it
            // to — a child that outlives its consumer would pin this thread.
            let mut consumer_live = true;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF = shell gone
                    Ok(n) => {
                        if !delivery.output(pane_id, buf[..n].to_vec()) {
                            consumer_live = false;
                            break;
                        }
                    } // runtime dropped handle
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            if consumer_live {
                // Output has run out, so a sink can now be told the child ended.
                delivery.finish(pane_id);
            }
        });

        // 8. Writer thread: own the master's write half and drain the input
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
            let mut writer = writer;
            while let Ok(message) = writer_receiver.recv() {
                match message {
                    WriterMsg::Bytes(bytes) => {
                        let _ = writer.write_all(&bytes).and_then(|_| writer.flush());
                    }
                    WriterMsg::Stop => break,
                }
            }
        });

        // 9. Watcher thread: block on `child.wait()`, map the OS exit status into
        //    our `ExitStatus`, flip `exited` so `kill` won't signal a corpse,
        //    publish the status on the exit channel, then release the writer
        //    thread, which is parked waiting for exactly that.
        let exited_w = Arc::clone(&exited);
        let writer_stop = writer_sender.clone();
        let watcher_thread = spawn_pty_thread("koshi-pty-watch", move || {
            let mut child = child; // owns it; wait() needs &mut
            let status = match child.wait() {
                Ok(s) => map_status(s),
                Err(_) => ExitStatus::ExitCode(-1),
            };
            exited_w.store(true, Ordering::SeqCst); // tell kill() it's dead
            let _ = exit_sender.send(status);
            let _ = writer_stop.send(WriterMsg::Stop);
        });

        // 10. Retain the master, writer, killer, flag and both thread handles
        //    under the pane id, then hand the caller its polling handle. The
        //    caller owns the id and must not reuse a live one — spawning over a
        //    live entry would drop its master fd and I/O threads on the floor.
        let mut panes = self.panes.lock().unwrap();
        debug_assert!(
            !panes.contains_key(&pane_id),
            "spawn into an already-live pane id {pane_id}; kill it before respawning"
        );
        panes.insert(
            pane_id,
            PaneEntry {
                master: pair.master,
                writer: writer_sender,
                killer,
                exited,
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
        entry
            .master
            .resize(to_pp_size(size))
            .map_err(|e| PtyError::Io {
                detail: e.to_string(),
            })
    }
    fn write(&self, pane: PaneId, bytes: &[u8]) -> Result<(), PtyError> {
        let mut panes = self.panes.lock().unwrap();
        let Some(entry) = panes.get_mut(&pane) else {
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
                    // Ask the leader to exit, give it the grace window; SIGKILL
                    // only if it overstays the deadline.
                    let _ = entry.killer.request_stop();
                    if !wait_for_exit(&entry.exited, timeout) {
                        let _ = entry.killer.force();
                    }
                }
            }
            KillPolicy::GracefulTree { timeout } => {
                if !entry.exited.load(Ordering::SeqCst) {
                    // Ask the whole group to exit — every member gets the stop
                    // request and the grace window — then wait for the leader.
                    let _ = entry.killer.request_stop_tree();
                    wait_for_exit(&entry.exited, timeout);
                }

                // Group-kill even when the leader already exited: a disowned
                // descendant can keep the group alive past its leader, and
                // `killpg`/`TerminateJobObject` reaps it with the rest.
                let _ = entry.killer.tree();
            }
        }

        drop(entry.writer);
        let _ = entry.watcher.join();

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

/// Poll the watcher's `exited` flag every 25ms until it flips or `timeout`
/// elapses, returning whether the child exited within the window. The shared
/// grace-window wait of the `Graceful` and `GracefulTree` kill policies.
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
