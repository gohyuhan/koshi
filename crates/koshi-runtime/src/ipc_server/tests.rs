//! Tests for the control-socket server over real sockets: serving lifecycle,
//! handshake gating, fault containment per connection, the reply path from a
//! stand-in dispatcher thread, and what an attached connection's reading half
//! carries.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime};

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
use koshi_image::{AnimationFrame, FrameDelay, LoopPolicy};
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::image_chunk::IMAGE_CHUNK_FRAME_HEAD_BYTE_COUNT;
use koshi_ipc::layout::SessionLayout;
use koshi_ipc::protocol::{
    GraphicsCapabilities, IpcRequest, WireMouseAction, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use koshi_ipc::transport::ReceivedFrame;
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::PaneSizing;
use koshi_layout::tree::LayoutNode;
use koshi_renderer::snapshot::{
    ClientSnapshot, CursorSnapshot, GridView, ImagePlacementSnapshot, PaneSnapshot,
    PlacementClientSnapshot, PlacementPaneSnapshot, PlacementSnapshot, PlacementTabSnapshot,
    RenderSnapshot, ScrollbackMetadata, SessionSnapshot, TabSnapshot,
};
use koshi_terminal::graphics::{
    DecodedAnimation, DecodedImage, GraphicsProtocol, ImageAction, ImageDisplay, ImageRecord,
};
use koshi_terminal::grid::state::{Cell, Grid};
use koshi_terminal::style::Style;
use koshi_test_support::fixtures::{
    build_accepted_hello_result, build_counting_bytes, build_image_chunk_frame,
    build_key_input_for_chord, build_paint_report_request, build_restarting_refusal,
    count_program_runs, read_prepared_and_send_commit, send_committed_request,
    send_prepare_request, write_printing_program,
};

use crate::executable_watch::RESTART_RETRY_INTERVAL_DURATION;
use crate::runtime::bus::SUBSCRIBER_QUEUE_CAPACITY;
use crate::runtime::delivery_queue::{create_delivery_queue, DeliverySender};
use crate::runtime::event::{AttachAccepted, EndingNotice, SessionEnding};
use crate::runtime::painted_update::{
    PaintedUpdateReceiver, PaintedUpdateSender, ReceivedPaintedFrame, SessionFrame,
};
use crate::server::RestartRefusal;

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

/// Delete a directory a test made, and everything inside it. A directory that
/// is already gone is left alone.
fn delete_test_directory(runtime_directory: &Path) {
    let _ = std::fs::remove_dir_all(runtime_directory);
}

/// A fresh directory to stand in for the machine-wide shared directory: the
/// [`build_test_runtime_directory`] path for `shared-<directory_tag>`.
/// [`IpcServer::start`] creates it and this user's directory inside it.
#[cfg(unix)]
fn build_test_shared_directory(directory_tag: &str) -> PathBuf {
    build_test_runtime_directory(&format!("shared-{directory_tag}"))
}

/// Answer `runtime_event` when it is the [`RuntimeEvent::IpcPrepare`] probe a
/// `Prepare` sends, and hand back every other event.
fn answer_prepare_probe(runtime_event: RuntimeEvent) -> Option<RuntimeEvent> {
    match runtime_event {
        RuntimeEvent::IpcPrepare { response_sender } => {
            let _ = response_sender.send(());
            None
        }
        other_runtime_event => Some(other_runtime_event),
    }
}

/// Read the next event on `inbox_receiver`, which must be the
/// [`RuntimeEvent::IpcPrepare`] probe of a `Prepare`, and answer it.
#[track_caller]
fn answer_next_prepare_probe(inbox_receiver: &Receiver<RuntimeEvent>) {
    let Ok(RuntimeEvent::IpcPrepare { response_sender }) = inbox_receiver.recv() else {
        panic!("expected the Prepare probe");
    };
    response_sender
        .send(())
        .expect("the connection waits for the probe answer");
}

/// A stand-in for the dispatcher thread: drains the inbox, answers every
/// submitted command with `Ok` echoing its id, every discovery request with
/// `session_overview`, and each `IpcPrepare` probe ([`answer_prepare_probe`]).
/// Exits when every inbox sender is gone.
fn spawn_answering_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
    session_overview: Option<SessionOverview>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        for runtime_event in inbox_receiver.iter().filter_map(answer_prepare_probe) {
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

/// The attach answer a stand-in dispatcher sends: `client_id` in `session_id`,
/// with the structure [`build_attached_structure`] gives, `deliveries` as the
/// client's queue, `writer_handoff` as the host writes and owed-frame mark
/// beside that queue, `ending_notice`, the [`MINTED_CONNECTION_TOKEN`] resume
/// token, and no pane area.
fn build_attach_accepted(
    client_id: ClientId,
    session_id: SessionId,
    deliveries: DeliveryReceiver,
    writer_handoff: Arc<WriterHandoff>,
    ending_notice: Arc<EndingNotice>,
) -> AttachAccepted {
    AttachAccepted {
        client_id,
        session_id,
        session_structure: build_attached_structure(session_id),
        deliveries,
        writer_handoff,
        ending_notice,
        resume_token: ConnectionToken::from_secret(MINTED_CONNECTION_TOKEN),
        pane_area: None,
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
            floating_pane_slots: Vec::new(),
            floating_pane_ids: Vec::new(),
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
            active_tab_focused_pane_id: Some(pane_id),
            lock_mode: LockMode::Normal,
            last_applied_lock_command_id: None,
            last_applied_floating_pane_move_command_id: None,
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

/// One `pixel_width` × `pixel_height` image whose first byte is `red_byte`
/// and whose other bytes are `0`.
fn build_sized_image_record(red_byte: u8, pixel_width: u32, pixel_height: u32) -> Arc<ImageRecord> {
    Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: build_sized_decoded_image(red_byte, pixel_width, pixel_height),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay::default(),
        anchor: (0, 0),
    })
}

/// The pixels of [`build_sized_image_record`].
fn build_sized_decoded_image(
    red_byte: u8,
    pixel_width: u32,
    pixel_height: u32,
) -> Arc<DecodedImage> {
    let pixel_count = usize::try_from(pixel_width * pixel_height).expect("the pixel count fits");
    let mut rgba_bytes = vec![0; pixel_count * 4];
    rgba_bytes[0] = red_byte;
    Arc::new(DecodedImage {
        pixel_width,
        pixel_height,
        rgba_bytes,
    })
}

/// One record for each frame of a looping animation of `frame_count`
/// frames of `pixel_width` × `pixel_height` pixels, in frame order. Frame `n`
/// has first byte `n`, and every record names the one animation.
fn build_animation_frame_records(
    frame_count: u8,
    pixel_width: u32,
    pixel_height: u32,
) -> Vec<Arc<ImageRecord>> {
    let frame_images: Vec<Arc<DecodedImage>> = (0..frame_count)
        .map(|frame_index| build_sized_decoded_image(frame_index, pixel_width, pixel_height))
        .collect();
    let animation = Arc::new(
        DecodedAnimation::from_frames_and_loop_policy(
            frame_images
                .iter()
                .map(|frame_image| {
                    AnimationFrame::from_image_and_delay(
                        Arc::clone(frame_image),
                        FrameDelay::from_millisecond_ratio(66, 1).expect("the delay is valid"),
                    )
                    .expect("the frame is valid")
                })
                .collect(),
            LoopPolicy::Infinite,
        )
        .expect("the animation is valid"),
    );
    frame_images
        .into_iter()
        .map(|frame_image| {
            Arc::new(ImageRecord {
                protocol: GraphicsProtocol::Kitty,
                image: frame_image,
                animation: Some(Arc::clone(&animation)),
                action: ImageAction::Display,
                display: ImageDisplay::default(),
                anchor: (0, 0),
            })
        })
        .collect()
}

/// `base_snapshot` with its one pane showing `image_records`, placement `n`
/// of them with placement id `n + 1` and content id `n + 1`.
fn build_snapshot_showing_records(
    base_snapshot: &RenderSnapshot,
    image_records: &[Arc<ImageRecord>],
) -> RenderSnapshot {
    let mut render_snapshot = base_snapshot.clone();
    render_snapshot.pane_snapshots[0].image_placement_snapshots =
        build_image_placements(image_records);
    render_snapshot
}

/// `base_snapshot` with its one placement showing `image_record`.
fn build_snapshot_showing_record(
    base_snapshot: &RenderSnapshot,
    image_record: Arc<ImageRecord>,
) -> RenderSnapshot {
    let mut render_snapshot = base_snapshot.clone();
    render_snapshot.pane_snapshots[0].image_placement_snapshots[0] =
        ImagePlacementSnapshot::with_content_id(7, 1, image_record, (0, 0), 1, 1)
            .expect("the placement is valid");
    render_snapshot
}

/// One one-cell placement for each of `image_records`: placement `n` has
/// placement id `n + 1` and content id `n + 1`.
fn build_image_placements(image_records: &[Arc<ImageRecord>]) -> Vec<ImagePlacementSnapshot> {
    (1..)
        .zip(image_records)
        .map(|(placement_id, image_record)| {
            ImagePlacementSnapshot::with_content_id(
                placement_id,
                placement_id,
                Arc::clone(image_record),
                (0, 0),
                1,
                1,
            )
            .expect("the placement is valid")
        })
        .collect()
}

/// A placement preview for `render_snapshot`'s client whose source tab holds
/// one pane showing `image_records`.
fn build_placement_snapshot_showing_records(
    render_snapshot: &RenderSnapshot,
    image_records: &[Arc<ImageRecord>],
) -> PlacementSnapshot {
    let source_pane_id = render_snapshot.pane_snapshots[0].pane_id;
    PlacementSnapshot {
        session_id: render_snapshot.session_snapshot.session_id,
        source_pane_id,
        source_tab_id: render_snapshot.client_snapshot.active_tab_id,
        destination_tab_id: render_snapshot.client_snapshot.active_tab_id,
        session_placement_revision: 0,
        client_placement_revision: 0,
        source_tab_snapshot: PlacementTabSnapshot {
            layout_tree: LayoutNode::Pane(source_pane_id),
            tab_snapshot: render_snapshot.session_snapshot.active_tab_snapshot.clone(),
            pane_snapshots: vec![PlacementPaneSnapshot {
                pane_id: source_pane_id,
                terminal_grid_view: None,
                image_placement_snapshots: build_image_placements(image_records),
            }],
        },
        destination_tab_snapshot: None,
        client_snapshot: PlacementClientSnapshot {
            client_snapshot: render_snapshot.client_snapshot.clone(),
            reported_pane_area: None,
        },
        pane_sizing: PaneSizing::default(),
    }
}

/// The content id and availability of every image placement in
/// `painted_frame`, pane by pane.
fn list_painted_image_availability(painted_frame: &PaintedFrame) -> Vec<(u64, bool)> {
    painted_frame
        .pane_snapshots
        .iter()
        .flat_map(|pane_snapshot| &pane_snapshot.image_placement_snapshots)
        .map(|image_placement| {
            (
                image_placement.image_content_id,
                image_placement.is_available,
            )
        })
        .collect()
}

/// The content id of each upload in `image_uploads`, in order.
fn list_upload_content_ids(image_uploads: &[(u64, Arc<ImageRecord>)]) -> Vec<u64> {
    image_uploads
        .iter()
        .map(|(image_content_id, _)| *image_content_id)
        .collect()
}

/// One frame for `client_id` whose one pane holds `row_count` rows by
/// `column_count` columns of alternating `a` and `b`, and no image. No two
/// neighbors fold into one run: each cell is its own run on the wire.
fn build_alternating_grid_snapshot(
    client_id: ClientId,
    row_count: u16,
    column_count: u16,
) -> RenderSnapshot {
    let mut alternating_grid = Grid::build_blank(row_count, column_count, Style::default());
    for row_index in 0..row_count {
        for column_index in 0..column_count {
            let character = if column_index % 2 == 0 { 'a' } else { 'b' };
            *alternating_grid
                .get_cell_mut(row_index, column_index)
                .expect("the grid holds every cell") =
                Cell::from_character(character, 1, Style::default());
        }
    }
    let mut render_snapshot = build_image_snapshot(client_id, build_image_record(0));
    render_snapshot.pane_snapshots[0]
        .image_placement_snapshots
        .clear();
    render_snapshot.pane_snapshots[0].terminal_grid_view =
        Some(GridView::from_grid(&alternating_grid, 0));
    render_snapshot
}

/// A stand-in dispatcher that answers the first attach as `client_id` in
/// `session_id`, with no host-write byte waiting, and returns the sender of the
/// delivery queue that attach streams.
fn spawn_frame_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
    client_id: ClientId,
    session_id: SessionId,
) -> (JoinHandle<()>, DeliverySender) {
    let (delivery_sender, delivery_receiver) = create_delivery_queue(SUBSCRIBER_QUEUE_CAPACITY);
    let dispatcher_thread = std::thread::spawn(move || {
        answer_first_attach(
            &inbox_receiver,
            client_id,
            session_id,
            delivery_receiver,
            Arc::new(WriterHandoff::default()),
            Arc::new(EndingNotice::default()),
        );
    });
    (dispatcher_thread, delivery_sender)
}

/// A stand-in dispatcher that accepts attaches: it answers every attach as
/// `client_id`, holds the delivery queue it hands out open, and closes those
/// queues on a detach. The writing thread of each attached connection stays
/// blocked until then. It answers each `IpcPrepare` probe
/// ([`answer_prepare_probe`]). Every other event it drains is forwarded to the
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
        for runtime_event in inbox_receiver.iter().filter_map(answer_prepare_probe) {
            match runtime_event {
                RuntimeEvent::IpcAttach {
                    response_sender, ..
                } => {
                    let (delivery_sender, delivery_receiver) =
                        create_delivery_queue(SUBSCRIBER_QUEUE_CAPACITY);
                    delivery_senders.push(delivery_sender);
                    let _ = response_sender.send(Some(build_attach_accepted(
                        client_id,
                        session_id,
                        delivery_receiver,
                        Arc::new(WriterHandoff::default()),
                        Arc::clone(&ending_notice),
                    )));
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
/// `delivery_receiver`, `writer_handoff` and `ending_notice`, and drops
/// everything else it drains. Exits when every inbox sender is gone.
fn spawn_ending_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
    client_id: ClientId,
    session_id: SessionId,
    delivery_receiver: DeliveryReceiver,
    writer_handoff: Arc<WriterHandoff>,
    ending_notice: Arc<EndingNotice>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        answer_first_attach(
            &inbox_receiver,
            client_id,
            session_id,
            delivery_receiver,
            writer_handoff,
            ending_notice,
        );
    })
}

/// Drain `inbox_receiver` until every inbox sender is gone. The first attach
/// is answered as `client_id` in `session_id`, streaming `delivery_receiver`
/// and `writer_handoff` under `ending_notice`. Each `IpcPrepare` probe is
/// answered ([`answer_prepare_probe`]). Every attach after it and every other
/// event is dropped.
fn answer_first_attach(
    inbox_receiver: &Receiver<RuntimeEvent>,
    client_id: ClientId,
    session_id: SessionId,
    delivery_receiver: DeliveryReceiver,
    writer_handoff: Arc<WriterHandoff>,
    ending_notice: Arc<EndingNotice>,
) {
    let mut unanswered_delivery_receiver = Some(delivery_receiver);
    for runtime_event in inbox_receiver.iter().filter_map(answer_prepare_probe) {
        let RuntimeEvent::IpcAttach {
            response_sender, ..
        } = runtime_event
        else {
            continue;
        };
        let Some(delivery_receiver) = unanswered_delivery_receiver.take() else {
            continue;
        };
        let _ = response_sender.send(Some(build_attach_accepted(
            client_id,
            session_id,
            delivery_receiver,
            Arc::clone(&writer_handoff),
            Arc::clone(&ending_notice),
        )));
    }
}

/// Wait up to 5 seconds until no client writing thread is left on
/// `ending_notice`, and return how many are left.
fn wait_for_running_writer_count(ending_notice: &EndingNotice) -> usize {
    ending_notice.wait_until_writers_end(std::time::Instant::now() + Duration::from_secs(5))
}

/// Call `is_done` every 5 milliseconds until it returns `true` or
/// `timeout_duration` passes. Returns the last answer of `is_done`.
fn wait_until(timeout_duration: Duration, mut is_done: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout_duration;
    while !is_done() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    true
}

/// Serve a socket whose stand-in dispatcher answers the first attach with
/// `delivery_receiver` and `writer_handoff` under `ending_notice`, attach
/// one client, and return the first `frame_count` frames that client reads, in
/// order. Asserts that the client's writing thread ends after those frames.
fn read_frames_of_ending_session(
    directory_tag: &str,
    delivery_receiver: DeliveryReceiver,
    writer_handoff: Arc<WriterHandoff>,
    ending_notice: &Arc<EndingNotice>,
    frame_count: usize,
) -> Vec<SessionFrame<SessionEvent>> {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory(directory_tag);
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let dispatcher_thread = spawn_ending_dispatcher(
        inbox_receiver,
        client_id,
        session_id,
        delivery_receiver,
        writer_handoff,
        Arc::clone(ending_notice),
    );
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start serving");
    let mut connection = attach_test_client(&runtime_directory, session_id, client_id);

    let mut painted_update_receiver = PaintedUpdateReceiver::new();
    let read_frames: Vec<SessionFrame<SessionEvent>> = (0..frame_count)
        .map(|_| read_session_frame(&mut connection, &mut painted_update_receiver))
        .collect();
    assert_eq!(
        wait_for_running_writer_count(ending_notice),
        0,
        "the writing thread must end once the client holds its last frame"
    );

    drop(connection);
    ipc_server.shutdown();
    dispatcher_thread.join().expect("dispatcher exits");
    delete_test_directory(&runtime_directory);
    read_frames
}

/// [`read_frames_of_ending_session`] on a stream that carries no painted
/// update: each frame's event. Panics on a painted update.
fn read_events_of_ending_session(
    directory_tag: &str,
    delivery_receiver: DeliveryReceiver,
    writer_handoff: Arc<WriterHandoff>,
    ending_notice: &Arc<EndingNotice>,
    frame_count: usize,
) -> Vec<SessionEvent> {
    read_frames_of_ending_session(
        directory_tag,
        delivery_receiver,
        writer_handoff,
        ending_notice,
        frame_count,
    )
    .into_iter()
    .map(|session_frame| match session_frame {
        SessionFrame::Event(session_event) => session_event,
        other_frame => panic!("expected an event frame, got {other_frame:?}"),
    })
    .collect()
}

/// Read frames off `connection` until one JSON frame or the last part of one
/// painted update arrives, applying each part with `painted_update_receiver`.
/// A painted update is reported painted on `connection`, as a client reports
/// each update it paints.
fn read_session_frame(
    connection: &mut Connection,
    painted_update_receiver: &mut PaintedUpdateReceiver,
) -> SessionFrame<SessionEvent> {
    loop {
        let received_frame = connection
            .recv_message_or_binary::<SessionEvent>()
            .expect("the client is told");
        if let Some(session_frame) = painted_update_receiver
            .accept_session_frame(received_frame)
            .expect("a painted update part that applies")
        {
            if let SessionFrame::Painted(received_painted_frame) = &session_frame {
                connection
                    .send(&build_paint_report_request(
                        received_painted_frame.update_sequence,
                    ))
                    .expect("the paint report is written");
            }
            return session_frame;
        }
    }
}

/// The painted update a client that holds nothing yet receives for
/// `render_snapshot` and its JSON part `painted_frame`: update 1.
fn build_first_received_painted_frame(
    render_snapshot: &RenderSnapshot,
    painted_frame: &PaintedFrame,
) -> ReceivedPaintedFrame {
    let part_payloads = PaintedUpdateSender::new()
        .build_update_frames(render_snapshot, painted_frame, false)
        .expect("the painted update fits the frame cap")
        .expect("a first update is sent");
    let mut painted_update_receiver = PaintedUpdateReceiver::new();
    let mut received_painted_frames: Vec<ReceivedPaintedFrame> = Vec::new();
    for part_payload in part_payloads {
        if let Some(SessionFrame::Painted(received_painted_frame)) = painted_update_receiver
            .accept_session_frame::<SessionEvent>(ReceivedFrame::Binary(part_payload))
            .expect("a painted update part that applies")
        {
            received_painted_frames.push(*received_painted_frame);
        }
    }
    assert_eq!(received_painted_frames.len(), 1);
    received_painted_frames.remove(0)
}

/// The byte count of every binary frame the first painted update of
/// `render_snapshot` goes out as.
fn compute_painted_update_byte_count(render_snapshot: &RenderSnapshot) -> usize {
    PaintedUpdateSender::new()
        .build_update_frames(render_snapshot, &build_wire_frame(render_snapshot), false)
        .expect("the painted update fits the frame cap")
        .expect("a first update is sent")
        .iter()
        .map(Vec::len)
        .sum()
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
        let (_, delivery_receiver, writer_handoff) = event_bus.subscribe();
        let tab_id = TabId::new();
        for _ in 0..SUBSCRIBER_QUEUE_CAPACITY {
            event_bus.publish(&Event::TabCreated(TabCreated { tab_id }));
        }
        let ending_notice = Arc::clone(event_bus.get_ending_notice());
        event_bus.publish(&ending_event);
        assert_eq!(ending_notice.get_session_ending(), Some(session_ending));

        assert_eq!(
            read_events_of_ending_session(
                directory_tag,
                delivery_receiver,
                writer_handoff,
                &ending_notice,
                1
            ),
            vec![ending_frame]
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
    let (subscriber_id, delivery_receiver, writer_handoff) = event_bus.subscribe();
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
        read_events_of_ending_session(
            "detach-then-quit",
            delivery_receiver,
            writer_handoff,
            &ending_notice,
            1
        ),
        vec![SessionEvent::Detached]
    );
}

#[test]
fn a_client_reads_the_quit_frame_alone_when_the_events_that_ended_the_session_are_still_queued() {
    // The pane's exit is queued ahead of the quit, and the notice is raised.
    // The client reads the quit alone.
    use koshi_core::event::{Event, PaneProcessExited, QuitCause};

    use crate::runtime::bus::EventBus;

    let mut event_bus = EventBus::new();
    let (_, delivery_receiver, writer_handoff) = event_bus.subscribe();
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
        read_events_of_ending_session(
            "quit-behind-queue",
            delivery_receiver,
            writer_handoff,
            &ending_notice,
            1
        ),
        vec![SessionEvent::Quit]
    );
}

