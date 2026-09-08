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
use koshi_core::process::PtySize;
use koshi_input::host::{GraphicAttributeError, GraphicAttributeReply, KeyCode};
use koshi_ipc::protocol::WireMouseAction;
use koshi_pty::backend::state::PtyBackend;
use koshi_renderer::image_paints;
use koshi_renderer::snapshot::{CommittedRegions, RenderSnapshot};
use koshi_runtime::runtime::bus::EventFilter;
use koshi_runtime::server::Server;
use koshi_terminal::engine::TerminalEngine;
use koshi_test_support::fake_pty::FakePtyBackend;

use koshi_link::config::LoadedConfig;

use crate::tests::VIEWPORT;

fn regions(viewport: Size) -> CommittedRegions {
    CommittedRegions::core(viewport, 0)
}

fn probe(graphics: GraphicsSupport) -> TerminalProbe {
    TerminalProbe {
        graphics,
        cell_size: None,
    }
}

struct ProbeSource {
    events: VecDeque<Event>,
}

impl reader::EventSource for ProbeSource {
    fn try_read(&mut self, _timeout: Option<Duration>) -> io::Result<Option<Event>> {
        Ok(self.events.pop_front())
    }
}

fn probe_reader(events: impl IntoIterator<Item = Event>) -> InputReader<ProbeSource> {
    InputReader::from_source(ProbeSource {
        events: events.into_iter().collect(),
    })
}

/// A bootstrapped server driven by `fake`, with its client id and sole pane id.
fn boot(fake: &Arc<FakePtyBackend>) -> (Server, ClientId, PaneId) {
    let backend: Arc<dyn PtyBackend> = fake.clone();
    let (tx, rx) = mpsc::channel();
    let mut server = Server::new(backend, rx, tx);
    let client_id = server
        .bootstrap_local(SessionId::new(), VIEWPORT, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake.spawned_panes()[0];
    (server, client_id, pane_id)
}

/// A client half for `client_id`, subscribed to `server`'s events, built the
/// way the launch builds it — through [`viewer`].
fn test_client(server: &mut Server, client_id: ClientId) -> Client {
    test_client_with(server, client_id, LoadedConfig::default())
}

/// The same, on the config files `loaded` stands for.
fn test_client_with(server: &mut Server, client_id: ClientId, loaded: LoadedConfig) -> Client {
    let events = server.subscribe(client_id, EventFilter::All);
    viewer(
        client_id,
        VIEWPORT,
        events,
        TerminalCleanupGuard::new(),
        loaded,
    )
}

/// The whole rendered screen flattened to a string, for substring assertions.
fn screen_text(terminal: &Terminal<TestBackend>) -> String {
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
    output: Vec<u8>,
}

impl Write for FailOnWrite {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let write_index = self.writes;
        self.writes += 1;
        if write_index == self.fail_at {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "test writer failed",
            ));
        }
        self.output.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct FailInsideSequence {
    sequence: &'static [u8],
    split: usize,
    fail_next: bool,
    failed: bool,
    output: Vec<u8>,
}

impl Write for FailInsideSequence {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.fail_next {
            self.fail_next = false;
            self.failed = true;
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "test writer failed inside a control sequence",
            ));
        }
        if !self.failed {
            if let Some(start) = bytes
                .windows(self.sequence.len())
                .position(|window| window == self.sequence)
            {
                let written = start + self.split;
                self.output.extend_from_slice(&bytes[..written]);
                self.fail_next = true;
                return Ok(written);
            }
        }
        self.output.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The frame this client is owed, as the session composes it.
fn frame(server: &Server, client_id: ClientId) -> RenderSnapshot {
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
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("trace lock").extend_from_slice(bytes);
        Ok(bytes.len())
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

fn protocol_image_input(
    protocol: koshi_terminal::graphics::GraphicsProtocol,
    pixels: Vec<u8>,
    image_id: u32,
) -> Vec<u8> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    let image = koshi_image::DecodedImage {
        width: 4,
        height: 4,
        rgba: pixels,
    };
    match protocol {
        koshi_terminal::graphics::GraphicsProtocol::Kitty => format!(
            "\x1b_Ga=T,f=32,s=4,v=4,i={image_id},c=4,r=4,C=1,q=2;{}\x1b\\",
            STANDARD.encode(image.rgba)
        )
        .into_bytes(),
        koshi_terminal::graphics::GraphicsProtocol::Iterm2 => {
            let mut encoder = koshi_iterm::Encoder::new(
                &image,
                koshi_iterm::OutputOptions::new(4, 4).expect("image dimensions"),
            )
            .expect("iTerm image encoding");
            let mut bytes = Vec::new();
            while let Some(packet) = encoder.next_packet() {
                bytes.extend_from_slice(packet);
            }
            bytes
        }
        koshi_terminal::graphics::GraphicsProtocol::Sixel => {
            let mut encoder = koshi_sixel::SixelEncoder::new(Arc::new(image), [0, 0, 0])
                .expect("Sixel image encoding");
            let mut bytes = b"\x1b[?80l".to_vec();
            encoder.write_to(&mut bytes).expect("Sixel image output");
            bytes
        }
    }
}

fn two_image_input(protocol: koshi_terminal::graphics::GraphicsProtocol) -> Vec<u8> {
    let mut bytes = b"\x1b[22;1Hscroll-test\x1b[2;2H".to_vec();
    bytes.extend_from_slice(&protocol_image_input(protocol, opaque_image_pixels(), 42));
    bytes.extend_from_slice(b"\x1b[4;4H");
    bytes.extend_from_slice(&protocol_image_input(
        protocol,
        second_opaque_image_pixels(),
        43,
    ));
    bytes
}

fn source_image_map(snapshot: &RenderSnapshot) -> BTreeMap<(u16, u16), [u8; 4]> {
    let mut map = BTreeMap::new();
    for paint in image_paints(snapshot, &regions(VIEWPORT), Rect::new(0, 0, 80, 24)) {
        for row in 0..paint.target.height {
            for column in 0..paint.target.width {
                let source_y = paint.source.y
                    + u32::from(row) * paint.source.height / u32::from(paint.target.height);
                let source_x = paint.source.x
                    + u32::from(column) * paint.source.width / u32::from(paint.target.width);
                let start = ((source_y * paint.record.image.width + source_x) * 4) as usize;
                let pixel = paint.record.image.rgba[start..start + 4]
                    .try_into()
                    .expect("source pixel");
                map.insert((paint.target.y + row, paint.target.x + column), pixel);
            }
        }
    }
    map
}

fn outer_image_map(outer: &TerminalEngine) -> BTreeMap<(u16, u16), [u8; 4]> {
    let placements = outer.state().image_placements_for_view(0);
    let mut map = BTreeMap::new();
    for placement in &placements {
        let record = placement.render_record_arc();
        let (source_x, source_y, source_width, source_height) =
            record.source_rect().expect("outer source rectangle");
        let geometry = placement.geometry();
        for row in 0..placement.dimensions().0 {
            for column in 0..placement.dimensions().1 {
                let image_y = source_y
                    + u32::from(geometry.offset.y + row) * source_height
                        / u32::from(geometry.full_size.rows);
                let image_x = source_x
                    + u32::from(geometry.offset.x + column) * source_width
                        / u32::from(geometry.full_size.cols);
                let start = ((image_y * record.image.width + image_x) * 4) as usize;
                let pixel = record.image.rgba[start..start + 4]
                    .try_into()
                    .expect("outer pixel");
                map.insert(
                    (placement.anchor().0 + row, placement.anchor().1 + column),
                    pixel,
                );
            }
        }
    }
    map
}

