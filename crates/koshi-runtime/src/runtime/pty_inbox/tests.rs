//! Tests for [`InboxSink`] and [`PaneOutputQueue`], which queue a pane's child
//! output and exit and wake the dispatcher once per empty queue; for the
//! dispatcher's turns over those queues; and for parking a pane, which marks
//! it live and records its size and a terminal engine.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::mpsc::TryRecvError;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use koshi_core::process::SpawnSpec;
use koshi_pty::backend::state::PtyBackend;
use koshi_test_support::fake_pty::FakePtyBackend;

use super::*;
use crate::runtime::driver::DispatchPassOutcome;

const TEST_PTY_SIZE: PtySize = PtySize {
    column_count: 80,
    row_count: 24,
};

/// The cutoff for an inbox event or a reader thread a test waits on. A test
/// fails once it elapses.
const INBOX_EVENT_DEADLINE_DURATION: Duration = Duration::from_secs(5);

/// The size of one chunk a terminal reader hands over at most.
const READ_CHUNK_BYTE_COUNT: usize = 8 * 1024;

/// Receive one inbox event within the deadline, and return the queue of the
/// wake it must be.
fn receive_readable_queue(event_receiver: &mpsc::Receiver<RuntimeEvent>) -> Arc<PaneOutputQueue> {
    match event_receiver.recv_timeout(INBOX_EVENT_DEADLINE_DURATION) {
        Ok(RuntimeEvent::PtyReadable { pane_output_queue }) => pane_output_queue,
        unexpected_event => panic!("expected PtyReadable, got {unexpected_event:?}"),
    }
}

/// The byte count `pane_output_queue` holds now.
fn get_queued_byte_count(pane_output_queue: &PaneOutputQueue) -> usize {
    pane_output_queue
        .queue_state
        .lock()
        .expect("pane output queue")
        .queued_byte_count
}

/// One reader thread's reports: the running count of its accepted pushes, and
/// the final count once it ends.
struct ReaderThreadReports {
    /// Receives the accepted push count after each accepted push.
    accepted_count_receiver: mpsc::Receiver<usize>,
    /// Receives the accepted push count once the thread ends.
    final_count_receiver: mpsc::Receiver<usize>,
}

impl ReaderThreadReports {
    /// The accepted push count the reader ended with. Fails the test when
    /// the reader has not ended within [`INBOX_EVENT_DEADLINE_DURATION`].
    fn wait_for_final_count(&self) -> usize {
        self.final_count_receiver
            .recv_timeout(INBOX_EVENT_DEADLINE_DURATION)
            .expect("the reader ends")
    }
}

/// Push `chunk_count` chunks of [`READ_CHUNK_BYTE_COUNT`] bytes for `pane_id`
/// through `inbox_sink` on a thread of its own, the way a pane's reader
/// does. The thread stops at the first push that returns `false`.
fn start_reader_thread(
    inbox_sink: Arc<InboxSink>,
    pane_id: PaneId,
    chunk_count: usize,
) -> ReaderThreadReports {
    let (accepted_count_sender, accepted_count_receiver) = mpsc::channel::<usize>();
    let (final_count_sender, final_count_receiver) = mpsc::channel::<usize>();
    thread::spawn(move || {
        let mut accepted_chunk_count = 0;
        for _ in 0..chunk_count {
            if !inbox_sink.accept_output_bytes(pane_id, vec![b'x'; READ_CHUNK_BYTE_COUNT]) {
                break;
            }
            accepted_chunk_count += 1;
            let _ = accepted_count_sender.send(accepted_chunk_count);
        }
        let _ = final_count_sender.send(accepted_chunk_count);
    });
    ReaderThreadReports {
        accepted_count_receiver,
        final_count_receiver,
    }
}

/// Wait until the reader thread behind `accepted_count_receiver` reports
/// `expected_accepted_count` accepted pushes.
fn wait_for_accepted_count(
    accepted_count_receiver: &mpsc::Receiver<usize>,
    expected_accepted_count: usize,
) {
    loop {
        let accepted_chunk_count = accepted_count_receiver
            .recv_timeout(INBOX_EVENT_DEADLINE_DURATION)
            .expect("the reader keeps pushing until the queue is full");
        if accepted_chunk_count == expected_accepted_count {
            return;
        }
    }
}

/// Wait until `pane_output_queue` has `reference_count_with_push` strong
/// references. A push holds one reference from the moment it finds the queue
/// until it returns. Fails the test when the count is not reached within
/// [`INBOX_EVENT_DEADLINE_DURATION`].
fn wait_for_push_holding_queue(
    pane_output_queue: &Arc<PaneOutputQueue>,
    reference_count_with_push: usize,
) {
    let push_wait_deadline = Instant::now() + INBOX_EVENT_DEADLINE_DURATION;
    while Arc::strong_count(pane_output_queue) < reference_count_with_push {
        assert!(
            Instant::now() < push_wait_deadline,
            "no push found the queue"
        );
        thread::yield_now();
    }
}

