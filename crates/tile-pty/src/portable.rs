use std::{
    collections::HashMap,
    io::{ErrorKind, Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty};
use tile_core::{
    ids::PaneId,
    process::{ExitStatus, KillPolicy, PtySize, SpawnSpec},
};

use crate::{
    backend::state::{PtyBackend, PtyHandle},
    error::PtyError,
    kill::PtyChildKillControl,
};

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
    /// Write half of the master — input bytes pushed here arrive at the child.
    writer: Box<dyn Write + Send>,
    /// Kill handle for the child process; `kill()` sends the terminating signal.
    killer: PtyChildKillControl,
    /// Flipped to `true` by the watcher thread the moment the child exits; read
    /// by [`kill`](PortablePtyBackend::kill) to avoid signalling a dead process.
    exited: Arc<AtomicBool>,
    /// Reader thread: drains the master's read half into the output channel.
    ///
    /// Retained but deliberately never joined: a child can `setsid` into a new
    /// process group while still holding the slave fd, so even `KillPolicy::Tree`
    /// cannot guarantee the reader reaches EOF — joining it could block forever.
    /// The handle is kept so a future backend-teardown path can join readers on
    /// clean shutdown; `expect` flags the day that wiring lands.
    #[expect(dead_code)]
    reader: JoinHandle<()>,
    /// Watcher thread: blocks on the child, records exit status, flips `exited`,
    /// and publishes the status on the exit channel.
    ///
    /// `kill` moves the whole entry onto a detached teardown thread and joins this
    /// watcher there, so the pty stays open until the leader actually dies — the
    /// child exits on the signal we send, not on a stdin EOF it would get from a
    /// prematurely dropped master — while the caller never blocks. The *reader* is
    /// never joined (a descendant can hold the slave open, so it may never EOF).
    watcher: JoinHandle<()>,
}

/// Real OS-PTY backend built on the `portable-pty` crate. Each spawned pane gets
/// a kernel PTY plus two helper threads (reader + watcher); the backend owns
/// them all through the [`PaneEntry`] map.
pub struct PortablePtyBackend {
    panes: Mutex<HashMap<PaneId, PaneEntry>>,
}

impl PortablePtyBackend {
    pub fn new() -> Self {
        PortablePtyBackend {
            panes: Mutex::new(HashMap::new()),
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
    /// Returns a [`PtyHandle`] the caller polls for output and exit status. The
    /// child runs detached on two background threads owned by the backend: a
    /// **reader** (master output → output channel) and a **watcher**
    /// (`child.wait()` → exit channel, and flips the `exited` flag).
    ///
    /// # Errors
    /// Returns [`PtyError::Spawn`] if the PTY can't be opened, the command can't
    /// be launched, or the master's reader/writer can't be taken.
    fn spawn(&self, spec: SpawnSpec, size: PtySize) -> Result<PtyHandle, PtyError> {
        // 1. Mint the pane id and a channel-backed handle. `output_sender` /
        //    `exit_sender` are the producing ends the threads below feed; the
        //    caller keeps the consuming ends inside `handle`.
        let pane_id = PaneId::new();
        let (handle, output_sender, exit_sender) = PtyHandle::new(pane_id);

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
        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
        }
        for (k, v) in &spec.env {
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

        // 7. Reader thread: block on the master read half, forwarding each chunk
        //    of child output into the output channel until EOF (child gone) or
        //    the caller drops the receiver.
        let r_handle = thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut reader = reader;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF = shell gone
                    Ok(n) => {
                        if output_sender.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    } // runtime dropped handle
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
        // 8. Watcher thread: block on `child.wait()`, map the OS exit status into
        //    our `ExitStatus`, flip `exited` so `kill` won't signal a corpse, then
        //    publish the status on the exit channel.
        let exited_w = Arc::clone(&exited);
        let w_handle = thread::spawn(move || {
            let mut child = child; // owns it; wait() needs &mut
            let status = match child.wait() {
                Ok(s) => map_status(s),
                Err(_) => ExitStatus::ExitCode(-1),
            };
            exited_w.store(true, Ordering::SeqCst); // tell kill() it's dead
            let _ = exit_sender.send(status);
        });

        // 9. Retain the master, writer, killer, flag and both thread handles
        //    under the pane id, then hand the caller its polling handle.
        self.panes.lock().unwrap().insert(
            pane_id,
            PaneEntry {
                master: pair.master,
                writer,
                killer,
                exited,
                reader: r_handle,
                watcher: w_handle,
            },
        );
        Ok(handle)
    }
    fn resize(&self, pane: PaneId, size: PtySize) -> Result<(), PtyError> {
        let panes = self.panes.lock().unwrap();
        let Some(target_pane) = panes.get(&pane) else {
            return Err(PtyError::UnknownPane { pane });
        };
        target_pane
            .master
            .resize(to_pp_size(size))
            .map_err(|e| PtyError::Io {
                detail: e.to_string(),
            })
    }
    fn write(&self, pane: PaneId, bytes: &[u8]) -> Result<(), PtyError> {
        let mut panes = self.panes.lock().unwrap();
        let Some(target_pane) = panes.get_mut(&pane) else {
            return Err(PtyError::UnknownPane { pane });
        };

        target_pane
            .writer
            .write_all(bytes)
            .and_then(|_| target_pane.writer.flush())
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

        // `kill` must never block the caller: the runtime issues it from an async
        // task on its event loop. So the whole teardown runs on a detached thread
        // and `kill` returns at once; the child's death is observed on the exit
        // channel. The thread keeps the pty open until the leader actually dies (it
        // joins the watcher) — a graceful child then exits on the signal we send,
        // not on the stdin EOF it would get from a dropped master mid-window. It
        // never joins the *reader*: a descendant can hold the slave open, so the
        // reader may never reach EOF.
        thread::spawn(move || {
            // `Force`/`Graceful` signal the leader PID, so skip them once the
            // watcher has reaped it — a recycled PID could belong to an unrelated
            // process. `Tree` signals the whole group/job (`killpg` /
            // `TerminateJobObject`), which stays valid while any member lives, so it
            // fires unconditionally: a same-group descendant can outlive the leader
            // and `Tree` must still reap it (the `exited` flag tracks only the
            // leader, not whether the group is empty).
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
                        // SIGTERM now, then the grace window; SIGKILL only if the
                        // child is still alive when it lapses.
                        let _ = entry.killer.request_stop();
                        let deadline = Instant::now() + timeout;
                        while Instant::now() < deadline {
                            if entry.exited.load(Ordering::SeqCst) {
                                break;
                            }
                            thread::sleep(Duration::from_millis(25));
                        }
                        if !entry.exited.load(Ordering::SeqCst) {
                            let _ = entry.killer.force();
                        }
                    }
                }
            }

            // Wait for the leader to die before the pty is released: `entry` (and
            // with it the master/writer) drops at the end of this closure, so
            // joining the watcher first keeps the child's exit driven by our signal
            // rather than by a premature stdin EOF. The reader is left detached.
            let _ = entry.watcher.join();
        });

        Ok(())
    }
}

fn to_pp_size(s: PtySize) -> portable_pty::PtySize {
    portable_pty::PtySize {
        rows: s.rows,
        cols: s.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

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

#[cfg(all(test, unix))]
mod tests;
