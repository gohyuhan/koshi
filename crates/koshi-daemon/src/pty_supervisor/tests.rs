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
const HANG_GUARD_DURATION: Duration = Duration::from_secs(10);

/// The idle window the tests run the supervisor with, short enough that a test
/// can sit through it.
const TEST_IDLE_EXIT_DURATION: Duration = Duration::from_millis(300);

/// The idle window for the tests that let the link go and bring it back, long
/// enough that none of them sits through it.
const LONG_IDLE_EXIT_DURATION: Duration = Duration::from_secs(30);

/// The secret every test link presents.
fn build_test_connection_token() -> ConnectionToken {
    ConnectionToken::from_secret("k7QxSecret")
}

/// A supervisor running on a thread of its own, with the address to link to
/// it.
struct RunningSupervisor {
    /// The address a link connects to.
    supervisor_address: String,
    /// The supervisor's own thread, joined to prove it ended.
    supervisor_thread: Option<thread::JoinHandle<()>>,
    /// The socket file's directory, kept so it outlives the supervisor.
    _runtime_directory: tempfile::TempDir,
}

impl RunningSupervisor {
    /// Start a supervisor holding no pane, with `idle_exit_duration` as its idle
    /// window.
    fn start_with_idle_exit_duration(idle_exit_duration: Duration) -> RunningSupervisor {
        let (runtime_directory, supervisor_address, supervisor_listener) = bind_test_listener();
        let supervisor_thread = thread::Builder::new()
            .name("supervisor-under-test".to_string())
            .spawn(move || {
                run_pane_supervisor_loop(
                    supervisor_listener,
                    &build_test_connection_token(),
                    idle_exit_duration,
                )
            })
            .expect("the supervisor thread starts");

        RunningSupervisor {
            supervisor_address,
            supervisor_thread: Some(supervisor_thread),
            _runtime_directory: runtime_directory,
        }
    }

    /// Whether the supervisor's thread has ended.
    fn is_ended(&self) -> bool {
        self.supervisor_thread
            .as_ref()
            .expect("the supervisor thread handle")
            .is_finished()
    }

    /// Wait for the supervisor to end, failing the test rather than hanging
    /// when it never does.
    fn join_supervisor_thread(&mut self) {
        wait_until_condition("the supervisor ended", || self.is_ended());
        self.supervisor_thread
            .take()
            .expect("the supervisor thread handle")
            .join()
            .expect("the supervisor thread ended without panicking");
    }
}

