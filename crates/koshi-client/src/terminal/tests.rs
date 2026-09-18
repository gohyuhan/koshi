//! Tests for the outer terminal an attached client owns: the viewer built for
//! it, painting a frame, the cursor-style mapping, and the window title. A fake PTY
//! backend stands in for real children and ratatui's `TestBackend` renders into
//! an in-memory buffer, so painting runs without a terminal. Platform terminal
//! setup and the input reader are TTY-bound; event conversion is tested here,
//! and key decoding is covered in `koshi-input`.

use super::*;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use ratatui::backend::TestBackend;
use ratatui::layout::{Position, Rect};

use koshi_config::layer::{PartialColorPalette, PartialKeybindingsConfig, PartialThemeConfig};
use koshi_config::types::RgbColor;
use koshi_core::command::{Command, CommandEnvelope, CommandSource};
use koshi_core::ids::{CommandId, PaneId, SessionId};
use koshi_core::key::{Key, KeyChord, KeyEventKind, KeyIdentity, KeyModifierFlags, ModFlags};
use koshi_core::process::PtySize;
use koshi_input::host::{GraphicAttributeError, GraphicAttributeReply, KeyCode};
use koshi_ipc::protocol::WireMouseAction;
use koshi_pty::backend::state::PtyBackend;
use koshi_renderer::snapshot::{CommittedRegions, RenderSnapshot};
use koshi_renderer::{build_image_cell_snapshot, build_image_paints};
use koshi_runtime::runtime::bus::EventFilter;
use koshi_runtime::server::Server;
use koshi_terminal::engine::TerminalEngine;
use koshi_test_support::fake_pty::FakePtyBackend;

use koshi_link::config::LoadedConfig;

use crate::tests::TEST_VIEWPORT_SIZE;

fn build_committed_regions(viewport_size: Size) -> CommittedRegions {
    CommittedRegions::core(viewport_size, 0)
}

fn build_terminal_probe(graphics_support: GraphicsSupport) -> TerminalProbe {
    TerminalProbe {
        graphics_support,
        cell_size: None,
    }
}

struct ProbeSource {
    events: VecDeque<Event>,
}

impl reader::EventSource for ProbeSource {
    fn try_read_event(&mut self, _timeout: Option<Duration>) -> io::Result<Option<Event>> {
        Ok(self.events.pop_front())
    }
}

fn probe_reader(events: impl IntoIterator<Item = Event>) -> InputReader<ProbeSource> {
    InputReader::from_event_source_for_tests(ProbeSource {
        events: events.into_iter().collect(),
    })
}

/// A bootstrapped server driven by `fake`, with its client id and sole pane id.
fn build_test_server_with_pane(fake: &Arc<FakePtyBackend>) -> (Server, ClientId, PaneId) {
    let backend: Arc<dyn PtyBackend> = fake.clone();
    let (tx, rx) = mpsc::channel();
    let mut server = Server::from_runtime_parts(backend, rx, tx);
    let client_id = server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake.list_spawned_pane_ids()[0];
    (server, client_id, pane_id)
}

/// A client half for `client_id`, subscribed to `server`'s events, built the
/// way the launch builds it — through [`build_client_with_loaded_config`].
fn build_test_client(server: &mut Server, client_id: ClientId) -> Client {
    build_test_client_with_config(server, client_id, LoadedConfig::default())
}

