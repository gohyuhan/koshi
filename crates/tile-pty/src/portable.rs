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

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty};
use tile_core::{
    ids::PaneId,
    process::{ExitStatus, KillPolicy, PtySize, SpawnSpec},
};

use crate::{
    backend::state::{PtyBackend, PtyHandle},
    error::PtyError,
};

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
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// Flipped to `true` by the watcher thread the moment the child exits; read
    /// by [`kill`](PortablePtyBackend::kill) to avoid signalling a dead process.
    exited: Arc<AtomicBool>,
    /// Reader thread: drains the master's read half into the output channel.
    reader: JoinHandle<()>,
    /// Watcher thread: blocks on the child, records exit status, flips `exited`.
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
    /// (`child.wait()` → exit channel, and flips the `exited` flag). Both are
    /// joined later in [`kill`](Self::kill).
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
        let child = pair.slave.spawn_command(cmd).map_err(|e| PtyError::Spawn {
            detail: e.to_string(),
        })?;

        // 4. Drop OUR copy of the slave. The child kept its own; once the child
        //    exits and the kernel closes its end, the master's read half reports
        //    EOF — that is how the reader thread (step 6) learns to stop.
        drop(pair.slave);

        // 5. Pull the control handles off the child/master before either is moved
        //    away. `killer` signals the child; `reader` is a cloned read half of
        //    the master (child output); `writer` is its write half (child input);
        //    `exited` is the flag the watcher flips and `kill` reads.
        let killer = child.clone_killer();
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

        // 6. Reader thread: block on the master read half, forwarding each chunk
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
        // 7. Watcher thread: block on `child.wait()`, map the OS exit status into
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

        // 8. Retain the master, writer, killer, flag and both thread handles
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
        let mut target_panes = self
            .panes
            .lock()
            .unwrap()
            .remove(&pane)
            .ok_or(PtyError::UnknownPane { pane })?;

        match kill_policy {
            KillPolicy::Force | KillPolicy::Tree => {
                let _ = target_panes.killer.kill();
            }
            KillPolicy::Graceful { timeout } => {
                // Give the child the grace window to exit on its own, polling the
                // watcher's `exited` flag; SIGKILL only if it overstays the deadline.
                let deadline = Instant::now() + timeout;
                while Instant::now() < deadline {
                    if target_panes.exited.load(Ordering::SeqCst) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                if !target_panes.exited.load(Ordering::SeqCst) {
                    let _ = target_panes.killer.kill();
                }
            }
        }

        let _ = target_panes.reader.join();
        let _ = target_panes.watcher.join();

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
