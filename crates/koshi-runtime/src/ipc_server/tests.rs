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
use koshi_core::key::{Key, KeyChord, ModFlags};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::layout::SessionLayout;
use koshi_ipc::protocol::{
    EventFilterSpec, GraphicsCapabilities, IpcRequest, WireMouseAction, MIN_PROTOCOL_VERSION,
    PROTOCOL_VERSION,
};
use koshi_layout::mode::LayoutMode;
use koshi_renderer::snapshot::{
    ClientSnapshot, CursorSnapshot, ImagePlacementSnapshot, PaneSnapshot, PluginUiSnapshot,
    RenderSnapshot, ScrollbackMeta, SessionSnapshot, TabSnapshot,
};
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDisplay, ImageRecord,
};

use crate::runtime::event::{AttachAccepted, EndingNotice, SessionEnding};

use super::*;

/// The terminal size every attaching client in these tests reports.
const TEST_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// The secret every stand-in attach mints, so an assertion names the exact
/// token the reply carries.
const MINTED_CONNECTION_TOKEN: &str =
    "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";

/// A fresh directory to stand in for the runtime directory, under a short base so
/// the Unix socket path stays inside the OS path-length cap.
/// [`IpcServer::start`] creates it private itself.
fn build_test_runtime_directory(tag: &str) -> PathBuf {
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    base.join(format!("koshi-serve-{}-{tag}", std::process::id()))
}

/// Remove a directory a test made, and everything inside it. A directory that
/// is already gone is left alone.
fn cleanup(runtime_directory: &Path) {
    let _ = std::fs::remove_dir_all(runtime_directory);
}

/// A fresh directory to stand in for the machine-wide shared directory, under
/// a short base so the Unix socket path stays inside the OS path-length cap.
/// [`IpcServer::start`] creates it and this user's directory inside it.
fn build_test_shared_directory(tag: &str) -> PathBuf {
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    base.join(format!("koshi-shared-{}-{tag}", std::process::id()))
}

/// A stand-in for the dispatcher thread: drains the inbox, answers every
/// submitted command with `Ok` echoing its id, and every discovery request
/// with `overview`. Exits when every inbox sender is gone.
fn spawn_dispatcher(
    inbox_rx: Receiver<RuntimeEvent>,
    overview: Option<SessionOverview>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while let Ok(event) = inbox_rx.recv() {
            match event {
                RuntimeEvent::Ipc {
                    envelope,
                    response_sender,
                } => {
                    let _ = response_sender.send(CommandResult::Ok {
                        command_id: envelope.command_id,
                        emitted_events: Vec::new(),
                    });
                }
                RuntimeEvent::IpcDiscovery { response_sender } => {
                    let _ = response_sender.send(overview.clone());
                }
                _ => {}
            }
        }
    })
}

/// The structure a stand-in attach answers with: the session, named, with
/// nothing in it.
fn attached_structure(session_id: SessionId) -> AttachedSessionStructureSnapshot {
    AttachedSessionStructureSnapshot {
        session_id,
        session_name: "attachable".to_string(),
        tabs: Vec::new(),
        panes: Vec::new(),
    }
}

/// One image-bearing frame for an attached stream.
fn image_snapshot(client_id: ClientId, image_record: Arc<ImageRecord>) -> RenderSnapshot {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    RenderSnapshot {
        session_snapshot: SessionSnapshot {
            session_id,
            session_name: String::from("session"),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: String::from("tab"),
                pane_slots: Vec::new(),
                effective_cell_size: TEST_VIEWPORT_SIZE,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                are_all_panes_suppressed: false,
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
            scrollback_meta: ScrollbackMeta {
                is_truncated: false,
                retained_line_count: 0,
            },
        }],
        client_snapshot: ClientSnapshot {
            client_id,
            viewport_size: TEST_VIEWPORT_SIZE,
            active_tab_id: tab_id,
            focused_pane_id: Some(pane_id),
            lock_mode: LockMode::Normal,
            is_mouse_selection_enabled: false,
        },
        plugin_ui_snapshot: PluginUiSnapshot::default(),
    }
}

/// One one-pixel image whose byte makes image record changes visible.
fn image_record(red: u8) -> Arc<ImageRecord> {
    Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: (DecodedImage {
            pixel_width: 1,
            pixel_height: 1,
            rgba_bytes: vec![red, 0, 0, 255],
        })
        .into(),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay::default(),
        anchor: (0, 0),
    })
}

/// A dispatcher that accepts one attach and exposes its event queue sender.
fn spawn_frame_dispatcher(
    inbox_rx: Receiver<RuntimeEvent>,
    client_id: ClientId,
    session_id: SessionId,
) -> (JoinHandle<()>, Sender<Delivery>) {
    let (events_tx, events_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut events = Some(events_rx);
        let ending_notice = Arc::new(EndingNotice::default());
        while let Ok(event) = inbox_rx.recv() {
            if let RuntimeEvent::IpcAttach {
                response_sender, ..
            } = event
            {
                let Some(events) = events.take() else {
                    continue;
                };
                let _ = response_sender.send(Some(AttachAccepted {
                    client_id,
                    session_id,
                    session_structure: attached_structure(session_id),
                    deliveries: events,
                    ending_notice: Arc::clone(&ending_notice),
                    resume_token: ConnectionToken::from_secret(MINTED_CONNECTION_TOKEN),
                    pane_area: None,
                }));
            }
        }
    });
    (handle, events_tx)
}

/// A stand-in dispatcher that accepts attaches: it answers every attach as
/// `client_id`, holds the queue it hands out open so the writing thread stays
/// blocked, and closes those queues on a detach the way the real dispatcher
/// does. Every other event it drains is forwarded to the returned receiver, so
/// a test reads exactly what an attached connection sent. Exits when every
/// inbox sender is gone.
fn spawn_attaching_dispatcher(
    inbox_rx: Receiver<RuntimeEvent>,
    client_id: ClientId,
    session_id: SessionId,
) -> (JoinHandle<()>, Receiver<RuntimeEvent>) {
    let (seen_tx, seen_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut queues = Vec::new();
        let ending_notice = Arc::new(EndingNotice::default());
        while let Ok(event) = inbox_rx.recv() {
            match event {
                RuntimeEvent::IpcAttach {
                    response_sender, ..
                } => {
                    let (events_tx, events_rx) = mpsc::channel();
                    queues.push(events_tx);
                    let _ = response_sender.send(Some(AttachAccepted {
                        client_id,
                        session_id,
                        session_structure: attached_structure(session_id),
                        deliveries: events_rx,
                        ending_notice: Arc::clone(&ending_notice),
                        resume_token: ConnectionToken::from_secret(MINTED_CONNECTION_TOKEN),
                        pane_area: None,
                    }));
                }
                detached @ RuntimeEvent::ClientDetached { .. } => {
                    queues.clear();
                    if seen_tx.send(detached).is_err() {
                        break;
                    }
                }
                other => {
                    if seen_tx.send(other).is_err() {
                        break;
                    }
                }
            }
        }
    });
    (handle, seen_rx)
}

/// A stand-in dispatcher that answers the first attach with `events` and
/// `ending_notice`, and drops everything else it drains. Exits when every
/// inbox sender is gone.
fn spawn_ending_dispatcher(
    inbox_rx: Receiver<RuntimeEvent>,
    client_id: ClientId,
    session_id: SessionId,
    events: Receiver<Delivery>,
    ending_notice: Arc<EndingNotice>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut queue = Some(events);
        while let Ok(event) = inbox_rx.recv() {
            if let RuntimeEvent::IpcAttach {
                response_sender, ..
            } = event
            {
                let Some(events) = queue.take() else {
                    continue;
                };
                let _ = response_sender.send(Some(AttachAccepted {
                    client_id,
                    session_id,
                    session_structure: attached_structure(session_id),
                    deliveries: events,
                    ending_notice: Arc::clone(&ending_notice),
                    resume_token: ConnectionToken::from_secret(MINTED_CONNECTION_TOKEN),
                    pane_area: None,
                }));
            }
        }
    })
}

/// Wait until no client writing thread is left on `notice`, and hand back how
/// many are. Fails the test rather than hanging if one never ends.
fn wait_for_writers_to_end(notice: &EndingNotice) -> usize {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while notice.count_running_writers() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    notice.count_running_writers()
}

