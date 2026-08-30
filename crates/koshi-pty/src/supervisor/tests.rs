//! Tests for [`SupervisorPtyBackend`]: every backend call becomes the request
//! it should, an answer that does not fit its request is refused, a pane this
//! backend does not drive is refused before the link is touched, events reach
//! the sink in the order they arrive, holding the readers still asks the
//! supervisor to hold its pane output and fails when it cannot, and
//! [`SupervisorPtyBackend::connect`] settles both ways a pane list can disagree
//! with what the supervisor holds.
//!
//! The peer here is a hand-written supervisor over a real socket: it answers
//! whatever the test queued and records what it was asked. The backend is
//! tested against the wire, not against a stub of itself.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use koshi_core::process::ShellKind;
use koshi_ipc::supervisor::{SupervisorPane, SupervisorRequestKind};
use koshi_ipc::transport::Listener;

use super::*;

/// How long a test waits for something it expects promptly. A wait that
/// reaches it fails the test.
const HANG_GUARD: Duration = Duration::from_secs(5);

/// The size every pane in these tests opens at.
const PANE_SIZE: PtySize = PtySize { cols: 80, rows: 24 };

/// A sink that keeps everything it is handed and accepts every chunk.
struct RecordingSink {
    /// Every output chunk taken, oldest first, with the pane that printed it.
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

