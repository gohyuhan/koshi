//! Tests for worker-side image eligibility, shared output state, and Sixel host modes.

use super::*;

use std::sync::mpsc;
use std::sync::Arc;

use koshi_core::geometry::PixelCellSize;
use koshi_core::ids::PaneId;
use koshi_renderer::ImageCellState;
use koshi_terminal::graphics::{ImageAction, ImageDisplay};
use koshi_terminal::style::{Color, Style};

fn paint(rgba: Vec<u8>, width: u32, height: u32, z_index: i32) -> OutputPaint {
    let image = DecodedImage {
        width,
        height,
        rgba,
    };
    let record = Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Iterm2,
        image: Arc::new(image),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay {
            z_index,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    });
    OutputPaint {
        key: (PaneId::new(), 1),
        content_id: 1,
        record,
        target: Rect::new(0, 0, width as u16, height as u16),
        source: ImageSourceRect {
            x: 0,
            y: 0,
            width,
            height,
        },
        z_index,
    }
}

fn cells(area: Rect, values: Vec<ImageCellState>) -> ImageCellSnapshot {
    assert_eq!(
        values.len(),
        usize::from(area.width) * usize::from(area.height)
    );
    ImageCellSnapshot::from_cells(area, values).expect("test cells fit area")
}

fn solid_cells(area: Rect, color: [u8; 3]) -> ImageCellSnapshot {
    let mut style = Style::default();
    style.set_bg(Color::Rgb(color[0], color[1], color[2]));
    cells(
        area,
        vec![
            ImageCellState {
                style,
                ..ImageCellState::default()
            };
            usize::from(area.width) * usize::from(area.height)
        ],
    )
}

#[test]
fn opaque_negative_z_image_is_unavailable_when_a_glyph_is_under_it() {
    let area = Rect::new(0, 0, 1, 1);
    let under = ImageCellState {
        ch: 'X',
        ..ImageCellState::default()
    };
    let snapshot = cells(area, vec![under]);
    let paint = paint(vec![255, 0, 0, 255], 1, 1, -1);
    let key = output_encode_key(
        ImageOutputKind::Iterm,
        PixelCellSize::new(1, 1).expect("test cell size"),
        &paint,
    );

    assert!(classify(ImageOutputKind::Iterm, &snapshot, &[], &paint, key).is_none());
}

#[test]
fn mixed_explicit_backgrounds_do_not_look_like_one_solid_color() {
    let area = Rect::new(0, 0, 2, 1);
    let first = solid_cells(area, [255, 0, 0]);
    let mut second_style = Style::default();
    second_style.set_bg(Color::Rgb(0, 0, 255));
    let second = cells(
        area,
        vec![
            ImageCellState {
                style: first.cell(0, 0).expect("first cell").style,
                ..ImageCellState::default()
            },
            ImageCellState {
                style: second_style,
                ..ImageCellState::default()
            },
        ],
    );
    let paint = paint(vec![255, 0, 0, 127, 255, 0, 0, 127], 2, 1, 0);
    let key = output_encode_key(
        ImageOutputKind::Iterm,
        PixelCellSize::new(1, 1).expect("test cell size"),
        &paint,
    );

    assert!(classify(ImageOutputKind::Iterm, &second, &[], &paint, key).is_none());
}

#[test]
fn a_default_background_between_rgb_cells_is_incompatible() {
    let area = Rect::new(0, 0, 3, 1);
    let mut red = Style::default();
    red.set_bg(Color::Rgb(255, 0, 0));
    let mut blue = Style::default();
    blue.set_bg(Color::Rgb(0, 0, 255));
    let snapshot = cells(
        area,
        vec![
            ImageCellState {
                style: red,
                ..ImageCellState::default()
            },
            ImageCellState::default(),
            ImageCellState {
                style: blue,
                ..ImageCellState::default()
            },
        ],
    );
    let paint = paint([255, 0, 0, 127].repeat(3), 3, 1, 0);
    let key = output_encode_key(
        ImageOutputKind::Iterm,
        PixelCellSize::new(1, 1).expect("test cell size"),
        &paint,
    );

    assert!(classify(ImageOutputKind::Iterm, &snapshot, &[], &paint, key).is_none());
}