fn opaque_image_input(protocol: koshi_terminal::graphics::GraphicsProtocol) -> Vec<u8> {
    use koshi_terminal::graphics::GraphicsProtocol;
    match protocol {
        GraphicsProtocol::Kitty => b"\x1b_Ga=T,f=32,s=4,v=4,i=42,c=4,r=4,C=1,q=2;/wAA//8AAP//AAD//wAA/wD/AP8A/wD/AP8A/wD/AP8AAP//AAD//wAA//8AAP///////////////////////w==\x1b\\".to_vec(),
        GraphicsProtocol::Sixel => b"\x1b[?80l\x1bP0;1q\"1;1;4;4#1;2;100;0;0#1@@@@$#2;2;0;100;0#2AAAA$#3;2;0;0;100#3CCCC$#4;2;100;100;100#4GGGG\x1b\\".to_vec(),
        GraphicsProtocol::Iterm2 => {
            let image = koshi_image::DecodedImage { width: 4, height: 4, rgba: opaque_image_pixels() };
            let mut encoder = koshi_iterm::Encoder::new(&image, koshi_iterm::OutputOptions::new(4, 4).unwrap()).unwrap();
            let mut bytes = Vec::new();
            while let Some(packet) = encoder.next_packet() {
                bytes.extend_from_slice(packet);
            }
            bytes
        }
    }
}

#[derive(Default)]
struct ImageWireContentIds {
    ids: HashMap<u64, (usize, u64)>,
    next_id: u64,
}

impl ImageWireContentIds {
    fn begin_frame(&mut self, snapshot: &RenderSnapshot) {
        let visible = snapshot
            .panes
            .iter()
            .flat_map(|pane| &pane.image_placements)
            .filter_map(|placement| {
                placement
                    .record()
                    .map(|record| (placement.content_id(), Arc::as_ptr(&record.image) as usize))
            })
            .collect::<HashMap<_, _>>();
        self.ids
            .retain(|canonical_id, (address, _)| visible.get(canonical_id) == Some(address));
    }

    fn id_for(&mut self, canonical_id: u64, image_address: usize) -> u64 {
        if let Some(&(address, wire_id)) = self.ids.get(&canonical_id) {
            assert_eq!(address, image_address);
            return wire_id;
        }
        self.next_id = self.next_id.saturating_add(1);
        let wire_id = self.next_id;
        self.ids.insert(canonical_id, (image_address, wire_id));
        wire_id
    }
}

fn image_frame_through_wire(
    snapshot: &RenderSnapshot,
    cache: &mut crate::attach::paint::ImageCache,
    content_ids: &mut ImageWireContentIds,
) -> RenderSnapshot {
    use koshi_ipc::frame::{FrameImageChunk, FrameImageTransfer, PaintedFrame};
    content_ids.begin_frame(snapshot);
    let mut wire = koshi_runtime::runtime::frame::wire_frame(snapshot);
    for (source_pane, wire_pane) in snapshot.panes.iter().zip(&mut wire.panes) {
        for (source, received) in source_pane
            .image_placements
            .iter()
            .zip(&mut wire_pane.image_placements)
        {
            let image_address = source
                .record()
                .map_or(0, |record| Arc::as_ptr(&record.image) as usize);
            received.content_id = content_ids.id_for(source.content_id(), image_address);
        }
    }
    let wire: PaintedFrame = serde_json::from_slice(&serde_json::to_vec(&wire).unwrap()).unwrap();
    let mut resolved = cache.begin_frame(Box::new(wire.clone())).unwrap();
    let mut transferred = HashSet::new();
    for (source, received) in snapshot.panes.iter().zip(&wire.panes) {
        for (source, received) in source
            .image_placements
            .iter()
            .zip(&received.image_placements)
        {
            if !transferred.insert(received.content_id) {
                continue;
            }
            let record = source.record().expect("source pixels");
            let transfer = FrameImageTransfer {
                id: received.content_id,
                record: received.record.clone().expect("wire metadata"),
                byte_len: record.image.rgba.len() as u64,
            };
            match cache
                .start(serde_json::from_slice(&serde_json::to_vec(&transfer).unwrap()).unwrap())
            {
                Ok(()) => {}
                Err(crate::attach::paint::ImageAssemblyError::TransferAlreadyComplete(id)) => {
                    assert_eq!(id, received.content_id);
                    continue;
                }
                Err(error) => panic!("image transfer failed to start: {error}"),
            }
            let chunk = FrameImageChunk {
                transfer_id: received.content_id,
                offset: 0,
                last: true,
                bytes: record.image.rgba.clone(),
            };
            if let Some(frame) = cache
                .accept(serde_json::from_slice(&serde_json::to_vec(&chunk).unwrap()).unwrap())
                .unwrap()
            {
                resolved = Some(frame);
            }
        }
    }
    resolved.expect("all required image records were transferred")
}

#[test]
fn all_input_and_output_protocols_preserve_opaque_pixels_through_scrolling() {
    assert_opaque_protocol_matrix(true);
}

#[test]
fn all_output_protocols_match_source_image_coverage_after_text_overwrite() {
    use koshi_terminal::graphics::GraphicsProtocol;
    for source in [
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Iterm2,
        GraphicsProtocol::Sixel,
    ] {
        let fake = Arc::new(FakePtyBackend::new());
        let (mut server, client_id, pane_id) = boot(&fake);
        let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
            client_id,
            size: PixelCellSize::new(1, 1).unwrap(),
        });
        let mut bytes = b"A\x1b[1;1H".to_vec();
        bytes.extend_from_slice(&opaque_image_input(source));
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput { pane_id, bytes });
        let initial = frame(&server, client_id);
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            bytes: b"\x1b[1;1HB".to_vec(),
        });
        let changed = frame(&server, client_id);
        let mut pixels = opaque_image_pixels();
        if source != GraphicsProtocol::Kitty {
            pixels[..4].fill(0);
        }
        let client = test_client(&mut server, client_id);
        assert_image_trace_output(
            &client,
            source,
            &[initial, changed],
            &[(0, 4, opaque_image_pixels()), (0, 4, pixels)],
            true,
        );
    }
}

