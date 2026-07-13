//! Real OS-PTY backend built on the `portable-pty` crate.
//!
//! A spawned pane gets a kernel PTY and three helper threads (reader, writer,
//! watcher), all owned through the [`crate::portable::PortablePtyBackend`] pane map. The
//! implementation handles child output streaming, input queuing, process
//! termination (with cross-platform kill policies), and exit status tracking.

use std::{
    collections::HashMap,
    io::{ErrorKind, Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{channel, RecvTimeoutError, Sender},
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
    backend::state::{PtyBackend, PtyHandle},
    env::build_env,
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
    /// Input channel to the per-pane writer thread. Bytes sent here are written
    /// to the master (and so reach the child) off the dispatcher, so a child that
    /// has stopped reading its stdin blocks only the writer thread, never the
    /// dispatcher. Dropping this `Sender` (on `kill`/teardown) closes the channel
    /// so a writer parked in `recv` exits. A writer already blocked *inside*
    /// `write_all` — child stopped reading while a `setsid` descendant still holds
    /// the slave open (Linux; macOS `revoke`s it) — cannot be interrupted; like the
    /// reader it detaches and exits only once that fd finally closes. `kill` never
    /// joins it, so this can leak a thread + fd in that case but never blocks the
    /// dispatcher.
    writer: Sender<Vec<u8>>,
    /// Kill handle for the child process; `kill()` sends the terminating signal.
    killer: PtyChildKillControl,
    /// Flipped to `true` by the watcher thread the moment the child exits; read
    /// by [`kill`](PortablePtyBackend::kill) to avoid signalling a dead process.
    exited: Arc<AtomicBool>,
    /// Reader thread: drains the master's read half into the output channel.
    ///
    /// Not joined on teardown: the slave fd may outlive the child (e.g., when the
    /// child `setsid`s into a new process group), so the thread could block forever
    /// if joined. It exits on its own once the fd closes. Retained so the struct
    /// owns the handle.
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
}

impl PortablePtyBackend {
    /// Creates a new, empty PTY backend with no active panes.
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
    /// Returns a [`crate::backend::state::PtyHandle`] the caller polls for output and exit status. The
    /// child runs detached on three background threads owned by the backend: a
    /// **reader** (master output → output channel), a **writer** (input channel →
    /// master, so writes never block the dispatcher), and a **watcher**
    /// (`child.wait()` → exit channel, and flips the `exited` flag).
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
        // 1. Build a channel-backed handle for the caller's pane id.
        //    `output_sender` / `exit_sender` are the producing ends the threads
        //    below feed; the caller keeps the consuming ends inside `handle`.
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

        // 9. Writer thread: own the master's write half and drain the input
        //    channel onto it, so a write to a child that has stopped reading
        //    blocks only this thread, never the dispatcher. It exits on either
        //    teardown path: the channel closing (the entry's `Sender` dropped by
        //    `kill`/teardown → `Disconnected`), or the watcher flagging the child
        //    as exited — `recv_timeout` wakes periodically to check `exited` so a
        //    pane kept open past its child's death still reclaims the thread.
        let (writer_sender_handler, writer_receiver) = channel::<Vec<u8>>();
        let exited_writer = Arc::clone(&exited);

        let _ = thread::spawn(move || {
            let writer_receiver = writer_receiver;
            let mut writer = writer;
            loop {
                match writer_receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(bytes) => {
                        let _ = writer.write_all(&bytes).and_then(|_| writer.flush());
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if exited_writer.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
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
                writer: writer_sender_handler,
                killer,
                exited,
                reader: r_handle,
                watcher: w_handle,
            },
        );
        drop(panes);
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
            .send(bytes.to_vec())
            .map_err(|e| PtyError::Io {
                detail: e.to_string(),
            })
    }
    fn kill(&self, pane: PaneId, kill_policy: KillPolicy) -> Result<(), PtyError> {
        let target_panes = self
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
                if !target_panes.exited.load(Ordering::SeqCst) {
                    let _ = target_panes.killer.force();
                }
            }
            KillPolicy::Tree => {
                let _ = target_panes.killer.tree();
            }
            KillPolicy::Graceful { timeout } => {
                if !target_panes.exited.load(Ordering::SeqCst) {
                    // Ask the leader to exit, give it the grace window; SIGKILL
                    // only if it overstays the deadline.
                    let _ = target_panes.killer.request_stop();
                    if !wait_for_exit(&target_panes.exited, timeout) {
                        let _ = target_panes.killer.force();
                    }
                }
            }
            KillPolicy::GracefulTree { timeout } => {
                if !target_panes.exited.load(Ordering::SeqCst) {
                    // Ask the whole group to exit — every member gets the stop
                    // request and the grace window — then wait for the leader.
                    let _ = target_panes.killer.request_stop_tree();
                    wait_for_exit(&target_panes.exited, timeout);
                }

                // Group-kill even when the leader already exited: a disowned
                // descendant can keep the group alive past its leader, and
                // `killpg`/`TerminateJobObject` reaps it with the rest.
                let _ = target_panes.killer.tree();
            }
        }

        drop(target_panes.writer);
        let _ = target_panes.watcher.join();

        Ok(())
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
