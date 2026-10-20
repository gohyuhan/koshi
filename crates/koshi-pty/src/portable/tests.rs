//! Unit tests for [`ChildGuard`]'s kill-on-drop backstop, the two reader
//! pumps, the watcher's standby wait, the reader's delivery gate, the reader
//! park, the writer flush, the pane hand-over across a process-image swap,
//! the reader that takes the terminal's opening cursor question out of the
//! output, and the pure status/size conversions. The tests that spawn a real
//! Unix PTY are Unix-gated; everything else runs on every platform.

use super::*;

use std::sync::atomic::AtomicBool;

#[cfg(unix)]
use koshi_core::process::ShellKind;
#[cfg(unix)]
use std::{collections::BTreeMap, path::PathBuf};

/// Whether process `process_id` is still around (`kill -0` succeeds).
#[cfg(unix)]
fn is_process_alive(process_id: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &process_id.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|process_status| process_status.success())
        .unwrap_or(false)
}

/// Launch a long-lived child inside a PTY, returning a guard over it and the
/// child's pid.
#[cfg(unix)]
fn spawn_guarded_sleep_process() -> (ChildGuard, u32) {
    let pty_pair = native_pty_system()
        .openpty(build_portable_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        }))
        .expect("openpty");
    let mut command_builder = CommandBuilder::new("/bin/sh");
    command_builder.arg("-c");
    command_builder.arg("sleep 300");
    let child_guard = ChildGuard::from_child(
        pty_pair
            .slave
            .spawn_command(command_builder)
            .expect("spawn"),
    );
    drop(pty_pair.slave);
    let child_process_id = child_guard.process_id().expect("process id");
    (child_guard, child_process_id)
}

#[cfg(unix)]
#[test]
fn dropping_an_armed_guard_kills_the_child() {
    let (child_guard, child_process_id) = spawn_guarded_sleep_process();
    assert!(
        is_process_alive(child_process_id),
        "child should be running before drop"
    );
    drop(child_guard);
    // Polls up to 3 seconds for the child to go.
    let exit_deadline = Instant::now() + Duration::from_secs(3);
    while is_process_alive(child_process_id) && Instant::now() < exit_deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !is_process_alive(child_process_id),
        "an armed guard must kill the child on drop"
    );
}

#[cfg(unix)]
#[test]
fn disarming_leaves_the_child_running() {
    let (child_guard, child_process_id) = spawn_guarded_sleep_process();
    let mut child_process = child_guard.release_child();
    assert!(
        is_process_alive(child_process_id),
        "disarming must not kill the child"
    );
    let _ = child_process.kill(); // clean up the still-running child
}

// `build_portable_pty_size` makes no platform syscalls. The Unix backend
// opens and resizes every terminal through it.

#[cfg(unix)]
#[test]
fn build_portable_pty_size_carries_columns_and_rows_and_zeroes_pixels() {
    let portable_pty_size = build_portable_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });
    assert_eq!(portable_pty_size.cols, 80);
    assert_eq!(portable_pty_size.rows, 24);
    assert_eq!(portable_pty_size.pixel_width, 0);
    assert_eq!(portable_pty_size.pixel_height, 0);
}

#[cfg(unix)]
#[test]
fn build_portable_pty_size_carries_boundary_dimensions_unchanged() {
    let portable_pty_size = build_portable_pty_size(PtySize {
        column_count: 0,
        row_count: 0,
    });
    assert_eq!((portable_pty_size.cols, portable_pty_size.rows), (0, 0));

    let portable_pty_size = build_portable_pty_size(PtySize {
        column_count: u16::MAX,
        row_count: u16::MAX,
    });
    assert_eq!(
        (portable_pty_size.cols, portable_pty_size.rows),
        (u16::MAX, u16::MAX)
    );
}

/// An [`ObservedChildExit`] for `exit_status` that keeps no zombie.
fn build_observed_exit(exit_status: ExitStatus) -> ObservedChildExit {
    ObservedChildExit {
        exit_status,
        #[cfg(unix)]
        is_zombie_kept: false,
    }
}

/// A [`ChildExit`] that records an exit with code `0`.
fn build_ended_child_exit() -> ChildExit {
    let child_exit = ChildExit::default();
    child_exit.publish_observed_exit(build_observed_exit(ExitStatus::ExitCode(0)));
    child_exit
}

/// What `waitid` with `WEXITED | WNOWAIT | WNOHANG` answers for
/// `child_process_id`, collecting nothing: `Ok(child_process_id)` for an ended
/// child that is still a zombie, `Ok(0)` for a child that still runs, and
/// `Err(errno)` otherwise, such as `ECHILD` once the child is reaped.
#[cfg(unix)]
fn read_child_wait_state(child_process_id: u32) -> Result<libc::pid_t, libc::c_int> {
    // SAFETY: an all-zero `siginfo_t` is valid, and `waitid` writes only into it.
    let mut child_signal_information: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `WNOHANG` returns at once, and `WNOWAIT` collects nothing.
    let wait_result = unsafe {
        libc::waitid(
            libc::P_PID,
            child_process_id,
            &mut child_signal_information,
            libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
        )
    };
    if wait_result != 0 {
        return Err(std::io::Error::last_os_error()
            .raw_os_error()
            .expect("waitid sets errno"));
    }
    // SAFETY: a successful `waitid` filled the fields it reports.
    Ok(unsafe { child_signal_information.si_pid() })
}

/// Wait until `is_condition_met` answers `true`, asking every 5 ms. Fails the
/// test with `failure_message` once [`HANG_GUARD_DURATION`] has passed.
#[cfg(unix)]
fn wait_for_condition(mut is_condition_met: impl FnMut() -> bool, failure_message: &str) {
    let deadline = Instant::now() + HANG_GUARD_DURATION;
    while !is_condition_met() {
        assert!(Instant::now() < deadline, "{failure_message}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Wait up to [`HANG_GUARD_DURATION`] until child `child_process_id` has
/// ended, leaving it a zombie.
#[cfg(unix)]
fn wait_until_child_is_zombie(child_process_id: u32) {
    let expected_process_id =
        libc::pid_t::try_from(child_process_id).expect("a child process id fits pid_t");
    wait_for_condition(
        || read_child_wait_state(child_process_id) == Ok(expected_process_id),
        &format!("child {child_process_id} did not end"),
    );
}

/// Nothing received the stop request. The 3-second window is not spent.
#[test]
fn is_child_stopped_within_grace_returns_false_for_an_undelivered_stop_request() {
    let child_exit = ChildExit::default();
    let wait_started_at = Instant::now();
    let is_stopped = is_child_stopped_within_grace(
        StopRequest::NotDelivered,
        &child_exit,
        Duration::from_secs(3),
    );
    let elapsed_duration = wait_started_at.elapsed();
    assert!(
        !is_stopped,
        "an undelivered stop request must report the child as still running"
    );
    assert!(
        elapsed_duration < Duration::from_millis(100),
        "an undelivered stop request must not spend the grace window; took {elapsed_duration:?}"
    );
}

/// A stop request that reached the child still waits the whole 200ms window
/// while the child stays alive.
#[test]
fn is_child_stopped_within_grace_waits_when_a_delivered_child_stays() {
    let child_exit = ChildExit::default();
    let wait_started_at = Instant::now();
    let is_stopped = is_child_stopped_within_grace(
        StopRequest::Delivered,
        &child_exit,
        Duration::from_millis(200),
    );
    let elapsed_duration = wait_started_at.elapsed();
    assert!(
        !is_stopped,
        "a child whose exit is never recorded must report as still running"
    );
    assert!(
        elapsed_duration >= Duration::from_millis(200),
        "a delivered stop request must still spend the whole window; took {elapsed_duration:?}"
    );
}

/// A stop request that reached part of a group waits the whole 200ms window
/// while the leader stays alive.
#[test]
fn is_child_stopped_within_grace_waits_for_a_partly_delivered_stop_request() {
    let child_exit = ChildExit::default();
    let wait_started_at = Instant::now();
    let is_stopped = is_child_stopped_within_grace(
        StopRequest::Unknown,
        &child_exit,
        Duration::from_millis(200),
    );
    let elapsed_duration = wait_started_at.elapsed();
    assert!(
        !is_stopped,
        "a group whose leader's exit is never recorded must report as still running"
    );
    assert!(
        elapsed_duration >= Duration::from_millis(200),
        "a partly delivered stop request must still spend the whole window; took {elapsed_duration:?}"
    );
}

/// A child that already exited is reported as stopped at once, without waiting
/// out the 3-second window.
#[test]
fn is_child_stopped_within_grace_returns_true_for_an_exited_child() {
    let child_exit = build_ended_child_exit();
    let wait_started_at = Instant::now();
    let is_stopped =
        is_child_stopped_within_grace(StopRequest::Delivered, &child_exit, Duration::from_secs(3));
    assert!(
        is_stopped,
        "a child whose exit is recorded must report as stopped"
    );
    assert!(
        wait_started_at.elapsed() < Duration::from_millis(100),
        "an already-exited child must not spend the grace window; took {:?}",
        wait_started_at.elapsed()
    );
}

/// A child that exits inside the window ends the wait when its exit is
/// recorded, not at the end of the window.
#[test]
fn is_child_stopped_within_grace_ends_when_the_child_exits_during_the_window() {
    let child_exit = Arc::new(ChildExit::default());
    let watcher_child_exit = Arc::clone(&child_exit);
    let exit_publisher_thread = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        watcher_child_exit.publish_observed_exit(build_observed_exit(ExitStatus::ExitCode(0)));
    });

    let wait_started_at = Instant::now();
    let is_stopped =
        is_child_stopped_within_grace(StopRequest::Delivered, &child_exit, Duration::from_secs(3));
    let elapsed_duration = wait_started_at.elapsed();
    exit_publisher_thread
        .join()
        .expect("the exit publisher thread ends");

    assert!(
        is_stopped,
        "a child that exits inside the window must report as stopped"
    );
    assert!(
        elapsed_duration < Duration::from_secs(1),
        "the wait must end at the flip, not at the window; took {elapsed_duration:?}"
    );
}

/// A sink that keeps every chunk and every exit it is handed, oldest first.
struct CountingSink {
    /// Every output chunk taken, oldest first.
    output_chunks: Mutex<Vec<Vec<u8>>>,
    /// Every exit taken, oldest first.
    exit_statuses: Mutex<Vec<ExitStatus>>,
}

impl CountingSink {
    fn new() -> Arc<Self> {
        Arc::new(CountingSink {
            output_chunks: Mutex::new(Vec::new()),
            exit_statuses: Mutex::new(Vec::new()),
        })
    }

    /// How many chunks have reached this sink.
    fn get_output_chunk_count(&self) -> usize {
        self.output_chunks.lock().expect("counting sink").len()
    }

    /// Every byte this sink has been handed, in order.
    #[cfg(unix)]
    fn collect_output_bytes(&self) -> Vec<u8> {
        self.output_chunks.lock().expect("counting sink").concat()
    }

    /// The exit this sink has been handed, or `None` while none has arrived.
    fn get_first_exit_status(&self) -> Option<ExitStatus> {
        self.exit_statuses
            .lock()
            .expect("counting sink")
            .first()
            .copied()
    }

    /// How many exits have reached this sink.
    fn get_exit_status_count(&self) -> usize {
        self.exit_statuses.lock().expect("counting sink").len()
    }
}

impl PtySink for CountingSink {
    fn accept_output_bytes(&self, _pane_id: PaneId, output_bytes: Vec<u8>) -> bool {
        self.output_chunks
            .lock()
            .expect("counting sink")
            .push(output_bytes);
        true
    }

    fn accept_exit_status(&self, _pane_id: PaneId, exit_status: ExitStatus) {
        self.exit_statuses
            .lock()
            .expect("counting sink")
            .push(exit_status);
    }
}

/// How long a test waits for a thread it expects back. A wait that reaches it
/// fails the test.
const HANG_GUARD_DURATION: Duration = Duration::from_secs(5);

/// [`should_publish_exit`] with the production limit and grace. The tests that
/// need a deadline already past call it directly.
fn should_publish_exit_after_wait(
    cancel_receiver: &Receiver<()>,
    exit_handover_state: &Mutex<ExitHandover>,
) -> bool {
    should_publish_exit(
        cancel_receiver,
        exit_handover_state,
        Instant::now() + EXIT_PUBLISH_LIMIT_DURATION,
        EXIT_PUBLISH_GRACE_DURATION,
    )
}

/// [`ReaderSignals`] over `reader_waker`, `child_exit` and `reader_gate`.
#[cfg(unix)]
fn build_reader_signals<'a>(
    reader_waker: &'a Waker,
    child_exit: &'a ChildExit,
    reader_gate: &'a ReaderGate,
) -> ReaderSignals<'a> {
    ReaderSignals {
        reader_waker,
        child_exit,
        reader_gate,
    }
}

/// A [`Delivery`] over `pty_sink`, and the [`ExitHandover`] it shares with the
/// watcher. The exit sender is dropped.
fn build_sink_delivery(pty_sink: Arc<dyn PtySink>) -> (Delivery, Arc<Mutex<ExitHandover>>) {
    let (_exit_sender, exit_receiver) = channel::<ExitStatus>();
    let exit_handover_state = Arc::new(Mutex::new(ExitHandover::default()));
    let delivery = Delivery {
        pty_sink,
        exit_receiver,
        exit_handover_state: Arc::clone(&exit_handover_state),
    };
    (delivery, exit_handover_state)
}

/// A handover holding `started_chunk_count` chunks claimed and
/// `finished_chunk_count` released.
fn build_exit_handover_state(
    started_chunk_count: u64,
    finished_chunk_count: u64,
) -> Mutex<ExitHandover> {
    Mutex::new(ExitHandover {
        started_chunk_count,
        finished_chunk_count,
        is_settled: false,
    })
}

#[test]
fn handing_a_chunk_over_counts_it_in_and_back_out() {
    // A chunk fully handed over advances `started_chunk_count` and
    // `finished_chunk_count` by one each and
    // leaves them equal.
    let pty_sink = CountingSink::new();
    let (delivery, exit_handover_state) = build_sink_delivery(pty_sink.clone());

    assert!(delivery.deliver_output(PaneId::new(), b"hi"));
    assert_eq!(pty_sink.get_output_chunk_count(), 1);
    assert_eq!(
        exit_handover_state
            .lock()
            .expect("exit handover state")
            .started_chunk_count,
        1,
        "the chunk was counted in"
    );
    assert_eq!(
        exit_handover_state
            .lock()
            .expect("exit handover state")
            .finished_chunk_count,
        1,
        "and counted back out"
    );
}

#[test]
fn a_settled_pane_stops_its_reader_and_takes_no_more_output() {
    // Output arriving after the pane is settled is refused: the reader is told
    // to stop, and the consumer is handed nothing.
    let pty_sink = CountingSink::new();
    let (delivery, exit_handover_state) = build_sink_delivery(pty_sink.clone());
    let pane_id = PaneId::new();

    assert!(delivery.deliver_output(pane_id, b"before"));
    exit_handover_state
        .lock()
        .expect("exit handover state")
        .is_settled = true;

    assert!(
        !delivery.deliver_output(pane_id, b"after"),
        "a settled pane must tell its reader to stop"
    );
    assert_eq!(
        pty_sink.get_output_chunk_count(),
        1,
        "no output may reach the consumer once the pane is settled"
    );
}

/// A sink that refuses every chunk and records no exit.
struct RefusingSink;

impl PtySink for RefusingSink {
    fn accept_output_bytes(&self, _pane_id: PaneId, _output_bytes: Vec<u8>) -> bool {
        false
    }

    fn accept_exit_status(&self, _pane_id: PaneId, _exit_status: ExitStatus) {}
}

#[test]
fn a_refused_chunk_settles_the_pane() {
    // A refused chunk settles the pane.
    let (delivery, exit_handover_state) = build_sink_delivery(Arc::new(RefusingSink));

    assert!(!delivery.deliver_output(PaneId::new(), b"hi"));
    assert!(
        exit_handover_state
            .lock()
            .expect("exit handover state")
            .is_settled,
        "a refused chunk must settle the pane so the watcher publishes no exit"
    );
}

