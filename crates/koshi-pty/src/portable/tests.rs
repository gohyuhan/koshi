//! Unit tests for [`ChildGuard`]'s kill-on-drop backstop, the two reader
//! pumps, the watcher's standby wait, the reader's delivery gate, the reader
//! park, the writer flush, the pane hand-over across a process-image swap,
//! the reader that takes the terminal's opening cursor question out of the
//! output, and the pure status/size conversions. The tests that spawn a real
//! Unix PTY are Unix-gated; everything else runs on every platform.

use super::*;

#[cfg(unix)]
use koshi_core::process::ShellKind;
#[cfg(unix)]
use std::{collections::BTreeMap, path::PathBuf};

/// True while process `pid` is still around (`kill -0` succeeds).
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Launch a long-lived child inside a PTY, returning a guard over it and the
/// child's pid.
#[cfg(unix)]
fn spawn_guarded_sleeper() -> (ChildGuard, u32) {
    let pair = native_pty_system()
        .openpty(to_pp_size(PtySize { cols: 80, rows: 24 }))
        .expect("openpty");
    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.arg("-c");
    cmd.arg("sleep 300");
    let child = ChildGuard::new(pair.slave.spawn_command(cmd).expect("spawn"));
    drop(pair.slave);
    let pid = child.process_id().expect("pid");
    (child, pid)
}

#[cfg(unix)]
#[test]
fn dropping_an_armed_guard_kills_the_child() {
    let (guard, pid) = spawn_guarded_sleeper();
    assert!(process_alive(pid), "child should be running before drop");
    drop(guard);
    // Polls up to 3 seconds for the child to go.
    let deadline = Instant::now() + Duration::from_secs(3);
    while process_alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !process_alive(pid),
        "an armed guard must kill the child on drop"
    );
}

#[cfg(unix)]
#[test]
fn disarming_leaves_the_child_running() {
    let (guard, pid) = spawn_guarded_sleeper();
    let mut child = guard.disarm();
    assert!(process_alive(pid), "disarming must not kill the child");
    let _ = child.kill(); // clean up the still-running child
}

// `sig_no`, `map_status`, and `to_pp_size` make no platform syscalls. The tests
// below run on every platform.

#[test]
fn sig_no_parses_the_macos_colon_number_form() {
    // macOS/BSD `strsignal(3)` text: "<description>: <n>".
    assert_eq!(sig_no("Terminated: 15"), 15);
    assert_eq!(sig_no("Hangup: 1"), 1);
}

#[test]
fn sig_no_parses_the_null_strsignal_fallback_form() {
    // portable-pty's own fallback when `strsignal` returns null.
    assert_eq!(sig_no("Signal 23"), 23);
    assert_eq!(sig_no("Signal 0"), 0);
}

#[test]
fn sig_no_maps_every_known_glibc_bare_description() {
    // Linux/glibc `strsignal(3)` text carries no trailing number at all.
    let cases: &[(&str, i32)] = &[
        ("Hangup", 1),
        ("Interrupt", 2),
        ("Quit", 3),
        ("Illegal instruction", 4),
        ("Trace/breakpoint trap", 5),
        ("Aborted", 6),
        ("Bus error", 7),
        ("Floating point exception", 8),
        ("Killed", 9),
        ("User defined signal 1", 10),
        ("Segmentation fault", 11),
        ("User defined signal 2", 12),
        ("Broken pipe", 13),
        ("Alarm clock", 14),
        ("Terminated", 15),
    ];
    for (desc, want) in cases {
        assert_eq!(sig_no(desc), *want, "sig_no({desc:?})");
    }
}

#[test]
fn sig_no_does_not_greedily_misparse_a_trailing_ordinal_as_a_signal_number() {
    // "User defined signal 1" ends in the digit `1` and "User defined signal
    // 2" ends in `2`. Neither has a `": "` separator or the `"Signal "`
    // prefix. Both resolve through the exact-match table: SIGUSR1 is 10 and
    // SIGUSR2 is 12.
    assert_eq!(sig_no("User defined signal 1"), 10);
    assert_eq!(sig_no("User defined signal 2"), 12);
}

#[test]
fn sig_no_unrecognized_description_is_zero() {
    assert_eq!(sig_no("Unknown Signal Foo"), 0);
    assert_eq!(sig_no(""), 0);
    // The "Signal " prefix with no number behind it: `strip_prefix` succeeds,
    // `parse::<i32>` fails, and the exact-match table has no entry.
    assert_eq!(sig_no("Signal abc"), 0);
    // A ": " separator with no number behind it takes the same path.
    assert_eq!(sig_no("foo: bar"), 0);
}

#[test]
fn to_pp_size_carries_cols_and_rows_and_zeroes_the_pixel_fields() {
    let got = to_pp_size(PtySize { cols: 80, rows: 24 });
    assert_eq!(got.cols, 80);
    assert_eq!(got.rows, 24);
    assert_eq!(got.pixel_width, 0);
    assert_eq!(got.pixel_height, 0);
}

#[test]
fn to_pp_size_carries_boundary_dimensions_unchanged() {
    let got = to_pp_size(PtySize { cols: 0, rows: 0 });
    assert_eq!((got.cols, got.rows), (0, 0));

    let got = to_pp_size(PtySize {
        cols: u16::MAX,
        rows: u16::MAX,
    });
    assert_eq!((got.cols, got.rows), (u16::MAX, u16::MAX));
}

#[test]
fn map_status_maps_a_clean_exit_code() {
    assert_eq!(
        map_status(portable_pty::ExitStatus::with_exit_code(0)),
        ExitStatus::ExitCode(0)
    );
    assert_eq!(
        map_status(portable_pty::ExitStatus::with_exit_code(137)),
        ExitStatus::ExitCode(137)
    );
}

#[test]
fn map_status_wraps_an_exit_code_above_i32_max_instead_of_panicking() {
    // `s.exit_code() as i32` wraps: `u32::MAX` (0xFFFF_FFFF) is `-1`, and
    // `i32::MAX as u32 + 1` is `i32::MIN`.
    assert_eq!(
        map_status(portable_pty::ExitStatus::with_exit_code(u32::MAX)),
        ExitStatus::ExitCode(-1)
    );
    assert_eq!(
        map_status(portable_pty::ExitStatus::with_exit_code(
            i32::MAX as u32 + 1
        )),
        ExitStatus::ExitCode(i32::MIN)
    );
}

#[test]
fn map_status_maps_a_signal_through_sig_no() {
    assert_eq!(
        map_status(portable_pty::ExitStatus::with_signal("Terminated")),
        ExitStatus::Signaled(15)
    );
    assert_eq!(
        map_status(portable_pty::ExitStatus::with_signal("Terminated: 15")),
        ExitStatus::Signaled(15)
    );
    assert_eq!(
        map_status(portable_pty::ExitStatus::with_signal(
            "User defined signal 1"
        )),
        ExitStatus::Signaled(10)
    );
    assert_eq!(
        map_status(portable_pty::ExitStatus::with_signal("nonsense")),
        ExitStatus::Signaled(0)
    );
}

/// Nothing received the stop request. The 3-second window is not spent.
#[test]
fn an_undelivered_stop_request_skips_the_grace_window() {
    let exited = AtomicBool::new(false);
    let started = Instant::now();
    let stopped = stopped_within_grace(StopRequest::NotDelivered, &exited, Duration::from_secs(3));
    let took = started.elapsed();
    assert!(
        !stopped,
        "an undelivered stop request must report the child as still running"
    );
    assert!(
        took < Duration::from_millis(100),
        "an undelivered stop request must not spend the grace window; took {took:?}"
    );
}

/// A stop request that reached the child still waits the whole 200ms window
/// while the child stays alive.
#[test]
fn a_delivered_stop_request_waits_out_the_window_when_the_child_stays() {
    let exited = AtomicBool::new(false);
    let started = Instant::now();
    let stopped = stopped_within_grace(StopRequest::Delivered, &exited, Duration::from_millis(200));
    let took = started.elapsed();
    assert!(
        !stopped,
        "a child that never sets the exited flag must report as still running"
    );
    assert!(
        took >= Duration::from_millis(200),
        "a delivered stop request must still spend the whole window; took {took:?}"
    );
}

/// A stop request that reached part of a group waits the whole 200ms window
/// while the leader stays alive.
#[test]
fn a_partly_delivered_stop_request_waits_out_the_window() {
    let exited = AtomicBool::new(false);
    let started = Instant::now();
    let stopped = stopped_within_grace(StopRequest::Unknown, &exited, Duration::from_millis(200));
    let took = started.elapsed();
    assert!(
        !stopped,
        "a group that never sets the exited flag must report as still running"
    );
    assert!(
        took >= Duration::from_millis(200),
        "a partly delivered stop request must still spend the whole window; took {took:?}"
    );
}

/// A child that already exited is reported as stopped at once, without waiting
/// out the 3-second window.
#[test]
fn a_delivered_stop_request_reports_a_child_that_has_already_exited() {
    let exited = AtomicBool::new(true);
    let started = Instant::now();
    let stopped = stopped_within_grace(StopRequest::Delivered, &exited, Duration::from_secs(3));
    assert!(
        stopped,
        "a child whose exited flag is set must report as stopped"
    );
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "an already-exited child must not spend the grace window; took {:?}",
        started.elapsed()
    );
}

/// A child that exits inside the window ends the wait at the next poll, not at
/// the end of the window.
#[test]
fn a_child_that_exits_during_the_window_ends_the_wait_early() {
    let exited = Arc::new(AtomicBool::new(false));
    let flips = Arc::clone(&exited);
    let flipper = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        flips.store(true, Ordering::SeqCst);
    });

    let started = Instant::now();
    let stopped = stopped_within_grace(StopRequest::Delivered, &exited, Duration::from_secs(3));
    let took = started.elapsed();
    flipper.join().expect("flipper");

    assert!(
        stopped,
        "a child that exits inside the window must report as stopped"
    );
    assert!(
        took < Duration::from_secs(1),
        "the wait must end at the flip, not at the window; took {took:?}"
    );
}

/// A sink that keeps every chunk and every exit it is handed, oldest first.
struct CountingSink {
    /// Every output chunk taken, oldest first.
    chunks: Mutex<Vec<Vec<u8>>>,
    /// Every exit taken, oldest first.
    exits: Mutex<Vec<ExitStatus>>,
}

impl CountingSink {
    fn new() -> Arc<Self> {
        Arc::new(CountingSink {
            chunks: Mutex::new(Vec::new()),
            exits: Mutex::new(Vec::new()),
        })
    }

    /// How many chunks have reached this sink.
    fn chunk_count(&self) -> usize {
        self.chunks.lock().expect("counting sink").len()
    }

    /// Every byte this sink has been handed, in order.
    #[cfg(unix)]
    fn bytes(&self) -> Vec<u8> {
        self.chunks.lock().expect("counting sink").concat()
    }

    /// The exit this sink has been handed, or `None` while none has arrived.
    fn exit_taken(&self) -> Option<ExitStatus> {
        self.exits.lock().expect("counting sink").first().copied()
    }

    /// How many exits have reached this sink.
    fn exit_count(&self) -> usize {
        self.exits.lock().expect("counting sink").len()
    }
}

impl PtySink for CountingSink {
    fn output(&self, _pane: PaneId, bytes: Vec<u8>) -> bool {
        self.chunks.lock().expect("counting sink").push(bytes);
        true
    }