/// A runtime sharing one fake PTY backend, returned alongside it so a test can
/// spawn panes on the backend.
fn build_test_server_with_fake_pty_backend() -> (Server, Arc<FakePtyBackend>) {
    let (event_sender, inbox_receiver) = mpsc::channel();
    let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(event_sender),
    )));
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let runtime = Server::from_runtime_parts(pty_backend, inbox_receiver);
    (runtime, fake_pty_backend)
}

/// Spawn a pane under `pane_id` in the fake PTY backend.
fn spawn_test_pane(fake_pty_backend: &FakePtyBackend, pane_id: PaneId) {
    fake_pty_backend
        .spawn_pane(
            pane_id,
            SpawnSpec::build_default_shell(None, BTreeMap::new()),
            TEST_PTY_SIZE,
        )
        .expect("spawn");
}

#[test]
fn parking_a_pane_marks_it_live_and_records_its_size_and_a_terminal_engine() {
    let (mut runtime, fake_pty_backend) = build_test_server_with_fake_pty_backend();
    let pane_id = PaneId::new();
    spawn_test_pane(&fake_pty_backend, pane_id);

    runtime.park_pane_pty(pane_id, TEST_PTY_SIZE);

    assert_eq!(runtime.live_pane_ids, HashSet::from([pane_id]));
    assert_eq!(
        runtime.pty_size_by_pane_id.get(&pane_id),
        Some(&TEST_PTY_SIZE)
    );
    assert_eq!(
        runtime.terminal_engine_by_pane_id[&pane_id]
            .get_terminal_state()
            .get_active_grid()
            .get_grid_dimensions(),
        (TEST_PTY_SIZE.row_count, TEST_PTY_SIZE.column_count)
    );
}

#[test]
fn parking_records_the_size_it_is_given_not_the_spawn_size() {
    let (mut runtime, fake_pty_backend) = build_test_server_with_fake_pty_backend();
    let pane_id = PaneId::new();
    spawn_test_pane(&fake_pty_backend, pane_id);
    let parked_pty_size = PtySize {
        column_count: 100,
        row_count: 40,
    };

    runtime.park_pane_pty(pane_id, parked_pty_size);

    assert_eq!(
        runtime.pty_size_by_pane_id.get(&pane_id),
        Some(&parked_pty_size)
    );
    assert_eq!(
        runtime.terminal_engine_by_pane_id[&pane_id]
            .get_terminal_state()
            .get_active_grid()
            .get_grid_dimensions(),
        (parked_pty_size.row_count, parked_pty_size.column_count)
    );
}

#[test]
fn parking_the_same_pane_again_replaces_its_size_and_engine() {
    let (mut runtime, fake_pty_backend) = build_test_server_with_fake_pty_backend();
    let pane_id = PaneId::new();
    spawn_test_pane(&fake_pty_backend, pane_id);
    runtime.park_pane_pty(pane_id, TEST_PTY_SIZE);

    let larger_pty_size = PtySize {
        column_count: 120,
        row_count: 50,
    };
    runtime.park_pane_pty(pane_id, larger_pty_size);

    assert_eq!(runtime.live_pane_ids.len(), 1);
    assert_eq!(
        runtime.pty_size_by_pane_id.get(&pane_id),
        Some(&larger_pty_size)
    );
    assert_eq!(
        runtime.terminal_engine_by_pane_id[&pane_id]
            .get_terminal_state()
            .get_active_grid()
            .get_grid_dimensions(),
        (larger_pty_size.row_count, larger_pty_size.column_count)
    );
}

#[test]
fn two_chunks_into_an_empty_queue_send_one_wake_and_one_turn_takes_both() {
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let inbox_sink = InboxSink::from_event_sender(event_sender);
    let pane_id = PaneId::new();

    assert!(inbox_sink.accept_output_bytes(pane_id, b"first".to_vec()));
    assert!(inbox_sink.accept_output_bytes(pane_id, b"second".to_vec()));

    let pane_output_queue = receive_readable_queue(&event_receiver);
    assert_eq!(pane_output_queue.pane_id, pane_id);
    assert_eq!(event_receiver.try_recv().err(), Some(TryRecvError::Empty));
    assert_eq!(
        pane_output_queue.take_output_turn(),
        PaneOutputTurn {
            output_chunks: vec![b"first".to_vec(), b"second".to_vec()],
            exit_status: None,
            has_more_output: false,
        }
    );
}

