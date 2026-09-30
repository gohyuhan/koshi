//! Tests for the control-socket server over real sockets: serving lifecycle,
//! handshake gating, fault containment per connection, the reply path from a
//! stand-in dispatcher thread, and what an attached connection's reading half
//! carries.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::SystemTime;

use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, ToggleLockModeArgs,
};
use koshi_core::discovery::{SessionDiscovery, SessionOverview};
use koshi_core::geometry::{PixelCellSize, Size};
use koshi_core::ids::{CommandId, PaneId, SessionId, TabId};
use koshi_core::key::{
    BindingModifierFlags, Key, KeyChord, KeyEventKind, KeyIdentity, KeyInput, KeyModifierFlags,
};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::layout::SessionLayout;
use koshi_ipc::protocol::{
    GraphicsCapabilities, IpcRequest, WireMouseAction, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use koshi_layout::mode::LayoutMode;
use koshi_renderer::snapshot::{
    ClientSnapshot, CursorSnapshot, ImagePlacementSnapshot, PaneSnapshot, RenderSnapshot,
    ScrollbackMetadata, SessionSnapshot, TabSnapshot,
};
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDisplay, ImageRecord,
};
use koshi_test_support::fixtures::build_key_input_for_chord;

use crate::runtime::event::{AttachAccepted, EndingNotice, SessionEnding};

use super::*;

/// The terminal size every attaching client in these tests reports.
const CLIENT_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// The secret every stand-in attach mints as the resume token.
const MINTED_CONNECTION_TOKEN: &str =
    "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";

/// A fresh directory to stand in for the runtime directory:
/// `/tmp/koshi-serve-<process id>-<directory_tag>` on Unix, and the same name
/// under the temporary directory on Windows. `/tmp` keeps the Unix socket path
/// inside the OS path-length cap. [`IpcServer::start`] creates it private
/// itself.
fn build_test_runtime_directory(directory_tag: &str) -> PathBuf {
    #[cfg(unix)]
    let base_directory = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base_directory = std::env::temp_dir();
    base_directory.join(format!(
        "koshi-serve-{}-{directory_tag}",
        std::process::id()
    ))
}

/// Remove a directory a test made, and everything inside it. A directory that
/// is already gone is left alone.
fn remove_test_directory(runtime_directory: &Path) {
    let _ = std::fs::remove_dir_all(runtime_directory);
}

/// A fresh directory to stand in for the machine-wide shared directory:
/// `/tmp/koshi-shared-<process id>-<directory_tag>` on Unix, and the same name
/// under the temporary directory on Windows. [`IpcServer::start`] creates it
/// and this user's directory inside it.
fn build_test_shared_directory(directory_tag: &str) -> PathBuf {
    #[cfg(unix)]
    let base_directory = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base_directory = std::env::temp_dir();
    base_directory.join(format!(
        "koshi-shared-{}-{directory_tag}",
        std::process::id()
    ))
}

/// A stand-in for the dispatcher thread: drains the inbox, answers every
/// submitted command with `Ok` echoing its id, and every discovery request
/// with `session_overview`. Exits when every inbox sender is gone.
fn spawn_answering_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
    session_overview: Option<SessionOverview>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while let Ok(runtime_event) = inbox_receiver.recv() {
            match runtime_event {
                RuntimeEvent::Ipc {
                    command_envelope,
                    response_sender,
                } => {
                    let _ = response_sender.send(CommandResult::Ok {
                        command_id: command_envelope.command_id,
                        emitted_events: Vec::new(),
                    });
                }
                RuntimeEvent::IpcDiscovery { response_sender } => {
                    let _ = response_sender.send(session_overview.clone());
                }
                _ => {}
            }
        }
    })
}

/// The structure a stand-in attach answers with: the session, named, with
/// nothing in it.
fn build_attached_structure(session_id: SessionId) -> AttachedSessionStructureSnapshot {
    AttachedSessionStructureSnapshot {
        session_id,
        session_name: "attachable".to_string(),
        tabs: Vec::new(),
    }
}

/// One image-bearing frame for an attached stream.
fn build_image_snapshot(client_id: ClientId, image_record: Arc<ImageRecord>) -> RenderSnapshot {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    RenderSnapshot {
        is_recovery_notice_visible: false,
        session_snapshot: SessionSnapshot {
            session_id,
            session_revision: 0,
            session_name: String::from("session"),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: String::from("tab"),
                pane_slots: Vec::new(),
                tab_size: CLIENT_VIEWPORT_SIZE,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                is_every_pane_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata: Vec::new(),
        },
        pane_snapshots: vec![PaneSnapshot {
            pane_id,
            pane_title: None,
            cursor_snapshot: CursorSnapshot {
                row_index: 0,
                column_index: 0,
                is_visible: false,
                is_blinking: false,
                shape: None,
            },
            terminal_grid_view: None,
            image_placement_snapshots: vec![ImagePlacementSnapshot::with_content_id(
                7,
                1,
                image_record,
                (0, 0),
                1,
                1,
            )
            .expect("the test image placement is valid")],
            is_reverse_video: false,
            mouse_tracking: MouseTracking::Off,
            is_alternate_scroll_enabled: false,
            is_on_alternate_screen: false,
            view_top_row_index: 0,
            selection_spans: None,
            has_selection: false,
            scrollback_metadata: ScrollbackMetadata {
                retained_line_count: 0,
            },
        }],
        client_snapshot: ClientSnapshot {
            client_id,
            client_revision: 0,
            viewport_size: CLIENT_VIEWPORT_SIZE,
            active_tab_id: tab_id,
            focused_pane_id: Some(pane_id),
            lock_mode: LockMode::Normal,
            is_mouse_selection_enabled: false,
        },
    }
}

/// One one-pixel image whose red byte is `red_byte`: `[red_byte, 0, 0, 255]`.
fn build_image_record(red_byte: u8) -> Arc<ImageRecord> {
    Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: (DecodedImage {
            pixel_width: 1,
            pixel_height: 1,
            rgba_bytes: vec![red_byte, 0, 0, 255],
        })
        .into(),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay::default(),
        anchor: (0, 0),
    })
}

/// A stand-in dispatcher that answers the first attach as `client_id` in
/// `session_id`, and returns the sender of the delivery queue that attach
/// streams.
fn spawn_frame_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
    client_id: ClientId,
    session_id: SessionId,
) -> (JoinHandle<()>, Sender<Delivery>) {
    let (delivery_sender, delivery_receiver) = mpsc::channel();
    let dispatcher_thread = std::thread::spawn(move || {
        answer_first_attach(
            &inbox_receiver,
            client_id,
            session_id,
            delivery_receiver,
            Arc::new(EndingNotice::default()),
        );
    });
    (dispatcher_thread, delivery_sender)
}

/// A stand-in dispatcher that accepts attaches: it answers every attach as
/// `client_id`, holds the delivery queue it hands out open, and closes those
/// queues on a detach. The writing thread of each attached connection stays
/// blocked until then. Every other event it drains is forwarded to the
/// returned receiver. Exits when every inbox sender is gone.
fn spawn_attaching_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
    client_id: ClientId,
    session_id: SessionId,
) -> (JoinHandle<()>, Receiver<RuntimeEvent>) {
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let dispatcher_thread = std::thread::spawn(move || {
        let mut delivery_senders = Vec::new();
        let ending_notice = Arc::new(EndingNotice::default());
        while let Ok(runtime_event) = inbox_receiver.recv() {
            match runtime_event {
                RuntimeEvent::IpcAttach {
                    response_sender, ..
                } => {
                    let (delivery_sender, delivery_receiver) = mpsc::channel();
                    delivery_senders.push(delivery_sender);
                    let _ = response_sender.send(Some(AttachAccepted {
                        client_id,
                        session_id,
                        session_structure: build_attached_structure(session_id),
                        deliveries: delivery_receiver,
                        ending_notice: Arc::clone(&ending_notice),
                        resume_token: ConnectionToken::from_secret(MINTED_CONNECTION_TOKEN),
                        pane_area: None,
                    }));
                }
                detached_event @ RuntimeEvent::ClientDetached { .. } => {
                    delivery_senders.clear();
                    if runtime_event_sender.send(detached_event).is_err() {
                        break;
                    }
                }
                forwarded_runtime_event => {
                    if runtime_event_sender.send(forwarded_runtime_event).is_err() {
                        break;
                    }
                }
            }
        }
    });
    (dispatcher_thread, runtime_event_receiver)
}

/// A stand-in dispatcher that answers the first attach with
/// `delivery_receiver` and `ending_notice`, and drops everything else it
/// drains. Exits when every inbox sender is gone.
fn spawn_ending_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
    client_id: ClientId,
    session_id: SessionId,
    delivery_receiver: Receiver<Delivery>,
    ending_notice: Arc<EndingNotice>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        answer_first_attach(
            &inbox_receiver,
            client_id,
            session_id,
            delivery_receiver,
            ending_notice,
        );
    })
}

/// Drain `inbox_receiver` until every inbox sender is gone. The first attach
/// is answered as `client_id` in `session_id`, streaming `delivery_receiver`
/// under `ending_notice`. Every later attach and every other event is dropped.
fn answer_first_attach(
    inbox_receiver: &Receiver<RuntimeEvent>,
    client_id: ClientId,
    session_id: SessionId,
    delivery_receiver: Receiver<Delivery>,
    ending_notice: Arc<EndingNotice>,
) {
    let mut unanswered_delivery_receiver = Some(delivery_receiver);
    while let Ok(runtime_event) = inbox_receiver.recv() {
        let RuntimeEvent::IpcAttach {
            response_sender, ..
        } = runtime_event
        else {
            continue;
        };
        let Some(delivery_receiver) = unanswered_delivery_receiver.take() else {
            continue;
        };
        let _ = response_sender.send(Some(AttachAccepted {
            client_id,
            session_id,
            session_structure: build_attached_structure(session_id),
            deliveries: delivery_receiver,
            ending_notice: Arc::clone(&ending_notice),
            resume_token: ConnectionToken::from_secret(MINTED_CONNECTION_TOKEN),
            pane_area: None,
        }));
    }
}

/// Wait up to 5 seconds until no client writing thread is left on
/// `ending_notice`, and return how many are left.
fn wait_for_running_writer_count(ending_notice: &EndingNotice) -> usize {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while ending_notice.count_running_writers() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    ending_notice.count_running_writers()
}

/// Serve a socket whose stand-in dispatcher answers the first attach with
/// `delivery_receiver` under `ending_notice`, attach one client, and return
/// the first frame that client reads. Asserts that the client's writing thread
/// ends after that frame.
fn read_first_frame_of_ending_session(
    directory_tag: &str,
    delivery_receiver: Receiver<Delivery>,
    ending_notice: &Arc<EndingNotice>,
) -> SessionEvent {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory(directory_tag);
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let dispatcher_thread = spawn_ending_dispatcher(
        inbox_receiver,
        client_id,
        session_id,
        delivery_receiver,
        Arc::clone(ending_notice),
    );
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None)
        .expect("start serving");
    let mut connection = attach_test_client(&runtime_directory, session_id, client_id);

    let first_frame = connection
        .recv::<SessionEvent>()
        .expect("the client is told");
    assert_eq!(
        wait_for_running_writer_count(ending_notice),
        0,
        "the writing thread must end once the client holds its last frame"
    );

    drop(connection);
    ipc_server.shutdown();
    dispatcher_thread.join().expect("dispatcher exits");
    remove_test_directory(&runtime_directory);
    first_frame
}

#[test]
fn a_client_whose_queue_is_full_is_still_told_the_session_is_ending() {
    // The queue holds its full 1024 deliveries, and the ending event finds no
    // room on it. The first frame the client reads is the ending frame.
    use koshi_core::event::{Event, QuitCause, TabCreated};

    use crate::runtime::bus::{EventBus, SUBSCRIBER_QUEUE_CAPACITY};

    for (ending_event, session_ending, ending_frame, directory_tag) in [
        (
            Event::Restarting,
            SessionEnding::Restarting,
            SessionEvent::Restarting,
            "restart-full-queue",
        ),
        (
            Event::Quit(QuitCause::Requested),
            SessionEnding::Quit,
            SessionEvent::Quit,
            "quit-full-queue",
        ),
    ] {
        let mut event_bus = EventBus::new();
        let (_, delivery_receiver) = event_bus.subscribe();
        let tab_id = TabId::new();
        for _ in 0..SUBSCRIBER_QUEUE_CAPACITY {
            event_bus.publish(&Event::TabCreated(TabCreated { tab_id }));
        }
        let ending_notice = Arc::clone(event_bus.get_ending_notice());
        event_bus.publish(&ending_event);
        assert_eq!(ending_notice.get_session_ending(), Some(session_ending));

        assert_eq!(
            read_first_frame_of_ending_session(directory_tag, delivery_receiver, &ending_notice),
            ending_frame
        );
    }
}

#[test]
fn a_client_the_server_detached_reads_its_own_goodbye_when_the_session_ends() {
    // With `auto-close-session`, the last client's detach ends the session
    // while that client's queue still holds a frame. The detach closes the
    // queue behind that frame, and the quit follows. The client reads the
    // detach.
    use koshi_core::event::{Event, QuitCause, TabCreated};

    use crate::runtime::bus::EventBus;

    let mut event_bus = EventBus::new();
    let (subscriber_id, delivery_receiver) = event_bus.subscribe();
    event_bus.publish(&Event::TabCreated(TabCreated {
        tab_id: TabId::new(),
    }));
    event_bus.unsubscribe(subscriber_id);
    let ending_notice = Arc::clone(event_bus.get_ending_notice());
    event_bus.publish(&Event::Quit(QuitCause::Requested));
    assert_eq!(
        ending_notice.get_session_ending(),
        Some(SessionEnding::Quit)
    );

    assert_eq!(
        read_first_frame_of_ending_session("detach-then-quit", delivery_receiver, &ending_notice),
        SessionEvent::Detached
    );
}

#[test]
fn a_client_reads_the_quit_frame_alone_when_the_events_that_ended_the_session_are_still_queued() {
    // The pane's exit is queued ahead of the quit, and the notice is raised.
    // The client reads the quit alone.
    use koshi_core::event::{Event, PaneProcessExited, QuitCause};

    use crate::runtime::bus::EventBus;

    let mut event_bus = EventBus::new();
    let (_, delivery_receiver) = event_bus.subscribe();
    event_bus.publish(&Event::PaneProcessExited(PaneProcessExited {
        pane_id: PaneId::new(),
        exit_code: Some(0),
        signal: None,
    }));
    let ending_notice = Arc::clone(event_bus.get_ending_notice());
    event_bus.publish(&Event::Quit(QuitCause::Requested));
    assert_eq!(
        ending_notice.get_session_ending(),
        Some(SessionEnding::Quit)
    );

    assert_eq!(
        read_first_frame_of_ending_session("quit-behind-queue", delivery_receiver, &ending_notice),
        SessionEvent::Quit
    );
}