    fn exit(&self, _pane: PaneId, status: ExitStatus) {
        self.exits.lock().expect("counting sink").push(status);
    }
}

/// How long a test waits for a thread it expects back. A wait that reaches it
/// fails the test.
const HANG_GUARD: Duration = Duration::from_secs(5);

/// [`should_publish_exit`] with the production limit and grace. The tests that
/// need a deadline already past call it directly.
fn stand_by(cancel: &Receiver<()>, handover: &Mutex<Handover>) -> bool {
    should_publish_exit(
        cancel,
        handover,
        Instant::now() + EXIT_PUBLISH_LIMIT,
        EXIT_PUBLISH_GRACE,
    )
}

/// [`ReaderSignals`] over `wake`, `exited` and `gate`.
#[cfg(unix)]
fn signals<'a>(wake: &'a Waker, exited: &'a AtomicBool, gate: &'a ReaderGate) -> ReaderSignals<'a> {
    ReaderSignals { wake, exited, gate }
}

/// A gate holding nobody.
#[cfg(unix)]
fn open_gate() -> ReaderGate {
    ReaderGate::new()
}

/// A sink-route [`Delivery`] over `sink`, and the [`Handover`] it shares with
/// the watcher. The exit sender is dropped.
fn sink_delivery(sink: Arc<dyn PtySink>) -> (Delivery, Arc<Mutex<Handover>>) {
    let (_exit_sender, exit_receiver) = channel::<ExitStatus>();
    let handover = Arc::new(Mutex::new(Handover::default()));
    let delivery = Delivery::Sink {
        sink,
        exit_receiver,
        handover: Arc::clone(&handover),
    };
    (delivery, handover)
}

/// A handover holding `begun` chunks claimed and `done` released.
fn handover_at(begun: u64, done: u64) -> Mutex<Handover> {
    Mutex::new(Handover {
        begun,
        done,
        settled: false,
    })
}

#[test]
fn handing_a_chunk_over_counts_it_in_and_back_out() {
    // A chunk fully handed over advances `begun` and `done` by one each and
    // leaves them equal.
    let sink = CountingSink::new();
    let (delivery, handover) = sink_delivery(sink.clone());

    assert!(delivery.output(PaneId::new(), b"hi"));
    assert_eq!(sink.chunk_count(), 1);
    assert_eq!(
        handover.lock().expect("handover").begun,
        1,
        "the chunk was counted in"
    );
    assert_eq!(
        handover.lock().expect("handover").done,
        1,
        "and counted back out"
    );
}

#[test]
fn a_settled_pane_stops_its_reader_and_takes_no_more_output() {
    // Output arriving after the pane is settled is refused: the reader is told
    // to stop, and the consumer is handed nothing.
    let sink = CountingSink::new();
    let (delivery, handover) = sink_delivery(sink.clone());
    let pane = PaneId::new();

    assert!(delivery.output(pane, b"before"));
    handover.lock().expect("handover").settled = true;

    assert!(
        !delivery.output(pane, b"after"),
        "a settled pane must tell its reader to stop"
    );
    assert_eq!(
        sink.chunk_count(),
        1,
        "no output may reach the consumer once the pane is settled"
    );
}

/// A sink that refuses every chunk and records no exit.
struct RefusingSink;

impl PtySink for RefusingSink {
    fn output(&self, _pane: PaneId, _bytes: Vec<u8>) -> bool {
        false
    }

    fn exit(&self, _pane: PaneId, _status: ExitStatus) {}
}

#[test]
fn a_refused_chunk_settles_the_pane() {
    // A refused chunk settles the pane.
    let (delivery, handover) = sink_delivery(Arc::new(RefusingSink));

    assert!(!delivery.output(PaneId::new(), b"hi"));
    assert!(
        handover.lock().expect("handover").settled,
        "a refused chunk must settle the pane so the watcher publishes no exit"
    );
}

#[test]
fn a_settled_pane_finishes_without_waiting_for_the_exit_status() {
    // `exit_sender` is held for the whole test and never sent on. `finish` on
    // a settled pane returns without reading the status channel.
    let (exit_sender, exit_receiver) = channel::<ExitStatus>();
    let delivery = Delivery::Sink {
        sink: CountingSink::new(),
        exit_receiver,
        handover: Arc::new(Mutex::new(Handover {
            begun: 0,
            done: 0,
            settled: true,
        })),
    };

    let (done, done_rx) = channel::<()>();
    thread::spawn(move || {
        delivery.finish(PaneId::new());
        let _ = done.send(());
    });

    assert_eq!(
        done_rx.recv_timeout(HANG_GUARD),
        Ok(()),
        "a settled pane waited for an exit status it will never publish"
    );
    drop(exit_sender);
}

#[test]
fn a_reader_at_the_end_of_its_terminal_hands_the_watchers_status_over_once() {
    // The watcher's status is already on the channel. `finish` hands it to the
    // consumer once and settles the pane.
    let sink = CountingSink::new();
    let (exit_sender, exit_receiver) = channel::<ExitStatus>();
    let handover = Arc::new(Mutex::new(Handover::default()));
    let delivery = Delivery::Sink {
        sink: sink.clone(),
        exit_receiver,
        handover: Arc::clone(&handover),
    };
    exit_sender
        .send(ExitStatus::ExitCode(4))
        .expect("queue the status");

    delivery.finish(PaneId::new());

    assert_eq!(sink.exit_taken(), Some(ExitStatus::ExitCode(4)));
    assert_eq!(sink.exit_count(), 1, "the exit must be handed over once");
    assert!(
        handover.lock().expect("handover").settled,
        "handing the exit over must settle the pane"
    );
}

#[test]
fn a_watcher_that_ends_without_a_status_tells_the_consumer_nothing() {
    // The status channel closes with nothing on it. `finish` returns and the
    // consumer is handed no exit.
    let sink = CountingSink::new();
    let (exit_sender, exit_receiver) = channel::<ExitStatus>();
    drop(exit_sender);
    let delivery = Delivery::Sink {
        sink: sink.clone(),
        exit_receiver,
        handover: Arc::new(Mutex::new(Handover::default())),
    };

    delivery.finish(PaneId::new());

    assert_eq!(
        sink.exit_count(),
        0,
        "a status that never arrived must not be published"
    );
}

#[cfg(unix)]
#[test]
fn a_channel_delivery_never_reads_as_settled() {
    let pane = PaneId::new();
    let (handle, output, _exit) = PtyHandle::new(pane);
    let delivery = Delivery::Channel(output);

    assert!(!delivery.settled(), "a held handle settles nothing");
    drop(handle);
    assert!(!delivery.settled(), "a dropped handle settles nothing");
}

/// [`stand_by_for_the_reader`] over `handover`, publishing `ExitCode(3)` to
/// `sink` with the production limit and grace, and a cancel that never fires.
fn stand_by_publishing_exit_code_3(sink: &Arc<CountingSink>, handover: &Mutex<Handover>) {
    let (_cancel, cancel_rx) = channel::<()>();
    let weak = Arc::downgrade(sink) as Weak<dyn PtySink>;
    stand_by_for_the_reader(
        &cancel_rx,
        handover,
        &weak,
        PaneId::new(),
        ExitStatus::ExitCode(3),
    );
}

#[test]
fn a_watcher_that_steps_in_hands_the_exit_over_and_settles_it() {
    // The pane is never settled. After the limit the watcher hands the exit
    // over, once, and settles the pane.
    let sink = CountingSink::new();
    let handover = handover_at(0, 0);

    stand_by_publishing_exit_code_3(&sink, &handover);

    assert_eq!(sink.exit_taken(), Some(ExitStatus::ExitCode(3)));
    assert_eq!(sink.exit_count(), 1);
    assert!(
        handover.lock().expect("handover").settled,
        "an exit was published without settling the pane, so the reader can \
         publish a second one"
    );
}

#[test]
fn a_watcher_that_publishes_nothing_still_settles_the_pane() {
    // A cancel queued before the standby starts ends it without publishing.
    // The pane is settled all the same.
    let sink = CountingSink::new();
    let (cancel, cancel_rx) = channel::<()>();
    cancel.send(()).expect("queue the cancel");
    let handover = handover_at(0, 0);
    let weak = Arc::downgrade(&sink) as Weak<dyn PtySink>;

    stand_by_for_the_reader(
        &cancel_rx,
        &handover,
        &weak,
        PaneId::new(),
        ExitStatus::ExitCode(3),
    );

    assert_eq!(
        sink.exit_count(),
        0,
        "a cancelled standby published an exit"
    );
    assert!(
        handover.lock().expect("handover").settled,
        "a cancelled standby left the pane unsettled"
    );
}

#[test]
fn a_pane_the_reader_already_settled_is_not_published_over() {
    // The pane is already settled. The watcher publishes nothing.
    let sink = CountingSink::new();
    let handover = Mutex::new(Handover {
        begun: 0,
        done: 0,
        settled: true,
    });

    stand_by_publishing_exit_code_3(&sink, &handover);

    assert_eq!(
        sink.exit_count(),
        0,
        "the consumer was handed a second exit for one child"
    );
}

#[test]
fn a_watcher_whose_consumer_is_gone_publishes_nothing() {
    // The sink has been dropped; only the watcher's weak reference is left.
    // The standby returns without publishing.
    let sink = CountingSink::new();
    let weak = Arc::downgrade(&sink) as Weak<dyn PtySink>;
    drop(sink);
    let (_cancel, cancel_rx) = channel::<()>();
    let handover = handover_at(0, 0);

    stand_by_for_the_reader(
        &cancel_rx,
        &handover,
        &weak,
        PaneId::new(),
        ExitStatus::ExitCode(3),
    );

    assert!(
        weak.upgrade().is_none(),
        "the standby brought the consumer back to life"
    );
}

#[test]
fn a_reader_that_never_reaches_the_end_leaves_the_exit_to_the_watcher() {
    // The pane is never settled. Once the deadline passes, the standby answers
    // `true`.
    let (_cancel, cancel_rx) = channel::<()>();
    let handover = handover_at(0, 0);

    let started = Instant::now();
    let publish = should_publish_exit(
        &cancel_rx,
        &handover,
        Instant::now() + EXIT_PUBLISH_GRACE * 2,
        EXIT_PUBLISH_GRACE,
    );

    assert!(
        publish,
        "a reader that cannot reach the end publishes nothing"
    );
    assert!(
        started.elapsed() >= EXIT_PUBLISH_GRACE * 2,
        "the watcher must give the reader until the limit before stepping in"
    );
}

#[test]
fn a_reader_that_settles_the_exit_stops_the_watcher_publishing() {
    // The pane is settled halfway through the first round. The standby answers
    // `false` before the limit.
    let (_cancel, cancel_rx) = channel::<()>();
    let handover = Arc::new(Mutex::new(Handover::default()));

    let reader = Arc::clone(&handover);
    thread::spawn(move || {
        thread::sleep(EXIT_PUBLISH_GRACE / 2);
        reader.lock().expect("handover").settled = true;
    });

    let started = Instant::now();
    let publish = stand_by(&cancel_rx, &handover);

    assert!(!publish, "the reader published, so the watcher must not");
    assert!(
        started.elapsed() < EXIT_PUBLISH_LIMIT,
        "a settled pane must end the standby rather than sit out the limit"
    );
}

