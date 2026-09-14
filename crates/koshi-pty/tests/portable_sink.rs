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

/// Standard test terminal size: 80 columns × 24 rows.
const STANDARD_PTY_SIZE: PtySize = PtySize {
    column_count: 80,
    row_count: 24,
};

/// How long a test waits for a short-lived child to finish and report. Long
/// enough for a cold ConPTY start, which is slower than opening a Unix pair.
const TEST_TIMEOUT_DURATION: Duration = Duration::from_secs(10);

/// How long the reader gets to stop once its consumer refuses a chunk.
///
/// [`BLOCKS_AFTER_PRINTING`] keeps its child alive far longer than this on
/// every platform, so the wait completes only when the reader gave up early
/// instead of sitting on the child's exit.
const READER_STOP_DEADLINE_DURATION: Duration = Duration::from_secs(5);

/// How many holders of a pane's sink remain once its reader has let the sink
/// go: the test itself and the backend. The reader publishes the exit on every
/// platform these tests run on, so the watcher keeps no hold of its own.
///
/// Letting the sink go is not always the thread ending: on Windows the reader
/// stays in `read` on a pane the consumer let go, and closing that pane's
/// console waits for its output to be read out.
const SINK_HOLDER_COUNT_WITHOUT_READER: usize = 2;

/// How many holders of a closed pane's sink remain once its reader thread has
/// stopped: the test itself and the backend. `kill` joins the watcher, so its
/// hold is already gone by the time `kill` returns.
const SINK_HOLDER_COUNT_AFTER_CLOSE: usize = 2;

/// The longest the watcher stands by, across every round. Mirrors
/// `EXIT_PUBLISH_LIMIT_DURATION` in the backend, which is private.
const EXIT_PUBLISH_LIMIT_DURATION: Duration = Duration::from_secs(1);

/// The grace `kill` gives a child to exit on the stop request. The child in
/// the test that uses it exits on the request inside this window, so `kill`
/// polls for that exit instead of forcing it.
const GRACEFUL_STOP_DURATION: Duration = Duration::from_secs(5);

/// Serializes PTY creation across the parallel test threads. macOS
/// `openpty(3)` fails with a transient `-6` under concurrent allocation.
static PTY_GATE: Mutex<()> = Mutex::new(());

/// The shell a test script is handed to.
#[cfg(windows)]
const TEST_SHELL_PROGRAM: &str = "cmd.exe";
#[cfg(not(windows))]
const TEST_SHELL_PROGRAM: &str = "/bin/sh";

/// The flag telling [`TEST_SHELL_PROGRAM`] to run the following argument as a script.
#[cfg(windows)]
const TEST_SHELL_SCRIPT_FLAG: &str = "/C";
#[cfg(not(windows))]
const TEST_SHELL_SCRIPT_FLAG: &str = "-c";

/// The word a test looks for in a child's output. Short enough that no
/// terminal wraps it, so it never arrives split by an escape sequence.
const SINK_OUTPUT_MARKER: &str = "koshi-sink-marker";

/// A script printing [`SINK_OUTPUT_MARKER`] and then exiting with code 3.
#[cfg(windows)]
const SCRIPT_PRINTS_THEN_EXITS_WITH_CODE_3: &str = "echo koshi-sink-marker& exit 3";
#[cfg(not(windows))]
const SCRIPT_PRINTS_THEN_EXITS_WITH_CODE_3: &str = "printf koshi-sink-marker; exit 3";

/// A script printing one character and then blocking for far longer than any
/// test here waits, so a child still running is unambiguous.
#[cfg(windows)]
const SCRIPT_BLOCKS_AFTER_PRINTING: &str = "echo x& ping -n 100 127.0.0.1 >NUL";
#[cfg(not(windows))]
const SCRIPT_BLOCKS_AFTER_PRINTING: &str = "printf x; sleep 30";

/// A script exiting successfully without printing anything.
const SCRIPT_EXITS_WITH_CODE_0: &str = "exit 0";

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
const SCRIPT_LEAVES_DESCENDANT_RUNNING: &str = "start /b ping -n 100 127.0.0.1 >NUL& exit 5";
#[cfg(not(windows))]
const SCRIPT_LEAVES_DESCENDANT_RUNNING: &str = "trap '' HUP; sleep 30 & exit 5";