/// The same, on the config files `loaded` stands for.
fn build_test_client_with_config(
    server: &mut Server,
    client_id: ClientId,
    loaded_config: LoadedConfig,
) -> Client {
    let events = server.subscribe(client_id, EventFilter::All);
    build_client_with_loaded_config(
        client_id,
        TEST_VIEWPORT_SIZE,
        events,
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
        .map(|cell| cell.symbol())
        .collect()
}

struct FailOnWrite {
    fail_at: usize,
    writes: usize,
    written_bytes: Vec<u8>,
}

impl Write for FailOnWrite {
    fn write(&mut self, write_bytes: &[u8]) -> io::Result<usize> {
        let write_index = self.writes;
        self.writes += 1;
        if write_index == self.fail_at {
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
    sequence: &'static [u8],
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
                .windows(self.sequence.len())
                .position(|window| window == self.sequence)
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

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.0.draw(content)
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
    fn clear_region(&mut self, region: ratatui::backend::ClearType) -> io::Result<()> {
        self.0.clear_region(region)
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

fn opaque_image_pixels() -> Vec<u8> {
    [
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 255, 255],
    ]
    .into_iter()
    .flat_map(|pixel| pixel.repeat(4))
    .collect()
}

fn second_opaque_image_pixels() -> Vec<u8> {
    [
        [255, 255, 0, 255],
        [0, 255, 255, 255],
        [255, 0, 255, 255],
        [0, 0, 0, 255],
    ]
    .into_iter()
    .flat_map(|pixel| pixel.repeat(4))
    .collect()
}

fn build_protocol_image_input(
    protocol: koshi_terminal::graphics::GraphicsProtocol,
    rgba_pixel_bytes: Vec<u8>,
    image_content_id: u32,
) -> Vec<u8> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    let image = koshi_image::DecodedImage {
        pixel_width: 4,
        pixel_height: 4,
        rgba_bytes: rgba_pixel_bytes,
    };
    match protocol {
        koshi_terminal::graphics::GraphicsProtocol::Kitty => format!(
            "\x1b_Ga=T,f=32,s=4,v=4,i={image_content_id},c=4,r=4,C=1,q=2;{}\x1b\\",
            STANDARD.encode(image.rgba_bytes)
        )
        .into_bytes(),
        koshi_terminal::graphics::GraphicsProtocol::Iterm2 => {
            let mut encoder = koshi_iterm::ItermEncoder::from_image(
                &image,
                koshi_iterm::ItermOutputOptions::from_cell_dimensions(4, 4)
                    .expect("image dimensions"),
            )
            .expect("iTerm image encoding");
            let mut iterm_output_bytes = Vec::new();
            while let Some(packet) = encoder.take_next_packet() {
                iterm_output_bytes.extend_from_slice(packet);
            }
            iterm_output_bytes
        }
        koshi_terminal::graphics::GraphicsProtocol::Sixel => {
            let mut encoder = koshi_sixel::SixelEncoder::from_image(Arc::new(image), [0, 0, 0])
                .expect("Sixel image encoding");
            let mut sixel_output_bytes = b"\x1b[?80l".to_vec();
            encoder
                .write_to(&mut sixel_output_bytes)
                .expect("Sixel image output");
            sixel_output_bytes
        }
    }
}

fn build_two_image_input(protocol: koshi_terminal::graphics::GraphicsProtocol) -> Vec<u8> {
    let mut terminal_input_bytes = b"\x1b[22;1Hscroll-test\x1b[2;2H".to_vec();
    terminal_input_bytes.extend_from_slice(&build_protocol_image_input(
        protocol,
        opaque_image_pixels(),
        42,
    ));
    terminal_input_bytes.extend_from_slice(b"\x1b[4;4H");
    terminal_input_bytes.extend_from_slice(&build_protocol_image_input(
        protocol,
        second_opaque_image_pixels(),
        43,
    ));
    terminal_input_bytes
}

fn build_source_image_pixel_map(snapshot: &RenderSnapshot) -> BTreeMap<(u16, u16), [u8; 4]> {
    let mut image_pixel_by_position = BTreeMap::new();
    for image_paint in build_image_paints(
        snapshot,
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
                let pixel = image_paint.image_record.image.rgba_bytes
                    [pixel_start_byte_index..pixel_start_byte_index + 4]
                    .try_into()
                    .expect("source pixel");
                image_pixel_by_position.insert(
                    (
                        image_paint.target_area.y + target_row_offset,
                        image_paint.target_area.x + target_column_offset,
                    ),
                    pixel,
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
        let geometry = image_placement.get_image_geometry();
        for image_row_index in 0..image_placement.get_image_cell_dimensions().0 {
            for image_column_index in 0..image_placement.get_image_cell_dimensions().1 {
                let image_pixel_y = source_pixel_y
                    + u32::from(geometry.cell_offset.row + image_row_index) * source_pixel_height
                        / u32::from(geometry.full_size.row_count);
                let image_pixel_x = source_pixel_x
                    + u32::from(geometry.cell_offset.column + image_column_index)
                        * source_pixel_width
                        / u32::from(geometry.full_size.column_count);
                let pixel_start_byte_index =
                    ((image_pixel_y * image_record.image.pixel_width + image_pixel_x) * 4) as usize;
                let pixel = image_record.image.rgba_bytes
                    [pixel_start_byte_index..pixel_start_byte_index + 4]
                    .try_into()
                    .expect("outer pixel");
                image_pixel_by_position.insert(
                    (
                        image_placement.get_image_anchor().0 + image_row_index,
                        image_placement.get_image_anchor().1 + image_column_index,
                    ),
                    pixel,
                );
            }
        }
    }
    image_pixel_by_position
}

fn build_opaque_image_input(protocol: koshi_terminal::graphics::GraphicsProtocol) -> Vec<u8> {
    use koshi_terminal::graphics::GraphicsProtocol;
    match protocol {
        GraphicsProtocol::Kitty => b"\x1b_Ga=T,f=32,s=4,v=4,i=42,c=4,r=4,C=1,q=2;/wAA//8AAP//AAD//wAA/wD/AP8A/wD/AP8A/wD/AP8AAP//AAD//wAA//8AAP///////////////////////w==\x1b\\".to_vec(),
        GraphicsProtocol::Sixel => b"\x1b[?80l\x1bP0;1q\"1;1;4;4#1;2;100;0;0#1@@@@$#2;2;0;100;0#2AAAA$#3;2;0;0;100#3CCCC$#4;2;100;100;100#4GGGG\x1b\\".to_vec(),
        GraphicsProtocol::Iterm2 => {
            let image = koshi_image::DecodedImage { pixel_width: 4, pixel_height: 4, rgba_bytes: opaque_image_pixels() };
            let mut encoder = koshi_iterm::ItermEncoder::from_image(
                &image,
                koshi_iterm::ItermOutputOptions::from_cell_dimensions(4, 4).unwrap(),
            )
            .unwrap();
            let mut iterm_output_bytes = Vec::new();
            while let Some(packet) = encoder.take_next_packet() {
                iterm_output_bytes.extend_from_slice(packet);
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
            .filter_map(|placement| {
                placement.clone_image_record().map(|image_record| {
                    (
                        placement.get_image_content_id(),
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
    cache: &mut crate::attach::paint::ImageCache,
    content_ids: &mut ImageWireContentIds,
) -> RenderSnapshot {
    use koshi_ipc::frame::{FrameImageChunk, FrameImageTransfer, PaintedFrame};
    content_ids.retain_visible_image_content_ids(render_snapshot);
    let mut wire = koshi_runtime::runtime::frame::wire_frame(render_snapshot);
    for (source_pane_snapshot, wire_pane) in render_snapshot
        .pane_snapshots
        .iter()
        .zip(&mut wire.pane_snapshots)
    {
        for (source_image_placement, wire_image_placement) in source_pane_snapshot
            .image_placement_snapshots
            .iter()
            .zip(&mut wire_pane.image_placement_snapshots)
        {
            let image_record_address = source_image_placement
                .clone_image_record()
                .map_or(0, |image_record| Arc::as_ptr(&image_record) as usize);
            wire_image_placement.image_content_id = content_ids
                .get_or_create_wire_image_content_id(
                    source_image_placement.get_image_content_id(),
                    image_record_address,
                );
        }
    }
    let wire: PaintedFrame = serde_json::from_slice(&serde_json::to_vec(&wire).unwrap()).unwrap();
    let mut resolved_render_snapshot = cache.adopt_painted_frame(Box::new(wire.clone())).unwrap();
    let mut transferred_image_content_ids = HashSet::new();
    for (source_pane_snapshot, wire_pane) in render_snapshot
        .pane_snapshots
        .iter()
        .zip(&wire.pane_snapshots)
    {
        for (source_image_placement, wire_image_placement) in source_pane_snapshot
            .image_placement_snapshots
            .iter()
            .zip(&wire_pane.image_placement_snapshots)
        {
            if !transferred_image_content_ids.insert(wire_image_placement.image_content_id) {
                continue;
            }
            let image_record = source_image_placement
                .get_image_record()
                .expect("source pixels");
            let transfer = FrameImageTransfer {
                image_content_id: wire_image_placement.image_content_id,
                image_record: wire_image_placement
                    .image_record
                    .clone()
                    .expect("wire metadata"),
                image_byte_count: image_record.image.rgba_bytes.len() as u64,
            };
            match cache.start_image_transfer(
                serde_json::from_slice(&serde_json::to_vec(&transfer).unwrap()).unwrap(),
            ) {
                Ok(()) => {}
                Err(crate::attach::paint::ImageAssemblyError::TransferAlreadyComplete {
                    image_content_id,
                }) => {
                    assert_eq!(image_content_id, wire_image_placement.image_content_id);
                    continue;
                }
                Err(error) => panic!("image transfer failed to start: {error}"),
            }
            let chunk = FrameImageChunk {
                image_transfer_id: wire_image_placement.image_content_id,
                byte_offset: 0,
                is_last: true,
                chunk_bytes: image_record.image.rgba_bytes.clone(),
            };
            if let Some(completed_render_snapshot) = cache
                .accept_image_chunk(
                    serde_json::from_slice(&serde_json::to_vec(&chunk).unwrap()).unwrap(),
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
        let fake = Arc::new(FakePtyBackend::new());
        let (mut server, client_id, pane_id) = build_test_server_with_pane(&fake);
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
        let initial = build_render_snapshot(&server, client_id);
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: b"\x1b[1;1HB".to_vec(),
        });
        let changed = build_render_snapshot(&server, client_id);
        let mut pixels = opaque_image_pixels();
        if image_protocol != GraphicsProtocol::Kitty {
            pixels[..4].fill(0);
        }
        let client = build_test_client(&mut server, client_id);
        assert_image_trace_output(
            &client,
            image_protocol,
            &[initial, changed],
            &[(0, 4, opaque_image_pixels()), (0, 4, pixels)],
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
        let fake = Arc::new(FakePtyBackend::new());
        let (mut server, client_id, pane_id) = build_test_server_with_pane(&fake);
        let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
            client_id,
            cell_size: PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size"),
        });
        let mut image = b"\x1b[1;1H".to_vec();
        image.extend_from_slice(&build_opaque_image_input(image_protocol));
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: image,
        });
        let initial = build_render_snapshot(&server, client_id);
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: b"\x1b[1;1HB".to_vec(),
        });
        let repainted = build_render_snapshot(&server, client_id);
        let client = build_test_client(&mut server, client_id);

        for graphics in [
            GraphicsSupport::Kitty,
            GraphicsSupport::Iterm,
            GraphicsSupport::Sixel {
                palette_color_count: 256,
                max_pixel_width: None,
                max_pixel_height: None,
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
                ImageOutputState::from_output_kind(ImageOutputKind::from_support(graphics));
            let mut cache = crate::attach::paint::ImageCache::new();
            let mut content_ids = ImageWireContentIds::default();

            for (stage_index, painted_frame) in [&initial, &repainted].into_iter().enumerate() {
                let snapshot = round_trip_image_frame_through_wire(
                    painted_frame,
                    &mut cache,
                    &mut content_ids,
                );
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stage_output_bytes = Vec::new();
                loop {
                    let committed = paint_frame_with_writer(
                        &mut writer,
                        &mut terminal,
                        &client,
                        &snapshot,
                        &build_committed_regions(TEST_VIEWPORT_SIZE),
                        &ViewerPaint::from_frame(&client, &snapshot),
                        graphics.get_image_render_mode(),
                        &mut image_output_state,
                        Some(PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size")),
                        &mut String::new(),
                        &mut None,
                    )
                    .expect("native frame");
                    stage_output_bytes.extend_from_slice(&std::mem::take(
                        &mut *writer.0.lock().expect("trace lock"),
                    ));
                    if committed && !image_output_state.work_pending() {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "{image_protocol:?} -> {graphics:?}, stage {stage_index} did not settle"
                    );
                    std::thread::yield_now();
                }

                let contains_marker = |marker: &[u8]| {
                    stage_output_bytes
                        .windows(marker.len())
                        .any(|window_bytes| window_bytes == marker)
                };
                let encoded_output_text = String::from_utf8_lossy(&stage_output_bytes);
                match graphics {
                    // Kitty pixels transmit once. A Kitty-source image keeps
                    // its cells under the text, so nothing is placed again. An
                    // iTerm2 or Sixel source loses the overwritten cell, so its
                    // placement geometry changes and is placed again.
                    GraphicsSupport::Kitty => {
                        assert_eq!(
                            contains_marker(b"\x1b_Ga=t"),
                            stage_index == 0,
                            "{image_protocol:?} -> Kitty, stage {stage_index} pixel transmit, bytes: {encoded_output_text:?}"
                        );
                        assert_eq!(
                            contains_marker(b"\x1b_Ga=p"),
                            stage_index == 0 || image_protocol != GraphicsProtocol::Kitty,
                            "{image_protocol:?} -> Kitty, stage {stage_index} placement, bytes: {encoded_output_text:?}"
                        );
                    }
                    // iTerm2 and Sixel pixels live in the cells, so the text
                    // repaint under the image writes the image again.
                    GraphicsSupport::Iterm => assert!(
                        contains_marker(b"\x1b]1337;File="),
                        "{image_protocol:?} -> Iterm, stage {stage_index} did not emit the image: {encoded_output_text:?}"
                    ),
                    GraphicsSupport::Sixel { .. } => assert!(
                        contains_marker(b"\x1bP"),
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

fn assert_pi_scrollbar_output(scrollbar: bool) {
    use koshi_terminal::graphics::GraphicsProtocol;
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, pane_id) = build_test_server_with_pane(&fake);
    let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
        client_id,
        cell_size: PixelCellSize::from_pixel_dimensions(1, 1).unwrap(),
    });
    let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
        pane_id,
        output_bytes: b"\x1b[?1049h".to_vec(),
    });
    let upload = String::from_utf8(build_opaque_image_input(GraphicsProtocol::Kitty))
        .unwrap()
        .replace("c=4,r=4,C=1", "c=4,C=1,y=3,h=1,r=1");
    let mut frames = Vec::new();
    let stages: [(u16, u16, u16); 5] = [(1, 1, 3), (1, 3, 1), (2, 4, 0), (1, 3, 1), (1, 1, 3)];
    let mut expected_frame_pixel_rows = Vec::new();
    for (stage_index, (image_row_index, image_row_count, source_pixel_row_index)) in
        stages.into_iter().enumerate()
    {
        let mut terminal_input_text = String::from("\x1b[?2026h");
        if stage_index != 0 {
            terminal_input_text.push_str("\x1b_Ga=d,d=a,q=2\x1b\\");
        }
        for terminal_row_index in 1..=10 {
            terminal_input_text.push_str(&format!("\x1b[{terminal_row_index};1H\x1b[2K"));
            if terminal_row_index == image_row_index {
                if stage_index == 0 {
                    terminal_input_text.push_str(&upload);
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
                if scrollbar && image_row_count != 1 {
                    let bar = if terminal_row_index == 3 {
                        '┃'
                    } else {
                        '│'
                    };
                    terminal_input_text
                        .push_str(&format!("{terminal_row_text:19}\x1b[90m{bar}\x1b[39m"));
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
        frames.push(build_render_snapshot(&server, client_id));
        expected_frame_pixel_rows.push((
            image_row_index - 1,
            image_row_count,
            opaque_image_pixels()[usize::from(source_pixel_row_index) * 16..].to_vec(),
        ));
    }
    let client = build_test_client(&mut server, client_id);
    assert_image_trace_output(
        &client,
        GraphicsProtocol::Kitty,
        &frames,
        &expected_frame_pixel_rows,
        true,
    );
}

#[test]
fn utf8_border_before_kitty_upload_preserves_pixels() {
    for prefix in ["", "┐"] {
        let mut terminal = TerminalEngine::from_pty_size(PtySize {
            column_count: 20,
            row_count: 10,
        });
        terminal.set_cell_size(PixelCellSize::from_pixel_dimensions(1, 1).unwrap());
        let mut terminal_input_bytes = prefix.as_bytes().to_vec();
        terminal_input_bytes.extend_from_slice(b"\x1b[1;1H");
        terminal_input_bytes.extend_from_slice(&build_opaque_image_input(
            koshi_terminal::graphics::GraphicsProtocol::Kitty,
        ));
        let _ = terminal.process_pty_output(&terminal_input_bytes);
        let pixels = terminal
            .get_terminal_state()
            .list_image_placements_for_view(0)
            .into_iter()
            .map(|placement| placement.get_image_record().image.rgba_bytes.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            pixels,
            [opaque_image_pixels()],
            "prefix {prefix:?}, events {:?}",
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
        let fake = Arc::new(FakePtyBackend::new());
        let (mut server, client_id, pane_id) = build_test_server_with_pane(&fake);
        let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
            client_id,
            cell_size: PixelCellSize::from_pixel_dimensions(1, 1).unwrap(),
        });
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: build_two_image_input(image_protocol),
        });
        let mut frames = vec![build_render_snapshot(&server, client_id)];

        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: b"\x1b[3S".to_vec(),
        });
        frames.push(build_render_snapshot(&server, client_id));

        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: b"\x1b[5S".to_vec(),
        });
        frames.push(build_render_snapshot(&server, client_id));

        let _ = server.handle_runtime_event(RuntimeEvent::ClientMouse {
            client_id,
            request_id: 1,
            mouse_actions: vec![WireMouseAction::Scroll {
                pane_id,
                is_scrolling_up: true,
                scroll_line_count: 8,
            }],
        });
        frames.push(build_render_snapshot(&server, client_id));

        let _ = server.handle_runtime_event(RuntimeEvent::ClientMouse {
            client_id,
            request_id: 2,
            mouse_actions: vec![WireMouseAction::Scroll {
                pane_id,
                is_scrolling_up: false,
                scroll_line_count: 8,
            }],
        });
        frames.push(build_render_snapshot(&server, client_id));

        let expected_frame_pixel_maps = frames
            .iter()
            .map(build_source_image_pixel_map)
            .collect::<Vec<_>>();
        assert!(
            build_image_paints(
                &frames[0],
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
                .any(|pixel| *pixel == [255, 0, 0, 255])
                && expected_frame_pixel_maps[0]
                    .values()
                    .any(|pixel| *pixel == [255, 255, 0, 255]),
            "source {image_protocol:?} must expose distinct pixels from both images"
        );

        let client = build_test_client(&mut server, client_id);
        assert_two_image_trace_output(&client, image_protocol, &frames, &expected_frame_pixel_maps);
    }
}

fn assert_two_image_trace_output(
    client: &Client,
    image_protocol: koshi_terminal::graphics::GraphicsProtocol,
    frames: &[RenderSnapshot],
    expected_frame_pixel_maps: &[BTreeMap<(u16, u16), [u8; 4]>],
) {
    for graphics in [
        GraphicsSupport::Kitty,
        GraphicsSupport::Iterm,
        GraphicsSupport::Sixel {
            palette_color_count: 256,
            max_pixel_width: None,
            max_pixel_height: None,
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
            ImageOutputState::from_output_kind(ImageOutputKind::from_support(graphics));
        let mut cache = crate::attach::paint::ImageCache::new();
        let mut content_ids = ImageWireContentIds::default();
        let mut terminal_engine = TerminalEngine::from_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        });
        terminal_engine.set_cell_size(PixelCellSize::from_pixel_dimensions(1, 1).unwrap());
        let _ = terminal_engine.process_pty_output(b"\x1b[?1049h");

        for (frame_stage_index, (snapshot, expected_pixel_by_position)) in
            frames.iter().zip(expected_frame_pixel_maps).enumerate()
        {
            let mut stage_bytes = Vec::new();
            let snapshot =
                round_trip_image_frame_through_wire(snapshot, &mut cache, &mut content_ids);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let committed = paint_frame_with_writer(
                    &mut writer,
                    &mut terminal,
                    client,
                    &snapshot,
                    &build_committed_regions(TEST_VIEWPORT_SIZE),
                    &ViewerPaint::from_frame(client, &snapshot),
                    graphics.get_image_render_mode(),
                    &mut image_output_state,
                    Some(PixelCellSize::from_pixel_dimensions(1, 1).unwrap()),
                    &mut String::new(),
                    &mut None,
                )
                .unwrap();
                let output_bytes = std::mem::take(&mut *writer.0.lock().unwrap());
                stage_bytes.extend_from_slice(&output_bytes);
                let _ = terminal_engine.process_pty_output(&output_bytes);
                if committed && !image_output_state.work_pending() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "{image_protocol:?} -> {graphics:?}, stage {frame_stage_index} did not settle"
                );
                std::thread::yield_now();
            }

            let terminal_text = terminal_engine
                .get_terminal_state()
                .get_active_grid()
                .list_rows()
                .iter()
                .flat_map(|row| row.iter())
                .map(|cell| cell.get_character())
                .collect::<String>();
            assert!(
                terminal_text.contains("scroll-test"),
                "{image_protocol:?} -> {graphics:?}, stage {frame_stage_index} lost the base-cell frame: {terminal_text:?}"
            );
            let actual_pixel_by_position = build_terminal_image_pixel_map(&terminal_engine);
            assert_eq!(
                &actual_pixel_by_position,
                expected_pixel_by_position,
                "{image_protocol:?} -> {graphics:?}, stage {frame_stage_index}, stream {:?}, events {:?}",
                String::from_utf8_lossy(&stage_bytes),
                terminal_engine.take_graphics_events()
            );
        }
    }
}

fn assert_partial_native_frame_write_recovers(
    graphics_support: GraphicsSupport,
    sequence: &'static [u8],
    split: usize,
) {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, pane_id) = build_test_server_with_pane(&fake);
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
        client_id,
        cell_size,
    });
    let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
        pane_id,
        output_bytes: build_opaque_image_input(koshi_terminal::graphics::GraphicsProtocol::Kitty),
    });
    let snapshot = build_render_snapshot(&server, client_id);
    let client = build_test_client(&mut server, client_id);
    let committed = build_committed_regions(TEST_VIEWPORT_SIZE);
    let area = Rect::new(0, 0, 80, 24);
    let paints = build_image_paints(&snapshot, &committed, area);
    let cells = build_image_cell_snapshot(&snapshot, &committed, area).map(Arc::new);
    let mut image_output_state =
        ImageOutputState::from_output_kind(ImageOutputKind::from_support(graphics_support));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !image_output_state.prepare_frame(&paints, cells.clone(), Some(cell_size)) {
        assert!(
            Instant::now() < deadline,
            "{graphics_support:?} output did not prepare"
        );
        std::thread::yield_now();
    }
    assert!(image_output_state.native_commit_pending());
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut writer = FailInsideSequence {
        sequence,
        split_byte_count: split,
        should_fail_next_write: false,
        has_failed: false,
        written_bytes: Vec::new(),
    };

    let error = paint_frame_with_writer(
        &mut writer,
        &mut terminal,
        &client,
        &snapshot,
        &committed,
        &ViewerPaint::from_frame(&client, &snapshot),
        graphics_support.get_image_render_mode(),
        &mut image_output_state,
        Some(cell_size),
        &mut build_window_title(&snapshot),
        &mut get_cursor_style(&snapshot),
    )
    .expect_err("the partial write is returned");

    let PaintError::Image(error) = error;
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(
        error.to_string(),
        "test writer failed inside a control sequence"
    );
    assert!(writer.has_failed);
    assert!(writer.written_bytes.ends_with(b"\x18\x1b\\\x1b[?2026l"));
    assert!(image_output_state.native_commit_pending());
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
    for (graphics, sequence, split) in [
        (GraphicsSupport::Kitty, &b"\x1b_G"[..], 3),
        (GraphicsSupport::Iterm, &b"\x1b]1337;"[..], 8),
        (
            GraphicsSupport::Sixel {
                palette_color_count: 256,
                max_pixel_width: None,
                max_pixel_height: None,
            },
            &b"\x1bP"[..],
            2,
        ),
    ] {
        assert_partial_native_frame_write_recovers(graphics, sequence, split);
    }
}