#[test]
fn an_exit_is_never_published_over_a_chunk_still_in_flight() {
    // The deadline has already passed and one chunk is in flight. The standby
    // answers only once that chunk lands.
    let (_cancel, cancel_rx) = channel::<()>();
    // One chunk begun and not yet finished: in the consumer's hands.
    let handover = Arc::new(Mutex::new(Handover {
        begun: 1,
        done: 0,
        settled: false,
    }));

    // Set right before the chunk lands.
    let landed = Arc::new(AtomicBool::new(false));
    let lands = Arc::clone(&landed);
    let consumer = Arc::clone(&handover);
    let hand_it_back = thread::spawn(move || {
        thread::sleep(EXIT_PUBLISH_GRACE * 3);
        lands.store(true, Ordering::SeqCst);
        consumer.lock().expect("handover").done = 1;
    });

    let publish = should_publish_exit(&cancel_rx, &handover, Instant::now(), EXIT_PUBLISH_GRACE);

    assert!(
        publish,
        "the deadline has passed and nothing is in flight, so the watcher \
         publishes"
    );
    assert!(
        landed.load(Ordering::SeqCst),
        "an exit was published while a chunk was still in the consumer's hands"
    );
    hand_it_back.join().expect("consumer");
}

#[test]
fn a_chunk_that_never_lands_holds_the_watcher_until_the_pane_is_closed() {
    // The deadline has already passed and one chunk is in flight that never
    // lands. The cancel ends the standby, and it answers `false`.
    //
    // The standby runs on its own thread; one that has not ended after
    // `HANG_GUARD` fails the test.
    let (cancel, cancel_rx) = channel::<()>();
    let (answer, answer_rx) = channel::<bool>();
    thread::spawn(move || {
        let publish = should_publish_exit(
            &cancel_rx,
            &handover_at(1, 0),
            Instant::now(),
            EXIT_PUBLISH_GRACE,
        );
        let _ = answer.send(publish);
    });

    cancel.send(()).expect("close the pane");

    let publish = answer_rx
        .recv_timeout(HANG_GUARD)
        .expect("closing the pane must end the standby");
    assert!(!publish, "a cancelled standby must publish no exit");
}

#[test]
fn closing_a_pane_ends_the_watchers_standby_without_publishing() {
    // A cancel queued before the standby starts ends it before the limit, and
    // it answers `false`.
    let (cancel, cancel_rx) = channel::<()>();
    cancel.send(()).expect("queue the cancel");

    let started = Instant::now();
    let publish = stand_by(&cancel_rx, &handover_at(0, 0));

    assert!(!publish, "a cancelled standby must publish no exit");
    assert!(
        started.elapsed() < EXIT_PUBLISH_LIMIT,
        "a cancelled standby must not sit through the rounds"
    );
}

#[test]
fn a_dropped_pane_entry_ends_the_watchers_standby_without_publishing() {
    // The cancel sender is dropped before the standby starts. The standby
    // answers `false`.
    let (cancel, cancel_rx) = channel::<()>();
    drop(cancel);

    let publish = stand_by(&cancel_rx, &handover_at(0, 0));

    assert!(!publish, "a dropped entry must publish no exit");
}

/// A stand-in terminal for the reader's wait: a connected socket pair, with
/// `far` playing the child writing into it and the returned descriptor the
/// master end the pump waits on and reads. Dropping `far` is the terminal
/// reporting an end.
#[cfg(unix)]
fn fake_terminal() -> (std::os::unix::net::UnixStream, std::os::fd::OwnedFd) {
    let (far, near) = std::os::unix::net::UnixStream::pair().expect("terminal pair");
    (far, std::os::fd::OwnedFd::from(near))
}

#[cfg(unix)]
#[test]
fn resizing_a_terminal_retunes_the_size_its_child_reads() {
    // The size set through the pane's own descriptor is read back through
    // `portable-pty`'s master of the same terminal.
    let pair = native_pty_system()
        .openpty(to_pp_size(PtySize { cols: 80, rows: 24 }))
        .expect("openpty");
    let terminal = own_terminal_fd(&*pair.master).expect("terminal descriptor");

    resize_terminal(
        &terminal,
        PtySize {
            cols: 132,
            rows: 43,
        },
    )
    .expect("resize the terminal");

    let got = pair.master.get_size().expect("read the size back");
    assert_eq!(
        (got.cols, got.rows),
        (132, 43),
        "the child would still be told the old window size"
    );
}

#[cfg(unix)]
#[test]
fn a_terminal_carries_both_directions_over_the_one_descriptor() {
    // One descriptor carries both directions: bytes written through it reach
    // the far end, and bytes the far end writes are read through it.
    let (far, near) = std::os::unix::net::UnixStream::pair().expect("terminal pair");
    let terminal = std::os::fd::OwnedFd::from(near);

    write_terminal(&terminal, b"typed").expect("write to the terminal");
    let mut got = [0u8; 8];
    let read = (&far).read(&mut got).expect("the child reads its input");
    assert_eq!(&got[..read], b"typed", "input never reached the child");

    (&far).write_all(b"printed").expect("the child prints");
    let mut got = [0u8; 8];
    let read = read_terminal(&terminal, &mut got).expect("read the terminal");
    assert_eq!(&got[..read], b"printed", "output never reached the reader");
}

#[cfg(unix)]
#[test]
fn a_writer_that_stops_sends_the_terminal_nothing() {
    // After `Stop`, the writer loop writes nothing more to the terminal. This
    // test drives its own copy of the loop in [`start_writer`], joined once
    // it stops.
    let (far, near) = std::os::unix::net::UnixStream::pair().expect("pair");
    let terminal = Arc::new(std::os::fd::OwnedFd::from(near));
    let (writer_sender, writer_receiver) = channel::<WriterMsg>();

    let write_side = WriteSide::Owned(Arc::clone(&terminal));
    let writer = thread::spawn(move || {
        let mut write_side = write_side;
        while let Ok(message) = writer_receiver.recv() {
            match message {
                WriterMsg::Bytes(bytes) => match &mut write_side {
                    WriteSide::Owned(terminal) => {
                        let _ = write_terminal(terminal, &bytes);
                    }
                    WriteSide::Crate(writer) => {
                        let _ = writer.write_all(&bytes);
                    }
                },
                WriterMsg::Barrier(answer) => {
                    let _ = answer.send(());
                }
                WriterMsg::Stop => break,
            }
        }
    });

    writer_sender
        .send(WriterMsg::Bytes(b"typed".to_vec()))
        .expect("queue input");
    writer_sender
        .send(WriterMsg::Stop)
        .expect("stop the writer");
    writer.join().expect("writer thread");

    far.set_nonblocking(true).expect("nonblocking");
    let mut got = [0u8; 32];
    let read = (&far).read(&mut got).expect("the child reads its input");
    assert_eq!(
        &got[..read],
        b"typed",
        "the terminal received something the writer was never asked to send"
    );
    assert_eq!(
        (&far).read(&mut got).map_err(|e| e.kind()),
        Err(ErrorKind::WouldBlock),
        "a writer that stops must send the terminal nothing of its own"
    );
}

#[cfg(unix)]
#[test]
fn only_a_pseudoterminal_master_is_named_as_one() {
    // `terminal_master_name` answers for a pseudoterminal master only: an
    // ordinary file, a pipe, and a closed number all give `None`.
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::FileTypeExt;

    let pair = native_pty_system()
        .openpty(to_pp_size(PtySize { cols: 80, rows: 24 }))
        .expect("openpty");
    let master = own_terminal_fd(&*pair.master).expect("terminal descriptor");
    let name = terminal_master_name(master.as_raw_fd())
        .expect("a pane's own terminal must be named as a pseudoterminal master");
    assert!(
        std::fs::metadata(&name)
            .expect("the name must be a path that exists")
            .file_type()
            .is_char_device(),
        "the name must be a terminal device, got {name:?}"
    );

    let ordinary_file = std::fs::File::open("/dev/null").expect("open an ordinary file");
    assert_eq!(
        terminal_master_name(ordinary_file.as_raw_fd()),
        None,
        "an ordinary file must not be named as a pseudoterminal master"
    );

    let mut ends = [0 as libc::c_int; 2];
    assert_eq!(
        unsafe { libc::pipe(ends.as_mut_ptr()) },
        0,
        "the pipe opens"
    );
    let reading_end = unsafe { OwnedFd::from_raw_fd(ends[0]) };
    let writing_end = unsafe { OwnedFd::from_raw_fd(ends[1]) };
    assert_eq!(
        terminal_master_name(reading_end.as_raw_fd()),
        None,
        "a pipe must not be named as a pseudoterminal master"
    );

    // The number of a descriptor this process has closed.
    let closed = writing_end.as_raw_fd();
    drop(writing_end);
    assert_eq!(
        terminal_master_name(closed),
        None,
        "a number naming nothing open must not be named as a pseudoterminal master"
    );
}

#[cfg(unix)]
#[test]
fn a_panes_descriptors_are_never_handed_to_a_later_child() {
    // A pane's terminal descriptor and its waker both carry `FD_CLOEXEC`.
    use std::os::fd::{AsFd, AsRawFd, FromRawFd};

    fn closes_on_exec(fd: std::os::fd::BorrowedFd<'_>) -> bool {
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0, "read the descriptor's flags");
        flags & libc::FD_CLOEXEC != 0
    }

    let pair = native_pty_system()
        .openpty(to_pp_size(PtySize { cols: 80, rows: 24 }))
        .expect("openpty");
    let terminal = own_terminal_fd(&*pair.master).expect("terminal descriptor");
    assert!(
        closes_on_exec(terminal.as_fd()),
        "a pane's terminal would be inherited by every child spawned after it"
    );

    let waker = Waker::new().expect("this platform offers a one-descriptor wake");
    assert!(
        closes_on_exec(waker.as_fd()),
        "a pane's waker would be inherited by every child spawned after it"
    );

    // The control: a plain `dup` of the terminal carries no `FD_CLOEXEC`.
    let inherited = unsafe { libc::dup(terminal.as_raw_fd()) };
    assert!(inherited >= 0, "duplicate the terminal");
    let inherited = unsafe { std::os::fd::OwnedFd::from_raw_fd(inherited) };
    assert!(
        !closes_on_exec(inherited.as_fd()),
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
    let ready = |fd: &Waker| {
        let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
        poll(&mut fds, PollTimeout::ZERO).expect("poll the doorbell");
        fds[0].revents().is_some_and(|r| !r.is_empty())
    };

    assert!(!ready(&waker), "a fresh doorbell must read as nothing");
    waker.wake();
    assert!(ready(&waker), "ringing must make the descriptor readable");
    assert!(ready(&waker), "a ring must stay on until it is drained");

    waker.drain();
    assert!(!ready(&waker), "draining must take the ring back off");

    waker.wake();
    assert!(ready(&waker), "a drained doorbell must ring again");
    waker.drain();
    assert!(!ready(&waker), "and drain again");
}