#[test]
fn a_client_whose_queue_is_full_is_still_told_the_session_is_restarting() {
    // A client's queue is bounded and the restart is published onto it like any
    // other event, so a client whose queue is full never takes it.
    // That client would read end of stream when the image is replaced and
    // report the session dead, instead of coming back on its new socket.
    use koshi_core::event::{Event, TabCreated};

    use crate::runtime::bus::{EventBus, EventFilter, SUBSCRIBER_QUEUE_CAPACITY};

    let client = ClientId::new();
    let session = SessionId::new();
    let runtime_directory = build_test_runtime_directory("restart-full-queue");

    let mut bus = EventBus::new();
    let (_, events) = bus.subscribe(EventFilter::All);
    let tab = TabId::new();
    for _ in 0..SUBSCRIBER_QUEUE_CAPACITY {
        bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));
    }
    // The announcement: publishing the restart raises the notice and puts the
    // event on a queue with no room left for it.
    let notice = Arc::clone(bus.ending_notice());
    bus.publish(&Event::Restarting);
    assert_eq!(notice.get_session_ending(), Some(SessionEnding::Restarting));

    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher =
        spawn_ending_dispatcher(inbox_rx, client, session, events, Arc::clone(&notice));
    let server =
        IpcServer::start(&runtime_directory, session, inbox_tx, None).expect("start serving");

    let mut connection = attach_to(&runtime_directory, session, client);

    // The first frame, not a frame somewhere behind the backlog: the queue
    // still holds its full 1024 deliveries, and none of them is written.
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("the client is told"),
        SessionEvent::Restarting,
    );
    assert_eq!(
        wait_for_writers_to_end(&notice),
        0,
        "the writing thread must end once the client holds the restart frame"
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_client_whose_queue_is_full_is_still_told_the_session_ended() {
    // A client's queue is bounded and the quit is published onto it like any
    // other event, so a client whose queue is full never takes it. That client
    // would read end of stream when the session ends and report it dead,
    // instead of saying the session ended.
    use koshi_core::event::{Event, TabCreated};

    use crate::runtime::bus::{EventBus, EventFilter, SUBSCRIBER_QUEUE_CAPACITY};

    let client = ClientId::new();
    let session = SessionId::new();
    let runtime_directory = build_test_runtime_directory("quit-full-queue");

    let mut bus = EventBus::new();
    let (_, events) = bus.subscribe(EventFilter::All);
    let tab = TabId::new();
    for _ in 0..SUBSCRIBER_QUEUE_CAPACITY {
        bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));
    }
    // The announcement: publishing the quit raises the notice and puts the
    // event on a queue with no room left for it.
    let notice = Arc::clone(bus.ending_notice());
    bus.publish(&Event::Quit);
    assert_eq!(notice.get_session_ending(), Some(SessionEnding::Quit));

    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher =
        spawn_ending_dispatcher(inbox_rx, client, session, events, Arc::clone(&notice));
    let server =
        IpcServer::start(&runtime_directory, session, inbox_tx, None).expect("start serving");

    let mut connection = attach_to(&runtime_directory, session, client);

    // The first frame, not a frame somewhere behind the backlog: the queue
    // still holds its full 1024 deliveries, and none of them is written.
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("the client is told"),
        SessionEvent::Quit,
    );
    assert_eq!(
        wait_for_writers_to_end(&notice),
        0,
        "the writing thread must end once the client holds the quit frame"
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_client_the_server_detached_reads_its_own_goodbye_when_the_session_ends() {
    // `auto-close-session` ends the session the moment its last client
    // detaches, so the notice is raised while that client's writing thread
    // still holds frames to write. That client asked to leave, so the detach is
    // what it reads.
    use koshi_core::event::{Event, TabCreated};

    use crate::runtime::bus::{EventBus, EventFilter};

    let client = ClientId::new();
    let session = SessionId::new();
    let runtime_directory = build_test_runtime_directory("detach-then-quit");

    let mut bus = EventBus::new();
    let (subscriber, events) = bus.subscribe(EventFilter::All);
    bus.publish(&Event::TabCreated(TabCreated {
        tab_id: TabId::new(),
    }));
    // The detach closes the queue behind the frame it already holds; the
    // session ends right after.
    bus.unsubscribe(subscriber);
    let notice = Arc::clone(bus.ending_notice());
    bus.publish(&Event::Quit);
    assert_eq!(notice.get_session_ending(), Some(SessionEnding::Quit));

    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher =
        spawn_ending_dispatcher(inbox_rx, client, session, events, Arc::clone(&notice));
    let server =
        IpcServer::start(&runtime_directory, session, inbox_tx, None).expect("start serving");

    let mut connection = attach_to(&runtime_directory, session, client);

    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("the client is told"),
        SessionEvent::Detached,
    );
    assert_eq!(
        wait_for_writers_to_end(&notice),
        0,
        "the writing thread must end once the client holds the detach frame"
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_client_reads_the_quit_frame_alone_when_the_events_that_ended_the_session_are_still_queued() {
    // The events that end a session and the quit itself are published in one
    // pass, so a client's queue can hold both when its writing thread takes its
    // first turn. The raised notice is what that thread writes, whatever the
    // queue still holds: the pane's exit is queued ahead of the quit here, and
    // the client reads the quit alone.
    use koshi_core::event::{Event, PaneProcessExited};

    use crate::runtime::bus::{EventBus, EventFilter};

    let client = ClientId::new();
    let session = SessionId::new();
    let runtime_directory = build_test_runtime_directory("quit-behind-queue");

    let mut bus = EventBus::new();
    let (_, events) = bus.subscribe(EventFilter::All);
    let pane = PaneId::new();
    bus.publish(&Event::PaneProcessExited(PaneProcessExited {
        pane_id: pane,
        exit_code: Some(0),
    }));
    // The announcement: the queue has room, so the quit is queued behind the
    // exit and the notice is raised as well.
    let notice = Arc::clone(bus.ending_notice());
    bus.publish(&Event::Quit);
    assert_eq!(notice.get_session_ending(), Some(SessionEnding::Quit));

    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher =
        spawn_ending_dispatcher(inbox_rx, client, session, events, Arc::clone(&notice));
    let server =
        IpcServer::start(&runtime_directory, session, inbox_tx, None).expect("start serving");

    let mut connection = attach_to(&runtime_directory, session, client);

    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("the client is told"),
        SessionEvent::Quit,
    );
    assert_eq!(
        wait_for_writers_to_end(&notice),
        0,
        "the writing thread must end once the client holds the quit frame"
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

/// A served socket whose stand-in dispatcher accepts an attach as `client_id`,
/// plus the events that attached connection sends the dispatcher.
fn serve_attachable(
    tag: &str,
    client_id: ClientId,
) -> (
    IpcServer,
    SessionId,
    PathBuf,
    JoinHandle<()>,
    Receiver<RuntimeEvent>,
) {
    let runtime_directory = build_test_runtime_directory(tag);
    let session = SessionId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let (dispatcher, seen) = spawn_attaching_dispatcher(inbox_rx, client_id, session);
    let server =
        IpcServer::start(&runtime_directory, session, inbox_tx, None).expect("start serving");
    (server, session, runtime_directory, dispatcher, seen)
}

/// Open a connection, say hello, attach on it, and read both replies back.
/// The connection comes back carrying `client_id`'s stream.
fn attach_to(runtime_directory: &Path, session: SessionId, client_id: ClientId) -> Connection {
    attach_to_with_graphics(
        runtime_directory,
        session,
        client_id,
        GraphicsCapabilities::default(),
    )
}

/// Open and attach one connection with the terminal graphics report supplied.
fn attach_to_with_graphics(
    runtime_directory: &Path,
    session: SessionId,
    client_id: ClientId,
    graphics_capabilities: GraphicsCapabilities,
) -> Connection {
    let mut connection = connect_to(runtime_directory, session);
    connection
        .send(&hello_for(runtime_directory, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, hello_accepted());

    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Attach {
                viewport: TEST_VIEWPORT_SIZE,
                event_filter: EventFilterSpec::All,
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
            session_id: session,
            session_structure: attached_structure(session),
            resume_token: Some(ConnectionToken::from_secret(MINTED_CONNECTION_TOKEN)),
            pane_area: None,
        },
    );
    connection
}

#[test]
fn an_attach_forwards_its_initial_cell_measurement_before_the_session_reply() {
    let client = ClientId::new();
    let session = SessionId::new();
    let runtime_directory = build_test_runtime_directory("attach-cell-size");
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let server =
        IpcServer::start(&runtime_directory, session, inbox_tx, None).expect("start serving");
    let mut connection = connect_to(&runtime_directory, session);
    connection
        .send(&hello_for(&runtime_directory, session))
        .expect("send hello");
    let _: IpcResponse = connection.recv().expect("hello reply");
    let measurement =
        PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions");
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Attach {
                viewport: TEST_VIEWPORT_SIZE,
                event_filter: EventFilterSpec::All,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities: GraphicsCapabilities::default(),
                cell_size: Some(measurement),
            },
        })
        .expect("send attach");

    let RuntimeEvent::IpcAttach {
        cell_size,
        response_sender,
        ..
    } = inbox_rx.recv().expect("attach reaches the dispatcher")
    else {
        panic!("expected IpcAttach");
    };
    assert_eq!(cell_size, Some(measurement));
    let (events_tx, events_rx) = mpsc::channel();
    response_sender
        .send(Some(AttachAccepted {
            client_id: client,
            session_id: session,
            session_structure: attached_structure(session),
            deliveries: events_rx,
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
                viewport: TEST_VIEWPORT_SIZE,
                pane_area: None,
                cell_size: Some(measurement),
            },
        })
        .expect("send resize");
    let RuntimeEvent::Resize { cell_size, .. } =
        inbox_rx.recv().expect("resize reaches the dispatcher")
    else {
        panic!("expected Resize");
    };
    assert_eq!(cell_size, Some(measurement));

    drop(events_tx);
    drop(connection);
    server.shutdown();
    cleanup(&runtime_directory);
}

#[test]
fn an_unsupported_terminal_receives_placement_geometry_and_no_pixel_events() {
    let runtime_directory = build_test_runtime_directory("unsupported-image-stream");
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let (dispatcher, events) = spawn_frame_dispatcher(inbox_rx, client_id, session_id);
    let server =
        IpcServer::start(&runtime_directory, session_id, inbox_tx, None).expect("start serving");
    let mut connection = attach_to_with_graphics(
        &runtime_directory,
        session_id,
        client_id,
        GraphicsCapabilities::default(),
    );
    let snapshot = image_snapshot(client_id, image_record(1));
    events
        .send(Delivery::Frame(Box::new(snapshot.clone())))
        .expect("send the image frame");
    events
        .send(Delivery::HostWrite(vec![9]))
        .expect("send the event after the image frame");

    assert_eq!(
        connection.recv::<SessionEvent>().expect("read the frame"),
        SessionEvent::Painted {
            frame: Box::new(wire_frame(&snapshot)),
        }
    );
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("read the next event"),
        SessionEvent::HostWrite {
            host_output_bytes: vec![9]
        }
    );

    drop(connection);
    drop(events);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_kitty_terminal_receives_pixels_once_then_placement_only_frames() {
    let runtime_directory = build_test_runtime_directory("cached-image-stream");
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let (dispatcher, events) = spawn_frame_dispatcher(inbox_rx, client_id, session_id);
    let server =
        IpcServer::start(&runtime_directory, session_id, inbox_tx, None).expect("start serving");
    let mut connection = attach_to_with_graphics(
        &runtime_directory,
        session_id,
        client_id,
        GraphicsCapabilities {
            supports_kitty: true,
            supports_iterm: false,
            supports_sixel: false,
        },
    );
    let image_record = image_record(1);
    let snapshot = image_snapshot(client_id, Arc::clone(&image_record));
    events
        .send(Delivery::Frame(Box::new(snapshot.clone())))
        .expect("send the first image frame");
    events
        .send(Delivery::Frame(Box::new(snapshot.clone())))
        .expect("send the repeated image frame");
    events
        .send(Delivery::HostWrite(vec![9]))
        .expect("send the event after both frames");

    let painted = SessionEvent::Painted {
        frame: Box::new(wire_frame(&snapshot)),
    };
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("read the first frame"),
        painted
    );
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("read the image start"),
        SessionEvent::ImageContentStart {
            image_transfer: wire_image_transfer(1, &image_record),
        }
    );
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("read the image bytes"),
        SessionEvent::ImageContentChunk {
            image_chunk: FrameImageChunk {
                image_transfer_id: 1,
                byte_offset: 0,
                is_last: true,
                chunk_bytes: vec![1, 0, 0, 255],
            },
        }
    );
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("read the placement-only frame"),
        painted
    );
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("read the next event"),
        SessionEvent::HostWrite {
            host_output_bytes: vec![9]
        }
    );

    drop(connection);
    drop(events);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn image_scroll_return_uses_a_new_identity_and_complete_transfer() {
    let runtime_directory = build_test_runtime_directory("image-scroll-return");
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let (dispatcher, events) = spawn_frame_dispatcher(inbox_rx, client_id, session_id);
    let server =
        IpcServer::start(&runtime_directory, session_id, inbox_tx, None).expect("start serving");
    let mut connection = attach_to_with_graphics(
        &runtime_directory,
        session_id,
        client_id,
        GraphicsCapabilities {
            supports_kitty: true,
            supports_iterm: false,
            supports_sixel: false,
        },
    );
    let first_record = image_record(1);
    let visible = image_snapshot(client_id, Arc::clone(&first_record));
    let mut absent = visible.clone();
    absent.pane_snapshots[0].image_placement_snapshots.clear();
    let changed_record = image_record(2);
    let mut changed = visible.clone();
    changed.pane_snapshots[0].image_placement_snapshots[0] =
        ImagePlacementSnapshot::with_content_id(7, 1, Arc::clone(&changed_record), (0, 0), 1, 1)
            .expect("the changed placement is valid");
    for snapshot in [&visible, &absent, &visible, &absent, &changed] {
        events
            .send(Delivery::Frame(Box::new(snapshot.clone())))
            .expect("send an image visibility frame");
    }
    events
        .send(Delivery::HostWrite(vec![9]))
        .expect("send the event after the image frames");

    for (snapshot, content_id, image_record) in [
        (&visible, 1, Some(&first_record)),
        (&absent, 0, None),
        (&visible, 2, Some(&first_record)),
        (&absent, 0, None),
        (&changed, 3, Some(&changed_record)),
    ] {
        let mut frame = wire_frame(snapshot);
        if let Some(placement) = frame
            .pane_snapshots
            .first_mut()
            .and_then(|pane_snapshot| pane_snapshot.image_placement_snapshots.first_mut())
        {
            placement.image_content_id = content_id;
        }
        assert_eq!(
            connection.recv::<SessionEvent>().expect("read a frame"),
            SessionEvent::Painted {
                frame: Box::new(frame),
            }
        );
        let Some(image_record) = image_record else {
            continue;
        };
        assert_eq!(
            connection
                .recv::<SessionEvent>()
                .expect("read an image start"),
            SessionEvent::ImageContentStart {
                image_transfer: wire_image_transfer(content_id, image_record),
            }
        );
        assert_eq!(
            connection.recv::<SessionEvent>().expect("read image bytes"),
            SessionEvent::ImageContentChunk {
                image_chunk: FrameImageChunk {
                    image_transfer_id: content_id,
                    byte_offset: 0,
                    is_last: true,
                    chunk_bytes: image_record.image.rgba_bytes.clone(),
                },
            }
        );
    }
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("read the next event"),
        SessionEvent::HostWrite {
            host_output_bytes: vec![9]
        }
    );

    drop(connection);
    drop(events);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn any_native_terminal_receives_image_content() {
    for (tag, graphics) in [
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
        let runtime_directory = build_test_runtime_directory(tag);
        let session_id = SessionId::new();
        let client_id = ClientId::new();
        let (inbox_tx, inbox_rx) = mpsc::channel();
        let (dispatcher, events) = spawn_frame_dispatcher(inbox_rx, client_id, session_id);
        let server = IpcServer::start(&runtime_directory, session_id, inbox_tx, None)
            .expect("start serving");
        let mut connection =
            attach_to_with_graphics(&runtime_directory, session_id, client_id, graphics);
        let image_record = image_record(1);
        let snapshot = image_snapshot(client_id, Arc::clone(&image_record));
        events
            .send(Delivery::Frame(Box::new(snapshot.clone())))
            .expect("send the image frame");

        assert_eq!(
            connection.recv::<SessionEvent>().expect("read the frame"),
            SessionEvent::Painted {
                frame: Box::new(wire_frame(&snapshot)),
            }
        );
        assert_eq!(
            connection
                .recv::<SessionEvent>()
                .expect("read the image start"),
            SessionEvent::ImageContentStart {
                image_transfer: wire_image_transfer(1, &image_record),
            }
        );
        assert_eq!(
            connection
                .recv::<SessionEvent>()
                .expect("read the image bytes"),
            SessionEvent::ImageContentChunk {
                image_chunk: FrameImageChunk {
                    image_transfer_id: 1,
                    byte_offset: 0,
                    is_last: true,
                    chunk_bytes: vec![1, 0, 0, 255],
                },
            }
        );

        drop(connection);
        drop(events);
        server.shutdown();
        dispatcher.join().expect("dispatcher exits");
        cleanup(&runtime_directory);
    }
}

#[test]
fn two_placements_of_one_record_share_one_content_transfer() {
    let client_id = ClientId::new();
    let image_record = image_record(1);
    let mut snapshot = image_snapshot(client_id, Arc::clone(&image_record));
    snapshot.pane_snapshots[0].image_placement_snapshots.push(
        ImagePlacementSnapshot::with_content_id(8, 2, Arc::clone(&image_record), (0, 1), 1, 1)
            .expect("the second placement is valid"),
    );
    let mut cache = ConnectionImageCache::new();

    let prepared = cache.prepare_image_frame(&snapshot);

    assert_eq!(prepared.image_uploads.len(), 1);
    assert_eq!(prepared.image_uploads[0].0, 1);
    assert!(Arc::ptr_eq(&prepared.image_uploads[0].1, &image_record));
    assert_eq!(
        prepared.painted_frame.pane_snapshots[0].image_placement_snapshots[0].image_content_id,
        1
    );
    assert_eq!(
        prepared.painted_frame.pane_snapshots[0].image_placement_snapshots[1].image_content_id,
        1
    );
}