#[test]
fn a_push_after_a_turn_empties_the_queue_sends_a_new_wake() {
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let inbox_sink = InboxSink::from_event_sender(event_sender);
    let pane_id = PaneId::new();
    assert!(inbox_sink.accept_output_bytes(pane_id, b"first".to_vec()));
    let pane_output_queue = receive_readable_queue(&event_receiver);
    let _ = pane_output_queue.take_output_turn();

    assert!(inbox_sink.accept_output_bytes(pane_id, b"second".to_vec()));

    let woken_queue = receive_readable_queue(&event_receiver);
    assert!(Arc::ptr_eq(&woken_queue, &pane_output_queue));
    assert_eq!(
        woken_queue.take_output_turn().output_chunks,
        vec![b"second".to_vec()]
    );
}

#[test]
fn an_empty_chunk_queues_nothing_and_sends_no_wake() {
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let inbox_sink = InboxSink::from_event_sender(event_sender);
    let pane_id = PaneId::new();

    assert!(inbox_sink.accept_output_bytes(pane_id, Vec::new()));

    assert_eq!(event_receiver.try_recv().err(), Some(TryRecvError::Empty));
}

#[test]
fn each_pane_gets_its_own_queue_and_its_own_wake() {
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let inbox_sink = InboxSink::from_event_sender(event_sender);
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();

    assert!(inbox_sink.accept_output_bytes(left_pane_id, b"L".to_vec()));
    assert!(inbox_sink.accept_output_bytes(right_pane_id, b"R".to_vec()));

    let left_queue = receive_readable_queue(&event_receiver);
    let right_queue = receive_readable_queue(&event_receiver);
    assert_eq!(
        (left_queue.pane_id, right_queue.pane_id),
        (left_pane_id, right_pane_id)
    );
    assert_eq!(
        left_queue.take_output_turn().output_chunks,
        vec![b"L".to_vec()]
    );
    assert_eq!(
        right_queue.take_output_turn().output_chunks,
        vec![b"R".to_vec()]
    );
}

#[test]
fn an_exit_is_taken_only_by_the_turn_that_takes_the_last_chunk() {
    // Ten 8 KiB chunks, then the exit: the first turn takes 64 KiB and leaves
    // the exit, the second takes the last two chunks and the exit.
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let inbox_sink = InboxSink::from_event_sender(event_sender);
    let pane_id = PaneId::new();
    for chunk_index in 0..10u8 {
        assert!(inbox_sink.accept_output_bytes(pane_id, vec![chunk_index; READ_CHUNK_BYTE_COUNT]));
    }
    inbox_sink.accept_exit_status(pane_id, ExitStatus::ExitCode(3));
    let pane_output_queue = receive_readable_queue(&event_receiver);

    let first_turn = pane_output_queue.take_output_turn();
    let second_turn = pane_output_queue.take_output_turn();

    assert_eq!(
        first_turn,
        PaneOutputTurn {
            output_chunks: (0..8u8)
                .map(|chunk_index| vec![chunk_index; READ_CHUNK_BYTE_COUNT])
                .collect(),
            exit_status: None,
            has_more_output: true,
        }
    );
    assert_eq!(
        second_turn,
        PaneOutputTurn {
            output_chunks: (8..10u8)
                .map(|chunk_index| vec![chunk_index; READ_CHUNK_BYTE_COUNT])
                .collect(),
            exit_status: Some(ExitStatus::ExitCode(3)),
            has_more_output: false,
        }
    );
    assert_eq!(event_receiver.try_recv().err(), Some(TryRecvError::Empty));
}

#[test]
fn a_turn_takes_a_first_chunk_larger_than_the_turn_limit_whole() {
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let inbox_sink = InboxSink::from_event_sender(event_sender);
    let pane_id = PaneId::new();
    let large_chunk = vec![b'x'; PANE_OUTPUT_TURN_BYTE_LIMIT + 1];
    assert!(inbox_sink.accept_output_bytes(pane_id, large_chunk.clone()));
    assert!(inbox_sink.accept_output_bytes(pane_id, b"next".to_vec()));
    let pane_output_queue = receive_readable_queue(&event_receiver);

    assert_eq!(
        pane_output_queue.take_output_turn(),
        PaneOutputTurn {
            output_chunks: vec![large_chunk],
            exit_status: None,
            has_more_output: true,
        }
    );
}

#[test]
fn a_child_exit_alone_sends_one_wake_and_one_turn_takes_it() {
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let inbox_sink = InboxSink::from_event_sender(event_sender);
    let pane_id = PaneId::new();

    inbox_sink.accept_exit_status(pane_id, ExitStatus::Signaled(9));

    let pane_output_queue = receive_readable_queue(&event_receiver);
    assert_eq!(
        pane_output_queue.take_output_turn(),
        PaneOutputTurn {
            output_chunks: Vec::new(),
            exit_status: Some(ExitStatus::Signaled(9)),
            has_more_output: false,
        }
    );
}