    /// Every chunk this sink has been handed, in order.
    fn chunks(&self) -> Vec<(PaneId, Vec<u8>)> {
        self.chunks.lock().expect("recording sink").clone()
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

/// One frame the fake supervisor writes before it reads its first request.
enum Frame {
    /// A message whose every name this build has.
    Message(SupervisorMessage),
    /// An answer whose result is a variant name this build does not have.
    UnknownAnswer {
        request_id: Option<u64>,
        name: String,
    },
    /// An event whose variant name this build does not have.
    UnknownEvent(String),
}

/// A hand-written supervisor on the other end of one link.
///
/// It answers each request from `answers`, in order, and records the requests
/// it was asked. The planted frames are sent before the first request is read.
struct FakeSupervisor {
    /// The address the backend connects to.
    addr: String,
    /// The requests this supervisor was asked, oldest first, each with the id
    /// it carried.
    asked: Arc<Mutex<Vec<(u64, SupervisorRequestKind)>>>,
    /// The socket file's directory. Outlives the link on Unix.
    _runtime_dir: tempfile::TempDir,
}

impl FakeSupervisor {
    /// Start a supervisor that answers `answers` in order and sends `events`
    /// before reading anything.
    ///
    /// The first two answers a [`SupervisorPtyBackend::connect`] needs, the
    /// Hello and the pane list, are the caller's to supply.
    fn start(answers: Vec<SupervisorResult>, events: Vec<SupervisorEvent>) -> FakeSupervisor {
        let frames = events
            .into_iter()
            .map(|event| Frame::Message(SupervisorMessage::Event(event)))
            .collect();
        FakeSupervisor::start_planted(answers, frames)
    }

    /// Start a supervisor that answers `answers` in order and writes `frames`
    /// before reading anything.
    fn start_planted(answers: Vec<SupervisorResult>, frames: Vec<Frame>) -> FakeSupervisor {
        let runtime_dir = tempfile::tempdir().expect("a temporary directory is created");
        let addr = link_addr(runtime_dir.path());
        let listener = Listener::bind(&addr).expect("the fake supervisor binds its link");
        let asked = Arc::new(Mutex::new(Vec::new()));

        let recorded = Arc::clone(&asked);
        thread::Builder::new()
            .name("fake-supervisor".to_string())
            .spawn(move || {
                let connection = listener.accept().expect("the backend connects");
                let (mut reader, mut writer) = connection.split();
                for frame in frames {
                    let sent = match frame {
                        Frame::Message(message) => writer.send(&message),
                        Frame::UnknownAnswer { request_id, name } => {
                            writer.send(&SupervisorMessage::<String, SupervisorEvent>::Response(
                                SupervisorResponse {
                                    request_id,
                                    result: name,
                                },
                            ))
                        }
                        Frame::UnknownEvent(name) => {
                            writer.send(&SupervisorMessage::<SupervisorResult, String>::Event(name))
                        }
                    };
                    sent.expect("the fake supervisor sends its planted frame");
                }
                let mut answers = answers.into_iter();
                while let Ok(request) = reader.recv::<SupervisorRequest>() {
                    recorded
                        .lock()
                        .expect("recorded requests")
                        .push((request.request_id, request.kind));
                    let Some(result) = answers.next() else {
                        return;
                    };
                    if writer
                        .send(&SupervisorMessage::<_, SupervisorEvent>::Response(
                            SupervisorResponse {
                                request_id: Some(request.request_id),
                                result,
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
            addr,
            asked,
            _runtime_dir: runtime_dir,
        }
    }

    /// The request kinds this supervisor was asked, oldest first.
    fn asked(&self) -> Vec<SupervisorRequestKind> {
        self.asked
            .lock()
            .expect("recorded requests")
            .iter()
            .map(|(_, kind)| kind.clone())
            .collect()
    }

    /// The request ids this supervisor was asked with, oldest first.
    fn asked_ids(&self) -> Vec<u64> {
        self.asked
            .lock()
            .expect("recorded requests")
            .iter()
            .map(|(request_id, _)| *request_id)
            .collect()
    }
}

/// An address for one test's link. On Unix it is a socket file inside `dir`;
/// on Windows it is a pipe name of its own, and `dir` goes unused.
fn link_addr(dir: &std::path::Path) -> String {
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

/// The answers every `connect` needs before the test's own answers: an
/// accepted Hello, then the pane list `held`.
fn opening_answers(held: Vec<SupervisorPane>) -> Vec<SupervisorResult> {
    vec![
        SupervisorResult::Hello {
            protocol_version: 1,
        },
        SupervisorResult::Panes(held),
    ]
}

/// A spawn spec launching `script`. Nothing here ever runs it: the fake
/// supervisor answers the request instead of spawning anything.
fn shell_spec(script: &str) -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_string(), script.to_string()],
        cwd: None,
        env: BTreeMap::new(),
        shell_kind: ShellKind::Other("sh".to_string()),
    }
}

/// Connect a backend to `peer`, expecting no pane to be carried in.
fn connect_to(peer: &FakeSupervisor, sink: Arc<RecordingSink>) -> SupervisorPtyBackend {
    SupervisorPtyBackend::connect(&peer.addr, ConnectionToken::new("k7QxSecret"), sink, &[])
        .expect("the backend opens the link")
}

/// Wait until `read` answers `true`. Fails the test after [`HANG_GUARD`].
fn wait_until(what: &str, read: impl Fn() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !read() {
        assert!(Instant::now() < deadline, "{what} never happened");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn opening_the_link_sends_a_hello_and_asks_what_the_supervisor_holds() {
    let peer = FakeSupervisor::start(opening_answers(Vec::new()), Vec::new());
    let sink = RecordingSink::new();

    let backend = connect_to(&peer, Arc::clone(&sink));

    assert_eq!(
        peer.asked(),
        vec![
            SupervisorRequestKind::hello(ConnectionToken::new("k7QxSecret")),
            SupervisorRequestKind::ListPanes,
        ]
    );
    assert_eq!(backend.carried_panes(), Vec::new());
    assert_eq!(sink.exits(), Vec::new());
}

#[test]
fn a_carried_pane_the_supervisor_does_not_hold_is_reported_as_ended() {
    // No session server observed the child's status; the consumer still
    // learns the pane is gone.
    let gone = PaneId::new();
    let peer = FakeSupervisor::start(opening_answers(Vec::new()), Vec::new());
    let sink = RecordingSink::new();

    let backend = SupervisorPtyBackend::connect(
        &peer.addr,
        ConnectionToken::new("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[gone],
    )
    .expect("the backend opens the link");

    assert_eq!(sink.exits(), vec![(gone, ExitStatus::ExitCode(-1))]);
    assert_eq!(backend.carried_panes(), Vec::new());
}

#[test]
fn a_link_carrying_many_panes_keeps_ends_and_kills_each_of_them_in_one_opening() {
    // The image swap settles every difference at once: panes both sides agree
    // on are kept, panes only this side carried are reported ended, and panes
    // only the supervisor holds are killed. All three in one link opening.
    let kept_first = PaneId::new();
    let kept_second = PaneId::new();
    let gone = PaneId::new();
    let orphan_first = PaneId::new();
    let orphan_second = PaneId::new();
    let bigger = PtySize {
        cols: 132,
        rows: 43,
    };
    let mut answers = opening_answers(vec![
        SupervisorPane {
            pane_id: orphan_first,
            pid: 4240,
            size: PANE_SIZE,
        },
        SupervisorPane {
            pane_id: kept_first,
            pid: 4241,
            size: PANE_SIZE,
        },
        SupervisorPane {
            pane_id: orphan_second,
            pid: 4242,
            size: bigger,
        },
        SupervisorPane {
            pane_id: kept_second,
            pid: 4243,
            size: bigger,
        },
    ]);
    // One answer per pane the opening kills.
    answers.push(SupervisorResult::Done);
    answers.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();

    let backend = SupervisorPtyBackend::connect(
        &peer.addr,
        ConnectionToken::new("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[kept_first, gone, kept_second],
    )
    .expect("the backend opens the link");

    assert_eq!(
        peer.asked(),
        vec![
            SupervisorRequestKind::hello(ConnectionToken::new("k7QxSecret")),
            SupervisorRequestKind::ListPanes,
            SupervisorRequestKind::Kill {
                pane_id: orphan_first,
                kill_policy: KillPolicy::Tree,
            },
            SupervisorRequestKind::Kill {
                pane_id: orphan_second,
                kill_policy: KillPolicy::Tree,
            },
        ],
        "the panes nobody carried are killed in the order the supervisor listed them"
    );
    assert_eq!(
        sink.exits(),
        vec![(gone, ExitStatus::ExitCode(-1))],
        "only the pane the supervisor no longer holds is reported ended"
    );

    let mut carried = backend.carried_panes();
    carried.sort_by_key(|pane| pane.pid);
    assert_eq!(
        carried,
        vec![
            CarriedPtyPane {
                pane_id: kept_first,
                #[cfg(unix)]
                terminal_fd: None,
                pid: 4241,
                size: PANE_SIZE,
                exit: None,
            },
            CarriedPtyPane {
                pane_id: kept_second,
                #[cfg(unix)]
                terminal_fd: None,
                pid: 4243,
                size: bigger,
                exit: None,
            },
        ],
        "each kept pane comes back with the process id and size the supervisor named"
    );
}

#[test]
fn a_pane_the_supervisor_holds_that_nobody_carried_is_killed() {
    // A pane nobody carried is killed at the opening.
    let orphan = PaneId::new();
    let kept = PaneId::new();
    let mut answers = opening_answers(vec![
        SupervisorPane {
            pane_id: orphan,
            pid: 4242,
            size: PANE_SIZE,
        },
        SupervisorPane {
            pane_id: kept,
            pid: 4243,
            size: PANE_SIZE,
        },
    ]);
    answers.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();

    let backend = SupervisorPtyBackend::connect(
        &peer.addr,
        ConnectionToken::new("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[kept],
    )
    .expect("the backend opens the link");

    assert_eq!(
        peer.asked(),
        vec![
            SupervisorRequestKind::hello(ConnectionToken::new("k7QxSecret")),
            SupervisorRequestKind::ListPanes,
            SupervisorRequestKind::Kill {
                pane_id: orphan,
                kill_policy: KillPolicy::Tree,
            },
        ]
    );
    assert_eq!(sink.exits(), Vec::new());
    assert_eq!(
        backend.carried_panes(),
        vec![CarriedPtyPane {
            pane_id: kept,
            #[cfg(unix)]
            terminal_fd: None,
            pid: 4243,
            size: PANE_SIZE,
            exit: None,
        }]
    );
}

#[test]
fn spawning_a_pane_asks_the_supervisor_and_records_the_child_it_reports() {
    let pane = PaneId::new();
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Spawned { pid: 4242 });
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));

    let handle = backend
        .spawn(pane, shell_spec("sleep 30"), PANE_SIZE)
        .expect("the supervisor opens the pane");

    assert_eq!(handle.pane_id(), pane);
    assert_eq!(
        handle.try_read_output(),
        None,
        "a pane delivering through a sink carries no channels"
    );
    assert_eq!(
        backend.carried_panes(),
        vec![CarriedPtyPane {
            pane_id: pane,
            #[cfg(unix)]
            terminal_fd: None,
            pid: 4242,
            size: PANE_SIZE,
            exit: None,
        }]
    );
}

#[test]
fn a_spawn_the_supervisor_refuses_is_a_spawn_failure() {
    let pane = PaneId::new();
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::Unknown,
            message: "no terminal could be opened".to_string(),
        },
    ));
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));

    assert_eq!(
        backend
            .spawn(pane, shell_spec("sleep 30"), PANE_SIZE)
            .expect_err("a refused spawn is a failure"),
        PtyError::Spawn {
            detail: "the supervisor refused Spawn: no terminal could be opened".to_string(),
        }
    );
    assert_eq!(
        backend.carried_panes(),
        Vec::new(),
        "a refused spawn leaves no pane behind"
    );
}

#[test]
fn resizing_records_the_size_only_once_the_supervisor_took_it() {
    let pane = PaneId::new();
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Spawned { pid: 4242 });
    answers.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));
    backend
        .spawn(pane, shell_spec("sleep 30"), PANE_SIZE)
        .expect("the supervisor opens the pane");

    let wider = PtySize {
        cols: 120,
        rows: 40,
    };
    backend.resize(pane, wider).expect("the pane is retuned");

    assert_eq!(
        peer.asked().last(),
        Some(&SupervisorRequestKind::Resize {
            pane_id: pane,
            size: wider,
        })
    );
    assert_eq!(
        backend.carried_panes(),
        vec![CarriedPtyPane {
            pane_id: pane,
            #[cfg(unix)]
            terminal_fd: None,
            pid: 4242,
            size: wider,
            exit: None,
        }]
    );
}