/// A served socket whose stand-in dispatcher accepts an attach as `client_id`,
/// plus the events that attached connection sends the dispatcher.
fn start_attachable_test_server(
    directory_tag: &str,
    client_id: ClientId,
) -> (
    IpcServer,
    SessionId,
    PathBuf,
    JoinHandle<()>,
    Receiver<RuntimeEvent>,
) {
    let runtime_directory = build_test_runtime_directory(directory_tag);
    let session_id = SessionId::new();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (dispatcher_thread, received_runtime_events) =
        spawn_attaching_dispatcher(inbox_receiver, client_id, session_id);
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None)
        .expect("start serving");
    (
        ipc_server,
        session_id,
        runtime_directory,
        dispatcher_thread,
        received_runtime_events,
    )
}

/// Open a connection, say hello, attach on it, and read both replies back.
/// The connection comes back carrying `client_id`'s stream.
fn attach_test_client(
    runtime_directory: &Path,
    session_id: SessionId,
    client_id: ClientId,
) -> Connection {
    attach_test_client_with_graphics(
        runtime_directory,
        session_id,
        client_id,
        GraphicsCapabilities::default(),
    )
}

/// Open and attach one connection that reports `graphics_capabilities`.
fn attach_test_client_with_graphics(
    runtime_directory: &Path,
    session_id: SessionId,
    client_id: ClientId,
    graphics_capabilities: GraphicsCapabilities,
) -> Connection {
    let mut connection = connect_to_session_socket(runtime_directory, session_id);
    connection
        .send(&build_hello_request(runtime_directory, session_id))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Attach {
                viewport_size: CLIENT_VIEWPORT_SIZE,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities,
                cell_size: None,
            },
        })
        .expect("send attach");
    let attach_reply: IpcResponse = connection.recv().expect("attach reply");
    assert_eq!(attach_reply.request_id, Some(2));
    assert_eq!(
        attach_reply.answer_result,
        IpcResult::Attached {
            client_id,
            session_id,
            session_structure: build_attached_structure(session_id),
            resume_token: Some(ConnectionToken::from_secret(MINTED_CONNECTION_TOKEN)),
            pane_area: None,
        },
    );
    connection
}

#[test]
fn an_attach_forwards_its_initial_cell_measurement_before_the_session_reply() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory("attach-cell-size");
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None)
        .expect("start serving");
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);
    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    let _: IpcResponse = connection.recv().expect("hello reply");
    let measured_cell_size =
        PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions");
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Attach {
                viewport_size: CLIENT_VIEWPORT_SIZE,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities: GraphicsCapabilities::default(),
                cell_size: Some(measured_cell_size),
            },
        })
        .expect("send attach");

    let RuntimeEvent::IpcAttach {
        cell_size,
        response_sender,
        ..
    } = inbox_receiver
        .recv()
        .expect("attach reaches the dispatcher")
    else {
        panic!("expected IpcAttach");
    };
    assert_eq!(cell_size, Some(measured_cell_size));
    let (delivery_sender, delivery_receiver) = mpsc::channel();
    response_sender
        .send(Some(AttachAccepted {
            client_id,
            session_id,
            session_structure: build_attached_structure(session_id),
            deliveries: delivery_receiver,
            ending_notice: Arc::new(EndingNotice::default()),
            resume_token: ConnectionToken::from_secret(MINTED_CONNECTION_TOKEN),
            pane_area: None,
        }))
        .expect("the dispatcher receives the accepted attach");
    let _: IpcResponse = connection.recv().expect("attach reply");

    connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Resize {
                viewport_size: CLIENT_VIEWPORT_SIZE,
                pane_area: None,
                cell_size: Some(measured_cell_size),
            },
        })
        .expect("send resize");
    let RuntimeEvent::Resize { cell_size, .. } = inbox_receiver
        .recv()
        .expect("resize reaches the dispatcher")
    else {
        panic!("expected Resize");
    };
    assert_eq!(cell_size, Some(measured_cell_size));

    drop(delivery_sender);
    drop(connection);
    ipc_server.shutdown();
    remove_test_directory(&runtime_directory);
}

/// The graphics report of a terminal that speaks the Kitty image protocol and
/// no other.
const KITTY_GRAPHICS_CAPABILITIES: GraphicsCapabilities = GraphicsCapabilities {
    supports_kitty: true,
    supports_iterm: false,
    supports_sixel: false,
};

/// One client attached to a served socket, with the sender of the delivery
/// queue that client's connection streams.
struct AttachedFrameStream {
    ipc_server: IpcServer,
    dispatcher_thread: JoinHandle<()>,
    runtime_directory: PathBuf,
    client_id: ClientId,
    connection: Connection,
    delivery_sender: Sender<Delivery>,
}

impl AttachedFrameStream {
    /// Serve a socket in a runtime directory named by `directory_tag`, and
    /// attach one client that reports `graphics_capabilities`.
    fn attach_frame_stream(
        directory_tag: &str,
        graphics_capabilities: GraphicsCapabilities,
    ) -> Self {
        let runtime_directory = build_test_runtime_directory(directory_tag);
        let session_id = SessionId::new();
        let client_id = ClientId::new();
        let (inbox_sender, inbox_receiver) = mpsc::channel();
        let (dispatcher_thread, delivery_sender) =
            spawn_frame_dispatcher(inbox_receiver, client_id, session_id);
        let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None)
            .expect("start serving");
        let connection = attach_test_client_with_graphics(
            &runtime_directory,
            session_id,
            client_id,
            graphics_capabilities,
        );
        Self {
            ipc_server,
            dispatcher_thread,
            runtime_directory,
            client_id,
            connection,
            delivery_sender,
        }
    }

    /// Queue `delivery` on the attached client's stream.
    fn send_delivery(&self, delivery: Delivery) {
        self.delivery_sender
            .send(delivery)
            .expect("the writing thread holds the delivery queue");
    }

    /// Read the next frame the attached client receives.
    fn read_session_event(&mut self) -> SessionEvent {
        self.connection
            .recv::<SessionEvent>()
            .expect("read the next frame")
    }

    /// Close the connection and the queue, stop the server, join the
    /// dispatcher, and remove the runtime directory.
    fn close_frame_stream(self) {
        drop(self.connection);
        drop(self.delivery_sender);
        self.ipc_server.shutdown();
        self.dispatcher_thread.join().expect("dispatcher exits");
        remove_test_directory(&self.runtime_directory);
    }
}

/// The `ImageContentStart` and `ImageContentChunk` frames that carry
/// `image_record` under `image_content_id`, in one chunk.
fn build_image_transfer_events(
    image_content_id: u64,
    image_record: &ImageRecord,
) -> [SessionEvent; 2] {
    [
        SessionEvent::ImageContentStart {
            image_transfer: build_wire_image_transfer(image_content_id, image_record),
        },
        SessionEvent::ImageContentChunk {
            image_chunk: FrameImageChunk {
                image_transfer_id: image_content_id,
                byte_offset: 0,
                is_last: true,
                chunk_bytes: image_record.image.rgba_bytes.clone(),
            },
        },
    ]
}

#[test]
fn an_unsupported_terminal_receives_placement_geometry_and_no_pixel_events() {
    let mut frame_stream = AttachedFrameStream::attach_frame_stream(
        "unsupported-image-stream",
        GraphicsCapabilities::default(),
    );
    let render_snapshot = build_image_snapshot(frame_stream.client_id, build_image_record(1));
    frame_stream.send_delivery(Delivery::Frame(Box::new(render_snapshot.clone())));
    frame_stream.send_delivery(Delivery::HostWrite(vec![9]));

    let mut expected_painted_frame = build_wire_frame(&render_snapshot);
    let expected_image_placement =
        &mut expected_painted_frame.pane_snapshots[0].image_placement_snapshots[0];
    expected_image_placement.image_record = None;
    expected_image_placement.is_available = false;
    assert_eq!(
        frame_stream.read_session_event(),
        SessionEvent::Painted {
            frame: Box::new(expected_painted_frame),
        }
    );
    assert_eq!(
        frame_stream.read_session_event(),
        SessionEvent::HostWrite {
            host_output_bytes: vec![9]
        }
    );

    frame_stream.close_frame_stream();
}

#[test]
fn a_kitty_terminal_receives_pixels_once_then_placement_only_frames() {
    let mut frame_stream = AttachedFrameStream::attach_frame_stream(
        "cached-image-stream",
        KITTY_GRAPHICS_CAPABILITIES,
    );
    let image_record = build_image_record(1);
    let render_snapshot = build_image_snapshot(frame_stream.client_id, Arc::clone(&image_record));
    frame_stream.send_delivery(Delivery::Frame(Box::new(render_snapshot.clone())));
    frame_stream.send_delivery(Delivery::Frame(Box::new(render_snapshot.clone())));
    frame_stream.send_delivery(Delivery::HostWrite(vec![9]));

    let painted_frame_event = SessionEvent::Painted {
        frame: Box::new(build_wire_frame(&render_snapshot)),
    };
    let [image_start_event, image_chunk_event] = build_image_transfer_events(1, &image_record);
    assert_eq!(frame_stream.read_session_event(), painted_frame_event);
    assert_eq!(frame_stream.read_session_event(), image_start_event);
    assert_eq!(frame_stream.read_session_event(), image_chunk_event);
    assert_eq!(frame_stream.read_session_event(), painted_frame_event);
    assert_eq!(
        frame_stream.read_session_event(),
        SessionEvent::HostWrite {
            host_output_bytes: vec![9]
        }
    );

    frame_stream.close_frame_stream();
}

#[test]
fn image_scroll_return_uses_a_new_identity_and_complete_transfer() {
    let mut frame_stream = AttachedFrameStream::attach_frame_stream(
        "image-scroll-return",
        KITTY_GRAPHICS_CAPABILITIES,
    );
    let first_image_record = build_image_record(1);
    let visible_image_snapshot =
        build_image_snapshot(frame_stream.client_id, Arc::clone(&first_image_record));
    let mut absent_image_snapshot = visible_image_snapshot.clone();
    absent_image_snapshot.pane_snapshots[0]
        .image_placement_snapshots
        .clear();
    let changed_image_record = build_image_record(2);
    let mut changed_image_snapshot = visible_image_snapshot.clone();
    changed_image_snapshot.pane_snapshots[0].image_placement_snapshots[0] =
        ImagePlacementSnapshot::with_content_id(
            7,
            1,
            Arc::clone(&changed_image_record),
            (0, 0),
            1,
            1,
        )
        .expect("the changed placement is valid");
    for render_snapshot in [
        &visible_image_snapshot,
        &absent_image_snapshot,
        &visible_image_snapshot,
        &absent_image_snapshot,
        &changed_image_snapshot,
    ] {
        frame_stream.send_delivery(Delivery::Frame(Box::new(render_snapshot.clone())));
    }
    frame_stream.send_delivery(Delivery::HostWrite(vec![9]));

    for (render_snapshot, image_content_id, image_record) in [
        (&visible_image_snapshot, 1, Some(&first_image_record)),
        (&absent_image_snapshot, 0, None),
        (&visible_image_snapshot, 2, Some(&first_image_record)),
        (&absent_image_snapshot, 0, None),
        (&changed_image_snapshot, 3, Some(&changed_image_record)),
    ] {
        let mut expected_painted_frame = build_wire_frame(render_snapshot);
        if let Some(expected_image_placement) = expected_painted_frame
            .pane_snapshots
            .first_mut()
            .and_then(|pane_snapshot| pane_snapshot.image_placement_snapshots.first_mut())
        {
            expected_image_placement.image_content_id = image_content_id;
        }
        assert_eq!(
            frame_stream.read_session_event(),
            SessionEvent::Painted {
                frame: Box::new(expected_painted_frame),
            }
        );
        let Some(image_record) = image_record else {
            continue;
        };
        let [image_start_event, image_chunk_event] =
            build_image_transfer_events(image_content_id, image_record);
        assert_eq!(frame_stream.read_session_event(), image_start_event);
        assert_eq!(frame_stream.read_session_event(), image_chunk_event);
    }
    assert_eq!(
        frame_stream.read_session_event(),
        SessionEvent::HostWrite {
            host_output_bytes: vec![9]
        }
    );

    frame_stream.close_frame_stream();
}

#[test]
fn any_native_terminal_receives_image_content() {
    for (directory_tag, graphics_capabilities) in [
        (
            "iterm-image-stream",
            GraphicsCapabilities {
                supports_kitty: false,
                supports_iterm: true,
                supports_sixel: false,
            },
        ),
        (
            "sixel-image-stream",
            GraphicsCapabilities {
                supports_kitty: false,
                supports_iterm: false,
                supports_sixel: true,
            },
        ),
    ] {
        let mut frame_stream =
            AttachedFrameStream::attach_frame_stream(directory_tag, graphics_capabilities);
        let image_record = build_image_record(1);
        let render_snapshot =
            build_image_snapshot(frame_stream.client_id, Arc::clone(&image_record));
        frame_stream.send_delivery(Delivery::Frame(Box::new(render_snapshot.clone())));

        let [image_start_event, image_chunk_event] = build_image_transfer_events(1, &image_record);
        assert_eq!(
            frame_stream.read_session_event(),
            SessionEvent::Painted {
                frame: Box::new(build_wire_frame(&render_snapshot)),
            }
        );
        assert_eq!(frame_stream.read_session_event(), image_start_event);
        assert_eq!(frame_stream.read_session_event(), image_chunk_event);

        frame_stream.close_frame_stream();
    }
}

#[test]
fn two_placements_of_one_record_share_one_content_transfer() {
    let image_record = build_image_record(1);
    let mut render_snapshot = build_image_snapshot(ClientId::new(), Arc::clone(&image_record));
    render_snapshot.pane_snapshots[0]
        .image_placement_snapshots
        .push(
            ImagePlacementSnapshot::with_content_id(8, 2, Arc::clone(&image_record), (0, 1), 1, 1)
                .expect("the second placement is valid"),
        );
    let mut image_cache = ConnectionImageCache::new();

    let prepared_image_frame = image_cache.prepare_image_frame(&render_snapshot);

    assert_eq!(prepared_image_frame.image_uploads.len(), 1);
    assert_eq!(prepared_image_frame.image_uploads[0].0, 1);
    assert!(Arc::ptr_eq(
        &prepared_image_frame.image_uploads[0].1,
        &image_record
    ));
    assert_eq!(
        list_painted_image_content_ids(&prepared_image_frame.painted_frame),
        vec![1, 1]
    );
}

/// The content identity of every image placement in `painted_frame`, pane by
/// pane.
fn list_painted_image_content_ids(painted_frame: &PaintedFrame) -> Vec<u64> {
    painted_frame
        .pane_snapshots
        .iter()
        .flat_map(|pane_snapshot| &pane_snapshot.image_placement_snapshots)
        .map(|image_placement| image_placement.image_content_id)
        .collect()
}