#[test]
fn composition_key_tracks_background_without_copying_underlying_glyphs() {
    let area = Rect::new(0, 0, 1, 1);
    let blank = solid_cells(area, [20, 30, 40]);
    let mut glyph = ImageCellState {
        style: blank.cell(0, 0).expect("blank cell").style,
        ..ImageCellState::default()
    };
    glyph.ch = 'X';
    let with_glyph = ImageCellSnapshot::from_cells(area, vec![glyph]).expect("test cells");
    let paint = paint(vec![255, 0, 0, 127], 1, 1, 0);

    assert_eq!(
        composition_fingerprint(&blank, &paint),
        composition_fingerprint(&with_glyph, &paint)
    );
}

#[test]
fn binary_alpha_sixel_output_can_keep_a_glyph_in_a_zero_bit() {
    let area = Rect::new(0, 0, 1, 1);
    let under = ImageCellState {
        ch: 'X',
        ..ImageCellState::default()
    };
    let snapshot = cells(area, vec![under]);
    let paint = paint(vec![255, 0, 0, 0], 1, 1, 0);
    let key = output_encode_key(
        ImageOutputKind::Sixel {
            palette_colors: 2,
            max_width: None,
            max_height: None,
        },
        PixelCellSize::new(1, 1).expect("test cell size"),
        &paint,
    );

    assert!(classify(
        ImageOutputKind::Sixel {
            palette_colors: 2,
            max_width: None,
            max_height: None,
        },
        &snapshot,
        &[],
        &paint,
        key,
    )
    .is_some());
}

#[test]
fn output_state_keeps_i_term_available_without_a_pixel_cell_query() {
    let state = ImageOutputState::new(Some(ImageOutputKind::Iterm));

    assert_eq!(state.kind(), Some(ImageOutputKind::Iterm));
    assert!(!state.work_pending());
}

#[test]
fn failed_worker_output_repairs_partial_native_output() {
    let paint = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let key = output_encode_key(
        ImageOutputKind::Iterm,
        PixelCellSize::new(1, 1).expect("test cell size"),
        &paint,
    );
    let mut state = ImageOutputState::new(Some(ImageOutputKind::Iterm));
    state.latest = vec![paint.clone()];
    state.latest_keys = vec![key];
    state.prepared = vec![paint.key];
    state.painted = vec![paint.key];
    state.written = vec![paint.key];
    state.base_ready = true;
    state.cache_building.insert(
        key,
        vec![CachedUnit {
            offset: (0, 0),
            first: true,
            last: false,
            bytes: Arc::from(&b"partial"[..]),
        }],
    );
    state.active = Some(ActiveJob {
        generation: 1,
        keys: vec![key],
        placement_keys: vec![paint.key],
        cancel: Arc::new(AtomicBool::new(false)),
        stale: false,
    });
    let (sender, receiver) = mpsc::sync_channel(1);
    state.messages = Some(receiver);
    sender
        .send(WorkerMessage::Finished {
            generation: 1,
            failed: true,
        })
        .expect("failure message is queued");

    state.poll();

    assert!(state.active.is_none());
    assert!(state.prepared.is_empty());
    assert!(state.painted.is_empty());
    assert!(state.written.is_empty());
    assert!(state.cache_building.is_empty());
    assert_eq!(state.repair_rects, vec![paint.target]);
    assert!(state.base_repaint_needed);
    assert!(!state.base_ready);
    assert!(state.work_pending());
}

