//! Tests for [`SupervisorPtyBackend`]: every backend call becomes the request
//! it should, an answer that does not fit its request is refused, a pane this
//! backend does not drive is refused before the link is touched, events reach
//! the sink in the order they arrive, holding the readers still asks the
//! supervisor to hold its pane output and fails when it cannot, and
//! [`SupervisorPtyBackend::connect`] reconciles both ways a pane list can disagree
//! with what the supervisor holds.
//!
//! The peer here is a hand-written supervisor over a real socket: it answers
//! whatever the test queued and records what it was asked. The backend is
//! tested against the wire, not against a stub of itself.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use koshi_core::process::{ExitStatus, ShellKind};
use koshi_ipc::supervisor::{SupervisorPane, SupervisorRequestKind};
use koshi_ipc::transport::Listener;

use super::*;

/// How long a test waits for something it expects promptly. A wait that
/// reaches it fails the test.
const HANG_GUARD_DURATION: Duration = Duration::from_secs(5);

/// The size every pane in these tests opens at.
const STANDARD_PTY_SIZE: PtySize = PtySize {
    column_count: 80,
    row_count: 24,
};

/// A sink that keeps everything it is handed and accepts every chunk.
struct RecordingSink {
    /// Every output chunk taken, oldest first, with the pane that printed it.
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

    /// Every chunk this sink has been handed, in order.
    fn list_output_chunks(&self) -> Vec<(PaneId, Vec<u8>)> {
        self.output_chunks.lock().expect("recording sink").clone()
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

/// One frame the fake supervisor writes before it reads its first request.
enum SupervisorTestFrame {
    /// A message whose every name this build has.
    Message(SupervisorMessage),
    /// An answer whose result is a variant name this build does not have.
    UnknownResponseVariant {
        request_id: Option<u64>,
        unknown_variant_name: String,
    },
    /// An event whose variant name this build does not have.
    UnknownEventVariant(String),
}

/// A hand-written supervisor on the other end of one link.
///
/// It answers each request from `supervisor_results`, in order, and records the requests
/// it was asked. The planted frames are sent before the first request is read.
struct FakeSupervisor {
    /// The address the backend connects to.
    supervisor_address: String,
    /// The requests this supervisor was asked, oldest first, each with the id
    /// it carried.
    recorded_requests: Arc<Mutex<Vec<(u64, SupervisorRequestKind)>>>,
    /// The socket file's directory. Outlives the link on Unix.
    _runtime_directory: tempfile::TempDir,
}

impl FakeSupervisor {
    /// Start a supervisor that answers `supervisor_results` in order and sends
    /// `supervisor_events`
    /// before reading anything.
    ///
    /// The first two responses a [`SupervisorPtyBackend::connect`] needs, the
    /// Hello and the pane list, are the caller's to supply.
    fn start_with_supervisor_results(
        supervisor_results: Vec<SupervisorResult>,
        supervisor_events: Vec<SupervisorEvent>,
    ) -> FakeSupervisor {
        let supervisor_frames = supervisor_events
            .into_iter()
            .map(|supervisor_event| {
                SupervisorTestFrame::Message(SupervisorMessage::Event(supervisor_event))
            })
            .collect();
        FakeSupervisor::start_with_supervisor_frames(supervisor_results, supervisor_frames)
    }

    /// Start a supervisor that answers `supervisor_results` in order and writes
    /// `supervisor_frames`
    /// before reading anything.
    fn start_with_supervisor_frames(
        supervisor_results: Vec<SupervisorResult>,
        supervisor_frames: Vec<SupervisorTestFrame>,
    ) -> FakeSupervisor {
        let runtime_directory = tempfile::tempdir().expect("an isolated test directory is created");
        let supervisor_address = build_test_supervisor_address(runtime_directory.path());
        let supervisor_listener =
            Listener::bind(&supervisor_address).expect("the fake supervisor binds its link");
        let recorded_requests = Arc::new(Mutex::new(Vec::new()));

        let recorded_requests_for_thread = Arc::clone(&recorded_requests);
        thread::Builder::new()
            .name("fake-supervisor".to_string())
            .spawn(move || {
                let supervisor_connection =
                    supervisor_listener.accept().expect("the backend connects");
                let (mut frame_reader, mut frame_writer) = supervisor_connection.split();
                for supervisor_frame in supervisor_frames {
                    let frame_send_result = match supervisor_frame {
                        SupervisorTestFrame::Message(supervisor_message) => {
                            frame_writer.send(&supervisor_message)
                        }
                        SupervisorTestFrame::UnknownResponseVariant {
                            request_id,
                            unknown_variant_name,
                        } => frame_writer.send(
                            &SupervisorMessage::<String, SupervisorEvent>::Response(
                                SupervisorResponse {
                                    request_id,
                                    answer_result: unknown_variant_name,
                                },
                            ),
                        ),
                        SupervisorTestFrame::UnknownEventVariant(unknown_variant_name) => {
                            frame_writer.send(
                                &SupervisorMessage::<SupervisorResult, String>::Event(
                                    unknown_variant_name,
                                ),
                            )
                        }
                    };
                    frame_send_result.expect("the fake supervisor sends its planted frame");
                }
                let mut supervisor_results = supervisor_results.into_iter();
                while let Ok(supervisor_request) = frame_reader.recv::<SupervisorRequest>() {
                    recorded_requests_for_thread
                        .lock()
                        .expect("recorded requests")
                        .push((
                            supervisor_request.request_id,
                            supervisor_request.request_kind,
                        ));
                    let Some(supervisor_result) = supervisor_results.next() else {
                        return;
                    };
                    if frame_writer
                        .send(&SupervisorMessage::<_, SupervisorEvent>::Response(
                            SupervisorResponse {
                                request_id: Some(supervisor_request.request_id),
                                answer_result: supervisor_result,
                            },
                        ))
                        .is_err()
                    {
                        return;
                    }
                }
            })
            .expect("the fake supervisor thread starts");

        FakeSupervisor {
            supervisor_address,
            recorded_requests,
            _runtime_directory: runtime_directory,
        }
    }

    /// The request kinds this supervisor was asked, oldest first.
    fn list_requested_kinds(&self) -> Vec<SupervisorRequestKind> {
        self.recorded_requests
            .lock()
            .expect("recorded requests")
            .iter()
            .map(|(_, request_kind)| request_kind.clone())
            .collect()
    }

    /// The request ids this supervisor was asked with, oldest first.
    fn list_request_ids(&self) -> Vec<u64> {
        self.recorded_requests
            .lock()
            .expect("recorded requests")
            .iter()
            .map(|(request_id, _)| *request_id)
            .collect()
    }
}

/// An address for one test's link. On Unix it is a socket file inside
/// `runtime_directory`; on Windows it is the pipe name
/// `koshi-pty-test-<process id>-<clock nanoseconds>-<index>`, whose index
/// differs for every call in one test process.
fn build_test_supervisor_address(runtime_directory: &std::path::Path) -> String {
    #[cfg(unix)]
    {
        runtime_directory
            .join("supervisor.sock")
            .display()
            .to_string()
    }
    #[cfg(windows)]
    {
        static NEXT_TEST_PIPE_INDEX: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        let _ = runtime_directory;
        format!(
            "koshi-pty-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the clock is past the epoch")
                .as_nanos(),
            NEXT_TEST_PIPE_INDEX.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
    }
}

/// The responses every `connect` needs before the test's own responses: an
/// accepted Hello, then the pane list `supervisor_panes`.
fn build_opening_supervisor_results(
    supervisor_panes: Vec<SupervisorPane>,
) -> Vec<SupervisorResult> {
    vec![
        SupervisorResult::Hello {
            protocol_version: 1,
        },
        SupervisorResult::Panes(supervisor_panes),
    ]
}

/// A spawn spec launching `shell_script`. Nothing here ever runs it: the fake
/// supervisor answers the request instead of spawning anything.
fn build_shell_spawn_spec(shell_script: &str) -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from("/bin/sh"),
        arguments: vec!["-c".to_string(), shell_script.to_string()],
        working_directory: None,
        environment_variables: BTreeMap::new(),
        shell_kind: ShellKind::Other("sh".to_string()),
    }
}