#[test]
fn host_writes_queued_before_the_session_ends_reach_the_client_ahead_of_its_last_frame() {
    // Both host writes wait behind one wake. The ending drops the pane's exit
    // and the wake, writes the waiting bytes as one host write, then the last
    // frame.
    use koshi_core::event::{Event, PaneProcessExited, QuitCause};

    use crate::runtime::bus::{EventBus, HostWrite};

    for (ending_event, ending_frame, directory_tag) in [
        (
            Event::Restarting,
            SessionEvent::Restarting,
            "restart-host-writes",
        ),
        (
            Event::Quit(QuitCause::Requested),
            SessionEvent::Quit,
            "quit-host-writes",
        ),
    ] {
        let mut event_bus = EventBus::new();
        let (subscriber_id, delivery_receiver, writer_handoff) = event_bus.subscribe();
        event_bus.publish(&Event::PaneProcessExited(PaneProcessExited {
            pane_id: PaneId::new(),
            exit_code: Some(0),
            signal: None,
        }));
        assert!(
            event_bus.queue_host_write(subscriber_id, &HostWrite::from_clipboard_copy("c", b"one"))
        );
        assert!(
            event_bus.queue_host_write(subscriber_id, &HostWrite::from_clipboard_copy("p", b"two"))
        );
        let ending_notice = Arc::clone(event_bus.get_ending_notice());
        event_bus.publish(&ending_event);

        assert_eq!(
            read_events_of_ending_session(
                directory_tag,
                delivery_receiver,
                writer_handoff,
                &ending_notice,
                2
            ),
            vec![
                SessionEvent::HostWrite {
                    host_write_bytes: b"\x1b]52;c;b25l\x07\x1b]52;p;dHdv\x07".to_vec(),
                },
                ending_frame,
            ]
        );
    }
}

#[test]
fn host_writes_that_wait_reach_the_client_ahead_of_the_deliveries_queued_after_them() {
    // Both host writes wait behind one wake, which the switch and the closed
    // queue follow.
    use crate::runtime::bus::{EventBus, HostWrite};

    let next_session_id = SessionId::new();
    let mut event_bus = EventBus::new();
    let (subscriber_id, delivery_receiver, writer_handoff) = event_bus.subscribe();
    assert!(event_bus.queue_host_write(subscriber_id, &HostWrite::from_clipboard_copy("c", b"one")));
    assert!(event_bus.queue_host_write(subscriber_id, &HostWrite::from_clipboard_copy("p", b"two")));
    assert!(event_bus.try_send_switch(subscriber_id, next_session_id, None));
    event_bus.unsubscribe(subscriber_id);
    let ending_notice = Arc::clone(event_bus.get_ending_notice());

    assert_eq!(
        read_events_of_ending_session(
            "host-writes-before-switch",
            delivery_receiver,
            writer_handoff,
            &ending_notice,
            3
        ),
        vec![
            SessionEvent::HostWrite {
                host_write_bytes: b"\x1b]52;c;b25l\x07\x1b]52;p;dHdv\x07".to_vec(),
            },
            SessionEvent::SwitchTo {
                session_id: next_session_id,
                owner_user_id: None,
            },
            SessionEvent::Detached,
        ]
    );
}

#[test]
fn a_host_write_whose_wake_found_the_queue_full_still_reaches_the_client() {
    // The queue holds its full 1024 deliveries, so the wake is dropped. The
    // waiting bytes go out before the first delivery the writing thread
    // reads.
    use koshi_core::event::{Event, TabCreated};

    use crate::runtime::bus::{EventBus, HostWrite, SUBSCRIBER_QUEUE_CAPACITY};

    let tab_id = TabId::new();
    let mut event_bus = EventBus::new();
    let (subscriber_id, delivery_receiver, writer_handoff) = event_bus.subscribe();
    for _ in 0..SUBSCRIBER_QUEUE_CAPACITY {
        event_bus.publish(&Event::TabCreated(TabCreated { tab_id }));
    }
    assert!(event_bus.queue_host_write(subscriber_id, &HostWrite::from_clipboard_copy("c", b"one")));
    event_bus.unsubscribe(subscriber_id);
    let ending_notice = Arc::clone(event_bus.get_ending_notice());

    let read_frames = read_events_of_ending_session(
        "host-write-full-queue",
        delivery_receiver,
        writer_handoff,
        &ending_notice,
        SUBSCRIBER_QUEUE_CAPACITY + 2,
    );

    let mut expected_frames = vec![SessionEvent::HostWrite {
        host_write_bytes: b"\x1b]52;c;b25l\x07".to_vec(),
    }];
    expected_frames.extend(vec![
        SessionEvent::TabCreated { tab_id };
        SUBSCRIBER_QUEUE_CAPACITY
    ]);
    expected_frames.push(SessionEvent::Detached);
    assert_eq!(read_frames, expected_frames);
}

#[test]
fn a_dropped_frame_wakes_the_dispatcher_for_its_client_after_the_next_read() {
    // The queue holds its full 1024 deliveries, so the frame is dropped and the
    // writer handoff owes a frame. The writing thread takes that mark after
    // the first delivery it reads, before it writes that delivery.
    use koshi_core::event::{Event, TabCreated};

    use crate::runtime::bus::{EventBus, SUBSCRIBER_QUEUE_CAPACITY};

    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let mut event_bus = EventBus::new();
    let (subscriber_id, delivery_receiver, writer_handoff) = event_bus.subscribe();
    for _ in 0..SUBSCRIBER_QUEUE_CAPACITY {
        event_bus.publish(&Event::TabCreated(TabCreated { tab_id }));
    }
    assert!(!event_bus.try_send_frame(
        subscriber_id,
        Box::new(build_alternating_grid_snapshot(client_id, 2, 2))
    ));
    let ending_notice = Arc::clone(event_bus.get_ending_notice());
    let runtime_directory = build_test_runtime_directory("owed-frame-wake");
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (forwarded_event_sender, forwarded_event_receiver) = mpsc::channel();
    let dispatcher_thread = std::thread::spawn(move || {
        let mut unanswered_stream = Some((delivery_receiver, writer_handoff));
        for runtime_event in inbox_receiver.iter().filter_map(answer_prepare_probe) {
            match runtime_event {
                RuntimeEvent::IpcAttach {
                    response_sender, ..
                } => {
                    if let Some((delivery_receiver, writer_handoff)) = unanswered_stream.take() {
                        let _ = response_sender.send(Some(build_attach_accepted(
                            client_id,
                            session_id,
                            delivery_receiver,
                            writer_handoff,
                            Arc::clone(&ending_notice),
                        )));
                    }
                }
                forwarded_runtime_event => {
                    let _ = forwarded_event_sender.send(forwarded_runtime_event);
                }
            }
        }
    });
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start serving");
    let mut connection = attach_test_client(&runtime_directory, session_id, client_id);

    assert_eq!(
        connection.recv::<SessionEvent>().expect("the first event"),
        SessionEvent::TabCreated { tab_id }
    );
    assert!(
        matches!(
            forwarded_event_receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("the writing thread wakes the dispatcher"),
            RuntimeEvent::QueueRoomFreed
        ),
        "the first forwarded event is the wake"
    );
    for _ in 1..SUBSCRIBER_QUEUE_CAPACITY {
        assert_eq!(
            connection.recv::<SessionEvent>().expect("the next event"),
            SessionEvent::TabCreated { tab_id }
        );
    }
    event_bus.unsubscribe(subscriber_id);
    assert_eq!(
        connection.recv::<SessionEvent>().expect("the goodbye"),
        SessionEvent::Detached
    );

    drop(connection);
    ipc_server.shutdown();
    dispatcher_thread.join().expect("dispatcher exits");
    delete_test_directory(&runtime_directory);
    assert_eq!(
        forwarded_event_receiver
            .try_iter()
            .filter(|runtime_event| matches!(runtime_event, RuntimeEvent::QueueRoomFreed))
            .count(),
        0,
        "the writing thread took the mark once"
    );
}

#[test]
fn a_host_write_longer_than_one_frame_reaches_the_client_as_parts_then_its_last_run() {
    use koshi_ipc::event::MAX_HOST_WRITE_BYTE_COUNT;

    use crate::runtime::bus::{EventBus, HostWrite};
    use crate::runtime::clipboard::encode_osc52_copy;

    let mut event_bus = EventBus::new();
    let (subscriber_id, delivery_receiver, writer_handoff) = event_bus.subscribe();
    let copied_bytes = vec![b'a'; MAX_HOST_WRITE_BYTE_COUNT];
    let host_write_bytes = encode_osc52_copy("c", &copied_bytes);
    assert_eq!(host_write_bytes.len(), 16_777_180);
    assert!(event_bus.queue_host_write(
        subscriber_id,
        &HostWrite::from_clipboard_copy("c", &copied_bytes)
    ));
    event_bus.unsubscribe(subscriber_id);
    let ending_notice = Arc::clone(event_bus.get_ending_notice());

    assert_eq!(
        read_events_of_ending_session(
            "host-write-parts",
            delivery_receiver,
            writer_handoff,
            &ending_notice,
            3
        ),
        vec![
            SessionEvent::HostWritePart {
                host_write_bytes: host_write_bytes[..MAX_HOST_WRITE_BYTE_COUNT].to_vec(),
            },
            SessionEvent::HostWrite {
                host_write_bytes: host_write_bytes[MAX_HOST_WRITE_BYTE_COUNT..].to_vec(),
            },
            SessionEvent::Detached,
        ]
    );
}

#[test]
fn a_host_write_after_the_writing_thread_ended_with_its_wake_unread_is_not_taken() {
    use crate::runtime::bus::{EventBus, HostWrite};

    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let mut event_bus = EventBus::new();
    let (subscriber_id, delivery_receiver, writer_handoff) = event_bus.subscribe();
    // One pane of 1000 rows by 1100 columns of alternating `a` and `b`: the
    // painted update is inside the frame cap, and larger than a connection
    // holds while its client reads nothing.
    let large_snapshot = build_alternating_grid_snapshot(client_id, 1000, 1100);
    let painted_update_byte_count = compute_painted_update_byte_count(&large_snapshot);
    assert!(
        painted_update_byte_count > 1024 * 1024
            && painted_update_byte_count < transport::MAX_FRAME_BYTE_COUNT as usize,
        "the painted update is {painted_update_byte_count} bytes"
    );
    // The frame, then a host write and its wake, wait on the queue before the
    // client attaches. The writing thread writes the host write first, then
    // stays on the frame: it never reads the wake.
    assert!(event_bus.try_send_frame(subscriber_id, Box::new(large_snapshot)));
    let host_write = HostWrite::from_clipboard_copy("c", b"hi");
    assert!(event_bus.queue_host_write(subscriber_id, &host_write));
    let ending_notice = Arc::new(EndingNotice::default());
    let runtime_directory = build_test_runtime_directory("host-write-after-writer-end");
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let dispatcher_thread = spawn_ending_dispatcher(
        inbox_receiver,
        client_id,
        session_id,
        delivery_receiver,
        writer_handoff,
        Arc::clone(&ending_notice),
    );
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start serving");
    let mut connection = attach_test_client(&runtime_directory, session_id, client_id);
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("the host write that waited"),
        SessionEvent::HostWrite {
            host_write_bytes: b"\x1b]52;c;aGk=\x07".to_vec(),
        }
    );

    // The client closes the connection: the frame write fails, and the
    // writing thread ends with the wake still on the queue.
    drop(connection);
    assert_eq!(
        wait_for_running_writer_count(&ending_notice),
        0,
        "the writing thread must end once its frame write fails"
    );

    assert!(!event_bus.queue_host_write(subscriber_id, &host_write));
    assert!(!event_bus.has_subscriber(subscriber_id));

    ipc_server.shutdown();
    dispatcher_thread.join().expect("dispatcher exits");
    delete_test_directory(&runtime_directory);
}

#[test]
fn a_detach_goodbye_is_written_only_while_the_link_is_not_lost() {
    assert_eq!(
        build_detach_goodbye(&AtomicBool::new(false)),
        Some(SessionEvent::Detached)
    );
    assert_eq!(build_detach_goodbye(&AtomicBool::new(true)), None);
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
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start serving");
    (
        ipc_server,
        session_id,
        runtime_directory,
        dispatcher_thread,
        received_runtime_events,
    )
}

/// The `Attach` request with id `2` for a [`CLIENT_VIEWPORT_SIZE`] viewport
/// that names `resume_client_id` to come back as and carries
/// `link_timeout_ms`, with no resume token, no pane area, no cell size, and
/// the default graphics capabilities.
fn build_attach_request(
    resume_client_id: Option<ClientId>,
    link_timeout_ms: Option<u32>,
) -> IpcRequest {
    IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Attach {
            viewport_size: CLIENT_VIEWPORT_SIZE,
            resume_client_id,
            resume_token: None,
            pane_area: None,
            graphics_capabilities: GraphicsCapabilities::default(),
            cell_size: None,
            link_timeout_ms,
            max_frames_per_second: 60,
        },
    }
}

/// Open a connection, say hello, attach on it with `Prepare` and `Commit`, and
/// read every reply back. The connection comes back carrying `client_id`'s
/// stream.
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
    let mut connection = connect_greeted_connection(runtime_directory, session_id);

    send_committed_request(
        &mut connection,
        IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Attach {
                viewport_size: CLIENT_VIEWPORT_SIZE,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities,
                cell_size: None,
                link_timeout_ms: None,
                max_frames_per_second: 60,
            },
        },
    );
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
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start serving");
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);
    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    let _: IpcResponse = connection.recv().expect("hello reply");
    let measured_cell_size =
        PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions");
    send_prepare_request(
        &mut connection,
        IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Attach {
                viewport_size: CLIENT_VIEWPORT_SIZE,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities: GraphicsCapabilities::default(),
                cell_size: Some(measured_cell_size),
                link_timeout_ms: None,
                max_frames_per_second: 60,
            },
        },
    );
    answer_next_prepare_probe(&inbox_receiver);
    read_prepared_and_send_commit(&mut connection, 2);

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
    let (delivery_sender, delivery_receiver) = create_delivery_queue(SUBSCRIBER_QUEUE_CAPACITY);
    response_sender
        .send(Some(build_attach_accepted(
            client_id,
            session_id,
            delivery_receiver,
            Arc::new(WriterHandoff::default()),
            Arc::new(EndingNotice::default()),
        )))
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
    delete_test_directory(&runtime_directory);
}

#[test]
fn a_resize_over_the_viewport_cell_limit_is_dropped_and_the_next_resize_crosses() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory("resize-cell-limit");
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start serving");
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);
    connection
        .send(&build_hello_request(&runtime_directory, session_id))
        .expect("send hello");
    let _: IpcResponse = connection.recv().expect("hello reply");
    send_prepare_request(&mut connection, build_attach_request(None, None));
    answer_next_prepare_probe(&inbox_receiver);
    read_prepared_and_send_commit(&mut connection, 2);
    let RuntimeEvent::IpcAttach {
        response_sender, ..
    } = inbox_receiver
        .recv()
        .expect("attach reaches the dispatcher")
    else {
        panic!("expected IpcAttach");
    };
    let (delivery_sender, delivery_receiver) = create_delivery_queue(SUBSCRIBER_QUEUE_CAPACITY);
    response_sender
        .send(Some(build_attach_accepted(
            client_id,
            session_id,
            delivery_receiver,
            Arc::new(WriterHandoff::default()),
            Arc::new(EndingNotice::default()),
        )))
        .expect("the dispatcher receives the accepted attach");
    let _: IpcResponse = connection.recv().expect("attach reply");
    let fitting_viewport_size = Size {
        column_count: 2048,
        row_count: 1024,
    };

    for (request_id, viewport_size) in [
        (
            3,
            Size {
                column_count: 65_535,
                row_count: 65_535,
            },
        ),
        (
            4,
            Size {
                column_count: 2048,
                row_count: 1025,
            },
        ),
        (5, fitting_viewport_size),
    ] {
        connection
            .send(&IpcRequest {
                request_id,
                request_kind: IpcRequestKind::Resize {
                    viewport_size,
                    pane_area: None,
                    cell_size: None,
                },
            })
            .expect("send resize");
    }
    let RuntimeEvent::Resize {
        client_id: resized_client_id,
        viewport_size,
        ..
    } = inbox_receiver
        .recv()
        .expect("the fitting resize reaches the dispatcher")
    else {
        panic!("expected Resize");
    };
    assert_eq!(
        (resized_client_id, viewport_size),
        (client_id, fitting_viewport_size)
    );

    drop(delivery_sender);
    drop(connection);
    ipc_server.shutdown();
    delete_test_directory(&runtime_directory);
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
    painted_update_receiver: PaintedUpdateReceiver,
    delivery_sender: DeliverySender,
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
        let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
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
            painted_update_receiver: PaintedUpdateReceiver::new(),
            delivery_sender,
        }
    }

    /// Queue `delivery` on the attached client's stream.
    fn send_delivery(&self, delivery: Delivery) {
        self.delivery_sender
            .try_send(delivery)
            .expect("the writing thread holds the delivery queue");
    }

    /// Queue a [`Delivery::PlacementCommandRejected`] with a new command id
    /// after every delivery already queued, and return the event the client
    /// reads for it.
    fn send_rejected_placement_command(&self) -> SessionEvent {
        let command_id = CommandId::new();
        self.send_delivery(Delivery::PlacementCommandRejected(command_id));
        SessionEvent::PlacementCommandRejected { command_id }
    }

    /// Read the next frame the attached client receives: a JSON frame, an
    /// image chunk, or a whole painted update.
    fn read_next_session_frame(&mut self) -> SessionFrame<SessionEvent> {
        read_session_frame(&mut self.connection, &mut self.painted_update_receiver)
    }

    /// Read the next JSON frame the attached client receives. Panics on any
    /// other frame.
    fn read_session_event(&mut self) -> SessionEvent {
        match self.read_next_session_frame() {
            SessionFrame::Event(session_event) => session_event,
            other_frame => panic!("expected an event frame, got {other_frame:?}"),
        }
    }

    /// Read the next painted update the attached client receives, and return
    /// its JSON part. Panics on any other frame.
    fn read_painted_frame(&mut self) -> PaintedFrame {
        match self.read_next_session_frame() {
            SessionFrame::Painted(received_painted_frame) => {
                PaintedFrame::clone(&received_painted_frame.painted_frame)
            }
            other_frame => panic!("expected a painted update, got {other_frame:?}"),
        }
    }

    /// Close the connection and the queue, stop the server, join the
    /// dispatcher, and remove the runtime directory.
    fn close_frame_stream(self) {
        drop(self.connection);
        drop(self.delivery_sender);
        self.ipc_server.shutdown();
        self.dispatcher_thread.join().expect("dispatcher exits");
        delete_test_directory(&self.runtime_directory);
    }
}

/// Read every frame the client of `frame_stream` receives through
/// `end_event`, and keep what the client shows: the available image
/// placements of the newest painted update, and of the newest placement
/// preview. Each paint report names that preview as held. An
/// `ImageCacheReset` clears both. Returns every identity the
/// `ImageContentReleased` frames name, in order.
///
/// # Panics
/// When an `ImageContentReleased` names an identity that the newest painted
/// update or the held preview shows.
fn read_frames_checking_image_releases(
    frame_stream: &mut AttachedFrameStream,
    end_event: &SessionEvent,
) -> Vec<u64> {
    let mut painted_image_content_ids: HashSet<u64> = HashSet::new();
    let mut preview_image_content_ids: HashSet<u64> = HashSet::new();
    let mut held_placement_request_id = None;
    let mut released_image_content_ids = Vec::new();
    loop {
        let received_frame = frame_stream
            .connection
            .recv_message_or_binary::<SessionEvent>()
            .expect("the client is told");
        let Some(session_frame) = frame_stream
            .painted_update_receiver
            .accept_session_frame(received_frame)
            .expect("a painted update part that applies")
        else {
            continue;
        };
        match session_frame {
            SessionFrame::Painted(received_painted_frame) => {
                painted_image_content_ids = received_painted_frame
                    .painted_frame
                    .pane_snapshots
                    .iter()
                    .flat_map(|pane_snapshot| &pane_snapshot.image_placement_snapshots)
                    .filter(|image_placement| image_placement.is_available)
                    .map(|image_placement| image_placement.image_content_id)
                    .collect();
                let update_sequence = received_painted_frame.update_sequence;
                frame_stream
                    .connection
                    .send(&IpcRequest {
                        request_id: update_sequence,
                        request_kind: IpcRequestKind::PaintedUpdate {
                            update_sequence,
                            shown_placement_request_id: held_placement_request_id,
                        },
                    })
                    .expect("the paint report is written");
            }
            SessionFrame::ImageChunk(_) => {}
            SessionFrame::Event(session_event) if session_event == *end_event => {
                return released_image_content_ids;
            }
            SessionFrame::Event(SessionEvent::ImageCacheReset) => {
                painted_image_content_ids.clear();
                preview_image_content_ids.clear();
                held_placement_request_id = None;
            }
            SessionFrame::Event(SessionEvent::PanePlacementSnapshot {
                request_id,
                snapshot,
            }) => {
                preview_image_content_ids = [
                    Some(&snapshot.source_tab_snapshot),
                    snapshot.destination_tab_snapshot.as_ref(),
                ]
                .into_iter()
                .flatten()
                .flat_map(|placement_tab_snapshot| &placement_tab_snapshot.pane_snapshots)
                .flat_map(|placement_pane_snapshot| {
                    &placement_pane_snapshot.image_placement_snapshots
                })
                .filter(|image_placement| image_placement.is_available)
                .map(|image_placement| image_placement.image_content_id)
                .collect();
                held_placement_request_id = Some(request_id);
            }
            SessionFrame::Event(SessionEvent::ImageContentReleased { image_content_ids }) => {
                for image_content_id in &image_content_ids {
                    assert!(
                        !painted_image_content_ids.contains(image_content_id),
                        "release of record {image_content_id}, which the newest painted update shows"
                    );
                    assert!(
                        !preview_image_content_ids.contains(image_content_id),
                        "release of record {image_content_id}, which the held preview shows"
                    );
                }
                released_image_content_ids.extend(image_content_ids);
            }
            SessionFrame::Event(_) => {}
        }
    }
}