#[test]
fn writing_to_a_pane_sends_exactly_the_bytes_it_was_given() {
    let pane = PaneId::new();
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Spawned { pid: 4242 });
    answers.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));
    backend
        .spawn(pane, shell_spec("cat"), PANE_SIZE)
        .expect("the supervisor opens the pane");

    backend
        .write(pane, b"hello\n")
        .expect("the bytes reach the supervisor");

    assert_eq!(
        peer.asked().last(),
        Some(&SupervisorRequestKind::Write {
            pane_id: pane,
            bytes: b"hello\n".to_vec(),
        })
    );
}

#[test]
fn killing_a_pane_drops_it_from_this_backend() {
    let pane = PaneId::new();
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Spawned { pid: 4242 });
    answers.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));
    backend
        .spawn(pane, shell_spec("sleep 30"), PANE_SIZE)
        .expect("the supervisor opens the pane");

    backend
        .kill(pane, KillPolicy::Tree)
        .expect("the pane is closed");

    assert_eq!(
        peer.asked().last(),
        Some(&SupervisorRequestKind::Kill {
            pane_id: pane,
            kill_policy: KillPolicy::Tree,
        })
    );
    assert_eq!(backend.carried_panes(), Vec::new());
    assert_eq!(
        backend.kill(pane, KillPolicy::Tree),
        Err(PtyError::UnknownPane { pane }),
        "a pane already closed is not closed twice"
    );
}

