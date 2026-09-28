//! Helpers the `portable` and `portable_windows` integration tests share: a
//! PTY sink that records each pane's output and exit, the waits that read it
//! back, and a `kill_pane` call bounded by a deadline.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use koshi_core::ids::PaneId;
use koshi_core::process::{ExitStatus, KillPolicy, PtySize, ShellKind, SpawnSpec};
use koshi_pty::backend::state::{PtyBackend, PtySink};
use koshi_pty::error::PtyError;
use koshi_pty::portable::PortablePtyBackend;

/// Standard test terminal size: 80 columns × 24 rows.
pub const STANDARD_PTY_SIZE: PtySize = PtySize {
    column_count: 80,
    row_count: 24,
};

/// The longest a test waits for one `kill_pane` call to return. Every grace
/// window a test hands `kill_pane` is shorter.
pub const KILL_RETURN_DEADLINE_DURATION: Duration = Duration::from_secs(10);

/// The output chunks and the exit status one pane has delivered and no helper
/// has taken yet.
#[derive(Default)]
struct PaneDeliveries {
    /// Output chunks, oldest first.
    output_chunks: VecDeque<Vec<u8>>,
    /// The child's exit status, once the backend reports it.
    exit_status: Option<ExitStatus>,
}

/// A PTY sink that keeps each pane's deliveries apart, for the helpers below
/// to take in arrival order.
#[derive(Default)]
pub struct PaneOutputRecorder {
    /// What each pane has delivered and no helper has taken yet.
    deliveries_by_pane_id: Mutex<HashMap<PaneId, PaneDeliveries>>,
}

impl PaneOutputRecorder {
    /// The oldest output chunk `pane_id` delivered and no call took yet, or
    /// `None` while there is none.
    fn take_pane_output_chunk(&self, pane_id: PaneId) -> Option<Vec<u8>> {
        self.deliveries_by_pane_id
            .lock()
            .expect("recorder")
            .get_mut(&pane_id)?
            .output_chunks
            .pop_front()
    }

    /// The exit status `pane_id` delivered, or `None` while it has none. The
    /// status is taken once; the next call answers `None`.
    fn take_pane_exit_status(&self, pane_id: PaneId) -> Option<ExitStatus> {
        self.deliveries_by_pane_id
            .lock()
            .expect("recorder")
            .get_mut(&pane_id)?
            .exit_status
            .take()
    }
}

impl PtySink for PaneOutputRecorder {
    fn accept_output_bytes(&self, pane_id: PaneId, output_bytes: Vec<u8>) -> bool {
        self.deliveries_by_pane_id
            .lock()
            .expect("recorder")
            .entry(pane_id)
            .or_default()
            .output_chunks
            .push_back(output_bytes);
        true
    }

    fn accept_exit_status(&self, pane_id: PaneId, exit_status: ExitStatus) {
        self.deliveries_by_pane_id
            .lock()
            .expect("recorder")
            .entry(pane_id)
            .or_default()
            .exit_status = Some(exit_status);
    }
}

/// A backend delivering every pane into a fresh [`PaneOutputRecorder`], and
/// that recorder.
pub fn build_pty_backend() -> (Arc<PortablePtyBackend>, Arc<PaneOutputRecorder>) {
    let pane_output_recorder = Arc::new(PaneOutputRecorder::default());
    let pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(
        Arc::clone(&pane_output_recorder) as _,
    ));
    (pty_backend, pane_output_recorder)
}

/// Build a spawn spec for `program` with `command_arguments`, inheriting the current working
/// directory and environment variables.
pub fn build_spawn_spec(program: &str, command_arguments: &[&str]) -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from(program),
        arguments: command_arguments
            .iter()
            .map(|argument| argument.to_string())
            .collect(),
        working_directory: None,
        environment_variables: BTreeMap::new(),
        shell_kind: ShellKind::from_program(Path::new(program)),
    }
}