/// The `ImageContentStart` frame and the one image chunk frame that carry
/// `image_record` under `image_content_id`.
fn build_image_transfer_frames(
    image_content_id: u64,
    image_record: &ImageRecord,
) -> [SessionFrame<SessionEvent>; 2] {
    [
        SessionFrame::Event(SessionEvent::ImageContentStart {
            image_transfer: build_wire_image_transfer(image_content_id, image_record),
        }),
        SessionFrame::ImageChunk(build_image_chunk_frame(
            image_content_id,
            0,
            true,
            &image_record.image.rgba_bytes,
        )),
    ]
}

#[test]
fn an_event_between_two_painted_updates_keeps_its_place_and_the_earlier_update_is_not_replaced() {
    let mut frame_stream = AttachedFrameStream::attach_frame_stream(
        "event-between-updates",
        GraphicsCapabilities::default(),
    );
    let first_render_snapshot = build_alternating_grid_snapshot(frame_stream.client_id, 2, 4);
    let second_render_snapshot = build_alternating_grid_snapshot(frame_stream.client_id, 3, 4);
    frame_stream
        .delivery_sender
        .try_send_frame(Box::new(first_render_snapshot.clone()))
        .expect("the first frame fits");
    let rejected_placement_command_event = frame_stream.send_rejected_placement_command();
    frame_stream
        .delivery_sender
        .try_send_frame(Box::new(second_render_snapshot.clone()))
        .expect("the second frame fits");

    assert_eq!(
        frame_stream.read_painted_frame(),
        build_wire_frame(&first_render_snapshot)
    );
    assert_eq!(
        frame_stream.read_session_event(),
        rejected_placement_command_event
    );
    assert_eq!(
        frame_stream.read_painted_frame(),
        build_wire_frame(&second_render_snapshot)
    );

    frame_stream.close_frame_stream();
}

#[test]
fn a_report_of_an_update_that_was_not_sent_closes_the_connection() {
    let AttachedFrameStream {
        ipc_server,
        dispatcher_thread,
        runtime_directory,
        connection,
        delivery_sender,
        ..
    } = AttachedFrameStream::attach_frame_stream(
        "unsent-paint-report",
        GraphicsCapabilities::default(),
    );
    let (mut frame_reader, mut frame_writer) = connection.split();

    frame_writer
        .send(&build_paint_report_request(1))
        .expect("the paint report is written");

    frame_reader.set_deadline(Some(Instant::now() + Duration::from_secs(5)));
    assert!(
        matches!(
            frame_reader.recv_message_or_binary::<SessionEvent>(),
            Err(IpcError::Disconnected)
        ),
        "the session closes the connection"
    );

    drop(frame_reader);
    drop(frame_writer);
    drop(delivery_sender);
    ipc_server.shutdown();
    dispatcher_thread.join().expect("dispatcher exits");
    delete_test_directory(&runtime_directory);
}

#[test]
fn a_client_that_reports_no_paint_receives_one_update_and_the_rest_waits_for_its_report() {
    let AttachedFrameStream {
        ipc_server,
        dispatcher_thread,
        runtime_directory,
        client_id,
        connection,
        mut painted_update_receiver,
        delivery_sender,
    } = AttachedFrameStream::attach_frame_stream(
        "unreported-paint",
        GraphicsCapabilities::default(),
    );
    let (mut frame_reader, mut frame_writer) = connection.split();
    let first_render_snapshot = build_alternating_grid_snapshot(client_id, 2, 4);
    let second_render_snapshot = build_alternating_grid_snapshot(client_id, 3, 4);
    let rejected_command_id = CommandId::new();
    for delivery in [
        Delivery::Frame(Box::new(first_render_snapshot.clone())),
        Delivery::Frame(Box::new(second_render_snapshot.clone())),
        Delivery::PlacementCommandRejected(rejected_command_id),
    ] {
        delivery_sender
            .try_send(delivery)
            .expect("the writing thread holds the delivery queue");
    }
    let mut read_session_frame_unreported = |frame_reader: &mut transport::FrameReader| loop {
        let received_frame = frame_reader
            .recv_message_or_binary::<SessionEvent>()
            .expect("the client is told");
        if let Some(session_frame) = painted_update_receiver
            .accept_session_frame(received_frame)
            .expect("a painted update part that applies")
        {
            break session_frame;
        }
    };

    let SessionFrame::Painted(first_received_frame) =
        read_session_frame_unreported(&mut frame_reader)
    else {
        panic!("the first update arrives");
    };
    assert_eq!(first_received_frame.update_sequence, 1);
    assert_eq!(
        *first_received_frame.painted_frame,
        build_wire_frame(&first_render_snapshot)
    );
    // The second update, and the event behind it, wait for the report.
    frame_reader.set_deadline(Some(Instant::now() + Duration::from_millis(300)));
    assert!(
        matches!(
            frame_reader.recv_message_or_binary::<SessionEvent>(),
            Err(IpcError::Transport { error_detail }) if error_detail == "this step ran out of time"
        ),
        "nothing arrives before the report"
    );
    frame_reader.set_deadline(None);

    frame_writer
        .send(&build_paint_report_request(1))
        .expect("the paint report is written");
    let SessionFrame::Painted(second_received_frame) =
        read_session_frame_unreported(&mut frame_reader)
    else {
        panic!("the second update arrives after the report");
    };
    assert_eq!(second_received_frame.update_sequence, 2);
    assert_eq!(
        *second_received_frame.painted_frame,
        build_wire_frame(&second_render_snapshot)
    );
    assert_eq!(
        read_session_frame_unreported(&mut frame_reader),
        SessionFrame::Event(SessionEvent::PlacementCommandRejected {
            command_id: rejected_command_id,
        })
    );

    drop(frame_reader);
    drop(frame_writer);
    drop(delivery_sender);
    ipc_server.shutdown();
    dispatcher_thread.join().expect("dispatcher exits");
    delete_test_directory(&runtime_directory);
}

#[test]
fn an_unsupported_terminal_receives_placement_geometry_and_no_pixel_events() {
    let mut frame_stream = AttachedFrameStream::attach_frame_stream(
        "unsupported-image-stream",
        GraphicsCapabilities::default(),
    );
    let render_snapshot = build_image_snapshot(frame_stream.client_id, build_image_record(1));
    frame_stream.send_delivery(Delivery::Frame(Box::new(render_snapshot.clone())));
    let rejected_placement_command_event = frame_stream.send_rejected_placement_command();

    let mut expected_painted_frame = build_wire_frame(&render_snapshot);
    let expected_image_placement =
        &mut expected_painted_frame.pane_snapshots[0].image_placement_snapshots[0];
    expected_image_placement.image_record = None;
    expected_image_placement.is_available = false;
    assert_eq!(frame_stream.read_painted_frame(), expected_painted_frame);
    assert_eq!(
        frame_stream.read_session_event(),
        rejected_placement_command_event
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
    let rejected_placement_command_event = frame_stream.send_rejected_placement_command();

    let [image_start_frame, image_chunk_frame] = build_image_transfer_frames(1, &image_record);
    assert_eq!(
        frame_stream.read_painted_frame(),
        build_wire_frame(&render_snapshot)
    );
    assert_eq!(frame_stream.read_next_session_frame(), image_start_frame);
    assert_eq!(frame_stream.read_next_session_frame(), image_chunk_frame);
    // The second frame changes nothing the client holds, and sends nothing.
    assert_eq!(
        frame_stream.read_session_event(),
        rejected_placement_command_event
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
    let rejected_placement_command_event = frame_stream.send_rejected_placement_command();

    // The release of a record follows the frame that stops showing it.
    for (released_image_content_id, render_snapshot, image_content_id, image_record) in [
        (None, &visible_image_snapshot, 1, Some(&first_image_record)),
        (Some(1), &absent_image_snapshot, 0, None),
        (None, &visible_image_snapshot, 2, Some(&first_image_record)),
        (Some(2), &absent_image_snapshot, 0, None),
        (
            None,
            &changed_image_snapshot,
            3,
            Some(&changed_image_record),
        ),
    ] {
        let mut expected_painted_frame = build_wire_frame(render_snapshot);
        if let Some(expected_image_placement) = expected_painted_frame
            .pane_snapshots
            .first_mut()
            .and_then(|pane_snapshot| pane_snapshot.image_placement_snapshots.first_mut())
        {
            expected_image_placement.image_content_id = image_content_id;
        }
        assert_eq!(frame_stream.read_painted_frame(), expected_painted_frame);
        if let Some(released_image_content_id) = released_image_content_id {
            assert_eq!(
                frame_stream.read_session_event(),
                SessionEvent::ImageContentReleased {
                    image_content_ids: vec![released_image_content_id]
                }
            );
        }
        let Some(image_record) = image_record else {
            continue;
        };
        let [image_start_frame, image_chunk_frame] =
            build_image_transfer_frames(image_content_id, image_record);
        assert_eq!(frame_stream.read_next_session_frame(), image_start_frame);
        assert_eq!(frame_stream.read_next_session_frame(), image_chunk_frame);
    }
    assert_eq!(
        frame_stream.read_session_event(),
        rejected_placement_command_event
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

        let [image_start_frame, image_chunk_frame] = build_image_transfer_frames(1, &image_record);
        assert_eq!(
            frame_stream.read_painted_frame(),
            build_wire_frame(&render_snapshot)
        );
        assert_eq!(frame_stream.read_next_session_frame(), image_start_frame);
        assert_eq!(frame_stream.read_next_session_frame(), image_chunk_frame);

        frame_stream.close_frame_stream();
    }
}

#[test]
fn a_full_hd_image_reaches_the_client_as_raw_chunks_of_its_own_bytes() {
    let mut frame_stream = AttachedFrameStream::attach_frame_stream(
        "full-hd-image-chunks",
        KITTY_GRAPHICS_CAPABILITIES,
    );
    let rgba_bytes = build_counting_bytes(1920 * 1080 * 4);
    let image_record = Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: (DecodedImage {
            pixel_width: 1920,
            pixel_height: 1080,
            rgba_bytes: rgba_bytes.clone(),
        })
        .into(),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay::default(),
        anchor: (0, 0),
    });
    let render_snapshot = build_image_snapshot(frame_stream.client_id, Arc::clone(&image_record));
    frame_stream.send_delivery(Delivery::Frame(Box::new(render_snapshot.clone())));

    assert_eq!(
        frame_stream.read_painted_frame(),
        build_wire_frame(&render_snapshot)
    );
    assert_eq!(
        frame_stream.read_next_session_frame(),
        SessionFrame::Event(SessionEvent::ImageContentStart {
            image_transfer: build_wire_image_transfer(1, &image_record),
        })
    );
    let mut received_rgba_bytes = Vec::new();
    let mut chunk_frame_count = 0;
    let mut chunk_frame_byte_count = 0;
    loop {
        let SessionFrame::ImageChunk(image_chunk_frame) = frame_stream.read_next_session_frame()
        else {
            panic!("the image's chunks follow its start");
        };
        assert_eq!(image_chunk_frame.get_image_transfer_id(), 1);
        assert_eq!(
            image_chunk_frame.get_byte_offset(),
            received_rgba_bytes.len() as u64
        );
        let chunk_bytes = image_chunk_frame.get_chunk_bytes();
        chunk_frame_count += 1;
        chunk_frame_byte_count += 4 + IMAGE_CHUNK_FRAME_HEAD_BYTE_COUNT + chunk_bytes.len();
        received_rgba_bytes.extend_from_slice(chunk_bytes);
        if image_chunk_frame.is_last_chunk() {
            break;
        }
    }

    assert_eq!(chunk_frame_count, 8);
    assert_eq!(chunk_frame_byte_count, 8_294_400 + 8 * 22);
    assert_eq!(received_rgba_bytes, rgba_bytes);
    frame_stream.close_frame_stream();
}