fn assert_opaque_protocol_matrix(include_text: bool) {
    use koshi_terminal::graphics::GraphicsProtocol;
    for image_protocol in [
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Iterm2,
        GraphicsProtocol::Sixel,
    ] {
        let fake = Arc::new(FakePtyBackend::new());
        let (mut server, client_id, pane_id) = build_test_server_with_pane(&fake);
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).unwrap();
        let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
            client_id,
            cell_size,
        });
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: build_opaque_image_input(image_protocol),
        });
        let initial = build_render_snapshot(&server, client_id);
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: b"\x1b[3S".to_vec(),
        });
        let cropped = build_render_snapshot(&server, client_id);
        let _ = server.handle_runtime_event(RuntimeEvent::ClientMouse {
            client_id,
            request_id: 1,
            mouse_actions: vec![WireMouseAction::Scroll {
                pane_id,
                is_scrolling_up: true,
                scroll_line_count: 3,
            }],
        });
        let restored = build_render_snapshot(&server, client_id);
        let client = build_test_client(&mut server, client_id);
        assert_image_trace_output(
            &client,
            image_protocol,
            &[initial, cropped, restored],
            &[
                (0, 4, opaque_image_pixels()),
                (0, 1, vec![255; 16]),
                (0, 4, opaque_image_pixels()),
            ],
            include_text,
        );
    }
}