#[test]
fn a_settled_pane_finishes_without_waiting_for_the_exit_status() {
    // `exit_sender` is held for the whole test and never sent on.
    // `publish_exit_status` on a settled pane returns without reading the
    // status channel.
    let (exit_sender, exit_receiver) = channel::<ExitStatus>();
    let delivery = Delivery {
        pty_sink: CountingSink::new(),
        exit_receiver,
        exit_handover_state: Arc::new(Mutex::new(ExitHandover {
            started_chunk_count: 0,
            finished_chunk_count: 0,
            is_settled: true,
        })),
    };

    let (completion_sender, completion_receiver) = channel::<()>();
    thread::spawn(move || {
        delivery.publish_exit_status(PaneId::new());
        let _ = completion_sender.send(());
    });

    assert_eq!(
        completion_receiver.recv_timeout(HANG_GUARD_DURATION),
        Ok(()),
        "a settled pane waited for an exit status it will never publish"
    );
    drop(exit_sender);
}

#[test]
fn a_reader_at_the_end_of_its_terminal_hands_the_watchers_status_over_once() {
    // The watcher's status is already on the channel. `publish_exit_status`
    // hands it to the consumer once and settles the pane.
    let pty_sink = CountingSink::new();
    let (exit_sender, exit_receiver) = channel::<ExitStatus>();
    let exit_handover_state = Arc::new(Mutex::new(ExitHandover::default()));
    let delivery = Delivery {
        pty_sink: pty_sink.clone(),
        exit_receiver,
        exit_handover_state: Arc::clone(&exit_handover_state),
    };
    exit_sender
        .send(ExitStatus::ExitCode(4))
        .expect("queue the status");

    delivery.publish_exit_status(PaneId::new());

    assert_eq!(
        pty_sink.get_first_exit_status(),
        Some(ExitStatus::ExitCode(4))
    );
    assert_eq!(
        pty_sink.get_exit_status_count(),
        1,
        "the exit must be handed over once"
    );
    assert!(
        exit_handover_state
            .lock()
            .expect("exit handover state")
            .is_settled,
        "handing the exit over must settle the pane"
    );
}

#[test]
fn a_watcher_that_ends_without_a_status_tells_the_consumer_nothing() {
    // The status channel closes with nothing on it. `publish_exit_status`
    // returns and the consumer is handed no exit.
    let pty_sink = CountingSink::new();
    let (exit_sender, exit_receiver) = channel::<ExitStatus>();
    drop(exit_sender);
    let delivery = Delivery {
        pty_sink: pty_sink.clone(),
        exit_receiver,
        exit_handover_state: Arc::new(Mutex::new(ExitHandover::default())),
    };

    delivery.publish_exit_status(PaneId::new());

    assert_eq!(
        pty_sink.get_exit_status_count(),
        0,
        "a status that never arrived must not be published"
    );
}

/// [`wait_for_reader_before_publishing`] over `exit_handover_state`, publishing
/// `ExitCode(3)` to `pty_sink` with the production limit and grace, and a cancel
/// that never fires.
fn publish_exit_code_3_after_reader_wait(
    pty_sink: &Arc<CountingSink>,
    exit_handover_state: &Mutex<ExitHandover>,
) {
    let (_cancel_sender, cancel_receiver) = channel::<()>();
    let weak_pty_sink = Arc::downgrade(pty_sink) as Weak<dyn PtySink>;
    wait_for_reader_before_publishing(
        &cancel_receiver,
        exit_handover_state,
        &weak_pty_sink,
        PaneId::new(),
        ExitStatus::ExitCode(3),
    );
}

#[test]
fn a_watcher_that_steps_in_hands_the_exit_over_and_settles_it() {
    // The pane is never settled. After the limit the watcher hands the exit
    // over, once, and settles the pane.
    let pty_sink = CountingSink::new();
    let exit_handover_state = build_exit_handover_state(0, 0);

    publish_exit_code_3_after_reader_wait(&pty_sink, &exit_handover_state);

    assert_eq!(
        pty_sink.get_first_exit_status(),
        Some(ExitStatus::ExitCode(3))
    );
    assert_eq!(pty_sink.get_exit_status_count(), 1);
    assert!(
        exit_handover_state
            .lock()
            .expect("exit handover state")
            .is_settled,
        "an exit was published without settling the pane, so the reader can \
         publish a second one"
    );
}

#[test]
fn a_watcher_that_publishes_nothing_still_settles_the_pane() {
    // A cancel queued before the standby starts ends it without publishing.
    // The pane is settled all the same.
    let pty_sink = CountingSink::new();
    let (cancel_sender, cancel_receiver) = channel::<()>();
    cancel_sender.send(()).expect("queue the cancel");
    let exit_handover_state = build_exit_handover_state(0, 0);
    let weak_pty_sink = Arc::downgrade(&pty_sink) as Weak<dyn PtySink>;

    wait_for_reader_before_publishing(
        &cancel_receiver,
        &exit_handover_state,
        &weak_pty_sink,
        PaneId::new(),
        ExitStatus::ExitCode(3),
    );

    assert_eq!(
        pty_sink.get_exit_status_count(),
        0,
        "a cancelled standby published an exit"
    );
    assert!(
        exit_handover_state
            .lock()
            .expect("exit handover state")
            .is_settled,
        "a cancelled standby left the pane unsettled"
    );
}

#[test]
fn a_pane_the_reader_already_settled_is_not_published_over() {
    // The pane is already settled. The watcher publishes nothing.
    let pty_sink = CountingSink::new();
    let exit_handover_state = Mutex::new(ExitHandover {
        started_chunk_count: 0,
        finished_chunk_count: 0,
        is_settled: true,
    });

    publish_exit_code_3_after_reader_wait(&pty_sink, &exit_handover_state);

    assert_eq!(
        pty_sink.get_exit_status_count(),
        0,
        "the consumer was handed a second exit for one child"
    );
}

#[test]
fn a_watcher_whose_consumer_is_gone_publishes_nothing() {
    // The sink has been dropped; only the watcher's weak reference is left.
    // The standby returns without publishing.
    let pty_sink = CountingSink::new();
    let weak_pty_sink = Arc::downgrade(&pty_sink) as Weak<dyn PtySink>;
    drop(pty_sink);
    let (_cancel_sender, cancel_receiver) = channel::<()>();
    let exit_handover_state = build_exit_handover_state(0, 0);

    wait_for_reader_before_publishing(
        &cancel_receiver,
        &exit_handover_state,
        &weak_pty_sink,
        PaneId::new(),
        ExitStatus::ExitCode(3),
    );

    assert!(
        weak_pty_sink.upgrade().is_none(),
        "the standby brought the consumer back to life"
    );
}

#[test]
fn a_reader_that_never_reaches_the_end_leaves_the_exit_to_the_watcher() {
    // The pane is never settled. Once the deadline passes, the standby answers
    // `true`.
    let (_cancel_sender, cancel_receiver) = channel::<()>();
    let exit_handover_state = build_exit_handover_state(0, 0);

    let wait_started_at = Instant::now();
    let should_publish_exit_status = should_publish_exit(
        &cancel_receiver,
        &exit_handover_state,
        Instant::now() + EXIT_PUBLISH_GRACE_DURATION * 2,
        EXIT_PUBLISH_GRACE_DURATION,
    );

    assert!(
        should_publish_exit_status,
        "a reader that cannot reach the end publishes nothing"
    );
    assert!(
        wait_started_at.elapsed() >= EXIT_PUBLISH_GRACE_DURATION * 2,
        "the watcher must give the reader until the limit before stepping in"
    );
}

#[test]
fn a_reader_that_settles_the_exit_stops_the_watcher_publishing() {
    // The pane is settled halfway through the first round. The standby answers
    // `false` before the limit.
    let (_cancel_sender, cancel_receiver) = channel::<()>();
    let exit_handover_state = Arc::new(Mutex::new(ExitHandover::default()));

    let reader_exit_handover_state = Arc::clone(&exit_handover_state);
    thread::spawn(move || {
        thread::sleep(EXIT_PUBLISH_GRACE_DURATION / 2);
        reader_exit_handover_state
            .lock()
            .expect("exit handover state")
            .is_settled = true;
    });

    let wait_started_at = Instant::now();
    let should_publish_exit_status =
        should_publish_exit_after_wait(&cancel_receiver, &exit_handover_state);

    assert!(
        !should_publish_exit_status,
        "the reader published, so the watcher must not"
    );
    assert!(
        wait_started_at.elapsed() < EXIT_PUBLISH_LIMIT_DURATION,
        "a settled pane must end the standby rather than sit out the limit"
    );
}

#[test]
fn an_exit_is_never_published_over_a_chunk_still_in_flight() {
    // The deadline has already passed and one chunk is in flight. The standby
    // answers only once that chunk lands.
    let (_cancel_sender, cancel_receiver) = channel::<()>();
    // One chunk begun and not yet finished: in the consumer's hands.
    let exit_handover_state = Arc::new(Mutex::new(ExitHandover {
        started_chunk_count: 1,
        finished_chunk_count: 0,
        is_settled: false,
    }));

    // Set right before the chunk lands.
    let chunk_landed = Arc::new(AtomicBool::new(false));
    let chunk_landed_flag = Arc::clone(&chunk_landed);
    let consumer_exit_handover_state = Arc::clone(&exit_handover_state);
    let finish_chunk_thread = thread::spawn(move || {
        thread::sleep(EXIT_PUBLISH_GRACE_DURATION * 3);
        chunk_landed_flag.store(true, Ordering::SeqCst);
        consumer_exit_handover_state
            .lock()
            .expect("exit handover state")
            .finished_chunk_count = 1;
    });

    let should_publish_exit_status = should_publish_exit(
        &cancel_receiver,
        &exit_handover_state,
        Instant::now(),
        EXIT_PUBLISH_GRACE_DURATION,
    );

    assert!(
        should_publish_exit_status,
        "the deadline has passed and nothing is in flight, so the watcher \
         publishes"
    );
    assert!(
        chunk_landed.load(Ordering::SeqCst),
        "an exit was published while a chunk was still in the consumer's hands"
    );
    finish_chunk_thread.join().expect("finish chunk thread");
}

#[test]
fn a_chunk_that_never_lands_holds_the_watcher_until_the_pane_is_closed() {
    // The deadline has already passed and one chunk is in flight that never
    // lands. The cancel ends the standby, and it answers `false`.
    //
    // The standby runs on its own thread; one that has not ended after
    // `HANG_GUARD_DURATION` fails the test.
    let (cancel_sender, cancel_receiver) = channel::<()>();
    let (publish_result_sender, publish_result_receiver) = channel::<bool>();
    thread::spawn(move || {
        let should_publish_exit_status = should_publish_exit(
            &cancel_receiver,
            &build_exit_handover_state(1, 0),
            Instant::now(),
            EXIT_PUBLISH_GRACE_DURATION,
        );
        let _ = publish_result_sender.send(should_publish_exit_status);
    });

    cancel_sender.send(()).expect("close the pane");

    let should_publish_exit_status = publish_result_receiver
        .recv_timeout(HANG_GUARD_DURATION)
        .expect("closing the pane must end the standby");
    assert!(
        !should_publish_exit_status,
        "a cancelled standby must publish no exit"
    );
}

#[test]
fn closing_a_pane_ends_the_watchers_standby_without_publishing() {
    // A cancel queued before the standby starts ends it before the limit, and
    // it answers `false`.
    let (cancel_sender, cancel_receiver) = channel::<()>();
    cancel_sender.send(()).expect("queue the cancel");

    let wait_started_at = Instant::now();
    let should_publish_exit_status =
        should_publish_exit_after_wait(&cancel_receiver, &build_exit_handover_state(0, 0));

    assert!(
        !should_publish_exit_status,
        "a cancelled standby must publish no exit"
    );
    assert!(
        wait_started_at.elapsed() < EXIT_PUBLISH_LIMIT_DURATION,
        "a cancelled standby must not sit through the rounds"
    );
}

#[test]
fn a_dropped_pane_entry_ends_the_watchers_standby_without_publishing() {
    // The cancel sender is dropped before the standby starts. The standby
    // answers `false`.
    let (cancel_sender, cancel_receiver) = channel::<()>();
    drop(cancel_sender);

    let should_publish_exit_status =
        should_publish_exit_after_wait(&cancel_receiver, &build_exit_handover_state(0, 0));

    assert!(
        !should_publish_exit_status,
        "a dropped entry must publish no exit"
    );
}

/// A stand-in terminal for the reader's wait: a connected socket pair, with
/// `peer_socket` playing the child writing into it and the returned descriptor
/// the master end the pump waits on and reads. Dropping `peer_socket` is the terminal
/// reporting an end.
#[cfg(unix)]
fn build_fake_terminal() -> (std::os::unix::net::UnixStream, std::os::fd::OwnedFd) {
    let (peer_socket, terminal_socket) =
        std::os::unix::net::UnixStream::pair().expect("terminal pair");
    (peer_socket, std::os::fd::OwnedFd::from(terminal_socket))
}

#[cfg(unix)]
#[test]
fn resizing_a_terminal_retunes_the_size_its_child_reads() {
    // The size set through the pane's own descriptor is read back through
    // `portable-pty`'s master of the same terminal.
    let pty_pair = native_pty_system()
        .openpty(build_portable_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        }))
        .expect("openpty");
    let terminal_fd =
        duplicate_terminal_file_descriptor(&*pty_pair.master).expect("terminal descriptor");

    resize_terminal(
        &terminal_fd,
        PtySize {
            column_count: 132,
            row_count: 43,
        },
    )
    .expect("resize the terminal");

    let portable_pty_size = pty_pair.master.get_size().expect("read the size back");
    assert_eq!(
        (portable_pty_size.cols, portable_pty_size.rows),
        (132, 43),
        "the child would still be told the old window size"
    );
}

#[cfg(unix)]
#[test]
fn a_terminal_carries_both_directions_over_the_one_descriptor() {
    // One descriptor carries both directions: bytes written through it reach
    // the far end, and bytes the far end writes are read through it.
    let (peer_socket, terminal_socket) =
        std::os::unix::net::UnixStream::pair().expect("terminal pair");
    let terminal_fd = std::os::fd::OwnedFd::from(terminal_socket);

    write_terminal(&terminal_fd, b"typed").expect("write to the terminal");
    let mut input_buffer = [0u8; 8];
    let input_byte_count = (&peer_socket)
        .read(&mut input_buffer)
        .expect("the child reads its input");
    assert_eq!(
        &input_buffer[..input_byte_count],
        b"typed",
        "input never reached the child"
    );

    (&peer_socket)
        .write_all(b"printed")
        .expect("the child prints");
    let mut output_buffer = [0u8; 8];
    let output_byte_count =
        read_terminal(&terminal_fd, &mut output_buffer).expect("read the terminal");
    assert_eq!(
        &output_buffer[..output_byte_count],
        b"printed",
        "output never reached the reader"
    );
}

#[cfg(unix)]
#[test]
fn a_writer_that_stops_sends_the_terminal_nothing() {
    // `typed` is queued before the `Stop` and `late` after it. The writer
    // thread holds the terminal's only descriptor, which closes when the thread
    // ends: the far end reads `typed` and then the end of the terminal.
    let (mut peer_socket, terminal_socket) =
        std::os::unix::net::UnixStream::pair().expect("terminal pair");
    let writer_sender = start_writer(
        WriteSide::Owned(Arc::new(std::os::fd::OwnedFd::from(terminal_socket))),
        Arc::new(AtomicUsize::new(b"typedlate".len())),
    );

    writer_sender
        .send(WriterMessage::Bytes(b"typed".to_vec()))
        .expect("queue input");
    writer_sender
        .send(WriterMessage::Stop)
        .expect("stop the writer");
    let _ = writer_sender.send(WriterMessage::Bytes(b"late".to_vec()));

    peer_socket
        .set_read_timeout(Some(HANG_GUARD_DURATION))
        .expect("bound the read");
    let mut terminal_input_bytes = Vec::new();
    peer_socket
        .read_to_end(&mut terminal_input_bytes)
        .expect("the writer thread ends and closes the terminal");
    assert_eq!(
        terminal_input_bytes, b"typed",
        "a writer that stops must send the terminal nothing after the `Stop`"
    );
}