#[test]
fn each_frame_of_a_looping_animation_reaches_each_of_two_clients_once() {
    let mut frame_streams = [
        AttachedFrameStream::attach_frame_stream(
            "animation-first-client",
            KITTY_GRAPHICS_CAPABILITIES,
        ),
        AttachedFrameStream::attach_frame_stream(
            "animation-second-client",
            KITTY_GRAPHICS_CAPABILITIES,
        ),
    ];
    let animation_frame_records = build_animation_frame_records(30, 1, 1);
    let animation_snapshots_by_client: Vec<Vec<RenderSnapshot>> = frame_streams
        .iter()
        .map(|frame_stream| {
            let first_snapshot = build_image_snapshot(
                frame_stream.client_id,
                Arc::clone(&animation_frame_records[0]),
            );
            animation_frame_records
                .iter()
                .map(|image_record| {
                    build_snapshot_showing_record(&first_snapshot, Arc::clone(image_record))
                })
                .collect()
        })
        .collect();

    // Three loops of 30 frames: frame `n` always travels under content id
    // `n + 1`, and its pixels follow it in the first loop only.
    for loop_index in 0..3 {
        for (frame_index, image_record) in animation_frame_records.iter().enumerate() {
            let image_content_id = frame_index as u64 + 1;
            for (frame_stream, animation_snapshots) in
                frame_streams.iter_mut().zip(&animation_snapshots_by_client)
            {
                let animation_snapshot = &animation_snapshots[frame_index];
                frame_stream.send_delivery(Delivery::Frame(Box::new(animation_snapshot.clone())));
                let mut expected_painted_frame = build_wire_frame(animation_snapshot);
                expected_painted_frame.pane_snapshots[0].image_placement_snapshots[0]
                    .image_content_id = image_content_id;

                assert_eq!(frame_stream.read_painted_frame(), expected_painted_frame);
                if loop_index == 0 {
                    for expected_session_frame in
                        build_image_transfer_frames(image_content_id, image_record)
                    {
                        assert_eq!(
                            frame_stream.read_next_session_frame(),
                            expected_session_frame
                        );
                    }
                }
            }
        }
    }
    for frame_stream in &mut frame_streams {
        let rejected_placement_command_event = frame_stream.send_rejected_placement_command();
        assert_eq!(
            frame_stream.read_session_event(),
            rejected_placement_command_event,
            "nothing else follows the last frame"
        );
    }
    for frame_stream in frame_streams {
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
    let rejected_placement_command_event = frame_stream.send_rejected_placement_command();
    let painted_frame = build_wire_frame(&render_snapshot);

    assert_eq!(frame_stream.read_painted_frame(), painted_frame);
    for image_transfer_index in 1..=MAX_FRAME_IMAGE_TRANSFER_COUNT {
        let image_content_id =
            u64::try_from(image_transfer_index).expect("the content identity fits");
        let [image_start_frame, image_chunk_frame] = build_image_transfer_frames(
            image_content_id,
            &build_red_image_record(image_content_id),
        );
        assert_eq!(frame_stream.read_next_session_frame(), image_start_frame);
        assert_eq!(frame_stream.read_next_session_frame(), image_chunk_frame);
    }
    assert_eq!(frame_stream.read_painted_frame(), painted_frame);
    let [last_image_start_frame, last_image_chunk_frame] = build_image_transfer_frames(
        last_image_content_id,
        &build_red_image_record(last_image_content_id),
    );
    assert_eq!(
        frame_stream.read_next_session_frame(),
        last_image_start_frame
    );
    assert_eq!(
        frame_stream.read_next_session_frame(),
        last_image_chunk_frame
    );
    assert_eq!(
        frame_stream.read_session_event(),
        rejected_placement_command_event
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
    assert_eq!(
        list_upload_content_ids(&initial_prepared_frame.image_uploads),
        vec![1]
    );
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
    assert_eq!(
        list_upload_content_ids(&replacement_prepared_frame.image_uploads),
        vec![2]
    );
    assert!(Arc::ptr_eq(
        &replacement_prepared_frame.image_uploads[0].1,
        &second_image_record
    ));
    assert_eq!(
        initial_prepared_frame.released_image_content_ids,
        Vec::<u64>::new()
    );
    assert_eq!(
        replacement_prepared_frame.released_image_content_ids,
        vec![1]
    );
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
    assert_eq!(
        list_upload_content_ids(&prepared_frame_after_clear.image_uploads),
        vec![1]
    );
}

#[test]
fn a_looping_animation_sends_each_frame_once_to_each_connection() {
    // A 30-frame 480×270 animation through 3 loops on 2 connections: frame
    // `n` travels under content id `n + 1`, its pixels go out in the first
    // loop only, and nothing is released.
    let animation_frame_records = build_animation_frame_records(30, 480, 270);
    let base_snapshot =
        build_image_snapshot(ClientId::new(), Arc::clone(&animation_frame_records[0]));
    let mut image_caches = [ConnectionImageCache::new(), ConnectionImageCache::new()];

    for loop_index in 0..3 {
        for (frame_index, image_record) in animation_frame_records.iter().enumerate() {
            let image_content_id = frame_index as u64 + 1;
            let animation_snapshot =
                build_snapshot_showing_record(&base_snapshot, Arc::clone(image_record));
            for image_cache in &mut image_caches {
                let prepared_image_frame = image_cache.prepare_image_frame(&animation_snapshot);

                let expected_upload_content_ids = if loop_index == 0 {
                    vec![image_content_id]
                } else {
                    Vec::new()
                };
                assert_eq!(
                    list_upload_content_ids(&prepared_image_frame.image_uploads),
                    expected_upload_content_ids
                );
                assert_eq!(
                    prepared_image_frame.released_image_content_ids,
                    Vec::<u64>::new()
                );
                assert_eq!(
                    list_painted_image_availability(&prepared_image_frame.painted_frame),
                    vec![(image_content_id, true)]
                );
            }
        }
    }
}

#[test]
fn an_animation_that_does_not_fit_beside_the_other_images_travels_frame_by_frame() {
    // A 40 MiB image and one 10 MiB frame of a 3-frame animation show: 50
    // MiB. All 3 frames need 70 MiB, over the 64 MiB the client holds, so
    // each frame is sent under a new id and the frame before it is released.
    let static_image_record = build_sized_image_record(1, 4096, 2560);
    let animation_frame_records = build_animation_frame_records(3, 2048, 1280);
    let base_snapshot = build_image_snapshot(ClientId::new(), Arc::clone(&static_image_record));
    let mut image_cache = ConnectionImageCache::new();
    let expected_steps: [(usize, Vec<u64>, Vec<u64>, u64); 4] = [
        (0, vec![1, 2], Vec::new(), 2),
        (1, vec![3], vec![2], 3),
        (2, vec![4], vec![3], 4),
        (0, vec![5], vec![4], 5),
    ];

    for (
        frame_index,
        expected_upload_content_ids,
        expected_released_content_ids,
        frame_content_id,
    ) in expected_steps
    {
        let render_snapshot = build_snapshot_showing_records(
            &base_snapshot,
            &[
                Arc::clone(&static_image_record),
                Arc::clone(&animation_frame_records[frame_index]),
            ],
        );

        let prepared_image_frame = image_cache.prepare_image_frame(&render_snapshot);

        assert_eq!(
            list_upload_content_ids(&prepared_image_frame.image_uploads),
            expected_upload_content_ids
        );
        assert_eq!(
            prepared_image_frame.released_image_content_ids,
            expected_released_content_ids
        );
        assert_eq!(
            list_painted_image_availability(&prepared_image_frame.painted_frame),
            vec![(1, true), (frame_content_id, true)]
        );
    }
}

#[test]
fn images_past_the_byte_budget_travel_without_their_records() {
    // Three 30 MiB images in one frame: the first two fit the 64 MiB the
    // client holds and are sent. The third travels with its own content id
    // 3, no record and `is_available` `false`, and is not sent.
    let image_records = [
        build_sized_image_record(1, 4096, 1920),
        build_sized_image_record(2, 4096, 1920),
        build_sized_image_record(3, 4096, 1920),
    ];
    let base_snapshot = build_image_snapshot(ClientId::new(), Arc::clone(&image_records[0]));
    let render_snapshot = build_snapshot_showing_records(&base_snapshot, &image_records);
    let mut image_cache = ConnectionImageCache::new();

    let prepared_image_frame = image_cache.prepare_image_frame(&render_snapshot);

    assert_eq!(
        list_upload_content_ids(&prepared_image_frame.image_uploads),
        vec![1, 2]
    );
    assert_eq!(
        list_painted_image_availability(&prepared_image_frame.painted_frame),
        vec![(1, true), (2, true), (3, false)]
    );
    assert_eq!(
        prepared_image_frame.painted_frame.pane_snapshots[0].image_placement_snapshots[2]
            .image_record,
        None
    );
}

#[test]
fn images_that_fill_the_byte_budget_exactly_travel_with_their_records() {
    // Two 32 MiB images in one frame fill the 64 MiB the client holds
    // exactly: both are sent.
    let image_records = [
        build_sized_image_record(1, 4096, 2048),
        build_sized_image_record(2, 4096, 2048),
    ];
    let base_snapshot = build_image_snapshot(ClientId::new(), Arc::clone(&image_records[0]));
    let render_snapshot = build_snapshot_showing_records(&base_snapshot, &image_records);
    let mut image_cache = ConnectionImageCache::new();

    let prepared_image_frame = image_cache.prepare_image_frame(&render_snapshot);

    assert_eq!(
        list_upload_content_ids(&prepared_image_frame.image_uploads),
        vec![1, 2]
    );
    assert_eq!(
        list_painted_image_availability(&prepared_image_frame.painted_frame),
        vec![(1, true), (2, true)]
    );
}

#[test]
fn an_animation_that_fills_the_byte_budget_exactly_stays_whole() {
    // Four 16 MiB frames fill the 64 MiB the client holds exactly. Each
    // frame is sent once, nothing is released, and frame 0 returns under
    // id 1 with no pixels.
    let animation_frame_records = build_animation_frame_records(4, 2048, 2048);
    let base_snapshot =
        build_image_snapshot(ClientId::new(), Arc::clone(&animation_frame_records[0]));
    let mut image_cache = ConnectionImageCache::new();
    let expected_steps: [(usize, Vec<u64>, u64); 5] = [
        (0, vec![1], 1),
        (1, vec![2], 2),
        (2, vec![3], 3),
        (3, vec![4], 4),
        (0, Vec::new(), 1),
    ];

    for (frame_index, expected_upload_content_ids, frame_content_id) in expected_steps {
        let render_snapshot = build_snapshot_showing_records(
            &base_snapshot,
            &[Arc::clone(&animation_frame_records[frame_index])],
        );

        let prepared_image_frame = image_cache.prepare_image_frame(&render_snapshot);

        assert_eq!(
            list_upload_content_ids(&prepared_image_frame.image_uploads),
            expected_upload_content_ids
        );
        assert_eq!(
            prepared_image_frame.released_image_content_ids,
            Vec::<u64>::new()
        );
        assert_eq!(
            list_painted_image_availability(&prepared_image_frame.painted_frame),
            vec![(frame_content_id, true)]
        );
    }
}

#[test]
fn a_frame_image_that_does_not_fit_beside_a_held_preview_travels_without_its_record() {
    // Frame 1 shows the small `first_frame_image` under id 1. The preview
    // that answers request 5 shows the 40 MiB `preview_image` under id 2. The
    // next frame shows the 40 MiB `next_frame_image`, which does not fit
    // beside `preview_image`: `next_frame_image` travels with its own content
    // id 1, no record and `is_available` `false`, and `first_frame_image` is
    // released.
    let first_frame_image = build_image_record(1);
    let preview_image = build_sized_image_record(2, 4096, 2560);
    let next_frame_image = build_sized_image_record(3, 4096, 2560);
    let frame_snapshot = build_image_snapshot(ClientId::new(), Arc::clone(&first_frame_image));
    let placement_snapshot =
        build_placement_snapshot_showing_records(&frame_snapshot, &[Arc::clone(&preview_image)]);
    let mut image_cache = ConnectionImageCache::new();
    image_cache.prepare_image_frame(&frame_snapshot);
    let prepared_placement_snapshot =
        image_cache.prepare_placement_snapshot(5, 1, &placement_snapshot);

    let prepared_image_frame = image_cache.prepare_image_frame(&build_snapshot_showing_records(
        &frame_snapshot,
        &[next_frame_image],
    ));

    assert_eq!(
        list_upload_content_ids(&prepared_placement_snapshot.image_uploads),
        vec![2]
    );
    assert_eq!(
        list_upload_content_ids(&prepared_image_frame.image_uploads),
        Vec::<u64>::new()
    );
    assert_eq!(prepared_image_frame.released_image_content_ids, vec![1]);
    assert_eq!(
        list_painted_image_availability(&prepared_image_frame.painted_frame),
        vec![(1, false)]
    );
}

#[test]
fn a_preview_image_that_does_not_fit_beside_the_frame_travels_without_its_record() {
    // The frame shows the 40 MiB `frame_image` under id 1. The preview shows
    // the 40 MiB `preview_image`, which does not fit beside `frame_image`:
    // `preview_image` travels with its own content id 1, no record and
    // `is_available` `false`, and nothing is released.
    let frame_image = build_sized_image_record(1, 4096, 2560);
    let preview_image = build_sized_image_record(2, 4096, 2560);
    let frame_snapshot = build_image_snapshot(ClientId::new(), Arc::clone(&frame_image));
    let placement_snapshot =
        build_placement_snapshot_showing_records(&frame_snapshot, &[Arc::clone(&preview_image)]);
    let mut image_cache = ConnectionImageCache::new();
    image_cache.prepare_image_frame(&frame_snapshot);

    let prepared_placement_snapshot =
        image_cache.prepare_placement_snapshot(5, 1, &placement_snapshot);

    assert_eq!(
        list_upload_content_ids(&prepared_placement_snapshot.image_uploads),
        Vec::<u64>::new()
    );
    assert_eq!(
        prepared_placement_snapshot.released_image_content_ids,
        Vec::<u64>::new()
    );
    assert_eq!(
        prepared_placement_snapshot
            .placement_snapshot
            .source_tab_snapshot
            .pane_snapshots[0]
            .image_placement_snapshots
            .iter()
            .map(|image_placement| (
                image_placement.image_content_id,
                image_placement.is_available
            ))
            .collect::<Vec<_>>(),
        vec![(1, false)]
    );
}

#[test]
fn a_placement_preview_keeps_its_records_until_a_paint_report_leaves_it_out() {
    // Frame 1 shows `frame_image`. The preview that answers request 5 goes
    // out after update 1 and shows `frame_image` and `preview_only_image`.
    // `preview_only_image` stays while a report could come from a client
    // that holds the preview: a report of update 1, and a report of update 2
    // that names request 5. A report of update 2 that names no preview
    // releases `preview_only_image`.
    let frame_image = build_image_record(1);
    let preview_only_image = build_image_record(4);
    let frame_snapshot = build_image_snapshot(ClientId::new(), Arc::clone(&frame_image));
    let placement_snapshot = build_placement_snapshot_showing_records(
        &frame_snapshot,
        &[Arc::clone(&frame_image), Arc::clone(&preview_only_image)],
    );
    let mut image_cache = ConnectionImageCache::new();

    let first_prepared_frame = image_cache.prepare_image_frame(&frame_snapshot);
    let prepared_placement_snapshot =
        image_cache.prepare_placement_snapshot(5, 1, &placement_snapshot);
    let second_prepared_frame = image_cache.prepare_image_frame(&frame_snapshot);

    assert_eq!(
        list_upload_content_ids(&first_prepared_frame.image_uploads),
        vec![1]
    );
    assert_eq!(
        list_upload_content_ids(&prepared_placement_snapshot.image_uploads),
        vec![2]
    );
    assert_eq!(
        prepared_placement_snapshot.released_image_content_ids,
        Vec::<u64>::new()
    );
    assert_eq!(
        second_prepared_frame.released_image_content_ids,
        Vec::<u64>::new()
    );
    assert_eq!(image_cache.record_paint_report(1, None), Vec::<u64>::new());
    assert_eq!(
        image_cache.record_paint_report(2, Some(5)),
        Vec::<u64>::new()
    );
    assert_eq!(image_cache.record_paint_report(2, None), vec![2]);
    assert_eq!(
        list_upload_content_ids(
            &image_cache
                .prepare_image_frame(&build_image_snapshot(ClientId::new(), preview_only_image))
                .image_uploads
        ),
        vec![3]
    );
}

#[test]
fn a_frame_that_shows_an_image_a_preview_sent_reuses_its_record() {
    // The preview sent `preview_image` under id 2. The next frame shows
    // `preview_image` in place of `first_frame_image`: it names id 2, sends no
    // pixels, and releases `first_frame_image`.
    let first_frame_image = build_image_record(1);
    let preview_image = build_image_record(2);
    let frame_snapshot = build_image_snapshot(ClientId::new(), Arc::clone(&first_frame_image));
    let placement_snapshot =
        build_placement_snapshot_showing_records(&frame_snapshot, &[Arc::clone(&preview_image)]);
    let mut image_cache = ConnectionImageCache::new();
    image_cache.prepare_image_frame(&frame_snapshot);
    image_cache.prepare_placement_snapshot(5, 1, &placement_snapshot);

    let prepared_image_frame = image_cache.prepare_image_frame(&build_snapshot_showing_record(
        &frame_snapshot,
        preview_image,
    ));

    assert_eq!(
        list_upload_content_ids(&prepared_image_frame.image_uploads),
        Vec::<u64>::new()
    );
    assert_eq!(prepared_image_frame.released_image_content_ids, vec![1]);
    assert_eq!(
        list_painted_image_availability(&prepared_image_frame.painted_frame),
        vec![(2, true)]
    );
}

#[test]
fn an_image_shown_again_after_its_release_gets_a_new_identity() {
    let shown_image = build_image_record(1);
    let frame_snapshot = build_image_snapshot(ClientId::new(), Arc::clone(&shown_image));
    let empty_snapshot = build_snapshot_showing_records(&frame_snapshot, &[]);
    let mut image_cache = ConnectionImageCache::new();
    image_cache.prepare_image_frame(&frame_snapshot);

    let empty_prepared_frame = image_cache.prepare_image_frame(&empty_snapshot);
    let returning_prepared_frame = image_cache.prepare_image_frame(&frame_snapshot);

    assert_eq!(empty_prepared_frame.released_image_content_ids, vec![1]);
    assert_eq!(
        list_upload_content_ids(&returning_prepared_frame.image_uploads),
        vec![2]
    );
}

#[test]
fn an_image_the_next_frame_stops_showing_is_released_after_that_frame_and_before_its_records() {
    let mut frame_stream = AttachedFrameStream::attach_frame_stream(
        "image-release-order",
        KITTY_GRAPHICS_CAPABILITIES,
    );
    let first_image_record = build_image_record(1);
    let second_image_record = build_image_record(2);
    let first_snapshot =
        build_image_snapshot(frame_stream.client_id, Arc::clone(&first_image_record));
    let second_snapshot =
        build_snapshot_showing_record(&first_snapshot, Arc::clone(&second_image_record));
    frame_stream.send_delivery(Delivery::Frame(Box::new(first_snapshot.clone())));
    assert_eq!(
        frame_stream.read_painted_frame(),
        build_wire_frame(&first_snapshot)
    );
    for expected_session_frame in build_image_transfer_frames(1, &first_image_record) {
        assert_eq!(
            frame_stream.read_next_session_frame(),
            expected_session_frame
        );
    }

    frame_stream.send_delivery(Delivery::Frame(Box::new(second_snapshot.clone())));

    let mut expected_painted_frame = build_wire_frame(&second_snapshot);
    expected_painted_frame.pane_snapshots[0].image_placement_snapshots[0].image_content_id = 2;
    assert_eq!(frame_stream.read_painted_frame(), expected_painted_frame);
    assert_eq!(
        frame_stream.read_session_event(),
        SessionEvent::ImageContentReleased {
            image_content_ids: vec![1]
        }
    );
    for expected_session_frame in build_image_transfer_frames(2, &second_image_record) {
        assert_eq!(
            frame_stream.read_next_session_frame(),
            expected_session_frame
        );
    }
    frame_stream.close_frame_stream();
}

#[test]
fn an_update_over_the_frame_cap_sends_no_release_and_the_next_update_resets_the_records() {
    // Update 2 stops showing record 1, and its session name alone is as long
    // as the frame cap: the update is not sent, and no release of record 1
    // follows it. The next update follows an image cache reset.
    let mut frame_stream = AttachedFrameStream::attach_frame_stream(
        "image-release-over-cap",
        KITTY_GRAPHICS_CAPABILITIES,
    );
    let image_record = build_image_record(1);
    let image_snapshot = build_image_snapshot(frame_stream.client_id, Arc::clone(&image_record));
    let empty_snapshot = build_snapshot_showing_records(&image_snapshot, &[]);
    let mut oversized_snapshot = empty_snapshot.clone();
    oversized_snapshot.session_snapshot.session_name =
        "a".repeat(transport::MAX_FRAME_BYTE_COUNT as usize);
    frame_stream.send_delivery(Delivery::Frame(Box::new(image_snapshot.clone())));
    assert_eq!(
        frame_stream.read_painted_frame(),
        build_wire_frame(&image_snapshot)
    );
    for expected_session_frame in build_image_transfer_frames(1, &image_record) {
        assert_eq!(
            frame_stream.read_next_session_frame(),
            expected_session_frame
        );
    }

    frame_stream.send_delivery(Delivery::Frame(Box::new(oversized_snapshot)));
    frame_stream.send_delivery(Delivery::Frame(Box::new(empty_snapshot.clone())));
    let rejected_placement_command_event = frame_stream.send_rejected_placement_command();

    assert_eq!(
        frame_stream.read_session_event(),
        SessionEvent::ImageCacheReset
    );
    assert_eq!(
        frame_stream.read_painted_frame(),
        build_wire_frame(&empty_snapshot)
    );
    assert_eq!(
        frame_stream.read_session_event(),
        rejected_placement_command_event
    );
    frame_stream.close_frame_stream();
}

#[test]
fn every_image_release_names_no_record_the_newest_update_or_the_held_preview_shows() {
    // Images 1 to 6 take content ids 1 to 6 in the order they first show.
    // The third update shows no image, and the release of id 3, which only
    // the second update showed, follows it.
    let mut frame_stream =
        AttachedFrameStream::attach_frame_stream("image-release-rule", KITTY_GRAPHICS_CAPABILITIES);
    let image_records: Vec<Arc<ImageRecord>> = (1..=6).map(build_image_record).collect();
    let base_snapshot = build_image_snapshot(frame_stream.client_id, Arc::clone(&image_records[0]));
    let list_image_records = |image_indexes: &[usize]| -> Vec<Arc<ImageRecord>> {
        image_indexes
            .iter()
            .map(|image_index| Arc::clone(&image_records[*image_index]))
            .collect()
    };
    let build_frame_delivery = |image_indexes: &[usize]| {
        Delivery::Frame(Box::new(build_snapshot_showing_records(
            &base_snapshot,
            &list_image_records(image_indexes),
        )))
    };
    let build_preview_delivery =
        |request_id: u64, image_indexes: &[usize]| Delivery::PanePlacementSnapshot {
            request_id,
            snapshot: Box::new(build_placement_snapshot_showing_records(
                &base_snapshot,
                &list_image_records(image_indexes),
            )),
        };
    for delivery in [
        build_frame_delivery(&[0]),
        build_preview_delivery(5, &[0, 1]),
        build_frame_delivery(&[2]),
        build_preview_delivery(6, &[3]),
        build_frame_delivery(&[]),
        build_frame_delivery(&[3, 4]),
        build_preview_delivery(7, &[5]),
        build_frame_delivery(&[4]),
        build_frame_delivery(&[]),
    ] {
        frame_stream.send_delivery(delivery);
    }
    let rejected_placement_command_event = frame_stream.send_rejected_placement_command();

    let released_image_content_ids =
        read_frames_checking_image_releases(&mut frame_stream, &rejected_placement_command_event);

    assert!(
        released_image_content_ids.contains(&3),
        "released {released_image_content_ids:?}"
    );
    frame_stream.close_frame_stream();
}

#[test]
fn a_paint_report_after_a_preview_releases_the_records_only_the_preview_showed() {
    // The preview that answers request 5 shows `frame_image` and
    // `preview_only_image`, and `preview_only_image` travels under id 2. The
    // client reads update 2 and reports it holding no preview: the session
    // releases `preview_only_image` at once.
    let mut frame_stream =
        AttachedFrameStream::attach_frame_stream("preview-release", KITTY_GRAPHICS_CAPABILITIES);
    let frame_image = build_image_record(1);
    let preview_only_image = build_image_record(4);
    let first_snapshot = build_image_snapshot(frame_stream.client_id, Arc::clone(&frame_image));
    let mut second_snapshot = first_snapshot.clone();
    second_snapshot.pane_snapshots[0]
        .cursor_snapshot
        .column_index = 1;
    let placement_snapshot = build_placement_snapshot_showing_records(
        &first_snapshot,
        &[Arc::clone(&frame_image), Arc::clone(&preview_only_image)],
    );
    frame_stream.send_delivery(Delivery::Frame(Box::new(first_snapshot.clone())));
    assert_eq!(
        frame_stream.read_painted_frame(),
        build_wire_frame(&first_snapshot)
    );
    for expected_session_frame in build_image_transfer_frames(1, &frame_image) {
        assert_eq!(
            frame_stream.read_next_session_frame(),
            expected_session_frame
        );
    }
    frame_stream.send_delivery(Delivery::PanePlacementSnapshot {
        request_id: 5,
        snapshot: Box::new(placement_snapshot.clone()),
    });
    let mut next_image_content_id = 0;
    assert_eq!(
        frame_stream.read_session_event(),
        SessionEvent::PanePlacementSnapshot {
            request_id: 5,
            snapshot: Box::new(
                crate::runtime::frame::build_wire_placement_snapshot_with_content_ids(
                    &placement_snapshot,
                    |_, _| {
                        next_image_content_id += 1;
                        Some(next_image_content_id)
                    },
                )
            ),
        }
    );
    for expected_session_frame in build_image_transfer_frames(2, &preview_only_image) {
        assert_eq!(
            frame_stream.read_next_session_frame(),
            expected_session_frame
        );
    }

    frame_stream.send_delivery(Delivery::Frame(Box::new(second_snapshot.clone())));

    assert_eq!(
        frame_stream.read_painted_frame(),
        build_wire_frame(&second_snapshot)
    );
    assert_eq!(
        frame_stream.read_session_event(),
        SessionEvent::ImageContentReleased {
            image_content_ids: vec![2]
        }
    );
    frame_stream.close_frame_stream();
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
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start serving");
    (ipc_server, session_id, runtime_directory, dispatcher_thread)
}

/// A served socket the other local users of this machine may reach, in a fresh
/// shared directory, with a stand-in dispatcher and the `allow-other-users`
/// setting reading `is_other_user_access_enabled`.
#[cfg(unix)]
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
        None,
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
/// returned receiver before answering it with `Ok`. It answers each
/// `IpcPrepare` probe ([`answer_prepare_probe`]). Every other event it drains
/// is dropped. Exits when every inbox sender is gone.
fn spawn_reporting_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
) -> (JoinHandle<()>, Receiver<CommandEnvelope>) {
    let (command_envelope_sender, command_envelope_receiver) = mpsc::channel();
    let dispatcher_thread = std::thread::spawn(move || {
        for runtime_event in inbox_receiver.iter().filter_map(answer_prepare_probe) {
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
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
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
    greet_connection(&mut connection, runtime_directory, session_id);
    connection
}

/// Send the Hello for `session_id` on `connection`, read its answer, and assert
/// that the session accepted it.
fn greet_connection(connection: &mut Connection, runtime_directory: &Path, session_id: SessionId) {
    connection
        .send(&build_hello_request(runtime_directory, session_id))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());
}

/// Submit `command_envelope` on `connection` with `Prepare` and `Commit`, and
/// hand back the envelope the dispatcher was given, after reading the reply
/// the submission earns.
fn submit_command_envelope(
    connection: &mut Connection,
    received_command_envelopes: &Receiver<CommandEnvelope>,
    command_envelope: CommandEnvelope,
) -> CommandEnvelope {
    send_committed_request(
        connection,
        IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope)),
        },
    );
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

/// Connect to the socket the endpoint file at `runtime_directory` advertises.
fn connect_to_session_socket(runtime_directory: &Path, session_id: SessionId) -> Connection {
    Connection::connect(&load_test_endpoint_file(runtime_directory, session_id).socket_address)
        .expect("connect")
}

/// A stand-in dispatcher answering every layout request with `session_layout`.
/// The returned receiver carries the tab each request named. It answers each
/// `IpcPrepare` probe ([`answer_prepare_probe`]). Exits when every inbox sender
/// is gone.
fn spawn_layout_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
    session_layout: Option<SessionLayout>,
) -> (JoinHandle<()>, Receiver<Option<TabId>>) {
    let (requested_tab_id_sender, requested_tab_id_receiver) = mpsc::channel();
    let dispatcher_thread = std::thread::spawn(move || {
        for runtime_event in inbox_receiver.iter().filter_map(answer_prepare_probe) {
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
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
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
    delete_test_directory(runtime_directory);
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
    send_prepare_request(
        &mut connection,
        IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope.clone())),
        },
    );

    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.request_id, Some(1));
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());

    read_prepared_and_send_commit(&mut connection, 2);
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
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);

    // A well-framed request naming a kind that this build does not have.
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
    send_committed_request(
        &mut connection,
        IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope.clone())),
        },
    );
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
        build_accepted_hello_result(),
        "the answer names the highest version both sides speak"
    );

    let command_envelope = build_test_command_envelope();
    send_committed_request(
        &mut connection,
        IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope.clone())),
        },
    );
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
    let previous_release_protocol_version = 4;
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
    greet_connection(&mut connection, &runtime_directory, session_id);

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
    let restarted_ipc_server =
        IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
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

    let accepted_connection = connect_greeted_connection(&runtime_directory, session_id);

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

    let connection = connect_greeted_connection(&runtime_directory, session_id);

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
    greet_connection(&mut connection, &runtime_directory, session_id);

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

    // The goodbye is the first frame after the attach reply: none of the six
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

    // A well-framed request naming a kind that this build does not have.
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
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);

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
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);

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
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);

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
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);

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
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start serving");
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);

    send_prepare_request(
        &mut connection,
        IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(build_test_command_envelope())),
        },
    );
    assert!(
        matches!(
            connection.recv::<IpcResponse>(),
            Err(IpcError::Disconnected),
        ),
        "no reply comes back once the dispatcher is gone",
    );

    drop(connection);
    ipc_server.shutdown();
    delete_test_directory(&runtime_directory);
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
    delete_test_directory(&runtime_directory);
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
    delete_test_directory(&runtime_directory);
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
    delete_test_directory(&runtime_directory);
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
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start reclaims the leftover and serves");

    ipc_server.shutdown();
    delete_test_directory(&runtime_directory);
}

#[test]
fn a_second_start_on_the_same_session_is_refused_while_serving() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("busy", None);

    let (inbox_sender, _inbox_receiver) = mpsc::channel();
    let Err(IpcError::SocketBusy { socket_address }) =
        IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
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
    // A file sits at the directory path: creating the directory under it fails,
    // and the start stops before it binds anything.
    let blocking_file_path = build_test_runtime_directory("runtime-dir-blocked");
    delete_test_directory(&blocking_file_path);
    std::fs::write(&blocking_file_path, b"").expect("plant a file where the directory would go");
    let runtime_directory = blocking_file_path.join("session");
    let (inbox_sender, _inbox_receiver) = mpsc::channel();

    let Err(IpcError::Transport { error_detail }) = IpcServer::start(
        &runtime_directory,
        SessionId::new(),
        inbox_sender,
        None,
        None,
    ) else {
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
    }) = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
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

    delete_test_directory(&runtime_directory);
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

#[cfg(unix)]
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
    delete_test_directory(&shared_directory);
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
    delete_test_directory(&shared_directory);
}