#[test]
fn a_waiting_reader_stops_at_the_queue_limit_until_a_turn_takes_bytes() {
    // 8 KiB chunks: sixteen fill the queue to exactly 128 KiB, and the
    // seventeenth waits. A turn takes 64 KiB, eight more chunks fit, and the
    // twenty-fifth waits. Closing the queue while the twenty-fifth push holds
    // it ends that push with `false`.
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let inbox_sink = Arc::new(InboxSink::from_event_sender(event_sender));
    let pane_id = PaneId::new();
    let reader_thread_reports = start_reader_thread(Arc::clone(&inbox_sink), pane_id, 128);
    let pane_output_queue = receive_readable_queue(&event_receiver);

    wait_for_accepted_count(&reader_thread_reports.accepted_count_receiver, 16);
    assert_eq!(
        get_queued_byte_count(&pane_output_queue),
        PANE_OUTPUT_QUEUE_BYTE_LIMIT
    );
    assert_eq!(
        pane_output_queue.take_output_turn(),
        PaneOutputTurn {
            output_chunks: vec![
                vec![b'x'; READ_CHUNK_BYTE_COUNT];
                PANE_OUTPUT_TURN_BYTE_LIMIT / READ_CHUNK_BYTE_COUNT
            ],
            exit_status: None,
            has_more_output: true,
        }
    );
    wait_for_accepted_count(&reader_thread_reports.accepted_count_receiver, 24);
    assert_eq!(
        get_queued_byte_count(&pane_output_queue),
        PANE_OUTPUT_QUEUE_BYTE_LIMIT
    );
    // The twenty-fifth push holds the second reference to the queue.
    wait_for_push_holding_queue(&pane_output_queue, 2);
    pane_output_queue.close_output_queue();

    assert_eq!(reader_thread_reports.wait_for_final_count(), 24);
    assert_eq!(get_queued_byte_count(&pane_output_queue), 0);
}

#[test]
fn a_sink_that_never_waits_queues_past_the_limit() {
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let inbox_sink = InboxSink::from_event_sender_without_queue_wait(event_sender);
    let pane_id = PaneId::new();

    for _ in 0..32 {
        assert!(inbox_sink.accept_output_bytes(pane_id, vec![b'x'; READ_CHUNK_BYTE_COUNT]));
    }

    let pane_output_queue = receive_readable_queue(&event_receiver);
    assert_eq!(
        get_queued_byte_count(&pane_output_queue),
        32 * READ_CHUNK_BYTE_COUNT
    );
}

#[test]
fn a_push_after_the_queue_closed_starts_a_new_queue_and_the_closed_one_stays_empty() {
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let inbox_sink = InboxSink::from_event_sender(event_sender);
    let pane_id = PaneId::new();
    assert!(inbox_sink.accept_output_bytes(pane_id, b"first".to_vec()));
    let closed_queue = receive_readable_queue(&event_receiver);

    closed_queue.close_output_queue();

    assert!(inbox_sink.accept_output_bytes(pane_id, b"second".to_vec()));
    let new_queue = receive_readable_queue(&event_receiver);
    assert!(!Arc::ptr_eq(&new_queue, &closed_queue));
    assert_eq!(
        closed_queue.take_output_turn(),
        PaneOutputTurn {
            output_chunks: Vec::new(),
            exit_status: None,
            has_more_output: false,
        }
    );
    assert_eq!(
        new_queue.take_output_turn().output_chunks,
        vec![b"second".to_vec()]
    );
}

#[test]
fn a_dropped_queue_leaves_the_registry_when_the_next_queue_starts() {
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let inbox_sink = InboxSink::from_event_sender(event_sender);
    let dropped_pane_id = PaneId::new();
    let next_pane_id = PaneId::new();
    assert!(inbox_sink.accept_output_bytes(dropped_pane_id, b"first".to_vec()));
    let dropped_queue = receive_readable_queue(&event_receiver);
    let _ = dropped_queue.take_output_turn();
    drop(dropped_queue);

    assert!(inbox_sink.accept_output_bytes(next_pane_id, b"second".to_vec()));

    assert_eq!(
        inbox_sink
            .output_queue_by_pane_id
            .lock()
            .expect("pane output queues")
            .keys()
            .copied()
            .collect::<Vec<PaneId>>(),
        vec![next_pane_id]
    );
}