/// Connect a backend to `fake_supervisor`, expecting no pane to be carried in.
fn connect_test_backend(
    fake_supervisor: &FakeSupervisor,
    recording_sink: Arc<RecordingSink>,
) -> SupervisorPtyBackend {
    SupervisorPtyBackend::connect(
        &fake_supervisor.supervisor_address,
        ConnectionToken::from_secret("k7QxSecret"),
        recording_sink,
        &[],
    )
    .expect("the backend opens the link")
}

/// Wait until `is_condition_met` answers `true`. Fails the test after
/// [`HANG_GUARD_DURATION`].
fn wait_until_condition(condition_description: &str, is_condition_met: impl Fn() -> bool) {
    let deadline = Instant::now() + HANG_GUARD_DURATION;
    while !is_condition_met() {
        assert!(
            Instant::now() < deadline,
            "{condition_description} never happened"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn opening_the_link_sends_a_hello_and_asks_what_the_supervisor_holds() {
    let peer = FakeSupervisor::start_with_supervisor_results(
        build_opening_supervisor_results(Vec::new()),
        Vec::new(),
    );
    let sink = RecordingSink::new();

    let backend = connect_test_backend(&peer, Arc::clone(&sink));

    assert_eq!(
        peer.list_requested_kinds(),
        vec![
            SupervisorRequestKind::build_hello_request(ConnectionToken::from_secret("k7QxSecret")),
            SupervisorRequestKind::ListPanes,
        ]
    );
    assert_eq!(backend.list_carried_panes(), Vec::new());
    assert_eq!(sink.list_exit_statuses(), Vec::new());
}

#[test]
fn carried_pane_the_supervisor_does_not_hold_is_reported_as_ended() {
    // No session server observed the child's status; the consumer still
    // learns the pane is gone.
    let gone_pane_id = PaneId::new();
    let peer = FakeSupervisor::start_with_supervisor_results(
        build_opening_supervisor_results(Vec::new()),
        Vec::new(),
    );
    let sink = RecordingSink::new();

    let backend = SupervisorPtyBackend::connect(
        &peer.supervisor_address,
        ConnectionToken::from_secret("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[gone_pane_id],
    )
    .expect("the backend opens the link");

    assert_eq!(
        sink.list_exit_statuses(),
        vec![(gone_pane_id, ExitStatus::ExitCode(-1))]
    );
    assert_eq!(backend.list_carried_panes(), Vec::new());
}

#[test]
fn opening_the_link_keeps_ends_and_kills_each_pane_difference() {
    // The image swap settles every difference at once: panes both sides agree
    // on are kept, panes only this side carried are reported ended, and panes
    // only the supervisor holds are killed. All three in one link opening.
    let first_kept_pane_id = PaneId::new();
    let second_kept_pane_id = PaneId::new();
    let gone_pane_id = PaneId::new();
    let first_orphan_pane_id = PaneId::new();
    let second_orphan_pane_id = PaneId::new();
    let larger_pty_size = PtySize {
        column_count: 132,
        row_count: 43,
    };
    let mut supervisor_results = build_opening_supervisor_results(vec![
        SupervisorPane {
            pane_id: first_orphan_pane_id,
            process_id: 4240,
            pty_size: STANDARD_PTY_SIZE,
        },
        SupervisorPane {
            pane_id: first_kept_pane_id,
            process_id: 4241,
            pty_size: STANDARD_PTY_SIZE,
        },
        SupervisorPane {
            pane_id: second_orphan_pane_id,
            process_id: 4242,
            pty_size: larger_pty_size,
        },
        SupervisorPane {
            pane_id: second_kept_pane_id,
            process_id: 4243,
            pty_size: larger_pty_size,
        },
    ]);
    // One answer per pane the opening kills.
    supervisor_results.push(SupervisorResult::Done);
    supervisor_results.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();

    let backend = SupervisorPtyBackend::connect(
        &peer.supervisor_address,
        ConnectionToken::from_secret("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[first_kept_pane_id, gone_pane_id, second_kept_pane_id],
    )
    .expect("the backend opens the link");

    assert_eq!(
        peer.list_requested_kinds(),
        vec![
            SupervisorRequestKind::build_hello_request(ConnectionToken::from_secret("k7QxSecret")),
            SupervisorRequestKind::ListPanes,
            SupervisorRequestKind::Kill {
                pane_id: first_orphan_pane_id,
                kill_policy: KillPolicy::Tree,
            },
            SupervisorRequestKind::Kill {
                pane_id: second_orphan_pane_id,
                kill_policy: KillPolicy::Tree,
            },
        ],
        "the panes nobody carried are killed in the order the supervisor listed them"
    );
    assert_eq!(
        sink.list_exit_statuses(),
        vec![(gone_pane_id, ExitStatus::ExitCode(-1))],
        "only the pane the supervisor no longer holds is reported ended"
    );

    let mut carried_panes = backend.list_carried_panes();
    carried_panes.sort_by_key(|carried_pane| carried_pane.process_id);
    assert_eq!(
        carried_panes,
        vec![
            CarriedPtyPane {
                pane_id: first_kept_pane_id,
                #[cfg(unix)]
                terminal_fd: None,
                process_id: 4241,
                pty_size: STANDARD_PTY_SIZE,
                exit_status: None,
            },
            CarriedPtyPane {
                pane_id: second_kept_pane_id,
                #[cfg(unix)]
                terminal_fd: None,
                process_id: 4243,
                pty_size: larger_pty_size,
                exit_status: None,
            },
        ],
        "each kept pane comes back with the process id and size the supervisor named"
    );
}

#[test]
fn a_pane_the_supervisor_holds_that_no_caller_carried_is_killed() {
    // A pane nobody carried is killed at the opening.
    let orphan_pane_id = PaneId::new();
    let kept_pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(vec![
        SupervisorPane {
            pane_id: orphan_pane_id,
            process_id: 4242,
            pty_size: STANDARD_PTY_SIZE,
        },
        SupervisorPane {
            pane_id: kept_pane_id,
            process_id: 4243,
            pty_size: STANDARD_PTY_SIZE,
        },
    ]);
    supervisor_results.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();

    let backend = SupervisorPtyBackend::connect(
        &peer.supervisor_address,
        ConnectionToken::from_secret("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[kept_pane_id],
    )
    .expect("the backend opens the link");

    assert_eq!(
        peer.list_requested_kinds(),
        vec![
            SupervisorRequestKind::build_hello_request(ConnectionToken::from_secret("k7QxSecret")),
            SupervisorRequestKind::ListPanes,
            SupervisorRequestKind::Kill {
                pane_id: orphan_pane_id,
                kill_policy: KillPolicy::Tree,
            },
        ]
    );
    assert_eq!(sink.list_exit_statuses(), Vec::new());
    assert_eq!(
        backend.list_carried_panes(),
        vec![CarriedPtyPane {
            pane_id: kept_pane_id,
            #[cfg(unix)]
            terminal_fd: None,
            process_id: 4243,
            pty_size: STANDARD_PTY_SIZE,
            exit_status: None,
        }]
    );
}

#[test]
fn spawning_a_pane_asks_the_supervisor_and_records_the_reported_child() {
    let pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Spawned { process_id: 4242 });
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));

    let handle = backend
        .spawn_pane(
            pane_id,
            build_shell_spawn_spec("sleep 30"),
            STANDARD_PTY_SIZE,
        )
        .expect("the supervisor opens the pane");

    assert_eq!(handle.get_pane_id(), pane_id);
    assert_eq!(
        handle.try_receive_output_chunk(),
        None,
        "a pane delivering through a sink carries no channels"
    );
    assert_eq!(
        backend.list_carried_panes(),
        vec![CarriedPtyPane {
            pane_id,
            #[cfg(unix)]
            terminal_fd: None,
            process_id: 4242,
            pty_size: STANDARD_PTY_SIZE,
            exit_status: None,
        }]
    );
}

#[test]
fn supervisor_spawn_refusal_is_reported_as_spawn_failure() {
    let pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::Unknown,
            message: "no terminal could be opened".to_string(),
        },
    ));
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));

    assert_eq!(
        backend
            .spawn_pane(
                pane_id,
                build_shell_spawn_spec("sleep 30"),
                STANDARD_PTY_SIZE
            )
            .expect_err("a refused spawn is a failure"),
        PtyError::Spawn {
            detail: "the supervisor refused Spawn: no terminal could be opened".to_string(),
        }
    );
    assert_eq!(
        backend.list_carried_panes(),
        Vec::new(),
        "a refused spawn leaves no pane behind"
    );
}

