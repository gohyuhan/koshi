//! What a session does for an attached client whose link cannot carry the
//! events as fast as the session produces them.
//!
//! One session server runs on a thread of this process over a fake PTY
//! backend, started by `common::in_process_session`, and two clients attach to
//! it:
//!
//! 1. The throttled client. Its frames do not travel the control socket
//!    directly. A loopback listener in this process accepts its connection,
//!    opens its own connection to the control socket, and relays raw bytes
//!    between the two with one
//!    [`pump_throttled`](koshi_test_support::throttle::pump_throttled) per
//!    direction. The direction carrying the session's events is held to
//!    1024 bytes per 10 ms slice, which the burst below outruns.
//! 2. The control client, attached straight to the control socket with no
//!    throttle, whose reader thread drains everything the session writes.
//!
//! The session then gets a burst it cannot deliver: pane output, which reaches
//! each client in its painted frames, and thousands of pane swaps, each one a
//! [`SessionEvent::PanePlacementCommitted`] event. The throttled client's queue
//! fills. A frame that does not fit is dropped and leaves the client live. The
//! first event that does not fit marks the client desynced, and the serve loop
//! resyncs it with a [`SessionEvent::Resync`] naming how many events went
//! missing. Nothing in this file reaches into the bus to make that happen. A
//! swap submitted after that resync must arrive on the throttled connection
//! itself.
//!
//! Every wait here is bounded. A queue read waits for the time left until its
//! [`Instant`] deadline, and a session that never resyncs fails the test at that
//! deadline.

mod common;

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, MovePaneArgs, NewPaneArgs,
    NewPanePlacement,
};
use koshi_core::event::Event;
use koshi_core::geometry::Direction;
use koshi_core::ids::{ClientId, CommandId, PaneId};
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::protocol::{IpcRequest, IpcRequestKind, IpcResponse, IpcResult};
use koshi_ipc::transport::{build_frame_halves, Connection, Deadlined, FrameReader, FrameWriter};
use koshi_test_support::fixtures::{send_committed_request, send_committed_request_through_halves};
use koshi_test_support::throttle::pump_throttled;

use common::in_process_session::{
    attach_test_client, forward_session_events, list_emitted_events, RunningSession,
};
use common::session_connection::{
    build_attach_request, build_hello_request, open_session_connection,
};

/// How long a poll waits for something the session server has to do over the
/// throttled link before the test calls it a failure: 60 seconds.
const THROTTLED_LINK_WAIT_DURATION: Duration = Duration::from_secs(60);

/// How long one read on the relay's client stream waits before the pump checks
/// its deadline: 10 ms.
const RELAY_READ_TIMEOUT_DURATION: Duration = Duration::from_millis(10);

/// The span one slice of a relay pump covers.
const RELAY_SLICE_DURATION: Duration = Duration::from_millis(10);

/// Bytes per slice on the direction carrying the throttled client's own frames
/// to the session: 64 KiB, more than its Hello, Attach and commands take.
const TO_SESSION_BYTE_COUNT_PER_SLICE: usize = 64 * 1024;

/// Bytes per slice on the direction carrying the session's events to the
/// throttled client: about 100 kilobytes a second, far under what the burst
/// below produces.
const FROM_SESSION_BYTE_COUNT_PER_SLICE: usize = 1024;

/// How many pane-output chunks the burst pushes.
const PANE_OUTPUT_CHUNK_COUNT: usize = 400;

/// One chunk of pane output: a row of 78 `k` characters, then a new line. A row
/// of equal cells travels as one run in the picture the session composes.
const PANE_OUTPUT_CHUNK_BYTES: &[u8] =
    b"kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk\r\n";

/// How many pane swaps the burst submits. Each one puts one event on every
/// subscriber's queue: several times the 1024 entries one queue holds.
const PANE_SWAP_COMMAND_COUNT: usize = 6000;

/// One loopback stream half, as a framed connection reads or writes it.
///
/// [`Deadlined::set_deadline`] and [`Deadlined::set_idle_timeout`] do nothing.
/// A read on it is bounded by the stream's own read timeout, when one is set.
struct LoopbackHalf(TcpStream);

impl Read for LoopbackHalf {
    fn read(&mut self, destination_bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(destination_bytes)
    }
}