#[test]
fn a_sink_reports_a_closed_inbox_so_the_reader_can_stop() {
    // The reader thread stops reading a pane the moment `accept_output_bytes`
    // answers `false`. A runtime that has gone away answers exactly that.
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let inbox_sink = InboxSink::from_event_sender(event_sender);
    let pane_id = PaneId::new();
    drop(event_receiver);

    assert!(!inbox_sink.accept_output_bytes(pane_id, b"nobody home".to_vec()));
    assert!(!inbox_sink.accept_output_bytes(pane_id, b"still nobody".to_vec()));
    // The exit half has no way to report a closed inbox. It does not panic.
    inbox_sink.accept_exit_status(pane_id, ExitStatus::ExitCode(0));
}

/// A server and a sink sharing one fake PTY backend, with `pane_count` panes
/// spawned and parked. The sink never waits: a push queues past
/// [`PANE_OUTPUT_QUEUE_BYTE_LIMIT`] with no dispatcher running.
fn build_server_with_parked_panes(
    pane_count: usize,
) -> (Server, Arc<InboxSink>, Arc<FakePtyBackend>, Vec<PaneId>) {
    let (event_sender, inbox_receiver) = mpsc::channel();
    let inbox_sink = Arc::new(InboxSink::from_event_sender_without_queue_wait(
        event_sender,
    ));
    let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(inbox_sink.clone()));
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let mut server = Server::from_runtime_parts(pty_backend, inbox_receiver);
    let pane_ids: Vec<PaneId> = (0..pane_count).map(|_| PaneId::new()).collect();
    for pane_id in &pane_ids {
        spawn_test_pane(&fake_pty_backend, *pane_id);
        server.park_pane_pty(*pane_id, TEST_PTY_SIZE);
    }
    (server, inbox_sink, fake_pty_backend, pane_ids)
}

/// A server and a sink whose pushes into a full queue wait, sharing one fake
/// PTY backend, with one 80x24 session seeded. Returns the server, the sink,
/// the backend and the session's one pane.
fn build_server_with_seeded_session() -> (Server, Arc<InboxSink>, Arc<FakePtyBackend>, PaneId) {
    let (event_sender, inbox_receiver) = mpsc::channel();
    let inbox_sink = Arc::new(InboxSink::from_event_sender(event_sender));
    let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(inbox_sink.clone()));
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let mut server = Server::from_runtime_parts(pty_backend, inbox_receiver);
    server
        .bootstrap_session(
            koshi_core::ids::SessionId::new(),
            "quiet-lake".to_string(),
            koshi_core::geometry::Size {
                column_count: 80,
                row_count: 24,
            },
            std::time::SystemTime::now(),
            None,
        )
        .expect("the session is seeded");
    let pane_id = fake_pty_backend.list_spawned_pane_ids()[0];
    (server, inbox_sink, fake_pty_backend, pane_id)
}

/// The text of row `row_index` on `pane_id`'s active grid, trailing blanks
/// removed.
fn read_pane_row_text(server: &Server, pane_id: PaneId, row_index: u16) -> String {
    let active_grid = server.terminal_engine_by_pane_id[&pane_id]
        .get_terminal_state()
        .get_active_grid();
    (0..TEST_PTY_SIZE.column_count)
        .map(|column_index| {
            active_grid
                .get_cell(row_index, column_index)
                .expect("cell in bounds")
                .get_character()
        })
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// 1 MiB of plain text: lines of `x` with no escape byte.
fn build_one_mebibyte_of_output() -> Vec<u8> {
    let mut output_bytes = Vec::with_capacity(1024 * 1024);
    while output_bytes.len() < 1024 * 1024 {
        output_bytes.extend_from_slice(&[b'x'; 63]);
        output_bytes.push(b'\n');
    }
    output_bytes
}

/// Queue `output_bytes` for `pane_id` in [`READ_CHUNK_BYTE_COUNT`] chunks.
fn queue_output_in_chunks(inbox_sink: &InboxSink, pane_id: PaneId, output_bytes: &[u8]) {
    for output_chunk in output_bytes.chunks(READ_CHUNK_BYTE_COUNT) {
        assert!(inbox_sink.accept_output_bytes(pane_id, output_chunk.to_vec()));
    }
}

#[test]
fn a_quiet_pane_is_parsed_after_one_turn_of_a_pane_holding_one_mebibyte() {
    let (mut server, inbox_sink, _fake_pty_backend, pane_ids) = build_server_with_parked_panes(2);
    let (busy_pane_id, quiet_pane_id) = (pane_ids[0], pane_ids[1]);
    queue_output_in_chunks(&inbox_sink, busy_pane_id, &build_one_mebibyte_of_output());
    assert!(inbox_sink.accept_output_bytes(quiet_pane_id, b"marker".to_vec()));

    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);
    assert_eq!(read_pane_row_text(&server, quiet_pane_id, 0), "");
    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);

    assert_eq!(read_pane_row_text(&server, quiet_pane_id, 0), "marker");
    assert_eq!(
        get_queued_byte_count(&server.pane_output_queue_by_pane_id[&busy_pane_id]),
        1024 * 1024 - PANE_OUTPUT_TURN_BYTE_LIMIT
    );
}