#[test]
fn resizing_records_the_size_only_once_the_supervisor_took_it() {
    let pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Spawned { process_id: 4242 });
    supervisor_results.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));
    backend
        .spawn_pane(
            pane_id,
            build_shell_spawn_spec("sleep 30"),
            STANDARD_PTY_SIZE,
        )
        .expect("the supervisor opens the pane");

    let wider_pty_size = PtySize {
        column_count: 120,
        row_count: 40,
    };
    backend
        .resize_pane(pane_id, wider_pty_size)
        .expect("the pane is retuned");

    assert_eq!(
        peer.list_requested_kinds().last(),
        Some(&SupervisorRequestKind::Resize {
            pane_id,
            pty_size: wider_pty_size,
        })
    );
    assert_eq!(
        backend.list_carried_panes(),
        vec![CarriedPtyPane {
            pane_id,
            #[cfg(unix)]
            terminal_fd: None,
            process_id: 4242,
            pty_size: wider_pty_size,
            exit_status: None,
        }]
    );
}

#[test]
fn writing_to_a_pane_sends_exactly_the_bytes_it_was_given() {
    let pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Spawned { process_id: 4242 });
    supervisor_results.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));
    backend
        .spawn_pane(pane_id, build_shell_spawn_spec("cat"), STANDARD_PTY_SIZE)
        .expect("the supervisor opens the pane");

    backend
        .write_pane_input(pane_id, b"hello\n")
        .expect("the bytes reach the supervisor");

    assert_eq!(
        peer.list_requested_kinds().last(),
        Some(&SupervisorRequestKind::Write {
            pane_id,
            input_bytes: b"hello\n".to_vec(),
        })
    );
}

#[test]
fn killing_a_pane_drops_it_from_this_backend() {
    let pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Spawned { process_id: 4242 });
    supervisor_results.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));
    backend
        .spawn_pane(
            pane_id,
            build_shell_spawn_spec("sleep 30"),
            STANDARD_PTY_SIZE,
        )
        .expect("the supervisor opens the pane");

    backend
        .kill_pane(pane_id, KillPolicy::Tree)
        .expect("the pane is closed");

    assert_eq!(
        peer.list_requested_kinds().last(),
        Some(&SupervisorRequestKind::Kill {
            pane_id,
            kill_policy: KillPolicy::Tree,
        })
    );
    assert_eq!(backend.list_carried_panes(), Vec::new());
    assert_eq!(
        backend.kill_pane(pane_id, KillPolicy::Tree),
        Err(PtyError::UnknownPane { pane_id }),
        "a pane already closed is not closed twice"
    );
}