#[test]
fn a_pane_this_backend_does_not_drive_is_refused_before_the_link_is_touched() {
    let ghost = PaneId::new();
    let peer = FakeSupervisor::start(opening_answers(Vec::new()), Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));
    let opening = peer.asked();

    assert_eq!(
        backend.resize(ghost, PANE_SIZE),
        Err(PtyError::UnknownPane { pane: ghost })
    );
    assert_eq!(
        backend.write(ghost, b"hi"),
        Err(PtyError::UnknownPane { pane: ghost })
    );
    assert_eq!(
        backend.kill(ghost, KillPolicy::Tree),
        Err(PtyError::UnknownPane { pane: ghost })
    );
    assert_eq!(backend.live_cwd(ghost), None);
    assert_eq!(
        peer.asked(),
        opening,
        "a pane this backend does not drive costs no round trip"
    );
}

#[test]
fn a_closed_pane_is_refused_while_the_backend_still_drives_another_one() {
    // The refusal must name the pane asked for, not merely notice that this
    // backend drives something.
    let closed = PaneId::new();
    let still_open = PaneId::new();
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Spawned { pid: 4242 });
    answers.push(SupervisorResult::Spawned { pid: 4243 });
    answers.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));
    backend
        .spawn(closed, shell_spec("sleep 30"), PANE_SIZE)
        .expect("the supervisor opens the pane that is closed again");
    backend
        .spawn(still_open, shell_spec("sleep 30"), PANE_SIZE)
        .expect("the supervisor opens the pane that stays open");
    backend
        .kill(closed, KillPolicy::Tree)
        .expect("the pane is closed");
    let asked_so_far = peer.asked();

    assert_eq!(
        backend.resize(closed, PANE_SIZE),
        Err(PtyError::UnknownPane { pane: closed })
    );
    assert_eq!(
        backend.write(closed, b"hi"),
        Err(PtyError::UnknownPane { pane: closed })
    );
    assert_eq!(backend.live_cwd(closed), None);
    assert_eq!(
        peer.asked(),
        asked_so_far,
        "a pane this backend no longer drives costs no round trip"
    );
    assert_eq!(
        backend.carried_panes(),
        vec![CarriedPtyPane {
            pane_id: still_open,
            #[cfg(unix)]
            terminal_fd: None,
            pid: 4243,
            size: PANE_SIZE,
            exit: None,
        }],
        "the pane that was never closed is still driven"
    );
}

#[test]
fn output_and_exit_events_reach_the_sink_in_the_order_they_arrive() {
    let pane = PaneId::new();
    let peer = FakeSupervisor::start(
        opening_answers(Vec::new()),
        vec![
            SupervisorEvent::Output {
                pane_id: pane,
                bytes: b"first".to_vec(),
            },
            SupervisorEvent::Output {
                pane_id: pane,
                bytes: b"second".to_vec(),
            },
            SupervisorEvent::Exited {
                pane_id: pane,
                status: ExitStatus::ExitCode(0),
            },
        ],
    );
    let sink = RecordingSink::new();
    let _backend = connect_to(&peer, Arc::clone(&sink));

    wait_until("the planted exit reached the sink", || {
        !sink.exits().is_empty()
    });

    assert_eq!(
        sink.chunks(),
        vec![(pane, b"first".to_vec()), (pane, b"second".to_vec()),]
    );
    assert_eq!(sink.exits(), vec![(pane, ExitStatus::ExitCode(0))]);
}