#[test]
fn a_text_repaint_under_an_image_rewrites_only_cell_bound_pixels() {
    use koshi_terminal::graphics::GraphicsProtocol;

    for source in [
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Iterm2,
        GraphicsProtocol::Sixel,
    ] {
        let fake = Arc::new(FakePtyBackend::new());
        let (mut server, client_id, pane_id) = boot(&fake);
        let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
            client_id,
            size: PixelCellSize::new(1, 1).expect("test cell size"),
        });
        let mut image = b"\x1b[1;1H".to_vec();
        image.extend_from_slice(&opaque_image_input(source));
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            bytes: image,
        });
        let initial = frame(&server, client_id);
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            bytes: b"\x1b[1;1HB".to_vec(),
        });
        let repainted = frame(&server, client_id);
        let client = test_client(&mut server, client_id);

        for graphics in [
            GraphicsSupport::Kitty,
            GraphicsSupport::Iterm,
            GraphicsSupport::Sixel {
                palette_colors: 256,
                max_width: None,
                max_height: None,
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
            let mut output = ImageOutputState::new(ImageOutputKind::from_support(graphics));
            let mut cache = crate::attach::paint::ImageCache::new();
            let mut content_ids = ImageWireContentIds::default();

            for (stage, source_snapshot) in [&initial, &repainted].into_iter().enumerate() {
                let snapshot =
                    image_frame_through_wire(source_snapshot, &mut cache, &mut content_ids);
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut bytes = Vec::new();
                loop {
                    let committed = paint_frame_with_writer(
                        &mut writer,
                        &mut terminal,
                        &client,
                        &snapshot,
                        &regions(VIEWPORT),
                        &ViewerPaint::from_frame(&client, &snapshot),
                        graphics.image_mode(),
                        &mut output,
                        Some(PixelCellSize::new(1, 1).expect("test cell size")),
                        &mut String::new(),
                        &mut None,
                    )
                    .expect("native frame");
                    bytes.extend_from_slice(&std::mem::take(
                        &mut *writer.0.lock().expect("trace lock"),
                    ));
                    if committed && !output.work_pending() {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "{source:?} -> {graphics:?}, stage {stage} did not settle"
                    );
                    std::thread::yield_now();
                }

                let contains = |marker: &[u8]| bytes.windows(marker.len()).any(|w| w == marker);
                let shown = String::from_utf8_lossy(&bytes);
                match graphics {
                    // Kitty pixels transmit once. A Kitty-source image keeps
                    // its cells under the text, so nothing is placed again. An
                    // iTerm2 or Sixel source loses the overwritten cell, so its
                    // placement geometry changes and is placed again.
                    GraphicsSupport::Kitty => {
                        assert_eq!(
                            contains(b"\x1b_Ga=t"),
                            stage == 0,
                            "{source:?} -> Kitty, stage {stage} pixel transmit, bytes: {shown:?}"
                        );
                        assert_eq!(
                            contains(b"\x1b_Ga=p"),
                            stage == 0 || source != GraphicsProtocol::Kitty,
                            "{source:?} -> Kitty, stage {stage} placement, bytes: {shown:?}"
                        );
                    }
                    // iTerm2 and Sixel pixels live in the cells, so the text
                    // repaint under the image writes the image again.
                    GraphicsSupport::Iterm => assert!(
                        contains(b"\x1b]1337;File="),
                        "{source:?} -> Iterm, stage {stage} did not emit the image: {shown:?}"
                    ),
                    GraphicsSupport::Sixel { .. } => assert!(
                        contains(b"\x1bP"),
                        "{source:?} -> Sixel, stage {stage} did not emit the image: {shown:?}"
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
    let (mut server, client_id, pane_id) = boot(&fake);
    let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
        client_id,
        size: PixelCellSize::new(1, 1).unwrap(),
    });
    let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
        pane_id,
        bytes: b"\x1b[?1049h".to_vec(),
    });
    let upload = String::from_utf8(opaque_image_input(GraphicsProtocol::Kitty))
        .unwrap()
        .replace("c=4,r=4,C=1", "c=4,C=1,y=3,h=1,r=1");
    let mut frames = Vec::new();
    let stages: [(u16, u16, u16); 5] = [(1, 1, 3), (1, 3, 1), (2, 4, 0), (1, 3, 1), (1, 1, 3)];
    let mut expected = Vec::new();
    for (stage, (image_row, rows, source_y)) in stages.into_iter().enumerate() {
        let mut bytes = String::from("\x1b[?2026h");
        if stage != 0 {
            bytes.push_str("\x1b_Ga=d,d=a,q=2\x1b\\");
        }
        for row in 1..=10 {
            bytes.push_str(&format!("\x1b[{row};1H\x1b[2K"));
            if row == image_row {
                if stage == 0 {
                    bytes.push_str(&upload);
                } else if rows == 4 {
                    bytes.push_str("\x1b_Ga=p,q=2,i=42,c=4,r=4,C=1\x1b\\");
                } else {
                    bytes.push_str(&format!(
                        "\x1b_Ga=p,q=2,i=42,c=4,C=1,y={source_y},h={rows},r={rows}\x1b\\"
                    ));
                }
            } else {
                let text = if row < image_row {
                    "before".to_owned()
                } else if row < image_row + rows {
                    String::new()
                } else {
                    format!("after{}", row - image_row - rows)
                };
                if scrollbar && rows != 1 {
                    let bar = if row == 3 { '┃' } else { '│' };
                    bytes.push_str(&format!("{text:19}\x1b[90m{bar}\x1b[39m"));
                } else {
                    bytes.push_str(&text);
                }
                bytes.push_str("\x1b[0m\x1b]8;;\x1b\\");
            }
        }
        bytes.push_str("\x1b[?2026l");
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            bytes: bytes.into_bytes(),
        });
        frames.push(frame(&server, client_id));
        expected.push((
            image_row - 1,
            rows,
            opaque_image_pixels()[usize::from(source_y) * 16..].to_vec(),
        ));
    }
    let client = test_client(&mut server, client_id);
    assert_image_trace_output(&client, GraphicsProtocol::Kitty, &frames, &expected, true);
}

#[test]
fn utf8_border_before_kitty_upload_preserves_pixels() {
    for prefix in ["", "┐"] {
        let mut terminal = TerminalEngine::new(PtySize { cols: 20, rows: 10 });
        terminal.set_cell_size(PixelCellSize::new(1, 1).unwrap());
        let mut input = prefix.as_bytes().to_vec();
        input.extend_from_slice(b"\x1b[1;1H");
        input.extend_from_slice(&opaque_image_input(
            koshi_terminal::graphics::GraphicsProtocol::Kitty,
        ));
        let _ = terminal.advance(&input);
        let pixels = terminal
            .state()
            .image_placements_for_view(0)
            .into_iter()
            .map(|placement| placement.record().image.rgba.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            pixels,
            [opaque_image_pixels()],
            "prefix {prefix:?}, events {:?}",
            terminal.take_graphics()
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

    for source in [
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Iterm2,
        GraphicsProtocol::Sixel,
    ] {
        let fake = Arc::new(FakePtyBackend::new());
        let (mut server, client_id, pane_id) = boot(&fake);
        let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
            client_id,
            size: PixelCellSize::new(1, 1).unwrap(),
        });
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            bytes: two_image_input(source),
        });
        let mut frames = vec![frame(&server, client_id)];

        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            bytes: b"\x1b[3S".to_vec(),
        });
        frames.push(frame(&server, client_id));

        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            bytes: b"\x1b[5S".to_vec(),
        });
        frames.push(frame(&server, client_id));

        let _ = server.handle_runtime_event(RuntimeEvent::ClientMouse {
            client_id,
            request_id: 1,
            actions: vec![WireMouseAction::Scroll {
                pane: pane_id,
                up: true,
                lines: 8,
            }],
        });
        frames.push(frame(&server, client_id));

        let _ = server.handle_runtime_event(RuntimeEvent::ClientMouse {
            client_id,
            request_id: 2,
            actions: vec![WireMouseAction::Scroll {
                pane: pane_id,
                up: false,
                lines: 8,
            }],
        });
        frames.push(frame(&server, client_id));

        let expected = frames.iter().map(source_image_map).collect::<Vec<_>>();
        assert!(
            image_paints(&frames[0], &regions(VIEWPORT), Rect::new(0, 0, 80, 24)).len() >= 2,
            "source {source:?} must retain both image portions in the first frame"
        );
        assert!(
            expected[0].values().any(|pixel| *pixel == [255, 0, 0, 255])
                && expected[0]
                    .values()
                    .any(|pixel| *pixel == [255, 255, 0, 255]),
            "source {source:?} must expose distinct pixels from both images"
        );

        let client = test_client(&mut server, client_id);
        assert_two_image_trace_output(&client, source, &frames, &expected);
    }
}