#[test]
fn a_pane_this_backend_does_not_drive_is_refused_before_the_link_is_touched() {
    let unknown_pane_id = PaneId::new();
    let peer = FakeSupervisor::start_with_supervisor_results(
        build_opening_supervisor_results(Vec::new()),
        Vec::new(),
    );
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));
    let opening = peer.list_requested_kinds();

    assert_eq!(
        backend.resize_pane(unknown_pane_id, STANDARD_PTY_SIZE),
        Err(PtyError::UnknownPane {
            pane_id: unknown_pane_id,
        })
    );
    assert_eq!(
        backend.write_pane_input(unknown_pane_id, b"hi"),
        Err(PtyError::UnknownPane {
            pane_id: unknown_pane_id,
        })
    );
    assert_eq!(
        backend.kill_pane(unknown_pane_id, KillPolicy::Tree),
        Err(PtyError::UnknownPane {
            pane_id: unknown_pane_id,
        })
    );
    assert_eq!(backend.find_live_working_directory(unknown_pane_id), None);
    assert_eq!(
        peer.list_requested_kinds(),
        opening,
        "a pane this backend does not drive costs no round trip"
    );
}

#[test]
fn a_closed_pane_is_refused_while_the_backend_still_drives_another_one() {
    // The refusal must name the pane asked for, not merely notice that this
    // backend drives something.
    let closed_pane_id = PaneId::new();
    let still_open_pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Spawned { process_id: 4242 });
    supervisor_results.push(SupervisorResult::Spawned { process_id: 4243 });
    supervisor_results.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));
    backend
        .spawn_pane(
            closed_pane_id,
            build_shell_spawn_spec("sleep 30"),
            STANDARD_PTY_SIZE,
        )
        .expect("the supervisor opens the pane that is closed again");
    backend
        .spawn_pane(
            still_open_pane_id,
            build_shell_spawn_spec("sleep 30"),
            STANDARD_PTY_SIZE,
        )
        .expect("the supervisor opens the pane that stays open");
    backend
        .kill_pane(closed_pane_id, KillPolicy::Tree)
        .expect("the pane is closed");
    let asked_so_far = peer.list_requested_kinds();

    assert_eq!(
        backend.resize_pane(closed_pane_id, STANDARD_PTY_SIZE),
        Err(PtyError::UnknownPane {
            pane_id: closed_pane_id,
        })
    );
    assert_eq!(
        backend.write_pane_input(closed_pane_id, b"hi"),
        Err(PtyError::UnknownPane {
            pane_id: closed_pane_id,
        })
    );
    assert_eq!(backend.find_live_working_directory(closed_pane_id), None);
    assert_eq!(
        peer.list_requested_kinds(),
        asked_so_far,
        "a pane this backend no longer drives costs no round trip"
    );
    assert_eq!(
        backend.list_carried_panes(),
        vec![CarriedPtyPane {
            pane_id: still_open_pane_id,
            #[cfg(unix)]
            terminal_fd: None,
            process_id: 4243,
            pty_size: STANDARD_PTY_SIZE,
            exit_status: None,
        }],
        "the pane that was never closed is still driven"
    );
}

#[test]
fn output_and_exit_events_reach_the_sink_in_the_order_they_arrive() {
    let pane_id = PaneId::new();
    let peer = FakeSupervisor::start_with_supervisor_results(
        build_opening_supervisor_results(Vec::new()),
        vec![
            SupervisorEvent::Output {
                pane_id,
                output_bytes: b"first".to_vec(),
            },
            SupervisorEvent::Output {
                pane_id,
                output_bytes: b"second".to_vec(),
            },
            SupervisorEvent::Exited {
                pane_id,
                exit_status: ExitStatus::ExitCode(0),
            },
        ],
    );
    let sink = RecordingSink::new();
    let _backend = connect_test_backend(&peer, Arc::clone(&sink));

    wait_until_condition("the planted exit reached the sink", || {
        !sink.list_exit_statuses().is_empty()
    });

    assert_eq!(
        sink.list_output_chunks(),
        vec![(pane_id, b"first".to_vec()), (pane_id, b"second".to_vec()),]
    );
    assert_eq!(
        sink.list_exit_statuses(),
        vec![(pane_id, ExitStatus::ExitCode(0))]
    );
}

/// A sink that refuses every chunk from `refused_pane_id` and records the rest.
struct RefusingSink {
    /// The pane whose chunks are refused.
    refused_pane_id: PaneId,
    /// Every chunk taken, oldest first, with the pane that printed it.
    output_chunks: Mutex<Vec<(PaneId, Vec<u8>)>>,
    /// Every exit taken, oldest first, with the pane that ended.
    exit_statuses: Mutex<Vec<(PaneId, ExitStatus)>>,
}

impl RefusingSink {
    fn from_refused_pane_id(refused_pane_id: PaneId) -> Arc<Self> {
        Arc::new(RefusingSink {
            refused_pane_id,
            output_chunks: Mutex::new(Vec::new()),
            exit_statuses: Mutex::new(Vec::new()),
        })
    }

    fn list_output_chunks(&self) -> Vec<(PaneId, Vec<u8>)> {
        self.output_chunks.lock().expect("refusing sink").clone()
    }

    fn list_exit_statuses(&self) -> Vec<(PaneId, ExitStatus)> {
        self.exit_statuses.lock().expect("refusing sink").clone()
    }
}

impl PtySink for RefusingSink {
    fn accept_output_bytes(&self, pane_id: PaneId, output_bytes: Vec<u8>) -> bool {
        if pane_id == self.refused_pane_id {
            return false;
        }
        self.output_chunks
            .lock()
            .expect("refusing sink")
            .push((pane_id, output_bytes));
        true
    }

    fn accept_exit_status(&self, pane_id: PaneId, exit_status: ExitStatus) {
        self.exit_statuses
            .lock()
            .expect("refusing sink")
            .push((pane_id, exit_status));
    }
}

