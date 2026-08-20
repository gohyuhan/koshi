//! Unit tests for [`ChildGuard`]'s kill-on-drop backstop, the two reader
//! pumps, the watcher's standby wait, the reader's delivery gate, the reader
//! park, the writer flush, the pane hand-over across a process-image swap, and
//! the pure status/size conversions. The tests that spawn a real Unix PTY are
//! Unix-gated; everything else runs on every platform.

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
    // kill is asynchronous; poll briefly for the child to go.
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

// `sig_no`, `map_status`, and `to_pp_size` are pure string/struct conversions
// with no platform syscalls, so everything below runs on every platform.

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
    // Regression pin (see the `sig_no` doc comment): "User defined signal 1"
    // ends in the digit `1`, and "User defined signal 2" ends in `2`. A naive
    // "parse the trailing number" implementation would misreport SIGUSR1/2
    // (10/12) as SIGHUP/SIGINT (1/2) — `sig_no("User defined signal 1")`
    // returning `1` instead of `10` is exactly that regression, wrong because
    // the description has no `": "` separator and does not start with
    // `"Signal "`, so it must fall through to the exact-match table, not a
    // trailing-digit scan.
    assert_eq!(sig_no("User defined signal 1"), 10);
    assert_eq!(sig_no("User defined signal 2"), 12);
}

#[test]
fn sig_no_unrecognized_description_is_zero() {
    assert_eq!(sig_no("Unknown Signal Foo"), 0);
    assert_eq!(sig_no(""), 0);
    // Has the "Signal " prefix but no parsable number after it: the
    // `strip_prefix` succeeds, the `.parse::<i32>()` fails, so this must fall
    // through to the exact-match table (which also misses) rather than panic
    // or silently return a non-zero value.
    assert_eq!(sig_no("Signal abc"), 0);
    // Has a ": " separator but the tail isn't numeric either — same
    // fall-through requirement.
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
    // `s.exit_code() as i32` on a `u32` is an `as` cast, not `try_into`: it
    // wraps rather than panicking or saturating. `u32::MAX` (0xFFFF_FFFF) `as
    // i32` is exactly `-1`, and `i32::MAX as u32 + 1` wraps to `i32::MIN`.
    // Pinning the wrap here means a change to a checked/saturating
    // conversion would be caught as a behavior change, not silently allowed.
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

/// Nothing received the stop request, so the 3-second window is not spent.
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

/// Part of a group may have received the stop request, so those members get the
/// whole 200ms window to exit on their own.
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

/// A sink that keeps everything it is handed, so a test can check exactly what
/// reached the consumer.
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

    /// How many exits have reached this sink. A pane publishes one, so any
    /// other count is a pane publishing twice or not at all.
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

/// How long a test waits for a thread it expects back promptly. Generous
/// enough that only a wait that never ends reaches it, so it fails the test
/// instead of hanging the suite.
const HANG_GUARD: Duration = Duration::from_secs(5);

/// Stand by with the usual rounds, so each test spells out only what it is
/// varying. The tests that need a deadline already past call
/// [`should_publish_exit`] directly.
fn stand_by(cancel: &Receiver<()>, handover: &Mutex<Handover>) -> bool {
    should_publish_exit(
        cancel,
        handover,
        Instant::now() + EXIT_PUBLISH_LIMIT,
        EXIT_PUBLISH_GRACE,
    )
}

/// The signals a pump waits on, for a test that varies only one of them: a
/// doorbell, whether the child is known reaped, and a gate holding nobody.
#[cfg(unix)]
fn signals<'a>(wake: &'a Waker, exited: &'a AtomicBool, gate: &'a ReaderGate) -> ReaderSignals<'a> {
    ReaderSignals { wake, exited, gate }
}

/// A gate holding nobody, for a test that is not exercising the park.
#[cfg(unix)]
fn open_gate() -> ReaderGate {
    ReaderGate::new()
}

/// A sink-route [`Delivery`] over `sink`, with the settled flag and the two
/// hand-over counters it shares with the watcher handed back so a test can
/// read them.
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
    // The counters are what tell the watcher whether the reader is done. A
    // chunk fully handed over leaves them equal — nothing in flight — and both
    // advanced, which is the reader having drained more of the PTY.
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
    // A disowned descendant can keep printing into a terminal whose pane the
    // consumer has already let go of. Those bytes belong to a pane that no
    // longer exists, so the reader is told to stop instead of forwarding them.
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