#[cfg(unix)]
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
        None,
    )
    .expect("start serving");

    let connection = attach_test_client(&runtime_directory, session_id, client_id);

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    delete_test_directory(&shared_directory);
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
    delete_socket_file(&socket_address);
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
            None,
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
    send_committed_request(caller_connection, build_attach_request(None, None));
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
    delete_socket_file(&socket_address);
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
    delete_socket_file(&socket_address);
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
    delete_socket_file(&socket_address);
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
    delete_socket_file(&socket_address);
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
    delete_socket_file(&socket_address);
}

#[cfg(unix)]
#[test]
fn the_directory_other_local_users_reach_holds_only_the_socket() {
    use std::os::unix::fs::MetadataExt;

    let (ipc_server, session_id, runtime_directory, shared_directory, dispatcher_thread) =
        start_shared_test_server("shared-only", true);
    let owner_user_id = std::fs::metadata(&runtime_directory)
        .expect("stat runtime directory")
        .uid();
    let user_directory = shared_directory.join(owner_user_id.to_string());

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
    assert_eq!(directory_entry_names, vec![format!("{session_id}.sock")]);

    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    delete_test_directory(&shared_directory);
}

/// A stand-in dispatcher that takes every restart request, answers every
/// discovery request with `session_overview`, and answers each `IpcPrepare`
/// probe ([`answer_prepare_probe`]). Exits when every inbox sender is gone.
fn spawn_restart_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
    session_overview: Option<SessionOverview>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        for runtime_event in inbox_receiver.iter().filter_map(answer_prepare_probe) {
            match runtime_event {
                RuntimeEvent::IpcRestart { response_sender } => {
                    let _ = response_sender.send(());
                }
                RuntimeEvent::IpcDiscovery { response_sender } => {
                    let _ = response_sender.send(session_overview.clone());
                }
                _ => {}
            }
        }
    })
}

/// The restart check that answers `restart_verdict` every time it runs.
fn build_restart_check_answering(restart_verdict: Result<(), RestartRefusal>) -> RestartCheck {
    Arc::new(move || restart_verdict.clone())
}

/// What a test session that can replace its image hands its socket: a watch of
/// `executable_path` for a session that runs `1.0.0`, and `restart_check`.
fn build_test_image_replacement(
    executable_path: PathBuf,
    restart_check: RestartCheck,
) -> ImageReplacement {
    ImageReplacement {
        executable_watch: Arc::new(ExecutableWatch::from_executable_path(
            executable_path,
            "1.0.0",
        )),
        restart_check,
    }
}

/// A served socket whose restart requests run `restart_check`, and whose
/// stand-in dispatcher takes every restart that passes it and answers
/// discovery requests with `session_overview`. The watch it serves with names
/// a program file that does not exist, so it starts no version read.
fn start_restartable_test_server(
    directory_tag: &str,
    restart_check: RestartCheck,
    session_overview: Option<SessionOverview>,
) -> (IpcServer, SessionId, PathBuf, JoinHandle<()>) {
    let runtime_directory = build_test_runtime_directory(directory_tag);
    let session_id = SessionId::new();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let dispatcher_thread = spawn_restart_dispatcher(inbox_receiver, session_overview);
    let image_replacement =
        build_test_image_replacement(runtime_directory.join("absent-koshi"), restart_check);
    let ipc_server = IpcServer::start(
        &runtime_directory,
        session_id,
        inbox_sender,
        None,
        Some(image_replacement),
    )
    .expect("start serving");
    (ipc_server, session_id, runtime_directory, dispatcher_thread)
}

/// Sends the Hello and a restart with `Prepare` and `Commit`, and returns the
/// connection and the answer to the restart.
fn send_restart_request(
    runtime_directory: &Path,
    session_id: SessionId,
) -> (Connection, IpcResult) {
    let mut connection = connect_greeted_connection(runtime_directory, session_id);

    send_committed_request(
        &mut connection,
        IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Restart,
        },
    );
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
fn a_restart_to_a_session_that_cannot_replace_its_image_is_refused_without_the_dispatcher() {
    let runtime_directory = build_test_runtime_directory("restart-unsupported");
    let session_id = SessionId::new();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (restart_report_sender, restart_report_receiver) = mpsc::channel();
    let dispatcher_thread =
        spawn_restart_reporting_dispatcher(inbox_receiver, restart_report_sender);
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start serving");

    let (connection, restart_result) = send_restart_request(&runtime_directory, session_id);

    assert_eq!(
        restart_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::RequestFailed,
            message: "this koshi cannot replace its own image, so it cannot restart".to_string(),
        }),
    );
    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    assert_eq!(
        restart_report_receiver.try_recv(),
        Err(mpsc::TryRecvError::Disconnected),
        "no restart reached the dispatcher"
    );
}

#[test]
fn a_restart_check_that_still_runs_leaves_the_dispatcher_answering_other_requests() {
    let (check_started_sender, check_started_receiver) = mpsc::channel::<()>();
    let (check_release_sender, check_release_receiver) = mpsc::channel::<()>();
    let check_started_sender = std::sync::Mutex::new(check_started_sender);
    let check_release_receiver = std::sync::Mutex::new(check_release_receiver);
    let blocking_restart_check: RestartCheck = Arc::new(move || {
        let _ = check_started_sender
            .lock()
            .expect("started sender")
            .send(());
        let _ = check_release_receiver
            .lock()
            .expect("release receiver")
            .recv();
        Ok(())
    });
    let session_overview = build_overview_named("still-here");
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_restartable_test_server(
            "restart-check-running",
            blocking_restart_check,
            Some(session_overview.clone()),
        );
    let restart_runtime_directory = runtime_directory.clone();
    let restart_thread =
        std::thread::spawn(move || send_restart_request(&restart_runtime_directory, session_id));
    check_started_receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the restart check starts");

    let mut discovery_connection = connect_greeted_connection(&runtime_directory, session_id);
    let discovery_result = request_discovery(&mut discovery_connection, 2);
    check_release_sender
        .send(())
        .expect("the check is released");
    let (restart_connection, restart_result) =
        restart_thread.join().expect("the restart request ends");

    assert_eq!(discovery_result, IpcResult::Overview(session_overview));
    assert_eq!(restart_result, IpcResult::Restarting);
    drop(discovery_connection);
    drop(restart_connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn an_accepted_restart_is_answered_restarting() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_restartable_test_server(
            "restart-accepted",
            build_restart_check_answering(Ok(())),
            None,
        );

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
            build_restart_check_answering(Ok(())),
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

/// A binary that this machine cannot run, in `binary_directory`: on Unix a file
/// without execute permission, elsewhere a path with nothing at it. Returns the
/// path and the sentence the check refuses it with.
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
            build_restart_check_answering(
                crate::server::is_binary_runnable(&executable_path)
                    .map_err(|refusal_reason| RestartRefusal::UnfitProgramFile { refusal_reason }),
            ),
            Some(session_overview.clone()),
        );

    let (mut connection, restart_result) = send_restart_request(&runtime_directory, session_id);

    assert_eq!(
        restart_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::RequestFailed,
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
    delete_test_directory(&binary_directory);
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
            build_restart_check_answering(
                crate::server::can_carry_panes(&carried_panes)
                    .map_err(|refusal_reason| RestartRefusal::PaneNotReady { refusal_reason }),
            ),
            Some(session_overview.clone()),
        );

    let (mut connection, restart_result) = send_restart_request(&runtime_directory, session_id);

    assert_eq!(
        restart_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::RequestFailed,
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
    wait_until(Duration::from_secs(5), || {
        ipc_server.count_attached_connections() == expected_attached_connection_count
    });
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
            request_kind: IpcRequestKind::Leaving { is_returning: true },
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
        is_returning,
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
    assert!(
        is_returning,
        "the detach carries the value the Leaving request named"
    );
    assert_eq!(
        ipc_server.count_attached_connections(),
        0,
        "the connection leaves the count before its detach reaches the session"
    );

    // The session closes the connection it was serving: the stream carries this
    // client's goodbye and then ends.
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("the goodbye frame"),
        SessionEvent::Detached,
    );
    let after_goodbye_result = connection.recv::<SessionEvent>();
    let Err(IpcError::Disconnected) = after_goodbye_result else {
        panic!("expected the connection to end, got {after_goodbye_result:?}");
    };

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_connection_that_leaves_drops_from_the_count_before_its_detach_is_handed_over() {
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
        start_attachable_test_server("leave-count-first", client_id);
    let mut connection = attach_test_client(&runtime_directory, session_id, client_id);
    assert_eq!(wait_for_attached_connection_count(&ipc_server, 1), 1);

    // While this test holds the intake's read lock, the serving thread cannot
    // drop the connection from the count, which takes the write lock.
    let held_intake_state = ipc_server
        .connection_intake
        .intake_state
        .read()
        .expect("intake");
    connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Leaving { is_returning: true },
        })
        .expect("send leaving");
    assert_eq!(
        received_runtime_events
            .recv_timeout(Duration::from_millis(500))
            .unwrap_err(),
        mpsc::RecvTimeoutError::Timeout,
        "no detach is handed over while the connection is still counted"
    );
    drop(held_intake_state);

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
    assert_eq!(ipc_server.count_attached_connections(), 0);

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_control_connection_that_leaves_is_closed_with_no_answer() {
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
        start_attachable_test_server("leaving-control", client_id);

    let mut connection = connect_greeted_connection(&runtime_directory, session_id);

    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Leaving {
                is_returning: false,
            },
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

    let accepted_connection = connect_greeted_connection(&runtime_directory, session_id);

    drop(stale_connection);
    drop(accepted_connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn rotating_the_token_hands_requests_to_the_dispatcher_again_after_they_were_refused() {
    // After `start_refusing_requests`, `rotate_token` hands requests to the
    // dispatcher again on the same socket.
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
        start_attachable_test_server("rotate-reopen", client_id);

    ipc_server.start_refusing_requests();
    ipc_server
        .rotate_token()
        .expect("the fresh token is advertised");

    let mut connection = connect_greeted_connection(&runtime_directory, session_id);

    // What this connection sends reaches the dispatcher again.
    let typed_key_chord = KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('r'));
    send_committed_request(&mut connection, build_attach_request(None, None));
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

// --- refusing requests ---

/// How long the refusal tests hold a request unanswered: 100 milliseconds.
const HELD_ANSWER_DURATION: Duration = Duration::from_millis(100);

/// The deadline the refusal tests give a wait for answers: 30 seconds, far
/// past the moment the held answer is written.
const DISTANT_ANSWER_WAIT_DURATION: Duration = Duration::from_secs(30);

/// The longest a wait for answers may take once the held answer is written at
/// 100 milliseconds: 10 seconds.
const ANSWER_WAKE_LIMIT_DURATION: Duration = Duration::from_secs(10);

/// The answer that refuses request `request_id` while the session restarts:
/// [`build_restarting_refusal`] under that request id.
fn build_restarting_refusal_response(request_id: u64) -> IpcResponse {
    IpcResponse {
        request_id: Some(request_id),
        answer_result: build_restarting_refusal(),
    }
}

/// The `Prepare` that hands [`build_test_command_envelope`] over under
/// request id `2`.
fn build_prepared_submit_request() -> IpcRequest {
    IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Prepare(Box::new(IpcRequestKind::SubmitCommand(Box::new(
            build_test_command_envelope(),
        )))),
    }
}

/// The `Commit` of the request held under request id `2`.
fn build_commit_request() -> IpcRequest {
    IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Commit,
    }
}

/// A served socket whose inbox the test reads itself, standing in for the
/// dispatcher. Returns the server, its session id, the runtime directory and
/// the inbox.
fn start_inbox_test_server(
    directory_tag: &str,
) -> (IpcServer, SessionId, PathBuf, Receiver<RuntimeEvent>) {
    let runtime_directory = build_test_runtime_directory(directory_tag);
    let session_id = SessionId::new();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start serving");
    (ipc_server, session_id, runtime_directory, inbox_receiver)
}

/// Wait for answers on `ipc_server` with a deadline
/// [`DISTANT_ANSWER_WAIT_DURATION`] away, run `write_held_answer`
/// [`HELD_ANSWER_DURATION`] after the wait starts, and check that the wait
/// ends with no request unanswered, no sooner than [`HELD_ANSWER_DURATION`]
/// and before [`ANSWER_WAKE_LIMIT_DURATION`].
fn run_answer_wait_around_held_answer(ipc_server: &IpcServer, write_held_answer: impl FnOnce()) {
    std::thread::scope(|thread_scope| {
        let wait_started_at = Instant::now();
        let answer_deadline = wait_started_at + DISTANT_ANSWER_WAIT_DURATION;
        let answer_wait_thread = thread_scope.spawn(move || {
            let unanswered_request_count = ipc_server.wait_until_requests_answered(answer_deadline);
            (unanswered_request_count, Instant::now())
        });
        std::thread::sleep(HELD_ANSWER_DURATION);
        write_held_answer();

        let (unanswered_request_count, wait_ended_at) =
            answer_wait_thread.join().expect("the wait ends");
        let wait_duration = wait_ended_at.duration_since(wait_started_at);
        assert_eq!(unanswered_request_count, 0);
        assert!(wait_duration >= HELD_ANSWER_DURATION);
        assert!(wait_duration < ANSWER_WAKE_LIMIT_DURATION);
    });
}

#[test]
fn a_key_a_client_sends_while_requests_are_refused_never_reaches_the_dispatcher() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory("intake-refusing");
    // The test keeps an inbox sender of its own. It queues the detach that ends
    // this client's writing thread after requests are refused.
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (dispatcher_thread, received_runtime_events) =
        spawn_attaching_dispatcher(inbox_receiver, client_id, session_id);
    let ipc_server = IpcServer::start(
        &runtime_directory,
        session_id,
        inbox_sender.clone(),
        None,
        None,
    )
    .expect("start serving");
    let mut connection = attach_test_client(&runtime_directory, session_id, client_id);
    let taken_key_chord = KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('a'));
    let refused_key_chord = KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('b'));

    // Before the refusal: the press reaches the dispatcher.
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

    ipc_server.start_refusing_requests();
    // The attach was answered: an attached client's connection holds no
    // request unanswered.
    assert_eq!(ipc_server.wait_until_requests_answered(Instant::now()), 0);

    // After the refusal: the press is read and never handed over, and the
    // reading loop ends. No event reaches the dispatcher, including the
    // detach the connection's own ending queues.
    connection
        .send(&IpcRequest {
            request_id: 4,
            request_kind: IpcRequestKind::Keyboard {
                key_input: build_key_input_for_chord(refused_key_chord),
            },
        })
        .expect("send the key press the session refuses");
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
            is_returning: false,
        })
        .expect("the detach is queued");
    drop(inbox_sender);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_connection_accepted_while_requests_are_refused_has_its_hello_answered_and_its_request_refused()
{
    let (ipc_server, session_id, runtime_directory, inbox_receiver) =
        start_inbox_test_server("refusing-accept");

    ipc_server.start_refusing_requests();

    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    connection
        .send(&build_prepared_submit_request())
        .expect("send the Prepare");
    let prepare_reply: IpcResponse = connection.recv().expect("the Prepare is answered");
    assert_eq!(prepare_reply, build_restarting_refusal_response(2));
    assert!(matches!(
        connection.recv::<IpcResponse>(),
        Err(IpcError::Disconnected)
    ));
    assert_eq!(
        inbox_receiver.try_recv().unwrap_err(),
        mpsc::TryRecvError::Empty
    );
    assert_eq!(ipc_server.wait_until_requests_answered(Instant::now()), 0);

    drop(connection);
    ipc_server.shutdown();
    drop(inbox_receiver);
    delete_test_directory(&runtime_directory);
}

#[test]
fn a_prepared_request_whose_commit_arrives_while_requests_are_refused_is_refused_and_does_not_run()
{
    let (ipc_server, session_id, runtime_directory, inbox_receiver) =
        start_inbox_test_server("refusing-commit");
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    connection
        .send(&build_prepared_submit_request())
        .expect("send the Prepare");
    answer_next_prepare_probe(&inbox_receiver);
    let prepare_reply: IpcResponse = connection.recv().expect("the Prepare is answered");
    assert_eq!(
        prepare_reply,
        IpcResponse {
            request_id: Some(2),
            answer_result: IpcResult::Prepared,
        }
    );

    ipc_server.start_refusing_requests();

    run_answer_wait_around_held_answer(&ipc_server, || {
        connection
            .send(&build_commit_request())
            .expect("send the Commit");
        let commit_reply: IpcResponse = connection.recv().expect("the Commit is answered");
        assert_eq!(commit_reply, build_restarting_refusal_response(2));
    });
    assert!(matches!(
        connection.recv::<IpcResponse>(),
        Err(IpcError::Disconnected)
    ));
    assert_eq!(
        inbox_receiver.try_recv().unwrap_err(),
        mpsc::TryRecvError::Empty
    );

    drop(connection);
    ipc_server.shutdown();
    drop(inbox_receiver);
    delete_test_directory(&runtime_directory);
}

#[test]
fn a_request_handed_over_before_the_refusal_is_answered_and_the_wait_ends_once_that_answer_is_written(
) {
    let (ipc_server, session_id, runtime_directory, inbox_receiver) =
        start_inbox_test_server("refusing-answered");
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    connection
        .send(&build_prepared_submit_request())
        .expect("send the Prepare");
    answer_next_prepare_probe(&inbox_receiver);
    let _: IpcResponse = connection.recv().expect("the Prepare is answered");
    connection
        .send(&build_commit_request())
        .expect("send the Commit");
    let Ok(RuntimeEvent::Ipc {
        command_envelope,
        response_sender,
    }) = inbox_receiver.recv()
    else {
        panic!("expected the committed command");
    };
    let command_result = CommandResult::Ok {
        command_id: command_envelope.command_id,
        emitted_events: Vec::new(),
    };

    ipc_server.start_refusing_requests();

    run_answer_wait_around_held_answer(&ipc_server, || {
        response_sender
            .send(command_result.clone())
            .expect("the serving thread waits for the answer");
    });
    let commit_reply: IpcResponse = connection.recv().expect("the Commit is answered");
    assert_eq!(
        commit_reply,
        IpcResponse {
            request_id: Some(2),
            answer_result: IpcResult::CommandResult(command_result),
        }
    );

    drop(connection);
    ipc_server.shutdown();
    drop(inbox_receiver);
    delete_test_directory(&runtime_directory);
}

#[test]
fn the_wait_for_answers_ends_at_its_deadline_with_a_request_still_unanswered() {
    let intake = Arc::new(Intake::default());
    let unanswered_request = intake.record_unanswered_request();
    let wait_started_at = Instant::now();

    let unanswered_request_count =
        intake.wait_until_requests_answered(wait_started_at + HELD_ANSWER_DURATION);

    assert_eq!(unanswered_request_count, 1);
    assert!(wait_started_at.elapsed() >= HELD_ANSWER_DURATION);
    drop(unanswered_request);
    assert_eq!(intake.wait_until_requests_answered(Instant::now()), 0);
}

#[test]
fn the_wait_for_answers_with_no_request_unanswered_ends_at_once() {
    let intake = Intake::default();

    assert_eq!(
        intake.wait_until_requests_answered(Instant::now() + DISTANT_ANSWER_WAIT_DURATION),
        0
    );
}

/// A detach event for `client_id` at timestamp `0`, for a client that does not
/// return.
fn build_detach_event(client_id: ClientId) -> RuntimeEvent {
    RuntimeEvent::ClientDetached {
        client_id,
        detached_at: SystemTime::UNIX_EPOCH,
        is_streamed: true,
        is_returning: false,
    }
}

#[test]
fn a_refusing_intake_hands_nothing_over_until_it_stops_refusing() {
    let intake = Intake::default();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let client_id = ClientId::new();

    assert_eq!(
        intake.hand_over_event(&inbox_sender, build_detach_event(client_id)),
        Ok(())
    );
    intake.start_refusing_requests();
    assert_eq!(
        intake.hand_over_event(&inbox_sender, build_detach_event(client_id)),
        Err(DispatchFailure::Refused)
    );
    // Refusing again leaves the intake refusing.
    intake.start_refusing_requests();
    assert_eq!(
        intake.hand_over_event(&inbox_sender, build_detach_event(client_id)),
        Err(DispatchFailure::Refused)
    );
    intake.stop_refusing_requests();
    assert_eq!(
        intake.hand_over_event(&inbox_sender, build_detach_event(client_id)),
        Ok(())
    );

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

    assert_eq!(
        intake.hand_over_event(&inbox_sender, build_detach_event(ClientId::new())),
        Err(DispatchFailure::DispatcherGone)
    );
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

#[test]
fn closing_a_served_connections_read_direction_ends_only_its_reads() {
    let socket_address = build_test_socket_address("intake-read-direction");
    delete_socket_file(&socket_address);
    let listener = Listener::bind(&socket_address).expect("bind");
    let accepting_thread = std::thread::spawn(move || listener.accept().expect("accept"));
    let mut caller_connection = Connection::connect(&socket_address).expect("connect");
    let mut accepted_connection = accepting_thread.join().expect("accepting thread");
    let intake = Arc::new(Intake::default());
    let served_connection = intake
        .accept_connection(&accepted_connection)
        .expect("the intake takes it");
    let ping_request = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::Ping,
    };

    caller_connection
        .send(&ping_request)
        .expect("send the first ping");
    intake.close_read_direction(served_connection.connection_number + 1);
    assert_eq!(
        accepted_connection
            .recv::<IpcRequest>()
            .expect("the first ping"),
        ping_request
    );

    caller_connection
        .send(&ping_request)
        .expect("send the second ping");
    intake.close_read_direction(served_connection.connection_number);
    match accepted_connection.recv::<IpcRequest>() {
        Err(IpcError::Disconnected) => {}
        unexpected_read_result => {
            panic!("the closed read direction read {unexpected_read_result:?}")
        }
    }

    drop(served_connection);
    drop(caller_connection);
    delete_socket_file(&socket_address);
}