#[test]
fn a_sink_refusal_stops_one_pane_while_other_panes_keep_being_delivered() {
    // `PtySink::accept_output_bytes` returning false means the consumer is done with that
    // one pane, exit included. The link keeps carrying the rest.
    let refused_pane_id = PaneId::new();
    let live_pane_id = PaneId::new();
    let peer = FakeSupervisor::start_with_supervisor_results(
        build_opening_supervisor_results(Vec::new()),
        vec![
            SupervisorEvent::Output {
                pane_id: refused_pane_id,
                output_bytes: b"dropped".to_vec(),
            },
            SupervisorEvent::Output {
                pane_id: live_pane_id,
                output_bytes: b"kept".to_vec(),
            },
            SupervisorEvent::Output {
                pane_id: refused_pane_id,
                output_bytes: b"dropped again".to_vec(),
            },
            SupervisorEvent::Exited {
                pane_id: refused_pane_id,
                exit_status: ExitStatus::ExitCode(1),
            },
            SupervisorEvent::Exited {
                pane_id: live_pane_id,
                exit_status: ExitStatus::ExitCode(0),
            },
        ],
    );
    let sink = RefusingSink::from_refused_pane_id(refused_pane_id);
    let _backend = SupervisorPtyBackend::connect(
        &peer.supervisor_address,
        ConnectionToken::from_secret("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[],
    )
    .expect("the backend opens the link");

    wait_until_condition("the live pane's exit reached the sink", || {
        !sink.list_exit_statuses().is_empty()
    });

    assert_eq!(
        sink.list_output_chunks(),
        vec![(live_pane_id, b"kept".to_vec())]
    );
    assert_eq!(
        sink.list_exit_statuses(),
        vec![(live_pane_id, ExitStatus::ExitCode(0))]
    );
}

#[test]
fn holding_the_readers_still_asks_the_supervisor_to_hold_pane_output() {
    // The hold reaches the supervisor over the link. The answer is the last
    // frame the link carries, and the link's one reader thread hands every
    // frame written before it to the sink first: a pause that answered leaves
    // nothing read but undelivered.
    let pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Spawned { process_id: 4242 });
    supervisor_results.push(SupervisorResult::Done);
    supervisor_results.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start_with_supervisor_results(
        supervisor_results,
        vec![SupervisorEvent::Output {
            pane_id,
            output_bytes: b"before".to_vec(),
        }],
    );
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));
    backend
        .spawn_pane(
            pane_id,
            build_shell_spawn_spec("sleep 30"),
            STANDARD_PTY_SIZE,
        )
        .expect("the supervisor opens the pane");

    assert_eq!(backend.pause_readers(), Ok(()));
    assert_eq!(
        sink.list_output_chunks(),
        vec![(pane_id, b"before".to_vec())],
        "a pause that answered has already handed the consumer every chunk \
         written before that answer"
    );
    assert_eq!(
        backend.list_carried_panes(),
        vec![CarriedPtyPane {
            pane_id,
            #[cfg(unix)]
            terminal_fd: None,
            process_id: 4242,
            pty_size: STANDARD_PTY_SIZE,
            exit_status: None,
        }],
        "a paused backend still names every pane the swap must carry"
    );
    backend.resume_readers();

    assert_eq!(
        peer.list_requested_kinds(),
        vec![
            SupervisorRequestKind::build_hello_request(ConnectionToken::from_secret("k7QxSecret")),
            SupervisorRequestKind::ListPanes,
            SupervisorRequestKind::Spawn {
                pane_id,
                spawn_spec: build_shell_spawn_spec("sleep 30"),
                pty_size: STANDARD_PTY_SIZE,
            },
            SupervisorRequestKind::PauseOutput,
            SupervisorRequestKind::ResumeOutput,
        ]
    );
}

#[test]
fn a_supervisor_that_cannot_hold_its_output_fails_the_pause() {
    // A supervisor built before the request existed refuses the kind by name,
    // and the refusal reaches the caller.
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::UnsupportedKind,
            message: "PauseOutput is not a request kind this supervisor has".to_string(),
        },
    ));
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));

    assert_eq!(
        backend.pause_readers(),
        Err(PtyError::Io {
            detail: "the supervisor refused PauseOutput: PauseOutput is not a request kind \
                     this supervisor has"
                .to_string(),
        })
    );
}

#[test]
fn a_pause_answered_with_something_else_is_refused() {
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Panes(Vec::new()));
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));

    assert_eq!(
        backend.pause_readers(),
        Err(PtyError::Io {
            detail: "the supervisor answered PauseOutput with Panes".to_string(),
        })
    );
}

#[test]
fn a_broken_link_fails_the_request_in_flight() {
    // The fake supervisor runs out of supervisor_results and closes, which is what a
    // supervisor that died mid-request looks like from here.
    let pane_id = PaneId::new();
    let peer = FakeSupervisor::start_with_supervisor_results(
        build_opening_supervisor_results(Vec::new()),
        Vec::new(),
    );
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));

    assert_eq!(
        backend
            .spawn_pane(
                pane_id,
                build_shell_spawn_spec("sleep 30"),
                STANDARD_PTY_SIZE
            )
            .expect_err("a broken link fails the spawn"),
        PtyError::Spawn {
            detail: "the supervisor link closed while Spawn was in flight".to_string(),
        }
    );
}

#[test]
fn an_answer_that_does_not_fit_its_request_is_refused() {
    let pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));

    assert_eq!(
        backend
            .spawn_pane(
                pane_id,
                build_shell_spawn_spec("sleep 30"),
                STANDARD_PTY_SIZE
            )
            .expect_err("an answer that does not fit is a failure"),
        PtyError::Io {
            detail: "the supervisor answered Spawn with Done".to_string(),
        }
    );
}

#[test]
fn shutting_the_supervisor_down_is_requested_once() {
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));

    backend
        .shutdown_supervisor()
        .expect("the supervisor is told to end");

    assert_eq!(
        peer.list_requested_kinds().last(),
        Some(&SupervisorRequestKind::Shutdown)
    );
}

#[test]
fn shutting_down_over_a_broken_link_still_returns_success() {
    let peer = FakeSupervisor::start_with_supervisor_results(
        build_opening_supervisor_results(Vec::new()),
        Vec::new(),
    );
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));

    assert_eq!(backend.shutdown_supervisor(), Ok(()));
}

#[test]
fn supervisor_shutdown_refusal_still_returns_success() {
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::Unknown,
            message: "a pane could not be closed".to_string(),
        },
    ));
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));

    assert_eq!(backend.shutdown_supervisor(), Ok(()));
    assert_eq!(
        peer.list_requested_kinds().last(),
        Some(&SupervisorRequestKind::Shutdown)
    );
}