/// A sink that refuses everything, standing in for a consumer that has gone
/// away — a runtime whose inbox is closed.
struct RefusingSink;

impl PtySink for RefusingSink {
    fn output(&self, _pane: PaneId, _bytes: Vec<u8>) -> bool {
        false
    }

    fn exit(&self, _pane: PaneId, _status: ExitStatus) {}
}

#[test]
fn a_refused_chunk_settles_the_pane() {
    // A consumer that refuses a chunk is done with the pane, so the watcher
    // must not hand it an exit for that pane afterwards.
    let (delivery, handover) = sink_delivery(Arc::new(RefusingSink));

    assert!(!delivery.output(PaneId::new(), b"hi"));
    assert!(
        handover.lock().expect("handover").settled,
        "a refused chunk must settle the pane so the watcher publishes no exit"
    );
}

#[test]
fn a_settled_pane_finishes_without_waiting_for_the_exit_status() {
    // The one ending that must not wait for the child: a consumer that already
    // let the pane go leaves nobody to hand the status to, and waiting for it
    // would pin the reader thread for as long as the child runs.
    //
    // `exit_sender` is held for the whole test and never sent on, so a
    // `finish` that waited for a status would still be waiting at the deadline.
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

    assert!(
        done_rx.recv_timeout(HANG_GUARD).is_ok(),
        "a settled pane waited for an exit status it will never publish"
    );
    drop(exit_sender);
}

/// The exit a watcher holds, handed to `sink` through the standby with the
/// deadline already passed, so the standby answers without spending a round.
fn stand_by_now(sink: &Arc<CountingSink>, handover: &Mutex<Handover>) {
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
    // The whole tail on the two paths that have one: a reader that never
    // reaches the end leaves the exit here, and the consumer is told once.
    let sink = CountingSink::new();
    let handover = handover_at(0, 0);

    stand_by_now(&sink, &handover);

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
    // `kill` settles before it cancels, so the standby publishes nothing. It
    // must still settle: a reader reaching the end of the terminal afterwards
    // would otherwise hand the consumer an exit for a pane it already closed.
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
    // The reader got there first and put the exit behind the last of the
    // output. The watcher must add nothing.
    let sink = CountingSink::new();
    let handover = Mutex::new(Handover {
        begun: 0,
        done: 0,
        settled: true,
    });

    stand_by_now(&sink, &handover);

    assert_eq!(
        sink.exit_count(),
        0,
        "the consumer was handed a second exit for one child"
    );
}

#[test]
fn a_watcher_whose_consumer_is_gone_publishes_nothing() {
    // Nothing holds the sink but the watcher's own weak reference, which is a
    // consumer that has been dropped. There is nobody to tell.
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
    // A reader that cannot be brought to the end of its terminal stays in
    // `read` and never settles the exit: a Unix terminal that exposes no
    // descriptor, and a Windows console another process is still attached to.
    // Once the deadline passes the watcher publishes the status it holds.
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
    // The reader is the one that knows: it settles once the terminal has gone
    // quiet with the child dead, which puts the exit behind the last of the
    // output. The watcher is only there for a terminal the reader cannot reach
    // the end of, so a settled pane ends its standby with nothing to publish.
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
    // The guarantee a consumer relies on: it sees every byte the child printed
    // before it sees the child end. The deadline here has already passed, so
    // the only thing left holding the standby is the chunk in the consumer's
    // hands — and it holds it until that chunk lands, however long that takes.
    let (_cancel, cancel_rx) = channel::<()>();
    // One chunk begun and not yet finished: in the consumer's hands.
    let handover = Arc::new(Mutex::new(Handover {
        begun: 1,
        done: 0,
        settled: false,
    }));

    // Set before the chunk lands, so a standby that published early would find
    // this still false. Nothing here times the scheduler: the flag is what the
    // claim rests on, not the clock.
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
    // The other side of the same guarantee: a chunk that never comes back holds
    // the standby for as long as it is out, and closing the pane is what
    // releases it. `kill` settles the exit before it sends, so nothing is
    // published on the way out.
    //
    // Run on its own thread and waited for: a standby that never ends would
    // otherwise hang this test rather than fail it, and a hung test reports
    // nothing about which guarantee broke.
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
    // What `kill` sends, so tearing a pane down joins the watcher straight
    // away. The wall-clock claim is deliberately loose: a tight bound would be
    // asserting how promptly this machine schedules a thread, not what the
    // wait does. Returning `false` is the claim that matters.
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
    // The backend shutting down drops the entry holding the cancel sender.
    // Nothing will ever cancel, and nothing is left to publish to.
    let (cancel, cancel_rx) = channel::<()>();
    drop(cancel);

    let publish = stand_by(&cancel_rx, &handover_at(0, 0));

    assert!(!publish, "a dropped entry must publish no exit");
}