#[test]
fn a_kitty_terminal_receives_more_than_4096_placements_in_bounded_batches() {
    let runtime_directory = build_test_runtime_directory("bounded-image-batches");
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let (dispatcher, events) = spawn_frame_dispatcher(inbox_rx, client_id, session_id);
    let server =
        IpcServer::start(&runtime_directory, session_id, inbox_tx, None).expect("start serving");
    let mut connection = attach_to_with_graphics(
        &runtime_directory,
        session_id,
        client_id,
        GraphicsCapabilities {
            supports_kitty: true,
            supports_iterm: false,
            supports_sixel: false,
        },
    );
    let mut snapshot = image_snapshot(client_id, image_record(1));
    snapshot.pane_snapshots[0].image_placement_snapshots = (1..=MAX_FRAME_IMAGE_TRANSFER_COUNT)
        .map(|placement_index| {
            let content_id = u64::try_from(placement_index).expect("the content identity fits");
            ImagePlacementSnapshot::with_content_id(
                content_id,
                content_id,
                image_record((content_id % 251) as u8),
                (0, 0),
                1,
                1,
            )
            .expect("the image placement is valid")
        })
        .collect();
    let mut second_pane = snapshot.pane_snapshots[0].clone();
    second_pane.pane_id = PaneId::new();
    let last_image_content_id =
        u64::try_from(MAX_FRAME_IMAGE_TRANSFER_COUNT + 1).expect("the content identity fits");
    second_pane.image_placement_snapshots = vec![ImagePlacementSnapshot::with_content_id(
        1,
        last_image_content_id,
        image_record((last_image_content_id % 251) as u8),
        (0, 0),
        1,
        1,
    )
    .expect("the image placement is valid")];
    snapshot.pane_snapshots.push(second_pane);
    events
        .send(Delivery::Frame(Box::new(snapshot.clone())))
        .expect("send the image frame");
    events
        .send(Delivery::HostWrite(vec![9]))
        .expect("send the event after the image frame");
    let painted = SessionEvent::Painted {
        frame: Box::new(wire_frame(&snapshot)),
    };

    assert_eq!(
        connection.recv::<SessionEvent>().expect("read batch one"),
        painted
    );
    for image_transfer_index in 1..=MAX_FRAME_IMAGE_TRANSFER_COUNT {
        let image_content_id =
            u64::try_from(image_transfer_index).expect("the content identity fits");
        let red = (image_content_id % 251) as u8;
        assert_eq!(
            connection
                .recv::<SessionEvent>()
                .expect("read an image start"),
            SessionEvent::ImageContentStart {
                image_transfer: wire_image_transfer(image_content_id, &image_record(red)),
            }
        );
        assert_eq!(
            connection.recv::<SessionEvent>().expect("read image bytes"),
            SessionEvent::ImageContentChunk {
                image_chunk: FrameImageChunk {
                    image_transfer_id: image_content_id,
                    byte_offset: 0,
                    is_last: true,
                    chunk_bytes: vec![red, 0, 0, 255],
                },
            }
        );
    }
    assert_eq!(
        connection.recv::<SessionEvent>().expect("read batch two"),
        painted
    );
    let last_red = (last_image_content_id % 251) as u8;
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("read the last image start"),
        SessionEvent::ImageContentStart {
            image_transfer: wire_image_transfer(last_image_content_id, &image_record(last_red),),
        }
    );
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("read the last image bytes"),
        SessionEvent::ImageContentChunk {
            image_chunk: FrameImageChunk {
                image_transfer_id: last_image_content_id,
                byte_offset: 0,
                is_last: true,
                chunk_bytes: vec![last_red, 0, 0, 255],
            },
        }
    );
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("read the next event"),
        SessionEvent::HostWrite {
            host_output_bytes: vec![9]
        }
    );

    drop(connection);
    drop(events);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn replacing_one_placement_record_assigns_a_new_content_identity() {
    let client_id = ClientId::new();
    let first_record = image_record(1);
    let initial_image_snapshot = image_snapshot(client_id, Arc::clone(&first_record));
    let mut replacement_image_snapshot = initial_image_snapshot.clone();
    let pane_id = replacement_image_snapshot.pane_snapshots[0].pane_id;
    let second_record = image_record(2);
    replacement_image_snapshot.pane_snapshots[0].image_placement_snapshots[0] =
        ImagePlacementSnapshot::with_content_id(7, 1, Arc::clone(&second_record), (0, 0), 1, 1)
            .expect("the replacement placement is valid");
    let mut cache = ConnectionImageCache::new();

    let initial_prepared_frame = cache.prepare_image_frame(&initial_image_snapshot);
    let replacement_prepared_frame = cache.prepare_image_frame(&replacement_image_snapshot);

    assert_eq!(
        initial_prepared_frame.painted_frame.pane_snapshots[0].image_placement_snapshots[0]
            .image_content_id,
        1
    );
    assert_eq!(initial_prepared_frame.image_uploads.len(), 1);
    assert_eq!(initial_prepared_frame.image_uploads[0].0, 1);
    assert!(Arc::ptr_eq(
        &initial_prepared_frame.image_uploads[0].1,
        &first_record
    ));
    assert_eq!(
        replacement_prepared_frame.painted_frame.pane_snapshots[0].pane_id,
        pane_id
    );
    assert_eq!(
        replacement_prepared_frame.painted_frame.pane_snapshots[0].image_placement_snapshots[0]
            .image_content_id,
        2
    );
    assert_eq!(replacement_prepared_frame.image_uploads.len(), 1);
    assert_eq!(replacement_prepared_frame.image_uploads[0].0, 2);
    assert!(Arc::ptr_eq(
        &replacement_prepared_frame.image_uploads[0].1,
        &second_record
    ));
}

#[test]
fn exhausted_content_id_space_resets_the_connection_cache_before_reuse() {
    let client_id = ClientId::new();
    let initial_image_snapshot = image_snapshot(client_id, image_record(1));
    let mut replacement_image_snapshot = initial_image_snapshot.clone();
    replacement_image_snapshot.pane_snapshots[0].image_placement_snapshots[0] =
        ImagePlacementSnapshot::with_content_id(7, 1, image_record(2), (0, 0), 1, 1)
            .expect("the replacement placement is valid");
    let mut cache = ConnectionImageCache::new();
    cache.next_image_content_id = u64::MAX;

    let prepared_frame_before_reset = cache.prepare_image_frame(&initial_image_snapshot);
    let prepared_frame_after_reset = cache.prepare_image_frame(&replacement_image_snapshot);

    assert!(!prepared_frame_before_reset.should_reset_image_cache);
    assert_eq!(
        prepared_frame_before_reset.painted_frame.pane_snapshots[0].image_placement_snapshots[0]
            .image_content_id,
        u64::MAX
    );
    assert!(prepared_frame_after_reset.should_reset_image_cache);
    assert_eq!(
        prepared_frame_after_reset.painted_frame.pane_snapshots[0].image_placement_snapshots[0]
            .image_content_id,
        1
    );
    assert_eq!(cache.next_image_content_id, 2);
}

#[test]
fn clearing_after_a_write_failure_resets_the_client_before_reusing_content_ids() {
    let client_id = ClientId::new();
    let snapshot = image_snapshot(client_id, image_record(1));
    let mut cache = ConnectionImageCache::new();

    let before_clear = cache.prepare_image_frame(&snapshot);
    cache.clear_image_cache();
    let after_clear = cache.prepare_image_frame(&snapshot);

    assert!(!before_clear.should_reset_image_cache);
    assert_eq!(
        before_clear.painted_frame.pane_snapshots[0].image_placement_snapshots[0].image_content_id,
        1
    );
    assert!(after_clear.should_reset_image_cache);
    assert_eq!(
        after_clear.painted_frame.pane_snapshots[0].image_placement_snapshots[0].image_content_id,
        1
    );
    assert_eq!(after_clear.image_uploads.len(), 1);
    assert_eq!(after_clear.image_uploads[0].0, 1);
}

/// A served socket in a fresh runtime directory, with a stand-in dispatcher
/// answering `overview`, plus everything a test needs to talk and clean up.
fn serve(
    tag: &str,
    overview: Option<SessionOverview>,
) -> (IpcServer, SessionId, PathBuf, JoinHandle<()>) {
    let runtime_directory = build_test_runtime_directory(tag);
    let session = SessionId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher = spawn_dispatcher(inbox_rx, overview);
    let server =
        IpcServer::start(&runtime_directory, session, inbox_tx, None).expect("start serving");
    (server, session, runtime_directory, dispatcher)
}

/// A served socket the other local users of this machine may reach, in a fresh
/// shared directory, with a stand-in dispatcher and the `allow-other-users`
/// setting reading `is_enabled`.
fn serve_shared(
    tag: &str,
    is_enabled: bool,
) -> (IpcServer, SessionId, PathBuf, PathBuf, JoinHandle<()>) {
    let runtime_directory = build_test_runtime_directory(tag);
    let shared_directory = build_test_shared_directory(tag);
    let session = SessionId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher = spawn_dispatcher(inbox_rx, None);
    let server = IpcServer::start(
        &runtime_directory,
        session,
        inbox_tx,
        Some(OtherUsers {
            shared_directory: shared_directory.clone(),
            is_enabled: Arc::new(move || is_enabled),
        }),
    )
    .expect("start serving");
    (
        server,
        session,
        runtime_directory,
        shared_directory,
        dispatcher,
    )
}

/// A stand-in dispatcher that reports every submitted command's envelope on the
/// returned receiver before answering it with `Ok`. Every other event it drains
/// is dropped. Exits when every inbox sender is gone.
fn spawn_reporting_dispatcher(
    inbox_rx: Receiver<RuntimeEvent>,
) -> (JoinHandle<()>, Receiver<CommandEnvelope>) {
    let (seen_tx, seen_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        while let Ok(event) = inbox_rx.recv() {
            if let RuntimeEvent::Ipc {
                envelope,
                response_sender,
            } = event
            {
                let _ = response_sender.send(CommandResult::Ok {
                    command_id: envelope.command_id,
                    emitted_events: Vec::new(),
                });
                if seen_tx.send(envelope).is_err() {
                    break;
                }
            }
        }
    });
    (handle, seen_rx)
}

/// A served socket in a fresh runtime directory whose stand-in dispatcher reports
/// every submitted command's envelope.
fn serve_reporting(
    tag: &str,
) -> (
    IpcServer,
    SessionId,
    PathBuf,
    JoinHandle<()>,
    Receiver<CommandEnvelope>,
) {
    let runtime_directory = build_test_runtime_directory(tag);
    let session = SessionId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let (dispatcher, seen) = spawn_reporting_dispatcher(inbox_rx);
    let server =
        IpcServer::start(&runtime_directory, session, inbox_tx, None).expect("start serving");
    (server, session, runtime_directory, dispatcher, seen)
}

/// Open a control connection to `session`, send the Hello, read its answer, and
/// hand back the connection ready for the next request.
fn greeted(runtime_directory: &Path, session: SessionId) -> Connection {
    let mut connection = connect_to(runtime_directory, session);
    connection
        .send(&hello_for(runtime_directory, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, hello_accepted());
    connection
}

/// Submit `envelope` on `connection` and hand back the envelope the dispatcher
/// was given, after reading the reply the submission earns.
fn submitted(
    connection: &mut Connection,
    seen: &Receiver<CommandEnvelope>,
    envelope: CommandEnvelope,
) -> CommandEnvelope {
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
        })
        .expect("send submit");
    let dispatched = seen.recv().expect("the dispatcher was given the command");
    let _: IpcResponse = connection.recv().expect("submit reply");
    dispatched
}

/// A deterministic envelope for submissions.
fn build_test_command_envelope() -> CommandEnvelope {
    CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::Internal,
        SystemTime::UNIX_EPOCH,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
}

/// The Hello that matches the endpoint file at `runtime_directory` for `session`.
fn hello_for(runtime_directory: &Path, session: SessionId) -> IpcRequest {
    let endpoint = EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session,
    ))
    .expect("endpoint file readable");
    IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            connection_token: endpoint.connection_token,
            is_remote: false,
        },
    }
}

/// The answer an accepted Hello earns: both sides speak this build's version,
/// so they settle on it, and the answer names the build the session runs.
fn hello_accepted() -> IpcResult {
    IpcResult::Hello {
        protocol_version: PROTOCOL_VERSION,
        build_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Connect to the socket the endpoint file at `runtime_directory` advertises.
fn connect_to(runtime_directory: &Path, session: SessionId) -> Connection {
    let endpoint = EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session,
    ))
    .expect("endpoint file readable");
    Connection::connect(&endpoint.socket_address).expect("connect")
}

/// A stand-in dispatcher answering every layout request with `layout`. The
/// returned receiver carries the tab each request named, so a test reads what
/// crossed the boundary. Exits when every inbox sender is gone.
fn spawn_layout_dispatcher(
    inbox_rx: Receiver<RuntimeEvent>,
    layout: Option<SessionLayout>,
) -> (JoinHandle<()>, Receiver<Option<TabId>>) {
    let (asked_tx, asked_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        while let Ok(event) = inbox_rx.recv() {
            if let RuntimeEvent::IpcLayout {
                tab_id,
                response_sender,
            } = event
            {
                let _ = asked_tx.send(tab_id);
                let _ = response_sender.send(layout.clone());
            }
        }
    });
    (handle, asked_rx)
}

