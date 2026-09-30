//! Tests for the outer terminal an attached client owns: the client built for
//! it, painting a frame, the cursor-style mapping, and the window title. A fake
//! PTY backend stands in for real children, and ratatui's `TestBackend` renders
//! into an in-memory buffer. Event conversion is tested here. Platform terminal
//! setup and the input reader need a TTY and are not tested here. Key decoding
//! is tested in `koshi-input`.

use super::*;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// [`paint_frame_with_images`] with image placeholders, no native image output,
/// no cell measurement and no placement preview.
fn paint_frame<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &Client,
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    frame_paint: &ViewerPaint,
    last_window_title: &mut String,
    last_cursor_style: &mut Option<CursorStyle>,
) -> Result<(), PaintError<B::Error>> {
    let mut image_output_state = ImageOutputState::from_output_kind(None);
    paint_frame_with_images(
        terminal,
        client,
        render_snapshot,
        committed_regions,
        frame_paint,
        ImageRenderMode::Placeholder,
        &mut image_output_state,
        None,
        last_window_title,
        last_cursor_style,
        None,
        None,
    )
    .map(|_| ())
}

/// Paint one frame, drawing the placement preview when `placement_display_snapshot`
/// or `placement_snapshot` names one, through
/// [`paint_frame_with_displayed_snapshot`].
#[allow(clippy::too_many_arguments)]
fn paint_frame_with_images<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &Client,
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    frame_paint: &ViewerPaint,
    image_mode: ImageRenderMode,
    image_output_state: &mut ImageOutputState,
    cell_size: Option<PixelCellSize>,
    last_window_title: &mut String,
    last_cursor_style: &mut Option<CursorStyle>,
    placement_snapshot: Option<&PlacementSnapshot>,
    placement_display_snapshot: Option<&PlacementSnapshot>,
) -> Result<bool, PaintError<B::Error>> {
    let placement_render_snapshot =
        placement_display_snapshot
            .or(placement_snapshot)
            .and_then(|placement_display_snapshot| {
                build_placement_render_snapshot(render_snapshot, placement_display_snapshot)
            });
    let displayed_render_snapshot = placement_render_snapshot
        .as_ref()
        .unwrap_or(render_snapshot);
    paint_frame_with_displayed_snapshot(
        terminal,
        client,
        displayed_render_snapshot,
        committed_regions,
        frame_paint,
        image_mode,
        image_output_state,
        cell_size,
        last_window_title,
        last_cursor_style,
        placement_snapshot,
        placement_display_snapshot,
    )
}

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};

use koshi_config::layer::{PartialColorPalette, PartialKeybindingsConfig, PartialThemeConfig};
use koshi_config::types::RgbColor;
use koshi_core::command::{
    Command, CommandEnvelope, CommandSource, PanePlacementAnchor, PanePlacementTarget,
};
use koshi_core::geometry::{
    Direction, PaneArea, Point as CorePoint, Rect as CoreRect, Size, SplitDirection,
};
use koshi_core::ids::{CommandId, PaneId, SessionId, TabId};
use koshi_core::key::{
    BindingModifierFlags, Key, KeyChord, KeyEventKind, KeyIdentity, KeyModifierFlags,
};
use koshi_core::lock::LockMode;
use koshi_core::process::PtySize;
use koshi_input::host::{GraphicAttributeError, GraphicAttributeReply, KeyCode};
use koshi_ipc::protocol::WireMouseAction;
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::{solve_layout_with_mode, PaneSizing};
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pty::backend::state::PtyBackend;
use koshi_renderer::snapshot::{
    ClientSnapshot, CommittedRegions, GridView, PaneSlot, PlacementClientSnapshot,
    PlacementPaneSnapshot, PlacementSnapshot, PlacementTabSnapshot, RenderSnapshot, TabSnapshot,
    ViewerChrome,
};
use koshi_renderer::{build_image_cell_snapshot, build_image_paints};
use koshi_runtime::runtime::pty_inbox::InboxSink;
use koshi_runtime::server::Server;
use koshi_terminal::engine::TerminalEngine;
use koshi_terminal::grid::state::{Cell, Grid};
use koshi_terminal::style::Style as TerminalStyle;
use koshi_test_support::fake_pty::FakePtyBackend;

use koshi_link::config::LoadedConfig;

use crate::tests::TEST_VIEWPORT_SIZE;

fn build_committed_regions(viewport_size: Size) -> CommittedRegions {
    CommittedRegions::build_core(viewport_size, 0)
}

fn build_terminal_probe(graphics_support: GraphicsSupport) -> TerminalProbe {
    TerminalProbe {
        graphics_support,
        cell_size: None,
    }
}

struct ProbeSource {
    pending_events: VecDeque<Event>,
}

impl reader::EventSource for ProbeSource {
    fn try_read_event(&mut self, _timeout: Option<Duration>) -> io::Result<Option<Event>> {
        Ok(self.pending_events.pop_front())
    }
}

fn build_probe_reader(probe_events: impl IntoIterator<Item = Event>) -> InputReader<ProbeSource> {
    InputReader::from_event_source_for_tests(ProbeSource {
        pending_events: probe_events.into_iter().collect(),
    })
}

/// A bootstrapped server over a fake backend, with its client id and sole pane id.
fn build_test_server_with_pane() -> (Server, ClientId, PaneId) {
    let (event_sender, event_receiver) = mpsc::channel();
    let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(event_sender),
    )));
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let mut server = Server::from_runtime_parts(pty_backend, event_receiver);
    let client_id = server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake_pty_backend.list_spawned_pane_ids()[0];
    (server, client_id, pane_id)
}

/// A client half for `client_id`, subscribed to `server`'s events, built the
/// way the launch builds it — through [`build_client_with_loaded_config`].
fn build_test_client(server: &mut Server, client_id: ClientId) -> Client {
    build_test_client_with_config(server, client_id, LoadedConfig::default())
}

/// A client half for `client_id`, subscribed to `server`'s events, built from
/// `loaded_config`.
fn build_test_client_with_config(
    server: &mut Server,
    client_id: ClientId,
    loaded_config: LoadedConfig,
) -> Client {
    let frame_delivery_receiver = server.subscribe(client_id);
    build_client_with_loaded_config(
        client_id,
        TEST_VIEWPORT_SIZE,
        frame_delivery_receiver,
        TerminalCleanupGuard::new(),
        loaded_config,
    )
}

/// Flatten the whole rendered terminal screen to a string for substring assertions.
fn serialize_terminal_screen_text(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|buffer_cell| buffer_cell.symbol())
        .collect()
}

struct FailOnWrite {
    failing_write_index: usize,
    write_count: usize,
    written_bytes: Vec<u8>,
}

impl Write for FailOnWrite {
    fn write(&mut self, write_bytes: &[u8]) -> io::Result<usize> {
        let write_index = self.write_count;
        self.write_count += 1;
        if write_index == self.failing_write_index {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "test writer failed",
            ));
        }
        self.written_bytes.extend_from_slice(write_bytes);
        Ok(write_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct FailInsideSequence {
    sequence_bytes: &'static [u8],
    split_byte_count: usize,
    should_fail_next_write: bool,
    has_failed: bool,
    written_bytes: Vec<u8>,
}

impl Write for FailInsideSequence {
    fn write(&mut self, write_bytes: &[u8]) -> io::Result<usize> {
        if self.should_fail_next_write {
            self.should_fail_next_write = false;
            self.has_failed = true;
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "test writer failed inside a control sequence",
            ));
        }
        if !self.has_failed {
            if let Some(sequence_start_byte_index) = write_bytes
                .windows(self.sequence_bytes.len())
                .position(|byte_window| byte_window == self.sequence_bytes)
            {
                let written_byte_count = sequence_start_byte_index + self.split_byte_count;
                self.written_bytes
                    .extend_from_slice(&write_bytes[..written_byte_count]);
                self.should_fail_next_write = true;
                return Ok(written_byte_count);
            }
        }
        self.written_bytes.extend_from_slice(write_bytes);
        Ok(write_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The frame this client is owed, as the session composes it.
fn build_render_snapshot(server: &Server, client_id: ClientId) -> RenderSnapshot {
    server.build_snapshot(client_id).expect("snapshot")
}

#[derive(Clone, Default)]
struct ImageTraceWriter(Arc<std::sync::Mutex<Vec<u8>>>);

struct ImageTraceBackend(ratatui::backend::CrosstermBackend<ImageTraceWriter>);

impl Backend for ImageTraceBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, cell_updates: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.0.draw(cell_updates)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.0.hide_cursor()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.0.show_cursor()
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        panic!("painting must not ask the host terminal for its cursor position");
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.0.set_cursor_position(position)
    }
    fn clear(&mut self) -> io::Result<()> {
        panic!("painting must not clear through the backend");
    }
    fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> io::Result<()> {
        self.0.clear_region(clear_type)
    }
    fn size(&self) -> io::Result<ratatui::layout::Size> {
        Ok(ratatui::layout::Size::new(80, 24))
    }
    fn window_size(&mut self) -> io::Result<ratatui::backend::WindowSize> {
        Ok(ratatui::backend::WindowSize {
            columns_rows: self.size()?,
            pixels: self.size()?,
        })
    }
    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.0)
    }
}

impl Write for ImageTraceWriter {
    fn write(&mut self, write_bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("trace lock")
            .extend_from_slice(write_bytes);
        Ok(write_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn build_opaque_image_pixels() -> Vec<u8> {
    [
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 255, 255],
    ]
    .into_iter()
    .flat_map(|rgba_pixel| rgba_pixel.repeat(4))
    .collect()
}

fn build_second_opaque_image_pixels() -> Vec<u8> {
    [
        [255, 255, 0, 255],
        [0, 255, 255, 255],
        [255, 0, 255, 255],
        [0, 0, 0, 255],
    ]
    .into_iter()
    .flat_map(|rgba_pixel| rgba_pixel.repeat(4))
    .collect()
}

fn build_protocol_image_input(
    graphics_protocol: koshi_terminal::graphics::GraphicsProtocol,
    rgba_pixel_bytes: Vec<u8>,
    image_content_id: u32,
) -> Vec<u8> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    let decoded_image = koshi_image::DecodedImage {
        pixel_width: 4,
        pixel_height: 4,
        rgba_bytes: rgba_pixel_bytes,
    };
    match graphics_protocol {
        koshi_terminal::graphics::GraphicsProtocol::Kitty => format!(
            "\x1b_Ga=T,f=32,s=4,v=4,i={image_content_id},c=4,r=4,C=1,q=2;{}\x1b\\",
            STANDARD.encode(decoded_image.rgba_bytes)
        )
        .into_bytes(),
        koshi_terminal::graphics::GraphicsProtocol::Iterm2 => {
            let mut iterm_encoder = koshi_iterm::ItermEncoder::from_image(
                &decoded_image,
                koshi_iterm::ItermOutputOptions::from_cell_dimensions(4, 4)
                    .expect("image dimensions"),
            )
            .expect("iTerm image encoding");
            let mut iterm_output_bytes = Vec::new();
            while let Some(iterm_packet) = iterm_encoder.take_next_packet() {
                iterm_output_bytes.extend_from_slice(iterm_packet);
            }
            iterm_output_bytes
        }
        koshi_terminal::graphics::GraphicsProtocol::Sixel => {
            let mut sixel_encoder =
                koshi_sixel::SixelEncoder::from_image(Arc::new(decoded_image), [0, 0, 0])
                    .expect("Sixel image encoding");
            let mut sixel_output_bytes = b"\x1b[?80l".to_vec();
            while let Some(sixel_chunk) = sixel_encoder
                .take_next_chunk(koshi_sixel::MAX_SIXEL_CHUNK_BYTE_COUNT)
                .expect("Sixel image output")
            {
                sixel_output_bytes.extend_from_slice(sixel_chunk);
            }
            sixel_output_bytes
        }
    }
}

fn build_two_image_input(graphics_protocol: koshi_terminal::graphics::GraphicsProtocol) -> Vec<u8> {
    let mut terminal_input_bytes = b"\x1b[22;1Hscroll-test\x1b[2;2H".to_vec();
    terminal_input_bytes.extend_from_slice(&build_protocol_image_input(
        graphics_protocol,
        build_opaque_image_pixels(),
        42,
    ));
    terminal_input_bytes.extend_from_slice(b"\x1b[4;4H");
    terminal_input_bytes.extend_from_slice(&build_protocol_image_input(
        graphics_protocol,
        build_second_opaque_image_pixels(),
        43,
    ));
    terminal_input_bytes
}

fn build_source_image_pixel_map(render_snapshot: &RenderSnapshot) -> BTreeMap<(u16, u16), [u8; 4]> {
    let mut image_pixel_by_position = BTreeMap::new();
    for image_paint in build_image_paints(
        render_snapshot,
        &build_committed_regions(TEST_VIEWPORT_SIZE),
        Rect::new(0, 0, 80, 24),
    ) {
        for target_row_offset in 0..image_paint.target_area.height {
            for target_column_offset in 0..image_paint.target_area.width {
                let source_pixel_row = image_paint.source_rect.pixel_y
                    + u32::from(target_row_offset) * image_paint.source_rect.pixel_height
                        / u32::from(image_paint.target_area.height);
                let source_pixel_column = image_paint.source_rect.pixel_x
                    + u32::from(target_column_offset) * image_paint.source_rect.pixel_width
                        / u32::from(image_paint.target_area.width);
                let pixel_start_byte_index = ((source_pixel_row
                    * image_paint.image_record.image.pixel_width
                    + source_pixel_column)
                    * 4) as usize;
                let rgba_pixel = image_paint.image_record.image.rgba_bytes
                    [pixel_start_byte_index..pixel_start_byte_index + 4]
                    .try_into()
                    .expect("source pixel");
                image_pixel_by_position.insert(
                    (
                        image_paint.target_area.y + target_row_offset,
                        image_paint.target_area.x + target_column_offset,
                    ),
                    rgba_pixel,
                );
            }
        }
    }
    image_pixel_by_position
}

fn build_terminal_image_pixel_map(
    terminal_engine: &TerminalEngine,
) -> BTreeMap<(u16, u16), [u8; 4]> {
    let image_placements = terminal_engine
        .get_terminal_state()
        .list_image_placements_for_view(0);
    let mut image_pixel_by_position = BTreeMap::new();
    for image_placement in &image_placements {
        let image_record = image_placement.create_render_image_record();
        let (source_pixel_x, source_pixel_y, source_pixel_width, source_pixel_height) =
            image_record
                .compute_source_rect()
                .expect("outer source rectangle");
        let image_geometry = image_placement.get_image_geometry();
        for image_row_index in 0..image_placement.get_image_cell_dimensions().0 {
            for image_column_index in 0..image_placement.get_image_cell_dimensions().1 {
                let image_pixel_y = source_pixel_y
                    + u32::from(image_geometry.cell_offset.row + image_row_index)
                        * source_pixel_height
                        / u32::from(image_geometry.full_size.row_count);
                let image_pixel_x = source_pixel_x
                    + u32::from(image_geometry.cell_offset.column + image_column_index)
                        * source_pixel_width
                        / u32::from(image_geometry.full_size.column_count);
                let pixel_start_byte_index =
                    ((image_pixel_y * image_record.image.pixel_width + image_pixel_x) * 4) as usize;
                let rgba_pixel = image_record.image.rgba_bytes
                    [pixel_start_byte_index..pixel_start_byte_index + 4]
                    .try_into()
                    .expect("outer pixel");
                image_pixel_by_position.insert(
                    (
                        image_placement.get_image_anchor().0 + image_row_index,
                        image_placement.get_image_anchor().1 + image_column_index,
                    ),
                    rgba_pixel,
                );
            }
        }
    }
    image_pixel_by_position
}

fn build_opaque_image_input(
    graphics_protocol: koshi_terminal::graphics::GraphicsProtocol,
) -> Vec<u8> {
    use koshi_terminal::graphics::GraphicsProtocol;
    match graphics_protocol {
        GraphicsProtocol::Kitty => b"\x1b_Ga=T,f=32,s=4,v=4,i=42,c=4,r=4,C=1,q=2;/wAA//8AAP//AAD//wAA/wD/AP8A/wD/AP8A/wD/AP8AAP//AAD//wAA//8AAP///////////////////////w==\x1b\\".to_vec(),
        GraphicsProtocol::Sixel => b"\x1b[?80l\x1bP0;1q\"1;1;4;4#1;2;100;0;0#1@@@@$#2;2;0;100;0#2AAAA$#3;2;0;0;100#3CCCC$#4;2;100;100;100#4GGGG\x1b\\".to_vec(),
        GraphicsProtocol::Iterm2 => {
            let decoded_image = koshi_image::DecodedImage { pixel_width: 4, pixel_height: 4, rgba_bytes: build_opaque_image_pixels() };
            let mut iterm_encoder = koshi_iterm::ItermEncoder::from_image(
                &decoded_image,
                koshi_iterm::ItermOutputOptions::from_cell_dimensions(4, 4).unwrap(),
            )
            .unwrap();
            let mut iterm_output_bytes = Vec::new();
            while let Some(iterm_packet) = iterm_encoder.take_next_packet() {
                iterm_output_bytes.extend_from_slice(iterm_packet);
            }
            iterm_output_bytes
        }
    }
}

#[derive(Default)]
struct ImageWireContentIds {
    wire_image_content_id_by_image_content_id: HashMap<u64, (usize, u64)>,
    next_wire_image_content_id: u64,
}

impl ImageWireContentIds {
    fn retain_visible_image_content_ids(&mut self, render_snapshot: &RenderSnapshot) {
        let visible_image_record_addresses_by_content_id = render_snapshot
            .pane_snapshots
            .iter()
            .flat_map(|pane_snapshot| &pane_snapshot.image_placement_snapshots)
            .filter_map(|image_placement| {
                image_placement.clone_image_record().map(|image_record| {
                    (
                        image_placement.get_image_content_id(),
                        Arc::as_ptr(&image_record) as usize,
                    )
                })
            })
            .collect::<HashMap<_, _>>();
        self.wire_image_content_id_by_image_content_id.retain(
            |image_content_id, (image_record_address, _)| {
                visible_image_record_addresses_by_content_id.get(image_content_id)
                    == Some(image_record_address)
            },
        );
    }