fn assert_image_trace_output(
    client: &Client,
    image_protocol: koshi_terminal::graphics::GraphicsProtocol,
    frames: &[RenderSnapshot],
    expected_frame_pixel_rows: &[(u16, u16, Vec<u8>)],
    include_text: bool,
) {
    for graphics in [
        GraphicsSupport::Kitty,
        GraphicsSupport::Iterm,
        GraphicsSupport::Sixel {
            palette_color_count: 256,
            max_pixel_width: None,
            max_pixel_height: None,
        },
    ] {
        let mut writer = ImageTraceWriter::default();
        let backend = ImageTraceBackend(ratatui::backend::CrosstermBackend::new(if include_text {
            writer.clone()
        } else {
            ImageTraceWriter::default()
        }));
        let mut terminal = Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .unwrap();
        let mut image_output_state =
            ImageOutputState::from_output_kind(ImageOutputKind::from_support(graphics));
        let mut cache = crate::attach::paint::ImageCache::new();
        let mut content_ids = ImageWireContentIds::default();
        let mut terminal_engine = TerminalEngine::from_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        });
        terminal_engine.set_cell_size(PixelCellSize::from_pixel_dimensions(1, 1).unwrap());
        let _ = terminal_engine.process_pty_output(b"\x1b[?1049h");
        for (
            frame_stage_index,
            (snapshot, (pixel_row_origin, pixel_row_count, image_pixel_bytes)),
        ) in frames.iter().zip(expected_frame_pixel_rows).enumerate()
        {
            let mut stage_bytes = Vec::new();
            let snapshot =
                round_trip_image_frame_through_wire(snapshot, &mut cache, &mut content_ids);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let committed = paint_frame_with_writer(
                    &mut writer,
                    &mut terminal,
                    client,
                    &snapshot,
                    &build_committed_regions(TEST_VIEWPORT_SIZE),
                    &ViewerPaint::from_frame(client, &snapshot),
                    graphics.get_image_render_mode(),
                    &mut image_output_state,
                    Some(PixelCellSize::from_pixel_dimensions(1, 1).unwrap()),
                    &mut String::new(),
                    &mut None,
                )
                .unwrap();
                let output_bytes = std::mem::take(&mut *writer.0.lock().unwrap());
                stage_bytes.extend_from_slice(&output_bytes);
                let _ = terminal_engine.process_pty_output(&output_bytes);
                if committed && !image_output_state.work_pending() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "{image_protocol:?} -> {graphics:?}, stage {frame_stage_index} did not settle"
                );
                std::thread::yield_now();
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
                        let pixel: [u8; 4] = render_image_record.image.rgba_bytes
                            [pixel_start_byte_index..pixel_start_byte_index + 4]
                            .try_into()
                            .unwrap();
                        actual_pixels.insert(
                            (
                                image_placement.get_image_anchor().0 + placement_row_index,
                                image_placement.get_image_anchor().1 + placement_column_index,
                            ),
                            pixel,
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
                "{image_protocol:?} -> {graphics:?}, stage {frame_stage_index}, stream {:?}, events {:?}",
                String::from_utf8_lossy(&stage_bytes),
                terminal_engine.take_graphics_events()
            );
        }
    }
}