/// A served socket whose stand-in dispatcher answers layout requests with
/// `layout`, plus the tab each request named.
fn serve_layout(
    tag: &str,
    layout: Option<SessionLayout>,
) -> (
    IpcServer,
    SessionId,
    PathBuf,
    JoinHandle<()>,
    Receiver<Option<TabId>>,
) {
    let runtime_directory = build_test_runtime_directory(tag);
    let session = SessionId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let (dispatcher, asked) = spawn_layout_dispatcher(inbox_rx, layout);
    let server =
        IpcServer::start(&runtime_directory, session, inbox_tx, None).expect("start serving");
    (server, session, runtime_directory, dispatcher, asked)
}

/// A tiny layout to answer a layout request with, distinguishable by its name.
fn layout_named(session_name: &str) -> SessionLayout {
    SessionLayout {
        session_id: SessionId::new(),
        session_name: session_name.to_string(),
        tabs: Vec::new(),
        clients: Vec::new(),
    }
}

/// A tiny overview to answer discovery with, distinguishable by its name.
fn overview_named(session_name: &str) -> SessionOverview {
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
fn a_control_connection_replaces_an_internal_source_with_an_external_cli_one() {
    // The CLI-admission check lets every command through an `Internal` source.
    // A peer presenting one on a control connection is stamped back to the
    // source that connection carries.
    let (server, session, runtime_directory, dispatcher, seen) = serve_reporting("stamp-internal");
    let mut connection = greeted(&runtime_directory, session);
    let sent = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::Internal,
        SystemTime::UNIX_EPOCH,
        Command::ToggleMouseSelect,
    );

    let dispatched = submitted(&mut connection, &seen, sent.clone());

    assert_eq!(
        dispatched.command_source,
        CommandSource::from_external_cli(None, None)
    );
    assert_eq!(dispatched.client_id, None);
    assert_eq!(dispatched.command_id, sent.command_id);
    assert_eq!(dispatched.command, Command::ToggleMouseSelect);

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher joins");
    cleanup(&runtime_directory);
}

#[test]
fn a_control_connection_cannot_present_another_clients_keybinding_source() {
    let (server, session, runtime_directory, dispatcher, seen) =
        serve_reporting("stamp-keybinding");
    let mut connection = greeted(&runtime_directory, session);
    let victim = ClientId::new();
    let sent = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(victim),
        SystemTime::UNIX_EPOCH,
        Command::ToggleMouseSelect,
    );

    let dispatched = submitted(&mut connection, &seen, sent);

    assert_eq!(
        dispatched.command_source,
        CommandSource::from_external_cli(None, None)
    );
    assert_eq!(dispatched.client_id, None);

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher joins");
    cleanup(&runtime_directory);
}

#[test]
fn a_control_connection_keeps_the_two_cli_sources_a_koshi_invocation_sends() {
    let (server, session, runtime_directory, dispatcher, seen) = serve_reporting("stamp-cli");
    let mut connection = greeted(&runtime_directory, session);
    let client = ClientId::new();
    let in_session = CommandSource::from_in_session_cli(
        session,
        Some(client),
        PaneId::new(),
        PathBuf::from("/sock"),
    );
    let external = CommandSource::from_external_cli(Some(session), Some(client));

    let dispatched = submitted(
        &mut connection,
        &seen,
        CommandEnvelope::from_parts(
            CommandId::new(),
            in_session.clone(),
            SystemTime::UNIX_EPOCH,
            Command::ToggleLockMode(ToggleLockModeArgs::default()),
        ),
    );
    assert_eq!(dispatched.command_source, in_session);
    assert_eq!(dispatched.client_id, Some(client));

    let dispatched = submitted(
        &mut connection,
        &seen,
        CommandEnvelope::from_parts(
            CommandId::new(),
            external.clone(),
            SystemTime::UNIX_EPOCH,
            Command::ToggleLockMode(ToggleLockModeArgs::default()),
        ),
    );
    assert_eq!(dispatched.command_source, external);
    assert_eq!(dispatched.client_id, None);

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher joins");
    cleanup(&runtime_directory);
}

#[test]
fn a_submitted_command_round_trips_with_the_dispatchers_result() {
    let (server, session, runtime_directory, dispatcher) = serve("roundtrip", None);
    let mut connection = connect_to(&runtime_directory, session);
    let command_envelope = build_test_command_envelope();

    connection
        .send(&hello_for(&runtime_directory, session))
        .expect("send hello");
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope.clone())),
        })
        .expect("send submit");

    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.request_id, Some(1));
    assert_eq!(hello_reply.answer_result, hello_accepted());

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
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

/// A newer koshi asking for something this build has no name for is refused by
/// name, and the caller keeps every other verb on the same connection. Killing
/// the connection instead would cost a caller its whole CLI surface for one
/// unfamiliar request.
#[test]
fn a_request_kind_this_build_lacks_is_refused_by_name_and_the_connection_keeps_serving() {
    let (server, session, runtime_directory, dispatcher) = serve("unknown-kind", None);
    let mut connection = connect_to(&runtime_directory, session);

    connection
        .send(&hello_for(&runtime_directory, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, hello_accepted());

    // A well-framed request naming a kind added by some later koshi.
    connection
        .send(&serde_json::json!({
            "request_id": 2,
            "kind": { "Floating": { "pane": "00000000-0000-0000-0000-000000000001" } }
        }))
        .expect("send a kind this build does not have");

    let refusal: IpcResponse = connection.recv().expect("refusal reply");
    assert_eq!(refusal.request_id, Some(2));
    assert_eq!(
        refusal.answer_result,
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
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

/// An unfamiliar kind arriving before the Hello is answered the same way any
/// other kind is: the gate is closed, so the caller learns nothing about which
/// kinds this build has.
#[test]
fn a_kind_this_build_lacks_before_hello_is_refused_as_hello_required() {
    let (server, session, runtime_directory, dispatcher) = serve("unknown-kind-early", None);
    let mut connection = connect_to(&runtime_directory, session);

    connection
        .send(&serde_json::json!({
            "request_id": 9,
            "kind": { "Floating": { "pane": "00000000-0000-0000-0000-000000000001" } }
        }))
        .expect("send a kind this build does not have, before the hello");

    let refusal: IpcResponse = connection.recv().expect("refusal reply");
    assert_eq!(refusal.request_id, Some(9));
    assert_eq!(
        refusal.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Floating arrived before a Hello opened the connection".to_string(),
        }),
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

/// A caller reaching higher than this build settles on this build's highest,
/// and the connection serves from there.
#[test]
fn a_caller_speaking_a_wider_range_settles_on_this_builds_highest() {
    let (server, session, runtime_directory, dispatcher) = serve("wider-range", None);
    let mut connection = connect_to(&runtime_directory, session);
    let endpoint = EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        &runtime_directory,
        session,
    ))
    .expect("endpoint file readable");

    connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                min_protocol_version: MIN_PROTOCOL_VERSION,
                max_protocol_version: PROTOCOL_VERSION + 5,
                connection_token: endpoint.connection_token,
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
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

/// A caller whose whole range sits above this build shares no version with it,
/// so the connection is refused naming both ranges and no verb is served.
#[test]
fn a_caller_sharing_no_version_is_refused_and_serves_nothing() {
    let (server, session, runtime_directory, dispatcher) = serve("no-shared-version", None);
    let mut connection = connect_to(&runtime_directory, session);
    let endpoint = EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        &runtime_directory,
        session,
    ))
    .expect("endpoint file readable");
    let above = PROTOCOL_VERSION + 1;

    connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                min_protocol_version: above,
                max_protocol_version: above + 2,
                connection_token: endpoint.connection_token,
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
                "the caller speaks protocol versions {above} to {}, \
                 this Koshi speaks {MIN_PROTOCOL_VERSION} to {PROTOCOL_VERSION}",
                above + 2
            ),
        }),
    );

    // The refusal left the gate closed, so the session is untouched.
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
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_request_before_hello_is_refused_and_the_connection_keeps_serving() {
    let (server, session, runtime_directory, dispatcher) = serve("hello-first", None);
    let mut connection = connect_to(&runtime_directory, session);

    connection
        .send(&IpcRequest {
            request_id: 7,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(build_test_command_envelope())),
        })
        .expect("send submit before hello");
    let refusal: IpcResponse = connection.recv().expect("refusal reply");
    assert_eq!(refusal.request_id, Some(7));
    assert_eq!(
        refusal.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "SubmitCommand arrived before a Hello opened the connection".to_string(),
        }),
    );

    // The same connection still serves: a Hello opens it and a submit works.
    connection
        .send(&hello_for(&runtime_directory, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, hello_accepted());

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_wrong_token_is_refused_as_bad_token() {
    let (server, session, runtime_directory, dispatcher) = serve("bad-token", None);
    let mut connection = connect_to(&runtime_directory, session);

    connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                min_protocol_version: MIN_PROTOCOL_VERSION,
                max_protocol_version: PROTOCOL_VERSION,
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
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_restart_advertises_a_fresh_token_and_refuses_the_old_one() {
    let (server, session, runtime_directory, dispatcher) = serve("restart-token", None);
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session);
    let initial_endpoint =
        EndpointFile::load_from_path(&endpoint_path).expect("endpoint file readable");
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");

    let (inbox_tx, inbox_rx) = mpsc::channel();
    let restarted_dispatcher = spawn_dispatcher(inbox_rx, None);
    let restarted =
        IpcServer::start(&runtime_directory, session, inbox_tx, None).expect("start serving again");
    let restarted_endpoint =
        EndpointFile::load_from_path(&endpoint_path).expect("endpoint file readable");
    assert_ne!(
        restarted_endpoint.connection_token, initial_endpoint.connection_token,
        "the restarted server advertises a new secret",
    );

    let mut stale_connection = connect_to(&runtime_directory, session);
    stale_connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                min_protocol_version: MIN_PROTOCOL_VERSION,
                max_protocol_version: PROTOCOL_VERSION,
                connection_token: initial_endpoint.connection_token,
                is_remote: false,
            },
        })
        .expect("send hello with the token from before the restart");
    let refusal: IpcResponse = stale_connection.recv().expect("reply");
    assert_eq!(
        refusal.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let mut accepted_connection = connect_to(&runtime_directory, session);
    accepted_connection
        .send(&hello_for(&runtime_directory, session))
        .expect("send hello with the new secret");
    let accepted: IpcResponse = accepted_connection.recv().expect("hello reply");
    assert_eq!(accepted.answer_result, hello_accepted());

    drop(stale_connection);
    drop(accepted_connection);
    restarted.shutdown();
    restarted_dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_detach_leaves_the_sessions_token_unchanged() {
    let client = ClientId::new();
    let (server, session, runtime_directory, dispatcher, seen) =
        serve_attachable("detach-token", client);
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session);
    let endpoint_before_detach =
        EndpointFile::load_from_path(&endpoint_path).expect("endpoint file readable");

    let attached = attach_to(&runtime_directory, session, client);
    drop(attached);
    let RuntimeEvent::ClientDetached { client_id, .. } = seen.recv().expect("detach event") else {
        panic!("expected ClientDetached");
    };
    assert_eq!(client_id, client);

    let endpoint_after_detach =
        EndpointFile::load_from_path(&endpoint_path).expect("endpoint file readable");
    assert_eq!(
        endpoint_after_detach.connection_token, endpoint_before_detach.connection_token,
        "the detached client's departure leaves the session's secret alone",
    );

    let mut connection = connect_to(&runtime_directory, session);
    connection
        .send(&hello_for(&runtime_directory, session))
        .expect("send hello with the secret from before the detach");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, hello_accepted());

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_malformed_frame_is_answered_and_the_connection_keeps_serving() {
    let (server, session, runtime_directory, dispatcher) = serve("malformed", None);
    let mut connection = connect_to(&runtime_directory, session);

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
        .send(&hello_for(&runtime_directory, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, hello_accepted());

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn an_oversize_frame_closes_the_connection() {
    let (server, session, runtime_directory, dispatcher) = serve("oversize", None);
    let endpoint = EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        &runtime_directory,
        session,
    ))
    .expect("endpoint file readable");

    // A raw stream, so the length prefix can lie past the cap without a
    // payload behind it.
    let mut raw_socket = raw_connect(&endpoint.socket_address);
    let oversize = (koshi_ipc::transport::MAX_FRAME_BYTE_COUNT + 1).to_be_bytes();
    std::io::Write::write_all(&mut raw_socket, &oversize).expect("write oversize header");

    // The server closes: the next read finds the stream at end.
    let mut buffer = [0u8; 1];
    let closed = match std::io::Read::read(&mut raw_socket, &mut buffer) {
        Ok(0) => true,
        Ok(_) => false,
        Err(_) => true,
    };
    assert!(
        closed,
        "the connection must be closed after an oversize frame"
    );

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

/// Open the control socket as a raw byte stream, bypassing the framed
/// [`Connection`], so a test can write a corrupt frame header.
#[cfg(unix)]
fn raw_connect(socket_address: &str) -> std::os::unix::net::UnixStream {
    std::os::unix::net::UnixStream::connect(socket_address).expect("raw connect")
}

/// Open the control socket as a raw byte stream, bypassing the framed
/// [`Connection`], so a test can write a corrupt frame header. The bare pipe
/// name is served at `\\.\pipe\<name>`.
#[cfg(windows)]
fn raw_connect(socket_address: &str) -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!(r"\\.\pipe\{socket_address}"))
        .expect("raw connect")
}