#[cfg(unix)]
#[test]
fn only_a_pseudoterminal_master_is_named_as_one() {
    // `find_terminal_master_name` answers for a pseudoterminal master only: an
    // ordinary file, a pipe, and a closed number all give `None`.
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileTypeExt;

    let pty_pair = native_pty_system()
        .openpty(build_portable_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        }))
        .expect("openpty");
    let terminal_master_fd =
        duplicate_terminal_file_descriptor(&*pty_pair.master).expect("terminal descriptor");
    let terminal_master_name = find_terminal_master_name(terminal_master_fd.as_raw_fd())
        .expect("a pane's own terminal must be named as a pseudoterminal master");
    assert!(
        std::fs::metadata(&terminal_master_name)
            .expect("the name must be a path that exists")
            .file_type()
            .is_char_device(),
        "the name must be a terminal device, got {terminal_master_name:?}"
    );

    let ordinary_file = std::fs::File::open("/dev/null").expect("open an ordinary file");
    assert_eq!(
        find_terminal_master_name(ordinary_file.as_raw_fd()),
        None,
        "an ordinary file must not be named as a pseudoterminal master"
    );

    let (reading_end, writing_end) = std::io::pipe().expect("the pipe opens");
    assert_eq!(
        find_terminal_master_name(reading_end.as_raw_fd()),
        None,
        "a pipe must not be named as a pseudoterminal master"
    );

    // The number of a descriptor this process has closed.
    let closed_file_descriptor = writing_end.as_raw_fd();
    drop(writing_end);
    assert_eq!(
        find_terminal_master_name(closed_file_descriptor),
        None,
        "a number naming nothing open must not be named as a pseudoterminal master"
    );
}

#[cfg(unix)]
#[test]
fn pane_descriptors_are_never_handed_to_a_subsequent_child() {
    // A pane's terminal descriptor and its waker both carry `FD_CLOEXEC`.
    use std::os::fd::{AsFd, AsRawFd, FromRawFd};

    fn is_close_on_exec(terminal_file_descriptor: std::os::fd::BorrowedFd<'_>) -> bool {
        // SAFETY: `F_GETFD` reads no memory, and the borrowed descriptor is
        // open.
        let descriptor_flags =
            unsafe { libc::fcntl(terminal_file_descriptor.as_raw_fd(), libc::F_GETFD) };
        assert!(descriptor_flags >= 0, "read the descriptor's flags");
        descriptor_flags & libc::FD_CLOEXEC != 0
    }

    let pty_pair = native_pty_system()
        .openpty(build_portable_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        }))
        .expect("openpty");
    let terminal_fd =
        duplicate_terminal_file_descriptor(&*pty_pair.master).expect("terminal descriptor");
    assert!(
        is_close_on_exec(terminal_fd.as_fd()),
        "a pane's terminal would be inherited by every child spawned after it"
    );

    let waker = Waker::new().expect("this platform offers a one-descriptor wake");
    assert!(
        is_close_on_exec(waker.as_fd()),
        "a pane's waker would be inherited by every child spawned after it"
    );

    // The control: a plain `dup` of the terminal carries no `FD_CLOEXEC`.
    // SAFETY: `dup` takes the descriptor by value and reads no memory.
    let inherited_terminal_descriptor = unsafe { libc::dup(terminal_fd.as_raw_fd()) };
    assert!(inherited_terminal_descriptor >= 0, "duplicate the terminal");
    // SAFETY: `dup` just opened `inherited_terminal_descriptor`, and nothing
    // else owns it.
    let inherited_terminal_fd =
        unsafe { std::os::fd::OwnedFd::from_raw_fd(inherited_terminal_descriptor) };
    assert!(
        !is_close_on_exec(inherited_terminal_fd.as_fd()),
        "the check reports close-on-exec for a descriptor that is not"
    );
}

#[cfg(unix)]
#[test]
fn a_doorbell_keeps_ringing_until_it_is_drained() {
    // A fresh doorbell reads as nothing. A ring makes it readable, and it stays
    // readable until drained. A drained doorbell rings again.
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::AsFd;

    let waker = Waker::new().expect("this platform offers a one-descriptor wake");
    let is_waker_ready = |waker: &Waker| {
        let mut poll_file_descriptors = [PollFd::new(waker.as_fd(), PollFlags::POLLIN)];
        poll(&mut poll_file_descriptors, PollTimeout::ZERO).expect("poll the doorbell");
        poll_file_descriptors[0]
            .revents()
            .is_some_and(|poll_events| !poll_events.is_empty())
    };

    assert!(
        !is_waker_ready(&waker),
        "a fresh doorbell must read as nothing"
    );
    waker.wake_reader();
    assert!(
        is_waker_ready(&waker),
        "ringing must make the descriptor readable"
    );
    assert!(
        is_waker_ready(&waker),
        "a ring must stay on until it is drained"
    );

    waker.drain_wake_signal();
    assert!(
        !is_waker_ready(&waker),
        "draining must take the ring back off"
    );

    waker.wake_reader();
    assert!(is_waker_ready(&waker), "a drained doorbell must ring again");
    waker.drain_wake_signal();
    assert!(!is_waker_ready(&waker), "and drain again");
}

#[cfg(unix)]
#[test]
fn a_woken_reader_hands_over_the_last_output_then_stops() {
    // `bye` is in the terminal when the child is marked reaped and the
    // doorbell rings. The pump hands `bye` over, then stops after one quiet
    // round.
    let (peer_socket, terminal_fd) = build_fake_terminal();
    let waker = Waker::new().expect("waker");
    let pty_sink = CountingSink::new();
    let (delivery, _exit_handover_state) = build_sink_delivery(pty_sink.clone());
    let reader_gate = ReaderGate::new();

    (&peer_socket)
        .write_all(b"bye")
        .expect("child prints on the way out");
    // The watcher's order: the exit record first, then the ring.
    let child_exit = build_ended_child_exit();
    waker.wake_reader();

    let pump_started_at = Instant::now();
    pump_waited(
        &delivery,
        PaneId::new(),
        &terminal_fd,
        build_reader_signals(&waker, &child_exit, &reader_gate),
        EXIT_PUBLISH_GRACE_DURATION,
        EXIT_PUBLISH_LIMIT_DURATION,
    );

    assert_eq!(
        pty_sink
            .output_chunks
            .lock()
            .expect("counting sink")
            .as_slice(),
        [b"bye".to_vec()],
        "output already in the terminal when the child died must still be \
         handed over"
    );
    assert!(
        pump_started_at.elapsed() >= EXIT_PUBLISH_GRACE_DURATION,
        "the reader must give the terminal a full quiet round before deciding \
         it has everything"
    );
    drop(peer_socket);
}

#[cfg(unix)]
#[test]
fn a_closed_pane_brings_its_reader_straight_back() {
    // `peer_socket` stays open for the whole test. The pane is settled and the
    // doorbell rung before the pump starts; the pump returns before the first
    // round is out.
    let (peer_socket, terminal_fd) = build_fake_terminal();
    let waker = Waker::new().expect("waker");
    let (delivery, exit_handover_state) = build_sink_delivery(CountingSink::new());

    // Rounds far longer than any scheduling delay.
    let grace_duration = Duration::from_secs(2);
    let limit_duration = Duration::from_secs(10);

    // `kill_pane`'s order: settle, then ring.
    exit_handover_state
        .lock()
        .expect("exit handover state")
        .is_settled = true;
    waker.wake_reader();

    let (completion_sender, completion_receiver) = channel::<Instant>();
    thread::spawn(move || {
        let child_exit = ChildExit::default();
        let reader_gate = ReaderGate::new();
        pump_waited(
            &delivery,
            PaneId::new(),
            &terminal_fd,
            build_reader_signals(&waker, &child_exit, &reader_gate),
            grace_duration,
            limit_duration,
        );
        let _ = completion_sender.send(Instant::now());
    });

    let pump_started_at = Instant::now();
    let pump_ended_at = completion_receiver.recv_timeout(limit_duration).expect(
        "a closed pane must not leave its reader waiting on a \
                 descendant that holds the terminal open",
    );
    assert!(
        pump_ended_at.duration_since(pump_started_at) < grace_duration,
        "a closed pane must release its reader at once, not after a round"
    );
    drop(peer_socket);
}

#[cfg(unix)]
#[test]
fn a_reader_waits_on_a_live_terminal_with_no_timer() {
    // With the child running and the terminal quiet, the pump stays in its
    // wait for three rounds. The terminal reporting an end releases it.
    let (peer_socket, terminal_fd) = build_fake_terminal();
    let waker = Waker::new().expect("waker");
    let (delivery, _exit_handover_state) = build_sink_delivery(CountingSink::new());

    let (completion_sender, completion_receiver) = channel::<()>();
    thread::spawn(move || {
        let child_exit = ChildExit::default();
        let reader_gate = ReaderGate::new();
        pump_waited(
            &delivery,
            PaneId::new(),
            &terminal_fd,
            build_reader_signals(&waker, &child_exit, &reader_gate),
            EXIT_PUBLISH_GRACE_DURATION,
            EXIT_PUBLISH_LIMIT_DURATION,
        );
        let _ = completion_sender.send(());
    });

    assert_eq!(
        completion_receiver
            .recv_timeout(EXIT_PUBLISH_GRACE_DURATION * 3)
            .err(),
        Some(std::sync::mpsc::RecvTimeoutError::Timeout),
        "a reader with a live child and a quiet terminal must not come back on \
         a timer"
    );

    drop(peer_socket); // the terminal reports an end
    assert_eq!(
        completion_receiver.recv_timeout(HANG_GUARD_DURATION),
        Ok(()),
        "a terminal that reports an end must release the reader"
    );
}

#[cfg(unix)]
#[test]
fn output_arriving_after_the_child_has_gone_is_still_handed_over() {
    // The child is marked reaped before `a`, `b`, `c` arrive 200ms apart. Each
    // round that brings bytes starts another round; every byte is handed over.
    let (peer_socket, terminal_fd) = build_fake_terminal();
    let waker = Waker::new().expect("waker");
    let pty_sink = CountingSink::new();
    let (delivery, _exit_handover_state) = build_sink_delivery(pty_sink.clone());

    // Rounds twice the gap between writes.
    let grace_duration = Duration::from_millis(400);
    let limit_duration = Duration::from_secs(5);

    let child_exit = build_ended_child_exit();
    waker.wake_reader(); // the child has gone
    let printer_thread = thread::spawn(move || {
        for output_byte in [b'a', b'b', b'c'] {
            thread::sleep(Duration::from_millis(200));
            (&peer_socket)
                .write_all(&[output_byte])
                .expect("the descendant prints");
        }
        peer_socket // hold the terminal open until the pump has stopped
    });

    let reader_gate = ReaderGate::new();
    pump_waited(
        &delivery,
        PaneId::new(),
        &terminal_fd,
        build_reader_signals(&waker, &child_exit, &reader_gate),
        grace_duration,
        limit_duration,
    );

    // Two writes landing close together come back as one chunk. The bytes are
    // compared end to end, not the chunk count.
    let output_bytes: Vec<u8> = pty_sink
        .output_chunks
        .lock()
        .expect("counting sink")
        .concat();
    assert_eq!(
        output_bytes, b"abc",
        "every byte a descendant printed before the terminal went quiet must \
         reach the consumer, in order"
    );
    drop(printer_thread.join().expect("printer thread"));
}

#[cfg(unix)]
#[test]
fn a_descendant_that_never_stops_printing_still_ends_the_reader() {
    // The child is marked reaped and bytes never stop arriving. The rounds
    // stop at the limit.
    let (peer_socket, terminal_fd) = build_fake_terminal();
    let waker = Waker::new().expect("waker");
    let (delivery, _exit_handover_state) = build_sink_delivery(CountingSink::new());

    let grace_duration = Duration::from_millis(200);
    let limit_duration = Duration::from_secs(1);

    let child_exit = build_ended_child_exit();
    waker.wake_reader(); // the child has gone

    // The printer never sleeps: the socket buffer always holds something
    // between the pump's reads. `peer_socket` is nonblocking: a full buffer
    // refuses the write instead of parking the printer.
    peer_socket.set_nonblocking(true).expect("nonblocking");
    let is_printing = Arc::new(AtomicBool::new(true));
    let should_keep_printing = Arc::clone(&is_printing);
    let printer_thread = thread::spawn(move || {
        while should_keep_printing.load(Ordering::SeqCst) {
            let _ = (&peer_socket).write(b"x");
            thread::yield_now();
        }
        peer_socket
    });

    let reader_gate = ReaderGate::new();
    let pump_started_at = Instant::now();
    pump_waited(
        &delivery,
        PaneId::new(),
        &terminal_fd,
        build_reader_signals(&waker, &child_exit, &reader_gate),
        grace_duration,
        limit_duration,
    );
    let elapsed_duration = pump_started_at.elapsed();

    is_printing.store(false, Ordering::SeqCst);
    drop(printer_thread.join().expect("printer thread"));

    assert!(
        elapsed_duration >= limit_duration,
        "the reader must keep handing output over until the limit"
    );
    assert!(
        elapsed_duration < limit_duration * 3,
        "and must stop there rather than running for as long as the \
         descendant does"
    );
}

/// A stand-in terminal for the blocking pump: hands back `output_chunks` in
/// order and then reports an end, counting how many the pump took.
struct ScriptedTerminal {
    /// Output chunks still to hand out, oldest first.
    output_chunks: Vec<&'static [u8]>,
    /// How many output chunks the pump has read.
    read_chunk_count: usize,
}

impl ScriptedTerminal {
    fn from_output_chunks(output_chunks: Vec<&'static [u8]>) -> Self {
        ScriptedTerminal {
            output_chunks,
            read_chunk_count: 0,
        }
    }
}

impl Read for ScriptedTerminal {
    fn read(&mut self, output_buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.output_chunks.is_empty() {
            return Ok(0); // the terminal reports an end
        }
        let output_chunk = self.output_chunks.remove(0);
        output_buffer[..output_chunk.len()].copy_from_slice(output_chunk);
        self.read_chunk_count += 1;
        Ok(output_chunk.len())
    }
}

#[test]
fn a_blocking_reader_hands_over_every_chunk_until_the_terminal_ends() {
    // The terminal hands out two chunks and then ends. The pump hands both
    // over, in order, and answers `true`.
    let pty_sink = CountingSink::new();
    let (delivery, _exit_handover_state) = build_sink_delivery(pty_sink.clone());
    let mut reader = ScriptedTerminal::from_output_chunks(vec![b"one", b"two"]);

    let is_terminal_ended = pump_blocking(&mut reader, &delivery, PaneId::new());

    assert!(
        is_terminal_ended,
        "the terminal ended, so the caller may report the child's exit"
    );
    assert_eq!(
        pty_sink
            .output_chunks
            .lock()
            .expect("counting sink")
            .as_slice(),
        [b"one".to_vec(), b"two".to_vec()],
        "every chunk before the end must reach the consumer, in order"
    );
}

#[test]
fn a_blocking_reader_stops_when_the_consumer_lets_the_pane_go() {
    // The consumer refuses the first chunk of a terminal that never ends. The
    // pump answers `false` and the pane is settled.
    struct Endless;

    impl Read for Endless {
        fn read(&mut self, destination_bytes: &mut [u8]) -> std::io::Result<usize> {
            destination_bytes[0] = b'x';
            Ok(1)
        }
    }

    let (delivery, exit_handover_state) = build_sink_delivery(Arc::new(RefusingSink));

    let is_terminal_ended = pump_blocking(&mut Endless, &delivery, PaneId::new());

    assert!(
        !is_terminal_ended,
        "the terminal is still open, so no exit may be reported behind it"
    );
    assert!(
        exit_handover_state
            .lock()
            .expect("exit handover state")
            .is_settled,
        "a refused chunk must stop the reader delivering and settle the pane"
    );
}