/// A script leaving a background process holding the terminal open for a few
/// seconds and then exiting with code 5.
///
/// The descendant outlives every wait in the test that uses it by a wide
/// margin, so a reader still inside a `read` on that terminal is unambiguous,
/// and it reaps itself shortly after the test ends.
#[cfg(windows)]
const SCRIPT_LEAVES_DESCENDANT_RUNNING_BRIEFLY: &str = "start /b ping -n 6 127.0.0.1 >NUL& exit 5";
#[cfg(not(windows))]
const SCRIPT_LEAVES_DESCENDANT_RUNNING_BRIEFLY: &str = "trap '' HUP; sleep 5 & exit 5";

/// A script leaving a background process that prints [`LATE_OUTPUT_MARKER`] a second
/// after the child has exited with code 5, so output arrives at the terminal
/// once the pane's exit is already settled.
#[cfg(unix)]
const SCRIPT_DESCENDANT_PRINTS_AFTER_CHILD_PROCESS_EXITS: &str =
    "trap '' HUP; (sleep 1; printf koshi-late-marker) & exit 5";

/// The word the descendant in [`SCRIPT_DESCENDANT_PRINTS_AFTER_CHILD_PROCESS_EXITS`] prints.
#[cfg(unix)]
const LATE_OUTPUT_MARKER: &str = "koshi-late-marker";

/// How long a test waits for output a descendant prints after its pane's exit
/// was settled. Outlasts the second that descendant waits before printing.
#[cfg(unix)]
const LATE_OUTPUT_WAIT_DURATION: Duration = Duration::from_secs(2);

/// One delivery the backend sent to the sink, in send order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SinkDelivery {
    /// A chunk of child output.
    OutputBytes(Vec<u8>),
    /// The child's final status.
    ExitStatus(ExitStatus),
}

/// Find the exit status among `sink_deliveries`, if a child has ended.
fn find_exit_status(sink_deliveries: &[SinkDelivery]) -> Option<ExitStatus> {
    sink_deliveries.iter().find_map(|delivery| match delivery {
        SinkDelivery::ExitStatus(exit_status) => Some(*exit_status),
        SinkDelivery::OutputBytes(_) => None,
    })
}

/// A sink that records every delivery, tagged with the pane it was for, so a
/// test can assert on the sequence.
struct SinkRecorder {
    /// Everything delivered so far, oldest first.
    sink_deliveries: Mutex<Vec<(PaneId, SinkDelivery)>>,
}

impl SinkRecorder {
    fn new() -> Arc<Self> {
        Arc::new(SinkRecorder {
            sink_deliveries: Mutex::new(Vec::new()),
        })
    }

    /// A snapshot of what has been delivered so far, for every pane.
    fn list_sink_deliveries(&self) -> Vec<SinkDelivery> {
        self.sink_deliveries
            .lock()
            .expect("recorder")
            .iter()
            .map(|(_, delivery)| delivery.clone())
            .collect()
    }

    /// The pane each delivery so far was tagged with, oldest first.
    fn list_delivered_pane_ids(&self) -> Vec<PaneId> {
        self.sink_deliveries
            .lock()
            .expect("recorder")
            .iter()
            .map(|(pane_id, _)| *pane_id)
            .collect()
    }

    /// Every output chunk so far, concatenated and read as lossy UTF-8.
    fn get_delivered_output_text(&self) -> String {
        let joined_output_bytes: Vec<u8> = self
            .list_sink_deliveries()
            .into_iter()
            .filter_map(|delivery| match delivery {
                SinkDelivery::OutputBytes(output_bytes) => Some(output_bytes),
                SinkDelivery::ExitStatus(_) => None,
            })
            .flatten()
            .collect();
        String::from_utf8_lossy(&joined_output_bytes).into_owned()
    }

    /// The recorded exit status, if any child has been reported as ended.
    fn get_exit_status(&self) -> Option<ExitStatus> {
        find_exit_status(&self.list_sink_deliveries())
    }