#[test]
fn an_attached_connection_forwards_input_unanswered_and_detaches_on_any_other_request() {
    let client = ClientId::new();
    let (server, session, runtime_directory, dispatcher, seen) =
        serve_attachable("attached-input", client);
    let mut connection = attach_to(&runtime_directory, session, client);
    let pressed = KeyChord::from_parts(ModFlags::CTRL, Key::Char('t'));
    let resized = Size {
        column_count: 120,
        row_count: 40,
    };
    let command_envelope = build_test_command_envelope();

    connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::KeyPress { chord: pressed },
        })
        .expect("send key press");
    let RuntimeEvent::ClientKeyPress { client_id, chord } = seen.recv().expect("key press event")
    else {
        panic!("expected ClientKeyPress");
    };
    assert_eq!(client_id, client);
    assert_eq!(chord, pressed);

    connection
        .send(&IpcRequest {
            request_id: 4,
            request_kind: IpcRequestKind::Resize {
                viewport: resized,
                pane_area: None,
                cell_size: None,
            },
        })
        .expect("send resize");
    let RuntimeEvent::Resize {
        client_id,
        viewport_size,
        pane_area,
        cell_size,
    } = seen.recv().expect("resize event")
    else {
        panic!("expected Resize");
    };
    assert_eq!(client_id, client);
    assert_eq!(viewport_size, resized);
    assert_eq!(pane_area, None);
    assert_eq!(cell_size, None);

    connection
        .send(&IpcRequest {
            request_id: 5,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope.clone())),
        })
        .expect("send submit");
    let RuntimeEvent::Ipc {
        envelope,
        response_sender,
    } = seen.recv().expect("submit event")
    else {
        panic!("expected Ipc");
    };
    assert_eq!(envelope.command_id, command_envelope.command_id);
    assert_eq!(envelope.command, command_envelope.command);
    assert_eq!(envelope.issued_at, command_envelope.issued_at);
    assert_eq!(
        envelope.command_source,
        CommandSource::from_key_binding(client),
        "this connection's own client is stamped over what the peer sent",
    );
    assert_eq!(envelope.client_id, Some(client));
    let unread = CommandResult::Ok {
        command_id: command_envelope.command_id,
        emitted_events: Vec::new(),
    };
    assert_eq!(
        response_sender.send(unread.clone()),
        Err(mpsc::SendError(unread)),
        "the reply channel's receiving end is already gone",
    );

    let round = vec![WireMouseAction::Scroll {
        pane_id: PaneId::new(),
        is_scrolling_up: true,
        scroll_line_count: 3,
    }];
    connection
        .send(&IpcRequest {
            request_id: 6,
            request_kind: IpcRequestKind::Mouse(round.clone()),
        })
        .expect("send mouse round");
    let RuntimeEvent::ClientMouse {
        client_id,
        request_id,
        mouse_actions,
    } = seen.recv().expect("mouse round event")
    else {
        panic!("expected ClientMouse");
    };
    assert_eq!(client_id, client);
    assert_eq!(request_id, 6, "the round's own id crosses with it");
    assert_eq!(mouse_actions, round);

    connection
        .send(&IpcRequest {
            request_id: 7,
            request_kind: IpcRequestKind::Paste {
                pasted_text: String::from("hello\nworld"),
            },
        })
        .expect("send paste");
    let RuntimeEvent::HostPaste {
        client_id,
        pasted_text,
    } = seen.recv().expect("paste event")
    else {
        panic!("expected HostPaste");
    };
    assert_eq!(client_id, client);
    assert_eq!(pasted_text, "hello\nworld");

    // A kind the reading half does not forward ends it, which detaches.
    connection
        .send(&IpcRequest {
            request_id: 8,
            request_kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");
    let RuntimeEvent::ClientDetached { client_id, .. } = seen.recv().expect("detach event") else {
        panic!("expected ClientDetached");
    };
    assert_eq!(client_id, client);

    // The goodbye is the first frame after the attach ipc_response, so none of the
    // five requests above was answered with an `IpcResponse`.
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
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_request_kind_this_build_lacks_on_an_attached_connection_is_dropped_and_the_stream_goes_on() {
    let client = ClientId::new();
    let (server, session, runtime_directory, dispatcher, seen) =
        serve_attachable("attached-unknown-kind", client);
    let mut connection = attach_to(&runtime_directory, session, client);
    let pressed = KeyChord::from_parts(ModFlags::CTRL, Key::Char('t'));

    // A well-framed request naming a kind added by some later koshi.
    connection
        .send(&serde_json::json!({
            "request_id": 3,
            "kind": { "Floating": { "pane": "00000000-0000-0000-0000-000000000001" } }
        }))
        .expect("send a kind this build does not have");
    connection
        .send(&IpcRequest {
            request_id: 4,
            request_kind: IpcRequestKind::KeyPress { chord: pressed },
        })
        .expect("send key press");

    // The key press is the first event the dispatcher sees, so the unfamiliar
    // request crossed nothing, and the stream carried the one behind it.
    let RuntimeEvent::ClientKeyPress { client_id, chord } = seen
        .recv_timeout(Duration::from_secs(5))
        .expect("key press event")
    else {
        panic!("expected ClientKeyPress");
    };
    assert_eq!(client_id, client);
    assert_eq!(chord, pressed);

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_malformed_frame_on_an_attached_connection_detaches_that_client() {
    let client = ClientId::new();
    let (server, session, runtime_directory, dispatcher, seen) =
        serve_attachable("attached-malformed", client);
    let mut connection = attach_to(&runtime_directory, session, client);

    // A well-framed message that is not a request at all.
    connection.send(&"not a request").expect("send junk frame");

    let RuntimeEvent::ClientDetached {
        client_id,
        is_streamed,
        ..
    } = seen
        .recv_timeout(Duration::from_secs(5))
        .expect("detach event")
    else {
        panic!("expected ClientDetached");
    };
    assert_eq!(client_id, client);
    assert!(is_streamed, "the detached client was carrying a stream");
    assert_eq!(
        connection.recv::<SessionEvent>().expect("goodbye frame"),
        SessionEvent::Detached,
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_mouse_round_before_an_attach_closes_the_connection() {
    // A round names no client until the connection carries one, so it belongs
    // on an attached connection only.
    let (server, session, runtime_directory, dispatcher) = serve("mouse-unattached", None);
    let mut connection = connect_to(&runtime_directory, session);

    connection
        .send(&hello_for(&runtime_directory, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, hello_accepted());

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
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn discovery_answers_with_the_dispatchers_overview() {
    let (server, session, runtime_directory, dispatcher) =
        serve("discovery", Some(overview_named("workspace")));
    let mut connection = connect_to(&runtime_directory, session);

    connection
        .send(&hello_for(&runtime_directory, session))
        .expect("send hello");
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");

    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, hello_accepted());
    let discovery_reply: IpcResponse = connection.recv().expect("discovery reply");
    let IpcResult::Overview(overview) = discovery_reply.answer_result else {
        panic!(
            "expected an overview, got {:?}",
            discovery_reply.answer_result
        );
    };
    assert_eq!(overview.session.session_name, "workspace");

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn discovery_with_no_running_session_closes_the_connection() {
    let (server, session, runtime_directory, dispatcher) = serve("discovery-none", None);
    let mut connection = connect_to(&runtime_directory, session);

    connection
        .send(&hello_for(&runtime_directory, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, hello_accepted());

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
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_recent_events_request_answers_from_the_ring_without_asking_the_dispatcher() {
    let (server, session, runtime_directory, dispatcher) = serve("recent-events", None);
    let tab_id = TabId::new();
    recent_events::record_event(&koshi_core::event::Event::LayoutChanged(
        koshi_core::event::LayoutChanged { tab_id },
    ));
    let mut connection = connect_to(&runtime_directory, session);

    connection
        .send(&hello_for(&runtime_directory, session))
        .expect("send hello");
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::RecentEvents,
        })
        .expect("send recent-events request");

    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, hello_accepted());
    let events_reply: IpcResponse = connection.recv().expect("recent-events reply");
    assert_eq!(events_reply.request_id, Some(2));
    let IpcResult::RecentEvents(recent_events) = events_reply.answer_result else {
        panic!(
            "expected recent events, got {:?}",
            events_reply.answer_result
        );
    };
    // The ring is process-wide and every test in this binary writes to it, so
    // the image record is found by this tab's own id rather than by position.
    let layout_event_record = recent_events
        .iter()
        .find(|event| event.tab_id == Some(tab_id))
        .expect("the answer carries the image record this test made");
    assert_eq!(layout_event_record.event_name, "LayoutChanged");

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_layout_request_answers_with_the_dispatchers_layout_and_names_the_tab_asked_for() {
    let (server, session, runtime_directory, dispatcher, asked) =
        serve_layout("layout-one-tab", Some(layout_named("workspace")));
    let requested_tab_id = TabId::new();
    let mut connection = connect_to(&runtime_directory, session);

    connection
        .send(&hello_for(&runtime_directory, session))
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
    assert_eq!(hello_reply.answer_result, hello_accepted());
    let layout_reply: IpcResponse = connection.recv().expect("layout reply");
    assert_eq!(layout_reply.request_id, Some(2));
    let IpcResult::Layout(layout) = layout_reply.answer_result else {
        panic!("expected a layout, got {:?}", layout_reply.answer_result);
    };
    assert_eq!(layout.session_name, "workspace");
    assert_eq!(
        asked.recv().expect("the dispatcher was asked"),
        Some(requested_tab_id)
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_layout_request_for_every_tab_names_no_tab_to_the_dispatcher() {
    let (server, session, runtime_directory, dispatcher, asked) =
        serve_layout("layout-every-tab", Some(layout_named("workspace")));
    let mut connection = connect_to(&runtime_directory, session);

    connection
        .send(&hello_for(&runtime_directory, session))
        .expect("send hello");
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Layout { tab_id: None },
        })
        .expect("send layout request");

    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, hello_accepted());
    let layout_reply: IpcResponse = connection.recv().expect("layout reply");
    let IpcResult::Layout(layout) = layout_reply.answer_result else {
        panic!("expected a layout, got {:?}", layout_reply.answer_result);
    };
    assert_eq!(layout.session_name, "workspace");
    assert_eq!(asked.recv().expect("the dispatcher was asked"), None);

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_layout_request_with_no_running_session_closes_the_connection() {
    let (server, session, runtime_directory, dispatcher, _asked) =
        serve_layout("layout-none", None);
    let mut connection = connect_to(&runtime_directory, session);

    connection
        .send(&hello_for(&runtime_directory, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, hello_accepted());

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
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_layout_request_on_an_attached_connection_ends_that_client_stream() {
    let client = ClientId::new();
    let (server, session, runtime_directory, dispatcher, seen) =
        serve_attachable("layout-attached", client);
    let mut connection = attach_to(&runtime_directory, session, client);

    connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Layout { tab_id: None },
        })
        .expect("send layout request");

    let RuntimeEvent::ClientDetached { client_id, .. } = seen.recv().expect("detach event") else {
        panic!("expected ClientDetached");
    };
    assert_eq!(client_id, client);
    assert_eq!(
        connection.recv::<SessionEvent>().expect("goodbye frame"),
        SessionEvent::Detached,
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_gone_dispatcher_closes_the_connection_instead_of_answering() {
    let runtime_directory = build_test_runtime_directory("no-dispatcher");
    let session = SessionId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    drop(inbox_rx);
    let server =
        IpcServer::start(&runtime_directory, session, inbox_tx, None).expect("start serving");
    let mut connection = connect_to(&runtime_directory, session);

    connection
        .send(&hello_for(&runtime_directory, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, hello_accepted());

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
    server.shutdown();
    cleanup(&runtime_directory);
}

#[test]
fn the_endpoint_file_lives_while_serving_and_both_files_go_at_shutdown() {
    let (server, session, runtime_directory, dispatcher) = serve("lifecycle", None);
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session);
    let endpoint = EndpointFile::load_from_path(&endpoint_path).expect("endpoint file readable");

    assert!(
        endpoint_path.exists(),
        "endpoint file present while serving"
    );
    assert_eq!(endpoint.process_id, std::process::id());
    #[cfg(unix)]
    assert!(
        Path::new(&endpoint.socket_address).exists(),
        "socket file present while serving",
    );

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");

    assert!(!endpoint_path.exists(), "endpoint file gone after shutdown");
    #[cfg(unix)]
    assert!(
        !Path::new(&endpoint.socket_address).exists(),
        "socket file gone after shutdown",
    );
    let Err(IpcError::NoListener { socket_address }) =
        Connection::connect(&endpoint.socket_address)
    else {
        panic!("nothing listens after shutdown");
    };
    assert_eq!(socket_address, endpoint.socket_address);
    cleanup(&runtime_directory);
}

#[test]
fn dropping_the_server_without_shutdown_still_removes_both_files() {
    let (server, session, runtime_directory, dispatcher) = serve("drop-cleans", None);
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session);
    let endpoint = EndpointFile::load_from_path(&endpoint_path).expect("endpoint file readable");

    drop(server);
    dispatcher.join().expect("dispatcher exits");

    assert!(!endpoint_path.exists(), "endpoint file gone after drop");
    let Err(IpcError::NoListener { socket_address }) =
        Connection::connect(&endpoint.socket_address)
    else {
        panic!("nothing listens after drop");
    };
    assert_eq!(socket_address, endpoint.socket_address);
    cleanup(&runtime_directory);
}

#[cfg(unix)]
#[test]
fn shutdown_returns_and_removes_the_endpoint_even_when_the_wake_cannot_connect() {
    let (server, session, runtime_directory, dispatcher) = serve("wake-fails", None);
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session);
    let endpoint = EndpointFile::load_from_path(&endpoint_path).expect("endpoint file readable");

    // Unlink the socket file out from under the listener: the wake connect
    // inside shutdown now fails, so shutdown must skip the join instead of
    // waiting forever on the still-blocked accept loop.
    std::fs::remove_file(&endpoint.socket_address).expect("unlink the live socket");

    server.shutdown();

    assert!(
        !endpoint_path.exists(),
        "endpoint file gone even though the accept loop could not be woken",
    );
    drop(dispatcher);
    cleanup(&runtime_directory);
}

#[cfg(unix)]
#[test]
fn a_leftover_socket_file_is_reclaimed_at_start() {
    let runtime_directory = build_test_runtime_directory("reclaim");
    koshi_paths::ensure_private_directory(&runtime_directory).expect("create runtime directory");
    let session = SessionId::new();
    let socket_address = compute_socket_address(&runtime_directory, session);
    std::fs::write(&socket_address, b"").expect("plant a leftover file at the socket path");

    let (inbox_tx, _inbox_rx) = mpsc::channel();
    let server = IpcServer::start(&runtime_directory, session, inbox_tx, None)
        .expect("start reclaims the leftover and serves");

    server.shutdown();
    cleanup(&runtime_directory);
}

#[test]
fn a_second_start_on_the_same_session_is_refused_while_serving() {
    let (server, session, runtime_directory, dispatcher) = serve("busy", None);

    let (inbox_tx, _inbox_rx) = mpsc::channel();
    let Err(IpcError::SocketBusy { socket_address }) =
        IpcServer::start(&runtime_directory, session, inbox_tx, None)
    else {
        panic!("the live listener must refuse a second bind");
    };
    assert_eq!(
        socket_address,
        compute_socket_address(&runtime_directory, session)
    );

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_runtime_directory_that_cannot_be_created_refuses_to_start() {
    // A file where the directory would go: creating the directory under it
    // fails, and the start stops before it binds anything.
    let blocker = build_test_runtime_directory("runtime-dir-blocked");
    cleanup(&blocker);
    std::fs::write(&blocker, b"").expect("plant a file where the directory would go");
    let runtime_directory = blocker.join("session");
    let (inbox_tx, _inbox_rx) = mpsc::channel();

    let Err(IpcError::Transport { error_detail }) =
        IpcServer::start(&runtime_directory, SessionId::new(), inbox_tx, None)
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

    std::fs::remove_file(&blocker).expect("take the planted file away");
}

#[test]
fn a_start_whose_endpoint_file_cannot_be_written_leaves_nothing_listening() {
    // A directory where the endpoint file goes: the write cannot rename over
    // it, so the start unwinds the bind it already made.
    let runtime_directory = build_test_runtime_directory("endpoint-write-fails");
    koshi_paths::ensure_private_directory(&runtime_directory).expect("create runtime directory");
    let session = SessionId::new();
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session);
    let socket_address = compute_socket_address(&runtime_directory, session);
    std::fs::create_dir_all(&endpoint_path).expect("plant a directory where the file goes");
    let (inbox_tx, _inbox_rx) = mpsc::channel();

    let Err(IpcError::EndpointFileWrite {
        endpoint_file_path, ..
    }) = IpcServer::start(&runtime_directory, session, inbox_tx, None)
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
        socket_address: named,
    }) = Connection::connect(&socket_address)
    else {
        panic!("nothing listens after a refused start");
    };
    assert_eq!(named, socket_address);

    cleanup(&runtime_directory);
}

#[test]
fn a_session_only_its_own_user_may_reach_binds_inside_the_runtime_directory() {
    let (server, session, runtime_directory, dispatcher) = serve("own-user-socket", None);
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session);
    let endpoint = EndpointFile::load_from_path(&endpoint_path).expect("endpoint file readable");

    assert_eq!(
        endpoint.socket_address,
        compute_socket_address(&runtime_directory, session)
    );
    assert_eq!(endpoint_path.parent(), Some(runtime_directory.as_path()));

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_session_other_local_users_may_reach_keeps_its_endpoint_file_private() {
    let (server, session, runtime_directory, shared_directory, dispatcher) =
        serve_shared("shared-endpoint", true);
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session);

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

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
    cleanup(&shared_directory);
}

#[cfg(unix)]
#[test]
fn the_socket_of_a_session_other_local_users_may_reach_is_open_to_every_local_user() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let (server, session, runtime_directory, shared_directory, dispatcher) =
        serve_shared("shared-mode", true);
    let endpoint = EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        &runtime_directory,
        session,
    ))
    .expect("endpoint file readable");

    // The runtime directory was created by this start, so its owner is the
    // user whose directory under the shared one holds the socket.
    let uid = std::fs::metadata(&runtime_directory)
        .expect("stat runtime directory")
        .uid();
    assert_eq!(
        PathBuf::from(&endpoint.socket_address),
        shared_directory
            .join(uid.to_string())
            .join(format!("{session}.sock")),
    );
    let socket_permission_mode = std::fs::metadata(&endpoint.socket_address)
        .expect("stat socket file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(socket_permission_mode, 0o666);

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
    cleanup(&shared_directory);
}

#[cfg(windows)]
#[test]
fn the_marker_naming_a_shared_session_lives_while_serving_and_goes_at_shutdown() {
    let (server, session, runtime_directory, shared_directory, dispatcher) =
        serve_shared("shared-marker", true);
    let marker = resolve_advertisement_marker_path(&shared_directory, session);

    assert!(marker.exists(), "marker present while serving");

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");

    assert!(!marker.exists(), "marker gone after shutdown");
    cleanup(&runtime_directory);
    cleanup(&shared_directory);
}

#[test]
fn the_user_who_started_the_session_attaches_over_the_shared_socket_with_the_token() {
    let runtime_directory = build_test_runtime_directory("shared-attach");
    let shared_directory = build_test_shared_directory("shared-attach");
    let session = SessionId::new();
    let client = ClientId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let (dispatcher, _seen) = spawn_attaching_dispatcher(inbox_rx, client, session);
    let server = IpcServer::start(
        &runtime_directory,
        session,
        inbox_tx,
        Some(OtherUsers {
            shared_directory: shared_directory.clone(),
            is_enabled: Arc::new(|| true),
        }),
    )
    .expect("start serving");

    let connection = attach_to(&runtime_directory, session, client);

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
    cleanup(&shared_directory);
}

// --- Serving one connection from another local user ---

/// A control-socket address unique to this test: a file path under the short
/// temporary base on Unix, a pipe name on Windows.
fn build_test_socket_address(tag: &str) -> String {
    let unique = format!("koshi-peer-{}-{tag}", std::process::id());
    #[cfg(unix)]
    {
        PathBuf::from("/tmp")
            .join(unique)
            .with_extension("sock")
            .display()
            .to_string()
    }
    #[cfg(windows)]
    {
        unique
    }
}

/// Serve one connection from another local user of this machine, with
/// `is_enabled` standing in for the `allow-other-users` setting the serving loop
/// reads. Hands back the caller's end, the serving thread and the address, so
/// a test can flip the setting under a live connection.
///
/// The user who started a session is served whatever the setting says, so
/// there is no such connection to cut and only this peer carries the live
/// read.
fn serve_other_user(
    tag: &str,
    is_enabled: &Arc<AtomicBool>,
    inbox_tx: Sender<RuntimeEvent>,
) -> (Connection, JoinHandle<()>, String) {
    let socket_address = build_test_socket_address(tag);
    remove_socket_file(&socket_address);
    let listener = Listener::bind(&socket_address).expect("bind");
    let setting = Arc::clone(is_enabled);
    let serving = std::thread::spawn(move || {
        let connection = listener.accept().expect("accept");
        let intake = Arc::new(Intake::default());
        let served = intake
            .accept_connection(&connection)
            .expect("the intake takes it");
        serve_connection(
            connection,
            ConnectionToken::generate(),
            &inbox_tx,
            Peer::Local {
                is_same_user: false,
                is_other_user_access_allowed: true,
            },
            Some(Arc::new(move || setting.load(Ordering::SeqCst))),
            &served,
        );
    });
    let caller = Connection::connect(&socket_address).expect("connect");
    (caller, serving, socket_address)
}

/// The Hello another local user sends: this build's range and no token, which
/// is all a user who cannot read the endpoint file has to present.
fn other_user_hello() -> IpcRequest {
    IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            connection_token: ConnectionToken::from_secret(""),
            is_remote: false,
        },
    }
}