/// A stand-in terminal for the reader's wait: a connected socket pair, with
/// `far` playing the child writing into it and the returned descriptor the
/// master end the pump waits on and reads. Dropping `far` is the terminal
/// reporting an end.
///
/// The pump only ever waits on and reads this descriptor, which a socket
/// answers exactly as a PTY master does.
#[cfg(unix)]
fn fake_terminal() -> (std::os::unix::net::UnixStream, std::os::fd::OwnedFd) {
    let (far, near) = std::os::unix::net::UnixStream::pair().expect("terminal pair");
    (far, std::os::fd::OwnedFd::from(near))
}

#[cfg(unix)]
#[test]
fn resizing_a_terminal_retunes_the_size_its_child_reads() {
    // The pane's own descriptor carries the resize, so a full-screen program
    // still learns the new window. Retuning through our descriptor and reading
    // the size back through `portable-pty`'s master proves both name the same
    // terminal.
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
    // What lets a pane spend one descriptor instead of three: reading and
    // writing are separate directions of the same terminal, so its reader and
    // its writer can share the descriptor its resize uses.
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
    // The writer only ever stops once the child is gone or the pane is closed,
    // so there is no child left to tell anything. A descendant that outlived
    // the child still holds that terminal: bytes written then are echoed back
    // by the line discipline as output nobody printed, and a descendant
    // reading its input may act on them.
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
    // A resume file carries a plain number, and the image that reads it holds
    // its own open descriptors under numbers of the same shape. Adopting one of
    // those as a pane's terminal would drive a log file, a pipe or this
    // process's own standard error as if it were a terminal, and close it when
    // the pane ends.
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let pair = native_pty_system()
        .openpty(to_pp_size(PtySize { cols: 80, rows: 24 }))
        .expect("openpty");
    let master = own_terminal_fd(&*pair.master).expect("terminal descriptor");
    assert!(
        terminal_master_name(master.as_raw_fd()).is_some(),
        "a pane's own terminal must be named as a pseudoterminal master"
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

    // The number of a descriptor this process has closed: what a resume file
    // written by an image that no longer exists leaves behind.
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
    // Both of a pane's descriptors outlive the child it was opened for, so
    // every child spawned afterwards would inherit them unless they are
    // close-on-exec. That leak grows with the number of panes and never shows
    // up in this process's own count, so it is checked on the flag itself.
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

    // The control: a plain `dup` leaves the flag off, so the two claims above
    // are answers rather than something this check always says.
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
    // What the reader's wait rests on: the doorbell reads as nothing until it
    // is rung, and stays readable until the reader drains it. Staying rung is
    // what stops a ring landing in the gap before the reader reaches its wait
    // and being lost — the reader would then wait out a terminal a descendant
    // holds open, for as long as that descendant runs. Draining is what lets
    // the next wait block again, so a pause and a reaped child are two
    // separate rings rather than one that never clears.
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
    assert!(
        ready(&waker),
        "a ring must stay on, so a reader arriving later still sees it"
    );

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
    // What keeps a short-lived command's final line from being lost: the child
    // is gone, but bytes it printed on the way out are still in the terminal.
    // The wake says the child has gone; the reader takes what is there before
    // it stops, so `bye` reaches the consumer and the exit follows it.
    let (far, watched) = fake_terminal();
    let wake = Waker::new().expect("waker");
    let sink = CountingSink::new();
    let (delivery, _handover) = sink_delivery(sink.clone());
    let gate = open_gate();

    (&far)
        .write_all(b"bye")
        .expect("child prints on the way out");
    // What the watcher does, in the order it does it: the flag first, then the
    // ring, so a reader that finds the doorbell knows the child is reaped.
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
    // `far` stays open for the whole test, standing in for a descendant that
    // outlived the child and still holds the terminal. Nothing will ever close
    // it, so a reader blocked in `read` would stay there for as long as that
    // descendant runs, holding a thread and a descriptor. The wake is what
    // brings it back.
    let (far, watched) = fake_terminal();
    let wake = Waker::new().expect("waker");
    let (delivery, handover) = sink_delivery(CountingSink::new());

    // Rounds far longer than any scheduling delay this machine can add, so
    // "before a round" is a claim about the pump rather than about load.
    let grace = Duration::from_secs(2);
    let limit = Duration::from_secs(10);

    // What `kill` does, in the order it does it.
    handover.lock().expect("handover").settled = true;
    wake.wake(); // the pane was closed

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
    // An idle pane must cost no wakeups: with the child still running and
    // nothing to read, the wait has no timer to fire and the reader stays in
    // it. Only the terminal reporting an end releases it here.
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
    assert!(
        done_rx.recv_timeout(HANG_GUARD).is_ok(),
        "a terminal that reports an end must release the reader"
    );
}

#[cfg(unix)]
#[test]
fn output_arriving_after_the_child_has_gone_is_still_handed_over() {
    // A descendant that outlived the child keeps printing into the terminal.
    // Each round that brings something restarts the wait, so the reader keeps
    // handing bytes over instead of stopping at the first quiet moment.
    let (far, watched) = fake_terminal();
    let wake = Waker::new().expect("waker");
    let sink = CountingSink::new();
    let (delivery, _handover) = sink_delivery(sink.clone());

    // Rounds long enough that a byte every 200ms always lands inside one, on a
    // machine loaded enough to overshoot every sleep here.
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

    // What the consumer is owed is every byte, in order — not one chunk per
    // write. A read takes whatever has arrived, so two writes landing close
    // together come back as one chunk, and counting chunks would call that a
    // failure when nothing was lost.
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
    // The bound on the round above: a descendant that holds the terminal open
    // and prints forever must not keep a dead child's pane open for as long as
    // it runs. The rounds stop at the limit whatever is still arriving.
    let (far, watched) = fake_terminal();
    let wake = Waker::new().expect("waker");
    let (delivery, _handover) = sink_delivery(CountingSink::new());

    let grace = Duration::from_millis(200);
    let limit = Duration::from_secs(1);

    let exited = AtomicBool::new(true);
    wake.wake(); // the child has gone
                 // The printer never sleeps, so the kernel's socket buffer always holds
                 // something between the pump's reads and a quiet round can never come from
                 // the printer simply not being scheduled. `far` is nonblocking, so a full
                 // buffer is refused rather than parking this thread.
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
    // The pump for a terminal that cannot be waited on — Windows, and a Unix
    // terminal that exposes no descriptor. It has no way to be brought back,
    // so the end of the terminal is the only thing that stops it.
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
    // The other ending: the consumer refuses a chunk, so there is nothing left
    // to deliver to and the reader stops rather than draining a terminal
    // nobody is listening to.
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
    // A refused chunk ends the pump where it stands: the chunks behind it are
    // left for whatever the caller does next, and the pump reads none of them.
    let (delivery, _handover) = sink_delivery(Arc::new(RefusingSink));
    let mut reader = Scripted::new(vec![b"one", b"two", b"three"]);

    let reached_the_end = pump_blocking(&mut reader, &delivery, PaneId::new());

    assert!(
        !reached_the_end,
        "the pump reported an end it never reached"
    );
    assert_eq!(reader.taken, 1, "the pump read past the refused chunk");
    assert_eq!(
        reader.chunks.len(),
        2,
        "the chunks behind the refused one must be left unread"
    );
}

// Windows only: the drain exists because closing a pane's console waits for
// the output it still holds to be read out.
#[cfg(windows)]
#[test]
fn draining_a_terminal_reads_it_to_the_end() {
    // The writer runs this on a settled pane, whose reader has stopped. Every
    // chunk has to be taken, or the console's close waits on what is left.
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
    // The watcher takes the master out and drops it once the child is reaped,
    // so a resize can arrive at a pane whose terminal has already closed. There
    // is nothing left to retune, and the caller is not handed an error for it.
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
    // Checking the pane and claiming the chunk are one step, so a reader can
    // never hold a chunk the handover does not know about. A settled pane
    // therefore turns the chunk away without claiming it, and the counts stay
    // where they were.
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
    // The standby is what reads this, and only a Unix terminal with no
    // descriptor has one, so the method is built nowhere else.
    #[cfg(not(windows))]
    assert!(!held.in_flight(), "so nothing reads as in flight");
}

// The reader park and the pane hand-over below drive real children in real
// PTYs, so they are built on Unix only. This backend's readers are never held
// still on Windows.

/// Standard test window: 80 columns × 24 rows.
#[cfg(unix)]
const PANE_SIZE: PtySize = PtySize { cols: 80, rows: 24 };

/// Serializes PTY creation across the parallel test threads. macOS `openpty(3)`
/// races under concurrent allocation; koshi itself only ever spawns from its
/// single runtime thread, so gating here matches production.
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

/// Wait until `sink` holds `needle`, and hand back everything it holds.
/// Fails the test rather than hanging if the bytes never arrive.
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

/// Pause `backend`'s readers on a thread of its own, so a pause that never
/// settles fails the test instead of hanging the whole suite.
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

#[cfg(unix)]
#[test]
fn pausing_holds_a_live_panes_output_and_resuming_releases_it() {
    // The case that used to wedge the session: the pane's child is alive for
    // the whole test, so nothing can be stopped by waiting for it to exit. The
    // reader parks instead — everything read before the pause is already with
    // the consumer, nothing read after it reaches the consumer while parked,
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

    // The terminal echoes what is written to it, so this is output the reader
    // would take at once if it were not parked.
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
    // The pause rings the same doorbell the watcher rings when the child is
    // reaped. A reader that read the ring as a reaped child would start its
    // grace rounds and end one quiet round later, killing a pane whose child
    // is still running. Waiting past the limit and then asking for a round
    // trip is what tells those two apart.
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
    // A reader whose child ended has left its pump and is no longer counted,
    // so the pause has nobody to wait for. Counting it would wait forever.
    let sink = CountingSink::new();
    let (backend, pane) = backend_running(sink.clone(), "printf bye");

    let deadline = Instant::now() + HANG_GUARD;
    while sink.exit_taken().is_none() {
        assert!(Instant::now() < deadline, "the child's exit never arrived");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        sink.exit_taken(),
        Some(ExitStatus::ExitCode(0)),
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
    // A terminal can reach its end while the pane is unsettled and no exit has
    // been published. The reader then waits for that exit, and it waits past
    // the pump, inside `finish`. It leaves the gate before that wait, or a
    // pause would be waiting for a reader that can never park again.
    let (far, terminal) = fake_terminal();
    let gate = Arc::new(ReaderGate::new());

    // Held for the whole test and never sent on, so the reader's wait for a
    // status never ends on its own.
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
    // A terminal that exposes no descriptor leaves its reader blocked in
    // `read`, where no doorbell reaches it. The pause has to say so before it
    // changes anything, or it would wait for a reader that can never arrive.
    let backend = PortablePtyBackend::new();
    let pane = PaneId::new();
    let (writer, _writer_rx) = channel::<WriterMsg>();
    let (exit_grace_cancel, _cancel_rx) = channel::<()>();
    backend.panes.lock().expect("panes").insert(
        pane,
        PaneEntry {
            terminal: Terminal::Crate(Arc::new(Mutex::new(None))),
            size: PANE_SIZE,
            writer,
            // The entry's own process, which nothing in this test signals.
            killer: PtyChildKillControl::new(std::process::id()),
            exited: Arc::new(AtomicBool::new(true)),
            exit: Arc::new(OnceLock::new()),
            handover: Arc::new(Mutex::new(Handover::default())),
            exit_grace_cancel,
            reader: spawn_pty_thread("koshi-pty-read", || {}),
            reader_wake: None,
            watcher: spawn_pty_thread("koshi-pty-watch", || {}),
        },
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
/// `release`. What a child that has stopped reading its stdin does to the pane's
/// writer thread.
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
    // What lets a process about to replace its own image know its panes have
    // been told everything: the barrier travels the same channel as the bytes,
    // so an answer to it is proof they are written.
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

/// A pane entry around `terminal` and `writer`, with no child of its own: what
/// a test needs to reach the backend's bookkeeping without spawning one.
#[cfg(unix)]
fn pane_entry(terminal: Terminal, writer: Sender<WriterMsg>) -> PaneEntry {
    let (exit_grace_cancel, _cancel_rx) = channel::<()>();
    PaneEntry {
        terminal,
        size: PANE_SIZE,
        writer,
        // The test's own process, which nothing here signals.
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
    // The bytes an image swap must not lose: the backend took them for the
    // child, and the writer thread holding them dies with the process image.
    // Only a flush that has answered proves they are on the terminal.
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
    // A child that stopped reading its stdin blocks its pane's writer inside
    // the write, where nothing can interrupt it. The flush has to say so, so
    // the caller can leave the session as it is instead of losing what is
    // queued behind that write.
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
    // Cleared so a descriptor survives into a new process image, set again so
    // no later child inherits it. `FD_CLOEXEC` is the only descriptor flag, so
    // reading the whole word back proves nothing else moved.
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
    // The new image tells the pane's terminal engine how big the child thinks
    // its window is. Reporting the size the pane was spawned at would redraw
    // every resized pane at the wrong geometry.
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

    // The descriptor has to be the pane's own terminal, not some other open
    // file: reading the window size back through it says which.
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
    // The swap, in one process: pause, carry the descriptor and the process id
    // across, take the pane back on the other side. The child never stops, and
    // the pane it comes back as drives it exactly as the first one did.
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let old_sink = CountingSink::new();
    let (old, pane) = backend_running(old_sink.clone(), "printf ready; sleep 30");

    read_sink_until(&old_sink, b"ready");
    let before = old_sink.bytes();
    pause_or_fail(&old).expect("pause the readers");

    let carried = old.carried_panes();
    assert_eq!(carried.len(), 1, "one live pane must give one record");
    let carried = carried[0];
    assert_eq!(carried.pane_id, pane, "the record must name the pane");
    let fd = carried
        .terminal_fd
        .expect("a real pty exposes a descriptor");

    // What replacing the image does to the descriptor: clear the flag so it
    // survives, then own it again on the other side and set the flag back.
    set_terminal_cloexec(fd, false).expect("clear close-on-exec");
    let survived = unsafe { libc::dup(fd) };
    assert!(survived >= 0, "the descriptor must survive the swap");
    let survived = unsafe { OwnedFd::from_raw_fd(survived) };
    set_terminal_cloexec(survived.as_raw_fd(), true).expect("set close-on-exec");

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

    // Closing through the old pane kills the one child both panes drive, which
    // ends the taken-back pane's threads too.
    old.resume_readers();
    old.kill(pane, KillPolicy::Tree).expect("close the pane");
}

#[cfg(unix)]
#[test]
fn a_pane_taken_back_after_its_child_ended_still_publishes_the_exit() {
    // A child can exit in the instant between the two images. The taken-back
    // pane has no `portable-pty` child to wait on, so its watcher reaps the
    // process id itself — and it has to answer rather than wait forever.
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
        // Left unreaped, exactly as an image swap leaves it: the taken-back
        // pane's watcher is the one that must reap it.
        drop(child);
        (terminal, pid)
    };

    let sink = CountingSink::new();
    let backend = PortablePtyBackend::with_sink(sink.clone());
    let pane = PaneId::new();
    backend
        .adopt(pane, terminal, pid, PANE_SIZE, None)
        .expect("take the pane back");

    let deadline = Instant::now() + HANG_GUARD;
    while sink.exit_taken().is_none() {
        assert!(
            Instant::now() < deadline,
            "a taken-back pane never published its child's exit"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        sink.exit_taken(),
        Some(ExitStatus::ExitCode(7)),
        "the exit the child really ended with must reach the consumer"
    );
}

#[cfg(unix)]
#[test]
fn a_child_that_ends_while_the_readers_are_held_keeps_the_code_it_ended_with() {
    // Holding the readers still does not hold the watchers still, so a child
    // that ends during the hand-over is reaped by the image that is leaving.
    // That takes the status out of the kernel, and a wait on the same process
    // id afterwards can only answer `ECHILD`. The status has to travel with the
    // pane instead, or the pane comes back reporting -1 for a child that ended
    // with 3.
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let old_sink = CountingSink::new();
    // The child prints, then ends a second later — long after the readers are
    // held, which is the moment this test is about.
    let (old, pane) = backend_running(old_sink.clone(), "printf ready; sleep 1; exit 3");

    read_sink_until(&old_sink, b"ready");
    pause_or_fail(&old).expect("hold the readers still");

    // The leaving image's watcher reaps the child while its reader is held.
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

    // What replacing the image does to the descriptor: clear the flag so it
    // survives, own it again on the other side, set the flag back.
    let fd = carried
        .terminal_fd
        .expect("a real pty exposes a descriptor");
    set_terminal_cloexec(fd, false).expect("clear close-on-exec");
    let survived = unsafe { libc::dup(fd) };
    assert!(survived >= 0, "the descriptor must survive the swap");
    let survived = unsafe { OwnedFd::from_raw_fd(survived) };
    set_terminal_cloexec(survived.as_raw_fd(), true).expect("set close-on-exec");

    let new_sink = CountingSink::new();
    let new = PortablePtyBackend::with_sink(new_sink.clone());
    new.adopt(pane, survived, carried.pid, carried.size, carried.exit)
        .expect("take the pane back");

    let deadline = Instant::now() + HANG_GUARD;
    while new_sink.exit_taken().is_none() {
        assert!(
            Instant::now() < deadline,
            "the taken-back pane never published its child's exit"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        new_sink.exit_taken(),
        Some(ExitStatus::ExitCode(3)),
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
    // The swap can reach the pause a second time: a swap that could not start
    // puts the readers back and the next restart request pauses them again, and
    // a pause that lands while the readers are already held must settle rather
    // than wait for a park that has already happened. One resume then has to
    // free them, since the pause is a flag and not a count.
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
    // The swap puts the readers back on every path that abandons it, including
    // the ones that never reached the pause. A release with nobody parked must
    // therefore change nothing at all.
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
    // A pane's child can end in the middle of a swap. The reader is parked, so
    // nothing may be published while it is held; the release then has to hand
    // over the last of the output and the real exit status, exactly once.
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

    let deadline = Instant::now() + HANG_GUARD;
    while sink.exit_taken().is_none() {
        assert!(
            Instant::now() < deadline,
            "the released reader never published the exit it was holding"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        sink.exit_taken(),
        Some(ExitStatus::ExitCode(3)),
        "the status the child really ended with must reach the consumer"
    );

    // A second publish would arrive after the first; the wait is what gives it
    // the chance to.
    thread::sleep(EXIT_PUBLISH_GRACE * 3);
    assert_eq!(sink.exit_count(), 1, "and it must arrive exactly once");
}

#[cfg(unix)]
#[test]
fn input_written_while_the_readers_are_held_reaches_the_child_and_its_answer_comes_on_release() {
    // The swap applies whatever the runtime inbox already held after the
    // readers are held still, so a key the user typed can be written to a pane
    // whose reader is parked. Holding the readers stops output being read out
    // of the terminal and leaves the input direction alone: the writer thread
    // puts the bytes on the terminal, and the child takes them and answers
    // while nothing is reading. What waits for the release is that answer and
    // the child's exit.
    let sink = CountingSink::new();
    let (backend, pane) = backend_running(
        sink.clone(),
        "printf ready; read go; printf 'took %s' \"$go\"; exit 3",
    );

    read_sink_until(&sink, b"ready");
    let before = sink.bytes();
    pause_or_fail(&backend).expect("hold the readers still");

    backend.write(pane, b"go\n").expect("write to the pane");
    // The write direction settles while the readers are held: the answer to
    // this barrier means the line is on the terminal, where its child reads it.
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

    let deadline = Instant::now() + HANG_GUARD;
    while sink.exit_taken().is_none() {
        assert!(
            Instant::now() < deadline,
            "the released reader never published the exit it was holding"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        sink.exit_taken(),
        Some(ExitStatus::ExitCode(3)),
        "the child read the line and ended with the status that line asks for"
    );
    assert_eq!(
        String::from_utf8_lossy(&sink.bytes()).into_owned(),
        "readygo\r\ntook go",
        "the release hands over the echo of the input and what the child printed from it"
    );
}

/// The numbers a bursting child printed, in the order they reached a sink.
/// Every line that child writes is four digits, so a line that reads as
/// anything else is a chunk boundary that lost or split bytes.
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

/// How many numbered lines the hand-over drives through a pane. Well past what
/// a terminal holds, so the child fills its terminal and blocks against a
/// reader that is held, the way a swap leaves it.
#[cfg(unix)]
const BURST_LINES: u32 = 1500;

#[cfg(unix)]
#[test]
fn every_byte_a_child_is_printing_crosses_the_hand_over_once_and_in_order() {
    // The swap at its hardest: the child is printing when its reader is held,
    // so bytes sit in the terminal and the child blocks against a terminal
    // nobody is reading. The pane is then taken back by the backend standing in
    // for the new process image. Reading what the two sinks hold, end to end,
    // proves the hand-over lost no byte, invented none, and reordered none —
    // wherever it fell in the child's output.
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

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
    let fd = carried
        .terminal_fd
        .expect("a real pty exposes a descriptor");

    // What replacing the image does to the descriptor: clear the flag so it
    // survives, own it again on the other side, set the flag back.
    set_terminal_cloexec(fd, false).expect("clear close-on-exec");
    let survived = unsafe { libc::dup(fd) };
    assert!(survived >= 0, "the descriptor must survive the swap");
    let survived = unsafe { OwnedFd::from_raw_fd(survived) };
    set_terminal_cloexec(survived.as_raw_fd(), true).expect("set close-on-exec");

    let new_sink = CountingSink::new();
    let new = PortablePtyBackend::with_sink(new_sink.clone());
    new.adopt(pane, survived, carried.pid, carried.size, carried.exit)
        .expect("take the pane back");

    // The two sinks end to end are the whole of what the consumer was handed.
    // The reader that was held took nothing more, so `before` never grows.
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
    // The descriptor a resume file names can be gone by the time the new image
    // reads it — a swap that took long enough for the child to be reaped and
    // its terminal closed. Taking the pane back must answer rather than leave
    // its reader waiting on a terminal that will never speak again.
    let (far, terminal) = fake_terminal();
    drop(far); // the terminal is already at its end

    let sink = CountingSink::new();
    let backend = PortablePtyBackend::with_sink(sink.clone());
    let pane = PaneId::new();

    // A process id nothing can reap: `waitpid` answers `ECHILD`, which is what
    // a child already reaped elsewhere leaves behind.
    let handle = backend
        .adopt(pane, terminal, u32::from(u16::MAX), PANE_SIZE, None)
        .expect("take the pane back");
    assert_eq!(handle.pane_id(), pane, "the handle must name the pane");

    let deadline = Instant::now() + HANG_GUARD;
    while sink.exit_taken().is_none() {
        assert!(
            Instant::now() < deadline,
            "a pane taken back on a dead terminal never reported its child gone"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        sink.exit_taken(),
        Some(ExitStatus::ExitCode(-1)),
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
    // The new image sets the flag on every descriptor the resume file names,
    // and that call is what catches a descriptor the swap did not carry. It
    // has to fail rather than report success over a descriptor that is not
    // there.
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
    // The terminal writes its own bytes before the question, so the watch
    // outlasts them.
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

// --- what a pane records, and what it answers about its child ---

#[test]
fn a_channel_consumer_that_dropped_its_handle_stops_its_readers_pump() {
    // The one thing a channel delivery reports: whether anybody is still
    // listening. A reader told `true` here for a handle nobody holds carries on
    // reading a terminal whose output has nowhere to go, for as long as the
    // child runs.
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
    // The size a pane reports is the size its child was really told. Recording
    // one the kernel refused would hand the next process image a window the
    // child never had, and that image would redraw the pane at it.
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

    // The kernel's own refusal, taken here rather than written down, so the
    // check holds on every system this runs on.
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
    // A pane's writer thread ends when its child does. The flush is about
    // bytes still queued for a live child, so a pane with no writer left has
    // nothing to answer and must not fail the call for the panes that do.
    let backend = PortablePtyBackend::new();

    // One pane whose writer channel is closed, so the barrier cannot even be
    // queued for it.
    let closed = PaneId::new();
    let (closed_writer, closed_rx) = channel::<WriterMsg>();
    drop(closed_rx);

    // One pane whose writer takes the barrier and then ends without answering
    // it, which is what a writer thread stopping between the two steps does.
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
    // The directory comes from the child's process id, and a reaped process id
    // can already name a stranger. The answer has to be nothing rather than
    // that stranger's directory.
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
    // The taken-back pane reaps the process id itself, so it is the one place
    // a child killed by a signal is turned into an exit. Reporting a code
    // there would tell the consumer the child chose to end.
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
        // Left unreaped, exactly as an image swap leaves it.
        drop(child);
        (terminal, pid)
    };

    let sink = CountingSink::new();
    let backend = PortablePtyBackend::with_sink(sink.clone());
    let pane = PaneId::new();
    backend
        .adopt(pane, terminal, pid, PANE_SIZE, None)
        .expect("take the pane back");

    let deadline = Instant::now() + HANG_GUARD;
    while sink.exit_taken().is_none() {
        assert!(
            Instant::now() < deadline,
            "a taken-back pane never published its child's exit"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        sink.exit_taken(),
        Some(ExitStatus::Signaled(9)),
        "a child killed by SIGKILL must be reported as signalled, with that signal's number"
    );
}