    fn get_or_create_wire_image_content_id(
        &mut self,
        image_content_id: u64,
        image_record_address: usize,
    ) -> u64 {
        if let Some(&(existing_image_record_address, wire_image_content_id)) = self
            .wire_image_content_id_by_image_content_id
            .get(&image_content_id)
        {
            assert_eq!(existing_image_record_address, image_record_address);
            return wire_image_content_id;
        }
        self.next_wire_image_content_id = self.next_wire_image_content_id.saturating_add(1);
        let wire_image_content_id = self.next_wire_image_content_id;
        self.wire_image_content_id_by_image_content_id.insert(
            image_content_id,
            (image_record_address, wire_image_content_id),
        );
        wire_image_content_id
    }
}

fn round_trip_image_frame_through_wire(
    render_snapshot: &RenderSnapshot,
    image_cache: &mut crate::attach::paint::ImageCache,
    image_wire_content_ids: &mut ImageWireContentIds,
) -> RenderSnapshot {
    use koshi_ipc::frame::{FrameImageChunk, FrameImageTransfer, PaintedFrame};
    image_wire_content_ids.retain_visible_image_content_ids(render_snapshot);
    let mut wire_frame = koshi_runtime::runtime::frame::build_wire_frame(render_snapshot);
    for (source_pane_snapshot, wire_pane_snapshot) in render_snapshot
        .pane_snapshots
        .iter()
        .zip(&mut wire_frame.pane_snapshots)
    {
        for (source_image_placement, wire_image_placement) in source_pane_snapshot
            .image_placement_snapshots
            .iter()
            .zip(&mut wire_pane_snapshot.image_placement_snapshots)
        {
            let image_record_address = source_image_placement
                .clone_image_record()
                .map_or(0, |image_record| Arc::as_ptr(&image_record) as usize);
            wire_image_placement.image_content_id = image_wire_content_ids
                .get_or_create_wire_image_content_id(
                    source_image_placement.get_image_content_id(),
                    image_record_address,
                );
        }
    }
    let wire_frame: PaintedFrame =
        serde_json::from_slice(&serde_json::to_vec(&wire_frame).unwrap()).unwrap();
    let mut resolved_render_snapshot = image_cache
        .adopt_painted_frame(Box::new(wire_frame.clone()))
        .unwrap();
    let mut transferred_image_content_ids = HashSet::new();
    for (source_pane_snapshot, wire_pane_snapshot) in render_snapshot
        .pane_snapshots
        .iter()
        .zip(&wire_frame.pane_snapshots)
    {
        for (source_image_placement, wire_image_placement) in source_pane_snapshot
            .image_placement_snapshots
            .iter()
            .zip(&wire_pane_snapshot.image_placement_snapshots)
        {
            if !transferred_image_content_ids.insert(wire_image_placement.image_content_id) {
                continue;
            }
            let image_record = source_image_placement
                .get_image_record()
                .expect("source pixels");
            let image_transfer = FrameImageTransfer {
                image_content_id: wire_image_placement.image_content_id,
                image_record: wire_image_placement
                    .image_record
                    .clone()
                    .expect("wire metadata"),
                image_byte_count: image_record.image.rgba_bytes.len() as u64,
            };
            match image_cache.start_image_transfer(
                serde_json::from_slice(&serde_json::to_vec(&image_transfer).unwrap()).unwrap(),
            ) {
                Ok(()) => {}
                Err(crate::attach::paint::ImageAssemblyError::TransferAlreadyComplete {
                    image_content_id,
                }) => {
                    assert_eq!(image_content_id, wire_image_placement.image_content_id);
                    continue;
                }
                Err(transfer_error) => panic!("image transfer failed to start: {transfer_error}"),
            }
            let image_chunk = FrameImageChunk {
                image_transfer_id: wire_image_placement.image_content_id,
                byte_offset: 0,
                is_last: true,
                chunk_bytes: image_record.image.rgba_bytes.clone(),
            };
            if let Some(completed_render_snapshot) = image_cache
                .accept_image_chunk(
                    serde_json::from_slice(&serde_json::to_vec(&image_chunk).unwrap()).unwrap(),
                )
                .unwrap()
            {
                resolved_render_snapshot = Some(completed_render_snapshot);
            }
        }
    }
    resolved_render_snapshot.expect("all required image records were transferred")
}

#[test]
fn all_input_and_output_protocols_preserve_opaque_pixels_through_scrolling() {
    assert_opaque_protocol_matrix(true);
}

#[test]
fn all_output_protocols_match_source_image_coverage_after_text_overwrite() {
    use koshi_terminal::graphics::GraphicsProtocol;
    for image_protocol in [
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Iterm2,
        GraphicsProtocol::Sixel,
    ] {
        let (mut server, client_id, pane_id) = build_test_server_with_pane();
        let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
            client_id,
            cell_size: PixelCellSize::from_pixel_dimensions(1, 1).unwrap(),
        });
        let mut output_bytes = b"A\x1b[1;1H".to_vec();
        output_bytes.extend_from_slice(&build_opaque_image_input(image_protocol));
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes,
        });
        let initial_render_snapshot = build_render_snapshot(&server, client_id);
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: b"\x1b[1;1HB".to_vec(),
        });
        let changed_render_snapshot = build_render_snapshot(&server, client_id);
        let mut overwritten_image_pixels = build_opaque_image_pixels();
        if image_protocol != GraphicsProtocol::Kitty {
            overwritten_image_pixels[..4].fill(0);
        }
        let client = build_test_client(&mut server, client_id);
        assert_image_trace_output(
            &client,
            image_protocol,
            &[initial_render_snapshot, changed_render_snapshot],
            &[
                (0, 4, build_opaque_image_pixels()),
                (0, 4, overwritten_image_pixels),
            ],
            true,
        );
    }
}

#[test]
fn a_text_repaint_under_an_image_rewrites_only_cell_bound_pixels() {
    use koshi_terminal::graphics::GraphicsProtocol;

    for image_protocol in [
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Iterm2,
        GraphicsProtocol::Sixel,
    ] {
        let (mut server, client_id, pane_id) = build_test_server_with_pane();
        let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
            client_id,
            cell_size: PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size"),
        });
        let mut image_input_bytes = b"\x1b[1;1H".to_vec();
        image_input_bytes.extend_from_slice(&build_opaque_image_input(image_protocol));
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: image_input_bytes,
        });
        let initial_render_snapshot = build_render_snapshot(&server, client_id);
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: b"\x1b[1;1HB".to_vec(),
        });
        let repainted_render_snapshot = build_render_snapshot(&server, client_id);
        let client = build_test_client(&mut server, client_id);

        for graphics_support in [
            GraphicsSupport::Kitty,
            GraphicsSupport::Iterm,
            GraphicsSupport::Sixel {
                palette_color_count: 256,
                maximum_pixel_width: None,
                maximum_pixel_height: None,
            },
        ] {
            let mut writer = ImageTraceWriter::default();
            let backend = ImageTraceBackend(ratatui::backend::CrosstermBackend::new(
                ImageTraceWriter::default(),
            ));
            let mut terminal = Terminal::with_options(
                backend,
                ratatui::TerminalOptions {
                    viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 80, 24)),
                },
            )
            .expect("test terminal");
            let mut image_output_state =
                ImageOutputState::from_output_kind(ImageOutputKind::from_support(graphics_support));
            let mut image_cache = crate::attach::paint::ImageCache::new();
            let mut image_wire_content_ids = ImageWireContentIds::default();

            for (stage_index, painted_frame) in
                [&initial_render_snapshot, &repainted_render_snapshot]
                    .into_iter()
                    .enumerate()
            {
                let render_snapshot = round_trip_image_frame_through_wire(
                    painted_frame,
                    &mut image_cache,
                    &mut image_wire_content_ids,
                );
                let settle_deadline = Instant::now() + Duration::from_secs(5);
                let mut stage_output_bytes = Vec::new();
                loop {
                    let is_frame_committed = paint_frame_with_writer(
                        &mut writer,
                        &mut terminal,
                        &client,
                        &render_snapshot,
                        &build_committed_regions(TEST_VIEWPORT_SIZE),
                        &ViewerPaint::from_client(
                            &client,
                            render_snapshot.client_snapshot.active_tab_id,
                            &render_snapshot,
                        ),
                        graphics_support.get_image_render_mode(),
                        &mut image_output_state,
                        Some(PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size")),
                        &mut String::new(),
                        &mut None,
                        None,
                        None,
                    )
                    .expect("native frame");
                    stage_output_bytes.extend_from_slice(&std::mem::take(
                        &mut *writer.0.lock().expect("trace lock"),
                    ));
                    if is_frame_committed && !image_output_state.is_work_pending() {
                        break;
                    }
                    assert!(
                        Instant::now() < settle_deadline,
                        "{image_protocol:?} -> {graphics_support:?}, stage {stage_index} did not settle"
                    );
                    std::thread::sleep(crate::tests::TEST_POLL_INTERVAL_DURATION);
                }

                let has_output_marker = |marker: &[u8]| {
                    stage_output_bytes
                        .windows(marker.len())
                        .any(|window_bytes| window_bytes == marker)
                };
                let encoded_output_text = String::from_utf8_lossy(&stage_output_bytes);
                match graphics_support {
                    // Kitty pixels transmit once. A Kitty-source image keeps
                    // its cells under the text and is not placed again. An
                    // iTerm2 or Sixel source loses the overwritten cell; its
                    // placement geometry changes and it is placed again.
                    GraphicsSupport::Kitty => {
                        assert_eq!(
                            has_output_marker(b"\x1b_Ga=t"),
                            stage_index == 0,
                            "{image_protocol:?} -> Kitty, stage {stage_index} pixel transmit, bytes: {encoded_output_text:?}"
                        );
                        assert_eq!(
                            has_output_marker(b"\x1b_Ga=p"),
                            stage_index == 0 || image_protocol != GraphicsProtocol::Kitty,
                            "{image_protocol:?} -> Kitty, stage {stage_index} placement, bytes: {encoded_output_text:?}"
                        );
                    }
                    // A text repaint under an iTerm2 or Sixel image writes the
                    // image again.
                    GraphicsSupport::Iterm => assert!(
                        has_output_marker(b"\x1b]1337;File="),
                        "{image_protocol:?} -> Iterm, stage {stage_index} did not emit the image: {encoded_output_text:?}"
                    ),
                    GraphicsSupport::Sixel { .. } => assert!(
                        has_output_marker(b"\x1bP"),
                        "{image_protocol:?} -> Sixel, stage {stage_index} did not emit the image: {encoded_output_text:?}"
                    ),
                    GraphicsSupport::Unsupported => unreachable!(),
                }
            }
        }
    }
}

#[test]
fn pi_auto_scrollbar_frames_preserve_pixels_in_every_output_protocol() {
    assert_pi_scrollbar_output(true);
}

#[test]
fn pi_hidden_scrollbar_frames_preserve_pixels_in_every_output_protocol() {
    assert_pi_scrollbar_output(false);
}

fn assert_pi_scrollbar_output(has_scrollbar: bool) {
    use koshi_terminal::graphics::GraphicsProtocol;
    let (mut server, client_id, pane_id) = build_test_server_with_pane();
    let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
        client_id,
        cell_size: PixelCellSize::from_pixel_dimensions(1, 1).unwrap(),
    });
    let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
        pane_id,
        output_bytes: b"\x1b[?1049h".to_vec(),
    });
    let kitty_upload_text = String::from_utf8(build_opaque_image_input(GraphicsProtocol::Kitty))
        .unwrap()
        .replace("c=4,r=4,C=1", "c=4,C=1,y=3,h=1,r=1");
    let mut render_snapshots = Vec::new();
    let image_row_stages: [(u16, u16, u16); 5] =
        [(1, 1, 3), (1, 3, 1), (2, 4, 0), (1, 3, 1), (1, 1, 3)];
    let mut expected_frame_pixel_rows = Vec::new();
    for (stage_index, (image_row_index, image_row_count, source_pixel_row_index)) in
        image_row_stages.into_iter().enumerate()
    {
        let mut terminal_input_text = String::from("\x1b[?2026h");
        if stage_index != 0 {
            terminal_input_text.push_str("\x1b_Ga=d,d=a,q=2\x1b\\");
        }
        for terminal_row_index in 1..=10 {
            terminal_input_text.push_str(&format!("\x1b[{terminal_row_index};1H\x1b[2K"));
            if terminal_row_index == image_row_index {
                if stage_index == 0 {
                    terminal_input_text.push_str(&kitty_upload_text);
                } else if image_row_count == 4 {
                    terminal_input_text.push_str("\x1b_Ga=p,q=2,i=42,c=4,r=4,C=1\x1b\\");
                } else {
                    terminal_input_text.push_str(&format!(
                        "\x1b_Ga=p,q=2,i=42,c=4,C=1,y={source_pixel_row_index},h={image_row_count},r={image_row_count}\x1b\\"
                    ));
                }
            } else {
                let terminal_row_text = if terminal_row_index < image_row_index {
                    "before".to_owned()
                } else if terminal_row_index < image_row_index + image_row_count {
                    String::new()
                } else {
                    format!(
                        "after{}",
                        terminal_row_index - image_row_index - image_row_count
                    )
                };
                if has_scrollbar && image_row_count != 1 {
                    let scrollbar_character = if terminal_row_index == 3 {
                        '┃'
                    } else {
                        '│'
                    };
                    terminal_input_text.push_str(&format!(
                        "{terminal_row_text:19}\x1b[90m{scrollbar_character}\x1b[39m"
                    ));
                } else {
                    terminal_input_text.push_str(&terminal_row_text);
                }
                terminal_input_text.push_str("\x1b[0m\x1b]8;;\x1b\\");
            }
        }
        terminal_input_text.push_str("\x1b[?2026l");
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: terminal_input_text.into_bytes(),
        });
        render_snapshots.push(build_render_snapshot(&server, client_id));
        expected_frame_pixel_rows.push((
            image_row_index - 1,
            image_row_count,
            build_opaque_image_pixels()[usize::from(source_pixel_row_index) * 16..].to_vec(),
        ));
    }
    let client = build_test_client(&mut server, client_id);
    assert_image_trace_output(
        &client,
        GraphicsProtocol::Kitty,
        &render_snapshots,
        &expected_frame_pixel_rows,
        true,
    );
}

#[test]
fn utf8_border_before_kitty_upload_preserves_pixels() {
    for border_prefix in ["", "┐"] {
        let mut terminal = TerminalEngine::from_pty_size(PtySize {
            column_count: 20,
            row_count: 10,
        });
        terminal.set_cell_size(PixelCellSize::from_pixel_dimensions(1, 1).unwrap());
        let mut terminal_input_bytes = border_prefix.as_bytes().to_vec();
        terminal_input_bytes.extend_from_slice(b"\x1b[1;1H");
        terminal_input_bytes.extend_from_slice(&build_opaque_image_input(
            koshi_terminal::graphics::GraphicsProtocol::Kitty,
        ));
        let _ = terminal.process_pty_output(&terminal_input_bytes);
        let placement_pixels = terminal
            .get_terminal_state()
            .list_image_placements_for_view(0)
            .into_iter()
            .map(|image_placement| image_placement.get_image_record().image.rgba_bytes.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            placement_pixels,
            [build_opaque_image_pixels()],
            "prefix {border_prefix:?}, events {:?}",
            terminal.take_graphics_events()
        );
    }
}

#[test]
fn native_graphics_without_text_preserve_opaque_pixels_through_scrolling() {
    assert_opaque_protocol_matrix(false);
}

