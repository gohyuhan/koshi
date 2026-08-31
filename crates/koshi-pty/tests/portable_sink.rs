//! Integration tests for the real `portable-pty` backend driven through a
//! [`PtySink`] instead of the handle's channels.
//!
//! This is the route the running binary takes: the pane's own reader thread
//! delivers each chunk to the consumer, and no relay thread exists per pane.
//! Each test asserts the order the consumer observes: every byte the child
//! printed, and only then the child's exit.
//!
//! Most tests here run on all three targets. The only platform difference is
//! the shell each script is handed to and the words that script is written
//! in; the behavior asserted is identical. Each Unix-gated test states its
//! own gate above itself, and each has an all-platform unit test in
//! `portable::tests` covering the same claim without a child.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use koshi_core::ids::PaneId;
use koshi_core::process::{ExitStatus, KillPolicy, PtySize, ShellKind, SpawnSpec};
use koshi_pty::backend::state::{PtyBackend, PtyHandle, PtySink};
use koshi_pty::error::PtyError;
use koshi_pty::portable::PortablePtyBackend;

/// Standard test window size: 80 columns × 24 rows.
const SIZE: PtySize = PtySize { cols: 80, rows: 24 };

/// How long a test waits for a short-lived child to finish and report. Long
/// enough for a cold ConPTY start, which is slower than opening a Unix pair.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How long the reader gets to stop once its consumer refuses a chunk.
///
/// [`BLOCKS_AFTER_PRINTING`] keeps its child alive far longer than this on
/// every platform, so the wait completes only when the reader gave up early
/// instead of sitting on the child's exit.
const READER_STOP_DEADLINE: Duration = Duration::from_secs(5);

/// How many holders of a pane's sink remain once its reader has let the sink
/// go: the test itself and the backend. The reader publishes the exit on every
/// platform these tests run on, so the watcher keeps no hold of its own.
///
/// Letting the sink go is not always the thread ending: on Windows the reader
/// stays in `read` on a pane the consumer let go, and closing that pane's
/// console waits for its output to be read out.
const HOLDERS_WITHOUT_READER: usize = 2;

/// How many holders of a closed pane's sink remain once its reader thread has
/// stopped: the test itself and the backend. `kill` joins the watcher, so its
/// hold is already gone by the time `kill` returns.
const HOLDERS_AFTER_CLOSE: usize = 2;

/// The longest the watcher stands by, across every round. Mirrors
/// `EXIT_PUBLISH_LIMIT` in the backend, which is private.
const EXIT_PUBLISH_LIMIT: Duration = Duration::from_secs(1);

/// The grace `kill` gives a child to exit on the stop request. The child in
/// the test that uses it exits on the request inside this window, so `kill`
/// polls for that exit instead of forcing it.
const GRACEFUL_STOP: Duration = Duration::from_secs(5);

/// Serializes PTY creation across the parallel test threads. macOS
/// `openpty(3)` fails with a transient `-6` under concurrent allocation.
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
///
/// `trap '' HUP` keeps the descendant alive past the shell: a session leader
/// exiting sends `SIGHUP` to the foreground process group, and a plain
/// `sleep 30 &` is in that group and dies with it. The background job inherits
/// the ignored signal.
///
/// The descendant surviving is not the same as the terminal staying readable.
/// macOS revokes a controlling terminal when its session leader exits, which
/// closes the descendant's end too, so the reader there reaches the end of
/// the terminal anyway. Linux leaves the slave open. The `pump_waited` unit
/// tests cover the mechanism on every platform with a socket pair that never
/// reports an end.
#[cfg(windows)]
const OUTLIVED_BY_A_DESCENDANT: &str = "start /b ping -n 100 127.0.0.1 >NUL& exit 5";
#[cfg(not(windows))]
const OUTLIVED_BY_A_DESCENDANT: &str = "trap '' HUP; sleep 30 & exit 5";