#[test]
fn a_kitty_terminal_receives_more_than_4096_placements_in_bounded_batches() {
    let mut frame_stream = AttachedFrameStream::attach_frame_stream(
        "bounded-image-batches",
        KITTY_GRAPHICS_CAPABILITIES,
    );
    let build_red_image_record =
        |image_content_id: u64| build_image_record((image_content_id % 251) as u8);
    let mut render_snapshot = build_image_snapshot(frame_stream.client_id, build_image_record(1));
    render_snapshot.pane_snapshots[0].image_placement_snapshots = (1
        ..=MAX_FRAME_IMAGE_TRANSFER_COUNT)
        .map(|placement_index| {
            let image_content_id =
                u64::try_from(placement_index).expect("the content identity fits");
            ImagePlacementSnapshot::with_content_id(
                image_content_id,
                image_content_id,
                build_red_image_record(image_content_id),
                (0, 0),
                1,
                1,
            )
            .expect("the image placement is valid")
        })
        .collect();
    let mut second_pane_snapshot = render_snapshot.pane_snapshots[0].clone();
    second_pane_snapshot.pane_id = PaneId::new();
    let last_image_content_id =
        u64::try_from(MAX_FRAME_IMAGE_TRANSFER_COUNT + 1).expect("the content identity fits");
    second_pane_snapshot.image_placement_snapshots = vec![ImagePlacementSnapshot::with_content_id(
        1,
        last_image_content_id,
        build_red_image_record(last_image_content_id),
        (0, 0),
        1,
        1,
    )
    .expect("the image placement is valid")];
    render_snapshot.pane_snapshots.push(second_pane_snapshot);
    frame_stream.send_delivery(Delivery::Frame(Box::new(render_snapshot.clone())));
    frame_stream.send_delivery(Delivery::HostWrite(vec![9]));
    let painted_frame_event = SessionEvent::Painted {
        frame: Box::new(build_wire_frame(&render_snapshot)),
    };

    assert_eq!(frame_stream.read_session_event(), painted_frame_event);
    for image_transfer_index in 1..=MAX_FRAME_IMAGE_TRANSFER_COUNT {
        let image_content_id =
            u64::try_from(image_transfer_index).expect("the content identity fits");
        let [image_start_event, image_chunk_event] = build_image_transfer_events(
            image_content_id,
            &build_red_image_record(image_content_id),
        );
        assert_eq!(frame_stream.read_session_event(), image_start_event);
        assert_eq!(frame_stream.read_session_event(), image_chunk_event);
    }
    assert_eq!(frame_stream.read_session_event(), painted_frame_event);
    let [last_image_start_event, last_image_chunk_event] = build_image_transfer_events(
        last_image_content_id,
        &build_red_image_record(last_image_content_id),
    );
    assert_eq!(frame_stream.read_session_event(), last_image_start_event);
    assert_eq!(frame_stream.read_session_event(), last_image_chunk_event);
    assert_eq!(
        frame_stream.read_session_event(),
        SessionEvent::HostWrite {
            host_output_bytes: vec![9]
        }
    );

    frame_stream.close_frame_stream();
}

#[test]
fn replacing_one_placement_record_assigns_a_new_content_identity() {
    let first_image_record = build_image_record(1);
    let initial_image_snapshot =
        build_image_snapshot(ClientId::new(), Arc::clone(&first_image_record));
    let mut replacement_image_snapshot = initial_image_snapshot.clone();
    let pane_id = replacement_image_snapshot.pane_snapshots[0].pane_id;
    let second_image_record = build_image_record(2);
    replacement_image_snapshot.pane_snapshots[0].image_placement_snapshots[0] =
        ImagePlacementSnapshot::with_content_id(
            7,
            1,
            Arc::clone(&second_image_record),
            (0, 0),
            1,
            1,
        )
        .expect("the replacement placement is valid");
    let mut image_cache = ConnectionImageCache::new();

    let initial_prepared_frame = image_cache.prepare_image_frame(&initial_image_snapshot);
    let replacement_prepared_frame = image_cache.prepare_image_frame(&replacement_image_snapshot);

    assert_eq!(
        list_painted_image_content_ids(&initial_prepared_frame.painted_frame),
        vec![1]
    );
    assert_eq!(initial_prepared_frame.image_uploads.len(), 1);
    assert_eq!(initial_prepared_frame.image_uploads[0].0, 1);
    assert!(Arc::ptr_eq(
        &initial_prepared_frame.image_uploads[0].1,
        &first_image_record
    ));
    assert_eq!(
        replacement_prepared_frame.painted_frame.pane_snapshots[0].pane_id,
        pane_id
    );
    assert_eq!(
        list_painted_image_content_ids(&replacement_prepared_frame.painted_frame),
        vec![2]
    );
    assert_eq!(replacement_prepared_frame.image_uploads.len(), 1);
    assert_eq!(replacement_prepared_frame.image_uploads[0].0, 2);
    assert!(Arc::ptr_eq(
        &replacement_prepared_frame.image_uploads[0].1,
        &second_image_record
    ));
}

#[test]
fn exhausted_content_id_space_resets_the_connection_cache_before_reuse() {
    let initial_image_snapshot = build_image_snapshot(ClientId::new(), build_image_record(1));
    let mut replacement_image_snapshot = initial_image_snapshot.clone();
    replacement_image_snapshot.pane_snapshots[0].image_placement_snapshots[0] =
        ImagePlacementSnapshot::with_content_id(7, 1, build_image_record(2), (0, 0), 1, 1)
            .expect("the replacement placement is valid");
    let mut image_cache = ConnectionImageCache::new();
    image_cache.next_image_content_id = u64::MAX;

    let prepared_frame_before_reset = image_cache.prepare_image_frame(&initial_image_snapshot);
    let prepared_frame_after_reset = image_cache.prepare_image_frame(&replacement_image_snapshot);

    assert!(!prepared_frame_before_reset.should_reset_image_cache);
    assert_eq!(
        list_painted_image_content_ids(&prepared_frame_before_reset.painted_frame),
        vec![u64::MAX]
    );
    assert!(prepared_frame_after_reset.should_reset_image_cache);
    assert_eq!(
        list_painted_image_content_ids(&prepared_frame_after_reset.painted_frame),
        vec![1]
    );
    assert_eq!(image_cache.next_image_content_id, 2);
}

#[test]
fn clearing_after_a_write_failure_resets_the_client_before_reusing_content_ids() {
    let render_snapshot = build_image_snapshot(ClientId::new(), build_image_record(1));
    let mut image_cache = ConnectionImageCache::new();

    let prepared_frame_before_clear = image_cache.prepare_image_frame(&render_snapshot);
    image_cache.clear_image_cache();
    let prepared_frame_after_clear = image_cache.prepare_image_frame(&render_snapshot);

    assert!(!prepared_frame_before_clear.should_reset_image_cache);
    assert_eq!(
        list_painted_image_content_ids(&prepared_frame_before_clear.painted_frame),
        vec![1]
    );
    assert!(prepared_frame_after_clear.should_reset_image_cache);
    assert_eq!(
        list_painted_image_content_ids(&prepared_frame_after_clear.painted_frame),
        vec![1]
    );
    assert_eq!(prepared_frame_after_clear.image_uploads.len(), 1);
    assert_eq!(prepared_frame_after_clear.image_uploads[0].0, 1);
}

/// A served socket in a fresh runtime directory, with a stand-in dispatcher
/// answering discovery with `session_overview`. Returns the server, its
/// session id, the runtime directory, and the dispatcher thread.
fn start_test_server(
    directory_tag: &str,
    session_overview: Option<SessionOverview>,
) -> (IpcServer, SessionId, PathBuf, JoinHandle<()>) {
    let runtime_directory = build_test_runtime_directory(directory_tag);
    let session_id = SessionId::new();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let dispatcher_thread = spawn_answering_dispatcher(inbox_receiver, session_overview);
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None)
        .expect("start serving");
    (ipc_server, session_id, runtime_directory, dispatcher_thread)
}

/// A served socket the other local users of this machine may reach, in a fresh
/// shared directory, with a stand-in dispatcher and the `allow-other-users`
/// setting reading `is_other_user_access_enabled`.
fn start_shared_test_server(
    directory_tag: &str,
    is_other_user_access_enabled: bool,
) -> (IpcServer, SessionId, PathBuf, PathBuf, JoinHandle<()>) {
    let runtime_directory = build_test_runtime_directory(directory_tag);
    let shared_directory = build_test_shared_directory(directory_tag);
    let session_id = SessionId::new();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let dispatcher_thread = spawn_answering_dispatcher(inbox_receiver, None);
    let ipc_server = IpcServer::start(
        &runtime_directory,
        session_id,
        inbox_sender,
        Some(OtherUsers {
            shared_directory: shared_directory.clone(),
            is_enabled: Arc::new(move || is_other_user_access_enabled),
        }),
    )
    .expect("start serving");
    (
        ipc_server,
        session_id,
        runtime_directory,
        shared_directory,
        dispatcher_thread,
    )
}

/// A stand-in dispatcher that reports every submitted command's envelope on the
/// returned receiver before answering it with `Ok`. Every other event it drains
/// is dropped. Exits when every inbox sender is gone.
fn spawn_reporting_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
) -> (JoinHandle<()>, Receiver<CommandEnvelope>) {
    let (command_envelope_sender, command_envelope_receiver) = mpsc::channel();
    let dispatcher_thread = std::thread::spawn(move || {
        while let Ok(runtime_event) = inbox_receiver.recv() {
            if let RuntimeEvent::Ipc {
                command_envelope,
                response_sender,
            } = runtime_event
            {
                let _ = response_sender.send(CommandResult::Ok {
                    command_id: command_envelope.command_id,
                    emitted_events: Vec::new(),
                });
                if command_envelope_sender.send(*command_envelope).is_err() {
                    break;
                }
            }
        }
    });
    (dispatcher_thread, command_envelope_receiver)
}

/// A served socket in a fresh runtime directory whose stand-in dispatcher reports
/// every submitted command's envelope.
fn start_reporting_test_server(
    directory_tag: &str,
) -> (
    IpcServer,
    SessionId,
    PathBuf,
    JoinHandle<()>,
    Receiver<CommandEnvelope>,
) {
    let runtime_directory = build_test_runtime_directory(directory_tag);
    let session_id = SessionId::new();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (dispatcher_thread, received_command_envelopes) =
        spawn_reporting_dispatcher(inbox_receiver);
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None)
        .expect("start serving");
    (
        ipc_server,
        session_id,
        runtime_directory,
        dispatcher_thread,
        received_command_envelopes,
    )
}

/// Open a control connection to `session_id`, send the Hello, read its answer,
/// and hand back the connection ready for the next request.
fn connect_greeted_connection(runtime_directory: &Path, session_id: SessionId) -> Connection {
    let mut connection = connect_to_session_socket(runtime_directory, session_id);
    connection
        .send(&build_hello_request(runtime_directory, session_id))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());
    connection
}

/// Submit `command_envelope` on `connection` and hand back the envelope the
/// dispatcher was given, after reading the reply the submission earns.
fn submit_command_envelope(
    connection: &mut Connection,
    received_command_envelopes: &Receiver<CommandEnvelope>,
    command_envelope: CommandEnvelope,
) -> CommandEnvelope {
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope)),
        })
        .expect("send submit");
    let dispatched_command_envelope = received_command_envelopes
        .recv()
        .expect("the dispatcher was given the command");
    let _: IpcResponse = connection.recv().expect("submit reply");
    dispatched_command_envelope
}

/// An envelope for submissions, from an external CLI naming no session.
fn build_test_command_envelope() -> CommandEnvelope {
    CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_external_cli(None, None),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
}

/// The endpoint file at `runtime_directory` for `session_id`.
fn load_test_endpoint_file(runtime_directory: &Path, session_id: SessionId) -> EndpointFile {
    EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("endpoint file readable")
}

/// The Hello that matches the endpoint file at `runtime_directory` for
/// `session_id`.
fn build_hello_request(runtime_directory: &Path, session_id: SessionId) -> IpcRequest {
    IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::Hello {
            minimum_protocol_version: MIN_PROTOCOL_VERSION,
            maximum_protocol_version: PROTOCOL_VERSION,
            connection_token: load_test_endpoint_file(runtime_directory, session_id)
                .connection_token,
            is_remote: false,
        },
    }
}

/// The answer an accepted Hello earns: [`PROTOCOL_VERSION`], and the version
/// of the build the session runs.
fn build_accepted_hello_result() -> IpcResult {
    IpcResult::Hello {
        protocol_version: PROTOCOL_VERSION,
        build_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Connect to the socket the endpoint file at `runtime_directory` advertises.
fn connect_to_session_socket(runtime_directory: &Path, session_id: SessionId) -> Connection {
    Connection::connect(&load_test_endpoint_file(runtime_directory, session_id).socket_address)
        .expect("connect")
}

/// A stand-in dispatcher answering every layout request with `session_layout`.
/// The returned receiver carries the tab each request named. Exits when every
/// inbox sender is gone.
fn spawn_layout_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
    session_layout: Option<SessionLayout>,
) -> (JoinHandle<()>, Receiver<Option<TabId>>) {
    let (requested_tab_id_sender, requested_tab_id_receiver) = mpsc::channel();
    let dispatcher_thread = std::thread::spawn(move || {
        while let Ok(runtime_event) = inbox_receiver.recv() {
            if let RuntimeEvent::IpcLayout {
                tab_id,
                response_sender,
            } = runtime_event
            {
                let _ = requested_tab_id_sender.send(tab_id);
                let _ = response_sender.send(session_layout.clone());
            }
        }
    });
    (dispatcher_thread, requested_tab_id_receiver)
}

/// A served socket whose stand-in dispatcher answers layout requests with
/// `session_layout`, plus the tab each request named.
fn start_layout_test_server(
    directory_tag: &str,
    session_layout: Option<SessionLayout>,
) -> (
    IpcServer,
    SessionId,
    PathBuf,
    JoinHandle<()>,
    Receiver<Option<TabId>>,
) {
    let runtime_directory = build_test_runtime_directory(directory_tag);
    let session_id = SessionId::new();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (dispatcher_thread, requested_tab_ids) =
        spawn_layout_dispatcher(inbox_receiver, session_layout);
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None)
        .expect("start serving");
    (
        ipc_server,
        session_id,
        runtime_directory,
        dispatcher_thread,
        requested_tab_ids,
    )
}

/// Stop `ipc_server`, join `dispatcher_thread`, and remove `runtime_directory`.
fn stop_test_server(
    ipc_server: IpcServer,
    dispatcher_thread: JoinHandle<()>,
    runtime_directory: &Path,
) {
    ipc_server.shutdown();
    dispatcher_thread.join().expect("dispatcher exits");
    remove_test_directory(runtime_directory);
}

/// A tiny layout to answer a layout request with, distinguishable by its name.
fn build_layout_named(session_name: &str) -> SessionLayout {
    SessionLayout {
        session_id: SessionId::new(),
        session_name: session_name.to_string(),
        tabs: Vec::new(),
        clients: Vec::new(),
    }
}

/// A tiny overview to answer discovery with, distinguishable by its name.
fn build_overview_named(session_name: &str) -> SessionOverview {
    SessionOverview {
        session: SessionDiscovery {
            session_id: SessionId::new(),
            session_name: session_name.to_string(),
            created_at: SystemTime::UNIX_EPOCH,
            attached_client_ids: Vec::new(),
            pane_count: 0,
        },
        tabs: Vec::new(),
        panes: Vec::new(),
        clients: Vec::new(),
    }
}