#[test]
fn two_images_survive_partial_full_and_reverse_scrolling_for_every_protocol_pair() {
    use koshi_terminal::graphics::GraphicsProtocol;

    for image_protocol in [
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Iterm2,
        GraphicsProtocol::Sixel,
    ] {
        let (mut server, client_id, pane_id) = build_test_server_with_pane();
        let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
            client_id,
            cell_size: PixelCellSize::from_pixel_dimensions(1, 1).unwrap(),
        });
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: build_two_image_input(image_protocol),
        });
        let mut render_snapshots = vec![build_render_snapshot(&server, client_id)];

        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: b"\x1b[3S".to_vec(),
        });
        render_snapshots.push(build_render_snapshot(&server, client_id));

        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: b"\x1b[5S".to_vec(),
        });
        render_snapshots.push(build_render_snapshot(&server, client_id));

        let _ = server.handle_runtime_event(RuntimeEvent::ClientMouse {
            client_id,
            request_id: 1,
            mouse_actions: vec![WireMouseAction::Scroll {
                pane_id,
                is_scrolling_up: true,
                scroll_line_count: 8,
            }],
        });
        render_snapshots.push(build_render_snapshot(&server, client_id));

        let _ = server.handle_runtime_event(RuntimeEvent::ClientMouse {
            client_id,
            request_id: 2,
            mouse_actions: vec![WireMouseAction::Scroll {
                pane_id,
                is_scrolling_up: false,
                scroll_line_count: 8,
            }],
        });
        render_snapshots.push(build_render_snapshot(&server, client_id));

        let expected_frame_pixel_maps = render_snapshots
            .iter()
            .map(build_source_image_pixel_map)
            .collect::<Vec<_>>();
        assert!(
            build_image_paints(
                &render_snapshots[0],
                &build_committed_regions(TEST_VIEWPORT_SIZE),
                Rect::new(0, 0, 80, 24)
            )
            .len()
                >= 2,
            "source {image_protocol:?} must retain both image portions in the first frame"
        );
        assert!(
            expected_frame_pixel_maps[0]
                .values()
                .any(|rgba_pixel| *rgba_pixel == [255, 0, 0, 255])
                && expected_frame_pixel_maps[0]
                    .values()
                    .any(|rgba_pixel| *rgba_pixel == [255, 255, 0, 255]),
            "source {image_protocol:?} must expose distinct pixels from both images"
        );

        let client = build_test_client(&mut server, client_id);
        assert_two_image_trace_output(
            &client,
            image_protocol,
            &render_snapshots,
            &expected_frame_pixel_maps,
        );
    }
}

#[test]
fn pane_placement_preview_clears_native_images_from_the_softened_pane() {
    let (mut server, client_id, pane_id) = build_test_server_with_pane();
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
        client_id,
        cell_size,
    });
    let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
        pane_id,
        output_bytes: build_opaque_image_input(koshi_terminal::graphics::GraphicsProtocol::Kitty),
    });
    let render_snapshot = build_render_snapshot(&server, client_id);
    let client = build_test_client(&mut server, client_id);
    let committed_regions = build_committed_regions(TEST_VIEWPORT_SIZE);

    for graphics_support in [
        GraphicsSupport::Kitty,
        GraphicsSupport::Iterm,
        GraphicsSupport::Sixel {
            palette_color_count: 256,
            maximum_pixel_width: None,
            maximum_pixel_height: None,
        },
    ] {
        let mut writer = ImageTraceWriter::default();
        let backend = ImageTraceBackend(ratatui::backend::CrosstermBackend::new(writer.clone()));
        let mut terminal = Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .expect("test terminal");
        let mut image_output_state =
            ImageOutputState::from_output_kind(ImageOutputKind::from_support(graphics_support));
        let mut terminal_engine = TerminalEngine::from_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        });
        terminal_engine.set_cell_size(cell_size);
        let _ = terminal_engine.process_pty_output(b"\x1b[?1049h");
        let mut frame_paint = ViewerPaint::from_client(
            &client,
            render_snapshot.client_snapshot.active_tab_id,
            &render_snapshot,
        );

        for should_show_preview in [false, true, false] {
            frame_paint.placement_presentation =
                should_show_preview.then(|| koshi_renderer::snapshot::PanePlacementPresentation {
                    source_pane_id: pane_id,
                    target_pane_ids: Vec::new(),
                    source_message: koshi_renderer::snapshot::PanePlacementMessage {
                        full_text: "Moving pane: choose a destination".to_string(),
                        compact_text: "Moving pane",
                        detail_text: None,
                    },
                    target_message: None,
                });
            let settle_deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let is_frame_committed = paint_frame_with_writer(
                    &mut writer,
                    &mut terminal,
                    &client,
                    &render_snapshot,
                    &committed_regions,
                    &frame_paint,
                    graphics_support.get_image_render_mode(),
                    &mut image_output_state,
                    Some(cell_size),
                    &mut String::new(),
                    &mut None,
                    None,
                    None,
                )
                .expect("native image frame paints");
                let output_bytes = std::mem::take(&mut *writer.0.lock().unwrap());
                let _ = terminal_engine.process_pty_output(&output_bytes);
                if is_frame_committed && !image_output_state.is_work_pending() {
                    break;
                }
                assert!(
                    Instant::now() < settle_deadline,
                    "{graphics_support:?} image frame settles"
                );
                std::thread::sleep(crate::tests::TEST_POLL_INTERVAL_DURATION);
            }
            assert_eq!(
                build_terminal_image_pixel_map(&terminal_engine).is_empty(),
                should_show_preview,
                "{graphics_support:?} native images follow the pane preview"
            );
        }
    }
}

#[test]
fn pane_placement_preview_hides_the_focused_panes_hardware_cursor() {
    let (mut server, client_id, pane_id) = build_test_server_with_pane();
    let mut render_snapshot = build_render_snapshot(&server, client_id);
    render_snapshot.pane_snapshots[0].cursor_snapshot.is_visible = true;
    let client = build_test_client(&mut server, client_id);
    let committed_regions = build_committed_regions(TEST_VIEWPORT_SIZE);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    let mut frame_paint = ViewerPaint::from_client(
        &client,
        render_snapshot.client_snapshot.active_tab_id,
        &render_snapshot,
    );
    assert!(get_cursor_position(
        &render_snapshot,
        &committed_regions,
        Rect::new(0, 0, 80, 24)
    )
    .is_some());

    paint_frame(
        &mut terminal,
        &client,
        &render_snapshot,
        &committed_regions,
        &frame_paint,
        &mut String::new(),
        &mut None,
    )
    .expect("normal frame paints");
    assert!(terminal.backend().cursor_visible());

    frame_paint.placement_presentation =
        Some(koshi_renderer::snapshot::PanePlacementPresentation {
            source_pane_id: pane_id,
            target_pane_ids: Vec::new(),
            source_message: koshi_renderer::snapshot::PanePlacementMessage {
                full_text: "Moving pane: choose a destination".to_string(),
                compact_text: "Moving pane",
                detail_text: None,
            },
            target_message: None,
        });
    paint_frame(
        &mut terminal,
        &client,
        &render_snapshot,
        &committed_regions,
        &frame_paint,
        &mut String::new(),
        &mut None,
    )
    .expect("placement preview paints");
    assert!(!terminal.backend().cursor_visible());

    frame_paint.placement_presentation = None;
    paint_frame(
        &mut terminal,
        &client,
        &render_snapshot,
        &committed_regions,
        &frame_paint,
        &mut String::new(),
        &mut None,
    )
    .expect("normal frame paints after placement");
    assert!(terminal.backend().cursor_visible());
}

fn assert_two_image_trace_output(
    client: &Client,
    image_protocol: koshi_terminal::graphics::GraphicsProtocol,
    render_snapshots: &[RenderSnapshot],
    expected_frame_pixel_maps: &[BTreeMap<(u16, u16), [u8; 4]>],
) {
    for graphics_support in [
        GraphicsSupport::Kitty,
        GraphicsSupport::Iterm,
        GraphicsSupport::Sixel {
            palette_color_count: 256,
            maximum_pixel_width: None,
            maximum_pixel_height: None,
        },
    ] {
        let mut writer = ImageTraceWriter::default();
        let backend = ImageTraceBackend(ratatui::backend::CrosstermBackend::new(writer.clone()));
        let mut terminal = Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .unwrap();
        let mut image_output_state =
            ImageOutputState::from_output_kind(ImageOutputKind::from_support(graphics_support));
        let mut image_cache = crate::attach::paint::ImageCache::new();
        let mut image_wire_content_ids = ImageWireContentIds::default();
        let mut terminal_engine = TerminalEngine::from_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        });
        terminal_engine.set_cell_size(PixelCellSize::from_pixel_dimensions(1, 1).unwrap());
        let _ = terminal_engine.process_pty_output(b"\x1b[?1049h");

        for (frame_stage_index, (render_snapshot, expected_pixel_by_position)) in render_snapshots
            .iter()
            .zip(expected_frame_pixel_maps)
            .enumerate()
        {
            let mut stage_bytes = Vec::new();
            let render_snapshot = round_trip_image_frame_through_wire(
                render_snapshot,
                &mut image_cache,
                &mut image_wire_content_ids,
            );
            let settle_deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let is_frame_committed = paint_frame_with_writer(
                    &mut writer,
                    &mut terminal,
                    client,
                    &render_snapshot,
                    &build_committed_regions(TEST_VIEWPORT_SIZE),
                    &ViewerPaint::from_client(
                        client,
                        render_snapshot.client_snapshot.active_tab_id,
                        &render_snapshot,
                    ),
                    graphics_support.get_image_render_mode(),
                    &mut image_output_state,
                    Some(PixelCellSize::from_pixel_dimensions(1, 1).unwrap()),
                    &mut String::new(),
                    &mut None,
                    None,
                    None,
                )
                .unwrap();
                let output_bytes = std::mem::take(&mut *writer.0.lock().unwrap());
                stage_bytes.extend_from_slice(&output_bytes);
                let _ = terminal_engine.process_pty_output(&output_bytes);
                if is_frame_committed && !image_output_state.is_work_pending() {
                    break;
                }
                assert!(
                    Instant::now() < settle_deadline,
                    "{image_protocol:?} -> {graphics_support:?}, stage {frame_stage_index} did not settle"
                );
                std::thread::sleep(crate::tests::TEST_POLL_INTERVAL_DURATION);
            }

            let terminal_text = terminal_engine
                .get_terminal_state()
                .get_active_grid()
                .list_rows()
                .iter()
                .flat_map(|grid_row| grid_row.iter())
                .map(|grid_cell| grid_cell.get_character())
                .collect::<String>();
            assert!(
                terminal_text.contains("scroll-test"),
                "{image_protocol:?} -> {graphics_support:?}, stage {frame_stage_index} lost the base-cell frame: {terminal_text:?}"
            );
            let actual_pixel_by_position = build_terminal_image_pixel_map(&terminal_engine);
            assert_eq!(
                &actual_pixel_by_position,
                expected_pixel_by_position,
                "{image_protocol:?} -> {graphics_support:?}, stage {frame_stage_index}, stream {:?}, events {:?}",
                String::from_utf8_lossy(&stage_bytes),
                terminal_engine.take_graphics_events()
            );
        }
    }
}

fn assert_partial_native_frame_write_recovers(
    graphics_support: GraphicsSupport,
    sequence_bytes: &'static [u8],
    split_byte_count: usize,
) {
    let (mut server, client_id, pane_id) = build_test_server_with_pane();
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
        client_id,
        cell_size,
    });
    let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
        pane_id,
        output_bytes: build_opaque_image_input(koshi_terminal::graphics::GraphicsProtocol::Kitty),
    });
    let render_snapshot = build_render_snapshot(&server, client_id);
    let client = build_test_client(&mut server, client_id);
    let committed_regions = build_committed_regions(TEST_VIEWPORT_SIZE);
    let render_area = Rect::new(0, 0, 80, 24);
    let image_paint_commands =
        build_image_paints(&render_snapshot, &committed_regions, render_area);
    let image_cell_snapshot =
        build_image_cell_snapshot(&render_snapshot, &committed_regions, render_area).map(Arc::new);
    let mut image_output_state =
        ImageOutputState::from_output_kind(ImageOutputKind::from_support(graphics_support));
    let settle_deadline = Instant::now() + Duration::from_secs(5);
    while !image_output_state.prepare_frame(
        &image_paint_commands,
        image_cell_snapshot.clone(),
        Some(cell_size),
    ) {
        assert!(
            Instant::now() < settle_deadline,
            "{graphics_support:?} output did not prepare"
        );
        std::thread::sleep(crate::tests::TEST_POLL_INTERVAL_DURATION);
    }
    assert!(image_output_state.is_native_commit_pending());
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut writer = FailInsideSequence {
        sequence_bytes,
        split_byte_count,
        should_fail_next_write: false,
        has_failed: false,
        written_bytes: Vec::new(),
    };

    let paint_error = paint_frame_with_writer(
        &mut writer,
        &mut terminal,
        &client,
        &render_snapshot,
        &committed_regions,
        &ViewerPaint::from_client(
            &client,
            render_snapshot.client_snapshot.active_tab_id,
            &render_snapshot,
        ),
        graphics_support.get_image_render_mode(),
        &mut image_output_state,
        Some(cell_size),
        &mut build_window_title(&render_snapshot),
        &mut get_cursor_style(&render_snapshot),
        None,
        None,
    )
    .expect_err("the partial write is returned");

    let PaintError::Image(paint_error) = paint_error;
    assert_eq!(paint_error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(
        paint_error.to_string(),
        "test writer failed inside a control sequence"
    );
    assert!(writer.has_failed);
    assert!(writer.written_bytes.ends_with(b"\x18\x1b\\\x1b[?2026l"));
    assert!(image_output_state.is_native_commit_pending());
    assert_eq!(image_output_state.list_prepared_placement_keys(), []);
}

#[test]
fn partial_synchronized_begin_is_aborted_before_the_end_sequence() {
    assert_partial_native_frame_write_recovers(
        GraphicsSupport::Iterm,
        b"\x1b[?2026h",
        b"\x1b[?20".len(),
    );
}

#[test]
fn partial_native_packets_are_aborted_before_synchronized_output_ends() {
    for (graphics_support, sequence_bytes, split_byte_count) in [
        (GraphicsSupport::Kitty, &b"\x1b_G"[..], 3),
        (GraphicsSupport::Iterm, &b"\x1b]1337;"[..], 8),
        (
            GraphicsSupport::Sixel {
                palette_color_count: 256,
                maximum_pixel_width: None,
                maximum_pixel_height: None,
            },
            &b"\x1bP"[..],
            2,
        ),
    ] {
        assert_partial_native_frame_write_recovers(
            graphics_support,
            sequence_bytes,
            split_byte_count,
        );
    }
}

fn assert_opaque_protocol_matrix(should_include_text: bool) {
    use koshi_terminal::graphics::GraphicsProtocol;
    for image_protocol in [
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Iterm2,
        GraphicsProtocol::Sixel,
    ] {
        let (mut server, client_id, pane_id) = build_test_server_with_pane();
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).unwrap();
        let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
            client_id,
            cell_size,
        });
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: build_opaque_image_input(image_protocol),
        });
        let initial_render_snapshot = build_render_snapshot(&server, client_id);
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: b"\x1b[3S".to_vec(),
        });
        let cropped_render_snapshot = build_render_snapshot(&server, client_id);
        let _ = server.handle_runtime_event(RuntimeEvent::ClientMouse {
            client_id,
            request_id: 1,
            mouse_actions: vec![WireMouseAction::Scroll {
                pane_id,
                is_scrolling_up: true,
                scroll_line_count: 3,
            }],
        });
        let restored_render_snapshot = build_render_snapshot(&server, client_id);
        let client = build_test_client(&mut server, client_id);
        assert_image_trace_output(
            &client,
            image_protocol,
            &[
                initial_render_snapshot,
                cropped_render_snapshot,
                restored_render_snapshot,
            ],
            &[
                (0, 4, build_opaque_image_pixels()),
                (0, 1, vec![255; 16]),
                (0, 4, build_opaque_image_pixels()),
            ],
            should_include_text,
        );
    }
}