#[cfg(unix)]
#[test]
fn a_woken_reader_hands_over_the_last_output_then_stops() {
    // `bye` is in the terminal when the child is marked reaped and the
    // doorbell rings. The pump hands `bye` over, then stops after one quiet
    // round.
    let (far, watched) = fake_terminal();
    let wake = Waker::new().expect("waker");
    let sink = CountingSink::new();
    let (delivery, _handover) = sink_delivery(sink.clone());
    let gate = open_gate();

    (&far)
        .write_all(b"bye")
        .expect("child prints on the way out");
    // The watcher's order: the flag first, then the ring.
    let exited = AtomicBool::new(true);
    wake.wake();

    let started = Instant::now();
    pump_waited(
        &delivery,
        PaneId::new(),
        &watched,
        signals(&wake, &exited, &gate),
        EXIT_PUBLISH_GRACE,
        EXIT_PUBLISH_LIMIT,
    );

    assert_eq!(
        sink.chunks.lock().expect("counting sink").as_slice(),
        [b"bye".to_vec()],
        "output already in the terminal when the child died must still be \
         handed over"
    );
    assert!(
        started.elapsed() >= EXIT_PUBLISH_GRACE,
        "the reader must give the terminal a full quiet round before deciding \
         it has everything"
    );
    drop(far);
}

#[cfg(unix)]
#[test]
fn a_closed_pane_brings_its_reader_straight_back() {
    // `far` stays open for the whole test. The pane is settled and the
    // doorbell rung before the pump starts; the pump returns before the first
    // round is out.
    let (far, watched) = fake_terminal();
    let wake = Waker::new().expect("waker");
    let (delivery, handover) = sink_delivery(CountingSink::new());

    // Rounds far longer than any scheduling delay.
    let grace = Duration::from_secs(2);
    let limit = Duration::from_secs(10);

    // `kill`'s order: settle, then ring.
    handover.lock().expect("handover").settled = true;
    wake.wake();

    let (done, done_rx) = channel::<Instant>();
    thread::spawn(move || {
        let exited = AtomicBool::new(false);
        let gate = open_gate();
        pump_waited(
            &delivery,
            PaneId::new(),
            &watched,
            signals(&wake, &exited, &gate),
            grace,
            limit,
        );
        let _ = done.send(Instant::now());
    });

    let started = Instant::now();
    let ended = done_rx.recv_timeout(limit).expect(
        "a closed pane must not leave its reader waiting on a \
                 descendant that holds the terminal open",
    );
    assert!(
        ended.duration_since(started) < grace,
        "a closed pane must release its reader at once, not after a round"
    );
    drop(far);
}

#[cfg(unix)]
#[test]
fn a_reader_waits_on_a_live_terminal_with_no_timer() {
    // With the child running and the terminal quiet, the pump stays in its
    // wait for three rounds. The terminal reporting an end releases it.
    let (far, watched) = fake_terminal();
    let wake = Waker::new().expect("waker");
    let (delivery, _handover) = sink_delivery(CountingSink::new());

    let (done, done_rx) = channel::<()>();
    thread::spawn(move || {
        let exited = AtomicBool::new(false);
        let gate = open_gate();
        pump_waited(
            &delivery,
            PaneId::new(),
            &watched,
            signals(&wake, &exited, &gate),
            EXIT_PUBLISH_GRACE,
            EXIT_PUBLISH_LIMIT,
        );
        let _ = done.send(());
    });

    assert_eq!(
        done_rx.recv_timeout(EXIT_PUBLISH_GRACE * 3).err(),
        Some(std::sync::mpsc::RecvTimeoutError::Timeout),
        "a reader with a live child and a quiet terminal must not come back on \
         a timer"
    );

    drop(far); // the terminal reports an end
    assert_eq!(
        done_rx.recv_timeout(HANG_GUARD),
        Ok(()),
        "a terminal that reports an end must release the reader"
    );
}

#[cfg(unix)]
#[test]
fn output_arriving_after_the_child_has_gone_is_still_handed_over() {
    // The child is marked reaped before `a`, `b`, `c` arrive 200ms apart. Each
    // round that brings bytes starts another round; every byte is handed over.
    let (far, watched) = fake_terminal();
    let wake = Waker::new().expect("waker");
    let sink = CountingSink::new();
    let (delivery, _handover) = sink_delivery(sink.clone());

    // Rounds twice the gap between writes.
    let grace = Duration::from_millis(400);
    let limit = Duration::from_secs(5);

    let exited = AtomicBool::new(true);
    wake.wake(); // the child has gone
    let printer = thread::spawn(move || {
        for mark in [b'a', b'b', b'c'] {
            thread::sleep(Duration::from_millis(200));
            (&far).write_all(&[mark]).expect("the descendant prints");
        }
        far // hold the terminal open until the pump has stopped
    });

    let gate = open_gate();
    pump_waited(
        &delivery,
        PaneId::new(),
        &watched,
        signals(&wake, &exited, &gate),
        grace,
        limit,
    );

    // Two writes landing close together come back as one chunk. The bytes are
    // compared end to end, not the chunk count.
    let handed: Vec<u8> = sink.chunks.lock().expect("counting sink").concat();
    assert_eq!(
        handed, b"abc",
        "every byte a descendant printed before the terminal went quiet must \
         reach the consumer, in order"
    );
    drop(printer.join().expect("printer"));
}

#[cfg(unix)]
#[test]
fn a_descendant_that_never_stops_printing_still_ends_the_reader() {
    // The child is marked reaped and bytes never stop arriving. The rounds
    // stop at the limit.
    let (far, watched) = fake_terminal();
    let wake = Waker::new().expect("waker");
    let (delivery, _handover) = sink_delivery(CountingSink::new());

    let grace = Duration::from_millis(200);
    let limit = Duration::from_secs(1);

    let exited = AtomicBool::new(true);
    wake.wake(); // the child has gone
                 // The printer never sleeps: the socket buffer always holds something
                 // between the pump's reads. `far` is nonblocking: a full buffer refuses
                 // the write instead of parking the printer.
    far.set_nonblocking(true).expect("nonblocking");
    let printing = Arc::new(AtomicBool::new(true));
    let stop = Arc::clone(&printing);
    let printer = thread::spawn(move || {
        while stop.load(Ordering::SeqCst) {
            let _ = (&far).write(b"x");
            thread::yield_now();
        }
        far
    });

    let gate = open_gate();
    let started = Instant::now();
    pump_waited(
        &delivery,
        PaneId::new(),
        &watched,
        signals(&wake, &exited, &gate),
        grace,
        limit,
    );
    let elapsed = started.elapsed();

    printing.store(false, Ordering::SeqCst);
    drop(printer.join().expect("printer"));

    assert!(
        elapsed >= limit,
        "the reader must keep handing output over until the limit"
    );
    assert!(
        elapsed < limit * 3,
        "and must stop there rather than running for as long as the \
         descendant does"
    );
}

/// A stand-in terminal for the blocking pump: hands back `chunks` in order and
/// then reports an end, counting how many the pump took.
struct Scripted {
    /// Chunks still to hand out, oldest first.
    chunks: Vec<&'static [u8]>,
    /// How many chunks the pump has read.
    taken: usize,
}

impl Scripted {
    fn new(chunks: Vec<&'static [u8]>) -> Self {
        Scripted { chunks, taken: 0 }
    }
}

impl Read for Scripted {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.chunks.is_empty() {
            return Ok(0); // the terminal reports an end
        }
        let chunk = self.chunks.remove(0);
        buf[..chunk.len()].copy_from_slice(chunk);
        self.taken += 1;
        Ok(chunk.len())
    }
}

#[test]
fn a_blocking_reader_hands_over_every_chunk_until_the_terminal_ends() {
    // The terminal hands out two chunks and then ends. The pump hands both
    // over, in order, and answers `true`.
    let sink = CountingSink::new();
    let (delivery, _handover) = sink_delivery(sink.clone());
    let mut reader = Scripted::new(vec![b"one", b"two"]);

    let reached_the_end = pump_blocking(&mut reader, &delivery, PaneId::new());

    assert!(
        reached_the_end,
        "the terminal ended, so the caller may report the child's exit"
    );
    assert_eq!(
        sink.chunks.lock().expect("counting sink").as_slice(),
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
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            buf[0] = b'x';
            Ok(1)
        }
    }

    let (delivery, handover) = sink_delivery(Arc::new(RefusingSink));

    let reached_the_end = pump_blocking(&mut Endless, &delivery, PaneId::new());

    assert!(
        !reached_the_end,
        "the terminal is still open, so no exit may be reported behind it"
    );
    assert!(
        handover.lock().expect("handover").settled,
        "a refused chunk must stop the reader delivering and settle the pane"
    );
}

#[test]
fn a_reader_told_to_stop_leaves_the_rest_of_the_terminal_unread() {
    // A refused chunk ends the pump where it stands. The chunks behind it stay
    // unread.
    let (delivery, _handover) = sink_delivery(Arc::new(RefusingSink));
    let mut reader = Scripted::new(vec![b"one", b"two", b"three"]);

    let reached_the_end = pump_blocking(&mut reader, &delivery, PaneId::new());

    assert!(
        !reached_the_end,
        "the pump reported an end it never reached"
    );
    assert_eq!(reader.taken, 1, "the pump read past the refused chunk");
    assert_eq!(
        reader.chunks,
        [b"two".as_slice(), b"three".as_slice()],
        "the chunks behind the refused one must be left unread"
    );
}

// Windows only: `drain_terminal` is built there.
#[cfg(windows)]
#[test]
fn draining_a_terminal_reads_it_to_the_end() {
    // The drain reads every chunk to the terminal's end.
    let mut reader = Scripted::new(vec![b"one", b"two", b"three"]);

    drain_terminal(&mut reader);

    assert_eq!(reader.taken, 3, "the drain stopped short of the end");
    assert_eq!(
        reader.chunks.len(),
        0,
        "and left output behind for the close to wait on"
    );
}

#[test]
fn resizing_a_terminal_that_is_already_closed_changes_nothing() {
    // A `Crate` terminal whose master has been taken out takes a resize as
    // `Ok(())`.
    let terminal = Terminal::Crate(Arc::new(Mutex::new(None)));

    assert_eq!(
        terminal.resize(PtySize {
            cols: 132,
            rows: 43
        }),
        Ok(()),
        "a closed terminal must take a resize as a no-op"
    );
}

#[test]
fn a_settled_pane_takes_no_chunk_and_claims_none() {
    // A settled pane refuses the chunk without claiming it: `begun` and `done`
    // stay at 0.
    let sink = CountingSink::new();
    let (delivery, handover) = sink_delivery(sink.clone());
    handover.lock().expect("handover").settled = true;

    assert!(
        !delivery.output(PaneId::new(), b"late"),
        "a settled pane takes no more output"
    );
    assert_eq!(sink.chunk_count(), 0, "and hands the consumer nothing");
    let held = handover.lock().expect("handover");
    assert_eq!(held.begun, 0, "an unclaimed chunk leaves nothing begun");
    assert_eq!(held.done, 0, "and nothing to release");
    assert!(!held.in_flight(), "and nothing reads as in flight");
}

// The reader park and the pane hand-over tests below drive real children in
// real PTYs and are built on Unix only.

/// Standard test window: 80 columns × 24 rows.
#[cfg(unix)]
const PANE_SIZE: PtySize = PtySize { cols: 80, rows: 24 };

/// Serializes PTY creation across the parallel test threads. macOS `openpty(3)`
/// races under concurrent allocation.
#[cfg(unix)]
static PTY_GATE: Mutex<()> = Mutex::new(());

/// A spawn spec running `script` under `/bin/sh`.
#[cfg(unix)]
fn shell_spec(script: &str) -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_string(), script.to_string()],
        cwd: None,
        env: BTreeMap::new(),
        shell_kind: ShellKind::from_program(std::path::Path::new("/bin/sh")),
    }
}