impl Write for LoopbackHalf {
    fn write(&mut self, source_bytes: &[u8]) -> io::Result<usize> {
        self.0.write(source_bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Deadlined for LoopbackHalf {
    fn set_deadline(&mut self, _deadline: Option<Instant>) {}

    fn set_idle_timeout(&mut self, _idle_timeout: Option<Duration>) {}
}

/// Start a relay listening on loopback, and return the address it listens on.
///
/// The relay accepts one connection, opens its own connection to the socket
/// `endpoint_file` advertises, and copies raw bytes between the two. The
/// direction carrying the session's events moves
/// [`FROM_SESSION_BYTE_COUNT_PER_SLICE`] per [`RELAY_SLICE_DURATION`]; the
/// direction carrying the client's own frames moves
/// [`TO_SESSION_BYTE_COUNT_PER_SLICE`]. Both pumps stop at `deadline`.
fn start_throttled_relay(endpoint_file: EndpointFile, deadline: Instant) -> String {
    let relay_listener = TcpListener::bind("127.0.0.1:0").expect("bind the relay's listener");
    let relay_socket_address = relay_listener
        .local_addr()
        .expect("read the relay's bound address")
        .to_string();
    std::thread::spawn(move || {
        let (accepted_client_stream, _) = relay_listener
            .accept()
            .expect("the relay accepts one client");
        // A read on this stream returns within `RELAY_READ_TIMEOUT_DURATION`,
        // and the pump checks its deadline after each one.
        accepted_client_stream
            .set_read_timeout(Some(RELAY_READ_TIMEOUT_DURATION))
            .expect("set the relay's read timeout");
        let client_reader_stream = accepted_client_stream
            .try_clone()
            .expect("duplicate the relay's client stream");
        let session_connection =
            Connection::connect(&endpoint_file.socket_address).expect("the socket answers");
        let (session_reader, session_writer) = session_connection.split_raw();
        pump_throttled(
            client_reader_stream,
            session_writer,
            TO_SESSION_BYTE_COUNT_PER_SLICE,
            RELAY_SLICE_DURATION,
            deadline,
        );
        pump_throttled(
            session_reader,
            accepted_client_stream,
            FROM_SESSION_BYTE_COUNT_PER_SLICE,
            RELAY_SLICE_DURATION,
            deadline,
        );
    });
    relay_socket_address
}

/// A client attached through the relay: the halves of its framed connection,
/// plus what the attach reply said.
struct ThrottledClient {
    /// The client the session minted for this connection.
    client_id: ClientId,
    /// The half the session's frames arrive on. Nothing reads it until the test
    /// hands it to [`forward_session_events`].
    session_frame_reader: FrameReader,
    /// The half this client's own frames go out on. The connection stays open
    /// while this is held: here, then on the thread of
    /// [`forward_session_events`].
    client_frame_writer: FrameWriter,
}

/// Connect to the relay at `relay_socket_address`, do the Hello and the Attach
/// for `session` over it, the Attach as a `Prepare` and its `Commit`, and hand
/// back the attached client with its stream unread.
fn attach_client_through_relay(
    session: &RunningSession,
    relay_socket_address: &str,
) -> ThrottledClient {
    let endpoint_file = session.load_session_endpoint();
    let client_relay_stream = TcpStream::connect(relay_socket_address).expect("the relay answers");
    let client_reader_half = client_relay_stream
        .try_clone()
        .expect("duplicate the client's relay stream");
    let (mut session_frame_reader, mut client_frame_writer) = build_frame_halves(
        Box::new(LoopbackHalf(client_reader_half)),
        Box::new(LoopbackHalf(client_relay_stream)),
    );

    client_frame_writer
        .send(&build_hello_request(&endpoint_file))
        .expect("the relay carries the Hello");
    let ipc_response: IpcResponse = session_frame_reader
        .recv()
        .expect("the server answers the Hello");
    let IpcResult::Hello { .. } = ipc_response.answer_result else {
        panic!(
            "the Hello was answered with {:?}",
            ipc_response.answer_result
        );
    };

    send_committed_request_through_halves(
        &mut session_frame_reader,
        &mut client_frame_writer,
        build_attach_request(),
    );
    let ipc_response: IpcResponse = session_frame_reader
        .recv()
        .expect("the server answers the attach");
    assert_eq!(ipc_response.request_id, Some(2));
    let IpcResult::Attached {
        client_id,
        session_id,
        ..
    } = ipc_response.answer_result
    else {
        panic!(
            "expected an attach reply, got {:?}",
            ipc_response.answer_result
        );
    };
    assert_eq!(session_id, session.session_id);

    ThrottledClient {
        client_id,
        session_frame_reader,
        client_frame_writer,
    }
}

/// Submit `command` over `connection` as a `Prepare` and its `Commit`,
/// enveloped the way the CLI running inside `pane_id` envelops it, and hand
/// back the dispatcher's result.
///
/// `connection` is used as is, and stays open for the next call: the control
/// socket serves requests on one connection until its peer hangs up.
fn submit_session_command(
    connection: &mut Connection,
    session: &RunningSession,
    client_id: ClientId,
    pane_id: PaneId,
    working_directory_path: &str,
    command: Command,
) -> CommandResult {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_in_session_cli(
            session.session_id,
            Some(client_id),
            pane_id,
            PathBuf::from(working_directory_path),
        ),
        command,
    );
    send_committed_request(
        connection,
        IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::SubmitCommand {
                command_envelope: Box::new(command_envelope),
                fired_action: None,
            },
        },
    );
    let ipc_response: IpcResponse = connection.recv().expect("the server answers the command");
    assert_eq!(ipc_response.request_id, Some(2));
    match ipc_response.answer_result {
        IpcResult::CommandResult(command_result) => command_result,
        unexpected_result => panic!("the command was answered with {unexpected_result:?}"),
    }
}

/// Swap `pane_id` with its neighbor in `direction` over `connection`, as the
/// CLI running inside `pane_id` on behalf of `client_id`, and hand back the
/// event the swap puts on every subscriber's queue, as a client reads it.
///
/// # Panics
/// When the swap is rejected, or commits no pane placement.
fn submit_pane_swap(
    connection: &mut Connection,
    session: &RunningSession,
    client_id: ClientId,
    pane_id: PaneId,
    working_directory_path: &str,
    direction: Direction,
) -> SessionEvent {
    let swap_result = submit_session_command(
        connection,
        session,
        client_id,
        pane_id,
        working_directory_path,
        Command::MovePane(MovePaneArgs {
            pane_id: Some(pane_id),
            direction,
        }),
    );
    let pane_placement_committed = list_emitted_events(&swap_result)
        .iter()
        .find_map(|emitted_event| match emitted_event {
            Event::PanePlacementCommitted(pane_placement_committed) => {
                Some(pane_placement_committed.clone())
            }
            _ => None,
        })
        .expect("the swap commits one pane placement");
    SessionEvent::PanePlacementCommitted {
        command_id: pane_placement_committed.command_id,
        source_pane_id: pane_placement_committed.source_pane_id,
        source_tab_id: pane_placement_committed.source_tab_id,
        destination_tab_id: pane_placement_committed.destination_tab_id,
        placement_target: pane_placement_committed.placement_target,
    }
}

/// `client_id` as the session reports its attached clients, and `None` when
/// the session no longer holds that client.
fn find_attached_client_id(session: &RunningSession, client_id: ClientId) -> Option<ClientId> {
    session
        .fetch_session_overview()
        .clients
        .iter()
        .find(|client_discovery| client_discovery.client_id == client_id)
        .map(|client_discovery| client_discovery.client_id)
}

#[test]
fn a_burst_the_throttled_link_cannot_carry_desyncs_that_client_and_resyncs_it() {
    let session = RunningSession::start_session();
    let session_socket_address = session.load_session_endpoint().socket_address;
    let root_pane_id = session.fake_pty_backend.list_spawned_pane_ids()[0];

    // Every pump stops at this instant, whether or not an assertion below ends
    // the test first.
    let relay_deadline = Instant::now() + THROTTLED_LINK_WAIT_DURATION * 3;
    let relay_socket_address =
        start_throttled_relay(session.load_session_endpoint(), relay_deadline);
    let throttled_client = attach_client_through_relay(&session, &relay_socket_address);
    let control_client = attach_test_client(&session);

    // A second pane right of the root pane: each swap below trades the two
    // places and emits one event.
    let mut control_connection = open_session_connection(&session.load_session_endpoint());
    let split_result = submit_session_command(
        &mut control_connection,
        &session,
        control_client.client_id,
        root_pane_id,
        &session_socket_address,
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: Some(root_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
            working_directory: None,
            spawn_spec: None,
            client_id: Some(control_client.client_id),
        }),
    );
    let split_pane_id = match list_emitted_events(&split_result) {
        [Event::PaneCreated(created_pane), ..] => created_pane.pane_id,
        unexpected_events => {
            panic!("expected a pane to be created, got {unexpected_events:?}")
        }
    };
    assert_eq!(
        session
            .fetch_session_overview()
            .panes
            .iter()
            .map(|pane_discovery| pane_discovery.pane_id)
            .collect::<Vec<PaneId>>(),
        vec![root_pane_id, split_pane_id]
    );