#[test]
fn a_reader_told_to_stop_leaves_the_rest_of_the_terminal_unread() {
    // A refused chunk ends the pump where it stands. The chunks behind it stay
    // unread.
    let (delivery, _exit_handover_state) = build_sink_delivery(Arc::new(RefusingSink));
    let mut reader = ScriptedTerminal::from_output_chunks(vec![b"one", b"two", b"three"]);

    let is_terminal_ended = pump_blocking(&mut reader, &delivery, PaneId::new());

    assert!(
        !is_terminal_ended,
        "the pump reported an end it never reached"
    );
    assert_eq!(
        reader.read_chunk_count, 1,
        "the pump read past the refused chunk"
    );
    assert_eq!(
        reader.output_chunks,
        [b"two".as_slice(), b"three".as_slice()],
        "the chunks behind the refused one must be left unread"
    );
}

// Windows only: `drain_terminal` is built there.
#[cfg(windows)]
#[test]
fn draining_a_terminal_reads_it_to_the_end() {
    // The drain reads every chunk to the terminal's end.
    let mut reader = ScriptedTerminal::from_output_chunks(vec![b"one", b"two", b"three"]);

    drain_terminal(&mut reader);

    assert_eq!(
        reader.read_chunk_count, 3,
        "the drain stopped short of the end"
    );
    assert_eq!(
        reader.output_chunks.len(),
        0,
        "and left output behind for the close to wait on"
    );
}

#[test]
fn resizing_a_terminal_that_is_already_closed_changes_nothing() {
    // A `Master` terminal whose master has been taken out takes a resize as
    // `Ok(())`.
    let terminal = Terminal::Master(Arc::new(Mutex::new(None)));

    assert_eq!(
        terminal.resize(PtySize {
            column_count: 132,
            row_count: 43
        }),
        Ok(()),
        "a closed terminal must take a resize as a no-op"
    );
}

#[test]
fn a_settled_pane_takes_no_chunk_and_claims_none() {
    // A settled pane refuses the chunk without claiming it:
    // `started_chunk_count` and `finished_chunk_count` stay at 0.
    let pty_sink = CountingSink::new();
    let (delivery, exit_handover_state) = build_sink_delivery(pty_sink.clone());
    exit_handover_state
        .lock()
        .expect("exit handover state")
        .is_settled = true;

    assert!(
        !delivery.deliver_output(PaneId::new(), b"late"),
        "a settled pane takes no more output"
    );
    assert_eq!(
        pty_sink.get_output_chunk_count(),
        0,
        "and hands the consumer nothing"
    );
    let handover_guard = exit_handover_state.lock().expect("exit handover state");
    assert_eq!(
        handover_guard.started_chunk_count, 0,
        "an unclaimed chunk leaves nothing begun"
    );
    assert_eq!(
        handover_guard.finished_chunk_count, 0,
        "and nothing to release"
    );
    assert!(
        !handover_guard.has_chunk_in_flight(),
        "and nothing reads as in flight"
    );
}

// The reader park and the pane hand-over tests below drive real children in
// real PTYs and are built on Unix only.

/// Standard test window: 80 columns × 24 rows.
#[cfg(unix)]
const STANDARD_PTY_SIZE: PtySize = PtySize {
    column_count: 80,
    row_count: 24,
};

/// Serializes PTY creation across the parallel test threads. macOS `openpty(3)`
/// races under concurrent allocation.
#[cfg(unix)]
static PTY_GATE: Mutex<()> = Mutex::new(());

/// A spawn spec running `shell_script` under `/bin/sh`.
#[cfg(unix)]
fn build_shell_spawn_spec(shell_script: &str) -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from("/bin/sh"),
        arguments: vec!["-c".to_string(), shell_script.to_string()],
        working_directory: None,
        environment_variables: BTreeMap::new(),
        shell_kind: ShellKind::from_program(std::path::Path::new("/bin/sh")),
    }
}

/// A backend delivering to `pty_sink`, with one pane running `shell_script` in a real
/// PTY, and that pane's id.
#[cfg(unix)]
fn start_backend_with_shell_script(
    pty_sink: Arc<CountingSink>,
    shell_script: &str,
) -> (Arc<PortablePtyBackend>, PaneId) {
    let backend = Arc::new(PortablePtyBackend::with_pty_sink(pty_sink));
    let pane_id = PaneId::new();
    let _gate = PTY_GATE.lock().expect("pty gate");
    backend
        .spawn_pane(
            pane_id,
            build_shell_spawn_spec(shell_script),
            STANDARD_PTY_SIZE,
        )
        .expect("spawn the pane");
    (backend, pane_id)
}

/// Wait until `pty_sink` holds `expected_output_bytes`, and hand back everything it holds. Fails
/// the test after `HANG_GUARD_DURATION` if the bytes never arrive.
#[cfg(unix)]
fn read_output_until_contains(pty_sink: &CountingSink, expected_output_bytes: &[u8]) -> Vec<u8> {
    let output_deadline = Instant::now() + HANG_GUARD_DURATION;
    loop {
        let output_bytes = pty_sink.collect_output_bytes();
        if output_bytes
            .windows(expected_output_bytes.len())
            .any(|window| window == expected_output_bytes)
        {
            return output_bytes;
        }
        assert!(
            Instant::now() < output_deadline,
            "the consumer was never handed {:?}; it holds {:?}",
            String::from_utf8_lossy(expected_output_bytes),
            String::from_utf8_lossy(&output_bytes)
        );
        thread::sleep(Duration::from_millis(5));
    }
}

/// Pause `pty_backend`'s readers and wait until they park, on a thread of its
/// own. A pause that has not settled after `HANG_GUARD_DURATION` fails the
/// test.
#[cfg(unix)]
fn pause_readers_or_fail(pty_backend: &Arc<PortablePtyBackend>) -> Result<(), PtyError> {
    let (pause_result_sender, pause_result_receiver) = channel::<Result<(), PtyError>>();
    let pausing_backend = Arc::clone(pty_backend);
    thread::spawn(move || {
        let pause_result = pausing_backend
            .pause_readers()
            .map(|()| pausing_backend.wait_until_readers_parked());
        let _ = pause_result_sender.send(pause_result);
    });
    pause_result_receiver
        .recv_timeout(HANG_GUARD_DURATION)
        .expect("pausing the readers never settled")
}

