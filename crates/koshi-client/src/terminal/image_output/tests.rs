//! Tests for worker-side image eligibility, shared output state, and Sixel host modes.

use super::*;

fn decode_sixel_output_unit(sixel_output_bytes: &[u8]) -> DecodedImage {
    let sixel_payload_bytes = sixel_output_bytes
        .strip_prefix(b"\x1bP")
        .unwrap()
        .strip_suffix(b"\x1b\\")
        .unwrap();
    let mut sixel_parser = koshi_sixel::SixelParser::new();
    for payload_byte in sixel_payload_bytes {
        sixel_parser.feed_input_byte(*payload_byte).unwrap();
    }
    let sixel_graphic = sixel_parser.finish_payload().unwrap();
    let mut sixel_palette = koshi_sixel::SixelPalette::default();
    sixel_palette.apply_palette_changes(sixel_graphic.get_palette_changes());
    sixel_graphic
        .get_indexed_image()
        .unwrap()
        .resolve_indexed_image(&sixel_palette, [0, 0, 0])
        .unwrap()
}

fn decode_iterm_output_units(template_units: &[TemplateUnit]) -> DecodedImage {
    let mut transfer_state = None;
    let mut decoded_image = None;
    for template_unit in template_units {
        let command_body = template_unit
            .output_bytes
            .strip_prefix(b"\x1b]1337;")
            .expect("iTerm2 packet prefix")
            .strip_suffix(b"\x1b\\")
            .expect("iTerm2 packet terminator");
        if let Some(iterm_graphics) =
            koshi_iterm::parse_iterm_command(command_body, &mut transfer_state)
                .expect("generated iTerm2 packet parses")
        {
            decoded_image = Some(iterm_graphics.image);
        }
    }
    assert_eq!(transfer_state, None);
    decoded_image.expect("generated iTerm2 packets contain one image")
}

fn classify_image_plans<'a>(
    output_kind: ImageOutputKind,
    cell_snapshot: &ImageCellSnapshot,
    output_paints: &'a [OutputPaint],
    measured_cell_size: Option<PixelCellSize>,
) -> Vec<Plan<'a>> {
    let output_cell_size = if output_kind.is_sixel() {
        measured_cell_size.expect("Sixel test cell size")
    } else {
        PixelCellSize::from_pixel_dimensions(1, 1).expect("one-pixel cell")
    };
    let mut covered_cell_positions = HashSet::new();
    let mut image_plans = output_paints
        .iter()
        .map(|output_paint| {
            let image_plan = classify_output_paint(
                output_kind,
                cell_snapshot,
                &covered_cell_positions,
                output_paint,
                build_output_encode_key(output_kind, output_cell_size, output_paint),
            )
            .expect("valid test image");
            add_target_area_cells(output_paint.target_area, &mut covered_cell_positions);
            image_plan
        })
        .collect::<Vec<_>>();
    resolve_image_compatibility(output_kind, measured_cell_size, &mut image_plans);
    image_plans
}

fn build_iterm_worker_request(
    cell_snapshot: Arc<ImageCellSnapshot>,
    output_paints: &[OutputPaint],
    measured_cell_size: Option<PixelCellSize>,
) -> WorkerRequest {
    let pixel_cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("one-pixel cell");
    WorkerRequest {
        frame_generation: 1,
        output_kind: ImageOutputKind::Iterm,
        pixel_cell_size,
        measured_pixel_cell_size: measured_cell_size,
        cell_snapshot: Some(cell_snapshot),
        output_paints: output_paints.to_vec(),
        encode_keys: output_paints
            .iter()
            .map(|output_paint| {
                build_output_encode_key(ImageOutputKind::Iterm, pixel_cell_size, output_paint)
            })
            .collect(),
        kitty_paint_images: Vec::new(),
        cancellation_token: Arc::new(AtomicBool::new(false)),
    }
}