#[test]
fn a_control_connection_replaces_a_mouse_source_with_an_external_cli_one() {
    // The CLI-admission check lets every command through a mouse source. A
    // peer presenting one on a control connection is stamped back to the
    // source that connection carries.
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_command_envelopes) =
        start_reporting_test_server("stamp-mouse");
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    let sent_command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_mouse(ClientId::new()),
        Command::ToggleMouseSelect,
    );

    let dispatched_command_envelope = submit_command_envelope(
        &mut connection,
        &received_command_envelopes,
        sent_command_envelope.clone(),
    );

    assert_eq!(
        dispatched_command_envelope,
        CommandEnvelope::from_parts(
            sent_command_envelope.command_id,
            CommandSource::from_external_cli(None, None),
            Command::ToggleMouseSelect,
        )
    );
    assert_eq!(dispatched_command_envelope.client_id, None);

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_control_connection_cannot_present_another_clients_keybinding_source() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_command_envelopes) =
        start_reporting_test_server("stamp-keybinding");
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    let sent_command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(ClientId::new()),
        Command::ToggleMouseSelect,
    );

    let dispatched_command_envelope = submit_command_envelope(
        &mut connection,
        &received_command_envelopes,
        sent_command_envelope.clone(),
    );

    assert_eq!(
        dispatched_command_envelope,
        CommandEnvelope::from_parts(
            sent_command_envelope.command_id,
            CommandSource::from_external_cli(None, None),
            Command::ToggleMouseSelect,
        )
    );
    assert_eq!(dispatched_command_envelope.client_id, None);

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_control_connection_keeps_the_two_cli_sources_a_koshi_invocation_sends() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_command_envelopes) =
        start_reporting_test_server("stamp-cli");
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    let client_id = ClientId::new();
    let in_session_cli_source = CommandSource::from_in_session_cli(
        session_id,
        Some(client_id),
        PaneId::new(),
        PathBuf::from("/sock"),
    );
    let external_cli_source = CommandSource::from_external_cli(Some(session_id), Some(client_id));

    for (command_source, expected_client_id) in [
        (in_session_cli_source, Some(client_id)),
        (external_cli_source, None),
    ] {
        let sent_command_envelope = CommandEnvelope::from_parts(
            CommandId::new(),
            command_source,
            Command::ToggleLockMode(ToggleLockModeArgs::default()),
        );

        let dispatched_command_envelope = submit_command_envelope(
            &mut connection,
            &received_command_envelopes,
            sent_command_envelope.clone(),
        );

        assert_eq!(dispatched_command_envelope, sent_command_envelope);
        assert_eq!(dispatched_command_envelope.client_id, expected_client_id);
    }

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_submitted_command_round_trips_with_the_dispatchers_result() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("roundtrip", None);
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);
    let command_envelope = build_test_command_envelope();

    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope.clone())),
        })
        .expect("send submit");

    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.request_id, Some(1));
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    let submit_reply: IpcResponse = connection.recv().expect("submit reply");
    assert_eq!(submit_reply.request_id, Some(2));
    assert_eq!(
        submit_reply.answer_result,
        IpcResult::CommandResult(CommandResult::Ok {
            command_id: command_envelope.command_id,
            emitted_events: Vec::new(),
        }),
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

/// A request kind this build has no name for is refused by name, and the same
/// connection goes on serving the next request.
#[test]
fn a_request_kind_this_build_lacks_is_refused_by_name_and_the_connection_keeps_serving() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("unknown-kind", None);
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);

    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    // A well-framed request naming a kind added by some later koshi.
    connection
        .send(&serde_json::json!({
            "request_id": 2,
            "request_kind": { "Floating": { "pane_id": "00000000-0000-0000-0000-000000000001" } }
        }))
        .expect("send a kind this build does not have");

    let refusal_response: IpcResponse = connection.recv().expect("refusal reply");
    assert_eq!(refusal_response.request_id, Some(2));
    assert_eq!(
        refusal_response.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: "this Koshi has no request kind named Floating".to_string(),
        }),
    );

    // The connection is still open and still serving: the next request is
    // answered normally.
    let command_envelope = build_test_command_envelope();
    connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope.clone())),
        })
        .expect("send a command after the refusal");
    let command_response: IpcResponse = connection.recv().expect("command reply");
    assert_eq!(command_response.request_id, Some(3));
    assert_eq!(
        command_response.answer_result,
        IpcResult::CommandResult(CommandResult::Ok {
            command_id: command_envelope.command_id,
            emitted_events: Vec::new(),
        }),
        "the verb after the refusal was served normally"
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

/// A request kind this build has no name for, sent before the Hello, gets the
/// same `HelloRequired` refusal as every other kind sent before the Hello.
#[test]
fn a_kind_this_build_lacks_before_hello_is_refused_as_hello_required() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("unknown-kind-early", None);
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);

    connection
        .send(&serde_json::json!({
            "request_id": 9,
            "request_kind": { "Floating": { "pane_id": "00000000-0000-0000-0000-000000000001" } }
        }))
        .expect("send a kind this build does not have, before the hello");

    let refusal_response: IpcResponse = connection.recv().expect("refusal reply");
    assert_eq!(refusal_response.request_id, Some(9));
    assert_eq!(
        refusal_response.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Floating arrived before a Hello opened the connection".to_string(),
        }),
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

/// A caller reaching higher than this build settles on this build's highest,
/// and the connection serves from there.
#[test]
fn a_caller_speaking_a_wider_range_settles_on_this_builds_highest() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("wider-range", None);
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);
    let endpoint_file = load_test_endpoint_file(&runtime_directory, session_id);

    connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                minimum_protocol_version: MIN_PROTOCOL_VERSION,
                maximum_protocol_version: PROTOCOL_VERSION + 5,
                connection_token: endpoint_file.connection_token,
                is_remote: false,
            },
        })
        .expect("send a hello reaching above this build");

    let ipc_response: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(
        ipc_response.answer_result,
        IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            build_version: env!("CARGO_PKG_VERSION").to_string(),
        },
        "the answer names the highest version both sides speak"
    );

    let command_envelope = build_test_command_envelope();
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope.clone())),
        })
        .expect("send a command");
    let command_response: IpcResponse = connection.recv().expect("command reply");
    assert_eq!(
        command_response.answer_result,
        IpcResult::CommandResult(CommandResult::Ok {
            command_id: command_envelope.command_id,
            emitted_events: Vec::new(),
        }),
        "the gate opened and the connection serves at the settled version"
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

/// A caller whose whole range sits above this build shares no version with it.
/// The connection is refused naming both ranges, and no verb is served.
#[test]
fn a_caller_sharing_no_version_is_refused_and_serves_nothing() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("no-shared-version", None);
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);
    let endpoint_file = load_test_endpoint_file(&runtime_directory, session_id);
    let caller_minimum_protocol_version = PROTOCOL_VERSION + 1;

    connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                minimum_protocol_version: caller_minimum_protocol_version,
                maximum_protocol_version: caller_minimum_protocol_version + 2,
                connection_token: endpoint_file.connection_token,
                is_remote: false,
            },
        })
        .expect("send a hello sharing no version");

    let ipc_response: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(
        ipc_response.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: format!(
                "the caller speaks protocol versions {caller_minimum_protocol_version} to {}, \
                 this Koshi speaks {MIN_PROTOCOL_VERSION} to {PROTOCOL_VERSION}",
                caller_minimum_protocol_version + 2
            ),
        }),
    );

    // The gate stays closed after the refusal.
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery after the refusal");
    let second_response: IpcResponse = connection.recv().expect("second reply");
    assert_eq!(
        second_response.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Discovery arrived before a Hello opened the connection".to_string(),
        }),
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

/// A peer that speaks only the protocol the last release spoke shares no
/// version with this build. The connection is refused, and the session it
/// aimed at goes on serving.
#[test]
fn a_peer_speaking_the_previous_protocol_is_refused_and_the_session_keeps_serving() {
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
        start_attachable_test_server("previous-protocol", client_id);
    // The protocol the last release speaks, written as a literal.
    let previous_release_protocol_version = 3;
    let endpoint_file = load_test_endpoint_file(&runtime_directory, session_id);

    let mut old_peer = connect_to_session_socket(&runtime_directory, session_id);
    old_peer
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                minimum_protocol_version: previous_release_protocol_version,
                maximum_protocol_version: previous_release_protocol_version,
                connection_token: endpoint_file.connection_token,
                is_remote: false,
            },
        })
        .expect("send a hello from the protocol before this one");

    let refusal_response: IpcResponse = old_peer.recv().expect("hello reply");
    assert_eq!(
        refusal_response.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: format!(
                "the caller speaks protocol versions \
                 {previous_release_protocol_version} to {previous_release_protocol_version}, \
                 this Koshi speaks {MIN_PROTOCOL_VERSION} to {PROTOCOL_VERSION}"
            ),
        }),
    );
    drop(old_peer);

    // After the refusal, a peer on this protocol still attaches, and its typing
    // still reaches the dispatcher.
    let mut connection = attach_test_client(&runtime_directory, session_id, client_id);
    let typed_key_chord = KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('t'));
    connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Keyboard {
                key_input: build_key_input_for_chord(typed_key_chord),
            },
        })
        .expect("send the keyboard request");
    let RuntimeEvent::ClientKeyboard {
        client_id: received_client_id,
        key_input: received_key_input,
    } = received_runtime_events
        .recv_timeout(Duration::from_secs(5))
        .expect("the keyboard request reached the dispatcher")
    else {
        panic!("expected ClientKeyboard");
    };
    assert_eq!(received_client_id, client_id);
    assert_eq!(
        received_key_input,
        build_key_input_for_chord(typed_key_chord)
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

/// Every field the client's terminal reported crosses the attached
/// connection: the event kind, both alternative keys, the text, and all eight
/// modifiers.
#[test]
fn an_attached_connection_carries_every_field_of_the_key_input() {
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
        start_attachable_test_server("attached-whole-key", client_id);
    let mut connection = attach_test_client(&runtime_directory, session_id, client_id);
    let reported_key_input = KeyInput {
        key: KeyIdentity::Key(Key::Char('1')),
        key_event_kind: KeyEventKind::Release,
        shifted_key: Some('!'),
        base_layout_key: Some('q'),
        associated_text: "e\u{301}".to_string(),
        modifier_flags: KeyModifierFlags::from_bits(0b1111_1111),
    };

    connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Keyboard {
                key_input: reported_key_input.clone(),
            },
        })
        .expect("send the keyboard request");

    let RuntimeEvent::ClientKeyboard {
        client_id: received_client_id,
        key_input: received_key_input,
    } = received_runtime_events
        .recv_timeout(Duration::from_secs(5))
        .expect("the keyboard request reached the dispatcher")
    else {
        panic!("expected ClientKeyboard");
    };
    assert_eq!(received_client_id, client_id);
    assert_eq!(received_key_input, reported_key_input);

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_request_before_hello_is_refused_and_the_connection_keeps_serving() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("hello-first", None);
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);

    connection
        .send(&IpcRequest {
            request_id: 7,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(build_test_command_envelope())),
        })
        .expect("send submit before hello");
    let refusal_response: IpcResponse = connection.recv().expect("refusal reply");
    assert_eq!(refusal_response.request_id, Some(7));
    assert_eq!(
        refusal_response.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "SubmitCommand arrived before a Hello opened the connection".to_string(),
        }),
    );

    // The same connection still serves: a Hello opens it.
    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_wrong_token_is_refused_as_bad_token() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("bad-token", None);
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);

    connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                minimum_protocol_version: MIN_PROTOCOL_VERSION,
                maximum_protocol_version: PROTOCOL_VERSION,
                connection_token: ConnectionToken::from_secret("not-the-secret"),
                is_remote: false,
            },
        })
        .expect("send hello");
    let ipc_response: IpcResponse = connection.recv().expect("reply");
    assert_eq!(
        ipc_response.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_restart_advertises_a_fresh_token_and_refuses_the_old_one() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("restart-token", None);
    let initial_endpoint_file = load_test_endpoint_file(&runtime_directory, session_id);
    ipc_server.shutdown();
    dispatcher_thread.join().expect("dispatcher exits");

    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let restarted_dispatcher_thread = spawn_answering_dispatcher(inbox_receiver, None);
    let restarted_ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None)
        .expect("start serving again");
    let restarted_endpoint_file = load_test_endpoint_file(&runtime_directory, session_id);
    assert_ne!(
        restarted_endpoint_file.connection_token, initial_endpoint_file.connection_token,
        "the restarted server advertises a new secret",
    );

    let mut stale_connection = connect_to_session_socket(&runtime_directory, session_id);
    stale_connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                minimum_protocol_version: MIN_PROTOCOL_VERSION,
                maximum_protocol_version: PROTOCOL_VERSION,
                connection_token: initial_endpoint_file.connection_token,
                is_remote: false,
            },
        })
        .expect("send hello with the token from before the restart");
    let refusal_response: IpcResponse = stale_connection.recv().expect("reply");
    assert_eq!(
        refusal_response.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let mut accepted_connection = connect_to_session_socket(&runtime_directory, session_id);
    accepted_connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello with the new secret");
    let hello_reply: IpcResponse = accepted_connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    drop(stale_connection);
    drop(accepted_connection);
    stop_test_server(
        restarted_ipc_server,
        restarted_dispatcher_thread,
        &runtime_directory,
    );
}

#[test]
fn a_detach_leaves_the_sessions_token_unchanged() {
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
        start_attachable_test_server("detach-token", client_id);
    let endpoint_file_before_detach = load_test_endpoint_file(&runtime_directory, session_id);

    let attached_connection = attach_test_client(&runtime_directory, session_id, client_id);
    drop(attached_connection);
    let RuntimeEvent::ClientDetached {
        client_id: detached_client_id,
        ..
    } = received_runtime_events.recv().expect("detach event")
    else {
        panic!("expected ClientDetached");
    };
    assert_eq!(detached_client_id, client_id);

    let endpoint_file_after_detach = load_test_endpoint_file(&runtime_directory, session_id);
    assert_eq!(
        endpoint_file_after_detach.connection_token, endpoint_file_before_detach.connection_token,
        "the detached client's departure leaves the session's secret alone",
    );

    let mut connection = connect_to_session_socket(&runtime_directory, session_id);
    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello with the secret from before the detach");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_malformed_frame_is_answered_and_the_connection_keeps_serving() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("malformed", None);
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);

    // A well-framed message that is not an `IpcRequest` at all.
    connection.send(&"not a request").expect("send junk frame");
    let ipc_response: IpcResponse = connection.recv().expect("refusal reply");
    assert_eq!(ipc_response.request_id, None);
    assert_eq!(
        ipc_response.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the bytes received are not a request this build can read".to_string(),
        }),
    );

    // The stream is still aligned: the same connection opens and serves.
    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn an_oversize_frame_closes_the_connection() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("oversize", None);
    let endpoint_file = load_test_endpoint_file(&runtime_directory, session_id);

    // A raw stream carries a length prefix past the cap with no payload behind it.
    let mut raw_socket = connect_raw(&endpoint_file.socket_address);
    let oversize_length_prefix = (koshi_ipc::transport::MAX_FRAME_BYTE_COUNT + 1).to_be_bytes();
    std::io::Write::write_all(&mut raw_socket, &oversize_length_prefix)
        .expect("write oversize header");

    // The server closes: the next read finds the stream at end.
    let mut probe_byte = [0u8; 1];
    let is_connection_closed = match std::io::Read::read(&mut raw_socket, &mut probe_byte) {
        Ok(0) => true,
        Ok(_) => false,
        Err(_) => true,
    };
    assert!(
        is_connection_closed,
        "the connection must be closed after an oversize frame"
    );

    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

