//! Tests for the process that holds one session's panes: it answers a link,
//! refuses a request kind it does not have without closing that link, ends on
//! a `Shutdown` and on an idle window, and — the property the whole swap rests
//! on — loses no byte and repeats none while the link is away or holding its
//! output.
//!
//! The peer here is a hand-written session server over a real socket, so the
//! supervisor is tested through the wire and not through a stub of itself.
//! Dropping that peer closes the socket outright, which is what a session
//! server replacing its own image looks like from the supervisor's side.
//!
//! Most tests that open a real pane run a shell script written for `/bin/sh`,
//! so they are Unix-gated the same way [`koshi_pty::portable`]'s own
//! real-terminal tests are, and so are the helpers only those tests use.
//!
//! [`a_panes_child_prints_through_the_link_once_its_terminal_is_answered`] is
//! the exception: it runs each platform's own command interpreter and answers
//! the pane terminal's cursor-position query, so the one hop where a real pane
//! meets a real link is covered on every platform. Every other test runs on all
//! platforms.

use std::thread;
use std::time::Instant;

use koshi_ipc::supervisor::{SupervisorRequest, SUPERVISOR_PROTOCOL_VERSION};
use koshi_pty::supervisor::SupervisorPtyBackend;

use super::*;

/// How long a test waits for something it expects promptly. Generous enough
/// that only a wait that never ends reaches it, so it fails the test instead
/// of hanging the suite.
const HANG_GUARD: Duration = Duration::from_secs(10);

/// The idle window the tests run the supervisor with, short enough that a test
/// can sit through it.
const TEST_IDLE_EXIT: Duration = Duration::from_millis(300);

/// The idle window for the tests that let the link go and bring it back, long
/// enough that none of them sits through it.
const LONG_IDLE_EXIT: Duration = Duration::from_secs(30);

/// The secret every test link presents.
fn token() -> ConnectionToken {
    ConnectionToken::new("k7QxSecret")
}

/// A supervisor running on a thread of its own, with the address to link to
/// it.
struct RunningSupervisor {
    /// The address a link connects to.
    addr: String,
    /// The supervisor's own thread, joined to prove it ended.
    thread: Option<thread::JoinHandle<()>>,
    /// The socket file's directory, kept so it outlives the supervisor.
    _runtime_dir: tempfile::TempDir,
}

impl RunningSupervisor {
    /// Start a supervisor holding no pane, with `idle_exit` as its idle
    /// window.
    fn start(idle_exit: Duration) -> RunningSupervisor {
        let runtime_dir = tempfile::tempdir().expect("a temporary directory is created");
        let addr = link_addr(runtime_dir.path());
        let listener = Listener::bind(&addr).expect("the supervisor binds its link");
        let thread = thread::Builder::new()
            .name("supervisor-under-test".to_string())
            .spawn(move || hold_panes(listener, &token(), idle_exit))
            .expect("the supervisor thread starts");

        RunningSupervisor {
            addr,
            thread: Some(thread),
            _runtime_dir: runtime_dir,
        }
    }

    /// Whether the supervisor's thread has ended.
    fn ended(&self) -> bool {
        self.thread
            .as_ref()
            .expect("the supervisor thread handle")
            .is_finished()
    }

    /// Wait for the supervisor to end, failing the test rather than hanging
    /// when it never does.
    fn join(&mut self) {
        wait_until("the supervisor ended", || self.ended());
        self.thread
            .take()
            .expect("the supervisor thread handle")
            .join()
            .expect("the supervisor thread ended without panicking");
    }
}

/// An address for one test's link. On Unix it is a socket file inside `dir`;
/// on Windows it is a pipe name of its own, since a pipe has no directory.
fn link_addr(dir: &Path) -> String {
    #[cfg(unix)]
    {
        dir.join("supervisor.sock").display().to_string()
    }
    #[cfg(windows)]
    {
        let _ = dir;
        format!(
            "koshi-pty-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the clock is past the epoch")
                .as_nanos()
        )
    }
}

/// One hand-written session server on the other end of a link.
///
/// Every answer and every event arrives on the same connection, so
/// [`ask`](TestLink::ask) keeps the events it passes on the way to an answer
/// and the `next_event` readers hand them back.
struct TestLink {
    /// The connection itself. Dropping it closes the socket outright.
    connection: Connection,
    /// The id the next request carries.
    next_request_id: u64,
    /// Events read while waiting for an answer, oldest first.
    events: Vec<SupervisorEvent>,
}

impl TestLink {
    /// Open a link to `addr` and send the Hello that opens it.
    fn open(addr: &str) -> TestLink {
        let mut link = TestLink {
            connection: Connection::connect(addr).expect("the link opens"),
            next_request_id: 1,
            events: Vec::new(),
        };
        assert_eq!(
            link.ask(SupervisorRequestKind::hello(token())),
            SupervisorResult::Hello {
                protocol_version: SUPERVISOR_PROTOCOL_VERSION,
            }
        );
        link
    }

    /// Send one request and read frames until its answer arrives, keeping any
    /// event read on the way.
    fn ask(&mut self, kind: SupervisorRequestKind) -> SupervisorResult {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        self.connection
            .send(&SupervisorRequest { request_id, kind })
            .expect("the request is sent");
        loop {
            match self.recv() {
                SupervisorMessage::Response(response) => {
                    assert_eq!(response.request_id, Some(request_id));
                    return response.result;
                }
                SupervisorMessage::Event(event) => self.events.push(event),
            }
        }
    }

    /// Send raw JSON as one frame, for a request no build has.
    fn send_raw(&mut self, text: &str) {
        let raw: serde_json::Value = serde_json::from_str(text).expect("the raw request is JSON");
        self.connection.send(&raw).expect("the raw request is sent");
    }

    /// The next event, reading more frames when none is held. An answer read
    /// on the way fails the test: nothing asked for one.
    #[cfg(unix)]
    fn next_event(&mut self) -> SupervisorEvent {
        if !self.events.is_empty() {
            return self.events.remove(0);
        }
        match self.recv() {
            SupervisorMessage::Event(event) => event,
            SupervisorMessage::Response(response) => {
                panic!("no request was in flight, yet an answer arrived: {response:?}")
            }
        }
    }