#[test]
fn failed_active_worker_releases_the_slot_for_the_queued_frame() {
    let paint = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let kind = ImageOutputKind::Iterm;
    let cell_size = PixelCellSize::new(1, 1).expect("test cell size");
    let key = output_encode_key(kind, cell_size, &paint);
    let cells = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let mut state = ImageOutputState::new(Some(kind));
    state.latest = vec![paint.clone()];
    state.latest_keys = vec![key];
    state.cache_building.insert(
        key,
        vec![CachedUnit {
            offset: (0, 0),
            first: true,
            last: false,
            bytes: Arc::from(&b"stale"[..]),
        }],
    );
    state.active = Some(ActiveJob {
        generation: 1,
        keys: vec![key],
        placement_keys: vec![paint.key],
        cancel: Arc::new(AtomicBool::new(false)),
        stale: true,
    });
    state.pending = Some(WorkerRequest {
        generation: 2,
        kind,
        cell_size,
        cells,
        paints: vec![paint],
        keys: vec![key],
        cancel: Arc::new(AtomicBool::new(false)),
    });
    let (sender, receiver) = mpsc::sync_channel(1);
    state.messages = Some(receiver);
    sender
        .send(WorkerMessage::Finished {
            generation: 1,
            failed: true,
        })
        .expect("failure message is queued");

    state.poll();

    assert_eq!(
        state.active.as_ref().map(|active| active.generation),
        Some(2)
    );
    assert!(state.pending.is_none());
    assert!(state.cache_building.is_empty());
}

#[test]
fn an_empty_replay_does_not_keep_the_output_loop_awake() {
    let mut state = ImageOutputState::new(Some(ImageOutputKind::Iterm));

    state.prepare_replay();

    assert!(!state.work_pending());
}

#[test]
fn sixel_mode_contract_saves_resets_and_restores_all_three_modes() {
    assert_eq!(sixel_mode_save(), b"\x1b[?80s\x1b[?8452s\x1b[?1070s");
    assert_eq!(SIXEL_MODE_RESET, b"\x1b[?80l\x1b[?8452l\x1b[?1070h");
    assert_eq!(sixel_mode_restore(), b"\x1b[?80r\x1b[?8452r\x1b[?1070r");
}

#[test]
fn crop_image_uses_the_requested_source_rectangle() {
    let mut source = paint(vec![255, 0, 0, 255, 0, 255, 0, 255], 2, 1, 0);
    source.source = ImageSourceRect {
        x: 1,
        y: 0,
        width: 1,
        height: 1,
    };

    let cropped = crop_image(&source, None).expect("source rectangle is valid");

    assert_eq!(cropped.width, 1);
    assert_eq!(cropped.height, 1);
    assert_eq!(cropped.rgba, vec![0, 255, 0, 255]);
}

#[test]
fn crop_image_rejects_a_source_rectangle_outside_the_decoded_pixels() {
    let mut source = paint(vec![255, 0, 0, 255], 1, 1, 0);
    source.source = ImageSourceRect {
        x: 1,
        y: 0,
        width: 1,
        height: 1,
    };

    assert!(crop_image(&source, None).is_err());
}