#[test]
fn a_wake_that_arrives_after_a_turn_is_served_after_one_more_turn_of_the_busy_pane() {
    let (mut server, inbox_sink, _fake_pty_backend, pane_ids) = build_server_with_parked_panes(2);
    let (busy_pane_id, quiet_pane_id) = (pane_ids[0], pane_ids[1]);
    queue_output_in_chunks(&inbox_sink, busy_pane_id, &build_one_mebibyte_of_output());
    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);

    assert!(inbox_sink.accept_output_bytes(quiet_pane_id, b"marker".to_vec()));
    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);
    assert_eq!(read_pane_row_text(&server, quiet_pane_id, 0), "");
    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);

    assert_eq!(read_pane_row_text(&server, quiet_pane_id, 0), "marker");
    assert_eq!(
        get_queued_byte_count(&server.pane_output_queue_by_pane_id[&busy_pane_id]),
        1024 * 1024 - 2 * PANE_OUTPUT_TURN_BYTE_LIMIT
    );
}

#[test]
fn a_control_event_behind_queued_output_is_handled_before_the_first_turn_ends() {
    let (mut server, inbox_sink, _fake_pty_backend, pane_ids) = build_server_with_parked_panes(1);
    let busy_pane_id = pane_ids[0];
    queue_output_in_chunks(&inbox_sink, busy_pane_id, &build_one_mebibyte_of_output());
    let (overview_sender, overview_receiver) = mpsc::channel();
    inbox_sink
        .inbox_sender
        .send(RuntimeEvent::IpcDiscovery {
            response_sender: overview_sender,
        })
        .expect("the inbox is open");

    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);

    assert_eq!(overview_receiver.try_recv(), Ok(None));
    assert_eq!(
        get_queued_byte_count(&server.pane_output_queue_by_pane_id[&busy_pane_id]),
        1024 * 1024 - PANE_OUTPUT_TURN_BYTE_LIMIT
    );
}

#[test]
fn a_pass_with_queued_output_gives_exactly_one_turn() {
    let (mut server, inbox_sink, _fake_pty_backend, pane_ids) = build_server_with_parked_panes(1);
    let busy_pane_id = pane_ids[0];
    queue_output_in_chunks(&inbox_sink, busy_pane_id, &build_one_mebibyte_of_output());

    for pass_index in 1..=16 {
        assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);
        let expected_queued_byte_count = if pass_index == 16 {
            0
        } else {
            1024 * 1024 - pass_index * PANE_OUTPUT_TURN_BYTE_LIMIT
        };
        assert_eq!(
            get_queued_byte_count(&server.pane_output_queue_by_pane_id[&busy_pane_id]),
            expected_queued_byte_count
        );
    }
    assert!(!server.has_queued_pane_output());
}

#[test]
fn closing_a_pane_while_its_reader_waits_ends_the_push_with_false() {
    let (mut server, inbox_sink, _fake_pty_backend, pane_id) = build_server_with_seeded_session();
    let reader_thread_reports = start_reader_thread(Arc::clone(&inbox_sink), pane_id, 128);
    wait_for_accepted_count(&reader_thread_reports.accepted_count_receiver, 16);
    let readable_event = server
        .get_inbox_receiver()
        .recv_timeout(INBOX_EVENT_DEADLINE_DURATION)
        .expect("the queue wakes the inbox");
    assert!(server.handle_runtime_event(readable_event).is_continue());
    // The server and this test hold two references to the queue, and the
    // seventeenth push holds the third.
    let pane_output_queue = Arc::clone(&server.pane_output_queue_by_pane_id[&pane_id]);
    wait_for_push_holding_queue(&pane_output_queue, 3);
    drop(pane_output_queue);

    let child_exit_events = server.handle_child_exit(pane_id, ExitStatus::ExitCode(0));
    server.publish_events(&child_exit_events);

    assert_eq!(reader_thread_reports.wait_for_final_count(), 16);
    assert!(!server.pane_output_queue_by_pane_id.contains_key(&pane_id));
    assert!(inbox_sink
        .output_queue_by_pane_id
        .lock()
        .expect("pane output queues")
        .is_empty());
}