    /// The exit status recorded for `pane_id`, if its child has been reported as
    /// ended.
    fn get_exit_status_for_pane(&self, pane_id: PaneId) -> Option<ExitStatus> {
        self.sink_deliveries
            .lock()
            .expect("recorder")
            .iter()
            .find_map(|(tagged_pane_id, delivery)| match delivery {
                SinkDelivery::ExitStatus(exit_status) if *tagged_pane_id == pane_id => {
                    Some(*exit_status)
                }
                _ => None,
            })
    }

    /// Block until an exit has been recorded, or `TEST_TIMEOUT_DURATION` elapses.
    fn wait_for_exit(&self) -> Option<ExitStatus> {
        let deadline = Instant::now() + TEST_TIMEOUT_DURATION;
        loop {
            if let Some(exit_status) = self.get_exit_status() {
                return Some(exit_status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl PtySink for SinkRecorder {
    fn accept_output_bytes(&self, pane_id: PaneId, output_bytes: Vec<u8>) -> bool {
        self.sink_deliveries
            .lock()
            .expect("recorder")
            .push((pane_id, SinkDelivery::OutputBytes(output_bytes)));
        true
    }

    fn accept_exit_status(&self, pane_id: PaneId, exit_status: ExitStatus) {
        self.sink_deliveries
            .lock()
            .expect("recorder")
            .push((pane_id, SinkDelivery::ExitStatus(exit_status)));
    }
}

/// A sink that closes the pane the moment it is told the child ended, which is
/// what a consumer whose close-on-exit policy runs inline does.
///
/// The exit can arrive on any of a pane's threads, and the call returns
/// whichever one it is.
struct ClosingSink {
    /// The backend to close the pane through, set once it exists.
    pty_backend: Mutex<Option<Arc<PortablePtyBackend>>>,
    /// What closing the pane returned. `None` until the close has returned.
    close_result: Mutex<Option<Result<(), PtyError>>>,
}

impl ClosingSink {
    fn new() -> Arc<Self> {
        Arc::new(ClosingSink {
            pty_backend: Mutex::new(None),
            close_result: Mutex::new(None),
        })
    }

    /// What closing the pane returned. `None` until the close has returned.
    fn get_close_result(&self) -> Option<Result<(), PtyError>> {
        self.close_result.lock().expect("closing sink").clone()
    }
}

impl PtySink for ClosingSink {
    fn accept_output_bytes(&self, _pane_id: PaneId, _output_bytes: Vec<u8>) -> bool {
        true
    }

    fn accept_exit_status(&self, pane_id: PaneId, _exit_status: ExitStatus) {
        let pty_backend = self.pty_backend.lock().expect("closing sink").clone();
        if let Some(pty_backend) = pty_backend {
            let close_result = pty_backend.kill_pane(pane_id, KillPolicy::Tree);
            *self.close_result.lock().expect("closing sink") = Some(close_result);
        }
    }
}

/// A sink that refuses everything it is handed, standing in for a consumer
/// that has gone away: a runtime whose inbox is closed.
struct RefusingSink {
    /// Set when a chunk was offered and refused.
    has_refused_output: Mutex<bool>,
    /// Set if an exit was reported. A gone consumer is told no exit.
    has_seen_exit_status: Mutex<bool>,
}

impl RefusingSink {
    fn new() -> Arc<Self> {
        Arc::new(RefusingSink {
            has_refused_output: Mutex::new(false),
            has_seen_exit_status: Mutex::new(false),
        })
    }

    /// Whether a chunk was offered and refused.
    fn has_refused_output(&self) -> bool {
        *self.has_refused_output.lock().expect("refusing sink")
    }

    /// Whether an exit was reported.
    fn has_seen_exit_status(&self) -> bool {
        *self.has_seen_exit_status.lock().expect("refusing sink")
    }
}

impl PtySink for RefusingSink {
    fn accept_output_bytes(&self, _pane_id: PaneId, _output_bytes: Vec<u8>) -> bool {
        *self.has_refused_output.lock().expect("refusing sink") = true;
        false
    }

    fn accept_exit_status(&self, _pane_id: PaneId, _exit_status: ExitStatus) {
        *self.has_seen_exit_status.lock().expect("refusing sink") = true;
    }
}

/// Build a spawn spec running `shell_script` through the platform's shell,
/// inheriting the working directory and environment variables.
fn build_shell_spawn_spec(shell_script: &str) -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from(TEST_SHELL_PROGRAM),
        arguments: vec![TEST_SHELL_SCRIPT_FLAG.to_string(), shell_script.to_string()],
        working_directory: None,
        environment_variables: BTreeMap::new(),
        shell_kind: ShellKind::from_program(Path::new(TEST_SHELL_PROGRAM)),
    }
}

/// Spawn `shell_script` as `pane_id` through [`PTY_GATE`], panicking on failure.
fn spawn_script(
    pty_backend: &PortablePtyBackend,
    pane_id: PaneId,
    shell_script: &str,
) -> PtyHandle {
    let _pty_creation_guard = PTY_GATE.lock().expect("pty gate");
    pty_backend
        .spawn_pane(
            pane_id,
            build_shell_spawn_spec(shell_script),
            STANDARD_PTY_SIZE,
        )
        .expect("spawn child")
}

/// Poll until `condition` returns true or `TEST_TIMEOUT_DURATION` elapses.
fn wait_until_condition(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + TEST_TIMEOUT_DURATION;
    while !condition() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_channel_backed_pane_reports_the_same_child_process_exit() {
    // The control for every sink test here: the same child, ending the same
    // way, delivered through the handle's channels instead. The watcher
    // observes the child's end and feeds both routes. A failure here is the
    // child's end not being observed at all; this passing alongside a failing
    // sink test puts the fault in the sink route.
    let pty_backend = PortablePtyBackend::new();
    let pty_handle = spawn_script(
        &pty_backend,
        PaneId::new(),
        SCRIPT_PRINTS_THEN_EXITS_WITH_CODE_3,
    );

    let mut exit_status = None;
    wait_until_condition(|| {
        exit_status = pty_handle.try_receive_exit_status();
        exit_status.is_some()
    });
    assert_eq!(
        exit_status,
        Some(ExitStatus::ExitCode(3)),
        "the child's end was never observed on the channel route either"
    );
}

#[test]
fn a_sink_receives_the_childs_output_and_then_its_exit() {
    let sink_recorder = SinkRecorder::new();
    let pty_backend = PortablePtyBackend::with_pty_sink(sink_recorder.clone());
    let pane_id = PaneId::new();
    spawn_script(&pty_backend, pane_id, SCRIPT_PRINTS_THEN_EXITS_WITH_CODE_3);

    assert_eq!(
        sink_recorder.wait_for_exit(),
        Some(ExitStatus::ExitCode(3)),
        "no exit reached the sink; it was handed: {:?}",
        sink_recorder.list_sink_deliveries()
    );

    // The child's whole output reached the sink...
    assert!(
        sink_recorder
            .get_delivered_output_text()
            .contains(SINK_OUTPUT_MARKER),
        "sink never saw the child's output; got {:?}",
        sink_recorder.get_delivered_output_text()
    );

    // ...and the exit came last, after every chunk.
    let sink_deliveries = sink_recorder.list_sink_deliveries();
    let exit_delivery_index = sink_deliveries
        .iter()
        .position(|delivery| matches!(delivery, SinkDelivery::ExitStatus(_)))
        .expect("exit recorded");
    assert_eq!(
        exit_delivery_index,
        sink_deliveries.len() - 1,
        "exit was not the last delivery: {sink_deliveries:?}"
    );

    // Every delivery named the pane it was for.
    let delivered_pane_ids = sink_recorder.list_delivered_pane_ids();
    assert_eq!(
        delivered_pane_ids,
        vec![pane_id; delivered_pane_ids.len()],
        "a delivery named a pane other than the one spawned"
    );
}

#[test]
fn two_panes_on_one_sink_each_report_under_their_own_id() {
    let sink_recorder = SinkRecorder::new();
    let pty_backend = PortablePtyBackend::with_pty_sink(sink_recorder.clone());
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    spawn_script(
        &pty_backend,
        first_pane_id,
        SCRIPT_PRINTS_THEN_EXITS_WITH_CODE_3,
    );
    spawn_script(&pty_backend, second_pane_id, SCRIPT_EXITS_WITH_CODE_0);

    wait_until_condition(|| {
        sink_recorder
            .get_exit_status_for_pane(first_pane_id)
            .is_some()
            && sink_recorder
                .get_exit_status_for_pane(second_pane_id)
                .is_some()
    });
    assert_eq!(
        sink_recorder.get_exit_status_for_pane(first_pane_id),
        Some(ExitStatus::ExitCode(3)),
        "the first pane's exit was not reported under its own id: {:?}",
        sink_recorder.list_sink_deliveries()
    );
    assert_eq!(
        sink_recorder.get_exit_status_for_pane(second_pane_id),
        Some(ExitStatus::ExitCode(0)),
        "the second pane's exit was not reported under its own id: {:?}",
        sink_recorder.list_sink_deliveries()
    );
}

#[test]
fn a_reader_stops_when_the_consumer_goes_away_even_while_the_child_lives_on() {
    // The reader waits for the child's exit before reporting it, which orders
    // output ahead of exit. That wait is skipped once the consumer is gone, so
    // a child that outlives its consumer does not pin the reader thread.
    let refusing_sink = RefusingSink::new();
    let pty_backend = PortablePtyBackend::with_pty_sink(refusing_sink.clone());
    let pane_id = PaneId::new();
    spawn_script(&pty_backend, pane_id, SCRIPT_BLOCKS_AFTER_PRINTING);

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
    let deadline = Instant::now() + READER_STOP_DEADLINE_DURATION;
    while Arc::strong_count(&refusing_sink) > SINK_HOLDER_COUNT_WITHOUT_READER
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        Arc::strong_count(&refusing_sink),
        SINK_HOLDER_COUNT_WITHOUT_READER,
        "the reader was still holding the consumer while the child blocked"
    );
    assert!(
        refusing_sink.has_refused_output(),
        "the sink was never offered any output"
    );
    assert!(
        !refusing_sink.has_seen_exit_status(),
        "an exit was reported to a consumer that had already gone"
    );

    // Reap the blocking child. The group kill takes any process the script
    // started with it: on Windows the script's `ping` is a child of `cmd.exe`.
    pty_backend
        .kill_pane(pane_id, KillPolicy::Tree)
        .expect("kill pane");
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
    let sink_recorder = SinkRecorder::new();
    let pty_backend = PortablePtyBackend::with_pty_sink(sink_recorder.clone());
    let pane_id = PaneId::new();
    spawn_script(
        &pty_backend,
        pane_id,
        SCRIPT_LEAVES_DESCENDANT_RUNNING_BRIEFLY,
    );

    let exit_wait_started_at = Instant::now();
    assert_eq!(
        sink_recorder.wait_for_exit(),
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
        let exit_wait_duration = exit_wait_started_at.elapsed();
        assert!(
            exit_wait_duration < EXIT_PUBLISH_LIMIT_DURATION,
            "the exit took {exit_wait_duration:?}, which is the watcher stepping in at its \
             {EXIT_PUBLISH_LIMIT_DURATION:?} limit rather than the reader publishing it"
        );
    }
    #[cfg(not(unix))]
    let _ = exit_wait_started_at;

    pty_backend
        .kill_pane(pane_id, KillPolicy::Force)
        .expect("close the pane");

    // The reader holds a reference to the sink for as long as it runs, so the
    // count falling to the test and the backend is the reader having stopped.
    // `kill` joins the watcher, so its hold is already gone. The descendant
    // holds the terminal open for seconds past this deadline, so a reader
    // released only by end-of-file would still be running here.
    let deadline = Instant::now() + EXIT_PUBLISH_LIMIT_DURATION;
    while Arc::strong_count(&sink_recorder) > SINK_HOLDER_COUNT_AFTER_CLOSE
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        Arc::strong_count(&sink_recorder),
        SINK_HOLDER_COUNT_AFTER_CLOSE,
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
    let closing_sink = ClosingSink::new();
    let pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(closing_sink.clone()));
    *closing_sink.pty_backend.lock().expect("closing sink") = Some(Arc::clone(&pty_backend));
    let pane_id = PaneId::new();
    spawn_script(
        &pty_backend,
        pane_id,
        SCRIPT_LEAVES_DESCENDANT_RUNNING_BRIEFLY,
    );

    // Waited for on this thread with a deadline: a close that never returns
    // fails this test instead of hanging it.
    wait_until_condition(|| closing_sink.get_close_result().is_some());
    assert_eq!(
        closing_sink.get_close_result(),
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
    let sink_recorder = SinkRecorder::new();
    let pty_backend = PortablePtyBackend::with_pty_sink(sink_recorder.clone());
    let pane_id = PaneId::new();
    spawn_script(&pty_backend, pane_id, SCRIPT_LEAVES_DESCENDANT_RUNNING);

    assert_eq!(
        sink_recorder.wait_for_exit(),
        Some(ExitStatus::ExitCode(5)),
        "the pane was never told its child ended"
    );

    // Reap the descendant still holding the terminal open.
    let _ = pty_backend.kill_pane(pane_id, KillPolicy::Tree);
}

#[test]
fn closing_a_pane_hands_its_consumer_no_exit_and_does_not_stand_by() {
    // Closing a pane is the consumer saying it is done with it, so no exit for
    // that pane reaches the sink from either helper thread, whichever reaches
    // the child's end first. Closing also does not sit through the watcher's
    // standby.
    let sink_recorder = SinkRecorder::new();
    let pty_backend = PortablePtyBackend::with_pty_sink(sink_recorder.clone());
    let pane_id = PaneId::new();
    spawn_script(&pty_backend, pane_id, SCRIPT_BLOCKS_AFTER_PRINTING);

    let close_started_at = Instant::now();
    pty_backend
        .kill_pane(pane_id, KillPolicy::Tree)
        .expect("kill pane");
    let close_duration = close_started_at.elapsed();

    // The bound is half the whole standby, not one round of it. A close that
    // sat through the standby takes the full limit, which is twice this; a
    // close that is woken takes the OS work alone, which measures in tenths
    // of a millisecond. The gap is room for process teardown on a loaded
    // host.
    let standby_limit_duration = EXIT_PUBLISH_LIMIT_DURATION / 2;
    assert!(
        close_duration < standby_limit_duration,
        "closing the pane stood by for the exit: took {close_duration:?}, which is over \
         the {standby_limit_duration:?} that separates a woken close from one that waited \
         out the {EXIT_PUBLISH_LIMIT_DURATION:?} standby"
    );

    // The reader reaches the end of the PTY after the close and tries to
    // report the exit it was waiting for. Let it finish doing so: once it
    // drops its reference, only this test and the backend hold the sink.
    wait_until_condition(|| Arc::strong_count(&sink_recorder) <= SINK_HOLDER_COUNT_AFTER_CLOSE);
    assert_eq!(
        sink_recorder.get_exit_status(),
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
fn a_settled_pane_forwards_no_more_of_a_descendant_process_output() {
    // The child exits while a descendant keeps the terminal open, so the
    // watcher publishes the exit and the consumer lets the pane go. What the
    // descendant prints afterwards belongs to a pane that no longer exists,
    // and the reader stops instead of forwarding it.
    let sink_recorder = SinkRecorder::new();
    let pty_backend = PortablePtyBackend::with_pty_sink(sink_recorder.clone());
    let pane_id = PaneId::new();
    spawn_script(
        &pty_backend,
        pane_id,
        SCRIPT_DESCENDANT_PRINTS_AFTER_CHILD_PROCESS_EXITS,
    );

    assert_eq!(
        sink_recorder.wait_for_exit(),
        Some(ExitStatus::ExitCode(5)),
        "the pane was never told its child ended"
    );

    // Outlast the descendant's own wait, so its output has reached the
    // terminal and the reader has had every chance to forward it.
    thread::sleep(LATE_OUTPUT_WAIT_DURATION);

    let delivered_output_text = sink_recorder.get_delivered_output_text();
    assert!(
        !delivered_output_text.contains(LATE_OUTPUT_MARKER),
        "output printed after the pane's exit was settled still reached the \
         consumer: {delivered_output_text:?}"
    );

    // Reap the descendant still holding the terminal open.
    let _ = pty_backend.kill_pane(pane_id, KillPolicy::Tree);
}

#[test]
fn a_sink_backed_pane_hands_back_a_handle_with_no_channels() {
    // The handle carries no receivers, which is how the runtime knows this
    // pane needs no forwarder thread: it is already delivering to the sink.
    let sink_recorder = SinkRecorder::new();
    let pty_backend = PortablePtyBackend::with_pty_sink(sink_recorder.clone());
    let pane_id = PaneId::new();
    let mut pty_handle = spawn_script(&pty_backend, pane_id, SCRIPT_EXITS_WITH_CODE_0);

    assert_eq!(pty_handle.get_pane_id(), pane_id);
    assert!(pty_handle.take_output_and_exit_receivers().is_none());
    assert_eq!(pty_handle.try_receive_output_chunk(), None);
    assert_eq!(pty_handle.try_receive_exit_status(), None);

    // The sink is still the one being fed.
    assert_eq!(sink_recorder.wait_for_exit(), Some(ExitStatus::ExitCode(0)));
}

#[test]
fn a_gone_consumer_is_never_told_the_child_ended() {
    // A consumer that refuses a chunk is finished with the pane. The watcher
    // stands by to report the exit itself when the PTY reports no end, and
    // the reader takes charge of that exit on its way out, so a child that
    // ends promptly has no exit handed to a consumer that already said it was
    // done. The refusing-sink test above cannot catch this: its child runs
    // long enough that the watcher never reaches its standby window.
    let refusing_sink = RefusingSink::new();
    let pty_backend = PortablePtyBackend::with_pty_sink(refusing_sink.clone());
    let pane_id = PaneId::new();
    spawn_script(&pty_backend, pane_id, SCRIPT_PRINTS_THEN_EXITS_WITH_CODE_3);

    // Well past the longest the watcher stands by, so an exit that was going
    // to be delivered has been by now.
    thread::sleep(EXIT_PUBLISH_LIMIT_DURATION * 2);

    assert!(
        refusing_sink.has_refused_output(),
        "the sink was never offered any output, so it never said it was done"
    );
    assert!(
        !refusing_sink.has_seen_exit_status(),
        "an exit was reported to a consumer that had already gone"
    );

    let _ = pty_backend.kill_pane(pane_id, KillPolicy::Tree);
}

/// A sink that holds the reader inside `output` until the test lets go, so a
/// chunk stays in the consumer's hands for as long as the test wants.
///
/// Unix only, alongside the one test that parks a reader: on Windows a parked
/// reader never answers the console's startup query, so the child never runs.
#[cfg(unix)]
struct StalledSink {
    /// Everything delivered so far, oldest first.
    sink_deliveries: Mutex<Vec<SinkDelivery>>,
    /// Held by the test; the reader blocks on it inside `output`.
    output_delivery_gate: Mutex<()>,
}

#[cfg(unix)]
impl StalledSink {
    fn new() -> Arc<Self> {
        Arc::new(StalledSink {
            sink_deliveries: Mutex::new(Vec::new()),
            output_delivery_gate: Mutex::new(()),
        })
    }

    /// A snapshot of everything delivered so far, oldest first.
    fn list_sink_deliveries(&self) -> Vec<SinkDelivery> {
        self.sink_deliveries.lock().expect("stalled sink").clone()
    }

    /// The recorded exit status, if one has been delivered.
    fn get_exit_status(&self) -> Option<ExitStatus> {
        find_exit_status(&self.list_sink_deliveries())
    }

    /// How many exits have been delivered. A consumer is told a child ended
    /// exactly once.
    fn count_delivered_exit_statuses(&self) -> usize {
        self.list_sink_deliveries()
            .iter()
            .filter(|delivery| matches!(delivery, SinkDelivery::ExitStatus(_)))
            .count()
    }
}

#[cfg(unix)]
impl PtySink for StalledSink {
    fn accept_output_bytes(&self, _pane_id: PaneId, output_bytes: Vec<u8>) -> bool {
        self.sink_deliveries
            .lock()
            .expect("stalled sink")
            .push(SinkDelivery::OutputBytes(output_bytes));
        // Park the reader here. `sink_deliveries` is released first, so the test can read
        // what has been delivered while this thread waits.
        let _output_delivery_guard = self
            .output_delivery_gate
            .lock()
            .expect("stalled sink output gate");
        true
    }

    fn accept_exit_status(&self, _pane_id: PaneId, exit_status: ExitStatus) {
        self.sink_deliveries
            .lock()
            .expect("stalled sink")
            .push(SinkDelivery::ExitStatus(exit_status));
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
    let stalled_sink = StalledSink::new();
    let output_delivery_guard = stalled_sink
        .output_delivery_gate
        .lock()
        .expect("hold the reader");
    let pty_backend = PortablePtyBackend::with_pty_sink(stalled_sink.clone());
    let pane_id = PaneId::new();
    spawn_script(&pty_backend, pane_id, SCRIPT_PRINTS_THEN_EXITS_WITH_CODE_3);

    // One recorded output means the reader is inside `output`, on the gate this
    // test holds.
    wait_until_condition(|| !stalled_sink.list_sink_deliveries().is_empty());
    assert!(
        !stalled_sink.list_sink_deliveries().is_empty(),
        "the sink was never offered any output, so no chunk is in its hands"
    );

    // Watch for twice the longest wait the backend has. Load only lengthens
    // this window, so nothing but a published exit can fail the check.
    thread::sleep(EXIT_PUBLISH_LIMIT_DURATION * 2);
    assert_eq!(
        stalled_sink.get_exit_status(),
        None,
        "an exit was handed to a consumer still holding a chunk: {:?}",
        stalled_sink.list_sink_deliveries()
    );

    // Let the chunk land. The reader reaches the end of the child's output and
    // reports the exit behind it.
    drop(output_delivery_guard);
    wait_until_condition(|| stalled_sink.get_exit_status().is_some());
    assert_eq!(
        stalled_sink.get_exit_status(),
        Some(ExitStatus::ExitCode(3)),
        "the stalled consumer was never told its child ended"
    );

    // Let the reader finish reacting: once it drops its reference, only this
    // test and the backend hold the sink.
    wait_until_condition(|| Arc::strong_count(&stalled_sink) <= SINK_HOLDER_COUNT_WITHOUT_READER);
    assert_eq!(
        stalled_sink.count_delivered_exit_statuses(),
        1,
        "the child's end was reported more than once"
    );

    let sink_deliveries = stalled_sink.list_sink_deliveries();
    let exit_delivery_index = sink_deliveries
        .iter()
        .position(|delivery| matches!(delivery, SinkDelivery::ExitStatus(_)))
        .expect("exit recorded");
    assert!(
        exit_delivery_index > 0,
        "the exit came before any output: {sink_deliveries:?}"
    );
    assert_eq!(
        exit_delivery_index,
        sink_deliveries.len() - 1,
        "output reached the consumer after its exit: {sink_deliveries:?}"
    );

    let _ = pty_backend.kill_pane(pane_id, KillPolicy::Tree);
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
    let sink_recorder = SinkRecorder::new();
    let pty_backend = PortablePtyBackend::with_pty_sink(sink_recorder.clone());
    let pane_id = PaneId::new();
    spawn_script(&pty_backend, pane_id, SCRIPT_BLOCKS_AFTER_PRINTING);

    pty_backend
        .kill_pane(
            pane_id,
            KillPolicy::Graceful {
                timeout_duration: GRACEFUL_STOP_DURATION,
            },
        )
        .expect("kill pane");

    // Let the reader finish reacting to the close before asking: once it drops
    // its reference, only this test and the backend hold the sink.
    wait_until_condition(|| Arc::strong_count(&sink_recorder) <= SINK_HOLDER_COUNT_AFTER_CLOSE);
    assert_eq!(
        sink_recorder.get_exit_status(),
        None,
        "a closed pane's consumer was handed an exit by its reader"
    );
}