/// Opens the control socket as a raw byte stream, without the framed
/// [`Connection`]. The caller writes frame header bytes directly.
#[cfg(unix)]
fn connect_raw(socket_address: &str) -> std::os::unix::net::UnixStream {
    std::os::unix::net::UnixStream::connect(socket_address).expect("raw connect")
}

/// Opens the control socket as a raw byte stream, without the framed
/// [`Connection`]. The caller writes frame header bytes directly. The bare pipe
/// name is served at `\\.\pipe\<name>`.
#[cfg(windows)]
fn connect_raw(socket_address: &str) -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!(r"\\.\pipe\{socket_address}"))
        .expect("raw connect")
}

#[test]
fn an_attached_connection_forwards_input_unanswered_and_detaches_on_any_other_request() {
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
        start_attachable_test_server("attached-input", client_id);
    let mut connection = attach_test_client(&runtime_directory, session_id, client_id);
    let pressed_key_chord = KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('t'));
    let resized_viewport_size = Size {
        column_count: 120,
        row_count: 40,
    };
    let command_envelope = build_test_command_envelope();

    connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Keyboard {
                key_input: build_key_input_for_chord(pressed_key_chord),
            },
        })
        .expect("send key press");
    let RuntimeEvent::ClientKeyboard {
        client_id: keyboard_client_id,
        key_input,
    } = received_runtime_events.recv().expect("key press event")
    else {
        panic!("expected ClientKeyboard");
    };
    assert_eq!(keyboard_client_id, client_id);
    assert_eq!(key_input, build_key_input_for_chord(pressed_key_chord));

    connection
        .send(&IpcRequest {
            request_id: 4,
            request_kind: IpcRequestKind::Resize {
                viewport_size: resized_viewport_size,
                pane_area: None,
                cell_size: None,
            },
        })
        .expect("send resize");
    let RuntimeEvent::Resize {
        client_id: resize_client_id,
        viewport_size,
        pane_area,
        cell_size,
    } = received_runtime_events.recv().expect("resize event")
    else {
        panic!("expected Resize");
    };
    assert_eq!(resize_client_id, client_id);
    assert_eq!(viewport_size, resized_viewport_size);
    assert_eq!(pane_area, None);
    assert_eq!(cell_size, None);

    connection
        .send(&IpcRequest {
            request_id: 5,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope.clone())),
        })
        .expect("send submit");
    let RuntimeEvent::Ipc {
        command_envelope: dispatched_command_envelope,
        response_sender,
    } = received_runtime_events.recv().expect("submit event")
    else {
        panic!("expected Ipc");
    };
    assert_eq!(
        *dispatched_command_envelope,
        CommandEnvelope::from_parts(
            command_envelope.command_id,
            CommandSource::from_key_binding(client_id),
            command_envelope.command.clone(),
        ),
        "this connection's own client is stamped over what the peer sent",
    );
    assert_eq!(dispatched_command_envelope.client_id, Some(client_id));
    let unread_command_result = CommandResult::Ok {
        command_id: command_envelope.command_id,
        emitted_events: Vec::new(),
    };
    assert_eq!(
        response_sender.send(unread_command_result.clone()),
        Err(mpsc::SendError(unread_command_result)),
        "the reply channel's receiving end is already gone",
    );

    let sent_mouse_actions = vec![WireMouseAction::Scroll {
        pane_id: PaneId::new(),
        is_scrolling_up: true,
        scroll_line_count: 3,
    }];
    connection
        .send(&IpcRequest {
            request_id: 6,
            request_kind: IpcRequestKind::Mouse(sent_mouse_actions.clone()),
        })
        .expect("send mouse round");
    let RuntimeEvent::ClientMouse {
        client_id: mouse_client_id,
        request_id,
        mouse_actions,
    } = received_runtime_events.recv().expect("mouse round event")
    else {
        panic!("expected ClientMouse");
    };
    assert_eq!(mouse_client_id, client_id);
    assert_eq!(request_id, 6, "the round's own id crosses with it");
    assert_eq!(mouse_actions, sent_mouse_actions);

    connection
        .send(&IpcRequest {
            request_id: 7,
            request_kind: IpcRequestKind::Paste {
                pasted_text: String::from("hello\nworld"),
            },
        })
        .expect("send paste");
    let RuntimeEvent::HostPaste {
        client_id: paste_client_id,
        pasted_text,
    } = received_runtime_events.recv().expect("paste event")
    else {
        panic!("expected HostPaste");
    };
    assert_eq!(paste_client_id, client_id);
    assert_eq!(pasted_text, "hello\nworld");

    // A kind the reading half does not forward ends it, which detaches.
    connection
        .send(&IpcRequest {
            request_id: 8,
            request_kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");
    let RuntimeEvent::ClientDetached {
        client_id: detached_client_id,
        ..
    } = received_runtime_events.recv().expect("detach event")
    else {
        panic!("expected ClientDetached");
    };
    assert_eq!(detached_client_id, client_id);

    // The goodbye is the first frame after the attach reply: none of the five
    // requests above was answered with an `IpcResponse`.
    assert_eq!(
        connection.recv::<SessionEvent>().expect("goodbye frame"),
        SessionEvent::Detached,
    );
    assert!(
        matches!(
            connection.recv::<SessionEvent>(),
            Err(IpcError::Disconnected),
        ),
        "the stream ends after the goodbye",
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_request_kind_this_build_lacks_on_an_attached_connection_is_dropped_and_the_stream_goes_on() {
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
        start_attachable_test_server("attached-unknown-kind", client_id);
    let mut connection = attach_test_client(&runtime_directory, session_id, client_id);
    let pressed_key_chord = KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('t'));

    // A well-framed request naming a kind added by some later koshi.
    connection
        .send(&serde_json::json!({
            "request_id": 3,
            "request_kind": { "Floating": { "pane_id": "00000000-0000-0000-0000-000000000001" } }
        }))
        .expect("send a kind this build does not have");
    connection
        .send(&IpcRequest {
            request_id: 4,
            request_kind: IpcRequestKind::Keyboard {
                key_input: build_key_input_for_chord(pressed_key_chord),
            },
        })
        .expect("send key press");

    // The key press is the first event the dispatcher sees: the unfamiliar
    // request crossed nothing, and the stream carried the one behind it.
    let RuntimeEvent::ClientKeyboard {
        client_id: keyboard_client_id,
        key_input,
    } = received_runtime_events
        .recv_timeout(Duration::from_secs(5))
        .expect("key press event")
    else {
        panic!("expected ClientKeyboard");
    };
    assert_eq!(keyboard_client_id, client_id);
    assert_eq!(key_input, build_key_input_for_chord(pressed_key_chord));

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_malformed_frame_on_an_attached_connection_is_dropped_and_the_stream_goes_on() {
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
        start_attachable_test_server("attached-malformed", client_id);
    let mut connection = attach_test_client(&runtime_directory, session_id, client_id);
    let pressed_key_chord = KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('t'));

    // A well-framed message that is not a request at all.
    connection.send(&"not a request").expect("send junk frame");
    connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Keyboard {
                key_input: build_key_input_for_chord(pressed_key_chord),
            },
        })
        .expect("send key press");

    // The key press is the first event the dispatcher sees: the unreadable
    // frame crossed nothing, and the stream carried the one behind it.
    let RuntimeEvent::ClientKeyboard {
        client_id: keyboard_client_id,
        key_input,
    } = received_runtime_events
        .recv_timeout(Duration::from_secs(5))
        .expect("key press event")
    else {
        panic!("expected ClientKeyboard");
    };
    assert_eq!(keyboard_client_id, client_id);
    assert_eq!(key_input, build_key_input_for_chord(pressed_key_chord));

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_keyboard_request_before_an_attach_closes_the_connection() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("key-unattached", None);
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);

    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Keyboard {
                key_input: build_key_input_for_chord(KeyChord::from_parts(
                    BindingModifierFlags::CTRL,
                    Key::Char('t'),
                )),
            },
        })
        .expect("send the keyboard request");
    assert!(
        matches!(
            connection.recv::<IpcResponse>(),
            Err(IpcError::Disconnected),
        ),
        "no reply comes back, and the connection is closed",
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_mouse_round_before_an_attach_closes_the_connection() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("mouse-unattached", None);
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);

    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                pane_id: PaneId::new(),
                is_scrolling_up: true,
                scroll_line_count: 3,
            }]),
        })
        .expect("send mouse round");
    assert!(
        matches!(
            connection.recv::<IpcResponse>(),
            Err(IpcError::Disconnected),
        ),
        "no reply comes back, and the connection is closed",
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn discovery_answers_with_the_dispatchers_overview() {
    let session_overview = build_overview_named("workspace");
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("discovery", Some(session_overview.clone()));
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);

    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");

    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());
    let discovery_reply: IpcResponse = connection.recv().expect("discovery reply");
    assert_eq!(discovery_reply.request_id, Some(2));
    assert_eq!(
        discovery_reply.answer_result,
        IpcResult::Overview(session_overview)
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn discovery_with_no_running_session_closes_the_connection() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("discovery-none", None);
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);

    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");
    assert!(
        matches!(
            connection.recv::<IpcResponse>(),
            Err(IpcError::Disconnected),
        ),
        "no reply comes back once no session is running",
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_recent_events_request_answers_from_the_ring_without_asking_the_dispatcher() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("recent-events", None);
    let tab_id = TabId::new();
    recent_events::record_event(&koshi_core::event::Event::LayoutChanged(
        koshi_core::event::LayoutChanged { tab_id },
    ));
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);

    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::RecentEvents,
        })
        .expect("send recent-events request");

    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());
    let recent_events_reply: IpcResponse = connection.recv().expect("recent-events reply");
    assert_eq!(recent_events_reply.request_id, Some(2));
    let IpcResult::RecentEvents(recent_event_records) = recent_events_reply.answer_result else {
        panic!(
            "expected recent events, got {:?}",
            recent_events_reply.answer_result
        );
    };
    // The ring is process-wide and shared by every test in this binary. The
    // test finds its record by this tab's own id.
    let layout_event_record = recent_event_records
        .iter()
        .find(|recent_event_record| recent_event_record.tab_id == Some(tab_id))
        .expect("the answer carries the layout record this test made");
    assert_eq!(layout_event_record.event_name, "LayoutChanged");

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_layout_request_answers_with_the_dispatchers_layout_and_names_the_tab_asked_for() {
    let session_layout = build_layout_named("workspace");
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, requested_tab_ids) =
        start_layout_test_server("layout-one-tab", Some(session_layout.clone()));
    let requested_tab_id = TabId::new();
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);

    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Layout {
                tab_id: Some(requested_tab_id),
            },
        })
        .expect("send layout request");

    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());
    let layout_reply: IpcResponse = connection.recv().expect("layout reply");
    assert_eq!(layout_reply.request_id, Some(2));
    assert_eq!(
        layout_reply.answer_result,
        IpcResult::Layout(session_layout)
    );
    assert_eq!(
        requested_tab_ids.recv().expect("the dispatcher was asked"),
        Some(requested_tab_id)
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_layout_request_for_every_tab_names_no_tab_to_the_dispatcher() {
    let session_layout = build_layout_named("workspace");
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, requested_tab_ids) =
        start_layout_test_server("layout-every-tab", Some(session_layout.clone()));
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);

    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Layout { tab_id: None },
        })
        .expect("send layout request");

    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());
    let layout_reply: IpcResponse = connection.recv().expect("layout reply");
    assert_eq!(
        layout_reply.answer_result,
        IpcResult::Layout(session_layout)
    );
    assert_eq!(
        requested_tab_ids.recv().expect("the dispatcher was asked"),
        None
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_layout_request_with_no_running_session_closes_the_connection() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, _requested_tab_ids) =
        start_layout_test_server("layout-none", None);
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);

    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Layout { tab_id: None },
        })
        .expect("send layout request");
    assert!(
        matches!(
            connection.recv::<IpcResponse>(),
            Err(IpcError::Disconnected),
        ),
        "no reply comes back once no session is running",
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_layout_request_on_an_attached_connection_ends_that_client_stream() {
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
        start_attachable_test_server("layout-attached", client_id);
    let mut connection = attach_test_client(&runtime_directory, session_id, client_id);

    connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Layout { tab_id: None },
        })
        .expect("send layout request");

    let RuntimeEvent::ClientDetached {
        client_id: detached_client_id,
        ..
    } = received_runtime_events.recv().expect("detach event")
    else {
        panic!("expected ClientDetached");
    };
    assert_eq!(detached_client_id, client_id);
    assert_eq!(
        connection.recv::<SessionEvent>().expect("goodbye frame"),
        SessionEvent::Detached,
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_gone_dispatcher_closes_the_connection_instead_of_answering() {
    let runtime_directory = build_test_runtime_directory("no-dispatcher");
    let session_id = SessionId::new();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    drop(inbox_receiver);
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None)
        .expect("start serving");
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);

    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(build_test_command_envelope())),
        })
        .expect("send submit");
    assert!(
        matches!(
            connection.recv::<IpcResponse>(),
            Err(IpcError::Disconnected),
        ),
        "no reply comes back once the dispatcher is gone",
    );

    drop(connection);
    ipc_server.shutdown();
    remove_test_directory(&runtime_directory);
}

#[test]
fn the_endpoint_file_lives_while_serving_and_both_files_go_at_shutdown() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("lifecycle", None);
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session_id);
    let endpoint_file =
        EndpointFile::load_from_path(&endpoint_path).expect("endpoint file readable");

    assert_eq!(endpoint_file.process_id, std::process::id());
    #[cfg(unix)]
    assert!(
        Path::new(&endpoint_file.socket_address).exists(),
        "socket file present while serving",
    );

    ipc_server.shutdown();
    dispatcher_thread.join().expect("dispatcher exits");

    assert!(!endpoint_path.exists(), "endpoint file gone after shutdown");
    #[cfg(unix)]
    assert!(
        !Path::new(&endpoint_file.socket_address).exists(),
        "socket file gone after shutdown",
    );
    let Err(IpcError::NoListener { socket_address }) =
        Connection::connect(&endpoint_file.socket_address)
    else {
        panic!("nothing listens after shutdown");
    };
    assert_eq!(socket_address, endpoint_file.socket_address);
    remove_test_directory(&runtime_directory);
}