fn assert_image_trace_output(
    client: &Client,
    image_protocol: koshi_terminal::graphics::GraphicsProtocol,
    render_snapshots: &[RenderSnapshot],
    expected_frame_pixel_rows: &[(u16, u16, Vec<u8>)],
    should_include_text: bool,
) {
    for graphics_support in [
        GraphicsSupport::Kitty,
        GraphicsSupport::Iterm,
        GraphicsSupport::Sixel {
            palette_color_count: 256,
            maximum_pixel_width: None,
            maximum_pixel_height: None,
        },
    ] {
        let mut writer = ImageTraceWriter::default();
        let backend = ImageTraceBackend(ratatui::backend::CrosstermBackend::new(
            if should_include_text {
                writer.clone()
            } else {
                ImageTraceWriter::default()
            },
        ));
        let mut terminal = Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .unwrap();
        let mut image_output_state =
            ImageOutputState::from_output_kind(ImageOutputKind::from_support(graphics_support));
        let mut image_cache = crate::attach::paint::ImageCache::new();
        let mut image_wire_content_ids = ImageWireContentIds::default();
        let mut terminal_engine = TerminalEngine::from_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        });
        terminal_engine.set_cell_size(PixelCellSize::from_pixel_dimensions(1, 1).unwrap());
        let _ = terminal_engine.process_pty_output(b"\x1b[?1049h");
        for (
            frame_stage_index,
            (render_snapshot, (pixel_row_origin, pixel_row_count, image_pixel_bytes)),
        ) in render_snapshots
            .iter()
            .zip(expected_frame_pixel_rows)
            .enumerate()
        {
            let mut stage_bytes = Vec::new();
            let render_snapshot = round_trip_image_frame_through_wire(
                render_snapshot,
                &mut image_cache,
                &mut image_wire_content_ids,
            );
            let settle_deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let is_frame_committed = paint_frame_with_writer(
                    &mut writer,
                    &mut terminal,
                    client,
                    &render_snapshot,
                    &build_committed_regions(TEST_VIEWPORT_SIZE),
                    &ViewerPaint::from_client(
                        client,
                        render_snapshot.client_snapshot.active_tab_id,
                        &render_snapshot,
                    ),
                    graphics_support.get_image_render_mode(),
                    &mut image_output_state,
                    Some(PixelCellSize::from_pixel_dimensions(1, 1).unwrap()),
                    &mut String::new(),
                    &mut None,
                    None,
                    None,
                )
                .unwrap();
                let output_bytes = std::mem::take(&mut *writer.0.lock().unwrap());
                stage_bytes.extend_from_slice(&output_bytes);
                let _ = terminal_engine.process_pty_output(&output_bytes);
                if is_frame_committed && !image_output_state.is_work_pending() {
                    break;
                }
                assert!(
                    Instant::now() < settle_deadline,
                    "{image_protocol:?} -> {graphics_support:?}, stage {frame_stage_index} did not settle"
                );
                std::thread::sleep(crate::tests::TEST_POLL_INTERVAL_DURATION);
            }
            let image_placements = terminal_engine
                .get_terminal_state()
                .list_image_placements_for_view(0);
            let mut actual_pixels = std::collections::BTreeMap::new();
            for image_placement in &image_placements {
                let render_image_record = image_placement.create_render_image_record();
                let (source_pixel_x, source_pixel_y, source_pixel_width, source_pixel_height) =
                    render_image_record.compute_source_rect().unwrap();
                let image_geometry = image_placement.get_image_geometry();
                for placement_row_index in 0..image_placement.get_image_cell_dimensions().0 {
                    for placement_column_index in 0..image_placement.get_image_cell_dimensions().1 {
                        let source_y = source_pixel_y
                            + u32::from(image_geometry.cell_offset.row + placement_row_index)
                                * source_pixel_height
                                / u32::from(image_geometry.full_size.row_count);
                        let source_x = source_pixel_x
                            + u32::from(image_geometry.cell_offset.column + placement_column_index)
                                * source_pixel_width
                                / u32::from(image_geometry.full_size.column_count);
                        let pixel_start_byte_index =
                            ((source_y * render_image_record.image.pixel_width + source_x) * 4)
                                as usize;
                        let rgba_pixel: [u8; 4] = render_image_record.image.rgba_bytes
                            [pixel_start_byte_index..pixel_start_byte_index + 4]
                            .try_into()
                            .unwrap();
                        actual_pixels.insert(
                            (
                                image_placement.get_image_anchor().0 + placement_row_index,
                                image_placement.get_image_anchor().1 + placement_column_index,
                            ),
                            rgba_pixel,
                        );
                    }
                }
            }
            let expected_pixel_by_position = (0..*pixel_row_count)
                .flat_map(|pixel_row_index| {
                    (0..4).filter_map(move |pixel_column_index| {
                        let pixel_start_byte_index = (usize::from(pixel_row_index) * 4
                            + usize::from(pixel_column_index))
                            * 4;
                        let expected_pixel = <[u8; 4]>::try_from(
                            &image_pixel_bytes[pixel_start_byte_index..pixel_start_byte_index + 4],
                        )
                        .unwrap();
                        (expected_pixel[3] != 0).then_some((
                            (
                                *pixel_row_origin + pixel_row_index + 2,
                                pixel_column_index + 1,
                            ),
                            expected_pixel,
                        ))
                    })
                })
                .collect::<std::collections::BTreeMap<_, _>>();
            assert_eq!(
                actual_pixels,
                expected_pixel_by_position,
                "{image_protocol:?} -> {graphics_support:?}, stage {frame_stage_index}, stream {:?}, events {:?}",
                String::from_utf8_lossy(&stage_bytes),
                terminal_engine.take_graphics_events()
            );
        }
    }
}

#[test]
fn the_launch_hands_the_viewer_the_config_files_it_read() {
    // The client's settings and colors come from the theme file the launch
    // read: theme `ocean` with a red focused border.
    let (mut server, client_id, _pane_id) = build_test_server_with_pane();

    let client = build_test_client_with_config(
        &mut server,
        client_id,
        LoadedConfig {
            app_config_layer: None,
            theme_config_layer: Some(PartialThemeConfig {
                theme_name: Some("ocean".to_owned()),
                colors: Some(PartialColorPalette {
                    border_focused: Some(RgbColor::from_channels(0xff, 0, 0)),
                    ..PartialColorPalette::default()
                }),
            }),
            keybindings_config_layer: None,
        },
    );

    assert_eq!(client.get_client_config().theme.theme_name, "ocean");
    assert_eq!(
        client.get_theme().focused_border_color,
        ratatui::style::Color::Rgb(0xff, 0, 0)
    );
}

#[test]
fn the_launch_hands_the_viewer_the_keymap_file_it_read() {
    // A keymap layer that validates replaces the built-in keybinding settings:
    // `chord_timeout_ms` becomes `1234`.
    let (mut server, client_id, _pane_id) = build_test_server_with_pane();

    let client = build_test_client_with_config(
        &mut server,
        client_id,
        LoadedConfig {
            app_config_layer: None,
            theme_config_layer: None,
            keybindings_config_layer: Some(PartialKeybindingsConfig {
                chord_timeout_ms: Some(1234),
                ..PartialKeybindingsConfig::default()
            }),
        },
    );

    assert_eq!(
        client.get_client_config().keybindings.chord_timeout_ms,
        1234
    );
}

#[test]
fn the_painted_hint_bar_follows_the_clients_mouse_select_state() {
    // The hint bar is painted from the client's own keymap. The mouse-select
    // entry reads `Mouse Select` while the session's mouse-select mode is off
    // and `Mouse Unselect` while it is on.
    let (mut server, client_id, _pane_id) = build_test_server_with_pane();
    let mut client = build_test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
    let render_snapshot = build_render_snapshot(&server, client_id);
    client.apply_render_snapshot(&render_snapshot);

    paint_frame(
        &mut terminal,
        &client,
        &render_snapshot,
        &build_committed_regions(Size {
            column_count: 120,
            row_count: 24,
        }),
        &ViewerPaint::from_client(
            &client,
            render_snapshot.client_snapshot.active_tab_id,
            &render_snapshot,
        ),
        &mut String::new(),
        &mut None,
    )
    .expect("paint");
    assert!(serialize_terminal_screen_text(&terminal).contains("Mouse Select"));

    server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        Command::ToggleMouseSelect,
    ));
    let render_snapshot = build_render_snapshot(&server, client_id);
    client.apply_render_snapshot(&render_snapshot);
    paint_frame(
        &mut terminal,
        &client,
        &render_snapshot,
        &build_committed_regions(Size {
            column_count: 120,
            row_count: 24,
        }),
        &ViewerPaint::from_client(
            &client,
            render_snapshot.client_snapshot.active_tab_id,
            &render_snapshot,
        ),
        &mut String::new(),
        &mut None,
    )
    .expect("paint");

    let painted_screen_text = serialize_terminal_screen_text(&terminal);
    assert!(
        painted_screen_text.contains("Mouse Unselect"),
        "{painted_screen_text}"
    );
}

#[test]
fn pty_output_is_painted_to_the_screen() {
    let (mut server, client_id, pane_id) = build_test_server_with_pane();

    assert!(server
        .handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: b"hello".to_vec(),
        },)
        .is_continue());

    let client = build_test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let render_snapshot = build_render_snapshot(&server, client_id);
    paint_frame(
        &mut terminal,
        &client,
        &render_snapshot,
        &build_committed_regions(TEST_VIEWPORT_SIZE),
        &ViewerPaint::from_client(
            &client,
            render_snapshot.client_snapshot.active_tab_id,
            &render_snapshot,
        ),
        &mut String::new(),
        &mut None,
    )
    .expect("paint");

    assert!(
        serialize_terminal_screen_text(&terminal).contains("hello"),
        "the shell's output should appear on the rendered screen"
    );
}

#[test]
fn painting_emits_a_changed_cursor_style_and_records_it() {
    let (mut server, client_id, pane_id) = build_test_server_with_pane();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

    // The pane asks for a steady bar via DECSCUSR (`CSI 6 SP q`); the first
    // painting sees it differ from the starting `None` and records the new style.
    assert!(server
        .handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: b"\x1b[6 q".to_vec(),
        },)
        .is_continue());
    let client = build_test_client(&mut server, client_id);
    let mut last_cursor_style = None;
    let render_snapshot = build_render_snapshot(&server, client_id);
    paint_frame(
        &mut terminal,
        &client,
        &render_snapshot,
        &build_committed_regions(TEST_VIEWPORT_SIZE),
        &ViewerPaint::from_client(
            &client,
            render_snapshot.client_snapshot.active_tab_id,
            &render_snapshot,
        ),
        &mut String::new(),
        &mut last_cursor_style,
    )
    .expect("paint");

    assert_eq!(
        last_cursor_style,
        Some(CursorStyle::Shaped {
            shape: CursorShape::Bar,
            is_blinking: false,
        })
    );
}

#[test]
fn painting_a_frame_that_names_no_cursor_style_records_none() {
    // A frame with no focused pane leaves `get_cursor_style` with nothing to
    // report. The record still follows the frame: the next frame that names a
    // style counts as a change and is sent again.
    let (mut server, client_id, _pane_id) = build_test_server_with_pane();
    let client = build_test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut render_snapshot = build_render_snapshot(&server, client_id);
    render_snapshot.client_snapshot.focused_pane_id = None;
    let mut last_cursor_style = Some(CursorStyle::Shaped {
        shape: CursorShape::Block,
        is_blinking: true,
    });

    paint_frame(
        &mut terminal,
        &client,
        &render_snapshot,
        &build_committed_regions(TEST_VIEWPORT_SIZE),
        &ViewerPaint::from_client(
            &client,
            render_snapshot.client_snapshot.active_tab_id,
            &render_snapshot,
        ),
        &mut String::new(),
        &mut last_cursor_style,
    )
    .expect("paint");

    assert_eq!(last_cursor_style, None);
}

#[test]
fn painting_records_the_window_title_it_sent() {
    let (mut server, client_id, pane_id) = build_test_server_with_pane();
    let client = build_test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut render_snapshot = build_render_snapshot(&server, client_id);
    render_snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    render_snapshot.client_snapshot.focused_pane_id = Some(pane_id);
    render_snapshot.pane_snapshots[0].pane_id = pane_id;
    render_snapshot.pane_snapshots[0].pane_title = Some("htop".to_string());
    let mut last_window_title = String::new();

    paint_frame(
        &mut terminal,
        &client,
        &render_snapshot,
        &build_committed_regions(TEST_VIEWPORT_SIZE),
        &ViewerPaint::from_client(
            &client,
            render_snapshot.client_snapshot.active_tab_id,
            &render_snapshot,
        ),
        &mut last_window_title,
        &mut None,
    )
    .expect("paint");

    assert_eq!(last_window_title, "quiet-lake | htop");
}

#[test]
fn a_paint_after_a_title_change_records_the_new_title() {
    // The last title decides whether `SetTitle` is written at all. It tracks every
    // frame, not only the first one.
    let (mut server, client_id, _pane_id) = build_test_server_with_pane();
    let client = build_test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut render_snapshot = build_render_snapshot(&server, client_id);
    render_snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    render_snapshot.client_snapshot.focused_pane_id = None;
    let mut last_window_title = String::new();
    let paint_frame_with_snapshot =
        |terminal: &mut Terminal<TestBackend>,
         render_snapshot: &RenderSnapshot,
         last_window_title: &mut String| {
            paint_frame(
                terminal,
                &client,
                render_snapshot,
                &build_committed_regions(TEST_VIEWPORT_SIZE),
                &ViewerPaint::from_client(
                    &client,
                    render_snapshot.client_snapshot.active_tab_id,
                    render_snapshot,
                ),
                last_window_title,
                &mut None,
            )
            .expect("paint");
        };

    paint_frame_with_snapshot(&mut terminal, &render_snapshot, &mut last_window_title);
    assert_eq!(last_window_title, "quiet-lake");

    render_snapshot.session_snapshot.session_name = "loud-hill".to_string();
    paint_frame_with_snapshot(&mut terminal, &render_snapshot, &mut last_window_title);

    assert_eq!(last_window_title, "loud-hill");
}

#[test]
fn each_pane_cursor_style_maps_to_the_crossterm_command_that_re_emits_it() {
    // Each DECSCUSR shape-and-blink pair maps to the crossterm command that
    // writes the same DECSCUSR sequence: `CSI 5 SP q` in, `CSI 5 SP q` out.
    let create_shaped_cursor_style = |cursor_shape, is_blinking| CursorStyle::Shaped {
        shape: cursor_shape,
        is_blinking,
    };
    let cases = [
        // A pane that asked for nothing hands the cursor back to the user.
        (CursorStyle::UserDefault, SetCursorStyle::DefaultUserShape),
        (
            create_shaped_cursor_style(CursorShape::Block, true),
            SetCursorStyle::BlinkingBlock,
        ),
        (
            create_shaped_cursor_style(CursorShape::Block, false),
            SetCursorStyle::SteadyBlock,
        ),
        (
            create_shaped_cursor_style(CursorShape::Underline, true),
            SetCursorStyle::BlinkingUnderScore,
        ),
        (
            create_shaped_cursor_style(CursorShape::Underline, false),
            SetCursorStyle::SteadyUnderScore,
        ),
        (
            create_shaped_cursor_style(CursorShape::Bar, true),
            SetCursorStyle::BlinkingBar,
        ),
        (
            create_shaped_cursor_style(CursorShape::Bar, false),
            SetCursorStyle::SteadyBar,
        ),
    ];
    for (cursor_style, expected_cursor_style) in cases {
        assert_eq!(
            resolve_cursor_style_command(cursor_style),
            expected_cursor_style,
            "{cursor_style:?}"
        );
    }
}

// --- build_window_title: the outer-terminal title string ---

#[test]
fn window_title_with_no_focused_pane_is_just_the_session_name() {
    let (server, client_id, _pane_id) = build_test_server_with_pane();
    let mut render_snapshot = build_render_snapshot(&server, client_id);
    render_snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    render_snapshot.client_snapshot.focused_pane_id = None;

    assert_eq!(build_window_title(&render_snapshot), "quiet-lake");
}

#[test]
fn window_title_with_a_titled_focused_pane_joins_session_and_title() {
    let (server, client_id, pane_id) = build_test_server_with_pane();
    let mut render_snapshot = build_render_snapshot(&server, client_id);
    render_snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    render_snapshot.client_snapshot.focused_pane_id = Some(pane_id);
    render_snapshot.pane_snapshots[0].pane_id = pane_id;
    render_snapshot.pane_snapshots[0].pane_title = Some("htop".to_string());

    assert_eq!(build_window_title(&render_snapshot), "quiet-lake | htop");
}

#[test]
fn window_title_reads_the_focused_pane_not_the_first_one_listed() {
    // The lookup matches on the pane id: the focused pane is listed second.
    let (server, client_id, pane_id) = build_test_server_with_pane();
    let mut render_snapshot = build_render_snapshot(&server, client_id);
    render_snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    let focused_pane_id = PaneId::new();
    let mut second_pane_snapshot = render_snapshot.pane_snapshots[0].clone();
    second_pane_snapshot.pane_id = focused_pane_id;
    second_pane_snapshot.pane_title = Some("htop".to_string());
    render_snapshot.pane_snapshots[0].pane_id = pane_id;
    render_snapshot.pane_snapshots[0].pane_title = Some("bash".to_string());
    render_snapshot.pane_snapshots.push(second_pane_snapshot);
    render_snapshot.client_snapshot.focused_pane_id = Some(focused_pane_id);

    assert_eq!(build_window_title(&render_snapshot), "quiet-lake | htop");
}

#[test]
fn window_title_with_an_untitled_focused_pane_falls_back_to_the_session_name() {
    let (server, client_id, pane_id) = build_test_server_with_pane();
    let mut render_snapshot = build_render_snapshot(&server, client_id);
    render_snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    render_snapshot.client_snapshot.focused_pane_id = Some(pane_id);
    render_snapshot.pane_snapshots[0].pane_id = pane_id;
    render_snapshot.pane_snapshots[0].pane_title = None;

    assert_eq!(build_window_title(&render_snapshot), "quiet-lake");
}