/// A stand-in dispatcher that reports the `is_remote` flag of every attach it
/// is asked for and accepts each one as `client_id`. Holds the delivery queues
/// it hands out open, and the writing threads stay blocked. It answers each
/// `IpcPrepare` probe ([`answer_prepare_probe`]). Exits when every inbox sender
/// is gone.
fn spawn_origin_reporting_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
    client_id: ClientId,
    session_id: SessionId,
) -> (JoinHandle<()>, Receiver<bool>) {
    let (remote_flag_sender, remote_flag_receiver) = mpsc::channel();
    let dispatcher_thread = std::thread::spawn(move || {
        let mut delivery_senders = Vec::new();
        let ending_notice = Arc::new(EndingNotice::default());
        for runtime_event in inbox_receiver.iter().filter_map(answer_prepare_probe) {
            match runtime_event {
                RuntimeEvent::IpcAttach {
                    is_remote,
                    response_sender,
                    ..
                } => {
                    let (delivery_sender, delivery_receiver) =
                        create_delivery_queue(SUBSCRIBER_QUEUE_CAPACITY);
                    delivery_senders.push(delivery_sender);
                    if remote_flag_sender.send(is_remote).is_err() {
                        break;
                    }
                    let _ = response_sender.send(Some(build_attach_accepted(
                        client_id,
                        session_id,
                        delivery_receiver,
                        Arc::new(WriterHandoff::default()),
                        Arc::clone(&ending_notice),
                    )));
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

    send_committed_request(&mut connection, build_attach_request(None, None));
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
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
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

/// A stand-in dispatcher that answers every restart request with `Ok(())` and
/// reports each one on `restart_report_sender`. It answers each `IpcPrepare`
/// probe ([`answer_prepare_probe`]). Exits when every inbox sender is gone.
fn spawn_restart_reporting_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
    restart_report_sender: Sender<()>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        for runtime_event in inbox_receiver.iter().filter_map(answer_prepare_probe) {
            if let RuntimeEvent::IpcRestart { response_sender } = runtime_event {
                let _ = response_sender.send(());
                let _ = restart_report_sender.send(());
            }
        }
    })
}

/// A served socket for a session that runs `1.0.0`, whose program file in
/// `program_directory` is replaced, once the socket serves, by one that prints
/// `koshi <installed_version>` and counts its runs in the file `runs` beside
/// it. Hands back the server, the session id, the runtime directory, the
/// dispatcher thread, the receiver of its restart reports, and the path of the
/// program file: `koshi` on Unix and `koshi.cmd` on Windows.
fn start_watched_test_server(
    directory_tag: &str,
    program_directory: &Path,
    installed_version: &str,
) -> (
    IpcServer,
    SessionId,
    PathBuf,
    JoinHandle<()>,
    Receiver<()>,
    PathBuf,
) {
    let program_path = write_printing_program(
        program_directory,
        "koshi",
        &program_directory.join("started_runs"),
        "koshi 1.0.0",
    );
    let runtime_directory = build_test_runtime_directory(directory_tag);
    let session_id = SessionId::new();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (restart_report_sender, restart_report_receiver) = mpsc::channel();
    let dispatcher_thread =
        spawn_restart_reporting_dispatcher(inbox_receiver, restart_report_sender);
    let ipc_server = IpcServer::start(
        &runtime_directory,
        session_id,
        inbox_sender,
        None,
        Some(build_test_image_replacement(
            program_path.clone(),
            build_restart_check_answering(Ok(())),
        )),
    )
    .expect("start serving");
    let replacement_path = write_printing_program(
        program_directory,
        "replacement",
        &program_directory.join("runs"),
        &format!("koshi {installed_version}"),
    );
    std::fs::rename(&replacement_path, &program_path).expect("the program file is replaced");
    (
        ipc_server,
        session_id,
        runtime_directory,
        dispatcher_thread,
        restart_report_receiver,
        program_path,
    )
}

#[test]
fn a_connection_to_a_session_whose_program_file_holds_another_version_restarts_it_once() {
    let program_directory = tempfile::TempDir::new().expect("a program directory");
    let (
        ipc_server,
        session_id,
        runtime_directory,
        dispatcher_thread,
        restart_report_receiver,
        _program_path,
    ) = start_watched_test_server(
        "program-file-other-version",
        program_directory.path(),
        "9.9.9",
    );

    let first_connection = connect_to_session_socket(&runtime_directory, session_id);
    assert_eq!(
        restart_report_receiver.recv_timeout(Duration::from_secs(10)),
        Ok(())
    );
    let second_connection = connect_to_session_socket(&runtime_directory, session_id);

    assert_eq!(
        restart_report_receiver.recv_timeout(Duration::from_millis(500)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    assert_eq!(
        count_program_runs(&program_directory.path().join("runs")),
        1
    );
    drop((first_connection, second_connection));
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

/// Send a Hello on `connection` whose whole range sits above this build, for
/// the session `session_id` advertises in `runtime_directory`, and read the
/// refusal.
fn send_hello_sharing_no_version(
    connection: &mut Connection,
    runtime_directory: &Path,
    session_id: SessionId,
) -> IpcResponse {
    connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                minimum_protocol_version: PROTOCOL_VERSION + 1,
                maximum_protocol_version: PROTOCOL_VERSION + 2,
                connection_token: load_test_endpoint_file(runtime_directory, session_id)
                    .connection_token,
                is_remote: false,
            },
        })
        .expect("send a hello sharing no version");
    connection.recv().expect("hello reply")
}

#[test]
fn a_hello_refused_for_its_version_reads_the_program_file_the_session_started_from() {
    let program_directory = tempfile::TempDir::new().expect("a program directory");
    let run_log_path = program_directory.path().join("runs");
    let program_path = write_printing_program(
        program_directory.path(),
        "koshi",
        &run_log_path,
        "koshi 9.9.9",
    );
    let runtime_directory = build_test_runtime_directory("refused-hello");
    let session_id = SessionId::new();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (restart_report_sender, restart_report_receiver) = mpsc::channel();
    let dispatcher_thread =
        spawn_restart_reporting_dispatcher(inbox_receiver, restart_report_sender);
    let ipc_server = IpcServer::start(
        &runtime_directory,
        session_id,
        inbox_sender,
        None,
        Some(build_test_image_replacement(
            program_path,
            build_restart_check_answering(Ok(())),
        )),
    )
    .expect("start serving");
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);
    assert_eq!(
        restart_report_receiver.recv_timeout(Duration::from_millis(500)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    assert_eq!(count_program_runs(&run_log_path), 0);

    let hello_response =
        send_hello_sharing_no_version(&mut connection, &runtime_directory, session_id);

    let IpcResult::Error(IpcErrorPayload {
        code: IpcErrorCode::UnsupportedVersion,
        ..
    }) = hello_response.answer_result
    else {
        panic!("expected a version refusal, got {hello_response:?}");
    };
    assert_eq!(
        restart_report_receiver.recv_timeout(Duration::from_secs(10)),
        Ok(())
    );
    assert_eq!(count_program_runs(&run_log_path), 1);
    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_hello_refused_for_its_version_by_a_session_with_no_watch_reads_nothing() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_test_server("refused-hello-no-watch", None);
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);

    let hello_response =
        send_hello_sharing_no_version(&mut connection, &runtime_directory, session_id);

    let IpcResult::Error(IpcErrorPayload {
        code: IpcErrorCode::UnsupportedVersion,
        ..
    }) = hello_response.answer_result
    else {
        panic!("expected a version refusal, got {hello_response:?}");
    };
    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_connection_to_a_session_whose_program_file_holds_its_own_version_restarts_nothing() {
    let program_directory = tempfile::TempDir::new().expect("a program directory");
    let (
        ipc_server,
        session_id,
        runtime_directory,
        dispatcher_thread,
        restart_report_receiver,
        _program_path,
    ) = start_watched_test_server(
        "program-file-same-version",
        program_directory.path(),
        "1.0.0",
    );

    let connection = connect_to_session_socket(&runtime_directory, session_id);
    assert!(
        wait_until(Duration::from_secs(10), || {
            count_program_runs(&program_directory.path().join("runs")) > 0
        }),
        "the program never ran"
    );

    assert_eq!(
        restart_report_receiver.recv_timeout(Duration::from_millis(500)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_session_with_an_executable_watch_writes_its_program_file_and_removes_it_at_shutdown() {
    let program_directory = tempfile::TempDir::new().expect("a program directory");
    let (
        ipc_server,
        session_id,
        runtime_directory,
        dispatcher_thread,
        _restart_report_receiver,
        program_path,
    ) = start_watched_test_server("program-file-written", program_directory.path(), "1.0.0");
    let program_file_path =
        ServerProgramFile::resolve_session_program_file_path(&runtime_directory, session_id);

    assert_eq!(
        ServerProgramFile::load_from_path(&program_file_path).expect("read the program file"),
        Some(ServerProgramFile {
            process_id: std::process::id(),
            build_version: "1.0.0".to_string(),
            program_path: program_path.to_string_lossy().into_owned(),
        })
    );

    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    assert!(
        !program_file_path.exists(),
        "shutdown removes the program file"
    );
}

#[test]
fn a_session_without_an_executable_watch_writes_no_program_file() {
    let runtime_directory = build_test_runtime_directory("no-program-file");
    let session_id = SessionId::new();
    let (inbox_sender, _inbox_receiver) = mpsc::channel();

    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start serving");

    assert!(
        !ServerProgramFile::resolve_session_program_file_path(&runtime_directory, session_id)
            .exists()
    );
    ipc_server.shutdown();
    delete_test_directory(&runtime_directory);
}

#[test]
fn a_start_whose_program_file_cannot_be_written_leaves_no_endpoint_file_and_nothing_listening() {
    // A directory holding a file where the program file goes: the write
    // cannot rename over it, and the start unwinds the bind it already made.
    let program_directory = tempfile::TempDir::new().expect("a program directory");
    let program_path = write_printing_program(
        program_directory.path(),
        "koshi",
        &program_directory.path().join("started_runs"),
        "koshi 1.0.0",
    );
    let runtime_directory = build_test_runtime_directory("program-write-fails");
    koshi_paths::ensure_private_directory(&runtime_directory).expect("create runtime directory");
    let session_id = SessionId::new();
    let program_file_path =
        ServerProgramFile::resolve_session_program_file_path(&runtime_directory, session_id);
    std::fs::create_dir_all(&program_file_path).expect("plant a directory where the file goes");
    std::fs::write(program_file_path.join("held"), b"").expect("fill the planted directory");
    let socket_address = compute_socket_address(&runtime_directory, session_id);
    let (inbox_sender, _inbox_receiver) = mpsc::channel();

    let Err(IpcError::ProgramFileWrite {
        program_file_path: reported_program_file_path,
        ..
    }) = IpcServer::start(
        &runtime_directory,
        session_id,
        inbox_sender,
        None,
        Some(build_test_image_replacement(
            program_path,
            build_restart_check_answering(Ok(())),
        )),
    )
    else {
        panic!("a program file that cannot be written must refuse the start");
    };

    assert_eq!(
        reported_program_file_path,
        program_file_path.display().to_string()
    );
    assert!(
        !EndpointFile::resolve_endpoint_file_path(&runtime_directory, session_id).exists(),
        "the refused start removes the endpoint file it wrote"
    );
    let Err(IpcError::NoListener {
        socket_address: refused_socket_address,
    }) = Connection::connect(&socket_address)
    else {
        panic!("nothing listens after a refused start");
    };
    assert_eq!(refused_socket_address, socket_address);
    delete_test_directory(&runtime_directory);
}

/// Run [`restart_into_installed_version`] for `9.9.9` with a restart check
/// answering `restart_verdict`, on a watch of a program printing `koshi
/// 1.0.0`, against a stand-in dispatcher that takes the restart it is sent.
/// Hands back the restart retry instant the watch holds, the instants just
/// before and just after the run, and whether a restart reached the
/// dispatcher.
fn run_restart_into_installed_version(
    restart_verdict: Result<(), RestartRefusal>,
) -> (
    Option<std::time::Instant>,
    std::time::Instant,
    std::time::Instant,
    bool,
) {
    let program_directory = tempfile::TempDir::new().expect("a program directory");
    let program_path = write_printing_program(
        program_directory.path(),
        "koshi",
        &program_directory.path().join("started_runs"),
        "koshi 1.0.0",
    );
    let image_replacement =
        build_test_image_replacement(program_path, build_restart_check_answering(restart_verdict));
    let connection_intake = Intake::default();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let dispatcher_thread = std::thread::spawn(move || {
        let Ok(RuntimeEvent::IpcRestart { response_sender }) = inbox_receiver.recv() else {
            return false;
        };
        let _ = response_sender.send(());
        true
    });

    let started_at = std::time::Instant::now();
    restart_into_installed_version(
        &connection_intake,
        &inbox_sender,
        &image_replacement,
        "9.9.9",
    );
    let finished_at = std::time::Instant::now();
    drop(inbox_sender);
    let has_dispatcher_taken_restart = dispatcher_thread.join().expect("the dispatcher ends");

    let restart_retry_at = image_replacement
        .executable_watch
        .watch_state
        .lock()
        .expect("executable watch")
        .restart_retry_at;
    (
        restart_retry_at,
        started_at,
        finished_at,
        has_dispatcher_taken_restart,
    )
}

#[test]
fn a_restart_refused_for_a_pane_not_ready_schedules_a_retry_and_asks_no_dispatcher() {
    let (restart_retry_at, started_at, finished_at, has_dispatcher_taken_restart) =
        run_restart_into_installed_version(Err(RestartRefusal::PaneNotReady {
            refusal_reason: "a pane is still being written to".to_string(),
        }));

    let restart_retry_window = (started_at + RESTART_RETRY_INTERVAL_DURATION)
        ..=(finished_at + RESTART_RETRY_INTERVAL_DURATION);
    let Some(restart_retry_at) = restart_retry_at else {
        panic!("the refused restart schedules a retry");
    };
    assert!(
        restart_retry_window.contains(&restart_retry_at),
        "the retry is due 30 s after the refusal: {restart_retry_at:?} is outside \
         {restart_retry_window:?}"
    );
    assert!(!has_dispatcher_taken_restart);
}

#[test]
fn a_restart_refused_for_an_unfit_program_file_schedules_no_retry_and_asks_no_dispatcher() {
    let (restart_retry_at, _started_at, _finished_at, has_dispatcher_taken_restart) =
        run_restart_into_installed_version(Err(RestartRefusal::UnfitProgramFile {
            refusal_reason: "the binary at /x is not executable".to_string(),
        }));

    assert_eq!(restart_retry_at, None);
    assert!(!has_dispatcher_taken_restart);
}

#[test]
fn a_restart_into_the_installed_version_that_passes_its_check_reaches_the_dispatcher() {
    let (restart_retry_at, _started_at, _finished_at, has_dispatcher_taken_restart) =
        run_restart_into_installed_version(Ok(()));

    assert_eq!(restart_retry_at, None);
    assert!(has_dispatcher_taken_restart);
}

/// A stand-in dispatcher that answers the first attach as `client_id` in
/// `session_id`, streaming the delivery queue whose sender it hands back,
/// answers each `IpcPrepare` probe ([`answer_prepare_probe`]), and forwards
/// every other event it drains to the returned receiver. Exits when every
/// inbox sender is gone.
fn spawn_forwarding_frame_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
    client_id: ClientId,
    session_id: SessionId,
) -> (JoinHandle<()>, DeliverySender, Receiver<RuntimeEvent>) {
    let (delivery_sender, delivery_receiver) = create_delivery_queue(SUBSCRIBER_QUEUE_CAPACITY);
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let dispatcher_thread = std::thread::spawn(move || {
        let mut unanswered_delivery_receiver = Some(delivery_receiver);
        for runtime_event in inbox_receiver.iter().filter_map(answer_prepare_probe) {
            match runtime_event {
                RuntimeEvent::IpcAttach {
                    response_sender, ..
                } => {
                    let Some(delivery_receiver) = unanswered_delivery_receiver.take() else {
                        continue;
                    };
                    let _ = response_sender.send(Some(build_attach_accepted(
                        client_id,
                        session_id,
                        delivery_receiver,
                        Arc::new(WriterHandoff::default()),
                        Arc::new(EndingNotice::default()),
                    )));
                }
                forwarded_runtime_event => {
                    if runtime_event_sender.send(forwarded_runtime_event).is_err() {
                        break;
                    }
                }
            }
        }
    });
    (dispatcher_thread, delivery_sender, runtime_event_receiver)
}

/// One clipboard request a stand-in clipboard dispatcher was asked, in the
/// order it was asked.
#[derive(Debug, PartialEq, Eq)]
enum ClipboardRequestReport {
    /// A `ClipboardCopyPart` asked whether the session takes a copy of
    /// `held_copy_byte_count` bytes.
    CopyPart { held_copy_byte_count: usize },
    /// A `Prepare` carrying a `ClipboardCopy` asked whether the session routes
    /// a copy of `copied_byte_count` bytes from `pane_id`.
    CopyPrepare {
        pane_id: PaneId,
        copied_byte_count: usize,
    },
    /// A committed `ClipboardCopy` handed over a whole copy.
    Copy {
        pane_id: PaneId,
        clipboard_targets: String,
        copied_bytes: Vec<u8>,
    },
}

/// A stand-in dispatcher that takes, and routes, a copy of at most
/// `maximum_copy_byte_count` bytes, answers every whole copy with
/// `should_run_clipboard_tool`, and reports each clipboard request on the
/// returned receiver. It answers each `IpcPrepare` probe
/// ([`answer_prepare_probe`]). Every other event it drains is dropped. Exits
/// when every inbox sender is gone.
fn spawn_clipboard_dispatcher(
    inbox_receiver: Receiver<RuntimeEvent>,
    maximum_copy_byte_count: usize,
    should_run_clipboard_tool: bool,
) -> (JoinHandle<()>, Receiver<ClipboardRequestReport>) {
    let (request_report_sender, request_report_receiver) = mpsc::channel();
    let dispatcher_thread = std::thread::spawn(move || {
        for runtime_event in inbox_receiver.iter().filter_map(answer_prepare_probe) {
            let request_report = match runtime_event {
                RuntimeEvent::IpcClipboardCopyPart {
                    held_copy_byte_count,
                    response_sender,
                } => {
                    let _ = response_sender.send(held_copy_byte_count <= maximum_copy_byte_count);
                    ClipboardRequestReport::CopyPart {
                        held_copy_byte_count,
                    }
                }
                RuntimeEvent::IpcClipboardCopyPrepare {
                    pane_id,
                    copied_byte_count,
                    response_sender,
                } => {
                    let _ = response_sender.send(copied_byte_count <= maximum_copy_byte_count);
                    ClipboardRequestReport::CopyPrepare {
                        pane_id,
                        copied_byte_count,
                    }
                }
                RuntimeEvent::IpcClipboardCopy {
                    pane_id,
                    clipboard_targets,
                    copied_bytes,
                    response_sender,
                } => {
                    let _ = response_sender.send(should_run_clipboard_tool);
                    ClipboardRequestReport::Copy {
                        pane_id,
                        clipboard_targets,
                        copied_bytes,
                    }
                }
                _ => continue,
            };
            if request_report_sender.send(request_report).is_err() {
                break;
            }
        }
    });
    (dispatcher_thread, request_report_receiver)
}

/// A served socket in a fresh runtime directory whose stand-in dispatcher is
/// [`spawn_clipboard_dispatcher`] with `maximum_copy_byte_count` and
/// `should_run_clipboard_tool`.
fn start_clipboard_test_server(
    directory_tag: &str,
    maximum_copy_byte_count: usize,
    should_run_clipboard_tool: bool,
) -> (
    IpcServer,
    SessionId,
    PathBuf,
    JoinHandle<()>,
    Receiver<ClipboardRequestReport>,
) {
    let runtime_directory = build_test_runtime_directory(directory_tag);
    let session_id = SessionId::new();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (dispatcher_thread, request_reports) = spawn_clipboard_dispatcher(
        inbox_receiver,
        maximum_copy_byte_count,
        should_run_clipboard_tool,
    );
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start serving");
    (
        ipc_server,
        session_id,
        runtime_directory,
        dispatcher_thread,
        request_reports,
    )
}

/// Send `request_kind` as request `request_id` on `connection`, and hand back
/// the answer.
fn exchange_request(
    connection: &mut Connection,
    request_id: u64,
    request_kind: IpcRequestKind,
) -> IpcResponse {
    connection
        .send(&IpcRequest {
            request_id,
            request_kind,
        })
        .expect("send the request");
    connection.recv().expect("the request is answered")
}

/// A `ClipboardCopyPart` carrying `copied_bytes`.
fn build_copy_part_request(copied_bytes: &[u8]) -> IpcRequestKind {
    IpcRequestKind::ClipboardCopyPart {
        copied_bytes: copied_bytes.to_vec(),
    }
}

/// A `ClipboardCopy` for `pane_id` and the targets `c`, carrying
/// `copied_bytes`.
fn build_copy_request(pane_id: PaneId, copied_bytes: &[u8]) -> IpcRequestKind {
    IpcRequestKind::ClipboardCopy {
        pane_id,
        clipboard_targets: "c".to_string(),
        copied_bytes: copied_bytes.to_vec(),
    }
}

/// A `Prepare` carrying [`build_copy_request`] for `pane_id` and
/// `copied_bytes`.
fn build_copy_prepare_request(pane_id: PaneId, copied_bytes: &[u8]) -> IpcRequestKind {
    IpcRequestKind::Prepare(Box::new(build_copy_request(pane_id, copied_bytes)))
}

/// The `Prepared` answer to request `request_id`.
fn build_prepared_response(request_id: u64) -> IpcResponse {
    IpcResponse {
        request_id: Some(request_id),
        answer_result: IpcResult::Prepared,
    }
}

/// The answer to request `request_id` that routes a copy and tells the caller
/// whether to run the real clipboard tool.
fn build_copy_routed_response(request_id: u64, should_run_clipboard_tool: bool) -> IpcResponse {
    IpcResponse {
        request_id: Some(request_id),
        answer_result: IpcResult::ClipboardCopyRouted {
            should_run_clipboard_tool,
        },
    }
}

/// The refusal a clipboard request from another machine or another local
/// user gets, as the answer to request `request_id`.
fn build_clipboard_refusal_response(request_id: u64) -> IpcResponse {
    IpcResponse {
        request_id: Some(request_id),
        answer_result: IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::RequestFailed,
            message: "a pane clipboard copy must come from this machine and this user".to_string(),
        }),
    }
}

#[test]
fn a_copy_in_one_request_reaches_the_dispatcher_as_it_is_and_carries_its_answer_back() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, request_reports) =
        start_clipboard_test_server("clipboard-one-request", 8, false);
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    let pane_id = PaneId::new();

    let prepare_response = exchange_request(
        &mut connection,
        2,
        IpcRequestKind::Prepare(Box::new(IpcRequestKind::ClipboardCopy {
            pane_id,
            clipboard_targets: "pX".to_string(),
            copied_bytes: b"hi".to_vec(),
        })),
    );
    let commit_response = exchange_request(&mut connection, 3, IpcRequestKind::Commit);

    assert_eq!(prepare_response, build_prepared_response(2));
    assert_eq!(commit_response, build_copy_routed_response(3, false));
    assert_eq!(
        request_reports.recv_timeout(Duration::from_secs(5)),
        Ok(ClipboardRequestReport::CopyPrepare {
            pane_id,
            copied_byte_count: 2,
        })
    );
    assert_eq!(
        request_reports.recv_timeout(Duration::from_secs(5)),
        Ok(ClipboardRequestReport::Copy {
            pane_id,
            clipboard_targets: "pX".to_string(),
            copied_bytes: b"hi".to_vec(),
        })
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn taken_copy_parts_reach_the_dispatcher_with_the_last_request_as_one_copy() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, request_reports) =
        start_clipboard_test_server("clipboard-parts", 8, true);
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    let pane_id = PaneId::new();

    for (request_id, part_bytes, held_copy_byte_count) in [(2, b"ab", 2), (3, b"cd", 4)] {
        assert_eq!(
            exchange_request(
                &mut connection,
                request_id,
                build_copy_part_request(part_bytes)
            ),
            IpcResponse {
                request_id: Some(request_id),
                answer_result: IpcResult::ClipboardCopyPartTaken,
            }
        );
        assert_eq!(
            request_reports.recv_timeout(Duration::from_secs(5)),
            Ok(ClipboardRequestReport::CopyPart {
                held_copy_byte_count
            })
        );
    }
    assert_eq!(
        exchange_request(
            &mut connection,
            4,
            build_copy_prepare_request(pane_id, b"ef")
        ),
        build_prepared_response(4)
    );
    assert_eq!(
        exchange_request(&mut connection, 5, IpcRequestKind::Commit),
        build_copy_routed_response(5, true)
    );
    assert_eq!(
        request_reports.recv_timeout(Duration::from_secs(5)),
        Ok(ClipboardRequestReport::CopyPrepare {
            pane_id,
            copied_byte_count: 6,
        })
    );
    assert_eq!(
        request_reports.recv_timeout(Duration::from_secs(5)),
        Ok(ClipboardRequestReport::Copy {
            pane_id,
            clipboard_targets: "c".to_string(),
            copied_bytes: b"abcdef".to_vec(),
        })
    );
    // The connection holds nothing once the copy is routed.
    assert_eq!(
        exchange_request(
            &mut connection,
            6,
            build_copy_prepare_request(pane_id, b"x")
        ),
        build_prepared_response(6)
    );
    assert_eq!(
        exchange_request(&mut connection, 7, IpcRequestKind::Commit),
        build_copy_routed_response(7, true)
    );
    assert_eq!(
        request_reports.recv_timeout(Duration::from_secs(5)),
        Ok(ClipboardRequestReport::CopyPrepare {
            pane_id,
            copied_byte_count: 1,
        })
    );
    assert_eq!(
        request_reports.recv_timeout(Duration::from_secs(5)),
        Ok(ClipboardRequestReport::Copy {
            pane_id,
            clipboard_targets: "c".to_string(),
            copied_bytes: b"x".to_vec(),
        })
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_copy_part_the_session_does_not_take_drops_every_held_byte_and_says_run_the_real_tool() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, request_reports) =
        start_clipboard_test_server("clipboard-part-refused", 3, false);
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    let pane_id = PaneId::new();

    assert_eq!(
        exchange_request(&mut connection, 2, build_copy_part_request(b"ab")),
        IpcResponse {
            request_id: Some(2),
            answer_result: IpcResult::ClipboardCopyPartTaken,
        }
    );
    assert_eq!(
        exchange_request(&mut connection, 3, build_copy_part_request(b"cd")),
        build_copy_routed_response(3, true)
    );
    assert_eq!(
        exchange_request(
            &mut connection,
            4,
            build_copy_prepare_request(pane_id, b"x")
        ),
        build_prepared_response(4)
    );
    assert_eq!(
        exchange_request(&mut connection, 5, IpcRequestKind::Commit),
        build_copy_routed_response(5, false)
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    assert_eq!(
        request_reports
            .try_iter()
            .collect::<Vec<ClipboardRequestReport>>(),
        vec![
            ClipboardRequestReport::CopyPart {
                held_copy_byte_count: 2
            },
            ClipboardRequestReport::CopyPart {
                held_copy_byte_count: 4
            },
            ClipboardRequestReport::CopyPrepare {
                pane_id,
                copied_byte_count: 1,
            },
            ClipboardRequestReport::Copy {
                pane_id,
                clipboard_targets: "c".to_string(),
                copied_bytes: b"x".to_vec(),
            },
        ]
    );
}

#[test]
fn a_clipboard_request_from_another_machine_is_refused_and_the_connection_keeps_serving() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, request_reports) =
        start_clipboard_test_server("clipboard-remote", 8, false);
    let endpoint_file = load_test_endpoint_file(&runtime_directory, session_id);
    let mut connection = connect_to_session_socket(&runtime_directory, session_id);
    let hello_response = exchange_request(
        &mut connection,
        1,
        IpcRequestKind::Hello {
            minimum_protocol_version: MIN_PROTOCOL_VERSION,
            maximum_protocol_version: PROTOCOL_VERSION,
            connection_token: endpoint_file.connection_token,
            is_remote: true,
        },
    );
    assert_eq!(hello_response.answer_result, build_accepted_hello_result());

    assert_eq!(
        exchange_request(&mut connection, 2, build_copy_part_request(b"ab")),
        build_clipboard_refusal_response(2)
    );
    assert_eq!(
        exchange_request(
            &mut connection,
            3,
            build_copy_prepare_request(PaneId::new(), b"x")
        ),
        build_clipboard_refusal_response(3)
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    assert_eq!(
        request_reports
            .try_iter()
            .collect::<Vec<ClipboardRequestReport>>(),
        Vec::new()
    );
}

#[test]
fn a_clipboard_request_from_another_local_user_is_refused_and_the_connection_keeps_serving() {
    let is_other_user_access_enabled = Arc::new(AtomicBool::new(true));
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (dispatcher_thread, request_reports) = spawn_clipboard_dispatcher(inbox_receiver, 8, false);
    let (mut caller_connection, serving_thread, socket_address) =
        serve_other_user("clipboard", &is_other_user_access_enabled, inbox_sender);
    caller_connection
        .send(&build_other_user_hello())
        .expect("send hello");
    let hello_response: IpcResponse = caller_connection.recv().expect("hello reply");
    assert_eq!(hello_response.answer_result, build_accepted_hello_result());

    assert_eq!(
        exchange_request(&mut caller_connection, 2, build_copy_part_request(b"ab")),
        build_clipboard_refusal_response(2)
    );
    assert_eq!(
        exchange_request(
            &mut caller_connection,
            3,
            build_copy_prepare_request(PaneId::new(), b"x")
        ),
        build_clipboard_refusal_response(3)
    );

    drop(caller_connection);
    serving_thread.join().expect("serving thread");
    dispatcher_thread.join().expect("dispatcher exits");
    delete_socket_file(&socket_address);
    assert_eq!(
        request_reports
            .try_iter()
            .collect::<Vec<ClipboardRequestReport>>(),
        Vec::new()
    );
}

#[test]
fn a_clipboard_request_with_no_dispatcher_closes_the_connection() {
    for (directory_tag, clipboard_request_kind) in [
        ("clipboard-part-gone", build_copy_part_request(b"ab")),
        (
            "clipboard-copy-gone",
            build_copy_prepare_request(PaneId::new(), b"x"),
        ),
    ] {
        let runtime_directory = build_test_runtime_directory(directory_tag);
        let session_id = SessionId::new();
        let (inbox_sender, inbox_receiver) = mpsc::channel();
        drop(inbox_receiver);
        let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
            .expect("start serving");
        let mut connection = connect_greeted_connection(&runtime_directory, session_id);

        connection
            .send(&IpcRequest {
                request_id: 2,
                request_kind: clipboard_request_kind,
            })
            .expect("send the clipboard request");
        assert!(
            matches!(
                connection.recv::<IpcResponse>(),
                Err(IpcError::Disconnected)
            ),
            "no answer comes back once the dispatcher is gone",
        );

        drop(connection);
        ipc_server.shutdown();
        delete_test_directory(&runtime_directory);
    }
}

/// Open a connection whose Hello carries `is_remote`, attach on it with the
/// request [`build_attach_request`] gives for `resume_client_id` and
/// `link_timeout_ms`, and read both replies back. The connection comes back
/// carrying `client_id`'s stream.
fn attach_test_client_over_link(
    runtime_directory: &Path,
    session_id: SessionId,
    client_id: ClientId,
    is_remote: bool,
    link_timeout_ms: Option<u32>,
    resume_client_id: Option<ClientId>,
) -> Connection {
    let mut connection = connect_to_session_socket(runtime_directory, session_id);
    connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                minimum_protocol_version: MIN_PROTOCOL_VERSION,
                maximum_protocol_version: PROTOCOL_VERSION,
                connection_token: load_test_endpoint_file(runtime_directory, session_id)
                    .connection_token,
                is_remote,
            },
        })
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.answer_result, build_accepted_hello_result());
    send_committed_request(
        &mut connection,
        build_attach_request(resume_client_id, link_timeout_ms),
    );
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