fn assert_two_image_trace_output(
    client: &Client,
    source: koshi_terminal::graphics::GraphicsProtocol,
    frames: &[RenderSnapshot],
    expected: &[BTreeMap<(u16, u16), [u8; 4]>],
) {
    for graphics in [
        GraphicsSupport::Kitty,
        GraphicsSupport::Iterm,
        GraphicsSupport::Sixel {
            palette_colors: 256,
            max_width: None,
            max_height: None,
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
        let mut output = ImageOutputState::new(ImageOutputKind::from_support(graphics));
        let mut cache = crate::attach::paint::ImageCache::new();
        let mut content_ids = ImageWireContentIds::default();
        let mut outer = TerminalEngine::new(PtySize { cols: 80, rows: 24 });
        outer.set_cell_size(PixelCellSize::new(1, 1).unwrap());
        let _ = outer.advance(b"\x1b[?1049h");

        for (stage, (snapshot, expected)) in frames.iter().zip(expected).enumerate() {
            let mut stage_bytes = Vec::new();
            let snapshot = image_frame_through_wire(snapshot, &mut cache, &mut content_ids);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let committed = paint_frame_with_writer(
                    &mut writer,
                    &mut terminal,
                    client,
                    &snapshot,
                    &regions(VIEWPORT),
                    &ViewerPaint::from_frame(client, &snapshot),
                    graphics.image_mode(),
                    &mut output,
                    Some(PixelCellSize::new(1, 1).unwrap()),
                    &mut String::new(),
                    &mut None,
                )
                .unwrap();
                let bytes = std::mem::take(&mut *writer.0.lock().unwrap());
                stage_bytes.extend_from_slice(&bytes);
                let _ = outer.advance(&bytes);
                if committed && !output.work_pending() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "{source:?} -> {graphics:?}, stage {stage} did not settle"
                );
                std::thread::yield_now();
            }

            let terminal_text = outer
                .state()
                .active_grid()
                .rows()
                .iter()
                .flat_map(|row| row.iter())
                .map(|cell| cell.ch())
                .collect::<String>();
            assert!(
                terminal_text.contains("scroll-test"),
                "{source:?} -> {graphics:?}, stage {stage} lost the base-cell frame: {terminal_text:?}"
            );
            let actual = outer_image_map(&outer);
            assert_eq!(
                &actual,
                expected,
                "{source:?} -> {graphics:?}, stage {stage}, stream {:?}, events {:?}",
                String::from_utf8_lossy(&stage_bytes),
                outer.take_graphics()
            );
        }
    }
}

fn assert_partial_native_frame_write_recovers(
    graphics: GraphicsSupport,
    sequence: &'static [u8],
    split: usize,
) {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, pane_id) = boot(&fake);
    let cell_size = PixelCellSize::new(1, 1).expect("test cell size");
    let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
        client_id,
        size: cell_size,
    });
    let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
        pane_id,
        bytes: opaque_image_input(koshi_terminal::graphics::GraphicsProtocol::Kitty),
    });
    let snapshot = frame(&server, client_id);
    let client = test_client(&mut server, client_id);
    let committed = regions(VIEWPORT);
    let area = Rect::new(0, 0, 80, 24);
    let paints = image_paints(&snapshot, &committed, area);
    let cells = image_cell_snapshot(&snapshot, &committed, area).map(Arc::new);
    let mut output = ImageOutputState::new(ImageOutputKind::from_support(graphics));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !output.prepare_frame(&paints, cells.clone(), Some(cell_size)) {
        assert!(
            Instant::now() < deadline,
            "{graphics:?} output did not prepare"
        );
        std::thread::yield_now();
    }
    assert!(output.native_commit_pending());
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut writer = FailInsideSequence {
        sequence,
        split,
        fail_next: false,
        failed: false,
        output: Vec::new(),
    };

    let error = paint_frame_with_writer(
        &mut writer,
        &mut terminal,
        &client,
        &snapshot,
        &committed,
        &ViewerPaint::from_frame(&client, &snapshot),
        graphics.image_mode(),
        &mut output,
        Some(cell_size),
        &mut window_title(&snapshot),
        &mut cursor_style(&snapshot),
    )
    .expect_err("the partial write is returned");

    let PaintError::Image(error) = error;
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(
        error.to_string(),
        "test writer failed inside a control sequence"
    );
    assert!(writer.failed);
    assert!(writer.output.ends_with(b"\x18\x1b\\\x1b[?2026l"));
    assert!(output.native_commit_pending());
    assert_eq!(output.prepared_keys(), []);
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
                palette_colors: 256,
                max_width: None,
                max_height: None,
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
    for source in [
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Iterm2,
        GraphicsProtocol::Sixel,
    ] {
        let fake = Arc::new(FakePtyBackend::new());
        let (mut server, client_id, pane_id) = boot(&fake);
        let cell_size = PixelCellSize::new(1, 1).unwrap();
        let _ = server.handle_runtime_event(RuntimeEvent::CellSize {
            client_id,
            size: cell_size,
        });
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            bytes: opaque_image_input(source),
        });
        let initial = frame(&server, client_id);
        let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            bytes: b"\x1b[3S".to_vec(),
        });
        let cropped = frame(&server, client_id);
        let _ = server.handle_runtime_event(RuntimeEvent::ClientMouse {
            client_id,
            request_id: 1,
            actions: vec![WireMouseAction::Scroll {
                pane: pane_id,
                up: true,
                lines: 3,
            }],
        });
        let restored = frame(&server, client_id);
        let client = test_client(&mut server, client_id);
        assert_image_trace_output(
            &client,
            source,
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
    source: koshi_terminal::graphics::GraphicsProtocol,
    frames: &[RenderSnapshot],
    expected: &[(u16, u16, Vec<u8>)],
    include_text: bool,
) {
    for graphics in [
        GraphicsSupport::Kitty,
        GraphicsSupport::Iterm,
        GraphicsSupport::Sixel {
            palette_colors: 256,
            max_width: None,
            max_height: None,
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
        let mut output = ImageOutputState::new(ImageOutputKind::from_support(graphics));
        let mut cache = crate::attach::paint::ImageCache::new();
        let mut content_ids = ImageWireContentIds::default();
        let mut outer = TerminalEngine::new(PtySize { cols: 80, rows: 24 });
        outer.set_cell_size(PixelCellSize::new(1, 1).unwrap());
        let _ = outer.advance(b"\x1b[?1049h");
        for (stage, (snapshot, (row, height, pixels))) in frames.iter().zip(expected).enumerate() {
            let mut stage_bytes = Vec::new();
            let snapshot = image_frame_through_wire(snapshot, &mut cache, &mut content_ids);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let committed = paint_frame_with_writer(
                    &mut writer,
                    &mut terminal,
                    client,
                    &snapshot,
                    &regions(VIEWPORT),
                    &ViewerPaint::from_frame(client, &snapshot),
                    graphics.image_mode(),
                    &mut output,
                    Some(PixelCellSize::new(1, 1).unwrap()),
                    &mut String::new(),
                    &mut None,
                )
                .unwrap();
                let bytes = std::mem::take(&mut *writer.0.lock().unwrap());
                stage_bytes.extend_from_slice(&bytes);
                let _ = outer.advance(&bytes);
                if committed && !output.work_pending() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "{source:?} -> {graphics:?}, stage {stage} did not settle"
                );
                std::thread::yield_now();
            }
            let placements = outer.state().image_placements_for_view(0);
            let mut actual = std::collections::BTreeMap::new();
            for p in &placements {
                let record = p.render_record_arc();
                let (x, y, width, height) = record.source_rect().unwrap();
                let geometry = p.geometry();
                for row in 0..p.dimensions().0 {
                    for column in 0..p.dimensions().1 {
                        let source_y = y + u32::from(geometry.offset.y + row) * height
                            / u32::from(geometry.full_size.rows);
                        let source_x = x + u32::from(geometry.offset.x + column) * width
                            / u32::from(geometry.full_size.cols);
                        let start = ((source_y * record.image.width + source_x) * 4) as usize;
                        let pixel: [u8; 4] =
                            record.image.rgba[start..start + 4].try_into().unwrap();
                        actual.insert((p.anchor().0 + row, p.anchor().1 + column), pixel);
                    }
                }
            }
            let expected = (0..*height)
                .flat_map(|y| {
                    (0..4).filter_map(move |x| {
                        let start = (usize::from(y) * 4 + usize::from(x)) * 4;
                        let pixel = <[u8; 4]>::try_from(&pixels[start..start + 4]).unwrap();
                        (pixel[3] != 0).then_some(((*row + y + 2, x + 1), pixel))
                    })
                })
                .collect::<std::collections::BTreeMap<_, _>>();
            assert_eq!(
                actual,
                expected,
                "{source:?} -> {graphics:?}, stage {stage}, stream {:?}, events {:?}",
                String::from_utf8_lossy(&stage_bytes),
                outer.take_graphics()
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
    let (mut server, client_id, _pane) = boot(&fake);

    let client = test_client_with(
        &mut server,
        client_id,
        LoadedConfig {
            app: None,
            theme: Some(PartialThemeConfig {
                name: Some("ocean".to_owned()),
                colors: Some(PartialColorPalette {
                    border_focused: Some(RgbColor::new(0xff, 0, 0)),
                    ..PartialColorPalette::default()
                }),
            }),
            keybindings: None,
        },
    );

    assert_eq!(client.config().theme.name, "ocean");
    assert_eq!(
        client.theme().border_focused,
        ratatui::style::Color::Rgb(0xff, 0, 0)
    );
}

#[test]
fn the_launch_hands_the_viewer_the_keymap_file_it_read() {
    // A keymap layer that validates replaces the built-in keybinding settings.
    // A launch that dropped `loaded.keybindings` would leave the stock 500 ms
    // chord timeout in place.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, _pane) = boot(&fake);

    let client = test_client_with(
        &mut server,
        client_id,
        LoadedConfig {
            app: None,
            theme: None,
            keybindings: Some(PartialKeybindingsConfig {
                chord_timeout_ms: Some(1234),
                ..PartialKeybindingsConfig::default()
            }),
        },
    );

    assert_eq!(client.config().keybindings.chord_timeout_ms, 1234);
}

#[test]
fn the_painted_hint_bar_follows_the_clients_mouse_select_state() {
    // The hint bar is painted from the viewer's own keymap, but which label the
    // mouse-select entry wears depends on session state the frame carries. A
    // frame that dropped that link would keep offering "Mouse Select" while
    // selection was already on.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, _pane_id) = boot(&fake);
    let client = test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
    let snapshot = frame(&server, client_id);

    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &regions(Size {
            cols: 120,
            rows: 24,
        }),
        &ViewerPaint::from_frame(&client, &snapshot),
        &mut String::new(),
        &mut None,
    )
    .expect("paint");
    assert!(screen_text(&terminal).contains("Mouse Select"));

    server.submit_command(CommandEnvelope::new(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        SystemTime::now(),
        Command::ToggleMouseSelect,
    ));
    let snapshot = frame(&server, client_id);
    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &regions(Size {
            cols: 120,
            rows: 24,
        }),
        &ViewerPaint::from_frame(&client, &snapshot),
        &mut String::new(),
        &mut None,
    )
    .expect("paint");

    let painted = screen_text(&terminal);
    assert!(painted.contains("Mouse Unselect"), "{painted}");
}