#[test]
fn holding_the_readers_still_asks_the_supervisor_to_hold_its_pane_output() {
    // The hold reaches the supervisor over the link. The answer is the last
    // frame the link carries, and the link's one reader thread hands every
    // frame written before it to the sink first: a pause that answered leaves
    // nothing read but undelivered.
    let pane = PaneId::new();
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Spawned { pid: 4242 });
    answers.push(SupervisorResult::Done);
    answers.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start(
        answers,
        vec![SupervisorEvent::Output {
            pane_id: pane,
            bytes: b"before".to_vec(),
        }],
    );
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));
    backend
        .spawn(pane, shell_spec("sleep 30"), PANE_SIZE)
        .expect("the supervisor opens the pane");

    assert_eq!(backend.pause_readers(), Ok(()));
    assert_eq!(
        sink.chunks(),
        vec![(pane, b"before".to_vec())],
        "a pause that answered has already handed the consumer every chunk \
         written before that answer"
    );
    assert_eq!(
        backend.carried_panes(),
        vec![CarriedPtyPane {
            pane_id: pane,
            #[cfg(unix)]
            terminal_fd: None,
            pid: 4242,
            size: PANE_SIZE,
            exit: None,
        }],
        "a paused backend still names every pane the swap must carry"
    );
    backend.resume_readers();

    assert_eq!(
        peer.asked(),
        vec![
            SupervisorRequestKind::hello(ConnectionToken::new("k7QxSecret")),
            SupervisorRequestKind::ListPanes,
            SupervisorRequestKind::Spawn {
                pane_id: pane,
                spec: shell_spec("sleep 30"),
                size: PANE_SIZE,
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
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::UnsupportedKind,
            message: "PauseOutput is not a request kind this supervisor has".to_string(),
        },
    ));
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));

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
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Panes(Vec::new()));
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));

    assert_eq!(
        backend.pause_readers(),
        Err(PtyError::Io {
            detail: "the supervisor answered PauseOutput with Panes".to_string(),
        })
    );
}

#[test]
fn a_link_that_breaks_fails_the_request_in_flight() {
    // The fake supervisor runs out of answers and closes, which is what a
    // supervisor that died mid-request looks like from here.
    let pane = PaneId::new();
    let peer = FakeSupervisor::start(opening_answers(Vec::new()), Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));

    assert_eq!(
        backend
            .spawn(pane, shell_spec("sleep 30"), PANE_SIZE)
            .expect_err("a broken link fails the spawn"),
        PtyError::Spawn {
            detail: "the supervisor link closed while Spawn was in flight".to_string(),
        }
    );
}

#[test]
fn an_answer_that_does_not_fit_its_request_is_refused() {
    let pane = PaneId::new();
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));

    assert_eq!(
        backend
            .spawn(pane, shell_spec("sleep 30"), PANE_SIZE)
            .expect_err("an answer that does not fit is a failure"),
        PtyError::Io {
            detail: "the supervisor answered Spawn with Done".to_string(),
        }
    );
}

#[test]
fn shutting_the_supervisor_down_is_asked_for_once() {
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));

    backend.shut_down().expect("the supervisor is told to end");

    assert_eq!(peer.asked().last(), Some(&SupervisorRequestKind::Shutdown));
}

#[test]
fn shutting_down_over_a_link_already_broken_is_the_outcome_asked_for() {
    let peer = FakeSupervisor::start(opening_answers(Vec::new()), Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));

    assert_eq!(backend.shut_down(), Ok(()));
}

#[test]
fn a_shutdown_the_supervisor_refuses_is_the_outcome_asked_for() {
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::Unknown,
            message: "a pane could not be closed".to_string(),
        },
    ));
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));

    assert_eq!(backend.shut_down(), Ok(()));
    assert_eq!(peer.asked().last(), Some(&SupervisorRequestKind::Shutdown));
}

#[test]
fn asking_a_pane_for_its_directory_answers_what_the_supervisor_said() {
    // Every answer other than a directory leaves the pane without one.
    let pane = PaneId::new();
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Spawned { pid: 4242 });
    answers.push(SupervisorResult::Cwd(Some(PathBuf::from("/tmp/work"))));
    answers.push(SupervisorResult::Cwd(None));
    answers.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));
    backend
        .spawn(pane, shell_spec("sleep 30"), PANE_SIZE)
        .expect("the supervisor opens the pane");

    assert_eq!(
        backend.live_cwd(pane),
        Some(PathBuf::from("/tmp/work")),
        "the directory the supervisor named must reach the caller unchanged"
    );
    assert_eq!(
        peer.asked().last(),
        Some(&SupervisorRequestKind::LiveCwd { pane_id: pane }),
        "the pane asked about must be the pane named"
    );
    assert_eq!(
        backend.live_cwd(pane),
        None,
        "an operating system that cannot answer leaves the pane without a directory"
    );
    assert_eq!(
        backend.live_cwd(pane),
        None,
        "an answer that is not a directory is not a directory"
    );
}

#[test]
fn a_kill_the_supervisor_refuses_still_drops_the_pane() {
    // The pane leaves this backend whatever the supervisor answers.
    let pane = PaneId::new();
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Spawned { pid: 4242 });
    answers.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::Unknown,
            message: "the pane could not be closed".to_string(),
        },
    ));
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));
    backend
        .spawn(pane, shell_spec("sleep 30"), PANE_SIZE)
        .expect("the supervisor opens the pane");

    assert_eq!(
        backend.kill(pane, KillPolicy::Tree),
        Err(PtyError::Io {
            detail: "the supervisor refused Kill: the pane could not be closed".to_string(),
        }),
        "a refusal must reach the caller"
    );
    assert_eq!(
        backend.carried_panes(),
        Vec::new(),
        "and the pane must be gone from this backend all the same"
    );
    assert_eq!(
        backend.kill(pane, KillPolicy::Tree),
        Err(PtyError::UnknownPane { pane }),
        "so closing it again is refused without a round trip"
    );
}