#[test]
fn oversized_sixel_tiles_cover_every_target_pixel_exactly_once() {
    let (image_pixel_width, image_pixel_height) = (256, 128);
    let mut random_value = 0x12345678u32;
    let mut pixel_bytes = Vec::new();
    for _ in 0..image_pixel_width * image_pixel_height {
        random_value ^= random_value << 13;
        random_value ^= random_value >> 17;
        random_value ^= random_value << 5;
        pixel_bytes.extend_from_slice(&[
            if random_value & 1 == 0 { 0 } else { 255 },
            if random_value & 2 == 0 { 0 } else { 255 },
            if random_value & 4 == 0 { 0 } else { 255 },
            255,
        ]);
    }
    let output_paint = build_output_paint(
        pixel_bytes.clone(),
        image_pixel_width,
        image_pixel_height,
        0,
    );
    let output_kind = ImageOutputKind::Sixel {
        palette_color_count: 256,
        max_pixel_width: Some(128),
        max_pixel_height: Some(128),
    };
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).unwrap();
    let encode_key = build_output_encode_key(output_kind, cell_size, &output_paint);
    let worker_request = WorkerRequest {
        frame_generation: 1,
        output_kind,
        pixel_cell_size: cell_size,
        measured_pixel_cell_size: Some(cell_size),
        cell_snapshot: Some(Arc::new(blank_snapshot(output_paint.target_area))),
        output_paints: vec![output_paint.clone()],
        encode_keys: vec![encode_key],
        kitty_paint_images: Vec::new(),
        cancellation_token: Arc::new(AtomicBool::new(false)),
    };
    let template_units = encode_sixel_template(
        &worker_request,
        &Plan {
            output_paint: &output_paint,
            encode_key,
            iterm_composition: ItermComposition::default(),
            sixel_composition: SixelComposition::default(),
            image_compatibility: ImageCompatibility::default(),
            is_opaque: true,
        },
        &[],
        256,
        Some(128),
        Some(128),
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .unwrap();
    let mut actual_pixel_values = vec![None; (image_pixel_width * image_pixel_height) as usize];
    for template_unit in template_units {
        assert!(template_unit.output_bytes.len() <= MAX_SIXEL_OUTPUT_BYTE_COUNT);
        let decoded_image = decode_sixel_output_unit(&template_unit.output_bytes);
        assert!(decoded_image.pixel_width <= 128 && decoded_image.pixel_height <= 128);
        for pixel_row_index in 0..decoded_image.pixel_height {
            for pixel_column_index in 0..decoded_image.pixel_width {
                let target_pixel_index =
                    ((u32::from(template_unit.tile_offset.1) + pixel_row_index) * image_pixel_width
                        + u32::from(template_unit.tile_offset.0)
                        + pixel_column_index) as usize;
                let source_byte_index = ((pixel_row_index * decoded_image.pixel_width
                    + pixel_column_index)
                    * 4) as usize;
                let decoded_pixel: [u8; 4] = decoded_image.rgba_bytes
                    [source_byte_index..source_byte_index + 4]
                    .try_into()
                    .unwrap();
                assert_eq!(
                    actual_pixel_values[target_pixel_index].replace(decoded_pixel),
                    None,
                    "pixel ({}, {}) emitted twice",
                    target_pixel_index % image_pixel_width as usize,
                    target_pixel_index / image_pixel_width as usize
                );
            }
        }
    }
    assert_eq!(
        actual_pixel_values,
        pixel_bytes
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
            let output_paint = build_output_paint([255, 0, 0, 255].repeat(64 * 64), 64, 64, 0);
            let output_kind = ImageOutputKind::Sixel {
                palette_color_count: 256,
                max_pixel_width: Some(1),
                max_pixel_height: Some(1),
            };
            let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).unwrap();
            let encode_key = build_output_encode_key(output_kind, cell_size, &output_paint);
            let worker_request = WorkerRequest {
                frame_generation: 1,
                output_kind,
                pixel_cell_size: cell_size,
                measured_pixel_cell_size: Some(cell_size),
                cell_snapshot: Some(Arc::new(blank_snapshot(output_paint.target_area))),
                output_paints: vec![output_paint.clone()],
                encode_keys: vec![encode_key],
                kitty_paint_images: Vec::new(),
                cancellation_token: Arc::new(AtomicBool::new(false)),
            };
            let template_units = encode_sixel_template(
                &worker_request,
                &Plan {
                    output_paint: &output_paint,
                    encode_key,
                    iterm_composition: ItermComposition::default(),
                    sixel_composition: SixelComposition::default(),
                    image_compatibility: ImageCompatibility::default(),
                    is_opaque: true,
                },
                &[],
                256,
                Some(1),
                Some(1),
                MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
            )
            .unwrap();
            let actual_template_units = template_units
                .into_iter()
                .map(|unit| {
                    (
                        unit.tile_offset,
                        decode_sixel_output_unit(&unit.output_bytes),
                    )
                })
                .collect::<Vec<_>>();
            let expected_template_units = (0..64)
                .flat_map(|tile_row_index| {
                    (0..64).map(move |tile_column_index| {
                        (
                            (tile_column_index, tile_row_index),
                            DecodedImage {
                                pixel_width: 1,
                                pixel_height: 1,
                                rgba_bytes: vec![255, 0, 0, 255],
                            },
                        )
                    })
                })
                .collect::<Vec<_>>();
            assert_eq!(actual_template_units, expected_template_units);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn maximum_visible_placements_reach_the_worker_without_drops() {
    for output_kind in [
        ImageOutputKind::Iterm,
        ImageOutputKind::Sixel {
            palette_color_count: 256,
            max_pixel_width: None,
            max_pixel_height: None,
        },
    ] {
        let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
        let mut output_paints = Vec::new();
        let mut expected_output_paints = Vec::new();
        for tile_row_index in 0..64 {
            for tile_column_index in 0..64 {
                let image_placement_id =
                    u64::from(tile_row_index) * 64 + u64::from(tile_column_index) + 1;
                let target_area = Rect::new(tile_column_index, tile_row_index, 1, 1);
                output_paints.push(ImagePaint::from_image_placement(
                    source_output_paint.placement_key.0,
                    image_placement_id,
                    Arc::clone(&source_output_paint.image_record),
                    target_area,
                    source_output_paint.source_rect,
                    0,
                ));
                expected_output_paints.push((
                    (source_output_paint.placement_key.0, image_placement_id),
                    target_area,
                    source_output_paint.source_rect,
                    vec![255, 0, 0, 255],
                ));
            }
        }
        let mut output_state = ImageOutputState::disabled();
        output_state.output_kind = Some(output_kind);
        let (sender, receiver) = mpsc::sync_channel(1);
        output_state.worker_request_sender = Some(sender);
        output_state.prepare_frame(
            &output_paints,
            Some(Arc::new(blank_snapshot(Rect::new(0, 0, 64, 64)))),
            Some(PixelCellSize::from_pixel_dimensions(1, 1).unwrap()),
        );
        let actual_output_paints = receiver.try_recv().map(|worker_request| {
            worker_request
                .output_paints
                .into_iter()
                .map(|output_paint| {
                    (
                        output_paint.placement_key,
                        output_paint.target_area,
                        output_paint.source_rect,
                        output_paint.image_record.image.rgba_bytes.clone(),
                    )
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            actual_output_paints,
            Ok(expected_output_paints),
            "{output_kind:?}"
        );
    }
}

use std::sync::mpsc;
use std::sync::Arc;

use koshi_core::geometry::PixelCellSize;
use koshi_core::ids::PaneId;
use koshi_renderer::ImageCellState;
use koshi_terminal::graphics::{ImageAction, ImageDisplay};
use koshi_terminal::style::{Color, Style};

fn build_output_paint(
    pixel_bytes: Vec<u8>,
    image_pixel_width: u32,
    image_pixel_height: u32,
    layer_index: i32,
) -> OutputPaint {
    let decoded_image = DecodedImage {
        pixel_width: image_pixel_width,
        pixel_height: image_pixel_height,
        rgba_bytes: pixel_bytes,
    };
    let image_source_rect = ImageSourceRect {
        pixel_x: 0,
        pixel_y: 0,
        pixel_width: image_pixel_width,
        pixel_height: image_pixel_height,
    };
    let image_alpha_stats = compute_alpha_stats(&decoded_image, image_source_rect);
    let image_record = Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Iterm2,
        image: Arc::new(decoded_image),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay {
            z_index: layer_index,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    });
    let placement_key = (PaneId::new(), 1);
    OutputPaint {
        placement_key,
        image_content_id: 1,
        image_record,
        target_area: Rect::new(0, 0, image_pixel_width as u16, image_pixel_height as u16),
        source_rect: image_source_rect,
        cell_pixel_offset_x: None,
        cell_pixel_offset_y: None,
        z_index: layer_index,
        alpha_stats: image_alpha_stats,
    }
}

fn build_image_cell_snapshot(
    layout_area: Rect,
    cell_states: Vec<ImageCellState>,
) -> ImageCellSnapshot {
    assert_eq!(
        cell_states.len(),
        usize::from(layout_area.width) * usize::from(layout_area.height)
    );
    ImageCellSnapshot::from_cell_states(layout_area, cell_states).expect("test cells fit area")
}

fn build_solid_cell_snapshot(layout_area: Rect, rgb_color: [u8; 3]) -> ImageCellSnapshot {
    let mut cell_style = Style::default();
    cell_style.set_background_color(Color::Rgb(rgb_color[0], rgb_color[1], rgb_color[2]));
    build_image_cell_snapshot(
        layout_area,
        vec![
            ImageCellState {
                style: cell_style,
                ..ImageCellState::default()
            };
            usize::from(layout_area.width) * usize::from(layout_area.height)
        ],
    )
}

#[test]
fn opaque_negative_z_image_is_kept_when_a_glyph_is_under_it() {
    let layout_area = Rect::new(0, 0, 1, 1);
    let underlying_cell_state = ImageCellState {
        character: 'X',
        ..ImageCellState::default()
    };
    let image_cell_snapshot = build_image_cell_snapshot(layout_area, vec![underlying_cell_state]);
    let output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, -1);
    let encode_key = build_output_encode_key(
        ImageOutputKind::Iterm,
        PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size"),
        &output_paint,
    );

    let image_plan = classify_output_paint(
        ImageOutputKind::Iterm,
        &image_cell_snapshot,
        &HashSet::new(),
        &output_paint,
        encode_key,
    )
    .expect("the image remains in the output plan");
    assert!(image_plan.image_compatibility.has_text_layer_order_mismatch);
}

#[test]
fn iterm_partial_alpha_on_a_default_blank_preserves_rgba() {
    let output_paint = build_output_paint(vec![10, 20, 30, 128], 1, 1, 1);
    let output_paints = [output_paint];
    let cell_snapshot = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let image_plans =
        classify_image_plans(ImageOutputKind::Iterm, &cell_snapshot, &output_paints, None);

    assert_eq!(
        image_plans[0].image_compatibility,
        ImageCompatibility::default()
    );
    assert_eq!(
        image_plans[0].iterm_composition,
        ItermComposition::default()
    );
    let worker_request =
        build_iterm_worker_request(Arc::clone(&cell_snapshot), &output_paints, None);
    let template_units = encode_iterm_template(
        &worker_request,
        &image_plans[0],
        &[],
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("iTerm2 image encodes");
    assert_eq!(
        decode_iterm_output_units(&template_units).rgba_bytes,
        [10, 20, 30, 128]
    );
}

#[test]
fn iterm_zero_alpha_on_a_default_blank_preserves_rgba() {
    let output_paint = build_output_paint(vec![10, 20, 30, 0], 1, 1, 1);
    let output_paints = [output_paint];
    let cell_snapshot = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let image_plans =
        classify_image_plans(ImageOutputKind::Iterm, &cell_snapshot, &output_paints, None);

    assert_eq!(
        image_plans[0].image_compatibility,
        ImageCompatibility::default()
    );
    let worker_request =
        build_iterm_worker_request(Arc::clone(&cell_snapshot), &output_paints, None);
    let template_units = encode_iterm_template(
        &worker_request,
        &image_plans[0],
        &[],
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("iTerm2 image encodes");
    assert_eq!(
        decode_iterm_output_units(&template_units).rgba_bytes,
        [10, 20, 30, 0]
    );
}

#[test]
fn iterm_partial_alpha_flattens_over_one_explicit_background_without_cell_size() {
    let output_paint = build_output_paint(vec![0, 0, 255, 128], 1, 1, 1);
    let output_paints = [output_paint];
    let cell_snapshot = Arc::new(build_solid_cell_snapshot(
        Rect::new(0, 0, 1, 1),
        [255, 0, 0],
    ));
    let image_plans =
        classify_image_plans(ImageOutputKind::Iterm, &cell_snapshot, &output_paints, None);

    assert_eq!(
        image_plans[0].image_compatibility,
        ImageCompatibility::default()
    );
    assert_eq!(
        image_plans[0].iterm_composition.background_color,
        Some([255, 0, 0])
    );
    let worker_request =
        build_iterm_worker_request(Arc::clone(&cell_snapshot), &output_paints, None);
    let template_units = encode_iterm_template(
        &worker_request,
        &image_plans[0],
        &[],
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("iTerm2 image encodes");
    assert_eq!(
        decode_iterm_output_units(&template_units).rgba_bytes,
        [127, 0, 128, 255]
    );
}

#[test]
fn iterm_nonopaque_pixels_over_a_glyph_are_unavailable() {
    let output_paint = build_output_paint(vec![0, 0, 255, 128], 1, 1, 1);
    let output_paints = [output_paint];
    let cell_snapshot = build_image_cell_snapshot(
        Rect::new(0, 0, 1, 1),
        vec![ImageCellState {
            character: 'A',
            ..ImageCellState::default()
        }],
    );
    let image_plans = classify_image_plans(
        ImageOutputKind::Iterm,
        &cell_snapshot,
        &output_paints,
        Some(PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size")),
    );

    assert_eq!(
        image_plans[0].image_compatibility,
        ImageCompatibility {
            has_iterm_alpha_mismatch: true,
            ..ImageCompatibility::default()
        }
    );
}

#[test]
fn iterm_opaque_positive_image_replaces_a_glyph() {
    let output_paint = build_output_paint(vec![0, 0, 255, 255], 1, 1, 1);
    let output_paints = [output_paint];
    let cell_snapshot = build_image_cell_snapshot(
        Rect::new(0, 0, 1, 1),
        vec![ImageCellState {
            character: 'A',
            ..ImageCellState::default()
        }],
    );
    let image_plans =
        classify_image_plans(ImageOutputKind::Iterm, &cell_snapshot, &output_paints, None);

    assert_eq!(
        image_plans[0].image_compatibility,
        ImageCompatibility::default()
    );
    let worker_request = build_iterm_worker_request(Arc::new(cell_snapshot), &output_paints, None);
    let template_units = encode_iterm_template(
        &worker_request,
        &image_plans[0],
        &[],
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("iTerm2 image encodes");
    assert_eq!(
        decode_iterm_output_units(&template_units).rgba_bytes,
        [0, 0, 255, 255]
    );
}

#[test]
fn iterm_partial_images_compose_over_every_lower_alpha_layer() {
    let lower_output_paint = build_output_paint(vec![255, 0, 0, 128], 1, 1, 0);
    let upper_output_paint = build_output_paint(vec![0, 0, 255, 128], 1, 1, 1);
    let output_paints = [lower_output_paint, upper_output_paint];
    let cell_snapshot = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let image_plans = classify_image_plans(
        ImageOutputKind::Iterm,
        &cell_snapshot,
        &output_paints,
        Some(cell_size),
    );

    assert_eq!(
        image_plans[0].image_compatibility,
        ImageCompatibility::default()
    );
    assert_eq!(
        image_plans[1].image_compatibility,
        ImageCompatibility::default()
    );
    assert!(image_plans[1].iterm_composition.uses_per_cell_composition);
    let worker_request =
        build_iterm_worker_request(Arc::clone(&cell_snapshot), &output_paints, Some(cell_size));
    let template_units = encode_iterm_template(
        &worker_request,
        &image_plans[1],
        &image_plans[..1],
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("composed iTerm2 image encodes");
    assert_eq!(
        decode_iterm_output_units(&template_units).rgba_bytes,
        [85, 0, 170, 192]
    );
}

#[test]
fn iterm_partial_lower_coverage_preserves_uncovered_default_pixels() {
    let lower_output_paint = build_output_paint(vec![255, 0, 0, 128], 1, 1, 0);
    let mut upper_output_paint = build_output_paint(vec![0, 0, 255, 128, 0, 0, 0, 0], 2, 1, 1);
    upper_output_paint.target_area = Rect::new(0, 0, 2, 1);
    let output_paints = [lower_output_paint, upper_output_paint];
    let cell_snapshot = Arc::new(blank_snapshot(Rect::new(0, 0, 2, 1)));
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let image_plans = classify_image_plans(
        ImageOutputKind::Iterm,
        &cell_snapshot,
        &output_paints,
        Some(cell_size),
    );
    let worker_request =
        build_iterm_worker_request(Arc::clone(&cell_snapshot), &output_paints, Some(cell_size));
    let template_units = encode_iterm_template(
        &worker_request,
        &image_plans[1],
        &image_plans[..1],
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("partly covered iTerm2 image encodes");

    assert_eq!(
        image_plans[1].image_compatibility,
        ImageCompatibility::default()
    );
    assert_eq!(
        decode_iterm_output_units(&template_units).rgba_bytes,
        [85, 0, 170, 192, 0, 0, 0, 0]
    );
}

#[test]
fn iterm_per_cell_backgrounds_preserve_explicit_and_default_cells() {
    let mut red_cell_style = Style::default();
    red_cell_style.set_background_color(Color::Rgb(255, 0, 0));
    let cell_snapshot = Arc::new(build_image_cell_snapshot(
        Rect::new(0, 0, 2, 1),
        vec![
            ImageCellState {
                style: red_cell_style,
                ..ImageCellState::default()
            },
            ImageCellState::default(),
        ],
    ));
    let output_paint = build_output_paint([0, 0, 255, 128].repeat(2), 2, 1, 1);
    let output_paints = [output_paint];
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let image_plans = classify_image_plans(
        ImageOutputKind::Iterm,
        &cell_snapshot,
        &output_paints,
        Some(cell_size),
    );
    let worker_request =
        build_iterm_worker_request(Arc::clone(&cell_snapshot), &output_paints, Some(cell_size));
    let template_units = encode_iterm_template(
        &worker_request,
        &image_plans[0],
        &[],
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("per-cell iTerm2 image encodes");

    assert_eq!(
        image_plans[0].image_compatibility,
        ImageCompatibility::default()
    );
    assert_eq!(
        decode_iterm_output_units(&template_units).rgba_bytes,
        [127, 0, 128, 255, 0, 0, 255, 128]
    );

    let image_plans_without_cell_size =
        classify_image_plans(ImageOutputKind::Iterm, &cell_snapshot, &output_paints, None);
    assert_eq!(
        image_plans_without_cell_size[0].image_compatibility,
        ImageCompatibility {
            has_iterm_alpha_mismatch: true,
            ..ImageCompatibility::default()
        }
    );
}

#[test]
fn kitty_background_layer_boundary_is_exact_for_native_host_protocols() {
    let explicit_background_cells = build_solid_cell_snapshot(Rect::new(0, 0, 1, 1), [1, 2, 3]);
    let default_background_cells = blank_snapshot(Rect::new(0, 0, 1, 1));
    for output_kind in [
        ImageOutputKind::Iterm,
        ImageOutputKind::Sixel {
            palette_color_count: 2,
            max_pixel_width: None,
            max_pixel_height: None,
        },
    ] {
        let measured_cell_size = output_kind
            .is_sixel()
            .then(|| PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size"));
        let below_boundary_paints = [build_output_paint(vec![1, 2, 3, 255], 1, 1, -1_073_741_825)];
        let boundary_paints = [build_output_paint(vec![1, 2, 3, 255], 1, 1, -1_073_741_824)];

        assert_eq!(
            classify_image_plans(
                output_kind,
                &explicit_background_cells,
                &below_boundary_paints,
                measured_cell_size,
            )[0]
            .image_compatibility,
            ImageCompatibility {
                has_text_layer_order_mismatch: true,
                ..ImageCompatibility::default()
            },
            "{output_kind:?} below boundary over RGB"
        );
        assert_eq!(
            classify_image_plans(
                output_kind,
                &explicit_background_cells,
                &boundary_paints,
                measured_cell_size,
            )[0]
            .image_compatibility,
            ImageCompatibility::default(),
            "{output_kind:?} at boundary over RGB"
        );
        assert_eq!(
            classify_image_plans(
                output_kind,
                &default_background_cells,
                &below_boundary_paints,
                measured_cell_size,
            )[0]
            .image_compatibility,
            ImageCompatibility::default(),
            "{output_kind:?} below boundary over default background"
        );
        assert_eq!(
            classify_image_plans(
                output_kind,
                &default_background_cells,
                &boundary_paints,
                measured_cell_size,
            )[0]
            .image_compatibility,
            ImageCompatibility::default(),
            "{output_kind:?} at boundary over default background"
        );
    }
}

#[test]
fn mixed_explicit_backgrounds_do_not_look_like_one_solid_color() {
    let layout_area = Rect::new(0, 0, 2, 1);
    let red_background_cells = build_solid_cell_snapshot(layout_area, [255, 0, 0]);
    let mut second_style = Style::default();
    second_style.set_background_color(Color::Rgb(0, 0, 255));
    let mixed_background_cells = build_image_cell_snapshot(
        layout_area,
        vec![
            ImageCellState {
                style: red_background_cells
                    .find_cell(0, 0)
                    .expect("red background cell")
                    .style,
                ..ImageCellState::default()
            },
            ImageCellState {
                style: second_style,
                ..ImageCellState::default()
            },
        ],
    );
    let output_paint = build_output_paint(vec![255, 0, 0, 127, 255, 0, 0, 127], 2, 1, 0);
    let encode_key = build_output_encode_key(
        ImageOutputKind::Iterm,
        PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size"),
        &output_paint,
    );

    let image_plan = classify_output_paint(
        ImageOutputKind::Iterm,
        &mixed_background_cells,
        &HashSet::new(),
        &output_paint,
        encode_key,
    )
    .expect("the image remains in the output plan");
    assert_eq!(image_plan.sixel_composition.background_color, None);
}

#[test]
fn a_default_background_between_rgb_cells_is_incompatible() {
    let layout_area = Rect::new(0, 0, 3, 1);
    let mut red_cell_style = Style::default();
    red_cell_style.set_background_color(Color::Rgb(255, 0, 0));
    let mut blue_cell_style = Style::default();
    blue_cell_style.set_background_color(Color::Rgb(0, 0, 255));
    let cell_snapshot = build_image_cell_snapshot(
        layout_area,
        vec![
            ImageCellState {
                style: red_cell_style,
                ..ImageCellState::default()
            },
            ImageCellState::default(),
            ImageCellState {
                style: blue_cell_style,
                ..ImageCellState::default()
            },
        ],
    );
    let output_paint = build_output_paint([255, 0, 0, 127].repeat(3), 3, 1, 0);
    let encode_key = build_output_encode_key(
        ImageOutputKind::Iterm,
        PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size"),
        &output_paint,
    );

    let image_plan = classify_output_paint(
        ImageOutputKind::Iterm,
        &cell_snapshot,
        &HashSet::new(),
        &output_paint,
        encode_key,
    )
    .expect("the image remains in the output plan");
    assert_eq!(image_plan.sixel_composition.background_color, None);
}

#[test]
fn sixel_partial_alpha_reencodes_when_the_cell_background_changes() {
    let output_kind = ImageOutputKind::Sixel {
        palette_color_count: 2,
        max_pixel_width: None,
        max_pixel_height: None,
    };
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let output_paint = build_output_paint(vec![255, 0, 0, 128], 1, 1, 0);
    let red_cells = Arc::new(build_solid_cell_snapshot(
        Rect::new(0, 0, 1, 1),
        [255, 0, 0],
    ));
    let blue_cells = Arc::new(build_solid_cell_snapshot(
        Rect::new(0, 0, 1, 1),
        [0, 0, 255],
    ));
    let mut output_state = ImageOutputState::disabled();
    output_state.output_kind = Some(output_kind);
    let red_key = {
        let mut encode_key = build_output_encode_key(output_kind, cell_size, &output_paint);
        encode_key.composition_revision = output_state.update_composition_revisions(
            output_kind,
            &red_cells,
            std::slice::from_ref(&output_paint),
        )[0];
        encode_key
    };
    let blue_key = {
        let mut encode_key = build_output_encode_key(output_kind, cell_size, &output_paint);
        encode_key.composition_revision = output_state.update_composition_revisions(
            output_kind,
            &blue_cells,
            std::slice::from_ref(&output_paint),
        )[0];
        encode_key
    };
    assert_ne!(red_key, blue_key);

    let red_image_plan = classify_output_paint(
        output_kind,
        &red_cells,
        &HashSet::new(),
        &output_paint,
        red_key,
    )
    .expect("red background is encodable");
    let blue_image_plan = classify_output_paint(
        output_kind,
        &blue_cells,
        &HashSet::new(),
        &output_paint,
        blue_key,
    )
    .expect("blue background is encodable");
    let build_worker_request = |cell_snapshot, encode_key| WorkerRequest {
        frame_generation: 1,
        output_kind,
        pixel_cell_size: cell_size,
        measured_pixel_cell_size: Some(cell_size),
        cell_snapshot: Some(cell_snapshot),
        output_paints: vec![output_paint.clone()],
        encode_keys: vec![encode_key],
        kitty_paint_images: Vec::new(),
        cancellation_token: Arc::new(AtomicBool::new(false)),
    };
    let red_template_units = encode_sixel_template(
        &build_worker_request(Arc::clone(&red_cells), red_key),
        &red_image_plan,
        &[],
        2,
        None,
        None,
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("red output encodes");
    let blue_template_units = encode_sixel_template(
        &build_worker_request(Arc::clone(&blue_cells), blue_key),
        &blue_image_plan,
        &[],
        2,
        None,
        None,
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("blue output encodes");
    assert_eq!(
        decode_sixel_output_unit(&red_template_units[0].output_bytes).rgba_bytes,
        [255, 0, 0, 255]
    );
    assert_eq!(
        decode_sixel_output_unit(&blue_template_units[0].output_bytes).rgba_bytes,
        [128, 0, 128, 255]
    );
    assert_ne!(
        red_template_units[0].output_bytes,
        blue_template_units[0].output_bytes
    );
}

#[test]
fn sixel_overlapping_partial_alpha_is_composed_over_the_lower_image() {
    let output_kind = ImageOutputKind::Sixel {
        palette_color_count: 2,
        max_pixel_width: None,
        max_pixel_height: None,
    };
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let lower_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let upper_paint = build_output_paint(vec![0, 0, 255, 128], 1, 1, 1);
    let cell_snapshot = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let lower_key = build_output_encode_key(output_kind, cell_size, &lower_paint);
    let upper_key = build_output_encode_key(output_kind, cell_size, &upper_paint);
    let lower_image_plan = classify_output_paint(
        output_kind,
        &cell_snapshot,
        &HashSet::new(),
        &lower_paint,
        lower_key,
    )
    .expect("lower image is encodable");
    let mut covered_cell_positions = HashSet::new();
    add_target_area_cells(lower_paint.target_area, &mut covered_cell_positions);
    let upper_image_plan = classify_output_paint(
        output_kind,
        &cell_snapshot,
        &covered_cell_positions,
        &upper_paint,
        upper_key,
    )
    .expect("upper image remains in the output plan");
    let mut image_plans = vec![lower_image_plan, upper_image_plan];
    resolve_image_compatibility(output_kind, Some(cell_size), &mut image_plans);
    let upper_image_plan = &image_plans[1];
    assert!(upper_image_plan.sixel_composition.has_alpha);
    assert_eq!(
        upper_image_plan.image_compatibility,
        ImageCompatibility::default()
    );
    let worker_request = WorkerRequest {
        frame_generation: 1,
        output_kind,
        pixel_cell_size: cell_size,
        measured_pixel_cell_size: Some(cell_size),
        cell_snapshot: Some(cell_snapshot),
        output_paints: vec![lower_paint.clone(), upper_paint.clone()],
        encode_keys: vec![lower_key, upper_key],
        kitty_paint_images: Vec::new(),
        cancellation_token: Arc::new(AtomicBool::new(false)),
    };
    let template_units = encode_sixel_template(
        &worker_request,
        upper_image_plan,
        &image_plans[..1],
        2,
        None,
        None,
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("composed Sixel output encodes");

    assert_eq!(
        decode_sixel_output_unit(&template_units[0].output_bytes).rgba_bytes,
        [128, 0, 128, 255]
    );
}

#[test]
fn sixel_terminal_background_requires_the_cell_background_under_opaque_lower_pixels() {
    let output_kind = ImageOutputKind::Sixel {
        palette_color_count: 2,
        max_pixel_width: None,
        max_pixel_height: None,
    };
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let lower_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let mut upper_output_paint = build_output_paint(vec![0, 0, 255, 0], 1, 1, 1);
    let mut upper_image_record = upper_output_paint.image_record.as_ref().clone();
    upper_image_record.display.sixel_background = Some(SixelBackground::Terminal);
    upper_output_paint.image_record = Arc::new(upper_image_record);
    let output_paints = [lower_output_paint, upper_output_paint];

    let default_cells = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let default_plans =
        classify_image_plans(output_kind, &default_cells, &output_paints, Some(cell_size));
    assert_eq!(
        default_plans[1].image_compatibility,
        ImageCompatibility {
            has_sixel_terminal_background_mismatch: true,
            ..ImageCompatibility::default()
        }
    );

    let rgb_cells = Arc::new(build_solid_cell_snapshot(
        Rect::new(0, 0, 1, 1),
        [0, 255, 0],
    ));
    let rgb_plans = classify_image_plans(output_kind, &rgb_cells, &output_paints, Some(cell_size));
    assert_eq!(
        rgb_plans[1].image_compatibility,
        ImageCompatibility::default()
    );
    let worker_request = WorkerRequest {
        frame_generation: 1,
        output_kind,
        pixel_cell_size: cell_size,
        measured_pixel_cell_size: Some(cell_size),
        cell_snapshot: Some(rgb_cells),
        output_paints: output_paints.to_vec(),
        encode_keys: output_paints
            .iter()
            .map(|output_paint| build_output_encode_key(output_kind, cell_size, output_paint))
            .collect(),
        kitty_paint_images: Vec::new(),
        cancellation_token: Arc::new(AtomicBool::new(false)),
    };
    let template_units = encode_sixel_template(
        &worker_request,
        &rgb_plans[1],
        &rgb_plans[..1],
        2,
        None,
        None,
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("known terminal background encodes");
    assert_eq!(
        decode_sixel_output_unit(&template_units[0].output_bytes).rgba_bytes,
        [0, 255, 0, 255]
    );
}

#[test]
fn sixel_composition_revision_tracks_exact_cells_targets_and_pixels() {
    let output_kind = ImageOutputKind::Sixel {
        palette_color_count: 2,
        max_pixel_width: None,
        max_pixel_height: None,
    };
    let layout_area = Rect::new(0, 0, 1, 1);
    let blank_cell_snapshot = Arc::new(build_solid_cell_snapshot(layout_area, [20, 30, 40]));
    let output_paint = build_output_paint(vec![255, 0, 0, 127], 1, 1, 0);
    let mut output_state = ImageOutputState::disabled();

    assert_eq!(
        output_state.update_composition_revisions(
            output_kind,
            &blank_cell_snapshot,
            std::slice::from_ref(&output_paint)
        ),
        [1]
    );
    assert_eq!(
        output_state.update_composition_revisions(
            output_kind,
            &Arc::new(blank_cell_snapshot.as_ref().clone()),
            std::slice::from_ref(&output_paint)
        ),
        [1]
    );

    let mut glyph_cell_state = ImageCellState {
        style: blank_cell_snapshot
            .find_cell(0, 0)
            .expect("blank cell")
            .style,
        ..ImageCellState::default()
    };
    glyph_cell_state.character = 'X';
    let cell_snapshot_with_glyph = Arc::new(
        ImageCellSnapshot::from_cell_states(layout_area, vec![glyph_cell_state])
            .expect("test cells"),
    );
    assert_eq!(
        output_state.update_composition_revisions(
            output_kind,
            &cell_snapshot_with_glyph,
            std::slice::from_ref(&output_paint)
        ),
        [2]
    );

    let mut moved_output_paint = output_paint.clone();
    moved_output_paint.target_area.x = 1;
    assert_eq!(
        output_state.update_composition_revisions(
            output_kind,
            &cell_snapshot_with_glyph,
            std::slice::from_ref(&moved_output_paint)
        ),
        [3]
    );

    let mut changed_pixel_paint = moved_output_paint.clone();
    let mut changed_image_record = changed_pixel_paint.image_record.as_ref().clone();
    changed_image_record.image = Arc::new(DecodedImage {
        pixel_width: 1,
        pixel_height: 1,
        rgba_bytes: vec![0, 0, 255, 127],
    });
    changed_pixel_paint.image_record = Arc::new(changed_image_record);
    assert_eq!(
        changed_pixel_paint.image_content_id,
        moved_output_paint.image_content_id
    );
    assert_eq!(
        changed_pixel_paint.placement_key,
        moved_output_paint.placement_key
    );
    assert_eq!(
        changed_pixel_paint.target_area,
        moved_output_paint.target_area
    );
    assert_eq!(
        output_state.update_composition_revisions(
            output_kind,
            &cell_snapshot_with_glyph,
            std::slice::from_ref(&changed_pixel_paint)
        ),
        [4]
    );
}

#[test]
fn sixel_composition_ignores_cells_outside_every_image() {
    let output_kind = ImageOutputKind::Sixel {
        palette_color_count: 2,
        max_pixel_width: None,
        max_pixel_height: None,
    };
    let layout_area = Rect::new(0, 0, 2, 1);
    let output_paint = build_output_paint(vec![255, 0, 0, 127], 1, 1, 0);
    let initial_cell_snapshot = Arc::new(build_image_cell_snapshot(
        layout_area,
        vec![ImageCellState::default(), ImageCellState::default()],
    ));
    let outside_glyph_cell_snapshot = Arc::new(build_image_cell_snapshot(
        layout_area,
        vec![
            ImageCellState::default(),
            ImageCellState {
                character: 'X',
                ..ImageCellState::default()
            },
        ],
    ));
    let mut output_state = ImageOutputState::disabled();

    assert_eq!(
        output_state.update_composition_revisions(
            output_kind,
            &initial_cell_snapshot,
            std::slice::from_ref(&output_paint)
        ),
        [1]
    );
    assert_eq!(
        output_state.update_composition_revisions(
            output_kind,
            &outside_glyph_cell_snapshot,
            std::slice::from_ref(&output_paint)
        ),
        [1]
    );
}

#[test]
fn iterm_composition_revision_tracks_only_each_target_and_its_lower_images() {
    let output_kind = ImageOutputKind::Iterm;
    let target_output_paint = build_output_paint(vec![255, 0, 0, 128], 1, 1, 0);
    let mut outside_output_paint = build_output_paint(vec![0, 255, 0, 128], 1, 1, 0);
    outside_output_paint.target_area.x = 1;
    let output_paints = [target_output_paint.clone(), outside_output_paint.clone()];
    let blank_cell_snapshot = Arc::new(blank_snapshot(Rect::new(0, 0, 2, 1)));
    let mut output_state = ImageOutputState::disabled();
    let initial_revisions = output_state.update_composition_revisions(
        output_kind,
        &blank_cell_snapshot,
        &output_paints,
    );
    assert_eq!(initial_revisions, [1, 2]);

    let mut blue_cell_style = Style::default();
    blue_cell_style.set_background_color(Color::Rgb(0, 0, 255));
    let changed_outside_cell_snapshot = Arc::new(build_image_cell_snapshot(
        Rect::new(0, 0, 2, 1),
        vec![
            ImageCellState::default(),
            ImageCellState {
                style: blue_cell_style,
                ..ImageCellState::default()
            },
        ],
    ));
    let outside_change_revisions = output_state.update_composition_revisions(
        output_kind,
        &changed_outside_cell_snapshot,
        &output_paints,
    );
    assert_eq!(outside_change_revisions[0], initial_revisions[0]);
    assert_ne!(outside_change_revisions[1], initial_revisions[1]);

    let mut red_cell_style = Style::default();
    red_cell_style.set_background_color(Color::Rgb(255, 0, 0));
    let changed_target_cell_snapshot = Arc::new(build_image_cell_snapshot(
        Rect::new(0, 0, 2, 1),
        vec![
            ImageCellState {
                style: red_cell_style,
                ..ImageCellState::default()
            },
            changed_outside_cell_snapshot
                .find_cell(1, 0)
                .expect("outside cell")
                .clone(),
        ],
    ));
    let target_change_revisions = output_state.update_composition_revisions(
        output_kind,
        &changed_target_cell_snapshot,
        &output_paints,
    );
    assert_ne!(target_change_revisions[0], outside_change_revisions[0]);
}

#[test]
fn iterm_composition_revision_changes_with_an_intersecting_lower_image() {
    let output_kind = ImageOutputKind::Iterm;
    let lower_output_paint = build_output_paint(vec![255, 0, 0, 128], 1, 1, 0);
    let upper_output_paint = build_output_paint(vec![0, 0, 255, 128], 1, 1, 1);
    let cell_snapshot = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let mut output_state = ImageOutputState::disabled();
    let initial_revisions = output_state.update_composition_revisions(
        output_kind,
        &cell_snapshot,
        &[lower_output_paint.clone(), upper_output_paint.clone()],
    );

    let mut changed_lower_output_paint = lower_output_paint;
    let mut changed_image_record = changed_lower_output_paint.image_record.as_ref().clone();
    changed_image_record.image = Arc::new(DecodedImage {
        pixel_width: 1,
        pixel_height: 1,
        rgba_bytes: vec![0, 255, 0, 128],
    });
    changed_lower_output_paint.image_record = Arc::new(changed_image_record);
    let changed_revisions = output_state.update_composition_revisions(
        output_kind,
        &cell_snapshot,
        &[changed_lower_output_paint, upper_output_paint],
    );

    assert_ne!(changed_revisions[0], initial_revisions[0]);
    assert_ne!(changed_revisions[1], initial_revisions[1]);
}

#[test]
fn binary_alpha_sixel_output_can_keep_a_glyph_in_a_zero_bit() {
    let layout_area = Rect::new(0, 0, 1, 1);
    let underlying_cell_state = ImageCellState {
        character: 'X',
        ..ImageCellState::default()
    };
    let cell_snapshot = build_image_cell_snapshot(layout_area, vec![underlying_cell_state]);
    let output_paint = build_output_paint(vec![255, 0, 0, 0], 1, 1, 0);
    let encode_key = build_output_encode_key(
        ImageOutputKind::Sixel {
            palette_color_count: 2,
            max_pixel_width: None,
            max_pixel_height: None,
        },
        PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size"),
        &output_paint,
    );

    let image_plan = classify_output_paint(
        ImageOutputKind::Sixel {
            palette_color_count: 2,
            max_pixel_width: None,
            max_pixel_height: None,
        },
        &cell_snapshot,
        &HashSet::new(),
        &output_paint,
        encode_key,
    )
    .expect("transparent Sixel pixels remain in the output plan");
    assert_eq!(
        image_plan.image_compatibility,
        ImageCompatibility::default()
    );
}

#[test]
fn output_state_keeps_i_term_available_without_a_pixel_cell_query() {
    let output_state = ImageOutputState::from_output_kind(Some(ImageOutputKind::Iterm));

    assert_eq!(output_state.output_kind(), Some(ImageOutputKind::Iterm));
    assert!(!output_state.work_pending());
}

#[test]
fn disconnected_worker_settles_each_distinct_frame_as_unavailable() {
    let mut output_state = ImageOutputState::disabled();
    output_state.output_kind = Some(ImageOutputKind::Iterm);
    let (sender, receiver) = mpsc::sync_channel(1);
    drop(receiver);
    output_state.worker_request_sender = Some(sender);
    let cell_snapshot = Some(Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1))));
    let first_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let first_image_paint = ImagePaint::from_image_placement(
        first_output_paint.placement_key.0,
        first_output_paint.placement_key.1,
        Arc::clone(&first_output_paint.image_record),
        first_output_paint.target_area,
        first_output_paint.source_rect,
        first_output_paint.z_index,
    );

    assert!(output_state.prepare_frame(
        std::slice::from_ref(&first_image_paint),
        cell_snapshot.clone(),
        Some(PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size")),
    ));
    assert_eq!(output_state.list_prepared_placement_keys(), []);
    assert!(!output_state.work_pending());
    output_state.commit_frame();

    let second_output_paint = build_output_paint(vec![0, 0, 255, 255], 1, 1, 0);
    let second_image_paint = ImagePaint::from_image_placement(
        first_output_paint.placement_key.0,
        2,
        Arc::clone(&second_output_paint.image_record),
        second_output_paint.target_area,
        second_output_paint.source_rect,
        second_output_paint.z_index,
    );
    assert!(output_state.prepare_frame(
        std::slice::from_ref(&second_image_paint),
        cell_snapshot,
        Some(PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size")),
    ));
    assert_eq!(output_state.list_prepared_placement_keys(), []);
    assert!(!output_state.work_pending());
}

#[test]
fn i_term_packet_accounting_uses_the_native_frame_limit() {
    assert_eq!(
        compute_checked_output_byte_count(
            MAX_SIXEL_OUTPUT_BYTE_COUNT,
            1,
            MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT
        )
        .expect("the native frame limit is larger"),
        MAX_SIXEL_OUTPUT_BYTE_COUNT + 1
    );
    assert!(compute_checked_output_byte_count(
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
        1,
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT
    )
    .is_err());
}

#[test]
fn placement_key_reuses_pixels_across_frame_record_wrappers() {
    let first_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let mut second_output_paint = first_output_paint.clone();
    second_output_paint.image_record = Arc::new(first_output_paint.image_record.as_ref().clone());

    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");

    assert_eq!(
        build_output_encode_key(ImageOutputKind::Iterm, cell_size, &first_output_paint),
        build_output_encode_key(ImageOutputKind::Iterm, cell_size, &second_output_paint)
    );

    let mut different_z_index = second_output_paint;
    different_z_index.z_index = 1;
    assert_ne!(
        build_output_encode_key(ImageOutputKind::Iterm, cell_size, &first_output_paint),
        build_output_encode_key(ImageOutputKind::Iterm, cell_size, &different_z_index)
    );
}

#[test]
fn sixel_mode_contract_saves_resets_and_restores_all_three_modes() {
    assert_eq!(
        get_sixel_mode_save_bytes(),
        b"\x1b[?80s\x1b[?8452s\x1b[?1070s"
    );
    assert_eq!(SIXEL_MODE_RESET_BYTES, b"\x1b[?80l\x1b[?8452l\x1b[?1070h");
    assert_eq!(
        get_sixel_mode_restore_bytes(),
        b"\x1b[?80r\x1b[?8452r\x1b[?1070r"
    );
}

#[test]
fn crop_image_uses_the_requested_source_rectangle() {
    let mut source_output_paint = build_output_paint(vec![255, 0, 0, 255, 0, 255, 0, 255], 2, 1, 0);
    source_output_paint.source_rect = ImageSourceRect {
        pixel_x: 1,
        pixel_y: 0,
        pixel_width: 1,
        pixel_height: 1,
    };

    let cropped = crop_output_image(&source_output_paint, None).expect("source rectangle is valid");

    assert_eq!(cropped.pixel_width, 1);
    assert_eq!(cropped.pixel_height, 1);
    assert_eq!(cropped.rgba_bytes, vec![0, 255, 0, 255]);
}

#[test]
fn crop_image_rejects_a_source_rectangle_outside_the_decoded_pixels() {
    let mut source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    source_output_paint.source_rect = ImageSourceRect {
        pixel_x: 1,
        pixel_y: 0,
        pixel_width: 1,
        pixel_height: 1,
    };

    assert_eq!(crop_output_image(&source_output_paint, None), Err(()));
}

#[test]
fn scaled_tile_samples_from_the_visible_crop_origin() {
    let decoded_image = DecodedImage {
        pixel_width: 2,
        pixel_height: 1,
        rgba_bytes: vec![255, 0, 0, 255, 0, 0, 255, 255],
    };

    let scaled = scale_output_tile(
        &decoded_image,
        ImageSourceRect {
            pixel_x: 1,
            pixel_y: 0,
            pixel_width: 1,
            pixel_height: 1,
        },
        Rect::new(0, 0, 1, 1),
        TileRect {
            column_offset: 0,
            row_offset: 0,
            column_count: 1,
            row_count: 1,
        },
        1,
        1,
        None,
    )
    .expect("the crop is inside the source image");

    assert_eq!(scaled.rgba_bytes, vec![0, 0, 255, 255]);
}

#[test]
fn worker_emits_one_complete_i_term_packet_for_one_opaque_pixel() {
    let output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let cell_snapshot = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let output_kind = ImageOutputKind::Iterm;
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let encode_key = build_output_encode_key(output_kind, cell_size, &output_paint);
    let worker_request = WorkerRequest {
        frame_generation: 1,
        output_kind,
        pixel_cell_size: cell_size,
        measured_pixel_cell_size: Some(cell_size),
        cell_snapshot: Some(cell_snapshot),
        output_paints: vec![output_paint],
        encode_keys: vec![encode_key],
        kitty_paint_images: Vec::new(),
        cancellation_token: Arc::new(AtomicBool::new(false)),
    };
    let (sender, receiver) = mpsc::sync_channel(8);

    run_worker_job(&worker_request, &sender).expect("worker encodes the pixel");
    let worker_messages = receiver.try_iter().collect::<Vec<_>>();

    assert_eq!(worker_messages.len(), 2);
    let WorkerMessage::Prepared { .. } = &worker_messages[0] else {
        panic!("the worker prepares before output");
    };
    let WorkerMessage::Unit(output_unit) = &worker_messages[1] else {
        panic!("the worker emits one packet");
    };
    assert!(output_unit.output_bytes.starts_with(b"\x1b]1337;"));
    assert!(output_unit.output_bytes.ends_with(b"\x1b\\"));
}

#[test]
fn worker_emits_a_complete_bounded_sixel_tile() {
    let output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let cell_snapshot = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let output_kind = ImageOutputKind::Sixel {
        palette_color_count: 2,
        max_pixel_width: None,
        max_pixel_height: None,
    };
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let encode_key = build_output_encode_key(output_kind, cell_size, &output_paint);
    let worker_request = WorkerRequest {
        frame_generation: 1,
        output_kind,
        pixel_cell_size: cell_size,
        measured_pixel_cell_size: Some(cell_size),
        cell_snapshot: Some(cell_snapshot),
        output_paints: vec![output_paint],
        encode_keys: vec![encode_key],
        kitty_paint_images: Vec::new(),
        cancellation_token: Arc::new(AtomicBool::new(false)),
    };
    let (sender, receiver) = mpsc::sync_channel(8);

    run_worker_job(&worker_request, &sender).expect("worker encodes the pixel");
    let worker_messages = receiver.try_iter().collect::<Vec<_>>();

    assert_eq!(worker_messages.len(), 2);
    let WorkerMessage::Unit(output_unit) = &worker_messages[1] else {
        panic!("the worker emits one tile");
    };
    assert_eq!(output_unit.tile_offset, (0, 0));
    assert!(output_unit.output_bytes.starts_with(b"\x1bP"));
    assert!(output_unit.output_bytes.ends_with(b"\x1b\\"));
    assert!(output_unit.output_bytes.len() <= MAX_SIXEL_TILE_BYTE_COUNT);
}

#[test]
fn worker_emits_shared_templates_in_original_paint_order() {
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let pane = PaneId::new();
    for output_kind in [
        ImageOutputKind::Iterm,
        ImageOutputKind::Sixel {
            palette_color_count: 2,
            max_pixel_width: None,
            max_pixel_height: None,
        },
    ] {
        let mut first_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
        first_output_paint.set_placement_key((pane, 1));
        let mut middle_output_paint = build_output_paint(vec![0, 0, 255, 255], 1, 1, 0);
        middle_output_paint.set_placement_key((pane, 2));
        middle_output_paint.image_content_id = 2;
        let mut last_output_paint = first_output_paint.clone();
        last_output_paint.set_placement_key((pane, 3));
        let output_paints = vec![first_output_paint, middle_output_paint, last_output_paint];
        let worker_request = WorkerRequest {
            frame_generation: 1,
            output_kind,
            pixel_cell_size: cell_size,
            measured_pixel_cell_size: Some(cell_size),
            cell_snapshot: Some(Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)))),
            encode_keys: output_paints
                .iter()
                .map(|output_paint| build_output_encode_key(output_kind, cell_size, output_paint))
                .collect(),
            output_paints,
            kitty_paint_images: Vec::new(),
            cancellation_token: Arc::new(AtomicBool::new(false)),
        };
        let (sender, receiver) = mpsc::sync_channel(16);

        run_worker_job(&worker_request, &sender).expect("the worker encodes all paints");
        let output_units = receiver
            .try_iter()
            .filter_map(|message| match message {
                WorkerMessage::Unit(unit) => Some(unit),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            output_units
                .iter()
                .map(|unit| unit.placement_key)
                .collect::<Vec<_>>(),
            [(pane, 1), (pane, 2), (pane, 3)],
            "{output_kind:?}"
        );
        let decoded_pixels = output_units
            .iter()
            .map(|unit| match output_kind {
                ImageOutputKind::Iterm => {
                    decode_iterm_output_units(&[TemplateUnit {
                        tile_offset: unit.tile_offset,
                        output_bytes: Arc::clone(&unit.output_bytes),
                    }])
                    .rgba_bytes
                }
                ImageOutputKind::Sixel { .. } => {
                    decode_sixel_output_unit(&unit.output_bytes).rgba_bytes
                }
                ImageOutputKind::Kitty => unreachable!(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            decoded_pixels,
            [
                vec![255, 0, 0, 255],
                vec![0, 0, 255, 255],
                vec![255, 0, 0, 255],
            ],
            "{output_kind:?}"
        );
    }
}

#[test]
fn unavailable_paint_does_not_skip_independent_output() {
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let pane = PaneId::new();
    for output_kind in [
        ImageOutputKind::Iterm,
        ImageOutputKind::Sixel {
            palette_color_count: 2,
            max_pixel_width: None,
            max_pixel_height: None,
        },
    ] {
        for unavailable_first in [true, false] {
            let mut unavailable_output_paint = build_output_paint(vec![255, 0, 0, 128], 1, 1, 0);
            unavailable_output_paint.set_placement_key((pane, 1));
            unavailable_output_paint.target_area.x = 1;
            let mut available_output_paint = build_output_paint(vec![0, 0, 255, 255], 1, 1, 0);
            available_output_paint.set_placement_key((pane, 2));
            available_output_paint.image_content_id = 2;
            let output_paints = if unavailable_first {
                vec![unavailable_output_paint, available_output_paint]
            } else {
                vec![available_output_paint, unavailable_output_paint]
            };
            let worker_request = WorkerRequest {
                frame_generation: 1,
                output_kind,
                pixel_cell_size: cell_size,
                measured_pixel_cell_size: Some(cell_size),
                cell_snapshot: Some(Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)))),
                encode_keys: output_paints
                    .iter()
                    .map(|output_paint| {
                        build_output_encode_key(output_kind, cell_size, output_paint)
                    })
                    .collect(),
                output_paints,
                kitty_paint_images: Vec::new(),
                cancellation_token: Arc::new(AtomicBool::new(false)),
            };
            let (sender, receiver) = mpsc::sync_channel(16);

            run_worker_job(&worker_request, &sender)
                .expect("the worker continues after an unavailable paint");
            let worker_messages = receiver.try_iter().collect::<Vec<_>>();
            let unavailable_placement_keys = worker_messages
                .iter()
                .filter_map(|message| match message {
                    WorkerMessage::Unavailable { placement_key, .. } => Some(*placement_key),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let available_placement_keys = worker_messages
                .iter()
                .filter_map(|message| match message {
                    WorkerMessage::Unit(unit) => Some(unit.placement_key),
                    _ => None,
                })
                .collect::<Vec<_>>();

            assert_eq!(unavailable_placement_keys, [(pane, 1)], "{output_kind:?}");
            assert_eq!(available_placement_keys, [(pane, 2)], "{output_kind:?}");
        }
    }
}

#[test]
fn worker_stops_before_unique_templates_exceed_the_frame_bound() {
    let output_kind = ImageOutputKind::Iterm;
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let pane = PaneId::new();
    let output_paints = (0u8..8)
        .map(|image_variant_index| {
            let mut output_paint = build_output_paint(
                vec![image_variant_index, 0, 255 - image_variant_index, 255],
                1,
                1,
                0,
            );
            output_paint.set_placement_key((pane, u64::from(image_variant_index) + 1));
            output_paint.image_content_id = u64::from(image_variant_index) + 1;
            output_paint
        })
        .collect::<Vec<_>>();
    let cell_snapshot = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
    let worker_request = WorkerRequest {
        frame_generation: 1,
        output_kind,
        pixel_cell_size: cell_size,
        measured_pixel_cell_size: Some(cell_size),
        cell_snapshot: Some(Arc::clone(&cell_snapshot)),
        encode_keys: output_paints
            .iter()
            .map(|output_paint| build_output_encode_key(output_kind, cell_size, output_paint))
            .collect(),
        output_paints,
        kitty_paint_images: Vec::new(),
        cancellation_token: Arc::new(AtomicBool::new(false)),
    };
    let image_plans = classify_image_plans(
        output_kind,
        &cell_snapshot,
        &worker_request.output_paints,
        Some(cell_size),
    );
    let first_template_byte_count = encode_iterm_template(
        &worker_request,
        &image_plans[0],
        &[],
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("first template encodes")
    .iter()
    .map(|unit| unit.output_bytes.len())
    .sum::<usize>();
    let second_template_byte_count = encode_iterm_template(
        &worker_request,
        &image_plans[1],
        &image_plans[..1],
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("second template encodes")
    .iter()
    .map(|unit| unit.output_bytes.len())
    .sum::<usize>();
    let output_byte_limit = first_template_byte_count + second_template_byte_count - 1;
    let (sender, receiver) = mpsc::sync_channel(32);

    assert_eq!(
        run_worker_job_with_limit(&worker_request, &sender, output_byte_limit),
        Err(())
    );
    let worker_messages = receiver.try_iter().collect::<Vec<_>>();
    assert_eq!(
        worker_messages
            .iter()
            .filter(|message| matches!(message, WorkerMessage::Prepared { .. }))
            .count(),
        1
    );
    assert_eq!(
        worker_messages
            .iter()
            .filter(|message| matches!(message, WorkerMessage::Unit(_)))
            .count(),
        1
    );
}

#[test]
fn rejected_cumulative_worker_output_cancels_the_generation() {
    let output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let placement_key = output_paint.placement_key;
    let cancellation_token = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::sync_channel(2);
    sender
        .send(WorkerMessage::Unit(OutputUnit {
            frame_generation: 1,
            placement_key,
            output_kind: ImageOutputKind::Iterm,
            tile_offset: (0, 0),
            output_bytes: Arc::from(&b"x"[..]),
        }))
        .expect("unit queues");
    sender
        .send(WorkerMessage::Finished {
            frame_generation: 1,
            has_failed: false,
        })
        .expect("finish queues");
    drop(sender);
    let mut output_state = ImageOutputState::disabled();
    output_state.output_kind = Some(ImageOutputKind::Iterm);
    output_state.frame_generation = 1;
    output_state.latest_output_paints = vec![output_paint];
    output_state.rebuild_latest_index();
    output_state
        .prepared_placement_key_set
        .insert(placement_key);
    output_state.output_unit_byte_count = MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT;
    output_state.worker_messages = Some(receiver);
    output_state.active_job = Some(ActiveJob {
        frame_generation: 1,
        cancellation_token: Arc::clone(&cancellation_token),
    });

    output_state.poll();

    assert!(cancellation_token.load(Ordering::Acquire));
    assert_eq!(output_state.output_unit_byte_count, 0);
    assert_eq!(output_state.output_units.len(), 0);
    assert!(output_state.is_ready);
    assert!(!output_state.work_pending());
}

#[test]
fn one_cell_sixel_output_may_exceed_one_transport_chunk() {
    let (image_pixel_width, image_pixel_height) = (256, 256);
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
    let mut random_value = 0x9e37_79b9u32;
    let mut rgba_bytes = Vec::with_capacity(image_pixel_width * image_pixel_height * 4);
    for _ in 0..image_pixel_width * image_pixel_height {
        random_value ^= random_value << 13;
        random_value ^= random_value >> 17;
        random_value ^= random_value << 5;
        rgba_bytes.extend_from_slice(&palette[(random_value as usize) % palette.len()]);
        rgba_bytes.push(255);
    }
    let mut output_paint = build_output_paint(
        rgba_bytes.clone(),
        image_pixel_width as u32,
        image_pixel_height as u32,
        0,
    );
    output_paint.target_area = Rect::new(0, 0, 1, 1);
    let output_kind = ImageOutputKind::Sixel {
        palette_color_count: palette.len(),
        max_pixel_width: None,
        max_pixel_height: None,
    };
    let cell_size =
        PixelCellSize::from_pixel_dimensions(image_pixel_width as u16, image_pixel_height as u16)
            .expect("test cell size");
    let encode_key = build_output_encode_key(output_kind, cell_size, &output_paint);
    let worker_request = WorkerRequest {
        frame_generation: 1,
        output_kind,
        pixel_cell_size: cell_size,
        measured_pixel_cell_size: Some(cell_size),
        cell_snapshot: Some(Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)))),
        output_paints: vec![output_paint.clone()],
        encode_keys: vec![encode_key],
        kitty_paint_images: Vec::new(),
        cancellation_token: Arc::new(AtomicBool::new(false)),
    };
    let template_units = encode_sixel_template(
        &worker_request,
        &Plan {
            output_paint: &output_paint,
            encode_key,
            iterm_composition: ItermComposition::default(),
            sixel_composition: SixelComposition::default(),
            image_compatibility: ImageCompatibility::default(),
            is_opaque: true,
        },
        &[],
        palette.len(),
        None,
        None,
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("one-cell Sixel output encodes");

    assert_eq!(template_units.len(), 1);
    assert!(template_units[0].output_bytes.len() > MAX_SIXEL_TILE_BYTE_COUNT);
    assert!(template_units[0].output_bytes.len() <= MAX_SIXEL_OUTPUT_BYTE_COUNT);
    assert_eq!(
        decode_sixel_output_unit(&template_units[0].output_bytes).rgba_bytes,
        rgba_bytes
    );
}

#[test]
fn i_term_unit_output_has_exact_position_payload_and_cursor_restore() {
    let output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let mut output_state = ImageOutputState::from_output_kind(Some(ImageOutputKind::Iterm));
    output_state.latest_output_paints = vec![output_paint];
    output_state.rebuild_latest_index();
    output_state.output_units.push(OutputUnit {
        frame_generation: 1,
        placement_key: output_state.latest_output_paints[0].placement_key,
        output_kind: ImageOutputKind::Iterm,
        tile_offset: (0, 0),
        output_bytes: Arc::from(&b"body"[..]),
    });
    let frame_output_bytes = output_state
        .frame_output(Some(ratatui::layout::Position { x: 4, y: 5 }))
        .expect("frame output writes");
    assert_eq!(frame_output_bytes, b"\x1b[1;1Hbody\x1b[6;5H");
}

#[test]
fn sixel_unit_output_has_exact_mode_boundaries_and_cursor_restore() {
    let output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let output_kind = ImageOutputKind::Sixel {
        palette_color_count: 2,
        max_pixel_width: None,
        max_pixel_height: None,
    };
    let mut output_state = ImageOutputState::from_output_kind(Some(output_kind));
    output_state.latest_output_paints = vec![output_paint];
    output_state.rebuild_latest_index();
    output_state.output_units.push(OutputUnit {
        frame_generation: 1,
        placement_key: output_state.latest_output_paints[0].placement_key,
        output_kind,
        tile_offset: (0, 0),
        output_bytes: Arc::from(&b"sixel"[..]),
    });
    let frame_output_bytes = output_state
        .frame_output(Some(ratatui::layout::Position { x: 4, y: 5 }))
        .expect("frame output writes");
    assert_eq!(
        frame_output_bytes,
        b"\x1b[?80l\x1b[?8452l\x1b[?1070h\x1b[1;1Hsixel\x1b[6;5H\x1b[?80r\x1b[?8452r\x1b[?1070r"
    );
}

fn blank_snapshot(area: Rect) -> ImageCellSnapshot {
    ImageCellSnapshot::from_cell_states(
        area,
        vec![ImageCellState::default(); usize::from(area.width) * usize::from(area.height)],
    )
    .expect("test cells fit area")
}

fn build_kitty_output_state() -> (ImageOutputState, mpsc::Receiver<WorkerRequest>) {
    let mut output_state = ImageOutputState::disabled();
    output_state.output_kind = Some(ImageOutputKind::Kitty);
    let (sender, receiver) = mpsc::sync_channel(4);
    output_state.worker_request_sender = Some(sender);
    (output_state, receiver)
}

#[test]
fn kitty_transmits_one_image_once_and_replaces_it_after_it_moves() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let build_image_paint_at_row = |row_index: u16| {
        ImagePaint::from_image_placement(
            source_output_paint.placement_key.0,
            1,
            Arc::clone(&source_output_paint.image_record),
            Rect::new(0, row_index, 1, 1),
            source_output_paint.source_rect,
            0,
        )
    };
    let (mut output_state, requests) = build_kitty_output_state();

    assert!(!output_state.prepare_frame(&[build_image_paint_at_row(0)], None, None));
    let first_worker_request = requests
        .try_recv()
        .expect("the first frame reaches the worker");
    assert_eq!(
        first_worker_request.kitty_paint_images,
        vec![KittyPaintImage {
            kitty_image_number: 1,
            should_transmit_image: true,
        }]
    );
    output_state.commit_frame();
    output_state.active_job = None;

    assert!(!output_state.prepare_frame(&[build_image_paint_at_row(5)], None, None));
    let moved_worker_request = requests
        .try_recv()
        .expect("the moved frame reaches the worker");
    assert_eq!(
        moved_worker_request.kitty_paint_images,
        vec![KittyPaintImage {
            kitty_image_number: 1,
            should_transmit_image: false,
        }]
    );
    assert_eq!(
        output_state.kitty_image_numbers_to_delete,
        Vec::<u32>::new()
    );
    assert!(!output_state.should_free_all_kitty_images);
}

#[test]
fn kitty_frees_one_image_number_after_its_content_leaves_the_frame() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let placed_image_paint = ImagePaint::from_image_placement(
        source_output_paint.placement_key.0,
        1,
        Arc::clone(&source_output_paint.image_record),
        Rect::new(0, 0, 1, 1),
        source_output_paint.source_rect,
        0,
    );
    let (mut output_state, requests) = build_kitty_output_state();

    assert!(!output_state.prepare_frame(&[placed_image_paint], None, None));
    requests
        .try_recv()
        .expect("the first frame reaches the worker");
    output_state.commit_frame();
    output_state.active_job = None;
    output_state.has_host_pixels = true;

    assert!(output_state.prepare_frame(&[], None, None));
    assert_eq!(output_state.kitty_image_numbers_to_delete, vec![1]);
    let mut frame_reset_bytes = Vec::new();
    assert!(!output_state
        .write_frame_reset(&mut frame_reset_bytes)
        .expect("the frame reset writes"));
    assert_eq!(
        frame_reset_bytes,
        b"\x1b_Ga=d,d=a,q=2;\x1b\\\x1b_Ga=d,d=N,I=1,q=2;\x1b\\"
    );
    assert_eq!(
        output_state.kitty_image_numbers_to_delete,
        Vec::<u32>::new()
    );
}

#[test]
fn a_failed_kitty_frame_frees_every_image_the_host_holds() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let placed_image_paint = ImagePaint::from_image_placement(
        source_output_paint.placement_key.0,
        1,
        Arc::clone(&source_output_paint.image_record),
        Rect::new(0, 0, 1, 1),
        source_output_paint.source_rect,
        0,
    );
    let (mut output_state, requests) = build_kitty_output_state();

    assert!(!output_state.prepare_frame(&[placed_image_paint], None, None));
    requests
        .try_recv()
        .expect("the first frame reaches the worker");
    output_state.commit_frame();
    output_state.fail_frame_commit();

    assert!(output_state.should_free_all_kitty_images);
    assert!(output_state.kitty_image_by_content_id.is_empty());
    let mut frame_reset_bytes = Vec::new();
    assert!(!output_state
        .write_frame_reset(&mut frame_reset_bytes)
        .expect("the frame reset writes"));
    assert_eq!(frame_reset_bytes, b"\x18\x1b\\\x1b_Ga=d,d=A,q=2;\x1b\\");
    assert!(!output_state.should_free_all_kitty_images);
}

#[test]
fn a_failed_iterm_frame_frees_no_kitty_image() {
    let mut output_state = ImageOutputState::disabled();
    output_state.output_kind = Some(ImageOutputKind::Iterm);

    output_state.fail_frame_commit();

    assert!(!output_state.should_free_all_kitty_images);
}

#[test]
fn an_opaque_iterm_or_sixel_image_that_moves_reuses_its_encoded_output() {
    use std::time::{Duration, Instant};

    for output_kind in [
        ImageOutputKind::Iterm,
        ImageOutputKind::Sixel {
            palette_color_count: 256,
            max_pixel_width: None,
            max_pixel_height: None,
        },
    ] {
        let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
        let build_image_paint_at_row = |row_index: u16| {
            ImagePaint::from_image_placement(
                source_output_paint.placement_key.0,
                1,
                Arc::clone(&source_output_paint.image_record),
                Rect::new(0, row_index, 1, 1),
                source_output_paint.source_rect,
                0,
            )
        };
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("one-pixel cell");
        let cell_snapshot = Arc::new(blank_snapshot(Rect::new(0, 0, 8, 8)));
        let mut output_state = ImageOutputState::from_output_kind(Some(output_kind));

        let deadline = Instant::now() + Duration::from_secs(5);
        while !output_state.prepare_frame(
            &[build_image_paint_at_row(0)],
            Some(Arc::clone(&cell_snapshot)),
            Some(cell_size),
        ) {
            assert!(Instant::now() < deadline, "{output_kind:?} did not settle");
            std::thread::sleep(crate::tests::TEST_POLL_INTERVAL_DURATION);
        }
        let first_frame_output_bytes = output_state
            .frame_output(None)
            .expect("the first frame writes");
        output_state.commit_frame();
        assert!(
            !first_frame_output_bytes.is_empty(),
            "{output_kind:?} wrote no pixels"
        );

        assert!(
            output_state.prepare_frame(
                &[build_image_paint_at_row(3)],
                Some(cell_snapshot),
                Some(cell_size),
            ),
            "{output_kind:?} did not commit the moved frame"
        );
        assert!(
            !output_state.work_pending(),
            "{output_kind:?} started another encode"
        );
        let moved_frame_output_bytes = output_state
            .frame_output(None)
            .expect("the moved frame writes");
        let rebased_frame_output_bytes = String::from_utf8(moved_frame_output_bytes)
            .expect("image output is ASCII")
            .replacen("\x1b[4;1H", "\x1b[1;1H", 1)
            .into_bytes();
        assert_eq!(
            rebased_frame_output_bytes, first_frame_output_bytes,
            "{output_kind:?} re-encoded the moved image"
        );
    }
}

#[test]
fn a_partly_transparent_iterm_image_that_moves_encodes_again() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 128], 1, 1, 0);
    let build_image_paint_at_row = |row_index: u16| {
        ImagePaint::from_image_placement(
            source_output_paint.placement_key.0,
            1,
            Arc::clone(&source_output_paint.image_record),
            Rect::new(0, row_index, 1, 1),
            source_output_paint.source_rect,
            0,
        )
    };
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("one-pixel cell");
    let cell_snapshot = Arc::new(blank_snapshot(Rect::new(0, 0, 8, 8)));
    let mut output_state = ImageOutputState::disabled();
    output_state.output_kind = Some(ImageOutputKind::Iterm);
    let (sender, requests) = mpsc::sync_channel(4);
    output_state.worker_request_sender = Some(sender);

    assert!(!output_state.prepare_frame(
        &[build_image_paint_at_row(0)],
        Some(Arc::clone(&cell_snapshot)),
        Some(cell_size),
    ));
    let first_worker_request = requests
        .try_recv()
        .expect("the first frame reaches the worker");
    output_state.active_job = None;

    assert!(!output_state.prepare_frame(
        &[build_image_paint_at_row(3)],
        Some(cell_snapshot),
        Some(cell_size),
    ));
    let moved_worker_request = requests
        .try_recv()
        .expect("the moved frame reaches the worker");
    assert_ne!(
        first_worker_request.encode_keys,
        moved_worker_request.encode_keys
    );
}

#[test]
fn a_new_image_under_one_content_identity_takes_a_new_kitty_number() {
    let first_source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let second_source_output_paint = build_output_paint(vec![0, 255, 0, 255], 1, 1, 0);
    let build_placed_image_paint = |image_record: &Arc<ImageRecord>| {
        ImagePaint::from_image_placement(
            first_source_output_paint.placement_key.0,
            1,
            Arc::clone(image_record),
            Rect::new(0, 0, 1, 1),
            first_source_output_paint.source_rect,
            0,
        )
    };
    let (mut output_state, requests) = build_kitty_output_state();

    assert!(!output_state.prepare_frame(
        &[build_placed_image_paint(
            &first_source_output_paint.image_record
        )],
        None,
        None
    ));
    requests
        .try_recv()
        .expect("the first frame reaches the worker");
    output_state.commit_frame();
    output_state.active_job = None;

    assert!(!output_state.prepare_frame(
        &[build_placed_image_paint(
            &second_source_output_paint.image_record
        )],
        None,
        None
    ));
    let replaced = requests
        .try_recv()
        .expect("the replacing frame reaches the worker");
    assert_eq!(
        replaced.kitty_paint_images,
        vec![KittyPaintImage {
            kitty_image_number: 2,
            should_transmit_image: true,
        }]
    );
    assert_eq!(output_state.kitty_image_numbers_to_delete, vec![1]);
}

#[test]
fn a_failed_kitty_encode_transmits_its_pixels_again() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let build_placed_image_paint = || {
        ImagePaint::from_image_placement(
            source_output_paint.placement_key.0,
            1,
            Arc::clone(&source_output_paint.image_record),
            Rect::new(0, 0, 1, 1),
            source_output_paint.source_rect,
            0,
        )
    };
    let (mut output_state, requests) = build_kitty_output_state();
    let (worker_message_sender, worker_messages) = mpsc::sync_channel(4);
    output_state.worker_messages = Some(worker_messages);

    assert!(!output_state.prepare_frame(&[build_placed_image_paint()], None, None));
    let worker_request = requests
        .try_recv()
        .expect("the first frame reaches the worker");
    assert_eq!(
        worker_request.kitty_paint_images,
        vec![KittyPaintImage {
            kitty_image_number: 1,
            should_transmit_image: true,
        }]
    );
    worker_message_sender
        .send(WorkerMessage::Finished {
            frame_generation: worker_request.frame_generation,
            has_failed: true,
        })
        .expect("the failure reaches the state");

    // The failed job wrote nothing, so the host holds no image number.
    output_state.poll();
    assert!(output_state.pending_kitty_images_by_content_id.is_empty());
    output_state.commit_frame();
    assert!(output_state.kitty_image_by_content_id.is_empty());

    assert!(!output_state.prepare_frame(&[build_placed_image_paint()], None, None,));
    let retried_worker_request = requests
        .try_recv()
        .expect("the retried frame reaches the worker");
    assert_eq!(
        retried_worker_request.kitty_paint_images,
        vec![KittyPaintImage {
            kitty_image_number: 2,
            should_transmit_image: true,
        }]
    );
}

#[test]
fn a_failed_iterm_or_sixel_encode_retries_an_unchanged_frame() {
    for output_kind in [
        ImageOutputKind::Iterm,
        ImageOutputKind::Sixel {
            palette_color_count: 256,
            max_pixel_width: None,
            max_pixel_height: None,
        },
    ] {
        let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
        let placed_image_paint = ImagePaint::from_image_placement(
            source_output_paint.placement_key.0,
            source_output_paint.placement_key.1,
            Arc::clone(&source_output_paint.image_record),
            source_output_paint.target_area,
            source_output_paint.source_rect,
            source_output_paint.z_index,
        );
        let mut output_state = ImageOutputState::disabled();
        output_state.output_kind = Some(output_kind);
        let (sender, requests) = mpsc::sync_channel(4);
        output_state.worker_request_sender = Some(sender);
        let (worker_message_sender, worker_messages) = mpsc::sync_channel(4);
        output_state.worker_messages = Some(worker_messages);
        let cell_snapshot = Arc::new(blank_snapshot(Rect::new(0, 0, 1, 1)));
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("one-pixel cell");

        assert!(
            !output_state.prepare_frame(
                std::slice::from_ref(&placed_image_paint),
                Some(Arc::clone(&cell_snapshot)),
                Some(cell_size),
            ),
            "{output_kind:?} did not submit the first frame"
        );
        let worker_request = requests
            .try_recv()
            .expect("the first frame reaches the worker");
        worker_message_sender
            .send(WorkerMessage::Finished {
                frame_generation: worker_request.frame_generation,
                has_failed: true,
            })
            .expect("the failure reaches the state");
        output_state.poll();

        assert!(
            !output_state.prepare_frame(
                std::slice::from_ref(&placed_image_paint),
                Some(cell_snapshot),
                Some(cell_size),
            ),
            "{output_kind:?} did not resubmit the unchanged frame"
        );
        let retried = requests
            .try_recv()
            .expect("the unchanged frame reaches the worker again");
        assert_eq!(
            retried.frame_generation,
            worker_request.frame_generation + 2,
            "{output_kind:?}"
        );
        assert_eq!(
            retried.encode_keys, worker_request.encode_keys,
            "{output_kind:?} changed the frame key"
        );
    }
}

#[test]
fn an_unchanged_frame_after_a_commit_starts_no_work_and_commits_nothing() {
    for output_kind in [ImageOutputKind::Kitty, ImageOutputKind::Iterm] {
        let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
        let build_placed_image_paint = || {
            ImagePaint::from_image_placement(
                source_output_paint.placement_key.0,
                1,
                Arc::clone(&source_output_paint.image_record),
                Rect::new(0, 0, 1, 1),
                source_output_paint.source_rect,
                0,
            )
        };
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("one-pixel cell");
        let build_cell_snapshot = || Some(Arc::new(blank_snapshot(Rect::new(0, 0, 8, 8))));
        let mut output_state = ImageOutputState::disabled();
        output_state.output_kind = Some(output_kind);
        let (sender, requests) = mpsc::sync_channel(4);
        output_state.worker_request_sender = Some(sender);

        assert!(!output_state.prepare_frame(
            &[build_placed_image_paint()],
            build_cell_snapshot(),
            Some(cell_size),
        ));
        requests
            .try_recv()
            .expect("the first frame reaches the worker");
        output_state.active_job = None;
        output_state.is_ready = true;
        output_state.commit_frame();

        assert!(
            output_state.prepare_frame(
                &[build_placed_image_paint()],
                build_cell_snapshot(),
                Some(cell_size),
            ),
            "{output_kind:?}"
        );
        assert!(
            requests.try_recv().is_err(),
            "{output_kind:?} started a job"
        );
        assert!(
            !output_state.native_commit_pending(),
            "{output_kind:?} commits again"
        );
        assert!(
            !output_state.needs_screen_reset,
            "{output_kind:?} resets the screen"
        );
    }
}

#[test]
fn text_written_under_an_opaque_iterm_image_rewrites_it_without_encoding_again() {
    use std::time::{Duration, Instant};

    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let build_placed_image_paint = || {
        ImagePaint::from_image_placement(
            source_output_paint.placement_key.0,
            1,
            Arc::clone(&source_output_paint.image_record),
            Rect::new(0, 0, 1, 1),
            source_output_paint.source_rect,
            0,
        )
    };
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("one-pixel cell");
    let layout_area = Rect::new(0, 0, 2, 1);
    let mut output_state = ImageOutputState::from_output_kind(Some(ImageOutputKind::Iterm));

    let deadline = Instant::now() + Duration::from_secs(5);
    while !output_state.prepare_frame(
        &[build_placed_image_paint()],
        Some(Arc::new(blank_snapshot(layout_area))),
        Some(cell_size),
    ) {
        assert!(Instant::now() < deadline, "the first frame did not settle");
        std::thread::sleep(crate::tests::TEST_POLL_INTERVAL_DURATION);
    }
    let first_frame_output_bytes = output_state
        .frame_output(None)
        .expect("the first frame writes");
    output_state.commit_frame();

    let glyph_under_image = build_image_cell_snapshot(
        layout_area,
        vec![
            ImageCellState {
                character: 'B',
                ..ImageCellState::default()
            },
            ImageCellState::default(),
        ],
    );
    assert!(output_state.prepare_frame(
        &[build_placed_image_paint()],
        Some(Arc::new(glyph_under_image)),
        Some(cell_size),
    ));
    assert!(!output_state.work_pending(), "the glyph started an encode");
    assert!(
        output_state.native_commit_pending(),
        "the image is not written again"
    );
    assert!(
        !output_state.needs_screen_reset,
        "an unmoved image reset the screen"
    );
    let again = output_state
        .frame_output(None)
        .expect("the repaired frame writes");
    assert_eq!(again, first_frame_output_bytes);
}

#[test]
fn a_host_resize_transmits_every_kitty_image_again() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let placed_image_paint = ImagePaint::from_image_placement(
        source_output_paint.placement_key.0,
        1,
        Arc::clone(&source_output_paint.image_record),
        Rect::new(0, 0, 1, 1),
        source_output_paint.source_rect,
        0,
    );
    let (mut output_state, requests) = build_kitty_output_state();
    output_state.set_host_terminal_size(80, 24);

    assert!(!output_state.prepare_frame(std::slice::from_ref(&placed_image_paint), None, None,));
    requests
        .try_recv()
        .expect("the first frame reaches the worker");
    output_state.commit_frame();
    output_state.active_job = None;

    output_state.set_host_terminal_size(80, 24);
    assert!(output_state.prepare_frame(std::slice::from_ref(&placed_image_paint), None, None,));
    assert!(
        requests.try_recv().is_err(),
        "an unchanged size keeps the upload"
    );

    output_state.set_host_terminal_size(100, 24);
    assert!(output_state.should_free_all_kitty_images);
    assert!(!output_state.prepare_frame(&[placed_image_paint], None, None));
    let resized_worker_request = requests
        .try_recv()
        .expect("the resized frame reaches the worker");
    assert_eq!(
        resized_worker_request.kitty_paint_images,
        vec![KittyPaintImage {
            kitty_image_number: 1,
            should_transmit_image: true,
        }]
    );
}

#[test]
fn an_iterm_reset_clears_the_screen_and_a_kitty_reset_does_not() {
    let mut iterm = ImageOutputState::disabled();
    iterm.output_kind = Some(ImageOutputKind::Iterm);
    iterm.needs_screen_reset = true;
    let mut iterm_frame_reset_bytes = Vec::new();
    assert!(iterm
        .write_frame_reset(&mut iterm_frame_reset_bytes)
        .expect("the frame reset writes"));
    assert_eq!(iterm_frame_reset_bytes, b"\x1b[2J");

    let (mut kitty, _requests) = build_kitty_output_state();
    kitty.needs_screen_reset = true;
    let mut kitty_frame_reset_bytes = Vec::new();
    assert!(!kitty
        .write_frame_reset(&mut kitty_frame_reset_bytes)
        .expect("the frame reset writes"));
    assert_eq!(kitty_frame_reset_bytes, b"\x1b_Ga=d,d=a,q=2;\x1b\\");
}

#[test]
fn a_kitty_paint_skips_the_alpha_scan_and_an_iterm_paint_runs_it() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 128], 1, 1, 0);
    let build_placed_image_paint = || {
        ImagePaint::from_image_placement(
            source_output_paint.placement_key.0,
            1,
            Arc::clone(&source_output_paint.image_record),
            Rect::new(0, 0, 1, 1),
            source_output_paint.source_rect,
            0,
        )
    };
    let (mut kitty, _requests) = build_kitty_output_state();
    assert!(!kitty.prepare_frame(&[build_placed_image_paint()], None, None));
    assert_eq!(kitty.latest_output_paints[0].alpha_stats, None);

    let mut iterm = ImageOutputState::disabled();
    iterm.output_kind = Some(ImageOutputKind::Iterm);
    let (sender, _requests) = mpsc::sync_channel(4);
    iterm.worker_request_sender = Some(sender);
    let cell_snapshot = Some(Arc::new(blank_snapshot(Rect::new(0, 0, 8, 8))));
    assert!(!iterm.prepare_frame(
        &[build_placed_image_paint()],
        cell_snapshot,
        PixelCellSize::from_pixel_dimensions(1, 1)
    ));
    assert_eq!(
        iterm.latest_output_paints[0].alpha_stats,
        Some(AlphaStats {
            has_zero: false,
            has_partial: true,
        })
    );
}

#[test]
fn the_first_host_size_forgets_nothing() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let placed_image_paint = ImagePaint::from_image_placement(
        source_output_paint.placement_key.0,
        1,
        Arc::clone(&source_output_paint.image_record),
        Rect::new(0, 0, 1, 1),
        source_output_paint.source_rect,
        0,
    );
    let (mut output_state, requests) = build_kitty_output_state();

    assert!(!output_state.prepare_frame(&[placed_image_paint], None, None));
    requests
        .try_recv()
        .expect("the first frame reaches the worker");
    output_state.commit_frame();
    assert_eq!(output_state.kitty_image_by_content_id.len(), 1);

    output_state.set_host_terminal_size(80, 24);
    assert_eq!(output_state.kitty_image_by_content_id.len(), 1);
    assert!(!output_state.should_free_all_kitty_images);
}

#[test]
fn an_iterm_host_resize_frees_no_kitty_image_and_clears_no_key() {
    let mut output_state = ImageOutputState::disabled();
    output_state.output_kind = Some(ImageOutputKind::Iterm);
    output_state.latest_encode_keys = vec![build_output_encode_key(
        ImageOutputKind::Iterm,
        PixelCellSize::from_pixel_dimensions(1, 1).expect("one-pixel cell"),
        &build_output_paint(vec![255, 0, 0, 255], 1, 1, 0),
    )];
    output_state.set_host_terminal_size(80, 24);

    output_state.set_host_terminal_size(100, 30);

    assert!(!output_state.should_free_all_kitty_images);
    assert_eq!(output_state.latest_encode_keys.len(), 1);
}

#[test]
fn a_kitty_reset_with_only_departed_numbers_frees_them_and_keeps_the_placements() {
    let (mut output_state, _requests) = build_kitty_output_state();
    output_state.kitty_image_numbers_to_delete = vec![3, 7];
    let mut frame_reset_bytes = Vec::new();

    assert!(!output_state
        .write_frame_reset(&mut frame_reset_bytes)
        .expect("the frame reset writes"));

    assert_eq!(
        frame_reset_bytes,
        b"\x1b_Ga=d,d=N,I=3,q=2;\x1b\\\x1b_Ga=d,d=N,I=7,q=2;\x1b\\"
    );
    assert_eq!(
        output_state.kitty_image_numbers_to_delete,
        Vec::<u32>::new()
    );
}

#[test]
fn two_paints_of_one_image_in_one_frame_transmit_it_once() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    // Two placements of one server-side image share one content identity.
    let build_image_paint = |placement_id: u64, row_index: u16| {
        let mut image_paint = ImagePaint::from_image_placement(
            source_output_paint.placement_key.0,
            placement_id,
            Arc::clone(&source_output_paint.image_record),
            Rect::new(0, row_index, 1, 1),
            source_output_paint.source_rect,
            0,
        );
        image_paint.image_content_id = 1;
        image_paint
    };
    let (mut output_state, requests) = build_kitty_output_state();

    assert!(!output_state.prepare_frame(
        &[build_image_paint(1, 0), build_image_paint(2, 3)],
        None,
        None,
    ));
    let worker_request = requests.try_recv().expect("the frame reaches the worker");

    assert_eq!(
        worker_request.kitty_paint_images,
        vec![
            KittyPaintImage {
                kitty_image_number: 1,
                should_transmit_image: true,
            },
            KittyPaintImage {
                kitty_image_number: 1,
                should_transmit_image: false,
            },
        ]
    );
    assert_eq!(output_state.pending_kitty_images_by_content_id.len(), 1);
    output_state.commit_frame();
    assert_eq!(output_state.kitty_image_by_content_id.len(), 1);
    assert_eq!(output_state.next_kitty_image_number, 2);
}

#[test]
fn an_exhausted_kitty_number_space_frees_every_image_and_restarts_at_one() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let placed_image_paint = ImagePaint::from_image_placement(
        source_output_paint.placement_key.0,
        1,
        Arc::clone(&source_output_paint.image_record),
        Rect::new(0, 0, 1, 1),
        source_output_paint.source_rect,
        0,
    );
    let (mut output_state, requests) = build_kitty_output_state();
    output_state.next_kitty_image_number = u32::MAX;
    output_state.kitty_image_by_content_id.insert(
        9,
        KittyImage {
            kitty_image_number: 5,
            image_memory_address: 0,
        },
    );

    assert!(!output_state.prepare_frame(&[placed_image_paint], None, None));
    let worker_request = requests.try_recv().expect("the frame reaches the worker");

    assert!(output_state.should_free_all_kitty_images);
    assert_eq!(
        worker_request.kitty_paint_images,
        vec![KittyPaintImage {
            kitty_image_number: 1,
            should_transmit_image: true,
        }]
    );
    assert_eq!(
        output_state.kitty_image_numbers_to_delete,
        Vec::<u32>::new()
    );
}