#[test]
fn a_removed_pane_stays_in_the_turn_rotation_until_its_turn_passes_it_over() {
    let (mut server, inbox_sink, _fake_pty_backend, pane_id) = build_server_with_seeded_session();
    assert!(inbox_sink.accept_output_bytes(pane_id, b"late".to_vec()));
    let readable_event = server
        .get_inbox_receiver()
        .recv_timeout(INBOX_EVENT_DEADLINE_DURATION)
        .expect("the queue wakes the inbox");
    assert!(server.handle_runtime_event(readable_event).is_continue());

    let child_exit_events = server.handle_child_exit(pane_id, ExitStatus::ExitCode(0));
    server.publish_events(&child_exit_events);

    assert_eq!(server.ready_output_pane_ids, VecDeque::from([pane_id]));
    assert!(server.has_queued_pane_output());
    assert_eq!(server.serve_output_turn(), None);
    assert!(!server.has_queued_pane_output());
}

#[test]
fn a_wake_for_a_pane_that_is_not_live_closes_its_queue() {
    let (mut server, inbox_sink, _fake_pty_backend, _pane_ids) = build_server_with_parked_panes(0);
    let unknown_pane_id = PaneId::new();
    assert!(inbox_sink.accept_output_bytes(unknown_pane_id, b"late".to_vec()));
    let late_queue = inbox_sink
        .output_queue_by_pane_id
        .lock()
        .expect("pane output queues")[&unknown_pane_id]
        .upgrade()
        .expect("the wake in the inbox holds the queue");

    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);

    assert!(!server.has_queued_pane_output());
    assert_eq!(
        late_queue.take_output_turn(),
        PaneOutputTurn {
            output_chunks: Vec::new(),
            exit_status: None,
            has_more_output: false,
        }
    );
    assert!(!inbox_sink
        .output_queue_by_pane_id
        .lock()
        .expect("pane output queues")
        .contains_key(&unknown_pane_id));
}

/// A fake backend whose first write to any pane also queues `echo_bytes` as
/// the output of `echo_pane_id`, through `inbox_sink`. Every call goes on to
/// `fake_pty_backend`.
struct EchoingPtyBackend {
    /// The backend every call goes to.
    fake_pty_backend: FakePtyBackend,
    /// The sink the echo is queued on.
    inbox_sink: Arc<InboxSink>,
    /// The pane the echo is queued for.
    echo_pane_id: PaneId,
    /// The output queued at the first write, taken by that write.
    echo_bytes: Mutex<Option<Vec<u8>>>,
}

impl PtyBackend for EchoingPtyBackend {
    fn spawn_pane(
        &self,
        pane_id: PaneId,
        spawn_spec: SpawnSpec,
        pty_size: PtySize,
    ) -> Result<(), koshi_pty::error::PtyError> {
        self.fake_pty_backend
            .spawn_pane(pane_id, spawn_spec, pty_size)
    }

    fn resize_pane(
        &self,
        pane_id: PaneId,
        pty_size: PtySize,
    ) -> Result<(), koshi_pty::error::PtyError> {
        self.fake_pty_backend.resize_pane(pane_id, pty_size)
    }

    /// Queues the echo on the first call, then writes.
    fn write_pane_input(
        &self,
        pane_id: PaneId,
        input_bytes: &[u8],
    ) -> Result<(), koshi_pty::error::PtyError> {
        if let Some(echo_bytes) = self.echo_bytes.lock().expect("echo bytes").take() {
            assert!(self
                .inbox_sink
                .accept_output_bytes(self.echo_pane_id, echo_bytes));
        }
        self.fake_pty_backend.write_pane_input(pane_id, input_bytes)
    }

    fn kill_pane(
        &self,
        pane_id: PaneId,
        kill_policy: koshi_core::process::KillPolicy,
    ) -> Result<(), koshi_pty::error::PtyError> {
        self.fake_pty_backend.kill_pane(pane_id, kill_policy)
    }

    fn find_live_working_directory(&self, pane_id: PaneId) -> Option<std::path::PathBuf> {
        self.fake_pty_backend.find_live_working_directory(pane_id)
    }
}