#[test]
fn a_resize_the_supervisor_refuses_leaves_the_pane_at_its_old_size() {
    // A refused resize leaves the recorded size unchanged: a carried pane
    // reports the size its child was really told.
    let pane = PaneId::new();
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Spawned { pid: 4242 });
    answers.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::Unknown,
            message: "the terminal could not be retuned".to_string(),
        },
    ));
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));
    backend
        .spawn(pane, shell_spec("sleep 30"), PANE_SIZE)
        .expect("the supervisor opens the pane");

    let wider = PtySize {
        cols: 120,
        rows: 40,
    };
    assert_eq!(
        backend.resize(pane, wider),
        Err(PtyError::Io {
            detail: "the supervisor refused Resize: the terminal could not be retuned".to_string(),
        }),
        "a refusal must reach the caller"
    );
    assert_eq!(
        backend.carried_panes(),
        vec![CarriedPtyPane {
            pane_id: pane,
            #[cfg(unix)]
            terminal_fd: None,
            pid: 4242,
            size: PANE_SIZE,
            exit: None,
        }],
        "a pane whose child was never told the new size must still report the old one"
    );
}

#[test]
fn a_kill_that_asks_the_child_to_stop_waits_out_its_grace_window_as_well() {
    // A kill granting a grace window waits `ANSWER_WAIT` plus that window;
    // the supervisor spends the window before it answers.
    let pane = PaneId::new();
    let grace = Duration::from_secs(4);

    assert_eq!(
        answer_wait(&SupervisorRequestKind::Kill {
            pane_id: pane,
            kill_policy: KillPolicy::Graceful { timeout: grace },
        }),
        ANSWER_WAIT + grace
    );
    assert_eq!(
        answer_wait(&SupervisorRequestKind::Kill {
            pane_id: pane,
            kill_policy: KillPolicy::GracefulTree { timeout: grace },
        }),
        ANSWER_WAIT + grace
    );
    assert_eq!(
        answer_wait(&SupervisorRequestKind::Kill {
            pane_id: pane,
            kill_policy: KillPolicy::Force,
        }),
        ANSWER_WAIT,
        "a kill that spends no grace window waits no longer than any other request"
    );
    assert_eq!(
        answer_wait(&SupervisorRequestKind::Kill {
            pane_id: pane,
            kill_policy: KillPolicy::Tree,
        }),
        ANSWER_WAIT
    );
    assert_eq!(
        answer_wait(&SupervisorRequestKind::Write {
            pane_id: pane,
            bytes: b"hi".to_vec(),
        }),
        ANSWER_WAIT
    );
}

#[test]
fn request_ids_start_at_one_and_count_up_by_one() {
    let pane = PaneId::new();
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Spawned { pid: 4242 });
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));

    backend
        .spawn(pane, shell_spec("sleep 30"), PANE_SIZE)
        .expect("the supervisor opens the pane");

    assert_eq!(peer.asked_ids(), vec![1, 2, 3]);
}