    // The first half of the burst: pane output, which reaches each client in
    // its painted frames.
    for _ in 0..PANE_OUTPUT_CHUNK_COUNT {
        session
            .fake_pty_backend
            .push_output(root_pane_id, PANE_OUTPUT_CHUNK_BYTES.to_vec())
            .expect("the fake backend takes the pane's output");
    }

    // The second half: pane swaps. Nothing reads the throttled client's
    // stream while this runs: its queue fills, then overflows. The root pane
    // starts on the left: each even turn moves it right, and each odd turn
    // moves it back.
    let first_swap_event = submit_pane_swap(
        &mut control_connection,
        &session,
        control_client.client_id,
        root_pane_id,
        &session_socket_address,
        Direction::Right,
    );
    for command_index in 1..PANE_SWAP_COMMAND_COUNT {
        let direction = if command_index % 2 == 0 {
            Direction::Right
        } else {
            Direction::Left
        };
        submit_pane_swap(
            &mut control_connection,
            &session,
            control_client.client_id,
            root_pane_id,
            &session_socket_address,
            direction,
        );
    }

    // Only now does the throttled client start reading. Its queue holds the
    // backlog, and the resync the serve loop owes it lands after that backlog.
    let throttled_events = forward_session_events(
        throttled_client.session_frame_reader,
        throttled_client.client_frame_writer,
    );
    let deadline = Instant::now() + THROTTLED_LINK_WAIT_DURATION;
    let mut received_event_count = 0;
    let dropped_event_count = loop {
        assert!(
            Instant::now() < deadline,
            "the throttled client was never resynced after reading {received_event_count} events"
        );
        let session_event = throttled_events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the session keeps writing to the throttled client");
        received_event_count += 1;
        assert!(
            Instant::now() < deadline,
            "the throttled client was never resynced after reading {received_event_count} events"
        );
        if let SessionEvent::Resync {
            dropped_event_count,
            ..
        } = session_event
        {
            break dropped_event_count;
        }
    };
    assert!(
        dropped_event_count >= 1,
        "the resync reported {dropped_event_count} missed events, not at least one"
    );

