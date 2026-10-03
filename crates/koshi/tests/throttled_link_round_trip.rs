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
//! The session then gets a burst it cannot deliver: pane output, which is a
//! lossy event class the bus may drop, and thousands of tab focus changes,
//! which are the critical class it may not. The throttled client's queue fills,
//! its lossy events are dropped, the first critical event that does not fit
//! marks it desynced, and the serve loop's own render pass resyncs it with a
//! [`SessionEvent::Resync`] naming how many events went missing. Nothing in
//! this file reaches into the bus to make that happen. A focus change
//! submitted after that resync must arrive on the throttled connection
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
    Command, CommandEnvelope, CommandResult, CommandSource, FocusTabArgs, NewTabArgs, TabTarget,
};
use koshi_core::event::{Event, TabFocused};
use koshi_core::ids::{ClientId, CommandId, PaneId, TabId};
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::protocol::{IpcRequest, IpcRequestKind, IpcResponse, IpcResult};
use koshi_ipc::transport::{build_frame_halves, Connection, Deadlined, FrameReader, FrameWriter};
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

/// How long a poll pauses between attempts.
const ROUND_TRIP_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(10);

/// The span one slice of a relay pump covers.
const RELAY_SLICE_DURATION: Duration = Duration::from_millis(10);

/// Bytes per slice on the direction carrying the throttled client's own frames
/// to the session: 64 KiB, more than its Hello, Attach and commands take.
const TO_SESSION_BYTE_COUNT_PER_SLICE: usize = 64 * 1024;

/// Bytes per slice on the direction carrying the session's events to the
/// throttled client: about 100 kilobytes a second, far under what the burst
/// below produces.
const FROM_SESSION_BYTE_COUNT_PER_SLICE: usize = 1024;

/// How many pane-output chunks the burst pushes. Each one becomes a lossy
/// `PaneOutputUpdated` event.
const LOSSY_CHUNK_COUNT: usize = 400;

/// One chunk of pane output: a row of 78 `k` characters, then a new line. A row
/// of equal cells travels as one run in the picture the session composes.
const PANE_OUTPUT_CHUNK_BYTES: &[u8] =
    b"kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk\r\n";

/// How many tab focus changes the burst submits. Each one puts one critical
/// event on every subscriber's queue: several times the 1024 entries one queue
/// holds.
const CRITICAL_COMMAND_COUNT: usize = 6000;

/// One loopback stream half, as a framed connection reads or writes it.
///
/// [`Deadlined::set_deadline`] does nothing. A read on it is bounded by the
/// stream's own read timeout, when one is set.
struct LoopbackHalf(TcpStream);

impl Read for LoopbackHalf {
    fn read(&mut self, destination_bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(destination_bytes)
    }
}

impl Write for LoopbackHalf {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Deadlined for LoopbackHalf {
    fn set_deadline(&mut self, _deadline: Option<Instant>) {}
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
        // A read on this stream returns within the poll interval, and the pump
        // checks its deadline after each one.
        accepted_client_stream
            .set_read_timeout(Some(ROUND_TRIP_POLL_INTERVAL_DURATION))
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
    /// while this is held.
    client_frame_writer: FrameWriter,
}

/// Connect to the relay at `relay_socket_address`, do the Hello and the Attach
/// for `session` over it, and hand back the attached client with its stream
/// unread.
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

    client_frame_writer
        .send(&build_attach_request())
        .expect("the relay carries the attach");
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

/// Submit `command` over `connection`, enveloped the way the CLI running inside
/// `pane_id` envelops it, and hand back the dispatcher's result.
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
    let submit_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope)),
    };
    connection
        .send(&submit_request)
        .expect("the server reads the command");
    let ipc_response: IpcResponse = connection.recv().expect("the server answers the command");
    assert_eq!(ipc_response.request_id, Some(2));
    match ipc_response.answer_result {
        IpcResult::CommandResult(command_result) => command_result,
        unexpected_result => panic!("the command was answered with {unexpected_result:?}"),
    }
}

/// Attach once more over the control socket and hand back only the structure
/// the reply carried, with the connection closed again.
fn get_reattached_structure(session: &RunningSession) -> AttachedSessionStructureSnapshot {
    let mut connection = open_session_connection(&session.load_session_endpoint());
    connection
        .send(&build_attach_request())
        .expect("the server reads the attach");
    let ipc_response: IpcResponse = connection.recv().expect("the server answers the attach");
    let IpcResult::Attached {
        session_structure, ..
    } = ipc_response.answer_result
    else {
        panic!(
            "expected an attach reply, got {:?}",
            ipc_response.answer_result
        );
    };
    session_structure
}