#[test]
fn another_local_user_keeps_being_served_while_the_setting_stays_on() {
    let is_enabled = Arc::new(AtomicBool::new(true));
    let overview = overview_named("shared-session");
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher = spawn_dispatcher(inbox_rx, Some(overview.clone()));
    let (mut caller, serving, socket_address) = serve_other_user("stays-on", &is_enabled, inbox_tx);

    caller.send(&other_user_hello()).expect("send hello");
    let ipc_response: IpcResponse = caller.recv().expect("hello reply");
    assert_eq!(ipc_response.answer_result, hello_accepted());

    for request_id in [2, 3] {
        caller
            .send(&IpcRequest {
                request_id,
                request_kind: IpcRequestKind::Discovery,
            })
            .expect("send discovery");
        let ipc_response: IpcResponse = caller.recv().expect("discovery reply");
        assert_eq!(
            ipc_response,
            IpcResponse {
                request_id: Some(request_id),
                answer_result: IpcResult::Overview(overview.clone()),
            }
        );
    }

    drop(caller);
    serving.join().expect("serving thread");
    dispatcher.join().expect("dispatcher exits");
    remove_socket_file(&socket_address);
}

#[test]
fn another_local_users_connection_is_cut_when_the_setting_goes_off() {
    let is_enabled = Arc::new(AtomicBool::new(true));
    let overview = overview_named("shared-session");
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher = spawn_dispatcher(inbox_rx, Some(overview.clone()));
    let (mut caller, serving, socket_address) = serve_other_user("goes-off", &is_enabled, inbox_tx);

    caller.send(&other_user_hello()).expect("send hello");
    let ipc_response: IpcResponse = caller.recv().expect("hello reply");
    assert_eq!(ipc_response.answer_result, hello_accepted());
    caller
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");
    let ipc_response: IpcResponse = caller.recv().expect("discovery reply");
    assert_eq!(
        ipc_response,
        IpcResponse {
            request_id: Some(2),
            answer_result: IpcResult::Overview(overview),
        }
    );

    // The serving loop is blocked reading, so the setting turns off between
    // one request and the next.
    is_enabled.store(false, Ordering::SeqCst);
    caller
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");

    assert!(
        matches!(caller.recv::<IpcResponse>(), Err(IpcError::Disconnected)),
        "the request is answered with a closed connection, not an overview",
    );

    drop(caller);
    serving.join().expect("serving thread");
    dispatcher.join().expect("dispatcher exits");
    remove_socket_file(&socket_address);
}

#[test]
fn an_attached_client_of_another_local_user_is_detached_when_the_setting_goes_off() {
    let client = ClientId::new();
    let session = SessionId::new();
    let is_enabled = Arc::new(AtomicBool::new(true));
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let (dispatcher, seen) = spawn_attaching_dispatcher(inbox_rx, client, session);
    let (mut caller, serving, socket_address) =
        serve_other_user("attached-off", &is_enabled, inbox_tx);

    caller.send(&other_user_hello()).expect("send hello");
    let ipc_response: IpcResponse = caller.recv().expect("hello reply");
    assert_eq!(ipc_response.answer_result, hello_accepted());
    caller
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Attach {
                viewport: TEST_VIEWPORT_SIZE,
                event_filter: EventFilterSpec::All,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities: koshi_ipc::protocol::GraphicsCapabilities::default(),
                cell_size: None,
            },
        })
        .expect("send attach");
    let ipc_response: IpcResponse = caller.recv().expect("attach reply");
    assert_eq!(
        ipc_response.answer_result,
        IpcResult::Attached {
            client_id: client,
            session_id: session,
            session_structure: attached_structure(session),
            resume_token: Some(ConnectionToken::from_secret(MINTED_CONNECTION_TOKEN)),
            pane_area: None,
        }
    );

    let pressed = KeyChord::from_parts(ModFlags::NONE, Key::Char('k'));
    caller
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::KeyPress { chord: pressed },
        })
        .expect("send key press");
    let RuntimeEvent::ClientKeyPress { client_id, chord } = seen.recv().expect("key press event")
    else {
        panic!("expected ClientKeyPress");
    };
    assert_eq!(client_id, client);
    assert_eq!(chord, pressed);

    is_enabled.store(false, Ordering::SeqCst);
    caller
        .send(&IpcRequest {
            request_id: 4,
            request_kind: IpcRequestKind::KeyPress { chord: pressed },
        })
        .expect("send key press");

    // The typing that arrived after the setting went off never reached the
    // session; the client left instead.
    let RuntimeEvent::ClientDetached { client_id, .. } = seen.recv().expect("detach event") else {
        panic!("expected ClientDetached");
    };
    assert_eq!(client_id, client);
    assert_eq!(
        caller.recv::<SessionEvent>().expect("goodbye frame"),
        SessionEvent::Detached,
    );

    drop(caller);
    serving.join().expect("serving thread");
    dispatcher.join().expect("dispatcher exits");
    remove_socket_file(&socket_address);
}