    // The resync arrived on the connection the client attached on, and the
    // session still holds that client: the throttled link was never dropped and
    // remade.
    assert_eq!(
        find_attached_client_id(&session, throttled_client.client_id),
        Some(throttled_client.client_id)
    );

    // A swap submitted after the resync must reach the throttled client
    // through the relay, as the exact event the dispatcher emitted. Frames
    // still in flight from the burst — and any further resync among them —
    // are read past. The burst made an even count of swaps, so the root pane
    // is on the left again.
    let probe_swap_event = submit_pane_swap(
        &mut control_connection,
        &session,
        control_client.client_id,
        root_pane_id,
        &session_socket_address,
        Direction::Right,
    );
    let probe_deadline = Instant::now() + THROTTLED_LINK_WAIT_DURATION;
    loop {
        assert!(
            Instant::now() < probe_deadline,
            "the swap submitted after the resync never reached the throttled client"
        );
        let session_event = throttled_events
            .recv_timeout(probe_deadline.saturating_duration_since(Instant::now()))
            .expect("the session keeps writing to the throttled client");
        assert!(
            Instant::now() < probe_deadline,
            "the swap submitted after the resync never reached the throttled client"
        );
        if session_event == probe_swap_event {
            break;
        }
    }

    // The control client's own stream never stopped: it read the first swap
    // the burst submitted.
    let first_control_event = control_client
        .session_events
        .recv_timeout(THROTTLED_LINK_WAIT_DURATION)
        .expect("the control client is told the session changed");
    assert_eq!(first_control_event, first_swap_event);
}