/// The next event the dispatcher was handed, within 5 seconds.
fn receive_forwarded_event(received_runtime_events: &Receiver<RuntimeEvent>) -> RuntimeEvent {
    received_runtime_events
        .recv_timeout(Duration::from_secs(5))
        .expect("the dispatcher was handed an event")
}

#[test]
fn a_ping_on_an_attached_stream_crosses_nothing_and_gets_no_answer() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory("ping-dropped");
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (dispatcher_thread, delivery_sender, forwarded_runtime_events) =
        spawn_forwarding_frame_dispatcher(inbox_receiver, client_id, session_id);
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start serving");
    let mut connection = attach_test_client(&runtime_directory, session_id, client_id);

    for request_id in [3, 4] {
        connection
            .send(&IpcRequest {
                request_id,
                request_kind: IpcRequestKind::Ping,
            })
            .expect("send the ping");
    }
    connection
        .send(&IpcRequest {
            request_id: 5,
            request_kind: IpcRequestKind::Leaving { is_returning: true },
        })
        .expect("send leaving");
    // The reading loop read both pings before the `Leaving`.
    let RuntimeEvent::ClientDetached {
        client_id: detached_client_id,
        is_returning,
        ..
    } = receive_forwarded_event(&forwarded_runtime_events)
    else {
        panic!("the first event handed over is the detach");
    };
    let rejected_command_id = CommandId::new();
    delivery_sender
        .try_send(Delivery::PlacementCommandRejected(rejected_command_id))
        .expect("the writing thread holds the delivery queue");

    assert_eq!(detached_client_id, client_id);
    assert!(is_returning);
    assert_eq!(
        connection.recv::<SessionEvent>().expect("the client reads"),
        SessionEvent::PlacementCommandRejected {
            command_id: rejected_command_id
        }
    );

    drop(delivery_sender);
    drop(connection);
    ipc_server.shutdown();
    dispatcher_thread.join().expect("dispatcher exits");
    delete_test_directory(&runtime_directory);
}

#[test]
fn a_ping_on_a_connection_with_no_attached_client_closes_it_with_no_answer() {
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
        start_attachable_test_server("ping-control", client_id);
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);

    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Ping,
        })
        .expect("send the ping");
    assert!(
        matches!(
            connection.recv::<IpcResponse>(),
            Err(IpcError::Disconnected)
        ),
        "a ping with no attached client closes the connection unanswered",
    );
    assert_eq!(
        received_runtime_events
            .recv_timeout(Duration::from_millis(300))
            .unwrap_err(),
        mpsc::RecvTimeoutError::Timeout,
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_silent_client_with_a_link_timeout_stays_attached_and_reads_a_pong_every_third_of_it() {
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
        start_attachable_test_server("link-heartbeat", client_id);
    let attach_started_at = Instant::now();
    let connection = attach_test_client_over_link(
        &runtime_directory,
        session_id,
        client_id,
        true,
        Some(MIN_LINK_TIMEOUT_MS),
        None,
    );
    // The client's writing half stays open, and sends nothing, until the end
    // of the test.
    let (mut frame_reader, frame_writer) = connection.split();
    frame_reader.set_deadline(Some(attach_started_at + Duration::from_secs(10)));
    let heartbeat_interval = compute_heartbeat_interval(MIN_LINK_TIMEOUT_MS);

    // Four pongs reach past the link timeout itself.
    let mut pong_read_times: Vec<Duration> = Vec::new();
    for _ in 0..4 {
        assert_eq!(
            frame_reader
                .recv::<SessionEvent>()
                .expect("a pong arrives inside 10 seconds"),
            SessionEvent::Pong
        );
        pong_read_times.push(attach_started_at.elapsed());
    }

    for (pong_index, pong_read_time) in (1u32..).zip(&pong_read_times) {
        assert!(
            *pong_read_time >= heartbeat_interval * pong_index,
            "pong {pong_index} came {pong_read_time:?} after the attach"
        );
    }
    assert!(
        pong_read_times[3] < Duration::from_millis(u64::from(MIN_LINK_TIMEOUT_MS)) * 4,
        "four pongs take four heartbeat intervals, not four link timeouts: {pong_read_times:?}"
    );
    assert_eq!(
        received_runtime_events.try_recv().unwrap_err(),
        mpsc::TryRecvError::Empty,
        "nothing detached the silent client"
    );

    drop(frame_writer);
    drop(frame_reader);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_frame_that_goes_out_puts_off_the_next_pong_by_a_whole_heartbeat_interval() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory("pong-after-frame");
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (dispatcher_thread, delivery_sender, _forwarded_runtime_events) =
        spawn_forwarding_frame_dispatcher(inbox_receiver, client_id, session_id);
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start serving");
    let connection = attach_test_client_over_link(
        &runtime_directory,
        session_id,
        client_id,
        true,
        Some(MIN_LINK_TIMEOUT_MS),
        None,
    );
    let (mut frame_reader, frame_writer) = connection.split();
    frame_reader.set_deadline(Some(Instant::now() + Duration::from_secs(10)));
    let heartbeat_interval = compute_heartbeat_interval(MIN_LINK_TIMEOUT_MS);

    // Half an interval after the first pong, a rejection is queued. A pong
    // that went out before the rejection is read past.
    assert_eq!(
        frame_reader.recv::<SessionEvent>().expect("the first pong"),
        SessionEvent::Pong
    );
    std::thread::sleep(heartbeat_interval / 2);
    let rejected_command_id = CommandId::new();
    let rejection_queued_at = Instant::now();
    delivery_sender
        .try_send(Delivery::PlacementCommandRejected(rejected_command_id))
        .expect("the writing thread holds the delivery queue");
    let rejection_event = loop {
        let session_event = frame_reader.recv::<SessionEvent>().expect("the rejection");
        if session_event != SessionEvent::Pong {
            break session_event;
        }
    };
    let next_event = frame_reader.recv::<SessionEvent>().expect("the next pong");
    let next_pong_delay = rejection_queued_at.elapsed();

    assert_eq!(
        rejection_event,
        SessionEvent::PlacementCommandRejected {
            command_id: rejected_command_id
        }
    );
    assert_eq!(next_event, SessionEvent::Pong);
    assert!(
        next_pong_delay >= heartbeat_interval,
        "the next pong came {next_pong_delay:?} after the rejection was queued"
    );

    drop(delivery_sender);
    drop(frame_writer);
    drop(frame_reader);
    ipc_server.shutdown();
    dispatcher_thread.join().expect("dispatcher exits");
    delete_test_directory(&runtime_directory);
}

#[test]
fn an_attach_with_a_link_timeout_below_the_minimum_is_refused_and_the_connection_keeps_serving() {
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, _received_runtime_events) =
        start_attachable_test_server("link-timeout-minimum", client_id);
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    let build_attach_request_with_link_timeout =
        |request_id: u64, link_timeout_ms: u32| IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Attach {
                viewport_size: CLIENT_VIEWPORT_SIZE,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities: GraphicsCapabilities::default(),
                cell_size: None,
                link_timeout_ms: Some(link_timeout_ms),
                max_frames_per_second: 60,
            },
        };

    send_committed_request(
        &mut connection,
        build_attach_request_with_link_timeout(2, MIN_LINK_TIMEOUT_MS - 1),
    );
    let refusal: IpcResponse = connection.recv().expect("the refusal");
    assert_eq!(refusal.request_id, Some(2));
    assert_eq!(
        refusal.answer_result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::RequestFailed,
            message: "a link timeout of 999 ms is below the minimum of 1000 ms".to_owned(),
        })
    );

    send_committed_request(
        &mut connection,
        build_attach_request_with_link_timeout(3, MIN_LINK_TIMEOUT_MS),
    );
    let attach_reply: IpcResponse = connection.recv().expect("attach reply");
    assert_eq!(attach_reply.request_id, Some(3));
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

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn an_attach_with_a_viewport_over_the_cell_limit_is_refused_and_the_connection_keeps_serving() {
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, _received_runtime_events) =
        start_attachable_test_server("viewport-cell-limit", client_id);
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    let build_attach_request_with_viewport = |request_id: u64, viewport_size: Size| IpcRequest {
        request_id,
        request_kind: IpcRequestKind::Attach {
            viewport_size,
            resume_client_id: None,
            resume_token: None,
            pane_area: None,
            graphics_capabilities: GraphicsCapabilities::default(),
            cell_size: None,
            link_timeout_ms: None,
            max_frames_per_second: 60,
        },
    };

    for (request_id, viewport_size) in [
        (
            2,
            Size {
                column_count: 65_535,
                row_count: 65_535,
            },
        ),
        (
            3,
            Size {
                column_count: 2048,
                row_count: 1025,
            },
        ),
    ] {
        send_committed_request(
            &mut connection,
            build_attach_request_with_viewport(request_id, viewport_size),
        );
        let refusal: IpcResponse = connection.recv().expect("the refusal");
        assert_eq!(
            refusal,
            build_request_failed_test_response(
                request_id,
                &format!(
                    "a viewport of {}x{} cells is over the limit of 2097152 cells",
                    viewport_size.column_count, viewport_size.row_count
                ),
            )
        );
    }

    send_committed_request(
        &mut connection,
        build_attach_request_with_viewport(
            4,
            Size {
                column_count: 2048,
                row_count: 1024,
            },
        ),
    );
    let attach_reply: IpcResponse = connection.recv().expect("attach reply");
    assert_eq!(attach_reply.request_id, Some(4));
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

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn an_attach_with_a_frame_rate_outside_30_to_120_is_refused_and_the_connection_keeps_serving() {
    let client_id = ClientId::new();
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, _received_runtime_events) =
        start_attachable_test_server("frame-rate-range", client_id);
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    let build_attach_request_with_frame_rate =
        |request_id: u64, max_frames_per_second: u16| IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Attach {
                viewport_size: CLIENT_VIEWPORT_SIZE,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities: GraphicsCapabilities::default(),
                cell_size: None,
                link_timeout_ms: None,
                max_frames_per_second,
            },
        };

    for (request_id, max_frames_per_second) in [(2, 0), (3, 29), (4, 121)] {
        send_committed_request(
            &mut connection,
            build_attach_request_with_frame_rate(request_id, max_frames_per_second),
        );
        let refusal: IpcResponse = connection.recv().expect("the refusal");
        assert_eq!(
            refusal,
            build_request_failed_test_response(
                request_id,
                &format!(
                    "a frame rate of {max_frames_per_second} frames a second is outside 30 to 120"
                ),
            )
        );
    }

    send_committed_request(
        &mut connection,
        build_attach_request_with_frame_rate(5, 120),
    );
    let attach_reply: IpcResponse = connection.recv().expect("attach reply");
    assert_eq!(attach_reply.request_id, Some(5));
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

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn a_connection_that_ends_with_no_leaving_reports_its_client_returning_only_from_another_machine() {
    for is_remote in [false, true] {
        let client_id = ClientId::new();
        let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
            start_attachable_test_server("end-without-leaving", client_id);
        let connection = attach_test_client_over_link(
            &runtime_directory,
            session_id,
            client_id,
            is_remote,
            None,
            None,
        );

        drop(connection);
        let RuntimeEvent::ClientDetached {
            client_id: detached_client_id,
            is_returning,
            ..
        } = receive_forwarded_event(&received_runtime_events)
        else {
            panic!("the closed connection detaches its client");
        };
        assert_eq!(detached_client_id, client_id);
        assert_eq!(is_returning, is_remote, "is_remote {is_remote}");

        stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    }
}

#[test]
fn leaving_from_another_machine_reports_whether_the_client_returns() {
    for is_client_returning in [false, true] {
        let client_id = ClientId::new();
        let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
            start_attachable_test_server("leaving-remote", client_id);
        let mut connection = attach_test_client_over_link(
            &runtime_directory,
            session_id,
            client_id,
            true,
            None,
            None,
        );

        connection
            .send(&IpcRequest {
                request_id: 3,
                request_kind: IpcRequestKind::Leaving {
                    is_returning: is_client_returning,
                },
            })
            .expect("send leaving");
        let RuntimeEvent::ClientDetached { is_returning, .. } =
            receive_forwarded_event(&received_runtime_events)
        else {
            panic!("leaving detaches the client");
        };
        assert_eq!(is_returning, is_client_returning);
        // The session closes the queue on the detach: the client reads its
        // goodbye.
        assert_eq!(
            connection
                .recv::<SessionEvent>()
                .expect("the goodbye frame"),
            SessionEvent::Detached
        );

        drop(connection);
        stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    }
}