/// Wait until `pty_sink` holds an exit, and hand it back. Fails the test with
/// `failure_message` after `HANG_GUARD_DURATION`.
#[cfg(unix)]
fn read_exit_status_or_fail(pty_sink: &CountingSink, failure_message: &str) -> ExitStatus {
    let exit_deadline = Instant::now() + HANG_GUARD_DURATION;
    loop {
        if let Some(exit_status) = pty_sink.get_first_exit_status() {
            return exit_status;
        }
        assert!(Instant::now() < exit_deadline, "{failure_message}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Carry `terminal_file_descriptor` across a process-image swap, in one process: clear its
/// close-on-exec flag, duplicate it, own the duplicate, and set the flag on
/// the duplicate.
#[cfg(unix)]
fn duplicate_terminal_for_process_swap(
    terminal_file_descriptor: std::os::fd::RawFd,
) -> std::os::fd::OwnedFd {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    set_terminal_cloexec(terminal_file_descriptor, false).expect("clear close-on-exec");
    // SAFETY: `dup` takes the descriptor by value and reads no memory.
    let carried_file_descriptor = unsafe { libc::dup(terminal_file_descriptor) };
    assert!(
        carried_file_descriptor >= 0,
        "the descriptor must survive the swap"
    );
    // SAFETY: `dup` just opened `carried_file_descriptor`, and nothing else
    // owns it.
    let carried_terminal_fd = unsafe { OwnedFd::from_raw_fd(carried_file_descriptor) };
    set_terminal_cloexec(carried_terminal_fd.as_raw_fd(), true).expect("set close-on-exec");
    carried_terminal_fd
}

#[cfg(unix)]
#[test]
fn pausing_holds_a_live_panes_output_and_resuming_releases_it() {
    // The child stays alive for the whole test. Everything read before the
    // pause is with the consumer, nothing reaches the consumer while parked,
    // and resuming hands the held bytes over.
    let pty_sink = CountingSink::new();
    let (backend, pane_id) =
        start_backend_with_shell_script(pty_sink.clone(), "printf ready; sleep 30");

    read_output_until_contains(&pty_sink, b"ready");
    let output_before_pause = pty_sink.collect_output_bytes();

    pause_readers_or_fail(&backend).expect("pause the readers");
    assert_eq!(
        pty_sink.collect_output_bytes(),
        output_before_pause,
        "pausing must neither lose nor invent output"
    );

    // The terminal echoes what is written to it.
    backend
        .write_pane_input(pane_id, b"held\n")
        .expect("write to the pane");
    thread::sleep(EXIT_PUBLISH_GRACE_DURATION * 3);
    assert_eq!(
        pty_sink.collect_output_bytes(),
        output_before_pause,
        "a parked reader must hand the consumer nothing"
    );

    backend.resume_readers();
    let output_after_resume = read_output_until_contains(&pty_sink, b"held");
    assert_eq!(
        &output_after_resume[..output_before_pause.len()],
        output_before_pause.as_slice(),
        "resuming must leave what was already delivered untouched"
    );

    backend
        .kill_pane(pane_id, KillPolicy::Tree)
        .expect("close the pane");
}

#[cfg(unix)]
#[test]
fn a_resumed_reader_is_not_left_in_its_exit_rounds() {
    // A pause and a resume, then a wait past the limit. The reader is still
    // pumping: a line written afterwards comes back.
    let pty_sink = CountingSink::new();
    let (backend, pane_id) =
        start_backend_with_shell_script(pty_sink.clone(), "printf ready; sleep 30");

    read_output_until_contains(&pty_sink, b"ready");
    pause_readers_or_fail(&backend).expect("pause the readers");
    backend.resume_readers();

    thread::sleep(EXIT_PUBLISH_LIMIT_DURATION + EXIT_PUBLISH_GRACE_DURATION * 2);

    backend
        .write_pane_input(pane_id, b"again\n")
        .expect("write to the pane");
    read_output_until_contains(&pty_sink, b"again");

    backend
        .kill_pane(pane_id, KillPolicy::Tree)
        .expect("close the pane");
}

#[cfg(unix)]
#[test]
fn pausing_settles_when_a_panes_child_has_already_ended() {
    // The pane's child has ended and its reader has left its pump. The pause
    // settles with `reader_count` at 0.
    let pty_sink = CountingSink::new();
    let (backend, pane_id) = start_backend_with_shell_script(pty_sink.clone(), "printf bye");

    assert_eq!(
        read_exit_status_or_fail(&pty_sink, "the child's exit never arrived"),
        ExitStatus::ExitCode(0),
        "the pane's child ran to a clean exit"
    );

    pause_readers_or_fail(&backend).expect("pause the readers");
    assert_eq!(
        backend
            .reader_gate
            .gate_state
            .lock()
            .expect("reader gate")
            .reader_count,
        0,
        "a reader that left its pump must no longer be counted"
    );

    backend.resume_readers();
    backend
        .kill_pane(pane_id, KillPolicy::Force)
        .expect("close the pane");
}

#[cfg(unix)]
#[test]
fn a_reader_waiting_for_an_exit_that_never_comes_is_no_longer_counted() {
    // The terminal ends while the pane is unsettled and no exit is published.
    // The reader waits inside `publish_exit_status`, and leaves the gate
    // before that wait: `reader_count` drops to 0 and `parked_reader_count`
    // stays 0.
    let (peer_socket, terminal_fd) = build_fake_terminal();
    let reader_gate = Arc::new(ReaderGate::new());

    // Held for the whole test and never sent on.
    let (exit_sender, exit_receiver) = channel::<ExitStatus>();
    let delivery = Delivery {
        pty_sink: CountingSink::new(),
        exit_receiver,
        exit_handover_state: Arc::new(Mutex::new(ExitHandover::default())),
    };

    let reader_ticket = reader_gate.register_reader();
    assert_eq!(
        reader_gate
            .gate_state
            .lock()
            .expect("reader gate")
            .reader_count,
        1,
        "a reader is counted from before its thread starts"
    );

    let _reader = start_owned_reader(
        delivery,
        PaneId::new(),
        Arc::new(terminal_fd),
        Arc::new(Waker::new().expect("waker")),
        Arc::new(ChildExit::default()),
        reader_ticket,
    );

    drop(peer_socket); // the terminal reports an end

    wait_for_condition(
        || {
            reader_gate
                .gate_state
                .lock()
                .expect("reader gate")
                .reader_count
                == 0
        },
        "a reader past its pump is still counted, so a pause would never settle",
    );
    assert_eq!(
        reader_gate
            .gate_state
            .lock()
            .expect("reader gate")
            .parked_reader_count,
        0,
        "and it must not read as parked either"
    );

    drop(exit_sender); // release the reader from its wait
}

#[cfg(unix)]
#[test]
fn a_pane_whose_reader_cannot_park_refuses_the_pause() {
    // A pane with no doorbell (`reader_waker: None`) refuses the pause by name,
    // and the gate stays open.
    let backend = PortablePtyBackend::with_pty_sink(CountingSink::new());
    let pane_id = PaneId::new();
    let (writer_sender, _writer_receiver) = channel::<WriterMessage>();
    backend.pane_by_id.lock().expect("panes").insert(
        pane_id,
        build_pane_entry(
            Terminal::Master(Arc::new(Mutex::new(None))),
            writer_sender,
            Arc::new(AtomicUsize::new(0)),
        ),
    );

    let pause_error = backend
        .pause_readers()
        .expect_err("a reader that cannot park must refuse the pause");

    assert_eq!(
        pause_error,
        PtyError::Io {
            detail: format!("pane {pane_id} has no terminal descriptor, so its reader cannot park"),
        },
        "the refusal must name the pane that cannot be paused"
    );
    assert!(
        !backend
            .reader_gate
            .gate_state
            .lock()
            .expect("reader gate")
            .is_paused,
        "a refused pause must leave the gate as it was"
    );
}

/// A terminal that takes no write until it is let go: each write waits for one
/// value on `release_receiver`.
struct HeldTerminal {
    /// One value here lets one write land.
    release_receiver: Receiver<()>,
    /// Everything the writes have landed, in order.
    written_bytes: Arc<Mutex<Vec<u8>>>,
}

impl Write for HeldTerminal {
    fn write(&mut self, input_bytes: &[u8]) -> std::io::Result<usize> {
        let _ = self.release_receiver.recv();
        self.written_bytes
            .lock()
            .expect("written bytes")
            .extend_from_slice(input_bytes);
        Ok(input_bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_writer_answers_a_barrier_only_after_writing_what_came_before() {
    // A barrier queued behind a write is answered only after that write lands.
    let written_bytes = Arc::new(Mutex::new(Vec::new()));
    let (release_sender, release_receiver) = channel::<()>();
    let writer_sender = start_writer(
        WriteSide::Blocking(Box::new(HeldTerminal {
            release_receiver,
            written_bytes: Arc::clone(&written_bytes),
        })),
        Arc::new(AtomicUsize::new(b"typed".len())),
    );

    writer_sender
        .send(WriterMessage::Bytes(b"typed".to_vec()))
        .expect("queue the input");
    let (barrier_sender, barrier_receiver) = channel::<()>();
    writer_sender
        .send(WriterMessage::Barrier(barrier_sender))
        .expect("queue the barrier");
    assert_eq!(
        barrier_receiver.recv_timeout(Duration::from_millis(100)),
        Err(RecvTimeoutError::Timeout),
        "a barrier must not be answered while the write before it is under way"
    );

    release_sender.send(()).expect("let the write land");
    assert_eq!(
        barrier_receiver.recv_timeout(HANG_GUARD_DURATION),
        Ok(()),
        "the barrier must be answered once the write has landed"
    );
    assert_eq!(
        written_bytes.lock().expect("written bytes").as_slice(),
        b"typed",
        "an answered barrier must mean every byte queued before it is written"
    );

    let _ = writer_sender.send(WriterMessage::Stop);
}

/// A pane entry around `terminal` and `writer_sender`, whose writer thread
/// counts its queued input in `queued_input_byte_count`, with no child of its
/// own. Its process id is this test process, its child is marked exited, and
/// its size is [`STANDARD_PTY_SIZE`].
#[cfg(unix)]
fn build_pane_entry(
    terminal: Terminal,
    writer_sender: Sender<WriterMessage>,
    queued_input_byte_count: Arc<AtomicUsize>,
) -> PaneEntry {
    let (exit_wait_cancel_sender, _exit_wait_cancel_receiver) = channel::<()>();
    PaneEntry {
        terminal,
        pty_size: STANDARD_PTY_SIZE,
        writer_sender,
        queued_input_byte_count,
        // This test process's own id.
        kill_control: PtyChildKillControl::from_process_id(std::process::id()),
        child_exit: Arc::new(build_ended_child_exit()),
        exit_handover_state: Arc::new(Mutex::new(ExitHandover::default())),
        exit_wait_cancel_sender,
        reader_thread: spawn_pty_thread("koshi-pty-read", || {}),
        reader_waker: None,
        watcher_thread: spawn_pty_thread("koshi-pty-watch", || {}),
    }
}

#[cfg(unix)]
#[test]
fn a_flush_answers_only_once_the_terminal_holds_every_byte_the_backend_took() {
    // A flush that answers `Ok` leaves every byte the backend took on the
    // terminal.
    let (peer_socket, terminal_socket) =
        std::os::unix::net::UnixStream::pair().expect("terminal pair");
    let terminal_fd = Arc::new(std::os::fd::OwnedFd::from(terminal_socket));
    let backend = PortablePtyBackend::with_pty_sink(CountingSink::new());
    let pane_id = PaneId::new();
    let queued_input_byte_count = Arc::new(AtomicUsize::new(0));
    let writer_sender = start_writer(
        WriteSide::Owned(Arc::clone(&terminal_fd)),
        Arc::clone(&queued_input_byte_count),
    );
    backend.pane_by_id.lock().expect("panes").insert(
        pane_id,
        build_pane_entry(
            Terminal::Owned(terminal_fd),
            writer_sender,
            queued_input_byte_count,
        ),
    );

    backend
        .write_pane_input(pane_id, b"typed")
        .expect("write to the pane");
    backend.flush_writers().expect("the flush answers");

    peer_socket.set_nonblocking(true).expect("nonblocking");
    let mut input_buffer = [0u8; 32];
    let input_byte_count = (&peer_socket)
        .read(&mut input_buffer)
        .expect("the child reads its input");
    assert_eq!(
        &input_buffer[..input_byte_count],
        b"typed",
        "a flush that answered left a byte the backend took unwritten"
    );
}

#[cfg(unix)]
#[test]
fn a_pane_whose_writer_cannot_finish_refuses_the_flush() {
    // The pane's writer is blocked inside its write. The flush refuses after
    // its limit and names that pane.
    let (release_sender, release_receiver) = channel::<()>();
    let backend = PortablePtyBackend::with_pty_sink(CountingSink::new());
    let pane_id = PaneId::new();
    let queued_input_byte_count = Arc::new(AtomicUsize::new(0));
    let writer_sender = start_writer(
        WriteSide::Blocking(Box::new(HeldTerminal {
            release_receiver,
            written_bytes: Arc::new(Mutex::new(Vec::new())),
        })),
        Arc::clone(&queued_input_byte_count),
    );
    backend.pane_by_id.lock().expect("panes").insert(
        pane_id,
        build_pane_entry(
            Terminal::Master(Arc::new(Mutex::new(None))),
            writer_sender,
            queued_input_byte_count,
        ),
    );

    backend
        .write_pane_input(pane_id, b"typed")
        .expect("write to the pane");
    let flush_error = backend
        .flush_writers()
        .expect_err("a writer that cannot finish must refuse the flush");

    assert_eq!(
        flush_error,
        PtyError::Io {
            detail: format!(
                "pane {pane_id} is still writing what it was handed, so it cannot settle"
            ),
        },
        "the refusal must name the pane that is still being written to"
    );

    release_sender.send(()).expect("let the write land");
}

#[cfg(unix)]
#[test]
fn a_pane_whose_child_takes_no_input_refuses_input_past_its_queue_limit() {
    // The writer holds its first write until it is let go: no write finishes
    // before the asserts, and the count keeps every queued byte.
    let (release_sender, release_receiver) = channel::<()>();
    let written_bytes = Arc::new(Mutex::new(Vec::new()));
    let backend = PortablePtyBackend::with_pty_sink(CountingSink::new());
    let pane_id = PaneId::new();
    let queued_input_byte_count = Arc::new(AtomicUsize::new(0));
    let writer_sender = start_writer(
        WriteSide::Blocking(Box::new(HeldTerminal {
            release_receiver,
            written_bytes: Arc::clone(&written_bytes),
        })),
        Arc::clone(&queued_input_byte_count),
    );
    backend.pane_by_id.lock().expect("panes").insert(
        pane_id,
        build_pane_entry(
            Terminal::Master(Arc::new(Mutex::new(None))),
            writer_sender,
            Arc::clone(&queued_input_byte_count),
        ),
    );
    let input_one_byte_short_of_the_limit = vec![b'a'; MAX_QUEUED_PANE_INPUT_BYTE_COUNT - 1];

    backend
        .write_pane_input(pane_id, &input_one_byte_short_of_the_limit)
        .expect("input up to the limit is queued");
    backend
        .write_pane_input(pane_id, b"b")
        .expect("the byte that reaches the limit is queued");
    assert_eq!(
        backend.write_pane_input(pane_id, b"c"),
        Err(PtyError::InputQueueFull {
            pane_id,
            queued_byte_count: MAX_QUEUED_PANE_INPUT_BYTE_COUNT,
        })
    );
    assert_eq!(
        queued_input_byte_count.load(Ordering::SeqCst),
        MAX_QUEUED_PANE_INPUT_BYTE_COUNT,
        "refused input is not counted"
    );

    release_sender.send(()).expect("let the first write land");
    release_sender.send(()).expect("let the second write land");
    backend.flush_writers().expect("the flush answers");
    assert_eq!(queued_input_byte_count.load(Ordering::SeqCst), 0);
    release_sender.send(()).expect("let the next write land");
    backend
        .write_pane_input(pane_id, b"d")
        .expect("input written out frees its room");
    backend.flush_writers().expect("the flush answers");
    let written_bytes = written_bytes.lock().expect("written bytes");
    assert_eq!(written_bytes.len(), MAX_QUEUED_PANE_INPUT_BYTE_COUNT + 1);
    assert_eq!(
        &written_bytes[MAX_QUEUED_PANE_INPUT_BYTE_COUNT - 1..],
        b"bd"
    );
}

#[cfg(unix)]
#[test]
fn input_for_a_writer_that_has_ended_is_refused_and_not_counted() {
    let backend = PortablePtyBackend::with_pty_sink(CountingSink::new());
    let pane_id = PaneId::new();
    let queued_input_byte_count = Arc::new(AtomicUsize::new(0));
    let (writer_sender, writer_receiver) = channel::<WriterMessage>();
    drop(writer_receiver);
    backend.pane_by_id.lock().expect("panes").insert(
        pane_id,
        build_pane_entry(
            Terminal::Master(Arc::new(Mutex::new(None))),
            writer_sender,
            Arc::clone(&queued_input_byte_count),
        ),
    );

    assert_eq!(
        backend.write_pane_input(pane_id, b"typed"),
        Err(PtyError::Io {
            detail: "sending on a closed channel".to_string(),
        })
    );
    assert_eq!(queued_input_byte_count.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[test]
fn setting_close_on_exec_touches_only_that_one_flag() {
    // `FD_CLOEXEC` is the only descriptor flag; the whole flag word is read
    // back.
    use std::os::fd::AsRawFd;

    let (_peer_socket, terminal_socket) =
        std::os::unix::net::UnixStream::pair().expect("terminal pair");
    let terminal_fd = std::os::fd::OwnedFd::from(terminal_socket);
    let terminal_file_descriptor = terminal_fd.as_raw_fd();
    // SAFETY: `F_GETFD` reads no memory, and `terminal_fd` keeps the
    // descriptor open for the whole test.
    let descriptor_flags = || unsafe { libc::fcntl(terminal_file_descriptor, libc::F_GETFD) };

    set_terminal_cloexec(terminal_file_descriptor, false).expect("clear close-on-exec");
    assert_eq!(
        descriptor_flags(),
        0,
        "clearing must leave no descriptor flag set"
    );

    set_terminal_cloexec(terminal_file_descriptor, true).expect("set close-on-exec");
    assert_eq!(
        descriptor_flags(),
        libc::FD_CLOEXEC,
        "setting must leave close-on-exec and nothing else"
    );

    set_terminal_cloexec(terminal_file_descriptor, true).expect("set close-on-exec again");
    assert_eq!(
        descriptor_flags(),
        libc::FD_CLOEXEC,
        "setting a flag that is already set must change nothing"
    );
}

#[cfg(unix)]
#[test]
fn a_carried_pane_reports_the_size_its_terminal_was_last_set_to() {
    // A carried pane reports the size of its last resize, names its child's
    // running process, and carries the pane's own terminal descriptor.
    use std::os::fd::AsRawFd;

    let pty_sink = CountingSink::new();
    let (backend, pane_id) = start_backend_with_shell_script(pty_sink, "sleep 30");

    let resized_pty_size = PtySize {
        column_count: 132,
        row_count: 43,
    };
    backend
        .resize_pane(pane_id, resized_pty_size)
        .expect("resize the pane");

    let carried_panes = backend.list_carried_panes();
    assert_eq!(carried_panes.len(), 1, "one live pane must give one record");
    assert_eq!(
        carried_panes[0].pane_id, pane_id,
        "the record must name the pane"
    );
    assert_eq!(
        carried_panes[0].pty_size, resized_pty_size,
        "a carried pane must report the size it was last resized to"
    );
    assert!(
        is_process_alive(carried_panes[0].process_id),
        "a carried pane must name its child's running process"
    );

    // The window size read back through the carried descriptor is the resized
    // one.
    let terminal_file_descriptor = carried_panes[0]
        .terminal_fd
        .expect("a real pty exposes a descriptor");
    // SAFETY: an all-zero `winsize` is valid.
    let mut window_size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: `TIOCGWINSZ` writes one `winsize` to `window_size`, which lives
    // for the call.
    let read_window_result = unsafe {
        libc::ioctl(
            terminal_file_descriptor,
            libc::TIOCGWINSZ as _,
            &mut window_size,
        )
    };
    assert_eq!(read_window_result, 0, "read the window size back");
    assert_eq!(
        (window_size.ws_col, window_size.ws_row),
        (132, 43),
        "the carried descriptor must name the pane's own terminal"
    );
    assert_eq!(
        terminal_file_descriptor,
        match &backend.pane_by_id.lock().expect("panes")[&pane_id].terminal {
            Terminal::Owned(owned) => owned.as_raw_fd(),
            Terminal::Master(_) => panic!("a real pty must own its descriptor"),
        },
        "the record must carry the pane's own descriptor"
    );

    backend
        .kill_pane(pane_id, KillPolicy::Tree)
        .expect("close the pane");
}

#[cfg(unix)]
#[test]
fn a_paused_panes_terminal_can_be_taken_back_by_another_backend() {
    // The swap in one process: pause, carry the descriptor and the process id,
    // take the pane back. The taken-back pane drives the same child.
    let old_pty_sink = CountingSink::new();
    let (old_backend, pane_id) =
        start_backend_with_shell_script(old_pty_sink.clone(), "printf ready; sleep 30");

    read_output_until_contains(&old_pty_sink, b"ready");
    let output_before_swap = old_pty_sink.collect_output_bytes();
    pause_readers_or_fail(&old_backend).expect("pause the readers");

    let carried_panes = old_backend.list_carried_panes();
    assert_eq!(carried_panes.len(), 1, "one live pane must give one record");
    let carried_pane = carried_panes[0];
    assert_eq!(
        carried_pane.pane_id, pane_id,
        "the record must name the pane"
    );
    let carried_terminal_fd = duplicate_terminal_for_process_swap(
        carried_pane
            .terminal_fd
            .expect("a real pty exposes a descriptor"),
    );

    let new_pty_sink = CountingSink::new();
    let new_backend = PortablePtyBackend::with_pty_sink(new_pty_sink.clone());
    new_backend
        .adopt(
            pane_id,
            carried_terminal_fd,
            carried_pane.process_id,
            carried_pane.pty_size,
            carried_pane.exit_status,
        )
        .expect("take the pane back");

    new_backend
        .write_pane_input(pane_id, b"adopted\n")
        .expect("write to the pane");
    read_output_until_contains(&new_pty_sink, b"adopted");
    assert_eq!(
        old_pty_sink.collect_output_bytes(),
        output_before_swap,
        "the parked reader must take none of the bytes the new pane is owed"
    );

    // Closing the old pane kills the child both panes drive, and the taken-back
    // pane's threads end with it.
    old_backend.resume_readers();
    old_backend
        .kill_pane(pane_id, KillPolicy::Tree)
        .expect("close the pane");
}

#[cfg(unix)]
#[test]
fn a_pane_taken_back_after_its_child_ended_still_publishes_the_exit() {
    // The child has ended, unreaped, before the pane is taken back. The
    // taken-back pane's watcher reads `ExitCode(7)` from the zombie and keeps
    // it, and closing the pane reaps it.
    let (terminal_fd, child_process_id) = {
        let _pty_gate_guard = PTY_GATE.lock().expect("pty gate");
        let pty_pair = native_pty_system()
            .openpty(build_portable_pty_size(STANDARD_PTY_SIZE))
            .expect("openpty");
        let terminal_fd =
            duplicate_terminal_file_descriptor(&*pty_pair.master).expect("terminal descriptor");
        let mut command_builder = CommandBuilder::new("/bin/sh");
        command_builder.arg("-c");
        command_builder.arg("exit 7");
        let child_process = pty_pair
            .slave
            .spawn_command(command_builder)
            .expect("spawn");
        drop(pty_pair.slave);
        let child_process_id = child_process.process_id().expect("process id");
        // Left unreaped.
        drop(child_process);
        (terminal_fd, child_process_id)
    };

    let pty_sink = CountingSink::new();
    let backend = PortablePtyBackend::with_pty_sink(pty_sink.clone());
    let pane_id = PaneId::new();
    backend
        .adopt(
            pane_id,
            terminal_fd,
            child_process_id,
            STANDARD_PTY_SIZE,
            None,
        )
        .expect("take the pane back");

    assert_eq!(
        read_exit_status_or_fail(
            &pty_sink,
            "a taken-back pane never published its child's exit",
        ),
        ExitStatus::ExitCode(7),
        "the exit the child really ended with must reach the consumer"
    );
    let child_wait_process_id =
        libc::pid_t::try_from(child_process_id).expect("a child process id fits pid_t");
    assert_eq!(
        read_child_wait_state(child_process_id),
        Ok(child_wait_process_id),
        "the ended child stays a zombie while the pane is open"
    );

    backend
        .kill_pane(pane_id, KillPolicy::Force)
        .expect("close the pane");
    assert_eq!(
        read_child_wait_state(child_process_id),
        Err(libc::ECHILD),
        "closing the pane reaps the child"
    );
}

#[cfg(unix)]
#[test]
fn a_child_that_ends_while_the_readers_are_held_keeps_the_code_it_ended_with() {
    // The child ends while the readers are held. The old backend's watcher
    // keeps it a zombie, the carried record holds no status, and the
    // taken-back pane reads `ExitCode(3)` from the zombie and publishes it once.
    let old_pty_sink = CountingSink::new();
    // The child prints, then ends after one second.
    let (old_backend, pane_id) =
        start_backend_with_shell_script(old_pty_sink.clone(), "printf ready; sleep 1; exit 3");

    read_output_until_contains(&old_pty_sink, b"ready");
    pause_readers_or_fail(&old_backend).expect("hold the readers still");

    // The old backend's watcher sees the child end while its reader is held.
    wait_for_condition(
        || {
            old_backend.pane_by_id.lock().expect("panes")[&pane_id]
                .child_exit
                .is_zombie_kept()
        },
        "the held pane's watcher never saw its child end",
    );
    let carried_panes = old_backend.list_carried_panes();
    assert_eq!(carried_panes.len(), 1, "one live pane must give one record");
    let carried_pane = carried_panes[0];
    assert_eq!(
        carried_pane.pane_id, pane_id,
        "the record must name the pane"
    );
    assert_eq!(
        carried_pane.exit_status, None,
        "a child kept as a zombie carries no status: the next image reads it"
    );

    let carried_terminal_fd = duplicate_terminal_for_process_swap(
        carried_pane
            .terminal_fd
            .expect("a real pty exposes a descriptor"),
    );

    let new_pty_sink = CountingSink::new();
    let new_backend = PortablePtyBackend::with_pty_sink(new_pty_sink.clone());
    new_backend
        .adopt(
            pane_id,
            carried_terminal_fd,
            carried_pane.process_id,
            carried_pane.pty_size,
            carried_pane.exit_status,
        )
        .expect("take the pane back");

    assert_eq!(
        read_exit_status_or_fail(
            &new_pty_sink,
            "the taken-back pane never published its child's exit"
        ),
        ExitStatus::ExitCode(3),
        "the taken-back pane must report the code the child really ended with"
    );
    assert_eq!(
        new_pty_sink.get_exit_status_count(),
        1,
        "and it must report it exactly once"
    );

    old_backend.resume_readers();
    old_backend
        .kill_pane(pane_id, KillPolicy::Tree)
        .expect("close the pane");
}

#[cfg(unix)]
#[test]
fn holding_the_readers_still_twice_settles_both_times_and_one_release_frees_them() {
    // Two pauses in a row both settle; the one reader parks once. One resume
    // frees it.
    let pty_sink = CountingSink::new();
    let (backend, pane_id) =
        start_backend_with_shell_script(pty_sink.clone(), "printf ready; sleep 30");

    read_output_until_contains(&pty_sink, b"ready");
    let output_before_pause = pty_sink.collect_output_bytes();

    pause_readers_or_fail(&backend).expect("hold the readers still");
    pause_readers_or_fail(&backend).expect("hold the readers still a second time");

    assert!(
        backend
            .reader_gate
            .gate_state
            .lock()
            .expect("reader gate")
            .is_paused,
        "the gate stays shut after the second hold"
    );
    assert_eq!(
        backend
            .reader_gate
            .gate_state
            .lock()
            .expect("reader gate")
            .parked_reader_count,
        1,
        "the one reader parks once, however many holds asked for it"
    );
    assert_eq!(
        backend
            .reader_gate
            .gate_state
            .lock()
            .expect("reader gate")
            .reader_count,
        1,
        "and it is still the one counted reader"
    );

    backend
        .write_pane_input(pane_id, b"held\n")
        .expect("write to the pane");
    thread::sleep(EXIT_PUBLISH_GRACE_DURATION * 3);
    assert_eq!(
        pty_sink.collect_output_bytes(),
        output_before_pause,
        "a reader held twice hands the consumer nothing"
    );

    backend.resume_readers();
    let output_after_resume = read_output_until_contains(&pty_sink, b"held");
    assert_eq!(
        &output_after_resume[..output_before_pause.len()],
        output_before_pause.as_slice(),
        "one release must free a reader two holds parked"
    );
    assert_eq!(
        backend
            .reader_gate
            .gate_state
            .lock()
            .expect("reader gate")
            .parked_reader_count,
        0,
        "and must leave nobody at the park"
    );

    backend
        .kill_pane(pane_id, KillPolicy::Tree)
        .expect("close the pane");
}

#[cfg(unix)]
#[test]
fn releasing_readers_nobody_held_leaves_a_live_pane_printing() {
    // A resume with nobody parked leaves the gate open and the pane printing.
    let pty_sink = CountingSink::new();
    let (backend, pane_id) =
        start_backend_with_shell_script(pty_sink.clone(), "printf ready; sleep 30");

    read_output_until_contains(&pty_sink, b"ready");
    backend.resume_readers();

    assert!(
        !backend
            .reader_gate
            .gate_state
            .lock()
            .expect("reader gate")
            .is_paused,
        "the gate was never shut and stays open"
    );
    assert_eq!(
        backend
            .reader_gate
            .gate_state
            .lock()
            .expect("reader gate")
            .parked_reader_count,
        0,
        "and holds nobody"
    );

    backend
        .write_pane_input(pane_id, b"still here\n")
        .expect("write to the pane");
    read_output_until_contains(&pty_sink, b"still here");

    backend
        .kill_pane(pane_id, KillPolicy::Tree)
        .expect("close the pane");
}

#[cfg(unix)]
#[test]
fn a_child_that_ends_while_its_reader_is_held_publishes_its_exit_once_on_release() {
    // The child ends while the reader is parked. No exit is published while
    // held; the resume publishes `ExitCode(3)` exactly once.
    let pty_sink = CountingSink::new();
    let (backend, pane_id) =
        start_backend_with_shell_script(pty_sink.clone(), "printf ready; sleep 2; exit 3");

    read_output_until_contains(&pty_sink, b"ready");
    pause_readers_or_fail(&backend).expect("hold the readers still");
    assert_eq!(
        backend
            .reader_gate
            .gate_state
            .lock()
            .expect("reader gate")
            .parked_reader_count,
        1,
        "the reader parks well before its child's two-second sleep runs out"
    );

    wait_for_condition(
        || {
            backend.pane_by_id.lock().expect("panes")[&pane_id]
                .child_exit
                .has_exited()
        },
        "the pane's child never ended while its reader was held",
    );

    assert_eq!(
        pty_sink.get_exit_status_count(),
        0,
        "a held reader must publish no exit, however long its child has been gone"
    );
    assert_eq!(
        backend
            .reader_gate
            .gate_state
            .lock()
            .expect("reader gate")
            .parked_reader_count,
        1,
        "and the reader must still be at the park"
    );

    backend.resume_readers();

    assert_eq!(
        read_exit_status_or_fail(
            &pty_sink,
            "the released reader never published the exit it was holding"
        ),
        ExitStatus::ExitCode(3),
        "the status the child really ended with must reach the consumer"
    );

    // The wait gives a second publish time to arrive.
    thread::sleep(EXIT_PUBLISH_GRACE_DURATION * 3);
    assert_eq!(
        pty_sink.get_exit_status_count(),
        1,
        "and it must arrive exactly once"
    );
}

#[cfg(unix)]
#[test]
fn input_written_while_the_readers_are_held_reaches_the_child_and_its_answer_comes_on_release() {
    // Input written while the readers are held reaches the child, and the
    // child answers. The answer and the exit reach the consumer on resume.
    let pty_sink = CountingSink::new();
    let (backend, pane_id) = start_backend_with_shell_script(
        pty_sink.clone(),
        "printf ready; read go; printf 'took %s' \"$go\"; exit 3",
    );

    read_output_until_contains(&pty_sink, b"ready");
    let output_before_pause = pty_sink.collect_output_bytes();
    pause_readers_or_fail(&backend).expect("hold the readers still");

    backend
        .write_pane_input(pane_id, b"go\n")
        .expect("write to the pane");
    // The writers settle while the readers are held.
    backend
        .flush_writers()
        .expect("the held readers must leave the writers free to settle");

    thread::sleep(EXIT_PUBLISH_GRACE_DURATION * 3);
    assert_eq!(
        pty_sink.collect_output_bytes(),
        output_before_pause,
        "a held reader hands the consumer nothing, however much its child printed"
    );
    assert_eq!(
        pty_sink.get_exit_status_count(),
        0,
        "and publishes no exit while it is held"
    );

    backend.resume_readers();

    assert_eq!(
        read_exit_status_or_fail(
            &pty_sink,
            "the released reader never published the exit it was holding"
        ),
        ExitStatus::ExitCode(3),
        "the child read the line and ended with the status that line asks for"
    );
    assert_eq!(
        String::from_utf8_lossy(&pty_sink.collect_output_bytes()).into_owned(),
        "readygo\r\ntook go",
        "the release hands over the echo of the input and what the child printed from it"
    );
}

/// The numbers a bursting child printed, in the order they reached a sink.
/// Panics on a non-empty line that is not one number.
#[cfg(unix)]
fn parse_printed_numbers(output_bytes: &[u8]) -> Vec<u32> {
    String::from_utf8_lossy(output_bytes)
        .split('\n')
        .map(|line| line.trim_matches(['\r', '\0']))
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.parse::<u32>()
                .unwrap_or_else(|_| panic!("a printed line must be one number, got {line:?}"))
        })
        .collect()
}

/// How many numbered lines the hand-over drives through a pane. More than a
/// terminal buffer holds: the child blocks against a held reader.
#[cfg(unix)]
const BURST_LINE_COUNT: u32 = 1500;

#[cfg(unix)]
#[test]
fn every_byte_a_child_is_printing_crosses_the_hand_over_once_and_in_order() {
    // The child is mid-burst when the readers are held and the pane is taken
    // back. The two sinks end to end hold every line once, in order.
    let old_pty_sink = CountingSink::new();
    let (old_backend, pane_id) = start_backend_with_shell_script(
        old_pty_sink.clone(),
        &format!("i=1; while [ $i -le {BURST_LINE_COUNT} ]; do printf '%04d\\n' $i; i=$((i+1)); done; sleep 30"),
    );

    // The child is printing by the time the first line lands.
    read_output_until_contains(&old_pty_sink, b"0001");
    pause_readers_or_fail(&old_backend).expect("hold the readers still");
    let output_before_swap = old_pty_sink.collect_output_bytes();

    let carried_panes = old_backend.list_carried_panes();
    assert_eq!(carried_panes.len(), 1, "one live pane must give one record");
    let carried_pane = carried_panes[0];
    assert_eq!(
        carried_pane.pane_id, pane_id,
        "the record must name the pane"
    );
    let carried_terminal_fd = duplicate_terminal_for_process_swap(
        carried_pane
            .terminal_fd
            .expect("a real pty exposes a descriptor"),
    );

    let new_pty_sink = CountingSink::new();
    let new_backend = PortablePtyBackend::with_pty_sink(new_pty_sink.clone());
    new_backend
        .adopt(
            pane_id,
            carried_terminal_fd,
            carried_pane.process_id,
            carried_pane.pty_size,
            carried_pane.exit_status,
        )
        .expect("take the pane back");

    // The two sinks end to end are everything the consumer was handed.
    // `output_before_swap` never grows.
    let final_output_line = format!("{BURST_LINE_COUNT:04}");
    let combined_output_bytes = {
        let output_deadline = Instant::now() + HANG_GUARD_DURATION * 4;
        loop {
            let mut combined_output_bytes = output_before_swap.clone();
            combined_output_bytes.extend_from_slice(&new_pty_sink.collect_output_bytes());
            if combined_output_bytes
                .windows(final_output_line.len())
                .any(|output_window| output_window == final_output_line.as_bytes())
            {
                break combined_output_bytes;
            }
            assert!(
                Instant::now() < output_deadline,
                "the child's last line never reached the consumer; it holds {} bytes",
                combined_output_bytes.len()
            );
            thread::sleep(Duration::from_millis(5));
        }
    };

    assert_eq!(
        old_pty_sink.collect_output_bytes(),
        output_before_swap,
        "the held reader must take none of the bytes the taken-back pane is owed"
    );

    let printed_output_numbers = parse_printed_numbers(&combined_output_bytes);
    assert_eq!(
        printed_output_numbers,
        (1..=BURST_LINE_COUNT).collect::<Vec<u32>>(),
        "every line the child printed must cross the hand-over once, in the order it printed them"
    );

    old_backend.resume_readers();
    old_backend
        .kill_pane(pane_id, KillPolicy::Tree)
        .expect("close the pane");
}

#[cfg(unix)]
#[test]
fn a_pane_taken_back_on_a_terminal_already_at_its_end_reports_its_child_gone() {
    // The terminal is already at its end and the process id is nobody's child.
    // The taken-back pane publishes `ExitCode(-1)` once and hands over no
    // output.
    let (peer_socket, terminal_fd) = build_fake_terminal();
    drop(peer_socket); // the terminal is already at its end

    let pty_sink = CountingSink::new();
    let backend = PortablePtyBackend::with_pty_sink(pty_sink.clone());
    let pane_id = PaneId::new();

    // A process id this process has no child under: `waitid` answers
    // `ECHILD`.
    backend
        .adopt(
            pane_id,
            terminal_fd,
            u32::from(u16::MAX),
            STANDARD_PTY_SIZE,
            None,
        )
        .expect("take the pane back");

    assert_eq!(
        read_exit_status_or_fail(
            &pty_sink,
            "a pane taken back on a dead terminal never reported its child gone"
        ),
        ExitStatus::ExitCode(-1),
        "a child that cannot be waited on reports the unobserved status"
    );
    assert_eq!(
        pty_sink.get_exit_status_count(),
        1,
        "and it reports it exactly once"
    );
    assert_eq!(
        pty_sink.get_output_chunk_count(),
        0,
        "a dead terminal hands over no output"
    );
}

#[cfg(unix)]
#[test]
fn a_descriptor_this_process_does_not_hold_refuses_the_close_on_exec_change() {
    // A descriptor number this process does not hold refuses both the set and
    // the clear with `EBADF`.
    let unopened_file_descriptor = libc::c_int::MAX;
    // SAFETY: `F_GETFD` reads no memory, and a descriptor that is not open
    // gives `-1`.
    let descriptor_flags = unsafe { libc::fcntl(unopened_file_descriptor, libc::F_GETFD) };
    assert_eq!(
        descriptor_flags, -1,
        "the descriptor under test must not be open"
    );

    let set_cloexec_error = set_terminal_cloexec(unopened_file_descriptor, true)
        .expect_err("a descriptor this process does not hold must refuse the change");
    assert_eq!(
        set_cloexec_error.raw_os_error(),
        Some(libc::EBADF),
        "and must name the descriptor as bad"
    );

    let clear_cloexec_error = set_terminal_cloexec(unopened_file_descriptor, false)
        .expect_err("clearing the flag must refuse it the same way");
    assert_eq!(clear_cloexec_error.raw_os_error(), Some(libc::EBADF));
}

// --- the terminal's opening cursor question ---

/// A terminal that hands back one of `output_chunks` per read, then reports its end.
/// A read never spans two chunks.
struct ChunkedTerminal {
    /// What each read hands back, in order.
    output_chunks: Vec<Vec<u8>>,
    /// How many chunks have been read.
    read_chunk_index: usize,
}

impl ChunkedTerminal {
    fn from_output_chunks(output_chunks: &[&[u8]]) -> ChunkedTerminal {
        ChunkedTerminal {
            output_chunks: output_chunks
                .iter()
                .map(|output_chunk| output_chunk.to_vec())
                .collect(),
            read_chunk_index: 0,
        }
    }
}

impl Read for ChunkedTerminal {
    fn read(&mut self, read_buffer: &mut [u8]) -> std::io::Result<usize> {
        let Some(output_chunk) = self.output_chunks.get(self.read_chunk_index) else {
            return Ok(0);
        };
        self.read_chunk_index += 1;
        let read_byte_count = output_chunk.len().min(read_buffer.len());
        read_buffer[..read_byte_count].copy_from_slice(&output_chunk[..read_byte_count]);
        Ok(read_byte_count)
    }
}

/// Read `output_chunks` through the reader that takes the request out, and hand back
/// what it delivered as output.
fn read_output_without_cursor_request(output_chunks: &[&[u8]]) -> Vec<u8> {
    read_delivered_output_to_end(&mut RemovesCursorRequest::from_inner_reader(
        ChunkedTerminal::from_output_chunks(output_chunks),
    ))
}

/// Read `reader` through 64-byte reads until a read gives `0`, and hand back
/// every byte it delivered.
fn read_delivered_output_to_end(reader: &mut impl Read) -> Vec<u8> {
    let mut delivered_output_bytes = Vec::new();
    let mut read_buffer = [0u8; 64];
    loop {
        match reader
            .read(&mut read_buffer)
            .expect("the terminal hands back its bytes")
        {
            0 => break,
            read_byte_count => {
                delivered_output_bytes.extend_from_slice(&read_buffer[..read_byte_count])
            }
        }
    }
    delivered_output_bytes
}

#[test]
fn an_empty_destination_buffer_does_not_end_the_request_watch() {
    // A read into no room reads no bytes; the request is still taken out of
    // the output that follows.
    let mut reader =
        RemovesCursorRequest::from_inner_reader(ChunkedTerminal::from_output_chunks(&[
            CURSOR_POSITION_REQUEST_BYTES,
            b"hello",
        ]));

    assert_eq!(reader.read(&mut []).expect("no room reads nothing"), 0);

    assert_eq!(read_delivered_output_to_end(&mut reader), b"hello");
}

#[test]
fn the_terminals_opening_question_is_taken_out_of_the_output() {
    assert_eq!(
        read_output_without_cursor_request(&[CURSOR_POSITION_REQUEST_BYTES, b"hello"]),
        b"hello"
    );
}

#[test]
fn an_opening_question_split_across_two_reads_is_taken_out_once() {
    assert_eq!(
        read_output_without_cursor_request(&[b"\x1b[", b"6nhello"]),
        b"hello"
    );
}

#[test]
fn an_opening_question_with_nothing_behind_it_ends_the_output() {
    assert_eq!(read_output_without_cursor_request(&[b"\x1b[6", b"n"]), b"");
}

#[test]
fn output_that_is_not_the_question_is_delivered_whole() {
    assert_eq!(
        read_output_without_cursor_request(&[b"hello world"]),
        b"hello world"
    );
}

#[test]
fn a_second_cursor_position_request_from_the_program_is_left_for_the_terminal_engine() {
    // The second request on the stream: the first was the terminal's and is
    // already taken out.
    assert_eq!(
        read_output_without_cursor_request(&[
            CURSOR_POSITION_REQUEST_BYTES,
            b"hello",
            CURSOR_POSITION_REQUEST_BYTES
        ]),
        b"hello\x1b[6n"
    );
}

#[test]
fn the_cursor_position_request_is_taken_out_behind_earlier_output() {
    // Output ahead of the request is delivered, and the request behind it is
    // taken out.
    assert_eq!(
        read_output_without_cursor_request(&[b"\x1b[?25l", CURSOR_POSITION_REQUEST_BYTES, b"hi"]),
        b"\x1b[?25lhi"
    );
}

#[test]
fn an_incomplete_cursor_position_request_at_terminal_end_is_delivered_as_output() {
    // Held back while it could still become the request, then delivered when
    // the terminal ends.
    assert_eq!(read_output_without_cursor_request(&[b"\x1b["]), b"\x1b[");
}

#[test]
fn a_cursor_position_request_prefix_that_does_not_match_is_delivered() {
    // `\x1b[` is held back, then delivered once `?25h` shows it is not the
    // request.
    assert_eq!(
        read_output_without_cursor_request(&[b"\x1b[", b"?25hhi"]),
        b"\x1b[?25hhi"
    );
}

#[test]
fn cursor_position_request_watch_outlasts_a_false_start() {
    assert_eq!(
        read_output_without_cursor_request(&[
            b"\x1b[",
            b"?25h",
            CURSOR_POSITION_REQUEST_BYTES,
            b"hi",
        ]),
        b"\x1b[?25hhi"
    );
}

#[test]
fn cursor_position_request_split_across_three_reads_is_taken_out_once() {
    assert_eq!(
        read_output_without_cursor_request(&[b"\x1b", b"[6", b"nhi"]),
        b"hi"
    );
}

#[test]
fn find_subslice_position_finds_only_the_first_cursor_position_request() {
    assert_eq!(
        find_subslice_position(b"ab\x1b[6ncd\x1b[6n", CURSOR_POSITION_REQUEST_BYTES),
        Some(2)
    );
    assert_eq!(
        find_subslice_position(b"abc", CURSOR_POSITION_REQUEST_BYTES),
        None
    );
    assert_eq!(
        find_subslice_position(b"", CURSOR_POSITION_REQUEST_BYTES),
        None
    );
}

#[test]
fn compute_partial_tail_length_counts_the_longest_request_prefix_at_the_end() {
    assert_eq!(
        compute_partial_tail_length(b"hello\x1b[", CURSOR_POSITION_REQUEST_BYTES),
        2
    );
    assert_eq!(
        compute_partial_tail_length(b"\x1b[6", CURSOR_POSITION_REQUEST_BYTES),
        3
    );
    assert_eq!(
        compute_partial_tail_length(b"\x1b", CURSOR_POSITION_REQUEST_BYTES),
        1
    );
    assert_eq!(
        compute_partial_tail_length(b"hello", CURSOR_POSITION_REQUEST_BYTES),
        0
    );
    assert_eq!(
        compute_partial_tail_length(b"", CURSOR_POSITION_REQUEST_BYTES),
        0
    );
}

#[test]
fn compute_partial_tail_length_never_counts_the_complete_request() {
    assert_eq!(
        compute_partial_tail_length(CURSOR_POSITION_REQUEST_BYTES, CURSOR_POSITION_REQUEST_BYTES),
        0
    );
    assert_eq!(
        compute_partial_tail_length(b"hi\x1b[6n", CURSOR_POSITION_REQUEST_BYTES),
        0
    );
}

// --- what a pane records, and what it answers about its child ---

#[test]
fn a_pane_this_backend_does_not_hold_is_refused_by_every_call() {
    let backend = PortablePtyBackend::with_pty_sink(CountingSink::new());
    let pane_id = PaneId::new();

    assert_eq!(
        backend.write_pane_input(pane_id, b"typed"),
        Err(PtyError::UnknownPane { pane_id })
    );
    assert_eq!(
        backend.resize_pane(
            pane_id,
            PtySize {
                column_count: 80,
                row_count: 24
            }
        ),
        Err(PtyError::UnknownPane { pane_id })
    );
    assert_eq!(
        backend.kill_pane(pane_id, KillPolicy::Force),
        Err(PtyError::UnknownPane { pane_id })
    );
    assert_eq!(backend.get_child_process_id(pane_id), None);
    assert_eq!(backend.find_live_working_directory(pane_id), None);
}

#[test]
fn a_backend_with_no_panes_carries_nothing_and_flushes_at_once() {
    let backend = PortablePtyBackend::with_pty_sink(CountingSink::new());

    assert_eq!(backend.list_carried_panes(), Vec::new());

    let flush_started_at = Instant::now();
    assert_eq!(backend.flush_writers(), Ok(()));
    assert!(
        flush_started_at.elapsed() < WRITER_FLUSH_LIMIT_DURATION,
        "a flush with no writer to wait for must not spend the limit"
    );
}

#[cfg(unix)]
#[test]
fn pausing_a_backend_with_no_readers_settles_at_once() {
    let backend = Arc::new(PortablePtyBackend::with_pty_sink(CountingSink::new()));

    assert_eq!(pause_readers_or_fail(&backend), Ok(()));
    assert!(
        backend
            .reader_gate
            .gate_state
            .lock()
            .expect("reader gate")
            .is_paused,
        "the gate is shut after the pause"
    );

    backend.resume_readers();
    assert!(
        !backend
            .reader_gate
            .gate_state
            .lock()
            .expect("reader gate")
            .is_paused,
        "the gate is open after the resume"
    );
}

#[cfg(unix)]
#[test]
fn a_reader_passes_an_open_gate_without_parking() {
    let reader_gate = ReaderGate::new();

    reader_gate.park_if_paused();

    let gate_state = reader_gate.gate_state.lock().expect("reader gate");
    assert_eq!(
        (
            gate_state.is_paused,
            gate_state.parked_reader_count,
            gate_state.reader_count
        ),
        (false, 0, 0)
    );
}

#[cfg(unix)]
#[test]
fn a_reader_that_leaves_its_pump_settles_a_pause_waiting_for_it() {
    // One reader is counted and never parks. The pause waits until that
    // reader's ticket is dropped.
    let reader_gate = Arc::new(ReaderGate::new());
    let reader_ticket = reader_gate.register_reader();
    reader_gate.pause_readers();

    let (completion_sender, completion_receiver) = channel::<()>();
    let waiting_reader_gate = Arc::clone(&reader_gate);
    thread::spawn(move || {
        waiting_reader_gate.wait_until_all_readers_parked();
        let _ = completion_sender.send(());
    });

    assert_eq!(
        completion_receiver.recv_timeout(Duration::from_millis(100)),
        Err(RecvTimeoutError::Timeout),
        "a pause must wait for a counted reader that has not parked"
    );
    drop(reader_ticket);
    assert_eq!(
        completion_receiver.recv_timeout(HANG_GUARD_DURATION),
        Ok(()),
        "a reader that left its pump must settle the pause"
    );
    assert_eq!(
        reader_gate
            .gate_state
            .lock()
            .expect("reader gate")
            .reader_count,
        0,
        "a dropped ticket counts its reader out"
    );
}

#[cfg(unix)]
#[test]
// `reap_child` is the reap under test.
#[expect(clippy::zombie_processes)]
fn wait_for_child_exit_reports_the_code_and_keeps_the_child_until_it_is_reaped() {
    let child_process = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 5"])
        .spawn()
        .expect("spawn");
    let child_process_id = child_process.id();
    let child_wait_process_id =
        libc::pid_t::try_from(child_process_id).expect("a child process id fits pid_t");

    assert_eq!(
        wait_for_child_exit(child_process_id),
        ObservedChildExit {
            exit_status: ExitStatus::ExitCode(5),
            is_zombie_kept: true,
        }
    );
    assert_eq!(
        read_child_wait_state(child_process_id),
        Ok(child_wait_process_id),
        "the ended child stays a zombie"
    );

    reap_child(child_process_id);
    assert_eq!(
        read_child_wait_state(child_process_id),
        Err(libc::ECHILD),
        "a reaped child is gone"
    );
}

#[cfg(unix)]
#[test]
// `reap_child` is the reap under test.
#[expect(clippy::zombie_processes)]
fn wait_for_child_exit_reports_the_number_of_the_signal_that_ended_the_child() {
    let child_process = std::process::Command::new("/bin/sh")
        .args(["-c", "kill -TERM $$"])
        .spawn()
        .expect("spawn");
    let child_process_id = child_process.id();

    assert_eq!(
        wait_for_child_exit(child_process_id),
        ObservedChildExit {
            exit_status: ExitStatus::Signaled(libc::SIGTERM),
            is_zombie_kept: true,
        }
    );
    reap_child(child_process_id);
}

#[cfg(unix)]
#[test]
fn wait_for_child_exit_reports_an_unobserved_exit_for_a_process_that_is_not_a_child() {
    assert_eq!(
        wait_for_child_exit(std::process::id()),
        ObservedChildExit {
            exit_status: UNOBSERVED_EXIT,
            is_zombie_kept: false,
        }
    );
}

#[test]
fn a_child_left_running_hands_the_reap_of_its_zombie_to_the_watcher() {
    let child_exit = ChildExit::default();

    assert!(
        child_exit.leave_child_running(),
        "a child with no recorded exit is left running"
    );
    assert!(
        child_exit.publish_observed_exit(build_observed_exit(ExitStatus::ExitCode(0))),
        "the exit published afterwards tells the watcher to reap"
    );
}

#[test]
fn a_child_whose_exit_was_recorded_first_is_not_left_running() {
    let child_exit = ChildExit::default();

    assert!(
        !child_exit.publish_observed_exit(build_observed_exit(ExitStatus::ExitCode(0))),
        "the watcher does not reap a child the closing call waits for"
    );
    assert!(
        !child_exit.leave_child_running(),
        "a recorded exit stays with the closing call"
    );
    assert_eq!(
        *child_exit.lock_exit_record(),
        ChildExitRecord {
            observed_exit: Some(build_observed_exit(ExitStatus::ExitCode(0))),
            is_left_running: false,
        }
    );
}

/// A [`WatchRelease`] for the child `child_process_id` that `child_exit`
/// records, whose status and writer channels have no receiver, and whose
/// backend's set of children left running is `left_running_child_process_ids`.
#[cfg(unix)]
fn build_watch_release(
    child_exit: &Arc<ChildExit>,
    child_process_id: u32,
    left_running_child_process_ids: &Arc<Mutex<BTreeSet<u32>>>,
) -> WatchRelease {
    let (exit_status_sender, _) = channel();
    let (writer_stop_sender, _) = channel();
    WatchRelease {
        child_exit: Arc::clone(child_exit),
        exit_status_sender,
        writer_stop_sender,
        reader_waker: None,
        child_process_id,
        left_running_child_process_ids: Arc::clone(left_running_child_process_ids),
    }
}

#[cfg(unix)]
#[test]
// The zombie is reaped by the watcher's `publish_exit_status`.
#[expect(clippy::zombie_processes)]
fn the_watcher_reaps_the_zombie_of_a_child_left_running() {
    let child_process = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("spawn");
    let child_process_id = child_process.id();
    let left_running_child_process_ids = Arc::new(Mutex::new(BTreeSet::from([child_process_id])));
    let child_exit = Arc::new(ChildExit::default());
    assert!(child_exit.leave_child_running());
    let observed_exit = wait_for_child_exit(child_process_id);

    build_watch_release(
        &child_exit,
        child_process_id,
        &left_running_child_process_ids,
    )
    .publish_exit_status(observed_exit);

    assert_eq!(
        read_child_wait_state(child_process_id),
        Err(libc::ECHILD),
        "the watcher reaped the child"
    );
    assert_eq!(
        *left_running_child_process_ids.lock().unwrap(),
        BTreeSet::new(),
        "the reaped child left the set"
    );
}

#[cfg(unix)]
#[test]
// The zombie is reaped by the closing `reap_child` call.
#[expect(clippy::zombie_processes)]
fn the_watcher_keeps_the_zombie_of_a_child_the_closing_call_waits_for() {
    let child_process = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("spawn");
    let child_process_id = child_process.id();
    let child_wait_process_id =
        libc::pid_t::try_from(child_process_id).expect("a child process id fits pid_t");
    let child_exit = Arc::new(ChildExit::default());
    let observed_exit = wait_for_child_exit(child_process_id);
    let left_running_child_process_ids = Arc::new(Mutex::new(BTreeSet::new()));

    build_watch_release(
        &child_exit,
        child_process_id,
        &left_running_child_process_ids,
    )
    .publish_exit_status(observed_exit);

    assert_eq!(
        read_child_wait_state(child_process_id),
        Ok(child_wait_process_id),
        "the zombie stays for the closing call"
    );
    reap_child(child_process_id);
}

#[cfg(unix)]
#[test]
// The zombie is reaped by the thread `watch_left_running_children` starts.
#[expect(clippy::zombie_processes)]
fn a_child_an_earlier_image_left_running_is_listed_until_it_is_reaped() {
    let child_process = std::process::Command::new("/bin/sh")
        .args(["-c", "read line"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    let child_process_id = child_process.id();
    let child_stdin = child_process.stdin.expect("the child's stdin is piped");
    let backend = PortablePtyBackend::with_pty_sink(CountingSink::new());

    backend.watch_left_running_children(&[child_process_id]);

    assert_eq!(
        backend.list_left_running_child_process_ids(),
        vec![child_process_id],
        "a running child is listed for the next image"
    );
    drop(child_stdin);
    wait_until_no_child_is_left_running(&backend);
    assert_eq!(
        read_child_wait_state(child_process_id),
        Err(libc::ECHILD),
        "the child was reaped"
    );
}

#[cfg(unix)]
#[test]
fn an_id_that_names_no_child_leaves_the_list_of_children_left_running() {
    let backend = PortablePtyBackend::with_pty_sink(CountingSink::new());

    backend.watch_left_running_children(&[1]);

    wait_until_no_child_is_left_running(&backend);
}

/// Wait until `backend` lists no child left running. Fails the test once
/// [`HANG_GUARD_DURATION`] has passed.
#[cfg(unix)]
fn wait_until_no_child_is_left_running(backend: &PortablePtyBackend) {
    wait_for_condition(
        || backend.list_left_running_child_process_ids().is_empty(),
        "a child is still listed as left running",
    );
}

#[cfg(unix)]
#[test]
// The zombie is reaped by the second `reap_child` call.
#[expect(clippy::zombie_processes)]
fn reap_child_passes_over_zero_and_leaves_every_zombie_of_this_process_group() {
    let child_process = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("spawn");
    let child_process_id = child_process.id();
    let child_wait_process_id =
        libc::pid_t::try_from(child_process_id).expect("a child process id fits pid_t");
    wait_until_child_is_zombie(child_process_id);

    reap_child(0);
    assert_eq!(
        read_child_wait_state(child_process_id),
        Ok(child_wait_process_id),
        "`0` names this process group and reaps none of its children"
    );

    reap_child(child_process_id);
    assert_eq!(read_child_wait_state(child_process_id), Err(libc::ECHILD));
}

/// A pane taken back with a carried exit status, on the live terminal of a
/// pane `old_backend` runs, for the process `process_id`.
#[cfg(unix)]
fn adopt_pane_with_carried_exit(
    old_backend: &Arc<PortablePtyBackend>,
    old_pane_id: PaneId,
    process_id: u32,
) -> (PortablePtyBackend, PaneId) {
    let carried_pane = old_backend
        .list_carried_panes()
        .into_iter()
        .find(|carried_pane| carried_pane.pane_id == old_pane_id)
        .expect("the old pane is carried");
    let carried_terminal_fd = duplicate_terminal_for_process_swap(
        carried_pane
            .terminal_fd
            .expect("a real pty exposes a descriptor"),
    );
    let new_pty_sink = CountingSink::new();
    let new_backend = PortablePtyBackend::with_pty_sink(new_pty_sink.clone());
    let new_pane_id = PaneId::new();
    new_backend
        .adopt(
            new_pane_id,
            carried_terminal_fd,
            process_id,
            STANDARD_PTY_SIZE,
            Some(ExitStatus::ExitCode(3)),
        )
        .expect("take the pane back");
    assert_eq!(
        new_backend.pane_by_id.lock().expect("panes")[&new_pane_id]
            .child_exit
            .get_exit_status(),
        Some(ExitStatus::ExitCode(3)),
        "the carried status is recorded before the take-back returns"
    );
    assert_eq!(
        read_exit_status_or_fail(&new_pty_sink, "the carried exit was never published"),
        ExitStatus::ExitCode(3)
    );
    (new_backend, new_pane_id)
}

#[cfg(unix)]
#[test]
// The zombie is reaped at the end of the test.
#[expect(clippy::zombie_processes)]
fn closing_a_pane_whose_carried_child_an_earlier_image_reaped_reaps_nothing() {
    let old_pty_sink = CountingSink::new();
    let (old_backend, old_pane_id) =
        start_backend_with_shell_script(old_pty_sink.clone(), "printf ready; sleep 30");
    read_output_until_contains(&old_pty_sink, b"ready");
    // A zombie of this process stands in for a process id the carried status
    // names: closing the pane must leave it alone.
    let unrelated_process = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("spawn");
    let unrelated_process_id = unrelated_process.id();
    let unrelated_wait_process_id =
        libc::pid_t::try_from(unrelated_process_id).expect("a child process id fits pid_t");
    wait_until_child_is_zombie(unrelated_process_id);

    let (new_backend, new_pane_id) =
        adopt_pane_with_carried_exit(&old_backend, old_pane_id, unrelated_process_id);
    new_backend
        .kill_pane(new_pane_id, KillPolicy::Force)
        .expect("close the taken-back pane");

    assert_eq!(
        read_child_wait_state(unrelated_process_id),
        Ok(unrelated_wait_process_id),
        "a pane whose child an earlier image reaped reaps nothing"
    );
    reap_child(unrelated_process_id);
    old_backend
        .kill_pane(old_pane_id, KillPolicy::Tree)
        .expect("close the old pane");
}

#[cfg(unix)]
#[test]
fn closing_a_pane_whose_carried_child_an_earlier_image_reaped_signals_no_process_group() {
    for kill_policy in [
        KillPolicy::Tree,
        KillPolicy::GracefulTree {
            timeout_duration: Duration::from_secs(3),
        },
    ] {
        close_pane_with_carried_exit_beside_a_running_process_group(kill_policy);
    }
}

/// Close, with `kill_policy`, a pane taken back with a carried exit status
/// whose process id names a running process that leads its own process group,
/// and check that the group took no signal.
#[cfg(unix)]
fn close_pane_with_carried_exit_beside_a_running_process_group(kill_policy: KillPolicy) {
    use std::os::unix::process::{CommandExt, ExitStatusExt};

    let old_pty_sink = CountingSink::new();
    let (old_backend, old_pane_id) =
        start_backend_with_shell_script(old_pty_sink.clone(), "printf ready; sleep 30");
    read_output_until_contains(&old_pty_sink, b"ready");
    // A running process that leads its own process group stands in for a
    // process id that a stranger took after the earlier image reaped the child.
    let mut unrelated_process = std::process::Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .expect("spawn");
    let unrelated_process_id = unrelated_process.id();

    let (new_backend, new_pane_id) =
        adopt_pane_with_carried_exit(&old_backend, old_pane_id, unrelated_process_id);
    new_backend
        .kill_pane(new_pane_id, kill_policy)
        .expect("close the taken-back pane");

    let unrelated_signal_process_id =
        libc::pid_t::try_from(unrelated_process_id).expect("a child process id fits pid_t");
    // SAFETY: `kill` reads only its two integer arguments.
    let kill_result = unsafe { libc::kill(unrelated_signal_process_id, libc::SIGTERM) };
    assert_eq!(
        kill_result, 0,
        "the stand-in is still a process of this test"
    );
    let stand_in_exit_status = unrelated_process.wait().expect("reap the stand-in");
    assert_eq!(
        stand_in_exit_status.signal(),
        Some(libc::SIGTERM),
        "the stand-in ends by this test's SIGTERM: {kill_policy:?} did not signal the \
         process group the carried process id names"
    );
    old_backend
        .kill_pane(old_pane_id, KillPolicy::Tree)
        .expect("close the old pane");
}

#[cfg(unix)]
#[test]
fn get_child_process_id_returns_the_panes_child_process_id() {
    let backend = PortablePtyBackend::with_pty_sink(CountingSink::new());
    let pane_id = PaneId::new();
    let (writer_sender, _writer_receiver) = channel::<WriterMessage>();
    backend.pane_by_id.lock().expect("panes").insert(
        pane_id,
        build_pane_entry(
            Terminal::Master(Arc::new(Mutex::new(None))),
            writer_sender,
            Arc::new(AtomicUsize::new(0)),
        ),
    );

    assert_eq!(
        backend.get_child_process_id(pane_id),
        Some(std::process::id())
    );
}

#[cfg(unix)]
#[test]
fn resizing_through_the_crates_master_retunes_the_terminal() {
    let pty_pair = {
        let _pty_gate_guard = PTY_GATE.lock().expect("pty gate");
        native_pty_system()
            .openpty(build_portable_pty_size(STANDARD_PTY_SIZE))
            .expect("openpty")
    };
    let pty_master_slot = Arc::new(Mutex::new(Some(pty_pair.master)));
    let terminal = Terminal::Master(Arc::clone(&pty_master_slot));

    assert_eq!(
        terminal.resize(PtySize {
            column_count: 132,
            row_count: 43
        }),
        Ok(())
    );

    let portable_pty_size = pty_master_slot
        .lock()
        .expect("terminal")
        .as_ref()
        .expect("the master stays in its slot")
        .get_size()
        .expect("read the size back");
    assert_eq!((portable_pty_size.cols, portable_pty_size.rows), (132, 43));
}

#[cfg(unix)]
#[test]
fn a_resize_the_kernel_refuses_leaves_the_pane_at_its_old_size() {
    // A resize the kernel refuses reaches the caller as `PtyError::Io`, and
    // the pane keeps its old size.
    let requested_pty_size = PtySize {
        column_count: 132,
        row_count: 43,
    };
    let (_peer_socket, terminal_socket) = build_fake_terminal();
    let terminal_fd = Arc::new(terminal_socket);
    let backend = PortablePtyBackend::with_pty_sink(CountingSink::new());
    let pane_id = PaneId::new();
    let (writer_sender, writer_receiver) = channel::<WriterMessage>();
    drop(writer_receiver);
    backend.pane_by_id.lock().expect("panes").insert(
        pane_id,
        build_pane_entry(
            Terminal::Owned(Arc::clone(&terminal_fd)),
            writer_sender,
            Arc::new(AtomicUsize::new(0)),
        ),
    );

    // The kernel's own error text on this system.
    let resize_error = resize_terminal(&terminal_fd, requested_pty_size)
        .expect_err("a socket is not a terminal and takes no window size");

    assert_eq!(
        backend.resize_pane(pane_id, requested_pty_size),
        Err(PtyError::Io {
            detail: resize_error.to_string(),
        }),
        "the kernel's refusal must reach the caller as it stands"
    );
    assert_eq!(
        backend
            .list_carried_panes()
            .first()
            .map(|carried_pane| carried_pane.pty_size),
        Some(STANDARD_PTY_SIZE),
        "a pane whose child was never told the new size must still report the old one"
    );
}

#[cfg(unix)]
#[test]
fn a_pane_whose_writer_has_already_ended_does_not_hold_up_the_flush() {
    // Two panes with no writer left: one whose channel is closed, one whose
    // writer takes the barrier and ends without answering it. The flush
    // answers `Ok(())`.
    let backend = PortablePtyBackend::with_pty_sink(CountingSink::new());

    // The barrier cannot be queued for this pane.
    let closed_pane_id = PaneId::new();
    let (closed_writer_sender, closed_writer_receiver) = channel::<WriterMessage>();
    drop(closed_writer_receiver);

    // This pane's writer takes the barrier and ends without answering it.
    let ending_pane_id = PaneId::new();
    let (ending_writer_sender, ending_writer_receiver) = channel::<WriterMessage>();
    let writer_thread = spawn_pty_thread("koshi-pty-write", move || {
        let _ = ending_writer_receiver.recv();
    });

    {
        let mut pane_entries_by_id = backend.pane_by_id.lock().expect("panes");
        pane_entries_by_id.insert(
            closed_pane_id,
            build_pane_entry(
                Terminal::Master(Arc::new(Mutex::new(None))),
                closed_writer_sender,
                Arc::new(AtomicUsize::new(0)),
            ),
        );
        pane_entries_by_id.insert(
            ending_pane_id,
            build_pane_entry(
                Terminal::Master(Arc::new(Mutex::new(None))),
                ending_writer_sender,
                Arc::new(AtomicUsize::new(0)),
            ),
        );
    }

    assert_eq!(
        backend.flush_writers(),
        Ok(()),
        "a writer that has already ended must not refuse the flush"
    );
    writer_thread.join().expect("the writer thread ends");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn only_a_pane_with_a_live_child_answers_a_directory() {
    // `find_live_working_directory` answers `None` for an unknown pane and for
    // a pane whose child is marked exited, and the child's directory for a
    // live one.
    let backend = PortablePtyBackend::with_pty_sink(CountingSink::new());
    let pane_id = PaneId::new();
    let (writer_sender, writer_receiver) = channel::<WriterMessage>();
    drop(writer_receiver);
    // The entry names this test process, whose directory is known here.
    backend.pane_by_id.lock().expect("panes").insert(
        pane_id,
        build_pane_entry(
            Terminal::Master(Arc::new(Mutex::new(None))),
            writer_sender,
            Arc::new(AtomicUsize::new(0)),
        ),
    );

    assert_eq!(
        backend.find_live_working_directory(PaneId::new()),
        None,
        "a pane this backend does not drive has no directory"
    );
    assert_eq!(
        backend.find_live_working_directory(pane_id),
        None,
        "a pane whose child ended must answer nothing"
    );

    backend
        .pane_by_id
        .lock()
        .expect("panes")
        .get_mut(&pane_id)
        .expect("the pane just inserted")
        .child_exit = Arc::new(ChildExit::default());

    let current_working_directory =
        std::fs::canonicalize(std::env::current_dir().expect("this process has a directory"))
            .expect("this process's directory is real");
    assert_eq!(
        backend
            .find_live_working_directory(pane_id)
            .map(|working_directory| {
                std::fs::canonicalize(working_directory).expect("the answer names a real directory")
            }),
        Some(current_working_directory),
        "a pane with a live child must answer the directory that child is in"
    );
}

#[cfg(unix)]
#[test]
fn a_pane_taken_back_after_its_child_was_signalled_reports_the_signal() {
    // The child was killed by SIGKILL before the pane is taken back. The
    // taken-back pane publishes `Signaled(9)`.
    let (terminal_fd, child_process_id) = {
        let _pty_gate_guard = PTY_GATE.lock().expect("pty gate");
        let pty_pair = native_pty_system()
            .openpty(build_portable_pty_size(STANDARD_PTY_SIZE))
            .expect("openpty");
        let terminal_fd =
            duplicate_terminal_file_descriptor(&*pty_pair.master).expect("terminal descriptor");
        let mut command_builder = CommandBuilder::new("/bin/sh");
        command_builder.arg("-c");
        command_builder.arg("kill -9 $$");
        let child_process = pty_pair
            .slave
            .spawn_command(command_builder)
            .expect("spawn");
        drop(pty_pair.slave);
        let child_process_id = child_process.process_id().expect("process id");
        // Left unreaped.
        drop(child_process);
        (terminal_fd, child_process_id)
    };

    let pty_sink = CountingSink::new();
    let backend = PortablePtyBackend::with_pty_sink(pty_sink.clone());
    let pane_id = PaneId::new();
    backend
        .adopt(
            pane_id,
            terminal_fd,
            child_process_id,
            STANDARD_PTY_SIZE,
            None,
        )
        .expect("take the pane back");

    assert_eq!(
        read_exit_status_or_fail(
            &pty_sink,
            "a taken-back pane never published its child's exit",
        ),
        ExitStatus::Signaled(9),
        "a child killed by SIGKILL must be reported as signalled, with that signal's number"
    );
}

#[cfg(unix)]
#[test]
fn spawning_a_pane_id_the_backend_already_holds_is_refused() {
    // The refused spawn leaves the live entry in place: the first pane still
    // resizes and closes.
    let pty_sink = CountingSink::new();
    let (backend, pane_id) = start_backend_with_shell_script(pty_sink.clone(), "sleep 30");

    let spawn_error = {
        let _gate = PTY_GATE.lock().expect("pty gate");
        backend
            .spawn_pane(
                pane_id,
                build_shell_spawn_spec("sleep 30"),
                STANDARD_PTY_SIZE,
            )
            .expect_err("the id is already open")
    };

    assert_eq!(
        spawn_error.to_string(),
        format!("failed to spawn pty: pane {pane_id} is already open")
    );
    backend
        .resize_pane(pane_id, STANDARD_PTY_SIZE)
        .expect("the first pane is still held");
    backend
        .kill_pane(pane_id, KillPolicy::Tree)
        .expect("close the pane");
}

#[cfg(unix)]
#[test]
fn taking_back_a_pane_id_the_backend_already_holds_is_refused() {
    // The refused take-back leaves the live entry in place, and the second
    // child keeps running: the first pane still resizes and closes.
    let pty_sink = CountingSink::new();
    let (backend, pane_id) = start_backend_with_shell_script(pty_sink.clone(), "sleep 30");

    let (second_child_guard, terminal_fd, child_process_id) = {
        let _pty_gate_guard = PTY_GATE.lock().expect("pty gate");
        let pty_pair = native_pty_system()
            .openpty(build_portable_pty_size(STANDARD_PTY_SIZE))
            .expect("openpty");
        let terminal_fd =
            duplicate_terminal_file_descriptor(&*pty_pair.master).expect("terminal descriptor");
        let mut command_builder = CommandBuilder::new("/bin/sh");
        command_builder.arg("-c");
        command_builder.arg("sleep 30");
        let child_guard = ChildGuard::from_child(
            pty_pair
                .slave
                .spawn_command(command_builder)
                .expect("spawn"),
        );
        drop(pty_pair.slave);
        let child_process_id = child_guard.process_id().expect("process id");
        (child_guard, terminal_fd, child_process_id)
    };

    let adopt_error = backend
        .adopt(
            pane_id,
            terminal_fd,
            child_process_id,
            STANDARD_PTY_SIZE,
            None,
        )
        .expect_err("the id is already open");

    assert_eq!(
        adopt_error.to_string(),
        format!("failed to spawn pty: pane {pane_id} is already open")
    );
    assert!(
        is_process_alive(child_process_id),
        "a refused take-back must leave the second child running"
    );
    backend
        .resize_pane(pane_id, STANDARD_PTY_SIZE)
        .expect("the first pane is still held");
    backend
        .kill_pane(pane_id, KillPolicy::Tree)
        .expect("close the pane");
    drop(second_child_guard);
}