/// A script leaving a background process holding the terminal open for a few
/// seconds and then exiting with code 5.
///
/// The descendant outlives every wait in the test that uses it by a wide
/// margin, so a reader still inside a `read` on that terminal is unambiguous,
/// and it reaps itself shortly after the test ends.
#[cfg(windows)]
const OUTLIVED_BRIEFLY: &str = "start /b ping -n 6 127.0.0.1 >NUL& exit 5";
#[cfg(not(windows))]
const OUTLIVED_BRIEFLY: &str = "trap '' HUP; sleep 5 & exit 5";

/// A script leaving a background process that prints [`LATE_MARKER`] a second
/// after the child has exited with code 5, so output arrives at the terminal
/// once the pane's exit is already settled.
#[cfg(unix)]
const PRINTS_AFTER_THE_CHILD_EXITS: &str =
    "trap '' HUP; (sleep 1; printf koshi-late-marker) & exit 5";

/// The word the descendant in [`PRINTS_AFTER_THE_CHILD_EXITS`] prints.
#[cfg(unix)]
const LATE_MARKER: &str = "koshi-late-marker";

/// How long a test waits for output a descendant prints after its pane's exit
/// was settled. Outlasts the second that descendant waits before printing.
#[cfg(unix)]
const LATE_OUTPUT_WAIT: Duration = Duration::from_secs(2);

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

/// A sink that records every delivery, tagged with the pane it was for, so a
/// test can assert on the sequence.
struct Recorder {
    /// Everything delivered so far, oldest first.
    seen: Mutex<Vec<(PaneId, Delivered)>>,
}

impl Recorder {
    fn new() -> Arc<Self> {
        Arc::new(Recorder {
            seen: Mutex::new(Vec::new()),
        })
    }

    /// A snapshot of what has been delivered so far, for every pane.
    fn snapshot(&self) -> Vec<Delivered> {
        self.seen
            .lock()
            .expect("recorder")
            .iter()
            .map(|(_, entry)| entry.clone())
            .collect()
    }

