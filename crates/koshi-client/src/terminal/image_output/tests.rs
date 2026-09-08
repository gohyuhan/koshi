//! Tests for worker-side image eligibility, shared output state, and Sixel host modes.

use super::*;

fn decode_sixel_unit(bytes: &[u8]) -> DecodedImage {
    let payload = bytes
        .strip_prefix(b"\x1bP")
        .unwrap()
        .strip_suffix(b"\x1b\\")
        .unwrap();
    let mut parser = koshi_sixel::SixelParser::new();
    for byte in payload {
        parser.feed(*byte).unwrap();
    }
    let graphic = parser.finish().unwrap();
    let mut palette = koshi_sixel::SixelPalette::default();
    palette.apply_changes(graphic.palette_changes());
    graphic
        .image()
        .unwrap()
        .resolve(&palette, [0, 0, 0])
        .unwrap()
}

fn decode_iterm_units(units: &[TemplateUnit]) -> DecodedImage {
    let mut transfer = None;
    let mut decoded = None;
    for unit in units {
        let body = unit
            .bytes
            .strip_prefix(b"\x1b]1337;")
            .expect("iTerm2 packet prefix")
            .strip_suffix(b"\x1b\\")
            .expect("iTerm2 packet terminator");
        if let Some(graphics) = koshi_iterm::parse_iterm_command(body, &mut transfer)
            .expect("generated iTerm2 packet parses")
        {
            decoded = Some(graphics.image);
        }
    }
    assert_eq!(transfer, None);
    decoded.expect("generated iTerm2 packets contain one image")
}

fn classify_plans<'a>(
    kind: ImageOutputKind,
    cells: &ImageCellSnapshot,
    paints: &'a [OutputPaint],
    measured_cell_size: Option<PixelCellSize>,
) -> Vec<Plan<'a>> {
    let cell_size = if kind.is_sixel() {
        measured_cell_size.expect("Sixel test cell size")
    } else {
        PixelCellSize::new(1, 1).expect("one-pixel cell")
    };
    let mut covered = HashSet::new();
    let mut plans = paints
        .iter()
        .map(|paint| {
            let plan = classify(
                kind,
                cells,
                &covered,
                paint,
                output_encode_key(kind, cell_size, paint),
            )
            .expect("valid test image");
            add_target_cells(paint.target, &mut covered);
            plan
        })
        .collect::<Vec<_>>();
    resolve_compatibility(kind, measured_cell_size, &mut plans);
    plans
}

fn iterm_request(
    cells: Arc<ImageCellSnapshot>,
    paints: &[OutputPaint],
    measured_cell_size: Option<PixelCellSize>,
) -> WorkerRequest {
    let cell_size = PixelCellSize::new(1, 1).expect("one-pixel cell");
    WorkerRequest {
        generation: 1,
        kind: ImageOutputKind::Iterm,
        cell_size,
        measured_cell_size,
        cells: Some(cells),
        paints: paints.to_vec(),
        keys: paints
            .iter()
            .map(|paint| output_encode_key(ImageOutputKind::Iterm, cell_size, paint))
            .collect(),
        kitty_images: Vec::new(),
        cancel: Arc::new(AtomicBool::new(false)),
    }
}