#[test]
fn the_launch_hands_the_viewer_the_config_files_it_read() {
    // The viewer's settings, colors, and keymap all come from the files the
    // launch read. A launch that built the viewer without them would paint the
    // stock palette over the user's theme and answer the stock keys.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, _pane) = build_test_server_with_pane(&fake);

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
            keybindings: None,
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
    // A keymap layer that validates replaces the built-in keybinding settings.
    // A launch that dropped `loaded.keybindings` would leave the stock 500 ms
    // chord timeout in place.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, _pane) = build_test_server_with_pane(&fake);

    let client = build_test_client_with_config(
        &mut server,
        client_id,
        LoadedConfig {
            app_config_layer: None,
            theme_config_layer: None,
            keybindings: Some(PartialKeybindingsConfig {
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
    // The hint bar is painted from the viewer's own keymap, but which label the
    // mouse-select entry wears depends on session state the frame carries. A
    // frame that dropped that link would keep offering "Mouse Select" while
    // selection was already on.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, _pane_id) = build_test_server_with_pane(&fake);
    let client = build_test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
    let snapshot = build_render_snapshot(&server, client_id);

    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &build_committed_regions(Size {
            column_count: 120,
            row_count: 24,
        }),
        &ViewerPaint::from_frame(&client, &snapshot),
        &mut String::new(),
        &mut None,
    )
    .expect("paint");
    assert!(serialize_terminal_screen_text(&terminal).contains("Mouse Select"));

    server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        SystemTime::now(),
        Command::ToggleMouseSelect,
    ));
    let snapshot = build_render_snapshot(&server, client_id);
    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &build_committed_regions(Size {
            column_count: 120,
            row_count: 24,
        }),
        &ViewerPaint::from_frame(&client, &snapshot),
        &mut String::new(),
        &mut None,
    )
    .expect("paint");

    let painted = serialize_terminal_screen_text(&terminal);
    assert!(painted.contains("Mouse Unselect"), "{painted}");
}

#[test]
fn pty_output_is_painted_to_the_screen() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, pane_id) = build_test_server_with_pane(&fake);

    assert!(server
        .handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            output_bytes: b"hello".to_vec(),
        },)
        .is_continue());

    let client = build_test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let snapshot = build_render_snapshot(&server, client_id);
    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &build_committed_regions(TEST_VIEWPORT_SIZE),
        &ViewerPaint::from_frame(&client, &snapshot),
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
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, pane_id) = build_test_server_with_pane(&fake);
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
    let mut last_cursor = None;
    let snapshot = build_render_snapshot(&server, client_id);
    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &build_committed_regions(TEST_VIEWPORT_SIZE),
        &ViewerPaint::from_frame(&client, &snapshot),
        &mut String::new(),
        &mut last_cursor,
    )
    .expect("paint");

    assert_eq!(
        last_cursor,
        Some(CursorStyle::Shaped {
            shape: CursorShape::Bar,
            blink: false,
        })
    );
}

#[test]
fn painting_a_frame_that_names_no_cursor_style_records_none() {
    // A frame with no focused pane leaves `get_cursor_style` with nothing to
    // report. The record still follows the frame: the next frame that names a
    // style counts as a change and is sent again.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, _pane_id) = build_test_server_with_pane(&fake);
    let client = build_test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut snapshot = build_render_snapshot(&server, client_id);
    snapshot.client_snapshot.focused_pane_id = None;
    let mut last_cursor = Some(CursorStyle::Shaped {
        shape: CursorShape::Block,
        blink: true,
    });

    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &build_committed_regions(TEST_VIEWPORT_SIZE),
        &ViewerPaint::from_frame(&client, &snapshot),
        &mut String::new(),
        &mut last_cursor,
    )
    .expect("paint");

    assert_eq!(last_cursor, None);
}

#[test]
fn painting_records_the_window_title_it_sent() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, pane_id) = build_test_server_with_pane(&fake);
    let client = build_test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut snapshot = build_render_snapshot(&server, client_id);
    snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    snapshot.client_snapshot.focused_pane_id = Some(pane_id);
    snapshot.pane_snapshots[0].pane_id = pane_id;
    snapshot.pane_snapshots[0].pane_title = Some("htop".to_string());
    let mut last_title = String::new();

    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &build_committed_regions(TEST_VIEWPORT_SIZE),
        &ViewerPaint::from_frame(&client, &snapshot),
        &mut last_title,
        &mut None,
    )
    .expect("paint");

    assert_eq!(last_title, "quiet-lake | htop");
}

#[test]
fn a_paint_after_a_title_change_records_the_new_title() {
    // The last title decides whether `SetTitle` is written at all. It tracks every
    // frame, not only the first one.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, _pane_id) = build_test_server_with_pane(&fake);
    let client = build_test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut snapshot = build_render_snapshot(&server, client_id);
    snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    snapshot.client_snapshot.focused_pane_id = None;
    let mut last_title = String::new();
    let paint_frame_with_snapshot = |terminal: &mut Terminal<TestBackend>,
                                     snapshot: &RenderSnapshot,
                                     last_title: &mut String| {
        paint_frame(
            terminal,
            &client,
            snapshot,
            &build_committed_regions(TEST_VIEWPORT_SIZE),
            &ViewerPaint::from_frame(&client, snapshot),
            last_title,
            &mut None,
        )
        .expect("paint");
    };

    paint_frame_with_snapshot(&mut terminal, &snapshot, &mut last_title);
    assert_eq!(last_title, "quiet-lake");

    snapshot.session_snapshot.session_name = "loud-hill".to_string();
    paint_frame_with_snapshot(&mut terminal, &snapshot, &mut last_title);

    assert_eq!(last_title, "loud-hill");
}

#[test]
fn each_pane_cursor_style_maps_to_the_crossterm_command_that_re_emits_it() {
    // koshi copies the focused pane's DECSCUSR style out to the terminal it is
    // itself running in, and crossterm writes these commands as the very same
    // DECSCUSR sequences. So each pair must map to the command whose bytes are
    // the sequence that produced it — `CSI 5 SP q` in, `CSI 5 SP q` out.
    // Nothing else in the suite would catch a swapped arm: a `Bar` sent as
    // `BlinkingUnderScore` renders vim's insert cursor as an underline while
    // every test still passes.
    let create_shaped_cursor_style = |cursor_shape, is_blinking| CursorStyle::Shaped {
        shape: cursor_shape,
        blink: is_blinking,
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
            set_cursor_style(cursor_style),
            expected_cursor_style,
            "{cursor_style:?}"
        );
    }
}

// --- window_title: the outer-terminal title string ---

#[test]
fn window_title_with_no_focused_pane_is_just_the_session_name() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, _pane_id) = build_test_server_with_pane(&fake);
    let mut snapshot = build_render_snapshot(&server, client_id);
    snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    snapshot.client_snapshot.focused_pane_id = None;

    assert_eq!(build_window_title(&snapshot), "quiet-lake");
}

#[test]
fn window_title_with_a_titled_focused_pane_joins_session_and_title() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = build_test_server_with_pane(&fake);
    let mut snapshot = build_render_snapshot(&server, client_id);
    snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    snapshot.client_snapshot.focused_pane_id = Some(pane_id);
    snapshot.pane_snapshots[0].pane_id = pane_id;
    snapshot.pane_snapshots[0].pane_title = Some("htop".to_string());

    assert_eq!(build_window_title(&snapshot), "quiet-lake | htop");
}

