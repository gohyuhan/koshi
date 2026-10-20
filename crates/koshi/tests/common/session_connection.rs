//! Speaking to a session server over its control socket, whether it runs on a
//! thread of the test process or as its own `koshi serve-session` process.
//!
//! [`open_session_connection`] connects to an endpoint file the test already
//! holds. [`wait_for_session_connection`] reads the endpoint file again until
//! the session server answers. [`attach_client_on_connection`] joins the
//! session as an attached client, and [`read_session_ending`] reads that
//! client's event stream until a frame, a read failure, or a write failure ends
//! it. [`read_next_session_event`] reads the next event on that stream and
//! reports each painted update it passes.
//! [`send_session_request`], [`submit_session_command`] and [`create_pane`]
//! ask the session for something on a connection that carries no client's
//! event stream.

use std::path::Path;
use std::sync::mpsc;
use std::time::Instant;

use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, NewPaneArgs, NewPanePlacement,
};
use koshi_core::event::Event;
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId};
use koshi_core::process::SpawnSpec;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::error::IpcError;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::protocol::{
    GraphicsCapabilities, IpcRequest, IpcRequestKind, IpcResponse, IpcResult,
};
use koshi_ipc::transport::{Connection, FrameReader, FrameWriter};
use koshi_runtime::runtime::painted_update::{PaintedUpdateReceiver, SessionFrame};
use koshi_test_support::fixtures::{build_paint_report_request, send_committed_request};

use super::{POLL_INTERVAL_DURATION, WAIT_DURATION};

/// The terminal size every attaching client reports, and the size
/// [`super::in_process_session::RunningSession::start_session`] starts its
/// session at: 80 columns by 24 rows.
pub const ATTACH_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// The opening request every connection sends: request id `1` and
/// [`IpcRequestKind::build_hello_request`] carrying the token `endpoint_file`
/// holds.
pub fn build_hello_request(endpoint_file: &EndpointFile) -> IpcRequest {
    IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::build_hello_request(endpoint_file.connection_token.clone()),
    }
}

/// The request that joins a session as an attached client: request id `2`,
/// [`ATTACH_VIEWPORT_SIZE`], no client to resume, and no graphics.
pub fn build_attach_request() -> IpcRequest {
    IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Attach {
            viewport_size: ATTACH_VIEWPORT_SIZE,
            resume_client_id: None,
            resume_token: None,
            pane_area: None,
            graphics_capabilities: GraphicsCapabilities::default(),
            cell_size: None,
            link_timeout_ms: None,
            max_frames_per_second: 60,
        },
    }
}

/// Open a connection to the socket `endpoint_file` advertises, with its
/// handshake already done.
///
/// # Panics
/// When the socket does not answer, or answers the Hello with anything but a
/// Hello.
pub fn open_session_connection(endpoint_file: &EndpointFile) -> Connection {
    let mut connection =
        Connection::connect(&endpoint_file.socket_address).expect("the socket answers");
    connection
        .send(&build_hello_request(endpoint_file))
        .expect("the server reads the Hello");
    let ipc_response: IpcResponse = connection.recv().expect("the server answers the Hello");
    match ipc_response.answer_result {
        IpcResult::Hello { .. } => connection,
        unexpected_result => panic!("the Hello was answered with {unexpected_result:?}"),
    }
}

/// Open a connection to `session_id`'s control socket in `runtime_directory`,
/// with its handshake already done, and hand back the endpoint file it was
/// advertised in. Each attempt reads the endpoint file again, and attempts
/// repeat every [`POLL_INTERVAL_DURATION`].
///
/// # Panics
/// When no attempt opens a connection within [`WAIT_DURATION`], or a Hello is
/// answered with anything but a Hello or an error.
pub fn wait_for_session_connection(
    runtime_directory: &Path,
    session_id: SessionId,
) -> (Connection, EndpointFile) {
    let wait_deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Some(opened_connection) =
            try_open_advertised_session_connection(runtime_directory, session_id)
        {
            return opened_connection;
        }
        assert!(
            Instant::now() < wait_deadline,
            "no session server answered for {session_id}"
        );
        std::thread::sleep(POLL_INTERVAL_DURATION);
    }
}

/// One attempt of [`wait_for_session_connection`]: read the endpoint file,
/// connect, and send the Hello.
///
/// `None` when the endpoint file cannot be read, the socket does not answer,
/// a frame cannot be written or read, or the Hello is refused with an error.
///
/// # Panics
/// When the Hello is answered with anything but a Hello or an error.
fn try_open_advertised_session_connection(
    runtime_directory: &Path,
    session_id: SessionId,
) -> Option<(Connection, EndpointFile)> {
    let session_endpoint = EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .ok()?;
    let mut connection = Connection::connect(&session_endpoint.socket_address).ok()?;
    connection
        .send(&build_hello_request(&session_endpoint))
        .ok()?;
    let ipc_response: IpcResponse = connection.recv().ok()?;
    match ipc_response.answer_result {
        IpcResult::Hello { .. } => Some((connection, session_endpoint)),
        IpcResult::Error(_) => None,
        unexpected_result => panic!("the Hello was answered with {unexpected_result:?}"),
    }
}