#[test]
fn pty_output_is_painted_to_the_screen() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, pane_id) = boot(&fake);

    assert!(server
        .handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            bytes: b"hello".to_vec(),
        },)
        .is_continue());

    let client = test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let snapshot = frame(&server, client_id);
    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &regions(VIEWPORT),
        &ViewerPaint::from_frame(&client, &snapshot),
        &mut String::new(),
        &mut None,
    )
    .expect("paint");

    assert!(
        screen_text(&terminal).contains("hello"),
        "the shell's output should appear on the rendered screen"
    );
}

#[test]
fn painting_emits_a_changed_cursor_style_and_records_it() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, pane_id) = boot(&fake);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

    // The pane asks for a steady bar via DECSCUSR (`CSI 6 SP q`); the first
    // paint sees it differ from the starting `None` and records the new style.
    assert!(server
        .handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            bytes: b"\x1b[6 q".to_vec(),
        },)
        .is_continue());
    let client = test_client(&mut server, client_id);
    let mut last_cursor = None;
    let snapshot = frame(&server, client_id);
    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &regions(VIEWPORT),
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
    // A frame with no focused pane leaves `cursor_style` with nothing to
    // report. The record still follows the frame: the next frame that names a
    // style counts as a change and is sent again.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, _pane_id) = boot(&fake);
    let client = test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut snapshot = frame(&server, client_id);
    snapshot.client.focused_pane = None;
    let mut last_cursor = Some(CursorStyle::Shaped {
        shape: CursorShape::Block,
        blink: true,
    });

    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &regions(VIEWPORT),
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
    let (mut server, client_id, pane_id) = boot(&fake);
    let client = test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = Some(pane_id);
    snapshot.panes[0].id = pane_id;
    snapshot.panes[0].title = Some("htop".to_string());
    let mut last_title = String::new();

    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &regions(VIEWPORT),
        &ViewerPaint::from_frame(&client, &snapshot),
        &mut last_title,
        &mut None,
    )
    .expect("paint");

    assert_eq!(last_title, "quiet-lake | htop");
}

#[test]
fn a_paint_after_a_title_change_records_the_new_title() {
    // The record decides whether `SetTitle` is written at all. It tracks every
    // frame, not only the first one.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, _pane_id) = boot(&fake);
    let client = test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = None;
    let mut last_title = String::new();
    let paint = |terminal: &mut Terminal<TestBackend>,
                 snapshot: &RenderSnapshot,
                 last_title: &mut String| {
        paint_frame(
            terminal,
            &client,
            snapshot,
            &regions(VIEWPORT),
            &ViewerPaint::from_frame(&client, snapshot),
            last_title,
            &mut None,
        )
        .expect("paint");
    };

    paint(&mut terminal, &snapshot, &mut last_title);
    assert_eq!(last_title, "quiet-lake");

    snapshot.session.name = "loud-hill".to_string();
    paint(&mut terminal, &snapshot, &mut last_title);

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
    let shaped = |shape, blink| CursorStyle::Shaped { shape, blink };
    let cases = [
        // A pane that asked for nothing hands the cursor back to the user.
        (CursorStyle::UserDefault, SetCursorStyle::DefaultUserShape),
        (
            shaped(CursorShape::Block, true),
            SetCursorStyle::BlinkingBlock,
        ),
        (
            shaped(CursorShape::Block, false),
            SetCursorStyle::SteadyBlock,
        ),
        (
            shaped(CursorShape::Underline, true),
            SetCursorStyle::BlinkingUnderScore,
        ),
        (
            shaped(CursorShape::Underline, false),
            SetCursorStyle::SteadyUnderScore,
        ),
        (shaped(CursorShape::Bar, true), SetCursorStyle::BlinkingBar),
        (shaped(CursorShape::Bar, false), SetCursorStyle::SteadyBar),
    ];
    for (style, expected) in cases {
        assert_eq!(set_cursor_style(style), expected, "{style:?}");
    }
}