#[test]
fn window_title_reads_the_focused_pane_not_the_first_one_listed() {
    // The lookup matches on the pane id. Every other title test lists a single
    // pane; a lookup that took the first entry would pass all of them.
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = build_test_server_with_pane(&fake);
    let mut snapshot = build_render_snapshot(&server, client_id);
    snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    let focused_pane_id = PaneId::new();
    let mut second_pane_snapshot = snapshot.pane_snapshots[0].clone();
    second_pane_snapshot.pane_id = focused_pane_id;
    second_pane_snapshot.pane_title = Some("htop".to_string());
    snapshot.pane_snapshots[0].pane_id = pane_id;
    snapshot.pane_snapshots[0].pane_title = Some("bash".to_string());
    snapshot.pane_snapshots.push(second_pane_snapshot);
    snapshot.client_snapshot.focused_pane_id = Some(focused_pane_id);

    assert_eq!(build_window_title(&snapshot), "quiet-lake | htop");
}

#[test]
fn window_title_with_an_untitled_focused_pane_falls_back_to_the_session_name() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = build_test_server_with_pane(&fake);
    let mut snapshot = build_render_snapshot(&server, client_id);
    snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    snapshot.client_snapshot.focused_pane_id = Some(pane_id);
    snapshot.pane_snapshots[0].pane_id = pane_id;
    snapshot.pane_snapshots[0].pane_title = None;

    assert_eq!(build_window_title(&snapshot), "quiet-lake");
}

#[test]
fn window_title_keeps_a_non_ascii_pane_title_whole() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = build_test_server_with_pane(&fake);
    let mut snapshot = build_render_snapshot(&server, client_id);
    snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    snapshot.client_snapshot.focused_pane_id = Some(pane_id);
    snapshot.pane_snapshots[0].pane_id = pane_id;
    snapshot.pane_snapshots[0].pane_title = Some("日本語 🙂".to_string());

    assert_eq!(build_window_title(&snapshot), "quiet-lake | 日本語 🙂");
}

#[test]
fn window_title_with_an_empty_pane_title_falls_back_to_the_session_name() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = build_test_server_with_pane(&fake);
    let mut snapshot = build_render_snapshot(&server, client_id);
    snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    snapshot.client_snapshot.focused_pane_id = Some(pane_id);
    snapshot.pane_snapshots[0].pane_id = pane_id;
    snapshot.pane_snapshots[0].pane_title = Some(String::new());

    assert_eq!(build_window_title(&snapshot), "quiet-lake");
}

#[test]
fn window_title_with_a_focused_pane_absent_from_the_pane_list_falls_back() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = build_test_server_with_pane(&fake);
    let mut snapshot = build_render_snapshot(&server, client_id);
    snapshot.session_snapshot.session_name = "quiet-lake".to_string();
    snapshot.client_snapshot.focused_pane_id = Some(pane_id);
    // No `PaneSnapshot` carries `pane_id`, so the lookup in `window_title`
    // cannot find a title for it.
    snapshot.pane_snapshots.clear();

    assert_eq!(build_window_title(&snapshot), "quiet-lake");
}

/// The title `window_title` builds for a session named `session_name` holding
/// one focused pane titled `pane_title`, after that frame has travelled the
/// session-to-client wire and been read back by
/// [`to_snapshot`](crate::attach::paint::to_snapshot).
fn title_off_the_wire(session_name: &str, pane_title: &str) -> String {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = build_test_server_with_pane(&fake);
    let mut sent = build_render_snapshot(&server, client_id);
    sent.session_snapshot.session_name = session_name.to_string();
    sent.client_snapshot.focused_pane_id = Some(pane_id);
    for pane_snapshot in &mut sent.pane_snapshots {
        if pane_snapshot.pane_id == pane_id {
            pane_snapshot.pane_title = Some(pane_title.to_string());
        }
    }

    let read_back = crate::attach::paint::build_render_snapshot(
        &koshi_runtime::runtime::frame::wire_frame(&sent),
    );
    build_window_title(&read_back)
}

#[test]
fn a_window_title_can_carry_no_osc_terminator() {
    // `window_title` is written into the viewer's own terminal verbatim,
    // inside `OSC 0; ... BEL` (crossterm `SetTitle`). A session server this
    // client did not build chooses both halves of that title.
    for hostile in [
        "x\u{7}pwned",          // BEL
        "x\u{1b}]0;pwned\u{7}", // ESC
        "x\u{9c}pwned",         // C1 ST
        "x\u{9b}2J",            // C1 CSI
    ] {
        let from_session_name = title_off_the_wire(hostile, "bash");
        assert!(
            !from_session_name.contains(['\u{7}', '\u{1b}', '\u{9c}', '\u{9b}']),
            "an OSC terminator survived into the window title: {from_session_name:?}"
        );
        let from_pane_title = title_off_the_wire("dev", hostile);
        assert!(
            !from_pane_title.contains(['\u{7}', '\u{1b}', '\u{9c}', '\u{9b}']),
            "an OSC terminator survived into the window title: {from_pane_title:?}"
        );
    }
}

#[test]
fn a_window_title_names_the_session_and_the_pane_the_wire_carried() {
    assert_eq!(title_off_the_wire("dev", "bash"), "dev | bash");
    assert_eq!(title_off_the_wire("dev\u{7}", "\u{1b}bash"), "dev | bash");
}

#[test]
fn a_window_title_is_bounded_by_the_pane_title_cap() {
    let cap = koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT;
    let title = title_off_the_wire("dev", &"a".repeat(100_000));

    assert_eq!(title, format!("dev | {}", "a".repeat(cap)));
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
    let mut called = false;
    let support = resolve_graphics_support_for_output(false, || {
        called = true;
        Ok(build_terminal_probe(GraphicsSupport::Kitty))
    })
    .expect("redirected output selects a supported fallback");

    assert_eq!(support, build_terminal_probe(GraphicsSupport::Unsupported));
    assert!(!called);

    let support = resolve_graphics_support_for_output(true, || {
        called = true;
        Ok(build_terminal_probe(GraphicsSupport::Kitty))
    })
    .expect("terminal output accepts the probe result");
    assert_eq!(support, build_terminal_probe(GraphicsSupport::Kitty));
    assert!(called);

    let error = resolve_graphics_support_for_output(true, || {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "query write failed",
        ))
    })
    .expect_err("terminal query errors are returned");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
}

#[test]
fn raw_mode_operation_restores_cooked_mode_after_success() {
    let mut calls = Vec::new();

    let mode_operation_result = run_with_raw_mode(
        &mut calls,
        |calls| {
            calls.push("raw");
            Ok(())
        },
        |calls| {
            calls.push("probe");
            Ok(build_terminal_probe(GraphicsSupport::Kitty))
        },
        |calls| {
            calls.push("cooked");
            Ok(())
        },
    )
    .expect("mode cycle succeeds");

    assert_eq!(
        mode_operation_result,
        build_terminal_probe(GraphicsSupport::Kitty)
    );
    assert_eq!(calls, ["raw", "probe", "cooked"]);
}

#[test]
fn raw_mode_operation_stops_when_raw_mode_entry_fails() {
    let mut calls = Vec::new();

    let error = run_with_raw_mode(
        &mut calls,
        |calls| {
            calls.push("raw");
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "raw error"))
        },
        |calls| {
            calls.push("probe");
            Ok(build_terminal_probe(GraphicsSupport::Kitty))
        },
        |calls| {
            calls.push("cooked");
            Ok(())
        },
    )
    .expect_err("the raw-mode error is returned");

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(calls, ["raw"]);
}

#[test]
fn raw_mode_operation_restores_cooked_mode_after_an_operation_error() {
    let mut calls = Vec::new();

    let error = run_with_raw_mode(
        &mut calls,
        |calls| {
            calls.push("raw");
            Ok(())
        },
        |calls| {
            calls.push("probe");
            Err::<TerminalProbe, _>(io::Error::new(io::ErrorKind::InvalidData, "probe error"))
        },
        |calls| {
            calls.push("cooked");
            Ok(())
        },
    )
    .expect_err("the probe error is returned");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(calls, ["raw", "probe", "cooked"]);
}

#[test]
fn raw_mode_operation_returns_a_cooked_mode_error() {
    let mut calls = Vec::new();

    let error = run_with_raw_mode(
        &mut calls,
        |calls| {
            calls.push("raw");
            Ok(())
        },
        |calls| {
            calls.push("probe");
            Ok(build_terminal_probe(GraphicsSupport::Kitty))
        },
        |calls| {
            calls.push("cooked");
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cooked error",
            ))
        },
    )
    .expect_err("the cooked-mode error is returned");

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(calls, ["raw", "probe", "cooked"]);
}