#[test]
fn a_hello_the_supervisor_refuses_fails_the_opening() {
    let peer = FakeSupervisor::start(
        vec![SupervisorResult::Error(
            koshi_ipc::protocol::IpcErrorPayload {
                code: koshi_ipc::protocol::IpcErrorCode::BadToken,
                message: "the token does not match".to_string(),
            },
        )],
        Vec::new(),
    );
    let sink = RecordingSink::new();

    let error = SupervisorPtyBackend::connect(
        &peer.addr,
        ConnectionToken::new("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[],
    )
    .err()
    .expect("a refused Hello fails the opening");

    assert_eq!(
        error,
        PtyError::Io {
            detail: "the supervisor refused Hello: the token does not match".to_string(),
        }
    );
    assert_eq!(
        peer.asked(),
        vec![SupervisorRequestKind::hello(ConnectionToken::new(
            "k7QxSecret"
        ))],
        "nothing is asked after a refused Hello"
    );
}

#[test]
fn a_hello_answered_with_something_else_fails_the_opening() {
    let peer = FakeSupervisor::start(vec![SupervisorResult::Done], Vec::new());
    let sink = RecordingSink::new();

    let error = SupervisorPtyBackend::connect(
        &peer.addr,
        ConnectionToken::new("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[],
    )
    .err()
    .expect("an answer that does not fit fails the opening");

    assert_eq!(
        error,
        PtyError::Io {
            detail: "the supervisor answered Hello with Done".to_string(),
        }
    );
}

#[test]
fn a_pane_list_answered_with_something_else_fails_the_opening() {
    let peer = FakeSupervisor::start(
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
        &peer.addr,
        ConnectionToken::new("k7QxSecret"),
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
    let runtime_dir = tempfile::tempdir().expect("a temporary directory is created");
    let addr = link_addr(runtime_dir.path());
    let sink = RecordingSink::new();

    let error = SupervisorPtyBackend::connect(
        &addr,
        ConnectionToken::new("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[],
    )
    .err()
    .expect("an address nobody listens on fails the opening");

    let PtyError::Io { detail } = error else {
        panic!("an unreachable supervisor is an io failure, not {error:?}");
    };
    let expected_start = format!("the supervisor at {addr} could not be reached: ");
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
    let peer = FakeSupervisor::start_planted(
        opening_answers(Vec::new()),
        vec![Frame::Message(SupervisorMessage::Response(
            SupervisorResponse {
                request_id: None,
                result: SupervisorResult::Done,
            },
        ))],
    );
    let sink = RecordingSink::new();

    let error = SupervisorPtyBackend::connect(
        &peer.addr,
        ConnectionToken::new("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[],
    )
    .err()
    .expect("an answer naming no request fails the opening");

    assert_eq!(
        error,
        PtyError::Io {
            detail: "the supervisor answered request None while Hello (request 1) was in flight"
                .to_string(),
        }
    );
}

#[test]
fn an_answer_to_a_request_not_yet_sent_fails_the_request_in_flight() {
    let peer = FakeSupervisor::start_planted(
        opening_answers(Vec::new()),
        vec![Frame::Message(SupervisorMessage::Response(
            SupervisorResponse {
                request_id: Some(7),
                result: SupervisorResult::Done,
            },
        ))],
    );
    let sink = RecordingSink::new();

    let error = SupervisorPtyBackend::connect(
        &peer.addr,
        ConnectionToken::new("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[],
    )
    .err()
    .expect("an answer to a request not yet sent fails the opening");

    assert_eq!(
        error,
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
    let peer = FakeSupervisor::start_planted(
        opening_answers(Vec::new()),
        vec![Frame::Message(SupervisorMessage::Response(
            SupervisorResponse {
                request_id: Some(0),
                result: SupervisorResult::Done,
            },
        ))],
    );
    let sink = RecordingSink::new();

    let backend = connect_to(&peer, Arc::clone(&sink));

    assert_eq!(
        peer.asked(),
        vec![
            SupervisorRequestKind::hello(ConnectionToken::new("k7QxSecret")),
            SupervisorRequestKind::ListPanes,
        ]
    );
    assert_eq!(backend.carried_panes(), Vec::new());
}

#[test]
fn an_answer_this_build_has_no_name_for_is_refused() {
    let peer = FakeSupervisor::start_planted(
        Vec::new(),
        vec![Frame::UnknownAnswer {
            request_id: Some(1),
            name: "Floating".to_string(),
        }],
    );
    let sink = RecordingSink::new();

    let error = SupervisorPtyBackend::connect(
        &peer.addr,
        ConnectionToken::new("k7QxSecret"),
        Arc::clone(&sink) as Arc<dyn PtySink>,
        &[],
    )
    .err()
    .expect("an answer this build has no name for fails the opening");

    assert_eq!(
        error,
        PtyError::Io {
            detail: "the supervisor answered Hello with Floating, which this build has no name for"
                .to_string(),
        }
    );
}

#[test]
fn an_event_this_build_has_no_name_for_is_passed_over() {
    let pane = PaneId::new();
    let peer = FakeSupervisor::start_planted(
        opening_answers(Vec::new()),
        vec![
            Frame::UnknownEvent("Bell".to_string()),
            Frame::Message(SupervisorMessage::Event(SupervisorEvent::Output {
                pane_id: pane,
                bytes: b"after".to_vec(),
            })),
        ],
    );
    let sink = RecordingSink::new();

    let backend = connect_to(&peer, Arc::clone(&sink));

    wait_until("the chunk after the unknown event reached the sink", || {
        !sink.chunks().is_empty()
    });
    assert_eq!(sink.chunks(), vec![(pane, b"after".to_vec())]);
    assert_eq!(sink.exits(), Vec::new());
    assert_eq!(backend.carried_panes(), Vec::new());
}

#[test]
fn an_orphan_the_supervisor_will_not_kill_does_not_stop_the_opening() {
    let orphan = PaneId::new();
    let mut answers = opening_answers(vec![SupervisorPane {
        pane_id: orphan,
        pid: 4242,
        size: PANE_SIZE,
    }]);
    answers.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::Unknown,
            message: "the pane could not be closed".to_string(),
        },
    ));
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();

    let backend = connect_to(&peer, Arc::clone(&sink));

    assert_eq!(
        peer.asked(),
        vec![
            SupervisorRequestKind::hello(ConnectionToken::new("k7QxSecret")),
            SupervisorRequestKind::ListPanes,
            SupervisorRequestKind::Kill {
                pane_id: orphan,
                kill_policy: KillPolicy::Tree,
            },
        ]
    );
    assert_eq!(
        backend.carried_panes(),
        Vec::new(),
        "a pane nobody carried is not driven, whatever the kill answered"
    );
    assert_eq!(sink.exits(), Vec::new());
}

#[test]
fn a_write_the_supervisor_refuses_reaches_the_caller_and_keeps_the_pane() {
    let pane = PaneId::new();
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Spawned { pid: 4242 });
    answers.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::Unknown,
            message: "the child is gone".to_string(),
        },
    ));
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));
    backend
        .spawn(pane, shell_spec("cat"), PANE_SIZE)
        .expect("the supervisor opens the pane");

    assert_eq!(
        backend.write(pane, b"hello\n"),
        Err(PtyError::Io {
            detail: "the supervisor refused Write: the child is gone".to_string(),
        })
    );
    assert_eq!(
        backend.carried_panes(),
        vec![CarriedPtyPane {
            pane_id: pane,
            #[cfg(unix)]
            terminal_fd: None,
            pid: 4242,
            size: PANE_SIZE,
            exit: None,
        }],
        "a refused write leaves the pane driven"
    );
}

