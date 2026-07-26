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

/// How many holders of a pane's sink remain once its reader thread has
/// stopped: the test itself, the backend, and the watcher — which keeps one so
/// it can publish the exit when the PTY never reports an end.
const HOLDERS_WITHOUT_READER: usize = 3;

/// The window the watcher stands by for before publishing a pane's exit
/// itself. Mirrors `EXIT_PUBLISH_GRACE` in the backend, which is private.
const EXIT_PUBLISH_GRACE: Duration = Duration::from_millis(100);

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

/// A script that leaves a background process holding the terminal open and
/// then exits with code 5, so the child ends while the PTY reports no end.
#[cfg(windows)]
const OUTLIVED_BY_A_DESCENDANT: &str = "start /b ping -n 100 127.0.0.1 >NUL& exit 5";
#[cfg(not(windows))]
const OUTLIVED_BY_A_DESCENDANT: &str = "sleep 30 & exit 5";

/// One thing the backend told the sink, in the order it was told.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Delivered {
    /// A chunk of child output.
    Output(Vec<u8>),
    /// The child's final status.
    Exit(ExitStatus),
}

/// The exit status among `delivered`, if a child has been reported as ended.
fn exit_among(delivered: &[Delivered]) -> Option<ExitStatus> {
    delivered.iter().find_map(|entry| match entry {
        Delivered::Exit(status) => Some(*status),
        Delivered::Output(_) => None,
    })
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
        exit_among(&self.snapshot())
    }

    /// Block until an exit has been recorded, or `TIMEOUT` elapses, answering
    /// the child's cursor-position queries meanwhile — see
    /// [`answer_cursor_queries`].
    fn wait_for_exit(&self, backend: &PortablePtyBackend, pane: PaneId) -> Option<ExitStatus> {
        let deadline = Instant::now() + TIMEOUT;
        let mut answered = 0;
        loop {
            answered = answer_cursor_queries(backend, pane, &self.text(), answered);
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

/// The cursor-position query a child sends to find out where it is printing:
/// `ESC [ 6 n`. Windows' console layer sends one as a pane starts up and waits
/// for the answer before letting the child run.
const CURSOR_QUERY: &str = "\x1b[6n";

/// The answer to [`CURSOR_QUERY`]: the cursor sits at row 1, column 1.
const CURSOR_REPLY: &[u8] = b"\x1b[1;1R";

/// Answer any cursor-position query in `output` that has not been answered
/// yet, returning the new total answered.
///
/// A real pane answers these through the terminal engine, which parses the
/// child's output and writes replies back. These tests drive the backend on
/// its own, with no engine between, so an unanswered query leaves the child
/// waiting forever and it never runs — on Windows that means no output, no
/// exit, and a pane that never ends.
///
/// Nothing is written when no query has arrived, so a child that never asks
/// (every Unix shell here) is never sent bytes it did not ask for.
fn answer_cursor_queries(
    backend: &PortablePtyBackend,
    pane: PaneId,
    output: &str,
    answered: usize,
) -> usize {
    let asked = output.matches(CURSOR_QUERY).count();
    for _ in answered..asked {
        let _ = backend.write(pane, CURSOR_REPLY);
    }
    asked
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
fn a_channel_backed_pane_reports_the_same_childs_exit() {
    // The control for every sink test here: the same child, ending the same
    // way, delivered through the handle's channels instead. The watcher
    // observes the child's end and feeds both routes, so this separates the
    // two halves — a failure here is the child's end not being observed at
    // all, while this passing alongside a failing sink test puts the fault in
    // the sink route.
    let backend = PortablePtyBackend::new();
    let pane_id = PaneId::new();
    let handle = {
        let _gate = PTY_GATE.lock().expect("pty gate");
        backend
            .spawn(pane_id, script(PRINTS_THEN_EXITS_3), SIZE)
            .expect("spawn child")
    };

    let deadline = Instant::now() + TIMEOUT;
    let mut status = None;
    let mut seen = Vec::new();
    let mut answered = 0;
    while status.is_none() && Instant::now() < deadline {
        // Drain output too, so a full buffer can never be what stops the child.
        while let Some(chunk) = handle.try_read_output() {
            seen.extend(chunk);
        }
        answered =
            answer_cursor_queries(&backend, pane_id, &String::from_utf8_lossy(&seen), answered);
        status = handle.try_exit_status();
        if status.is_none() {
            thread::sleep(Duration::from_millis(5));
        }
    }
    assert_eq!(
        status,
        Some(ExitStatus::ExitCode(3)),
        "the child's end was never observed on the channel route either"
    );
}

#[test]
fn a_sink_receives_the_childs_output_and_then_its_exit() {
    let recorder = Recorder::new();
    let backend = PortablePtyBackend::with_sink(recorder.clone());
    let pane_id = PaneId::new();
    {
        let _gate = PTY_GATE.lock().expect("pty gate");
        backend
            .spawn(pane_id, script(PRINTS_THEN_EXITS_3), SIZE)
            .expect("spawn child");
    }

    assert_eq!(
        recorder.wait_for_exit(&backend, pane_id),
        Some(ExitStatus::ExitCode(3)),
        "no exit reached the sink; it was handed: {:?}",
        recorder.snapshot()
    );

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
    // count falling to the holders that outlive it is the reader having
    // stopped. Those are this test, the backend, and the watcher — which stays
    // parked on the child, and the child is still blocking, so it cannot be the
    // one that let go inside this deadline. A reader that waited for the exit
    // would hold its own reference far past it.
    let deadline = Instant::now() + READER_STOP_DEADLINE;
    while Arc::strong_count(&sink) > HOLDERS_WITHOUT_READER && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        Arc::strong_count(&sink),
        HOLDERS_WITHOUT_READER,
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
fn a_pane_reports_its_child_ending_even_when_the_pty_never_reports_an_end() {
    // The child exits while a descendant it started keeps the terminal open,
    // so no end-of-file ever arrives and the reader stays blocked. The pane
    // must still be told its child ended — a consumer waiting on that is how a
    // pane closes, and waiting for the descendant would hold it open for as
    // long as that descendant runs.
    //
    // Windows reaches this path for every pane, not just this arrangement:
    // ConPTY keeps a pane's console readable after its child is gone.
    let recorder = Recorder::new();
    let backend = PortablePtyBackend::with_sink(recorder.clone());
    let pane_id = PaneId::new();
    {
        let _gate = PTY_GATE.lock().expect("pty gate");
        backend
            .spawn(pane_id, script(OUTLIVED_BY_A_DESCENDANT), SIZE)
            .expect("spawn child");
    }

    assert_eq!(
        recorder.wait_for_exit(&backend, pane_id),
        Some(ExitStatus::ExitCode(5)),
        "the pane was never told its child ended"
    );

    // Reap the descendant still holding the terminal open.
    let _ = backend.kill(pane_id, KillPolicy::Tree);
}

#[test]
fn closing_a_pane_does_not_wait_on_the_exit_backstop() {
    // The watcher stands by to publish a pane's exit when the PTY reports no
    // end. Closing a pane must not sit through that wait: the caller is
    // removing the pane itself, and a close that blocked for the standby
    // window would stall the loop that asked for it once per pane.
    let recorder = Recorder::new();
    let backend = PortablePtyBackend::with_sink(recorder.clone());
    let pane_id = PaneId::new();
    {
        let _gate = PTY_GATE.lock().expect("pty gate");
        backend
            .spawn(pane_id, script(BLOCKS_AFTER_PRINTING), SIZE)
            .expect("spawn child");
    }

    let started = Instant::now();
    backend.kill(pane_id, KillPolicy::Tree).expect("kill pane");
    let took = started.elapsed();

    assert!(
        took < EXIT_PUBLISH_GRACE,
        "closing the pane waited on the exit backstop: took {took:?}, \
         which is not under the {EXIT_PUBLISH_GRACE:?} standby window"
    );
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
    assert_eq!(
        recorder.wait_for_exit(&backend, pane_id),
        Some(ExitStatus::ExitCode(0))
    );
}

#[test]
fn a_gone_consumer_is_never_told_the_child_ended() {
    // A consumer that refuses a chunk is finished with the pane. The watcher
    // stands by to report the exit itself when the PTY reports no end, so
    // unless the reader takes charge of that exit on its way out, a child that
    // ends promptly has its exit handed to a consumer that already said it was
    // done. The refusing-sink test above cannot catch this: its child runs long
    // enough that the watcher never reaches its standby window.
    let sink = RefusingSink::new();
    let backend = PortablePtyBackend::with_sink(sink.clone());
    let pane_id = PaneId::new();
    {
        let _gate = PTY_GATE.lock().expect("pty gate");
        backend
            .spawn(pane_id, script(PRINTS_THEN_EXITS_3), SIZE)
            .expect("spawn child");
    }

    // Well past the window the watcher stands by for, so an exit that was
    // going to be delivered has been by now.
    thread::sleep(EXIT_PUBLISH_GRACE * 4);

    assert!(
        !*sink.saw_exit.lock().expect("refusing sink"),
        "an exit was reported to a consumer that had already gone"
    );

    let _ = backend.kill(pane_id, KillPolicy::Tree);
}

/// A sink that holds the reader inside `output` until the test lets go, so the
/// reader cannot reach the end of the child's output and report the exit
/// itself. Whatever exit arrives came from the watcher.
///
/// Unix only, alongside the one test that parks a reader: on Windows a parked
/// reader never answers the console's startup query, so the child never runs.
#[cfg(unix)]
struct StalledSink {
    /// Everything delivered so far, oldest first.
    seen: Mutex<Vec<Delivered>>,
    /// Held by the test; the reader blocks on it inside `output`.
    gate: Mutex<()>,
}

#[cfg(unix)]
impl StalledSink {
    fn new() -> Arc<Self> {
        Arc::new(StalledSink {
            seen: Mutex::new(Vec::new()),
            gate: Mutex::new(()),
        })
    }

    /// The recorded exit status, if one has been delivered.
    fn exit_status(&self) -> Option<ExitStatus> {
        exit_among(&self.seen.lock().expect("stalled sink"))
    }

    /// How many exits have been delivered. More than one breaks the promise
    /// that a consumer is told a child ended exactly once.
    fn exits_delivered(&self) -> usize {
        self.seen
            .lock()
            .expect("stalled sink")
            .iter()
            .filter(|entry| matches!(entry, Delivered::Exit(_)))
            .count()
    }
}

#[cfg(unix)]
impl PtySink for StalledSink {
    fn output(&self, _pane: PaneId, bytes: Vec<u8>) -> bool {
        self.seen
            .lock()
            .expect("stalled sink")
            .push(Delivered::Output(bytes));
        // Park the reader here. `seen` is released first, so the watcher can
        // still record an exit while this thread waits.
        let _held = self.gate.lock().expect("stalled sink gate");
        true
    }

    fn exit(&self, _pane: PaneId, status: ExitStatus) {
        self.seen
            .lock()
            .expect("stalled sink")
            .push(Delivered::Exit(status));
    }
}

// Unix only: on Windows the reader has to stay responsive to answer the
// console's startup cursor query, and a parked reader never gets there, so the
// child would never run. Windows covers this path another way — its console
// keeps a pane readable after the child is gone, so the reader never reaches
// the end there and every sink test relies on the watcher.
#[cfg(unix)]
#[test]
fn the_watcher_reports_the_exit_when_the_reader_cannot() {
    // The reader is parked inside the sink, so it cannot reach the end of the
    // child's output and report the exit. This is the case the watcher's
    // standby window exists for: without it the pane would never be told its
    // child ended.
    let sink = StalledSink::new();
    let held = sink.gate.lock().expect("hold the reader");
    let backend = PortablePtyBackend::with_sink(sink.clone());
    let pane_id = PaneId::new();
    {
        let _gate = PTY_GATE.lock().expect("pty gate");
        backend
            .spawn(pane_id, script(PRINTS_THEN_EXITS_3), SIZE)
            .expect("spawn child");
    }

    let deadline = Instant::now() + TIMEOUT;
    while sink.exit_status().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        sink.exit_status(),
        Some(ExitStatus::ExitCode(3)),
        "the reader was parked, so only the watcher could report the exit — \
         and nothing did"
    );

    // Release the reader. It now reaches the end of the child's output and
    // tries to report the same exit the watcher already did — the pane must
    // still have been told exactly once.
    drop(held);
    let deadline = Instant::now() + TIMEOUT;
    while Arc::strong_count(&sink) > HOLDERS_WITHOUT_READER - 1 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        sink.exits_delivered(),
        1,
        "the child's end was reported more than once"
    );

    let _ = backend.kill(pane_id, KillPolicy::Tree);
}