#[test]
fn window_title_keeps_a_non_ascii_pane_title_whole() {
    let (server, client_id, pane_id) = build_test_server_with_pane();
    let mut render_snapshot = build_render_snapshot(&server, client_id);
    render_snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    render_snapshot.client_snapshot.focused_pane_id = Some(pane_id);
    render_snapshot.pane_snapshots[0].pane_id = pane_id;
    render_snapshot.pane_snapshots[0].pane_title = Some("日本語 🙂".to_string());

    assert_eq!(
        build_window_title(&render_snapshot),
        "quiet-lake | 日本語 🙂"
    );
}

#[test]
fn window_title_with_an_empty_pane_title_falls_back_to_the_session_name() {
    let (server, client_id, pane_id) = build_test_server_with_pane();
    let mut render_snapshot = build_render_snapshot(&server, client_id);
    render_snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    render_snapshot.client_snapshot.focused_pane_id = Some(pane_id);
    render_snapshot.pane_snapshots[0].pane_id = pane_id;
    render_snapshot.pane_snapshots[0].pane_title = Some(String::new());

    assert_eq!(build_window_title(&render_snapshot), "quiet-lake");
}

#[test]
fn window_title_with_a_focused_pane_absent_from_the_pane_list_falls_back() {
    let (server, client_id, pane_id) = build_test_server_with_pane();
    let mut render_snapshot = build_render_snapshot(&server, client_id);
    render_snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    render_snapshot.client_snapshot.focused_pane_id = Some(pane_id);
    // No `PaneSnapshot` carries `pane_id`.
    render_snapshot.pane_snapshots.clear();

    assert_eq!(build_window_title(&render_snapshot), "quiet-lake");
}

/// The title [`build_window_title`] builds for a session named `session_name`
/// holding one focused pane titled `pane_title`, after that frame has travelled
/// the session-to-client wire and been read back by
/// [`build_render_snapshot`](crate::attach::paint::build_render_snapshot).
fn round_trip_title_through_wire(session_name: &str, pane_title: &str) -> String {
    let (server, client_id, pane_id) = build_test_server_with_pane();
    let mut sent_render_snapshot = build_render_snapshot(&server, client_id);
    sent_render_snapshot.session_snapshot.session_name = session_name.to_string();
    sent_render_snapshot.client_snapshot.focused_pane_id = Some(pane_id);
    for pane_snapshot in &mut sent_render_snapshot.pane_snapshots {
        if pane_snapshot.pane_id == pane_id {
            pane_snapshot.pane_title = Some(pane_title.to_string());
        }
    }

    let read_back_render_snapshot = crate::attach::paint::build_render_snapshot(
        &koshi_runtime::runtime::frame::build_wire_frame(&sent_render_snapshot),
    );
    build_window_title(&read_back_render_snapshot)
}

#[test]
fn a_window_title_can_carry_no_osc_terminator() {
    // The window title is written into the client's own terminal inside
    // `OSC 0; ... BEL` (crossterm `SetTitle`). The session server chooses both
    // halves of that title.
    for hostile_title_text in [
        "x\u{7}pwned",          // BEL
        "x\u{1b}]0;pwned\u{7}", // ESC
        "x\u{9c}pwned",         // C1 ST
        "x\u{9b}2J",            // C1 CSI
    ] {
        let from_session_name = round_trip_title_through_wire(hostile_title_text, "bash");
        assert!(
            !from_session_name.contains(['\u{7}', '\u{1b}', '\u{9c}', '\u{9b}']),
            "an OSC terminator survived into the window title: {from_session_name:?}"
        );
        let from_pane_title = round_trip_title_through_wire("dev", hostile_title_text);
        assert!(
            !from_pane_title.contains(['\u{7}', '\u{1b}', '\u{9c}', '\u{9b}']),
            "an OSC terminator survived into the window title: {from_pane_title:?}"
        );
    }
}

#[test]
fn a_window_title_names_the_session_and_the_pane_the_wire_carried() {
    assert_eq!(round_trip_title_through_wire("dev", "bash"), "dev | bash");
    assert_eq!(
        round_trip_title_through_wire("dev\u{7}", "\u{1b}bash"),
        "dev | bash"
    );
}

#[test]
fn a_window_title_is_bounded_by_the_pane_title_cap() {
    let pane_title_byte_cap = koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT;
    let window_title = round_trip_title_through_wire("dev", &"a".repeat(100_000));

    assert_eq!(
        window_title,
        format!("dev | {}", "a".repeat(pane_title_byte_cap))
    );
}

#[test]
fn terminal_probe_uses_a_300_ms_deadline_and_a_non_storing_query() {
    let mut probe_output_bytes = Vec::new();

    write_terminal_probe_queries(&mut probe_output_bytes).expect("query writes");

    assert_eq!(TERMINAL_QUERY_TIMEOUT_DURATION, Duration::from_millis(300));
    assert_eq!(
        probe_output_bytes,
        b"\x1b_Gi=4294967295,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b]1337;Capabilities\x1b\\\x1b[c\x1b[?1;1;0S\x1b[?2;4;0S\x1b[16t"
    );
}

#[test]
fn either_terminal_stream_opens_the_terminal_device() {
    assert!(!needs_terminal_device(false, false));
    assert!(needs_terminal_device(true, false));
    assert!(needs_terminal_device(false, true));
    assert!(needs_terminal_device(true, true));
}

#[test]
fn redirected_standard_output_skips_the_controlling_terminal_probe() {
    let mut has_been_called = false;
    let terminal_probe = resolve_graphics_support_for_output(false, || {
        has_been_called = true;
        Ok(build_terminal_probe(GraphicsSupport::Kitty))
    })
    .expect("redirected output selects a supported fallback");

    assert_eq!(
        terminal_probe,
        build_terminal_probe(GraphicsSupport::Unsupported)
    );
    assert!(!has_been_called);

    let terminal_probe = resolve_graphics_support_for_output(true, || {
        has_been_called = true;
        Ok(build_terminal_probe(GraphicsSupport::Kitty))
    })
    .expect("terminal output accepts the probe result");
    assert_eq!(terminal_probe, build_terminal_probe(GraphicsSupport::Kitty));
    assert!(has_been_called);

    let probe_error = resolve_graphics_support_for_output(true, || {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "query write failed",
        ))
    })
    .expect_err("terminal query errors are returned");
    assert_eq!(probe_error.kind(), io::ErrorKind::BrokenPipe);
}

#[test]
fn raw_mode_operation_restores_cooked_mode_after_success() {
    let mut mode_step_names = Vec::new();

    let mode_operation_result = run_with_raw_mode(
        &mut mode_step_names,
        |mode_step_names| {
            mode_step_names.push("raw");
            Ok(())
        },
        |mode_step_names| {
            mode_step_names.push("probe");
            Ok(build_terminal_probe(GraphicsSupport::Kitty))
        },
        |mode_step_names| {
            mode_step_names.push("cooked");
            Ok(())
        },
    )
    .expect("mode cycle succeeds");

    assert_eq!(
        mode_operation_result,
        build_terminal_probe(GraphicsSupport::Kitty)
    );
    assert_eq!(mode_step_names, ["raw", "probe", "cooked"]);
}

#[test]
fn raw_mode_operation_stops_when_raw_mode_entry_fails() {
    let mut mode_step_names = Vec::new();

    let raw_mode_error = run_with_raw_mode(
        &mut mode_step_names,
        |mode_step_names| {
            mode_step_names.push("raw");
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "raw error"))
        },
        |mode_step_names| {
            mode_step_names.push("probe");
            Ok(build_terminal_probe(GraphicsSupport::Kitty))
        },
        |mode_step_names| {
            mode_step_names.push("cooked");
            Ok(())
        },
    )
    .expect_err("the raw-mode error is returned");

    assert_eq!(raw_mode_error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(mode_step_names, ["raw"]);
}

#[test]
fn raw_mode_operation_restores_cooked_mode_after_an_operation_error() {
    let mut mode_step_names = Vec::new();

    let raw_mode_error = run_with_raw_mode(
        &mut mode_step_names,
        |mode_step_names| {
            mode_step_names.push("raw");
            Ok(())
        },
        |mode_step_names| {
            mode_step_names.push("probe");
            Err::<TerminalProbe, _>(io::Error::new(io::ErrorKind::InvalidData, "probe error"))
        },
        |mode_step_names| {
            mode_step_names.push("cooked");
            Ok(())
        },
    )
    .expect_err("the probe error is returned");

    assert_eq!(raw_mode_error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(mode_step_names, ["raw", "probe", "cooked"]);
}

#[test]
fn raw_mode_operation_returns_a_cooked_mode_error() {
    let mut mode_step_names = Vec::new();

    let raw_mode_error = run_with_raw_mode(
        &mut mode_step_names,
        |mode_step_names| {
            mode_step_names.push("raw");
            Ok(())
        },
        |mode_step_names| {
            mode_step_names.push("probe");
            Ok(build_terminal_probe(GraphicsSupport::Kitty))
        },
        |mode_step_names| {
            mode_step_names.push("cooked");
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cooked error",
            ))
        },
    )
    .expect_err("the cooked-mode error is returned");

    assert_eq!(raw_mode_error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(mode_step_names, ["raw", "probe", "cooked"]);
}

#[test]
fn raw_mode_operation_keeps_the_operation_error_when_restore_also_fails() {
    let mut mode_step_names = Vec::new();

    let raw_mode_error = run_with_raw_mode(
        &mut mode_step_names,
        |mode_step_names| {
            mode_step_names.push("raw");
            Ok(())
        },
        |mode_step_names| {
            mode_step_names.push("probe");
            Err::<TerminalProbe, _>(io::Error::new(io::ErrorKind::InvalidData, "probe error"))
        },
        |mode_step_names| {
            mode_step_names.push("cooked");
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cooked error",
            ))
        },
    )
    .expect_err("the probe error is returned");

    assert_eq!(raw_mode_error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(raw_mode_error.to_string(), "probe error");
    assert_eq!(mode_step_names, ["raw", "probe", "cooked"]);
}

#[test]
fn terminal_probe_filter_leaves_keys_and_other_replies_buffered() {
    use koshi_input::host::{KeyCode, KittyGraphicsReply};

    assert!(is_probe_event(&Event::KittyGraphicsReply(
        KittyGraphicsReply {
            image_id: u32::MAX,
            is_successful: true,
        }
    )));
    assert!(!is_probe_event(&Event::KittyGraphicsReply(
        KittyGraphicsReply {
            image_id: 31,
            is_successful: true,
        }
    )));
    assert!(!is_probe_event(&Event::Key(KeyCode::Char('x').into())));
    assert!(is_probe_event(&Event::PrimaryDeviceAttributes(vec![1, 2])));
}

#[test]
fn terminal_probe_collects_reordered_replies_before_selecting_kitty() {
    use koshi_input::host::KittyGraphicsReply;

    let mut reader = build_probe_reader([
        Event::Key(KeyCode::Char('k').into()),
        Event::PrimaryDeviceAttributes(vec![1, 2, 4]),
        Event::TerminalFeatures(b"FSx".to_vec()),
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Palette(Ok(256))),
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Geometry(Ok((640, 480)))),
        Event::CellSize(PixelCellSize::from_pixel_dimensions(10, 20).expect("nonzero cell size")),
        Event::KittyGraphicsReply(KittyGraphicsReply {
            image_id: KITTY_QUERY_IMAGE_ID,
            is_successful: true,
        }),
        Event::Paste("pasted".to_string()),
    ]);
    let mut probe_output_bytes = Vec::new();

    let probe_result =
        probe_terminal(&mut probe_output_bytes, &mut reader).expect("probe reads replies");

    assert_eq!(probe_result.graphics_support, GraphicsSupport::Kitty);
    assert_eq!(
        probe_result.cell_size,
        PixelCellSize::from_pixel_dimensions(10, 20),
        "the cell-size reply is retained beside protocol support"
    );
    assert_eq!(
        reader
            .read_matching_event(|host_event| matches!(host_event, Event::Key(_)))
            .expect("the unrelated key remains buffered"),
        Event::Key(KeyCode::Char('k').into())
    );
    assert_eq!(
        reader
            .read_matching_event(|host_event| matches!(host_event, Event::Paste(_)))
            .expect("the unrelated paste remains buffered"),
        Event::Paste("pasted".to_string())
    );
}

#[test]
fn terminal_probe_prefers_iterm_file_over_sixel() {
    let mut reader = build_probe_reader([
        Event::PrimaryDeviceAttributes(vec![1, 2, 4]),
        Event::TerminalFeatures(b"SxF".to_vec()),
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Palette(Err(
            GraphicAttributeError::Failure,
        ))),
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Geometry(Ok((0, 0)))),
    ]);

    let probe_result = probe_terminal(&mut Vec::new(), &mut reader).expect("probe reads replies");

    assert_eq!(probe_result.graphics_support, GraphicsSupport::Iterm);
}

#[test]
fn terminal_probe_uses_two_sixel_colors_without_a_palette_reply() {
    let mut reader = build_probe_reader([Event::PrimaryDeviceAttributes(vec![1, 4])]);

    let probe_result = probe_terminal(&mut Vec::new(), &mut reader).expect("probe reads replies");

    assert_eq!(
        probe_result.graphics_support,
        GraphicsSupport::Sixel {
            palette_color_count: 2,
            maximum_pixel_width: None,
            maximum_pixel_height: None,
        }
    );
}

#[test]
fn terminal_probe_caps_sixel_palette_and_maps_zero_geometry_to_unlimited_axes() {
    let mut reader = build_probe_reader([
        Event::TerminalFeatures(b"Sx".to_vec()),
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Palette(Ok(512))),
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Geometry(Ok((0, 480)))),
    ]);

    let probe_result = probe_terminal(&mut Vec::new(), &mut reader).expect("probe reads replies");

    assert_eq!(
        probe_result.graphics_support,
        GraphicsSupport::Sixel {
            palette_color_count: 256,
            maximum_pixel_width: None,
            maximum_pixel_height: Some(480),
        }
    );
}

#[test]
fn terminal_probe_does_not_treat_a_regis_palette_reply_as_sixel_support() {
    let mut reader = build_probe_reader([
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Palette(Ok(256))),
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Geometry(Err(
            GraphicAttributeError::Failure,
        ))),
    ]);

    let probe_result = probe_terminal(&mut Vec::new(), &mut reader).expect("probe reads replies");

    assert_eq!(probe_result.graphics_support, GraphicsSupport::Unsupported);
}

#[test]
fn terminal_probe_accepts_successful_sixel_geometry_without_other_sixel_evidence() {
    let mut reader = build_probe_reader([Event::SixelGraphicsAttributeReply(
        GraphicAttributeReply::Geometry(Ok((640, 480))),
    )]);

    let probe_result = probe_terminal(&mut Vec::new(), &mut reader).expect("probe reads replies");

    assert_eq!(
        probe_result.graphics_support,
        GraphicsSupport::Sixel {
            palette_color_count: 2,
            maximum_pixel_width: Some(640),
            maximum_pixel_height: Some(480),
        }
    );
}

#[test]
fn terminal_probe_rejects_a_reported_one_color_sixel_palette() {
    let mut reader = build_probe_reader([
        Event::PrimaryDeviceAttributes(vec![1, 2, 4]),
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Palette(Ok(1))),
    ]);

    let probe_result = probe_terminal(&mut Vec::new(), &mut reader).expect("probe reads replies");

    assert_eq!(probe_result.graphics_support, GraphicsSupport::Unsupported);
}

#[test]
fn terminal_probe_without_replies_preserves_unrelated_input_and_reports_unsupported() {
    let mut reader = build_probe_reader([
        Event::Key(KeyCode::Char('x').into()),
        Event::Paste("input".to_string()),
    ]);

    let probe_result = probe_terminal(&mut Vec::new(), &mut reader).expect("empty probe completes");

    assert_eq!(probe_result.graphics_support, GraphicsSupport::Unsupported);
    assert_eq!(probe_result.cell_size, None);
    assert_eq!(
        reader
            .read_matching_event(|host_event| matches!(host_event, Event::Key(_)))
            .expect("the key is not consumed by the probe"),
        Event::Key(KeyCode::Char('x').into())
    );
    assert_eq!(
        reader
            .read_matching_event(|host_event| matches!(host_event, Event::Paste(_)))
            .expect("the paste is not consumed by the probe"),
        Event::Paste("input".to_string())
    );
}

#[test]
fn new_protocols_use_native_image_mode_when_their_writer_is_available() {
    assert_eq!(
        GraphicsSupport::Iterm.get_image_render_mode(),
        ImageRenderMode::Native
    );
    assert_eq!(
        GraphicsSupport::Sixel {
            palette_color_count: 2,
            maximum_pixel_width: None,
            maximum_pixel_height: None,
        }
        .get_image_render_mode(),
        ImageRenderMode::Native
    );
}

#[test]
fn terminal_modes_are_enabled_after_entering_the_alternate_screen() {
    let mut terminal_mode_bytes = Vec::new();

    enable_terminal_modes(&mut terminal_mode_bytes, GraphicsSupport::Unsupported)
        .expect("terminal modes write");

    assert_eq!(
        terminal_mode_bytes,
        b"\x1b[?1049h\x1b[>31u\x1b[?1003h\x1b[?1006h\x1b[?2004h\x1b[?u"
    );
}