#[test]
fn asking_a_pane_for_its_working_directory_returns_the_supervisor_answer() {
    // Every answer other than a directory leaves the pane without one.
    let pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Spawned { process_id: 4242 });
    supervisor_results.push(SupervisorResult::Cwd(Some(PathBuf::from("/tmp/work"))));
    supervisor_results.push(SupervisorResult::Cwd(None));
    supervisor_results.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));
    backend
        .spawn_pane(
            pane_id,
            build_shell_spawn_spec("sleep 30"),
            STANDARD_PTY_SIZE,
        )
        .expect("the supervisor opens the pane");

    assert_eq!(
        backend.find_live_working_directory(pane_id),
        Some(PathBuf::from("/tmp/work")),
        "the directory the supervisor named must reach the caller unchanged"
    );
    assert_eq!(
        peer.list_requested_kinds().last(),
        Some(&SupervisorRequestKind::LiveCwd { pane_id }),
        "the pane asked about must be the pane named"
    );
    assert_eq!(
        backend.find_live_working_directory(pane_id),
        None,
        "an operating system that cannot answer leaves the pane without a directory"
    );
    assert_eq!(
        backend.find_live_working_directory(pane_id),
        None,
        "an answer that is not a directory is not a directory"
    );
}

#[test]
fn supervisor_kill_refusal_still_drops_the_pane() {
    // The pane leaves this backend whatever the supervisor answers.
    let pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Spawned { process_id: 4242 });
    supervisor_results.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::Unknown,
            message: "the pane could not be closed".to_string(),
        },
    ));
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));
    backend
        .spawn_pane(
            pane_id,
            build_shell_spawn_spec("sleep 30"),
            STANDARD_PTY_SIZE,
        )
        .expect("the supervisor opens the pane");

    assert_eq!(
        backend.kill_pane(pane_id, KillPolicy::Tree),
        Err(PtyError::Io {
            detail: "the supervisor refused Kill: the pane could not be closed".to_string(),
        }),
        "a refusal must reach the caller"
    );
    assert_eq!(
        backend.list_carried_panes(),
        Vec::new(),
        "and the pane must be gone from this backend all the same"
    );
    assert_eq!(
        backend.kill_pane(pane_id, KillPolicy::Tree),
        Err(PtyError::UnknownPane { pane_id }),
        "so closing it again is refused without a round trip"
    );
}

#[test]
fn supervisor_resize_refusal_leaves_the_pane_at_its_old_size() {
    // A refused resize leaves the recorded size unchanged: a carried pane
    // reports the size its child was really told.
    let pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Spawned { process_id: 4242 });
    supervisor_results.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::Unknown,
            message: "the terminal could not be retuned".to_string(),
        },
    ));
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));
    backend
        .spawn_pane(
            pane_id,
            build_shell_spawn_spec("sleep 30"),
            STANDARD_PTY_SIZE,
        )
        .expect("the supervisor opens the pane");

    let wider_pty_size = PtySize {
        column_count: 120,
        row_count: 40,
    };
    assert_eq!(
        backend.resize_pane(pane_id, wider_pty_size),
        Err(PtyError::Io {
            detail: "the supervisor refused Resize: the terminal could not be retuned".to_string(),
        }),
        "a refusal must reach the caller"
    );
    assert_eq!(
        backend.list_carried_panes(),
        vec![CarriedPtyPane {
            pane_id,
            #[cfg(unix)]
            terminal_fd: None,
            process_id: 4242,
            pty_size: STANDARD_PTY_SIZE,
            exit_status: None,
        }],
        "a pane whose child was never told the new size must still report the old one"
    );
}

#[test]
fn graceful_kill_waits_out_its_grace_window_in_the_answer_timeout() {
    // A kill granting a grace window waits `ANSWER_WAIT_DURATION` plus that window;
    // the supervisor spends the window before it answers.
    let pane_id = PaneId::new();
    let grace_duration = Duration::from_secs(4);

    assert_eq!(
        compute_answer_wait_duration(&SupervisorRequestKind::Kill {
            pane_id,
            kill_policy: KillPolicy::Graceful {
                timeout_duration: grace_duration
            },
        }),
        ANSWER_WAIT_DURATION + grace_duration
    );
    assert_eq!(
        compute_answer_wait_duration(&SupervisorRequestKind::Kill {
            pane_id,
            kill_policy: KillPolicy::GracefulTree {
                timeout_duration: grace_duration
            },
        }),
        ANSWER_WAIT_DURATION + grace_duration
    );
    assert_eq!(
        compute_answer_wait_duration(&SupervisorRequestKind::Kill {
            pane_id,
            kill_policy: KillPolicy::Force,
        }),
        ANSWER_WAIT_DURATION,
        "a kill that spends no grace window waits no longer than any other request"
    );
    assert_eq!(
        compute_answer_wait_duration(&SupervisorRequestKind::Kill {
            pane_id,
            kill_policy: KillPolicy::Tree,
        }),
        ANSWER_WAIT_DURATION
    );
    assert_eq!(
        compute_answer_wait_duration(&SupervisorRequestKind::Write {
            pane_id,
            input_bytes: b"hi".to_vec(),
        }),
        ANSWER_WAIT_DURATION
    );
}

#[test]
fn request_ids_start_at_one_and_count_up_by_one() {
    let pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Spawned { process_id: 4242 });
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));

    backend
        .spawn_pane(
            pane_id,
            build_shell_spawn_spec("sleep 30"),
            STANDARD_PTY_SIZE,
        )
        .expect("the supervisor opens the pane");

    assert_eq!(peer.list_request_ids(), vec![1, 2, 3]);
}