/// Read `pane_id`'s output from `pane_output_recorder` until
/// `expected_pane_output_text` appears or `timeout_duration` runs out, and hand back
/// everything read. Writes nothing to the pane.
pub fn read_pane_output_until(
    pane_output_recorder: &PaneOutputRecorder,
    pane_id: PaneId,
    expected_pane_output_text: &str,
    timeout_duration: Duration,
) -> String {
    let deadline = Instant::now() + timeout_duration;
    let mut child_output_bytes: Vec<u8> = Vec::new();
    while Instant::now() < deadline {
        match pane_output_recorder.take_pane_output_chunk(pane_id) {
            Some(pane_output_chunk_bytes) => {
                child_output_bytes.extend_from_slice(&pane_output_chunk_bytes);
                if String::from_utf8_lossy(&child_output_bytes).contains(expected_pane_output_text)
                {
                    break;
                }
            }
            None => thread::sleep(Duration::from_millis(5)),
        }
    }
    String::from_utf8_lossy(&child_output_bytes).into_owned()
}

/// Poll `pane_output_recorder` for `pane_id`'s exit status until it arrives or
/// `timeout_duration` elapses.
pub fn wait_for_pane_exit(
    pane_output_recorder: &PaneOutputRecorder,
    pane_id: PaneId,
    timeout_duration: Duration,
) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout_duration;
    loop {
        if let Some(exit_status) = pane_output_recorder.take_pane_exit_status(pane_id) {
            return Some(exit_status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// A `kill_pane` call running on its own thread.
pub struct PendingPaneKill {
    /// The policy the call was made with.
    kill_policy: KillPolicy,
    /// What the call returned, and how long it took.
    kill_outcome_receiver: Receiver<(Result<(), PtyError>, Duration)>,
}

impl PendingPaneKill {
    /// Wait for the call to return, and hand back how long it took.
    ///
    /// Panics when the call returns an error, or has not returned within
    /// [`KILL_RETURN_DEADLINE_DURATION`].
    pub fn wait_for_kill_return(self) -> Duration {
        match self
            .kill_outcome_receiver
            .recv_timeout(KILL_RETURN_DEADLINE_DURATION)
        {
            Ok((pane_kill_result, kill_elapsed_duration)) => {
                assert_eq!(
                    pane_kill_result,
                    Ok(()),
                    "kill_pane({:?}) failed",
                    self.kill_policy
                );
                kill_elapsed_duration
            }
            Err(_) => panic!(
                "kill_pane({:?}) did not return within {KILL_RETURN_DEADLINE_DURATION:?}",
                self.kill_policy
            ),
        }
    }
}

/// Start `kill_pane(pane_id, kill_policy)` on `pty_backend` on its own thread.
pub fn start_pane_kill(
    pty_backend: &Arc<PortablePtyBackend>,
    pane_id: PaneId,
    kill_policy: KillPolicy,
) -> PendingPaneKill {
    let (kill_outcome_sender, kill_outcome_receiver) = mpsc::channel();
    let killing_pty_backend = Arc::clone(pty_backend);
    thread::spawn(move || {
        let kill_started_at = Instant::now();
        let pane_kill_result = killing_pty_backend.kill_pane(pane_id, kill_policy);
        let _ = kill_outcome_sender.send((pane_kill_result, kill_started_at.elapsed()));
    });
    PendingPaneKill {
        kill_policy,
        kill_outcome_receiver,
    }
}

/// Run `kill_pane(pane_id, kill_policy)` on `pty_backend`, and hand back how
/// long it took.
///
/// Panics when the call returns an error, or has not returned within
/// [`KILL_RETURN_DEADLINE_DURATION`].
pub fn kill_pane_within_deadline(
    pty_backend: &Arc<PortablePtyBackend>,
    pane_id: PaneId,
    kill_policy: KillPolicy,
) -> Duration {
    start_pane_kill(pty_backend, pane_id, kill_policy).wait_for_kill_return()
}