#[test]
fn oversized_sixel_tiles_cover_every_target_pixel_exactly_once() {
    let (width, height) = (256, 128);
    let mut random = 0x12345678u32;
    let mut pixels = Vec::new();
    for _ in 0..width * height {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        pixels.extend_from_slice(&[
            if random & 1 == 0 { 0 } else { 255 },
            if random & 2 == 0 { 0 } else { 255 },
            if random & 4 == 0 { 0 } else { 255 },
            255,
        ]);
    }
    let paint = paint(pixels.clone(), width, height, 0);
    let kind = ImageOutputKind::Sixel {
        palette_colors: 256,
        max_width: Some(128),
        max_height: Some(128),
    };
    let cell_size = PixelCellSize::new(1, 1).unwrap();
    let key = output_encode_key(kind, cell_size, &paint);
    let request = WorkerRequest {
        generation: 1,
        kind,
        cell_size,
        measured_cell_size: Some(cell_size),
        cells: Some(Arc::new(blank_snapshot(paint.target))),
        paints: vec![paint.clone()],
        keys: vec![key],
        kitty_images: Vec::new(),
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let units = encode_sixel_template(
        &request,
        &Plan {
            paint: &paint,
            key,
            iterm: ItermComposition::default(),
            sixel: SixelComposition::default(),
            compatibility: ImageCompatibility::default(),
            opaque: true,
        },
        &[],
        256,
        Some(128),
        Some(128),
        MAX_NATIVE_FRAME_OUTPUT_BYTES,
    )
    .unwrap();
    let mut actual = vec![None; (width * height) as usize];
    for unit in units {
        assert!(unit.bytes.len() <= MAX_SIXEL_OUTPUT_BYTES);
        let image = decode_sixel_unit(&unit.bytes);
        assert!(image.width <= 128 && image.height <= 128);
        for y in 0..image.height {
            for x in 0..image.width {
                let target = ((u32::from(unit.offset.1) + y) * width + u32::from(unit.offset.0) + x)
                    as usize;
                let source = ((y * image.width + x) * 4) as usize;
                let pixel: [u8; 4] = image.rgba[source..source + 4].try_into().unwrap();
                assert_eq!(
                    actual[target].replace(pixel),
                    None,
                    "pixel ({}, {}) emitted twice",
                    target % width as usize,
                    target / width as usize
                );
            }
        }
    }
    assert_eq!(
        actual,
        pixels
            .chunks_exact(4)
            .map(|pixel| Some(<[u8; 4]>::try_from(pixel).unwrap()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn one_cell_sixel_tiles_use_bounded_stack_space() {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let paint = paint([255, 0, 0, 255].repeat(64 * 64), 64, 64, 0);
            let kind = ImageOutputKind::Sixel {
                palette_colors: 256,
                max_width: Some(1),
                max_height: Some(1),
            };
            let cell_size = PixelCellSize::new(1, 1).unwrap();
            let key = output_encode_key(kind, cell_size, &paint);
            let request = WorkerRequest {
                generation: 1,
                kind,
                cell_size,
                measured_cell_size: Some(cell_size),
                cells: Some(Arc::new(blank_snapshot(paint.target))),
                paints: vec![paint.clone()],
                keys: vec![key],
                kitty_images: Vec::new(),
                cancel: Arc::new(AtomicBool::new(false)),
            };
            let units = encode_sixel_template(
                &request,
                &Plan {
                    paint: &paint,
                    key,
                    iterm: ItermComposition::default(),
                    sixel: SixelComposition::default(),
                    compatibility: ImageCompatibility::default(),
                    opaque: true,
                },
                &[],
                256,
                Some(1),
                Some(1),
                MAX_NATIVE_FRAME_OUTPUT_BYTES,
            )
            .unwrap();
            let actual = units
                .into_iter()
                .map(|unit| (unit.offset, decode_sixel_unit(&unit.bytes)))
                .collect::<Vec<_>>();
            let expected = (0..64)
                .flat_map(|y| {
                    (0..64).map(move |x| {
                        (
                            (x, y),
                            DecodedImage {
                                width: 1,
                                height: 1,
                                rgba: vec![255, 0, 0, 255],
                            },
                        )
                    })
                })
                .collect::<Vec<_>>();
            assert_eq!(actual, expected);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn maximum_visible_placements_reach_the_worker_without_drops() {
    for kind in [
        ImageOutputKind::Iterm,
        ImageOutputKind::Sixel {
            palette_colors: 256,
            max_width: None,
            max_height: None,
        },
    ] {
        let source = paint(vec![255, 0, 0, 255], 1, 1, 0);
        let mut paints = Vec::new();
        let mut expected = Vec::new();
        for y in 0..64 {
            for x in 0..64 {
                let id = u64::from(y) * 64 + u64::from(x) + 1;
                let target = Rect::new(x, y, 1, 1);
                paints.push(ImagePaint::new(
                    source.key.0,
                    id,
                    Arc::clone(&source.record),
                    target,
                    source.source,
                    0,
                ));
                expected.push((
                    (source.key.0, id),
                    target,
                    source.source,
                    vec![255, 0, 0, 255],
                ));
            }
        }
        let mut state = ImageOutputState::disabled();
        state.kind = Some(kind);
        let (sender, receiver) = mpsc::sync_channel(1);
        state.requests = Some(sender);
        state.prepare_frame(
            &paints,
            Some(Arc::new(blank_snapshot(Rect::new(0, 0, 64, 64)))),
            Some(PixelCellSize::new(1, 1).unwrap()),
        );
        let actual = receiver.try_recv().map(|request| {
            request
                .paints
                .into_iter()
                .map(|paint| {
                    (
                        paint.key,
                        paint.target,
                        paint.source,
                        paint.record.image.rgba.clone(),
                    )
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(actual, Ok(expected), "{kind:?}");
    }
}

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
    let source = ImageSourceRect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let alpha = alpha_stats(&image, source);
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
        source,
        cell_offset_x: None,
        cell_offset_y: None,
        z_index,
        alpha,
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
fn opaque_negative_z_image_is_kept_when_a_glyph_is_under_it() {
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

    let plan = classify(
        ImageOutputKind::Iterm,
        &snapshot,
        &HashSet::new(),
        &paint,
        key,
    )
    .expect("the image remains in the output plan");
    assert!(plan.compatibility.text_layer_order);
}

#[test]
fn iterm_partial_alpha_on_a_default_blank_preserves_rgba() {
    let paint = paint(vec![10, 20, 30, 128], 1, 1, 1);
    let paints = [paint];
    let cells = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let plans = classify_plans(ImageOutputKind::Iterm, &cells, &paints, None);

    assert_eq!(plans[0].compatibility, ImageCompatibility::default());
    assert_eq!(plans[0].iterm, ItermComposition::default());
    let request = iterm_request(Arc::clone(&cells), &paints, None);
    let units = encode_iterm_template(&request, &plans[0], &[], MAX_NATIVE_FRAME_OUTPUT_BYTES)
        .expect("iTerm2 image encodes");
    assert_eq!(decode_iterm_units(&units).rgba, [10, 20, 30, 128]);
}

#[test]
fn iterm_zero_alpha_on_a_default_blank_preserves_rgba() {
    let paint = paint(vec![10, 20, 30, 0], 1, 1, 1);
    let paints = [paint];
    let cells = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let plans = classify_plans(ImageOutputKind::Iterm, &cells, &paints, None);

    assert_eq!(plans[0].compatibility, ImageCompatibility::default());
    let request = iterm_request(Arc::clone(&cells), &paints, None);
    let units = encode_iterm_template(&request, &plans[0], &[], MAX_NATIVE_FRAME_OUTPUT_BYTES)
        .expect("iTerm2 image encodes");
    assert_eq!(decode_iterm_units(&units).rgba, [10, 20, 30, 0]);
}

#[test]
fn iterm_partial_alpha_flattens_over_one_explicit_background_without_cell_size() {
    let paint = paint(vec![0, 0, 255, 128], 1, 1, 1);
    let paints = [paint];
    let cells = Arc::new(solid_cells(Rect::new(0, 0, 1, 1), [255, 0, 0]));
    let plans = classify_plans(ImageOutputKind::Iterm, &cells, &paints, None);

    assert_eq!(plans[0].compatibility, ImageCompatibility::default());
    assert_eq!(plans[0].iterm.background, Some([255, 0, 0]));
    let request = iterm_request(Arc::clone(&cells), &paints, None);
    let units = encode_iterm_template(&request, &plans[0], &[], MAX_NATIVE_FRAME_OUTPUT_BYTES)
        .expect("iTerm2 image encodes");
    assert_eq!(decode_iterm_units(&units).rgba, [127, 0, 128, 255]);
}

#[test]
fn iterm_nonopaque_pixels_over_a_glyph_are_unavailable() {
    let paint = paint(vec![0, 0, 255, 128], 1, 1, 1);
    let paints = [paint];
    let snapshot = cells(
        Rect::new(0, 0, 1, 1),
        vec![ImageCellState {
            ch: 'A',
            ..ImageCellState::default()
        }],
    );
    let plans = classify_plans(
        ImageOutputKind::Iterm,
        &snapshot,
        &paints,
        Some(PixelCellSize::new(1, 1).expect("test cell size")),
    );

    assert_eq!(
        plans[0].compatibility,
        ImageCompatibility {
            iterm_alpha: true,
            ..ImageCompatibility::default()
        }
    );
}

#[test]
fn iterm_opaque_positive_image_replaces_a_glyph() {
    let paint = paint(vec![0, 0, 255, 255], 1, 1, 1);
    let paints = [paint];
    let snapshot = cells(
        Rect::new(0, 0, 1, 1),
        vec![ImageCellState {
            ch: 'A',
            ..ImageCellState::default()
        }],
    );
    let plans = classify_plans(ImageOutputKind::Iterm, &snapshot, &paints, None);

    assert_eq!(plans[0].compatibility, ImageCompatibility::default());
    let request = iterm_request(Arc::new(snapshot), &paints, None);
    let units = encode_iterm_template(&request, &plans[0], &[], MAX_NATIVE_FRAME_OUTPUT_BYTES)
        .expect("iTerm2 image encodes");
    assert_eq!(decode_iterm_units(&units).rgba, [0, 0, 255, 255]);
}

#[test]
fn iterm_partial_images_compose_over_every_lower_alpha_layer() {
    let lower = paint(vec![255, 0, 0, 128], 1, 1, 0);
    let upper = paint(vec![0, 0, 255, 128], 1, 1, 1);
    let paints = [lower, upper];
    let cells = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let cell_size = PixelCellSize::new(1, 1).expect("test cell size");
    let plans = classify_plans(ImageOutputKind::Iterm, &cells, &paints, Some(cell_size));

    assert_eq!(plans[0].compatibility, ImageCompatibility::default());
    assert_eq!(plans[1].compatibility, ImageCompatibility::default());
    assert!(plans[1].iterm.per_cell);
    let request = iterm_request(Arc::clone(&cells), &paints, Some(cell_size));
    let units = encode_iterm_template(
        &request,
        &plans[1],
        &plans[..1],
        MAX_NATIVE_FRAME_OUTPUT_BYTES,
    )
    .expect("composed iTerm2 image encodes");
    assert_eq!(decode_iterm_units(&units).rgba, [85, 0, 170, 192]);
}

#[test]
fn iterm_partial_lower_coverage_preserves_uncovered_default_pixels() {
    let lower = paint(vec![255, 0, 0, 128], 1, 1, 0);
    let mut upper = paint(vec![0, 0, 255, 128, 0, 0, 0, 0], 2, 1, 1);
    upper.target = Rect::new(0, 0, 2, 1);
    let paints = [lower, upper];
    let cells = Arc::new(blank_snapshot(Rect::new(0, 0, 2, 1)));
    let cell_size = PixelCellSize::new(1, 1).expect("test cell size");
    let plans = classify_plans(ImageOutputKind::Iterm, &cells, &paints, Some(cell_size));
    let request = iterm_request(Arc::clone(&cells), &paints, Some(cell_size));
    let units = encode_iterm_template(
        &request,
        &plans[1],
        &plans[..1],
        MAX_NATIVE_FRAME_OUTPUT_BYTES,
    )
    .expect("partly covered iTerm2 image encodes");

    assert_eq!(plans[1].compatibility, ImageCompatibility::default());
    assert_eq!(
        decode_iterm_units(&units).rgba,
        [85, 0, 170, 192, 0, 0, 0, 0]
    );
}

#[test]
fn iterm_per_cell_backgrounds_preserve_explicit_and_default_cells() {
    let mut red = Style::default();
    red.set_bg(Color::Rgb(255, 0, 0));
    let cells = Arc::new(cells(
        Rect::new(0, 0, 2, 1),
        vec![
            ImageCellState {
                style: red,
                ..ImageCellState::default()
            },
            ImageCellState::default(),
        ],
    ));
    let paint = paint([0, 0, 255, 128].repeat(2), 2, 1, 1);
    let paints = [paint];
    let cell_size = PixelCellSize::new(1, 1).expect("test cell size");
    let plans = classify_plans(ImageOutputKind::Iterm, &cells, &paints, Some(cell_size));
    let request = iterm_request(Arc::clone(&cells), &paints, Some(cell_size));
    let units = encode_iterm_template(&request, &plans[0], &[], MAX_NATIVE_FRAME_OUTPUT_BYTES)
        .expect("per-cell iTerm2 image encodes");

    assert_eq!(plans[0].compatibility, ImageCompatibility::default());
    assert_eq!(
        decode_iterm_units(&units).rgba,
        [127, 0, 128, 255, 0, 0, 255, 128]
    );

    let without_size = classify_plans(ImageOutputKind::Iterm, &cells, &paints, None);
    assert_eq!(
        without_size[0].compatibility,
        ImageCompatibility {
            iterm_alpha: true,
            ..ImageCompatibility::default()
        }
    );
}

#[test]
fn kitty_background_layer_boundary_is_exact_for_native_host_protocols() {
    let explicit = solid_cells(Rect::new(0, 0, 1, 1), [1, 2, 3]);
    let default = blank_snapshot(Rect::new(0, 0, 1, 1));
    for kind in [
        ImageOutputKind::Iterm,
        ImageOutputKind::Sixel {
            palette_colors: 2,
            max_width: None,
            max_height: None,
        },
    ] {
        let measured = kind
            .is_sixel()
            .then(|| PixelCellSize::new(1, 1).expect("test cell size"));
        let below = [paint(vec![1, 2, 3, 255], 1, 1, -1_073_741_825)];
        let boundary = [paint(vec![1, 2, 3, 255], 1, 1, -1_073_741_824)];

        assert_eq!(
            classify_plans(kind, &explicit, &below, measured)[0].compatibility,
            ImageCompatibility {
                text_layer_order: true,
                ..ImageCompatibility::default()
            },
            "{kind:?} below boundary over RGB"
        );
        assert_eq!(
            classify_plans(kind, &explicit, &boundary, measured)[0].compatibility,
            ImageCompatibility::default(),
            "{kind:?} at boundary over RGB"
        );
        assert_eq!(
            classify_plans(kind, &default, &below, measured)[0].compatibility,
            ImageCompatibility::default(),
            "{kind:?} below boundary over default background"
        );
        assert_eq!(
            classify_plans(kind, &default, &boundary, measured)[0].compatibility,
            ImageCompatibility::default(),
            "{kind:?} at boundary over default background"
        );
    }
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

    let plan = classify(
        ImageOutputKind::Iterm,
        &second,
        &HashSet::new(),
        &paint,
        key,
    )
    .expect("the image remains in the output plan");
    assert_eq!(plan.sixel.background, None);
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

    let plan = classify(
        ImageOutputKind::Iterm,
        &snapshot,
        &HashSet::new(),
        &paint,
        key,
    )
    .expect("the image remains in the output plan");
    assert_eq!(plan.sixel.background, None);
}

#[test]
fn sixel_partial_alpha_reencodes_when_the_cell_background_changes() {
    let kind = ImageOutputKind::Sixel {
        palette_colors: 2,
        max_width: None,
        max_height: None,
    };
    let cell_size = PixelCellSize::new(1, 1).expect("test cell size");
    let paint = paint(vec![255, 0, 0, 128], 1, 1, 0);
    let red_cells = Arc::new(solid_cells(Rect::new(0, 0, 1, 1), [255, 0, 0]));
    let blue_cells = Arc::new(solid_cells(Rect::new(0, 0, 1, 1), [0, 0, 255]));
    let mut state = ImageOutputState::disabled();
    state.kind = Some(kind);
    let red_key = {
        let mut key = output_encode_key(kind, cell_size, &paint);
        key.composition =
            state.update_composition_revisions(kind, &red_cells, std::slice::from_ref(&paint))[0];
        key
    };
    let blue_key = {
        let mut key = output_encode_key(kind, cell_size, &paint);
        key.composition =
            state.update_composition_revisions(kind, &blue_cells, std::slice::from_ref(&paint))[0];
        key
    };
    assert_ne!(red_key, blue_key);

    let red_plan = classify(kind, &red_cells, &HashSet::new(), &paint, red_key)
        .expect("red background is encodable");
    let blue_plan = classify(kind, &blue_cells, &HashSet::new(), &paint, blue_key)
        .expect("blue background is encodable");
    let make_request = |cells, key| WorkerRequest {
        generation: 1,
        kind,
        cell_size,
        measured_cell_size: Some(cell_size),
        cells: Some(cells),
        paints: vec![paint.clone()],
        keys: vec![key],
        kitty_images: Vec::new(),
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let red = encode_sixel_template(
        &make_request(Arc::clone(&red_cells), red_key),
        &red_plan,
        &[],
        2,
        None,
        None,
        MAX_NATIVE_FRAME_OUTPUT_BYTES,
    )
    .expect("red output encodes");
    let blue = encode_sixel_template(
        &make_request(Arc::clone(&blue_cells), blue_key),
        &blue_plan,
        &[],
        2,
        None,
        None,
        MAX_NATIVE_FRAME_OUTPUT_BYTES,
    )
    .expect("blue output encodes");
    assert_eq!(decode_sixel_unit(&red[0].bytes).rgba, [255, 0, 0, 255]);
    assert_eq!(decode_sixel_unit(&blue[0].bytes).rgba, [128, 0, 128, 255]);
    assert_ne!(red[0].bytes, blue[0].bytes);
}

#[test]
fn sixel_overlapping_partial_alpha_is_composed_over_the_lower_image() {
    let kind = ImageOutputKind::Sixel {
        palette_colors: 2,
        max_width: None,
        max_height: None,
    };
    let cell_size = PixelCellSize::new(1, 1).expect("test cell size");
    let lower_paint = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let upper_paint = paint(vec![0, 0, 255, 128], 1, 1, 1);
    let cells = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let lower_key = output_encode_key(kind, cell_size, &lower_paint);
    let upper_key = output_encode_key(kind, cell_size, &upper_paint);
    let lower = classify(kind, &cells, &HashSet::new(), &lower_paint, lower_key)
        .expect("lower image is encodable");
    let mut covered = HashSet::new();
    add_target_cells(lower_paint.target, &mut covered);
    let upper = classify(kind, &cells, &covered, &upper_paint, upper_key)
        .expect("upper image remains in the output plan");
    let mut plans = vec![lower, upper];
    resolve_compatibility(kind, Some(cell_size), &mut plans);
    let upper = &plans[1];
    assert!(upper.sixel.alpha);
    assert_eq!(upper.compatibility, ImageCompatibility::default());
    let request = WorkerRequest {
        generation: 1,
        kind,
        cell_size,
        measured_cell_size: Some(cell_size),
        cells: Some(cells),
        paints: vec![lower_paint.clone(), upper_paint.clone()],
        keys: vec![lower_key, upper_key],
        kitty_images: Vec::new(),
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let units = encode_sixel_template(
        &request,
        upper,
        &plans[..1],
        2,
        None,
        None,
        MAX_NATIVE_FRAME_OUTPUT_BYTES,
    )
    .expect("composed Sixel output encodes");

    assert_eq!(decode_sixel_unit(&units[0].bytes).rgba, [128, 0, 128, 255]);
}

#[test]
fn sixel_terminal_background_requires_the_cell_background_under_opaque_lower_pixels() {
    let kind = ImageOutputKind::Sixel {
        palette_colors: 2,
        max_width: None,
        max_height: None,
    };
    let cell_size = PixelCellSize::new(1, 1).expect("test cell size");
    let lower = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let mut upper = paint(vec![0, 0, 255, 0], 1, 1, 1);
    let mut record = upper.record.as_ref().clone();
    record.display.sixel_background = Some(SixelBackground::Terminal);
    upper.record = Arc::new(record);
    let paints = [lower, upper];

    let default_cells = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let default_plans = classify_plans(kind, &default_cells, &paints, Some(cell_size));
    assert_eq!(
        default_plans[1].compatibility,
        ImageCompatibility {
            sixel_terminal_background: true,
            ..ImageCompatibility::default()
        }
    );

    let rgb_cells = Arc::new(solid_cells(Rect::new(0, 0, 1, 1), [0, 255, 0]));
    let rgb_plans = classify_plans(kind, &rgb_cells, &paints, Some(cell_size));
    assert_eq!(rgb_plans[1].compatibility, ImageCompatibility::default());
    let request = WorkerRequest {
        generation: 1,
        kind,
        cell_size,
        measured_cell_size: Some(cell_size),
        cells: Some(rgb_cells),
        paints: paints.to_vec(),
        keys: paints
            .iter()
            .map(|paint| output_encode_key(kind, cell_size, paint))
            .collect(),
        kitty_images: Vec::new(),
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let units = encode_sixel_template(
        &request,
        &rgb_plans[1],
        &rgb_plans[..1],
        2,
        None,
        None,
        MAX_NATIVE_FRAME_OUTPUT_BYTES,
    )
    .expect("known terminal background encodes");
    assert_eq!(decode_sixel_unit(&units[0].bytes).rgba, [0, 255, 0, 255]);
}

#[test]
fn sixel_composition_revision_tracks_exact_cells_targets_and_pixels() {
    let kind = ImageOutputKind::Sixel {
        palette_colors: 2,
        max_width: None,
        max_height: None,
    };
    let area = Rect::new(0, 0, 1, 1);
    let blank = Arc::new(solid_cells(area, [20, 30, 40]));
    let paint = paint(vec![255, 0, 0, 127], 1, 1, 0);
    let mut state = ImageOutputState::disabled();

    assert_eq!(
        state.update_composition_revisions(kind, &blank, std::slice::from_ref(&paint)),
        [1]
    );
    assert_eq!(
        state.update_composition_revisions(
            kind,
            &Arc::new(blank.as_ref().clone()),
            std::slice::from_ref(&paint)
        ),
        [1]
    );

    let mut glyph = ImageCellState {
        style: blank.cell(0, 0).expect("blank cell").style,
        ..ImageCellState::default()
    };
    glyph.ch = 'X';
    let with_glyph =
        Arc::new(ImageCellSnapshot::from_cells(area, vec![glyph]).expect("test cells"));
    assert_eq!(
        state.update_composition_revisions(kind, &with_glyph, std::slice::from_ref(&paint)),
        [2]
    );

    let mut moved = paint.clone();
    moved.target.x = 1;
    assert_eq!(
        state.update_composition_revisions(kind, &with_glyph, std::slice::from_ref(&moved)),
        [3]
    );

    let mut changed_pixels = moved.clone();
    let mut record = changed_pixels.record.as_ref().clone();
    record.image = Arc::new(DecodedImage {
        width: 1,
        height: 1,
        rgba: vec![0, 0, 255, 127],
    });
    changed_pixels.record = Arc::new(record);
    assert_eq!(changed_pixels.content_id, moved.content_id);
    assert_eq!(changed_pixels.key, moved.key);
    assert_eq!(changed_pixels.target, moved.target);
    assert_eq!(
        state.update_composition_revisions(
            kind,
            &with_glyph,
            std::slice::from_ref(&changed_pixels)
        ),
        [4]
    );
}

#[test]
fn sixel_composition_ignores_cells_outside_every_image() {
    let kind = ImageOutputKind::Sixel {
        palette_colors: 2,
        max_width: None,
        max_height: None,
    };
    let area = Rect::new(0, 0, 2, 1);
    let paint = paint(vec![255, 0, 0, 127], 1, 1, 0);
    let initial = Arc::new(cells(
        area,
        vec![ImageCellState::default(), ImageCellState::default()],
    ));
    let outside_glyph = Arc::new(cells(
        area,
        vec![
            ImageCellState::default(),
            ImageCellState {
                ch: 'X',
                ..ImageCellState::default()
            },
        ],
    ));
    let mut state = ImageOutputState::disabled();

    assert_eq!(
        state.update_composition_revisions(kind, &initial, std::slice::from_ref(&paint)),
        [1]
    );
    assert_eq!(
        state.update_composition_revisions(kind, &outside_glyph, std::slice::from_ref(&paint)),
        [1]
    );
}

#[test]
fn iterm_composition_revision_tracks_only_each_target_and_its_lower_images() {
    let kind = ImageOutputKind::Iterm;
    let first = paint(vec![255, 0, 0, 128], 1, 1, 0);
    let mut outside = paint(vec![0, 255, 0, 128], 1, 1, 0);
    outside.target.x = 1;
    let paints = [first.clone(), outside.clone()];
    let blank = Arc::new(blank_snapshot(Rect::new(0, 0, 2, 1)));
    let mut state = ImageOutputState::disabled();
    let initial = state.update_composition_revisions(kind, &blank, &paints);
    assert_eq!(initial, [1, 2]);

    let mut blue = Style::default();
    blue.set_bg(Color::Rgb(0, 0, 255));
    let changed_outside = Arc::new(cells(
        Rect::new(0, 0, 2, 1),
        vec![
            ImageCellState::default(),
            ImageCellState {
                style: blue,
                ..ImageCellState::default()
            },
        ],
    ));
    let outside_change = state.update_composition_revisions(kind, &changed_outside, &paints);
    assert_eq!(outside_change[0], initial[0]);
    assert_ne!(outside_change[1], initial[1]);

    let mut red = Style::default();
    red.set_bg(Color::Rgb(255, 0, 0));
    let changed_target = Arc::new(cells(
        Rect::new(0, 0, 2, 1),
        vec![
            ImageCellState {
                style: red,
                ..ImageCellState::default()
            },
            changed_outside.cell(1, 0).expect("outside cell").clone(),
        ],
    ));
    let target_change = state.update_composition_revisions(kind, &changed_target, &paints);
    assert_ne!(target_change[0], outside_change[0]);
}

#[test]
fn iterm_composition_revision_changes_with_an_intersecting_lower_image() {
    let kind = ImageOutputKind::Iterm;
    let lower = paint(vec![255, 0, 0, 128], 1, 1, 0);
    let upper = paint(vec![0, 0, 255, 128], 1, 1, 1);
    let cells = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let mut state = ImageOutputState::disabled();
    let initial = state.update_composition_revisions(kind, &cells, &[lower.clone(), upper.clone()]);

    let mut changed_lower = lower;
    let mut record = changed_lower.record.as_ref().clone();
    record.image = Arc::new(DecodedImage {
        width: 1,
        height: 1,
        rgba: vec![0, 255, 0, 128],
    });
    changed_lower.record = Arc::new(record);
    let changed = state.update_composition_revisions(kind, &cells, &[changed_lower, upper]);

    assert_ne!(changed[0], initial[0]);
    assert_ne!(changed[1], initial[1]);
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

    let plan = classify(
        ImageOutputKind::Sixel {
            palette_colors: 2,
            max_width: None,
            max_height: None,
        },
        &snapshot,
        &HashSet::new(),
        &paint,
        key,
    )
    .expect("transparent Sixel pixels remain in the output plan");
    assert_eq!(plan.compatibility, ImageCompatibility::default());
}

#[test]
fn output_state_keeps_i_term_available_without_a_pixel_cell_query() {
    let state = ImageOutputState::new(Some(ImageOutputKind::Iterm));

    assert_eq!(state.kind(), Some(ImageOutputKind::Iterm));
    assert!(!state.work_pending());
}

#[test]
fn disconnected_worker_settles_each_distinct_frame_as_unavailable() {
    let mut state = ImageOutputState::disabled();
    state.kind = Some(ImageOutputKind::Iterm);
    let (sender, receiver) = mpsc::sync_channel(1);
    drop(receiver);
    state.requests = Some(sender);
    let cells = Some(Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1))));
    let first_output = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let first = ImagePaint::new(
        first_output.key.0,
        first_output.key.1,
        Arc::clone(&first_output.record),
        first_output.target,
        first_output.source,
        first_output.z_index,
    );

    assert!(state.prepare_frame(
        std::slice::from_ref(&first),
        cells.clone(),
        Some(PixelCellSize::new(1, 1).expect("test cell size")),
    ));
    assert_eq!(state.prepared_keys(), []);
    assert!(!state.work_pending());
    state.commit_frame();

    let second_output = paint(vec![0, 0, 255, 255], 1, 1, 0);
    let second = ImagePaint::new(
        first_output.key.0,
        2,
        Arc::clone(&second_output.record),
        second_output.target,
        second_output.source,
        second_output.z_index,
    );
    assert!(state.prepare_frame(
        std::slice::from_ref(&second),
        cells,
        Some(PixelCellSize::new(1, 1).expect("test cell size")),
    ));
    assert_eq!(state.prepared_keys(), []);
    assert!(!state.work_pending());
}

#[test]
fn i_term_packet_accounting_uses_the_native_frame_limit() {
    assert_eq!(
        checked_output_len(MAX_SIXEL_OUTPUT_BYTES, 1, MAX_NATIVE_FRAME_OUTPUT_BYTES)
            .expect("the native frame limit is larger"),
        MAX_SIXEL_OUTPUT_BYTES + 1
    );
    assert!(checked_output_len(
        MAX_NATIVE_FRAME_OUTPUT_BYTES,
        1,
        MAX_NATIVE_FRAME_OUTPUT_BYTES
    )
    .is_err());
}

#[test]
fn output_key_reuses_pixels_across_frame_record_wrappers() {
    let first = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let mut second = first.clone();
    second.record = Arc::new(first.record.as_ref().clone());

    let cell_size = PixelCellSize::new(1, 1).expect("test cell size");

    assert_eq!(
        output_encode_key(ImageOutputKind::Iterm, cell_size, &first),
        output_encode_key(ImageOutputKind::Iterm, cell_size, &second)
    );

    let mut different_z_index = second;
    different_z_index.z_index = 1;
    assert_ne!(
        output_encode_key(ImageOutputKind::Iterm, cell_size, &first),
        output_encode_key(ImageOutputKind::Iterm, cell_size, &different_z_index)
    );
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
fn scaled_tile_samples_from_the_visible_crop_origin() {
    let image = DecodedImage {
        width: 2,
        height: 1,
        rgba: vec![255, 0, 0, 255, 0, 0, 255, 255],
    };

    let scaled = scaled_tile(
        &image,
        ImageSourceRect {
            x: 1,
            y: 0,
            width: 1,
            height: 1,
        },
        Rect::new(0, 0, 1, 1),
        TileRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        1,
        1,
        None,
    )
    .expect("the crop is inside the source image");

    assert_eq!(scaled.rgba, vec![0, 0, 255, 255]);
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
        measured_cell_size: Some(cell_size),
        cells: Some(cells),
        paints: vec![paint],
        keys: vec![key],
        kitty_images: Vec::new(),
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let (sender, receiver) = mpsc::sync_channel(8);

    run_job(&request, &sender).expect("worker encodes the pixel");
    let messages = receiver.try_iter().collect::<Vec<_>>();

    assert_eq!(messages.len(), 2);
    let WorkerMessage::Prepared { .. } = &messages[0] else {
        panic!("the worker prepares before output");
    };
    let WorkerMessage::Unit(unit) = &messages[1] else {
        panic!("the worker emits one packet");
    };
    assert!(unit.bytes.starts_with(b"\x1b]1337;"));
    assert!(unit.bytes.ends_with(b"\x1b\\"));
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
        measured_cell_size: Some(cell_size),
        cells: Some(cells),
        paints: vec![paint],
        keys: vec![key],
        kitty_images: Vec::new(),
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let (sender, receiver) = mpsc::sync_channel(8);

    run_job(&request, &sender).expect("worker encodes the pixel");
    let messages = receiver.try_iter().collect::<Vec<_>>();

    assert_eq!(messages.len(), 2);
    let WorkerMessage::Unit(unit) = &messages[1] else {
        panic!("the worker emits one tile");
    };
    assert_eq!(unit.offset, (0, 0));
    assert!(unit.bytes.starts_with(b"\x1bP"));
    assert!(unit.bytes.ends_with(b"\x1b\\"));
    assert!(unit.bytes.len() <= MAX_SIXEL_TILE_BYTES);
}

#[test]
fn worker_emits_shared_templates_in_original_paint_order() {
    let cell_size = PixelCellSize::new(1, 1).expect("test cell size");
    let pane = PaneId::new();
    for kind in [
        ImageOutputKind::Iterm,
        ImageOutputKind::Sixel {
            palette_colors: 2,
            max_width: None,
            max_height: None,
        },
    ] {
        let mut first = paint(vec![255, 0, 0, 255], 1, 1, 0);
        first.key = (pane, 1);
        let mut middle = paint(vec![0, 0, 255, 255], 1, 1, 0);
        middle.key = (pane, 2);
        middle.content_id = 2;
        let mut last = first.clone();
        last.key = (pane, 3);
        let paints = vec![first, middle, last];
        let request = WorkerRequest {
            generation: 1,
            kind,
            cell_size,
            measured_cell_size: Some(cell_size),
            cells: Some(Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)))),
            keys: paints
                .iter()
                .map(|paint| output_encode_key(kind, cell_size, paint))
                .collect(),
            paints,
            kitty_images: Vec::new(),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let (sender, receiver) = mpsc::sync_channel(16);

        run_job(&request, &sender).expect("the worker encodes all paints");
        let units = receiver
            .try_iter()
            .filter_map(|message| match message {
                WorkerMessage::Unit(unit) => Some(unit),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            units.iter().map(|unit| unit.key).collect::<Vec<_>>(),
            [(pane, 1), (pane, 2), (pane, 3)],
            "{kind:?}"
        );
        let pixels = units
            .iter()
            .map(|unit| match kind {
                ImageOutputKind::Iterm => {
                    decode_iterm_units(&[TemplateUnit {
                        offset: unit.offset,
                        bytes: Arc::clone(&unit.bytes),
                    }])
                    .rgba
                }
                ImageOutputKind::Sixel { .. } => decode_sixel_unit(&unit.bytes).rgba,
                ImageOutputKind::Kitty => unreachable!(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            pixels,
            [
                vec![255, 0, 0, 255],
                vec![0, 0, 255, 255],
                vec![255, 0, 0, 255],
            ],
            "{kind:?}"
        );
    }
}

#[test]
fn unavailable_paint_does_not_skip_independent_output() {
    let cell_size = PixelCellSize::new(1, 1).expect("test cell size");
    let pane = PaneId::new();
    for kind in [
        ImageOutputKind::Iterm,
        ImageOutputKind::Sixel {
            palette_colors: 2,
            max_width: None,
            max_height: None,
        },
    ] {
        for unavailable_first in [true, false] {
            let mut unavailable = paint(vec![255, 0, 0, 128], 1, 1, 0);
            unavailable.key = (pane, 1);
            unavailable.target.x = 1;
            let mut available = paint(vec![0, 0, 255, 255], 1, 1, 0);
            available.key = (pane, 2);
            available.content_id = 2;
            let paints = if unavailable_first {
                vec![unavailable, available]
            } else {
                vec![available, unavailable]
            };
            let request = WorkerRequest {
                generation: 1,
                kind,
                cell_size,
                measured_cell_size: Some(cell_size),
                cells: Some(Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)))),
                keys: paints
                    .iter()
                    .map(|paint| output_encode_key(kind, cell_size, paint))
                    .collect(),
                paints,
                kitty_images: Vec::new(),
                cancel: Arc::new(AtomicBool::new(false)),
            };
            let (sender, receiver) = mpsc::sync_channel(16);

            run_job(&request, &sender).expect("the worker continues after an unavailable paint");
            let messages = receiver.try_iter().collect::<Vec<_>>();
            let unavailable_keys = messages
                .iter()
                .filter_map(|message| match message {
                    WorkerMessage::Unavailable { key, .. } => Some(*key),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let unit_keys = messages
                .iter()
                .filter_map(|message| match message {
                    WorkerMessage::Unit(unit) => Some(unit.key),
                    _ => None,
                })
                .collect::<Vec<_>>();

            assert_eq!(unavailable_keys, [(pane, 1)], "{kind:?}");
            assert_eq!(unit_keys, [(pane, 2)], "{kind:?}");
        }
    }
}

#[test]
fn worker_stops_before_unique_templates_exceed_the_frame_bound() {
    let kind = ImageOutputKind::Iterm;
    let cell_size = PixelCellSize::new(1, 1).expect("test cell size");
    let pane = PaneId::new();
    let paints = (0u8..8)
        .map(|index| {
            let mut paint = paint(vec![index, 0, 255 - index, 255], 1, 1, 0);
            paint.key = (pane, u64::from(index) + 1);
            paint.content_id = u64::from(index) + 1;
            paint
        })
        .collect::<Vec<_>>();
    let cells = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let request = WorkerRequest {
        generation: 1,
        kind,
        cell_size,
        measured_cell_size: Some(cell_size),
        cells: Some(Arc::clone(&cells)),
        keys: paints
            .iter()
            .map(|paint| output_encode_key(kind, cell_size, paint))
            .collect(),
        paints,
        kitty_images: Vec::new(),
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let plans = classify_plans(kind, &cells, &request.paints, Some(cell_size));
    let first_size = encode_iterm_template(&request, &plans[0], &[], MAX_NATIVE_FRAME_OUTPUT_BYTES)
        .expect("first template encodes")
        .iter()
        .map(|unit| unit.bytes.len())
        .sum::<usize>();
    let second_size = encode_iterm_template(
        &request,
        &plans[1],
        &plans[..1],
        MAX_NATIVE_FRAME_OUTPUT_BYTES,
    )
    .expect("second template encodes")
    .iter()
    .map(|unit| unit.bytes.len())
    .sum::<usize>();
    let limit = first_size + second_size - 1;
    let (sender, receiver) = mpsc::sync_channel(32);

    assert_eq!(run_job_with_limit(&request, &sender, limit), Err(()));
    let messages = receiver.try_iter().collect::<Vec<_>>();
    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message, WorkerMessage::Prepared { .. }))
            .count(),
        1
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message, WorkerMessage::Unit(_)))
            .count(),
        1
    );
}

#[test]
fn rejected_cumulative_worker_output_cancels_the_generation() {
    let paint = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let key = paint.key;
    let cancel = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::sync_channel(2);
    sender
        .send(WorkerMessage::Unit(OutputUnit {
            generation: 1,
            key,
            kind: ImageOutputKind::Iterm,
            offset: (0, 0),
            bytes: Arc::from(&b"x"[..]),
        }))
        .expect("unit queues");
    sender
        .send(WorkerMessage::Finished {
            generation: 1,
            failed: false,
        })
        .expect("finish queues");
    drop(sender);
    let mut state = ImageOutputState::disabled();
    state.kind = Some(ImageOutputKind::Iterm);
    state.generation = 1;
    state.latest = vec![paint];
    state.rebuild_latest_index();
    state.prepared_set.insert(key);
    state.unit_bytes = MAX_NATIVE_FRAME_OUTPUT_BYTES;
    state.messages = Some(receiver);
    state.active = Some(ActiveJob {
        generation: 1,
        cancel: Arc::clone(&cancel),
    });

    state.poll();

    assert!(cancel.load(Ordering::Acquire));
    assert_eq!(state.unit_bytes, 0);
    assert_eq!(state.units.len(), 0);
    assert!(state.ready);
    assert!(!state.work_pending());
}

#[test]
fn one_cell_sixel_output_may_exceed_one_transport_chunk() {
    let (width, height) = (256, 256);
    let palette = [
        [0, 0, 0],
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [255, 255, 0],
        [255, 0, 255],
        [0, 255, 255],
        [255, 255, 255],
    ];
    let mut random = 0x9e37_79b9u32;
    let mut rgba = Vec::with_capacity(width * height * 4);
    for _ in 0..width * height {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        rgba.extend_from_slice(&palette[(random as usize) % palette.len()]);
        rgba.push(255);
    }
    let mut paint = paint(rgba.clone(), width as u32, height as u32, 0);
    paint.target = Rect::new(0, 0, 1, 1);
    let kind = ImageOutputKind::Sixel {
        palette_colors: palette.len(),
        max_width: None,
        max_height: None,
    };
    let cell_size = PixelCellSize::new(width as u16, height as u16).expect("test cell size");
    let key = output_encode_key(kind, cell_size, &paint);
    let request = WorkerRequest {
        generation: 1,
        kind,
        cell_size,
        measured_cell_size: Some(cell_size),
        cells: Some(Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)))),
        paints: vec![paint.clone()],
        keys: vec![key],
        kitty_images: Vec::new(),
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let units = encode_sixel_template(
        &request,
        &Plan {
            paint: &paint,
            key,
            iterm: ItermComposition::default(),
            sixel: SixelComposition::default(),
            compatibility: ImageCompatibility::default(),
            opaque: true,
        },
        &[],
        palette.len(),
        None,
        None,
        MAX_NATIVE_FRAME_OUTPUT_BYTES,
    )
    .expect("one-cell Sixel output encodes");

    assert_eq!(units.len(), 1);
    assert!(units[0].bytes.len() > MAX_SIXEL_TILE_BYTES);
    assert!(units[0].bytes.len() <= MAX_SIXEL_OUTPUT_BYTES);
    assert_eq!(decode_sixel_unit(&units[0].bytes).rgba, rgba);
}

#[test]
fn i_term_unit_output_has_exact_position_payload_and_cursor_restore() {
    let paint = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let mut state = ImageOutputState::new(Some(ImageOutputKind::Iterm));
    state.latest = vec![paint];
    state.rebuild_latest_index();
    state.units.push(OutputUnit {
        generation: 1,
        key: state.latest[0].key,
        kind: ImageOutputKind::Iterm,
        offset: (0, 0),
        bytes: Arc::from(&b"body"[..]),
    });
    let output = state
        .frame_output(Some(ratatui::layout::Position { x: 4, y: 5 }))
        .expect("frame output writes");
    assert_eq!(output, b"\x1b[1;1Hbody\x1b[6;5H");
}

#[test]
fn sixel_unit_output_has_exact_mode_boundaries_and_cursor_restore() {
    let paint = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let kind = ImageOutputKind::Sixel {
        palette_colors: 2,
        max_width: None,
        max_height: None,
    };
    let mut state = ImageOutputState::new(Some(kind));
    state.latest = vec![paint];
    state.rebuild_latest_index();
    state.units.push(OutputUnit {
        generation: 1,
        key: state.latest[0].key,
        kind,
        offset: (0, 0),
        bytes: Arc::from(&b"sixel"[..]),
    });
    let output = state
        .frame_output(Some(ratatui::layout::Position { x: 4, y: 5 }))
        .expect("frame output writes");
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

fn kitty_state() -> (ImageOutputState, mpsc::Receiver<WorkerRequest>) {
    let mut state = ImageOutputState::disabled();
    state.kind = Some(ImageOutputKind::Kitty);
    let (sender, receiver) = mpsc::sync_channel(4);
    state.requests = Some(sender);
    (state, receiver)
}

#[test]
fn kitty_transmits_one_image_once_and_replaces_it_after_it_moves() {
    let source = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let at = |y: u16| {
        ImagePaint::new(
            source.key.0,
            1,
            Arc::clone(&source.record),
            Rect::new(0, y, 1, 1),
            source.source,
            0,
        )
    };
    let (mut state, requests) = kitty_state();

    assert!(!state.prepare_frame(&[at(0)], None, None));
    let first = requests
        .try_recv()
        .expect("the first frame reaches the worker");
    assert_eq!(
        first.kitty_images,
        vec![KittyPaintImage {
            number: 1,
            transmit: true,
        }]
    );
    state.commit_frame();
    state.active = None;

    assert!(!state.prepare_frame(&[at(5)], None, None));
    let moved = requests
        .try_recv()
        .expect("the moved frame reaches the worker");
    assert_eq!(
        moved.kitty_images,
        vec![KittyPaintImage {
            number: 1,
            transmit: false,
        }]
    );
    assert_eq!(state.kitty_image_deletes, Vec::<u32>::new());
    assert!(!state.kitty_free_all);
}

#[test]
fn kitty_frees_one_image_number_after_its_content_leaves_the_frame() {
    let source = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let placed = ImagePaint::new(
        source.key.0,
        1,
        Arc::clone(&source.record),
        Rect::new(0, 0, 1, 1),
        source.source,
        0,
    );
    let (mut state, requests) = kitty_state();

    assert!(!state.prepare_frame(&[placed], None, None));
    requests
        .try_recv()
        .expect("the first frame reaches the worker");
    state.commit_frame();
    state.active = None;
    state.host_pixels_present = true;

    assert!(state.prepare_frame(&[], None, None));
    assert_eq!(state.kitty_image_deletes, vec![1]);
    let mut bytes = Vec::new();
    assert!(!state
        .write_frame_reset(&mut bytes)
        .expect("the frame reset writes"));
    assert_eq!(
        bytes,
        b"\x1b_Ga=d,d=a,q=2;\x1b\\\x1b_Ga=d,d=N,I=1,q=2;\x1b\\"
    );
    assert_eq!(state.kitty_image_deletes, Vec::<u32>::new());
}

#[test]
fn a_failed_kitty_frame_frees_every_image_the_host_holds() {
    let source = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let placed = ImagePaint::new(
        source.key.0,
        1,
        Arc::clone(&source.record),
        Rect::new(0, 0, 1, 1),
        source.source,
        0,
    );
    let (mut state, requests) = kitty_state();

    assert!(!state.prepare_frame(&[placed], None, None));
    requests
        .try_recv()
        .expect("the first frame reaches the worker");
    state.commit_frame();
    state.fail_frame_commit();

    assert!(state.kitty_free_all);
    assert!(state.kitty_images.is_empty());
    let mut bytes = Vec::new();
    assert!(!state
        .write_frame_reset(&mut bytes)
        .expect("the frame reset writes"));
    assert_eq!(bytes, b"\x18\x1b\\\x1b_Ga=d,d=A,q=2;\x1b\\");
    assert!(!state.kitty_free_all);
}

#[test]
fn a_failed_iterm_frame_frees_no_kitty_image() {
    let mut state = ImageOutputState::disabled();
    state.kind = Some(ImageOutputKind::Iterm);

    state.fail_frame_commit();

    assert!(!state.kitty_free_all);
}

#[test]
fn an_opaque_iterm_or_sixel_image_that_moves_reuses_its_encoded_output() {
    use std::time::{Duration, Instant};

    for kind in [
        ImageOutputKind::Iterm,
        ImageOutputKind::Sixel {
            palette_colors: 256,
            max_width: None,
            max_height: None,
        },
    ] {
        let source = paint(vec![255, 0, 0, 255], 1, 1, 0);
        let at = |y: u16| {
            ImagePaint::new(
                source.key.0,
                1,
                Arc::clone(&source.record),
                Rect::new(0, y, 1, 1),
                source.source,
                0,
            )
        };
        let cell_size = PixelCellSize::new(1, 1).expect("one-pixel cell");
        let cells = Arc::new(blank_snapshot(Rect::new(0, 0, 8, 8)));
        let mut state = ImageOutputState::new(Some(kind));

        let deadline = Instant::now() + Duration::from_secs(5);
        while !state.prepare_frame(&[at(0)], Some(Arc::clone(&cells)), Some(cell_size)) {
            assert!(Instant::now() < deadline, "{kind:?} did not settle");
            std::thread::yield_now();
        }
        let first = state.frame_output(None).expect("the first frame writes");
        state.commit_frame();
        assert!(!first.is_empty(), "{kind:?} wrote no pixels");

        assert!(
            state.prepare_frame(&[at(3)], Some(cells), Some(cell_size)),
            "{kind:?} did not commit the moved frame"
        );
        assert!(!state.work_pending(), "{kind:?} started another encode");
        let moved = state.frame_output(None).expect("the moved frame writes");
        let rebased = String::from_utf8(moved)
            .expect("image output is ASCII")
            .replacen("\x1b[4;1H", "\x1b[1;1H", 1)
            .into_bytes();
        assert_eq!(rebased, first, "{kind:?} re-encoded the moved image");
    }
}

#[test]
fn a_partly_transparent_iterm_image_that_moves_encodes_again() {
    let source = paint(vec![255, 0, 0, 128], 1, 1, 0);
    let at = |y: u16| {
        ImagePaint::new(
            source.key.0,
            1,
            Arc::clone(&source.record),
            Rect::new(0, y, 1, 1),
            source.source,
            0,
        )
    };
    let cell_size = PixelCellSize::new(1, 1).expect("one-pixel cell");
    let cells = Arc::new(blank_snapshot(Rect::new(0, 0, 8, 8)));
    let mut state = ImageOutputState::disabled();
    state.kind = Some(ImageOutputKind::Iterm);
    let (sender, requests) = mpsc::sync_channel(4);
    state.requests = Some(sender);

    assert!(!state.prepare_frame(&[at(0)], Some(Arc::clone(&cells)), Some(cell_size)));
    let first = requests
        .try_recv()
        .expect("the first frame reaches the worker");
    state.active = None;

    assert!(!state.prepare_frame(&[at(3)], Some(cells), Some(cell_size)));
    let moved = requests
        .try_recv()
        .expect("the moved frame reaches the worker");
    assert_ne!(first.keys, moved.keys);
}

#[test]
fn a_new_image_under_one_content_identity_takes_a_new_kitty_number() {
    let first_source = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let second_source = paint(vec![0, 255, 0, 255], 1, 1, 0);
    let placed = |record: &Arc<ImageRecord>| {
        ImagePaint::new(
            first_source.key.0,
            1,
            Arc::clone(record),
            Rect::new(0, 0, 1, 1),
            first_source.source,
            0,
        )
    };
    let (mut state, requests) = kitty_state();

    assert!(!state.prepare_frame(&[placed(&first_source.record)], None, None));
    requests
        .try_recv()
        .expect("the first frame reaches the worker");
    state.commit_frame();
    state.active = None;

    assert!(!state.prepare_frame(&[placed(&second_source.record)], None, None));
    let replaced = requests
        .try_recv()
        .expect("the replacing frame reaches the worker");
    assert_eq!(
        replaced.kitty_images,
        vec![KittyPaintImage {
            number: 2,
            transmit: true,
        }]
    );
    assert_eq!(state.kitty_image_deletes, vec![1]);
}

#[test]
fn a_failed_kitty_encode_transmits_its_pixels_again() {
    let source = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let placed = || {
        ImagePaint::new(
            source.key.0,
            1,
            Arc::clone(&source.record),
            Rect::new(0, 0, 1, 1),
            source.source,
            0,
        )
    };
    let (mut state, requests) = kitty_state();
    let (messages, inbox) = mpsc::sync_channel(4);
    state.messages = Some(inbox);

    assert!(!state.prepare_frame(&[placed()], None, None));
    let job = requests
        .try_recv()
        .expect("the first frame reaches the worker");
    assert_eq!(
        job.kitty_images,
        vec![KittyPaintImage {
            number: 1,
            transmit: true,
        }]
    );
    messages
        .send(WorkerMessage::Finished {
            generation: job.generation,
            failed: true,
        })
        .expect("the failure reaches the state");

    // The failed job wrote nothing, so the host holds no image number.
    state.poll();
    assert!(state.pending_kitty_images.is_empty());
    state.commit_frame();
    assert!(state.kitty_images.is_empty());

    assert!(!state.prepare_frame(&[placed()], None, None));
    let retried = requests
        .try_recv()
        .expect("the retried frame reaches the worker");
    assert_eq!(
        retried.kitty_images,
        vec![KittyPaintImage {
            number: 2,
            transmit: true,
        }]
    );
}

#[test]
fn a_failed_iterm_or_sixel_encode_retries_an_unchanged_frame() {
    for kind in [
        ImageOutputKind::Iterm,
        ImageOutputKind::Sixel {
            palette_colors: 256,
            max_width: None,
            max_height: None,
        },
    ] {
        let source = paint(vec![255, 0, 0, 255], 1, 1, 0);
        let placed = ImagePaint::new(
            source.key.0,
            source.key.1,
            Arc::clone(&source.record),
            source.target,
            source.source,
            source.z_index,
        );
        let mut state = ImageOutputState::disabled();
        state.kind = Some(kind);
        let (sender, requests) = mpsc::sync_channel(4);
        state.requests = Some(sender);
        let (messages, inbox) = mpsc::sync_channel(4);
        state.messages = Some(inbox);
        let cells = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
        let cell_size = PixelCellSize::new(1, 1).expect("one-pixel cell");

        assert!(
            !state.prepare_frame(
                std::slice::from_ref(&placed),
                Some(Arc::clone(&cells)),
                Some(cell_size),
            ),
            "{kind:?} did not submit the first frame"
        );
        let job = requests
            .try_recv()
            .expect("the first frame reaches the worker");
        messages
            .send(WorkerMessage::Finished {
                generation: job.generation,
                failed: true,
            })
            .expect("the failure reaches the state");
        state.poll();

        assert!(
            !state.prepare_frame(std::slice::from_ref(&placed), Some(cells), Some(cell_size),),
            "{kind:?} did not resubmit the unchanged frame"
        );
        let retried = requests
            .try_recv()
            .expect("the unchanged frame reaches the worker again");
        assert_eq!(retried.generation, job.generation + 2, "{kind:?}");
        assert_eq!(retried.keys, job.keys, "{kind:?} changed the frame key");
    }
}

#[test]
fn an_unchanged_frame_after_a_commit_starts_no_work_and_commits_nothing() {
    for kind in [ImageOutputKind::Kitty, ImageOutputKind::Iterm] {
        let source = paint(vec![255, 0, 0, 255], 1, 1, 0);
        let placed = || {
            ImagePaint::new(
                source.key.0,
                1,
                Arc::clone(&source.record),
                Rect::new(0, 0, 1, 1),
                source.source,
                0,
            )
        };
        let cell_size = PixelCellSize::new(1, 1).expect("one-pixel cell");
        let cells = || Some(Arc::new(blank_snapshot(Rect::new(0, 0, 8, 8))));
        let mut state = ImageOutputState::disabled();
        state.kind = Some(kind);
        let (sender, requests) = mpsc::sync_channel(4);
        state.requests = Some(sender);

        assert!(!state.prepare_frame(&[placed()], cells(), Some(cell_size)));
        requests
            .try_recv()
            .expect("the first frame reaches the worker");
        state.active = None;
        state.ready = true;
        state.commit_frame();

        assert!(
            state.prepare_frame(&[placed()], cells(), Some(cell_size)),
            "{kind:?}"
        );
        assert!(requests.try_recv().is_err(), "{kind:?} started a job");
        assert!(!state.native_commit_pending(), "{kind:?} commits again");
        assert!(!state.screen_reset_needed, "{kind:?} resets the screen");
    }
}

#[test]
fn text_written_under_an_opaque_iterm_image_rewrites_it_without_encoding_again() {
    use std::time::{Duration, Instant};

    let source = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let placed = || {
        ImagePaint::new(
            source.key.0,
            1,
            Arc::clone(&source.record),
            Rect::new(0, 0, 1, 1),
            source.source,
            0,
        )
    };
    let cell_size = PixelCellSize::new(1, 1).expect("one-pixel cell");
    let area = Rect::new(0, 0, 2, 1);
    let mut state = ImageOutputState::new(Some(ImageOutputKind::Iterm));

    let deadline = Instant::now() + Duration::from_secs(5);
    while !state.prepare_frame(
        &[placed()],
        Some(Arc::new(blank_snapshot(area))),
        Some(cell_size),
    ) {
        assert!(Instant::now() < deadline, "the first frame did not settle");
        std::thread::yield_now();
    }
    let first = state.frame_output(None).expect("the first frame writes");
    state.commit_frame();

    let glyph_under_image = cells(
        area,
        vec![
            ImageCellState {
                ch: 'B',
                ..ImageCellState::default()
            },
            ImageCellState::default(),
        ],
    );
    assert!(state.prepare_frame(
        &[placed()],
        Some(Arc::new(glyph_under_image)),
        Some(cell_size),
    ));
    assert!(!state.work_pending(), "the glyph started an encode");
    assert!(
        state.native_commit_pending(),
        "the image is not written again"
    );
    assert!(
        !state.screen_reset_needed,
        "an unmoved image reset the screen"
    );
    let again = state.frame_output(None).expect("the repaired frame writes");
    assert_eq!(again, first);
}

#[test]
fn a_host_resize_transmits_every_kitty_image_again() {
    let source = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let placed = ImagePaint::new(
        source.key.0,
        1,
        Arc::clone(&source.record),
        Rect::new(0, 0, 1, 1),
        source.source,
        0,
    );
    let (mut state, requests) = kitty_state();
    state.note_host_size(80, 24);

    assert!(!state.prepare_frame(std::slice::from_ref(&placed), None, None));
    requests
        .try_recv()
        .expect("the first frame reaches the worker");
    state.commit_frame();
    state.active = None;

    state.note_host_size(80, 24);
    assert!(state.prepare_frame(std::slice::from_ref(&placed), None, None));
    assert!(
        requests.try_recv().is_err(),
        "an unchanged size keeps the upload"
    );

    state.note_host_size(100, 24);
    assert!(state.kitty_free_all);
    assert!(!state.prepare_frame(&[placed], None, None));
    let resized = requests
        .try_recv()
        .expect("the resized frame reaches the worker");
    assert_eq!(
        resized.kitty_images,
        vec![KittyPaintImage {
            number: 1,
            transmit: true,
        }]
    );
}

#[test]
fn an_iterm_reset_clears_the_screen_and_a_kitty_reset_does_not() {
    let mut iterm = ImageOutputState::disabled();
    iterm.kind = Some(ImageOutputKind::Iterm);
    iterm.screen_reset_needed = true;
    let mut bytes = Vec::new();
    assert!(iterm
        .write_frame_reset(&mut bytes)
        .expect("the frame reset writes"));
    assert_eq!(bytes, b"\x1b[2J");

    let (mut kitty, _requests) = kitty_state();
    kitty.screen_reset_needed = true;
    let mut bytes = Vec::new();
    assert!(!kitty
        .write_frame_reset(&mut bytes)
        .expect("the frame reset writes"));
    assert_eq!(bytes, b"\x1b_Ga=d,d=a,q=2;\x1b\\");
}

#[test]
fn a_kitty_paint_skips_the_alpha_scan_and_an_iterm_paint_runs_it() {
    let source = paint(vec![255, 0, 0, 128], 1, 1, 0);
    let placed = || {
        ImagePaint::new(
            source.key.0,
            1,
            Arc::clone(&source.record),
            Rect::new(0, 0, 1, 1),
            source.source,
            0,
        )
    };
    let (mut kitty, _requests) = kitty_state();
    assert!(!kitty.prepare_frame(&[placed()], None, None));
    assert_eq!(kitty.latest[0].alpha, None);

    let mut iterm = ImageOutputState::disabled();
    iterm.kind = Some(ImageOutputKind::Iterm);
    let (sender, _requests) = mpsc::sync_channel(4);
    iterm.requests = Some(sender);
    let cells = Some(Arc::new(blank_snapshot(Rect::new(0, 0, 8, 8))));
    assert!(!iterm.prepare_frame(&[placed()], cells, PixelCellSize::new(1, 1)));
    assert_eq!(
        iterm.latest[0].alpha,
        Some(AlphaStats {
            has_zero: false,
            has_partial: true,
        })
    );
}

#[test]
fn the_first_host_size_forgets_nothing() {
    let source = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let placed = ImagePaint::new(
        source.key.0,
        1,
        Arc::clone(&source.record),
        Rect::new(0, 0, 1, 1),
        source.source,
        0,
    );
    let (mut state, requests) = kitty_state();

    assert!(!state.prepare_frame(&[placed], None, None));
    requests
        .try_recv()
        .expect("the first frame reaches the worker");
    state.commit_frame();
    assert_eq!(state.kitty_images.len(), 1);

    state.note_host_size(80, 24);
    assert_eq!(state.kitty_images.len(), 1);
    assert!(!state.kitty_free_all);
}

#[test]
fn an_iterm_host_resize_frees_no_kitty_image_and_clears_no_key() {
    let mut state = ImageOutputState::disabled();
    state.kind = Some(ImageOutputKind::Iterm);
    state.latest_keys = vec![output_encode_key(
        ImageOutputKind::Iterm,
        PixelCellSize::new(1, 1).expect("one-pixel cell"),
        &paint(vec![255, 0, 0, 255], 1, 1, 0),
    )];
    state.note_host_size(80, 24);

    state.note_host_size(100, 30);

    assert!(!state.kitty_free_all);
    assert_eq!(state.latest_keys.len(), 1);
}

#[test]
fn a_kitty_reset_with_only_departed_numbers_frees_them_and_keeps_the_placements() {
    let (mut state, _requests) = kitty_state();
    state.kitty_image_deletes = vec![3, 7];
    let mut bytes = Vec::new();

    assert!(!state
        .write_frame_reset(&mut bytes)
        .expect("the frame reset writes"));

    assert_eq!(
        bytes,
        b"\x1b_Ga=d,d=N,I=3,q=2;\x1b\\\x1b_Ga=d,d=N,I=7,q=2;\x1b\\"
    );
    assert_eq!(state.kitty_image_deletes, Vec::<u32>::new());
}

#[test]
fn two_paints_of_one_image_in_one_frame_transmit_it_once() {
    let source = paint(vec![255, 0, 0, 255], 1, 1, 0);
    // Two placements of one server-side image share one content identity.
    let at = |placement_id: u64, y: u16| {
        let mut paint = ImagePaint::new(
            source.key.0,
            placement_id,
            Arc::clone(&source.record),
            Rect::new(0, y, 1, 1),
            source.source,
            0,
        );
        paint.content_id = 1;
        paint
    };
    let (mut state, requests) = kitty_state();

    assert!(!state.prepare_frame(&[at(1, 0), at(2, 3)], None, None));
    let job = requests.try_recv().expect("the frame reaches the worker");

    assert_eq!(
        job.kitty_images,
        vec![
            KittyPaintImage {
                number: 1,
                transmit: true,
            },
            KittyPaintImage {
                number: 1,
                transmit: false,
            },
        ]
    );
    assert_eq!(state.pending_kitty_images.len(), 1);
    state.commit_frame();
    assert_eq!(state.kitty_images.len(), 1);
    assert_eq!(state.next_kitty_image_number, 2);
}

#[test]
fn an_exhausted_kitty_number_space_frees_every_image_and_restarts_at_one() {
    let source = paint(vec![255, 0, 0, 255], 1, 1, 0);
    let placed = ImagePaint::new(
        source.key.0,
        1,
        Arc::clone(&source.record),
        Rect::new(0, 0, 1, 1),
        source.source,
        0,
    );
    let (mut state, requests) = kitty_state();
    state.next_kitty_image_number = u32::MAX;
    state.kitty_images.insert(
        9,
        KittyImage {
            number: 5,
            address: 0,
        },
    );

    assert!(!state.prepare_frame(&[placed], None, None));
    let job = requests.try_recv().expect("the frame reaches the worker");

    assert!(state.kitty_free_all);
    assert_eq!(
        job.kitty_images,
        vec![KittyPaintImage {
            number: 1,
            transmit: true,
        }]
    );
    assert_eq!(state.kitty_image_deletes, Vec::<u32>::new());
}