/// An address for one test's link, different on every call. On Unix it is a
/// socket file inside `dir`; on Windows it is a pipe name that does not use
/// `dir`, made of this process's id and a counter.
fn build_test_supervisor_address(runtime_directory: &Path) -> String {
    #[cfg(unix)]
    {
        runtime_directory
            .join("supervisor.sock")
            .display()
            .to_string()
    }
    #[cfg(windows)]
    {
        let _ = runtime_directory;
        static NEXT_PIPE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        format!(
            "koshi-pty-test-{}-{}",
            std::process::id(),
            NEXT_PIPE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
    }
}

/// A listener bound to an address of its own, with that address and the
/// directory it lives in. The directory is returned so it outlives the
/// listener: dropping it removes a Unix socket file.
fn bind_test_listener() -> (tempfile::TempDir, String, Listener) {
    let runtime_directory = tempfile::tempdir().expect("a temporary directory is created");
    let supervisor_address = build_test_supervisor_address(runtime_directory.path());
    let supervisor_listener = Listener::bind(&supervisor_address).expect("the link binds");
    (runtime_directory, supervisor_address, supervisor_listener)
}

/// One hand-written session server on the other end of a link.
///
/// Every response and every event arrives on the same connection, so
/// [`send_request_and_receive_result`](TestLink::send_request_and_receive_result)
/// keeps the events it passes on the way to a response and the
/// `receive_next_event` readers hand them back.
struct TestLink {
    /// The connection itself. Dropping it closes the socket outright.
    connection: Connection,
    /// The id the next request carries.
    next_request_id: u64,
    /// Events read while waiting for a response, oldest first.
    pending_events: Vec<SupervisorEvent>,
}

impl TestLink {
    /// Open a link to `supervisor_address` without a token, so the supervisor's gate
    /// stays closed.
    fn connect_without_hello(supervisor_address: &str) -> TestLink {
        TestLink {
            connection: Connection::connect(supervisor_address).expect("the link opens"),
            next_request_id: 1,
            pending_events: Vec::new(),
        }
    }

    /// Open a link to `address` and send the Hello that opens it.
    fn connect_with_connection_token(supervisor_address: &str) -> TestLink {
        let mut link = TestLink::connect_without_hello(supervisor_address);
        assert_eq!(
            link.send_request_and_receive_result(SupervisorRequestKind::build_hello_request(
                build_test_connection_token(),
            )),
            SupervisorResult::Hello {
                protocol_version: SUPERVISOR_PROTOCOL_VERSION,
            }
        );
        link
    }

    /// Send one request and read frames until its response arrives, keeping any
    /// event read on the way.
    fn send_request_and_receive_result(
        &mut self,
        request_kind: SupervisorRequestKind,
    ) -> SupervisorResult {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        self.connection
            .send(&SupervisorRequest {
                request_id,
                request_kind,
            })
            .expect("the request is sent");
        loop {
            match self.receive_supervisor_message() {
                SupervisorMessage::Response(response) => {
                    assert_eq!(response.request_id, Some(request_id));
                    return response.answer_result;
                }
                SupervisorMessage::Event(supervisor_event) => {
                    self.pending_events.push(supervisor_event)
                }
            }
        }
    }

    /// Send raw JSON as one frame, for a request no build has.
    fn send_raw_frame(&mut self, frame_text: &str) {
        let raw_frame: serde_json::Value =
            serde_json::from_str(frame_text).expect("the raw request is JSON");
        self.connection
            .send(&raw_frame)
            .expect("the raw request is sent");
    }

    /// The next event, reading more frames when none is held. A response read
    /// on the way fails the test: nothing asked for one.
    #[cfg(unix)]
    fn receive_next_event(&mut self) -> SupervisorEvent {
        if !self.pending_events.is_empty() {
            return self.pending_events.remove(0);
        }
        match self.receive_supervisor_message() {
            SupervisorMessage::Event(supervisor_event) => supervisor_event,
            SupervisorMessage::Response(response) => {
                panic!("no request was in flight, yet a response arrived: {response:?}")
            }
        }
    }

    /// The next chunk of `pane_id`'s output, skipping every other event.
    #[cfg(unix)]
    fn receive_next_output(&mut self, pane_id: PaneId) -> Vec<u8> {
        loop {
            if let SupervisorEvent::Output {
                pane_id: event_pane_id,
                output_bytes,
            } = self.receive_next_event()
            {
                if event_pane_id == pane_id {
                    return output_bytes;
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
    fn receive_next_event_or_fail(&mut self) -> SupervisorEvent {
        let deadline = Instant::now() + HANG_GUARD_DURATION;
        loop {
            if !self.pending_events.is_empty() {
                return self.pending_events.remove(0);
            }
            assert!(
                Instant::now() < deadline,
                "no event reached this link, and one was due"
            );
            self.send_request_and_receive_result(SupervisorRequestKind::ListPanes);
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// The next chunk of `pane_id`'s output, without blocking on one that never
    /// comes.
    #[cfg(unix)]
    fn receive_next_output_or_fail(&mut self, pane_id: PaneId) -> Vec<u8> {
        loop {
            if let SupervisorEvent::Output {
                pane_id: event_pane_id,
                output_bytes,
            } = self.receive_next_event_or_fail()
            {
                if event_pane_id == pane_id {
                    return output_bytes;
                }
            }
        }
    }

    /// One frame from the supervisor, decoded as this build reads it.
    fn receive_supervisor_message(&mut self) -> SupervisorMessage {
        self.connection.recv().expect("a frame arrives")
    }
}

/// Wait until `is_condition_met` answers `true`, failing the test rather than hanging when
/// it never does.
fn wait_until_condition(condition_description: &str, is_condition_met: impl Fn() -> bool) {
    let deadline = Instant::now() + HANG_GUARD_DURATION;
    while !is_condition_met() {
        assert!(
            Instant::now() < deadline,
            "{condition_description} never happened"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// A spec running `shell_script` under `/bin/sh`.
#[cfg(unix)]
fn build_shell_spawn_spec(shell_script: &str) -> koshi_core::process::SpawnSpec {
    koshi_core::process::SpawnSpec {
        program: std::path::PathBuf::from("/bin/sh"),
        arguments: vec!["-c".to_string(), shell_script.to_string()],
        working_directory: None,
        environment_variables: std::collections::BTreeMap::new(),
        shell_kind: koshi_core::process::ShellKind::Other("sh".to_string()),
    }
}

/// The size every pane in these tests opens at.
const TEST_PTY_SIZE: koshi_core::process::PtySize = koshi_core::process::PtySize {
    column_count: 80,
    row_count: 24,
};

/// The cursor-position query (DSR, `CSI 6 n`) a pane's terminal asks.
///
/// A Windows pseudoconsole asks one as the pane starts and hands over nothing
/// its child printed until the response reaches it. The supervisor's backend
/// queues the send_response as the pane opens, and the pane's reader removes the query
/// from the output, so none reaches the link.
const CURSOR_POSITION_QUERY_BYTES: &[u8] = b"\x1b[6n";

/// The line the pane opened by
/// [`a_panes_child_prints_through_the_link_once_its_terminal_is_answered`]
/// prints. Nothing else in that pane's output holds it, so finding it means the
/// child ran.
const PRINTED_OUTPUT_MARKER: &str = "koshi-printed";

/// How long the wait for a pane to print gives it, which covers opening the
/// terminal, launching the child and carrying its bytes over the link on a
/// shared runner.
const PRINT_WAIT_DURATION: Duration = Duration::from_secs(20);

/// A spec whose child prints `marker` and then stays alive, under the
/// platform's own command interpreter.
///
/// `cmd.exe /K` runs the command and keeps the interpreter, the way `cat` keeps
/// `/bin/sh` waiting on its input.
fn build_printing_spawn_spec(output_marker: &str) -> koshi_core::process::SpawnSpec {
    #[cfg(unix)]
    let (program, shell_flag, shell_script) =
        ("/bin/sh", "-c", format!("printf '{output_marker}'; cat"));
    #[cfg(windows)]
    let (program, shell_flag, shell_script) = ("cmd.exe", "/K", format!("echo {output_marker}"));
    let program = std::path::PathBuf::from(program);
    koshi_core::process::SpawnSpec {
        shell_kind: koshi_core::process::ShellKind::from_program(&program),
        program,
        arguments: vec![shell_flag.to_string(), shell_script],
        working_directory: None,
        environment_variables: std::collections::BTreeMap::new(),
    }
}

/// Wait up to `wait_duration` until `recording_sink` holds `expected_bytes` for
/// `pane_id`, and hand back every byte it holds for that pane.
/// Sends nothing to the supervisor.
///
/// Fails the test once `wait_duration` has passed, naming what the sink holds: nothing
/// held means no byte crossed the link, and the cursor-position query alone
/// means the pane's reader delivered it instead of removing it.
fn collect_sink_bytes_until(
    recording_sink: &RecordingSink,
    pane_id: PaneId,
    expected_bytes: &[u8],
    wait_duration: Duration,
) -> Vec<u8> {
    let deadline = Instant::now() + wait_duration;
    loop {
        let recorded_bytes = recording_sink.list_bytes_for_pane(pane_id);
        if recorded_bytes
            .windows(expected_bytes.len())
            .any(|byte_window| byte_window == expected_bytes)
        {
            return recorded_bytes;
        }
        assert!(
            Instant::now() < deadline,
            "the consumer was never handed {:?}; it holds {:?}",
            String::from_utf8_lossy(expected_bytes),
            String::from_utf8_lossy(&recorded_bytes),
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// How many cursor-position queries `output` holds.
fn count_cursor_position_queries(output_bytes: &[u8]) -> usize {
    output_bytes
        .windows(CURSOR_POSITION_QUERY_BYTES.len())
        .filter(|byte_window| *byte_window == CURSOR_POSITION_QUERY_BYTES)
        .count()
}

#[test]
fn a_link_opens_on_a_hello_and_the_supervisor_starts_holding_no_pane() {
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new())
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[test]
fn a_request_before_a_hello_is_refused_and_the_link_keeps_serving() {
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_without_hello(&supervisor.supervisor_address);

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "ListPanes arrived before a Hello opened the link".to_string(),
        })
    );
    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::build_hello_request(
            build_test_connection_token(),
        )),
        SupervisorResult::Hello {
            protocol_version: SUPERVISOR_PROTOCOL_VERSION,
        }
    );
    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new())
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[test]
fn a_hello_carrying_the_wrong_token_is_refused_and_the_link_keeps_serving() {
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_without_hello(&supervisor.supervisor_address);

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::build_hello_request(
            ConnectionToken::from_secret("wrongToken")
        )),
        SupervisorResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match the supervisor's".to_string(),
        })
    );
    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::build_hello_request(
            build_test_connection_token(),
        )),
        SupervisorResult::Hello {
            protocol_version: SUPERVISOR_PROTOCOL_VERSION,
        }
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[test]
fn a_request_kind_the_supervisor_does_not_have_is_refused_by_name_and_the_link_keeps_serving() {
    // What a newer session server driving an older supervisor looks like. The
    // refusal names the kind so that server learns which one it cannot use,
    // and every other request on the same link still works.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);

    link.send_raw_frame(r#"{"request_id":99,"kind":{"Rehome":{"pane_id":1}}}"#);
    let SupervisorMessage::Response(response) = link.receive_supervisor_message() else {
        panic!("the refusal is a response, not an event");
    };
    assert_eq!(
        response,
        SupervisorResponse {
            request_id: Some(99),
            answer_result: SupervisorResult::Error(IpcErrorPayload {
                code: IpcErrorCode::UnsupportedKind,
                message: "this supervisor has no request kind named Rehome".to_string(),
            }),
        }
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new()),
        "the link keeps serving after a refusal"
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[test]
fn bytes_that_are_not_a_readable_request_are_refused_and_the_link_keeps_serving() {
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);

    link.send_raw_frame(r#"{"request_id":"not a number","kind":"ListPanes"}"#);
    let SupervisorMessage::Response(response) = link.receive_supervisor_message() else {
        panic!("the refusal is a response, not an event");
    };
    assert_eq!(
        response,
        SupervisorResponse {
            request_id: None,
            answer_result: SupervisorResult::Error(IpcErrorPayload {
                code: IpcErrorCode::MalformedRequest,
                message: "the bytes received are not a request this build can read".to_string(),
            }),
        }
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new()),
        "an unreadable frame leaves the stream aligned"
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[test]
fn a_request_kind_the_supervisor_does_not_have_is_refused_for_the_missing_hello_before_one_arrives()
{
    // A peer that has presented no token is told nothing about which kinds
    // exist, not even that this one does not: the refusal names the missing
    // Hello, the same as any other kind arriving before one.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_without_hello(&supervisor.supervisor_address);

    link.send_raw_frame(r#"{"request_id":99,"kind":{"Rehome":{"pane_id":1}}}"#);
    let SupervisorMessage::Response(response) = link.receive_supervisor_message() else {
        panic!("the refusal is a response, not an event");
    };
    assert_eq!(
        response,
        SupervisorResponse {
            request_id: Some(99),
            answer_result: SupervisorResult::Error(IpcErrorPayload {
                code: IpcErrorCode::HelloRequired,
                message: "Rehome arrived before a Hello opened the link".to_string(),
            }),
        }
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::build_hello_request(
            build_test_connection_token(),
        )),
        SupervisorResult::Hello {
            protocol_version: SUPERVISOR_PROTOCOL_VERSION,
        }
    );
    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[test]
fn bytes_that_are_not_a_readable_request_before_a_hello_are_refused_and_the_link_keeps_serving() {
    // An unreadable frame is answered without the gate being asked, so a peer
    // that has presented no token still gets the aligned stream back.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_without_hello(&supervisor.supervisor_address);

    link.send_raw_frame(r#"{"request_id":"not a number","kind":"ListPanes"}"#);
    let SupervisorMessage::Response(response) = link.receive_supervisor_message() else {
        panic!("the refusal is a response, not an event");
    };
    assert_eq!(
        response,
        SupervisorResponse {
            request_id: None,
            answer_result: SupervisorResult::Error(IpcErrorPayload {
                code: IpcErrorCode::MalformedRequest,
                message: "the bytes received are not a request this build can read".to_string(),
            }),
        }
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::build_hello_request(
            build_test_connection_token(),
        )),
        SupervisorResult::Hello {
            protocol_version: SUPERVISOR_PROTOCOL_VERSION,
        },
        "a Hello still opens the link after an unreadable frame"
    );
    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new())
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[test]
fn a_second_hello_on_an_open_link_is_answered_again_and_leaves_it_open() {
    // A session server that presents the token twice is answered twice. The
    // second Hello settles the same version and the link keeps serving.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::build_hello_request(
            build_test_connection_token(),
        )),
        SupervisorResult::Hello {
            protocol_version: SUPERVISOR_PROTOCOL_VERSION,
        }
    );
    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new())
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[test]
fn a_hello_carrying_the_wrong_token_after_an_open_one_is_refused_and_leaves_the_link_open() {
    // A refused Hello leaves the gate as it was, so a wrong token presented
    // second cannot shut a link that a right one already opened.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::build_hello_request(
            ConnectionToken::from_secret("wrongToken")
        )),
        SupervisorResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match the supervisor's".to_string(),
        })
    );
    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new()),
        "the link an accepted Hello opened stays open"
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[test]
fn killing_a_pane_the_supervisor_does_not_hold_is_refused_by_name() {
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    let missing_pane_id = PaneId::new();

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Kill {
            pane_id: missing_pane_id,
            kill_policy: KillPolicy::Force,
        }),
        SupervisorResult::Error(IpcErrorPayload {
            code: IpcErrorCode::Unknown,
            message: format!("invalid pane: id - {missing_pane_id}"),
        })
    );
    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new()),
        "the link keeps serving after a refused kill"
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[test]
fn a_pane_the_supervisor_does_not_hold_reports_no_live_working_directory() {
    // Asking for a directory is answered rather than refused: the send_response for a
    // pane nothing holds is the same as for one the operating system cannot
    // report on.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    let missing_pane_id = PaneId::new();

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::LiveCwd {
            pane_id: missing_pane_id,
        }),
        SupervisorResult::Cwd(None)
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[test]
fn lifting_a_hold_nobody_asked_for_is_answered_and_the_link_keeps_serving() {
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ResumeOutput),
        SupervisorResult::Done
    );
    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new())
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[test]
fn a_supervisor_holding_no_pane_ends_once_the_idle_window_passes_with_no_link() {
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);

    assert!(
        !supervisor.is_ended(),
        "the supervisor must still be there inside its idle window"
    );
    supervisor.join_supervisor_thread();
}