    /// The pane each delivery so far was tagged with, oldest first.
    fn panes_named(&self) -> Vec<PaneId> {
        self.seen
            .lock()
            .expect("recorder")
            .iter()
            .map(|(pane, _)| *pane)
            .collect()
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

    /// The recorded exit status, if any child has been reported as ended.
    fn exit(&self) -> Option<ExitStatus> {
        exit_among(&self.snapshot())
    }

    /// The exit status recorded for `pane`, if its child has been reported as
    /// ended.
    fn exit_for(&self, pane: PaneId) -> Option<ExitStatus> {
        self.seen
            .lock()
            .expect("recorder")
            .iter()
            .find_map(|(tagged, entry)| match entry {
                Delivered::Exit(status) if *tagged == pane => Some(*status),
                _ => None,
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
    fn output(&self, pane: PaneId, bytes: Vec<u8>) -> bool {
        self.seen
            .lock()
            .expect("recorder")
            .push((pane, Delivered::Output(bytes)));
        true
    }

    fn exit(&self, pane: PaneId, status: ExitStatus) {
        self.seen
            .lock()
            .expect("recorder")
            .push((pane, Delivered::Exit(status)));
    }
}

/// A sink that closes the pane the moment it is told the child ended, which is
/// what a consumer whose close-on-exit policy runs inline does.
///
/// The exit can arrive on any of a pane's threads, and the call returns
/// whichever one it is.
struct ClosingSink {
    /// The backend to close the pane through, set once it exists.
    backend: Mutex<Option<Arc<PortablePtyBackend>>>,
    /// What closing the pane returned. `None` until the close has returned.
    closed: Mutex<Option<Result<(), PtyError>>>,
}

impl ClosingSink {
    fn new() -> Arc<Self> {
        Arc::new(ClosingSink {
            backend: Mutex::new(None),
            closed: Mutex::new(None),
        })
    }

    /// What closing the pane returned. `None` until the close has returned.
    fn closed(&self) -> Option<Result<(), PtyError>> {
        self.closed.lock().expect("closing sink").clone()
    }
}

impl PtySink for ClosingSink {
    fn output(&self, _pane: PaneId, _bytes: Vec<u8>) -> bool {
        true
    }

    fn exit(&self, pane: PaneId, _status: ExitStatus) {
        let backend = self.backend.lock().expect("closing sink").clone();
        if let Some(backend) = backend {
            let result = backend.kill(pane, KillPolicy::Tree);
            *self.closed.lock().expect("closing sink") = Some(result);
        }
    }
}

/// A sink that refuses everything it is handed, standing in for a consumer
/// that has gone away: a runtime whose inbox is closed.
struct RefusingSink {
    /// Set when a chunk was offered and refused.
    refused_output: Mutex<bool>,
    /// Set if an exit was reported. A gone consumer is told no exit.
    saw_exit: Mutex<bool>,
}

impl RefusingSink {
    fn new() -> Arc<Self> {
        Arc::new(RefusingSink {
            refused_output: Mutex::new(false),
            saw_exit: Mutex::new(false),
        })
    }

    /// Whether a chunk was offered and refused.
    fn refused_output(&self) -> bool {
        *self.refused_output.lock().expect("refusing sink")
    }

    /// Whether an exit was reported.
    fn saw_exit(&self) -> bool {
        *self.saw_exit.lock().expect("refusing sink")
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

/// Spawn `body` as `pane_id` through [`PTY_GATE`], panicking on failure.
fn spawn_script(backend: &PortablePtyBackend, pane_id: PaneId, body: &str) -> PtyHandle {
    let _gate = PTY_GATE.lock().expect("pty gate");
    backend
        .spawn(pane_id, script(body), SIZE)
        .expect("spawn child")
}

/// Poll until `done` returns true or `TIMEOUT` elapses.
fn wait_until(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_channel_backed_pane_reports_the_same_childs_exit() {
    // The control for every sink test here: the same child, ending the same
    // way, delivered through the handle's channels instead. The watcher
    // observes the child's end and feeds both routes. A failure here is the
    // child's end not being observed at all; this passing alongside a failing
    // sink test puts the fault in the sink route.
    let backend = PortablePtyBackend::new();
    let handle = spawn_script(&backend, PaneId::new(), PRINTS_THEN_EXITS_3);

    let mut status = None;
    wait_until(|| {
        status = handle.try_exit_status();
        status.is_some()
    });
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
    spawn_script(&backend, pane_id, PRINTS_THEN_EXITS_3);

    assert_eq!(
        recorder.wait_for_exit(),
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

    // ...and the exit came last, after every chunk.
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

    // Every delivery named the pane it was for.
    let named = recorder.panes_named();
    assert_eq!(
        named,
        vec![pane_id; named.len()],
        "a delivery named a pane other than the one spawned"
    );
}

#[test]
fn two_panes_on_one_sink_each_report_under_their_own_id() {
    let recorder = Recorder::new();
    let backend = PortablePtyBackend::with_sink(recorder.clone());
    let first = PaneId::new();
    let second = PaneId::new();
    spawn_script(&backend, first, PRINTS_THEN_EXITS_3);
    spawn_script(&backend, second, EXITS_0);

    wait_until(|| recorder.exit_for(first).is_some() && recorder.exit_for(second).is_some());
    assert_eq!(
        recorder.exit_for(first),
        Some(ExitStatus::ExitCode(3)),
        "the first pane's exit was not reported under its own id: {:?}",
        recorder.snapshot()
    );
    assert_eq!(
        recorder.exit_for(second),
        Some(ExitStatus::ExitCode(0)),
        "the second pane's exit was not reported under its own id: {:?}",
        recorder.snapshot()
    );
}

#[test]
fn a_reader_stops_when_the_consumer_goes_away_even_while_the_child_lives_on() {
    // The reader waits for the child's exit before reporting it, which orders
    // output ahead of exit. That wait is skipped once the consumer is gone, so
    // a child that outlives its consumer does not pin the reader thread.
    let sink = RefusingSink::new();
    let backend = PortablePtyBackend::with_sink(sink.clone());
    let pane_id = PaneId::new();
    spawn_script(&backend, pane_id, BLOCKS_AFTER_PRINTING);

    // The reader holds a reference to the sink until it gives up on the pane,
    // so the count falling to this test and the backend is the reader having
    // let the consumer go. A reader that waited for the exit would hold its
    // reference for as long as the child blocks, which is far past this
    // deadline.
    //
    // On Windows the thread itself keeps running after that: it stays in
    // `read` on the console, discarding, and closing that console waits for
    // its output to be read out. What stops is everything the consumer can
    // see, and that is what this counts.
    let deadline = Instant::now() + READER_STOP_DEADLINE;
    while Arc::strong_count(&sink) > HOLDERS_WITHOUT_READER && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        Arc::strong_count(&sink),
        HOLDERS_WITHOUT_READER,
        "the reader was still holding the consumer while the child blocked"
    );
    assert!(
        sink.refused_output(),
        "the sink was never offered any output"
    );
    assert!(
        !sink.saw_exit(),
        "an exit was reported to a consumer that had already gone"
    );

    // Reap the blocking child. The group kill takes any process the script
    // started with it: on Windows the script's `ping` is a child of `cmd.exe`.
    backend.kill(pane_id, KillPolicy::Tree).expect("kill pane");
}

#[test]
fn closing_a_pane_releases_its_reader_while_a_descendant_still_holds_the_terminal() {
    // The arrangement a shell leaves behind whenever a background job outlives
    // it: `sleep 5 & exit 5`. The child is gone, so the pane is told its child
    // ended and the consumer closes it, but the descendant still holds the
    // terminal, so no end-of-file arrives for as long as it runs.
    //
    // Closing releases the reader anyway. A reader left inside that `read`
    // keeps a thread and a PTY descriptor for the descendant's whole life.
    //
    // `Force` is the policy the runtime closes an exited pane with: the leader
    // is already reaped, so it signals nothing and the descendant runs on.
    let recorder = Recorder::new();
    let backend = PortablePtyBackend::with_sink(recorder.clone());
    let pane_id = PaneId::new();
    spawn_script(&backend, pane_id, OUTLIVED_BRIEFLY);

    let started = Instant::now();
    assert_eq!(
        recorder.wait_for_exit(),
        Some(ExitStatus::ExitCode(5)),
        "the pane was never told its child ended"
    );
    // Where the reader can wait on its terminal it publishes here, one quiet
    // round after being told the child has gone. An exit that took the full
    // limit is the watcher stepping in, with the reader never brought back
    // from that wait.
    //
    // Windows has no descriptor to wait on, so its reader stays in `read` and
    // the watcher publishes at the limit.
    #[cfg(unix)]
    {
        let took = started.elapsed();
        assert!(
            took < EXIT_PUBLISH_LIMIT,
            "the exit took {took:?}, which is the watcher stepping in at its \
             {EXIT_PUBLISH_LIMIT:?} limit rather than the reader publishing it"
        );
    }
    #[cfg(not(unix))]
    let _ = started;

    backend
        .kill(pane_id, KillPolicy::Force)
        .expect("close the pane");

    // The reader holds a reference to the sink for as long as it runs, so the
    // count falling to the test and the backend is the reader having stopped.
    // `kill` joins the watcher, so its hold is already gone. The descendant
    // holds the terminal open for seconds past this deadline, so a reader
    // released only by end-of-file would still be running here.
    let deadline = Instant::now() + EXIT_PUBLISH_LIMIT;
    while Arc::strong_count(&recorder) > HOLDERS_AFTER_CLOSE && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        Arc::strong_count(&recorder),
        HOLDERS_AFTER_CLOSE,
        "the reader thread was still holding the terminal a descendant had \
         open, so closing the pane released neither it nor its descriptor"
    );
}

#[test]
fn a_consumer_may_close_the_pane_from_inside_the_exit_it_is_handed() {
    // A consumer whose close-on-exit policy runs inline closes the pane from
    // inside the `exit` call it was just handed. That call returns: it runs on
    // one of the pane's own threads, and `kill` tears those threads down.
    //
    // The script leaves a descendant holding the terminal, so no end-of-file
    // arrives on its own: on Unix the reader is woken and publishes; on
    // Windows the watcher closes the console and the reader publishes behind
    // it.
    let sink = ClosingSink::new();
    let backend = Arc::new(PortablePtyBackend::with_sink(sink.clone()));
    *sink.backend.lock().expect("closing sink") = Some(Arc::clone(&backend));
    let pane_id = PaneId::new();
    spawn_script(&backend, pane_id, OUTLIVED_BRIEFLY);

    // Waited for on this thread with a deadline: a close that never returns
    // fails this test instead of hanging it.
    wait_until(|| sink.closed().is_some());
    assert_eq!(
        sink.closed(),
        Some(Ok(())),
        "closing the pane from inside the exit never returned"
    );
}

#[test]
fn a_pane_reports_its_child_ending_even_when_the_pty_never_reports_an_end() {
    // The child exits while a descendant it started keeps the terminal open,
    // so no end-of-file ever arrives and the reader stays blocked. The pane is
    // still told its child ended, without waiting for the descendant.
    //
    // Windows reaches this path for every pane, not just this arrangement:
    // ConPTY keeps a pane's console readable after its child is gone.
    let recorder = Recorder::new();
    let backend = PortablePtyBackend::with_sink(recorder.clone());
    let pane_id = PaneId::new();
    spawn_script(&backend, pane_id, OUTLIVED_BY_A_DESCENDANT);

    assert_eq!(
        recorder.wait_for_exit(),
        Some(ExitStatus::ExitCode(5)),
        "the pane was never told its child ended"
    );

    // Reap the descendant still holding the terminal open.
    let _ = backend.kill(pane_id, KillPolicy::Tree);
}

#[test]
fn closing_a_pane_hands_its_consumer_no_exit_and_does_not_stand_by() {
    // Closing a pane is the consumer saying it is done with it, so no exit for
    // that pane reaches the sink from either helper thread, whichever reaches
    // the child's end first. Closing also does not sit through the watcher's
    // standby.
    let recorder = Recorder::new();
    let backend = PortablePtyBackend::with_sink(recorder.clone());
    let pane_id = PaneId::new();
    spawn_script(&backend, pane_id, BLOCKS_AFTER_PRINTING);

    let started = Instant::now();
    backend.kill(pane_id, KillPolicy::Tree).expect("kill pane");
    let took = started.elapsed();

    // The bound is half the whole standby, not one round of it. A close that
    // sat through the standby takes the full limit, which is twice this; a
    // close that is woken takes the OS work alone, which measures in tenths
    // of a millisecond. The gap is room for process teardown on a loaded
    // host.
    let stood_by = EXIT_PUBLISH_LIMIT / 2;
    assert!(
        took < stood_by,
        "closing the pane stood by for the exit: took {took:?}, which is over \
         the {stood_by:?} that separates a woken close from one that waited \
         out the {EXIT_PUBLISH_LIMIT:?} standby"
    );

    // The reader reaches the end of the PTY after the close and tries to
    // report the exit it was waiting for. Let it finish doing so: once it
    // drops its reference, only this test and the backend hold the sink.
    wait_until(|| Arc::strong_count(&recorder) <= HOLDERS_AFTER_CLOSE);
    assert_eq!(
        recorder.exit(),
        None,
        "a closed pane's consumer was handed an exit for it"
    );
}

// Unix only: the script leaves a background process holding the terminal and
// printing into it after the child is gone, which `cmd.exe` has no plain
// equivalent for. The gate it exercises is platform-independent; the
// `portable::tests` unit tests make the same claim without a child.
#[cfg(unix)]
#[test]
fn a_settled_pane_forwards_no_more_of_a_descendants_output() {
    // The child exits while a descendant keeps the terminal open, so the
    // watcher publishes the exit and the consumer lets the pane go. What the
    // descendant prints afterwards belongs to a pane that no longer exists,
    // and the reader stops instead of forwarding it.
    let recorder = Recorder::new();
    let backend = PortablePtyBackend::with_sink(recorder.clone());
    let pane_id = PaneId::new();
    spawn_script(&backend, pane_id, PRINTS_AFTER_THE_CHILD_EXITS);

    assert_eq!(
        recorder.wait_for_exit(),
        Some(ExitStatus::ExitCode(5)),
        "the pane was never told its child ended"
    );

    // Outlast the descendant's own wait, so its output has reached the
    // terminal and the reader has had every chance to forward it.
    thread::sleep(LATE_OUTPUT_WAIT);

    let seen = recorder.text();
    assert!(
        !seen.contains(LATE_MARKER),
        "output printed after the pane's exit was settled still reached the \
         consumer: {seen:?}"
    );

    // Reap the descendant still holding the terminal open.
    let _ = backend.kill(pane_id, KillPolicy::Tree);
}

#[test]
fn a_sink_backed_pane_hands_back_a_handle_with_no_channels() {
    // The handle carries no receivers, which is how the runtime knows this
    // pane needs no forwarder thread: it is already delivering to the sink.
    let recorder = Recorder::new();
    let backend = PortablePtyBackend::with_sink(recorder.clone());
    let pane_id = PaneId::new();
    let mut handle = spawn_script(&backend, pane_id, EXITS_0);

    assert_eq!(handle.pane_id(), pane_id);
    assert!(handle.take_receivers().is_none());
    assert_eq!(handle.try_read_output(), None);
    assert_eq!(handle.try_exit_status(), None);

    // The sink is still the one being fed.
    assert_eq!(recorder.wait_for_exit(), Some(ExitStatus::ExitCode(0)));
}

#[test]
fn a_gone_consumer_is_never_told_the_child_ended() {
    // A consumer that refuses a chunk is finished with the pane. The watcher
    // stands by to report the exit itself when the PTY reports no end, and
    // the reader takes charge of that exit on its way out, so a child that
    // ends promptly has no exit handed to a consumer that already said it was
    // done. The refusing-sink test above cannot catch this: its child runs
    // long enough that the watcher never reaches its standby window.
    let sink = RefusingSink::new();
    let backend = PortablePtyBackend::with_sink(sink.clone());
    let pane_id = PaneId::new();
    spawn_script(&backend, pane_id, PRINTS_THEN_EXITS_3);

    // Well past the longest the watcher stands by, so an exit that was going
    // to be delivered has been by now.
    thread::sleep(EXIT_PUBLISH_LIMIT * 2);

    assert!(
        sink.refused_output(),
        "the sink was never offered any output, so it never said it was done"
    );
    assert!(
        !sink.saw_exit(),
        "an exit was reported to a consumer that had already gone"
    );

    let _ = backend.kill(pane_id, KillPolicy::Tree);
}

/// A sink that holds the reader inside `output` until the test lets go, so a
/// chunk stays in the consumer's hands for as long as the test wants.
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

    /// A snapshot of everything delivered so far, oldest first.
    fn snapshot(&self) -> Vec<Delivered> {
        self.seen.lock().expect("stalled sink").clone()
    }

    /// The recorded exit status, if one has been delivered.
    fn exit_status(&self) -> Option<ExitStatus> {
        exit_among(&self.snapshot())
    }

    /// How many exits have been delivered. A consumer is told a child ended
    /// exactly once.
    fn exits_delivered(&self) -> usize {
        self.snapshot()
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
        // Park the reader here. `seen` is released first, so the test can read
        // what has been delivered while this thread waits.
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
// child would never run.
#[cfg(unix)]
#[test]
fn an_exit_waits_out_a_consumer_stalled_in_output() {
    // The consumer is holding a chunk: it went into `output` and has not come
    // back. An exit means "you have seen everything the child printed", so
    // nothing hands it one while that chunk is still in its hands, however
    // long it holds on. Once it lets go, the exit follows: once, and behind
    // the output.
    let sink = StalledSink::new();
    let held = sink.gate.lock().expect("hold the reader");
    let backend = PortablePtyBackend::with_sink(sink.clone());
    let pane_id = PaneId::new();
    spawn_script(&backend, pane_id, PRINTS_THEN_EXITS_3);

    // One recorded output means the reader is inside `output`, on the gate this
    // test holds.
    wait_until(|| !sink.snapshot().is_empty());
    assert!(
        !sink.snapshot().is_empty(),
        "the sink was never offered any output, so no chunk is in its hands"
    );

    // Watch for twice the longest wait the backend has. Load only lengthens
    // this window, so nothing but a published exit can fail the check.
    thread::sleep(EXIT_PUBLISH_LIMIT * 2);
    assert_eq!(
        sink.exit_status(),
        None,
        "an exit was handed to a consumer still holding a chunk: {:?}",
        sink.snapshot()
    );

    // Let the chunk land. The reader reaches the end of the child's output and
    // reports the exit behind it.
    drop(held);
    wait_until(|| sink.exit_status().is_some());
    assert_eq!(
        sink.exit_status(),
        Some(ExitStatus::ExitCode(3)),
        "the stalled consumer was never told its child ended"
    );

    // Let the reader finish reacting: once it drops its reference, only this
    // test and the backend hold the sink.
    wait_until(|| Arc::strong_count(&sink) <= HOLDERS_WITHOUT_READER);
    assert_eq!(
        sink.exits_delivered(),
        1,
        "the child's end was reported more than once"
    );

    let seen = sink.snapshot();
    let exit_at = seen
        .iter()
        .position(|entry| matches!(entry, Delivered::Exit(_)))
        .expect("exit recorded");
    assert!(exit_at > 0, "the exit came before any output: {seen:?}");
    assert_eq!(
        exit_at,
        seen.len() - 1,
        "output reached the consumer after its exit: {seen:?}"
    );

    let _ = backend.kill(pane_id, KillPolicy::Tree);
}

#[test]
fn closing_a_pane_hands_no_exit_even_when_its_reader_gets_there_first() {
    // `kill` settles the pane's exit before it signals anything, so neither
    // helper thread hands the consumer an exit for a pane it is closing.
    //
    // The reader is the one that gets there first here. A graceful close polls
    // for the child to go before it wakes the watcher, and the reader reaches
    // the end of the PTY during that poll, so it is holding the exit status
    // and about to report it while `kill` is still running.
    let recorder = Recorder::new();
    let backend = PortablePtyBackend::with_sink(recorder.clone());
    let pane_id = PaneId::new();
    spawn_script(&backend, pane_id, BLOCKS_AFTER_PRINTING);

    backend
        .kill(
            pane_id,
            KillPolicy::Graceful {
                timeout: GRACEFUL_STOP,
            },
        )
        .expect("kill pane");

    // Let the reader finish reacting to the close before asking: once it drops
    // its reference, only this test and the backend hold the sink.
    wait_until(|| Arc::strong_count(&recorder) <= HOLDERS_AFTER_CLOSE);
    assert_eq!(
        recorder.exit(),
        None,
        "a closed pane's consumer was handed an exit by its reader"
    );
}