#[test]
fn the_directory_other_local_users_reach_holds_only_the_socket() {
    let (server, session, runtime_directory, shared_directory, dispatcher) =
        serve_shared("shared-only", true);
    #[cfg(unix)]
    let user_dir = {
        use std::os::unix::fs::MetadataExt;

        let uid = std::fs::metadata(&runtime_directory)
            .expect("stat runtime directory")
            .uid();
        shared_directory.join(uid.to_string())
    };
    // Pipe names share one machine-wide namespace, so Windows advertises in
    // the shared directory itself.
    #[cfg(windows)]
    let user_dir = shared_directory.clone();

    let mut directory_entry_names: Vec<String> = std::fs::read_dir(&user_dir)
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

    // The endpoint file carrying the token is not among them: it stayed in the
    // private runtime directory the socket left.
    #[cfg(unix)]
    assert_eq!(directory_entry_names, vec![format!("{session}.sock")]);
    #[cfg(windows)]
    assert_eq!(directory_entry_names, vec![session.to_string()]);

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
    cleanup(&shared_directory);
}

#[test]
fn resolve_peer_gates_the_starting_user_always_and_the_other_users_by_the_setting() {
    assert_eq!(
        resolve_peer(true, false),
        Peer::Local {
            is_same_user: true,
            is_other_user_access_allowed: false,
        },
    );
    assert_eq!(
        resolve_peer(true, true),
        Peer::Local {
            is_same_user: true,
            is_other_user_access_allowed: true,
        },
    );
    assert_eq!(
        resolve_peer(false, false),
        Peer::Local {
            is_same_user: false,
            is_other_user_access_allowed: false,
        },
    );
    assert_eq!(
        resolve_peer(false, true),
        Peer::Local {
            is_same_user: false,
            is_other_user_access_allowed: true,
        },
    );
}

/// A stand-in dispatcher that answers every restart request with `verdict` and
/// every discovery request with `overview`, so a test reads what a refusal or
/// an acceptance looks like on the socket and whether the session keeps
/// serving after it. Exits when every inbox sender is gone.
fn spawn_restart_dispatcher(
    inbox_rx: Receiver<RuntimeEvent>,
    verdict: Result<(), String>,
    overview: Option<SessionOverview>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while let Ok(event) = inbox_rx.recv() {
            match event {
                RuntimeEvent::IpcRestart { response_sender } => {
                    let _ = response_sender.send(verdict.clone());
                }
                RuntimeEvent::IpcDiscovery { response_sender } => {
                    let _ = response_sender.send(overview.clone());
                }
                _ => {}
            }
        }
    })
}

/// A served socket whose stand-in dispatcher answers restart requests with
/// `verdict` and discovery requests with `overview`.
fn serve_restartable(
    tag: &str,
    verdict: Result<(), String>,
    overview: Option<SessionOverview>,
) -> (IpcServer, SessionId, PathBuf, JoinHandle<()>) {
    let runtime_directory = build_test_runtime_directory(tag);
    let session = SessionId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher = spawn_restart_dispatcher(inbox_rx, verdict, overview);
    let server =
        IpcServer::start(&runtime_directory, session, inbox_tx, None).expect("start serving");
    (server, session, runtime_directory, dispatcher)
}

/// Say hello, send a restart, and hand back what the restart was answered.
fn restart_over(runtime_directory: &Path, session: SessionId) -> (Connection, IpcResult) {
    let mut connection = connect_to(runtime_directory, session);
    connection
        .send(&hello_for(runtime_directory, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, hello_accepted());

    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Restart,
        })
        .expect("send restart");
    let ipc_response: IpcResponse = connection.recv().expect("restart reply");
    assert_eq!(ipc_response.request_id, Some(2));
    (connection, ipc_response.answer_result)
}

/// Ask for the session's description on an open connection and hand back the
/// answer, so a test can show the session still serves after a refusal.
fn discovery_over(connection: &mut Connection, request_id: u64) -> IpcResult {
    connection
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");
    let ipc_response: IpcResponse = connection.recv().expect("discovery reply");
    assert_eq!(ipc_response.request_id, Some(request_id));
    ipc_response.answer_result
}

#[test]
fn an_accepted_restart_is_answered_restarting() {
    let (server, session, runtime_directory, dispatcher) =
        serve_restartable("restart-accepted", Ok(()), None);

    let (connection, restart_result) = restart_over(&runtime_directory, session);

    assert_eq!(restart_result, IpcResult::Restarting);

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

/// The Hello gate covers the restart like every other kind, so a caller that
/// never opened the connection cannot make the session replace its own image.
#[test]
fn a_restart_before_hello_is_refused_as_hello_required_and_the_connection_keeps_serving() {
    let (server, session, runtime_directory, dispatcher) =
        serve_restartable("restart-early", Ok(()), Some(overview_named("still-here")));
    let mut connection = connect_to(&runtime_directory, session);

    connection
        .send(&IpcRequest {
            request_id: 9,
            request_kind: IpcRequestKind::Restart,
        })
        .expect("send restart before the hello");
    let refusal: IpcResponse = connection.recv().expect("refusal reply");

    assert_eq!(refusal.request_id, Some(9));
    assert_eq!(
        refusal.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Restart arrived before a Hello opened the connection".to_string(),
        }),
    );

    // The gate is still closed, so the same connection still answers.
    assert_eq!(
        discovery_over(&mut connection, 10),
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Discovery arrived before a Hello opened the connection".to_string(),
        }),
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
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

/// The reply is the session's only chance to refuse: after it the swap runs. A
/// binary this machine could not run must never reach it, and the refusal must
/// leave the session serving.
#[test]
fn a_restart_naming_a_binary_that_cannot_run_is_refused_and_the_session_keeps_serving() {
    let binary_directory = build_test_runtime_directory("restart-bad-binary-directory");
    let (executable_path, rejection_message) = build_unrunnable_binary(&binary_directory);
    let overview = overview_named("still-here");
    let (server, session, runtime_directory, dispatcher) = serve_restartable(
        "restart-bad-binary",
        crate::server::is_binary_runnable(&executable_path),
        Some(overview.clone()),
    );

    let (mut connection, restart_result) = restart_over(&runtime_directory, session);

    assert_eq!(
        restart_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: rejection_message,
        }),
    );
    // Nothing was torn down, so the session answers the next request.
    assert_eq!(
        discovery_over(&mut connection, 3),
        IpcResult::Overview(overview)
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
    cleanup(&binary_directory);
}

/// A pane whose terminal exposes no descriptor cannot cross the swap, so the
/// restart is refused and the pane is named. Windows keeps every pane's
/// pseudoconsole in the supervisor process, so no pane holds a restart back
/// there.
#[cfg(unix)]
#[test]
fn a_restart_with_a_pane_that_has_no_terminal_descriptor_is_refused_naming_that_pane() {
    let stranded = PaneId::new();
    let panes = [koshi_pty::backend::state::CarriedPtyPane {
        pane_id: stranded,
        terminal_fd: None,
        process_id: 51234,
        pty_size: koshi_core::process::PtySize {
            column_count: 80,
            row_count: 24,
        },
        exit_status: None,
    }];
    let overview = overview_named("still-here");
    let (server, session, runtime_directory, dispatcher) = serve_restartable(
        "restart-no-fd",
        crate::server::can_carry_panes(&panes),
        Some(overview.clone()),
    );

    let (mut connection, restart_result) = restart_over(&runtime_directory, session);

    assert_eq!(
        restart_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: format!(
                "pane {stranded} has no terminal descriptor, \
                 so its terminal cannot cross the swap"
            ),
        }),
    );
    assert_eq!(
        discovery_over(&mut connection, 3),
        IpcResult::Overview(overview)
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_restart_on_an_attached_connection_detaches_that_client_and_restarts_nothing() {
    // A `Restart` reaches the dispatcher only over a connection that is serving
    // requests. An attached connection is carrying one client's events instead,
    // so the request ends that stream and no restart request is made.
    let client = ClientId::new();
    let (server, session, runtime_directory, dispatcher, seen) =
        serve_attachable("attached-restart", client);
    let mut connection = attach_to(&runtime_directory, session, client);

    connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Restart,
        })
        .expect("send restart");

    let RuntimeEvent::ClientDetached { client_id, .. } = seen.recv().expect("detach event") else {
        panic!("expected ClientDetached");
    };
    assert_eq!(client_id, client);
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
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

// --- leaving ---

/// Wait until `server` counts no attached client's connection, and hand back
/// how many it counts. Fails the test rather than hanging if one never ends.
fn wait_for_clients_to_leave(server: &IpcServer) -> usize {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while server.attached_connections() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    server.attached_connections()
}

/// Wait until `server` counts `want` attached clients' connections, and hand
/// back how many it counts. The count rises on the serving thread after the
/// attach reply is written, so a caller that just read that reply polls here.
fn wait_for_attached(server: &IpcServer, want: usize) -> usize {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while server.attached_connections() != want && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    server.attached_connections()
}