// --- window_title: the outer-terminal title string ---

#[test]
fn window_title_with_no_focused_pane_is_just_the_session_name() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, _pane_id) = boot(&fake);
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = None;

    assert_eq!(window_title(&snapshot), "quiet-lake");
}

#[test]
fn window_title_with_a_titled_focused_pane_joins_session_and_title() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = boot(&fake);
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = Some(pane_id);
    snapshot.panes[0].id = pane_id;
    snapshot.panes[0].title = Some("htop".to_string());

    assert_eq!(window_title(&snapshot), "quiet-lake | htop");
}

#[test]
fn window_title_reads_the_focused_pane_not_the_first_one_listed() {
    // The lookup matches on the pane id. Every other title test lists a single
    // pane; a lookup that took the first entry would pass all of them.
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = boot(&fake);
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    let focused = PaneId::new();
    let mut second = snapshot.panes[0].clone();
    second.id = focused;
    second.title = Some("htop".to_string());
    snapshot.panes[0].id = pane_id;
    snapshot.panes[0].title = Some("bash".to_string());
    snapshot.panes.push(second);
    snapshot.client.focused_pane = Some(focused);

    assert_eq!(window_title(&snapshot), "quiet-lake | htop");
}

#[test]
fn window_title_with_an_untitled_focused_pane_falls_back_to_the_session_name() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = boot(&fake);
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = Some(pane_id);
    snapshot.panes[0].id = pane_id;
    snapshot.panes[0].title = None;

    assert_eq!(window_title(&snapshot), "quiet-lake");
}

#[test]
fn window_title_keeps_a_non_ascii_pane_title_whole() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = boot(&fake);
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = Some(pane_id);
    snapshot.panes[0].id = pane_id;
    snapshot.panes[0].title = Some("日本語 🙂".to_string());

    assert_eq!(window_title(&snapshot), "quiet-lake | 日本語 🙂");
}

#[test]
fn window_title_with_an_empty_pane_title_falls_back_to_the_session_name() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = boot(&fake);
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = Some(pane_id);
    snapshot.panes[0].id = pane_id;
    snapshot.panes[0].title = Some(String::new());

    assert_eq!(window_title(&snapshot), "quiet-lake");
}

#[test]
fn window_title_with_a_focused_pane_absent_from_the_pane_list_falls_back() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = boot(&fake);
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = Some(pane_id);
    // No `PaneSnapshot` carries `pane_id`, so the lookup in `window_title`
    // cannot find a title for it.
    snapshot.panes.clear();

    assert_eq!(window_title(&snapshot), "quiet-lake");
}

/// The title `window_title` builds for a session named `session_name` holding
/// one focused pane titled `pane_title`, after that frame has travelled the
/// session-to-client wire and been read back by
/// [`to_snapshot`](crate::attach::paint::to_snapshot).
fn title_off_the_wire(session_name: &str, pane_title: &str) -> String {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = boot(&fake);
    let mut sent = frame(&server, client_id);
    sent.session.name = session_name.to_string();
    sent.client.focused_pane = Some(pane_id);
    for pane in &mut sent.panes {
        if pane.id == pane_id {
            pane.title = Some(pane_title.to_string());
        }
    }

    let read_back =
        crate::attach::paint::to_snapshot(&koshi_runtime::runtime::frame::wire_frame(&sent));
    window_title(&read_back)
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
    let cap = koshi_core::text::MAX_REPORTED_TEXT_BYTES;
    let title = title_off_the_wire("dev", &"a".repeat(100_000));

    assert_eq!(title, format!("dev | {}", "a".repeat(cap)));
}

#[test]
fn terminal_probe_uses_a_300_ms_deadline_and_a_non_storing_query() {
    let mut output = Vec::new();

    write_terminal_probe_queries(&mut output).expect("query writes");

    assert_eq!(TERMINAL_QUERY_TIMEOUT, Duration::from_millis(300));
    assert_eq!(
        output,
        b"\x1b_Gi=4294967295,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b]1337;Capabilities\x1b\\\x1b[c\x1b[?1;1;0S\x1b[?2;4;0S\x1b[16t"
    );
}

#[test]
fn either_terminal_stream_opens_the_terminal_device() {
    assert!(!terminal_device_needed(false, false));
    assert!(terminal_device_needed(true, false));
    assert!(terminal_device_needed(false, true));
    assert!(terminal_device_needed(true, true));
}