#[test]
fn a_hello_the_supervisor_refuses_fails_the_opening() {
    let peer = FakeSupervisor::start_with_supervisor_results(
        vec![SupervisorResult::Error(
            koshi_ipc::protocol::IpcErrorPayload {
                code: koshi_ipc::protocol::IpcErrorCode::BadToken,
                message: "the token does not match".to_string(),
            },
        )],
        Vec::new(),
    );
    let sink = RecordingSink::new();

    let supervisor_error = SupervisorPtyBackend::connect(
        &peer.supervisor_address,
        ConnectionToken::from_secret("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[],
    )
    .err()
    .expect("a refused Hello fails the opening");

    assert_eq!(
        supervisor_error,
        PtyError::Io {
            detail: "the supervisor refused Hello: the token does not match".to_string(),
        }
    );
    assert_eq!(
        peer.list_requested_kinds(),
        vec![SupervisorRequestKind::build_hello_request(
            ConnectionToken::from_secret("k7QxSecret")
        )],
        "nothing is asked after a refused Hello"
    );
}

#[test]
fn a_hello_answered_with_something_else_fails_the_opening() {
    let peer =
        FakeSupervisor::start_with_supervisor_results(vec![SupervisorResult::Done], Vec::new());
    let sink = RecordingSink::new();

    let supervisor_error = SupervisorPtyBackend::connect(
        &peer.supervisor_address,
        ConnectionToken::from_secret("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[],
    )
    .err()
    .expect("an answer that does not fit fails the opening");

    assert_eq!(
        supervisor_error,
        PtyError::Io {
            detail: "the supervisor answered Hello with Done".to_string(),
        }
    );
}

#[test]
fn a_pane_list_answered_with_something_else_fails_the_opening() {
    let peer = FakeSupervisor::start_with_supervisor_results(
        vec![
            SupervisorResult::Hello {
                protocol_version: 1,
            },
            SupervisorResult::Done,
        ],
        Vec::new(),
    );
    let sink = RecordingSink::new();

    let error = SupervisorPtyBackend::connect(
        &peer.supervisor_address,
        ConnectionToken::from_secret("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[],
    )
    .err()
    .expect("an answer that does not fit fails the opening");

    assert_eq!(
        error,
        PtyError::Io {
            detail: "the supervisor answered ListPanes with Done".to_string(),
        }
    );
}

#[test]
fn an_address_nobody_listens_on_fails_the_opening() {
    let runtime_directory = tempfile::tempdir().expect("an isolated test directory is created");
    let supervisor_address = build_test_supervisor_address(runtime_directory.path());
    let sink = RecordingSink::new();

    let supervisor_error = SupervisorPtyBackend::connect(
        &supervisor_address,
        ConnectionToken::from_secret("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[],
    )
    .err()
    .expect("an address nobody listens on fails the opening");

    let PtyError::Io { detail } = supervisor_error else {
        panic!("an unreachable supervisor is an io failure, not {supervisor_error:?}");
    };
    let expected_start = format!("the supervisor at {supervisor_address} could not be reached: ");
    assert!(
        detail.starts_with(&expected_start),
        "the failure names the address: {detail}"
    );
    assert!(
        detail.len() > expected_start.len(),
        "the failure carries the operating system's reason: {detail}"
    );
}

#[test]
fn an_answer_naming_no_request_fails_the_request_in_flight() {
    let peer = FakeSupervisor::start_with_supervisor_frames(
        build_opening_supervisor_results(Vec::new()),
        vec![SupervisorTestFrame::Message(SupervisorMessage::Response(
            SupervisorResponse {
                request_id: None,
                answer_result: SupervisorResult::Done,
            },
        ))],
    );
    let sink = RecordingSink::new();

    let supervisor_error = SupervisorPtyBackend::connect(
        &peer.supervisor_address,
        ConnectionToken::from_secret("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[],
    )
    .err()
    .expect("an answer naming no request fails the opening");

    assert_eq!(
        supervisor_error,
        PtyError::Io {
            detail: "the supervisor answered request None while Hello (request 1) was in flight"
                .to_string(),
        }
    );
}

#[test]
fn an_answer_to_a_request_not_yet_sent_fails_the_request_in_flight() {
    let peer = FakeSupervisor::start_with_supervisor_frames(
        build_opening_supervisor_results(Vec::new()),
        vec![SupervisorTestFrame::Message(SupervisorMessage::Response(
            SupervisorResponse {
                request_id: Some(7),
                answer_result: SupervisorResult::Done,
            },
        ))],
    );
    let sink = RecordingSink::new();

    let supervisor_error = SupervisorPtyBackend::connect(
        &peer.supervisor_address,
        ConnectionToken::from_secret("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[],
    )
    .err()
    .expect("an answer to a request not yet sent fails the opening");

    assert_eq!(
        supervisor_error,
        PtyError::Io {
            detail: "the supervisor answered request Some(7) while Hello (request 1) was in flight"
                .to_string(),
        }
    );
}

#[test]
fn an_answer_to_an_earlier_request_is_passed_over() {
    // Request ids start at 1, so an answer to request 0 reads as the answer
    // to a request whose wait already ran out.
    let peer = FakeSupervisor::start_with_supervisor_frames(
        build_opening_supervisor_results(Vec::new()),
        vec![SupervisorTestFrame::Message(SupervisorMessage::Response(
            SupervisorResponse {
                request_id: Some(0),
                answer_result: SupervisorResult::Done,
            },
        ))],
    );
    let sink = RecordingSink::new();

    let backend = connect_test_backend(&peer, Arc::clone(&sink));

    assert_eq!(
        peer.list_requested_kinds(),
        vec![
            SupervisorRequestKind::build_hello_request(ConnectionToken::from_secret("k7QxSecret")),
            SupervisorRequestKind::ListPanes,
        ]
    );
    assert_eq!(backend.list_carried_panes(), Vec::new());
}

#[test]
fn an_answer_variant_this_build_does_not_name_is_refused() {
    let peer = FakeSupervisor::start_with_supervisor_frames(
        Vec::new(),
        vec![SupervisorTestFrame::UnknownResponseVariant {
            request_id: Some(1),
            unknown_variant_name: "Floating".to_string(),
        }],
    );
    let sink = RecordingSink::new();

    let supervisor_error = SupervisorPtyBackend::connect(
        &peer.supervisor_address,
        ConnectionToken::from_secret("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[],
    )
    .err()
    .expect("an answer this build has no name for fails the opening");

    assert_eq!(
        supervisor_error,
        PtyError::Io {
            detail: "the supervisor answered Hello with Floating, which this build has no name for"
                .to_string(),
        }
    );
}

#[test]
fn an_event_variant_this_build_does_not_name_is_passed_over() {
    let pane_id = PaneId::new();
    let peer = FakeSupervisor::start_with_supervisor_frames(
        build_opening_supervisor_results(Vec::new()),
        vec![
            SupervisorTestFrame::UnknownEventVariant("Bell".to_string()),
            SupervisorTestFrame::Message(SupervisorMessage::Event(SupervisorEvent::Output {
                pane_id,
                output_bytes: b"after".to_vec(),
            })),
        ],
    );
    let sink = RecordingSink::new();

    let backend = connect_test_backend(&peer, Arc::clone(&sink));

    wait_until_condition("the chunk after the unknown event reached the sink", || {
        !sink.list_output_chunks().is_empty()
    });
    assert_eq!(
        sink.list_output_chunks(),
        vec![(pane_id, b"after".to_vec())]
    );
    assert_eq!(sink.list_exit_statuses(), Vec::new());
    assert_eq!(backend.list_carried_panes(), Vec::new());
}

#[test]
fn an_orphan_the_supervisor_will_not_kill_does_not_stop_the_opening() {
    let orphan_pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(vec![SupervisorPane {
        pane_id: orphan_pane_id,
        process_id: 4242,
        pty_size: STANDARD_PTY_SIZE,
    }]);
    supervisor_results.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::Unknown,
            message: "the pane could not be closed".to_string(),
        },
    ));
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();

    let backend = connect_test_backend(&peer, Arc::clone(&sink));

    assert_eq!(
        peer.list_requested_kinds(),
        vec![
            SupervisorRequestKind::build_hello_request(ConnectionToken::from_secret("k7QxSecret")),
            SupervisorRequestKind::ListPanes,
            SupervisorRequestKind::Kill {
                pane_id: orphan_pane_id,
                kill_policy: KillPolicy::Tree,
            },
        ]
    );
    assert_eq!(
        backend.list_carried_panes(),
        Vec::new(),
        "a pane nobody carried is not driven, whatever the kill answered"
    );
    assert_eq!(sink.list_exit_statuses(), Vec::new());
}

#[test]
fn supervisor_write_refusal_reaches_the_caller_and_keeps_the_pane() {
    let pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Spawned { process_id: 4242 });
    supervisor_results.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::Unknown,
            message: "the child is gone".to_string(),
        },
    ));
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));
    backend
        .spawn_pane(pane_id, build_shell_spawn_spec("cat"), STANDARD_PTY_SIZE)
        .expect("the supervisor opens the pane");

    assert_eq!(
        backend.write_pane_input(pane_id, b"hello\n"),
        Err(PtyError::Io {
            detail: "the supervisor refused Write: the child is gone".to_string(),
        })
    );
    assert_eq!(
        backend.list_carried_panes(),
        vec![CarriedPtyPane {
            pane_id,
            #[cfg(unix)]
            terminal_fd: None,
            process_id: 4242,
            pty_size: STANDARD_PTY_SIZE,
            exit_status: None,
        }],
        "a refused write leaves the pane driven"
    );
}