    /// The next chunk of `pane`'s output, skipping every other event.
    #[cfg(unix)]
    fn next_output(&mut self, pane: PaneId) -> Vec<u8> {
        loop {
            if let SupervisorEvent::Output { pane_id, bytes } = self.next_event() {
                if pane_id == pane {
                    return bytes;
                }
            }
        }
    }

    /// The next event, without blocking on one that never comes.
    ///
    /// Each round asks for the pane list, which always answers, and keeps
    /// whatever event arrives on the way. A round that finds nothing pauses
    /// and asks again, and a whole window with nothing fails the test.
    #[cfg(unix)]
    fn next_event_or_fail(&mut self) -> SupervisorEvent {
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            if !self.events.is_empty() {
                return self.events.remove(0);
            }
            assert!(
                Instant::now() < deadline,
                "no event reached this link, and one was due"
            );
            self.ask(SupervisorRequestKind::ListPanes);
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// The next chunk of `pane`'s output, without blocking on one that never
    /// comes.
    #[cfg(unix)]
    fn next_output_or_fail(&mut self, pane: PaneId) -> Vec<u8> {
        loop {
            if let SupervisorEvent::Output { pane_id, bytes } = self.next_event_or_fail() {
                if pane_id == pane {
                    return bytes;
                }
            }
        }
    }

    /// One frame from the supervisor, decoded as this build reads it.
    fn recv(&mut self) -> SupervisorMessage {
        self.connection.recv().expect("a frame arrives")
    }
}

/// Wait until `read` answers `true`, failing the test rather than hanging when
/// it never does.
fn wait_until(what: &str, read: impl Fn() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !read() {
        assert!(Instant::now() < deadline, "{what} never happened");
        thread::sleep(Duration::from_millis(10));
    }
}

/// A spec running `script` under `/bin/sh`.
#[cfg(unix)]
fn shell_spec(script: &str) -> koshi_core::process::SpawnSpec {
    koshi_core::process::SpawnSpec {
        program: std::path::PathBuf::from("/bin/sh"),
        args: vec!["-c".to_string(), script.to_string()],
        cwd: None,
        env: std::collections::BTreeMap::new(),
        shell_kind: koshi_core::process::ShellKind::Other("sh".to_string()),
    }
}

/// The size every pane in these tests opens at.
const PANE_SIZE: koshi_core::process::PtySize = koshi_core::process::PtySize { cols: 80, rows: 24 };

/// The cursor-position query (DSR, `CSI 6 n`) a pane's terminal asks.
///
/// A Windows pseudoconsole asks one as the pane starts and hands over nothing
/// its child printed until the answer reaches it. The supervisor's backend
/// queues the answer as the pane opens, and the pane's reader removes the query
/// from the output, so none reaches the link.
const CURSOR_QUERY: &[u8] = b"\x1b[6n";

/// The line the pane opened by
/// [`a_panes_child_prints_through_the_link_once_its_terminal_is_answered`]
/// prints. Nothing else in that pane's output holds it, so finding it means the
/// child ran.
const PRINTED_MARKER: &str = "koshi-printed";

/// How long [`read_until_printed`] waits for a pane to print, which covers
/// opening the terminal, launching the child and carrying its bytes over the
/// link on a shared runner.
const PRINT_WAIT: Duration = Duration::from_secs(20);

/// A spec whose child prints `marker` and then stays alive, under the
/// platform's own command interpreter.
///
/// `cmd.exe /K` runs the command and keeps the interpreter, the way `cat` keeps
/// `/bin/sh` waiting on its input.
fn printing_spec(marker: &str) -> koshi_core::process::SpawnSpec {
    #[cfg(unix)]
    let (program, flag, script) = ("/bin/sh", "-c", format!("printf '{marker}'; cat"));
    #[cfg(windows)]
    let (program, flag, script) = ("cmd.exe", "/K", format!("echo {marker}"));
    let program = std::path::PathBuf::from(program);
    koshi_core::process::SpawnSpec {
        shell_kind: koshi_core::process::ShellKind::from_program(&program),
        program,
        args: vec![flag.to_string(), script],
        cwd: None,
        env: std::collections::BTreeMap::new(),
    }
}

/// Wait until the consumer holds `needle` for `pane`, and hand back everything
/// the consumer holds. Answers nothing; the supervisor answers its own panes'
/// terminals.
///
/// Fails the test once [`PRINT_WAIT`] has passed, naming what the consumer
/// holds: nothing held means no byte crossed the link, and the query alone
/// means the pane's reader delivered it instead of removing it.
fn read_until_printed(sink: &RecordingSink, pane: PaneId, needle: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + PRINT_WAIT;
    loop {
        let held = sink.bytes(pane);
        if held.windows(needle.len()).any(|window| window == needle) {
            return held;
        }
        assert!(
            Instant::now() < deadline,
            "the pane never printed {:?}; the consumer holds {:?}",
            String::from_utf8_lossy(needle),
            String::from_utf8_lossy(&held),
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// How many cursor-position queries `output` holds.
fn count_queries(output: &[u8]) -> usize {
    output
        .windows(CURSOR_QUERY.len())
        .filter(|window| *window == CURSOR_QUERY)
        .count()
}

#[test]
fn a_link_opens_on_a_hello_and_the_supervisor_starts_holding_no_pane() {
    let mut supervisor = RunningSupervisor::start(TEST_IDLE_EXIT);
    let mut link = TestLink::open(&supervisor.addr);

    assert_eq!(
        link.ask(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new())
    );

    assert_eq!(
        link.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}

#[test]
fn a_request_before_a_hello_is_refused_and_the_link_keeps_serving() {
    let mut supervisor = RunningSupervisor::start(TEST_IDLE_EXIT);
    let mut link = TestLink {
        connection: Connection::connect(&supervisor.addr).expect("the link opens"),
        next_request_id: 1,
        events: Vec::new(),
    };

    assert_eq!(
        link.ask(SupervisorRequestKind::ListPanes),
        SupervisorResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "ListPanes arrived before a Hello opened the link".to_string(),
        })
    );
    assert_eq!(
        link.ask(SupervisorRequestKind::hello(token())),
        SupervisorResult::Hello {
            protocol_version: SUPERVISOR_PROTOCOL_VERSION,
        }
    );
    assert_eq!(
        link.ask(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new())
    );

    assert_eq!(
        link.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}

#[test]
fn a_hello_carrying_the_wrong_token_is_refused_and_the_link_keeps_serving() {
    let mut supervisor = RunningSupervisor::start(TEST_IDLE_EXIT);
    let mut link = TestLink {
        connection: Connection::connect(&supervisor.addr).expect("the link opens"),
        next_request_id: 1,
        events: Vec::new(),
    };

    assert_eq!(
        link.ask(SupervisorRequestKind::hello(ConnectionToken::new(
            "wrongToken"
        ))),
        SupervisorResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match the supervisor's".to_string(),
        })
    );
    assert_eq!(
        link.ask(SupervisorRequestKind::hello(token())),
        SupervisorResult::Hello {
            protocol_version: SUPERVISOR_PROTOCOL_VERSION,
        }
    );

    assert_eq!(
        link.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}

#[test]
fn a_request_kind_the_supervisor_does_not_have_is_refused_by_name_and_the_link_keeps_serving() {
    // What a newer session server driving an older supervisor looks like. The
    // refusal names the kind so that server learns which one it cannot use,
    // and every other request on the same link still works.
    let mut supervisor = RunningSupervisor::start(TEST_IDLE_EXIT);
    let mut link = TestLink::open(&supervisor.addr);

    link.send_raw(r#"{"request_id":99,"kind":{"Rehome":{"pane_id":1}}}"#);
    let SupervisorMessage::Response(response) = link.recv() else {
        panic!("the refusal is an answer, not an event");
    };
    assert_eq!(
        response,
        SupervisorResponse {
            request_id: Some(99),
            result: SupervisorResult::Error(IpcErrorPayload {
                code: IpcErrorCode::UnsupportedKind,
                message: "this supervisor has no request kind named Rehome".to_string(),
            }),
        }
    );

    assert_eq!(
        link.ask(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new()),
        "the link keeps serving after a refusal"
    );

    assert_eq!(
        link.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}

#[test]
fn bytes_that_are_not_a_readable_request_are_refused_and_the_link_keeps_serving() {
    let mut supervisor = RunningSupervisor::start(TEST_IDLE_EXIT);
    let mut link = TestLink::open(&supervisor.addr);

    link.send_raw(r#"{"request_id":"not a number","kind":"ListPanes"}"#);
    let SupervisorMessage::Response(response) = link.recv() else {
        panic!("the refusal is an answer, not an event");
    };
    assert_eq!(
        response,
        SupervisorResponse {
            request_id: None,
            result: SupervisorResult::Error(IpcErrorPayload {
                code: IpcErrorCode::MalformedRequest,
                message: "the bytes received are not a request this build can read".to_string(),
            }),
        }
    );

    assert_eq!(
        link.ask(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new()),
        "an unreadable frame leaves the stream aligned"
    );

    assert_eq!(
        link.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}

#[test]
fn a_supervisor_holding_no_pane_ends_once_the_idle_window_passes_with_no_link() {
    let mut supervisor = RunningSupervisor::start(TEST_IDLE_EXIT);

    assert!(
        !supervisor.ended(),
        "the supervisor must still be there inside its idle window"
    );
    supervisor.join();
}

#[test]
fn a_link_inside_the_idle_window_keeps_the_supervisor_alive() {
    // A session server whose first link lands just as the supervisor would
    // have ended must still be served, so a session never loses the process
    // holding its panes to a race.
    let mut supervisor = RunningSupervisor::start(TEST_IDLE_EXIT);
    let mut link = TestLink::open(&supervisor.addr);

    thread::sleep(TEST_IDLE_EXIT * 3);
    assert!(
        !supervisor.ended(),
        "a linked supervisor must not end on its idle window"
    );
    assert_eq!(
        link.ask(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new())
    );

    assert_eq!(
        link.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}

#[cfg(unix)]
#[test]
fn a_pane_opened_through_the_link_reports_its_child_and_prints_back_over_it() {
    let mut supervisor = RunningSupervisor::start(TEST_IDLE_EXIT);
    let mut link = TestLink::open(&supervisor.addr);
    let pane = PaneId::new();

    let SupervisorResult::Spawned { pid } = link.ask(SupervisorRequestKind::Spawn {
        pane_id: pane,
        spec: shell_spec("printf 'ready'; sleep 30"),
        size: PANE_SIZE,
    }) else {
        panic!("the supervisor opens the pane");
    };

    assert!(pid > 0, "an open pane names a real child process");
    assert_eq!(
        link.ask(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(vec![SupervisorPane {
            pane_id: pane,
            pid,
            size: PANE_SIZE,
        }])
    );
    assert_eq!(link.next_output(pane), b"ready".to_vec());

    assert_eq!(
        link.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}

#[cfg(unix)]
#[test]
fn bytes_written_over_the_link_reach_the_child_and_come_back() {
    let mut supervisor = RunningSupervisor::start(TEST_IDLE_EXIT);
    let mut link = TestLink::open(&supervisor.addr);
    let pane = PaneId::new();
    let SupervisorResult::Spawned { .. } = link.ask(SupervisorRequestKind::Spawn {
        pane_id: pane,
        spec: shell_spec("printf 'ready'; cat"),
        size: PANE_SIZE,
    }) else {
        panic!("the supervisor opens the pane");
    };
    assert_eq!(link.next_output(pane), b"ready".to_vec());

    assert_eq!(
        link.ask(SupervisorRequestKind::Write {
            pane_id: pane,
            bytes: b"echoed\n".to_vec(),
        }),
        SupervisorResult::Done
    );

    // The terminal echoes what is written to it and `cat` prints it again, so
    // the bytes come back over the link.
    let mut seen = Vec::new();
    while !seen
        .windows(b"echoed".len())
        .any(|window| window == b"echoed")
    {
        seen.extend(link.next_output(pane));
    }

    assert_eq!(
        link.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}

#[cfg(unix)]
#[test]
fn resizing_and_killing_a_pane_over_the_link_reach_the_backend() {
    let mut supervisor = RunningSupervisor::start(TEST_IDLE_EXIT);
    let mut link = TestLink::open(&supervisor.addr);
    let pane = PaneId::new();
    let SupervisorResult::Spawned { pid } = link.ask(SupervisorRequestKind::Spawn {
        pane_id: pane,
        spec: shell_spec("sleep 30"),
        size: PANE_SIZE,
    }) else {
        panic!("the supervisor opens the pane");
    };

    let wider = koshi_core::process::PtySize {
        cols: 120,
        rows: 40,
    };
    assert_eq!(
        link.ask(SupervisorRequestKind::Resize {
            pane_id: pane,
            size: wider,
        }),
        SupervisorResult::Done
    );
    assert_eq!(
        link.ask(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(vec![SupervisorPane {
            pane_id: pane,
            pid,
            size: wider,
        }])
    );

    assert_eq!(
        link.ask(SupervisorRequestKind::Kill {
            pane_id: pane,
            kill_policy: KillPolicy::Tree,
        }),
        SupervisorResult::Done
    );
    assert_eq!(
        link.ask(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new()),
        "a killed pane leaves the supervisor"
    );

    assert_eq!(
        link.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}

#[cfg(unix)]
#[test]
fn a_pane_the_supervisor_does_not_hold_is_refused_by_name() {
    let mut supervisor = RunningSupervisor::start(TEST_IDLE_EXIT);
    let mut link = TestLink::open(&supervisor.addr);
    let ghost = PaneId::new();

    assert_eq!(
        link.ask(SupervisorRequestKind::Resize {
            pane_id: ghost,
            size: PANE_SIZE,
        }),
        SupervisorResult::Error(IpcErrorPayload {
            code: IpcErrorCode::Unknown,
            message: format!("invalid pane: id - {ghost}"),
        })
    );

    assert_eq!(
        link.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}

#[cfg(unix)]
#[test]
fn a_pane_survives_the_link_going_away_and_coming_back_inside_the_idle_window() {
    // This is the swap: the session server goes, and the supervisor must still
    // be holding every pane when the replacement image links to it.
    let mut supervisor = RunningSupervisor::start(LONG_IDLE_EXIT);
    let mut link = TestLink::open(&supervisor.addr);
    let pane = PaneId::new();
    let SupervisorResult::Spawned { pid } = link.ask(SupervisorRequestKind::Spawn {
        pane_id: pane,
        spec: shell_spec("sleep 30"),
        size: PANE_SIZE,
    }) else {
        panic!("the supervisor opens the pane");
    };

    drop(link);
    thread::sleep(TEST_IDLE_EXIT * 4);
    assert!(
        !supervisor.ended(),
        "a supervisor inside its idle window must wait for the next link"
    );

    let mut next = TestLink::open(&supervisor.addr);
    assert_eq!(
        next.ask(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(vec![SupervisorPane {
            pane_id: pane,
            pid,
            size: PANE_SIZE,
        }]),
        "the pane survived the link going away"
    );

    assert_eq!(
        next.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}

#[cfg(unix)]
#[test]
fn a_supervisor_holding_a_pane_closes_it_and_ends_once_the_idle_window_passes() {
    // A session server that dies before it links leaves the supervisor holding
    // panes nothing can reach, so the idle window has to end it too.
    let mut supervisor = RunningSupervisor::start(TEST_IDLE_EXIT);
    let mut link = TestLink::open(&supervisor.addr);
    let pane = PaneId::new();
    let SupervisorResult::Spawned { pid } = link.ask(SupervisorRequestKind::Spawn {
        pane_id: pane,
        spec: shell_spec("sleep 300"),
        size: PANE_SIZE,
    }) else {
        panic!("the supervisor opens the pane");
    };

    drop(link);
    supervisor.join();

    wait_until("the pane's child was reaped", || !process_alive(pid));
}

#[cfg(unix)]
#[test]
fn output_produced_while_the_link_is_away_arrives_whole_and_once_on_the_next_link() {
    // The property the whole swap rests on. The pane prints while no link is
    // up, so the chunk is in the sink's hands with nowhere to write it. It
    // must be held, not dropped, and it must be written to the next link
    // exactly once.
    let mut supervisor = RunningSupervisor::start(LONG_IDLE_EXIT);
    let mut link = TestLink::open(&supervisor.addr);
    let pane = PaneId::new();
    let SupervisorResult::Spawned { .. } = link.ask(SupervisorRequestKind::Spawn {
        pane_id: pane,
        spec: shell_spec("sleep 1; printf 'held'; sleep 30"),
        size: PANE_SIZE,
    }) else {
        panic!("the supervisor opens the pane");
    };

    // Gone before the pane prints anything at all, so nothing can have been
    // written into the socket this link leaves behind.
    drop(link);
    thread::sleep(Duration::from_secs(2));

    let mut next = TestLink::open(&supervisor.addr);
    assert_eq!(
        next.next_output_or_fail(pane),
        b"held".to_vec(),
        "the chunk produced with no link must reach the next one whole"
    );

    // Written once, not twice. The terminal echoes what is written to it, so
    // the next thing the pane prints is that echo — and a second copy of the
    // held chunk would have arrived before it.
    assert_eq!(
        next.ask(SupervisorRequestKind::Write {
            pane_id: pane,
            bytes: b"x\n".to_vec(),
        }),
        SupervisorResult::Done
    );
    let echo = next.next_output_or_fail(pane);
    assert!(
        echo.contains(&b'x'),
        "the echo of the written byte must follow the held chunk, and this is {:?}",
        String::from_utf8_lossy(&echo)
    );
    assert!(
        !echo.windows(4).any(|window| window == b"held"),
        "the held chunk must not be handed over a second time, and this is {:?}",
        String::from_utf8_lossy(&echo)
    );

    assert_eq!(
        next.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}

#[cfg(unix)]
#[test]
fn a_child_that_ends_while_the_link_is_away_is_reported_on_the_next_link() {
    let mut supervisor = RunningSupervisor::start(LONG_IDLE_EXIT);
    let mut link = TestLink::open(&supervisor.addr);
    let pane = PaneId::new();
    let SupervisorResult::Spawned { .. } = link.ask(SupervisorRequestKind::Spawn {
        pane_id: pane,
        spec: shell_spec("printf 'ready'; sleep 1; exit 7"),
        size: PANE_SIZE,
    }) else {
        panic!("the supervisor opens the pane");
    };
    assert_eq!(link.next_output(pane), b"ready".to_vec());
    drop(link);

    // The child ends with nobody linked, so its exit has to wait for the next
    // link rather than being dropped.
    thread::sleep(Duration::from_secs(2));

    let mut next = TestLink::open(&supervisor.addr);
    assert_eq!(
        next.next_event_or_fail(),
        SupervisorEvent::Exited {
            pane_id: pane,
            status: ExitStatus::ExitCode(7),
        }
    );

    assert_eq!(
        next.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}

#[cfg(unix)]
#[test]
fn a_pane_whose_exit_reached_a_session_server_that_never_closed_it_is_closed_at_the_next_link() {
    // The swap window: the child ends after the session server applied its
    // events for the last time, so the exit is written to a link that process
    // never reads again and no Kill follows it. The image linking next must be
    // handed no pane whose child is gone.
    let mut supervisor = RunningSupervisor::start(LONG_IDLE_EXIT);
    let mut leaving = TestLink::open(&supervisor.addr);
    let pane = PaneId::new();
    let SupervisorResult::Spawned { .. } = leaving.ask(SupervisorRequestKind::Spawn {
        pane_id: pane,
        spec: shell_spec("printf 'ready'; exit 7"),
        size: PANE_SIZE,
    }) else {
        panic!("the supervisor opens the pane");
    };

    let exit = loop {
        match leaving.next_event() {
            SupervisorEvent::Exited { pane_id, status } => break (pane_id, status),
            SupervisorEvent::Output { .. } => {}
        }
    };
    assert_eq!(exit, (pane, ExitStatus::ExitCode(7)));

    // The session server's process image goes with that exit unapplied.
    drop(leaving);

    let mut next = TestLink::open(&supervisor.addr);
    assert_eq!(
        next.ask(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new()),
        "a pane whose child ended must not be listed to the next link as a live one"
    );
    assert_eq!(
        next.ask(SupervisorRequestKind::Write {
            pane_id: pane,
            bytes: b"x".to_vec(),
        }),
        SupervisorResult::Error(IpcErrorPayload {
            code: IpcErrorCode::Unknown,
            message: format!("invalid pane: id - {pane}"),
        }),
        "and its entry left the supervisor, not only the listing"
    );

    assert_eq!(
        next.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}

#[cfg(unix)]
#[test]
fn output_held_for_a_swap_reaches_no_frame_on_this_link_and_arrives_whole_on_the_next() {
    // The swap window. The session server asks for the hold, then spends real
    // time on it: it tells its clients, writes its carried state to disk and
    // starts the new image before its process ends. A pane printing inside that
    // window must be held, because a frame written into the socket that process
    // is about to close reaches nobody, and the supervisor counts it as sent.
    let mut supervisor = RunningSupervisor::start(LONG_IDLE_EXIT);
    let mut leaving = TestLink::open(&supervisor.addr);
    let pane = PaneId::new();
    let SupervisorResult::Spawned { pid } = leaving.ask(SupervisorRequestKind::Spawn {
        pane_id: pane,
        spec: shell_spec("printf 'ready'; sleep 1; printf 'held'; sleep 30"),
        size: PANE_SIZE,
    }) else {
        panic!("the supervisor opens the pane");
    };
    assert_eq!(leaving.next_output(pane), b"ready".to_vec());

    assert_eq!(
        leaving.ask(SupervisorRequestKind::PauseOutput),
        SupervisorResult::Done
    );

    // The pane prints its second chunk inside this window. The round trip that
    // follows reads every frame the supervisor wrote before its answer, so
    // anything written for the pane would be held here.
    thread::sleep(Duration::from_secs(2));
    assert_eq!(
        leaving.ask(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(vec![SupervisorPane {
            pane_id: pane,
            pid,
            size: PANE_SIZE,
        }])
    );
    assert_eq!(
        leaving.events,
        Vec::new(),
        "no pane event may reach a link that asked for the output to be held"
    );

    // The session server's process image goes here, taking its socket with it.
    drop(leaving);

    let mut next = TestLink::open(&supervisor.addr);
    assert_eq!(
        next.next_output_or_fail(pane),
        b"held".to_vec(),
        "what the pane printed under the hold must reach the next link whole"
    );

    assert_eq!(
        next.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}

#[cfg(unix)]
#[test]
fn output_held_for_a_swap_that_was_abandoned_reaches_the_same_link_again() {
    // The swap could not start, so the session server keeps serving on the link
    // it already has and lifts the hold there.
    let mut supervisor = RunningSupervisor::start(LONG_IDLE_EXIT);
    let mut link = TestLink::open(&supervisor.addr);
    let pane = PaneId::new();
    let SupervisorResult::Spawned { pid } = link.ask(SupervisorRequestKind::Spawn {
        pane_id: pane,
        spec: shell_spec("printf 'ready'; sleep 1; printf 'held'; sleep 30"),
        size: PANE_SIZE,
    }) else {
        panic!("the supervisor opens the pane");
    };
    assert_eq!(link.next_output(pane), b"ready".to_vec());

    assert_eq!(
        link.ask(SupervisorRequestKind::PauseOutput),
        SupervisorResult::Done
    );

    // The pane prints its second chunk inside this window, and the round trip
    // that follows reads every frame the supervisor wrote before its answer.
    thread::sleep(Duration::from_secs(2));
    assert_eq!(
        link.ask(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(vec![SupervisorPane {
            pane_id: pane,
            pid,
            size: PANE_SIZE,
        }])
    );
    assert_eq!(
        link.events,
        Vec::new(),
        "no pane event may reach a link that asked for the output to be held"
    );

    assert_eq!(
        link.ask(SupervisorRequestKind::ResumeOutput),
        SupervisorResult::Done
    );

    assert_eq!(
        link.next_output_or_fail(pane),
        b"held".to_vec(),
        "what the pane printed under the hold must reach the link that lifts it"
    );

    assert_eq!(
        link.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}

#[cfg(unix)]
#[test]
fn a_pane_closed_while_the_output_is_held_does_not_wedge_the_supervisor() {
    // A quit applied while the swap was starting kills the panes with the hold
    // still on. Closing a pane's terminal waits for its reader, and that reader
    // is parked in a held send, so the close has to release it.
    let mut supervisor = RunningSupervisor::start(LONG_IDLE_EXIT);
    let mut link = TestLink::open(&supervisor.addr);
    let pane = PaneId::new();
    let SupervisorResult::Spawned { pid } = link.ask(SupervisorRequestKind::Spawn {
        pane_id: pane,
        spec: shell_spec("printf 'ready'; sleep 1; printf 'held'; sleep 30"),
        size: PANE_SIZE,
    }) else {
        panic!("the supervisor opens the pane");
    };
    assert_eq!(link.next_output(pane), b"ready".to_vec());

    assert_eq!(
        link.ask(SupervisorRequestKind::PauseOutput),
        SupervisorResult::Done
    );
    thread::sleep(Duration::from_secs(2));

    assert_eq!(
        link.ask(SupervisorRequestKind::Kill {
            pane_id: pane,
            kill_policy: KillPolicy::Tree,
        }),
        SupervisorResult::Done
    );
    assert_eq!(
        link.ask(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new())
    );

    assert_eq!(
        link.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
    wait_until("the pane's child was reaped", || !process_alive(pid));
}

#[cfg(unix)]
#[test]
fn shutting_down_closes_every_pane_the_supervisor_still_holds() {
    let mut supervisor = RunningSupervisor::start(TEST_IDLE_EXIT);
    let mut link = TestLink::open(&supervisor.addr);
    let pane = PaneId::new();
    let SupervisorResult::Spawned { pid } = link.ask(SupervisorRequestKind::Spawn {
        pane_id: pane,
        spec: shell_spec("sleep 300"),
        size: PANE_SIZE,
    }) else {
        panic!("the supervisor opens the pane");
    };

    assert_eq!(
        link.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();

    wait_until("the pane's child was reaped", || !process_alive(pid));
}

/// True while process `pid` is still around (`kill -0` succeeds).
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// A sink that keeps everything it is handed, so a test can check exactly what
/// reached the consumer.
struct RecordingSink {
    /// Every chunk taken, oldest first, with the pane that printed it.
    chunks: Mutex<Vec<(PaneId, Vec<u8>)>>,
    /// Every exit taken, oldest first, with the pane that ended.
    exits: Mutex<Vec<(PaneId, ExitStatus)>>,
}

impl RecordingSink {
    fn new() -> Arc<Self> {
        Arc::new(RecordingSink {
            chunks: Mutex::new(Vec::new()),
            exits: Mutex::new(Vec::new()),
        })
    }

    /// Every byte this sink has been handed for `pane`, in order.
    fn bytes(&self, pane: PaneId) -> Vec<u8> {
        self.chunks
            .lock()
            .expect("recording sink")
            .iter()
            .filter(|(taken, _)| *taken == pane)
            .flat_map(|(_, bytes)| bytes.clone())
            .collect()
    }

    /// Every exit this sink has been handed, in order.
    fn exits(&self) -> Vec<(PaneId, ExitStatus)> {
        self.exits.lock().expect("recording sink").clone()
    }
}

impl PtySink for RecordingSink {
    fn output(&self, pane: PaneId, bytes: Vec<u8>) -> bool {
        self.chunks
            .lock()
            .expect("recording sink")
            .push((pane, bytes));
        true
    }

    fn exit(&self, pane: PaneId, status: ExitStatus) {
        self.exits
            .lock()
            .expect("recording sink")
            .push((pane, status));
    }
}

/// Wait until `sink` holds `needle` for `pane`, and hand back everything it
/// holds for it.
#[cfg(unix)]
fn read_sink_until(sink: &RecordingSink, pane: PaneId, needle: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        let got = sink.bytes(pane);
        if got.windows(needle.len()).any(|window| window == needle) {
            return got;
        }
        assert!(
            Instant::now() < deadline,
            "the consumer was never handed {:?}; it holds {:?}",
            String::from_utf8_lossy(needle),
            String::from_utf8_lossy(&got)
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_panes_child_prints_through_the_link_once_its_terminal_is_answered() {
    // A real pane inside the supervisor, driven by the backend a session server
    // drives, with only the supervisor running. Nothing outside the pane
    // answers its terminal's cursor-position query, and a Windows pseudoconsole
    // hands over nothing its child printed until that answer lands.
    let mut supervisor = RunningSupervisor::start(LONG_IDLE_EXIT);
    let sink = RecordingSink::new();
    let backend = SupervisorPtyBackend::connect(
        &supervisor.addr,
        token(),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[],
    )
    .expect("the backend opens the link");
    let pane = PaneId::new();

    backend
        .spawn(pane, printing_spec(PRINTED_MARKER), PANE_SIZE)
        .expect("the supervisor opens the pane");
    let held = read_until_printed(&sink, pane, PRINTED_MARKER.as_bytes());

    assert_eq!(
        count_queries(&held),
        0,
        "the pane's reader takes the terminal's query out, so none crosses the link"
    );
    // The child kept running after it printed, so what arrived is its output
    // and not the flush of a child that ended.
    assert_eq!(
        sink.exits(),
        Vec::<(PaneId, ExitStatus)>::new(),
        "the pane that printed is still running"
    );

    backend.shut_down().expect("the supervisor is told to end");
    supervisor.join();
}

#[cfg(unix)]
#[test]
fn a_pane_driven_through_the_backend_prints_back_into_its_sink() {
    // Both halves of the link, end to end: the session server's backend opens
    // a pane in the supervisor and the child's bytes arrive in the sink, which
    // is what the runtime inbox reads in the running binary.
    let mut supervisor = RunningSupervisor::start(TEST_IDLE_EXIT);
    let sink = RecordingSink::new();
    let backend = SupervisorPtyBackend::connect(
        &supervisor.addr,
        token(),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[],
    )
    .expect("the backend opens the link");
    let pane = PaneId::new();

    backend
        .spawn(pane, shell_spec("printf 'ready'; cat"), PANE_SIZE)
        .expect("the supervisor opens the pane");
    read_sink_until(&sink, pane, b"ready");
    backend
        .write(pane, b"echoed\n")
        .expect("the bytes reach the child");
    let seen = read_sink_until(&sink, pane, b"echoed");

    assert_eq!(
        &seen[..b"ready".len()],
        b"ready",
        "the child's first bytes reach the sink before anything written to it"
    );

    backend.shut_down().expect("the supervisor is told to end");
    supervisor.join();
}

#[cfg(unix)]
#[test]
fn holding_the_readers_still_stops_a_live_pane_reaching_the_consumer() {
    // The Windows half of the swap, end to end through the backend the session
    // server drives: every reader is inside the supervisor, so the pause has to
    // stop the supervisor writing, and what the pane prints under the hold has
    // to reach the consumer once the readers go back to work.
    let mut supervisor = RunningSupervisor::start(LONG_IDLE_EXIT);
    let sink = RecordingSink::new();
    let backend = SupervisorPtyBackend::connect(
        &supervisor.addr,
        token(),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[],
    )
    .expect("the backend opens the link");
    let pane = PaneId::new();
    backend
        .spawn(
            pane,
            shell_spec("printf 'ready'; sleep 1; printf 'held'; cat"),
            PANE_SIZE,
        )
        .expect("the supervisor opens the pane");
    read_sink_until(&sink, pane, b"ready");

    backend.pause_readers().expect("the pause answers");
    let carried = backend.carried_panes();

    // The pane prints its second chunk inside this window.
    thread::sleep(Duration::from_secs(2));
    assert_eq!(
        sink.bytes(pane),
        b"ready".to_vec(),
        "nothing may reach the consumer while its readers are held"
    );

    assert_eq!(carried.len(), 1, "the paused backend names its one pane");
    assert_eq!(carried[0].pane_id, pane);
    assert_eq!(carried[0].size, PANE_SIZE);

    backend.resume_readers();
    let seen = read_sink_until(&sink, pane, b"held");
    assert_eq!(
        seen,
        b"readyheld".to_vec(),
        "the held chunk reaches the consumer whole, once, and after the first"
    );

    backend
        .write(pane, b"after\n")
        .expect("the bytes reach the child");
    read_sink_until(&sink, pane, b"after");

    backend.shut_down().expect("the supervisor is told to end");
    supervisor.join();
}

#[cfg(unix)]
#[test]
fn reconnecting_keeps_the_panes_the_supervisor_still_holds_and_settles_the_rest() {
    // The swap seen from the replacement image: it carries a list of panes,
    // one of which the supervisor no longer holds, while the supervisor holds
    // one the list does not name. Neither side may be left with an orphan.
    //
    // The first link is a hand-written one, because dropping it closes its
    // socket outright — which is what the session server process exiting looks
    // like from the supervisor's side.
    let mut supervisor = RunningSupervisor::start(LONG_IDLE_EXIT);
    let mut opening = TestLink::open(&supervisor.addr);
    let kept = PaneId::new();
    let unnamed = PaneId::new();
    let SupervisorResult::Spawned { .. } = opening.ask(SupervisorRequestKind::Spawn {
        pane_id: kept,
        spec: shell_spec("sleep 300"),
        size: PANE_SIZE,
    }) else {
        panic!("the supervisor opens the kept pane");
    };
    let SupervisorResult::Spawned { pid: unnamed_pid } =
        opening.ask(SupervisorRequestKind::Spawn {
            pane_id: unnamed,
            spec: shell_spec("sleep 300"),
            size: PANE_SIZE,
        })
    else {
        panic!("the supervisor opens the pane nobody will carry");
    };
    drop(opening);

    // The replacement image carries the pane that survived and one that never
    // existed, and does not carry the one the supervisor still holds.
    let vanished = PaneId::new();
    let resumed = RecordingSink::new();
    let backend = SupervisorPtyBackend::connect(
        &supervisor.addr,
        token(),
        Arc::clone(&resumed) as Arc<dyn PtySink>,
        &[kept, vanished],
    )
    .expect("the replacement image opens the link");

    assert_eq!(
        resumed.exits(),
        vec![(vanished, ExitStatus::ExitCode(-1))],
        "a carried pane the supervisor does not hold is reported as ended"
    );
    let carried = backend.carried_panes();
    assert_eq!(carried.len(), 1, "only the surviving pane is driven");
    assert_eq!(carried[0].pane_id, kept);
    wait_until("the pane nobody carried was killed", || {
        !process_alive(unnamed_pid)
    });

    backend.shut_down().expect("the supervisor is told to end");
    supervisor.join();
}

#[test]
fn the_hidden_subcommand_is_the_one_the_supervisor_is_started_under() {
    use clap::Parser;

    let session = SessionId::new();
    let cli = koshi::cli::Cli::try_parse_from([
        "koshi",
        PTY_SUPERVISOR_SUBCOMMAND,
        &session.to_string(),
        "k7QxSecret",
        koshi_link::router_client::RUNTIME_DIR_FLAG,
        "/run/user/1000/koshi",
    ])
    .expect("the hidden subcommand parses");

    assert_eq!(
        cli.command,
        Some(koshi::cli::CliCommand::ServePtySupervisor {
            session_id: session,
            token: "k7QxSecret".to_string(),
            runtime_dir: Some(std::path::PathBuf::from("/run/user/1000/koshi")),
        })
    );
}

#[test]
fn a_pane_event_waits_until_a_hello_opens_the_link() {
    // The token is what makes a link this session's own. A peer that opens the
    // socket and never presents it must be handed no pane output, so the sink
    // holds its chunk until the Hello is accepted and hands it over then.
    let runtime_dir = tempfile::tempdir().expect("a temporary directory is created");
    let addr = link_addr(runtime_dir.path());
    let listener = Listener::bind(&addr).expect("the link binds");
    let connecting = {
        let addr = addr.clone();
        thread::spawn(move || Connection::connect(&addr).expect("the peer connects"))
    };
    let accepted = listener.accept().expect("the peer is accepted");
    let mut peer = connecting.join().expect("the connecting thread ended");
    let (_reader, writer) = accepted.split();

    let sink = LinkSink::new();
    sink.link_up(writer);
    let pane = PaneId::new();
    let forwarding = {
        let sink = Arc::clone(&sink);
        thread::spawn(move || sink.output(pane, b"held".to_vec()))
    };

    thread::sleep(TEST_IDLE_EXIT);
    assert!(
        !forwarding.is_finished(),
        "a link with no accepted Hello must be handed nothing"
    );

    sink.open();
    wait_until("the held chunk went out", || forwarding.is_finished());
    assert!(
        forwarding.join().expect("the forwarding thread ended"),
        "the chunk goes out once a Hello opened the link"
    );
    assert_eq!(
        peer.recv::<SupervisorMessage>().expect("a frame arrives"),
        SupervisorMessage::Event(SupervisorEvent::Output {
            pane_id: pane,
            bytes: b"held".to_vec(),
        })
    );
}

#[test]
fn a_send_parked_for_a_pane_being_closed_gives_up_and_leaves_the_others_parked() {
    // Closing a pane's terminal waits for that pane's reader to carry the
    // terminal to its end, and that reader can be parked inside a send. Letting
    // the pane go has to release exactly that send, or the close never
    // finishes; every other pane's send must stay where it is.
    let sink = LinkSink::new();
    let closing = PaneId::new();
    let other = PaneId::new();

    let for_closing = {
        let sink = Arc::clone(&sink);
        thread::spawn(move || sink.output(closing, b"last words".to_vec()))
    };
    let for_other = {
        let sink = Arc::clone(&sink);
        thread::spawn(move || sink.output(other, b"still going".to_vec()))
    };
    thread::sleep(TEST_IDLE_EXIT);
    assert!(
        !for_closing.is_finished() && !for_other.is_finished(),
        "with no link up, both sends wait"
    );

    sink.let_go(closing);

    wait_until("the send for the closing pane gave up", || {
        for_closing.is_finished()
    });
    assert!(
        !for_closing.join().expect("the send thread ended"),
        "a send for a pane being closed reports that nobody wants its chunk"
    );
    assert!(
        !for_other.is_finished(),
        "and a send for another pane keeps waiting for a link"
    );

    sink.close();

    wait_until("every remaining send gave up", || for_other.is_finished());
    assert!(
        !for_other.join().expect("the send thread ended"),
        "the supervisor ending reports the same to every send left"
    );
}

#[test]
fn a_send_for_a_pane_being_closed_gives_up_without_waiting_and_waits_again_once_it_is_closed() {
    // A pane is let go, closed, then forgotten. A send arriving while it is
    // being closed must answer at once rather than park; the list holds only
    // the panes being closed right now, so a send after that parks as usual.
    let sink = LinkSink::new();
    let pane = PaneId::new();

    sink.let_go(pane);
    assert!(
        !sink.output(pane, b"dropped".to_vec()),
        "a chunk for a pane being closed is refused on the spot"
    );

    sink.forget(pane);
    let parked = {
        let sink = Arc::clone(&sink);
        thread::spawn(move || sink.output(pane, b"parked".to_vec()))
    };
    thread::sleep(TEST_IDLE_EXIT);
    assert!(
        !parked.is_finished(),
        "a pane the supervisor is no longer closing waits for a link again"
    );

    sink.close();
    wait_until("the parked send gave up", || parked.is_finished());
    assert!(!parked.join().expect("the send thread ended"));
}

#[test]
fn every_send_after_the_supervisor_starts_ending_gives_up_at_once() {
    // The supervisor closes every pane it holds when it ends, and each close
    // waits on that pane's reader. Ending the sends first is what lets those
    // closes finish, including for a pane whose send arrives afterwards.
    let sink = LinkSink::new();
    let pane = PaneId::new();

    sink.close();

    assert!(
        !sink.output(pane, b"too late".to_vec()),
        "a chunk arriving after the supervisor started ending is refused at once"
    );
    // An exit takes the same route and must not wait either; a wait here would
    // hold the supervisor's own teardown.
    sink.exit(pane, ExitStatus::ExitCode(0));
    assert!(
        !sink.output(PaneId::new(), b"another pane".to_vec()),
        "and so is every other pane's"
    );
}

#[test]
fn a_shutdown_before_a_hello_is_refused_and_the_supervisor_keeps_running() {
    // Ending the process is the one request that cannot be allowed through the
    // gate: a peer that has not presented the token must not be able to close
    // the session's panes.
    let mut supervisor = RunningSupervisor::start(TEST_IDLE_EXIT * 10);
    let mut stranger = TestLink {
        connection: Connection::connect(&supervisor.addr).expect("the link opens"),
        next_request_id: 1,
        events: Vec::new(),
    };

    assert_eq!(
        stranger.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Shutdown arrived before a Hello opened the link".to_string(),
        })
    );
    assert!(
        !supervisor.ended(),
        "a refused Shutdown must leave the supervisor running"
    );

    assert_eq!(
        stranger.ask(SupervisorRequestKind::hello(token())),
        SupervisorResult::Hello {
            protocol_version: SUPERVISOR_PROTOCOL_VERSION,
        }
    );
    assert_eq!(
        stranger.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}

#[test]
fn a_second_link_is_answered_only_once_the_first_one_ends() {
    // The supervisor serves one linked session server at a time. Two swaps
    // reaching it at once must not have the second one driving the panes while
    // the first still holds the link: the second waits, and it is served the
    // moment the first goes.
    let mut supervisor = RunningSupervisor::start(Duration::from_secs(30));
    let mut first = TestLink::open(&supervisor.addr);
    assert_eq!(
        first.ask(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new())
    );

    let mut opening = Connection::connect(&supervisor.addr).expect("the second link opens");
    opening
        .send(&SupervisorRequest {
            request_id: 1,
            kind: SupervisorRequestKind::hello(token()),
        })
        .expect("the second Hello is sent");
    let waiting = thread::spawn(move || {
        let answer: SupervisorMessage = opening.recv().expect("a frame arrives");
        (answer, opening)
    });
    thread::sleep(TEST_IDLE_EXIT);
    assert!(
        !waiting.is_finished(),
        "a second link must wait while the first one is being served"
    );

    // The first session server's process image goes.
    drop(first);

    wait_until("the second link was served", || waiting.is_finished());
    let (answer, connection) = waiting.join().expect("the waiting thread ended");
    assert_eq!(
        answer,
        SupervisorMessage::Response(SupervisorResponse {
            request_id: Some(1),
            result: SupervisorResult::Hello {
                protocol_version: SUPERVISOR_PROTOCOL_VERSION,
            },
        })
    );

    let mut second = TestLink {
        connection,
        next_request_id: 2,
        events: Vec::new(),
    };
    assert_eq!(
        second.ask(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join();
}