#[test]
fn redirected_standard_output_skips_the_controlling_terminal_probe() {
    let mut called = false;
    let support = graphics_support_for_output(false, || {
        called = true;
        Ok(probe(GraphicsSupport::Kitty))
    })
    .expect("redirected output selects a supported fallback");

    assert_eq!(support, probe(GraphicsSupport::Unsupported));
    assert!(!called);

    let support = graphics_support_for_output(true, || {
        called = true;
        Ok(probe(GraphicsSupport::Kitty))
    })
    .expect("terminal output accepts the probe result");
    assert_eq!(support, probe(GraphicsSupport::Kitty));
    assert!(called);

    let error = graphics_support_for_output(true, || {
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

    let value = with_raw_mode(
        &mut calls,
        |calls| {
            calls.push("raw");
            Ok(())
        },
        |calls| {
            calls.push("probe");
            Ok(probe(GraphicsSupport::Kitty))
        },
        |calls| {
            calls.push("cooked");
            Ok(())
        },
    )
    .expect("mode cycle succeeds");

    assert_eq!(value, probe(GraphicsSupport::Kitty));
    assert_eq!(calls, ["raw", "probe", "cooked"]);
}

#[test]
fn raw_mode_operation_stops_when_raw_mode_entry_fails() {
    let mut calls = Vec::new();

    let error = with_raw_mode(
        &mut calls,
        |calls| {
            calls.push("raw");
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "raw error"))
        },
        |calls| {
            calls.push("probe");
            Ok(probe(GraphicsSupport::Kitty))
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

    let error = with_raw_mode(
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

    let error = with_raw_mode(
        &mut calls,
        |calls| {
            calls.push("raw");
            Ok(())
        },
        |calls| {
            calls.push("probe");
            Ok(probe(GraphicsSupport::Kitty))
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

    let error = with_raw_mode(
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
            ok: true,
        }
    )));
    assert!(!is_probe_event(&Event::KittyGraphicsReply(
        KittyGraphicsReply {
            image_id: 31,
            ok: true,
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
        Event::CellSize(PixelCellSize::new(10, 20).expect("nonzero cell size")),
        Event::KittyGraphicsReply(KittyGraphicsReply {
            image_id: KITTY_QUERY_IMAGE_ID,
            ok: true,
        }),
        Event::Paste("pasted".to_string()),
    ]);
    let mut output = Vec::new();

    let result = probe_terminal(&mut output, &mut reader).expect("probe reads replies");

    assert_eq!(result.graphics, GraphicsSupport::Kitty);
    assert_eq!(
        result.cell_size,
        PixelCellSize::new(10, 20),
        "the cell-size reply is retained beside protocol support"
    );
    assert_eq!(
        reader
            .read(|event| matches!(event, Event::Key(_)))
            .expect("the unrelated key remains buffered"),
        Event::Key(KeyCode::Char('k').into())
    );
    assert_eq!(
        reader
            .read(|event| matches!(event, Event::Paste(_)))
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

    let result = probe_terminal(&mut Vec::new(), &mut reader).expect("probe reads replies");

    assert_eq!(result.graphics, GraphicsSupport::Iterm);
}

#[test]
fn terminal_probe_uses_two_sixel_colors_without_a_palette_reply() {
    let mut reader = probe_reader([Event::PrimaryDeviceAttributes(vec![1, 4])]);

    let result = probe_terminal(&mut Vec::new(), &mut reader).expect("probe reads replies");

    assert_eq!(
        result.graphics,
        GraphicsSupport::Sixel {
            palette_colors: 2,
            max_width: None,
            max_height: None,
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

    let result = probe_terminal(&mut Vec::new(), &mut reader).expect("probe reads replies");

    assert_eq!(
        result.graphics,
        GraphicsSupport::Sixel {
            palette_colors: 256,
            max_width: None,
            max_height: Some(480),
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

    let result = probe_terminal(&mut Vec::new(), &mut reader).expect("probe reads replies");

    assert_eq!(result.graphics, GraphicsSupport::Unsupported);
}

#[test]
fn terminal_probe_accepts_successful_sixel_geometry_without_other_sixel_evidence() {
    let mut reader = probe_reader([Event::SixelGraphicsAttributeReply(
        GraphicAttributeReply::Geometry(Ok((640, 480))),
    )]);

    let result = probe_terminal(&mut Vec::new(), &mut reader).expect("probe reads replies");

    assert_eq!(
        result.graphics,
        GraphicsSupport::Sixel {
            palette_colors: 2,
            max_width: Some(640),
            max_height: Some(480),
        }
    );
}

#[test]
fn terminal_probe_rejects_a_reported_one_color_sixel_palette() {
    let mut reader = probe_reader([
        Event::PrimaryDeviceAttributes(vec![1, 2, 4]),
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Palette(Ok(1))),
    ]);

    let result = probe_terminal(&mut Vec::new(), &mut reader).expect("probe reads replies");

    assert_eq!(result.graphics, GraphicsSupport::Unsupported);
}

#[test]
fn terminal_probe_without_replies_preserves_unrelated_input_and_reports_unsupported() {
    let mut reader = probe_reader([
        Event::Key(KeyCode::Char('x').into()),
        Event::Paste("input".to_string()),
    ]);

    let result = probe_terminal(&mut Vec::new(), &mut reader).expect("empty probe completes");

    assert_eq!(result.graphics, GraphicsSupport::Unsupported);
    assert_eq!(result.cell_size, None);
    assert_eq!(
        reader
            .read(|event| matches!(event, Event::Key(_)))
            .expect("the key is not consumed by the probe"),
        Event::Key(KeyCode::Char('x').into())
    );
    assert_eq!(
        reader
            .read(|event| matches!(event, Event::Paste(_)))
            .expect("the paste is not consumed by the probe"),
        Event::Paste("input".to_string())
    );
}

#[test]
fn new_protocols_use_native_image_mode_when_their_writer_is_available() {
    assert_eq!(GraphicsSupport::Iterm.image_mode(), ImageRenderMode::Native);
    assert_eq!(
        GraphicsSupport::Sixel {
            palette_colors: 2,
            max_width: None,
            max_height: None,
        }
        .image_mode(),
        ImageRenderMode::Native
    );
}

#[test]
fn terminal_modes_are_enabled_after_entering_the_alternate_screen() {
    let mut output = Vec::new();

    enable_terminal_modes(&mut output, GraphicsSupport::Unsupported).expect("terminal modes write");

    assert_eq!(
        output,
        b"\x1b[?1049h\x1b[>7u\x1b[?1003h\x1b[?1006h\x1b[?2004h"
    );
}

#[test]
fn sixel_modes_are_saved_before_application_modes() {
    let mut output = Vec::new();
    let graphics = GraphicsSupport::Sixel {
        palette_colors: 2,
        max_width: None,
        max_height: None,
    };

    enable_terminal_modes(&mut output, graphics).expect("terminal modes write");

    assert_eq!(
        output,
        b"\x1b[?80s\x1b[?8452s\x1b[?1070s\x1b[?1049h\x1b[>7u\x1b[?1003h\x1b[?1006h\x1b[?2004h"
    );
}

#[test]
fn terminal_cleanup_reverses_modes_and_deletes_kitty_images() {
    let mut output = Vec::new();
    let claimed = AtomicBool::new(false);

    write_terminal_cleanup(&mut output, GraphicsSupport::Kitty, &claimed)
        .expect("terminal cleanup writes");

    assert_eq!(
        output,
        b"\x18\x1b\\\x1b_Ga=d,d=A,q=2;\x1b\\\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q"
    );
    assert!(claimed.load(Ordering::Acquire));
}

#[test]
fn sixel_cleanup_aborts_a_control_string_before_restoring_modes() {
    let mut output = Vec::new();
    let claimed = AtomicBool::new(false);
    let graphics = GraphicsSupport::Sixel {
        palette_colors: 2,
        max_width: None,
        max_height: None,
    };

    write_terminal_cleanup(&mut output, graphics, &claimed).expect("terminal cleanup writes");

    assert_eq!(
        output,
        b"\x18\x1b\\\x1b[?80r\x1b[?8452r\x1b[?1070r\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q"
    );
}

#[test]
fn terminal_cleanup_attempts_mode_resets_after_image_delete_fails() {
    let mut writer = FailOnWrite {
        fail_at: 1,
        writes: 0,
        output: Vec::new(),
    };
    let claimed = AtomicBool::new(false);

    let error = write_terminal_cleanup(&mut writer, GraphicsSupport::Kitty, &claimed)
        .expect_err("the failed delete is returned");

    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(
        writer.output,
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

    let mut engine = TerminalEngine::new(PtySize { cols: 80, rows: 24 });
    assert!(engine.advance(&writer).is_empty());
    assert!(engine.take_graphics().is_empty());
    assert!(engine.finish().is_empty());
}

#[test]
fn terminal_application_modes_are_restored_once_across_cleanup_paths() {
    let mut output = Vec::new();
    let active = AtomicBool::new(true);
    let image_claimed = AtomicBool::new(false);

    restore_application_modes(&mut output, GraphicsSupport::Kitty, &active, &image_claimed)
        .expect("the panic cleanup writes");
    restore_application_modes(&mut output, GraphicsSupport::Kitty, &active, &image_claimed)
        .expect("the unwind cleanup is already complete");

    assert_eq!(
        output,
        b"\x18\x1b\\\x1b_Ga=d,d=A,q=2;\x1b\\\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q"
    );
    assert!(!active.load(Ordering::Acquire));
    assert!(image_claimed.load(Ordering::Acquire));
}

#[test]
fn terminal_cleanup_skips_image_commands_before_application_modes_are_active() {
    let mut output = Vec::new();
    let active = AtomicBool::new(false);
    let image_claimed = AtomicBool::new(false);

    restore_application_modes(&mut output, GraphicsSupport::Kitty, &active, &image_claimed)
        .expect("inactive application modes need no cleanup");

    assert!(output.is_empty());
    assert!(!image_claimed.load(Ordering::Acquire));
}

#[test]
fn host_resize_and_paste_events_keep_their_exact_values() {
    let client_id = ClientId::new();
    let resize = terminal_runtime_event(
        client_id,
        Event::WindowResized(WindowSize {
            cols: 101,
            rows: 37,
            pixel_width: Some(1_010),
            pixel_height: Some(740),
        }),
    );
    let Some(RuntimeEvent::Resize {
        client_id: actual_client_id,
        size,
        pane_area,
        cell_size,
    }) = resize
    else {
        panic!("expected the exact resize event, got {resize:?}");
    };
    assert_eq!(actual_client_id, client_id);
    assert_eq!(
        size,
        Size {
            cols: 101,
            rows: 37,
        }
    );
    assert_eq!(pane_area, Some(core_pane_area(size)));
    assert_eq!(
        cell_size,
        Some(PixelCellSize::new(10, 20).expect("positive cell dimensions"))
    );

    let paste = terminal_runtime_event(client_id, Event::Paste("hello 🐈".to_string()));
    let Some(RuntimeEvent::HostPaste {
        client_id: actual_client_id,
        text,
    }) = paste
    else {
        panic!("expected the exact paste event, got {paste:?}");
    };
    assert_eq!(actual_client_id, client_id);
    assert_eq!(text, "hello 🐈");
}

#[test]
fn local_pixel_cell_size_requires_complete_evenly_divisible_metrics() {
    let valid = WindowSize {
        cols: 10,
        rows: 20,
        pixel_width: Some(100),
        pixel_height: Some(400),
    };
    assert_eq!(
        pixel_cell_size_from_window(valid),
        Some(PixelCellSize::new(10, 20).expect("positive cell dimensions"))
    );

    for invalid in [
        WindowSize {
            cols: 10,
            rows: 20,
            pixel_width: None,
            pixel_height: Some(400),
        },
        WindowSize {
            cols: 10,
            rows: 20,
            pixel_width: Some(100),
            pixel_height: None,
        },
        WindowSize {
            cols: 10,
            rows: 20,
            pixel_width: Some(101),
            pixel_height: Some(400),
        },
        WindowSize {
            cols: 0,
            rows: 20,
            pixel_width: Some(100),
            pixel_height: Some(400),
        },
        WindowSize {
            cols: 10,
            rows: 0,
            pixel_width: Some(100),
            pixel_height: Some(400),
        },
        WindowSize {
            cols: 10,
            rows: 20,
            pixel_width: Some(0),
            pixel_height: Some(400),
        },
        WindowSize {
            cols: 10,
            rows: 20,
            pixel_width: Some(100),
            pixel_height: Some(0),
        },
    ] {
        assert_eq!(pixel_cell_size_from_window(invalid), None);
    }
}

#[test]
fn initial_cell_size_uses_only_native_image_measurements() {
    let probed = PixelCellSize::new(10, 20).expect("positive cell dimensions");
    let local = PixelCellSize::new(12, 24).expect("positive cell dimensions");

    assert_eq!(
        initial_cell_size(GraphicsSupport::Unsupported, Some(probed), Some(local)),
        None
    );
    assert_eq!(
        initial_cell_size(GraphicsSupport::Kitty, None, Some(local)),
        Some(local)
    );
    assert_eq!(
        initial_cell_size(GraphicsSupport::Iterm, Some(probed), Some(local)),
        Some(probed)
    );
}

#[test]
fn an_outstanding_cell_size_query_cannot_accept_a_reply_from_an_old_resize() {
    let old = PixelCellSize::new(10, 20).expect("positive cell dimensions");
    let current = PixelCellSize::new(12, 24).expect("positive cell dimensions");
    let mut query = CellSizeQuery::new(None, true, false);
    let mut wire = Vec::new();

    assert!(query.resize(None));
    query
        .request_to(&mut wire)
        .expect("the first cell-size query writes");
    assert_eq!(wire, b"\x1b[16t");

    assert!(!query.resize(Some(current)));
    assert_eq!(query.accept(old), (None, false));
    assert_eq!(query.current(), Some(current));

    assert_eq!(
        query.accept(old),
        (None, false),
        "an unsolicited reply cannot overwrite the current resize"
    );
    query
        .request_to(&mut wire)
        .expect("an already measured resize needs no replacement query");
    assert_eq!(wire, b"\x1b[16t");
}

#[test]
fn an_old_reply_is_discarded_then_an_unknown_resize_gets_one_new_query() {
    let old = PixelCellSize::new(10, 20).expect("positive cell dimensions");
    let mut query = CellSizeQuery::new(None, true, false);
    let mut wire = Vec::new();

    query
        .request_to(&mut wire)
        .expect("the initial cell-size query writes");
    assert!(!query.resize(None));
    assert_eq!(query.accept(old), (None, true));
    query
        .request_to(&mut wire)
        .expect("the replacement query writes after the old reply");
    assert_eq!(wire, b"\x1b[16t\x1b[16t");
}

#[test]
fn a_timed_out_probe_does_not_block_a_resize_query() {
    let measurement = PixelCellSize::new(10, 20).expect("positive cell dimensions");
    let mut query = CellSizeQuery::new(None, true, false);
    let mut wire = Vec::new();

    assert!(query.resize(None));
    query
        .request_to(&mut wire)
        .expect("the resize query writes after the timed-out probe");
    assert_eq!(query.accept(measurement), (Some(measurement), false));
    assert_eq!(wire, b"\x1b[16t");
}

#[test]
fn an_ipc_reconnect_keeps_an_outer_terminal_query_pending() {
    let old = PixelCellSize::new(10, 20).expect("positive cell dimensions");
    let mut query = CellSizeQuery::new(None, true, false);
    let mut wire = Vec::new();

    query
        .request_to(&mut wire)
        .expect("the closed connection's query writes");
    assert!(!query.resize(Some(old)));
    assert!(!query.resize(None));
    assert_eq!(query.accept(old), (None, true));
    query
        .request_to(&mut wire)
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
        match terminal_runtime_event(client_id, event) {
            None => {}
            Some(runtime_event) => panic!("unexpected runtime event: {runtime_event:?}"),
        }
    }
}

#[test]
fn unsupported_paint_writes_no_terminal_image_output() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, pane_id) = boot(&fake);
    let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
        pane_id,
        bytes: b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\".to_vec(),
    });
    let client = test_client(&mut server, client_id);
    let snapshot = frame(&server, client_id);
    assert_eq!(snapshot.panes[0].image_placements.len(), 1);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut output = Vec::new();
    let mut last_title = window_title(&snapshot);
    let mut last_cursor = cursor_style(&snapshot);
    let mut image_output = ImageOutputState::disabled();

    paint_frame_with_writer(
        &mut output,
        &mut terminal,
        &client,
        &snapshot,
        &regions(VIEWPORT),
        &ViewerPaint::from_frame(&client, &snapshot),
        ImageRenderMode::Placeholder,
        &mut image_output,
        None,
        &mut last_title,
        &mut last_cursor,
    )
    .expect("placeholder paint");

    assert_eq!(output, Vec::<u8>::new());
}

#[test]
fn image_cleanup_has_one_owner() {
    let claimed = AtomicBool::new(false);

    assert!(claim_image_cleanup(&claimed));
    assert!(!claim_image_cleanup(&claimed));
}