#[test]
fn worker_emits_one_complete_i_term_packet_for_one_opaque_pixel() {
    let paint = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let cells = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let kind = ImageOutputKind::Iterm;
    let cell_size = PixelCellSize::new(1, 1).expect("test cell size");
    let key = output_encode_key(kind, cell_size, &paint);
    let request = WorkerRequest {
        generation: 1,
        kind,
        cell_size,
        cells,
        paints: vec![paint],
        keys: vec![key],
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let (sender, receiver) = mpsc::sync_channel(8);

    run_job(&request, &sender).expect("worker encodes the pixel");
    let messages = receiver.try_iter().collect::<Vec<_>>();

    assert_eq!(messages.len(), 3);
    let WorkerMessage::Prepared { .. } = &messages[0] else {
        panic!("the worker prepares before output");
    };
    let WorkerMessage::Unit(unit) = &messages[1] else {
        panic!("the worker emits one packet");
    };
    assert!(unit.first);
    assert!(unit.last);
    assert!(unit.bytes.starts_with(b"\x1b]1337;"));
    assert!(unit.bytes.ends_with(b"\x1b\\"));
    assert!(matches!(&messages[2], WorkerMessage::Complete { .. }));
}

#[test]
fn worker_emits_a_complete_bounded_sixel_tile() {
    let paint = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let cells = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let kind = ImageOutputKind::Sixel {
        palette_colors: 2,
        max_width: None,
        max_height: None,
    };
    let cell_size = PixelCellSize::new(1, 1).expect("test cell size");
    let key = output_encode_key(kind, cell_size, &paint);
    let request = WorkerRequest {
        generation: 1,
        kind,
        cell_size,
        cells,
        paints: vec![paint],
        keys: vec![key],
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let (sender, receiver) = mpsc::sync_channel(8);

    run_job(&request, &sender).expect("worker encodes the pixel");
    let messages = receiver.try_iter().collect::<Vec<_>>();

    assert_eq!(messages.len(), 3);
    let WorkerMessage::Unit(unit) = &messages[1] else {
        panic!("the worker emits one tile");
    };
    assert_eq!(unit.offset, (0, 0));
    assert!(unit.first);
    assert!(unit.last);
    assert!(unit.bytes.starts_with(b"\x1bP"));
    assert!(unit.bytes.ends_with(b"\x1b\\"));
    assert!(unit.bytes.len() <= MAX_SIXEL_TILE_BYTES);
}

#[test]
fn i_term_unit_output_has_exact_position_payload_and_cursor_restore() {
    let paint = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let key = output_encode_key(
        ImageOutputKind::Iterm,
        PixelCellSize::new(1, 1).expect("test cell size"),
        &paint,
    );
    let mut state = ImageOutputState::new(Some(ImageOutputKind::Iterm));
    state.latest = vec![paint];
    let unit = OutputUnit {
        generation: 1,
        key: state.latest[0].key,
        encode_key: key,
        kind: ImageOutputKind::Iterm,
        offset: (0, 0),
        first: true,
        last: true,
        replay: false,
        bytes: Arc::from(&b"body"[..]),
    };
    let mut output = Vec::new();
    assert!(state
        .write_unit(
            &mut output,
            Some(ratatui::layout::Position { x: 4, y: 5 }),
            unit,
        )
        .expect("unit writes"));
    assert_eq!(output, b"\x1b[1;1Hbody\x1b[6;5H");
    assert_eq!(state.written, [state.latest[0].key]);
}

#[test]
fn sixel_unit_output_has_exact_mode_boundaries_and_cursor_restore() {
    let paint = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let kind = ImageOutputKind::Sixel {
        palette_colors: 2,
        max_width: None,
        max_height: None,
    };
    let key = output_encode_key(
        kind,
        PixelCellSize::new(1, 1).expect("test cell size"),
        &paint,
    );
    let mut state = ImageOutputState::new(Some(kind));
    state.latest = vec![paint];
    let unit = OutputUnit {
        generation: 1,
        key: state.latest[0].key,
        encode_key: key,
        kind,
        offset: (0, 0),
        first: true,
        last: true,
        replay: false,
        bytes: Arc::from(&b"sixel"[..]),
    };
    let mut output = Vec::new();
    assert!(state
        .write_unit(
            &mut output,
            Some(ratatui::layout::Position { x: 4, y: 5 }),
            unit,
        )
        .expect("unit writes"));
    assert_eq!(
        output,
        b"\x1b[?80l\x1b[?8452l\x1b[?1070h\x1b[1;1Hsixel\x1b[6;5H\x1b[?80r\x1b[?8452r\x1b[?1070r"
    );
}

fn blank_snapshot(area: Rect) -> ImageCellSnapshot {
    ImageCellSnapshot::from_cells(
        area,
        vec![ImageCellState::default(); usize::from(area.width) * usize::from(area.height)],
    )
    .expect("test cells fit area")
}