#[test]
fn a_write_answered_with_something_else_is_refused() {
    let pane = PaneId::new();
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Spawned { pid: 4242 });
    answers.push(SupervisorResult::Cwd(None));
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));
    backend
        .spawn(pane, shell_spec("cat"), PANE_SIZE)
        .expect("the supervisor opens the pane");

    assert_eq!(
        backend.write(pane, b"hello\n"),
        Err(PtyError::Io {
            detail: "the supervisor answered Write with Cwd".to_string(),
        })
    );
}

#[test]
fn a_kill_over_a_link_already_broken_still_drops_the_pane() {
    let pane = PaneId::new();
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Spawned { pid: 4242 });
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));
    backend
        .spawn(pane, shell_spec("sleep 30"), PANE_SIZE)
        .expect("the supervisor opens the pane");

    assert_eq!(
        backend.kill(pane, KillPolicy::Tree),
        Err(PtyError::Io {
            detail: "the supervisor link closed while Kill was in flight".to_string(),
        })
    );
    assert_eq!(backend.carried_panes(), Vec::new());
    assert_eq!(
        backend.kill(pane, KillPolicy::Tree),
        Err(PtyError::UnknownPane { pane })
    );
}

#[test]
fn a_directory_the_supervisor_refuses_to_name_is_none() {
    let pane = PaneId::new();
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Spawned { pid: 4242 });
    answers.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::Unknown,
            message: "no such pane".to_string(),
        },
    ));
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));
    backend
        .spawn(pane, shell_spec("sleep 30"), PANE_SIZE)
        .expect("the supervisor opens the pane");

    assert_eq!(backend.live_cwd(pane), None);
    assert_eq!(
        peer.asked().last(),
        Some(&SupervisorRequestKind::LiveCwd { pane_id: pane })
    );
}

#[test]
fn a_resume_the_supervisor_refuses_leaves_the_link_serving() {
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Error(
        koshi_ipc::protocol::IpcErrorPayload {
            code: koshi_ipc::protocol::IpcErrorCode::UnsupportedKind,
            message: "ResumeOutput is not a request kind this supervisor has".to_string(),
        },
    ));
    answers.push(SupervisorResult::Done);
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));

    backend.resume_readers();

    assert_eq!(backend.shut_down(), Ok(()));
    assert_eq!(
        peer.asked(),
        vec![
            SupervisorRequestKind::hello(ConnectionToken::new("k7QxSecret")),
            SupervisorRequestKind::ListPanes,
            SupervisorRequestKind::ResumeOutput,
            SupervisorRequestKind::Shutdown,
        ]
    );
}

#[test]
fn a_shutdown_answered_with_something_else_is_refused() {
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Panes(Vec::new()));
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));

    assert_eq!(
        backend.shut_down(),
        Err(PtyError::Io {
            detail: "the supervisor answered Shutdown with Panes".to_string(),
        })
    );
}

#[test]
fn flushing_the_writers_never_fails() {
    let peer = FakeSupervisor::start(opening_answers(Vec::new()), Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));
    let opening = peer.asked();

    assert_eq!(backend.flush_writers(), Ok(()));
    assert_eq!(
        peer.asked(),
        opening,
        "flushing asks the supervisor nothing"
    );
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "spawn into an already-live pane id")]
fn spawning_into_a_live_pane_id_panics_in_debug_builds() {
    let pane = PaneId::new();
    let mut answers = opening_answers(Vec::new());
    answers.push(SupervisorResult::Spawned { pid: 4242 });
    let peer = FakeSupervisor::start(answers, Vec::new());
    let sink = RecordingSink::new();
    let backend = connect_to(&peer, Arc::clone(&sink));
    backend
        .spawn(pane, shell_spec("sleep 30"), PANE_SIZE)
        .expect("the supervisor opens the pane");

    let _ = backend.spawn(pane, shell_spec("sleep 30"), PANE_SIZE);
}