#[test]
fn shift_mouse_capture_is_requested_only_while_placement_is_active() {
    let mut terminal_mode_bytes = Vec::new();
    let mut is_shift_mouse_capture_requested = false;

    update_shift_mouse_capture(
        &mut terminal_mode_bytes,
        false,
        &mut is_shift_mouse_capture_requested,
    )
    .expect("normal mouse mode");
    assert_eq!(terminal_mode_bytes, b"");

    update_shift_mouse_capture(
        &mut terminal_mode_bytes,
        true,
        &mut is_shift_mouse_capture_requested,
    )
    .expect("placement mouse mode");
    assert_eq!(terminal_mode_bytes, b"\x1b[>1s");
    assert!(is_shift_mouse_capture_requested);

    update_shift_mouse_capture(
        &mut terminal_mode_bytes,
        true,
        &mut is_shift_mouse_capture_requested,
    )
    .expect("unchanged placement mouse mode");
    assert_eq!(terminal_mode_bytes, b"\x1b[>1s");

    update_shift_mouse_capture(
        &mut terminal_mode_bytes,
        false,
        &mut is_shift_mouse_capture_requested,
    )
    .expect("normal mouse mode restored");
    assert_eq!(terminal_mode_bytes, b"\x1b[>1s\x1b[>0s");
    assert!(!is_shift_mouse_capture_requested);
}

#[test]
fn failed_shift_mouse_capture_write_can_be_retried() {
    let mut writer = FailOnWrite {
        failing_write_index: 0,
        write_count: 0,
        written_bytes: Vec::new(),
    };
    let mut is_shift_mouse_capture_requested = false;

    let io_error =
        update_shift_mouse_capture(&mut writer, true, &mut is_shift_mouse_capture_requested)
            .expect_err("the terminal write fails");
    assert_eq!(io_error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(writer.written_bytes, b"");
    assert!(!is_shift_mouse_capture_requested);

    update_shift_mouse_capture(&mut writer, true, &mut is_shift_mouse_capture_requested)
        .expect("the next write succeeds");
    assert_eq!(writer.written_bytes, b"\x1b[>1s");
    assert!(is_shift_mouse_capture_requested);
}

#[test]
fn sixel_modes_are_saved_before_application_modes() {
    let mut terminal_mode_bytes = Vec::new();
    let graphics_support = GraphicsSupport::Sixel {
        palette_color_count: 2,
        maximum_pixel_width: None,
        maximum_pixel_height: None,
    };

    enable_terminal_modes(&mut terminal_mode_bytes, graphics_support)
        .expect("terminal modes write");

    assert_eq!(
        terminal_mode_bytes,
        b"\x1b[?80s\x1b[?8452s\x1b[?1070s\x1b[?1049h\x1b[>31u\x1b[?1003h\x1b[?1006h\x1b[?2004h\x1b[?u"
    );
}

#[test]
fn terminal_cleanup_reverses_modes_and_deletes_kitty_images() {
    let mut cleanup_bytes = Vec::new();
    let is_image_cleanup_claimed = AtomicBool::new(false);

    write_terminal_cleanup(
        &mut cleanup_bytes,
        GraphicsSupport::Kitty,
        &is_image_cleanup_claimed,
    )
    .expect("terminal cleanup writes");

    assert_eq!(
        cleanup_bytes,
        b"\x18\x1b\\\x1b_Ga=d,d=A,q=2;\x1b\\\x1b[>0s\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q"
    );
    assert!(is_image_cleanup_claimed.load(Ordering::Acquire));
}

#[test]
fn sixel_cleanup_aborts_a_control_string_before_restoring_modes() {
    let mut cleanup_bytes = Vec::new();
    let is_image_cleanup_claimed = AtomicBool::new(false);
    let graphics_support = GraphicsSupport::Sixel {
        palette_color_count: 2,
        maximum_pixel_width: None,
        maximum_pixel_height: None,
    };

    write_terminal_cleanup(
        &mut cleanup_bytes,
        graphics_support,
        &is_image_cleanup_claimed,
    )
    .expect("terminal cleanup writes");

    assert_eq!(
        cleanup_bytes,
        b"\x18\x1b\\\x1b[?80r\x1b[?8452r\x1b[?1070r\x1b[>0s\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q"
    );
}

#[test]
fn terminal_cleanup_attempts_mode_resets_after_image_delete_fails() {
    let mut writer = FailOnWrite {
        failing_write_index: 1,
        write_count: 0,
        written_bytes: Vec::new(),
    };
    let is_image_cleanup_claimed = AtomicBool::new(false);

    let cleanup_error = write_terminal_cleanup(
        &mut writer,
        GraphicsSupport::Kitty,
        &is_image_cleanup_claimed,
    )
    .expect_err("the failed delete is returned");

    assert_eq!(cleanup_error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(
        writer.written_bytes,
        b"\x18\x1b\\\x1b[>0s\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q"
    );
}

#[test]
fn terminal_cleanup_cancels_a_partial_kitty_apc_before_deleting_images() {
    let partial_kitty_packet = b"\x1b_Ga=t,f=32,s=1,v=1,I=1,q=2,o=z,m=1;AAAA";
    let mut writer = partial_kitty_packet.to_vec();

    let is_image_cleanup_claimed = AtomicBool::new(false);
    write_terminal_cleanup(
        &mut writer,
        GraphicsSupport::Kitty,
        &is_image_cleanup_claimed,
    )
    .expect("cleanup writes after the partial packet");
    assert_eq!(
        &writer[partial_kitty_packet.len()..],
        b"\x18\x1b\\\x1b_Ga=d,d=A,q=2;\x1b\\\x1b[>0s\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q"
    );

    let mut terminal_engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });
    assert!(terminal_engine.process_pty_output(&writer).is_empty());
    assert!(terminal_engine.take_graphics_events().is_empty());
    assert!(terminal_engine.finish_graphics_stream().is_empty());
}

#[test]
fn terminal_application_modes_are_restored_once_across_cleanup_paths() {
    let mut cleanup_bytes = Vec::new();
    let has_active_application_modes = AtomicBool::new(true);
    let is_image_cleanup_claimed = AtomicBool::new(false);

    restore_application_modes(
        &mut cleanup_bytes,
        GraphicsSupport::Kitty,
        &has_active_application_modes,
        &is_image_cleanup_claimed,
    )
    .expect("the panic cleanup writes");
    restore_application_modes(
        &mut cleanup_bytes,
        GraphicsSupport::Kitty,
        &has_active_application_modes,
        &is_image_cleanup_claimed,
    )
    .expect("the unwind cleanup is already complete");

    assert_eq!(
        cleanup_bytes,
        b"\x18\x1b\\\x1b_Ga=d,d=A,q=2;\x1b\\\x1b[>0s\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q"
    );
    assert!(!has_active_application_modes.load(Ordering::Acquire));
    assert!(is_image_cleanup_claimed.load(Ordering::Acquire));
}

#[test]
fn terminal_cleanup_skips_image_commands_before_application_modes_are_active() {
    let mut cleanup_bytes = Vec::new();
    let has_active_application_modes = AtomicBool::new(false);
    let is_image_cleanup_claimed = AtomicBool::new(false);

    restore_application_modes(
        &mut cleanup_bytes,
        GraphicsSupport::Kitty,
        &has_active_application_modes,
        &is_image_cleanup_claimed,
    )
    .expect("inactive application modes need no cleanup");

    assert!(cleanup_bytes.is_empty());
    assert!(!is_image_cleanup_claimed.load(Ordering::Acquire));
}

#[test]
fn host_resize_and_paste_events_keep_their_exact_values() {
    let client_id = ClientId::new();
    let resize_runtime_event = build_terminal_runtime_event(
        client_id,
        Event::WindowResized(WindowSize {
            column_count: 101,
            row_count: 37,
            pixel_width: Some(1_010),
            pixel_height: Some(740),
        }),
    );
    let Some(RuntimeEvent::Resize {
        client_id: actual_client_id,
        viewport_size,
        pane_area,
        cell_size,
    }) = resize_runtime_event
    else {
        panic!("expected the exact resize event, got {resize_runtime_event:?}");
    };
    assert_eq!(actual_client_id, client_id);
    assert_eq!(
        viewport_size,
        Size {
            column_count: 101,
            row_count: 37,
        }
    );
    assert_eq!(
        pane_area,
        Some(crate::compute_core_pane_area(viewport_size))
    );
    assert_eq!(
        cell_size,
        Some(PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions"))
    );

    let paste_runtime_event =
        build_terminal_runtime_event(client_id, Event::Paste("hello 🐈".to_string()));
    let Some(RuntimeEvent::HostPaste {
        client_id: actual_client_id,
        pasted_text,
    }) = paste_runtime_event
    else {
        panic!("expected the exact paste event, got {paste_runtime_event:?}");
    };
    assert_eq!(actual_client_id, client_id);
    assert_eq!(pasted_text, "hello 🐈");
}

#[test]
fn local_pixel_cell_size_requires_complete_evenly_divisible_metrics() {
    let complete_window_size = WindowSize {
        column_count: 10,
        row_count: 20,
        pixel_width: Some(100),
        pixel_height: Some(400),
    };
    assert_eq!(
        compute_pixel_cell_size(complete_window_size),
        Some(PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions"))
    );

    for invalid_window_size in [
        WindowSize {
            column_count: 10,
            row_count: 20,
            pixel_width: None,
            pixel_height: Some(400),
        },
        WindowSize {
            column_count: 10,
            row_count: 20,
            pixel_width: Some(100),
            pixel_height: None,
        },
        WindowSize {
            column_count: 10,
            row_count: 20,
            pixel_width: Some(101),
            pixel_height: Some(400),
        },
        WindowSize {
            column_count: 0,
            row_count: 20,
            pixel_width: Some(100),
            pixel_height: Some(400),
        },
        WindowSize {
            column_count: 10,
            row_count: 0,
            pixel_width: Some(100),
            pixel_height: Some(400),
        },
        WindowSize {
            column_count: 10,
            row_count: 20,
            pixel_width: Some(0),
            pixel_height: Some(400),
        },
        WindowSize {
            column_count: 10,
            row_count: 20,
            pixel_width: Some(100),
            pixel_height: Some(0),
        },
    ] {
        assert_eq!(compute_pixel_cell_size(invalid_window_size), None);
    }
}

#[test]
fn initial_cell_size_uses_only_native_image_measurements() {
    let probed_cell_size =
        PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions");
    let local_cell_size =
        PixelCellSize::from_pixel_dimensions(12, 24).expect("positive cell dimensions");

    assert_eq!(
        resolve_initial_cell_size(
            GraphicsSupport::Unsupported,
            Some(probed_cell_size),
            Some(local_cell_size),
        ),
        None
    );
    assert_eq!(
        resolve_initial_cell_size(GraphicsSupport::Kitty, None, Some(local_cell_size)),
        Some(local_cell_size)
    );
    assert_eq!(
        resolve_initial_cell_size(
            GraphicsSupport::Iterm,
            Some(probed_cell_size),
            Some(local_cell_size),
        ),
        Some(probed_cell_size)
    );
}

#[test]
fn an_outstanding_cell_size_query_cannot_accept_a_reply_from_an_old_resize() {
    let old_cell_size =
        PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions");
    let current_cell_size =
        PixelCellSize::from_pixel_dimensions(12, 24).expect("positive cell dimensions");
    let mut cell_size_query = CellSizeQuery::from_current_measurement(None, true, false);
    let mut written_query_bytes = Vec::new();

    assert!(cell_size_query.update_cell_size_for_resize(None));
    cell_size_query
        .write_cell_size_request(&mut written_query_bytes)
        .expect("the first cell-size query writes");
    assert_eq!(written_query_bytes, b"\x1b[16t");

    assert!(!cell_size_query.update_cell_size_for_resize(Some(current_cell_size)));
    assert_eq!(
        cell_size_query.accept_cell_size_reply(old_cell_size),
        (None, false)
    );
    assert_eq!(
        cell_size_query.get_current_cell_size(),
        Some(current_cell_size)
    );

    assert_eq!(
        cell_size_query.accept_cell_size_reply(old_cell_size),
        (None, false),
        "an unsolicited reply cannot overwrite the current resize"
    );
    cell_size_query
        .write_cell_size_request(&mut written_query_bytes)
        .expect("an already measured resize needs no replacement query");
    assert_eq!(written_query_bytes, b"\x1b[16t");
}

#[test]
fn an_old_reply_is_discarded_then_an_unknown_resize_gets_one_new_query() {
    let old_cell_size =
        PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions");
    let mut cell_size_query = CellSizeQuery::from_current_measurement(None, true, false);
    let mut written_query_bytes = Vec::new();

    cell_size_query
        .write_cell_size_request(&mut written_query_bytes)
        .expect("the initial cell-size query writes");
    assert!(!cell_size_query.update_cell_size_for_resize(None));
    assert_eq!(
        cell_size_query.accept_cell_size_reply(old_cell_size),
        (None, true)
    );
    cell_size_query
        .write_cell_size_request(&mut written_query_bytes)
        .expect("the replacement query writes after the old reply");
    assert_eq!(written_query_bytes, b"\x1b[16t\x1b[16t");
}

#[test]
fn a_timed_out_probe_does_not_block_a_resize_query() {
    let measured_cell_size =
        PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions");
    let mut cell_size_query = CellSizeQuery::from_current_measurement(None, true, false);
    let mut written_query_bytes = Vec::new();

    assert!(cell_size_query.update_cell_size_for_resize(None));
    cell_size_query
        .write_cell_size_request(&mut written_query_bytes)
        .expect("the resize query writes after the timed-out probe");
    assert_eq!(
        cell_size_query.accept_cell_size_reply(measured_cell_size),
        (Some(measured_cell_size), false)
    );
    assert_eq!(written_query_bytes, b"\x1b[16t");
}

#[test]
fn an_ipc_reconnect_keeps_an_outer_terminal_query_pending() {
    let old_cell_size =
        PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions");
    let mut cell_size_query = CellSizeQuery::from_current_measurement(None, true, false);
    let mut written_query_bytes = Vec::new();

    cell_size_query
        .write_cell_size_request(&mut written_query_bytes)
        .expect("the closed connection's query writes");
    assert!(!cell_size_query.update_cell_size_for_resize(Some(old_cell_size)));
    assert!(!cell_size_query.update_cell_size_for_resize(None));
    assert_eq!(
        cell_size_query.accept_cell_size_reply(old_cell_size),
        (None, true)
    );
    cell_size_query
        .write_cell_size_request(&mut written_query_bytes)
        .expect("the replacement connection's query writes");
    assert_eq!(written_query_bytes, b"\x1b[16t\x1b[16t");
}

#[test]
fn terminal_runtime_discards_capability_reply_events() {
    use koshi_input::host::GraphicAttributeReply;

    let client_id = ClientId::new();
    for host_event in [
        Event::PrimaryDeviceAttributes(vec![1, 2]),
        Event::TerminalFeatures(b"F".to_vec()),
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Palette(Ok(256))),
    ] {
        match build_terminal_runtime_event(client_id, host_event) {
            None => {}
            Some(runtime_event) => panic!("unexpected runtime event: {runtime_event:?}"),
        }
    }
}

#[test]
fn terminal_focus_loss_becomes_a_viewer_cancellation_event() {
    let client_id = ClientId::new();

    assert!(matches!(
        build_terminal_runtime_event(client_id, Event::FocusOut),
        Some(RuntimeEvent::OuterTerminalFocusLost {
            client_id: focus_lost_client_id,
        }) if focus_lost_client_id == client_id
    ));
    assert!(build_terminal_runtime_event(client_id, Event::FocusIn).is_none());
}

#[test]
fn unsupported_paint_writes_no_terminal_image_output() {
    let (mut server, client_id, pane_id) = build_test_server_with_pane();
    let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
        pane_id,
        output_bytes: b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\".to_vec(),
    });
    let client = build_test_client(&mut server, client_id);
    let render_snapshot = build_render_snapshot(&server, client_id);
    assert_eq!(
        render_snapshot.pane_snapshots[0]
            .image_placement_snapshots
            .len(),
        1
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut terminal_output_bytes = Vec::new();
    let mut last_window_title = build_window_title(&render_snapshot);
    let mut last_cursor_style = get_cursor_style(&render_snapshot);
    let mut image_output_state = ImageOutputState::from_output_kind(None);

    paint_frame_with_writer(
        &mut terminal_output_bytes,
        &mut terminal,
        &client,
        &render_snapshot,
        &build_committed_regions(TEST_VIEWPORT_SIZE),
        &ViewerPaint::from_client(
            &client,
            render_snapshot.client_snapshot.active_tab_id,
            &render_snapshot,
        ),
        ImageRenderMode::Placeholder,
        &mut image_output_state,
        None,
        &mut last_window_title,
        &mut last_cursor_style,
        None,
        None,
    )
    .expect("placeholder paint");

    assert_eq!(terminal_output_bytes, Vec::<u8>::new());
}

