//! Unit tests for [`ChildGuard`]'s kill-on-drop backstop, the two reader
//! pumps, the watcher's standby wait, the reader's delivery gate, and the pure
//! status/size conversions. The guard tests spawn a real Unix PTY and are
//! Unix-gated; everything else runs on every platform.

use super::*;

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

/// A sink that keeps everything it is handed, so a test can check exactly what
/// reached the consumer.
struct CountingSink {
    /// Every output chunk taken, oldest first.
    chunks: Mutex<Vec<Vec<u8>>>,
}

impl CountingSink {
    fn new() -> Arc<Self> {
        Arc::new(CountingSink {
            chunks: Mutex::new(Vec::new()),
        })
    }

    /// How many chunks have reached this sink.
    fn chunk_count(&self) -> usize {
        self.chunks.lock().expect("counting sink").len()
    }
}

impl PtySink for CountingSink {
    fn output(&self, _pane: PaneId, bytes: Vec<u8>) -> bool {
        self.chunks.lock().expect("counting sink").push(bytes);
        true
    }

    fn exit(&self, _pane: PaneId, _status: ExitStatus) {}
}

/// How long a test waits for a thread it expects back promptly. Generous
/// enough that only a wait that never ends reaches it, so it fails the test
/// instead of hanging the suite.
const HANG_GUARD: Duration = Duration::from_secs(5);

/// Stand by with the usual rounds, so each test spells out only what it is
/// varying. The tests that need a deadline already past call
/// [`should_publish_exit`] directly.
///
/// Only a Unix terminal that exposes no descriptor stands by, so the standby
/// and everything testing it are built nowhere else.
#[cfg(not(windows))]
fn stand_by(cancel: &Receiver<()>, handover: &Mutex<Handover>) -> bool {
    should_publish_exit(
        cancel,
        handover,
        Instant::now() + EXIT_PUBLISH_LIMIT,
        EXIT_PUBLISH_GRACE,
    )
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
#[cfg(not(windows))]
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

#[cfg(not(windows))]
#[test]
fn a_reader_that_never_reaches_the_end_leaves_the_exit_to_the_watcher() {
    // A Unix terminal that exposes no descriptor cannot be waited on, so its
    // reader stays in `read` and never settles the exit. Nobody else would ever
    // tell the consumer the child died, so once the deadline passes the watcher
    // publishes the status it already holds.
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

#[cfg(not(windows))]
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

#[cfg(not(windows))]
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

#[cfg(not(windows))]
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

#[cfg(not(windows))]
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

#[cfg(not(windows))]
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
fn a_waker_is_quiet_until_woken_and_stays_woken_after() {
    // What the reader's wait rests on: the waker reads as nothing until it is
    // fired, and stays readable once it has been. Latching is what stops a
    // wake landing in the gap before the reader reaches its wait and being
    // lost — the reader would then wait out a terminal a descendant holds
    // open, for as long as that descendant runs.
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::AsFd;

    let waker = Waker::new().expect("this platform offers a one-descriptor wake");
    let ready = |fd: &Waker| {
        let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
        poll(&mut fds, PollTimeout::ZERO).expect("poll the waker");
        fds[0].revents().is_some_and(|r| !r.is_empty())
    };

    assert!(!ready(&waker), "a fresh waker must read as nothing");
    waker.wake();
    assert!(ready(&waker), "waking must make the descriptor readable");
    assert!(
        ready(&waker),
        "a wake must latch, so a reader arriving later still sees it"
    );
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

    (&far)
        .write_all(b"bye")
        .expect("child prints on the way out");
    wake.wake(); // the child has gone

    let started = Instant::now();
    pump_waited(
        &delivery,
        PaneId::new(),
        &watched,
        &wake,
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
        pump_waited(&delivery, PaneId::new(), &watched, &wake, grace, limit);
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
        pump_waited(
            &delivery,
            PaneId::new(),
            &watched,
            &wake,
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

    wake.wake(); // the child has gone
    let printer = thread::spawn(move || {
        for mark in [b'a', b'b', b'c'] {
            thread::sleep(Duration::from_millis(200));
            (&far).write_all(&[mark]).expect("the descendant prints");
        }
        far // hold the terminal open until the pump has stopped
    });

    pump_waited(&delivery, PaneId::new(), &watched, &wake, grace, limit);

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

    let started = Instant::now();
    pump_waited(&delivery, PaneId::new(), &watched, &wake, grace, limit);
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