#[test]
fn raw_mode_operation_keeps_the_operation_error_when_restore_also_fails() {
    let mut calls = Vec::new();

    let error = run_with_raw_mode(
        &mut calls,
        |calls| {
            calls.push("raw");
            Ok(())
        },
        |calls| {
            calls.push("probe");
            Err::<TerminalProbe, _>(io::Error::new(io::ErrorKind::InvalidData, "probe error"))
        },
        |calls| {
            calls.push("cooked");
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cooked error",
            ))
        },
    )
    .expect_err("the probe error is returned");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "probe error");
    assert_eq!(calls, ["raw", "probe", "cooked"]);
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

    let mut reader = probe_reader([
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
            .read_matching_event(|event| matches!(event, Event::Key(_)))
            .expect("the unrelated key remains buffered"),
        Event::Key(KeyCode::Char('k').into())
    );
    assert_eq!(
        reader
            .read_matching_event(|event| matches!(event, Event::Paste(_)))
            .expect("the unrelated paste remains buffered"),
        Event::Paste("pasted".to_string())
    );
}

#[test]
fn terminal_probe_prefers_iterm_file_over_sixel() {
    let mut reader = probe_reader([
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
    let mut reader = probe_reader([Event::PrimaryDeviceAttributes(vec![1, 4])]);

    let probe_result = probe_terminal(&mut Vec::new(), &mut reader).expect("probe reads replies");

    assert_eq!(
        probe_result.graphics_support,
        GraphicsSupport::Sixel {
            palette_color_count: 2,
            max_pixel_width: None,
            max_pixel_height: None,
        }
    );
}

#[test]
fn terminal_probe_caps_sixel_palette_and_maps_zero_geometry_to_unlimited_axes() {
    let mut reader = probe_reader([
        Event::TerminalFeatures(b"Sx".to_vec()),
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Palette(Ok(512))),
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Geometry(Ok((0, 480)))),
    ]);

    let probe_result = probe_terminal(&mut Vec::new(), &mut reader).expect("probe reads replies");

    assert_eq!(
        probe_result.graphics_support,
        GraphicsSupport::Sixel {
            palette_color_count: 256,
            max_pixel_width: None,
            max_pixel_height: Some(480),
        }
    );
}

#[test]
fn terminal_probe_does_not_treat_a_regis_palette_reply_as_sixel_support() {
    let mut reader = probe_reader([
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
    let mut reader = probe_reader([Event::SixelGraphicsAttributeReply(
        GraphicAttributeReply::Geometry(Ok((640, 480))),
    )]);

    let probe_result = probe_terminal(&mut Vec::new(), &mut reader).expect("probe reads replies");

    assert_eq!(
        probe_result.graphics_support,
        GraphicsSupport::Sixel {
            palette_color_count: 2,
            max_pixel_width: Some(640),
            max_pixel_height: Some(480),
        }
    );
}

#[test]
fn terminal_probe_rejects_a_reported_one_color_sixel_palette() {
    let mut reader = probe_reader([
        Event::PrimaryDeviceAttributes(vec![1, 2, 4]),
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Palette(Ok(1))),
    ]);

    let probe_result = probe_terminal(&mut Vec::new(), &mut reader).expect("probe reads replies");

    assert_eq!(probe_result.graphics_support, GraphicsSupport::Unsupported);
}

#[test]
fn terminal_probe_without_replies_preserves_unrelated_input_and_reports_unsupported() {
    let mut reader = probe_reader([
        Event::Key(KeyCode::Char('x').into()),
        Event::Paste("input".to_string()),
    ]);

    let probe_result = probe_terminal(&mut Vec::new(), &mut reader).expect("empty probe completes");

    assert_eq!(probe_result.graphics_support, GraphicsSupport::Unsupported);
    assert_eq!(probe_result.cell_size, None);
    assert_eq!(
        reader
            .read_matching_event(|event| matches!(event, Event::Key(_)))
            .expect("the key is not consumed by the probe"),
        Event::Key(KeyCode::Char('x').into())
    );
    assert_eq!(
        reader
            .read_matching_event(|event| matches!(event, Event::Paste(_)))
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
            max_pixel_width: None,
            max_pixel_height: None,
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
fn sixel_modes_are_saved_before_application_modes() {
    let mut terminal_mode_bytes = Vec::new();
    let graphics = GraphicsSupport::Sixel {
        palette_color_count: 2,
        max_pixel_width: None,
        max_pixel_height: None,
    };

    enable_terminal_modes(&mut terminal_mode_bytes, graphics).expect("terminal modes write");

    assert_eq!(
        terminal_mode_bytes,
        b"\x1b[?80s\x1b[?8452s\x1b[?1070s\x1b[?1049h\x1b[>31u\x1b[?1003h\x1b[?1006h\x1b[?2004h\x1b[?u"
    );
}

#[test]
fn terminal_cleanup_reverses_modes_and_deletes_kitty_images() {
    let mut cleanup_bytes = Vec::new();
    let claimed = AtomicBool::new(false);

    write_terminal_cleanup(&mut cleanup_bytes, GraphicsSupport::Kitty, &claimed)
        .expect("terminal cleanup writes");

    assert_eq!(
        cleanup_bytes,
        b"\x18\x1b\\\x1b_Ga=d,d=A,q=2;\x1b\\\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q"
    );
    assert!(claimed.load(Ordering::Acquire));
}

#[test]
fn sixel_cleanup_aborts_a_control_string_before_restoring_modes() {
    let mut cleanup_bytes = Vec::new();
    let claimed = AtomicBool::new(false);
    let graphics = GraphicsSupport::Sixel {
        palette_color_count: 2,
        max_pixel_width: None,
        max_pixel_height: None,
    };

    write_terminal_cleanup(&mut cleanup_bytes, graphics, &claimed)
        .expect("terminal cleanup writes");

    assert_eq!(
        cleanup_bytes,
        b"\x18\x1b\\\x1b[?80r\x1b[?8452r\x1b[?1070r\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q"
    );
}

#[test]
fn terminal_cleanup_attempts_mode_resets_after_image_delete_fails() {
    let mut writer = FailOnWrite {
        fail_at: 1,
        writes: 0,
        written_bytes: Vec::new(),
    };
    let claimed = AtomicBool::new(false);

    let error = write_terminal_cleanup(&mut writer, GraphicsSupport::Kitty, &claimed)
        .expect_err("the failed delete is returned");

    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(
        writer.written_bytes,
        b"\x18\x1b\\\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q"
    );
}

#[test]
fn terminal_cleanup_cancels_a_partial_kitty_apc_before_deleting_images() {
    let partial = b"\x1b_Ga=t,f=32,s=1,v=1,I=1,q=2,o=z,m=1;AAAA";
    let mut writer = partial.to_vec();

    let claimed = AtomicBool::new(false);
    write_terminal_cleanup(&mut writer, GraphicsSupport::Kitty, &claimed)
        .expect("cleanup writes after the partial packet");
    assert_eq!(
        &writer[partial.len()..],
        b"\x18\x1b\\\x1b_Ga=d,d=A,q=2;\x1b\\\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q"
    );

    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });
    assert!(engine.process_pty_output(&writer).is_empty());
    assert!(engine.take_graphics_events().is_empty());
    assert!(engine.finish_graphics_stream().is_empty());
}

#[test]
fn terminal_application_modes_are_restored_once_across_cleanup_paths() {
    let mut cleanup_bytes = Vec::new();
    let active = AtomicBool::new(true);
    let image_claimed = AtomicBool::new(false);

    restore_application_modes(
        &mut cleanup_bytes,
        GraphicsSupport::Kitty,
        &active,
        &image_claimed,
    )
    .expect("the panic cleanup writes");
    restore_application_modes(
        &mut cleanup_bytes,
        GraphicsSupport::Kitty,
        &active,
        &image_claimed,
    )
    .expect("the unwind cleanup is already complete");

    assert_eq!(
        cleanup_bytes,
        b"\x18\x1b\\\x1b_Ga=d,d=A,q=2;\x1b\\\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q"
    );
    assert!(!active.load(Ordering::Acquire));
    assert!(image_claimed.load(Ordering::Acquire));
}

#[test]
fn terminal_cleanup_skips_image_commands_before_application_modes_are_active() {
    let mut cleanup_bytes = Vec::new();
    let active = AtomicBool::new(false);
    let image_claimed = AtomicBool::new(false);

    restore_application_modes(
        &mut cleanup_bytes,
        GraphicsSupport::Kitty,
        &active,
        &image_claimed,
    )
    .expect("inactive application modes need no cleanup");

    assert!(cleanup_bytes.is_empty());
    assert!(!image_claimed.load(Ordering::Acquire));
}

#[test]
fn host_resize_and_paste_events_keep_their_exact_values() {
    let client_id = ClientId::new();
    let resize = build_terminal_runtime_event(
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
    }) = resize
    else {
        panic!("expected the exact resize event, got {resize:?}");
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

    let paste = build_terminal_runtime_event(client_id, Event::Paste("hello 🐈".to_string()));
    let Some(RuntimeEvent::HostPaste {
        client_id: actual_client_id,
        pasted_text,
    }) = paste
    else {
        panic!("expected the exact paste event, got {paste:?}");
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

    for invalid in [
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
        assert_eq!(compute_pixel_cell_size(invalid), None);
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
    let mut query = CellSizeQuery::from_current_measurement(None, true, false);
    let mut wire = Vec::new();

    assert!(query.update_cell_size_for_resize(None));
    query
        .write_cell_size_request(&mut wire)
        .expect("the first cell-size query writes");
    assert_eq!(wire, b"\x1b[16t");

    assert!(!query.update_cell_size_for_resize(Some(current_cell_size)));
    assert_eq!(query.accept_cell_size_reply(old_cell_size), (None, false));
    assert_eq!(query.get_current_cell_size(), Some(current_cell_size));

    assert_eq!(
        query.accept_cell_size_reply(old_cell_size),
        (None, false),
        "an unsolicited reply cannot overwrite the current resize"
    );
    query
        .write_cell_size_request(&mut wire)
        .expect("an already measured resize needs no replacement query");
    assert_eq!(wire, b"\x1b[16t");
}

#[test]
fn an_old_reply_is_discarded_then_an_unknown_resize_gets_one_new_query() {
    let old_cell_size =
        PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions");
    let mut query = CellSizeQuery::from_current_measurement(None, true, false);
    let mut wire = Vec::new();

    query
        .write_cell_size_request(&mut wire)
        .expect("the initial cell-size query writes");
    assert!(!query.update_cell_size_for_resize(None));
    assert_eq!(query.accept_cell_size_reply(old_cell_size), (None, true));
    query
        .write_cell_size_request(&mut wire)
        .expect("the replacement query writes after the old reply");
    assert_eq!(wire, b"\x1b[16t\x1b[16t");
}

#[test]
fn a_timed_out_probe_does_not_block_a_resize_query() {
    let measured_cell_size =
        PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions");
    let mut query = CellSizeQuery::from_current_measurement(None, true, false);
    let mut wire = Vec::new();

    assert!(query.update_cell_size_for_resize(None));
    query
        .write_cell_size_request(&mut wire)
        .expect("the resize query writes after the timed-out probe");
    assert_eq!(
        query.accept_cell_size_reply(measured_cell_size),
        (Some(measured_cell_size), false)
    );
    assert_eq!(wire, b"\x1b[16t");
}

#[test]
fn an_ipc_reconnect_keeps_an_outer_terminal_query_pending() {
    let old_cell_size =
        PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions");
    let mut query = CellSizeQuery::from_current_measurement(None, true, false);
    let mut wire = Vec::new();

    query
        .write_cell_size_request(&mut wire)
        .expect("the closed connection's query writes");
    assert!(!query.update_cell_size_for_resize(Some(old_cell_size)));
    assert!(!query.update_cell_size_for_resize(None));
    assert_eq!(query.accept_cell_size_reply(old_cell_size), (None, true));
    query
        .write_cell_size_request(&mut wire)
        .expect("the replacement connection's query writes");
    assert_eq!(wire, b"\x1b[16t\x1b[16t");
}

#[test]
fn terminal_runtime_discards_capability_reply_events() {
    use koshi_input::host::GraphicAttributeReply;

    let client_id = ClientId::new();
    for event in [
        Event::PrimaryDeviceAttributes(vec![1, 2]),
        Event::TerminalFeatures(b"F".to_vec()),
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Palette(Ok(256))),
    ] {
        match build_terminal_runtime_event(client_id, event) {
            None => {}
            Some(runtime_event) => panic!("unexpected runtime event: {runtime_event:?}"),
        }
    }
}

#[test]
fn unsupported_paint_writes_no_terminal_image_output() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, pane_id) = build_test_server_with_pane(&fake);
    let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
        pane_id,
        output_bytes: b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\".to_vec(),
    });
    let client = build_test_client(&mut server, client_id);
    let snapshot = build_render_snapshot(&server, client_id);
    assert_eq!(
        snapshot.pane_snapshots[0].image_placement_snapshots.len(),
        1
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut terminal_output_bytes = Vec::new();
    let mut last_title = build_window_title(&snapshot);
    let mut last_cursor = get_cursor_style(&snapshot);
    let mut image_output = ImageOutputState::disabled();

    paint_frame_with_writer(
        &mut terminal_output_bytes,
        &mut terminal,
        &client,
        &snapshot,
        &build_committed_regions(TEST_VIEWPORT_SIZE),
        &ViewerPaint::from_frame(&client, &snapshot),
        ImageRenderMode::Placeholder,
        &mut image_output,
        None,
        &mut last_title,
        &mut last_cursor,
    )
    .expect("placeholder paint");

    assert_eq!(terminal_output_bytes, Vec::<u8>::new());
}

