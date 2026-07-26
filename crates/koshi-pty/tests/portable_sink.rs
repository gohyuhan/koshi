//! Integration tests for the real `portable-pty` backend driven through a
//! [`PtySink`] instead of the handle's channels.
//!
//! This is the route the running binary takes: the pane's own reader thread
//! delivers each chunk to the consumer, so no relay thread exists per pane.
//! What has to hold is the order the consumer observes — every byte the child
//! printed, and only then the child's exit — because that is what decides
//! whether a pane closes before its last output is drawn.
//!
//! Every test here runs on all three targets. The only platform difference is
//! the shell each script is handed to and the words that script is written in;
//! the behavior asserted is identical, because a pane on Windows has to order
//! its output and exit exactly like a pane on Linux or macOS.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use koshi_core::ids::PaneId;
use koshi_core::process::{ExitStatus, KillPolicy, PtySize, ShellKind, SpawnSpec};
use koshi_pty::backend::state::{PtyBackend, PtySink};
use koshi_pty::portable::PortablePtyBackend;

/// Standard test window size: 80 columns × 24 rows.
const SIZE: PtySize = PtySize { cols: 80, rows: 24 };

/// How long a test waits for a short-lived child to finish and report. Long
/// enough for a cold ConPTY start, which is slower than opening a Unix pair.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How long the reader gets to stop once its consumer refuses a chunk.
///
/// [`BLOCKS_AFTER_PRINTING`] keeps its child alive far longer than this on
/// every platform, so the wait only completes if the reader really did give up
/// early rather than sitting on the child's exit.
const READER_STOP_DEADLINE: Duration = Duration::from_secs(5);

/// Serializes PTY creation across the parallel test threads, for the same
/// reason the channel-route tests do: macOS `openpty(3)` races under
/// concurrent allocation.
static PTY_GATE: Mutex<()> = Mutex::new(());

/// The shell a test script is handed to.
#[cfg(windows)]
const SHELL: &str = "cmd.exe";
#[cfg(not(windows))]
const SHELL: &str = "/bin/sh";

/// The flag telling [`SHELL`] to run the following argument as a script.
#[cfg(windows)]
const SHELL_FLAG: &str = "/C";
#[cfg(not(windows))]
const SHELL_FLAG: &str = "-c";

/// The word a test looks for in a child's output. Short enough that no
/// terminal wraps it, so it never arrives split by an escape sequence.
const MARKER: &str = "koshi-sink-marker";

/// A script printing [`MARKER`] and then exiting with code 3.
#[cfg(windows)]
const PRINTS_THEN_EXITS_3: &str = "echo koshi-sink-marker& exit 3";
#[cfg(not(windows))]
const PRINTS_THEN_EXITS_3: &str = "printf koshi-sink-marker; exit 3";

/// A script printing one character and then blocking for far longer than any
/// test here waits, so a child still running is unambiguous.
#[cfg(windows)]
const BLOCKS_AFTER_PRINTING: &str = "echo x& ping -n 100 127.0.0.1 >NUL";
#[cfg(not(windows))]
const BLOCKS_AFTER_PRINTING: &str = "printf x; sleep 30";

/// A script exiting successfully without printing anything.
const EXITS_0: &str = "exit 0";

/// One thing the backend told the sink, in the order it was told.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Delivered {
    /// A chunk of child output.
    Output(Vec<u8>),
    /// The child's final status.
    Exit(ExitStatus),
}

/// A sink that records every delivery so a test can assert on the sequence.
struct Recorder {
    /// Everything delivered so far, oldest first.
    seen: Mutex<Vec<Delivered>>,
}

impl Recorder {
    fn new() -> Arc<Self> {
        Arc::new(Recorder {
            seen: Mutex::new(Vec::new()),
        })
    }

    /// A snapshot of what has been delivered so far.
    fn snapshot(&self) -> Vec<Delivered> {
        self.seen.lock().expect("recorder").clone()
    }

    /// Every output chunk so far, concatenated and read as lossy UTF-8.
    fn text(&self) -> String {
        let joined: Vec<u8> = self
            .snapshot()
            .into_iter()
            .filter_map(|entry| match entry {
                Delivered::Output(bytes) => Some(bytes),
                Delivered::Exit(_) => None,
            })
            .flatten()
            .collect();
        String::from_utf8_lossy(&joined).into_owned()
    }

    /// The recorded exit status, if the child has been reported as ended.
    fn exit(&self) -> Option<ExitStatus> {
        self.snapshot().into_iter().find_map(|entry| match entry {
            Delivered::Exit(status) => Some(status),
            Delivered::Output(_) => None,
        })
    }