#[test]
fn image_cleanup_has_one_owner() {
    let is_image_cleanup_claimed = AtomicBool::new(false);

    assert!(claim_image_cleanup(&is_image_cleanup_claimed));
    assert!(!claim_image_cleanup(&is_image_cleanup_claimed));
}

// ------------------------------------ the keyboard enhancement flag query ----

#[test]
fn the_mode_setup_pushes_the_keyboard_flags_and_asks_what_landed() {
    let mut terminal_mode_bytes = Vec::new();

    enable_terminal_modes(&mut terminal_mode_bytes, GraphicsSupport::Unsupported)
        .expect("terminal modes write");

    // The push asks for flags 1|2|4|8|16, and the query follows it in the same
    // batch.
    let mode_setup_text = String::from_utf8(terminal_mode_bytes).expect("mode bytes are text");
    let push_position = mode_setup_text
        .find("\x1b[>31u")
        .expect("the keyboard push is written");
    let query_position = mode_setup_text
        .find("\x1b[?u")
        .expect("the keyboard query is written");
    assert!(
        push_position < query_position,
        "the query must follow the push: {mode_setup_text:?}"
    );
}

#[test]
fn ordinary_typing_reaches_the_runtime_as_the_character_typed() {
    // The plain bytes of `å` reach the runtime as the chord `å` with no
    // modifiers.
    let mut parser = koshi_input::host::Parser::default();
    parser.process_input_bytes("å".as_bytes());
    let host_event = parser.remove_next_pending_event().expect("one key event");

    let runtime_event = build_terminal_runtime_event(ClientId::new(), host_event)
        .expect("a typed character is input");

    match runtime_event {
        RuntimeEvent::KeyInput { key_input, .. } => {
            assert_eq!(
                key_input.to_binding_chord(),
                Some(KeyChord::from_parts(
                    BindingModifierFlags::NONE,
                    Key::Char('å')
                ))
            );
        }
        other_runtime_event => panic!("expected a key, got {other_runtime_event:?}"),
    }
}

#[test]
fn a_release_reaches_the_runtime_and_resolves_no_binding() {
    // A key release reaches the runtime as a `KeyInput`. The release has no
    // binding chord.
    let mut parser = koshi_input::host::Parser::default();
    parser.process_input_bytes(b"\x1b[97;1:3u");
    let host_event = parser.remove_next_pending_event().expect("one key event");

    let runtime_event = build_terminal_runtime_event(ClientId::new(), host_event)
        .expect("a release is still an event");

    match runtime_event {
        RuntimeEvent::KeyInput { key_input, .. } => {
            assert_eq!(key_input.key_event_kind, KeyEventKind::Release);
            assert_eq!(key_input.to_binding_chord(), None);
        }
        other_runtime_event => panic!("expected a key, got {other_runtime_event:?}"),
    }
}

#[test]
fn every_captured_field_survives_the_runtime_boundary() {
    // Alternate keys, associated text and the lock modifiers reach the
    // runtime.
    let mut parser = koshi_input::host::Parser::default();
    parser.process_input_bytes(b"\x1b[39:34:113;130:2;34u");
    let host_event = parser.remove_next_pending_event().expect("one key event");

    let runtime_event =
        build_terminal_runtime_event(ClientId::new(), host_event).expect("a key is input");

    match runtime_event {
        RuntimeEvent::KeyInput { key_input, .. } => {
            assert_eq!(key_input.key, KeyIdentity::Key(Key::Char('\'')));
            assert_eq!(key_input.key_event_kind, KeyEventKind::Repeat);
            assert_eq!(key_input.shifted_key, Some('"'));
            assert_eq!(key_input.base_layout_key, Some('q'));
            assert_eq!(key_input.associated_text, "\"");
            assert!(key_input
                .modifier_flags
                .has_all_modifiers(KeyModifierFlags::NUM_LOCK));
        }
        other_runtime_event => panic!("expected a key, got {other_runtime_event:?}"),
    }
}

#[test]
fn the_enhancement_answer_reaches_no_runtime_event() {
    // The answer is a terminal reply, not input. It produces no runtime event.
    assert!(
        build_terminal_runtime_event(ClientId::new(), Event::KeyboardEnhancementFlags(31))
            .is_none()
    );
}

#[test]
fn a_typed_key_becomes_a_key_input_for_its_client() {
    let client_id = ClientId::new();
    let runtime_event =
        build_terminal_runtime_event(client_id, Event::Key(KeyCode::Char('x').into()))
            .expect("a typed key is input");

    match runtime_event {
        RuntimeEvent::KeyInput {
            client_id: key_client_id,
            key_input,
        } => {
            assert_eq!(key_client_id, client_id);
            assert_eq!(
                key_input.to_binding_chord(),
                Some(KeyChord::from_parts(
                    BindingModifierFlags::NONE,
                    Key::Char('x')
                ))
            );
        }
        other_runtime_event => panic!("expected a key, got {other_runtime_event:?}"),
    }
}

#[test]
fn placement_projection_keeps_scaled_rectangles_inside_the_preview() {
    let projected_rect = project_core_rect(
        CoreRect {
            origin: CorePoint {
                column: 20,
                row: 10,
            },
            size: Size {
                column_count: 40,
                row_count: 20,
            },
        },
        Size {
            column_count: 120,
            row_count: 40,
        },
        Rect::new(2, 3, 60, 20),
    )
    .expect("the source rectangle is visible");

    assert_eq!(projected_rect, Rect::new(12, 8, 20, 10));
    assert!(projected_rect.right() <= 62);
    assert!(projected_rect.bottom() <= 23);
}

#[test]
fn proposed_same_tab_swap_replaces_slots_without_mutating_the_snapshot() {
    let source_pane_id = PaneId::new();
    let target_pane_id = PaneId::new();
    let tab_id = TabId::new();
    let tab_snapshot =
        build_test_placement_tab_snapshot(tab_id, "work", [source_pane_id, target_pane_id]);
    let placement_snapshot = PlacementSnapshot {
        session_id: SessionId::new(),
        source_pane_id,
        source_tab_id: tab_id,
        destination_tab_id: tab_id,
        session_placement_revision: 4,
        client_placement_revision: 7,
        source_tab_snapshot: tab_snapshot,
        destination_tab_snapshot: None,
        client_snapshot: PlacementClientSnapshot {
            client_snapshot: ClientSnapshot {
                client_id: ClientId::new(),
                client_revision: 7,
                viewport_size: Size {
                    column_count: 20,
                    row_count: 8,
                },
                active_tab_id: tab_id,
                focused_pane_id: Some(source_pane_id),
                lock_mode: LockMode::Normal,
                is_mouse_selection_enabled: false,
            },
            reported_pane_area: Some(PaneArea::Reported(Size {
                column_count: 20,
                row_count: 8,
            })),
        },
        pane_sizing: PaneSizing::default(),
    };
    let original_snapshot = placement_snapshot.clone();

    let proposed_snapshot = build_proposed_placement_snapshot(
        &placement_snapshot,
        &PanePlacementTarget::Swap { target_pane_id },
    )
    .expect("the swap target is in the tab");

    let proposed_pane_ids = proposed_snapshot
        .source_tab_snapshot
        .tab_snapshot
        .pane_slots
        .iter()
        .map(|pane_slot| pane_slot.pane_id)
        .collect::<Vec<_>>();
    assert_eq!(proposed_pane_ids, vec![target_pane_id, source_pane_id]);
    assert_eq!(placement_snapshot, original_snapshot);
    assert_eq!(
        proposed_snapshot.source_tab_snapshot.pane_snapshots[0].pane_id,
        target_pane_id
    );
}

#[test]
fn placement_interpolation_reaches_the_target_without_overshooting() {
    let source_pane_id = PaneId::new();
    let target_pane_id = PaneId::new();
    let tab_id = TabId::new();
    let placement_snapshot = PlacementSnapshot {
        session_id: SessionId::new(),
        source_pane_id,
        source_tab_id: tab_id,
        destination_tab_id: tab_id,
        session_placement_revision: 4,
        client_placement_revision: 7,
        source_tab_snapshot: build_test_placement_tab_snapshot(
            tab_id,
            "work",
            [source_pane_id, target_pane_id],
        ),
        destination_tab_snapshot: None,
        client_snapshot: PlacementClientSnapshot {
            client_snapshot: ClientSnapshot {
                client_id: ClientId::new(),
                client_revision: 7,
                viewport_size: Size {
                    column_count: 20,
                    row_count: 8,
                },
                active_tab_id: tab_id,
                focused_pane_id: Some(source_pane_id),
                lock_mode: LockMode::Normal,
                is_mouse_selection_enabled: false,
            },
            reported_pane_area: Some(PaneArea::Reported(Size {
                column_count: 20,
                row_count: 8,
            })),
        },
        pane_sizing: PaneSizing::default(),
    };
    let proposed_snapshot = build_proposed_placement_snapshot(
        &placement_snapshot,
        &PanePlacementTarget::Swap { target_pane_id },
    )
    .expect("the swap target is in the tab");
    let halfway_snapshot =
        interpolate_placement_snapshot(&placement_snapshot, &proposed_snapshot, 0.5);

    assert_eq!(
        interpolate_placement_snapshot(&placement_snapshot, &proposed_snapshot, 0.0),
        placement_snapshot
    );
    assert_eq!(
        interpolate_placement_snapshot(&placement_snapshot, &proposed_snapshot, 1.0),
        proposed_snapshot
    );
    for pane_id in [source_pane_id, target_pane_id] {
        let from_pane_slot = placement_snapshot
            .source_tab_snapshot
            .tab_snapshot
            .pane_slots
            .iter()
            .find(|pane_slot| pane_slot.pane_id == pane_id)
            .expect("the source slot exists");
        let to_pane_slot = proposed_snapshot
            .source_tab_snapshot
            .tab_snapshot
            .pane_slots
            .iter()
            .find(|pane_slot| pane_slot.pane_id == pane_id)
            .expect("the target slot exists");
        let halfway_pane_slot = halfway_snapshot
            .source_tab_snapshot
            .tab_snapshot
            .pane_slots
            .iter()
            .find(|pane_slot| pane_slot.pane_id == pane_id)
            .expect("the halfway slot exists");
        assert!(
            halfway_pane_slot.outer_rect.origin.column
                >= from_pane_slot
                    .outer_rect
                    .origin
                    .column
                    .min(to_pane_slot.outer_rect.origin.column)
        );
        assert!(
            halfway_pane_slot.outer_rect.origin.column
                <= from_pane_slot
                    .outer_rect
                    .origin
                    .column
                    .max(to_pane_slot.outer_rect.origin.column)
        );
    }
}

#[test]
fn pane_insertion_preview_shows_both_messages_and_terminal_contents() {
    let (session_server, client_id, source_pane_id) = build_test_server_with_pane();
    let mut render_snapshot = build_render_snapshot(&session_server, client_id);
    let tab_id = render_snapshot.client_snapshot.active_tab_id;
    let target_pane_id = PaneId::new();
    let placement_snapshot = PlacementSnapshot {
        session_id: render_snapshot.session_snapshot.session_id,
        source_pane_id,
        source_tab_id: tab_id,
        destination_tab_id: tab_id,
        session_placement_revision: render_snapshot.session_snapshot.session_revision,
        client_placement_revision: render_snapshot.client_snapshot.client_revision,
        source_tab_snapshot: build_test_placement_tab_snapshot(
            tab_id,
            "work",
            [source_pane_id, target_pane_id],
        ),
        destination_tab_snapshot: None,
        client_snapshot: PlacementClientSnapshot {
            client_snapshot: render_snapshot.client_snapshot.clone(),
            reported_pane_area: Some(PaneArea::Reported(Size {
                column_count: 80,
                row_count: 24,
            })),
        },
        pane_sizing: PaneSizing::default(),
    };
    let placement_target = PanePlacementTarget::Split {
        destination_tab_id: tab_id,
        anchor: PanePlacementAnchor::Pane(target_pane_id),
        direction: Direction::Down,
    };
    let placement_display_snapshot =
        build_placement_preview_snapshot(&placement_snapshot, Some(&placement_target));
    render_snapshot.session_snapshot.active_tab_snapshot =
        placement_snapshot.source_tab_snapshot.tab_snapshot.clone();
    let displayed_render_snapshot =
        build_placement_render_snapshot(&render_snapshot, &placement_display_snapshot)
            .expect("the proposed source tab is the displayed tab");
    let viewport_area = Rect::new(0, 0, 80, 24);
    let committed_regions = CommittedRegions::build_core(
        Size {
            column_count: 80,
            row_count: 24,
        },
        0,
    );
    let theme = Theme::default();
    let mut render_buffer = Buffer::empty(viewport_area);
    let placement_presentation = koshi_renderer::snapshot::PanePlacementPresentation {
        source_pane_id,
        target_pane_ids: vec![target_pane_id],
        source_message: koshi_renderer::snapshot::PanePlacementMessage {
            full_text: "Moving pane will insert below".to_string(),
            compact_text: "Moving pane",
            detail_text: None,
        },
        target_message: Some(koshi_renderer::snapshot::PanePlacementMessage {
            full_text: "Other pane will make room below".to_string(),
            compact_text: "Other pane",
            detail_text: None,
        }),
    };

    koshi_renderer::render_frame(
        &displayed_render_snapshot,
        &committed_regions,
        &theme,
        &koshi_renderer::snapshot::KeymapHints::default(),
        None,
        ViewerChrome {
            is_pane_placement_visible: true,
            placement_source_pane_id: Some(source_pane_id),
            ..ViewerChrome::default()
        },
        ImageRenderMode::Placeholder,
        None,
        Some(&placement_presentation),
        Some(&placement_target),
        viewport_area,
        &mut render_buffer,
    );
    draw_placement_target_outline(
        &placement_snapshot,
        &placement_display_snapshot,
        &displayed_render_snapshot,
        &placement_target,
        &theme,
        &committed_regions,
        viewport_area,
        &mut render_buffer,
    );

    assert_eq!(
        get_rendered_pane_heading(
            &render_buffer,
            &committed_regions,
            viewport_area,
            &displayed_render_snapshot,
            source_pane_id,
            29,
        ),
        "Moving pane will insert below"
    );
    assert_eq!(
        get_rendered_pane_heading(
            &render_buffer,
            &committed_regions,
            viewport_area,
            &displayed_render_snapshot,
            target_pane_id,
            31,
        ),
        "Other pane will make room below"
    );
    assert_eq!(
        get_rendered_pane_cell(
            &render_buffer,
            &committed_regions,
            viewport_area,
            &displayed_render_snapshot,
            source_pane_id,
        ),
        "A",
        "the moving pane keeps its terminal cells in the proposed slot"
    );
    assert_eq!(
        get_rendered_pane_cell(
            &render_buffer,
            &committed_regions,
            viewport_area,
            &displayed_render_snapshot,
            target_pane_id,
        ),
        "B",
        "the target pane keeps its terminal cells in the proposed slot"
    );
}

fn get_rendered_pane_heading(
    render_buffer: &Buffer,
    committed_regions: &CommittedRegions,
    viewport_area: Rect,
    displayed_render_snapshot: &RenderSnapshot,
    pane_id: PaneId,
    heading_column_count: u16,
) -> String {
    let pane_slot = displayed_render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter()
        .find(|pane_slot| pane_slot.pane_id == pane_id)
        .expect("the displayed pane has a proposed slot");
    let layout_rect = koshi_renderer::compute_content_rect(
        koshi_renderer::compute_pane_area(committed_regions, viewport_area),
        displayed_render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .tab_size,
    );
    let heading_column = layout_rect.x
        + pane_slot.outer_rect.origin.column
        + (pane_slot.outer_rect.size.column_count - heading_column_count) / 2;
    let heading_row =
        layout_rect.y + pane_slot.outer_rect.origin.row + pane_slot.outer_rect.size.row_count / 2;
    (heading_column..heading_column + heading_column_count)
        .map(|column_index| render_buffer[(column_index, heading_row)].symbol())
        .collect()
}

fn get_rendered_pane_cell(
    render_buffer: &Buffer,
    committed_regions: &CommittedRegions,
    viewport_area: Rect,
    displayed_render_snapshot: &RenderSnapshot,
    pane_id: PaneId,
) -> String {
    let pane_slot = displayed_render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter()
        .find(|pane_slot| pane_slot.pane_id == pane_id)
        .expect("the displayed pane has a proposed slot");
    let content_rect = pane_slot
        .content_rect
        .expect("the displayed pane has a content rect");
    let layout_rect = koshi_renderer::compute_content_rect(
        koshi_renderer::compute_pane_area(committed_regions, viewport_area),
        displayed_render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .tab_size,
    );
    render_buffer[(
        layout_rect.x + content_rect.origin.column,
        layout_rect.y + content_rect.origin.row,
    )]
        .symbol()
        .to_owned()
}