#[test]
fn dropping_the_server_without_shutdown_still_removes_both_files() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("drop-cleans", None);
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session_id);
    let endpoint_file =
        EndpointFile::load_from_path(&endpoint_path).expect("endpoint file readable");

    drop(ipc_server);
    dispatcher_thread.join().expect("dispatcher exits");

    assert!(!endpoint_path.exists(), "endpoint file gone after drop");
    let Err(IpcError::NoListener { socket_address }) =
        Connection::connect(&endpoint_file.socket_address)
    else {
        panic!("nothing listens after drop");
    };
    assert_eq!(socket_address, endpoint_file.socket_address);
    remove_test_directory(&runtime_directory);
}

#[cfg(unix)]
#[test]
fn shutdown_returns_and_removes_the_endpoint_even_when_the_wake_cannot_connect() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("wake-fails", None);
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session_id);
    let endpoint_file =
        EndpointFile::load_from_path(&endpoint_path).expect("endpoint file readable");

    // The socket file is unlinked under the listener: the wake connect inside
    // shutdown fails, and shutdown returns without joining the accept loop.
    std::fs::remove_file(&endpoint_file.socket_address).expect("unlink the live socket");

    ipc_server.shutdown();

    assert!(
        !endpoint_path.exists(),
        "endpoint file gone even though the accept loop could not be woken",
    );
    drop(dispatcher_thread);
    remove_test_directory(&runtime_directory);
}

#[cfg(unix)]
#[test]
fn a_leftover_socket_file_is_reclaimed_at_start() {
    let runtime_directory = build_test_runtime_directory("reclaim");
    koshi_paths::ensure_private_directory(&runtime_directory).expect("create runtime directory");
    let session_id = SessionId::new();
    let socket_address = compute_socket_address(&runtime_directory, session_id);
    std::fs::write(&socket_address, b"").expect("plant a leftover file at the socket path");

    let (inbox_sender, _inbox_receiver) = mpsc::channel();
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None)
        .expect("start reclaims the leftover and serves");

    ipc_server.shutdown();
    remove_test_directory(&runtime_directory);
}

#[test]
fn a_second_start_on_the_same_session_is_refused_while_serving() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("busy", None);

    let (inbox_sender, _inbox_receiver) = mpsc::channel();
    let Err(IpcError::SocketBusy { socket_address }) =
        IpcServer::start(&runtime_directory, session_id, inbox_sender, None)
    else {
        panic!("the live listener must refuse a second bind");
    };
    assert_eq!(
        socket_address,
        compute_socket_address(&runtime_directory, session_id)
    );

    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_runtime_directory_that_cannot_be_created_refuses_to_start() {
    // A file where the directory would go: creating the directory under it
    // fails, and the start stops before it binds anything.
    let blocking_file_path = build_test_runtime_directory("runtime-dir-blocked");
    remove_test_directory(&blocking_file_path);
    std::fs::write(&blocking_file_path, b"").expect("plant a file where the directory would go");
    let runtime_directory = blocking_file_path.join("session");
    let (inbox_sender, _inbox_receiver) = mpsc::channel();

    let Err(IpcError::Transport { error_detail }) =
        IpcServer::start(&runtime_directory, SessionId::new(), inbox_sender, None)
    else {
        panic!("a runtime directory that cannot be created must refuse the start");
    };
    assert!(
        error_detail.starts_with(&format!(
            "could not create the runtime directory {}: ",
            runtime_directory.display()
        )),
        "the refusal names the directory it could not create: {error_detail}",
    );

    std::fs::remove_file(&blocking_file_path).expect("take the planted file away");
}

#[test]
fn a_start_whose_endpoint_file_cannot_be_written_leaves_nothing_listening() {
    // A directory where the endpoint file goes: the write cannot rename over
    // it, and the start unwinds the bind it already made.
    let runtime_directory = build_test_runtime_directory("endpoint-write-fails");
    koshi_paths::ensure_private_directory(&runtime_directory).expect("create runtime directory");
    let session_id = SessionId::new();
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session_id);
    let socket_address = compute_socket_address(&runtime_directory, session_id);
    std::fs::create_dir_all(&endpoint_path).expect("plant a directory where the file goes");
    let (inbox_sender, _inbox_receiver) = mpsc::channel();

    let Err(IpcError::EndpointFileWrite {
        endpoint_file_path, ..
    }) = IpcServer::start(&runtime_directory, session_id, inbox_sender, None)
    else {
        panic!("an endpoint file that cannot be written must refuse the start");
    };
    assert_eq!(endpoint_file_path, endpoint_path.display().to_string());

    #[cfg(unix)]
    assert!(
        !Path::new(&socket_address).exists(),
        "the socket file the refused start bound is gone",
    );
    let Err(IpcError::NoListener {
        socket_address: refused_socket_address,
    }) = Connection::connect(&socket_address)
    else {
        panic!("nothing listens after a refused start");
    };
    assert_eq!(refused_socket_address, socket_address);

    remove_test_directory(&runtime_directory);
}

#[test]
fn a_session_only_its_own_user_may_reach_binds_inside_the_runtime_directory() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("own-user-socket", None);
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session_id);
    let endpoint_file =
        EndpointFile::load_from_path(&endpoint_path).expect("endpoint file readable");

    assert_eq!(
        endpoint_file.socket_address,
        compute_socket_address(&runtime_directory, session_id)
    );
    assert_eq!(endpoint_path.parent(), Some(runtime_directory.as_path()));

    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_session_other_local_users_may_reach_keeps_its_endpoint_file_private() {
    let (ipc_server, session_id, runtime_directory, shared_directory, dispatcher_thread) =
        start_shared_test_server("shared-endpoint", true);
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session_id);

    assert_eq!(endpoint_path.parent(), Some(runtime_directory.as_path()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let endpoint_permission_mode = std::fs::metadata(&endpoint_path)
            .expect("stat endpoint file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(endpoint_permission_mode, 0o600);
    }

    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    remove_test_directory(&shared_directory);
}

#[cfg(unix)]
#[test]
fn the_socket_of_a_session_other_local_users_may_reach_is_open_to_every_local_user() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let (ipc_server, session_id, runtime_directory, shared_directory, dispatcher_thread) =
        start_shared_test_server("shared-mode", true);
    let endpoint_file = load_test_endpoint_file(&runtime_directory, session_id);

    // This start created the runtime directory. Its owner is the user whose
    // directory under the shared one holds the socket.
    let owner_user_id = std::fs::metadata(&runtime_directory)
        .expect("stat runtime directory")
        .uid();
    assert_eq!(
        PathBuf::from(&endpoint_file.socket_address),
        shared_directory
            .join(owner_user_id.to_string())
            .join(format!("{session_id}.sock")),
    );
    let socket_permission_mode = std::fs::metadata(&endpoint_file.socket_address)
        .expect("stat socket file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(socket_permission_mode, 0o666);

    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    remove_test_directory(&shared_directory);
}

#[cfg(windows)]
#[test]
fn the_marker_naming_a_shared_session_lives_while_serving_and_goes_at_shutdown() {
    let (ipc_server, session_id, runtime_directory, shared_directory, dispatcher_thread) =
        start_shared_test_server("shared-marker", true);
    let advertisement_marker_path =
        resolve_advertisement_marker_path(&shared_directory, session_id);

    assert!(
        advertisement_marker_path.exists(),
        "marker present while serving"
    );

    ipc_server.shutdown();
    dispatcher_thread.join().expect("dispatcher exits");

    assert!(
        !advertisement_marker_path.exists(),
        "marker gone after shutdown"
    );
    remove_test_directory(&runtime_directory);
    remove_test_directory(&shared_directory);
}

#[test]
fn the_user_who_started_the_session_attaches_over_the_shared_socket_with_the_token() {
    let runtime_directory = build_test_runtime_directory("shared-attach");
    let shared_directory = build_test_shared_directory("shared-attach");
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (dispatcher_thread, _received_runtime_events) =
        spawn_attaching_dispatcher(inbox_receiver, client_id, session_id);
    let ipc_server = IpcServer::start(
        &runtime_directory,
        session_id,
        inbox_sender,
        Some(OtherUsers {
            shared_directory: shared_directory.clone(),
            is_enabled: Arc::new(|| true),
        }),
    )
    .expect("start serving");

    let connection = attach_test_client(&runtime_directory, session_id, client_id);

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    remove_test_directory(&shared_directory);
}

// --- Serving one connection from another local user ---

/// A control-socket address unique to this test: a file path under `/tmp` on
/// Unix, a pipe name on Windows.
fn build_test_socket_address(socket_tag: &str) -> String {
    let unique_socket_name = format!("koshi-peer-{}-{socket_tag}", std::process::id());
    #[cfg(unix)]
    {
        PathBuf::from("/tmp")
            .join(unique_socket_name)
            .with_extension("sock")
            .display()
            .to_string()
    }
    #[cfg(windows)]
    {
        unique_socket_name
    }
}

/// Serves one connection from another local user of this machine.
/// `is_other_user_access_enabled` stands in for the `allow-other-users`
/// setting, and the serving loop reads it again for each request. Returns the
/// caller's end, the serving thread and the socket address.
fn serve_other_user(
    socket_tag: &str,
    is_other_user_access_enabled: &Arc<AtomicBool>,
    inbox_sender: Sender<RuntimeEvent>,
) -> (Connection, JoinHandle<()>, String) {
    let socket_address = build_test_socket_address(socket_tag);
    remove_socket_file(&socket_address);
    let listener = Listener::bind(&socket_address).expect("bind");
    let other_user_access_setting = Arc::clone(is_other_user_access_enabled);
    let serving_thread = std::thread::spawn(move || {
        let connection = listener.accept().expect("accept");
        let intake = Arc::new(Intake::default());
        let served_connection = intake
            .accept_connection(&connection)
            .expect("the intake takes it");
        serve_connection(
            connection,
            ConnectionToken::generate(),
            &inbox_sender,
            Peer::Local {
                is_same_user: false,
                is_other_user_access_allowed: true,
            },
            Some(Arc::new(move || {
                other_user_access_setting.load(Ordering::SeqCst)
            })),
            &served_connection,
        );
    });
    let caller_connection = Connection::connect(&socket_address).expect("connect");
    (caller_connection, serving_thread, socket_address)
}

/// The Hello another local user sends: this build's protocol range and an
/// empty token.
fn build_other_user_hello() -> IpcRequest {
    IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::Hello {
            minimum_protocol_version: MIN_PROTOCOL_VERSION,
            maximum_protocol_version: PROTOCOL_VERSION,
            connection_token: ConnectionToken::from_secret(""),
            is_remote: false,
        },
    }
}

/// Sends the Hello and the Attach as another local user on `caller_connection`,
/// and checks both replies. The connection carries `client_id`'s event stream
/// afterwards.
fn attach_as_other_user(
    caller_connection: &mut Connection,
    client_id: ClientId,
    session_id: SessionId,
) {
    caller_connection
        .send(&build_other_user_hello())
        .expect("send hello");
    let hello_response: IpcResponse = caller_connection.recv().expect("hello reply");
    assert_eq!(hello_response.answer_result, build_accepted_hello_result());
    caller_connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Attach {
                viewport_size: CLIENT_VIEWPORT_SIZE,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities: GraphicsCapabilities::default(),
                cell_size: None,
            },
        })
        .expect("send attach");
    let attach_response: IpcResponse = caller_connection.recv().expect("attach reply");
    assert_eq!(
        attach_response.answer_result,
        IpcResult::Attached {
            client_id,
            session_id,
            session_structure: build_attached_structure(session_id),
            resume_token: Some(ConnectionToken::from_secret(MINTED_CONNECTION_TOKEN)),
            pane_area: None,
        }
    );
}

#[test]
fn another_local_user_keeps_being_served_while_the_setting_stays_on() {
    let is_other_user_access_enabled = Arc::new(AtomicBool::new(true));
    let session_overview = build_overview_named("shared-session");
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let dispatcher_thread =
        spawn_answering_dispatcher(inbox_receiver, Some(session_overview.clone()));
    let (mut caller_connection, serving_thread, socket_address) =
        serve_other_user("stays-on", &is_other_user_access_enabled, inbox_sender);

    caller_connection
        .send(&build_other_user_hello())
        .expect("send hello");
    let hello_response: IpcResponse = caller_connection.recv().expect("hello reply");
    assert_eq!(hello_response.answer_result, build_accepted_hello_result());

    for request_id in [2, 3] {
        caller_connection
            .send(&IpcRequest {
                request_id,
                request_kind: IpcRequestKind::Discovery,
            })
            .expect("send discovery");
        let discovery_response: IpcResponse = caller_connection.recv().expect("discovery reply");
        assert_eq!(
            discovery_response,
            IpcResponse {
                request_id: Some(request_id),
                answer_result: IpcResult::Overview(session_overview.clone()),
            }
        );
    }

    drop(caller_connection);
    serving_thread.join().expect("serving thread");
    dispatcher_thread.join().expect("dispatcher exits");
    remove_socket_file(&socket_address);
}

#[test]
fn another_local_users_connection_is_cut_when_the_setting_goes_off() {
    let is_other_user_access_enabled = Arc::new(AtomicBool::new(true));
    let session_overview = build_overview_named("shared-session");
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let dispatcher_thread =
        spawn_answering_dispatcher(inbox_receiver, Some(session_overview.clone()));
    let (mut caller_connection, serving_thread, socket_address) =
        serve_other_user("goes-off", &is_other_user_access_enabled, inbox_sender);

    caller_connection
        .send(&build_other_user_hello())
        .expect("send hello");
    let hello_response: IpcResponse = caller_connection.recv().expect("hello reply");
    assert_eq!(hello_response.answer_result, build_accepted_hello_result());
    caller_connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");
    let discovery_response: IpcResponse = caller_connection.recv().expect("discovery reply");
    assert_eq!(
        discovery_response,
        IpcResponse {
            request_id: Some(2),
            answer_result: IpcResult::Overview(session_overview),
        }
    );

    // The setting turns off while the serving loop waits for the next request.
    is_other_user_access_enabled.store(false, Ordering::SeqCst);
    caller_connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");

    assert!(
        matches!(
            caller_connection.recv::<IpcResponse>(),
            Err(IpcError::Disconnected)
        ),
        "the request is answered with a closed connection, not an overview",
    );

    drop(caller_connection);
    serving_thread.join().expect("serving thread");
    dispatcher_thread.join().expect("dispatcher exits");
    remove_socket_file(&socket_address);
}