/// A backend delivering to `sink`, with one pane running `script` in a real
/// PTY, and that pane's id.
#[cfg(unix)]
fn backend_running(sink: Arc<CountingSink>, script: &str) -> (Arc<PortablePtyBackend>, PaneId) {
    let backend = Arc::new(PortablePtyBackend::with_sink(sink));
    let pane = PaneId::new();
    let _gate = PTY_GATE.lock().expect("pty gate");
    backend
        .spawn(pane, shell_spec(script), PANE_SIZE)
        .expect("spawn the pane");
    (backend, pane)
}

/// Wait until `sink` holds `needle`, and hand back everything it holds. Fails
/// the test after `HANG_GUARD` if the bytes never arrive.
#[cfg(unix)]
fn read_sink_until(sink: &CountingSink, needle: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        let got = sink.bytes();
        if got.windows(needle.len()).any(|window| window == needle) {
            return got;
        }
        assert!(
            Instant::now() < deadline,
            "the consumer was never handed {:?}; it holds {:?}",
            String::from_utf8_lossy(needle),
            String::from_utf8_lossy(&got)
        );
        thread::sleep(Duration::from_millis(5));
    }
}

/// Pause `backend`'s readers on a thread of its own. A pause that has not
/// settled after `HANG_GUARD` fails the test.
#[cfg(unix)]
fn pause_or_fail(backend: &Arc<PortablePtyBackend>) -> Result<(), PtyError> {
    let (done, done_rx) = channel::<Result<(), PtyError>>();
    let pausing = Arc::clone(backend);
    thread::spawn(move || {
        let _ = done.send(pausing.pause_readers());
    });
    done_rx
        .recv_timeout(HANG_GUARD)
        .expect("pausing the readers never settled")
}