#[test]
fn a_local_attach_that_names_a_client_to_come_back_as_reports_it_returning_until_its_first_request()
{
    for has_sent_request in [false, true] {
        let client_id = ClientId::new();
        let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
            start_attachable_test_server("end-before-first-request", client_id);
        let mut connection = attach_test_client_over_link(
            &runtime_directory,
            session_id,
            client_id,
            false,
            None,
            Some(client_id),
        );
        if has_sent_request {
            connection
                .send(&IpcRequest {
                    request_id: 3,
                    request_kind: IpcRequestKind::Resize {
                        viewport_size: CLIENT_VIEWPORT_SIZE,
                        pane_area: None,
                        cell_size: None,
                    },
                })
                .expect("send the size report");
            let RuntimeEvent::Resize {
                client_id: resized_client_id,
                ..
            } = receive_forwarded_event(&received_runtime_events)
            else {
                panic!("the size report reaches the dispatcher");
            };
            assert_eq!(resized_client_id, client_id);
        }

        drop(connection);
        let RuntimeEvent::ClientDetached {
            client_id: detached_client_id,
            is_streamed,
            is_returning,
            ..
        } = receive_forwarded_event(&received_runtime_events)
        else {
            panic!("the closed connection detaches its client");
        };
        assert_eq!(detached_client_id, client_id);
        assert!(is_streamed);
        assert_eq!(
            is_returning, !has_sent_request,
            "has_sent_request {has_sent_request}"
        );

        stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    }
}

#[test]
fn an_attach_whose_answer_cannot_be_written_reports_its_client_returning_only_when_it_names_a_client_to_come_back_as(
) {
    for resume_client_id in [None, Some(ClientId::new())] {
        let client_id = ClientId::new();
        let session_id = SessionId::new();
        let runtime_directory = build_test_runtime_directory("attach-answer-unwritten");
        let (inbox_sender, inbox_receiver) = mpsc::channel();
        let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
            .expect("start serving");
        let mut connection = connect_greeted_connection(&runtime_directory, session_id);
        send_prepare_request(
            &mut connection,
            build_attach_request(resume_client_id, None),
        );
        answer_next_prepare_probe(&inbox_receiver);
        read_prepared_and_send_commit(&mut connection, 2);
        let RuntimeEvent::IpcAttach {
            response_sender, ..
        } = receive_forwarded_event(&inbox_receiver)
        else {
            panic!("the attach reaches the dispatcher");
        };

        // The answer names a session whose name alone is as long as the frame
        // cap: the session cannot write it.
        let (_delivery_sender, delivery_receiver) =
            create_delivery_queue(SUBSCRIBER_QUEUE_CAPACITY);
        let mut attach_accepted = build_attach_accepted(
            client_id,
            session_id,
            delivery_receiver,
            Arc::new(WriterHandoff::default()),
            Arc::new(EndingNotice::default()),
        );
        attach_accepted.session_structure.session_name =
            "a".repeat(transport::MAX_FRAME_BYTE_COUNT as usize);
        response_sender
            .send(Some(attach_accepted))
            .expect("the session waits for the answer");
        let RuntimeEvent::ClientDetached {
            client_id: detached_client_id,
            is_streamed,
            is_returning,
            ..
        } = receive_forwarded_event(&inbox_receiver)
        else {
            panic!("the answer that cannot be written detaches its client");
        };
        assert_eq!(detached_client_id, client_id);
        assert!(!is_streamed);
        assert_eq!(
            is_returning,
            resume_client_id.is_some(),
            "resume_client_id {resume_client_id:?}"
        );

        drop(connection);
        ipc_server.shutdown();
        delete_test_directory(&runtime_directory);
    }
}

#[test]
fn a_stream_the_session_closed_hands_over_one_detach_when_the_client_closes() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory("one-detach");
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (dispatcher_thread, delivery_sender, forwarded_runtime_events) =
        spawn_forwarding_frame_dispatcher(inbox_receiver, client_id, session_id);
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start serving");
    let mut connection = attach_test_client(&runtime_directory, session_id, client_id);

    // The closed queue ends the writing thread after its goodbye.
    drop(delivery_sender);
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("the goodbye frame"),
        SessionEvent::Detached
    );
    assert_eq!(
        forwarded_runtime_events
            .recv_timeout(Duration::from_millis(300))
            .unwrap_err(),
        mpsc::RecvTimeoutError::Timeout,
        "the writing thread hands over no detach"
    );

    drop(connection);
    let RuntimeEvent::ClientDetached {
        client_id: detached_client_id,
        ..
    } = receive_forwarded_event(&forwarded_runtime_events)
    else {
        panic!("the reading half detaches the client");
    };
    assert_eq!(detached_client_id, client_id);
    assert_eq!(
        forwarded_runtime_events
            .recv_timeout(Duration::from_millis(300))
            .unwrap_err(),
        mpsc::RecvTimeoutError::Timeout,
        "the client is detached once"
    );

    ipc_server.shutdown();
    dispatcher_thread.join().expect("dispatcher exits");
    delete_test_directory(&runtime_directory);
}

#[cfg(target_os = "linux")]
#[test]
fn a_failed_write_detaches_a_client_whose_connection_stays_open() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory("failed-write");
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let (dispatcher_thread, delivery_sender, forwarded_runtime_events) =
        spawn_forwarding_frame_dispatcher(inbox_receiver, client_id, session_id);
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start serving");
    let connection = attach_test_client(&runtime_directory, session_id, client_id);
    // On Linux, a write to a peer that shut its read direction fails with
    // `EPIPE`. The client keeps its writing direction open.
    connection
        .create_read_closer()
        .expect("the client's read direction")
        .close();

    delivery_sender
        .try_send(Delivery::PlacementCommandRejected(CommandId::new()))
        .expect("the writing thread holds the delivery queue");

    let RuntimeEvent::ClientDetached {
        client_id: detached_client_id,
        ..
    } = receive_forwarded_event(&forwarded_runtime_events)
    else {
        panic!("the failed write detaches the client");
    };
    assert_eq!(detached_client_id, client_id);

    drop(delivery_sender);
    drop(connection);
    ipc_server.shutdown();
    dispatcher_thread.join().expect("dispatcher exits");
    delete_test_directory(&runtime_directory);
}

#[test]
fn a_resynced_client_reads_the_resync_then_the_frame_it_resumes_from() {
    // The queue holds its full 1024 deliveries, so one more event pauses the
    // subscriber. The backlog is read, and the resync queues its snapshot.
    use koshi_core::event::{Event, TabCreated};

    use crate::runtime::bus::{EventBus, SUBSCRIBER_QUEUE_CAPACITY};

    let tab_id = TabId::new();
    let mut event_bus = EventBus::new();
    let (subscriber_id, delivery_receiver, writer_handoff) = event_bus.subscribe();
    for _ in 0..=SUBSCRIBER_QUEUE_CAPACITY {
        event_bus.publish(&Event::TabCreated(TabCreated { tab_id }));
    }
    assert_eq!(
        delivery_receiver.try_iter().count(),
        SUBSCRIBER_QUEUE_CAPACITY
    );
    let render_snapshot = build_alternating_grid_snapshot(ClientId::new(), 2, 2);
    assert!(event_bus.try_resync(subscriber_id, Box::new(render_snapshot.clone())));
    event_bus.unsubscribe(subscriber_id);
    let ending_notice = Arc::clone(event_bus.get_ending_notice());

    assert_eq!(
        read_frames_of_ending_session(
            "resync-then-frame",
            delivery_receiver,
            writer_handoff,
            &ending_notice,
            2
        ),
        vec![
            SessionFrame::Event(SessionEvent::Resync {
                dropped_event_count: 1
            }),
            SessionFrame::Painted(Box::new(build_first_received_painted_frame(
                &render_snapshot,
                &build_wire_frame(&render_snapshot)
            ))),
        ]
    );
}

// --- prepare and commit ---

/// The refusal [`IpcErrorCode::RequestFailed`] carrying `message`, as the
/// answer to request `request_id`.
fn build_request_failed_test_response(request_id: u64, message: &str) -> IpcResponse {
    IpcResponse {
        request_id: Some(request_id),
        answer_result: IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::RequestFailed,
            message: message.to_string(),
        }),
    }
}

#[test]
fn a_held_command_runs_on_its_commit_and_answers_under_the_commit_id() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_command_envelopes) =
        start_reporting_test_server("prepare-commit-runs");
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    let command_envelope = build_test_command_envelope();

    let prepare_response = exchange_request(
        &mut connection,
        2,
        IpcRequestKind::Prepare(Box::new(IpcRequestKind::SubmitCommand(Box::new(
            command_envelope.clone(),
        )))),
    );
    let commit_response = exchange_request(&mut connection, 3, IpcRequestKind::Commit);

    assert_eq!(prepare_response, build_prepared_response(2));
    assert_eq!(
        commit_response,
        IpcResponse {
            request_id: Some(3),
            answer_result: IpcResult::CommandResult(CommandResult::Ok {
                command_id: command_envelope.command_id,
                emitted_events: Vec::new(),
            }),
        }
    );
    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    assert_eq!(
        received_command_envelopes
            .try_iter()
            .map(|dispatched_command_envelope| dispatched_command_envelope.command_id)
            .collect::<Vec<CommandId>>(),
        vec![command_envelope.command_id]
    );
}

#[test]
fn a_held_command_never_runs_when_its_connection_ends_before_the_commit() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_command_envelopes) =
        start_reporting_test_server("prepare-connection-ends");
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);

    let prepare_response = exchange_request(
        &mut connection,
        2,
        IpcRequestKind::Prepare(Box::new(IpcRequestKind::SubmitCommand(Box::new(
            build_test_command_envelope(),
        )))),
    );
    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);

    assert_eq!(prepare_response, build_prepared_response(2));
    assert_eq!(
        received_command_envelopes
            .try_iter()
            .collect::<Vec<CommandEnvelope>>(),
        Vec::new()
    );
}

#[test]
fn a_ping_between_prepare_and_commit_gets_no_answer_and_keeps_the_held_command() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_command_envelopes) =
        start_reporting_test_server("prepare-ping");
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    let command_envelope = build_test_command_envelope();

    let prepare_response = exchange_request(
        &mut connection,
        2,
        IpcRequestKind::Prepare(Box::new(IpcRequestKind::SubmitCommand(Box::new(
            command_envelope.clone(),
        )))),
    );
    connection
        .send(&IpcRequest {
            request_id: 4,
            request_kind: IpcRequestKind::Ping,
        })
        .expect("send the ping");
    // The first frame after the ping is the Commit's answer.
    let commit_response = exchange_request(&mut connection, 3, IpcRequestKind::Commit);

    assert_eq!(prepare_response, build_prepared_response(2));
    assert_eq!(
        commit_response,
        IpcResponse {
            request_id: Some(3),
            answer_result: IpcResult::CommandResult(CommandResult::Ok {
                command_id: command_envelope.command_id,
                emitted_events: Vec::new(),
            }),
        }
    );
    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    assert_eq!(
        received_command_envelopes
            .try_iter()
            .map(|dispatched_command_envelope| dispatched_command_envelope.command_id)
            .collect::<Vec<CommandId>>(),
        vec![command_envelope.command_id]
    );
}

#[test]
fn a_second_prepare_replaces_the_held_command() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_command_envelopes) =
        start_reporting_test_server("prepare-replaced");
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    let first_command_envelope = build_test_command_envelope();
    let second_command_envelope = build_test_command_envelope();

    let first_prepare_response = exchange_request(
        &mut connection,
        2,
        IpcRequestKind::Prepare(Box::new(IpcRequestKind::SubmitCommand(Box::new(
            first_command_envelope,
        )))),
    );
    let second_prepare_response = exchange_request(
        &mut connection,
        3,
        IpcRequestKind::Prepare(Box::new(IpcRequestKind::SubmitCommand(Box::new(
            second_command_envelope.clone(),
        )))),
    );
    let commit_response = exchange_request(&mut connection, 4, IpcRequestKind::Commit);

    assert_eq!(first_prepare_response, build_prepared_response(2));
    assert_eq!(second_prepare_response, build_prepared_response(3));
    assert_eq!(
        commit_response,
        IpcResponse {
            request_id: Some(4),
            answer_result: IpcResult::CommandResult(CommandResult::Ok {
                command_id: second_command_envelope.command_id,
                emitted_events: Vec::new(),
            }),
        }
    );
    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    assert_eq!(
        received_command_envelopes
            .try_iter()
            .map(|dispatched_command_envelope| dispatched_command_envelope.command_id)
            .collect::<Vec<CommandId>>(),
        vec![second_command_envelope.command_id]
    );
}

#[test]
fn another_request_between_prepare_and_commit_drops_the_held_request() {
    let session_overview = build_overview_named("still-here");
    let (ipc_server, session_id, runtime_directory, dispatcher_thread) =
        start_restartable_test_server(
            "prepare-dropped",
            build_restart_check_answering(Ok(())),
            Some(session_overview.clone()),
        );
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);

    let prepare_response = exchange_request(
        &mut connection,
        2,
        IpcRequestKind::Prepare(Box::new(IpcRequestKind::Restart)),
    );
    let discovery_result = request_discovery(&mut connection, 3);
    let commit_response = exchange_request(&mut connection, 4, IpcRequestKind::Commit);

    assert_eq!(prepare_response, build_prepared_response(2));
    assert_eq!(discovery_result, IpcResult::Overview(session_overview));
    assert_eq!(
        commit_response,
        build_request_failed_test_response(4, "no request waits for a Commit on this connection")
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
}

#[test]
fn every_held_kind_without_prepare_a_kind_that_cannot_be_held_and_a_bare_commit_are_refused_before_the_dispatcher(
) {
    let runtime_directory = build_test_runtime_directory("prepare-refusals");
    let session_id = SessionId::new();
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let ipc_server = IpcServer::start(&runtime_directory, session_id, inbox_sender, None, None)
        .expect("start serving");
    let (mut frame_reader, mut frame_writer) =
        connect_greeted_connection(&runtime_directory, session_id).split();
    frame_reader.set_deadline(Some(Instant::now() + Duration::from_secs(10)));
    let refused_request_kinds = [
        (
            IpcRequestKind::SubmitCommand(Box::new(build_test_command_envelope())),
            "the session runs SubmitCommand requests only after Prepare and Commit",
        ),
        (
            build_attach_request(None, None).request_kind,
            "the session runs Attach requests only after Prepare and Commit",
        ),
        (
            IpcRequestKind::Restart,
            "the session runs Restart requests only after Prepare and Commit",
        ),
        (
            build_copy_request(PaneId::new(), b"x"),
            "the session runs ClipboardCopy requests only after Prepare and Commit",
        ),
        (
            IpcRequestKind::Prepare(Box::new(IpcRequestKind::Discovery)),
            "the session cannot prepare Discovery requests",
        ),
        (
            IpcRequestKind::Prepare(Box::new(IpcRequestKind::Prepare(Box::new(
                IpcRequestKind::Restart,
            )))),
            "the session cannot prepare Prepare requests",
        ),
        (
            IpcRequestKind::Prepare(Box::new(IpcRequestKind::Commit)),
            "the session cannot prepare Commit requests",
        ),
        (
            IpcRequestKind::Commit,
            "no request waits for a Commit on this connection",
        ),
    ];

    for (request_index, (request_kind, refusal_message)) in
        refused_request_kinds.into_iter().enumerate()
    {
        let request_id = request_index as u64 + 2;
        frame_writer
            .send(&IpcRequest {
                request_id,
                request_kind,
            })
            .expect("send the request");
        let refusal_response: IpcResponse = frame_reader.recv().expect("the refusal");

        assert_eq!(
            refusal_response,
            build_request_failed_test_response(request_id, refusal_message)
        );
    }
    assert_eq!(
        inbox_receiver.try_recv().err(),
        Some(mpsc::TryRecvError::Empty)
    );

    drop((frame_reader, frame_writer));
    ipc_server.shutdown();
    delete_test_directory(&runtime_directory);
}

#[test]
fn a_clipboard_copy_the_session_would_not_route_says_run_the_real_tool_and_drops_every_held_byte() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, request_reports) =
        start_clipboard_test_server("clipboard-prepare-unroutable", 3, false);
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    let pane_id = PaneId::new();

    assert_eq!(
        exchange_request(&mut connection, 2, build_copy_part_request(b"ab")),
        IpcResponse {
            request_id: Some(2),
            answer_result: IpcResult::ClipboardCopyPartTaken,
        }
    );
    assert_eq!(
        exchange_request(
            &mut connection,
            3,
            build_copy_prepare_request(pane_id, b"cd")
        ),
        build_copy_routed_response(3, true)
    );
    assert_eq!(
        exchange_request(&mut connection, 4, IpcRequestKind::Commit),
        build_request_failed_test_response(4, "no request waits for a Commit on this connection")
    );
    // The next copy counts only its own byte.
    assert_eq!(
        exchange_request(
            &mut connection,
            5,
            build_copy_prepare_request(pane_id, b"x")
        ),
        build_prepared_response(5)
    );
    assert_eq!(
        exchange_request(&mut connection, 6, IpcRequestKind::Commit),
        build_copy_routed_response(6, false)
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    assert_eq!(
        request_reports
            .try_iter()
            .collect::<Vec<ClipboardRequestReport>>(),
        vec![
            ClipboardRequestReport::CopyPart {
                held_copy_byte_count: 2
            },
            ClipboardRequestReport::CopyPrepare {
                pane_id,
                copied_byte_count: 4,
            },
            ClipboardRequestReport::CopyPrepare {
                pane_id,
                copied_byte_count: 1,
            },
            ClipboardRequestReport::Copy {
                pane_id,
                clipboard_targets: "c".to_string(),
                copied_bytes: b"x".to_vec(),
            },
        ]
    );
}

#[test]
fn a_request_between_a_clipboard_prepare_and_its_commit_drops_the_held_copy_and_its_parts() {
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, request_reports) =
        start_clipboard_test_server("clipboard-prepare-dropped", 8, false);
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    let pane_id = PaneId::new();

    assert_eq!(
        exchange_request(&mut connection, 2, build_copy_part_request(b"ab")),
        IpcResponse {
            request_id: Some(2),
            answer_result: IpcResult::ClipboardCopyPartTaken,
        }
    );
    assert_eq!(
        exchange_request(
            &mut connection,
            3,
            build_copy_prepare_request(pane_id, b"cd")
        ),
        build_prepared_response(3)
    );
    assert_eq!(
        exchange_request(&mut connection, 4, build_copy_part_request(b"ef")),
        IpcResponse {
            request_id: Some(4),
            answer_result: IpcResult::ClipboardCopyPartTaken,
        }
    );
    assert_eq!(
        exchange_request(
            &mut connection,
            5,
            build_copy_prepare_request(pane_id, b"x")
        ),
        build_prepared_response(5)
    );
    assert_eq!(
        exchange_request(&mut connection, 6, IpcRequestKind::Commit),
        build_copy_routed_response(6, false)
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    assert_eq!(
        request_reports
            .try_iter()
            .collect::<Vec<ClipboardRequestReport>>(),
        vec![
            ClipboardRequestReport::CopyPart {
                held_copy_byte_count: 2
            },
            ClipboardRequestReport::CopyPrepare {
                pane_id,
                copied_byte_count: 4,
            },
            ClipboardRequestReport::CopyPart {
                held_copy_byte_count: 2
            },
            ClipboardRequestReport::CopyPrepare {
                pane_id,
                copied_byte_count: 3,
            },
            ClipboardRequestReport::Copy {
                pane_id,
                clipboard_targets: "c".to_string(),
                copied_bytes: b"efx".to_vec(),
            },
        ]
    );
}

#[test]
fn a_held_clipboard_copy_is_refused_at_its_commit_once_a_second_hello_marks_the_connection_remote()
{
    let (ipc_server, session_id, runtime_directory, dispatcher_thread, request_reports) =
        start_clipboard_test_server("clipboard-prepare-remote", 8, false);
    let mut connection = connect_greeted_connection(&runtime_directory, session_id);
    let pane_id = PaneId::new();

    assert_eq!(
        exchange_request(
            &mut connection,
            2,
            build_copy_prepare_request(pane_id, b"hi")
        ),
        build_prepared_response(2)
    );
    let remote_hello_response = exchange_request(
        &mut connection,
        3,
        IpcRequestKind::Hello {
            minimum_protocol_version: MIN_PROTOCOL_VERSION,
            maximum_protocol_version: PROTOCOL_VERSION,
            connection_token: load_test_endpoint_file(&runtime_directory, session_id)
                .connection_token,
            is_remote: true,
        },
    );
    assert_eq!(
        remote_hello_response.answer_result,
        build_accepted_hello_result()
    );
    assert_eq!(
        exchange_request(&mut connection, 4, IpcRequestKind::Commit),
        build_clipboard_refusal_response(4)
    );

    drop(connection);
    stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    assert_eq!(
        request_reports
            .try_iter()
            .collect::<Vec<ClipboardRequestReport>>(),
        vec![ClipboardRequestReport::CopyPrepare {
            pane_id,
            copied_byte_count: 2,
        }]
    );
}

#[test]
fn a_prepare_or_a_commit_on_an_attached_connection_detaches_its_client_and_runs_nothing() {
    for (directory_tag, request_kind) in [
        (
            "attached-prepare",
            IpcRequestKind::Prepare(Box::new(IpcRequestKind::SubmitCommand(Box::new(
                build_test_command_envelope(),
            )))),
        ),
        ("attached-commit", IpcRequestKind::Commit),
    ] {
        let client_id = ClientId::new();
        let (ipc_server, session_id, runtime_directory, dispatcher_thread, received_runtime_events) =
            start_attachable_test_server(directory_tag, client_id);
        let mut connection = attach_test_client(&runtime_directory, session_id, client_id);

        connection
            .send(&IpcRequest {
                request_id: 3,
                request_kind,
            })
            .expect("send the request");

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
            "the stream ends after the goodbye, with no answer to the request",
        );

        drop(connection);
        stop_test_server(ipc_server, dispatcher_thread, &runtime_directory);
    }
}