#[test]
fn a_write_answered_with_something_else_is_refused() {
    let pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Spawned { process_id: 4242 });
    supervisor_results.push(SupervisorResult::Cwd(None));
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));
    backend
        .spawn_pane(pane_id, build_shell_spawn_spec("cat"), STANDARD_PTY_SIZE)
        .expect("the supervisor opens the pane");

    assert_eq!(
        backend.write_pane_input(pane_id, b"hello\n"),
        Err(PtyError::Io {
            detail: "the supervisor answered Write with Cwd".to_string(),
        })
    );
}

#[test]
fn kill_over_a_broken_link_still_drops_the_pane() {
    let pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Spawned { process_id: 4242 });
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));
    backend
        .spawn_pane(
            pane_id,
            build_shell_spawn_spec("sleep 30"),
            STANDARD_PTY_SIZE,
        )
        .expect("the supervisor opens the pane");

    assert_eq!(
        backend.kill_pane(pane_id, KillPolicy::Tree),
        Err(PtyError::Io {
            detail: "the supervisor link closed while Kill was in flight".to_string(),
        })
    );
    assert_eq!(backend.list_carried_panes(), Vec::new());
    assert_eq!(
        backend.kill_pane(pane_id, KillPolicy::Tree),
        Err(PtyError::UnknownPane { pane_id })
    );
}

#[test]
fn supervisor_working_directory_refusal_returns_none() {
    let pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Spawned { process_id: 4242 });
    supervisor_results.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::Unknown,
            message: "no such pane".to_string(),
        },
    ));
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));
    backend
        .spawn_pane(
            pane_id,
            build_shell_spawn_spec("sleep 30"),
            STANDARD_PTY_SIZE,
        )
        .expect("the supervisor opens the pane");

    assert_eq!(backend.find_live_working_directory(pane_id), None);
    assert_eq!(
        peer.list_requested_kinds().last(),
        Some(&SupervisorRequestKind::LiveCwd { pane_id })
    );
}

#[test]
fn supervisor_resume_refusal_leaves_the_link_serving() {
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::UnsupportedKind,
            message: "ResumeOutput is not a request kind this supervisor has".to_string(),
        },
    ));
    supervisor_results.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));

    backend.resume_readers();

    assert_eq!(backend.shutdown_supervisor(), Ok(()));
    assert_eq!(
        peer.list_requested_kinds(),
        vec![
            SupervisorRequestKind::build_hello_request(ConnectionToken::from_secret("k7QxSecret")),
            SupervisorRequestKind::ListPanes,
            SupervisorRequestKind::ResumeOutput,
            SupervisorRequestKind::Shutdown,
        ]
    );
}

#[test]
fn a_shutdown_answered_with_something_else_is_refused() {
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Panes(Vec::new()));
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));

    assert_eq!(
        backend.shutdown_supervisor(),
        Err(PtyError::Io {
            detail: "the supervisor answered Shutdown with Panes".to_string(),
        })
    );
}

#[test]
fn flushing_the_writers_never_fails() {
    let peer = FakeSupervisor::start_with_supervisor_results(
        build_opening_supervisor_results(Vec::new()),
        Vec::new(),
    );
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));
    let opening_request_kinds = peer.list_requested_kinds();

    assert_eq!(backend.flush_writers(), Ok(()));
    assert_eq!(
        peer.list_requested_kinds(),
        opening_request_kinds,
        "flushing asks the supervisor nothing"
    );
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "spawn into an already-live pane id")]
fn spawning_into_a_live_pane_id_panics_in_debug_builds() {
    let pane_id = PaneId::new();
    let mut supervisor_results = build_opening_supervisor_results(Vec::new());
    supervisor_results.push(SupervisorResult::Spawned { process_id: 4242 });
    let peer = FakeSupervisor::start_with_supervisor_results(supervisor_results, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_test_backend(&peer, Arc::clone(&sink));
    backend
        .spawn_pane(
            pane_id,
            build_shell_spawn_spec("sleep 30"),
            STANDARD_PTY_SIZE,
        )
        .expect("the supervisor opens the pane");

    let _ = backend.spawn_pane(
        pane_id,
        build_shell_spawn_spec("sleep 30"),
        STANDARD_PTY_SIZE,
    );
}