#[test]
fn placement_render_snapshot_replaces_the_active_pane_layout() {
    let (session_server, client_id, source_pane_id) = build_test_server_with_pane();
    let mut render_snapshot = build_render_snapshot(&session_server, client_id);
    let tab_id = render_snapshot.client_snapshot.active_tab_id;
    let target_pane_id = PaneId::new();
    let placement_snapshot = PlacementSnapshot {
        session_id: render_snapshot.session_snapshot.session_id,
        source_pane_id,
        source_tab_id: tab_id,
        destination_tab_id: tab_id,
        session_placement_revision: render_snapshot.session_snapshot.session_revision,
        client_placement_revision: render_snapshot.client_snapshot.client_revision,
        source_tab_snapshot: build_test_placement_tab_snapshot(
            tab_id,
            "work",
            [source_pane_id, target_pane_id],
        ),
        destination_tab_snapshot: None,
        client_snapshot: PlacementClientSnapshot {
            client_snapshot: render_snapshot.client_snapshot.clone(),
            reported_pane_area: Some(PaneArea::Reported(Size {
                column_count: 20,
                row_count: 8,
            })),
        },
        pane_sizing: PaneSizing::default(),
    };
    let proposed_snapshot = build_proposed_placement_snapshot(
        &placement_snapshot,
        &PanePlacementTarget::Swap { target_pane_id },
    )
    .expect("the test target is in the source tab");
    render_snapshot.session_snapshot.active_tab_snapshot =
        placement_snapshot.source_tab_snapshot.tab_snapshot.clone();

    let displayed_render_snapshot =
        build_placement_render_snapshot(&render_snapshot, &proposed_snapshot)
            .expect("the source tab is the active tab");

    assert_eq!(
        displayed_render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .pane_slots,
        proposed_snapshot
            .source_tab_snapshot
            .tab_snapshot
            .pane_slots
    );
    assert_eq!(
        displayed_render_snapshot.client_snapshot.focused_pane_id,
        Some(source_pane_id)
    );
    assert_eq!(
        displayed_render_snapshot.pane_snapshots[0]
            .terminal_grid_view
            .as_ref(),
        proposed_snapshot
            .source_tab_snapshot
            .pane_snapshots
            .iter()
            .find(|pane_snapshot| pane_snapshot.pane_id == source_pane_id)
            .expect("the source content is in the placement snapshot")
            .terminal_grid_view
            .as_ref()
    );
}

#[test]
fn placement_render_snapshot_shows_a_cross_tab_destination_in_the_pane_area() {
    let (session_server, client_id, source_pane_id) = build_test_server_with_pane();
    let render_snapshot = build_render_snapshot(&session_server, client_id);
    let source_tab_id = render_snapshot.client_snapshot.active_tab_id;
    let destination_tab_id = TabId::new();
    let destination_pane_ids = [PaneId::new(), PaneId::new()];
    let placement_snapshot = PlacementSnapshot {
        session_id: render_snapshot.session_snapshot.session_id,
        source_pane_id,
        source_tab_id,
        destination_tab_id,
        session_placement_revision: render_snapshot.session_snapshot.session_revision,
        client_placement_revision: render_snapshot.client_snapshot.client_revision,
        source_tab_snapshot: build_test_placement_tab_snapshot(
            source_tab_id,
            "source",
            [source_pane_id, PaneId::new()],
        ),
        destination_tab_snapshot: Some(build_test_placement_tab_snapshot(
            destination_tab_id,
            "destination",
            destination_pane_ids,
        )),
        client_snapshot: PlacementClientSnapshot {
            client_snapshot: render_snapshot.client_snapshot.clone(),
            reported_pane_area: Some(PaneArea::Reported(Size {
                column_count: 20,
                row_count: 8,
            })),
        },
        pane_sizing: PaneSizing::default(),
    };

    let displayed_render_snapshot =
        build_placement_render_snapshot(&render_snapshot, &placement_snapshot)
            .expect("the destination tab is carried by the placement snapshot");

    assert_eq!(
        displayed_render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .tab_id,
        destination_tab_id
    );
    assert_eq!(
        displayed_render_snapshot.client_snapshot.active_tab_id,
        destination_tab_id
    );
    assert_eq!(
        displayed_render_snapshot.client_snapshot.focused_pane_id,
        None
    );
    assert_eq!(
        displayed_render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .pane_slots
            .iter()
            .map(|pane_slot| pane_slot.pane_id)
            .collect::<Vec<_>>(),
        destination_pane_ids
    );
    assert_eq!(
        displayed_render_snapshot
            .pane_snapshots
            .iter()
            .map(|pane_snapshot| pane_snapshot.pane_id)
            .collect::<Vec<_>>(),
        destination_pane_ids.to_vec()
    );
}

#[test]
fn target_outline_skips_a_group_member_with_an_empty_rect() {
    let tab_id = TabId::new();
    let (left_pane_id, right_pane_id, collapsed_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut placement_tab_snapshot =
        build_test_placement_tab_snapshot(tab_id, "work", [left_pane_id, right_pane_id]);
    let right_outer_rect = placement_tab_snapshot
        .tab_snapshot
        .pane_slots
        .iter()
        .find(|pane_slot| pane_slot.pane_id == right_pane_id)
        .expect("the right pane has a slot")
        .outer_rect;
    // The second leaf of a collapsed stack member solves to an empty rect.
    placement_tab_snapshot
        .tab_snapshot
        .pane_slots
        .push(PaneSlot {
            pane_id: collapsed_pane_id,
            outer_rect: CoreRect::from_size_at_origin(Size {
                column_count: 0,
                row_count: 0,
            }),
            content_rect: None,
            is_visible: false,
            is_suppressed: false,
        });
    let tab_size = placement_tab_snapshot.tab_snapshot.tab_size;
    let layout_rect = Rect::new(0, 0, tab_size.column_count, tab_size.row_count);
    let placement_target = PanePlacementTarget::Split {
        destination_tab_id: tab_id,
        anchor: PanePlacementAnchor::Group(vec![right_pane_id, collapsed_pane_id]),
        direction: Direction::Right,
    };

    assert_eq!(
        build_target_outline_rect(
            &placement_tab_snapshot,
            &placement_target,
            tab_size,
            layout_rect,
        ),
        Some(Rect::new(
            right_outer_rect.origin.column,
            right_outer_rect.origin.row,
            right_outer_rect.size.column_count,
            right_outer_rect.size.row_count,
        ))
    );
}

#[test]
fn placement_tab_interpolation_switches_headers_and_layout_mode_at_halfway() {
    let tab_id = TabId::new();
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let from_tab_snapshot =
        build_test_placement_tab_snapshot(tab_id, "work", [left_pane_id, right_pane_id]);
    let mut to_tab_snapshot = from_tab_snapshot.clone();
    to_tab_snapshot.tab_snapshot.stack_headers = vec![koshi_layout::solver::StackHeader {
        pane_id: left_pane_id,
        header_rect: CoreRect::from_origin_and_size(
            CorePoint { column: 0, row: 0 },
            Size {
                column_count: 24,
                row_count: 1,
            },
        ),
        member_index: 0,
        member_count: 2,
    }];
    to_tab_snapshot.tab_snapshot.layout_mode = LayoutMode::Fullscreen {
        focused_pane_id: right_pane_id,
    };
    to_tab_snapshot.tab_snapshot.is_every_pane_suppressed = true;

    let before_halfway_tab_snapshot =
        interpolate_placement_tab_snapshot(&from_tab_snapshot, &to_tab_snapshot, 0.25);
    let after_halfway_tab_snapshot =
        interpolate_placement_tab_snapshot(&from_tab_snapshot, &to_tab_snapshot, 0.75);

    assert_eq!(
        before_halfway_tab_snapshot.tab_snapshot.stack_headers,
        from_tab_snapshot.tab_snapshot.stack_headers
    );
    assert_eq!(
        before_halfway_tab_snapshot.tab_snapshot.layout_mode,
        LayoutMode::Tiled
    );
    assert!(
        !before_halfway_tab_snapshot
            .tab_snapshot
            .is_every_pane_suppressed
    );
    assert_eq!(
        after_halfway_tab_snapshot.tab_snapshot.stack_headers,
        to_tab_snapshot.tab_snapshot.stack_headers
    );
    assert_eq!(
        after_halfway_tab_snapshot.tab_snapshot.layout_mode,
        LayoutMode::Fullscreen {
            focused_pane_id: right_pane_id,
        }
    );
    assert!(
        after_halfway_tab_snapshot
            .tab_snapshot
            .is_every_pane_suppressed
    );
}

#[test]
fn target_outline_leaves_cells_on_the_source_border_unstyled() {
    let theme = Theme::default();
    let mut render_buffer = Buffer::empty(Rect::new(0, 0, 10, 4));
    let target_outline = Rect::new(0, 0, 6, 4);
    let source_pane_rect = Rect::new(5, 0, 5, 4);

    apply_placement_target_outline_style(
        target_outline,
        Some(source_pane_rect),
        &theme,
        &mut render_buffer,
    );

    let is_styled_outline_cell = |column_index: u16, row_index: u16| {
        let buffer_cell = &render_buffer[(column_index, row_index)];
        buffer_cell.fg == theme.accent_color && buffer_cell.modifier.contains(Modifier::BOLD)
    };
    for column_index in 0..5 {
        assert!(
            is_styled_outline_cell(column_index, 0),
            "top edge {column_index}"
        );
        assert!(
            is_styled_outline_cell(column_index, 3),
            "bottom edge {column_index}"
        );
    }
    for row_index in 0..4 {
        assert!(
            is_styled_outline_cell(0, row_index),
            "left edge {row_index}"
        );
        assert!(
            !is_styled_outline_cell(5, row_index),
            "shared source border {row_index}"
        );
    }
    assert!(
        !is_styled_outline_cell(2, 1),
        "the outline interior stays unstyled"
    );
    assert!(
        !is_styled_outline_cell(8, 0),
        "a cell outside the outline stays unstyled"
    );
}

fn build_test_placement_tab_snapshot(
    tab_id: TabId,
    tab_name: &str,
    pane_ids: [PaneId; 2],
) -> PlacementTabSnapshot {
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        pane_ids.iter().copied().map(LayoutNode::Pane).collect(),
    ));
    let tab_size = Size {
        column_count: 48,
        row_count: 8,
    };
    let pane_sizing = PaneSizing::default();
    let layout_solve = solve_layout_with_mode(
        &layout_tree,
        LayoutMode::Tiled,
        CoreRect::from_size_at_origin(tab_size),
        pane_sizing,
    );
    let content_rect_by_pane_id = koshi_layout::content::list_content_rects(&layout_solve)
        .into_iter()
        .collect::<HashMap<_, _>>();
    let pane_slots = layout_solve
        .pane_rects
        .iter()
        .map(|(pane_id, outer_rect)| PaneSlot {
            pane_id: *pane_id,
            outer_rect: *outer_rect,
            content_rect: content_rect_by_pane_id.get(pane_id).copied().flatten(),
            is_visible: true,
            is_suppressed: false,
        })
        .collect::<Vec<_>>();
    let pane_snapshots = pane_ids
        .iter()
        .enumerate()
        .map(|(pane_index, pane_id)| {
            let mut test_grid = Grid::build_blank(4, 8, TerminalStyle::default());
            *test_grid
                .get_cell_mut(0, 0)
                .expect("the test grid has a first cell") = Cell::from_character(
                char::from(b'A' + u8::try_from(pane_index).expect("two panes fit in a byte")),
                1,
                TerminalStyle::default(),
            );
            PlacementPaneSnapshot {
                pane_id: *pane_id,
                terminal_grid_view: Some(GridView {
                    grid: Arc::new(test_grid),
                    view_row_offset: 0,
                }),
                image_placement_snapshots: if pane_index == 0 {
                    vec![
                        koshi_renderer::snapshot::ImagePlacementSnapshot::build_unavailable(
                            1,
                            1,
                            (1, 1),
                            1,
                            1,
                        )
                        .expect("the test image placement is valid"),
                    ]
                } else {
                    Vec::new()
                },
            }
        })
        .collect::<Vec<_>>();
    PlacementTabSnapshot {
        layout_tree,
        tab_snapshot: TabSnapshot {
            tab_id,
            tab_name: tab_name.to_owned(),
            pane_slots,
            tab_size,
            stack_headers: layout_solve.stack_headers,
            layout_mode: LayoutMode::Tiled,
            is_every_pane_suppressed: layout_solve.is_every_pane_suppressed,
            gap_cell_count: pane_sizing.gap_cell_count,
        },
        pane_snapshots,
    }
}

/// One pane slot at `outer_rect`. A `None` `content_rect` marks a collapsed
/// stack member.
fn build_committed_test_pane_slot(
    pane_id: PaneId,
    outer_rect: CoreRect,
    content_rect: Option<CoreRect>,
) -> PaneSlot {
    PaneSlot {
        pane_id,
        outer_rect,
        content_rect,
        is_visible: content_rect.is_some(),
        is_suppressed: false,
    }
}

#[test]
fn committed_placement_slide_moves_shown_panes_and_keeps_every_other_slot_at_its_new_place() {
    let moved_pane_id = PaneId::new();
    let arriving_pane_id = PaneId::new();
    let leaving_pane_id = PaneId::new();
    let collapsing_pane_id = PaneId::new();
    let half_width_size = Size {
        column_count: 40,
        row_count: 12,
    };
    let build_rect = |column: u16, row: u16, rect_size: Size| {
        CoreRect::from_origin_and_size(Point { column, row }, rect_size)
    };
    let tab_id = TabId::new();
    let from_tab_snapshot = TabSnapshot {
        tab_id,
        tab_name: String::from("work"),
        pane_slots: vec![
            build_committed_test_pane_slot(
                moved_pane_id,
                build_rect(0, 0, half_width_size),
                Some(build_rect(0, 0, half_width_size).compute_inner_with_border()),
            ),
            build_committed_test_pane_slot(
                leaving_pane_id,
                build_rect(40, 0, half_width_size),
                Some(build_rect(40, 0, half_width_size).compute_inner_with_border()),
            ),
            build_committed_test_pane_slot(
                collapsing_pane_id,
                build_rect(40, 12, half_width_size),
                Some(build_rect(40, 12, half_width_size).compute_inner_with_border()),
            ),
        ],
        tab_size: Size {
            column_count: 80,
            row_count: 24,
        },
        stack_headers: Vec::new(),
        layout_mode: LayoutMode::Tiled,
        is_every_pane_suppressed: false,
        gap_cell_count: 0,
    };
    let collapsed_header_rect = build_rect(
        40,
        12,
        Size {
            column_count: 40,
            row_count: 1,
        },
    );
    let mut to_tab_snapshot = from_tab_snapshot.clone();
    to_tab_snapshot.pane_slots = vec![
        build_committed_test_pane_slot(
            moved_pane_id,
            build_rect(40, 0, half_width_size),
            Some(build_rect(40, 0, half_width_size).compute_inner_with_border()),
        ),
        build_committed_test_pane_slot(
            arriving_pane_id,
            build_rect(0, 0, half_width_size),
            Some(build_rect(0, 0, half_width_size).compute_inner_with_border()),
        ),
        build_committed_test_pane_slot(collapsing_pane_id, collapsed_header_rect, None),
    ];
    to_tab_snapshot.stack_headers = vec![koshi_layout::solver::StackHeader {
        pane_id: collapsing_pane_id,
        header_rect: collapsed_header_rect,
        member_index: 1,
        member_count: 2,
    }];

    let halfway_tab_snapshot =
        interpolate_committed_placement_tab_snapshot(&from_tab_snapshot, &to_tab_snapshot, 0.5);
    let mut expected_halfway_tab_snapshot = to_tab_snapshot.clone();
    expected_halfway_tab_snapshot.pane_slots[0] = build_committed_test_pane_slot(
        moved_pane_id,
        build_rect(20, 0, half_width_size),
        Some(build_rect(20, 0, half_width_size).compute_inner_with_border()),
    );
    assert_eq!(
        halfway_tab_snapshot, expected_halfway_tab_snapshot,
        "the moved pane is halfway, the arriving and collapsing panes sit at their \
         is_frame_committed slots, the leaving pane is gone, and the is_frame_committed headers show"
    );

    let early_tab_snapshot =
        interpolate_committed_placement_tab_snapshot(&from_tab_snapshot, &to_tab_snapshot, 0.25);
    assert_eq!(
        early_tab_snapshot.stack_headers,
        Vec::new(),
        "the headers on screen stay until the slide is halfway"
    );
    assert_eq!(
        early_tab_snapshot.pane_slots[0].outer_rect,
        build_rect(10, 0, half_width_size)
    );
    assert_eq!(
        interpolate_committed_placement_tab_snapshot(&from_tab_snapshot, &to_tab_snapshot, 1.0),
        to_tab_snapshot
    );
    let mut expected_start_tab_snapshot = to_tab_snapshot.clone();
    expected_start_tab_snapshot.pane_slots[0] = from_tab_snapshot.pane_slots[0].clone();
    expected_start_tab_snapshot.stack_headers = Vec::new();
    assert_eq!(
        interpolate_committed_placement_tab_snapshot(&from_tab_snapshot, &to_tab_snapshot, -1.0),
        expected_start_tab_snapshot,
        "a progress below 0.0 draws the start of the slide"
    );
}