    /// Block until an exit has been recorded, or `TIMEOUT` elapses.
    fn wait_for_exit(&self) -> Option<ExitStatus> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.exit() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl PtySink for Recorder {
    fn output(&self, _pane: PaneId, bytes: Vec<u8>) -> bool {
        self.seen
            .lock()
            .expect("recorder")
            .push(Delivered::Output(bytes));
        true
    }

    fn exit(&self, _pane: PaneId, status: ExitStatus) {
        self.seen
            .lock()
            .expect("recorder")
            .push(Delivered::Exit(status));
    }
}

/// A sink that refuses everything it is handed, standing in for a consumer
/// that has gone away — a runtime whose inbox is closed.
struct RefusingSink {
    /// Set when a chunk was offered and refused.
    refused_output: Mutex<bool>,
    /// Set if an exit was reported, which must not happen to a gone consumer.
    saw_exit: Mutex<bool>,
}

impl RefusingSink {
    fn new() -> Arc<Self> {
        Arc::new(RefusingSink {
            refused_output: Mutex::new(false),
            saw_exit: Mutex::new(false),
        })
    }
}

impl PtySink for RefusingSink {
    fn output(&self, _pane: PaneId, _bytes: Vec<u8>) -> bool {
        *self.refused_output.lock().expect("refusing sink") = true;
        false
    }

    fn exit(&self, _pane: PaneId, _status: ExitStatus) {
        *self.saw_exit.lock().expect("refusing sink") = true;
    }
}

/// Build a spawn spec running `body` through the platform's shell, inheriting
/// cwd and env.
fn script(body: &str) -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from(SHELL),
        args: vec![SHELL_FLAG.to_string(), body.to_string()],
        cwd: None,
        env: BTreeMap::new(),
        shell_kind: ShellKind::from_program(Path::new(SHELL)),
    }
}

#[test]
fn a_sink_receives_the_childs_output_and_then_its_exit() {
    let recorder = Recorder::new();
    let backend = PortablePtyBackend::with_sink(recorder.clone());
    {
        let _gate = PTY_GATE.lock().expect("pty gate");
        backend
            .spawn(PaneId::new(), script(PRINTS_THEN_EXITS_3), SIZE)
            .expect("spawn child");
    }

    assert_eq!(recorder.wait_for_exit(), Some(ExitStatus::ExitCode(3)));

    // The child's whole output reached the sink...
    assert!(
        recorder.text().contains(MARKER),
        "sink never saw the child's output; got {:?}",
        recorder.text()
    );

    // ...and the exit came last, after every chunk. A pane told its child
    // ended while output is still arriving would close over unread text.
    let seen = recorder.snapshot();
    let exit_index = seen
        .iter()
        .position(|entry| matches!(entry, Delivered::Exit(_)))
        .expect("exit recorded");
    assert_eq!(
        exit_index,
        seen.len() - 1,
        "exit was not the last delivery: {seen:?}"
    );
}

#[test]
fn a_reader_stops_when_the_consumer_goes_away_even_while_the_child_lives_on() {
    // The reader waits for the child's exit before reporting it, which is what
    // orders output ahead of exit. That wait must be skipped once the consumer
    // is gone, or a child that outlives its consumer pins the reader thread for
    // as long as it keeps running.
    let sink = RefusingSink::new();
    let backend = PortablePtyBackend::with_sink(sink.clone());
    let pane_id = PaneId::new();
    {
        let _gate = PTY_GATE.lock().expect("pty gate");
        backend
            .spawn(pane_id, script(BLOCKS_AFTER_PRINTING), SIZE)
            .expect("spawn child");
    }

    // The reader holds a reference to the sink for as long as it runs, so the
    // count falling back to two — this test and the backend — is the reader
    // having stopped. The child is still blocking, so a reader that waited for
    // the exit would hold its reference far past this deadline.
    let deadline = Instant::now() + READER_STOP_DEADLINE;
    while Arc::strong_count(&sink) > 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        Arc::strong_count(&sink),
        2,
        "reader thread was still running while the child blocked"
    );
    assert!(
        *sink.refused_output.lock().expect("refusing sink"),
        "the sink was never offered any output"
    );
    assert!(
        !*sink.saw_exit.lock().expect("refusing sink"),
        "an exit was reported to a consumer that had already gone"
    );

    // Reap the blocking child rather than leaving it behind. The group kill
    // takes any process the script started with it — on Windows the script's
    // `ping` is a child of `cmd.exe`, so killing the leader alone would orphan
    // it.
    backend.kill(pane_id, KillPolicy::Tree).expect("kill pane");
}

#[test]
fn a_sink_backed_pane_hands_back_a_handle_with_no_channels() {
    // The handle carries no receivers, which is how the runtime knows this
    // pane needs no forwarder thread — it is already delivering to the sink.
    let recorder = Recorder::new();
    let backend = PortablePtyBackend::with_sink(recorder.clone());
    let pane_id = PaneId::new();
    let mut handle = {
        let _gate = PTY_GATE.lock().expect("pty gate");
        backend
            .spawn(pane_id, script(EXITS_0), SIZE)
            .expect("spawn child")
    };

    assert_eq!(handle.pane_id(), pane_id);
    assert!(handle.take_receivers().is_none());
    assert_eq!(handle.try_read_output(), None);
    assert_eq!(handle.try_exit_status(), None);

    // The sink is still the one being fed.
    assert_eq!(recorder.wait_for_exit(), Some(ExitStatus::ExitCode(0)));
}