#[test]
fn every_attached_clients_connection_is_counted_while_it_is_read() {
    let client = ClientId::new();
    let (server, session, runtime_directory, dispatcher, _seen) =
        serve_attachable("two-attached", client);

    let first_attached_connection = attach_to(&runtime_directory, session, client);
    let second_attached_connection = attach_to(&runtime_directory, session, client);
    assert_eq!(
        wait_for_attached(&server, 2),
        2,
        "both attached clients' connections are counted"
    );

    drop(first_attached_connection);
    assert_eq!(
        wait_for_attached(&server, 1),
        1,
        "the connection that is still read is the one left counted"
    );

    drop(second_attached_connection);
    assert_eq!(wait_for_clients_to_leave(&server), 0);

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn every_key_a_client_sent_reaches_the_dispatcher_before_that_client_leaves() {
    // A client that reads the restart frame sends `Leaving` and writes nothing
    // after it. Requests arrive in the order the client queued them, so reading
    // that one is what says the session holds every key that client typed. The
    // image swap waits for exactly this before it carries the session out.
    let client = ClientId::new();
    let (server, session, runtime_directory, dispatcher, seen) =
        serve_attachable("leaving", client);
    let mut connection = attach_to(&runtime_directory, session, client);
    assert_eq!(
        wait_for_attached(&server, 1),
        1,
        "the attached client's connection is counted while it is read"
    );

    let typed = [
        KeyChord::from_parts(ModFlags::CTRL, Key::Char('a')),
        KeyChord::from_parts(ModFlags::CTRL, Key::Char('b')),
        KeyChord::from_parts(ModFlags::CTRL, Key::Char('c')),
    ];
    for (round, chord) in typed.iter().enumerate() {
        connection
            .send(&IpcRequest {
                request_id: 3 + round as u64,
                request_kind: IpcRequestKind::KeyPress { chord: *chord },
            })
            .expect("send the key press");
    }
    connection
        .send(&IpcRequest {
            request_id: 6,
            request_kind: IpcRequestKind::Leaving,
        })
        .expect("send leaving");

    for chord in typed {
        let RuntimeEvent::ClientKeyPress {
            client_id,
            chord: pressed,
        } = seen
            .recv_timeout(Duration::from_secs(5))
            .expect("key press event")
        else {
            panic!("expected ClientKeyPress");
        };
        assert_eq!(client_id, client);
        assert_eq!(pressed, chord);
    }
    // The reading half ends on the request that follows those keys, so the
    // client's image record is released here and nowhere earlier.
    let RuntimeEvent::ClientDetached {
        client_id,
        is_streamed,
        ..
    } = seen
        .recv_timeout(Duration::from_secs(5))
        .expect("detach event")
    else {
        panic!("expected ClientDetached");
    };
    assert_eq!(client_id, client, "leaving detaches the client that left");
    assert!(is_streamed, "the client that left was carrying a stream");
    assert_eq!(
        wait_for_clients_to_leave(&server),
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
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_control_connection_that_leaves_is_closed_with_no_answer() {
    // Nothing on a control connection is left half-answered when it leaves:
    // every request it sent was answered as it was served, and the request that
    // ends it carries no answer of its own.
    let client = ClientId::new();
    let (server, session, runtime_directory, dispatcher, seen) =
        serve_attachable("leaving-control", client);

    let mut connection = connect_to(&runtime_directory, session);
    connection
        .send(&hello_for(&runtime_directory, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, hello_accepted());

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
    // A control connection carries no client, so nothing about it reaches the
    // session.
    assert_eq!(
        seen.recv_timeout(Duration::from_secs(2)).unwrap_err(),
        mpsc::RecvTimeoutError::Timeout,
    );
    assert_eq!(server.attached_connections(), 0);

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

// --- rotating the token ---

#[test]
fn a_rotated_token_is_advertised_and_the_one_before_it_is_refused() {
    let (server, session, runtime_directory, dispatcher) = serve("rotate-token", None);
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session);
    let initial_endpoint =
        EndpointFile::load_from_path(&endpoint_path).expect("endpoint file readable");

    server
        .rotate_token()
        .expect("the fresh token is advertised");

    let rotated_endpoint =
        EndpointFile::load_from_path(&endpoint_path).expect("endpoint file readable");
    assert_ne!(
        rotated_endpoint.connection_token, initial_endpoint.connection_token,
        "the rotation advertises a new secret",
    );
    assert_eq!(
        rotated_endpoint.socket_address, initial_endpoint.socket_address,
        "the address the session is serving on does not change",
    );

    let mut stale_connection = connect_to(&runtime_directory, session);
    stale_connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                min_protocol_version: MIN_PROTOCOL_VERSION,
                max_protocol_version: PROTOCOL_VERSION,
                connection_token: initial_endpoint.connection_token,
                is_remote: false,
            },
        })
        .expect("send hello with the token from before the rotation");
    let refusal: IpcResponse = stale_connection.recv().expect("reply");
    assert_eq!(
        refusal.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let mut accepted_connection = connect_to(&runtime_directory, session);
    accepted_connection
        .send(&hello_for(&runtime_directory, session))
        .expect("send hello with the rotated secret");
    let accepted: IpcResponse = accepted_connection.recv().expect("hello reply");
    assert_eq!(accepted.answer_result, hello_accepted());

    drop(stale_connection);
    drop(accepted_connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn rotating_the_token_takes_connections_again_after_the_intake_closed() {
    // The image swap closes the intake and then finds it cannot go through
    // with the swap. The session keeps this socket, so it has to serve on it
    // again.
    let client = ClientId::new();
    let (server, session, runtime_directory, dispatcher, seen) =
        serve_attachable("rotate-reopen", client);

    server.close_intake();
    server
        .rotate_token()
        .expect("the fresh token is advertised");

    let mut connection = connect_to(&runtime_directory, session);
    connection
        .send(&hello_for(&runtime_directory, session))
        .expect("send hello");
    let accepted: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(accepted.answer_result, hello_accepted());

    // Served, not merely accepted: what this connection sends reaches the
    // dispatcher again.
    let chord = KeyChord::from_parts(ModFlags::CTRL, Key::Char('r'));
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Attach {
                viewport: TEST_VIEWPORT_SIZE,
                event_filter: EventFilterSpec::All,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities: koshi_ipc::protocol::GraphicsCapabilities::default(),
                cell_size: None,
            },
        })
        .expect("send attach");
    let attached: IpcResponse = connection.recv().expect("attach reply");
    assert_eq!(attached.request_id, Some(2));
    connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::KeyPress { chord },
        })
        .expect("send the key press");
    let RuntimeEvent::ClientKeyPress {
        client_id,
        chord: pressed,
    } = seen
        .recv_timeout(Duration::from_secs(5))
        .expect("key press")
    else {
        panic!("expected ClientKeyPress");
    };
    assert_eq!(client_id, client);
    assert_eq!(pressed, chord);

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_rotation_that_cannot_advertise_the_fresh_token_refuses_the_one_before_it() {
    let (server, session, runtime_directory, dispatcher) = serve("rotate-write-fails", None);
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session);
    let initial_endpoint =
        EndpointFile::load_from_path(&endpoint_path).expect("endpoint file readable");

    // A directory where the endpoint file goes: the rotation mints the fresh
    // token and then cannot rename over it.
    std::fs::remove_file(&endpoint_path).expect("take the endpoint file away");
    std::fs::create_dir_all(&endpoint_path).expect("plant a directory in its place");

    let Err(IpcError::EndpointFileWrite {
        endpoint_file_path, ..
    }) = server.rotate_token()
    else {
        panic!("a rotation that cannot write the endpoint file must report it");
    };
    assert_eq!(endpoint_file_path, endpoint_path.display().to_string());

    // The fresh token is the one the server accepts, so the token the endpoint
    // file advertised before the rotation opens nothing.
    let mut stale_connection =
        Connection::connect(&initial_endpoint.socket_address).expect("connect");
    stale_connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                min_protocol_version: MIN_PROTOCOL_VERSION,
                max_protocol_version: PROTOCOL_VERSION,
                connection_token: initial_endpoint.connection_token,
                is_remote: false,
            },
        })
        .expect("send hello with the token from before the rotation");
    let refusal: IpcResponse = stale_connection.recv().expect("reply");
    assert_eq!(
        refusal.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    drop(stale_connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

// --- closing the intake ---

#[test]
fn a_request_a_client_sends_after_the_intake_closes_never_reaches_the_dispatcher() {
    // The image swap closes the intake and then makes one pass over the runtime
    // inbox. A key press that reached the dispatcher after that pass would be
    // neither applied nor carried across, so the user's keystroke would vanish.
    let client = ClientId::new();
    let session = SessionId::new();
    let runtime_directory = build_test_runtime_directory("intake-closed");
    // The test keeps an inbox sender of its own, so it can end this client's
    // writing thread once the intake refuses the detach that would.
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let (dispatcher, seen) = spawn_attaching_dispatcher(inbox_rx, client, session);
    let server = IpcServer::start(&runtime_directory, session, inbox_tx.clone(), None)
        .expect("start serving");
    let mut connection = attach_to(&runtime_directory, session, client);
    let taken = KeyChord::from_parts(ModFlags::CTRL, Key::Char('a'));
    let refused = KeyChord::from_parts(ModFlags::CTRL, Key::Char('b'));

    // Before the close: the connection carries this client's keys, so the press
    // reaches the dispatcher.
    connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::KeyPress { chord: taken },
        })
        .expect("send the key press the session takes");
    let RuntimeEvent::ClientKeyPress { client_id, chord } = seen.recv().expect("key press event")
    else {
        panic!("expected ClientKeyPress");
    };
    assert_eq!(client_id, client);
    assert_eq!(chord, taken);

    server.close_intake();

    // After the close: the client is refused. Its send fails outright, or the
    // press is read and never handed over. Either way nothing more reaches the
    // dispatcher — including the detach the connection's own ending would
    // otherwise queue, which is what keeps this client's image record carried across.
    let _ = connection.send(&IpcRequest {
        request_id: 4,
        request_kind: IpcRequestKind::KeyPress { chord: refused },
    });
    assert_eq!(
        seen.recv_timeout(Duration::from_secs(2)).unwrap_err(),
        mpsc::RecvTimeoutError::Timeout,
    );

    drop(connection);
    // Closing this client's queue is what ends its writing thread, and the
    // dispatcher ends once every inbox sender is gone.
    inbox_tx
        .send(RuntimeEvent::ClientDetached {
            client_id: client,
            detached_at: SystemTime::now(),
            is_streamed: true,
        })
        .expect("the detach is queued");
    drop(inbox_tx);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

#[test]
fn a_connection_accepted_after_the_intake_closes_is_not_served() {
    // A caller that connects while the swap is carrying the state out must not
    // be answered: the session it would reach is about to be replaced.
    let client = ClientId::new();
    let (server, session, runtime_directory, dispatcher, seen) =
        serve_attachable("intake-closed-accept", client);

    server.close_intake();

    let mut connection = connect_to(&runtime_directory, session);
    // A send may fail as the accept loop drops the connection; the read that
    // follows reports end of stream either way.
    let _ = connection.send(&hello_for(&runtime_directory, session));
    assert!(
        matches!(
            connection.recv::<IpcResponse>(),
            Err(IpcError::Disconnected),
        ),
        "a connection accepted after the intake closed is closed unanswered",
    );
    assert_eq!(
        seen.recv_timeout(Duration::from_secs(2)).unwrap_err(),
        mpsc::RecvTimeoutError::Timeout,
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}

/// A detach for `client_id`, to hand an intake something to carry.
fn detach_of(client_id: ClientId) -> RuntimeEvent {
    RuntimeEvent::ClientDetached {
        client_id,
        detached_at: SystemTime::UNIX_EPOCH,
        is_streamed: true,
    }
}

#[test]
fn a_closed_intake_hands_nothing_over_until_it_reopens() {
    let intake = Intake::default();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let client = ClientId::new();

    assert!(intake.hand_over_event(&inbox_tx, detach_of(client)));
    intake.close_intake();
    assert!(!intake.hand_over_event(&inbox_tx, detach_of(client)));
    // Closing an intake that is already closed leaves it closed.
    intake.close_intake();
    assert!(!intake.hand_over_event(&inbox_tx, detach_of(client)));
    intake.reopen_intake();
    assert!(intake.hand_over_event(&inbox_tx, detach_of(client)));

    // The two the intake took, and neither of the two it refused.
    for _ in 0..2 {
        let RuntimeEvent::ClientDetached { client_id, .. } =
            inbox_rx.try_recv().expect("the event was handed over")
        else {
            panic!("expected ClientDetached");
        };
        assert_eq!(client_id, client);
    }
    assert_eq!(inbox_rx.try_recv().unwrap_err(), mpsc::TryRecvError::Empty);
}

#[test]
fn an_intake_hands_nothing_over_once_the_dispatcher_is_gone() {
    let intake = Intake::default();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    drop(inbox_rx);

    assert!(!intake.hand_over_event(&inbox_tx, detach_of(ClientId::new())));
}

#[test]
fn an_attached_clients_connection_is_counted_until_its_entry_is_dropped() {
    let intake = Arc::new(Intake::default());
    assert_eq!(intake.attached_connections(), 0);

    let first_attachment_guard = intake.record_attached_connection();
    let second_attachment_guard = intake.record_attached_connection();
    assert_eq!(intake.attached_connections(), 2);

    drop(first_attachment_guard);
    assert_eq!(intake.attached_connections(), 1);
    drop(second_attachment_guard);
    assert_eq!(intake.attached_connections(), 0);
}

/// A stand-in dispatcher that reports the `remote` flag of every attach it is
/// asked for and accepts each one as `client_id`. Holds the queues it hands out
/// open so the writing threads stay blocked. Exits when every inbox sender is
/// gone.
fn spawn_origin_reporting_dispatcher(
    inbox_rx: Receiver<RuntimeEvent>,
    client_id: ClientId,
    session_id: SessionId,
) -> (JoinHandle<()>, Receiver<bool>) {
    let (seen_tx, seen_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut queues = Vec::new();
        let ending_notice = Arc::new(EndingNotice::default());
        while let Ok(event) = inbox_rx.recv() {
            match event {
                RuntimeEvent::IpcAttach {
                    is_remote,
                    response_sender,
                    ..
                } => {
                    let (events_tx, events_rx) = mpsc::channel();
                    queues.push(events_tx);
                    if seen_tx.send(is_remote).is_err() {
                        break;
                    }
                    let _ = response_sender.send(Some(AttachAccepted {
                        client_id,
                        session_id,
                        session_structure: attached_structure(session_id),
                        deliveries: events_rx,
                        ending_notice: Arc::clone(&ending_notice),
                        resume_token: ConnectionToken::from_secret(MINTED_CONNECTION_TOKEN),
                        pane_area: None,
                    }));
                }
                RuntimeEvent::ClientDetached { .. } => queues.clear(),
                _ => {}
            }
        }
    });
    (handle, seen_rx)
}

/// Open a connection, say hello naming whether the caller reached this session
/// from another machine, attach on it, and read both replies back. The
/// connection comes back carrying `client_id`'s stream.
fn attach_saying_remote(
    runtime_directory: &Path,
    session: SessionId,
    client_id: ClientId,
    is_remote: bool,
) -> Connection {
    let endpoint = EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session,
    ))
    .expect("endpoint file readable");
    let mut connection = connect_to(runtime_directory, session);
    connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                min_protocol_version: MIN_PROTOCOL_VERSION,
                max_protocol_version: PROTOCOL_VERSION,
                connection_token: endpoint.connection_token,
                is_remote,
            },
        })
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, hello_accepted());

    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Attach {
                viewport: TEST_VIEWPORT_SIZE,
                event_filter: EventFilterSpec::All,
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
            session_id: session,
            session_structure: attached_structure(session),
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
    // The router sets `remote` on the Hello it sends for a caller its remote
    // listener admitted, and the dispatcher mints that caller's client with the
    // flag the attach carries as its origin. A connection that drops the flag on
    // the way makes a viewer on another machine read as a local one, and
    // `koshi share` prints a secret into a pane that viewer sees.
    let client = ClientId::new();
    let session = SessionId::new();
    let runtime_directory = build_test_runtime_directory("attach-origin");
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let (dispatcher, seen) = spawn_origin_reporting_dispatcher(inbox_rx, client, session);
    let server =
        IpcServer::start(&runtime_directory, session, inbox_tx, None).expect("start serving");

    let local = attach_saying_remote(&runtime_directory, session, client, false);
    assert_eq!(
        seen.recv_timeout(Duration::from_secs(5)),
        Ok(false),
        "a hello naming no other machine leaves the attach it carries local",
    );

    let remote = attach_saying_remote(&runtime_directory, session, client, true);
    assert_eq!(
        seen.recv_timeout(Duration::from_secs(5)),
        Ok(true),
        "a hello naming another machine marks the attach it carries remote",
    );

    drop(local);
    drop(remote);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_directory);
}