#[test]
fn an_attached_client_of_another_local_user_is_detached_when_the_setting_goes_off() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let is_other_user_access_enabled = Arc::new(AtomicBool::new(true));
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (dispatcher_thread, received_runtime_events) =
        spawn_attaching_dispatcher(inbox_receiver, client_id, session_id);
    let (mut caller_connection, serving_thread, socket_address) =
        serve_other_user("attached-off", &is_other_user_access_enabled, inbox_sender);

    attach_as_other_user(&mut caller_connection, client_id, session_id);

    let pressed_key_chord = KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('k'));
    caller_connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Keyboard {
                key_input: build_key_input_for_chord(pressed_key_chord),
            },
        })
        .expect("send key press");
    let RuntimeEvent::ClientKeyboard {
        client_id: keyboard_client_id,
        key_input,
    } = received_runtime_events.recv().expect("key press event")
    else {
        panic!("expected ClientKeyboard");
    };
    assert_eq!(keyboard_client_id, client_id);
    assert_eq!(key_input, build_key_input_for_chord(pressed_key_chord));

    is_other_user_access_enabled.store(false, Ordering::SeqCst);
    caller_connection
        .send(&IpcRequest {
            request_id: 4,
            request_kind: IpcRequestKind::Keyboard {
                key_input: build_key_input_for_chord(pressed_key_chord),
            },
        })
        .expect("send key press");

    // The typing that arrived after the setting went off never reached the
    // session. The next event is the detach.
    let RuntimeEvent::ClientDetached {
        client_id: detached_client_id,
        ..
    } = received_runtime_events.recv().expect("detach event")
    else {
        panic!("expected ClientDetached");
    };
    assert_eq!(detached_client_id, client_id);
    assert_eq!(
        caller_connection
            .recv::<SessionEvent>()
            .expect("goodbye frame"),
        SessionEvent::Detached,
    );

    drop(caller_connection);
    serving_thread.join().expect("serving thread");
    dispatcher_thread.join().expect("dispatcher exits");
    remove_socket_file(&socket_address);
}

#[test]
fn a_withdrawn_local_user_is_detached_by_a_frame_this_build_cannot_read() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let is_other_user_access_enabled = Arc::new(AtomicBool::new(true));
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (dispatcher_thread, received_runtime_events) =
        spawn_attaching_dispatcher(inbox_receiver, client_id, session_id);
    let (mut caller_connection, serving_thread, socket_address) = serve_other_user(
        "attached-off-junk",
        &is_other_user_access_enabled,
        inbox_sender,
    );
    attach_as_other_user(&mut caller_connection, client_id, session_id);

    // The frame after the setting goes off is one this build cannot read. The
    // client is detached, not kept.
    is_other_user_access_enabled.store(false, Ordering::SeqCst);
    caller_connection
        .send(&"not a request")
        .expect("send junk frame");

    let RuntimeEvent::ClientDetached {
        client_id: detached_client_id,
        ..
    } = received_runtime_events
        .recv_timeout(Duration::from_secs(5))
        .expect("detach event")
    else {
        panic!("expected ClientDetached");
    };
    assert_eq!(detached_client_id, client_id);
    assert_eq!(
        caller_connection
            .recv::<SessionEvent>()
            .expect("goodbye frame"),
        SessionEvent::Detached,
    );

    drop(caller_connection);
    serving_thread.join().expect("serving thread");
    dispatcher_thread.join().expect("dispatcher exits");
    remove_socket_file(&socket_address);
}

#[test]
fn a_lost_connection_ends_an_attached_clients_reading_half() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let is_other_user_access_enabled = Arc::new(AtomicBool::new(true));
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (dispatcher_thread, received_runtime_events) =
        spawn_attaching_dispatcher(inbox_receiver, client_id, session_id);
    let (mut caller_connection, serving_thread, socket_address) =
        serve_other_user("attached-lost", &is_other_user_access_enabled, inbox_sender);
    attach_as_other_user(&mut caller_connection, client_id, session_id);

    // Nothing is queued for this client, and its writing half stays blocked on
    // the open queue. The detach below comes from the reading half.
    drop(caller_connection);

    let RuntimeEvent::ClientDetached {
        client_id: detached_client_id,
        ..
    } = received_runtime_events
        .recv_timeout(Duration::from_secs(5))
        .expect("detach event")
    else {
        panic!("expected ClientDetached");
    };
    assert_eq!(detached_client_id, client_id);

    serving_thread.join().expect("serving thread");
    dispatcher_thread.join().expect("dispatcher exits");
    remove_socket_file(&socket_address);
}

#[test]
fn the_directory_other_local_users_reach_holds_only_the_socket() {
    let (ipc_server, session_id, runtime_directory, shared_directory, dispatcher_thread) =
        start_shared_test_server("shared-only", true);
    #[cfg(unix)]
    let user_directory = {
        use std::os::unix::fs::MetadataExt;

        let owner_user_id = std::fs::metadata(&runtime_directory)
            .expect("stat runtime directory")
            .uid();
        shared_directory.join(owner_user_id.to_string())
    };
    // On Windows the advertisement lives in the shared directory itself.
    #[cfg(windows)]
    let user_directory = shared_directory.clone();

    let mut directory_entry_names: Vec<String> = std::fs::read_dir(&user_directory)
        .expect("read the shared directory")
        .map(|directory_entry| {
            directory_entry
                .expect("read an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    directory_entry_names.sort();

    // The endpoint file carrying the token is not among them: it stays in the
    // private runtime directory.
    #[cfg(unix)]
    assert_eq!(directory_entry_names, vec![format!("{session_id}.sock")]);
    #[cfg(windows)]
    assert_eq!(directory_entry_names, vec![session_id.to_string()]);

    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    remove_test_directory(&shared_directory);
}

/// A stand-in dispatcher that answers every restart request with
/// `restart_verdict` and every discovery request with `session_overview`.
/// Exits when every inbox sender is gone.
fn spawn_restart_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
    restart_verdict: Result<(), String>,
    session_overview: Option<SessionOverview>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while let Ok(runtime_event) = inbox_receiver.recv() {
            match runtime_event {
                RuntimeEvent::IpcRestart { response_sender } => {
                    let _ = response_sender.send(restart_verdict.clone());
                }
                RuntimeEvent::IpcDiscovery { response_sender } => {
                    let _ = response_sender.send(session_overview.clone());
                }
                _ => {}
            }
        }
    })
}

/// A served socket whose stand-in dispatcher answers restart requests with
/// `restart_verdict` and discovery requests with `session_overview`.
fn start_restartable_test_server(
    directory_tag: &str,
    restart_verdict: Result<(), String>,
    session_overview: Option<SessionOverview>,
) -> (IpcServer, SessionId, PathBuf, JoinHandle<()>) {
    let runtime_directory = build_test_runtime_directory(directory_tag);
    let session_id = SessionId::new();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let dispatcher_thread =
        spawn_restart_dispatcher(inbox_receiver, restart_verdict, session_overview);
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None)
        .expect("start serving");
    (ipc_server, session_id, runtime_directory, dispatcher_thread)
}

/// Sends the Hello and a restart, and returns the connection and the answer to
/// the restart.
fn send_restart_request(
    runtime_directory: &Path,
    session_id: SessionId,
) -> (Connection, IpcResult) {
    let mut connection = connect_to_session_socket(runtime_directory, session_id);
    connection
        .send(&build_hello_request(runtime_directory, session_id))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Restart,
        })
        .expect("send restart");
    let restart_response: IpcResponse = connection.recv().expect("restart reply");
    assert_eq!(restart_response.request_id, Some(2));
    (connection, restart_response.answer_result)
}

/// Sends a discovery request on an open connection and returns the answer.
fn request_discovery(connection: &mut Connection, request_id: u64) -> IpcResult {
    connection
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");
    let discovery_response: IpcResponse = connection.recv().expect("discovery reply");
    assert_eq!(discovery_response.request_id, Some(request_id));
    discovery_response.answer_result
}

#[test]
fn an_accepted_restart_is_answered_restarting() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_restartable_test_server("restart-accepted", Ok(()), None);

    let (connection, restart_result) = send_restart_request(&runtime_directory, session_id);

    assert_eq!(restart_result, IpcResult::Restarting);

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

/// A restart sent before the Hello is refused as `HelloRequired`, like every
/// other kind.
#[test]
fn a_restart_before_hello_is_refused_as_hello_required_and_the_connection_keeps_serving() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_restartable_test_server(
            "restart-early",
            Ok(()),
            Some(build_overview_named("still-here")),
        );
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);

    connection
        .send(&IpcRequest {
            request_id: 9,
            request_kind: IpcRequestKind::Restart,
        })
        .expect("send restart before the hello");
    let refusal_response: IpcResponse = connection.recv().expect("refusal reply");

    assert_eq!(refusal_response.request_id, Some(9));
    assert_eq!(
        refusal_response.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Restart arrived before a Hello opened the connection".to_string(),
        }),
    );

    // The gate is still closed, and the same connection still answers.
    assert_eq!(
        request_discovery(&mut connection, 10),
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Discovery arrived before a Hello opened the connection".to_string(),
        }),
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

/// A binary this machine could not run, written into `binary_directory`: on Unix a file
/// with its execute permission dropped, elsewhere a path with nothing at it.
/// Hands back the path and the sentence the check refuses it with.
fn build_unrunnable_binary(binary_directory: &Path) -> (PathBuf, String) {
    std::fs::create_dir_all(binary_directory).expect("the directory is created");
    let executable_path = binary_directory.join("koshi");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::write(&executable_path, b"").expect("the stand-in binary is written");
        std::fs::set_permissions(&executable_path, std::fs::Permissions::from_mode(0o644))
            .expect("the execute permission is dropped");
        let rejection_message = format!(
            "the binary at {} is not executable",
            executable_path.display()
        );
        (executable_path, rejection_message)
    }
    #[cfg(not(unix))]
    {
        let metadata_error =
            std::fs::metadata(&executable_path).expect_err("nothing is at that path");
        let rejection_message = format!(
            "the binary at {} could not be read: {metadata_error}",
            executable_path.display()
        );
        (executable_path, rejection_message)
    }
}

/// A restart whose binary cannot run is refused with the sentence the check
/// names, and the session answers the next request.
#[test]
fn a_restart_naming_a_binary_that_cannot_run_is_refused_and_the_session_keeps_serving() {
    let binary_directory = build_test_runtime_directory("restart-bad-binary-directory");
    let (executable_path, rejection_message) = build_unrunnable_binary(&binary_directory);
    let session_overview = build_overview_named("still-here");
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_restartable_test_server(
            "restart-bad-binary",
            crate::server::is_binary_runnable(&executable_path),
            Some(session_overview.clone()),
        );

    let (mut connection, restart_result) = send_restart_request(&runtime_directory, session_id);

    assert_eq!(
        restart_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: rejection_message,
        }),
    );
    // The session answers the next request.
    assert_eq!(
        request_discovery(&mut connection, 3),
        IpcResult::Overview(session_overview)
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    remove_test_directory(&binary_directory);
}

/// A restart with a pane whose terminal exposes no descriptor is refused
/// naming that pane. On Windows, `can_carry_panes` accepts every pane.
#[cfg(unix)]
#[test]
fn a_restart_with_a_pane_that_has_no_terminal_descriptor_is_refused_naming_that_pane() {
    let stranded_pane_id = PaneId::new();
    let carried_panes = [koshi_pty::backend::state::CarriedPtyPane {
        pane_id: stranded_pane_id,
        terminal_fd: None,
        process_id: 5000,
        pty_size: koshi_core::process::PtySize {
            column_count: 80,
            row_count: 24,
        },
        exit_status: None,
    }];
    let session_overview = build_overview_named("still-here");
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_restartable_test_server(
            "restart-no-fd",
            crate::server::can_carry_panes(&carried_panes),
            Some(session_overview.clone()),
        );

    let (mut connection, restart_result) = send_restart_request(&runtime_directory, session_id);

    assert_eq!(
        restart_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: format!(
                "pane {stranded_pane_id} has no terminal descriptor, \
                 so its terminal cannot cross the swap"
            ),
        }),
    );
    assert_eq!(
        request_discovery(&mut connection, 3),
        IpcResult::Overview(session_overview)
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_restart_on_an_attached_connection_detaches_that_client_and_restarts_nothing() {
    // On an attached connection, a `Restart` ends the client's stream, and no
    // restart request reaches the dispatcher.
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
        start_attachable_test_server("attached-restart", client_id);
    let mut connection = attach_test_client(&runtime_directory, session_id, client_id);

    connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Restart,
        })
        .expect("send restart");

    let RuntimeEvent::ClientDetached {
        client_id: detached_client_id,
        ..
    } = received_runtime_events.recv().expect("detach event")
    else {
        panic!("expected ClientDetached");
    };
    assert_eq!(detached_client_id, client_id);
    assert_eq!(
        connection.recv::<SessionEvent>().expect("goodbye frame"),
        SessionEvent::Detached,
    );
    assert!(
        matches!(
            connection.recv::<SessionEvent>(),
            Err(IpcError::Disconnected),
        ),
        "the stream ends after the goodbye, with no answer to the restart",
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

// --- leaving ---

/// Polls every 5 ms, for up to 5 seconds, until `ipc_server` counts
/// `expected_attached_connection_count` attached clients' connections. Returns
/// the last count. The count changes on the serving thread after the attach
/// reply is written.
fn wait_for_attached_connection_count(
    ipc_server: &IpcServer,
    expected_attached_connection_count: usize,
) -> usize {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while ipc_server.count_attached_connections() != expected_attached_connection_count
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    ipc_server.count_attached_connections()
}

#[test]
fn every_attached_clients_connection_is_counted_while_it_is_read() {
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, _received_runtime_events) =
        start_attachable_test_server("two-attached", client_id);

    let first_attached_connection = attach_test_client(&runtime_directory, session_id, client_id);
    let second_attached_connection = attach_test_client(&runtime_directory, session_id, client_id);
    assert_eq!(
        wait_for_attached_connection_count(&ipc_server, 2),
        2,
        "both attached clients' connections are counted"
    );

    drop(first_attached_connection);
    assert_eq!(
        wait_for_attached_connection_count(&ipc_server, 1),
        1,
        "the connection that is still read is the one left counted"
    );

    drop(second_attached_connection);
    assert_eq!(wait_for_attached_connection_count(&ipc_server, 0), 0);

    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn every_key_a_client_sent_reaches_the_dispatcher_before_that_client_leaves() {
    // A client that reads the restart frame sends `Leaving` and writes nothing
    // after it. Requests arrive in the order the client queued them: when
    // `Leaving` is read, the session holds every key that client typed.
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
        start_attachable_test_server("leaving", client_id);
    let mut connection = attach_test_client(&runtime_directory, session_id, client_id);
    assert_eq!(
        wait_for_attached_connection_count(&ipc_server, 1),
        1,
        "the attached client's connection is counted while it is read"
    );

    let typed_key_chords = [
        KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('a')),
        KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('b')),
        KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('c')),
    ];
    for (chord_index, typed_key_chord) in typed_key_chords.iter().enumerate() {
        connection
            .send(&IpcRequest {
                request_id: 3 + chord_index as u64,
                request_kind: IpcRequestKind::Keyboard {
                    key_input: build_key_input_for_chord(*typed_key_chord),
                },
            })
            .expect("send the key press");
    }
    connection
        .send(&IpcRequest {
            request_id: 6,
            request_kind: IpcRequestKind::Leaving,
        })
        .expect("send leaving");

    for typed_key_chord in typed_key_chords {
        let RuntimeEvent::ClientKeyboard {
            client_id: keyboard_client_id,
            key_input,
        } = received_runtime_events
            .recv_timeout(Duration::from_secs(5))
            .expect("key press event")
        else {
            panic!("expected ClientKeyboard");
        };
        assert_eq!(keyboard_client_id, client_id);
        assert_eq!(key_input, build_key_input_for_chord(typed_key_chord));
    }
    // The reading half ends on the request that follows those keys: the detach
    // comes after every key.
    let RuntimeEvent::ClientDetached {
        client_id: detached_client_id,
        is_streamed,
        ..
    } = received_runtime_events
        .recv_timeout(Duration::from_secs(5))
        .expect("detach event")
    else {
        panic!("expected ClientDetached");
    };
    assert_eq!(
        detached_client_id, client_id,
        "leaving detaches the client that left"
    );
    assert!(is_streamed, "the client that left was carrying a stream");
    assert_eq!(
        wait_for_attached_connection_count(&ipc_server, 0),
        0,
        "the connection is no longer counted once its client has left"
    );

    // The session closes the connection it was serving: the stream carries this
    // client's goodbye and then ends.
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("the goodbye frame"),
        SessionEvent::Detached,
    );
    assert!(matches!(
        connection.recv::<SessionEvent>(),
        Err(IpcError::Disconnected),
    ));

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_control_connection_that_leaves_is_closed_with_no_answer() {
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
        start_attachable_test_server("leaving-control", client_id);

    let mut connection = connect_to_session_socket(&runtime_directory, session_id);
    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Leaving,
        })
        .expect("send leaving");
    assert!(
        matches!(
            connection.recv::<IpcResponse>(),
            Err(IpcError::Disconnected),
        ),
        "a connection that leaves is closed with no answer",
    );
    // A control connection carries no client: no event about it reaches the
    // session.
    assert_eq!(
        received_runtime_events
            .recv_timeout(Duration::from_secs(2))
            .unwrap_err(),
        mpsc::RecvTimeoutError::Timeout,
    );
    assert_eq!(ipc_server.count_attached_connections(), 0);

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