#[test]
fn image_cleanup_has_one_owner() {
    let claimed = AtomicBool::new(false);

    assert!(claim_image_cleanup(&claimed));
    assert!(!claim_image_cleanup(&claimed));
}

// ------------------------------------ the keyboard enhancement flag query ----

#[test]
fn the_mode_setup_pushes_the_keyboard_flags_and_asks_what_landed() {
    let mut terminal_mode_bytes = Vec::new();

    enable_terminal_modes(&mut terminal_mode_bytes, GraphicsSupport::Unsupported)
        .expect("terminal modes write");

    // The push asks for flags 1|2|4|8|16, and the query follows it in the same
    // batch so the answer describes the push.
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
fn ordinary_typing_still_reaches_a_pane_as_the_character_typed() {
    // Flag 8 would move typing off the plain-byte path into `CSI u` reports
    // whose text no pane encoding reads yet. Without it, a typed character
    // still arrives as itself.
    let mut parser = koshi_input::host::Parser::default();
    parser.process_input_bytes("å".as_bytes());
    let host_event = parser.remove_next_pending_event().expect("one key event");

    let runtime_event = build_terminal_runtime_event(ClientId::new(), host_event)
        .expect("a typed character is input");

    match runtime_event {
        RuntimeEvent::KeyInput { key_input, .. } => {
            assert_eq!(
                key_input.to_binding_chord(),
                Some(KeyChord::from_parts(ModFlags::NONE, Key::Char('å')))
            );
        }
        other_runtime_event => panic!("expected a key, got {other_runtime_event:?}"),
    }
}

#[test]
fn a_release_reaches_the_runtime_and_resolves_no_binding() {
    // The boundary no longer drops what a chord cannot hold. The release
    // travels, and the viewer is what declines to resolve it.
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
    // The whole point of the capture: alternate keys, associated text and the
    // lock modifiers reach the runtime instead of dying at the decode.
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
    // The answer is a terminal reply, not input. It records what the terminal
    // applied and produces nothing for the viewer to act on.
    assert!(
        build_terminal_runtime_event(ClientId::new(), Event::KeyboardEnhancementFlags(31))
            .is_none()
    );
}

#[test]
fn a_typed_key_still_becomes_a_runtime_event() {
    // The added reply arm must not swallow ordinary keys.
    let runtime_event =
        build_terminal_runtime_event(ClientId::new(), Event::Key(KeyCode::Char('x').into()))
            .expect("a typed key is input");
    assert!(matches!(runtime_event, RuntimeEvent::KeyInput { .. }));
}