#[test]
fn a_wake_that_arrives_during_a_turn_is_served_before_the_busy_pane_again() {
    // The busy pane's first chunk asks for its status (`CSI 5 n`). Writing
    // the answer queues the quiet pane's output, so its wake arrives during
    // the busy pane's turn. The next pass serves the quiet pane.
    let (event_sender, inbox_receiver) = mpsc::channel();
    let inbox_sink = Arc::new(InboxSink::from_event_sender_without_queue_wait(
        event_sender,
    ));
    let busy_pane_id = PaneId::new();
    let quiet_pane_id = PaneId::new();
    let echoing_pty_backend = Arc::new(EchoingPtyBackend {
        fake_pty_backend: FakePtyBackend::with_pty_sink(inbox_sink.clone()),
        inbox_sink: Arc::clone(&inbox_sink),
        echo_pane_id: quiet_pane_id,
        echo_bytes: Mutex::new(Some(b"marker".to_vec())),
    });
    let pty_backend: Arc<dyn PtyBackend> = echoing_pty_backend.clone();
    let mut server = Server::from_runtime_parts(pty_backend, inbox_receiver);
    for pane_id in [busy_pane_id, quiet_pane_id] {
        echoing_pty_backend
            .spawn_pane(
                pane_id,
                SpawnSpec::build_default_shell(None, BTreeMap::new()),
                TEST_PTY_SIZE,
            )
            .expect("spawn");
        server.park_pane_pty(pane_id, TEST_PTY_SIZE);
    }
    let mut busy_output_bytes = b"\x1b[5n".to_vec();
    busy_output_bytes.extend_from_slice(&build_one_mebibyte_of_output());
    queue_output_in_chunks(&inbox_sink, busy_pane_id, &busy_output_bytes);

    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);
    assert_eq!(read_pane_row_text(&server, quiet_pane_id, 0), "");
    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);

    assert_eq!(read_pane_row_text(&server, quiet_pane_id, 0), "marker");
    assert_eq!(
        get_queued_byte_count(&server.pane_output_queue_by_pane_id[&busy_pane_id]),
        busy_output_bytes.len() - PANE_OUTPUT_TURN_BYTE_LIMIT
    );
}

#[test]
fn a_quit_event_behind_queued_output_ends_the_pass_before_any_turn() {
    let (mut server, inbox_sink, _fake_pty_backend, pane_ids) = build_server_with_parked_panes(1);
    let busy_pane_id = pane_ids[0];
    queue_output_in_chunks(&inbox_sink, busy_pane_id, &build_one_mebibyte_of_output());
    inbox_sink
        .inbox_sender
        .send(RuntimeEvent::Quit)
        .expect("the inbox is open");

    assert_eq!(
        server.run_dispatch_pass(),
        DispatchPassOutcome::QuitEventHandled
    );

    assert_eq!(
        get_queued_byte_count(&server.pane_output_queue_by_pane_id[&busy_pane_id]),
        1024 * 1024
    );
}

#[test]
fn a_push_while_the_queue_still_holds_output_after_a_turn_sends_no_wake() {
    let (event_sender, event_receiver) = mpsc::channel::<RuntimeEvent>();
    let inbox_sink = InboxSink::from_event_sender(event_sender);
    let pane_id = PaneId::new();
    for _ in 0..9 {
        assert!(inbox_sink.accept_output_bytes(pane_id, vec![b'x'; READ_CHUNK_BYTE_COUNT]));
    }
    let pane_output_queue = receive_readable_queue(&event_receiver);
    assert_eq!(
        pane_output_queue.take_output_turn(),
        PaneOutputTurn {
            output_chunks: vec![
                vec![b'x'; READ_CHUNK_BYTE_COUNT];
                PANE_OUTPUT_TURN_BYTE_LIMIT / READ_CHUNK_BYTE_COUNT
            ],
            exit_status: None,
            has_more_output: true,
        }
    );

    assert!(inbox_sink.accept_output_bytes(pane_id, b"more".to_vec()));

    assert_eq!(event_receiver.try_recv().err(), Some(TryRecvError::Empty));
}

#[test]
fn an_exit_behind_queued_output_is_handled_after_every_chunk_is_parsed() {
    // The child asks for its status (`CSI 5 n`), then exits. One turn parses
    // the question, writes `CSI 0 n` back, then handles the exit, which
    // removes the session's only pane.
    let (mut server, inbox_sink, fake_pty_backend, pane_id) = build_server_with_seeded_session();
    assert!(inbox_sink.accept_output_bytes(pane_id, b"\x1b[5n".to_vec()));
    inbox_sink.accept_exit_status(pane_id, ExitStatus::ExitCode(3));

    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("the pane was spawned"),
        vec![b"\x1b[0n".to_vec()]
    );
    assert!(!server.has_active_panes());
    assert!(!server.pane_output_queue_by_pane_id.contains_key(&pane_id));
}

#[test]
fn a_turn_reports_every_byte_it_parsed_to_the_backend_once() {
    let (mut server, inbox_sink, fake_pty_backend, pane_ids) = build_server_with_parked_panes(1);
    let pane_id = pane_ids[0];
    for output_chunk in [vec![b'a'; 10], vec![b'b'; 20], vec![b'c'; 30]] {
        assert!(inbox_sink.accept_output_bytes(pane_id, output_chunk));
    }

    assert_eq!(server.run_dispatch_pass(), DispatchPassOutcome::Served);

    assert_eq!(
        fake_pty_backend
            .list_pane_output_grant_byte_counts(pane_id)
            .expect("the pane was spawned"),
        vec![60]
    );
}