// --- rotating the token ---

#[test]
fn a_rotated_token_is_advertised_and_the_one_before_it_is_refused() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("rotate-token", None);
    let initial_endpoint_file = load_test_endpoint_file(&runtime_directory, session_id);

    ipc_server
        .rotate_token()
        .expect("the fresh token is advertised");

    let rotated_endpoint_file = load_test_endpoint_file(&runtime_directory, session_id);
    assert_ne!(
        rotated_endpoint_file.connection_token, initial_endpoint_file.connection_token,
        "the rotation advertises a new secret",
    );
    assert_eq!(
        rotated_endpoint_file.socket_address, initial_endpoint_file.socket_address,
        "the address the session is serving on does not change",
    );

    let mut stale_connection = connect_to_session_socket(&runtime_directory, session_id);
    stale_connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                minimum_protocol_version: MIN_PROTOCOL_VERSION,
                maximum_protocol_version: PROTOCOL_VERSION,
                connection_token: initial_endpoint_file.connection_token,
                is_remote: false,
            },
        })
        .expect("send hello with the token from before the rotation");
    let refusal_response: IpcResponse = stale_connection.recv().expect("reply");
    assert_eq!(
        refusal_response.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let mut accepted_connection = connect_to_session_socket(&runtime_directory, session_id);
    accepted_connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello with the rotated secret");
    let hello_reply: IpcResponse = accepted_connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    drop(stale_connection);
    drop(accepted_connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn rotating_the_token_takes_connections_again_after_the_intake_closed() {
    // After `close_intake`, `rotate_token` opens the intake again on the same
    // socket.
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
        start_attachable_test_server("rotate-reopen", client_id);

    ipc_server.close_intake();
    ipc_server
        .rotate_token()
        .expect("the fresh token is advertised");

    let mut connection = connect_to_session_socket(&runtime_directory, session_id);
    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    // What this connection sends reaches the dispatcher again.
    let typed_key_chord = KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('r'));
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Attach {
                viewport_size: CLIENT_VIEWPORT_SIZE,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities: koshi_ipc::protocol::GraphicsCapabilities::default(),
                cell_size: None,
            },
        })
        .expect("send attach");
    let attach_reply: IpcResponse = connection.recv().expect("attach reply");
    assert_eq!(attach_reply.request_id, Some(2));
    connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Keyboard {
                key_input: build_key_input_for_chord(typed_key_chord),
            },
        })
        .expect("send the key press");
    let RuntimeEvent::ClientKeyboard {
        client_id: keyboard_client_id,
        key_input,
    } = received_runtime_events
        .recv_timeout(Duration::from_secs(5))
        .expect("key press")
    else {
        panic!("expected ClientKeyboard");
    };
    assert_eq!(keyboard_client_id, client_id);
    assert_eq!(key_input, build_key_input_for_chord(typed_key_chord));

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_rotation_that_cannot_advertise_the_fresh_token_refuses_the_one_before_it() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("rotate-write-fails", None);
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session_id);
    let initial_endpoint_file =
        EndpointFile::load_from_path(&endpoint_path).expect("endpoint file readable");

    // A directory where the endpoint file goes: the rotation mints the fresh
    // token and then cannot rename over it.
    std::fs::remove_file(&endpoint_path).expect("take the endpoint file away");
    std::fs::create_dir_all(&endpoint_path).expect("plant a directory in its place");

    let Err(IpcError::EndpointFileWrite {
        endpoint_file_path, ..
    }) = ipc_server.rotate_token()
    else {
        panic!("a rotation that cannot write the endpoint file must report it");
    };
    assert_eq!(endpoint_file_path, endpoint_path.display().to_string());

    // The server accepts only the fresh token: the token the endpoint file
    // advertised before the rotation opens nothing.
    let mut stale_connection =
        Connection::connect(&initial_endpoint_file.socket_address).expect("connect");
    stale_connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                minimum_protocol_version: MIN_PROTOCOL_VERSION,
                maximum_protocol_version: PROTOCOL_VERSION,
                connection_token: initial_endpoint_file.connection_token,
                is_remote: false,
            },
        })
        .expect("send hello with the token from before the rotation");
    let refusal_response: IpcResponse = stale_connection.recv().expect("reply");
    assert_eq!(
        refusal_response.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    drop(stale_connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

// --- closing the intake ---

#[test]
fn a_request_a_client_sends_after_the_intake_closes_never_reaches_the_dispatcher() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory("intake-closed");
    // The test keeps an inbox sender of its own. It queues the detach that ends
    // this client's writing thread after the intake is closed.
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (dispatcher_thread, received_runtime_events) =
        spawn_attaching_dispatcher(inbox_receiver, client_id, session_id);
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender.clone(), None)
        .expect("start serving");
    let mut connection = attach_test_client(&runtime_directory, session_id, client_id);
    let taken_key_chord = KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('a'));
    let refused_key_chord = KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('b'));

    // Before the close: the press reaches the dispatcher.
    connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Keyboard {
                key_input: build_key_input_for_chord(taken_key_chord),
            },
        })
        .expect("send the key press the session takes");
    let RuntimeEvent::ClientKeyboard {
        client_id: keyboard_client_id,
        key_input,
    } = received_runtime_events.recv().expect("key press event")
    else {
        panic!("expected ClientKeyboard");
    };
    assert_eq!(keyboard_client_id, client_id);
    assert_eq!(key_input, build_key_input_for_chord(taken_key_chord));

    ipc_server.close_intake();

    // After the close: the send fails outright, or the press is read and never
    // handed over. Either way no event reaches the dispatcher, including the
    // detach the connection's own ending queues.
    let _ = connection.send(&IpcRequest {
        request_id: 4,
        request_kind: IpcRequestKind::Keyboard {
            key_input: build_key_input_for_chord(refused_key_chord),
        },
    });
    assert_eq!(
        received_runtime_events
            .recv_timeout(Duration::from_secs(2))
            .unwrap_err(),
        mpsc::RecvTimeoutError::Timeout,
    );

    drop(connection);
    // The detach closes this client's queue and ends its writing thread. The
    // dispatcher ends once every inbox sender is gone.
    inbox_sender
        .send(RuntimeEvent::ClientDetached {
            client_id,
            detached_at: SystemTime::UNIX_EPOCH,
            is_streamed: true,
        })
        .expect("the detach is queued");
    drop(inbox_sender);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_connection_accepted_after_the_intake_closes_is_not_served() {
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
        start_attachable_test_server("intake-closed-accept", client_id);

    ipc_server.close_intake();

    let mut connection = connect_to_session_socket(&runtime_directory, session_id);
    // The send can fail when the accept loop drops the connection. The read
    // that follows reports end of stream in both cases.
    let _ = connection.send(&build_hello_request(&runtime_directory, session_id));
    assert!(
        matches!(
            connection.recv::<IpcResponse>(),
            Err(IpcError::Disconnected),
        ),
        "a connection accepted after the intake closed is closed unanswered",
    );
    assert_eq!(
        received_runtime_events
            .recv_timeout(Duration::from_secs(2))
            .unwrap_err(),
        mpsc::RecvTimeoutError::Timeout,
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

/// A detach event for `client_id` at timestamp `0`.
fn build_detach_event(client_id: ClientId) -> RuntimeEvent {
    RuntimeEvent::ClientDetached {
        client_id,
        detached_at: SystemTime::UNIX_EPOCH,
        is_streamed: true,
    }
}

#[test]
fn a_closed_intake_hands_nothing_over_until_it_reopens() {
    let intake = Intake::default();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let client_id = ClientId::new();

    assert!(intake.hand_over_event(&inbox_sender, build_detach_event(client_id)));
    intake.close_intake();
    assert!(!intake.hand_over_event(&inbox_sender, build_detach_event(client_id)));
    // Closing an intake that is already closed leaves it closed.
    intake.close_intake();
    assert!(!intake.hand_over_event(&inbox_sender, build_detach_event(client_id)));
    intake.reopen_intake();
    assert!(intake.hand_over_event(&inbox_sender, build_detach_event(client_id)));

    // The two the intake took, and neither of the two it refused.
    for _ in 0..2 {
        let RuntimeEvent::ClientDetached {
            client_id: detached_client_id,
            ..
        } = inbox_receiver
            .try_recv()
            .expect("the event was handed over")
        else {
            panic!("expected ClientDetached");
        };
        assert_eq!(detached_client_id, client_id);
    }
    assert_eq!(
        inbox_receiver.try_recv().unwrap_err(),
        mpsc::TryRecvError::Empty
    );
}

#[test]
fn an_intake_hands_nothing_over_once_the_dispatcher_is_gone() {
    let intake = Intake::default();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    drop(inbox_receiver);

    assert!(!intake.hand_over_event(&inbox_sender, build_detach_event(ClientId::new())));
}

#[test]
fn an_attached_clients_connection_is_counted_until_its_attachment_guard_is_dropped() {
    let intake = Arc::new(Intake::default());
    assert_eq!(intake.count_attached_connections(), 0);

    let first_attachment_guard = intake.record_attached_connection();
    let second_attachment_guard = intake.record_attached_connection();
    assert_eq!(intake.count_attached_connections(), 2);

    drop(first_attachment_guard);
    assert_eq!(intake.count_attached_connections(), 1);
    drop(second_attachment_guard);
    assert_eq!(intake.count_attached_connections(), 0);
}

/// A stand-in dispatcher that reports the `is_remote` flag of every attach it
/// is asked for and accepts each one as `client_id`. Holds the delivery queues
/// it hands out open, and the writing threads stay blocked. Exits when every
/// inbox sender is gone.
fn spawn_origin_reporting_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
    client_id: ClientId,
    session_id: SessionId,
) -> (JoinHandle<()>, Receiver<bool>) {
    let (remote_flag_sender, remote_flag_receiver) = mpsc::channel();
    let dispatcher_thread = std::thread::spawn(move || {
        let mut delivery_senders = Vec::new();
        let ending_notice = Arc::new(EndingNotice::default());
        while let Ok(runtime_event) = inbox_receiver.recv() {
            match runtime_event {
                RuntimeEvent::IpcAttach {
                    is_remote,
                    response_sender,
                    ..
                } => {
                    let (delivery_sender, delivery_receiver) = mpsc::channel();
                    delivery_senders.push(delivery_sender);
                    if remote_flag_sender.send(is_remote).is_err() {
                        break;
                    }
                    let _ = response_sender.send(Some(AttachAccepted {
                        client_id,
                        session_id,
                        session_structure: build_attached_structure(session_id),
                        deliveries: delivery_receiver,
                        ending_notice: Arc::clone(&ending_notice),
                        resume_token: ConnectionToken::from_secret(MINTED_CONNECTION_TOKEN),
                        pane_area: None,
                    }));
                }
                RuntimeEvent::ClientDetached { .. } => delivery_senders.clear(),
                _ => {}
            }
        }
    });
    (dispatcher_thread, remote_flag_receiver)
}

/// Opens a connection, sends a Hello with `is_remote`, attaches on it, and
/// checks both replies. The connection comes back carrying `client_id`'s
/// stream.
fn attach_saying_remote(
    runtime_directory: &Path,
    session_id: SessionId,
    client_id: ClientId,
    is_remote: bool,
) -> Connection {
    let endpoint_file = load_test_endpoint_file(runtime_directory, session_id);
    let mut connection = connect_to_session_socket(runtime_directory, session_id);
    connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                minimum_protocol_version: MIN_PROTOCOL_VERSION,
                maximum_protocol_version: PROTOCOL_VERSION,
                connection_token: endpoint_file.connection_token,
                is_remote,
            },
        })
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Attach {
                viewport_size: CLIENT_VIEWPORT_SIZE,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities: koshi_ipc::protocol::GraphicsCapabilities::default(),
                cell_size: None,
            },
        })
        .expect("send attach");
    let attach_reply: IpcResponse = connection.recv().expect("attach reply");
    assert_eq!(
        attach_reply.answer_result,
        IpcResult::Attached {
            client_id,
            session_id,
            session_structure: build_attached_structure(session_id),
            resume_token: Some(ConnectionToken::from_secret(MINTED_CONNECTION_TOKEN)),
            pane_area: None,
        },
    );
    connection
}

/// The attach the dispatcher is asked for is marked remote exactly when the
/// Hello that opened the connection said so.
#[test]
fn an_attach_is_marked_remote_exactly_when_its_hello_named_another_machine() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory("attach-origin");
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (dispatcher_thread, received_remote_flags) =
        spawn_origin_reporting_dispatcher(inbox_receiver, client_id, session_id);
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None)
        .expect("start serving");

    let local_connection = attach_saying_remote(&runtime_directory, session_id, client_id, false);
    assert_eq!(
        received_remote_flags.recv_timeout(Duration::from_secs(5)),
        Ok(false),
        "a hello naming no other machine leaves the attach it carries local",
    );

    let remote_connection = attach_saying_remote(&runtime_directory, session_id, client_id, true);
    assert_eq!(
        received_remote_flags.recv_timeout(Duration::from_secs(5)),
        Ok(true),
        "a hello naming another machine marks the attach it carries remote",
    );

    drop(local_connection);
    drop(remote_connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}