/// Wait until the session reports exactly `tab_count` tabs.
///
/// # Panics
/// When [`THROTTLED_LINK_WAIT_DURATION`] passes first.
fn wait_for_tab_count(session: &RunningSession, tab_count: usize) {
    let deadline = Instant::now() + THROTTLED_LINK_WAIT_DURATION;
    loop {
        let session_overview = session.fetch_session_overview();
        if session_overview.tabs.len() == tab_count {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the session settled at {} tabs, not {tab_count}",
            session_overview.tabs.len()
        );
        std::thread::sleep(ROUND_TRIP_POLL_INTERVAL_DURATION);
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

    // A second tab: each focus change below moves focus between the two and
    // emits one critical event.
    let first_tab_id = session.fetch_session_overview().tabs[0].tab_id;
    let mut control_connection = open_session_connection(&session.load_session_endpoint());
    let create_tab_result = submit_session_command(
        &mut control_connection,
        &session,
        control_client.client_id,
        root_pane_id,
        &session_socket_address,
        Command::NewTab(NewTabArgs::default()),
    );
    let second_tab_id = match list_emitted_events(&create_tab_result) {
        [Event::TabCreated(created_tab), ..] => created_tab.tab_id,
        unexpected_events => {
            panic!("expected a tab to be created, got {unexpected_events:?}")
        }
    };
    assert_eq!(session.fetch_session_overview().tabs.len(), 2);

    // The lossy half of the burst: pane output the bus may drop.
    for _ in 0..LOSSY_CHUNK_COUNT {
        session
            .fake_pty_backend
            .push_output(root_pane_id, PANE_OUTPUT_CHUNK_BYTES.to_vec())
            .expect("the fake backend takes the pane's output");
    }

    // The critical half: tab focus changes, which the bus never drops
    // silently. Nothing reads the throttled client's stream while this runs:
    // its queue fills, then overflows. The control client is on the new tab:
    // the first `Next` wraps back to the first tab, and every turn after it
    // swaps the two.
    for command_index in 0..CRITICAL_COMMAND_COUNT {
        let focus_tab_result = submit_session_command(
            &mut control_connection,
            &session,
            control_client.client_id,
            root_pane_id,
            &session_socket_address,
            Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Next,
                client_id: Some(control_client.client_id),
            }),
        );
        let (focused_tab_id, previous_tab_id) = if command_index % 2 == 0 {
            (first_tab_id, second_tab_id)
        } else {
            (second_tab_id, first_tab_id)
        };
        assert_eq!(
            list_emitted_events(&focus_tab_result),
            [Event::TabFocused(TabFocused {
                client_id: control_client.client_id,
                tab_id: focused_tab_id,
                previous_tab_id,
            })]
        );
    }
    wait_for_tab_count(&session, 2);

    // Only now does the throttled client start reading. Its queue holds the
    // backlog, and the resync the serve loop owes it lands after that backlog.
    let mut session_frame_reader = throttled_client.session_frame_reader;
    let throttled_events =
        forward_session_events(move || session_frame_reader.recv::<SessionEvent>());
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

    // A focus change submitted after the resync must reach the throttled
    // client through the relay, as the exact event the dispatcher emitted.
    // Frames still in flight from the burst — and any further resync among
    // them — are read past.
    let focus_tab_result = submit_session_command(
        &mut control_connection,
        &session,
        control_client.client_id,
        root_pane_id,
        &session_socket_address,
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Next,
            client_id: Some(control_client.client_id),
        }),
    );
    let focus_event = match list_emitted_events(&focus_tab_result) {
        [Event::TabFocused(tab_focused)] => *tab_focused,
        unexpected_events => panic!("expected one focus change, got {unexpected_events:?}"),
    };
    let probe_deadline = Instant::now() + THROTTLED_LINK_WAIT_DURATION;
    loop {
        assert!(
            Instant::now() < probe_deadline,
            "the focus change submitted after the resync never reached the throttled client"
        );
        let session_event = throttled_events
            .recv_timeout(probe_deadline.saturating_duration_since(Instant::now()))
            .expect("the session keeps writing to the throttled client");
        assert!(
            Instant::now() < probe_deadline,
            "the focus change submitted after the resync never reached the throttled client"
        );
        if session_event
            == (SessionEvent::TabFocused {
                client_id: focus_event.client_id,
                tab_id: focus_event.tab_id,
                previous_tab_id: focus_event.previous_tab_id,
            })
        {
            break;
        }
    }

    // The session the throttled client caught up to is the session an
    // unthrottled client sees. Both reads are fresh attaches over the control
    // socket on the settled session, and every id in them matches.
    let settled_structure = get_reattached_structure(&session);
    let repeated_structure = get_reattached_structure(&session);
    assert_eq!(settled_structure, repeated_structure);
    assert_eq!(settled_structure.session_id, session.session_id);
    assert_eq!(
        settled_structure
            .tabs
            .iter()
            .map(|tab_discovery| tab_discovery.tab_id)
            .collect::<Vec<TabId>>(),
        vec![first_tab_id, second_tab_id]
    );

    // The control client's own stream never stopped: it read the tab creation
    // the burst opened with.
    let first_control_event = control_client
        .session_events
        .recv_timeout(THROTTLED_LINK_WAIT_DURATION)
        .expect("the control client is told the session changed");
    assert_eq!(
        first_control_event,
        SessionEvent::TabCreated {
            tab_id: second_tab_id
        }
    );
    assert_eq!(control_client.session_structure.tabs.len(), 1);

    // Closes the throttled client's writing half. It is open through every
    // assertion above.
    drop(throttled_client.client_frame_writer);
}