#[test]
fn a_link_inside_the_idle_window_keeps_the_supervisor_alive() {
    // A session server whose first link lands just as the supervisor would
    // have ended must still be served, so a session never loses the process
    // holding its panes to a race.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);

    thread::sleep(TEST_IDLE_EXIT_DURATION * 3);
    assert!(
        !supervisor.is_ended(),
        "a linked supervisor must not end on its idle window"
    );
    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new())
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[cfg(unix)]
#[test]
fn a_pane_opened_through_the_link_reports_its_child_and_prints_back_over_it() {
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    let pane_id = PaneId::new();

    let SupervisorResult::Spawned { process_id } =
        link.send_request_and_receive_result(SupervisorRequestKind::Spawn {
            pane_id,
            spawn_spec: build_shell_spawn_spec("printf 'ready'; sleep 30"),
            pty_size: TEST_PTY_SIZE,
        })
    else {
        panic!("the supervisor opens the pane");
    };

    assert!(process_id > 0, "an open pane names a real child process");
    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(vec![SupervisorPane {
            pane_id,
            process_id,
            pty_size: TEST_PTY_SIZE,
        }])
    );
    assert_eq!(link.receive_next_output(pane_id), b"ready".to_vec());

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[cfg(unix)]
#[test]
fn bytes_written_over_the_link_reach_the_child_and_come_back() {
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    let pane_id = PaneId::new();
    let SupervisorResult::Spawned { .. } =
        link.send_request_and_receive_result(SupervisorRequestKind::Spawn {
            pane_id,
            spawn_spec: build_shell_spawn_spec("printf 'ready'; cat"),
            pty_size: TEST_PTY_SIZE,
        })
    else {
        panic!("the supervisor opens the pane");
    };
    assert_eq!(link.receive_next_output(pane_id), b"ready".to_vec());

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Write {
            pane_id,
            input_bytes: b"echoed\n".to_vec(),
        }),
        SupervisorResult::Done
    );

    // The terminal echoes what is written to it and `cat` prints it again, so
    // the bytes come back over the link.
    let mut observed_output_bytes = Vec::new();
    while !observed_output_bytes
        .windows(b"echoed".len())
        .any(|window| window == b"echoed")
    {
        observed_output_bytes.extend(link.receive_next_output(pane_id));
    }

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[cfg(unix)]
#[test]
fn resizing_and_killing_a_pane_over_the_link_reach_the_backend() {
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    let pane_id = PaneId::new();
    let SupervisorResult::Spawned { process_id } =
        link.send_request_and_receive_result(SupervisorRequestKind::Spawn {
            pane_id,
            spawn_spec: build_shell_spawn_spec("sleep 30"),
            pty_size: TEST_PTY_SIZE,
        })
    else {
        panic!("the supervisor opens the pane");
    };

    let wider_pty_size = koshi_core::process::PtySize {
        column_count: 120,
        row_count: 40,
    };
    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Resize {
            pane_id,
            pty_size: wider_pty_size,
        }),
        SupervisorResult::Done
    );
    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(vec![SupervisorPane {
            pane_id,
            process_id,
            pty_size: wider_pty_size,
        }])
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Kill {
            pane_id,
            kill_policy: KillPolicy::Tree,
        }),
        SupervisorResult::Done
    );
    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new()),
        "a killed pane leaves the supervisor"
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[cfg(unix)]
#[test]
fn a_pane_the_supervisor_does_not_hold_is_refused_by_name() {
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    let missing_pane_id = PaneId::new();

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Resize {
            pane_id: missing_pane_id,
            pty_size: TEST_PTY_SIZE,
        }),
        SupervisorResult::Error(IpcErrorPayload {
            code: IpcErrorCode::Unknown,
            message: format!("invalid pane: id - {missing_pane_id}"),
        })
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[cfg(unix)]
#[test]
fn a_pane_survives_the_link_going_away_and_coming_back_inside_the_idle_window() {
    // This is the swap: the session server goes, and the supervisor must still
    // be holding every pane when the replacement image links to it.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(LONG_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    let pane_id = PaneId::new();
    let SupervisorResult::Spawned { process_id } =
        link.send_request_and_receive_result(SupervisorRequestKind::Spawn {
            pane_id,
            spawn_spec: build_shell_spawn_spec("sleep 30"),
            pty_size: TEST_PTY_SIZE,
        })
    else {
        panic!("the supervisor opens the pane");
    };

    drop(link);
    thread::sleep(TEST_IDLE_EXIT_DURATION * 4);
    assert!(
        !supervisor.is_ended(),
        "a supervisor inside its idle window must wait for the next link"
    );

    let mut replacement_link =
        TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    assert_eq!(
        replacement_link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(vec![SupervisorPane {
            pane_id,
            process_id,
            pty_size: TEST_PTY_SIZE,
        }]),
        "the pane survived the link going away"
    );

    assert_eq!(
        replacement_link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[cfg(unix)]
#[test]
fn a_supervisor_holding_a_pane_closes_it_and_ends_once_the_idle_window_passes() {
    // A session server that dies before it links leaves the supervisor holding
    // panes nothing can reach, so the idle window has to end it too.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    let pane_id = PaneId::new();
    let SupervisorResult::Spawned { process_id } =
        link.send_request_and_receive_result(SupervisorRequestKind::Spawn {
            pane_id,
            spawn_spec: build_shell_spawn_spec("sleep 300"),
            pty_size: TEST_PTY_SIZE,
        })
    else {
        panic!("the supervisor opens the pane");
    };

    drop(link);
    supervisor.join_supervisor_thread();

    wait_until_condition("the pane's child was reaped", || {
        !is_process_alive(process_id)
    });
}

#[cfg(unix)]
#[test]
fn output_produced_while_the_link_is_away_arrives_whole_and_once_on_the_next_link() {
    // The property the whole swap rests on. The pane prints while no link is
    // up, so the chunk is in the sink's hands with nowhere to write it. It
    // must be held, not dropped, and it must be written to the next link
    // exactly once.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(LONG_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    let pane_id = PaneId::new();
    let SupervisorResult::Spawned { .. } =
        link.send_request_and_receive_result(SupervisorRequestKind::Spawn {
            pane_id,
            spawn_spec: build_shell_spawn_spec("sleep 1; printf 'held'; sleep 30"),
            pty_size: TEST_PTY_SIZE,
        })
    else {
        panic!("the supervisor opens the pane");
    };

    // Gone before the pane prints anything at all, so nothing can have been
    // written into the socket this link leaves behind.
    drop(link);
    thread::sleep(Duration::from_secs(2));

    let mut replacement_link =
        TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    assert_eq!(
        replacement_link.receive_next_output_or_fail(pane_id),
        b"held".to_vec(),
        "the chunk produced with no link must reach the next one whole"
    );

    // Written once, not twice. The terminal echoes what is written to it, so
    // the next thing the pane prints is that echo — and a second copy of the
    // held chunk would have arrived before it.
    assert_eq!(
        replacement_link.send_request_and_receive_result(SupervisorRequestKind::Write {
            pane_id,
            input_bytes: b"x\n".to_vec(),
        }),
        SupervisorResult::Done
    );
    let echoed_output_bytes = replacement_link.receive_next_output_or_fail(pane_id);
    assert!(
        echoed_output_bytes.contains(&b'x'),
        "the echo of the written byte must follow the held chunk, and this is {:?}",
        String::from_utf8_lossy(&echoed_output_bytes)
    );
    assert!(
        !echoed_output_bytes
            .windows(4)
            .any(|byte_window| byte_window == b"held"),
        "the held chunk must not be handed over a second time, and this is {:?}",
        String::from_utf8_lossy(&echoed_output_bytes)
    );

    assert_eq!(
        replacement_link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[cfg(unix)]
#[test]
fn a_child_that_ends_while_the_link_is_away_is_reported_on_the_next_link() {
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(LONG_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    let pane_id = PaneId::new();
    let SupervisorResult::Spawned { .. } =
        link.send_request_and_receive_result(SupervisorRequestKind::Spawn {
            pane_id,
            spawn_spec: build_shell_spawn_spec("printf 'ready'; sleep 1; exit 7"),
            pty_size: TEST_PTY_SIZE,
        })
    else {
        panic!("the supervisor opens the pane");
    };
    assert_eq!(link.receive_next_output(pane_id), b"ready".to_vec());
    drop(link);

    // The child ends with nobody linked, so its exit has to wait for the next
    // link rather than being dropped.
    thread::sleep(Duration::from_secs(2));

    let mut replacement_link =
        TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    assert_eq!(
        replacement_link.receive_next_event_or_fail(),
        SupervisorEvent::Exited {
            pane_id,
            exit_status: ExitStatus::ExitCode(7),
        }
    );

    assert_eq!(
        replacement_link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[cfg(unix)]
#[test]
fn a_pane_whose_exit_reached_a_session_server_that_never_closed_it_is_closed_at_the_next_link() {
    // The swap window: the child ends after the session server applied its
    // events for the last time, so the exit is written to a link that process
    // never reads again and no Kill follows it. The image linking next must be
    // handed no pane whose child is gone.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(LONG_IDLE_EXIT_DURATION);
    let mut departing_link =
        TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    let pane_id = PaneId::new();
    let SupervisorResult::Spawned { .. } =
        departing_link.send_request_and_receive_result(SupervisorRequestKind::Spawn {
            pane_id,
            spawn_spec: build_shell_spawn_spec("printf 'ready'; exit 7"),
            pty_size: TEST_PTY_SIZE,
        })
    else {
        panic!("the supervisor opens the pane");
    };

    let exit_event = loop {
        match departing_link.receive_next_event() {
            SupervisorEvent::Exited {
                pane_id: event_pane_id,
                exit_status,
            } => break (event_pane_id, exit_status),
            SupervisorEvent::Output { .. } => {}
        }
    };
    assert_eq!(exit_event, (pane_id, ExitStatus::ExitCode(7)));

    // The session server's process image goes with that exit unapplied.
    drop(departing_link);

    let mut replacement_link =
        TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    assert_eq!(
        replacement_link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new()),
        "a pane whose child ended must not be listed to the next link as a live one"
    );
    assert_eq!(
        replacement_link.send_request_and_receive_result(SupervisorRequestKind::Write {
            pane_id,
            input_bytes: b"x".to_vec(),
        }),
        SupervisorResult::Error(IpcErrorPayload {
            code: IpcErrorCode::Unknown,
            message: format!("invalid pane: id - {pane_id}"),
        }),
        "and its entry left the supervisor, not only the listing"
    );

    assert_eq!(
        replacement_link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[cfg(unix)]
#[test]
fn output_held_for_a_swap_reaches_no_frame_on_this_link_and_arrives_whole_on_the_next() {
    // The swap window. The session server asks for the hold, then spends real
    // time on it: it tells its clients, writes its carried state to disk and
    // starts the new image before its process ends. A pane printing inside that
    // window must be held, because a frame written into the socket that process
    // is about to close reaches nobody, and the supervisor counts it as sent.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(LONG_IDLE_EXIT_DURATION);
    let mut departing_link =
        TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    let pane_id = PaneId::new();
    let SupervisorResult::Spawned { process_id } =
        departing_link.send_request_and_receive_result(SupervisorRequestKind::Spawn {
            pane_id,
            spawn_spec: build_shell_spawn_spec("printf 'ready'; sleep 1; printf 'held'; sleep 30"),
            pty_size: TEST_PTY_SIZE,
        })
    else {
        panic!("the supervisor opens the pane");
    };
    assert_eq!(
        departing_link.receive_next_output(pane_id),
        b"ready".to_vec()
    );

    assert_eq!(
        departing_link.send_request_and_receive_result(SupervisorRequestKind::PauseOutput),
        SupervisorResult::Done
    );

    // The pane prints its second chunk inside this window. The round trip that
    // follows reads every frame the supervisor wrote before its send_response, so
    // anything written for the pane would be held here.
    thread::sleep(Duration::from_secs(2));
    assert_eq!(
        departing_link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(vec![SupervisorPane {
            pane_id,
            process_id,
            pty_size: TEST_PTY_SIZE,
        }])
    );
    assert_eq!(
        departing_link.pending_events,
        Vec::new(),
        "no pane event may reach a link that asked for the output to be held"
    );

    // The session server's process image goes here, taking its socket with it.
    drop(departing_link);

    let mut replacement_link =
        TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    assert_eq!(
        replacement_link.receive_next_output_or_fail(pane_id),
        b"held".to_vec(),
        "what the pane printed under the hold must reach the next link whole"
    );

    assert_eq!(
        replacement_link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[cfg(unix)]
#[test]
fn output_held_for_a_swap_that_was_abandoned_reaches_the_same_link_again() {
    // The swap could not start, so the session server keeps serving on the link
    // it already has and lifts the hold there.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(LONG_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    let pane_id = PaneId::new();
    let SupervisorResult::Spawned { process_id } =
        link.send_request_and_receive_result(SupervisorRequestKind::Spawn {
            pane_id,
            spawn_spec: build_shell_spawn_spec("printf 'ready'; sleep 1; printf 'held'; sleep 30"),
            pty_size: TEST_PTY_SIZE,
        })
    else {
        panic!("the supervisor opens the pane");
    };
    assert_eq!(link.receive_next_output(pane_id), b"ready".to_vec());

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::PauseOutput),
        SupervisorResult::Done
    );

    // The pane prints its second chunk inside this window, and the round trip
    // that follows reads every frame the supervisor wrote before its send_response.
    thread::sleep(Duration::from_secs(2));
    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(vec![SupervisorPane {
            pane_id,
            process_id,
            pty_size: TEST_PTY_SIZE,
        }])
    );
    assert_eq!(
        link.pending_events,
        Vec::new(),
        "no pane event may reach a link that asked for the output to be held"
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ResumeOutput),
        SupervisorResult::Done
    );

    assert_eq!(
        link.receive_next_output_or_fail(pane_id),
        b"held".to_vec(),
        "what the pane printed under the hold must reach the link that lifts it"
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[cfg(unix)]
#[test]
fn a_pane_closed_while_the_output_is_held_does_not_wedge_the_supervisor() {
    // A quit applied while the swap was starting kills the panes with the hold
    // still on. Closing a pane's terminal waits for its reader, and that reader
    // is parked in a held send, so the close has to release it.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(LONG_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    let pane_id = PaneId::new();
    let SupervisorResult::Spawned { process_id } =
        link.send_request_and_receive_result(SupervisorRequestKind::Spawn {
            pane_id,
            spawn_spec: build_shell_spawn_spec("printf 'ready'; sleep 1; printf 'held'; sleep 30"),
            pty_size: TEST_PTY_SIZE,
        })
    else {
        panic!("the supervisor opens the pane");
    };
    assert_eq!(link.receive_next_output(pane_id), b"ready".to_vec());

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::PauseOutput),
        SupervisorResult::Done
    );
    thread::sleep(Duration::from_secs(2));

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Kill {
            pane_id,
            kill_policy: KillPolicy::Tree,
        }),
        SupervisorResult::Done
    );
    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new())
    );

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
    wait_until_condition("the pane's child was reaped", || {
        !is_process_alive(process_id)
    });
}

#[cfg(unix)]
#[test]
fn shutting_down_closes_every_pane_the_supervisor_still_holds() {
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let mut link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    let pane_id = PaneId::new();
    let SupervisorResult::Spawned { process_id } =
        link.send_request_and_receive_result(SupervisorRequestKind::Spawn {
            pane_id,
            spawn_spec: build_shell_spawn_spec("sleep 300"),
            pty_size: TEST_PTY_SIZE,
        })
    else {
        panic!("the supervisor opens the pane");
    };

    assert_eq!(
        link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();

    wait_until_condition("the pane's child was reaped", || {
        !is_process_alive(process_id)
    });
}

/// True while process `pid` is still around (`kill -0` succeeds).
#[cfg(unix)]
fn is_process_alive(process_id: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &process_id.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|process_exit_status| process_exit_status.success())
        .unwrap_or(false)
}

/// A sink that keeps everything it is handed, so a test can check exactly what
/// reached the consumer.
struct RecordingSink {
    /// Every chunk taken, oldest first, with the pane that printed it.
    output_chunks: Mutex<Vec<(PaneId, Vec<u8>)>>,
    /// Every exit taken, oldest first, with the pane that ended.
    exit_statuses: Mutex<Vec<(PaneId, ExitStatus)>>,
}

impl RecordingSink {
    fn new() -> Arc<Self> {
        Arc::new(RecordingSink {
            output_chunks: Mutex::new(Vec::new()),
            exit_statuses: Mutex::new(Vec::new()),
        })
    }

    /// Every byte this sink has been handed for `pane_id`, in order.
    fn list_bytes_for_pane(&self, pane_id: PaneId) -> Vec<u8> {
        self.output_chunks
            .lock()
            .expect("recording sink")
            .iter()
            .filter(|(recorded_pane_id, _)| *recorded_pane_id == pane_id)
            .flat_map(|(_, output_bytes)| output_bytes.iter().copied())
            .collect()
    }

    /// Every exit this sink has been handed, in order.
    fn list_exit_statuses(&self) -> Vec<(PaneId, ExitStatus)> {
        self.exit_statuses.lock().expect("recording sink").clone()
    }
}

impl PtySink for RecordingSink {
    fn accept_output_bytes(&self, pane_id: PaneId, output_bytes: Vec<u8>) -> bool {
        self.output_chunks
            .lock()
            .expect("recording sink")
            .push((pane_id, output_bytes));
        true
    }

    fn accept_exit_status(&self, pane_id: PaneId, exit_status: ExitStatus) {
        self.exit_statuses
            .lock()
            .expect("recording sink")
            .push((pane_id, exit_status));
    }
}

#[test]
fn a_panes_child_prints_through_the_link_once_its_terminal_is_answered() {
    // A real pane inside the supervisor, driven by the backend a session server
    // drives, with only the supervisor running. Nothing outside the pane
    // answers its terminal's cursor-position query, and a Windows pseudoconsole
    // hands over nothing its child printed until that send_response lands.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(LONG_IDLE_EXIT_DURATION);
    let recording_sink = RecordingSink::new();
    let pty_backend = SupervisorPtyBackend::connect(
        &supervisor.supervisor_address,
        build_test_connection_token(),
        Arc::clone(&recording_sink) as Arc<dyn PtySink>,
        &[],
    )
    .expect("the backend opens the link");
    let pane_id = PaneId::new();

    pty_backend
        .spawn_pane(
            pane_id,
            build_printing_spawn_spec(PRINTED_OUTPUT_MARKER),
            TEST_PTY_SIZE,
        )
        .expect("the supervisor opens the pane");
    let recorded_output_bytes = collect_sink_bytes_until(
        &recording_sink,
        pane_id,
        PRINTED_OUTPUT_MARKER.as_bytes(),
        PRINT_WAIT_DURATION,
    );

    assert_eq!(
        count_cursor_position_queries(&recorded_output_bytes),
        0,
        "the pane's reader takes the terminal's query out, so none crosses the link"
    );
    // The child kept running after it printed, so what arrived is its output
    // and not the flush of a child that ended.
    assert_eq!(
        recording_sink.list_exit_statuses(),
        Vec::<(PaneId, ExitStatus)>::new(),
        "the pane that printed is still running"
    );

    pty_backend
        .shutdown_supervisor()
        .expect("the supervisor is told to end");
    supervisor.join_supervisor_thread();
}

#[cfg(unix)]
#[test]
fn a_pane_driven_through_the_backend_prints_back_into_its_sink() {
    // Both halves of the link, end to end: the session server's backend opens
    // a pane in the supervisor and the child's bytes arrive in the sink, which
    // is what the runtime inbox reads in the running binary.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION);
    let recording_sink = RecordingSink::new();
    let pty_backend = SupervisorPtyBackend::connect(
        &supervisor.supervisor_address,
        build_test_connection_token(),
        Arc::clone(&recording_sink) as Arc<dyn PtySink>,
        &[],
    )
    .expect("the backend opens the link");
    let pane_id = PaneId::new();

    pty_backend
        .spawn_pane(
            pane_id,
            build_shell_spawn_spec("printf 'ready'; cat"),
            TEST_PTY_SIZE,
        )
        .expect("the supervisor opens the pane");
    collect_sink_bytes_until(&recording_sink, pane_id, b"ready", HANG_GUARD_DURATION);
    pty_backend
        .write_pane_input(pane_id, b"echoed\n")
        .expect("the bytes reach the child");
    let observed_output_bytes =
        collect_sink_bytes_until(&recording_sink, pane_id, b"echoed", HANG_GUARD_DURATION);

    assert_eq!(
        &observed_output_bytes[..b"ready".len()],
        b"ready",
        "the child's first bytes reach the sink before anything written to it"
    );

    pty_backend
        .shutdown_supervisor()
        .expect("the supervisor is told to end");
    supervisor.join_supervisor_thread();
}

#[cfg(unix)]
#[test]
fn holding_the_readers_still_stops_a_live_pane_reaching_the_consumer() {
    // The Windows half of the swap, end to end through the backend the session
    // server drives: every reader is inside the supervisor, so the pause has to
    // stop the supervisor writing, and what the pane prints under the hold has
    // to reach the consumer once the readers go back to work.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(LONG_IDLE_EXIT_DURATION);
    let recording_sink = RecordingSink::new();
    let pty_backend = SupervisorPtyBackend::connect(
        &supervisor.supervisor_address,
        build_test_connection_token(),
        Arc::clone(&recording_sink) as Arc<dyn PtySink>,
        &[],
    )
    .expect("the backend opens the link");
    let pane_id = PaneId::new();
    pty_backend
        .spawn_pane(
            pane_id,
            build_shell_spawn_spec("printf 'ready'; sleep 1; printf 'held'; cat"),
            TEST_PTY_SIZE,
        )
        .expect("the supervisor opens the pane");
    collect_sink_bytes_until(&recording_sink, pane_id, b"ready", HANG_GUARD_DURATION);

    pty_backend.pause_readers().expect("the pause answers");
    let carried_panes = pty_backend.list_carried_panes();

    // The pane prints its second chunk inside this window.
    thread::sleep(Duration::from_secs(2));
    assert_eq!(
        recording_sink.list_bytes_for_pane(pane_id),
        b"ready".to_vec(),
        "nothing may reach the consumer while its readers are held"
    );

    assert_eq!(
        carried_panes.len(),
        1,
        "the paused backend names its one pane"
    );
    assert_eq!(carried_panes[0].pane_id, pane_id);
    assert_eq!(carried_panes[0].pty_size, TEST_PTY_SIZE);

    pty_backend.resume_readers();
    let observed_output_bytes =
        collect_sink_bytes_until(&recording_sink, pane_id, b"held", HANG_GUARD_DURATION);
    assert_eq!(
        observed_output_bytes,
        b"readyheld".to_vec(),
        "the held chunk reaches the consumer whole, once, and after the first"
    );

    pty_backend
        .write_pane_input(pane_id, b"after\n")
        .expect("the bytes reach the child");
    collect_sink_bytes_until(&recording_sink, pane_id, b"after", HANG_GUARD_DURATION);

    pty_backend
        .shutdown_supervisor()
        .expect("the supervisor is told to end");
    supervisor.join_supervisor_thread();
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
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(LONG_IDLE_EXIT_DURATION);
    let mut opening_link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    let kept_pane_id = PaneId::new();
    let uncarried_pane_id = PaneId::new();
    let SupervisorResult::Spawned { .. } =
        opening_link.send_request_and_receive_result(SupervisorRequestKind::Spawn {
            pane_id: kept_pane_id,
            spawn_spec: build_shell_spawn_spec("sleep 300"),
            pty_size: TEST_PTY_SIZE,
        })
    else {
        panic!("the supervisor opens the kept pane");
    };
    let SupervisorResult::Spawned {
        process_id: uncarried_process_id,
    } = opening_link.send_request_and_receive_result(SupervisorRequestKind::Spawn {
        pane_id: uncarried_pane_id,
        spawn_spec: build_shell_spawn_spec("sleep 300"),
        pty_size: TEST_PTY_SIZE,
    })
    else {
        panic!("the supervisor opens the pane nobody will carry");
    };
    drop(opening_link);

    // The replacement image carries the pane that survived and one that never
    // existed, and does not carry the one the supervisor still holds.
    let missing_pane_id = PaneId::new();
    let resumed_sink = RecordingSink::new();
    let pty_backend = SupervisorPtyBackend::connect(
        &supervisor.supervisor_address,
        build_test_connection_token(),
        Arc::clone(&resumed_sink) as Arc<dyn PtySink>,
        &[kept_pane_id, missing_pane_id],
    )
    .expect("the replacement image opens the link");

    assert_eq!(
        resumed_sink.list_exit_statuses(),
        vec![(missing_pane_id, ExitStatus::ExitCode(-1))],
        "a carried pane the supervisor does not hold is reported as ended"
    );
    let carried_panes = pty_backend.list_carried_panes();
    assert_eq!(carried_panes.len(), 1, "only the surviving pane is driven");
    assert_eq!(carried_panes[0].pane_id, kept_pane_id);
    wait_until_condition("the pane nobody carried was killed", || {
        !is_process_alive(uncarried_process_id)
    });

    pty_backend
        .shutdown_supervisor()
        .expect("the supervisor is told to end");
    supervisor.join_supervisor_thread();
}

#[test]
fn the_hidden_subcommand_is_the_one_the_supervisor_is_started_under() {
    use clap::Parser;

    let session_id = SessionId::new();
    let cli = koshi::cli::Cli::try_parse_from([
        "koshi",
        PTY_SUPERVISOR_SUBCOMMAND,
        &session_id.to_string(),
        "k7QxSecret",
        koshi_link::router_client::RUNTIME_DIRECTORY_FLAG,
        "/run/user/1000/koshi",
    ])
    .expect("the hidden subcommand parses");

    assert_eq!(
        cli.command,
        Some(koshi::cli::CliCommand::ServePtySupervisor {
            session_id,
            supervisor_token: "k7QxSecret".to_string(),
            runtime_directory: Some(std::path::PathBuf::from("/run/user/1000/koshi")),
        })
    );
}

#[test]
fn a_pane_event_waits_until_a_hello_opens_the_link() {
    // The token is what makes a link this session's own. A peer that opens the
    // socket and never presents it must be handed no pane output, so the sink
    // holds its chunk until the Hello is accepted and hands it over then.
    let (_runtime_directory, supervisor_address, supervisor_listener) = bind_test_listener();
    let connection_thread = {
        let supervisor_address = supervisor_address.clone();
        thread::spawn(move || Connection::connect(&supervisor_address).expect("the peer connects"))
    };
    let accepted_connection = supervisor_listener.accept().expect("the peer is accepted");
    let mut peer_connection = connection_thread
        .join()
        .expect("the connecting thread ended");
    let (_frame_reader, frame_writer) = accepted_connection.split();

    let link_sink = LinkSink::new();
    link_sink.set_link_writer(frame_writer);
    let pane_id = PaneId::new();
    let output_send_thread = {
        let link_sink = Arc::clone(&link_sink);
        thread::spawn(move || link_sink.accept_output_bytes(pane_id, b"held".to_vec()))
    };

    thread::sleep(TEST_IDLE_EXIT_DURATION);
    assert!(
        !output_send_thread.is_finished(),
        "a link with no accepted Hello must be handed nothing"
    );

    link_sink.mark_link_open();
    wait_until_condition("the held chunk went out", || {
        output_send_thread.is_finished()
    });
    assert!(
        output_send_thread
            .join()
            .expect("the output send thread ended"),
        "the chunk goes out once a Hello opened the link"
    );
    assert_eq!(
        peer_connection
            .recv::<SupervisorMessage>()
            .expect("a frame arrives"),
        SupervisorMessage::Event(SupervisorEvent::Output {
            pane_id,
            output_bytes: b"held".to_vec(),
        })
    );
}

/// One connected pair on the supervisor's link address: the accepted end's
/// writing half, and the peer's own connection.
fn connect_test_link_pair(
    supervisor_address: &str,
    supervisor_listener: &Listener,
) -> (FrameWriter, Connection) {
    let connection_thread = {
        let supervisor_address = supervisor_address.to_string();
        thread::spawn(move || Connection::connect(&supervisor_address).expect("the peer connects"))
    };
    let accepted_connection = supervisor_listener.accept().expect("the peer is accepted");
    let peer_connection = connection_thread
        .join()
        .expect("the connecting thread ended");
    let (_frame_reader, frame_writer) = accepted_connection.split();
    (frame_writer, peer_connection)
}

#[test]
fn a_second_link_carries_no_pane_output_until_its_own_hello_opens_it() {
    // Being open belongs to the link that was opened. A session server that
    // replaced its own image takes the link, and the peer that arrives next
    // has presented no token yet: it must be handed nothing until it does, the
    // same as the first one.
    let (_runtime_directory, supervisor_address, supervisor_listener) = bind_test_listener();
    let link_sink = LinkSink::new();
    let pane_id = PaneId::new();

    let (first_frame_writer, mut first_peer_connection) =
        connect_test_link_pair(&supervisor_address, &supervisor_listener);
    link_sink.set_link_writer(first_frame_writer);
    link_sink.mark_link_open();
    assert!(
        link_sink.accept_output_bytes(pane_id, b"before the swap".to_vec()),
        "the opened link takes the chunk"
    );
    assert_eq!(
        first_peer_connection
            .recv::<SupervisorMessage>()
            .expect("a frame arrives"),
        SupervisorMessage::Event(SupervisorEvent::Output {
            pane_id,
            output_bytes: b"before the swap".to_vec(),
        })
    );

    // The session server's process image goes, and the next peer takes the
    // link without presenting the token.
    link_sink.clear_link();
    drop(first_peer_connection);
    let (second_frame_writer, mut second_peer_connection) =
        connect_test_link_pair(&supervisor_address, &supervisor_listener);
    link_sink.set_link_writer(second_frame_writer);

    let output_send_thread = {
        let link_sink = Arc::clone(&link_sink);
        thread::spawn(move || link_sink.accept_output_bytes(pane_id, b"after the swap".to_vec()))
    };
    thread::sleep(TEST_IDLE_EXIT_DURATION);
    assert!(
        !output_send_thread.is_finished(),
        "a link that has presented no token must be handed nothing, however the link before it ended"
    );

    link_sink.mark_link_open();
    wait_until_condition("the held chunk went out", || {
        output_send_thread.is_finished()
    });
    assert!(
        output_send_thread
            .join()
            .expect("the output send thread ended"),
        "the chunk goes out once this link's own Hello opened it"
    );
    assert_eq!(
        second_peer_connection
            .recv::<SupervisorMessage>()
            .expect("a frame arrives"),
        SupervisorMessage::Event(SupervisorEvent::Output {
            pane_id,
            output_bytes: b"after the swap".to_vec(),
        }),
        "and it is the chunk this link was handed, not the one the link before it took"
    );
}

#[test]
fn a_send_parked_for_a_pane_being_closed_gives_up_and_leaves_the_others_parked() {
    // Closing a pane's terminal waits for that pane's reader to carry the
    // terminal to its end, and that reader can be parked inside a send. Letting
    // the pane go has to release exactly that send, or the close never
    // finishes; every other pane's send must stay where it is.
    let link_sink = LinkSink::new();
    let closing_pane_id = PaneId::new();
    let other_pane_id = PaneId::new();

    let closing_output_thread = {
        let link_sink = Arc::clone(&link_sink);
        thread::spawn(move || {
            link_sink.accept_output_bytes(closing_pane_id, b"last words".to_vec())
        })
    };
    let other_output_thread = {
        let link_sink = Arc::clone(&link_sink);
        thread::spawn(move || link_sink.accept_output_bytes(other_pane_id, b"still going".to_vec()))
    };
    thread::sleep(TEST_IDLE_EXIT_DURATION);
    assert!(
        !closing_output_thread.is_finished() && !other_output_thread.is_finished(),
        "with no link up, both sends wait"
    );

    link_sink.mark_pane_closing(closing_pane_id);

    wait_until_condition("the send for the closing pane gave up", || {
        closing_output_thread.is_finished()
    });
    assert!(
        !closing_output_thread
            .join()
            .expect("the closing output thread ended"),
        "a send for a pane being closed reports that nobody wants its chunk"
    );
    assert!(
        !other_output_thread.is_finished(),
        "and a send for another pane keeps waiting for a link"
    );

    link_sink.mark_closing();

    wait_until_condition("every remaining send gave up", || {
        other_output_thread.is_finished()
    });
    assert!(
        !other_output_thread
            .join()
            .expect("the other output thread ended"),
        "the supervisor ending reports the same to every send left"
    );
}

#[test]
fn a_send_for_a_closed_pane_gives_up_without_waiting_before_and_after_it_is_forgotten() {
    // A pane is let go, closed, then forgotten. Every send for it answers at
    // once rather than parking, both while it is being closed and afterwards:
    // a reader that wakes late must not write a chunk to a link after the Kill
    // that closed the pane was answered.
    let link_sink = LinkSink::new();
    let pane_id = PaneId::new();

    link_sink.mark_pane_closing(pane_id);
    assert!(
        !link_sink.accept_output_bytes(pane_id, b"dropped".to_vec()),
        "a chunk for a pane being closed is refused on the spot"
    );

    link_sink.remove_ended_pane(pane_id);
    assert!(
        !link_sink.accept_output_bytes(pane_id, b"late".to_vec()),
        "a chunk for a pane that is already closed is refused too"
    );

    // A pane the supervisor never closed still parks for a link.
    let other_pane_id = PaneId::new();
    let parked_output_thread = {
        let link_sink = Arc::clone(&link_sink);
        thread::spawn(move || link_sink.accept_output_bytes(other_pane_id, b"parked".to_vec()))
    };
    thread::sleep(TEST_IDLE_EXIT_DURATION);
    assert!(
        !parked_output_thread.is_finished(),
        "a live pane waits for a link"
    );

    link_sink.mark_closing();
    wait_until_condition("the parked send gave up", || {
        parked_output_thread.is_finished()
    });
    assert!(!parked_output_thread
        .join()
        .expect("the output thread ended"));
}

#[test]
fn every_send_after_the_supervisor_starts_ending_gives_up_at_once() {
    // The supervisor closes every pane it holds when it ends, and each close
    // waits on that pane's reader. Ending the sends first is what lets those
    // closes finish, including for a pane whose send arrives afterwards.
    let link_sink = LinkSink::new();
    let pane_id = PaneId::new();

    link_sink.mark_closing();

    assert!(
        !link_sink.accept_output_bytes(pane_id, b"too late".to_vec()),
        "a chunk arriving after the supervisor started ending is refused at once"
    );
    // An exit takes the same route and must not wait either; a wait here would
    // hold the supervisor's own teardown.
    link_sink.accept_exit_status(pane_id, ExitStatus::ExitCode(0));
    assert_eq!(
        link_sink.list_ended_pane_ids(),
        Vec::<PaneId>::new(),
        "an exit no link was handed leaves no pane whose entry is still to close"
    );
    assert!(
        !link_sink.accept_output_bytes(PaneId::new(), b"another pane".to_vec()),
        "and so is every other pane's"
    );
}

#[test]
fn a_shutdown_before_a_hello_is_refused_and_the_supervisor_keeps_running() {
    // Ending the process is the one request that cannot be allowed through the
    // gate: a peer that has not presented the token must not be able to close
    // the session's panes.
    let mut supervisor =
        RunningSupervisor::start_with_idle_exit_duration(TEST_IDLE_EXIT_DURATION * 10);
    let mut unauthenticated_link = TestLink::connect_without_hello(&supervisor.supervisor_address);

    assert_eq!(
        unauthenticated_link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Shutdown arrived before a Hello opened the link".to_string(),
        })
    );
    assert!(
        !supervisor.is_ended(),
        "a refused Shutdown must leave the supervisor running"
    );

    assert_eq!(
        unauthenticated_link.send_request_and_receive_result(
            SupervisorRequestKind::build_hello_request(build_test_connection_token()),
        ),
        SupervisorResult::Hello {
            protocol_version: SUPERVISOR_PROTOCOL_VERSION,
        }
    );
    assert_eq!(
        unauthenticated_link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[test]
fn a_second_link_is_answered_only_once_the_first_one_ends() {
    // The supervisor serves one linked session server at a time. Two swaps
    // reaching it at once must not have the second one driving the panes while
    // the first still holds the link: the second waits, and it is served the
    // moment the first goes.
    let mut supervisor = RunningSupervisor::start_with_idle_exit_duration(LONG_IDLE_EXIT_DURATION);
    let mut first_link = TestLink::connect_with_connection_token(&supervisor.supervisor_address);
    assert_eq!(
        first_link.send_request_and_receive_result(SupervisorRequestKind::ListPanes),
        SupervisorResult::Panes(Vec::new())
    );

    let mut second_connection =
        Connection::connect(&supervisor.supervisor_address).expect("the second link opens");
    second_connection
        .send(&SupervisorRequest {
            request_id: 1,
            request_kind: SupervisorRequestKind::build_hello_request(build_test_connection_token()),
        })
        .expect("the second Hello is sent");
    let second_link_thread = thread::spawn(move || {
        let received_response: SupervisorMessage =
            second_connection.recv().expect("a frame arrives");
        (received_response, second_connection)
    });
    thread::sleep(TEST_IDLE_EXIT_DURATION);
    assert!(
        !second_link_thread.is_finished(),
        "a second link must wait while the first one is being served"
    );

    // The first session server's process image goes.
    drop(first_link);

    wait_until_condition("the second link was served", || {
        second_link_thread.is_finished()
    });
    let (received_response, second_connection) =
        second_link_thread.join().expect("the waiting thread ended");
    assert_eq!(
        received_response,
        SupervisorMessage::Response(SupervisorResponse {
            request_id: Some(1),
            answer_result: SupervisorResult::Hello {
                protocol_version: SUPERVISOR_PROTOCOL_VERSION,
            },
        })
    );

    let mut second_test_link = TestLink {
        connection: second_connection,
        next_request_id: 2,
        pending_events: Vec::new(),
    };
    assert_eq!(
        second_test_link.send_request_and_receive_result(SupervisorRequestKind::Shutdown),
        SupervisorResult::Done
    );
    supervisor.join_supervisor_thread();
}

#[test]
fn an_answer_goes_out_on_a_link_that_no_hello_has_opened_and_that_is_holding_its_output() {
    // A response belongs to the link its request arrived on. Neither gate that
    // parks a pane event may park a response: a Hello is answered on a link no
    // Hello has opened yet, and the response to a hold is written under it.
    let (_runtime_directory, supervisor_address, supervisor_listener) = bind_test_listener();
    let link_sink = LinkSink::new();
    let (frame_writer, mut peer_connection) =
        connect_test_link_pair(&supervisor_address, &supervisor_listener);
    link_sink.set_link_writer(frame_writer);
    link_sink.pause_event_output();

    let supervisor_response = SupervisorResponse {
        request_id: Some(7),
        answer_result: SupervisorResult::Panes(Vec::new()),
    };
    assert!(
        link_sink.send_response(supervisor_response.clone()),
        "a link that is up takes the response"
    );
    assert_eq!(
        peer_connection
            .recv::<SupervisorMessage>()
            .expect("a frame arrives"),
        SupervisorMessage::Response(supervisor_response)
    );
}

#[test]
fn an_answer_with_no_link_up_reports_the_link_is_gone() {
    let link_sink = LinkSink::new();

    assert!(
        !link_sink.send_response(SupervisorResponse {
            request_id: Some(1),
            answer_result: SupervisorResult::Done,
        }),
        "a response with nowhere to go reports the link ended"
    );
}

#[test]
fn a_hold_asked_for_on_one_link_does_not_carry_to_the_next_one() {
    // A hold belongs to the link that asked for it. The session server that
    // asked is gone, so the image linking next must be written the pane's
    // output without lifting a hold it never asked for.
    let (_runtime_directory, supervisor_address, supervisor_listener) = bind_test_listener();
    let link_sink = LinkSink::new();
    let pane_id = PaneId::new();

    let (first_frame_writer, first_peer_connection) =
        connect_test_link_pair(&supervisor_address, &supervisor_listener);
    link_sink.set_link_writer(first_frame_writer);
    link_sink.mark_link_open();
    link_sink.pause_event_output();
    link_sink.clear_link();
    drop(first_peer_connection);

    let (second_frame_writer, mut second_peer_connection) =
        connect_test_link_pair(&supervisor_address, &supervisor_listener);
    link_sink.set_link_writer(second_frame_writer);
    link_sink.mark_link_open();
    let output_send_thread = {
        let link_sink = Arc::clone(&link_sink);
        thread::spawn(move || link_sink.accept_output_bytes(pane_id, b"after the swap".to_vec()))
    };

    wait_until_condition("the chunk went out on the next link", || {
        output_send_thread.is_finished()
    });
    assert!(
        output_send_thread
            .join()
            .expect("the output send thread ended"),
        "the next link takes the chunk"
    );
    assert_eq!(
        second_peer_connection
            .recv::<SupervisorMessage>()
            .expect("a frame arrives"),
        SupervisorMessage::Event(SupervisorEvent::Output {
            pane_id,
            output_bytes: b"after the swap".to_vec(),
        })
    );
}

#[test]
fn two_holds_on_one_link_are_lifted_by_one_resume() {
    let (_runtime_directory, supervisor_address, supervisor_listener) = bind_test_listener();
    let link_sink = LinkSink::new();
    let pane_id = PaneId::new();
    let (frame_writer, mut peer_connection) =
        connect_test_link_pair(&supervisor_address, &supervisor_listener);
    link_sink.set_link_writer(frame_writer);
    link_sink.mark_link_open();

    link_sink.pause_event_output();
    link_sink.pause_event_output();
    let output_send_thread = {
        let link_sink = Arc::clone(&link_sink);
        thread::spawn(move || link_sink.accept_output_bytes(pane_id, b"held".to_vec()))
    };
    thread::sleep(TEST_IDLE_EXIT_DURATION);
    assert!(
        !output_send_thread.is_finished(),
        "a link holding its output is handed nothing"
    );

    link_sink.resume_event_output();

    wait_until_condition("the held chunk went out", || {
        output_send_thread.is_finished()
    });
    assert!(
        output_send_thread
            .join()
            .expect("the output send thread ended"),
        "one resume lifts the hold, however many asked for it"
    );
    assert_eq!(
        peer_connection
            .recv::<SupervisorMessage>()
            .expect("a frame arrives"),
        SupervisorMessage::Event(SupervisorEvent::Output {
            pane_id,
            output_bytes: b"held".to_vec(),
        })
    );
}

#[test]
fn an_exit_written_to_a_link_leaves_the_pane_to_close_until_it_is_forgotten() {
    // `close_panes_that_ended` closes exactly the panes this list names, so an
    // exit that reached a link puts its pane on the list and closing the pane
    // takes it off again.
    let (_runtime_directory, supervisor_address, supervisor_listener) = bind_test_listener();
    let link_sink = LinkSink::new();
    let pane_id = PaneId::new();
    let (frame_writer, mut peer_connection) =
        connect_test_link_pair(&supervisor_address, &supervisor_listener);
    link_sink.set_link_writer(frame_writer);
    link_sink.mark_link_open();

    link_sink.accept_exit_status(pane_id, ExitStatus::ExitCode(3));

    assert_eq!(
        peer_connection
            .recv::<SupervisorMessage>()
            .expect("a frame arrives"),
        SupervisorMessage::Event(SupervisorEvent::Exited {
            pane_id,
            exit_status: ExitStatus::ExitCode(3),
        })
    );
    assert_eq!(
        link_sink.list_ended_pane_ids(),
        vec![pane_id],
        "the pane whose exit went out still has an entry to close"
    );

    link_sink.remove_ended_pane(pane_id);

    assert_eq!(
        link_sink.list_ended_pane_ids(),
        Vec::<PaneId>::new(),
        "a pane that has been closed has no entry left to close"
    );
}

#[test]
fn an_exit_for_a_pane_being_closed_leaves_no_pane_to_close_again() {
    // The pane is being closed already, so its exit reaches no link and the
    // close under way is the only one.
    let link_sink = LinkSink::new();
    let pane_id = PaneId::new();

    link_sink.mark_pane_closing(pane_id);
    link_sink.accept_exit_status(pane_id, ExitStatus::ExitCode(0));

    assert_eq!(
        link_sink.list_ended_pane_ids(),
        Vec::<PaneId>::new(),
        "an exit nobody was handed names no pane whose entry is still to close"
    );
}