/// Wait until `sink` holds an exit, and hand it back. Fails the test with
/// `never` after `HANG_GUARD`.
#[cfg(unix)]
fn read_sink_until_exit(sink: &CountingSink, never: &str) -> ExitStatus {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        if let Some(exit) = sink.exit_taken() {
            return exit;
        }
        assert!(Instant::now() < deadline, "{never}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Carry `fd` across a process-image swap, in one process: clear its
/// close-on-exec flag, duplicate it, own the duplicate, and set the flag on
/// the duplicate.
#[cfg(unix)]
fn carry_across_the_swap(fd: std::os::fd::RawFd) -> std::os::fd::OwnedFd {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    set_terminal_cloexec(fd, false).expect("clear close-on-exec");
    let survived = unsafe { libc::dup(fd) };
    assert!(survived >= 0, "the descriptor must survive the swap");
    let survived = unsafe { OwnedFd::from_raw_fd(survived) };
    set_terminal_cloexec(survived.as_raw_fd(), true).expect("set close-on-exec");
    survived
}

#[cfg(unix)]
#[test]
fn pausing_holds_a_live_panes_output_and_resuming_releases_it() {
    // The child stays alive for the whole test. Everything read before the
    // pause is with the consumer, nothing reaches the consumer while parked,
    // and resuming hands the held bytes over.
    let sink = CountingSink::new();
    let (backend, pane) = backend_running(sink.clone(), "printf ready; sleep 30");

    read_sink_until(&sink, b"ready");
    let before = sink.bytes();

    pause_or_fail(&backend).expect("pause the readers");
    assert_eq!(
        sink.bytes(),
        before,
        "pausing must neither lose nor invent output"
    );

    // The terminal echoes what is written to it.
    backend.write(pane, b"held\n").expect("write to the pane");
    thread::sleep(EXIT_PUBLISH_GRACE * 3);
    assert_eq!(
        sink.bytes(),
        before,
        "a parked reader must hand the consumer nothing"
    );

    backend.resume_readers();
    let after = read_sink_until(&sink, b"held");
    assert_eq!(
        &after[..before.len()],
        before.as_slice(),
        "resuming must leave what was already delivered untouched"
    );

    backend
        .kill(pane, KillPolicy::Tree)
        .expect("close the pane");
}

#[cfg(unix)]
#[test]
fn a_resumed_reader_is_not_left_in_its_exit_rounds() {
    // A pause and a resume, then a wait past the limit. The reader is still
    // pumping: a line written afterwards comes back.
    let sink = CountingSink::new();
    let (backend, pane) = backend_running(sink.clone(), "printf ready; sleep 30");

    read_sink_until(&sink, b"ready");
    pause_or_fail(&backend).expect("pause the readers");
    backend.resume_readers();

    thread::sleep(EXIT_PUBLISH_LIMIT + EXIT_PUBLISH_GRACE * 2);

    backend.write(pane, b"again\n").expect("write to the pane");
    read_sink_until(&sink, b"again");

    backend
        .kill(pane, KillPolicy::Tree)
        .expect("close the pane");
}

#[cfg(unix)]
#[test]
fn pausing_settles_when_a_panes_child_has_already_ended() {
    // The pane's child has ended and its reader has left its pump. The pause
    // settles with `live` at 0.
    let sink = CountingSink::new();
    let (backend, pane) = backend_running(sink.clone(), "printf bye");

    assert_eq!(
        read_sink_until_exit(&sink, "the child's exit never arrived"),
        ExitStatus::ExitCode(0),
        "the pane's child ran to a clean exit"
    );

    pause_or_fail(&backend).expect("pause the readers");
    assert_eq!(
        backend.readers.state.lock().expect("reader gate").live,
        0,
        "a reader that left its pump must no longer be counted"
    );

    backend.resume_readers();
    backend
        .kill(pane, KillPolicy::Force)
        .expect("close the pane");
}

#[cfg(unix)]
#[test]
fn a_reader_waiting_for_an_exit_that_never_comes_is_no_longer_counted() {
    // The terminal ends while the pane is unsettled and no exit is published.
    // The reader waits inside `finish`, and leaves the gate before that wait:
    // `live` drops to 0 and `parked` stays 0.
    let (far, terminal) = fake_terminal();
    let gate = Arc::new(ReaderGate::new());

    // Held for the whole test and never sent on.
    let (exit_sender, exit_receiver) = channel::<ExitStatus>();
    let delivery = Delivery::Sink {
        sink: CountingSink::new(),
        exit_receiver,
        handover: Arc::new(Mutex::new(Handover::default())),
    };

    let ticket = gate.enter();
    assert_eq!(
        gate.state.lock().expect("reader gate").live,
        1,
        "a reader is counted from before its thread starts"
    );

    let _reader = start_owned_reader(
        delivery,
        PaneId::new(),
        Arc::new(terminal),
        Arc::new(Waker::new().expect("waker")),
        Arc::new(AtomicBool::new(false)),
        ticket,
    );

    drop(far); // the terminal reports an end

    let deadline = Instant::now() + HANG_GUARD;
    while gate.state.lock().expect("reader gate").live != 0 {
        assert!(
            Instant::now() < deadline,
            "a reader past its pump is still counted, so a pause would never settle"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        gate.state.lock().expect("reader gate").parked,
        0,
        "and it must not read as parked either"
    );

    drop(exit_sender); // release the reader from its wait
}

#[cfg(unix)]
#[test]
fn a_pane_whose_reader_cannot_park_refuses_the_pause() {
    // A pane with no doorbell (`reader_wake: None`) refuses the pause by name,
    // and the gate stays open.
    let backend = PortablePtyBackend::new();
    let pane = PaneId::new();
    let (writer, _writer_rx) = channel::<WriterMsg>();
    backend.panes.lock().expect("panes").insert(
        pane,
        pane_entry(Terminal::Crate(Arc::new(Mutex::new(None))), writer),
    );

    let refused = backend
        .pause_readers()
        .expect_err("a reader that cannot park must refuse the pause");

    assert_eq!(
        refused,
        PtyError::Io {
            detail: format!("pane {pane} has no terminal descriptor, so its reader cannot park"),
        },
        "the refusal must name the pane that cannot be paused"
    );
    assert!(
        !backend.readers.state.lock().expect("reader gate").paused,
        "a refused pause must leave the gate as it was"
    );
}

/// A terminal that takes no write until it is let go: each write waits for one
/// `release`.
struct HeldTerminal {
    /// One value here lets one write land.
    release: Receiver<()>,
    /// Everything the writes have landed, in order.
    written: Arc<Mutex<Vec<u8>>>,
}

impl Write for HeldTerminal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.release.recv();
        self.written.lock().expect("written").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_writer_answers_a_barrier_only_after_writing_what_came_before() {
    // A barrier queued behind a write is answered only after that write lands.
    let written = Arc::new(Mutex::new(Vec::new()));
    let (release, release_rx) = channel::<()>();
    let writer = start_writer(WriteSide::Crate(Box::new(HeldTerminal {
        release: release_rx,
        written: Arc::clone(&written),
    })));

    writer
        .send(WriterMsg::Bytes(b"typed".to_vec()))
        .expect("queue the input");
    let (answer, answer_rx) = channel::<()>();
    writer
        .send(WriterMsg::Barrier(answer))
        .expect("queue the barrier");
    assert_eq!(
        answer_rx.recv_timeout(Duration::from_millis(100)),
        Err(RecvTimeoutError::Timeout),
        "a barrier must not be answered while the write before it is under way"
    );

    release.send(()).expect("let the write land");
    assert_eq!(
        answer_rx.recv_timeout(HANG_GUARD),
        Ok(()),
        "the barrier must be answered once the write has landed"
    );
    assert_eq!(
        written.lock().expect("written").as_slice(),
        b"typed",
        "an answered barrier must mean every byte queued before it is written"
    );

    let _ = writer.send(WriterMsg::Stop);
}

/// A pane entry around `terminal` and `writer` with no child of its own. Its
/// process id is this test process, its child is marked exited, and its size
/// is [`PANE_SIZE`].
#[cfg(unix)]
fn pane_entry(terminal: Terminal, writer: Sender<WriterMsg>) -> PaneEntry {
    let (exit_grace_cancel, _cancel_rx) = channel::<()>();
    PaneEntry {
        terminal,
        size: PANE_SIZE,
        writer,
        // This test process's own id.
        killer: PtyChildKillControl::new(std::process::id()),
        exited: Arc::new(AtomicBool::new(true)),
        exit: Arc::new(OnceLock::new()),
        handover: Arc::new(Mutex::new(Handover::default())),
        exit_grace_cancel,
        reader: spawn_pty_thread("koshi-pty-read", || {}),
        reader_wake: None,
        watcher: spawn_pty_thread("koshi-pty-watch", || {}),
    }
}

#[cfg(unix)]
#[test]
fn a_flush_answers_only_once_the_terminal_holds_every_byte_the_backend_took() {
    // A flush that answers `Ok` leaves every byte the backend took on the
    // terminal.
    let (far, near) = std::os::unix::net::UnixStream::pair().expect("terminal pair");
    let terminal = Arc::new(std::os::fd::OwnedFd::from(near));
    let backend = PortablePtyBackend::new();
    let pane = PaneId::new();
    let writer = start_writer(WriteSide::Owned(Arc::clone(&terminal)));
    backend
        .panes
        .lock()
        .expect("panes")
        .insert(pane, pane_entry(Terminal::Owned(terminal), writer));

    backend.write(pane, b"typed").expect("write to the pane");
    backend.flush_writers().expect("the flush answers");

    far.set_nonblocking(true).expect("nonblocking");
    let mut got = [0u8; 32];
    let read = (&far).read(&mut got).expect("the child reads its input");
    assert_eq!(
        &got[..read],
        b"typed",
        "a flush that answered left a byte the backend took unwritten"
    );
}

#[cfg(unix)]
#[test]
fn a_pane_whose_writer_cannot_finish_refuses_the_flush() {
    // The pane's writer is blocked inside its write. The flush refuses after
    // its limit and names that pane.
    let (release, release_rx) = channel::<()>();
    let backend = PortablePtyBackend::new();
    let pane = PaneId::new();
    let writer = start_writer(WriteSide::Crate(Box::new(HeldTerminal {
        release: release_rx,
        written: Arc::new(Mutex::new(Vec::new())),
    })));
    backend.panes.lock().expect("panes").insert(
        pane,
        pane_entry(Terminal::Crate(Arc::new(Mutex::new(None))), writer),
    );

    backend.write(pane, b"typed").expect("write to the pane");
    let refused = backend
        .flush_writers()
        .expect_err("a writer that cannot finish must refuse the flush");

    assert_eq!(
        refused,
        PtyError::Io {
            detail: format!("pane {pane} is still writing what it was handed, so it cannot settle"),
        },
        "the refusal must name the pane that is still being written to"
    );

    release.send(()).expect("let the write land");
}

#[cfg(unix)]
#[test]
fn setting_close_on_exec_touches_only_that_one_flag() {
    // `FD_CLOEXEC` is the only descriptor flag; the whole flag word is read
    // back.
    use std::os::fd::AsRawFd;

    let (_far, near) = std::os::unix::net::UnixStream::pair().expect("terminal pair");
    let near = std::os::fd::OwnedFd::from(near);
    let fd = near.as_raw_fd();
    let flags = || unsafe { libc::fcntl(fd, libc::F_GETFD) };

    set_terminal_cloexec(fd, false).expect("clear close-on-exec");
    assert_eq!(flags(), 0, "clearing must leave no descriptor flag set");

    set_terminal_cloexec(fd, true).expect("set close-on-exec");
    assert_eq!(
        flags(),
        libc::FD_CLOEXEC,
        "setting must leave close-on-exec and nothing else"
    );

    set_terminal_cloexec(fd, true).expect("set close-on-exec again");
    assert_eq!(
        flags(),
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

    let sink = CountingSink::new();
    let (backend, pane) = backend_running(sink, "sleep 30");

    let resized = PtySize {
        cols: 132,
        rows: 43,
    };
    backend.resize(pane, resized).expect("resize the pane");

    let carried = backend.carried_panes();
    assert_eq!(carried.len(), 1, "one live pane must give one record");
    assert_eq!(carried[0].pane_id, pane, "the record must name the pane");
    assert_eq!(
        carried[0].size, resized,
        "a carried pane must report the size it was last resized to"
    );
    assert!(
        process_alive(carried[0].pid),
        "a carried pane must name its child's running process"
    );

    // The window size read back through the carried descriptor is the resized
    // one.
    let fd = carried[0]
        .terminal_fd
        .expect("a real pty exposes a descriptor");
    let mut window: libc::winsize = unsafe { std::mem::zeroed() };
    let read_back = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut window) };
    assert_eq!(read_back, 0, "read the window size back");
    assert_eq!(
        (window.ws_col, window.ws_row),
        (132, 43),
        "the carried descriptor must name the pane's own terminal"
    );
    assert_eq!(
        fd,
        match &backend.panes.lock().expect("panes")[&pane].terminal {
            Terminal::Owned(owned) => owned.as_raw_fd(),
            Terminal::Crate(_) => panic!("a real pty must own its descriptor"),
        },
        "the record must carry the pane's own descriptor"
    );

    backend
        .kill(pane, KillPolicy::Tree)
        .expect("close the pane");
}

#[cfg(unix)]
#[test]
fn a_paused_panes_terminal_can_be_taken_back_by_another_backend() {
    // The swap in one process: pause, carry the descriptor and the process id,
    // take the pane back. The taken-back pane drives the same child.
    let old_sink = CountingSink::new();
    let (old, pane) = backend_running(old_sink.clone(), "printf ready; sleep 30");

    read_sink_until(&old_sink, b"ready");
    let before = old_sink.bytes();
    pause_or_fail(&old).expect("pause the readers");

    let carried = old.carried_panes();
    assert_eq!(carried.len(), 1, "one live pane must give one record");
    let carried = carried[0];
    assert_eq!(carried.pane_id, pane, "the record must name the pane");
    let survived = carry_across_the_swap(
        carried
            .terminal_fd
            .expect("a real pty exposes a descriptor"),
    );

    let new_sink = CountingSink::new();
    let new = PortablePtyBackend::with_sink(new_sink.clone());
    new.adopt(pane, survived, carried.pid, carried.size, carried.exit)
        .expect("take the pane back");

    new.write(pane, b"adopted\n").expect("write to the pane");
    read_sink_until(&new_sink, b"adopted");
    assert_eq!(
        old_sink.bytes(),
        before,
        "the parked reader must take none of the bytes the new pane is owed"
    );

    // Closing the old pane kills the child both panes drive, and the taken-back
    // pane's threads end with it.
    old.resume_readers();
    old.kill(pane, KillPolicy::Tree).expect("close the pane");
}

#[cfg(unix)]
#[test]
fn a_pane_taken_back_after_its_child_ended_still_publishes_the_exit() {
    // The child has ended, unreaped, before the pane is taken back. The
    // taken-back pane's watcher reaps it and publishes `ExitCode(7)`.
    let (terminal, pid) = {
        let _gate = PTY_GATE.lock().expect("pty gate");
        let pair = native_pty_system()
            .openpty(to_pp_size(PANE_SIZE))
            .expect("openpty");
        let terminal = own_terminal_fd(&*pair.master).expect("terminal descriptor");
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.arg("-c");
        cmd.arg("exit 7");
        let child = pair.slave.spawn_command(cmd).expect("spawn");
        drop(pair.slave);
        let pid = child.process_id().expect("pid");
        // Left unreaped.
        drop(child);
        (terminal, pid)
    };

    let sink = CountingSink::new();
    let backend = PortablePtyBackend::with_sink(sink.clone());
    let pane = PaneId::new();
    backend
        .adopt(pane, terminal, pid, PANE_SIZE, None)
        .expect("take the pane back");

    assert_eq!(
        read_sink_until_exit(&sink, "a taken-back pane never published its child's exit"),
        ExitStatus::ExitCode(7),
        "the exit the child really ended with must reach the consumer"
    );
}

#[cfg(unix)]
#[test]
fn a_child_that_ends_while_the_readers_are_held_keeps_the_code_it_ended_with() {
    // The child ends while the readers are held. The old backend's watcher
    // reaps it, the carried record holds `ExitCode(3)`, and the taken-back
    // pane publishes that status once.
    let old_sink = CountingSink::new();
    // The child prints, then ends after one second.
    let (old, pane) = backend_running(old_sink.clone(), "printf ready; sleep 1; exit 3");

    read_sink_until(&old_sink, b"ready");
    pause_or_fail(&old).expect("hold the readers still");

    // The old backend's watcher reaps the child while its reader is held.
    let deadline = Instant::now() + HANG_GUARD;
    let carried = loop {
        let carried = old.carried_panes();
        assert_eq!(carried.len(), 1, "one live pane must give one record");
        if carried[0].exit.is_some() {
            break carried[0];
        }
        assert!(
            Instant::now() < deadline,
            "the held pane's child was never reaped"
        );
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(carried.pane_id, pane, "the record must name the pane");
    assert_eq!(
        carried.exit,
        Some(ExitStatus::ExitCode(3)),
        "a carried pane must report the exit its own watcher observed"
    );

    let survived = carry_across_the_swap(
        carried
            .terminal_fd
            .expect("a real pty exposes a descriptor"),
    );

    let new_sink = CountingSink::new();
    let new = PortablePtyBackend::with_sink(new_sink.clone());
    new.adopt(pane, survived, carried.pid, carried.size, carried.exit)
        .expect("take the pane back");

    assert_eq!(
        read_sink_until_exit(
            &new_sink,
            "the taken-back pane never published its child's exit"
        ),
        ExitStatus::ExitCode(3),
        "the taken-back pane must report the code the child really ended with"
    );
    assert_eq!(
        new_sink.exit_count(),
        1,
        "and it must report it exactly once"
    );

    old.resume_readers();
    old.kill(pane, KillPolicy::Tree).expect("close the pane");
}

#[cfg(unix)]
#[test]
fn holding_the_readers_still_twice_settles_both_times_and_one_release_frees_them() {
    // Two pauses in a row both settle; the one reader parks once. One resume
    // frees it.
    let sink = CountingSink::new();
    let (backend, pane) = backend_running(sink.clone(), "printf ready; sleep 30");

    read_sink_until(&sink, b"ready");
    let before = sink.bytes();

    pause_or_fail(&backend).expect("hold the readers still");
    pause_or_fail(&backend).expect("hold the readers still a second time");

    assert!(
        backend.readers.state.lock().expect("reader gate").paused,
        "the gate stays shut after the second hold"
    );
    assert_eq!(
        backend.readers.state.lock().expect("reader gate").parked,
        1,
        "the one reader parks once, however many holds asked for it"
    );
    assert_eq!(
        backend.readers.state.lock().expect("reader gate").live,
        1,
        "and it is still the one counted reader"
    );

    backend.write(pane, b"held\n").expect("write to the pane");
    thread::sleep(EXIT_PUBLISH_GRACE * 3);
    assert_eq!(
        sink.bytes(),
        before,
        "a reader held twice hands the consumer nothing"
    );

    backend.resume_readers();
    let after = read_sink_until(&sink, b"held");
    assert_eq!(
        &after[..before.len()],
        before.as_slice(),
        "one release must free a reader two holds parked"
    );
    assert_eq!(
        backend.readers.state.lock().expect("reader gate").parked,
        0,
        "and must leave nobody at the park"
    );

    backend
        .kill(pane, KillPolicy::Tree)
        .expect("close the pane");
}

#[cfg(unix)]
#[test]
fn releasing_readers_nobody_held_leaves_a_live_pane_printing() {
    // A resume with nobody parked leaves the gate open and the pane printing.
    let sink = CountingSink::new();
    let (backend, pane) = backend_running(sink.clone(), "printf ready; sleep 30");

    read_sink_until(&sink, b"ready");
    backend.resume_readers();

    assert!(
        !backend.readers.state.lock().expect("reader gate").paused,
        "the gate was never shut and stays open"
    );
    assert_eq!(
        backend.readers.state.lock().expect("reader gate").parked,
        0,
        "and holds nobody"
    );

    backend
        .write(pane, b"still here\n")
        .expect("write to the pane");
    read_sink_until(&sink, b"still here");

    backend
        .kill(pane, KillPolicy::Tree)
        .expect("close the pane");
}

#[cfg(unix)]
#[test]
fn a_child_that_ends_while_its_reader_is_held_publishes_its_exit_once_on_release() {
    // The child ends while the reader is parked. No exit is published while
    // held; the resume publishes `ExitCode(3)` exactly once.
    let sink = CountingSink::new();
    let (backend, pane) = backend_running(sink.clone(), "printf ready; sleep 2; exit 3");

    read_sink_until(&sink, b"ready");
    pause_or_fail(&backend).expect("hold the readers still");
    assert_eq!(
        backend.readers.state.lock().expect("reader gate").parked,
        1,
        "the reader parks well before its child's two-second sleep runs out"
    );

    let deadline = Instant::now() + HANG_GUARD;
    loop {
        let reaped = backend.panes.lock().expect("panes")[&pane]
            .exited
            .load(Ordering::SeqCst);
        if reaped {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the pane's child never ended while its reader was held"
        );
        thread::sleep(Duration::from_millis(5));
    }

    assert_eq!(
        sink.exit_count(),
        0,
        "a held reader must publish no exit, however long its child has been gone"
    );
    assert_eq!(
        backend.readers.state.lock().expect("reader gate").parked,
        1,
        "and the reader must still be at the park"
    );

    backend.resume_readers();

    assert_eq!(
        read_sink_until_exit(
            &sink,
            "the released reader never published the exit it was holding"
        ),
        ExitStatus::ExitCode(3),
        "the status the child really ended with must reach the consumer"
    );

    // The wait gives a second publish time to arrive.
    thread::sleep(EXIT_PUBLISH_GRACE * 3);
    assert_eq!(sink.exit_count(), 1, "and it must arrive exactly once");
}

#[cfg(unix)]
#[test]
fn input_written_while_the_readers_are_held_reaches_the_child_and_its_answer_comes_on_release() {
    // Input written while the readers are held reaches the child, and the
    // child answers. The answer and the exit reach the consumer on resume.
    let sink = CountingSink::new();
    let (backend, pane) = backend_running(
        sink.clone(),
        "printf ready; read go; printf 'took %s' \"$go\"; exit 3",
    );

    read_sink_until(&sink, b"ready");
    let before = sink.bytes();
    pause_or_fail(&backend).expect("hold the readers still");

    backend.write(pane, b"go\n").expect("write to the pane");
    // The writers settle while the readers are held.
    backend
        .flush_writers()
        .expect("the held readers must leave the writers free to settle");

    thread::sleep(EXIT_PUBLISH_GRACE * 3);
    assert_eq!(
        sink.bytes(),
        before,
        "a held reader hands the consumer nothing, however much its child printed"
    );
    assert_eq!(
        sink.exit_count(),
        0,
        "and publishes no exit while it is held"
    );

    backend.resume_readers();

    assert_eq!(
        read_sink_until_exit(
            &sink,
            "the released reader never published the exit it was holding"
        ),
        ExitStatus::ExitCode(3),
        "the child read the line and ended with the status that line asks for"
    );
    assert_eq!(
        String::from_utf8_lossy(&sink.bytes()).into_owned(),
        "readygo\r\ntook go",
        "the release hands over the echo of the input and what the child printed from it"
    );
}

/// The numbers a bursting child printed, in the order they reached a sink.
/// Panics on a non-empty line that is not one number.
#[cfg(unix)]
fn printed_numbers(bytes: &[u8]) -> Vec<u32> {
    String::from_utf8_lossy(bytes)
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
const BURST_LINES: u32 = 1500;

#[cfg(unix)]
#[test]
fn every_byte_a_child_is_printing_crosses_the_hand_over_once_and_in_order() {
    // The child is mid-burst when the readers are held and the pane is taken
    // back. The two sinks end to end hold every line once, in order.
    let old_sink = CountingSink::new();
    let (old, pane) = backend_running(
        old_sink.clone(),
        &format!("i=1; while [ $i -le {BURST_LINES} ]; do printf '%04d\\n' $i; i=$((i+1)); done; sleep 30"),
    );

    // The child is printing by the time the first line lands.
    read_sink_until(&old_sink, b"0001");
    pause_or_fail(&old).expect("hold the readers still");
    let before = old_sink.bytes();

    let carried = old.carried_panes();
    assert_eq!(carried.len(), 1, "one live pane must give one record");
    let carried = carried[0];
    assert_eq!(carried.pane_id, pane, "the record must name the pane");
    let survived = carry_across_the_swap(
        carried
            .terminal_fd
            .expect("a real pty exposes a descriptor"),
    );

    let new_sink = CountingSink::new();
    let new = PortablePtyBackend::with_sink(new_sink.clone());
    new.adopt(pane, survived, carried.pid, carried.size, carried.exit)
        .expect("take the pane back");

    // The two sinks end to end are everything the consumer was handed.
    // `before` never grows.
    let last = format!("{BURST_LINES:04}");
    let whole = {
        let deadline = Instant::now() + HANG_GUARD * 4;
        loop {
            let mut whole = before.clone();
            whole.extend_from_slice(&new_sink.bytes());
            if whole
                .windows(last.len())
                .any(|window| window == last.as_bytes())
            {
                break whole;
            }
            assert!(
                Instant::now() < deadline,
                "the child's last line never reached the consumer; it holds {} bytes",
                whole.len()
            );
            thread::sleep(Duration::from_millis(5));
        }
    };

    assert_eq!(
        old_sink.bytes(),
        before,
        "the held reader must take none of the bytes the taken-back pane is owed"
    );

    let printed = printed_numbers(&whole);
    assert_eq!(
        printed,
        (1..=BURST_LINES).collect::<Vec<u32>>(),
        "every line the child printed must cross the hand-over once, in the order it printed them"
    );

    old.resume_readers();
    old.kill(pane, KillPolicy::Tree).expect("close the pane");
}

#[cfg(unix)]
#[test]
fn a_pane_taken_back_on_a_terminal_already_at_its_end_reports_its_child_gone() {
    // The terminal is already at its end and the process id is nobody's child.
    // The taken-back pane publishes `ExitCode(-1)` once and hands over no
    // output.
    let (far, terminal) = fake_terminal();
    drop(far); // the terminal is already at its end

    let sink = CountingSink::new();
    let backend = PortablePtyBackend::with_sink(sink.clone());
    let pane = PaneId::new();

    // A process id this process has no child under: `waitpid` answers
    // `ECHILD`.
    let handle = backend
        .adopt(pane, terminal, u32::from(u16::MAX), PANE_SIZE, None)
        .expect("take the pane back");
    assert_eq!(handle.pane_id(), pane, "the handle must name the pane");

    assert_eq!(
        read_sink_until_exit(
            &sink,
            "a pane taken back on a dead terminal never reported its child gone"
        ),
        ExitStatus::ExitCode(-1),
        "a child that cannot be waited on reports the unobserved status"
    );
    assert_eq!(sink.exit_count(), 1, "and it reports it exactly once");
    assert_eq!(
        sink.chunk_count(),
        0,
        "a dead terminal hands over no output"
    );
}

#[cfg(unix)]
#[test]
fn a_descriptor_this_process_does_not_hold_refuses_the_close_on_exec_change() {
    // A descriptor number this process does not hold refuses both the set and
    // the clear with `EBADF`.
    let never_open = libc::c_int::MAX;
    let checked = unsafe { libc::fcntl(never_open, libc::F_GETFD) };
    assert_eq!(checked, -1, "the descriptor under test must not be open");

    let refused = set_terminal_cloexec(never_open, true)
        .expect_err("a descriptor this process does not hold must refuse the change");
    assert_eq!(
        refused.raw_os_error(),
        Some(libc::EBADF),
        "and must name the descriptor as bad"
    );

    let refused = set_terminal_cloexec(never_open, false)
        .expect_err("clearing the flag must refuse it the same way");
    assert_eq!(refused.raw_os_error(), Some(libc::EBADF));
}

// --- the terminal's opening cursor question ---

/// A terminal that hands back one of `chunks` per read, then reports its end.
/// A read never spans two chunks.
struct ChunkedTerminal {
    /// What each read hands back, in order.
    chunks: Vec<Vec<u8>>,
    /// How many chunks have been read.
    read: usize,
}

impl ChunkedTerminal {
    fn new(chunks: &[&[u8]]) -> ChunkedTerminal {
        ChunkedTerminal {
            chunks: chunks.iter().map(|chunk| chunk.to_vec()).collect(),
            read: 0,
        }
    }
}

impl Read for ChunkedTerminal {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let Some(chunk) = self.chunks.get(self.read) else {
            return Ok(0);
        };
        self.read += 1;
        let take = chunk.len().min(buf.len());
        buf[..take].copy_from_slice(&chunk[..take]);
        Ok(take)
    }
}

/// Read `chunks` through the reader that takes the request out, and hand back
/// what it delivered as output.
fn through_the_stripping_reader(chunks: &[&[u8]]) -> Vec<u8> {
    let mut reader = RemovesCursorRequest::new(ChunkedTerminal::new(chunks));

    let mut delivered = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        match reader
            .read(&mut buf)
            .expect("the terminal hands back its bytes")
        {
            0 => break,
            read => delivered.extend_from_slice(&buf[..read]),
        }
    }
    delivered
}

#[test]
fn the_terminals_opening_question_is_taken_out_of_the_output() {
    assert_eq!(
        through_the_stripping_reader(&[CURSOR_REQUEST, b"hello"]),
        b"hello"
    );
}

#[test]
fn an_opening_question_split_across_two_reads_is_taken_out_once() {
    assert_eq!(
        through_the_stripping_reader(&[b"\x1b[", b"6nhello"]),
        b"hello"
    );
}

#[test]
fn an_opening_question_with_nothing_behind_it_ends_the_output() {
    assert_eq!(through_the_stripping_reader(&[b"\x1b[6", b"n"]), b"");
}

#[test]
fn output_that_is_not_the_question_is_delivered_whole() {
    assert_eq!(
        through_the_stripping_reader(&[b"hello world"]),
        b"hello world"
    );
}

#[test]
fn a_question_the_panes_own_program_asks_is_left_for_the_terminal_engine() {
    // The second request on the stream: the first was the terminal's and is
    // already taken out.
    assert_eq!(
        through_the_stripping_reader(&[CURSOR_REQUEST, b"hello", CURSOR_REQUEST]),
        b"hello\x1b[6n"
    );
}

#[test]
fn the_terminals_question_is_taken_out_behind_output_that_came_first() {
    // Output ahead of the request is delivered, and the request behind it is
    // taken out.
    assert_eq!(
        through_the_stripping_reader(&[b"\x1b[?25l", CURSOR_REQUEST, b"hi"]),
        b"\x1b[?25lhi"
    );
}

#[test]
fn a_half_question_the_terminal_never_finishes_is_delivered_as_output() {
    // Held back while it could still become the request, then delivered when
    // the terminal ends.
    assert_eq!(through_the_stripping_reader(&[b"\x1b["]), b"\x1b[");
}

#[test]
fn a_start_that_turns_out_not_to_be_the_question_is_delivered() {
    // `\x1b[` is held back, then delivered once `?25h` shows it is not the
    // request.
    assert_eq!(
        through_the_stripping_reader(&[b"\x1b[", b"?25hhi"]),
        b"\x1b[?25hhi"
    );
}

#[test]
fn the_watch_outlasts_a_false_start() {
    assert_eq!(
        through_the_stripping_reader(&[b"\x1b[", b"?25h", CURSOR_REQUEST, b"hi"]),
        b"\x1b[?25hhi"
    );
}

#[test]
fn an_opening_question_split_across_three_reads_is_taken_out_once() {
    assert_eq!(
        through_the_stripping_reader(&[b"\x1b", b"[6", b"nhi"]),
        b"hi"
    );
}

#[test]
fn position_of_finds_the_first_request_only() {
    assert_eq!(position_of(b"ab\x1b[6ncd\x1b[6n", CURSOR_REQUEST), Some(2));
    assert_eq!(position_of(b"abc", CURSOR_REQUEST), None);
    assert_eq!(position_of(b"", CURSOR_REQUEST), None);
}

#[test]
fn partial_tail_counts_the_longest_start_of_the_request_at_the_end() {
    assert_eq!(partial_tail(b"hello\x1b[", CURSOR_REQUEST), 2);
    assert_eq!(partial_tail(b"\x1b[6", CURSOR_REQUEST), 3);
    assert_eq!(partial_tail(b"\x1b", CURSOR_REQUEST), 1);
    assert_eq!(partial_tail(b"hello", CURSOR_REQUEST), 0);
    assert_eq!(partial_tail(b"", CURSOR_REQUEST), 0);
}

#[test]
fn partial_tail_never_counts_the_whole_request() {
    assert_eq!(partial_tail(CURSOR_REQUEST, CURSOR_REQUEST), 0);
    assert_eq!(partial_tail(b"hi\x1b[6n", CURSOR_REQUEST), 0);
}

// --- what a pane records, and what it answers about its child ---

#[test]
fn a_pane_this_backend_does_not_hold_is_refused_by_every_call() {
    let backend = PortablePtyBackend::new();
    let pane = PaneId::new();

    assert_eq!(
        backend.write(pane, b"typed"),
        Err(PtyError::UnknownPane { pane })
    );
    assert_eq!(
        backend.resize(pane, PtySize { cols: 80, rows: 24 }),
        Err(PtyError::UnknownPane { pane })
    );
    assert_eq!(
        backend.kill(pane, KillPolicy::Force),
        Err(PtyError::UnknownPane { pane })
    );
    assert_eq!(backend.child_pid(pane), None);
    assert_eq!(backend.live_cwd(pane), None);
}

#[test]
fn a_backend_with_no_panes_carries_nothing_and_flushes_at_once() {
    let backend = PortablePtyBackend::new();

    assert_eq!(backend.carried_panes(), Vec::new());

    let started = Instant::now();
    assert_eq!(backend.flush_writers(), Ok(()));
    assert!(
        started.elapsed() < WRITER_FLUSH_LIMIT,
        "a flush with no writer to wait for must not spend the limit"
    );
}

#[cfg(unix)]
#[test]
fn pausing_a_backend_with_no_readers_settles_at_once() {
    let backend = Arc::new(PortablePtyBackend::new());

    assert_eq!(pause_or_fail(&backend), Ok(()));
    assert!(
        backend.readers.state.lock().expect("reader gate").paused,
        "the gate is shut after the pause"
    );

    backend.resume_readers();
    assert!(
        !backend.readers.state.lock().expect("reader gate").paused,
        "the gate is open after the resume"
    );
}

#[cfg(unix)]
#[test]
fn a_reader_passes_an_open_gate_without_parking() {
    let gate = ReaderGate::new();

    gate.park_if_paused();

    let state = gate.state.lock().expect("reader gate");
    assert_eq!((state.paused, state.parked, state.live), (false, 0, 0));
}

#[cfg(unix)]
#[test]
fn a_reader_that_leaves_its_pump_settles_a_pause_waiting_for_it() {
    // One reader is counted and never parks. The pause waits until that
    // reader's ticket is dropped.
    let gate = Arc::new(ReaderGate::new());
    let ticket = gate.enter();
    gate.pause();

    let (done, done_rx) = channel::<()>();
    let waiting = Arc::clone(&gate);
    thread::spawn(move || {
        waiting.wait_all_parked();
        let _ = done.send(());
    });

    assert_eq!(
        done_rx.recv_timeout(Duration::from_millis(100)),
        Err(RecvTimeoutError::Timeout),
        "a pause must wait for a counted reader that has not parked"
    );
    drop(ticket);
    assert_eq!(
        done_rx.recv_timeout(HANG_GUARD),
        Ok(()),
        "a reader that left its pump must settle the pause"
    );
    assert_eq!(
        gate.state.lock().expect("reader gate").live,
        0,
        "a dropped ticket counts its reader out"
    );
}

#[cfg(unix)]
#[test]
// `wait_for_child` is the reap under test.
#[expect(clippy::zombie_processes)]
fn wait_for_child_reports_the_code_the_child_ended_with() {
    let child = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 5"])
        .spawn()
        .expect("spawn");

    assert_eq!(wait_for_child(child.id()), ExitStatus::ExitCode(5));
}

#[cfg(unix)]
#[test]
fn child_pid_names_the_panes_child() {
    let backend = PortablePtyBackend::new();
    let pane = PaneId::new();
    let (writer, _writer_rx) = channel::<WriterMsg>();
    backend.panes.lock().expect("panes").insert(
        pane,
        pane_entry(Terminal::Crate(Arc::new(Mutex::new(None))), writer),
    );

    assert_eq!(backend.child_pid(pane), Some(std::process::id()));
}

#[cfg(unix)]
#[test]
fn resizing_through_the_crates_master_retunes_the_terminal() {
    let pair = {
        let _gate = PTY_GATE.lock().expect("pty gate");
        native_pty_system()
            .openpty(to_pp_size(PANE_SIZE))
            .expect("openpty")
    };
    let slot = Arc::new(Mutex::new(Some(pair.master)));
    let terminal = Terminal::Crate(Arc::clone(&slot));

    assert_eq!(
        terminal.resize(PtySize {
            cols: 132,
            rows: 43
        }),
        Ok(())
    );

    let got = slot
        .lock()
        .expect("terminal")
        .as_ref()
        .expect("the master stays in its slot")
        .get_size()
        .expect("read the size back");
    assert_eq!((got.cols, got.rows), (132, 43));
}

#[test]
fn a_channel_consumer_that_dropped_its_handle_stops_its_readers_pump() {
    // A channel delivery answers `true` while the handle is held and `false`
    // once it is dropped.
    let pane = PaneId::new();
    let (handle, output, _exit) = PtyHandle::new(pane);
    let delivery = Delivery::Channel(output);

    assert!(
        delivery.output(pane, b"printed"),
        "a handle the caller still holds must keep the pump running"
    );
    assert_eq!(
        handle.try_read_output(),
        Some(b"printed".to_vec()),
        "the chunk must reach the handle unchanged"
    );

    drop(handle);
    assert!(
        !delivery.output(pane, b"more"),
        "a handle the caller has let go must stop the pump"
    );
}

#[cfg(unix)]
#[test]
fn a_resize_the_kernel_refuses_leaves_the_pane_at_its_old_size() {
    // A resize the kernel refuses reaches the caller as `PtyError::Io`, and
    // the pane keeps its old size.
    let bigger = PtySize {
        cols: 132,
        rows: 43,
    };
    let (_far, socket) = fake_terminal();
    let terminal = Arc::new(socket);
    let backend = PortablePtyBackend::new();
    let pane = PaneId::new();
    let (writer, writer_rx) = channel::<WriterMsg>();
    drop(writer_rx);
    backend.panes.lock().expect("panes").insert(
        pane,
        pane_entry(Terminal::Owned(Arc::clone(&terminal)), writer),
    );

    // The kernel's own error text on this system.
    let refused = resize_terminal(&terminal, bigger)
        .expect_err("a socket is not a terminal and takes no window size");

    assert_eq!(
        backend.resize(pane, bigger),
        Err(PtyError::Io {
            detail: refused.to_string(),
        }),
        "the kernel's refusal must reach the caller as it stands"
    );
    assert_eq!(
        backend.carried_panes().first().map(|carried| carried.size),
        Some(PANE_SIZE),
        "a pane whose child was never told the new size must still report the old one"
    );
}

#[cfg(unix)]
#[test]
fn a_pane_whose_writer_has_already_ended_does_not_hold_up_the_flush() {
    // Two panes with no writer left: one whose channel is closed, one whose
    // writer takes the barrier and ends without answering it. The flush
    // answers `Ok(())`.
    let backend = PortablePtyBackend::new();

    // The barrier cannot be queued for this pane.
    let closed = PaneId::new();
    let (closed_writer, closed_rx) = channel::<WriterMsg>();
    drop(closed_rx);

    // This pane's writer takes the barrier and ends without answering it.
    let ending = PaneId::new();
    let (ending_writer, ending_rx) = channel::<WriterMsg>();
    let ended = spawn_pty_thread("koshi-pty-write", move || {
        let _ = ending_rx.recv();
    });

    {
        let mut panes = backend.panes.lock().expect("panes");
        panes.insert(
            closed,
            pane_entry(Terminal::Crate(Arc::new(Mutex::new(None))), closed_writer),
        );
        panes.insert(
            ending,
            pane_entry(Terminal::Crate(Arc::new(Mutex::new(None))), ending_writer),
        );
    }

    assert_eq!(
        backend.flush_writers(),
        Ok(()),
        "a writer that has already ended must not refuse the flush"
    );
    ended.join().expect("the writer thread ends");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn only_a_pane_with_a_live_child_answers_a_directory() {
    // `live_cwd` answers `None` for an unknown pane and for a pane whose child
    // is marked exited, and the child's directory for a live one.
    let backend = PortablePtyBackend::new();
    let pane = PaneId::new();
    let (writer, writer_rx) = channel::<WriterMsg>();
    drop(writer_rx);
    // The entry names this test process, whose directory is known here.
    backend.panes.lock().expect("panes").insert(
        pane,
        pane_entry(Terminal::Crate(Arc::new(Mutex::new(None))), writer),
    );

    assert_eq!(
        backend.live_cwd(PaneId::new()),
        None,
        "a pane this backend does not drive has no directory"
    );
    assert_eq!(
        backend.live_cwd(pane),
        None,
        "a pane whose child was reaped must answer nothing"
    );

    backend
        .panes
        .lock()
        .expect("panes")
        .get(&pane)
        .expect("the pane just inserted")
        .exited
        .store(false, Ordering::SeqCst);

    let here =
        std::fs::canonicalize(std::env::current_dir().expect("this process has a directory"))
            .expect("this process's directory is real");
    assert_eq!(
        backend
            .live_cwd(pane)
            .map(|dir| std::fs::canonicalize(dir).expect("the answer names a real directory")),
        Some(here),
        "a pane with a live child must answer the directory that child is in"
    );
}

#[cfg(unix)]
#[test]
fn a_pane_taken_back_after_its_child_was_signalled_reports_the_signal() {
    // The child was killed by SIGKILL before the pane is taken back. The
    // taken-back pane publishes `Signaled(9)`.
    let (terminal, pid) = {
        let _gate = PTY_GATE.lock().expect("pty gate");
        let pair = native_pty_system()
            .openpty(to_pp_size(PANE_SIZE))
            .expect("openpty");
        let terminal = own_terminal_fd(&*pair.master).expect("terminal descriptor");
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.arg("-c");
        cmd.arg("kill -9 $$");
        let child = pair.slave.spawn_command(cmd).expect("spawn");
        drop(pair.slave);
        let pid = child.process_id().expect("pid");
        // Left unreaped.
        drop(child);
        (terminal, pid)
    };

    let sink = CountingSink::new();
    let backend = PortablePtyBackend::with_sink(sink.clone());
    let pane = PaneId::new();
    backend
        .adopt(pane, terminal, pid, PANE_SIZE, None)
        .expect("take the pane back");

    assert_eq!(
        read_sink_until_exit(&sink, "a taken-back pane never published its child's exit"),
        ExitStatus::Signaled(9),
        "a child killed by SIGKILL must be reported as signalled, with that signal's number"
    );
}