/// Send [`build_attach_request`] on `connection` as a `Prepare` and its
/// `Commit`, and hand back the client the session server minted. The
/// connection carries only that client's event stream and that client's own
/// requests afterwards.
///
/// # Panics
/// When the `Prepare` is not answered `Prepared`, or the reply to the
/// `Commit` is not `Attached` for request `2` and `session_id`.
pub fn attach_client_on_connection(connection: &mut Connection, session_id: SessionId) -> ClientId {
    send_committed_request(connection, build_attach_request());
    let ipc_response: IpcResponse = connection.recv().expect("the server answers the attach");
    assert_eq!(ipc_response.request_id, Some(2));
    let IpcResult::Attached {
        client_id,
        session_id: joined_session_id,
        ..
    } = ipc_response.answer_result
    else {
        panic!(
            "expected an attach reply, got {:?}",
            ipc_response.answer_result
        );
    };
    assert_eq!(joined_session_id, session_id);
    client_id
}

/// Ask the session for `request_kind` on a connection that carries no client's
/// event stream, as a `Prepare` and its `Commit` numbered `request_id`, and
/// hand back the answer to the `Commit`.
///
/// # Panics
/// When the `Prepare` is not answered `Prepared`, or the answer to the
/// `Commit` does not come under `request_id`.
pub fn send_session_request(
    connection: &mut Connection,
    request_id: u64,
    request_kind: IpcRequestKind,
) -> IpcResult {
    send_committed_request(
        connection,
        IpcRequest {
            request_id,
            request_kind,
        },
    );
    let ipc_response: IpcResponse = connection.recv().expect("the session answers the request");
    assert_eq!(ipc_response.request_id, Some(request_id));
    ipc_response.answer_result
}

/// Submit `command` on a control connection to `session_id`, targeting
/// `client_id`, and hand back the events it emitted. A rejected command fails
/// the test.
pub fn submit_session_command(
    connection: &mut Connection,
    session_id: SessionId,
    client_id: ClientId,
    command: Command,
) -> Vec<Event> {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_external_cli(Some(session_id), Some(client_id)),
        command,
    );
    match send_session_request(
        connection,
        7,
        IpcRequestKind::SubmitCommand(Box::new(command_envelope)),
    ) {
        IpcResult::CommandResult(CommandResult::Ok { emitted_events, .. }) => emitted_events,
        unexpected_result => panic!("the command was answered with {unexpected_result:?}"),
    }
}

/// Split a new pane off the client's focused one, running `spawn_spec`, and hand
/// back the pane the session created. `None` launches the platform shell.
pub fn create_pane(
    connection: &mut Connection,
    session_id: SessionId,
    client_id: ClientId,
    spawn_spec: Option<SpawnSpec>,
) -> PaneId {
    let emitted_events = submit_session_command(
        connection,
        session_id,
        client_id,
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: None,
                tab_id: None,
                direction: Direction::Right,
            },
            working_directory: None,
            spawn_spec,
            client_id: Some(client_id),
        }),
    );
    emitted_events
        .iter()
        .find_map(|emitted_event| match emitted_event {
            Event::PaneCreated(created_pane) => Some(created_pane.pane_id),
            _ => None,
        })
        .expect("the new pane is announced")
}

/// Read `connection`'s event stream on a thread of its own with
/// [`read_next_session_event`], passing over every event but
/// [`SessionEvent::Detached`], [`SessionEvent::Quit`], and
/// [`SessionEvent::SwitchTo`], and hand back the first of those, or the read
/// or write failure that came first.
///
/// # Panics
/// When neither arrives within [`WAIT_DURATION`], and on the reading thread
/// when a painted update part does not apply.
pub fn read_session_ending(connection: Connection) -> Result<SessionEvent, IpcError> {
    let (ending_sender, ending_receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut session_frame_reader, mut client_frame_writer) = connection.split();
        let mut painted_update_receiver = PaintedUpdateReceiver::new();
        let session_ending = loop {
            match read_next_session_event(
                &mut session_frame_reader,
                &mut client_frame_writer,
                &mut painted_update_receiver,
            ) {
                Ok(
                    ending_event @ (SessionEvent::Detached
                    | SessionEvent::Quit
                    | SessionEvent::SwitchTo { .. }),
                ) => break Ok(ending_event),
                Ok(_) => {}
                Err(stream_error) => break Err(stream_error),
            }
        };
        let _ = ending_sender.send(session_ending);
    });
    ending_receiver
        .recv_timeout(WAIT_DURATION)
        .expect("the event stream ends")
}

/// Read `session_frame_reader` until it carries a JSON event, and hand that
/// event back. `painted_update_receiver` applies each painted update part on
/// the way, and each whole painted update is reported painted on
/// `client_frame_writer`, as a client reports each update it paints.
///
/// # Errors
/// The read of `session_frame_reader` or the report write that failed first.
///
/// # Panics
/// When a painted update part does not apply.
pub fn read_next_session_event(
    session_frame_reader: &mut FrameReader,
    client_frame_writer: &mut FrameWriter,
    painted_update_receiver: &mut PaintedUpdateReceiver,
) -> Result<SessionEvent, IpcError> {
    loop {
        let received_frame = session_frame_reader.recv_message_or_binary::<SessionEvent>()?;
        match painted_update_receiver
            .accept_session_frame(received_frame)
            .expect("a painted update part that applies")
        {
            Some(SessionFrame::Event(session_event)) => return Ok(session_event),
            Some(SessionFrame::Painted(received_painted_frame)) => {
                let update_sequence = received_painted_frame.update_sequence;
                client_frame_writer.send(&build_paint_report_request(update_sequence))?;
            }
            Some(SessionFrame::ImageChunk(_)) | None => {}
        }
    }
}
